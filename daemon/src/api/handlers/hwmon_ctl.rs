//! Hwmon PWM and lease endpoints.
//!
//! Named `hwmon_ctl` to avoid confusion with the top-level `crate::hwmon` module.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::hwmon::lease::LeaseError;
use crate::hwmon::pwm_control::HwmonControlError;

/// GET /hwmon/headers — list discovered controllable PWM headers.
pub async fn hwmon_headers_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return json_ok(
            StatusCode::OK,
            PwmHeadersResponse {
                api_version: API_VERSION,
                headers: vec![],
            },
        );
    };

    let ctrl = controller.lock();

    let headers = ctrl
        .headers()
        .iter()
        .map(|h| PwmHeaderEntry {
            id: h.id.clone(),
            label: h.label.clone(),
            chip_name: h.chip_name.clone(),
            device_id: h.device_id.clone(),
            pwm_index: h.pwm_index,
            supports_enable: h.supports_enable,
            rpm_available: h.rpm_available,
            min_pwm_percent: h.min_pwm_percent,
            max_pwm_percent: h.max_pwm_percent,
            is_writable: h.is_writable,
            pwm_mode: h.pwm_mode,
        })
        .collect();

    json_ok(
        StatusCode::OK,
        PwmHeadersResponse {
            api_version: API_VERSION,
            headers,
        },
    )
}

/// POST /hwmon/lease/take — acquire the exclusive hwmon write lease.
pub async fn hwmon_lease_take_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TakeLeaseRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let mut ctrl = controller.lock();

    // GUI always preempts internal holders (profile engine, thermal safety).
    // Use force_take to evict any current holder.
    let lease = ctrl.lease_manager_mut().force_take_lease(&body.owner_hint);
    json_ok(
        StatusCode::OK,
        LeaseResponse {
            api_version: API_VERSION,
            lease_id: lease.lease_id.clone(),
            owner_hint: lease.owner_hint.clone(),
            ttl_seconds: lease.ttl_seconds(),
        },
    )
}

/// POST /hwmon/lease/release — release the hwmon write lease.
pub async fn hwmon_lease_release_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ReleaseLeaseRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.lease_manager_mut().release_lease(&body.lease_id) {
        Ok(()) => {
            ctrl.on_lease_released();
            json_ok(
                StatusCode::OK,
                LeaseReleasedResponse {
                    api_version: API_VERSION,
                    released: true,
                },
            )
        }
        Err(LeaseError::InvalidLease) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error("invalid or expired lease"),
        ),
        Err(LeaseError::NoLease) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error("no active lease to release"),
        ),
        Err(e) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error(e.to_string()),
        ),
    }
}

/// GET /hwmon/lease/status — lease status for UI display.
pub async fn hwmon_lease_status_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        let resp = LeaseStatusResponse {
            api_version: API_VERSION,
            lease_required: true,
            held: false,
            lease_id: None,
            ttl_seconds_remaining: None,
            owner_hint: None,
        };
        return json_ok(StatusCode::OK, resp);
    };

    let ctrl = controller.lock();

    let resp = match ctrl.lease_manager().active_lease() {
        Some(lease) => LeaseStatusResponse {
            api_version: API_VERSION,
            lease_required: true,
            held: true,
            lease_id: Some(lease.lease_id.clone()),
            ttl_seconds_remaining: Some(lease.ttl_seconds()),
            owner_hint: Some(lease.owner_hint.clone()),
        },
        None => LeaseStatusResponse {
            api_version: API_VERSION,
            lease_required: true,
            held: false,
            lease_id: None,
            ttl_seconds_remaining: None,
            owner_hint: None,
        },
    };

    json_ok(StatusCode::OK, resp)
}

/// POST /hwmon/lease/renew — extend the TTL of the current lease.
pub async fn hwmon_lease_renew_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RenewLeaseRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.lease_manager_mut().renew_lease(&body.lease_id) {
        Ok(lease) => json_ok(
            StatusCode::OK,
            LeaseResponse {
                api_version: API_VERSION,
                lease_id: lease.lease_id.clone(),
                owner_hint: lease.owner_hint.clone(),
                ttl_seconds: lease.ttl_seconds(),
            },
        ),
        Err(LeaseError::InvalidLease) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error("invalid or expired lease"),
        ),
        Err(e) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error(e.to_string()),
        ),
    }
}

