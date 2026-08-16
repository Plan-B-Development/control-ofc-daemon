//! OpenFan serial calibration endpoint. The bare PWM/RPM write endpoints were
//! retired at 2.0.0 (DEC-165) — the profile engine is the sole writer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

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

/// Deadman window for the profile-engine write-pause held across an OpenFan
/// calibration sweep (DEC-191). It must span the WHOLE sweep — `(steps + 1)`
/// settle holds plus slack for the per-step + restore writes and scheduling —
/// because the generic [`crate::constants::VERIFY_PAUSE_DEADMAN`] (30 s) is sized
/// for the brief hwmon/GPU verifies and a sweep runs far longer (a default
/// 10 × 5 s sweep is ~55 s); too short a deadman would self-clear mid-sweep and
/// reopen the overwrite race. `steps`/`hold_seconds` are clamped to the same
/// range [`crate::api::calibration::calibrate_openfan_channel`] uses, so the
/// window matches the actual sweep duration. The handler's RAII guard clears the
/// pause on the normal path; this bound only matters if that guard leaks.
fn calibration_pause_window(steps: u8, hold_seconds: u64) -> Duration {
    let clamped_steps = steps.clamp(2, 20) as u64;
    let clamped_hold = hold_seconds.clamp(2, 15);
    // (steps+1) settle holds + ~1 s per write (the serial timeout is 500 ms and
    // there are steps+1 sweep writes plus the restore) + 10 s scheduling slack.
    // The write-time term matters at the maximum (20 × 15 ≈ 325 s of holds):
    // without it the deadman could expire ~1 s before a slow-serial sweep
    // finished and let the engine overwrite the final data point.
    Duration::from_secs((clamped_steps + 1) * clamped_hold + (clamped_steps + 2) + 10)
}

