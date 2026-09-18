//! OpenFan serial calibration endpoint. The bare PWM/RPM write endpoints were
//! retired at 2.0.0 (DEC-165) — the profile engine is the sole writer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AdoptOutcome, AppState, LastRescan};
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
/// Enumeration opens no *candidate* port — that is what lets it run on a timer
/// at all — so this interval is what bounds "how long after a device appears
/// until the daemon notices it". Five seconds keeps that inside a user's
/// attention span.
///
/// **It is not free, and the flat "opens nothing" it used to claim was wrong
/// (`OFN-x`).** `serialport::available_ports()` opens the devnode of any tty
/// whose parent driver is `serial8250`, *before*
/// [`crate::serial::real_transport::enumerate_serial_candidates`]'s
/// `ttyACM`/`ttyUSB` filter runs — a blocking `open(2)`. Harmless on the
/// packaged daemon, whose unit carries `DeviceAllow=char-ttyACM/ttyUSB`, but
/// blocking all the same, which is why the loop runs it under `spawn_blocking`
/// rather than inline on a runtime worker.
pub const POST_BOOT_ADOPTION_INTERVAL: Duration = Duration::from_secs(5);

/// Probes permitted over an UNCHANGED candidate set, after a change (`OFN-r`).
///
/// Zero would be wrong: a board can enumerate its tty a moment before its
/// firmware answers the DEC-250 handshake, so the first probe after a device
/// appears can legitimately fail on a device that is really there. These are the
/// retries for exactly that, and they are consumed only after a set change — a
/// machine whose serial devices never change spends none of them, and is never
/// re-probed at all.
///
/// **They are spent per REAL probe, never per tick (`OFN-w`).** The loop drives
/// [`openfan_rescan_handler`], which carries a cooldown of its own; a tick that
/// cooldown refuses has opened nothing, so it costs nothing from this budget.
/// Spending on refusals left one probe of the four on a slow bus. See
/// [`post_boot_adoption_loop`].
const POST_BOOT_HANDSHAKE_RETRIES: u32 = 3;

/// What one tick of the post-boot loop should do (`OFN-r`, `OFN-w`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// The candidate set changed: probe, and refresh the retry budget.
    Fresh,
    /// Unchanged set with budget remaining: probe, and spend one **only if that
    /// probe actually runs**.
    Retry,
    /// Unchanged set, budget spent: do nothing at all.
    Skip,
}

/// Whether this tick of the post-boot loop should probe (`OFN-r`).
///
/// Pure over its inputs — it mutates nothing and spends nothing — so all three
/// arms are testable without a serial device, and the two state updates a
/// decision implies stay visible at the call site, next to the spend rule that
/// `OFN-w` put there.
///
/// A changed candidate set always probes and refreshes the retry budget; an
/// unchanged one probes only while that budget lasts, which covers the board
/// whose tty enumerates a moment before its firmware answers the handshake.
fn probe_decision(last_probed: &[String], candidates: &[String], retries_left: u32) -> Probe {
    if !crate::serial::adoption::same_port_set(last_probed, candidates) {
        Probe::Fresh
    } else if retries_left == 0 {
        Probe::Skip
    } else {
        Probe::Retry
    }
}

/// When the last REAL probe ended, or `None` if none has run.
///
/// This is the one fact that separates a probe from a refusal: [`RescanGuard`]
/// stamps it on drop, when the probe task actually finishes, and every refusal in
/// [`openfan_rescan_handler`] — the cooldown, the already-connected return, the
/// single-flight CAS — returns before that guard is ever built.
///
/// Read as a **fact rather than by re-testing the handler’s predicate**. Copying
/// that predicate here would create a second place to keep in step with the
/// first, and drifting from it is the exact mistake `OFN-r` made: the loop’s
/// first draft reasoned about [`OPENFAN_RESCAN_COOLDOWN`] from memory instead of
/// reading it, and got the `&&` backwards.
///
/// **It says "somebody probed", not "this loop probed", and that is accepted.**
/// `LastRescan` records only `at` and `candidates`, so a probe cannot be
/// attributed to its caller. A user `POST /fans/openfan/rescan` in flight when the
/// loop attempts one will win the single-flight CAS and stamp before the loop's
/// second read, and the loop will then spend a retry on an attempt it did not make
/// itself. Left as is because the error is **conservative in the direction that
/// matters** — it can only ever cost the loop probes, never grant it extra ones,
/// so the DTR bound `OFN-b` cares about cannot be exceeded by the race — and
/// because the bus really was walked with the same candidate set, by someone.
fn probe_stamp(state: &AppState) -> Option<Instant> {
    state.last_openfan_rescan.lock().as_ref().map(|l| l.at)
}

/// Build the candidate list on the BLOCKING pool, never on a runtime worker
/// (`OFN-x`).
///
/// [`crate::serial::real_transport::enumerate_serial_candidates`] opens no
/// *candidate* port — that is what lets the rescan cooldown ration DTR resets at
/// all, and it is load-bearing — but `serialport::available_ports()` opens the
/// devnode of any tty whose parent driver is `serial8250` *before* the
/// `ttyACM`/`ttyUSB` filter runs. That is a blocking `open(2)`, and
/// [`post_boot_adoption_loop`] runs it every
/// [`POST_BOOT_ADOPTION_INTERVAL`] for the whole adoption window, so inline it
/// would park a runtime worker on somebody's serial bus once every five seconds
/// for 60-180 s of every boot. It was reachable only on an explicit user rescan
/// before the loop existed, which is why the cadence — not the call — is what
/// changed.
///
/// The enumerator is a parameter so the property above is testable without a
/// serial bus: a test can pass a genuinely blocking double and observe that the
/// executor kept running. Production passes the real one.
///
/// **`None` means "could not enumerate", and must NOT be flattened to an empty
/// set.** The first draft returned `Vec::new()` on a `JoinError`, with a comment
/// arguing that was bounded because an empty set differs from the last probed set
/// exactly once. Both halves were wrong, and the review found them. An empty set
/// **bypasses [`crate::serial::adoption::serial_port_candidates_enumerated`]
/// entirely**, so a configured `[serial] port` — which that function prepends
/// unconditionally, without enumerating anything — is dropped from the candidate
/// list too. And a failure that alternates with success makes *every* alternation
/// a set change, so [`probe_decision`] returns `Probe::Fresh` and **refreshes**
/// the retry budget each time rather than spending it: the "an unchanged bus is
/// never re-probed" guarantee is gone, and the only remaining bound is
/// [`OPENFAN_RESCAN_COOLDOWN`] at one DTR sweep per ten seconds — 6-18 per window,
/// which is the number [`post_boot_adoption_loop`]'s own doc calls worse than the
/// twelve `OFN-b` removed. A failed enumeration is *unknown*, not *empty*, and the
/// caller skips the tick.
async fn enumerate_off_executor<E>(configured: Option<String>, enumerate: E) -> Option<Vec<String>>
where
    E: FnOnce() -> Vec<String> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        crate::serial::adoption::serial_port_candidates_enumerated(configured.as_deref(), enumerate)
    })
    .await
    {
        Ok(candidates) => Some(candidates),
        Err(e) => {
            log::warn!("serial enumeration task failed: {e} — skipping this adoption tick");
            None
        }
    }
}

