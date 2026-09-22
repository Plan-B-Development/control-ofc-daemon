//! `POST /hwmon/{header_id}/stall-probe` and `GET`/`DELETE
//! /diagnostics/stall-probe` (DEC-407, DEC-404 Stage 3).
//!
//! [SAFETY] The one diagnostic that writes below 20 % on purpose. Read
//! `api::stall_probe`'s module docs for the envelope before changing anything
//! here; this file only gathers state, applies the entry refusals and owns the
//! guards whose drop order the restore depends on.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::characterization::RestoreOutcome;
use crate::api::responses::{ErrorEnvelope, HwmonVerifyState};
use crate::api::stall_probe as sp;
use crate::hwmon::lease::HwmonWriter;

/// Start a stall/restart probe. Returns **202** with the run snapshot; the probe
/// runs detached and the client polls `GET /diagnostics/stall-probe`.
///
/// Refusals, in order — every one of them before anything is written:
/// shutting down (503) · no `acknowledge_below_floor: true` (400) · the three
/// thermal guards · no hwmon controller (503) · unknown header (404) · not
/// eligible (400 `validation_error`, `details.reason` = the `INELIGIBLE_*`
/// token; a read-only header or one without a tach is `feature_unavailable`) ·
/// no fresh CPU temperature (400, retryable) · the single-flight slot (409).
///
/// [SAFETY] The task is detached, so it is NOT in `main::shutdown_sequence`'s
/// `task_handles`. What makes that safe is the shutdown check inside the shared
/// `characterization::RestoreOnDrop`, plus `run_probe`'s own shutdown check on
/// every write and every sample — the guard covers only the restore.
pub async fn stall_probe_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(header_id): axum::extract::Path<String>,
    Json(body): Json<sp::StallProbeRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // [SAFETY] Refuse once the daemon is going down (DEC-317), for the reason
    // every diagnostic POST does: a write after `hand_back_hwmon` re-asserts
    // `pwm_enable=1` through the reclaim watchdog on a header nothing will drive.
    if *state.openfan_runtime.shutdown.borrow() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("the daemon is shutting down"),
        );
    }
    // [SAFETY] S3-6: the explicit acknowledgement, before anything else is
    // looked at. The GUI sends it after its own per-header confirmation.
    if body.acknowledge_below_floor != Some(true) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(
                "the stall probe drives this header below 20% and down to 0%; send \
                 {\"acknowledge_below_floor\": true} to confirm",
            ),
        );
    }
    if let Some(resp) = super::verify_thermal_guard(&state.cache) {
        return resp;
    }
    if let Some(resp) =
        super::stale_temperature_guard(&state.cache, sp::STALL_PROBE_DIAGNOSTIC, "the stall probe")
    {
        return resp;
    }
    let Some(controller) = state.hwmon_controller.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let (pwm_path, enable_path, rpm_path, is_writable) = {
        let ctrl = controller.lock();
        match ctrl.header(&header_id) {
            Some(h) => (
                h.pwm_path.clone(),
                h.enable_path.clone(),
                h.rpm_path.clone(),
                h.is_writable,
            ),
            None => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!("unknown header: {header_id}")),
                )
            }
        }
    };
    let has_tach = rpm_path.is_some();

    // [SAFETY] The eligibility rule — the same function the preflight row and
    // the mid-run re-check call. The pump UNION, never the wire role (DEC-312);
    // the controller lock above is released, as `header_role_parts` requires.
    let pump_protected = state.header_is_pump_protected(&header_id);
    let role = state.resolved_header_role(&header_id);
    if let Some(token) = sp::ineligibility(role, pump_protected, is_writable, has_tach) {
        let message = format!(
            "header {header_id} cannot be stall-probed: {}",
            sp::ineligibility_detail(token)
        );
        let details = serde_json::json!({ "reason": token, "role": role.as_str() });
        let envelope = if matches!(token, sp::INELIGIBLE_READ_ONLY | sp::INELIGIBLE_NO_TACH) {
            let mut e = ErrorEnvelope::feature_unavailable(message);
            e.error.details = Some(details);
            e
        } else {
            ErrorEnvelope::validation_with_details(message, details)
        };
        return error_response(StatusCode::BAD_REQUEST, &envelope);
    }
    // [SAFETY] S3-3: the +5 °C rise gate needs a fresh CPU reading to compare
    // against. Retryable — the reading may simply be between polls.
    if sp::hottest_fresh_cpu_c(&state.cache).is_none() {
        let mut e = ErrorEnvelope::validation(
            "no fresh CPU temperature reading, so the stall probe's rise gate cannot be \
             evaluated",
        );
        e.error.retryable = true;
        e.error.details = Some(serde_json::json!({ "reason": sp::ABORT_NO_CPU_TEMPERATURE }));
        return error_response(StatusCode::BAD_REQUEST, &e);
    }

    // The SAME single-flight slot every hardware diagnostic claims, so at most
    // one ever drives hardware and the engine's write phase is paused for the
    // probe's lifetime — every sibling holds its last commanded duty.
    let Some(verify_guard) =
        super::begin_verify_pause(&state.cache, crate::constants::VERIFY_PAUSE_DEADMAN)
    else {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation("a hardware verify or calibration is already in progress"),
        );
    };

    let verify_lease_id = {
        let mut ctrl = controller.lock();
        ctrl.lease_manager_mut()
            .force_take_lease(HwmonWriter::Verify)
            .lease_id
    };
    let verify_lease = super::hwmon_ctl::VerifyLeaseGuard {
        controller: controller.clone(),
        lease_id: verify_lease_id.clone(),
    };
    let lease_for_renew = verify_lease_id.clone();

    let run = sp::StallProbeRun {
        run_id: sp::next_run_id(),
        header_id: header_id.clone(),
        state: crate::api::characterization::STATE_RUNNING.to_string(),
        rise_limit_c: crate::constants::STALL_PROBE_RISE_LIMIT_C,
        // Read by the task, on the blocking pool, and published from there: no
        // sysfs read runs on this async worker while the slot and the lease are
        // held (a wedged chip would park the worker with both guards).
        original_pct: None,
        restore_outcome: RestoreOutcome::Pending.token().to_string(),
        provenance: sp::provenance_legend(),
        ..Default::default()
    };
    // Cancel flag cleared and run installed under ONE lock, paired with the
    // cancel handler's check-and-set — the characterisation rule.
    {
        let mut slot_guard = state.stall_probe.lock();
        state.stall_probe_cancel.store(false, Ordering::SeqCst);
        *slot_guard = Some(run.clone());
    }

    let slot = state.stall_probe.clone();
    let my_run_id = run.run_id.clone();
    let cancel = state.stall_probe_cancel.clone();
    let cache = state.cache.clone();
    let ctrl_arc = controller.clone();
    let shutdown_rx = state.openfan_runtime.shutdown.clone();
    let hid = header_id.clone();
    let state_for_eligibility = state.clone();

    tokio::spawn(async move {
        let report = crate::api::characterization::RestoreReport::new();
        // The chip's declared tach cadence (S3-2), read off the runtime and
        // bounded like every probe read. A wedge here is simply "not declared";
        // the probe's own first read then finds the wedge and stops.
        let interval_path = pwm_path.clone();
        let driver_refresh_ms = tokio::time::timeout(
            crate::constants::STALL_PROBE_READ_BUDGET,
            tokio::task::spawn_blocking(move || {
                super::discovery::read_update_interval(&interval_path)
            }),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
        // Guard drop order is load-bearing, and is the characterisation order:
        // `run_probe` declares its own `RestoreOnDrop` internally, so that guard
        // drops when the probe future completes — BEFORE `pause` and `_lease`
        // below, which is the only order in which the restore can still succeed.
        {
            let pause = verify_guard;
            let _lease = verify_lease;

            // [SAFETY] Renews BOTH the engine pause and the hwmon lease (DEC-296).
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
            // On the blocking pool, per the DEC-290 / `P8-am` precedent: a tach
            // `open(2)` wedged in a driver parks a pool thread instead of a
            // tokio worker. [SAFETY] And BOUNDED (S3-R4): while a read is
            // outstanding no per-sample gate runs, so an unbounded join would
            // leave the header at a probe duty — possibly 0 % — with the rise,
            // thermal and budget gates blind. A read that does not return within
            // `STALL_PROBE_READ_BUDGET` is `None`, a wedge, and ends the run. A
            // failed join is a read that finished without a value: unreadable,
            // never a value.
            let paths = Arc::new((pwm_path, enable_path, rpm_path));
            let read_fn = move || {
                let paths = paths.clone();
                async move {
                    let join = tokio::task::spawn_blocking(move || {
                        super::hwmon_ctl::read_header_state(&paths.0, &paths.1, &paths.2)
                    });
                    match tokio::time::timeout(crate::constants::STALL_PROBE_READ_BUDGET, join)
                        .await
                    {
                        Err(_elapsed) => None,
                        Ok(joined) => Some(joined.unwrap_or(HwmonVerifyState {
                            pwm_enable: None,
                            pwm_raw: None,
                            pwm_percent: None,
                            rpm: None,
                        })),
                    }
                }
            };
            // [SAFETY] The mid-run re-check (the `TS-aw` lesson), before every
            // write and on every sample (S3-R2), through the same two lookups and
            // the same rule as the entry check above.
            // Neither lookup holds a lock across the other, per
            // `header_is_pump_protected`'s ABBA note.
            let eligible = || {
                let pump = state_for_eligibility.header_is_pump_protected(&hid);
                let role = state_for_eligibility.resolved_header_role(&hid);
                sp::Eligibility {
                    ineligible: sp::ineligibility(role, pump, is_writable, has_tach),
                    pump_protected: pump,
                }
            };
            // Fenced on `run_id`: a run whose deadman elapsed can be superseded,
            // and without the fence the loser would publish over the winner.
            let publish = |r: &sp::ProbeResult| {
                if let Some(run) = slot.lock().as_mut() {
                    if run.run_id == my_run_id && run.is_running() {
                        let mut progress = r.clone();
                        // Mid-run publishes never end the run; the terminal
                        // publish below is the only one allowed to. Nor do they
                        // carry its ending: the loop settles `outcome` before
                        // the kick, and a `running` run must never read as
                        // finished while that kick is still being held.
                        progress.state = "";
                        progress.outcome = None;
                        progress.abort_reason = None;
                        progress.detail = None;
                        run.apply(&progress);
                    }
                }
            };

            let result = sp::run_probe(
                &cache,
                &hid,
                // [SAFETY] 0 for an eligible header — a pump is refused above.
                // `run_probe` raises it to the pump floor if a pump role appears
                // while the probe runs (`AUD3-l`).
                0,
                std::time::Duration::from_secs(crate::constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
                driver_refresh_ms,
                write_fn,
                read_fn,
                eligible,
                &cancel,
                shutting_down,
                keepalive,
                &report,
                publish,
            )
            .await;

            // Terminal publish, INSIDE the guarded scope and fenced on `run_id`:
            // the slot is released the moment this block ends.
            if let Some(run) = slot.lock().as_mut() {
                if run.run_id == my_run_id {
                    run.apply(&result);
                    let restore = report.get();
                    run.restore_failed = restore.header_left_moved();
                    run.restore_outcome = restore.token().to_string();
                    run.completed_unix_ms = Some(crate::control_paths::unix_ms());
                }
            }
        };
    });

    json_ok(StatusCode::ACCEPTED, run)
}

/// GET /diagnostics/stall-probe — the current or most recent run. Held in
/// memory only (S3-8): the report built on it is the record.
pub async fn stall_probe_status_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.stall_probe.lock().clone() {
        Some(run) => json_ok(StatusCode::OK, run),
        None => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no stall probe has run"),
        ),
    }
}

