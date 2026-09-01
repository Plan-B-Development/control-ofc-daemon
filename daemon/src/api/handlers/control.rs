//! Manual-override + fan-identify endpoints (DEC-163 / DEC-166).
//!
//! Daemon-owned, expiring, fencing-guarded control intent that replaces the
//! GUI's two in-process transient manual mechanisms (the per-control Manual
//! card and the wizard's per-fan stop/restore). These handlers only mutate the
//! shared `OverrideTable`; the profile engine tick sweeps it on the daemon's
//! own clock and applies the overlay.
//!
//! An override is daemon-applied intent — the engine reads the `OverrideTable`
//! and applies the overlay each tick. The GUI never writes fans (DEC-165), so
//! these handlers only mutate shared state; there is no direct hardware write.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{build_fan_entries, error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::constants;
use crate::control_override::OverrideReject;

/// Clamp a requested TTL to `[1, OVERRIDE_TTL_SECS]`, defaulting to the full
/// window. Capping the per-grant TTL keeps the deadman meaningful — a client
/// extends an override by renewing, never by requesting one long grant.
fn resolve_ttl(requested: Option<u64>) -> Duration {
    let secs = requested
        .unwrap_or(constants::OVERRIDE_TTL_SECS)
        .clamp(1, constants::OVERRIDE_TTL_SECS);
    Duration::from_secs(secs)
}

/// POST /control/{control_id}/override — pin a control's members to a fixed PWM.
pub async fn override_take_handler(
    State(state): State<Arc<AppState>>,
    Path(control_id): Path<String>,
    Json(body): Json<OverrideTakeRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.pwm_percent > 100 {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!(
                "pwm_percent must be 0..=100, got {}",
                body.pwm_percent
            )),
        );
    }
    let ttl = resolve_ttl(body.ttl_secs);
    // The control must exist in the active profile — nothing to override
    // otherwise. (Matches the GUI's per-control Manual card, which only appears
    // for controls in the active profile.)
    //
    // DEC-189: hold `active_profile` across BOTH the existence check and the
    // override insert. Releasing it between the two (as the original code did)
    // left a window where a concurrent `POST /profile/activate` could swap the
    // profile after the check passed, pinning an override against a control
    // that is not in the now-active profile (TOCTOU). With the lock held, an
    // activation serialises strictly before (the check sees the new profile) or
    // strictly after (the activation's `clear_all_overrides` wipes this insert).
    // Lock order `active_profile` (outer) → `override_table` (inner) matches the
    // activate handler; the engine takes `override_table` alone, so there is no
    // inversion. No `.await` is held across the guard (a parking_lot guard
    // across `.await` would not compile — the handler future must be `Send`).
    let grant = {
        let profile_guard = state.active_profile.lock();
        let exists = profile_guard
            .as_ref()
            .is_some_and(|p| p.controls.iter().any(|c| c.id == control_id));
        if !exists {
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorEnvelope::validation(format!(
                    "no control '{control_id}' in the active profile"
                )),
            );
        }
        state
            .override_table
            .lock()
            .take_override(&control_id, body.pwm_percent, ttl)
    };

    json_ok(
        StatusCode::OK,
        OverrideGrantResponse {
            api_version: API_VERSION,
            control_id,
            override_token: grant.token,
            pwm_percent: body.pwm_percent,
            ttl_secs: grant.ttl_secs,
            renew_secs: constants::OVERRIDE_RENEW_SECS,
            expires_in_secs: grant.expires_in_secs,
        },
    )
}

/// POST /control/{control_id}/override/renew — extend the deadman (fresh full TTL).
pub async fn override_renew_handler(
    State(state): State<Arc<AppState>>,
    Path(control_id): Path<String>,
    Json(body): Json<OverrideTokenRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let ttl = resolve_ttl(None);
    let result = state
        .override_table
        .lock()
        .renew_override(&control_id, body.override_token, ttl);
    match result {
        Ok(grant) => json_ok(
            StatusCode::OK,
            OverrideRenewResponse {
                api_version: API_VERSION,
                control_id,
                override_token: grant.token,
                ttl_secs: grant.ttl_secs,
                expires_in_secs: grant.expires_in_secs,
            },
        ),
        Err(reject) => override_reject_response(reject),
    }
}

/// DELETE /control/{control_id}/override — revert to curve immediately.
pub async fn override_release_handler(
    State(state): State<Arc<AppState>>,
    Path(control_id): Path<String>,
    Json(body): Json<OverrideTokenRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = state
        .override_table
        .lock()
        .release_override(&control_id, body.override_token);
    match result {
        Ok(()) => json_ok(
            StatusCode::OK,
            OverrideReleaseResponse {
                api_version: API_VERSION,
                control_id,
                released: true,
            },
        ),
        // Idempotent: an already-gone override is a successful no-op release.
        Err(OverrideReject::NotActive) => json_ok(
            StatusCode::OK,
            OverrideReleaseResponse {
                api_version: API_VERSION,
                control_id,
                released: false,
            },
        ),
        Err(reject) => override_reject_response(reject),
    }
}

