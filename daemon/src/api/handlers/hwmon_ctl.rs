//! Hwmon PWM and verify endpoints.
//!
//! Named `hwmon_ctl` to avoid confusion with the top-level `crate::hwmon` module.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::hwmon::lease::HwmonWriter;
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

    // DEC-146 P3-12: single mapping source — `PwmHeaderEntry::from_descriptor`.
    // DEC-311: the user's role assignment is overlaid here; the descriptor
    // carries only what discovery could infer.
    let assigned = state.header_roles();
    // AIO-MB Phase 4: snapshot the topology once. `state.header_is_pump_protected`
    // must NOT be called in this loop — it re-takes the controller lock this
    // scope already holds, which would deadlock; the pure union takes the parts
    // we have.
    let devices = state.cooling_devices();
    let headers = ctrl
        .headers()
        .into_iter()
        .map(|h| {
            let assign = assigned.get(&h.id).copied();
            PwmHeaderEntry::from_descriptor(
                h,
                assign,
                crate::hwmon::roles::is_pump_protected(assign, (h.role, h.role_source)),
                devices.iter().find(|d| d.claims(&h.id)),
            )
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

/// Map a `HwmonControlError` to an HTTP error response.
///
/// Post-2.0.0 the client holds no hwmon lease — the profile engine is the sole
/// writer (DEC-165) and `/hwmon/{id}/verify` drives its own short-lived internal
/// "verify" lease. A `Lease` error from a verify write is therefore an internal
/// race (the daemon's own lease lapsed mid-write), never a client precondition,
/// so it surfaces as a retryable `503 hardware_unavailable` rather than the
/// retired `403 lease_required` / `409 lease_already_held` client codes (DEC-170).
fn hwmon_control_error_response(err: HwmonControlError) -> (StatusCode, Json<serde_json::Value>) {
    match err {
        HwmonControlError::Lease(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "hwmon verify could not hold the daemon's internal lease (transient) — retry",
            ),
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
/// Also flags the sensor polling loop to rebuild its cached descriptor set
/// (labels, types, DEC-117 threshold snapshot) on its next tick (DEC-133) —
/// this is how newly loaded sensor chips appear without a daemon restart.
/// Does not replace the running PWM controller — header discovery results
/// are returned for the GUI to refresh its view; a daemon restart is needed
/// to pick up truly new PWM-control hardware.
pub async fn hwmon_rescan_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::hwmon::pwm_discovery::discover_pwm_headers;
    use crate::hwmon::HWMON_SYSFS_ROOT;

    // DEC-133: the polling loop owns the descriptor cache; it swap-checks
    // this flag each tick and re-runs sensor discovery when set.
    state
        .sensor_rescan_requested
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // DEC-207: a rescan is a discovery-changing event — proactively refresh the
    // shared hardware assessment (and the mirrored Dashboard rollup) once the poll
    // loop has had a chance to rebuild its descriptor set on its next tick, so the
    // chip reflects newly-visible chips without waiting for an inventory GET.
    // Deferred + fire-and-forget + coalesced; the readiness page's own force-fetch
    // remains the authoritative path.
    {
        let s = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let _ = super::ensure_assessment(s, true).await;
        });
    }

    let hwmon_root = std::path::Path::new(HWMON_SYSFS_ROOT);
    match discover_pwm_headers(hwmon_root) {
        Ok(headers) => {
            // DEC-146 P3-12: single mapping source — `from_descriptor` (DEC-311
            // overlays the user's role assignment).
            let assigned = state.header_roles();
            let devices = state.cooling_devices();
            let entries: Vec<PwmHeaderEntry> = headers
                .iter()
                .map(|h| {
                    let assign = assigned.get(&h.id).copied();
                    PwmHeaderEntry::from_descriptor(
                        h,
                        assign,
                        crate::hwmon::roles::is_pump_protected(assign, (h.role, h.role_source)),
                        devices.iter().find(|d| d.claims(&h.id)),
                    )
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

/// Settle window for `hwmon_verify_handler`, aliased from the single source
/// of truth in `constants.rs` so the hwmon and GPU verify paths cannot drift
/// apart (DEC-101). See `constants::VERIFY_WAIT_SECONDS` for the rationale and
/// the cross-repo GUI coupling (`VERIFY_PAUSE_SAFETY_MS`).
const VERIFY_WAIT_SECONDS: u8 = crate::constants::VERIFY_WAIT_SECONDS;

/// RAII release of the force-taken "verify" lease on EVERY handler exit path —
/// including a cancelled or panicked future (e.g. the client disconnects during
/// the settle sleep). The profile engine deliberately does NOT adopt a "verify"
/// lease (P2-1, `profile_engine/backends.rs`), so a leaked lease would strand
/// hwmon control until its 60 s TTL; releasing it here lets the engine
/// re-acquire its own lease on the next tick. Releases only — NOT paired with
/// `on_lease_released()`: the verify restores the header to its pre-verify value
/// itself, so the controller's coalescing state already matches the hardware;
/// resetting it would force a pure-churn pwm_enable+PWM re-write on the engine's
/// next tick, every verify (P3-3). **That premise is only sound because the
/// restore is uncancellable** (DEC-290): this guard is moved into the verify's
/// blocking task, so it cannot release before the restore has run. If the
/// sequence is ever made cancellable again, this reasoning fails with it — the
/// coalescing state would then claim the pre-verify duty while the hardware sat
/// at the test duty.
struct VerifyLeaseGuard {
    controller: std::sync::Arc<parking_lot::Mutex<crate::hwmon::pwm_control::HwmonPwmController>>,
    lease_id: String,
}
impl Drop for VerifyLeaseGuard {
    fn drop(&mut self) {
        let _ = self
            .controller
            .lock()
            .lease_manager_mut()
            .release_lease(&self.lease_id);
    }
}

/// Read one header's live sysfs state: `pwm_enable`, the raw PWM byte, that byte
/// as a percentage, and the tach.
///
/// Lifted out of `hwmon_verify_handler`'s inner closure (AIO-MB Phase 3) so the
/// verify and the characterisation sweep read hardware through **one**
/// implementation. Duplicating a three-file read triple is the DEC-276 mistake:
/// the second copy drifts, and nothing catches it because both copies have
/// tests. Every field is `Option` because any of the three files may be absent
/// (no `pwm_enable` on a read-only header) or transiently unreadable.
pub(crate) fn read_header_state(
    pwm: &str,
    en: &Option<String>,
    rpm: &Option<String>,
) -> HwmonVerifyState {
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
}

/// [SAFETY] The duty a diagnostic may restore a header to (`AUD3-l`).
///
/// A diagnostic captures the pre-test duty and puts it back on the way out. For
/// a pump-protected header that captured value is **not** unconditionally safe
/// to write: `set_pwm` always asserts `pwm_enable=1`, so restoring a captured 0
/// converts "0 under firmware control" into "0 under manual control with no
/// writer" — a stopped pump, held until the engine's next tick if the header is
/// a controlled member and indefinitely if no profile is active.
///
/// Clamped for pump-protected headers only. An ordinary chassis fan restored to
/// its own captured 0 is being put back exactly where it was found, and raising
/// it would be a real behaviour change rather than a safety fix.
///
/// **A CPU-labelled header is outside this clamp, and that is a decision rather
/// than a consequence of the predicate (`322-b`).** The constant is named
/// `HARD_PUMP_CPU_FLOOR_PCT` and the *engine* applies it to CPU members too
/// (`profile::CPU_PUMP_LABEL_HINTS` = cpu / pump / aio), but
/// `header_is_pump_protected` resolves through `HeaderRole::is_pump()`, which is
/// `Pump` only. So a `CPU_FAN` header sitting at 0 under BIOS fan-stop is
/// restored to 0 in manual mode, exactly as a chassis fan is.
///
/// It is left that way here because widening the predicate would change what the
/// machine does — raising a fan the BIOS deliberately stopped — and that is a
/// design decision this change did not have. The narrower reading of the real
/// hazard is that the *mode*, not the duty, is what makes a restored 0 dangerous
/// for any header (`322-c`), and neither diagnostic restores `pwm_enable`.
/// Raised by `ofc:security-reviewer`; recorded rather than silently inherited.
///
/// This is the counterpart of [`verify_test_duty`], which has always floored the
/// duty written on the way IN. `api/characterization.rs` applies the same clamp
/// to its own restore through `RestoreOnDrop::restore_floor`.
fn restore_duty(is_pump: bool, captured_pct: u8) -> u8 {
    if is_pump {
        captured_pct.max(crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8)
    } else {
        captured_pct
    }
}

/// The duty `POST /hwmon/{id}/verify` drives the header to for its settle window.
///
/// Pure and separately testable **because the two constraints on it pull against
/// each other**, and the first attempt at this satisfied one while breaking the
/// other:
///
/// 1. **Never under-drive a pump** (`AIO1-a` / DEC-311). The old rule was a flat
///    `if current > 50 { 20 } else { 80 }`, and 20 % is below the daemon's own
///    30 % pump floor — so verifying a motherboard AIO pump, which normally idles
///    above 50 %, under-drove it for ~6 s. Verify was the one write path that
///    never consulted `member_effective_floor`.
/// 2. **Move far enough to be measurable.** `classify_verify_result` requires a
///    **>20 % RPM change** (`init_rpm / 5`) before it will call a header
///    `effective`. A duty that respects the floor but barely moves therefore
///    reports a perfectly good pump as `no_rpm_effect` — "PWM writes may be
///    accepted but have no hardware effect". The fix for (1) originally read
///    `if current <= 75 { 80 }`, which at 75 % is a **5-point** delta: safe, and
///    a false alarm on every pump between roughly 60 % and 80 %.
///
/// So a pump takes whichever direction yields the **larger** delta, preferring
/// upward on a tie (upward never walks a pump toward its stall floor). The
/// result is always `>= HARD_PUMP_CPU_FLOOR_PCT` and always at least 35 points
/// from `current_pct` — see `pump_verify_duty_is_always_floored_and_measurable`,
/// which asserts both over every possible input rather than sampling.
///
/// A non-pump header keeps the original 20/80 pair, byte-identical.
fn verify_test_duty(is_pump: bool, current_pct: u8) -> u8 {
    if !is_pump {
        return if current_pct > 50 { 20 } else { 80 };
    }
    const DELTA: u8 = 40;
    let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
    let up = current_pct.saturating_add(DELTA).min(100).max(floor);
    let down = current_pct.saturating_sub(DELTA).max(floor);
    let up_delta = up.abs_diff(current_pct);
    let down_delta = down.abs_diff(current_pct);
    if up_delta >= down_delta {
        up
    } else {
        down
    }
}

/// POST /hwmon/{header_id}/verify — behavioural test of PWM write effectiveness.
///
/// Writes a test PWM value, waits for hardware to respond, then reads back
/// pwm_enable, PWM value, and RPM to classify the result. The daemon manages
/// coordination itself (DEC-165): it pauses the profile engine's write phase
/// for the verify window and force-takes a short-lived "verify" lease for its
/// own test writes — no client lease is required. Takes ~6 seconds: slow-
/// spinning fans (pumps, large 140mm chassis fans) need >3s to settle, and a
/// too-short wait produced false `no_rpm_effect` verdicts. The per-call HTTP
/// timeout in `client.py::verify_hwmon_pwm` must stay above this value (≥12 s).
pub async fn hwmon_verify_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(header_id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // [SAFETY] Refuse once the daemon is going down (AIO-MB Phase 5, DEC-317).
    //
    // This used to be structurally unnecessary: the only caller was an HTTP
    // request, and `shutdown_sequence` stops the IPC server *before* it drains
    // tasks and restores hardware, so no request could arrive this late. The
    // validation orchestrator calls this handler as a FUNCTION from a detached
    // task, which breaks that invariant — and a diagnostic that starts after
    // `restore_hwmon_to_auto` writes its duty, re-asserts `pwm_enable=1` through
    // `set_pwm`'s reclaim watchdog, and then deliberately skips its own restore
    // (DEC-290), leaving the header latched in manual with no daemon left to
    // drive it. Guarding at entry closes that for every caller, present and
    // future, rather than relying on each one to remember.
    if *state.openfan_runtime.shutdown.borrow() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("the daemon is shutting down"),
        );
    }
    // DEC-201/DEC-297: refuse to start a verify while the system is hot, OR while
    // the ladder is forcing — a verify drives the header AWAY from its commanded
    // duty. NOT because it suppresses the thermal `force_all_with_floor`, which is what this
    // comment used to claim: `force_all_with_floor` runs before the engine's
    // `verify_active()` gate and always outranks a verify.
    if let Some(resp) = super::verify_thermal_guard(&state.cache) {
        return resp;
    }
    let controller = match &state.hwmon_controller {
        Some(c) => c,
        None => {
            // M12: match sibling hwmon handlers which all return 503
            // hardware_unavailable when the controller is absent. Returning
            // 404 here implied the endpoint itself was missing, which it is
            // not — the hardware is.
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
            );
        }
    };

    // Extract header paths (404 if unknown) before pausing the engine.
    let (pwm_path, enable_path, rpm_path) = {
        let ctrl = controller.lock();
        match ctrl.header(&header_id) {
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

    // Single-flight + pause the engine's write phase for the verify's lifetime
    // (reject a concurrent verify with 409), then force-take a daemon-owned
    // "verify" lease for our own controlled writes. The engine is the sole
    // writer now (DEC-165) — no client lease.
    let Some(verify_guard) =
        super::begin_verify_pause(&state.cache, crate::constants::VERIFY_PAUSE_DEADMAN)
    else {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation("a hardware verify or calibration is already in progress"),
        );
    };
    // Force-take a daemon-owned "verify" lease for our own controlled writes,
    // released by `VerifyLeaseGuard` (defined above the handler) on EVERY exit
    // path — including a cancelled or panicked future.
    let verify_lease_id = {
        let mut ctrl = controller.lock();
        ctrl.lease_manager_mut()
            .force_take_lease(HwmonWriter::Verify)
            .lease_id
    };
    let verify_lease = VerifyLeaseGuard {
        controller: controller.clone(),
        lease_id: verify_lease_id.clone(),
    };

    // DEC-290: the ENTIRE test-write -> settle -> restore sequence runs inside a
    // single `spawn_blocking`, and BOTH guards are moved into it.
    //
    // It used to run inline, with `tokio::time::sleep(...).await` between the
    // test write and the restore. That `.await` is a cancellation point: if the
    // client disconnected — or the GUI's own 12 s timeout fired — axum dropped
    // the handler future, both RAII guards ran their `Drop`, and **the restore
    // simply never happened**. The header stayed at the test duty, which for any
    // header above 50% means it was dropped to 20% and left there. Nothing
    // recovers it when no active profile owns that header, because then nothing
    // else ever writes it.
    //
    // A `spawn_blocking` task is NOT cancelled when its `JoinHandle` is dropped;
    // it runs to completion. So the restore now happens whether or not anyone is
    // still listening. This is the DEC-255 shape already used for GPU writes, and
    // the guards must move in for the same reason they do there: leaving them on
    // the handler would release the lease and the engine pause *while the
    // blocking write was still in flight*, and the restore would then fail
    // `InvalidLease` — trading a stranded duty for a failed one.
    let bg_controller = controller.clone();
    // DEC-290 review: the shared shutdown watch, read by the task before its
    // restore. Making the sequence uncancellable also made it survive the
    // shutdown that used to cancel it, and `main.rs` guarantees "the restore is
    // the guaranteed last writer" (277-c). Without this check a verify caught by
    // SIGTERM writes its duty AFTER `restore_hwmon_to_auto` has handed the
    // header back — and `set_pwm`'s enable watchdog reads the `pwm_enable=2` that
    // restore just wrote, classifies it as a BIOS reclaim, and re-asserts
    // `pwm_enable=1`. The daemon then exits with the header latched in manual at
    // a fixed duty and nothing left to drive it.
    let bg_shutdown = state.openfan_runtime.shutdown.clone();
    let bg_header_id = header_id.clone();
    // DEC-311: resolved BEFORE the blocking task so the role lookup never runs
    // under `spawn_blocking` with the controller lock in play. The UNION
    // predicate, not the resolved role — a user assignment must not be able to
    // strip protection the header's own label already earned.
    let bg_is_pump = state.header_is_pump_protected(&header_id);
    let bg_lease_id = verify_lease_id.clone();
    let join = tokio::task::spawn_blocking(move || {
        // Moved, not borrowed: dropped only when this task finishes, so the
        // pause and the lease outlive every write below on every path.
        let verify_guard = verify_guard;
        let _verify_lease = verify_lease;

        let initial = read_header_state(&pwm_path, &enable_path, &rpm_path);

        // Test PWM: a significant delta from current, in whichever direction has room.
        let current_pct = initial.pwm_percent.unwrap_or(50);
        let test_pct: u8 = verify_test_duty(bg_is_pump, current_pct);

        // Write test value via controller (sets pwm_enable=1 + PWM).
        // Route errors through the shared HwmonControlError mapper: if the daemon's
        // own force-taken "verify" lease lapses between here and the write, that is
        // an internal race, surfaced as a retryable 503 hardware_unavailable
        // (DEC-170) — not a client lease error and not a 500 internal_error.
        {
            let mut ctrl = bg_controller.lock();
            ctrl.set_pwm(&bg_header_id, test_pct, &bg_lease_id)?;
        }

        // Blocking sleep, deliberately: this task is the uncancellable unit, and
        // an async sleep here would reintroduce the cancellation point the whole
        // restructure exists to remove.
        std::thread::sleep(std::time::Duration::from_secs(VERIFY_WAIT_SECONDS as u64));

        let final_state = read_header_state(&pwm_path, &enable_path, &rpm_path);

        // DEC-296: prove we are still alive before restoring, and keep the slot.
        // The settle and the reads above are blocking sysfs work with no lock
        // held, and `read_state` is plain `fs::read_to_string` — on a wedged chip
        // it can outlast VERIFY_PAUSE_DEADMAN. Without this checkpoint a merely
        // SLOW verify is superseded at the window, the successor force-takes the
        // hwmon lease, and the restore below then fails with an opaque
        // InvalidLease, leaving the header parked at the test duty. Renewing here
        // makes the deadman measure liveness rather than total duration.
        //
        // If we HAVE been superseded, the restore cannot succeed — our lease is
        // already gone — so say why, rather than emitting a lease error that
        // reads like an internal race.
        if !verify_guard.renew(crate::constants::VERIFY_PAUSE_DEADMAN) {
            log::warn!(
                "verify: {bg_header_id} was superseded by a later diagnostic while \
                 settling, so its lease is gone and the restore to {}% cannot land. \
                 The header is left at the test duty for the new owner or the next \
                 engine tick to correct.",
                initial.pwm_percent.unwrap_or(50)
            );
            return Ok((initial, final_state, test_pct, true));
        }

        // Restore original PWM. Failures here are surfaced via ``restore_failed``
        // rather than overwriting the diagnostic verify outcome — a successful
        // verify with a failed restore is its own condition the caller can act on
        // (typically: re-write the desired PWM).
        // Skip the restore entirely if the daemon is going down: firmware
        // control — which the shutdown restore has either already applied or is
        // about to — is strictly safer than re-asserting a fixed duty that no
        // writer will ever revise. Signalled at `main.rs:1117`, well before
        // `restore_hardware()` at `:1153`, so this check cannot lose the race.
        if *bg_shutdown.borrow() {
            log::info!(
                "verify: skipping restore of {bg_header_id} — the daemon is \
                 shutting down and the hardware restore owns the header"
            );
            return Ok((initial, final_state, test_pct, true));
        }

        // [SAFETY] `AUD3-l`: clamp the RESTORE to the pump floor, not just the
        // test duty. `verify_test_duty` has always floored the duty we write on
        // the way in; the way out wrote the captured original straight into
        // `set_pwm`, which applies no floor of its own (pinned by
        // `set_pwm_accepts_low_values_no_floor`). So a pump header that read 0
        // got `pwm_enable=1` plus a floored test duty, then a 0% restore —
        // latched in MANUAL at 0 with no writer, until the engine's next tick if
        // it is a controlled member and indefinitely if no profile is active.
        // The mode is the part that makes it dangerous: 0 under firmware control
        // is the firmware's business, 0 under `pwm_enable=1` is a stopped pump.
        //
        // **This clamps the duty; it does NOT restore the mode** — neither
        // diagnostic writes the captured `pwm_enable` back, so a header taken
        // from firmware control stays in manual for the daemon's lifetime
        // whatever duty it lands on. Pre-existing, wider than this change, and
        // recorded as `322-c` rather than fixed here (`CLAUDE.md § Review blast
        // radius`). Named because the argument above is a mode argument, and it
        // would be dishonest to borrow it and imply the mode were handled.
        //
        // Newly reachable rather than merely old: Phase 5's orchestrator aims
        // both diagnostics at `device.pump_member` by default.
        let restore_pct = restore_duty(bg_is_pump, current_pct);
        let restore_failed = {
            let mut ctrl = bg_controller.lock();
            match ctrl.set_pwm(&bg_header_id, restore_pct, &bg_lease_id) {
                Ok(_) => false,
                Err(e) => {
                    log::warn!(
                        "verify: restore PWM to {restore_pct}% on {bg_header_id} \
                         failed (header left at test value {test_pct}%): {e}"
                    );
                    true
                }
            }
        };

        Ok::<_, HwmonControlError>((initial, final_state, test_pct, restore_failed))
    });

    // If the caller has gone away this `.await` resolves with a JoinError only
    // when the task itself panicked — the task keeps running either way, and its
    // restore lands regardless. That is the point.
    let (initial, final_state, test_pct, restore_failed) = match join.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return hwmon_control_error_response(e),
        // `JoinError` is also returned when the runtime shuts down before the
        // task is polled. Logging that as a panic would put a false `error!` in
        // the one log a future verify investigation would trust.
        Err(e) if e.is_cancelled() => {
            log::info!("verify: hwmon verify task cancelled during shutdown");
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorEnvelope::hardware_unavailable("daemon is shutting down"),
            );
        }
        Err(e) => {
            log::error!("verify: hwmon verify task panicked: {e}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal("hwmon verify failed"),
            );
        }
    };

    // Both guards live inside the blocking task above, so they release only when
    // the whole sequence — including the restore — has finished. A cancelled or
    // panicked verify therefore strands neither the lease, the engine pause, nor
    // the header's duty. The comment that used to sit here claimed that last part
    // while the restore sat after an `.await` and was skipped on cancellation; it
    // is true now because the sequence is uncancellable, not because it ever was
    // (DEC-290).

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
            restore_failed,
        },
    )
}

