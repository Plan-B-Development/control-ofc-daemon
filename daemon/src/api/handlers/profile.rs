//! Profile management endpoints: active profile query, profile activation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State};
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
    ConnectInfo(peer): ConnectInfo<crate::api::server::UdsConnectInfo>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Accept either profile_id (search by name) or profile_path (direct file).
    // profile_path is restricted to the search directories, and every read goes
    // through `profile_store`'s confined reader (P1-R4, FFA-a).
    enum Request {
        Path(std::path::PathBuf),
        Id(String),
    }
    let request = if let Some(path) = body.get("profile_path").and_then(|v| v.as_str()) {
        Request::Path(std::path::PathBuf::from(path))
    } else if let Some(id) = body.get("profile_id").and_then(|v| v.as_str()) {
        Request::Id(id.to_string())
    } else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("missing 'profile_id' or 'profile_path'"),
        );
    };

    // Snapshot the configured dirs so the read lock is released before any
    // filesystem I/O — never hold it across that.
    let search_dirs: Vec<std::path::PathBuf> = state.profile_search_dirs.read().clone();
    use crate::profile_store::{locate_by_path, read_in_dir, PathReadError};
    // FFA-b: every read runs off the async workers, bounded. A path names its
    // directory as a registered entry, normally as written (the GUI's does); only
    // when it does not is the directory resolved — a client-supplied path, so in
    // the resolution pool, one at a time per uid.
    let read = match request {
        Request::Id(id) => {
            let dirs = search_dirs;
            let lookup = id.clone();
            super::profile_io(move |stop_at| {
                match crate::profile_store::read_by_id_until(&dirs, &lookup, stop_at) {
                    Ok(Some(found)) => Ok(found),
                    Ok(None) => Err(PathReadError::NotFound),
                    Err(_) => Err(PathReadError::OutOfTime),
                }
            })
            .await
            .map(|found| found.map_err(|e| (id, e)))
        }
        Request::Path(path) => {
            let named = path.display().to_string();
            let located = match locate_by_path(&search_dirs, &path, None) {
                Ok((dir, name)) => Ok((dir.to_path_buf(), name)),
                Err(PathReadError::OutsideSearchDirs)
                    if path.is_absolute() && path.parent().is_some() =>
                {
                    let parent = path.parent().map(std::path::Path::to_path_buf);
                    let resolved = super::resolve_io(peer.uid, move || {
                        parent.map(|p| super::path_confine::resolve_client_path(&p))
                    })
                    .await;
                    match resolved {
                        Ok(Some(Ok(real))) => locate_by_path(&search_dirs, &path, Some(&real))
                            .map(|(dir, name)| (dir.to_path_buf(), name)),
                        Ok(_) => Err(PathReadError::NotFound),
                        Err(e) => return super::profile_io_error_response("profile activation", e),
                    }
                }
                Err(e) => Err(e),
            };
            match located {
                Ok((dir, name)) => {
                    super::profile_io(move |stop_at| read_in_dir(&dir, &name, stop_at))
                        .await
                        .map(|found| found.map_err(|e| (named, e)))
                }
                Err(e) => Ok(Err((named, e))),
            }
        }
    };
    let (profile_path, content) = match read {
        Ok(Ok(found)) => found,
        Ok(Err((named, error))) => {
            return match error {
                PathReadError::NotFound => error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!(
                        "profile '{named}' not found in the profile search directories"
                    )),
                ),
                PathReadError::OutsideSearchDirs => error_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorEnvelope::validation(
                        "profile_path must name a file directly inside a profile search directory",
                    ),
                ),
                PathReadError::Unreadable(detail) => {
                    // Path-bearing detail to the log only (DEC-173).
                    log::error!("Profile for activation refused: {detail}");
                    error_response(
                        StatusCode::BAD_REQUEST,
                        &ErrorEnvelope::validation("profile could not be read or parsed"),
                    )
                }
                PathReadError::OutOfTime => super::profile_io_error_response(
                    "profile activation",
                    super::ProfileIoError::TimedOut,
                ),
            };
        }
        Err(e) => return super::profile_io_error_response("profile activation", e),
    };

    // Load and validate
    let profile = match crate::profile::parse_profile(&content, &profile_path) {
        Ok(p) => p,
        Err(e) => {
            // Path-bearing read/parse detail to the log only (DEC-173 —
            // internal fs paths must not leak in the envelope).
            log::error!("Failed to load profile for activation: {e}");
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("profile could not be read or parsed"),
            );
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
    // TS-af / DEC-394: the headers this profile names as pumps, computed from the
    // owned profile before the swap moves it — a pure read, no lock needed.
    let pump_ids = super::pump_header_ids(&profile);

    // FFA-j: the swap and the persist run under the store lock, which a delete
    // holds across its active check and unlink and a deactivation across its own
    // swap and persist. The read above ran without it, so a hung search directory
    // cannot hold the lock; instead a profile read from the store is checked to
    // still exist under the lock, so a delete that won the race is not activated
    // and left saved as the active profile with no file behind it.
    let store = store_dir(&state).map(|d| crate::profile_store::normalize_lexically(&d));
    let from_store = store.is_some() && profile_path.parent() == store.as_deref();
    let state = Arc::clone(&state);
    let id_for_log = profile_id.clone();
    let committed = super::under_store_lock(async move {
        if from_store {
            let path = profile_path.clone();
            let still_there = super::persist_off_runtime(move || Ok(path.exists()))
                .await
                .unwrap_or(false);
            if !still_there {
                return None;
            }
        }
        commit_activation(&state, profile, pump_ids, profile_path).await;
        Some(())
    })
    .await;
    match committed {
        Ok(Some(())) => {}
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorEnvelope::validation(format!(
                    "profile '{id_for_log}' was deleted while it was being activated"
                )),
            );
        }
        Err(e) => {
            log::error!("Profile activation of '{id_for_log}' did not complete: {e}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal("profile activation did not complete"),
            );
        }
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