/// DELETE /diagnostics/stall-probe — ask the running probe to stop.
///
/// Honoured on the next sample (≤ 500 ms), unlike a characterisation settle,
/// and it always ends with the recovery kick and then the restore.
pub async fn stall_probe_cancel_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // ONE lock across the check and the set (the characterisation rule).
    let snapshot = {
        let guard = state.stall_probe.lock();
        match guard.as_ref() {
            Some(run) if run.is_running() => {
                state.stall_probe_cancel.store(true, Ordering::SeqCst);
                run.clone()
            }
            _ => {
                return error_response(
                    StatusCode::CONFLICT,
                    &ErrorEnvelope::validation("no stall probe is running"),
                )
            }
        }
    };
    json_ok(StatusCode::ACCEPTED, snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handlers::hwmon_ctl::tests::{build_verify_state_with, pwm_duties, WriteLog};
    use crate::hwmon::roles::HeaderRole;
    use std::time::Duration;

    const HID: &str = "hwmon:test:dev:pwm1";

    /// A fan on the fake tree that stops at or below raw 15 (6 %) and spins
    /// above it — the tach follows every `pwm1` write.
    fn stall_fan(raw: u32) -> u32 {
        if raw <= 15 {
            0
        } else {
            300 + raw * 4
        }
    }

    struct Fixture {
        state: Arc<AppState>,
        writes: WriteLog,
        shutdown: tokio::sync::watch::Sender<bool>,
        _tmp: Option<tempfile::TempDir>,
    }

    /// A header at 40 % on a real fake-sysfs tree. With `fan`, it has a tach
    /// and the driver declares a 1 s `update_interval`.
    fn fixture(role: HeaderRole, fan: bool) -> Fixture {
        let (state, writes, shutdown, tmp) =
            build_verify_state_with(Some(102), role, fan.then_some(stall_fan as fn(u32) -> u32));
        if let Some(dir) = &tmp {
            std::fs::write(dir.path().join("update_interval"), "1000\n").unwrap();
        }
        Fixture {
            state,
            writes,
            shutdown,
            _tmp: tmp,
        }
    }

    async fn post(
        state: &Arc<AppState>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let (status, Json(b)) = stall_probe_handler(
            State(state.clone()),
            axum::extract::Path(HID.to_string()),
            Json(serde_json::from_value(body).expect("request")),
        )
        .await;
        (status, b)
    }

    fn ack() -> serde_json::Value {
        serde_json::json!({ "acknowledge_below_floor": true })
    }

    /// Poll the slot (virtual time) until the run is no longer running.
    async fn finished(state: &Arc<AppState>) -> sp::StallProbeRun {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        loop {
            if let Some(r) = state.stall_probe.lock().clone() {
                if !r.is_running() && r.restore_outcome != "pending" {
                    return r;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the probe never finished"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    fn pump_profile(member_label: &str) -> crate::profile::DaemonProfile {
        crate::profile::DaemonProfile {
            id: "p".into(),
            name: "p".into(),
            version: 7,
            description: String::new(),
            controls: vec![crate::profile::LogicalControl {
                id: "ctl".into(),
                name: "ctl".into(),
                mode: "manual".into(),
                curve_id: String::new(),
                manual_output_pct: 50.0,
                members: vec![crate::profile::ControlMember {
                    source: "hwmon".into(),
                    member_id: HID.into(),
                    member_label: member_label.into(),
                    fan_zero_rpm: false,
                }],
                step_up_pct: 100.0,
                step_down_pct: 100.0,
                offset_pct: 0.0,
                minimum_pct: 20.0,
                start_pct: 0.0,
                stop_pct: 0.0,
            }],
            curves: vec![],
        }
    }

    /// The whole route, end to end, on the fake tree: the handler's gatherer,
    /// the real controller's writes, the blocking-pool reads, the driver's
    /// declared cadence and the terminal publish.
    #[tokio::test(start_paused = true)]
    async fn an_eligible_chassis_fan_is_probed_end_to_end() {
        let f = fixture(HeaderRole::ChassisFan, true);
        let (status, body) = post(&f.state, ack()).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["state"], "running");
        // Concurrency F3: the POST reads no sysfs, so its snapshot cannot know
        // the pre-probe duty yet; the task publishes it.
        assert!(body["original_pct"].is_null(), "{body}");

        let run = finished(&f.state).await;
        assert_eq!(run.original_pct, Some(40));
        assert_eq!(
            run.outcome.as_deref(),
            Some(sp::OUTCOME_STALL_AND_RESTART_FOUND),
            "{run:?}"
        );
        assert_eq!(run.stall_duty_pct, Some(6));
        assert_eq!(run.restart_duty_pct, Some(8));
        assert_eq!(run.hysteresis_pct, Some(2));
        assert_eq!(run.refresh_source.as_deref(), Some(sp::REFRESH_DRIVER));
        assert_eq!(run.refresh_ms, Some(1000));
        assert_eq!(run.restore_outcome, "restored");
        assert!(!run.restore_failed);
        assert!(run.completed_unix_ms.is_some());
        let duties = pwm_duties(&f.writes);
        assert_eq!(duties.iter().min(), Some(&6), "{duties:?}");
        assert_eq!(
            duties.last(),
            Some(&40),
            "restored to the pre-probe duty: {duties:?}"
        );
        // The slot is released for the next diagnostic.
        assert!(!f.state.cache.verify_active());
        let (status, Json(got)) = stall_probe_status_handler(State(f.state.clone())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["outcome"], sp::OUTCOME_STALL_AND_RESTART_FOUND);
    }

    #[tokio::test]
    async fn the_request_must_acknowledge_the_walk_below_20() {
        let f = fixture(HeaderRole::ChassisFan, true);
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "acknowledge_below_floor": false }),
        ] {
            let (status, resp) = post(&f.state, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
            assert_eq!(resp["error"]["code"], "validation_error");
        }
        assert!(pwm_duties(&f.writes).is_empty());
        assert!(f.state.stall_probe.lock().is_none());
    }

    /// No tunables: a crafted request cannot name a duty, a step or a dwell.
    #[test]
    fn unknown_request_fields_are_rejected() {
        for extra in ["points_pct", "step_pct", "dwell_seconds", "floor_pct"] {
            let v = serde_json::json!({ "acknowledge_below_floor": true, extra: 0 });
            assert!(
                serde_json::from_value::<sp::StallProbeRequest>(v).is_err(),
                "{extra} was accepted"
            );
        }
    }

    /// [SAFETY] A crafted, acknowledged request can never take a pump below
    /// 20 % — by any of the three routes into the pump union: the hardware label,
    /// a label the user tried to override, and the active profile's member label
    /// (DEC-384). Nothing is written, and the shared slot is never claimed.
    #[tokio::test]
    async fn a_pump_is_refused_by_every_route_into_the_union() {
        // (1) the hardware says pump.
        let labelled = fixture(HeaderRole::Pump, true);
        // (2) … and the user assigned chassis_fan over it (DEC-312).
        let overridden = fixture(HeaderRole::Pump, true);
        overridden.state.header_roles.write().clone_from(&Arc::new(
            [(HID.to_string(), HeaderRole::ChassisFan)]
                .into_iter()
                .collect(),
        ));
        assert_eq!(
            overridden.state.resolved_header_role(HID),
            HeaderRole::ChassisFan,
            "precondition: the display role reads eligible"
        );
        // (3) no pump evidence on the header; the active profile names it one.
        let profiled = fixture(HeaderRole::ChassisFan, true);
        *profiled.state.active_profile.lock() = Some(pump_profile("Pump"));

        for (name, f) in [
            ("labelled", &labelled),
            ("overridden", &overridden),
            ("profiled", &profiled),
        ] {
            assert!(
                f.state.header_is_pump_protected(HID),
                "{name}: precondition"
            );
            let (status, body) = post(&f.state, ack()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {body}");
            assert_eq!(
                body["error"]["details"]["reason"],
                sp::INELIGIBLE_PUMP_PROTECTED,
                "{name}"
            );
            assert!(
                pwm_duties(&f.writes).is_empty(),
                "{name}: wrote {:?}",
                pwm_duties(&f.writes)
            );
            assert!(!f.state.cache.verify_active(), "{name}: claimed the slot");
            assert!(f.state.stall_probe.lock().is_none(), "{name}");
        }
    }

    #[tokio::test]
    async fn cpu_fan_unknown_role_and_no_tach_are_refused() {
        for (role, fan, reason, code) in [
            (
                HeaderRole::CpuFan,
                true,
                sp::INELIGIBLE_CPU_FAN,
                "validation_error",
            ),
            (
                HeaderRole::Unknown,
                true,
                sp::INELIGIBLE_ROLE_UNKNOWN,
                "validation_error",
            ),
            (
                HeaderRole::ChassisFan,
                false,
                sp::INELIGIBLE_NO_TACH,
                "feature_unavailable",
            ),
        ] {
            let f = fixture(role, fan);
            let (status, body) = post(&f.state, ack()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{reason}: {body}");
            assert_eq!(body["error"]["code"], code, "{reason}");
            assert_eq!(body["error"]["details"]["reason"], reason);
            assert!(pwm_duties(&f.writes).is_empty(), "{reason}");
        }
    }

    /// S3-5: the DISPLAY role decides, so a user who assigns `radiator_fan` to a
    /// CPU-fan header makes it eligible. Cancelled mid-run, which must end with
    /// the kick and then the restore.
    #[tokio::test(start_paused = true)]
    async fn a_radiator_assignment_makes_a_cpu_header_eligible_and_a_cancel_kicks() {
        let f = fixture(HeaderRole::CpuFan, true);
        f.state.header_roles.write().clone_from(&Arc::new(
            [(HID.to_string(), HeaderRole::RadiatorFan)]
                .into_iter()
                .collect(),
        ));
        let (status, body) = post(&f.state, ack()).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        // Into the descent: the 20 % baseline holds up to 12 s.
        tokio::time::sleep(Duration::from_secs(20)).await;
        let before_cancel = pwm_duties(&f.writes).len();
        assert!(
            pwm_duties(&f.writes).iter().any(|&d| d < 20),
            "precondition: the probe is below 20 % when cancelled: {:?}",
            pwm_duties(&f.writes)
        );
        let (status, _) = stall_probe_cancel_handler(State(f.state.clone())).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        // Concurrency F4, asserted across the whole kick, not at its end: the
        // loop settles `outcome` before the kick is held, and a run that still
        // reads `running` must never look finished while it is.
        let mut running_seen = 0;
        loop {
            let snap = f.state.stall_probe.lock().clone().expect("a run");
            if !snap.is_running() {
                break;
            }
            running_seen += 1;
            assert_eq!(
                snap.outcome, None,
                "running, yet published an outcome: {snap:?}"
            );
            assert_eq!(snap.abort_reason, None);
            assert_eq!(snap.detail, None);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            running_seen > 10,
            "precondition: the kick window was observed ({running_seen})"
        );

        let run = finished(&f.state).await;
        assert_eq!(run.state, "cancelled");
        assert_eq!(run.outcome.as_deref(), Some(sp::OUTCOME_CANCELLED));
        let after: Vec<u8> = pwm_duties(&f.writes)[before_cancel..].to_vec();
        assert_eq!(after, vec![100, 40], "the kick, then the restore");
        assert_eq!(run.restore_outcome, "restored");
    }

    /// [SAFETY] S3-3, at the POST. A machine whose only fresh temperature is
    /// not a CPU sensor passes the shared staleness guard — it falls back to
    /// every sensor — so only the probe's own check can refuse, and it must
    /// refuse with a 400 rather than accept and abort.
    #[tokio::test]
    async fn no_fresh_cpu_temperature_is_refused_at_the_post() {
        let f = fixture(HeaderRole::ChassisFan, true);
        f.state
            .cache
            .retain_sensors(&std::collections::HashSet::new());
        f.state
            .cache
            .update_sensors(vec![crate::health::state::CachedSensorReading {
                id: "board".into(),
                kind: crate::hwmon::types::SensorKind::MbTemp,
                label: "SYSTIN".into(),
                value_c: 35.0,
                source: crate::health::state::DeviceLabel::Hwmon,
                updated_at: std::time::Instant::now(),
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
                chip_name: "nct6798".into(),
                temp_type: None,
                thresholds: None,
            }]);
        assert!(
            crate::api::calibration::temperature_refusal(&f.state.cache).is_none(),
            "precondition: the shared staleness guard passes"
        );
        let (status, body) = post(&f.state, ack()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["retryable"], true);
        assert_eq!(
            body["error"]["details"]["reason"],
            sp::ABORT_NO_CPU_TEMPERATURE
        );
        assert!(pwm_duties(&f.writes).is_empty());
        assert!(f.state.stall_probe.lock().is_none());
    }

    /// Concurrency F2 / S3-R4, through the REAL read path: a tach whose `open(2)`
    /// blocks in the kernel (a FIFO with no writer — the DEC-342 wedge, not a
    /// sleep) is abandoned after `STALL_PROBE_READ_BUDGET`, and the run ends as
    /// `tach_unreadable` having written nothing. Real time on purpose: tokio will
    /// not auto-advance paused time while a `spawn_blocking` task is outstanding
    /// (tokio-test trap 2), so a paused clock would hang here instead of failing.
    #[tokio::test]
    async fn a_wedged_tach_read_is_abandoned_after_its_budget() {
        let f = fixture(HeaderRole::ChassisFan, true);
        let tach = f._tmp.as_ref().unwrap().path().join("fan1_input");
        std::fs::remove_file(&tach).unwrap();
        let c = std::ffi::CString::new(tach.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        // Self-release (tokio-test trap 3): whatever happens below, the blocked
        // reader is let go, so a failed assertion cannot hang the runtime drop.
        // NON-BLOCKING, both here and below: a blocking write-open of a FIFO
        // waits for a reader, and once the one reader has been released there
        // is none — the first draft of this test hung forever on exactly that.
        // Non-blocking, the open releases a reader blocked in `open(2)` and
        // fails at once (`ENXIO`) when there is none.
        fn release_fifo(path: &std::path::Path) {
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path);
        }
        let releaser = tach.clone();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let release = std::thread::spawn(move || {
            // A clean end sends `()` and one release suffices. Anything else —
            // a failed assertion unwinding (the sender dropped unsent), or the
            // backstop timeout — keeps releasing for a while, because a probe
            // left running can issue further reads that would each block again
            // and turn a red test into a hung one (tokio-test trap 3).
            if wait.recv_timeout(Duration::from_secs(8)).is_ok() {
                release_fifo(&releaser);
                return;
            }
            for _ in 0..200 {
                release_fifo(&releaser);
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let started = std::time::Instant::now();
        let (status, body) = post(&f.state, ack()).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let run = loop {
            if let Some(r) = f.state.stall_probe.lock().clone() {
                if !r.is_running() && r.restore_outcome != "pending" {
                    break r;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(6),
                "the wedge was never bounded"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(
            run.abort_reason.as_deref(),
            Some(sp::ABORT_TACH_UNREADABLE),
            "{run:?}"
        );
        assert!(started.elapsed() >= crate::constants::STALL_PROBE_READ_BUDGET);
        assert!(
            pwm_duties(&f.writes).is_empty(),
            "{:?}",
            pwm_duties(&f.writes)
        );
        release_fifo(&tach);
        done.send(()).unwrap();
        release.join().unwrap();
    }

    #[tokio::test]
    async fn a_busy_slot_is_409_and_writes_nothing() {
        let f = fixture(HeaderRole::ChassisFan, true);
        let _held = crate::api::handlers::begin_verify_pause(
            &f.state.cache,
            crate::constants::VERIFY_PAUSE_DEADMAN,
        )
        .expect("the slot is free");
        let (status, body) = post(&f.state, ack()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(pwm_duties(&f.writes).is_empty());
    }

    #[tokio::test]
    async fn shutting_down_is_refused() {
        let f = fixture(HeaderRole::ChassisFan, true);
        f.shutdown.send(true).unwrap();
        let (status, _) = post(&f.state, ack()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(pwm_duties(&f.writes).is_empty());
    }

    #[tokio::test]
    async fn get_and_delete_with_no_run() {
        let f = fixture(HeaderRole::ChassisFan, true);
        let (status, Json(body)) = stall_probe_status_handler(State(f.state.clone())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
        let (status, _) = stall_probe_cancel_handler(State(f.state.clone())).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    /// DEC-340: the preflight through its REAL gatherer, never a hand-built
    /// input — the row is `pass` on an eligible header, `fail` and blocking on a
    /// header the active profile names a pump, and absent from every other
    /// diagnostic's report.
    #[tokio::test]
    async fn the_preflight_row_comes_from_the_same_rule_through_the_gatherer() {
        async fn preflight(state: &Arc<AppState>, diagnostic: &str) -> serde_json::Value {
            let params = [
                ("header".to_string(), HID.to_string()),
                ("diagnostic".to_string(), diagnostic.to_string()),
            ]
            .into_iter()
            .collect();
            let (status, Json(body)) = crate::api::handlers::discovery::preflight_handler(
                State(state.clone()),
                axum::extract::Query(params),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body
        }
        let row = |report: &serde_json::Value| {
            report["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["check_id"] == crate::api::preflight::CHECK_STALL_PROBE_ELIGIBLE)
                .cloned()
        };

        let ok = fixture(HeaderRole::ChassisFan, true);
        let report = preflight(&ok.state, "pwm_stall_probe").await;
        assert_eq!(row(&report).expect("the row")["state"], "pass", "{report}");
        assert_ne!(report["verdict"], "blocked", "{report}");
        assert!(row(&preflight(&ok.state, "pwm_characterization").await).is_none());

        // Sec F6: a header discovery cannot see gets no invented reason — the
        // row defers to `target_discoverable`, which blocks on its own.
        let unknown = {
            let params = [
                ("header".to_string(), "hwmon:test:dev:pwm9".to_string()),
                ("diagnostic".to_string(), "pwm_stall_probe".to_string()),
            ]
            .into_iter()
            .collect();
            let (_, Json(body)) = crate::api::handlers::discovery::preflight_handler(
                State(ok.state.clone()),
                axum::extract::Query(params),
            )
            .await;
            body
        };
        assert_eq!(
            row(&unknown).expect("the row")["state"],
            "not_applicable",
            "{unknown}"
        );
        assert_eq!(
            unknown["verdict"], "blocked",
            "target_discoverable still blocks"
        );

        let pump = fixture(HeaderRole::ChassisFan, true);
        *pump.state.active_profile.lock() = Some(pump_profile("Pump"));
        let report = preflight(&pump.state, "pwm_stall_probe").await;
        assert_eq!(row(&report).expect("the row")["state"], "fail", "{report}");
        assert_eq!(report["verdict"], "blocked");
        assert!(report["blocking"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b == crate::api::preflight::CHECK_STALL_PROBE_ELIGIBLE));
        // The POST refuses the same header for the same reason.
        let (status, body) = post(&pump.state, ack()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"]["details"]["reason"],
            sp::INELIGIBLE_PUMP_PROTECTED
        );
    }
}
