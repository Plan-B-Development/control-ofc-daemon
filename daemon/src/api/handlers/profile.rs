//! Profile management endpoints: active profile query, profile activation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::hwmon::lease::HwmonWriter;

/// The set of sensor entity ids currently discovered on this machine, used to
/// flag (as a warning, never an error) curve `sensor_id`s that aren't present
/// here. See `crate::profile::validate`.
fn known_sensor_ids(state: &AppState) -> HashSet<String> {
    state.cache.sensors_snapshot().keys().cloned().collect()
}

/// GET /profile/active — return the currently active profile, if any.
pub async fn active_profile_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let guard = state.active_profile.lock();
    match guard.as_ref() {
        Some(profile) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "api_version": API_VERSION,
                "active": true,
                "profile_id": profile.id,
                "profile_name": profile.name,
            })),
        ),
        None => (
            StatusCode::OK,
            Json(serde_json::json!({
                "api_version": API_VERSION,
                "active": false,
            })),
        ),
    }
}

/// POST /profile/activate — switch the active profile at runtime.
pub async fn activate_profile_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Accept either profile_id (search by name) or profile_path (direct file).
    // profile_path is restricted to known search directories to prevent
    // arbitrary filesystem reads (P1-R4 security hardening).
    let profile_path = if let Some(path) = body.get("profile_path").and_then(|v| v.as_str()) {
        let p = std::path::PathBuf::from(path);
        let canonical = match p.canonicalize() {
            Ok(c) => c,
            Err(_) => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!("profile path not found: {path}")),
                );
            }
        };
        // Canonicalize both sides to prevent symlink-based path traversal (CWE-22).
        // Skip search dirs that don't exist on disk (can't canonicalize).
        let search_dirs = state.profile_search_dirs.read();
        let allowed: Vec<std::path::PathBuf> = search_dirs
            .iter()
            .filter_map(|d| d.canonicalize().ok())
            .collect();
        if allowed.is_empty() {
            log::warn!(
                "No profile search directories exist on disk: {:?}",
                *search_dirs
            );
        }
        drop(search_dirs); // release lock before potentially long operations
        if !allowed.iter().any(|d| canonical.starts_with(d)) {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(
                    "profile_path must be within a profile search directory",
                ),
            );
        }
        canonical
    } else if let Some(id) = body.get("profile_id").and_then(|v| v.as_str()) {
        let search_dirs = state.profile_search_dirs.read();
        match crate::profile::find_profile(id, &search_dirs) {
            Some(p) => p,
            None => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!("profile '{id}' not found in search paths")),
                );
            }
        }
    } else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("missing 'profile_id' or 'profile_path'"),
        );
    };

    // Load and validate
    let profile = match crate::profile::load_profile(&profile_path) {
        Ok(p) => p,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(e));
        }
    };

    // Reject a hard-invalid profile, leaving the previously active profile
    // running (DEC-160). Warnings (e.g. a sensor absent on this host) never
    // block activation — the engine tolerates a missing sensor at eval time.
    let report = crate::profile::validate(&profile, &known_sensor_ids(&state));
    if !report.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation_with_details(
                format!("profile '{}' failed validation", profile.id),
                report.field_violations_json(),
            ),
        );
    }

    let profile_name = profile.name.clone();
    let profile_id = profile.id.clone();

    // Apply. Everything in this block runs under the `active_profile` lock so
    // the swap and the dependent state resets are observed atomically by the
    // engine tick (which reads `active_profile` then the epoch). Lock order is
    // `active_profile` (outer) → `override_table` / `cache.inner` (inner); the
    // engine never holds those inner locks while waiting on `active_profile`
    // (it releases `override_table` before `profile.lock()`), so there is no
    // inversion (DEC-189).
    {
        let mut guard = state.active_profile.lock();
        *guard = Some(profile);
        // DEC-188: re-anchor the engine even on a same-id re-activation (the
        // "edit the active profile's curve and re-apply" path). Bump the epoch
        // inside the `active_profile` lock so the engine observes the swap and
        // the epoch atomically and re-evaluates the new curve on its next tick,
        // instead of holding the previous output through the 2°C deadband
        // (DEC-096). Switching to a *different* id already re-anchored via
        // `sync_profile_id`; this closes the same-id gap.
        state.cache.bump_profile_activation_epoch();
        // DEC-189: a freshly-activated profile owns its controls' intent. Clear
        // any standing manual overrides while holding `active_profile`, so an
        // override taken against the previous profile cannot bleed onto a
        // same-id control in the new one, and so a concurrent override-take
        // (which now also holds `active_profile`, control.rs) serialises either
        // fully before this clear — and is wiped — or fully after — and is
        // validated against the new profile. Identify-stops are per-fan and
        // profile-independent, so they are left intact. The engine resets the
        // cleared controls' cross-tick state on its next tick via the epoch
        // path above.
        state.override_table.lock().clear_all_overrides();
        // DEC-165 / audit P3-4: a freshly-activated profile takes control of
        // all its members, so clear any GPU fans previously relinquished to
        // firmware-auto via reset. Done inside the `active_profile` lock so the
        // engine cannot evaluate the new profile and skip a still-relinquished
        // GPU fan for one tick (the clear used to run after the lock dropped).
        state.cache.clear_relinquished_gpu_fans();
    }

    // Persist
    let new_state = crate::daemon_state::DaemonState {
        version: 1,
        active_profile_id: Some(profile_id.clone()),
        active_profile_path: Some(profile_path.display().to_string()),
    };
    if let Err(e) = crate::daemon_state::save_state(&new_state) {
        log::warn!("Failed to persist profile state: {e}");
    }

    log::info!("Profile activated: '{profile_name}' (id={profile_id})");

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "api_version": API_VERSION,
            "activated": true,
            "profile_id": profile_id,
            "profile_name": profile_name,
        })),
    )
}

