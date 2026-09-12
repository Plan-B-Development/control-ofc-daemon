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

/// How often the post-boot adoption loop ENUMERATES (it rarely probes).
///
/// Enumeration is a config read plus a sysfs scan and opens nothing, so this
/// interval is what bounds "how long after a device appears until the daemon
/// notices it". Five seconds keeps that inside a user's attention span.
pub const POST_BOOT_ADOPTION_INTERVAL: Duration = Duration::from_secs(5);

/// Probes permitted over an UNCHANGED candidate set, after a change (`OFN-r`).
///
/// Zero would be wrong: a board can enumerate its tty a moment before its
/// firmware answers the DEC-250 handshake, so the first probe after a device
/// appears can legitimately fail on a device that is really there. These are the
/// retries for exactly that, and they are consumed only after a set change — a
/// machine whose serial devices never change spends none of them, and is never
/// re-probed at all.
const POST_BOOT_HANDSHAKE_RETRIES: u32 = 3;

/// Whether this tick of the post-boot loop should probe (`OFN-r`).
///
/// Pure over its inputs so BOTH arms are testable without a serial device — the
/// loop-level test can only safely exercise the "do not probe" arm, because
/// actually probing opens whatever ttys the host really has.
///
/// A changed candidate set always probes and refreshes the retry budget; an
/// unchanged one probes only while that budget lasts, which covers the board
/// whose tty enumerates a moment before its firmware answers the handshake.
fn should_probe(
    last_probed: &mut Vec<String>,
    candidates: Vec<String>,
    retries_left: &mut u32,
) -> bool {
    if crate::serial::adoption::same_port_set(last_probed, &candidates) {
        if *retries_left == 0 {
            return false;
        }
        *retries_left -= 1;
        true
    } else {
        *last_probed = candidates;
        *retries_left = POST_BOOT_HANDSHAKE_RETRIES;
        true
    }
}

