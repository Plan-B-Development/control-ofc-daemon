//! OpenFan serial write endpoints: set PWM, set RPM, calibrate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::serial::controller::FanControlError;

/// POST /fans/openfan/{channel}/pwm — set PWM on a single channel.
///
/// DEC-099: dispatched on `spawn_blocking` so the serial-write critical
/// section (parking_lot mutex + USB-CDC turnaround, up to ~500ms) does not
/// pin a tokio worker thread. The polling loop already follows this rule;
/// matching it here lets the runtime keep accepting requests while the
/// FanController lock is held by another task.
pub async fn set_pwm_handler(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<u8>,
    Json(body): Json<SetPwmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(controller) = state.fan_controller.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    };

    let pwm_percent = body.pwm_percent;
    let result =
        tokio::task::spawn_blocking(move || controller.lock().set_pwm(channel, pwm_percent)).await;

    match result {
        Ok(Ok(set_result)) => {
            state.cache.record_gui_write();
            json_ok(
                StatusCode::OK,
                SetPwmResponse {
                    api_version: API_VERSION,
                    channel: set_result.channel,
                    pwm_percent: set_result.pwm_percent,
                    coalesced: set_result.coalesced,
                },
            )
        }
        Ok(Err(e)) => fan_control_error_response(e),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("openfan write task failed: {e}")),
        ),
    }
}

/// POST /fans/openfan/pwm — set PWM on all channels.
///
/// DEC-099: dispatched on `spawn_blocking` (see `set_pwm_handler`).
pub async fn set_pwm_all_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SetPwmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(controller) = state.fan_controller.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    };

    let pwm_percent = body.pwm_percent;
    let result =
        tokio::task::spawn_blocking(move || controller.lock().set_pwm_all(pwm_percent)).await;

    match result {
        Ok(Ok(set_result)) => {
            state.cache.record_gui_write();
            json_ok(
                StatusCode::OK,
                SetPwmAllResponse {
                    api_version: API_VERSION,
                    pwm_percent: set_result.pwm_percent,
                    channels_affected: set_result.channels_affected,
                },
            )
        }
        Ok(Err(e)) => fan_control_error_response(e),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("openfan write-all task failed: {e}")),
        ),
    }
}

/// POST /fans/openfan/{channel}/target_rpm — set target RPM on a single channel.
///
/// DEC-099: dispatched on `spawn_blocking` (see `set_pwm_handler`).
pub async fn set_target_rpm_handler(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<u8>,
    Json(body): Json<SetRpmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(controller) = state.fan_controller.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    };

    let target_rpm = body.target_rpm;
    let result =
        tokio::task::spawn_blocking(move || controller.lock().set_target_rpm(channel, target_rpm))
            .await;

    match result {
        Ok(Ok(set_result)) => {
            state.cache.record_gui_write();
            json_ok(
                StatusCode::OK,
                SetRpmResponse {
                    api_version: API_VERSION,
                    channel: set_result.channel,
                    target_rpm: set_result.target_rpm,
                },
            )
        }
        Ok(Err(e)) => fan_control_error_response(e),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("openfan target_rpm task failed: {e}")),
        ),
    }
}

/// Map a `FanControlError` to an HTTP error response.
fn fan_control_error_response(err: FanControlError) -> (StatusCode, Json<serde_json::Value>) {
    match err {
        FanControlError::Validation(msg) => {
            error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(msg))
        }
        FanControlError::Serial(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(err.to_string()),
        ),
    }
}