/// POST /fans/{fan_id}/identify — stop/restore one fan (deadman auto-restore).
pub async fn fan_identify_handler(
    State(state): State<Arc<AppState>>,
    Path(fan_id): Path<String>,
    Json(body): Json<IdentifyRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match body.action.as_str() {
        "stop" => {
            // The fan must exist. (Restore stays lenient below — you must always
            // be able to clear a hold.)
            //
            // The baseline duty is read from the same snapshot, so "does this fan
            // exist?" and "what is it running at?" cannot disagree.
            let (known, last_commanded) = {
                let snap = state.cache.snapshot();
                let entry = build_fan_entries(&snap, Instant::now())
                    .into_iter()
                    .find(|f| f.id == fan_id);
                (entry.is_some(), entry.and_then(|f| f.last_commanded_pwm))
            };
            if !known {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!("unknown fan '{fan_id}'")),
                );
            }

            // [SAFETY] DEC-311. The CLIENT asks to identify a fan; the DAEMON
            // decides what that means from the header's role. A pump is
            // perturbed, never stopped — and because the decision is made here
            // rather than in the request, a GUI predating this daemon (which can
            // only ever send `action: "stop"`) gets the safe behaviour without
            // knowing the feature exists. This supersedes DEC-166's floor-exempt
            // "you must be able to stop a pump to find it".
            // The UNION predicate, not the resolved role. A user assignment can
            // add pump protection but must never strip protection the header's
            // own label or chip already earned — otherwise
            // `POST /config/header-role {"role":"chassis_fan"}` on an `AIO_PUMP`
            // header would hand identify permission to stop a real pump, while
            // the floor path (which already unions) went on treating it as one.
            let role = if state.header_is_pump_protected(&fan_id) {
                crate::hwmon::roles::HeaderRole::Pump
            } else {
                state.resolved_header_role(&fan_id)
            };
            let (target_pct, mode) =
                crate::control_override::identify_target_for_role(role, last_commanded);

            let ttl = resolve_ttl(body.ttl_secs);
            state
                .override_table
                .lock()
                .identify_hold(&fan_id, target_pct, mode, ttl);
            log::info!(
                "Fan identify: {fan_id} held at {target_pct}% ({}) for {}s (role: {})",
                mode.as_str(),
                ttl.as_secs(),
                role.as_str()
            );
            json_ok(
                StatusCode::OK,
                IdentifyResponse {
                    api_version: API_VERSION,
                    fan_id,
                    action: "stop".into(),
                    expires_in_secs: Some(ttl.as_secs()),
                    mode: Some(mode.as_str().into()),
                    identify_pwm_percent: Some(target_pct),
                    baseline_pwm_percent: last_commanded,
                },
            )
        }
        "restore" => {
            state.override_table.lock().identify_restore(&fan_id);
            json_ok(
                StatusCode::OK,
                IdentifyResponse {
                    api_version: API_VERSION,
                    fan_id,
                    action: "restore".into(),
                    expires_in_secs: None,
                    mode: None,
                    identify_pwm_percent: None,
                    baseline_pwm_percent: None,
                },
            )
        }
        other => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!(
                "action must be 'stop' or 'restore', got '{other}'"
            )),
        ),
    }
}

/// Map an [`OverrideReject`] to its HTTP error envelope (DEC-163).
fn override_reject_response(reject: OverrideReject) -> (StatusCode, Json<serde_json::Value>) {
    match reject {
        OverrideReject::StaleToken => error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::stale_fencing_token(
                "override token is stale or superseded — re-take to regain control",
            ),
        ),
        OverrideReject::NotActive => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::override_expired(
                "no active override for this control — it expired or was never taken",
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve_ttl` is the deadman cap on a per-grant override TTL: a client
    /// extends control by renewing, never by requesting one long grant. Pin both
    /// clamp ends, the default, and an in-range passthrough so removing or
    /// loosening the bound is caught — the integration roundtrip
    /// (`ipc_integration.rs`) only exercises the default.
    #[test]
    fn resolve_ttl_clamps_both_ends_and_defaults() {
        let max = constants::OVERRIDE_TTL_SECS;
        // No request → the full default window.
        assert_eq!(resolve_ttl(None), Duration::from_secs(max));
        // Above the cap → clamped down to the cap.
        assert_eq!(resolve_ttl(Some(999)), Duration::from_secs(max));
        // Exactly at the cap → unchanged (upper boundary).
        assert_eq!(resolve_ttl(Some(max)), Duration::from_secs(max));
        // Zero → floored to 1s (a 0s deadman would expire instantly).
        assert_eq!(resolve_ttl(Some(0)), Duration::from_secs(1));
        // In range → passed through unchanged.
        assert_eq!(resolve_ttl(Some(10)), Duration::from_secs(10));
    }
}