fn classify_verify_result(
    initial: &HwmonVerifyState,
    final_state: &HwmonVerifyState,
    test_pct: u8,
) -> (String, String) {
    // Check if pwm_enable was reclaimed.
    //
    // [HOST-a / DEC-326] Exempt the driver's full-speed alias. `verify_test_duty`
    // returns exactly 100 for a pump header already running at >=60%, which is an
    // ordinary state — so without this the verify reports a BIOS reclaim that did
    // not happen, on the one endpoint built to answer "does this header accept
    // writes?".
    if let Some(final_enable) = final_state.pwm_enable {
        // The exemption additionally requires that the daemon HELD manual mode
        // to begin with (`initial.pwm_enable == Some(1)`) — the verdict's own
        // message says "changed from 1", and without that term a header the
        // firmware already pins at full speed presents identically to the alias
        // (enable 0, duty 100%, zero `pwm_raw` delta) and would verify as a pass
        // on the one endpoint built to answer "does this header accept writes?".
        if final_enable != 1
            && !(initial.pwm_enable == Some(1)
                && crate::pwm::is_full_speed_alias(
                    test_pct,
                    final_state.pwm_percent,
                    final_state.pwm_enable,
                ))
        {
            return (
                "pwm_enable_reverted".into(),
                format!(
                    "pwm_enable changed from 1 to {final_enable} after write. \
                     Most likely cause: BIOS/EC firmware reasserting automatic \
                     mode (e.g. AORUS Smart Fan reclaim). Less likely: another \
                     in-process writer (lease holder, thermal-safety override) \
                     flipped pwm_enable during the {VERIFY_WAIT_SECONDS}s test \
                     window. Re-run with no profile active and no other client \
                     writing to disambiguate."
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
                    "PWM value changed from test {test_raw} to {final_raw} during \
                     the {VERIFY_WAIT_SECONDS}s verify window. Most likely cause: \
                     BIOS/EC firmware overriding the PWM register. Less likely: \
                     another writer (lease holder, thermal-safety override, or a \
                     CLI tool poking sysfs directly) wrote to the same header \
                     during the test. Re-run with no profile active and no other \
                     client writing to confirm BIOS/EC reclaim."
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

// ── PWM/RPM characterisation (AIO-MB Phase 3) ────────────────────────

/// POST /hwmon/{header_id}/characterize — start a PWM/RPM response sweep.
///
/// Returns **202 immediately** with the run snapshot; the sweep itself runs as a
/// detached task and the client polls `GET /diagnostics/characterization`. That
/// shape is the one requirement `AIO-Phase3.md` states outright — "show live or
/// progressive results" — and it is also what makes a Cancel button possible.
///
/// Deliberately NOT the DEC-290 `spawn_blocking` shape used by the two verifies,
/// and for the reason already recorded at `calibration.rs:143-156`: a sweep is
/// minutes long, and making it uncancellable would pin a blocking thread and
/// hold the single verify slot for that whole time after the client had gone.
/// The restore is protected by a drop guard instead, which restores the hardware
/// without extending the work's lifetime.
///
/// [SAFETY] The task is detached, so it is not in `main::shutdown_sequence`'s
/// `task_handles`. `characterization::RestoreOnDrop` carries the shutdown check
/// that makes that safe — read its docs before changing anything here.
pub async fn hwmon_characterize_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(header_id): axum::extract::Path<String>,
    Json(body): Json<crate::api::characterization::CharacterizationRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // [SAFETY] Refuse once the daemon is going down (AIO-MB Phase 5, DEC-317).
    //
    // This used to be structurally unnecessary: the only caller was an HTTP
    // request, and `shutdown_sequence` stops the IPC server *before* it drains
    // tasks and restores hardware, so no request could arrive this late. The
    // validation orchestrator calls this handler as a FUNCTION from a detached
    // task, which breaks that invariant — and a diagnostic that starts after
    // `restore_hwmon_to_auto` writes its duty, re-asserts `pwm_enable=1` through
    // `set_pwm`'s reclaim watchdog, and then deliberately skips its own restore
    // (DEC-290), leaving the header latched in manual with no daemon left to
    // drive it. Guarding at entry closes that for every caller, present and
    // future, rather than relying on each one to remember.
    if *state.openfan_runtime.shutdown.borrow() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("the daemon is shutting down"),
        );
    }
    use crate::api::characterization as ch;

    // Same two refusals as a verify, for the same reason: a sweep drives the
    // header away from its commanded duty, which must not happen while the
    // system is hot or while the ladder is forcing (DEC-297).
    if let Some(resp) = super::verify_thermal_guard(&state.cache) {
        return resp;
    }
    let Some(controller) = state.hwmon_controller.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let (pwm_path, enable_path, rpm_path) = {
        let ctrl = controller.lock();
        match ctrl.header(&header_id) {
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

    // Claim the SAME single-flight slot verify and calibrate use, so at most one
    // of the three ever drives hardware, and the engine's write phase is paused
    // for the sweep's lifetime. Renewed once per point inside the sweep so the
    // deadman measures liveness, not total duration (DEC-296).
    let Some(verify_guard) =
        super::begin_verify_pause(&state.cache, crate::constants::VERIFY_PAUSE_DEADMAN)
    else {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation("a hardware verify or calibration is already in progress"),
        );
    };

    // [SAFETY] The UNION predicate, never the wire `role` (DEC-312): a user who
    // assigns `chassis_fan` to a header the hardware labels PUMP must still get
    // the pump floor. Resolved here, before the task, so the role lookup never
    // races the controller lock inside the sweep.
    let floor = if state.header_is_pump_protected(&header_id) {
        crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8
    } else {
        0
    };
    let points = ch::resolve_points(body.points_pct.as_deref(), floor);
    let settle = ch::resolve_settle(body.settle_seconds);
    if points.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("no usable sweep points after clamping"),
        );
    }

    let verify_lease_id = {
        let mut ctrl = controller.lock();
        ctrl.lease_manager_mut()
            .force_take_lease(HwmonWriter::Verify)
            .lease_id
    };
    let verify_lease = VerifyLeaseGuard {
        controller: controller.clone(),
        lease_id: verify_lease_id.clone(),
    };
    let lease_for_renew = verify_lease_id.clone();

    let run = ch::CharacterizationRun {
        run_id: ch::next_run_id(),
        header_id: header_id.clone(),
        state: ch::STATE_RUNNING.to_string(),
        requested_points_pct: points.clone(),
        settle_seconds: settle.as_secs(),
        points: vec![],
        summary: None,
        original_pct: read_header_state(&pwm_path, &enable_path, &rpm_path).pwm_percent,
        restore_failed: false,
        restore_outcome: ch::RestoreOutcome::Pending.token().to_string(),
        detail: None,
    };
    // The cancel flag is cleared and the run installed under ONE lock, and the
    // cancel handler takes the same lock across its check-and-set. Without that
    // pairing a DELETE aimed at a finishing run could set the flag after this
    // reset and abort the run that replaced it, reporting the new run's snapshot
    // as though the user had cancelled it.
    {
        let mut slot_guard = state.characterization.lock();
        state.characterization_cancel.store(false, Ordering::SeqCst);
        *slot_guard = Some(run.clone());
    }

    let slot = state.characterization.clone();
    let my_run_id = run.run_id.clone();
    let cancel = state.characterization_cancel.clone();
    let cache = state.cache.clone();
    let ctrl_arc = controller.clone();
    let shutdown_rx = state.openfan_runtime.shutdown.clone();
    let hid = header_id.clone();

    tokio::spawn(async move {
        let report = ch::RestoreReport::new();

        // Guard drop order is load-bearing and is asserted by
        // `characterization::tests::the_restore_write_lands_while_the_lease_is_still_valid`.
        // `run_sweep`
        // declares its own `RestoreOnDrop` internally, so that guard drops when
        // the sweep future completes — i.e. BEFORE `lease` and `pause` below,
        // which is the only order in which the restore write can still succeed.
        {
            let pause = verify_guard;
            let _lease = verify_lease;

            // [SAFETY] Renews BOTH the engine pause and the hwmon lease, once per
            // point. Renewing only the pause was a defect: `force_take_lease`
            // stamps a 60 s TTL (`hwmon::lease::DEFAULT_LEASE_TTL`), nothing else
            // renews a Verify lease, and `set_pwm` merely *validates* it without
            // refreshing. A documented-legal 20 x 15 s sweep therefore wrote fine
            // until t~60 s and then failed every write — **including the drop
            // guard's restore**, stranding the header at a mid-sweep duty with no
            // writer left to correct it. The renewal is what makes both deadlines
            // measure liveness rather than total duration.
            let keepalive = || {
                let lease_ok = ctrl_arc
                    .lock()
                    .lease_manager_mut()
                    .renew_lease(&lease_for_renew)
                    .is_ok();
                let pause_ok = pause.renew(crate::constants::VERIFY_PAUSE_DEADMAN);
                lease_ok && pause_ok
            };
            let shutting_down = || *shutdown_rx.borrow();
            let write_fn = |pct: u8| -> Result<(), String> {
                let mut c = ctrl_arc.lock();
                c.set_pwm(&hid, pct, &verify_lease_id)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            };
            // KNOWN LIMITATION, recorded as `AIO3-d`: these are blocking
            // `std::fs` reads issued from the async runtime — ~12 per point
            // during the settle sub-sampling — where the verify does the same
            // reads inside `spawn_blocking`. On a wedged chip (the DEC-278
            // hazard) this parks a tokio worker with no bound. Calibrate is NOT
            // a precedent for this specific point: its sweep reads RPM from the
            // cache, not from sysfs. Left as-is deliberately — wrapping each read
            // is a change to a hardware path that this diff did not scope — but
            // the note is here because an unremarked gap is an invisible one.
            let read_fn = || read_header_state(&pwm_path, &enable_path, &rpm_path);
            // Fenced on `run_id`: a run whose deadman elapsed can be superseded
            // (`try_begin_verify` deliberately permits the steal), and without the
            // fence the loser would append its points into the winner's list and
            // then mark it terminal — reporting a live sweep as finished, with
            // another run's data, and making the one actually driving the header
            // uncancellable.
            let publish = |pt: ch::CharPoint| {
                if let Some(r) = slot.lock().as_mut() {
                    if r.run_id == my_run_id && r.state == ch::STATE_RUNNING {
                        r.points.push(pt);
                    }
                }
            };

            let outcome = ch::run_sweep(
                &cache,
                &hid,
                &points,
                // [SAFETY] `AUD3-l`: the same header floor `resolve_points` used for
                // the sweep, reused for the RESTORE — 30% for a pump-protected
                // header, 0 for everything else. Before this the restore wrote the
                // captured pre-sweep duty straight through, so a pump reading 0
                // was restored to 0 with `pwm_enable=1` and left stopped.
                floor,
                settle,
                write_fn,
                read_fn,
                &cancel,
                shutting_down,
                keepalive,
                &report,
                publish,
            )
            .await;

            // Terminal publish, INSIDE the guarded scope and fenced on `run_id`.
            // Inside, because the single-flight slot is released the moment this
            // block ends — a terminal write placed after it could legally land on
            // a run that had already started in the gap. `run_sweep`'s own
            // `RestoreOnDrop` has already dropped by here (it lives in that
            // future), so the restore report is final — which is why
            // `RestoreOutcome::Pending` is unreachable below.
            //
            // `summarise` is the ONLY place the derived verdicts come from —
            // deriving any of them here instead would be the "extracted rule the
            // call site never uses" defect this project has hit five times.
            if let Some(r) = slot.lock().as_mut() {
                if r.run_id == my_run_id {
                    r.points = outcome.points;
                    r.summary = Some(ch::summarise(&r.points));
                    r.state = outcome.state.to_string();
                    r.detail = outcome.detail;
                    // ONE source of truth for both fields (`AUD2-c`): the boolean
                    // is derived from the reason, so a future exit path cannot
                    // report "restored" while naming a skip.
                    let restore = report.get();
                    r.restore_failed = restore.header_left_moved();
                    r.restore_outcome = restore.token().to_string();
                }
            }
        };
    });

    json_ok(StatusCode::ACCEPTED, run)
}

/// GET /diagnostics/characterization — the current or most recent run.
pub async fn characterization_status_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.characterization.lock().clone() {
        Some(run) => json_ok(StatusCode::OK, run),
        None => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::validation("no characterisation run has been started"),
        ),
    }
}