/// Install `profile` as the active profile and persist the choice. Called under
/// the store lock (FFA-j).
async fn commit_activation(
    state: &AppState,
    profile: crate::profile::DaemonProfile,
    pump_ids: HashSet<String>,
    profile_path: std::path::PathBuf,
) {
    let profile_id = profile.id.clone();

    // Apply. Everything in this block runs under the `active_profile` lock so
    // the swap and the dependent state resets are observed atomically by the
    // engine tick (which reads `active_profile` then the epoch). Lock order is
    // `active_profile` (outer) → `override_table` / `cache.inner` (inner); the
    // engine never holds those inner locks while waiting on `active_profile`
    // (it releases `override_table` before `profile.lock()`), so there is no
    // inversion (DEC-189).
    let released_stops = {
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
        //
        // [SAFETY] TS-af / DEC-394 — the one exception. An identify STOP on a
        // header this profile names as a pump is released: the stop's 0 was
        // chosen before the profile existed to protect it, and would otherwise
        // hold a now-protected pump stopped until its deadman fired. This is the
        // activation twin of the release `update_header_role_handler` performs
        // for a `pump` assignment (DEC-311). Done under this guard because
        // `fan_identify_handler` decides and inserts under it too, so an identify
        // serialises fully before this release — and is released — or fully
        // after — and sees this profile's pump term. A pump perturbation is kept:
        // it is already at or above the floor.
        let released = {
            let mut table = state.override_table.lock();
            table.clear_all_overrides();
            table.release_identify_stops(&pump_ids)
        };
        // DEC-165 / audit P3-4: a freshly-activated profile takes control of
        // all its members, so clear any GPU fans previously relinquished to
        // firmware-auto via reset. Done inside the `active_profile` lock so the
        // engine cannot evaluate the new profile and skip a still-relinquished
        // GPU fan for one tick (the clear used to run after the lock dropped).
        state.cache.clear_relinquished_gpu_fans();
        released
    };
    // Logged after the guard drops — nothing but the swap runs under it.
    for fan_id in &released_stops {
        log::info!(
            "Fan identify: released the stop on {fan_id} — the activated profile names it a pump"
        );
    }

    // Persist
    let new_state = crate::daemon_state::DaemonState {
        version: 1,
        active_profile_id: Some(profile_id),
        active_profile_path: Some(profile_path.display().to_string()),
    };
    // DEC-252: fsync off the async worker threads the engine shares.
    if let Err(e) =
        super::persist_off_runtime(move || crate::daemon_state::save_state(&new_state)).await
    {
        log::warn!("Failed to persist profile state: {e}");
    }
}