/// POST /hwmon/{header_id}/pwm — set PWM on an hwmon header (requires lease).
pub async fn hwmon_set_pwm_handler(
    State(state): State<Arc<AppState>>,
    Path(header_id): Path<String>,
    Json(body): Json<HwmonSetPwmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.set_pwm(&header_id, body.pwm_percent, &body.lease_id) {
        Ok(result) => {
            state.cache.record_gui_write();
            json_ok(
                StatusCode::OK,
                HwmonSetPwmResponse {
                    api_version: API_VERSION,
                    header_id: result.header_id,
                    pwm_percent: result.pwm_percent,
                    raw_value: result.raw_value,
                },
            )
        }
        Err(e) => hwmon_control_error_response(e),
    }
}

/// Map a `HwmonControlError` to an HTTP error response.
fn hwmon_control_error_response(err: HwmonControlError) -> (StatusCode, Json<serde_json::Value>) {
    match err {
        HwmonControlError::Lease(LeaseError::AlreadyHeld {
            owner_hint,
            ttl_seconds,
        }) => error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::lease_already_held(format!(
                "lease held by '{owner_hint}' (expires in {ttl_seconds}s)"
            )),
        ),
        HwmonControlError::Lease(_) => error_response(
            StatusCode::FORBIDDEN,
            &ErrorEnvelope::lease_error("valid lease required for hwmon PWM writes"),
        ),
        HwmonControlError::Validation(msg) => {
            error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(msg))
        }
        HwmonControlError::Hardware(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(err.to_string()),
        ),
    }
}

/// POST /hwmon/rescan — re-enumerate hwmon devices and return fresh header list.
///
/// Does not replace the running controller — returns discovery results for the
/// GUI to refresh its view. A daemon restart is needed to pick up truly new hardware.
pub async fn hwmon_rescan_handler(
    State(_state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::hwmon::pwm_discovery::discover_pwm_headers;
    use crate::hwmon::HWMON_SYSFS_ROOT;

    let hwmon_root = std::path::Path::new(HWMON_SYSFS_ROOT);
    match discover_pwm_headers(hwmon_root) {
        Ok(headers) => {
            let entries: Vec<PwmHeaderEntry> = headers
                .iter()
                .map(|h| PwmHeaderEntry {
                    id: h.id.clone(),
                    label: h.label.clone(),
                    chip_name: h.chip_name.clone(),
                    device_id: h.device_id.clone(),
                    pwm_index: h.pwm_index,
                    supports_enable: h.supports_enable,
                    rpm_available: h.rpm_available,
                    min_pwm_percent: h.min_pwm_percent,
                    max_pwm_percent: h.max_pwm_percent,
                    is_writable: h.is_writable,
                    pwm_mode: h.pwm_mode,
                })
                .collect();
            log::info!("Hwmon rescan: found {} PWM header(s)", entries.len());
            let count = entries.len();
            json_ok(
                StatusCode::OK,
                serde_json::json!({
                    "api_version": API_VERSION,
                    "headers": entries,
                    "count": count,
                }),
            )
        }
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("hwmon rescan failed: {e}")),
        ),
    }
}

const VERIFY_WAIT_SECONDS: u8 = 3;