/// DELETE /diagnostics/characterization — ask the running sweep to stop.
///
/// Cooperative: the sweep checks the flag between points, so the current point
/// finishes its settle first. That is deliberate — tearing down mid-write would
/// leave the header at an unmeasured duty, and the restore is what the caller
/// actually wants. It always runs.
pub async fn characterization_cancel_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // ONE lock across the check and the set. Two acquisitions left a window in
    // which the running run could finish and a new one be installed between them,
    // so the late `store(true)` aborted the *successor* after its first point and
    // returned its snapshot as though the user had cancelled it.
    let snapshot = {
        let guard = state.characterization.lock();
        match guard.as_ref() {
            Some(run) if run.is_running() => {
                state.characterization_cancel.store(true, Ordering::SeqCst);
                run.clone()
            }
            _ => {
                return error_response(
                    StatusCode::CONFLICT,
                    &ErrorEnvelope::validation("no characterisation run is in progress"),
                )
            }
        }
    };
    json_ok(StatusCode::ACCEPTED, snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::lease::LeaseError;
    use crate::hwmon::pwm_control::HwmonControlError;

    type WriteLog = Arc<parking_lot::Mutex<Vec<(String, String)>>>;

    /// Records every sysfs write so a test can see whether the restore landed.
    struct RecordingWriter(WriteLog);
    impl crate::hwmon::pwm_control::SysfsWriter for RecordingWriter {
        fn write_file(&mut self, p: &str, v: &str) -> Result<(), crate::error::HwmonError> {
            self.0.lock().push((p.to_string(), v.to_string()));
            Ok(())
        }
        fn read_file(&self, _p: &str) -> Result<String, crate::error::HwmonError> {
            Ok("1".into())
        }
    }

    /// [SAFETY] `AIO1-a` / DEC-311, over EVERY possible input.
    ///
    /// Two properties, and the first draft of this rule satisfied one while
    /// breaking the other — which is why both are asserted here and why the
    /// assertion is exhaustive rather than sampled:
    ///   1. a pump is never driven below `HARD_PUMP_CPU_FLOOR_PCT`;
    ///   2. the duty moves far enough for `classify_verify_result` to see it
    ///      (>20% RPM change), or a healthy pump reports `no_rpm_effect`.
    #[test]
    fn pump_verify_duty_is_always_floored_and_measurable() {
        let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
        for current in 0..=100u8 {
            let t = verify_test_duty(true, current);
            assert!(
                t >= floor,
                "pump at {current}% tested at {t}%, below the {floor}% floor"
            );
            assert!(t <= 100, "pump at {current}% tested at {t}%");
            assert!(
                t.abs_diff(current) >= 30,
                "pump at {current}% tested at {t}% — a {}-point move is too small \
                 for classify_verify_result's >20% RPM threshold, so a healthy \
                 pump would report `no_rpm_effect`",
                t.abs_diff(current)
            );
        }
    }

    /// The non-pump path must be byte-identical to the pre-DEC-311 rule, or this
    /// change silently altered verify for every fan on every machine.
    #[test]
    fn ordinary_verify_duty_is_unchanged() {
        for current in 0..=100u8 {
            let expected = if current > 50 { 20 } else { 80 };
            assert_eq!(
                verify_test_duty(false, current),
                expected,
                "ordinary header at {current}% must keep the original duty"
            );
        }
    }
    /// [SAFETY] `AUD3-l`: a diagnostic must not restore a pump to a stop.
    ///
    /// `verify_test_duty` has always floored the duty written on the way IN, and
    /// is exhaustively tested over all 101 inputs. Nothing asserted the way OUT,
    /// and the way out wrote the captured duty straight into `set_pwm`, which
    /// applies no floor of its own. Exhaustive here too, for the same reason:
    /// the interesting inputs are the low ones and there are only 101 of them.
    #[test]
    fn a_pump_is_never_restored_below_its_floor() {
        let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
        for captured in 0u8..=100 {
            assert!(
                restore_duty(true, captured) >= floor,
                "a pump captured at {captured}% would be restored to {}%, below \
                 the {floor}% floor — with pwm_enable=1 asserted, that is a \
                 stopped pump no writer will revise",
                restore_duty(true, captured)
            );
            // Above the floor the captured value is returned untouched: this is a
            // restore, not a re-clamp, and raising a pump that was legitimately
            // at 55% would be its own defect.
            if captured >= floor {
                assert_eq!(restore_duty(true, captured), captured);
            }
        }
    }

    /// The other half of the same rule: an ordinary fan is put back EXACTLY where
    /// it was found, 0 included. Clamping it up would be a real behaviour change
    /// rather than a safety fix, and this is what stops the floor spreading.
    ///
    /// **Read what this does and does not pin.** `is_pump` here is
    /// `header_is_pump_protected`, i.e. `HeaderRole::is_pump()`, which is `Pump`
    /// only — so this case also covers **CPU-labelled** headers, which the engine
    /// *does* floor at the same 30%. That exclusion is deliberate and argued at
    /// `restore_duty`'s own doc; it is recorded as `322-b` precisely so this
    /// assertion is not mistaken for evidence that the question was considered
    /// and settled. If `322-b` is ever decided the other way, this test is the
    /// one that must change, and it should be changed knowingly.
    #[test]
    fn a_non_pump_header_is_restored_exactly_as_captured() {
        for captured in 0u8..=100 {
            assert_eq!(restore_duty(false, captured), captured);
        }
    }

    /// [SAFETY] The CALL SITE, not just the helper (`CLAUDE.md`: extracting a
    /// rule into a testable function does not test the call site).
    ///
    /// Two things break independently and neither shows up in the table tests
    /// above: the handler could stop consulting the role at all, and
    /// `header_is_pump_protected` is keyed by *header* id while identify and
    /// verify address a *fan* id — if those key spaces diverge the lookup
    /// silently misses and every pump falls back to the unprotected path,
    /// reporting success the whole way.
    ///
    /// **The baseline must be above 50%** or this test proves nothing: the
    /// ordinary path picks 80% for anything at-or-below 50%, so a header whose
    /// `pwm_path` does not exist (`current_pct` defaults to 50) passes with the
    /// role check deleted. The first version of this test did exactly that and
    /// its own bypass-the-fix validity check caught it.
    #[tokio::test]
    async fn verify_never_under_drives_a_pump_labelled_header() {
        let (state, writes, _shutdown_tx, _tmp) =
            verify_test_state_at_duty(230, crate::hwmon::roles::HeaderRole::Pump); // ~90%

        let _ = hwmon_verify_handler(
            axum::extract::State(state.clone()),
            axum::extract::Path("hwmon:test:dev:pwm1".to_string()),
        )
        .await;

        let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
        let duties = pwm_duties(&writes);
        assert!(!duties.is_empty(), "the verify wrote no PWM value at all");
        for d in &duties {
            assert!(
                *d >= floor,
                "verify drove a pump-labelled header to {d}%, below the {floor}% \
                 floor (all duties: {duties:?})"
            );
        }
    }

    /// The other half: an ordinary header at the same 90% baseline must still
    /// take the original downward 20% test. Without this, clamping everything to
    /// the floor would pass the test above and change verify for every fan.
    #[tokio::test]
    async fn verify_leaves_an_ordinary_header_on_the_original_duty() {
        let (state, writes, _shutdown_tx, _tmp) =
            verify_test_state_at_duty(230, crate::hwmon::roles::HeaderRole::ChassisFan);
        let _ = hwmon_verify_handler(
            axum::extract::State(state.clone()),
            axum::extract::Path("hwmon:test:dev:pwm1".to_string()),
        )
        .await;
        let duties = pwm_duties(&writes);
        assert!(
            duties.contains(&20),
            "an ordinary header at 90% must still test at 20%: {duties:?}"
        );
    }

    /// [SAFETY] DEC-311: assigning `pump` to a header that is ALREADY held at 0
    /// by a live identify must release that hold.
    ///
    /// The identify target is an absolute duty chosen from the role at take
    /// time, so without this the exact sequence a user performs during setup —
    /// start identify on an unlabelled header, hear the pump stop, assign
    /// `pump` — would leave a real pump at 0 until the deadman fired (up to
    /// `OVERRIDE_TTL_SECS`). The union predicate does NOT close this: on a
    /// label-less header, which is the whole AIO-MB target case, the assignment
    /// is the only evidence and it arrives after the hold was taken.
    ///
    /// Tests the WIRING — that `update_header_role_handler` actually calls
    /// `identify_restore` — not just that `identify_restore` works.
    #[tokio::test]
    async fn assigning_pump_releases_a_live_identify_hold() {
        let (state, _writes, _shutdown_tx, _tmp) =
            verify_test_state_at_duty(230, crate::hwmon::roles::HeaderRole::Unknown);
        let id = "hwmon:test:dev:pwm1";

        // A hold taken while the header looked like an ordinary fan: pinned to 0.
        state.override_table.lock().identify_hold(
            id,
            0,
            crate::control_override::IdentifyMode::Stop,
            std::time::Duration::from_secs(15),
        );
        assert_eq!(
            state.override_table.lock().snapshot().identify.get(id),
            Some(&0),
            "precondition: the header must actually be held at 0"
        );

        let (status, body) = crate::api::handlers::update_header_role_handler(
            axum::extract::State(state.clone()),
            axum::response::Json(serde_json::json!({ "header_id": id, "role": "pump" })),
        )
        .await;
        assert_eq!(status, 200, "{body:?}");

        assert!(
            !state
                .override_table
                .lock()
                .snapshot()
                .identify
                .contains_key(id),
            "assigning `pump` must release a hold that is pinning it to 0"
        );
    }

    /// The converse: a NON-pump assignment must not disturb a live hold, or
    /// every role edit would silently cancel an identify in progress.
    #[tokio::test]
    async fn assigning_a_non_pump_role_leaves_a_live_hold_alone() {
        let (state, _writes, _shutdown_tx, _tmp) =
            verify_test_state_at_duty(230, crate::hwmon::roles::HeaderRole::Unknown);
        let id = "hwmon:test:dev:pwm1";
        state.override_table.lock().identify_hold(
            id,
            0,
            crate::control_override::IdentifyMode::Stop,
            std::time::Duration::from_secs(15),
        );

        let (status, _) = crate::api::handlers::update_header_role_handler(
            axum::extract::State(state.clone()),
            axum::response::Json(serde_json::json!({ "header_id": id, "role": "chassis_fan" })),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            state.override_table.lock().snapshot().identify.get(id),
            Some(&0),
            "a non-pump assignment must not cancel an identify in progress"
        );
    }

    /// Every PWM percentage this verify commanded.
    fn pwm_duties(writes: &WriteLog) -> Vec<u8> {
        writes
            .lock()
            .iter()
            .filter(|(path, _)| path.ends_with("/pwm1"))
            .filter_map(|(_, v)| v.trim().parse::<u8>().ok())
            .map(crate::pwm::raw_to_percent)
            .collect()
    }

    fn verify_test_state() -> (Arc<AppState>, WriteLog, tokio::sync::watch::Sender<bool>) {
        let (state, writes, tx, _tmp) =
            build_verify_state(None, crate::hwmon::roles::HeaderRole::Unknown);
        // `_tmp` is dropped here on purpose: this variant's callers never read a
        // duty back, and its paths were always non-existent.
        (state, writes, tx)
    }

    /// Verify harness with a REAL `pwm1` file holding `raw` (0..=255), so
    /// `initial.pwm_percent` is a genuine value rather than the 50% default, and
    /// with the header's inferred role set.
    ///
    /// The real file is the load-bearing part: at the 50% default the ordinary
    /// and pump branches both choose 80%, so a test built on the old harness
    /// passed with the role check deleted.
    fn verify_test_state_at_duty(
        raw: u8,
        role: crate::hwmon::roles::HeaderRole,
    ) -> (
        Arc<AppState>,
        WriteLog,
        tokio::sync::watch::Sender<bool>,
        tempfile::TempDir,
    ) {
        let (state, writes, tx, tmp) = build_verify_state(Some(raw), role);
        (state, writes, tx, tmp.expect("a duty was requested"))
    }

    fn build_verify_state(
        initial_raw: Option<u8>,
        role: crate::hwmon::roles::HeaderRole,
    ) -> (
        Arc<AppState>,
        WriteLog,
        tokio::sync::watch::Sender<bool>,
        Option<tempfile::TempDir>,
    ) {
        let (pwm_path, enable_path, tmp) = match initial_raw {
            Some(raw) => {
                let dir = tempfile::tempdir().unwrap();
                let p = dir.path().join("pwm1");
                let e = dir.path().join("pwm1_enable");
                std::fs::write(&p, format!("{raw}\n")).unwrap();
                std::fs::write(&e, "1\n").unwrap();
                (
                    p.display().to_string(),
                    Some(e.display().to_string()),
                    Some(dir),
                )
            }
            // Paths need not exist: the state reads are best-effort `.ok()` and a
            // missing file simply yields `None`.
            None => (
                "/nonexistent/pwm1".to_string(),
                Some("/nonexistent/pwm1_enable".to_string()),
                None,
            ),
        };
        let header = crate::hwmon::pwm_discovery::PwmHeaderDescriptor {
            id: "hwmon:test:dev:pwm1".into(),
            label: if role.is_pump() {
                "AIO_PUMP"
            } else {
                "SYS_FAN"
            }
            .into(),
            chip_name: "test".into(),
            device_id: "dev".into(),
            pwm_index: 1,
            supports_enable: true,
            pwm_path,
            enable_path,
            rpm_available: false,
            rpm_path: None,
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: None,
            is_aio: false,
            role,
            role_source: if role.is_pump() {
                crate::hwmon::roles::RoleSource::Label
            } else {
                crate::hwmon::roles::RoleSource::None
            },
            ..Default::default()
        };
        let cache = Arc::new(crate::health::cache::StateCache::new());
        let writes: WriteLog = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let ctrl = crate::hwmon::pwm_control::HwmonPwmController::new(
            vec![header],
            crate::hwmon::lease::LeaseManager::new(),
            Box::new(RecordingWriter(writes.clone())),
            cache.clone(),
        );
        let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let state = Arc::new(AppState {
            cache,
            staleness_config: crate::health::staleness::StalenessConfig::default(),
            daemon_version: "0.0.0-test".into(),
            fan_controller: Arc::new(parking_lot::RwLock::new(None)),
            openfan_runtime: crate::api::handlers::OpenFanRuntime {
                timeout: std::time::Duration::from_millis(500),
                interval: std::time::Duration::from_millis(1000),
                shutdown: shutdown_rx,
            },
            hwmon_controller: Some(Arc::new(parking_lot::Mutex::new(ctrl))),
            start_time: std::time::Instant::now(),
            history: Arc::new(crate::health::history::HistoryRing::new(250)),
            active_profile: Arc::new(parking_lot::Mutex::new(None)),
            calibrating: std::sync::atomic::AtomicBool::new(false),
            characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            validation: std::sync::Arc::new(Default::default()),
            characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
            last_openfan_rescan: Arc::new(parking_lot::Mutex::new(None)),
            adopted_poll_handles: Arc::new(parking_lot::Mutex::new(Vec::new())),
            amd_gpus: Vec::new(),
            intel_gpus: Vec::new(),
            nvidia_gpus: Vec::new(),
            profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
            config_path: String::new(),
            runtime_config_path: tmp
                .as_ref()
                .map(|d| d.path().join("runtime.toml"))
                .unwrap_or_default(),
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
        });
        (state, writes, shutdown_tx, tmp)
    }

    /// DEC-290 regression. The verify used to run inline with an `.await` between
    /// the test write and the restore, so dropping the handler future — a client
    /// disconnect, or the GUI's own 12 s timeout — ran both RAII guards' `Drop`
    /// and skipped the restore entirely, stranding the header at the test duty.
    ///
    /// The whole sequence now runs in one `spawn_blocking`, which is NOT cancelled
    /// when its `JoinHandle` is dropped, and **both guards are moved into it**. So
    /// the observable proof that cancellation no longer strands anything is that
    /// the verify pause is STILL held after the future is dropped: the guards can
    /// only release when the task — restore included — has finished.
    ///
    /// Asserts BOTH halves: the guards are still held immediately after the drop,
    /// and the restore write actually lands afterwards. Waiting out the settle is
    /// free here — dropping the test's runtime blocks on the detached blocking
    /// task regardless, so the 6 s is paid whether or not the assertion is made.
    #[tokio::test]
    async fn a_cancelled_verify_still_restores_the_header() {
        let (state, writes, _shutdown_tx) = verify_test_state();
        let cache = state.cache.clone();

        {
            let fut = hwmon_verify_handler(
                axum::extract::State(state.clone()),
                axum::extract::Path("hwmon:test:dev:pwm1".to_string()),
            );
            tokio::pin!(fut);
            // Let it get past the test write and into the settle, then abandon it
            // exactly the way axum does when a client goes away: drop the future.
            tokio::select! {
                _ = &mut fut => panic!("the verify completed too fast to model a cancellation"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            }
        } // <- future dropped here

        assert!(
            cache.verify_active(),
            "the engine pause was released when the handler future was dropped — \
             the guards are back on the handler, so the restore is skipped again \
             and the header stays at the test duty"
        );

        // The abandoned task keeps running. Poll until it finishes rather than
        // sleeping a fixed span, so this is not a timing guess; the bound is a
        // backstop, not the expected path.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(VERIFY_WAIT_SECONDS as u64 + 6);
        while cache.verify_active() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            !cache.verify_active(),
            "the abandoned verify never finished — it should run to completion"
        );

        // pwm_percent reads come back `None` (the paths do not exist), so the
        // verify takes current=50 and therefore test=80. The restore writes 50.
        let pwm_writes: Vec<String> = writes
            .lock()
            .iter()
            .filter(|(path, _)| path.ends_with("/pwm1"))
            .map(|(_, v)| v.clone())
            .collect();
        let expect_test = crate::pwm::percent_to_raw(80).to_string();
        let expect_restore = crate::pwm::percent_to_raw(50).to_string();
        assert_eq!(
            pwm_writes.first(),
            Some(&expect_test),
            "the verify should have written its test duty first: {pwm_writes:?}"
        );
        assert_eq!(
            pwm_writes.last(),
            Some(&expect_restore),
            "the header was left at the TEST duty after the client went away — \
             this is exactly the DEC-290 defect: {pwm_writes:?}"
        );
    }

    /// DEC-290 review regression. Making the verify uncancellable also made it
    /// survive the shutdown that used to cancel it — and `main.rs` guarantees
    /// "the restore is the guaranteed last writer" (277-c). Without the shutdown
    /// check, a verify caught by SIGTERM writes its duty AFTER
    /// `restore_hwmon_to_auto` hands the header back, and `set_pwm`'s enable
    /// watchdog then reads that restore's `pwm_enable=2`, calls it a BIOS reclaim,
    /// and re-asserts `pwm_enable=1` — leaving the header latched in manual at a
    /// fixed duty with no writer left in the process.
    ///
    /// Firmware control is strictly safer than a duty nothing will ever revise,
    /// so the restore is skipped once shutdown is signalled.
    #[tokio::test]
    async fn a_verify_running_into_shutdown_does_not_rewrite_the_header() {
        let (state, writes, shutdown_tx) = verify_test_state();
        let cache = state.cache.clone();

        {
            let fut = hwmon_verify_handler(
                axum::extract::State(state.clone()),
                axum::extract::Path("hwmon:test:dev:pwm1".to_string()),
            );
            tokio::pin!(fut);
            tokio::select! {
                _ = &mut fut => panic!("the verify completed too fast to model shutdown"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            }
            // The daemon starts shutting down while the verify is mid-settle.
            shutdown_tx.send(true).unwrap();
        }

        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(VERIFY_WAIT_SECONDS as u64 + 6);
        while cache.verify_active() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(!cache.verify_active(), "the verify task never finished");

        let pwm_writes: Vec<String> = writes
            .lock()
            .iter()
            .filter(|(path, _)| path.ends_with("/pwm1"))
            .map(|(_, v)| v.clone())
            .collect();
        assert_eq!(
            pwm_writes.len(),
            1,
            "the verify wrote the header again during shutdown — its restore \
             would land after the hardware restore and re-latch manual mode: \
             {pwm_writes:?}"
        );
    }

    /// DEC-170 regression: post-2.0.0 the client holds no hwmon lease, so a
    /// `HwmonControlError::Lease(_)` from a verify write is an internal race
    /// (the daemon's own "verify" lease lapsed mid-call), not a client
    /// precondition. Every `LeaseError` variant must map to a retryable
    /// `503 hardware_unavailable` — never the retired `403 lease_required` or
    /// `409 lease_already_held` client codes. Pins the arm-collapse so a future
    /// edit cannot reintroduce a lease-named client error.
    #[test]
    fn hwmon_control_error_response_maps_no_lease_to_503() {
        let err = HwmonControlError::Lease(LeaseError::NoLease);
        let (status, body) = hwmon_control_error_response(err);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json = body.0;
        assert_eq!(json["error"]["code"], "hardware_unavailable");
        assert_eq!(json["error"]["retryable"], true);
    }

    #[test]
    fn hwmon_control_error_response_maps_invalid_lease_to_503() {
        let err = HwmonControlError::Lease(LeaseError::InvalidLease);
        let (status, body) = hwmon_control_error_response(err);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json = body.0;
        assert_eq!(json["error"]["code"], "hardware_unavailable");
        assert_eq!(json["error"]["retryable"], true);
    }

    #[test]
    fn hwmon_control_error_response_maps_expired_lease_to_503() {
        let err = HwmonControlError::Lease(LeaseError::Expired);
        let (status, body) = hwmon_control_error_response(err);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json = body.0;
        assert_eq!(json["error"]["code"], "hardware_unavailable");
        assert_eq!(json["error"]["retryable"], true);
    }

    #[test]
    fn hwmon_control_error_response_maps_already_held_to_503() {
        // The AlreadyHeld arm used to map to 409 lease_already_held; after the
        // DEC-170 collapse it joins the wildcard → 503 hardware_unavailable.
        let err = HwmonControlError::Lease(LeaseError::AlreadyHeld {
            owner: HwmonWriter::Verify,
            ttl_seconds: 6,
        });
        let (status, body) = hwmon_control_error_response(err);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json = body.0;
        assert_eq!(json["error"]["code"], "hardware_unavailable");
        assert_eq!(json["error"]["retryable"], true);
    }

    /// B1: classify_verify_result `details` must acknowledge that an
    /// in-process concurrent writer is a possible cause of a register
    /// change during the verify wait. Before B1 the wording named BIOS/EC
    /// as the only cause, which produced false `pwm_value_clamped`
    /// verdicts on the X870E AORUS MASTER where the GUI's own control
    /// loop was the racer.
    #[test]
    fn pwm_value_clamped_details_acknowledges_concurrent_writers() {
        let initial = HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(128),
            pwm_percent: Some(50),
            rpm: Some(1000),
        };
        // pwm_raw drifted significantly from the test value — should
        // produce pwm_value_clamped.
        let final_state = HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(200),
            pwm_percent: Some(78),
            rpm: Some(1500),
        };
        let (result, details) = classify_verify_result(&initial, &final_state, 20);
        assert_eq!(result, "pwm_value_clamped");
        // Headline cause is still BIOS/EC — the most common case in the wild.
        assert!(details.contains("BIOS/EC"), "details: {details:?}");
        // But the wording must also call out the concurrent-writer alternative.
        assert!(details.contains("another writer"), "details: {details:?}");
        // The disambiguation hint must be present so users know how to confirm.
        assert!(
            details.contains("Re-run with no profile active"),
            "details: {details:?}"
        );
    }

    // ── [HOST-a / DEC-326] the driver's full-speed alias ─────────────

    #[test]
    fn verify_does_not_report_a_reclaim_for_its_own_full_speed_write() {
        let initial = HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(153),
            pwm_percent: Some(60),
            rpm: Some(1000),
        };
        // The test wrote 100%; the driver now reports mode 0 because the duty
        // register holds 0xff. Nothing reclaimed anything.
        let final_state = HwmonVerifyState {
            pwm_enable: Some(0),
            pwm_raw: Some(255),
            pwm_percent: Some(100),
            rpm: Some(1436),
        };
        let (result, _details) = classify_verify_result(&initial, &final_state, 100);
        assert_ne!(
            result, "pwm_enable_reverted",
            "the header accepted the write; reporting a BIOS reclaim would be a lie"
        );
    }

    #[test]
    fn verify_still_reports_a_reclaim_when_the_mode_is_genuinely_lost() {
        // Opposite branch, and the reachability argument in one test: this is
        // the same 100% test duty, but mode 2 (automatic) is a real reclaim.
        let initial = HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(153),
            pwm_percent: Some(60),
            rpm: Some(1000),
        };
        let final_state = HwmonVerifyState {
            pwm_enable: Some(2),
            pwm_raw: Some(255),
            pwm_percent: Some(100),
            rpm: Some(1436),
        };
        let (result, _) = classify_verify_result(&initial, &final_state, 100);
        assert_eq!(result, "pwm_enable_reverted");
    }

    #[test]
    fn a_header_the_firmware_already_pins_at_full_speed_does_not_verify_as_a_pass() {
        // [DEC-326 remediation, `concurrency-reviewer` finding 4] The exemption
        // requires evidence the daemon HELD manual mode. Without that term, a
        // header the firmware owns at full speed is indistinguishable from the
        // alias — enable 0, duty 100%, zero `pwm_raw` delta — and the endpoint
        // built to answer "does this header accept writes?" answers yes.
        let initial = HwmonVerifyState {
            pwm_enable: Some(0), // firmware already owns it: never was 1
            pwm_raw: Some(255),
            pwm_percent: Some(100),
            rpm: Some(1436),
        };
        let final_state = HwmonVerifyState {
            pwm_enable: Some(0),
            pwm_raw: Some(255),
            pwm_percent: Some(100),
            rpm: Some(1436),
        };
        let (result, _) = classify_verify_result(&initial, &final_state, 100);
        assert_eq!(
            result, "pwm_enable_reverted",
            "the daemon never held manual mode here, so this is not our duty \
             read back — it is a header we do not control"
        );
    }

    #[test]
    fn a_pump_at_an_ordinary_idle_duty_is_verified_at_exactly_full_speed() {
        // Why the alias case above is not hypothetical. The window is DERIVED
        // from the real function rather than asserted as a literal, so it stays
        // true if DELTA or the pump floor ever move — the failure this pins is
        // "no pump duty reaches 100" (exposure gone, test now vacuous), not a
        // particular arithmetic result.
        let window: Vec<u8> = (0..=100u8)
            .filter(|&c| verify_test_duty(true, c) == 100)
            .collect();
        assert!(
            !window.is_empty(),
            "if no pump duty verifies at 100%, the alias case above is unreachable \
             and must be re-justified rather than silently kept"
        );
        // Measured today: 60..=65 — an ordinary idle duty for an AIO pump, which
        // is what makes the false `pwm_enable_reverted` verdict reachable in
        // normal use rather than only under a contrived duty.
        assert!(
            window.iter().all(|&c| (30..=99).contains(&c)),
            "the window must sit inside ordinary running duties, got {window:?}"
        );
        // ...and no non-pump header reaches it, which bounds the exposure.
        assert!(
            (0..=100u8).all(|c| verify_test_duty(false, c) != 100),
            "an ordinary fan is never verified at full speed"
        );
    }

    #[test]
    fn pwm_enable_reverted_details_acknowledges_concurrent_writers() {
        let initial = HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(128),
            pwm_percent: Some(50),
            rpm: Some(1000),
        };
        // pwm_enable flipped back to 2 (auto/cruise) — pwm_enable_reverted.
        let final_state = HwmonVerifyState {
            pwm_enable: Some(2),
            pwm_raw: Some(128),
            pwm_percent: Some(50),
            rpm: Some(1000),
        };
        let (result, details) = classify_verify_result(&initial, &final_state, 20);
        assert_eq!(result, "pwm_enable_reverted");
        assert!(details.contains("BIOS/EC"), "details: {details:?}");
        assert!(
            details.contains("another in-process writer"),
            "details: {details:?}"
        );
        assert!(
            details.contains("Re-run with no profile active"),
            "details: {details:?}"
        );
    }

    /// B1: result enum values must remain unchanged so the GUI's
    /// `hwmon_guidance.verification_guidance` lookup still finds the right
    /// keys without coordinated GUI redeploys. The wording change is
    /// limited to the `details` field.
    #[test]
    fn b1_result_enum_values_unchanged() {
        let initial = HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(128),
            pwm_percent: Some(50),
            rpm: None,
        };
        let reverted = HwmonVerifyState {
            pwm_enable: Some(2),
            pwm_raw: Some(128),
            pwm_percent: Some(50),
            rpm: None,
        };
        let (result, _) = classify_verify_result(&initial, &reverted, 20);
        assert_eq!(result, "pwm_enable_reverted");

        let clamped = HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(200),
            pwm_percent: Some(78),
            rpm: None,
        };
        let (result, _) = classify_verify_result(&initial, &clamped, 20);
        assert_eq!(result, "pwm_value_clamped");
    }

    /// No-op sysfs writer for constructing a bare `HwmonPwmController` in tests
    /// that only exercise lease bookkeeping (no real writes).
    struct NoopWriter;
    impl crate::hwmon::pwm_control::SysfsWriter for NoopWriter {
        fn write_file(
            &mut self,
            _path: &str,
            _value: &str,
        ) -> Result<(), crate::error::HwmonError> {
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, crate::error::HwmonError> {
            Ok("0\n".into())
        }
    }

    #[test]
    fn verify_lease_guard_releases_lease_on_drop() {
        // F1/P2-1: the force-taken "verify" lease must be released on EVERY exit
        // path (incl. a cancelled/panicked handler future), so the profile engine
        // — which deliberately does not adopt a "verify" lease — is never
        // stranded for the lease TTL. Dropping the guard releases it.
        use crate::hwmon::lease::LeaseManager;
        use crate::hwmon::pwm_control::HwmonPwmController;
        let cache = std::sync::Arc::new(crate::health::cache::StateCache::new());
        let ctrl = std::sync::Arc::new(parking_lot::Mutex::new(HwmonPwmController::new(
            vec![],
            LeaseManager::new(),
            Box::new(NoopWriter),
            cache,
        )));
        let lease_id = ctrl
            .lock()
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::Verify)
            .lease_id;
        assert!(ctrl.lock().lease_manager().active_lease().is_some());
        {
            let _g = VerifyLeaseGuard {
                controller: ctrl.clone(),
                lease_id,
            };
        } // guard drops here
        assert!(
            ctrl.lock().lease_manager().active_lease().is_none(),
            "the verify lease must be released when the guard drops"
        );
    }
}