/// POST /profile/deactivate — clear the active profile so the daemon stops
/// driving fans from a curve. Idempotent: deactivating when no profile is
/// active is a success no-op. After deactivation no fan curve is evaluated
/// until a new profile is activated: the engine hands back the motherboard
/// (hwmon) headers it took (DEC-382) — not a GPU (`DC-cr`) — and the thermal
/// emergency still acts on its own. There is
/// no client PWM write to fall back to — those were retired at 2.0.0
/// (DEC-165).
pub async fn deactivate_profile_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // FFA-j: the swap and the persist run under the store lock, as activation's
    // do, so the saved state is the last swap's: an activation persisting after
    // this deactivation could otherwise bring its profile back at restart.
    let locked_state = Arc::clone(&state);
    let previous = match super::under_store_lock(deactivate_and_persist(locked_state)).await {
        Ok(previous) => previous,
        Err(e) => {
            log::error!("Profile deactivation did not complete: {e}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal("profile deactivation did not complete"),
            );
        }
    };
    release_engine_lease_and_report(&state, previous)
}

/// Clear the active profile and persist that. Called under the store lock.
async fn deactivate_and_persist(state: Arc<AppState>) -> Option<(String, String)> {
    let previous = {
        let mut guard = state.active_profile.lock();
        let previous = guard.take().map(|p| (p.id, p.name));
        // DEC-218: deactivation relinquishes curve-driven control, so it must
        // also clear standing manual overrides — symmetric with activation
        // (DEC-189). An override outlives the profile it was taken against
        // otherwise, and would bleed onto a same-id control in the next
        // profile. Done while holding `active_profile` (lock order
        // active_profile → override_table, matching activate at line 159) so a
        // concurrent override-take (control.rs, which also holds
        // active_profile) serialises fully before this clear — and is wiped —
        // or fully after.
        state.override_table.lock().clear_all_overrides();
        previous
    };

    // Persist the cleared state so a daemon restart doesn't resurrect the
    // profile from disk. Best-effort — log on failure but still return
    // success so the caller knows the in-memory state is clean.
    let new_state = crate::daemon_state::DaemonState {
        version: 1,
        active_profile_id: None,
        active_profile_path: None,
    };
    // DEC-252: fsync off the async worker threads the engine shares.
    if let Err(e) =
        super::persist_off_runtime(move || crate::daemon_state::save_state(&new_state)).await
    {
        log::warn!("Failed to persist deactivation: {e}");
    }
    previous
}

/// The rest of deactivation: release the engine's lease and answer.
fn release_engine_lease_and_report(
    state: &AppState,
    previous: Option<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
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
            // force_all_with_floor). This clears the engine's stale `manual_mode_set` so a
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
    // FFA-b: off the async workers, bounded.
    let listed = super::profile_io(move |stop_at| crate::profile_store::list_until(&dirs, stop_at));
    let profiles = match listed.await {
        Ok(Ok(profiles)) => profiles,
        Ok(Err(_)) => {
            return super::profile_io_error_response(
                "GET /profiles",
                super::ProfileIoError::TimedOut,
            )
        }
        Err(e) => return super::profile_io_error_response("GET /profiles", e),
    };
    json_ok(
        StatusCode::OK,
        ProfileListResponse {
            api_version: API_VERSION,
            profiles,
        },
    )
}