/// POST /profile/deactivate — clear the active profile so the daemon stops
/// driving fans from a curve. Idempotent: deactivating when no profile is
/// active is a success no-op. After deactivation, the daemon falls back to
/// imperative-only behaviour — manual API writes from the GUI still work,
/// but the headless evaluation loop will not push new PWM values until a
/// new profile is activated.
pub async fn deactivate_profile_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let previous = {
        let mut guard = state.active_profile.lock();
        guard.take().map(|p| (p.id, p.name))
    };

    // Persist the cleared state so a daemon restart doesn't resurrect the
    // profile from disk. Best-effort — log on failure but still return
    // success so the caller knows the in-memory state is clean.
    let new_state = crate::daemon_state::DaemonState {
        version: 1,
        active_profile_id: None,
        active_profile_path: None,
    };
    if let Err(e) = crate::daemon_state::save_state(&new_state) {
        log::warn!("Failed to persist deactivation: {e}");
    }

    // Release the profile engine's own self-lease so the next activation
    // re-acquires cleanly. The engine is the sole hwmon writer post-2.0.0
    // (DEC-165); only the "profile-engine" owner is released.
    if let Some(ref ctrl) = state.hwmon_controller {
        let mut guard = ctrl.lock();
        let release_id = guard
            .lease_manager()
            .active_lease()
            .filter(|l| l.owner == HwmonWriter::Engine)
            .map(|l| l.lease_id.clone());
        if let Some(id) = release_id {
            if let Err(e) = guard.lease_manager_mut().release_lease(&id) {
                log::debug!("profile-engine lease release after deactivate failed: {e}");
            }
            // Audit P3-3: pair the release with a coalescing reset, exactly as
            // the thermal force-take does (`profile_engine/backends.rs`
            // force_all). This clears the engine's stale `manual_mode_set` so a
            // later re-activation re-asserts `pwm_enable=1` from a clean slate
            // after the deactivated gap — defense-in-depth alongside the
            // per-write pwm_enable watchdog in `HwmonPwmController::set_pwm`,
            // not an I/O optimisation (the watchdog already preserves
            // correctness; this trades a steady-state readback for a clean
            // re-assert on the next acquisition).
            guard.on_lease_released();
        }
    }

    let (deactivated_id, deactivated_name) = previous
        .map(|(id, name)| (Some(id), Some(name)))
        .unwrap_or((None, None));

    log::info!(
        "Profile deactivated (previous: {:?})",
        deactivated_id.as_deref()
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "api_version": API_VERSION,
            "deactivated": true,
            "previous_profile_id": deactivated_id,
            "previous_profile_name": deactivated_name,
        })),
    )
}

// ───────────────────── Profile CRUD (DEC-160) ─────────────────────
//
// The daemon is the store of record. Writes go to the primary (first) search
// dir — the daemon-owned store that `main` prepends (`with_store_dir`). Reads
// (list/get) span all search dirs so package presets remain visible and
// shadowable. Editing the stored profile set is not a fan write and does not
// affect the running engine until a profile is activated.

/// `true` when `?validate_only=true` is present.
fn is_validate_only(params: &HashMap<String, String>) -> bool {
    params
        .get("validate_only")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Resolve the primary store dir (first search dir). `with_store_dir` makes
/// this the daemon-owned `{state_dir}/profiles` in production.
fn store_dir(state: &AppState) -> Option<std::path::PathBuf> {
    state.profile_search_dirs.read().first().cloned()
}

/// Whether `id` is the currently active profile.
fn is_active(state: &AppState, id: &str) -> bool {
    state
        .active_profile
        .lock()
        .as_ref()
        .map(|p| p.id == id)
        .unwrap_or(false)
}

/// GET /profiles — list stored profiles ∪ package presets (deduped, store wins).
pub async fn list_profiles_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dirs = state.profile_search_dirs.read().clone();
    let profiles = crate::profile_store::list(&dirs);
    json_ok(
        StatusCode::OK,
        ProfileListResponse {
            api_version: API_VERSION,
            profiles,
        },
    )
}

/// GET /profiles/{id} — fetch one profile's full document (lossless).
pub async fn get_profile_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dirs = state.profile_search_dirs.read().clone();
    match crate::profile_store::get_raw(&dirs, &id) {
        Some(value) => (StatusCode::OK, Json(value)),
        None => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::validation(format!("profile '{id}' not found")),
        ),
    }
}