/// Keep looking for an OpenFanController after boot, off the critical path.
///
/// [SAFETY] This is what makes a one-attempt boot safe (`OFN-r`, `OFN-s`).
/// Adoption used to be a synchronous ladder of up to six attempts over ~31 s,
/// ahead of the API server, both poll loops and the profile engine — so a
/// machine with no controller paid the whole stall for nothing, and a machine
/// whose controller enumerated late got no second chance ever: the reconnect
/// probe lives inside the OpenFan poll loop, which is only spawned when boot
/// already adopted something. Losing adoption is not merely losing fan control;
/// the thermal emergency's `force_all_with_floor` is guarded by
/// `if let Some(be) = openfan_be`, so it loses its only path to those fans.
///
/// **It drives `openfan_rescan_handler` rather than probing directly, and that
/// is the design.** A second probe-and-install path would be a second chance to
/// skip the DEC-250 identity handshake, the DEC-266 conditional install, the
/// poll-loop spawn or the 277-c handle registration — the exact duplication
/// `serial::adoption`'s module docs warn about. Going through the handler also
/// inherits its single-flight guard, so this loop and a user clicking *Rescan
/// Hardware* can never probe the same ports concurrently.
///
/// **This loop owns its own "has anything changed?" test, and must.** The first
/// draft leaned on [`OPENFAN_RESCAN_COOLDOWN`] to make a 5 s tick free over
/// unchanged hardware. It does not: that predicate is
/// `elapsed < COOLDOWN && same_port_set(..)` — an **AND** — so it *spaces*
/// repeat probes to one per ten seconds, it never skips one. A 60 s window would
/// therefore have opened every unrelated tty on the bus about six times per
/// boot, and a 180 s configured window about eighteen — worse than the twelve
/// this change set out to remove, and every one of those opens asserts DTR and
/// resets an Arduino-class board. So the probe is gated on the candidate set
/// actually differing from the one boot already tried, plus
/// [`POST_BOOT_HANDSHAKE_RETRIES`] for the device that enumerates before its
/// firmware answers.
///
/// Stops early on the first adoption, and on shutdown.
pub async fn post_boot_adoption_loop(
    state: Arc<AppState>,
    window: Duration,
    interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    boot_candidates: Vec<String>,
) {
    // Seeded with what BOOT already probed, so an unchanged bus is never
    // re-probed at all — and a device that appeared between boot's probe and this
    // loop's first tick still reads as a change.
    let mut last_probed = boot_candidates;
    let mut retries_left: u32 = 0;
    let deadline = tokio::time::Instant::now() + window;
    let mut ticker = tokio::time::interval(interval);
    // `interval` yields immediately on its first tick; boot has just probed, so
    // skip it rather than probing twice in the same instant.
    ticker.tick().await;

    // Before waiting on anything: boot only spawns this when it adopted nothing,
    // but a user rescan can win the race in the interval before the first tick,
    // and there is no reason to hold a task open for five seconds to discover it.
    if state.openfan().is_some() {
        return;
    }

    loop {
        // The deadline is an ARM of the select, not a check inside the body. As a
        // body check it could only fire on a tick, so the loop overran its window
        // by up to one interval and a task registered in `task_handles` stayed
        // alive that much longer into shutdown.
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep_until(deadline) => {
                log::debug!(
                    "No OpenFanController appeared within the post-boot adoption window \
                     ({window:?}) — use POST /fans/openfan/rescan if one is attached later"
                );
                return;
            }
            _ = ticker.tick() => {}
        }
        if *shutdown.borrow() {
            return;
        }
        if state.openfan().is_some() {
            // Adopted — by this loop's previous tick, or by a user rescan.
            return;
        }

        // Enumerate — a config read plus a sysfs scan, opening nothing — and probe
        // only if the world actually changed since the last thing that DID probe.
        let candidates = crate::serial::adoption::serial_port_candidates_enumerated(
            state.running_config.serial.port.as_deref(),
            crate::serial::real_transport::enumerate_serial_candidates,
        );
        if !should_probe(&mut last_probed, candidates, &mut retries_left) {
            continue;
        }

        // The result is deliberately discarded: every outcome is either already
        // logged by the handler or is the expected one. "Not found" is the normal
        // answer on a machine that has no controller, and must stay silent — it is
        // the whole point of `OFN-c`.
        let _ = openfan_rescan_handler(axum::extract::State(Arc::clone(&state))).await;

        if state.openfan().is_some() {
            // The handler already logged the adoption and the port; this only
            // records WHICH search found it, so it is debug rather than a second
            // info line saying the same thing.
            log::debug!("...adopted by the post-boot search rather than at startup");
            return;
        }
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
/// engine's thermal `force_all_with_floor` is guarded by `if let Some(be) = openfan_be`, so
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
            first_openfan_port(&candidates, configured.as_deref(), timeout, |p| {
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

                    // Deliberately no trigger in the wording: this path is reached both by
                    // `POST /fans/openfan/rescan` and by the post-boot adoption loop,
                    // and the old "via rescan" told an operator they had performed an
                    // action they had not (`OFN-c` is a log-honesty register). Each
                    // caller records its own context.
                    log::info!("OpenFanController adopted on {port}");
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

    // ── `OFN-r`: probe only when the bus actually changed ────────────────────

    #[test]
    fn an_unchanged_candidate_set_stops_probing_once_its_retries_are_spent() {
        // The DISCRIMINATING arm, and the defect this replaced. The first draft
        // leaned on OPENFAN_RESCAN_COOLDOWN to make repeat ticks free; that
        // predicate is `elapsed < COOLDOWN && same_port_set(..)`, an AND, so it
        // only SPACES repeats — every tick past the cooldown would have opened
        // every unrelated tty again, asserting DTR and resetting Arduino-class
        // boards ~6 times a boot instead of the 2 this change advertises.
        let mut last = vec!["/dev/ttyACM0".to_string()];
        let mut retries = POST_BOOT_HANDSHAKE_RETRIES;

        let mut probes = 0;
        for _ in 0..50 {
            if should_probe(&mut last, vec!["/dev/ttyACM0".to_string()], &mut retries) {
                probes += 1;
            }
        }
        assert_eq!(
            probes, POST_BOOT_HANDSHAKE_RETRIES as usize,
            "an unchanged bus must be probed only for the handshake-retry budget, \
             then never again — each probe is a DTR reset of somebody's Arduino"
        );
    }

    #[test]
    fn a_changed_candidate_set_probes_and_refreshes_the_budget() {
        // The opposite arm: without it, a `should_probe` that always returned
        // false would pass the test above, and a controller plugged in during the
        // window would never be adopted.
        let mut last = vec!["/dev/ttyACM0".to_string()];
        let mut retries = 0;

        assert!(
            should_probe(
                &mut last,
                vec!["/dev/ttyACM0".into(), "/dev/ttyACM1".into()],
                &mut retries
            ),
            "a newly appeared tty must be probed at once, even with no retries left"
        );
        assert_eq!(
            retries, POST_BOOT_HANDSHAKE_RETRIES,
            "a change refreshes the budget"
        );
        assert_eq!(
            last.len(),
            2,
            "and becomes the set future ticks compare against"
        );
    }

    #[test]
    fn the_retry_budget_covers_a_tty_that_appears_before_its_firmware_answers() {
        // Why the budget is not zero: a board can enumerate its tty a moment
        // before it will answer the DEC-250 handshake, so the first probe after a
        // change can fail on a device that is genuinely there.
        let mut last: Vec<String> = Vec::new();
        let mut retries = 0;
        assert!(should_probe(
            &mut last,
            vec!["/dev/ttyACM0".into()],
            &mut retries
        ));
        for i in 0..POST_BOOT_HANDSHAKE_RETRIES {
            assert!(
                should_probe(&mut last, vec!["/dev/ttyACM0".into()], &mut retries),
                "retry {i} must still be permitted after the tty appeared"
            );
        }
    }

    /// The call site, on the arm that is safe to run: an unchanged bus must not
    /// reach the probe at all.
    ///
    /// Seeded with what the host REALLY enumerates, so the set genuinely matches
    /// on any machine — and so the test never opens a serial device. The opposite
    /// arm is deliberately left to `should_probe`'s unit tests above: exercising
    /// it here would mean actually probing whatever ttys the developer has
    /// attached.
    #[tokio::test]
    async fn the_loop_does_not_probe_an_unchanged_bus() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx.clone());
        let real = crate::serial::real_transport::enumerate_serial_candidates();

        let finished = tokio::time::timeout(
            Duration::from_secs(5),
            post_boot_adoption_loop(
                state.clone(),
                Duration::from_millis(120),
                Duration::from_millis(5),
                rx,
                real,
            ),
        )
        .await;

        assert!(finished.is_ok(), "the loop must end at its window");
        assert!(
            state.last_openfan_rescan.lock().is_none(),
            "the loop probed an unchanged bus — every probe opens a tty and asserts \
             DTR, which resets Arduino-class boards"
        );
    }

    // ── `OFN-r`: the post-boot adoption loop ─────────────────────────────────

    /// A bare AppState with no controller adopted, so the loop has work to do.
    fn adoption_state(shutdown: tokio::sync::watch::Receiver<bool>) -> Arc<AppState> {
        let cache = Arc::new(crate::health::cache::StateCache::new());
        let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
        Arc::new(AppState {
            cache,
            staleness_config: crate::health::staleness::StalenessConfig::default(),
            daemon_version: "0.0.0-test".into(),
            fan_controller: Arc::new(parking_lot::RwLock::new(None)),
            openfan_runtime: crate::api::handlers::OpenFanRuntime {
                timeout: Duration::from_millis(50),
                interval: Duration::from_millis(1000),
                shutdown,
            },
            hwmon_controller: None,
            start_time: std::time::Instant::now(),
            history: Arc::new(crate::health::history::HistoryRing::new(10)),
            active_profile: Arc::new(parking_lot::Mutex::new(None)),
            calibrating: std::sync::atomic::AtomicBool::new(false),
            characterization: Arc::new(parking_lot::Mutex::new(None)),
            validation: Arc::new(Default::default()),
            characterization_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            control_path: Arc::new(parking_lot::Mutex::new(None)),
            control_path_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            control_paths: Arc::new(parking_lot::RwLock::new(Default::default())),
            pwm_baselines: Default::default(),
            openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
            last_openfan_rescan: Arc::new(parking_lot::Mutex::new(None)),
            adopted_poll_handles: Arc::new(parking_lot::Mutex::new(Vec::new())),
            amd_gpus: Vec::new(),
            intel_gpus: Vec::new(),
            nvidia_gpus: Vec::new(),
            profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
            config_path: String::new(),
            runtime_config_path: std::path::PathBuf::new(),
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

    /// The loop must END at its window, not run for the process lifetime.
    ///
    /// Bounded by a real deadline rather than a bare sleep (tokio trap 3 in
    /// `CLAUDE.md`): this task is registered in `task_handles`, so a loop that
    /// never returns turns shutdown into a drain that blocks until
    /// `SHUTDOWN_TASK_TIMEOUT` — a hung CI job rather than a red test.
    #[tokio::test]
    async fn the_loop_stops_when_its_window_expires() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx.clone());

        // Tighter than POST_BOOT_ADOPTION_INTERVAL, deliberately: a generous
        // timeout is satisfied by a loop that only notices its deadline on the
        // next tick, so it would pass with the deadline arm deleted. Measured —
        // the first draft used 20 s and did exactly that.
        let finished = tokio::time::timeout(
            Duration::from_secs(1),
            post_boot_adoption_loop(
                state.clone(),
                Duration::from_millis(1),
                Duration::from_millis(5),
                rx,
                Vec::new(),
            ),
        )
        .await;

        assert!(
            finished.is_ok(),
            "the loop must return AT its window, not on the next tick after it"
        );
        // Precondition on the fixture itself: if a controller had somehow been
        // adopted, the loop would have returned for the OTHER reason and this
        // test would pass without exercising the deadline at all.
        assert!(
            state.openfan().is_none(),
            "fixture adopted a controller — the deadline arm was not the one tested"
        );
    }

    /// The loop must stop on shutdown even with its window wide open.
    #[tokio::test]
    async fn the_loop_stops_on_shutdown() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx.clone());

        let handle = tokio::spawn(post_boot_adoption_loop(
            state,
            Duration::from_secs(3600),
            Duration::from_secs(5),
            rx,
            Vec::new(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).expect("shutdown watch");

        // Tighter than POST_BOOT_ADOPTION_INTERVAL for the same reason as above:
        // with 20 s this passed with the shutdown ARM deleted, because the body's
        // `*shutdown.borrow()` check still exits on the next tick. What matters is
        // that shutdown is observed PROMPTLY — the drain in `task_handles` joins
        // this task, so a tick's delay is a tick added to every shutdown.
        let joined = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(
            joined.is_ok(),
            "the loop must observe the shutdown watch immediately; waiting for the next \
             tick adds that delay to the shutdown drain that joins it"
        );
    }

    /// A controller already adopted means there is nothing to look for, so the
    /// loop must not probe at all — every probe opens a tty and asserts DTR.
    #[tokio::test]
    async fn the_loop_does_not_run_once_a_controller_exists() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx.clone());
        // Stand in for an adopted controller by filling the slot the loop checks.
        // A local double rather than a shared one: the trait is two methods, and
        // this test only needs the slot to be non-empty — it must never be
        // spoken to, which is the property being asserted.
        struct IdleTransport;
        impl crate::serial::transport::SerialTransport for IdleTransport {
            fn write_line(&mut self, _data: &str) -> Result<(), crate::error::SerialError> {
                panic!("the post-boot loop must not talk to an adopted controller")
            }
            fn read_line(
                &mut self,
                _timeout: Duration,
            ) -> Result<String, crate::error::SerialError> {
                panic!("the post-boot loop must not talk to an adopted controller")
            }
        }
        let transport: Box<dyn crate::serial::transport::SerialTransport + Send> =
            Box::new(IdleTransport);
        let shared = Arc::new(parking_lot::Mutex::new(transport));
        let ctrl = crate::serial::controller::FanController::new_shared(
            shared,
            state.cache.clone(),
            Duration::from_millis(50),
        );
        *state.fan_controller.write() = Some(Arc::new(parking_lot::Mutex::new(ctrl)));

        // Tighter than POST_BOOT_ADOPTION_INTERVAL, and that is the whole test:
        // the rescan handler ALREADY refuses to probe when a controller is
        // connected, so the cooldown stamp below cannot distinguish a loop that
        // checked from one that called the handler and was turned away. Measured —
        // with a 20 s timeout this passed with both adoption checks deleted. What
        // only a loop that checks for itself can do is return before the first tick.
        let finished = tokio::time::timeout(
            Duration::from_secs(1),
            post_boot_adoption_loop(
                state.clone(),
                Duration::from_secs(3600),
                Duration::from_secs(5),
                rx,
                Vec::new(),
            ),
        )
        .await;

        assert!(
            finished.is_ok(),
            "the loop must return at once when a controller is already adopted, rather \
             than holding a task open until its first tick"
        );
        assert!(
            state.last_openfan_rescan.lock().is_none(),
            "it must not have probed — a probe stamps the rescan cooldown, and every \
             probe opens a tty and asserts DTR"
        );
    }

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