/// RAII guard that resets the calibrating flag on drop, ensuring cleanup
/// even on early return or panic.
struct CalibrationGuard<'a> {
    flag: &'a AtomicBool,
}
impl Drop for CalibrationGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// POST /fans/openfan/{channel}/calibrate — run a PWM-to-RPM calibration sweep.
pub async fn calibrate_openfan_handler(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<u8>,
    Json(body): Json<crate::api::calibration::CalibrationRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if state.fan_controller.is_none() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    }

    if channel > 9 {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!("invalid channel: {channel}")),
        );
    }

    // Prevent concurrent calibration sweeps
    if state
        .calibrating
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation("calibration already in progress"),
        );
    }

    // Drop guard resets `calibrating` to false on any exit path (early return, panic, success)
    let _guard = CalibrationGuard {
        flag: &state.calibrating,
    };

    let steps = body.steps.clamp(2, 20);
    if steps != body.steps {
        log::info!(
            "Calibration API: steps clamped from {} to {steps}",
            body.steps
        );
    }
    let clamped_hold = body.hold_seconds.clamp(2, 15);
    if clamped_hold != body.hold_seconds {
        log::info!(
            "Calibration API: hold_seconds clamped from {} to {clamped_hold}",
            body.hold_seconds
        );
    }
    let hold = std::time::Duration::from_secs(clamped_hold);
    let cache = state.cache.clone();
    let fan_id = format!("openfan:ch{channel:02}");

    // Read pre-calibration PWM
    let pre_cal_pwm = {
        let snap = cache.snapshot();
        snap.openfan_fans
            .get(&channel)
            .and_then(|f| f.last_commanded_pwm)
    };

    let step_size = 100.0 / steps as f64;
    let mut points = Vec::with_capacity(steps as usize + 1);

    for i in 0..=steps {
        let pwm = (i as f64 * step_size).round().min(100.0) as u8;

        // Thermal check
        if let Err(e) = crate::api::calibration::check_thermal_safety(&cache) {
            // Restore pre-cal PWM before returning
            if let (Some(restore), Some(ref ctrl)) = (pre_cal_pwm, &state.fan_controller) {
                {
                    let mut guard = ctrl.lock(); // parking_lot — always succeeds
                    if let Err(e) = guard.set_pwm(channel, restore) {
                        log::warn!("failed to restore pre-calibration PWM on ch{channel}: {e}");
                    }
                }
            }
            return match e {
                crate::api::calibration::CalibrationError::ThermalAbort {
                    sensor_id,
                    temp_c,
                    ..
                } => error_response(
                    StatusCode::CONFLICT,
                    &ErrorEnvelope {
                        error: ErrorBody {
                            code: "thermal_abort".into(),
                            message: format!("Thermal abort: {sensor_id} at {temp_c:.1}\u{00B0}C"),
                            retryable: true,
                            source: "hardware".into(),
                            details: None,
                        },
                    },
                ),
                _ => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorEnvelope::internal("calibration check failed"),
                ),
            };
        }

        // Set PWM via controller
        {
            let Some(ref ctrl) = state.fan_controller else {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &ErrorEnvelope::hardware_unavailable(
                        "OpenFanController disconnected during calibration",
                    ),
                );
            };
            let mut guard = ctrl.lock();
            if let Err(e) = guard.set_pwm(channel, pwm) {
                return fan_control_error_response(e);
            }
        }

        tokio::time::sleep(hold).await;

        // Read RPM from cache
        let snap = cache.snapshot();
        let rpm = snap.openfan_fans.get(&channel).map(|f| f.rpm).unwrap_or(0);

        points.push(crate::api::calibration::CalPoint {
            pwm_percent: pwm,
            rpm,
        });
    }

    // Restore pre-calibration PWM
    if let (Some(restore), Some(ref ctrl)) = (pre_cal_pwm, &state.fan_controller) {
        {
            let mut guard = ctrl.lock(); // parking_lot — always succeeds
            if let Err(e) = guard.set_pwm(channel, restore) {
                log::warn!("failed to restore pre-calibration PWM on ch{channel}: {e}");
            }
        }
    }

    // Derive start_pwm and stop_pwm
    let start_pwm = points.iter().find(|p| p.rpm > 0).map(|p| p.pwm_percent);
    let stop_pwm = points
        .iter()
        .rev()
        .find(|p| p.rpm == 0)
        .map(|p| p.pwm_percent);
    let min_rpm = points
        .iter()
        .map(|p| p.rpm)
        .filter(|&r| r > 0)
        .min()
        .unwrap_or(0);
    let max_rpm = points.iter().map(|p| p.rpm).max().unwrap_or(0);

    json_ok(
        StatusCode::OK,
        CalibrationResponse {
            api_version: API_VERSION,
            fan_id,
            points,
            start_pwm,
            stop_pwm,
            min_rpm,
            max_rpm,
        },
    )
}
