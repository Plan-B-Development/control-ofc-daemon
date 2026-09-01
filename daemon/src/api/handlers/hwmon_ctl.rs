//! Hwmon PWM and verify endpoints.
//!
//! Named `hwmon_ctl` to avoid confusion with the top-level `crate::hwmon` module.

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

    // DEC-146 P3-12: single mapping source — From<&PwmHeaderDescriptor>.
    let headers = ctrl
        .headers()
        .into_iter()
        .map(PwmHeaderEntry::from)
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
            // DEC-146 P3-12: single mapping source — From<&PwmHeaderDescriptor>.
            let entries: Vec<PwmHeaderEntry> = headers.iter().map(PwmHeaderEntry::from).collect();
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
    let bg_lease_id = verify_lease_id.clone();
    let join = tokio::task::spawn_blocking(move || {
        // Moved, not borrowed: dropped only when this task finishes, so the
        // pause and the lease outlive every write below on every path.
        let verify_guard = verify_guard;
        let _verify_lease = verify_lease;

        let read_state =
            |pwm: &str, en: &Option<String>, rpm: &Option<String>| -> HwmonVerifyState {
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

        let initial = read_state(&pwm_path, &enable_path, &rpm_path);

        // Test PWM: a significant delta from current, in whichever direction has room.
        let current_pct = initial.pwm_percent.unwrap_or(50);
        let test_pct: u8 = if current_pct > 50 { 20 } else { 80 };

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

        let final_state = read_state(&pwm_path, &enable_path, &rpm_path);

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

        let restore_failed = {
            let mut ctrl = bg_controller.lock();
            match ctrl.set_pwm(&bg_header_id, current_pct, &bg_lease_id) {
                Ok(_) => false,
                Err(e) => {
                    log::warn!(
                        "verify: restore PWM to {current_pct}% on {bg_header_id} \
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
    // Check if pwm_enable was reclaimed
    if let Some(final_enable) = final_state.pwm_enable {
        if final_enable != 1 {
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

    fn verify_test_state() -> (Arc<AppState>, WriteLog, tokio::sync::watch::Sender<bool>) {
        let header = crate::hwmon::pwm_discovery::PwmHeaderDescriptor {
            id: "hwmon:test:dev:pwm1".into(),
            label: "SYS_FAN".into(),
            chip_name: "test".into(),
            device_id: "dev".into(),
            pwm_index: 1,
            supports_enable: true,
            // Paths need not exist: the state reads are best-effort `.ok()` and a
            // missing file simply yields `None`, which is all this test needs.
            pwm_path: "/nonexistent/pwm1".into(),
            enable_path: Some("/nonexistent/pwm1_enable".into()),
            rpm_available: false,
            rpm_path: None,
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: None,
            is_aio: false,
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
            override_table: Arc::new(parking_lot::Mutex::new(
                crate::control_override::OverrideTable::new(),
            )),
            allow_port_probe: false,
            running_config: Default::default(),
            readiness_rollup: readiness_rollup.clone(),
            assessment: Arc::new(crate::api::handlers::AssessmentCache::new(readiness_rollup)),
        });
        (state, writes, shutdown_tx)
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