/// POST /hwmon/{header_id}/verify — behavioural test of PWM write effectiveness.
///
/// Writes a test PWM value, waits for hardware to respond, then reads back
/// pwm_enable, PWM value, and RPM to classify the result. Requires a valid
/// hwmon lease. Takes ~3 seconds.
pub async fn hwmon_verify_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(header_id): axum::extract::Path<String>,
    Json(body): Json<HwmonVerifyRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let controller = match &state.hwmon_controller {
        Some(c) => c,
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorEnvelope::validation("hwmon controller not available"),
            )
        }
    };

    // Validate lease and extract header paths
    let (pwm_path, enable_path, rpm_path) = {
        let ctrl = controller.lock();
        if let Err(e) = ctrl.lease_manager().validate_lease(&body.lease_id) {
            return error_response(
                StatusCode::FORBIDDEN,
                &ErrorEnvelope::lease_error(e.to_string()),
            );
        }
        match ctrl.headers().into_iter().find(|h| h.id == header_id) {
            Some(h) => (
                h.pwm_path.clone(),
                h.enable_path.clone(),
                h.rpm_path.clone(),
            ),
            None => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!("unknown header: {header_id}")),
                )
            }
        }
    };

    let read_state = |pwm: &str, en: &Option<String>, rpm: &Option<String>| -> HwmonVerifyState {
        let pwm_raw = std::fs::read_to_string(pwm)
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok());
        let pwm_enable = en.as_ref().and_then(|p| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
        });
        let rpm_val = rpm.as_ref().and_then(|p| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok())
        });
        HwmonVerifyState {
            pwm_enable,
            pwm_raw,
            pwm_percent: pwm_raw.map(crate::pwm::raw_to_percent),
            rpm: rpm_val,
        }
    };

    // Read initial state
    let initial = read_state(&pwm_path, &enable_path, &rpm_path);

    // Calculate test PWM: ensure a significant delta from current
    let current_pct = initial.pwm_percent.unwrap_or(50);
    let test_pct: u8 = if current_pct > 50 { 20 } else { 80 };

    // Write test value via controller (sets pwm_enable=1 + PWM)
    {
        let mut ctrl = controller.lock();
        if let Err(e) = ctrl.set_pwm(&header_id, test_pct, &body.lease_id) {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal(format!("test write failed: {e}")),
            );
        }
    }

    // Wait for hardware to respond
    tokio::time::sleep(std::time::Duration::from_secs(VERIFY_WAIT_SECONDS as u64)).await;

    // Read back state after wait
    let final_state = read_state(&pwm_path, &enable_path, &rpm_path);

    // Restore original PWM
    {
        let mut ctrl = controller.lock();
        let _ = ctrl.set_pwm(&header_id, current_pct, &body.lease_id);
    }

    // Classify result
    let (result, details) = classify_verify_result(&initial, &final_state, test_pct);

    json_ok(
        StatusCode::OK,
        HwmonVerifyResponse {
            header_id,
            result,
            initial_state: initial,
            final_state,
            test_pwm_percent: test_pct,
            wait_seconds: VERIFY_WAIT_SECONDS,
            details,
        },
    )
}

fn classify_verify_result(
    initial: &HwmonVerifyState,
    final_state: &HwmonVerifyState,
    test_pct: u8,
) -> (String, String) {
    // Check if pwm_enable was reclaimed
    if let Some(final_enable) = final_state.pwm_enable {
        if final_enable != 1 {
            return (
                "pwm_enable_reverted".into(),
                format!(
                    "pwm_enable changed from 1 to {final_enable} after write — \
                     BIOS/EC is actively reclaiming fan control"
                ),
            );
        }
    }

    // Check if PWM value was clamped/overridden
    let test_raw = crate::pwm::percent_to_raw(test_pct);
    if let Some(final_raw) = final_state.pwm_raw {
        let delta = (final_raw as i16 - test_raw as i16).unsigned_abs();
        if delta > 10 {
            return (
                "pwm_value_clamped".into(),
                format!(
                    "PWM value changed from test {test_raw} to {final_raw} — \
                     BIOS/EC is overriding the PWM register"
                ),
            );
        }
    }

    // Check RPM change (if available)
    match (initial.rpm, final_state.rpm) {
        (Some(init_rpm), Some(final_rpm)) if init_rpm > 100 => {
            let expected_decrease = test_pct < initial.pwm_percent.unwrap_or(50);
            let rpm_changed = if expected_decrease {
                final_rpm < init_rpm.saturating_sub(init_rpm / 5)
            } else {
                final_rpm > init_rpm + init_rpm / 5
            };
            if !rpm_changed {
                return (
                    "no_rpm_effect".into(),
                    format!(
                        "RPM unchanged ({init_rpm} \u{2192} {final_rpm}) despite PWM change — \
                         PWM writes may be accepted but have no hardware effect"
                    ),
                );
            }
            (
                "effective".into(),
                format!("PWM control verified: RPM changed {init_rpm} \u{2192} {final_rpm}"),
            )
        }
        _ => (
            "rpm_unavailable".into(),
            "PWM values held but RPM sensor unavailable or too low to verify".into(),
        ),
    }
}
