//! Manual-override + fan-identify endpoints (DEC-163 / DEC-166).
//!
//! Daemon-owned, expiring, fencing-guarded control intent that replaces the
//! GUI's two in-process transient manual mechanisms (the per-control Manual
//! card and the wizard's per-fan stop/restore). These handlers only mutate the
//! shared `OverrideTable`; the profile engine tick sweeps it on the daemon's
//! own clock and applies the overlay.
//!
//! They deliberately do **not** call `record_gui_write()` — an override is
//! daemon-applied intent, not a GUI direct write, so it must not mark the
//! engine `gui_active` (which would make the write backends defer).

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
    // The control must exist in the active profile — nothing to override
    // otherwise. (Matches the GUI's per-control Manual card, which only appears
    // for controls in the active profile.)
    let exists = state
        .active_profile
        .lock()
        .as_ref()
        .is_some_and(|p| p.controls.iter().any(|c| c.id == control_id));
    if !exists {
        return error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::validation(format!("no control '{control_id}' in the active profile")),
        );
    }

    let ttl = resolve_ttl(body.ttl_secs);
    let grant = state
        .override_table
        .lock()
        .take_override(&control_id, body.pwm_percent, ttl);

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
            // be able to clear a stop.)
            let known = {
                let snap = state.cache.snapshot();
                build_fan_entries(&snap, Instant::now())
                    .iter()
                    .any(|f| f.id == fan_id)
            };
            if !known {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!("unknown fan '{fan_id}'")),
                );
            }
            let ttl = resolve_ttl(body.ttl_secs);
            state.override_table.lock().identify_stop(&fan_id, ttl);
            json_ok(
                StatusCode::OK,
                IdentifyResponse {
                    api_version: API_VERSION,
                    fan_id,
                    action: "stop".into(),
                    expires_in_secs: Some(ttl.as_secs()),
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