/// The post-boot adoption window closed with nothing adopted, and no probe in
/// flight — so the advice is true.
///
/// Extracted because it is emitted from two arms of two different `select!`s and
/// a drifting copy of an operator-facing line is its own defect. The third
/// deadline arm — the one that fires while a probe is still running — deliberately
/// says something else (`OFN-c`): that probe is detached and may yet adopt.
fn log_adoption_window_expired(window: Duration) {
    log::debug!(
        "No OpenFanController appeared within the post-boot adoption window \
         ({window:?}) — use POST /fans/openfan/rescan if one is attached later"
    );
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
/// the thermal emergency's forced write skips an absent backend
/// (`force_present_backends`, DEC-371), so it loses its only path to those fans.
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
/// **And those retries are spent per real probe, not per tick (`OFN-w`).** The
/// budget was originally decremented before the handler was called, whose result
/// the loop discards — so a tick the handler refused on its own cooldown still
/// cost a retry. Since the loop ticks at [`POST_BOOT_ADOPTION_INTERVAL`] against
/// a longer [`OPENFAN_RESCAN_COOLDOWN`], stamped at probe END, that spent the
/// budget on refusals: 2 real probes of the intended 4 on a quick bus, 1 when a
/// slow probe overran a tick. Raising the interval to match the cooldown does NOT
/// fix it and was rejected — the stamp lands when the probe finishes, so the next
/// tick is still inside the window by however long the probe took. What the loop
/// does instead is read [`probe_stamp`] either side of the call and spend only
/// when it moved.
///
/// **Stops early on the first adoption, and on shutdown — including while a
/// probe is still running (`OFN-v`).** The probe itself cannot be cancelled and
/// is not: it is detached so a dropped caller never discards a controller that
/// was found (DEC-266). Only the waiting stops, which is what keeps a SIGTERM
/// mid-probe from adding `SHUTDOWN_TASK_TIMEOUT` to shutdown. An install that
/// lands after this task has returned is safe because
/// [`AppState::adopt_openfan_controller`] is the only route to one: it either
/// registers into a list `shutdown_sequence` will still drain, or refuses.
pub async fn post_boot_adoption_loop(
    state: Arc<AppState>,
    window: Duration,
    interval: Duration,
    shutdown: tokio::sync::watch::Receiver<bool>,
    boot_candidates: Vec<String>,
) {
    let enumerating = Arc::clone(&state);
    let probing = Arc::clone(&state);
    post_boot_adoption_loop_with(
        state,
        window,
        interval,
        shutdown,
        boot_candidates,
        // Enumerate, OFF the executor (`OFN-x`) — see `enumerate_off_executor`.
        move || {
            let configured = enumerating.running_config.serial.port.clone();
            enumerate_off_executor(
                configured,
                crate::serial::real_transport::enumerate_serial_candidates,
            )
        },
        // The result is deliberately discarded: every outcome is either already
        // logged by the handler or is the expected one. "Not found" is the normal
        // answer on a machine that has no controller, and must stay silent — it is
        // the whole point of `OFN-c`. Whether a probe actually RAN is read from
        // the cooldown stamp instead, never from this value (`OFN-w`).
        move || {
            let state = Arc::clone(&probing);
            async move {
                let _ = openfan_rescan_handler(axum::extract::State(state)).await;
            }
        },
    )
    .await
}

/// [`post_boot_adoption_loop`] with its two hardware touchpoints injected.
///
/// The seam exists for the `OFN-w` regression test and for nothing else: the
/// budget rule below is about how the loop reacts to a probe that did *not* run,
/// and there is no way to produce one from a real handler without a serial bus to
/// stage. Production passes the real enumeration and the real handler, so the
/// tested body is the shipped body rather than a model of it.
async fn post_boot_adoption_loop_with<E, EFut, P, PFut>(
    state: Arc<AppState>,
    window: Duration,
    interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    boot_candidates: Vec<String>,
    mut enumerate: E,
    mut probe: P,
) where
    E: FnMut() -> EFut,
    EFut: std::future::Future<Output = Option<Vec<String>>>,
    P: FnMut() -> PFut,
    PFut: std::future::Future<Output = ()>,
{
    // Seeded with what BOOT already probed, so an unchanged bus is never
    // re-probed at all — and a device that appeared between boot's probe and this
    // loop's first tick still reads as a change.
    let mut last_probed = boot_candidates;
    let mut retries_left: u32 = 0;
    let deadline = tokio::time::Instant::now() + window;
    let mut ticker = tokio::time::interval(interval);
    // A probe can overrun several ticks, and the default `Burst` then fires every
    // missed one back to back the instant it returns — a flurry of attempts
    // against a cooldown stamped moments earlier, which is pure waste now that
    // they no longer cost budget. `polling.rs` makes the same choice.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                log_adoption_window_expired(window);
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

        // Probe only if the world actually changed since the last thing that DID
        // probe, or if the handshake-retry budget still has something in it.
        //
        // `OFN-v`, second await. Enumeration was a synchronous call until `OFN-x`
        // moved it to the blocking pool, and a `spawn_blocking` cannot be
        // cancelled — so leaving this await unguarded put the same unobserved wait
        // back one line above the one that had just been fixed. `available_ports()`
        // opens each `serial8250` devnode (with `O_NONBLOCK`, so it cannot hang,
        // but it is not free either) and this task is drained ahead of
        // `restore_hardware()`. Returning abandons the wait, never the work: the
        // blocking task finishes on its own and its result is simply dropped.
        let candidates = tokio::select! {
            c = enumerate() => c,
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep_until(deadline) => {
                // Truthful here, unlike the probe arm below: nothing is in flight
                // that could still adopt.
                log_adoption_window_expired(window);
                return;
            }
        };
        // `None` is "could not enumerate", never "no ports" — see
        // `enumerate_off_executor`. Skipping leaves `last_probed` and
        // `retries_left` untouched, so a failure costs nothing and cannot be
        // mistaken for the bus changing.
        let Some(candidates) = candidates else {
            continue;
        };
        let decision = probe_decision(&last_probed, &candidates, retries_left);
        match decision {
            Probe::Skip => continue,
            Probe::Fresh => {
                last_probed = candidates;
                retries_left = POST_BOOT_HANDSHAKE_RETRIES;
            }
            Probe::Retry => {}
        }

        // `OFN-w`: spend a retry only for a probe that actually ran — see
        // `probe_stamp` for why "ran" here means "by someone", and why that is
        // the safe way round to be wrong.
        //
        // The handler refuses a probe of its own accord — `OPENFAN_RESCAN_COOLDOWN`
        // spaces repeats to one per ten seconds — and this loop ticks faster than
        // that. Decrementing before the call therefore spent the budget on ticks
        // that opened nothing: 2 real probes of the intended 4 on a quick bus, and
        // 1 where the probe itself outran a tick. The board those retries exist
        // for — a tty that enumerates before its firmware answers — got a single
        // attempt, and the forced write skips an absent backend
        // (`force_present_backends`, DEC-371), so a missed adoption is the thermal
        // emergency losing its only route to those fans.
        let before = probe_stamp(&state);
        // `OFN-v`: the probe is the one place this loop waits for something it
        // does not control, and it used to wait for it unconditionally — so a
        // SIGTERM landing mid-probe added up to `SHUTDOWN_TASK_TIMEOUT` (3 s) to
        // shutdown while `main` sat in the per-task drain, and a probe started
        // just under the deadline ran the window past its own end.
        //
        // Returning does NOT cancel the probe, and cannot: the handler's work is
        // an uncancellable `spawn_blocking` behind a oneshot, and it is detached
        // precisely so a dropped caller never discards a controller that was
        // found (DEC-266). All that stops is this loop *waiting* for it. That was
        // the reason this row was left unfixed — abandoning the wait meant the
        // install could land after the task had returned and register a handle
        // nothing would join, i.e. it traded a bounded delay for a more likely
        // `OFN-t`. `AdoptedTasks` removes that trade: a late install either
        // registers into a list that will still be drained, or is refused.
        tokio::select! {
            _ = probe() => {}
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep_until(deadline) => {
                // Deliberately NOT `log_adoption_window_expired` (`OFN-c`). A
                // probe is still running, it is detached, and it will install
                // whatever it finds — so telling the operator that nothing
                // appeared and that they should run a rescan would be advice to
                // act on a conclusion that has not been reached.
                log::debug!(
                    "The post-boot adoption window ({window:?}) expired while a probe was \
                     still running — that probe is detached and will still adopt a \
                     controller if it finds one"
                );
                return;
            }
        }
        if decision == Probe::Retry && probe_stamp(&state) != before {
            retries_left -= 1;
        }

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
        // DEC-385 (`TS-q`): the same 409 shape as the verify-family's
        // `stale_temperature_guard` — the machine may be cool, and the daemon
        // cannot tell; the condition clears once the poll recovers.
        Err(e @ CalibrationError::StaleTemperature { .. }) => error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope {
                error: ErrorBody {
                    code: "validation_error".into(),
                    message: format!("{e}. Retry once sensor polling recovers."),
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
/// engine's thermal force skips an absent backend (`force_present_backends`,
/// DEC-371), so the thermal emergency silently lost its reach to every OpenFan-attached fan
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
///
/// **Everything above describes `openfan_rescan_with`, which this delegates to
/// (`OFN-ab`).** The two are one function split in two, and the split is the only
/// difference: the wrapper's whole body is the pair of real hardware functions
/// the body used to name inline — `enumerate_serial_candidates` and
/// `RealSerialTransport::open`. The doc stays here because this is the route's
/// entry point and the name a reader looks for; the reasoning applies to the
/// code below it.
pub async fn openfan_rescan_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    openfan_rescan_with(
        state,
        crate::serial::real_transport::enumerate_serial_candidates,
        crate::serial::real_transport::RealSerialTransport::open,
    )
    .await
}

/// [`openfan_rescan_handler`] with its two hardware touchpoints injected
/// (`OFN-ab`).
///
/// **The seam exists so the suite can stop probing the developer's bus, and that
/// was a real defect rather than a tidiness argument.** Three integration tests
/// drove this route with `running_config` defaulted — so `port: None`,
/// auto-detect — and their load-bearing assertions could only be reached by
/// letting the probe run: one asserts [`RescanGuard`]'s drop cleared the
/// single-flight flag, which only happens when a probe really ran; another that a
/// completed probe stamps the cooldown and that the cooldown lapses rather than
/// latches. Six real probes per `cargo test` run, each opening every enumerated
/// `ttyACM*`/`ttyUSB*` and asserting DTR, which resets Arduino-class boards. On a
/// machine with hardware attached the canonical gate was therefore resetting the
/// operator's own controller on every run — the exact harm `OFN-b` removed from
/// boot and [`OPENFAN_RESCAN_COOLDOWN`] exists to ration, arriving through the
/// test suite instead of through production. One test even carried the comment
/// "No serial hardware in CI", which is true of CI and false of a workstation.
///
/// **Deliberately module-private, not `pub`.** `open` is a hardware primitive:
/// exposing a way to hand this route a different one would be exposing a way to
/// bypass `RealSerialTransport::open`'s allow-list, and a seam nothing constrains
/// is a rule the type cannot state (DEC-361, and the same reasoning that made
/// [`AppState::adopt_openfan_controller`] private). The cost is that the three
/// tests that need a fake bus live in this module's `#[cfg(test)]` block rather
/// than in `tests/ipc_integration.rs`; the ones that never probe stay there, over
/// real HTTP, so route registration and envelope serialisation keep their
/// coverage.
///
/// `enumerate` is called INLINE here, not under `spawn_blocking` — unchanged, and
/// deliberately so: `OFN-x` scoped that fix to the post-boot loop, whose cadence
/// is what made it matter, and explicitly left this call as pre-existing and off
/// any timer.
async fn openfan_rescan_with<E, O, T>(
    state: Arc<AppState>,
    enumerate: E,
    open: O,
) -> (StatusCode, Json<serde_json::Value>)
where
    E: FnOnce() -> Vec<String>,
    O: FnMut(&str, Duration) -> Result<T, crate::error::SerialError> + Send + 'static,
    T: crate::serial::transport::SerialTransport + Send + 'static,
{
    use crate::serial::adoption::{
        first_openfan_port, same_port_set, serial_port_candidates_enumerated,
    };
    use crate::serial::controller::FanController;
    use crate::serial::transport::SerialTransport;

    // `OFN-t`, first thing and before anything touches the bus. The
    // authoritative check is the one inside `adopt_openfan_controller`, which
    // holds the lock that makes it race-free; this one is an early out, and it
    // is here rather than beside the CAS because ENUMERATION itself is not free.
    // `available_ports()` opens the devnode of any tty whose parent driver is
    // `serial8250` before the ttyACM/ttyUSB filter runs, and `first_openfan_port`
    // then opens every candidate — each open asserts DTR and resets
    // Arduino-class boards. Walking somebody's bus on behalf of a daemon that is
    // already restoring hardware and exiting buys nothing at all.
    //
    // Being racy is fine HERE and only here: the watch can flip the instant
    // after this reads it, and the probe then runs to completion and is refused
    // at the install instead. What this cannot do is let an adoption through —
    // that is the other check's job.
    if *state.openfan_runtime.shutdown.borrow() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "the daemon is shutting down — no OpenFan probe was started",
            ),
        );
    }

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
    let candidates = serial_port_candidates_enumerated(configured.as_deref(), enumerate);

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
                        // `OFN-u`: "a probe", not "an OpenFan rescan". The
                        // daemon's own post-boot loop probes on the same guard,
                        // so the last probe the cooldown is rationing is often
                        // one the *daemon* made — and the old wording
                        // ("an OpenFan rescan ... was attempted") told the user
                        // they had done something they had not. `RescanGuard`'s
                        // drop re-stamps `last_openfan_rescan` with the new
                        // candidate set, so the loop can even consume the
                        // set-change exemption a user's "plug it in and click
                        // rescan" was meant to get. Naming the actor neutrally is
                        // the whole fix; the rationing itself is unchanged.
                        message: format!(
                            "a probe over the same ports was attempted \
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
            let mut open = open;
            first_openfan_port(&candidates, configured.as_deref(), timeout, |p| {
                open(p, timeout)
            })
        })
        .await;

        let outcome = match probe {
            Ok(Some((port, transport))) => {
                let boxed: Box<dyn SerialTransport + Send> = Box::new(transport);
                let shared = Arc::new(parking_lot::Mutex::new(boxed));
                let ctrl =
                    FanController::new_shared(shared.clone(), task_state.cache.clone(), timeout);

                // The install, the DEC-266 conditional and the 277-c handle
                // registration all happen inside `adopt_openfan_controller`,
                // under ONE lock (`OFN-t`). They used to be three statements
                // here, and the shutdown check that belongs with them was
                // nowhere at all — `main` drains the handle list *before*
                // `finish_shutdown` sets the shutdown watch, so an adoption
                // completing in between registered a poll loop that nothing
                // would ever join, and one completing later could install a
                // controller after `restore_hardware()` had already run.
                let rt = task_state.openfan_runtime.clone();
                let poll_cache = task_state.cache.clone();
                match task_state.adopt_openfan_controller(ctrl, || {
                    tokio::spawn(async move {
                        crate::polling::openfan_poll_loop(
                            poll_cache,
                            shared,
                            rt.timeout,
                            rt.interval,
                            rt.shutdown,
                        )
                        .await;
                    })
                }) {
                    AdoptOutcome::Adopted => {
                        // Deliberately no trigger in the wording: this path is reached both by
                        // `POST /fans/openfan/rescan` and by the post-boot adoption loop,
                        // and the old "via rescan" told an operator they had performed an
                        // action they had not (`OFN-c` is a log-honesty register). Each
                        // caller records its own context.
                        log::info!("OpenFanController adopted on {port}");
                        RescanOutcome::Adopted(port)
                    }
                    AdoptOutcome::AlreadyAdopted => {
                        log::warn!(
                            "OpenFanController found on {port} but another rescan had already \
                             adopted one — discarding this probe rather than replacing it"
                        );
                        RescanOutcome::AlreadyAdopted
                    }
                    AdoptOutcome::ShuttingDown => {
                        // Found, identified, and deliberately discarded. Logged at
                        // info rather than warn: it is the correct outcome, not a
                        // fault, and the operator asked for the shutdown.
                        log::info!(
                            "OpenFanController found on {port} while the daemon was shutting \
                             down — not adopted; it will be picked up on the next start"
                        );
                        RescanOutcome::ShuttingDown
                    }
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
        // Deliberately NOT reported as `NotFound`: a controller was found and
        // identified, and telling the operator otherwise is the log/report
        // dishonesty the `OFN-*` register exists for (`OFN-c`). Same 503
        // `hardware_unavailable` code — this is retryable and self-clearing on
        // the next start — with a message that says which of the two happened.
        Ok(RescanOutcome::ShuttingDown) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "an OpenFanController was found but the daemon is shutting down — \
                 it will be adopted on the next start",
            ),
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
    /// Registration had closed before the probe finished — the daemon is
    /// shutting down, so nothing was installed (`OFN-t`).
    ShuttingDown,
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
    fn an_unchanged_candidate_set_stops_being_probed_once_its_retries_are_spent() {
        // The DISCRIMINATING arm, and the defect the OFN-r design replaced. The
        // first draft leaned on OPENFAN_RESCAN_COOLDOWN to make repeat ticks
        // free; that predicate is `elapsed < COOLDOWN && same_port_set(..)`, an
        // AND, so it only SPACES repeats — every tick past the cooldown would
        // have opened every unrelated tty again, asserting DTR and resetting
        // Arduino-class boards ~6 times a boot.
        let last = vec!["/dev/ttyACM0".to_string()];
        let same = vec!["/dev/ttyACM0".to_string()];

        assert_eq!(
            probe_decision(&last, &same, POST_BOOT_HANDSHAKE_RETRIES),
            Probe::Retry,
            "an unchanged bus is still worth a retry while the budget lasts"
        );
        assert_eq!(
            probe_decision(&last, &same, 0),
            Probe::Skip,
            "an unchanged bus with no budget left must never be probed again — \
             each probe is a DTR reset of somebody's Arduino"
        );
    }

    #[test]
    fn a_changed_candidate_set_is_always_probed() {
        // The opposite arm: without it, a `probe_decision` that always returned
        // `Skip` would pass the test above, and a controller plugged in during
        // the window would never be adopted.
        let last = vec!["/dev/ttyACM0".to_string()];
        let changed = vec!["/dev/ttyACM0".to_string(), "/dev/ttyACM1".to_string()];

        assert_eq!(
            probe_decision(&last, &changed, 0),
            Probe::Fresh,
            "a newly appeared tty must be probed at once, even with no retries left"
        );
    }

    #[test]
    fn a_fresh_probe_does_not_come_out_of_the_retry_budget() {
        // Why the budget is not zero, and why the fresh probe is a separate arm:
        // a board can enumerate its tty a moment before it will answer the
        // DEC-250 handshake, so the first probe after a change can fail on a
        // device that is genuinely there. `Fresh` and `Retry` are distinct
        // precisely so the loop knows which of the two it is spending.
        let last: Vec<String> = Vec::new();
        let appeared = vec!["/dev/ttyACM0".to_string()];
        assert_eq!(probe_decision(&last, &appeared, 0), Probe::Fresh);
        assert_ne!(
            probe_decision(&last, &appeared, 0),
            Probe::Retry,
            "the probe that follows a change is not one of the retries; conflating \
             them costs the late-answering board one of its attempts"
        );
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

    /// Drive the loop with a probe double and report (attempts, real probes).
    ///
    /// The double models the ONE contract the loop reads: a real probe stamps
    /// `last_openfan_rescan` when its task ends, and every refusal in
    /// `openfan_rescan_handler` returns before `RescanGuard` is built, so it
    /// leaves the stamp alone. That contract is pinned independently at the
    /// handler by `openfan_rescan_cooldown_does_not_advance_the_probe_stamp`
    /// (`tests/ipc_integration.rs`) — a double is only honest while the half it
    /// stands in for is tested too.
    ///
    /// `refuse_ratio` is how many attempts out of each group of that size are
    /// refused: 3 means one real probe then two refusals, the spacing this loop
    /// actually meets against a 10 s cooldown at a 5 s tick. 1 means every
    /// attempt runs.
    async fn drive_loop_with_probe(window: Duration, refuse_ratio: u32) -> (u32, u32) {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx.clone());
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let reals = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let probe_state = Arc::clone(&state);
        let a = Arc::clone(&attempts);
        let r = Arc::clone(&reals);

        let finished = tokio::time::timeout(
            window + Duration::from_secs(5),
            post_boot_adoption_loop_with(
                Arc::clone(&state),
                window,
                Duration::from_millis(2),
                rx,
                vec!["/dev/ttyBOOT".to_string()],
                // The bus changed once against what boot probed, then never
                // again — a controller attached during the window.
                || async { Some(vec!["/dev/ttyBOOT".to_string(), "/dev/ttyNEW".to_string()]) },
                move || {
                    let st = Arc::clone(&probe_state);
                    let a = Arc::clone(&a);
                    let r = Arc::clone(&r);
                    async move {
                        if a.fetch_add(1, Ordering::SeqCst)
                            .is_multiple_of(refuse_ratio)
                        {
                            r.fetch_add(1, Ordering::SeqCst);
                            *st.last_openfan_rescan.lock() = Some(LastRescan {
                                at: Instant::now(),
                                candidates: vec!["/dev/ttyNEW".to_string()],
                            });
                        }
                    }
                },
            ),
        )
        .await;
        assert!(finished.is_ok(), "the loop must end at its window");
        (
            attempts.load(Ordering::SeqCst),
            reals.load(Ordering::SeqCst),
        )
    }

    /// `OFN-w`: a probe the handler REFUSED must not cost a handshake retry.
    ///
    /// The defect this replaced spent the budget in `should_probe` *before*
    /// calling the handler, whose result the loop discards. The handler refuses
    /// on a cooldown of its own, and this loop ticks faster than that cooldown —
    /// so the four intended probes became two on a quick bus, and one where the
    /// probe itself outran a tick. The board those retries exist for, whose tty
    /// enumerates before its firmware answers, got a single attempt; and
    /// the forced write skips an absent backend (`force_present_backends`,
    /// DEC-371), so a missed adoption is the thermal emergency losing its route
    /// to those fans.
    #[tokio::test]
    async fn refused_probes_do_not_spend_the_handshake_retry_budget() {
        let (attempted, ran) = drive_loop_with_probe(Duration::from_millis(400), 3).await;

        // Precondition on the fixture: the situation this test is about must
        // actually have arisen. With nothing refused there is no accounting for
        // the loop to get wrong, and the assertion below would hold against the
        // defect it exists to catch.
        assert!(
            attempted > ran,
            "precondition: no probe was refused ({attempted} attempts, {ran} ran), \
             so the retry accounting was never exercised"
        );
        assert_eq!(
            ran,
            1 + POST_BOOT_HANDSHAKE_RETRIES,
            "a refused probe opened no tty, so it must not cost a handshake retry: \
             the fresh probe and all {POST_BOOT_HANDSHAKE_RETRIES} retries must \
             reach the bus ({attempted} attempts made, {ran} ran)"
        );
    }

    /// The arm that must not regress while fixing the one above: the budget still
    /// BOUNDS real probes.
    ///
    /// Every probe opens whatever ttys the host has and asserts DTR, which resets
    /// Arduino-class boards. A spend rule that never fired would make this loop
    /// probe once per tick for the whole window — `OFN-b`'s twelve resets a boot,
    /// back again and worse.
    #[tokio::test]
    async fn the_handshake_retry_budget_still_bounds_real_probes() {
        // Every attempt runs, over a window with room for far more ticks than the
        // budget allows.
        let (attempted, ran) = drive_loop_with_probe(Duration::from_millis(400), 1).await;

        assert_eq!(
            ran,
            1 + POST_BOOT_HANDSHAKE_RETRIES,
            "an unchanged bus must be probed for the fresh probe plus the budget \
             and then never again ({attempted} attempts, {ran} ran)"
        );
        assert_eq!(
            attempted, ran,
            "with nothing refused, every attempt must have been a real probe — \
             otherwise this test is measuring the refusal path, not the bound"
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
            adopted_poll_tasks: Arc::new(parking_lot::Mutex::new(Default::default())),
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

    // ── `TS-q` / DEC-385: calibration refuses a stale temperature source ─────

    /// Records every frame written, answers nothing — a controller that would
    /// show any write the handler made.
    struct RecordingTransport(Arc<parking_lot::Mutex<Vec<String>>>);
    impl crate::serial::transport::SerialTransport for RecordingTransport {
        fn write_line(&mut self, data: &str) -> Result<(), crate::error::SerialError> {
            self.0.lock().push(data.to_string());
            Ok(())
        }
        fn read_line(&mut self, _timeout: Duration) -> Result<String, crate::error::SerialError> {
            Err(crate::error::SerialError::Timeout { timeout_ms: 1 })
        }
    }

    /// [SAFETY] TS-q at the CALL SITE: `POST /fans/openfan/{ch}/calibrate` with
    /// the poll wedged on a hot reading answers the verify family's `409
    /// validation_error` (retryable) and writes NOTHING to the controller. The
    /// sweep's own tests prove the check exists; only this proves the endpoint
    /// maps it — a missing arm would be a 500 or a 503.
    ///
    /// Opposite arm: the same reading fresh reaches the wire, so the refusal is
    /// the reading's age and nothing else about the fixture.
    #[tokio::test]
    async fn calibrate_refuses_a_stale_temperature_source_before_any_frame() {
        let stale = crate::constants::DIAGNOSTIC_TEMP_MAX_AGE + Duration::from_secs(60);
        for (age, refused) in [(stale, true), (Duration::ZERO, false)] {
            let (_tx, rx) = tokio::sync::watch::channel(false);
            let state = adoption_state(rx);
            let frames = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let transport: Box<dyn crate::serial::transport::SerialTransport + Send> =
                Box::new(RecordingTransport(frames.clone()));
            let ctrl = crate::serial::controller::FanController::new_shared(
                Arc::new(parking_lot::Mutex::new(transport)),
                state.cache.clone(),
                Duration::from_millis(1),
            );
            *state.fan_controller.write() = Some(Arc::new(parking_lot::Mutex::new(ctrl)));
            state
                .cache
                .update_sensors(vec![crate::health::state::CachedSensorReading {
                    id: "cpu".into(),
                    kind: crate::hwmon::types::SensorKind::CpuTemp,
                    label: "Tctl".into(),
                    value_c: 84.0,
                    source: crate::health::state::DeviceLabel::Hwmon,
                    updated_at: std::time::Instant::now() - age,
                    rate_c_per_s: None,
                    session_min_c: None,
                    session_max_c: None,
                    chip_name: "k10temp".into(),
                    temp_type: None,
                    thresholds: None,
                }]);

            let (status, Json(body)) = calibrate_openfan_handler(
                State(state.clone()),
                Path(0),
                Json(crate::api::calibration::CalibrationRequest {
                    steps: 2,
                    hold_seconds: 2,
                }),
            )
            .await;

            if refused {
                assert_eq!(status, StatusCode::CONFLICT, "{body}");
                assert_eq!(body["error"]["code"], "validation_error", "{body}");
                assert_eq!(body["error"]["retryable"], true, "{body}");
                assert!(
                    frames.lock().is_empty(),
                    "a refused calibration wrote to the controller: {:?}",
                    frames.lock()
                );
            } else {
                assert_ne!(
                    status,
                    StatusCode::CONFLICT,
                    "a FRESH reading was refused: {body}"
                );
                assert!(
                    !frames.lock().is_empty(),
                    "precondition: with a fresh reading the sweep reaches the wire"
                );
            }
        }
    }

    // ── `OFN-t`: adoption racing shutdown ────────────────────────────────────

    /// A transport that answers nothing. Enough to build a `FanController`,
    /// which is all the gate tests need — none of them talks to it.
    struct SilentTransport;
    impl crate::serial::transport::SerialTransport for SilentTransport {
        fn write_line(&mut self, _data: &str) -> Result<(), crate::error::SerialError> {
            Ok(())
        }
        fn read_line(&mut self, _timeout: Duration) -> Result<String, crate::error::SerialError> {
            Err(crate::error::SerialError::Timeout { timeout_ms: 1 })
        }
    }

    fn silent_controller(state: &AppState) -> crate::serial::controller::FanController {
        let boxed: Box<dyn crate::serial::transport::SerialTransport + Send> =
            Box::new(SilentTransport);
        crate::serial::controller::FanController::new_shared(
            Arc::new(parking_lot::Mutex::new(boxed)),
            state.cache.clone(),
            Duration::from_millis(1),
        )
    }

    /// A poll-loop stand-in: parks until the runtime drops it, so a handle that
    /// leaks is a handle that would genuinely have needed joining.
    fn spawn_stub_loop() -> tokio::task::JoinHandle<()> {
        tokio::spawn(async {
            std::future::pending::<()>().await;
        })
    }

    /// The DISCRIMINATING arm: once shutdown has TAKEN the list, an adoption must
    /// not install and must not register.
    ///
    /// This is the defect. `main` drains `adopted_poll_tasks` into `task_handles`
    /// and only then calls `finish_shutdown`, whose first act is to set the
    /// shutdown watch — so the drain provably runs before the signal, and nothing
    /// on the install path consulted a shutdown signal at any point. A controller
    /// probed in that window was installed, its poll loop spawned, and its handle
    /// pushed into a list nothing would read again; one probed later could be
    /// installed after `restore_hardware()` had already run.
    #[tokio::test]
    async fn an_adoption_after_the_drain_is_refused_rather_than_leaking_an_unjoined_loop() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx);

        // Exactly what `main` does at shutdown assembly.
        let drained = state.adopted_poll_tasks.lock().close_and_drain();
        assert!(
            drained.is_empty(),
            "precondition: nothing was adopted before the drain, so anything the \
             drain returns came from somewhere this test does not model"
        );

        let outcome = state.adopt_openfan_controller(silent_controller(&state), spawn_stub_loop);

        assert_eq!(
            outcome,
            AdoptOutcome::ShuttingDown,
            "an adoption completing after the drain must be refused — registering \
             would put a poll handle in a list nothing will ever read again"
        );
        assert!(
            state.openfan().is_none(),
            "nothing may be installed once shutdown has taken the list: the engine \
             is stopped and `restore_hardware()` may already have run"
        );
        assert!(
            state.adopted_poll_tasks.lock().is_empty(),
            "a refused adoption must register no handle"
        );
    }

    /// The OPPOSITE arm, without which the test above passes against an
    /// `adopt_openfan_controller` that refuses unconditionally — and the daemon
    /// would then never adopt a controller at all.
    #[tokio::test]
    async fn an_adoption_before_the_drain_registers_a_handle_the_drain_takes() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx);

        let outcome = state.adopt_openfan_controller(silent_controller(&state), spawn_stub_loop);

        assert_eq!(outcome, AdoptOutcome::Adopted);
        assert!(
            state.openfan().is_some(),
            "a controller adopted before shutdown must be installed, or the engine \
             — and with it `force_all_with_floor` — has no OpenFan backend"
        );

        let drained = state.adopted_poll_tasks.lock().close_and_drain();
        assert_eq!(
            drained.len(),
            1,
            "the poll loop must be in the list `shutdown_sequence` joins (277-c)"
        );
        assert!(
            state.adopted_poll_tasks.lock().is_closed(),
            "close_and_drain must CLOSE as well as take — taking alone leaves the \
             window this fix exists to remove"
        );
        for h in drained {
            h.abort();
        }
    }

    /// DEC-266's arm, which must not regress while the gate is added: a second
    /// adoption never replaces a live controller.
    #[tokio::test]
    async fn a_second_adoption_does_not_replace_a_live_controller() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx);

        assert_eq!(
            state.adopt_openfan_controller(silent_controller(&state), spawn_stub_loop),
            AdoptOutcome::Adopted
        );
        let first = state.openfan().expect("the first adoption installed");

        assert_eq!(
            state.adopt_openfan_controller(silent_controller(&state), spawn_stub_loop),
            AdoptOutcome::AlreadyAdopted,
            "the loser of the install race must discard its probe, not overwrite \
             the winner — the engine only re-reads the slot while it has no \
             backend, so it would keep writing through the first controller while \
             the second's poll loop read a different transport"
        );
        assert!(
            Arc::ptr_eq(&first, &state.openfan().expect("still installed")),
            "the installed controller must still be the FIRST one"
        );
        let drained = state.adopted_poll_tasks.lock().close_and_drain();
        assert_eq!(
            drained.len(),
            1,
            "the refused adoption must not have registered a second poll loop"
        );
        for h in drained {
            h.abort();
        }
    }

    /// The handler's early out (`OFN-t`): a rescan arriving during shutdown is
    /// refused before anything touches the bus.
    ///
    /// The message is the discriminator, and it has to be: the only other 503 on
    /// this route says "no OpenFanController found", which would be a lie about a
    /// probe that never ran. `last_openfan_rescan` staying unset is what proves
    /// no probe ran — `RescanGuard` stamps it when a probe task ends, and every
    /// refusal returns before that guard is built.
    #[tokio::test]
    async fn a_rescan_during_shutdown_is_refused_without_probing() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx);
        tx.send(true).expect("the receiver is alive in AppState");

        let (status, body) = openfan_rescan_handler(axum::extract::State(state.clone())).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let message = body["error"]["message"]
            .as_str()
            .expect("the error envelope carries a message")
            .to_string();
        assert!(
            message.contains("shutting down"),
            "the refusal must say WHICH refusal it is; got {message:?}"
        );
        assert!(
            !message.contains("not found") && !message.contains("No OpenFanController"),
            "reporting a shutdown refusal as 'not found' tells the operator a probe \
             ran and came back empty, which is the log-honesty defect `OFN-c` is \
             about; got {message:?}"
        );
        assert!(
            state.last_openfan_rescan.lock().is_none(),
            "no probe may have run — every probe opens a tty and asserts DTR, which \
             resets Arduino-class boards, and this daemon is already exiting"
        );
    }

    // ── `OFN-v`: the loop must not WAIT for a probe it no longer needs ───────

    /// Run the loop with a probe that takes `probe_for`, and report how long the
    /// loop took to return once `after_probe_starts` had elapsed and `act` ran.
    ///
    /// Every wait is a bounded poll against a deadline, never a bare sleep: a
    /// missed window must make this red, not green (`CLAUDE.md`, tokio trap 3).
    async fn time_loop_exit_during_a_probe(
        window: Duration,
        probe_for: Duration,
        act: impl FnOnce(&tokio::sync::watch::Sender<bool>),
    ) -> (Duration, u32) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx.clone());
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let a = Arc::clone(&attempts);
        let handle = tokio::spawn(post_boot_adoption_loop_with(
            Arc::clone(&state),
            window,
            Duration::from_millis(2),
            rx,
            vec!["/dev/ttyBOOT".to_string()],
            || async { Some(vec!["/dev/ttyNEW".to_string()]) },
            move || {
                let a = Arc::clone(&a);
                async move {
                    a.fetch_add(1, Ordering::SeqCst);
                    // Self-releasing, so a test that fails an assertion cannot
                    // leave an unbounded wedge behind and turn a red test into a
                    // hung CI job (tokio trap 3).
                    tokio::time::sleep(probe_for).await;
                }
            },
        ));

        // Bounded poll until the probe is genuinely in flight. Without this the
        // measurement below could be of a loop that had not yet started waiting,
        // which is the case the row is not about.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "precondition: no probe ever started, so nothing was measured"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        act(&tx);
        let started = tokio::time::Instant::now();
        let finished = tokio::time::timeout(probe_for * 3, handle).await;
        assert!(
            finished.is_ok(),
            "the loop never returned at all — it outlasted three times the probe"
        );
        (started.elapsed(), attempts.load(Ordering::SeqCst))
    }

    /// `OFN-v`: a SIGTERM landing mid-probe must not add the probe's remaining
    /// time to shutdown.
    ///
    /// The loop is in `main`'s `task_handles`, so `shutdown_sequence` joins it
    /// with a `timeout(SHUTDOWN_TASK_TIMEOUT)` — waiting for the probe therefore
    /// delayed the hardware restore by up to 3 s. Returning does NOT cancel the
    /// probe and is not meant to: it is detached precisely so a dropped caller
    /// never discards a controller that was found (DEC-266), and a late install
    /// is safe because `adopt_openfan_controller` either registers into a list
    /// that will still be drained or refuses.
    ///
    /// The window is long, so the deadline arm cannot be what ends this loop —
    /// only the shutdown arm can.
    #[tokio::test]
    async fn a_shutdown_during_a_probe_is_not_waited_out() {
        let probe_for = Duration::from_millis(600);
        let (elapsed, attempts) =
            time_loop_exit_during_a_probe(Duration::from_secs(60), probe_for, |tx| {
                tx.send(true).expect("the loop holds a receiver");
            })
            .await;

        assert!(attempts >= 1, "precondition: the probe must have started");
        assert!(
            elapsed < probe_for / 2,
            "the loop waited out the probe before honouring shutdown ({elapsed:?} of \
             a {probe_for:?} probe) — that time is added to `shutdown_sequence`'s \
             per-task drain, and the hardware restore is behind it"
        );
    }

    /// `OFN-v`, the other arm: a probe started just under the deadline must not
    /// carry the loop past its own window.
    ///
    /// Shutdown is never signalled here, so the shutdown arm cannot be what ends
    /// it — this measures the deadline arm specifically.
    #[tokio::test]
    async fn a_probe_that_outruns_the_window_does_not_extend_it() {
        let probe_for = Duration::from_millis(600);
        let (elapsed, attempts) =
            time_loop_exit_during_a_probe(Duration::from_millis(30), probe_for, |_tx| {}).await;

        assert!(attempts >= 1, "precondition: the probe must have started");
        assert!(
            elapsed < probe_for / 2,
            "the loop ran past its adoption window waiting for a probe ({elapsed:?} \
             of a {probe_for:?} probe) — the window is what bounds how long this \
             task stays alive into shutdown"
        );
    }

    /// `OFN-x` round 2: a failed enumeration must SKIP the tick, never read as
    /// "no ports".
    ///
    /// The first draft returned `Vec::new()` on a `JoinError` and argued it was
    /// bounded. It is not: an empty set bypasses
    /// `serial_port_candidates_enumerated`, dropping a configured `[serial] port`
    /// as well, and a failure that alternates with success makes every alternation
    /// a `Probe::Fresh` — which REFRESHES the retry budget instead of spending it.
    /// The bound would then be `OPENFAN_RESCAN_COOLDOWN` alone: one DTR sweep per
    /// ten seconds, 6-18 per window, which `post_boot_adoption_loop`'s own doc
    /// calls worse than the twelve `OFN-b` removed.
    ///
    /// Alternating is what discriminates. A *constant* failure would also probe
    /// zero times under the old `Vec::new()` once its four attempts were spent, so
    /// a test that never enumerated successfully would pass against the defect.
    #[tokio::test]
    async fn an_enumeration_that_fails_never_looks_like_the_bus_changing() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx.clone());
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let a = Arc::clone(&attempts);
        let c = Arc::clone(&calls);
        let finished = tokio::time::timeout(
            Duration::from_secs(5),
            post_boot_adoption_loop_with(
                Arc::clone(&state),
                Duration::from_millis(300),
                Duration::from_millis(2),
                rx,
                // Exactly what boot probed, so a successful enumeration is NOT a
                // change and must never be probed — the only thing that can make
                // this loop probe is a failure being mistaken for a change.
                vec!["/dev/ttyBOOT".to_string()],
                move || {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if n.is_multiple_of(2) {
                            None
                        } else {
                            Some(vec!["/dev/ttyBOOT".to_string()])
                        }
                    }
                },
                move || {
                    let a = Arc::clone(&a);
                    async move {
                        a.fetch_add(1, Ordering::SeqCst);
                    }
                },
            ),
        )
        .await;

        assert!(finished.is_ok(), "the loop must end at its window");
        let calls = calls.load(Ordering::SeqCst);
        assert!(
            calls >= 4,
            "precondition: the alternation must actually have run several times \
             ({calls} enumerations) — with too few, nothing was measured"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            0,
            "a failed enumeration must skip the tick. Reading it as an empty port \
             set makes every alternation a `Fresh` decision, which refreshes the \
             handshake budget rather than spending it — and every probe opens each \
             candidate tty and asserts DTR, resetting Arduino-class boards \
             ({calls} enumerations)"
        );
    }

    /// `OFN-v`, third arm: a shutdown landing while ENUMERATION is in flight must
    /// not be waited out either.
    ///
    /// Enumeration was a synchronous call until `OFN-x` moved it to the blocking
    /// pool, where it cannot be cancelled — so leaving its await unguarded put the
    /// same unobserved wait one line above the one that had just been fixed. This
    /// task is drained by `shutdown_sequence` ahead of `restore_hardware()`.
    #[tokio::test]
    async fn a_shutdown_during_enumeration_is_not_waited_out() {
        let enumerate_for = Duration::from_millis(600);
        let (tx, rx) = tokio::sync::watch::channel(false);
        let state = adoption_state(rx.clone());
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let c = Arc::clone(&calls);
        let handle = tokio::spawn(post_boot_adoption_loop_with(
            Arc::clone(&state),
            Duration::from_secs(60),
            Duration::from_millis(2),
            rx,
            vec!["/dev/ttyBOOT".to_string()],
            move || {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    // Self-releasing, so a failed assertion cannot leave an
                    // unbounded wedge and turn a red test into a hung CI job.
                    tokio::time::sleep(enumerate_for).await;
                    Some(vec!["/dev/ttyBOOT".to_string()])
                }
            },
            || async {
                unreachable!("an unchanged bus must never be probed by this test");
            },
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "precondition: enumeration never started, so nothing was measured"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        tx.send(true).expect("the loop holds a receiver");
        let started = tokio::time::Instant::now();
        let finished = tokio::time::timeout(enumerate_for * 3, handle).await;
        assert!(finished.is_ok(), "the loop never returned at all");
        let elapsed = started.elapsed();
        assert!(
            elapsed < enumerate_for / 2,
            "the loop waited out the enumeration before honouring shutdown \
             ({elapsed:?} of a {enumerate_for:?} enumeration) — that time is added \
             to `shutdown_sequence`'s per-task drain, and the hardware restore is \
             behind it"
        );
    }

    // ── `OFN-x`: enumeration must not sit on a runtime worker ────────────────

    /// A CURRENT-THREAD runtime, so an enumeration left inline starves the
    /// executor and the ordering below inverts.
    ///
    /// `available_ports()` opens the devnode of any `serial8250` tty before the
    /// ttyACM/ttyUSB filter runs — a real blocking `open(2)`, which the loop runs
    /// every five seconds for the whole adoption window. The double blocks the
    /// same way (`std::thread::sleep`, not `tokio::time::sleep`, which would
    /// yield and prove nothing).
    ///
    /// **The first draft used `flavor = "multi_thread", worker_threads = 1` and
    /// passed with the fix deleted** — measured, by the fix-out-must-fail check.
    /// A multi-thread runtime runs the test body on the CALLING thread via
    /// `block_on` and gives spawned tasks the worker, so the observer had a
    /// thread of its own whether or not the enumeration blocked, and the
    /// ordering held for a reason that had nothing to do with the fix. On
    /// `current_thread` the body and the observer share one thread, which is what
    /// makes the ordering discriminate. `spawn_blocking` still uses the separate
    /// blocking pool there, so the fixed path is unaffected.
    #[tokio::test]
    async fn enumeration_does_not_block_the_executor() {
        let order: Arc<parking_lot::Mutex<Vec<&'static str>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

        let observer_order = Arc::clone(&order);
        let observer = tokio::spawn(async move {
            // Only reachable if the executor is still able to run tasks WHILE the
            // enumeration is in flight.
            let _ = started_rx.await;
            observer_order.lock().push("executor-ran");
        });

        let candidates = enumerate_off_executor(None, move || {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(300));
            vec!["/dev/ttyACM7".to_string()]
        })
        .await;
        let candidates = candidates.expect("a successful enumeration is Some");
        order.lock().push("enumerate-returned");
        let _ = observer.await;

        assert_eq!(
            candidates,
            vec!["/dev/ttyACM7".to_string()],
            "precondition: the enumerator's result must still reach the caller — \
             moving it off the executor must not lose it"
        );
        assert_eq!(
            order.lock().as_slice(),
            ["executor-ran", "enumerate-returned"],
            "the executor was parked for the whole enumeration: with the call \
             inline, nothing else on this runtime can run until it returns"
        );
    }

    /// The CALL SITE, which the test above cannot see: the production loop must
    /// actually route its enumeration through `enumerate_off_executor`, and the
    /// handler's shutdown check must come before anything that touches the bus.
    ///
    /// Both are one-line facts about where a call sits, and neither can be
    /// observed at runtime without a real serial bus — the whole point is that
    /// the real enumerator must never run in a test. Scoped to production source
    /// (everything before the test module) so this guard cannot match its own
    /// explanation, which is how the `polling.rs` precedent failed.
    #[test]
    fn the_production_paths_enumerate_off_the_executor_and_refuse_during_shutdown() {
        let whole = include_str!("openfan.rs");
        let src = whole
            .split_once("\n#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("openfan.rs has a test module");

        let loop_at = src
            .find("pub async fn post_boot_adoption_loop(")
            .expect("the post-boot loop exists");
        let seam_at = src
            .find("async fn post_boot_adoption_loop_with")
            .expect("the injected-seam variant exists");
        assert!(
            src[loop_at..seam_at].contains("enumerate_off_executor("),
            "the production loop must enumerate through the blocking-pool helper; \
             calling the enumerator inline parks a runtime worker on the serial \
             bus once every tick for the whole adoption window"
        );

        // Anchored on the INJECTED variant, not the thin `openfan_rescan_handler`
        // wrapper that delegates to it (`OFN-ab`): the wrapper contains neither
        // the check nor the enumeration, so anchoring there would assert an
        // ordering over a window that merely happens to follow it in the file.
        let handler_at = src
            .find("async fn openfan_rescan_with<")
            .expect("the rescan handler's injected variant exists");

        // ...but re-anchoring narrowed this guard's reach, and the wrapper is now
        // the one place in the file whose whole job is NAMING the two hardware
        // functions — with nothing observing that it names the right ones. No test
        // can: the three that drive the route pass their own pair, and the one
        // that drives `openfan_rescan_handler` returns at the shutdown check
        // before the opener is reached. So a later edit could swap
        // `RealSerialTransport::open` for a raw opener and take
        // `is_allowed_serial_path` — the only path guard on a client-supplied
        // `serial.port` — off this route with the whole suite green. Before the
        // split this could not be wrong by construction, because there was no
        // binding to get wrong.
        //
        // Comments are STRIPPED first: both function names appear in the prose of
        // the doc comment that sits between the wrapper and the seam, so an
        // unstripped window would satisfy every assertion below by quoting itself
        // — the `polling.rs` self-match trap, and the reason `main.rs`'s sibling
        // guard strips too.
        let wrapper_at = src
            .find("pub async fn openfan_rescan_handler(")
            .expect("the rescan route's entry point exists");
        let wrapper_code: String = src[wrapper_at..handler_at]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            wrapper_code.contains("RealSerialTransport::open"),
            "the shipped route must pass the allow-listed opener; swapping it here \
             removes `is_allowed_serial_path` from the only endpoint that opens a \
             client-named serial path"
        );
        assert!(
            wrapper_code.contains("enumerate_serial_candidates"),
            "the shipped route must pass the NON-opening enumerator — the one whose \
             result the DEC-291 cooldown compares"
        );
        assert!(
            !wrapper_code.contains("RealSerialTransport::open("),
            "the opener must be PASSED as a fn item, not called in the wrapper: an \
             open here would happen before the seam's shutdown refusal"
        );
        assert!(
            !wrapper_code.contains("serial_port_candidates_enumerated("),
            "the wrapper must not touch the bus before delegating. It is the natural \
             place to add one now that it is where hardware facts are named, and \
             anything here runs BEFORE the `OFN-t` shutdown refusal — enumerating on \
             behalf of a daemon that is already exiting"
        );

        let handler = &src[handler_at..];
        let refusal_at = handler
            .find(".shutdown.borrow()")
            .expect("the handler must consult the shutdown watch");
        let enumerate_at = handler
            .find("serial_port_candidates_enumerated(")
            .expect("the handler must build its candidate list");
        assert!(
            refusal_at < enumerate_at,
            "the shutdown refusal must come BEFORE the candidate list is built — \
             building it opens every serial8250 tty, and probing opens every \
             candidate, each open asserting DTR and resetting Arduino-class \
             boards on behalf of a daemon that is already exiting"
        );
    }

    // ── `OFN-ab`: the rescan route, driven over a FAKE bus ───────────────────
    //
    // These three moved here from `tests/ipc_integration.rs`, where they drove
    // the real route over a UDS socket with `running_config` defaulted — so
    // `port: None`, auto-detect — and every load-bearing assertion could only be
    // reached by letting the probe run: `RescanGuard`'s drop only happens when a
    // probe really ran, and "a completed probe stamps the cooldown" is not
    // observable without one. Six real probes per `cargo test` run between them,
    // each opening every enumerated `ttyACM*`/`ttyUSB*` and asserting DTR, which
    // resets Arduino-class boards. On a workstation with hardware attached the
    // canonical gate was resetting the operator's own controller on every run.
    //
    // They live here rather than there because `openfan_rescan_with` is
    // module-private and must stay so — `open` is a hardware primitive, and a
    // `pub` seam handing this route a different one is a `pub` way around
    // `RealSerialTransport::open`'s allow-list. The four rescan tests that never
    // probe stayed in `ipc_integration.rs`, over real HTTP, so route
    // registration and envelope serialisation keep their coverage.

    /// Ports that exist only in this test file.
    fn fake_enumerate() -> Vec<String> {
        vec!["/dev/ttyFAKE0".to_string(), "/dev/ttyFAKE1".to_string()]
    }

    /// Opens anything and identifies as nothing: [`SilentTransport::read_line`]
    /// times out, so `verify_openfan_identity`'s `ReadAllRpm` fails and the
    /// handler concludes `NotFound` — the same answer the real bus gives a
    /// machine with no controller, which is what all three of these tests
    /// assumed and none of them could previously guarantee.
    ///
    /// Records what it opened, so each test can assert as a **precondition** that
    /// the probe really ran *and that it ran on the fake bus*. Without that, a
    /// handler that silently stopped probing would satisfy every assertion below.
    fn recording_opener(
        opened: Arc<parking_lot::Mutex<Vec<String>>>,
    ) -> impl FnMut(&str, Duration) -> Result<SilentTransport, crate::error::SerialError> + Send + 'static
    {
        move |path, _timeout| {
            opened.lock().push(path.to_string());
            Ok(SilentTransport)
        }
    }

    fn rescan_state() -> Arc<AppState> {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        adoption_state(rx)
    }

    /// DEC-266. The single-flight flag is set by a CAS in the handler and cleared
    /// by a `Drop` guard that lives in a DETACHED task, so a client disconnect
    /// cannot release it early while the uncancellable probe still holds a tty.
    /// The risk of moving it there is the opposite failure: never releasing it.
    /// That would wedge this route at 409 for the whole process lifetime — on the
    /// one endpoint whose entire purpose is recovering without a restart.
    ///
    /// Gutting `RescanGuard::drop` leaves every other test in the suite green.
    /// This one fails.
    #[tokio::test]
    async fn openfan_rescan_releases_its_single_flight_flag_when_the_probe_ends() {
        let state = rescan_state();
        let opened = Arc::new(parking_lot::Mutex::new(Vec::new()));

        let (code, body) = openfan_rescan_with(
            Arc::clone(&state),
            fake_enumerate,
            recording_opener(Arc::clone(&opened)),
        )
        .await;
        assert_ne!(
            code,
            StatusCode::CONFLICT,
            "the first rescan cannot conflict with itself: {:?}",
            body.0
        );
        assert_eq!(
            *opened.lock(),
            fake_enumerate(),
            "precondition: the probe must actually have run, and on the FAKE bus — \
             a handler that stopped probing would satisfy the flag assertion below \
             while testing nothing"
        );

        assert!(
            !state
                .openfan_rescanning
                .load(std::sync::atomic::Ordering::SeqCst),
            "the single-flight flag must be clear once the probe has finished"
        );

        // 10-e: clear the cooldown before the second probe. Without this the call
        // below is rejected by the rate limit, which runs BEFORE the single-flight
        // CAS — so it would never reach the flag at all and this test would assert
        // nothing while still looking like it did. Gutting `RescanGuard::drop` must
        // fail here, and it can only do that if the request gets far enough to try
        // the CAS.
        *state.last_openfan_rescan.lock() = None;

        let (code2, body2) = openfan_rescan_with(
            Arc::clone(&state),
            fake_enumerate,
            recording_opener(Arc::clone(&opened)),
        )
        .await;
        assert_ne!(
            code2,
            StatusCode::CONFLICT,
            "a second rescan after the first completed must not be rejected as \
             'already in progress' — the flag leaked: {:?}",
            body2.0
        );
    }

    /// 10-e. `openfan_rescanning` bounds concurrency; nothing bounded repetition,
    /// and every probe asserts DTR across each candidate tty — which RESETS
    /// Arduino-class boards. So a client looping on a failing rescan was holding
    /// unrelated serial hardware in reset, indefinitely.
    ///
    /// Asserted as an OUTCOME (the second call is refused) rather than by reading
    /// the timestamp: a cooldown that records a stamp nothing consults would pass
    /// a state assertion and change no behaviour at all.
    #[tokio::test]
    async fn openfan_rescan_spaces_repeated_probes() {
        let state = rescan_state();
        let opened = Arc::new(parking_lot::Mutex::new(Vec::new()));

        let (code, body) = openfan_rescan_with(
            Arc::clone(&state),
            fake_enumerate,
            recording_opener(Arc::clone(&opened)),
        )
        .await;
        assert_ne!(
            code,
            StatusCode::CONFLICT,
            "the first probe must not be rate-limited: {:?}",
            body.0
        );
        assert_eq!(
            *opened.lock(),
            fake_enumerate(),
            "precondition: the first call must really have probed the fake bus"
        );
        assert!(
            state.last_openfan_rescan.lock().is_some(),
            "a completed probe must stamp the cooldown, or nothing is ever spaced"
        );

        let opened_before = opened.lock().len();
        let (code2, body2) = openfan_rescan_with(
            Arc::clone(&state),
            fake_enumerate,
            recording_opener(Arc::clone(&opened)),
        )
        .await;
        assert_eq!(
            code2,
            StatusCode::CONFLICT,
            "an immediate second probe must be refused: {:?}",
            body2.0
        );
        assert_eq!(
            opened.lock().len(),
            opened_before,
            "a refused rescan must open nothing — the refusal exists to ration \
             exactly those opens"
        );
        // Distinguish the two 409s. They are the same status by design (docs/08's
        // code set is a contract), so only the message separates "too soon" from
        // "already running" — and a test that cannot tell them apart would pass
        // against a cooldown that never fired but a leaked single-flight flag.
        let msg = body2["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("moments ago"),
            "the refusal must be the cooldown, not a leaked single-flight flag: {:?}",
            body2.0
        );
        // `OFN-u`: and it must not blame the caller for it. The daemon's own
        // post-boot loop probes on this same guard, so the probe being rationed
        // is frequently one the daemon made — the previous wording ("an OpenFan
        // rescan over the same ports was attempted") asserted an action by the
        // client. Asserted as the whole phrase rather than as an absence of
        // "rescan": the word legitimately appears in the sibling single-flight
        // message, so `!msg.contains("rescan")` would be a guard on the wrong
        // string and would pass against any rewording at all.
        assert!(
            msg.contains("a probe over the same ports was attempted moments ago"),
            "the cooldown message must name the actor neutrally — the probe it is \
             rationing is often the daemon's own: {:?}",
            body2.0
        );

        // The 409 must advertise itself as RETRYABLE. This one clears on its own in
        // seconds and the message says so, so reporting `retryable: false` — the
        // default for `validation_error` — tells a client keying its backoff off that
        // field, which is the field's documented purpose, that the wait is permanent.
        assert_eq!(
            body2["error"]["retryable"], true,
            "a cooldown that expires in seconds must not present as permanent: {:?}",
            body2.0
        );

        // And it must expire rather than latch — a rate limit that never lifts is
        // the same wedged route DEC-266's guard exists to prevent.
        *state.last_openfan_rescan.lock() = Some(LastRescan {
            at: Instant::now() - Duration::from_secs(3600),
            candidates: Vec::new(),
        });
        let opened_before_lapse = opened.lock().len();
        let (code3, body3) = openfan_rescan_with(
            Arc::clone(&state),
            fake_enumerate,
            recording_opener(Arc::clone(&opened)),
        )
        .await;
        assert_ne!(
            code3,
            StatusCode::CONFLICT,
            "the cooldown must lapse, not latch the route closed: {:?}",
            body3.0
        );
        assert!(
            opened.lock().len() > opened_before_lapse,
            "the lapse must reach the PROBE, not merely return a different status — \
             an assertion on the code alone holds against a handler that lapsed the \
             cooldown and then did nothing"
        );
    }

    /// 10-e, round 2. Rate-limiting on elapsed time ALONE refused the single most
    /// likely legitimate retry: plug a controller in, click rescan. That is a
    /// human action measured in seconds, so the device went unadopted and the GUI
    /// showed nothing — transiently re-opening the "restart the daemon" mis-advice
    /// DEC-265/266 exists to remove, on the one endpoint whose whole purpose is
    /// recovery without a restart.
    ///
    /// The cooldown therefore applies only while the candidate port set is
    /// UNCHANGED. A newly attached controller enumerates a new tty, so the sets
    /// differ and the retry proceeds at once.
    #[tokio::test]
    async fn openfan_rescan_cooldown_yields_when_the_ports_change() {
        let state = rescan_state();
        let opened = Arc::new(parking_lot::Mutex::new(Vec::new()));

        let (code, body) = openfan_rescan_with(
            Arc::clone(&state),
            fake_enumerate,
            recording_opener(Arc::clone(&opened)),
        )
        .await;
        assert_ne!(
            code,
            StatusCode::CONFLICT,
            "the first probe must not be rate-limited: {:?}",
            body.0
        );

        // Establish the PRESENCE of the cooldown before asserting it yields —
        // otherwise this passes against a build where the cooldown never fires at
        // all, proving nothing about the bypass (DEC-272).
        let (blocked, blocked_body) = openfan_rescan_with(
            Arc::clone(&state),
            fake_enumerate,
            recording_opener(Arc::clone(&opened)),
        )
        .await;
        assert_eq!(
            blocked,
            StatusCode::CONFLICT,
            "precondition: an unchanged port set must still be spaced: {:?}",
            blocked_body.0
        );

        // Now claim the last probe walked a DIFFERENT set — the state a freshly
        // attached controller produces — with the clock left well inside the window.
        *state.last_openfan_rescan.lock() = Some(LastRescan {
            at: Instant::now(),
            candidates: vec!["/dev/ttyFAKE-was-not-here-before".to_string()],
        });
        let opened_before = opened.lock().len();
        let (code2, body2) = openfan_rescan_with(
            Arc::clone(&state),
            fake_enumerate,
            recording_opener(Arc::clone(&opened)),
        )
        .await;
        assert_ne!(
            code2,
            StatusCode::CONFLICT,
            "a changed candidate set must bypass the cooldown — otherwise plugging a \
             controller in and rescanning immediately is refused, which is exactly \
             the recovery this endpoint exists for: {:?}",
            body2.0
        );
        assert!(
            opened.lock().len() > opened_before,
            "the bypass must reach the PROBE, not merely return a different status \
             — an assertion on the code alone holds against a handler that yielded \
             the cooldown and then did nothing"
        );
    }
}