/// POST /fans/openfan/{channel}/calibrate — run a PWM-to-RPM calibration sweep.
///
/// Delegates the sweep to [`crate::api::calibration::calibrate_openfan_channel`]
/// (DEC-134) — the handler owns only HTTP mapping, the concurrency flag, the
/// profile-engine write-pause for the sweep's duration (DEC-191), and the
/// controller-backed write closure. The helper restores the
/// pre-calibration PWM on every exit path, including a failed write
/// mid-sweep (previously the inline copy returned early without restoring,
/// which could park a fan at a sweep step).
pub async fn calibrate_openfan_handler(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<u8>,
    Json(body): Json<crate::api::calibration::CalibrationRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::api::calibration::CalibrationError;

    let Some(ctrl) = state.openfan() else {
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

    // DEC-191: pause the profile engine's write phase for the whole sweep. With
    // an active profile, the engine's 1 Hz tick would otherwise overwrite each
    // step's test PWM during the settle window — corrupting the RPM readback and
    // the derived start/stop PWM (the OpenFan backend has no lease to fence it,
    // unlike hwmon). The pause reuses the verify single-flight slot, so a
    // hardware verify in progress rejects calibration (and vice-versa) — both
    // drive hardware directly. `calibrating` above still guards
    // calibration-vs-calibration.
    let pause_window = calibration_pause_window(body.steps, body.hold_seconds);
    let Some(_pause) = super::begin_verify_pause(&state.cache, pause_window) else {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation(
                "a hardware verify is in progress — retry calibration once it completes",
            ),
        );
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

/// `POST /fans/openfan/rescan` — look for an OpenFanController and adopt it
/// without restarting the daemon (DEC-265).
///
/// [SAFETY] The gap this closes is not merely "fan control is unavailable". The
/// controller used to be adopted once, during boot, into a plain `Option` that
/// nothing could subsequently write. A device that enumerated a second too late,
/// or that failed its DEC-250 identity handshake once, therefore left the daemon
/// with no OpenFan backend for the entire process lifetime — and the profile
/// engine's 105 C `force_all` is guarded by `if let Some(be) = openfan_be`, so
/// the thermal emergency silently lost its reach to every OpenFan-attached fan
/// too. A failed boot connect only logs a warning, so `Restart=on-failure` never
/// fired and nothing recovered it.
///
/// Adoption goes through the same [`crate::serial::adoption`] pair the boot path
/// uses, so the identity handshake cannot be skipped here. On success the
/// controller is installed and a poll loop is started for it; the profile engine
/// picks it up on its next tick.
///
/// Idempotent: rescanning while a controller is already adopted reports the
/// existing one and probes nothing.
pub async fn openfan_rescan_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::serial::adoption::{first_openfan_port, serial_port_candidates};
    use crate::serial::controller::FanController;
    use crate::serial::real_transport::{auto_detect_port, RealSerialTransport};
    use crate::serial::transport::SerialTransport;

    if state.openfan().is_some() {
        return json_ok(
            StatusCode::OK,
            serde_json::json!({
                "adopted": false,
                "already_connected": true,
                "message": "an OpenFanController is already connected",
            }),
        );
    }

    // Single-flight. Two concurrent rescans would both probe the same tty — and
    // the loser would install a second controller over the winner's, leaving an
    // orphaned poll loop reading a transport nothing writes through.
    if state
        .openfan_rescanning
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation("an OpenFan rescan is already in progress"),
        );
    }
    let _guard = CalibrationGuard {
        flag: &state.openfan_rescanning,
    };

    let timeout = state.openfan_runtime.timeout;
    let configured = state.running_config.serial.port.clone();

    // Serial probing is blocking and can take seconds across several candidates.
    let probe = tokio::task::spawn_blocking(move || {
        let candidates =
            serial_port_candidates(configured.as_deref(), || auto_detect_port(timeout));
        first_openfan_port(&candidates, timeout, |p| {
            RealSerialTransport::open(p, timeout)
        })
    })
    .await;

    match probe {
        Ok(Some((port, transport))) => {
            let boxed: Box<dyn SerialTransport + Send> = Box::new(transport);
            let shared = Arc::new(parking_lot::Mutex::new(boxed));
            let ctrl = FanController::new_shared(shared.clone(), state.cache.clone(), timeout);

            // Install BEFORE spawning the loop: the engine polls this slot every
            // tick while it has no backend, and a controller that is reachable
            // but not yet polled is strictly better than the reverse.
            *state.fan_controller.write() = Some(Arc::new(parking_lot::Mutex::new(ctrl)));

            let rt = state.openfan_runtime.clone();
            let poll_cache = state.cache.clone();
            tokio::spawn(async move {
                crate::polling::openfan_poll_loop(
                    poll_cache,
                    shared,
                    rt.timeout,
                    rt.interval,
                    rt.shutdown,
                )
                .await;
            });

            log::info!("OpenFanController adopted on {port} via rescan");
            json_ok(
                StatusCode::OK,
                serde_json::json!({
                    "adopted": true,
                    "already_connected": false,
                    "port": port,
                    "message": format!("OpenFanController adopted on {port}"),
                }),
            )
        }
        Ok(None) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "no OpenFanController found — no candidate port both opened and \
                 identified as one",
            ),
        ),
        Err(e) => {
            log::error!("OpenFan rescan probe task panicked: {e}");
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorEnvelope::hardware_unavailable("OpenFan rescan failed to run"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_pause_window_spans_the_whole_sweep() {
        // DEC-191: the engine write-pause must outlast the sweep. A default
        // 10×5 s sweep (~55 s of settle holds) must get a window comfortably
        // above the 30 s VERIFY_PAUSE_DEADMAN, which would otherwise self-clear
        // mid-sweep and reopen the overwrite race.
        let w = calibration_pause_window(10, 5);
        assert!(
            w >= Duration::from_secs(11 * 5),
            "window must cover (steps+1) settle holds; got {w:?}"
        );
        assert!(
            w > crate::constants::VERIFY_PAUSE_DEADMAN,
            "a calibration window must exceed the generic verify deadman"
        );

        // Maximum sweep (clamped 20 steps × 15 s) — the case the bespoke window
        // sizing exists for. It must outlast the worst-case real sweep:
        // (steps+1) settle holds + (steps+1) sweep writes + 1 restore write, each
        // write bounded by the 500 ms serial timeout (audit P3 follow-up).
        let max = calibration_pause_window(20, 15);
        let worst_case_sweep = Duration::from_millis(21 * 15_000 + 22 * 500);
        assert!(
            max > worst_case_sweep,
            "max-param window {max:?} must outlast the worst-case sweep {worst_case_sweep:?}"
        );

        // Clamps mirror calibrate_openfan_channel (steps 2..=20, hold 2..=15),
        // so out-of-range inputs cannot under- or over-size the window.
        assert_eq!(
            calibration_pause_window(0, 0),
            calibration_pause_window(2, 2)
        );
        assert_eq!(
            calibration_pause_window(99, 99),
            calibration_pause_window(20, 15)
        );
    }
}