/// Shared validate → (optional) persist body for create/update. `expected_id`
/// is the id the document must carry (the path id for PUT, or the body id for
/// POST). `allow_overwrite` is true for PUT (replace) and false for POST
/// (409 on a store-scoped duplicate). On `validate_only`, nothing is persisted.
fn validate_and_store(
    state: &AppState,
    body: &serde_json::Value,
    expected_id: &str,
    allow_overwrite: bool,
    validate_only: bool,
    success_status: StatusCode,
    success_verb: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    if !crate::profile::is_safe_profile_id(expected_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!("unsafe profile id: {expected_id:?}")),
        );
    }

    // The document's id must match the target id.
    match body.get("id").and_then(|v| v.as_str()) {
        Some(bid) if bid == expected_id => {}
        Some(bid) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!(
                    "profile id '{bid}' in the body does not match '{expected_id}'"
                )),
            )
        }
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("missing 'id' field"),
            )
        }
    }

    // Parse into the model to validate (storage keeps the raw document).
    let profile: crate::profile::DaemonProfile = match serde_json::from_value(body.clone()) {
        Ok(p) => p,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!("malformed profile document: {e}")),
            )
        }
    };

    let report = crate::profile::validate(&profile, &known_sensor_ids(state));
    if !report.is_valid() {
        // AIP-163: a validate_only request fails exactly when a real one would.
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation_with_details(
                "profile failed validation",
                report.field_violations_json(),
            ),
        );
    }

    if validate_only {
        return json_ok(
            StatusCode::OK,
            serde_json::json!({
                "api_version": API_VERSION,
                "valid": true,
                "field_violations": report.warnings,
            }),
        );
    }

    let Some(dir) = store_dir(state) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal("no profile store directory configured"),
        );
    };

    if !allow_overwrite && crate::profile_store::exists_in_store(&dir, expected_id) {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::already_exists(format!("profile '{expected_id}' already exists")),
        );
    }

    // Persist the document as supplied (round-tripped through Value), so fields
    // the daemon model doesn't know are preserved.
    let bytes = match serde_json::to_vec_pretty(body) {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal(format!("failed to serialize profile: {e}")),
            )
        }
    };
    if let Err(e) = crate::profile_store::save_raw(&dir, expected_id, &bytes) {
        // Keep the path-bearing detail server-side; the client gets a generic
        // message (DEC-173 — internal fs paths must not leak in the envelope).
        log::error!("Failed to save profile '{expected_id}': {e}");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal("failed to save profile"),
        );
    }

    log::info!("Profile '{expected_id}' {success_verb} via API");
    let mut resp = serde_json::json!({
        "api_version": API_VERSION,
        "profile_id": expected_id,
        "warnings": report.warnings,
        // PUT updates stored desired-state only; if this id is active the
        // running engine is undisturbed until an explicit re-activate
        // (systemd reload-vs-restart model, DEC-160).
        "active_reactivate_required": is_active(state, expected_id),
    });
    // Action flag matching the API convention ("activated"/"deactivated"):
    // `"created": true` for POST, `"updated": true` for PUT.
    resp[success_verb] = serde_json::Value::Bool(true);
    json_ok(success_status, resp)
}

/// POST /profiles — create a new profile. 409 if the id already exists in the
/// store. `?validate_only=true` validates without persisting.
pub async fn create_profile_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = match body.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("missing 'id' field"),
            )
        }
    };
    validate_and_store(
        &state,
        &body,
        &id,
        false,
        is_validate_only(&params),
        StatusCode::CREATED,
        "created",
    )
}

/// PUT /profiles/{id} — create-or-replace by id. Does NOT hot-reload the active
/// profile. `?validate_only=true` validates without persisting.
pub async fn update_profile_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    validate_and_store(
        &state,
        &body,
        &id,
        true,
        is_validate_only(&params),
        StatusCode::OK,
        "updated",
    )
}

/// DELETE /profiles/{id} — remove a stored profile. 409 if it is active; 404 if
/// it isn't in the store (presets cannot be deleted).
pub async fn delete_profile_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !crate::profile::is_safe_profile_id(&id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!("unsafe profile id: {id:?}")),
        );
    }
    if is_active(&state, &id) {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::profile_in_use(format!(
                "profile '{id}' is active; deactivate or activate another profile first"
            )),
        );
    }
    let Some(dir) = store_dir(&state) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal("no profile store directory configured"),
        );
    };
    match crate::profile_store::delete(&dir, &id) {
        Ok(true) => json_ok(
            StatusCode::OK,
            serde_json::json!({
                "api_version": API_VERSION,
                "deleted": true,
                "profile_id": id,
            }),
        ),
        Ok(false) => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::validation(format!("profile '{id}' not found in store")),
        ),
        Err(e) => {
            // Path-bearing detail to the log only (DEC-173); generic to client.
            log::error!("Failed to delete profile '{id}': {e}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal("failed to delete profile"),
            )
        }
    }
}
