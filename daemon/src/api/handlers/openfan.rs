//! OpenFan serial calibration endpoint. The bare PWM/RPM write endpoints were
//! retired at 2.0.0 (DEC-165) — the profile engine is the sole writer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState, LastRescan};
use crate::api::responses::*;
use crate::serial::controller::FanControlError;

/// Minimum spacing between `POST /fans/openfan/rescan` probes (register row
/// 10-e).
///
/// This bounds **repetition**, which `openfan_rescanning` does not: that flag
/// stops two probes running *at once*, and nothing stopped a client from firing
/// them back to back forever. Each probe asserts DTR across every candidate tty,
/// which **resets Arduino-class boards** — so a loop on a failing rescan is not
/// merely wasted work, it holds unrelated serial hardware in reset.
///
/// Ten seconds is chosen against the cost of being wrong in each direction: a
/// genuine "I just plugged it in, try again" retry is a human action and tolerates
/// it easily, while a runaway client is cut from unbounded to six probes a minute.
/// A successful adoption never reaches this check at all — the handler returns
/// early once a controller is connected — so only failing probes are spaced.
const OPENFAN_RESCAN_COOLDOWN: Duration = Duration::from_secs(10);

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
        // DEC-295: 409 + `validation_error`, matching the DEC-191 single-flight
        // refusal two functions up rather than inventing a shape. `retryable`
        // is TRUE like the rescan cooldown's 409 and unlike the sibling
        // single-flight ones: the condition clears by itself when the ladder
        // releases. A 400 would have told the client its REQUEST was malformed
        // and not to retry, which is wrong on both counts.
        Err(e @ CalibrationError::ThermalForceActive { .. }) => error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope {
                error: ErrorBody {
                    code: "validation_error".into(),
                    message: e.to_string(),
                    retryable: true,
                    source: "validation".into(),
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
/// engine's thermal `force_all` is guarded by `if let Some(be) = openfan_be`, so
/// the thermal emergency silently lost its reach to every OpenFan-attached fan
/// too. A failed boot connect only logs a warning, so `Restart=on-failure` never
/// fired and nothing recovered it.
///
/// Adoption goes through the same [`crate::serial::adoption`] pair the boot path
/// uses, so the identity handshake cannot be skipped here. On success the
/// controller is installed and a poll loop is started for it; the profile engine
/// picks it up on its next tick.
///
/// Rescanning while a controller is already adopted reports the existing one and
/// probes nothing — **unless the cooldown fires first** (DEC-291), which it may,
/// because the cooldown is now checked before that return. So this is idempotent
/// in effect (it never re-probes or re-adopts) but not in status code.
///
/// [SAFETY] The adoption is deliberately **not** performed by the handler's own
/// future (DEC-266). Serial probing can outlast the client's HTTP timeout, and a
/// dropped handler future would then have discarded a controller that was found
/// and identified — losing the thermal emergency's OpenFan leg from a request
/// that merely *looked* like it timed out. Probe, install and single-flight
/// release all live in a detached task; the handler only waits for the answer.
pub async fn openfan_rescan_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::serial::adoption::{
        first_openfan_port, same_port_set, serial_port_candidates_enumerated,
    };
    use crate::serial::controller::FanController;
    use crate::serial::real_transport::{enumerate_serial_candidates, RealSerialTransport};
    use crate::serial::transport::SerialTransport;

    let timeout = state.openfan_runtime.timeout;
    let configured = state.running_config.serial.port.clone();
    // Enumerating candidates is a config read plus a sysfs/dev scan. It does NOT
    // open anything, and DTR is asserted on open — so this costs nothing of what
    // the cooldown below exists to ration.
    //
    // **That claim was false until DEC-291 and is now true.** This used to call
    // `auto_detect_port`, which probes each candidate by *opening* it — so every
    // rescan reset the board before the cooldown was even consulted, and the
    // cooldown rationed nothing. Identification still happens, once, in
    // `first_openfan_port` below, on the far side of the cooldown and the
    // single-flight guard. Keep enumeration non-opening: it is load-bearing.
    let candidates =
        serial_port_candidates_enumerated(configured.as_deref(), enumerate_serial_candidates);

    // 10-e: space repeated probes. **Checked BEFORE the already-connected return
    // below (DEC-291) — this ordering was deliberately the other way round until
    // 2026-08-28, and the reversal is a knowing trade.**
    //
    // The original order let the common success path skip the cooldown entirely,
    // which is better UX: a client that has just adopted a controller and asks
    // again gets the informative `already_connected` payload rather than a
    // refusal. Checking the cooldown first means that client gets a 409 instead,
    // for up to `OPENFAN_RESCAN_COOLDOWN`, even though a controller is present.
    //
    // It is ordered this way so that the cooldown is the FIRST thing a repeated
    // probe meets, unconditionally, rather than a rule two earlier branches can
    // step in front of. The accepted cost is the case above; the message below is
    // worded so it stays true in it. See DEC-291 for the full reasoning and for
    // what this does NOT fix.
    //
    // **The cooldown applies only while the world has not changed.** Rate-limiting
    // on time alone was wrong in the one case that matters most: plug a controller
    // in and immediately click rescan — which is a human action measured in
    // seconds, not tens of seconds — and the request was refused, so the device
    // was not adopted and the GUI showed nothing. That transiently re-opens the
    // "you need to restart the daemon" mis-advice DEC-265/266 exists to remove, on
    // the one endpoint whose entire purpose is recovery without a restart.
    //
    // Comparing the candidate set separates the two cases by what actually
    // differs. A newly attached controller enumerates a new tty, so a genuine
    // retry proceeds at once; a client looping against unchanged hardware learns
    // nothing new by probing again and is spaced.
    if let Some(last) = state.last_openfan_rescan.lock().as_ref() {
        let elapsed = last.at.elapsed();
        if elapsed < OPENFAN_RESCAN_COOLDOWN && same_port_set(&last.candidates, &candidates) {
            let wait = (OPENFAN_RESCAN_COOLDOWN - elapsed).as_secs() + 1;
            return error_response(
                StatusCode::CONFLICT,
                &ErrorEnvelope {
                    error: ErrorBody {
                        code: "validation_error".into(),
                        message: format!(
                            "an OpenFan rescan over the same ports was attempted \
                             moments ago — each probe resets Arduino-class boards, so \
                             retry in {wait}s, or retry immediately once the attached \
                             hardware changes"
                        ),
                        // retryable: TRUE, unlike the sibling 409s. This condition
                        // clears on its own within seconds and the message says so;
                        // reporting `false` here — the default for
                        // `validation_error` — would tell a client keying its
                        // backoff off this field, which is the field's documented
                        // purpose, that a ten-second wait is permanent.
                        retryable: true,
                        source: "validation".into(),
                        details: None,
                    },
                },
            );
        }
    }

    // Now that the cooldown has had first refusal (DEC-291), report an existing
    // connection. Reaching here means either no probe has run recently or the
    // candidate set has changed since one did.
    if state.openfan().is_some() {
        return json_ok(
            StatusCode::OK,
            OpenFanRescanResponse {
                api_version: API_VERSION,
                adopted: false,
                already_connected: true,
                port: None,
                message: "an OpenFanController is already connected".into(),
            },
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
    // Built IMMEDIATELY after the CAS, in the handler, then moved into the task.
    // Constructing it inside the task instead would leave a gap in which the flag
    // is set but nothing owns its release: any future early return between the two
    // — or the task being dropped before its first poll, which `tokio::spawn` does
    // at runtime shutdown — would wedge the route at 409 for the process lifetime,
    // on the one endpoint whose purpose is recovery without a restart.
    let guard = RescanGuard(state.clone(), candidates.clone());

    // Everything below runs detached, so dropping the handler future (client
    // disconnect, read timeout) cancels neither the probe nor the adoption, and
    // does not release the single-flight flag early. `spawn_blocking` cannot be
    // cancelled anyway — before DEC-266 the probe kept running while the guard
    // was already gone, so a retry raced the orphan for the same tty.
    let (tx, rx) = tokio::sync::oneshot::channel::<RescanOutcome>();
    let task_state = state.clone();
    tokio::spawn(async move {
        let guard = guard;

        // Serial probing is blocking and can take seconds across several candidates.
        let probe = tokio::task::spawn_blocking(move || {
            // The SAME list the cooldown was evaluated against — recomputing it
            // here could probe a set the cooldown never saw, and stamp a set that
            // was never probed.
            first_openfan_port(&candidates, timeout, |p| {
                RealSerialTransport::open(p, timeout)
            })
        })
        .await;

        let outcome = match probe {
            Ok(Some((port, transport))) => {
                let boxed: Box<dyn SerialTransport + Send> = Box::new(transport);
                let shared = Arc::new(parking_lot::Mutex::new(boxed));
                let ctrl =
                    FanController::new_shared(shared.clone(), task_state.cache.clone(), timeout);

                // Install BEFORE spawning the loop: the engine polls this slot
                // every tick while it has no backend, and a controller that is
                // reachable but not yet polled is strictly better than the
                // reverse.
                //
                // DEC-266: check AND set under one write guard. The
                // `already_connected` early return and the CAS above are two
                // adjacent statements with no `.await` between them, but on the
                // multi-thread runtime two handlers on different OS threads can
                // still interleave there — B reads the slot empty, A wins the CAS,
                // probes, installs and releases, then B's CAS succeeds and B
                // installs a SECOND controller over A's. The engine only re-reads
                // the slot while it has no backend, so it would keep writing
                // through A's controller while B's poll loop read a different
                // transport. Making the install itself conditional closes the
                // window and makes `polling.rs`'s "written once, never replaced"
                // invariant true rather than nearly true.
                let won = {
                    let mut slot = task_state.fan_controller.write();
                    if slot.is_none() {
                        *slot = Some(Arc::new(parking_lot::Mutex::new(ctrl)));
                        true
                    } else {
                        false
                    }
                };

                if !won {
                    log::warn!(
                        "OpenFanController found on {port} but another rescan had already \
                         adopted one — discarding this probe rather than replacing it"
                    );
                    RescanOutcome::AlreadyAdopted
                } else {
                    let rt = task_state.openfan_runtime.clone();
                    let poll_cache = task_state.cache.clone();
                    let poll_handle = tokio::spawn(async move {
                        crate::polling::openfan_poll_loop(
                            poll_cache,
                            shared,
                            rt.timeout,
                            rt.interval,
                            rt.shutdown,
                        )
                        .await;
                    });
                    // 277-c: register the handle so `shutdown_sequence` DRAINS
                    // this loop, not merely signals it. `main`'s `task_handles`
                    // was built at boot and cannot know about a loop started
                    // here. Nothing in this loop writes PWM today, so the fix is
                    // pre-emptive — but the drain invariant is what makes the
                    // restore the guaranteed last writer, and a loop outside it
                    // would break that silently the first time one did.
                    task_state.adopted_poll_handles.lock().push(poll_handle);

                    log::info!("OpenFanController adopted on {port} via rescan");
                    RescanOutcome::Adopted(port)
                }
            }
            Ok(None) => RescanOutcome::NotFound,
            Err(e) => {
                log::error!("OpenFan rescan probe task panicked: {e}");
                RescanOutcome::ProbeFailed
            }
        };

        // Release the single-flight flag BEFORE answering, so a client that
        // retries the instant it reads "no controller found" is not met with a
        // spurious 409 from its own previous attempt. Safe here: the install has
        // already completed, so nothing the guard protects is still in flight.
        drop(guard);

        // Err means the client already gave up. The adoption above still stands.
        let _ = tx.send(outcome);
    });

    match rx.await {
        Ok(RescanOutcome::Adopted(port)) => json_ok(
            StatusCode::OK,
            OpenFanRescanResponse {
                api_version: API_VERSION,
                adopted: true,
                already_connected: false,
                message: format!("OpenFanController adopted on {port}"),
                port: Some(port),
            },
        ),
        // Someone else's rescan won the race and a controller IS now adopted, so
        // the honest answer is the same one an idempotent repeat gets.
        Ok(RescanOutcome::AlreadyAdopted) => json_ok(
            StatusCode::OK,
            OpenFanRescanResponse {
                api_version: API_VERSION,
                adopted: false,
                already_connected: true,
                port: None,
                message: "an OpenFanController is already connected".into(),
            },
        ),
        Ok(RescanOutcome::NotFound) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "no OpenFanController found — no candidate port both opened and \
                 identified as one",
            ),
        ),
        // Sender dropped without sending = the detached task itself died.
        Ok(RescanOutcome::ProbeFailed) | Err(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFan rescan failed to run"),
        ),
    }
}

/// What a detached rescan concluded. Carried back to the handler over a oneshot
/// so the reply is decided by the task that actually did the work.
enum RescanOutcome {
    Adopted(String),
    /// A concurrent rescan won the install race; this probe's controller was
    /// discarded rather than replacing the live one (DEC-266).
    AlreadyAdopted,
    NotFound,
    ProbeFailed,
}

/// Releases the rescan single-flight flag when the *probe* ends.
///
/// Owned (an `Arc<AppState>`) rather than borrowing, so it can be moved into the
/// detached task. A borrow-based guard lives on the handler's stack and is
/// therefore dropped by client disconnect — clearing the flag while the probe it
/// guards is still holding a tty open (DEC-266).
struct RescanGuard(Arc<AppState>, Vec<String>);

impl Drop for RescanGuard {
    fn drop(&mut self) {
        // 10-e: stamp the cooldown from HERE — the guard drops when the probe
        // task actually finishes, so the window is measured from the end of the
        // last DTR assertion rather than from the request that started it. A
        // probe that takes 8 s to time out therefore still gets a full quiet
        // period afterwards. Stamped before the flag is released so no racing
        // request can pass the CAS while the cooldown is still unset.
        *self.0.last_openfan_rescan.lock() = Some(LastRescan {
            at: Instant::now(),
            candidates: std::mem::take(&mut self.1),
        });
        self.0.openfan_rescanning.store(false, Ordering::SeqCst);
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
