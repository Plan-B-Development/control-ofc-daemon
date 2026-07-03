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
/// next tick, every verify (P3-3).
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
    let Some(_verify_guard) =
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
    let _verify_lease = VerifyLeaseGuard {
        controller: controller.clone(),
        lease_id: verify_lease_id.clone(),
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

    // Write test value via controller (sets pwm_enable=1 + PWM).
    // Route errors through the shared HwmonControlError mapper: if the daemon's
    // own force-taken "verify" lease lapses between here and the write, that is
    // an internal race, surfaced as a retryable 503 hardware_unavailable
    // (DEC-170) — not a client lease error and not a 500 internal_error.
    {
        let mut ctrl = controller.lock();
        if let Err(e) = ctrl.set_pwm(&header_id, test_pct, &verify_lease_id) {
            return hwmon_control_error_response(e);
        }
    }

    // Wait for hardware to respond
    tokio::time::sleep(std::time::Duration::from_secs(VERIFY_WAIT_SECONDS as u64)).await;

    // Read back state after wait
    let final_state = read_state(&pwm_path, &enable_path, &rpm_path);

    // Restore original PWM. Failures here are surfaced via
    // ``restore_failed`` rather than overwriting the diagnostic verify
    // outcome — a successful verify with a failed restore is its own
    // condition the caller can act on (typically: re-write the desired
    // PWM). Previously the error was silently swallowed.
    let restore_failed = {
        let mut ctrl = controller.lock();
        match ctrl.set_pwm(&header_id, current_pct, &verify_lease_id) {
            Ok(_) => false,
            Err(e) => {
                log::warn!(
                    "verify: restore PWM to {current_pct}% on {header_id} \
                     failed (header left at test value {test_pct}%): {e}"
                );
                true
            }
        }
    };

    // The "verify" lease is released by `_verify_lease`'s RAII guard on every
    // exit path (see its definition above), and the engine's write pause by
    // `_verify_guard` — both on scope exit, so a cancelled/panicked verify can
    // never strand hwmon control.

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
