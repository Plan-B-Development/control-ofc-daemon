//! OpenFan serial calibration endpoint. The bare PWM/RPM write endpoints were
//! retired at 2.0.0 (DEC-165) — the profile engine is the sole writer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::serial::controller::FanControlError;

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
///
/// Delegates the sweep to [`crate::api::calibration::calibrate_openfan_channel`]
/// (DEC-134) — the handler owns only HTTP mapping, the concurrency flag, and
/// the controller-backed write closure. The helper restores the
/// pre-calibration PWM on every exit path, including a failed write
/// mid-sweep (previously the inline copy returned early without restoring,
/// which could park a fan at a sweep step).
pub async fn calibrate_openfan_handler(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<u8>,
    Json(body): Json<crate::api::calibration::CalibrationRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::api::calibration::CalibrationError;

    let Some(ctrl) = state.fan_controller.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    };

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

    // Controller-backed write closure. Preserves the pre-DEC-134 status
    // mapping: serial faults surface as Hardware (503), controller-side
    // validation (e.g. stop-timeout safety) as Validation (400).
    let write_fn = move |ch: u8, pwm: u8| -> Result<(), CalibrationError> {
        let mut guard = ctrl.lock(); // parking_lot — always succeeds
        match guard.set_pwm(ch, pwm) {
            Ok(_) => Ok(()),
            Err(FanControlError::Validation(msg)) => Err(CalibrationError::Validation(msg)),
            Err(e @ FanControlError::Serial(_)) => Err(CalibrationError::Hardware(e.to_string())),
        }
    };

    match crate::api::calibration::calibrate_openfan_channel(
        state.cache.clone(),
        channel,
        body.steps,
        body.hold_seconds,
        write_fn,
    )
    .await
    {
        Ok(result) => json_ok(
            StatusCode::OK,
            CalibrationResponse {
                api_version: API_VERSION,
                fan_id: result.fan_id,
                points: result.points,
                start_pwm: result.start_pwm,
                stop_pwm: result.stop_pwm,
                min_rpm: result.min_rpm,
                max_rpm: result.max_rpm,
            },
        ),
        Err(CalibrationError::ThermalAbort {
            sensor_id, temp_c, ..
        }) => error_response(
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
        Err(CalibrationError::Validation(msg)) => {
            error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(msg))
        }
        Err(CalibrationError::Hardware(msg)) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(msg),
        ),
    }
}