/// GET /profiles/{id} — fetch one profile's full document, lossless except
/// that hwmon ids are canonicalised (DEC-442): a document stored with the it87
/// v2.0 suffixed chip spelling is served with the ids discovery publishes, so a
/// client comparing its members against `/hwmon/headers` does not see them as
/// missing. The file itself is not rewritten.
pub async fn get_profile_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dirs = state.profile_search_dirs.read().clone();
    // FFA-b: off the async workers, bounded.
    let lookup = id.clone();
    let read = super::profile_io(move |stop_at| {
        crate::profile_store::get_raw_until(&dirs, &lookup, stop_at)
    });
    let found = match read.await {
        Ok(Ok(found)) => found,
        Ok(Err(_)) => {
            return super::profile_io_error_response(
                "GET /profiles/{id}",
                super::ProfileIoError::TimedOut,
            )
        }
        Err(e) => return super::profile_io_error_response("GET /profiles/{id}", e),
    };
    match found {
        Some(mut value) => {
            crate::profile::canonicalize_profile_document(&mut value);
            (StatusCode::OK, Json(value))
        }
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
async fn validate_and_store(
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

    // DEC-442: canonicalise hwmon ids before validating and storing, so a
    // member or sensor carrying the it87 v2.0 suffixed chip spelling is stored
    // under the id discovery publishes. Every other field is untouched.
    let mut body = body.clone();
    crate::profile::canonicalize_profile_document(&mut body);
    let body = &body;

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

    // Persist the document as supplied (round-tripped through Value), so fields
    // the daemon model doesn't know are preserved. Compact, and refused past the
    // read cap (FFA-j): pretty-printing a deeply nested document multiplied its
    // size, and re-serialising expands numbers (`1e9` is stored as
    // `1000000000.0`), so a body the 4 MiB request limit accepted could be
    // stored as a file the daemon then refused to read, list or activate.
    let bytes = match serde_json::to_vec(body) {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal(format!("failed to serialize profile: {e}")),
            )
        }
    };
    let limit = crate::atomic_io::MAX_CONFIG_BYTES;
    if bytes.len() as u64 > limit {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!(
                "profile document is too large to store: {} bytes, the limit is {limit}",
                bytes.len()
            )),
        );
    }
    // FFA-j: the existence and count checks and the write are one step under
    // the store lock. DEC-252: the fsync runs off the async worker threads the
    // engine shares.
    let save_dir = dir.clone();
    let save_id = expected_id.to_string();
    let stored = super::under_store_lock(super::persist_off_runtime(move || {
        Ok(crate::profile_store::store(
            &save_dir,
            &save_id,
            &bytes,
            allow_overwrite,
        ))
    }))
    .await
    .and_then(|stored| stored);
    match stored {
        Ok(Ok(())) => {}
        Ok(Err(crate::profile_store::StoreError::AlreadyExists)) => {
            return error_response(
                StatusCode::CONFLICT,
                &ErrorEnvelope::already_exists(format!("profile '{expected_id}' already exists")),
            );
        }
        Ok(Err(crate::profile_store::StoreError::Full)) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!(
                    "the profile store is full ({} profiles); delete one first",
                    crate::constants::MAX_STORED_PROFILES
                )),
            );
        }
        Ok(Err(crate::profile_store::StoreError::TooLarge)) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!(
                    "the profile store is full ({} MiB in all); delete a profile first",
                    crate::constants::MAX_STORE_BYTES / (1024 * 1024)
                )),
            );
        }
        Ok(Err(crate::profile_store::StoreError::Io(e))) | Err(e) => {
            // Keep the path-bearing detail server-side; the client gets a generic
            // message (DEC-173 — internal fs paths must not leak in the envelope).
            log::error!("Failed to save profile '{expected_id}': {e}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal("failed to save profile"),
            );
        }
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
    .await
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
    .await
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
    let Some(dir) = store_dir(&state) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal("no profile store directory configured"),
        );
    };
    // FFA-j: the active check and the unlink are one step under the store lock,
    // which activation holds across its existence check and swap.
    let locked_state = Arc::clone(&state);
    let del_id = id.clone();
    let deleted = super::under_store_lock(async move {
        if is_active(&locked_state, &del_id) {
            return None;
        }
        // DEC-252: unlink + directory fsync off the async worker threads.
        Some(super::persist_off_runtime(move || crate::profile_store::delete(&dir, &del_id)).await)
    })
    .await
    .unwrap_or_else(|e| Some(Err(e)));
    let Some(deleted) = deleted else {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::profile_in_use(format!(
                "profile '{id}' is active; deactivate or activate another profile first"
            )),
        );
    };
    match deleted {
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

/// FFA-b / FFA-j: the routes' wiring, with a directory or a path that stops
/// answering. The test hooks block an open or a resolution as a hung FUSE
/// mount does. Plain `#[tokio::test]` (current_thread): a read run on the
/// executor would block the only thread, so a route that lost its bound hangs
/// until the hook releases itself and fails its time assertion (rust.md).
#[cfg(test)]
mod wiring_tests {
    use super::*;
    use crate::api::server::UdsConnectInfo;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const SAMPLE: &str = r#"{"id":"%ID%","name":"%ID%","version":7,"controls":[],"curves":[]}"#;

    fn write_profile(dir: &std::path::Path, id: &str) {
        std::fs::write(dir.join(format!("{id}.json")), SAMPLE.replace("%ID%", id)).unwrap();
    }

    /// A hook that blocks until `release` sends (or a self-release deadline), and
    /// reports each entry on `entered`.
    fn wedge() -> (
        impl Fn() + Send + Sync + 'static,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    ) {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = parking_lot::Mutex::new(release_rx);
        let entered_tx = parking_lot::Mutex::new(entered_tx);
        let hook = move || {
            let _ = entered_tx.lock().send(());
            let _ = release_rx.lock().recv_timeout(Duration::from_secs(10));
        };
        (hook, release_tx, entered_rx)
    }

    fn test_state(dirs: Vec<PathBuf>, runtime_config_path: PathBuf) -> Arc<AppState> {
        let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
        Arc::new(AppState {
            cache: Arc::new(crate::health::cache::StateCache::new()),
            staleness_config: crate::health::staleness::StalenessConfig::default(),
            daemon_version: "0.0.0-test".into(),
            fan_controller: Arc::new(parking_lot::RwLock::new(None)),
            openfan_runtime: crate::api::handlers::OpenFanRuntime {
                timeout: Duration::from_millis(50),
                interval: Duration::from_millis(1000),
                shutdown: tokio::sync::watch::channel(false).1,
            },
            hwmon_controller: None,
            start_time: Instant::now(),
            history: Arc::new(crate::health::history::HistoryRing::new(10)),
            active_profile: Arc::new(parking_lot::Mutex::new(None)),
            openfan_calibration: Default::default(),
            characterization: Arc::new(parking_lot::Mutex::new(None)),
            validation: Arc::new(Default::default()),
            characterization_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            control_path: Arc::new(parking_lot::Mutex::new(None)),
            control_path_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stall_probe: Arc::new(parking_lot::Mutex::new(None)),
            stall_probe_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            control_paths: Arc::new(parking_lot::RwLock::new(Default::default())),
            pwm_baselines: Default::default(),
            pwm_verification: Default::default(),
            openfan_rescanning: Default::default(),
            last_openfan_rescan: Arc::new(parking_lot::Mutex::new(None)),
            adopted_poll_tasks: Arc::new(parking_lot::Mutex::new(Default::default())),
            openfan_maintenance: Default::default(),
            amd_gpus: Vec::new(),
            intel_gpus: Vec::new(),
            nvidia_gpus: Vec::new(),
            profile_search_dirs: parking_lot::RwLock::new(dirs),
            config_path: String::new(),
            runtime_config_path,
            sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
                std::collections::HashMap::new(),
            ))),
            cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
            override_table: Arc::new(parking_lot::Mutex::new(
                crate::control_override::OverrideTable::new(),
            )),
            allow_port_probe: false,
            running_config: Default::default(),
            readiness_rollup: readiness_rollup.clone(),
            config_write: Default::default(),
            runtime_config_degraded: Default::default(),
            assessment: Arc::new(crate::api::handlers::AssessmentCache::new(readiness_rollup)),
        })
    }

    fn peer(uid: u32) -> ConnectInfo<UdsConnectInfo> {
        ConnectInfo(UdsConnectInfo { uid: Some(uid) })
    }

    /// The bound the routes promise: the budget, plus slack for a loaded host.
    fn within_budget(started: Instant) {
        let waited = started.elapsed();
        assert!(
            waited < crate::constants::PROFILE_IO_BUDGET + Duration::from_millis(1500),
            "the route waited {waited:?}"
        );
    }

    #[tokio::test]
    async fn every_read_route_answers_while_a_registered_directory_hangs() {
        let store = tempfile::tempdir().unwrap();
        let hung = tempfile::tempdir().unwrap();
        write_profile(store.path(), "mine");
        write_profile(hung.path(), "theirs");
        let rc = tempfile::tempdir().unwrap();
        let state = test_state(
            vec![store.path().to_path_buf(), hung.path().to_path_buf()],
            rc.path().join("runtime.toml"),
        );
        let (hook, release, entered) = wedge();
        let _guard = crate::profile_store::test_hook::block_opens_of(hung.path(), hook);

        // The first list meets the hang and is answered at the budget.
        let started = Instant::now();
        let (status, Json(body)) = list_profiles_handler(State(Arc::clone(&state))).await;
        within_budget(started);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(body["error"]["retryable"], true);
        entered
            .recv_timeout(Duration::from_secs(1))
            .expect("the hang was hit");

        // Activation by a path in the hung directory is retryable, not refused.
        let (status, Json(body)) = activate_profile_handler(
            State(Arc::clone(&state)),
            peer(1000),
            Json(serde_json::json!({
                "profile_path": hung.path().join("theirs.json").display().to_string()
            })),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(body["error"]["retryable"], true);

        // Later requests skip the hung directory at once: one parked thread, not
        // one per request, and every route still answers from the others.
        let started = Instant::now();
        let (status, Json(body)) = list_profiles_handler(State(Arc::clone(&state))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profiles"][0]["id"], "mine");
        assert_eq!(body["profiles"].as_array().unwrap().len(), 1, "{body}");
        let (status, _) = get_profile_handler(State(Arc::clone(&state)), Path("mine".into())).await;
        assert_eq!(status, StatusCode::OK);
        let (status, Json(body)) = activate_profile_handler(
            State(Arc::clone(&state)),
            peer(1000),
            Json(serde_json::json!({ "profile_id": "mine" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a hung directory must be skipped, not waited on: {:?}",
            started.elapsed()
        );
        assert!(
            entered.try_recv().is_err(),
            "no later request may enter the hung directory"
        );
        let _ = release.send(());
    }

    #[tokio::test]
    async fn activation_by_path_resolution_is_bounded_and_per_user() {
        // A path whose directory is not registered as written is resolved — a
        // client-supplied path, in its own pool, one at a time per uid.
        let store = tempfile::tempdir().unwrap();
        write_profile(store.path(), "mine");
        let elsewhere = tempfile::tempdir().unwrap();
        let unregistered = elsewhere.path().join("x.json");
        let rc = tempfile::tempdir().unwrap();
        let state = test_state(vec![store.path().to_path_buf()], rc.path().join("rt.toml"));
        let (hook, release, entered) = wedge();
        let _guard = crate::api::handlers::path_confine::test_hook::block_resolution_of(
            elsewhere.path(),
            hook,
        );
        let body = serde_json::json!({ "profile_path": unregistered.display().to_string() });

        let started = Instant::now();
        let (status, _) =
            activate_profile_handler(State(Arc::clone(&state)), peer(4242), Json(body.clone()))
                .await;
        within_budget(started);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        entered
            .recv_timeout(Duration::from_secs(1))
            .expect("the hang was hit");

        // The same user is refused at once: its resolution is still hung.
        let started = Instant::now();
        let (status, _) =
            activate_profile_handler(State(Arc::clone(&state)), peer(4242), Json(body)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        assert!(entered.try_recv().is_err(), "no second thread may enter");

        // Another user still resolves: the gate is per uid. Its directory is
        // not registered, so a resolution that ran answers 400, not 500.
        let other = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let (status, Json(body)) = activate_profile_handler(
            State(Arc::clone(&state)),
            peer(4343),
            Json(serde_json::json!({
                "profile_path": other.path().join("x.json").display().to_string()
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );

        // Anyone naming a registered directory as written needs no resolution.
        let path = store.path().join("mine.json").display().to_string();
        let (status, Json(body)) = activate_profile_handler(
            State(Arc::clone(&state)),
            peer(4242),
            Json(serde_json::json!({ "profile_path": path })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let _ = release.send(());
    }

    #[tokio::test]
    async fn root_can_remove_a_directory_that_stopped_answering() {
        // Concurrency review finding 1: removing a hung directory is the API's
        // only remedy for one, so it must not touch that directory.
        let store = tempfile::tempdir().unwrap();
        let hung = tempfile::tempdir().unwrap();
        let rc = tempfile::tempdir().unwrap();
        let state = test_state(
            vec![store.path().to_path_buf(), hung.path().to_path_buf()],
            rc.path().join("runtime.toml"),
        );
        let (hook, release, entered) = wedge();
        let _resolve =
            crate::api::handlers::path_confine::test_hook::block_resolution_of(hung.path(), hook);

        let started = Instant::now();
        let (status, Json(body)) = crate::api::handlers::update_profile_search_dirs_handler(
            State(Arc::clone(&state)),
            peer(0),
            Json(serde_json::json!({ "remove": [hung.path().display().to_string()] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        assert!(
            entered.try_recv().is_err(),
            "the removal must not resolve the directory"
        );
        assert_eq!(
            body["search_dirs"],
            serde_json::json!([store.path().display().to_string()])
        );
        let _ = release.send(());
    }

    #[tokio::test]
    async fn a_hung_add_refuses_only_that_users_later_edits() {
        let store = tempfile::tempdir().unwrap();
        let hung = tempfile::tempdir().unwrap();
        let rc = tempfile::tempdir().unwrap();
        let state = test_state(
            vec![store.path().to_path_buf()],
            rc.path().join("runtime.toml"),
        );
        let (hook, release, entered) = wedge();
        let _guard =
            crate::api::handlers::path_confine::test_hook::block_resolution_of(hung.path(), hook);
        let add = serde_json::json!({ "add": [hung.path().display().to_string()] });

        // The test's own uid: it has a home to look up, and the per-uid gate is
        // process-wide, so a uid other tests use (root) must not be the hung one.
        // SAFETY: getuid has no preconditions and cannot fail.
        let me = unsafe { libc::getuid() };
        let started = Instant::now();
        let (status, _) = crate::api::handlers::update_profile_search_dirs_handler(
            State(Arc::clone(&state)),
            peer(me),
            Json(add.clone()),
        )
        .await;
        within_budget(started);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        entered
            .recv_timeout(Duration::from_secs(1))
            .expect("the hang was hit");

        let started = Instant::now();
        let (status, _) = crate::api::handlers::update_profile_search_dirs_handler(
            State(Arc::clone(&state)),
            peer(me),
            Json(add),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        assert!(entered.try_recv().is_err(), "no second thread may enter");
        let _ = release.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_store_profile_deleted_after_the_read_is_not_activated() {
        // FFA-j: the read runs outside the store lock, so a delete can win the
        // race to it; the existence check under the lock must see that, or the
        // deleted profile is activated and saved as active with no file.
        let store = tempfile::tempdir().unwrap();
        write_profile(store.path(), "p");
        let rc = tempfile::tempdir().unwrap();
        let state = test_state(vec![store.path().to_path_buf()], rc.path().join("rt.toml"));
        let (signal, opened) = std::sync::mpsc::channel::<()>();
        let signal = parking_lot::Mutex::new(signal);
        let _guard = crate::profile_store::test_hook::block_opens_of(store.path(), move || {
            let _ = signal.lock().send(());
        });

        let held = crate::profile_store::STORE_LOCK.lock().await;
        let activation = tokio::spawn(activate_profile_handler(
            State(Arc::clone(&state)),
            peer(0),
            Json(serde_json::json!({ "profile_id": "p" })),
        ));
        opened
            .recv_timeout(Duration::from_secs(5))
            .expect("the read started");
        // The read is over once it releases the directory; only then is the
        // file removed, so the read found it and the check under the lock this
        // test holds is what must refuse it.
        let give_up = Instant::now() + Duration::from_secs(5);
        while crate::profile_store::dir_in_use(store.path()) {
            assert!(Instant::now() < give_up, "the read never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::remove_file(store.path().join("p.json")).unwrap();
        drop(held);

        let (status, Json(body)) = activation.await.unwrap();
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(state.active_profile.lock().is_none());
    }

    #[tokio::test]
    async fn a_user_with_a_slow_mount_holds_one_resolution_slot_at_most() {
        // Concurrency review of FFA-b: a mount that answers each lookup just
        // under the hung threshold, flooded by its owner, must not fill the
        // resolution pool and refuse everyone else.
        let store = tempfile::tempdir().unwrap();
        let slow = tempfile::tempdir().unwrap();
        let rc = tempfile::tempdir().unwrap();
        let state = test_state(vec![store.path().to_path_buf()], rc.path().join("rt.toml"));
        let _guard =
            crate::api::handlers::path_confine::test_hook::block_resolution_of(slow.path(), || {
                std::thread::sleep(Duration::from_millis(900));
            });
        let flood: Vec<_> = (0..crate::constants::PATH_RESOLVE_MAX_OUTSTANDING + 2)
            .map(|_| {
                tokio::spawn(activate_profile_handler(
                    State(Arc::clone(&state)),
                    peer(5151),
                    Json(serde_json::json!({
                        "profile_path": slow.path().join("x.json").display().to_string()
                    })),
                ))
            })
            .collect();
        // Let every flood request reach its slot (or its refusal).
        tokio::time::sleep(Duration::from_millis(100)).await;

        let other = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let (status, Json(body)) = activate_profile_handler(
            State(Arc::clone(&state)),
            peer(5252),
            Json(serde_json::json!({
                "profile_path": other.path().join("x.json").display().to_string()
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "another user must not wait behind the flood: {:?}",
            started.elapsed()
        );
        for request in flood {
            let _ = request.await;
        }
    }

    #[tokio::test]
    async fn slow_directories_do_not_run_a_listing_past_its_budget() {
        // Concurrency review of FFA-b: directories that are slow but never hung
        // are not skipped, so the walk itself must stop in time — and with what
        // it found, not a 500 for every caller.
        let store = tempfile::tempdir().unwrap();
        write_profile(store.path(), "mine");
        let slow: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let mut dirs = vec![store.path().to_path_buf()];
        let mut guards = Vec::new();
        for (i, dir) in slow.iter().enumerate() {
            write_profile(dir.path(), &format!("slow{i}"));
            dirs.push(dir.path().to_path_buf());
            guards.push(crate::profile_store::test_hook::block_opens_of(
                dir.path(),
                || std::thread::sleep(Duration::from_millis(800)),
            ));
        }
        let rc = tempfile::tempdir().unwrap();
        let state = test_state(dirs, rc.path().join("rt.toml"));

        for _ in 0..2 {
            let started = Instant::now();
            let (status, Json(body)) = list_profiles_handler(State(Arc::clone(&state))).await;
            assert!(
                started.elapsed() < crate::constants::PROFILE_IO_BUDGET,
                "{:?}",
                started.elapsed()
            );
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["profiles"][0]["id"], "mine");
        }
    }
}
