//! Handlers for diagnostic preflight and control-path discovery
//! (AIO Phase 8 Batch 1 §1, §2, §6.1, §6.2).
//!
//! Four routes:
//!
//! | Route | Shape |
//! | --- | --- |
//! | `GET /diagnostics/preflight` | read-only; never writes hardware |
//! | `POST /hwmon/{id}/discover-control-path` | 202 + snapshot, sweep runs detached |
//! | `GET /diagnostics/control-path` | last run, plus the persisted map |
//! | `DELETE /diagnostics/control-path` | cooperative cancel |
//!
//! The POST handler's entry sequence is deliberately the **same sequence, in the
//! same order**, as `hwmon_characterize_handler`: shutdown refusal → thermal
//! guard → **staleness refusal** → controller present → header known → claim the
//! single-flight slot → resolve the pump floor → force-take the Verify lease →
//! install the run → spawn. Every step of it is the existing shared function, so
//! this is a fourth consumer of one implementation rather than a fourth copy of
//! a sequence.
//!
//! The staleness refusal (DEC-336, `P8-p`) was the one step discovery did not
//! share; since DEC-385 verify and characterisation refuse on it too, through
//! the same `stale_temperature_guard`. It is keyed on
//! `Diagnostic::blocks_on_stale_temperature` rather than written out here, so
//! this handler cannot end up refusing on a rule different from the one the
//! preflight published.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::discovery as disc;
use crate::api::preflight as pf;
use crate::api::responses::ErrorEnvelope;
use crate::control_paths::{ControlPathRecord, ControlPathStore};
use crate::hwmon::lease::HwmonWriter;

// ── GET /diagnostics/preflight ───────────────────────────────────────

/// Read-only safety preflight for one header and one diagnostic.
///
/// §6.1: "Do not make the GUI responsible for enforcing safety; it reflects
/// daemon decisions." This is that endpoint — the daemon evaluates every check
/// and publishes the verdict, so a client renders rather than derives it.
///
/// **Writes nothing.** It reads sysfs (a `pwmN` / `pwmN_enable` / `fanN_input`
/// triple) and the state cache, and takes no lease and no slot. Calling it does
/// not reserve anything, so a `ready` verdict is a statement about *now*, and the
/// POST below still performs its own guards — the preflight informs the operator,
/// it does not authorise the run.
pub async fn preflight_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(header_id) = params.get("header").cloned() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("missing required query parameter: header"),
        );
    };
    let diagnostic_token = params
        .get("diagnostic")
        .cloned()
        .unwrap_or_else(|| pf::Diagnostic::ControlPathDiscovery.token().to_string());
    let Some(diagnostic) = pf::Diagnostic::from_token(&diagnostic_token) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!(
                "unknown diagnostic '{diagnostic_token}'; expected one of pwm_verify, \
                 pwm_characterization, control_path_discovery"
            )),
        );
    };

    let inputs = gather_preflight(&state, &header_id, diagnostic);
    json_ok(StatusCode::OK, pf::build_report(&inputs))
}

/// Collect everything the pure report builder needs.
///
/// Split out so the handler is a thin adapter and every *decision* stays in
/// `api::preflight`, where it is testable without hardware.
fn gather_preflight(
    state: &Arc<AppState>,
    header_id: &str,
    diagnostic: pf::Diagnostic,
) -> pf::PreflightInputs {
    let pump_protected = state.header_is_pump_protected(header_id);
    let role = state.resolved_header_role(header_id).as_str().to_string();

    // One controller lock, released before anything else is done with the result
    // — the same discipline `header_role_parts` documents for the ABBA hazard.
    let header_bits = state.hwmon_controller.as_ref().and_then(|c| {
        let ctrl = c.lock();
        let reverts = ctrl
            .enable_revert_counts()
            .get(header_id)
            .copied()
            .unwrap_or(0);
        ctrl.header(header_id).map(|h| {
            (
                h.is_writable,
                h.pwm_path.clone(),
                h.enable_path.clone(),
                h.rpm_path.clone(),
                reverts,
            )
        })
    });

    let (header_known, is_writable, live, enable_revert_count) = match header_bits {
        Some((writable, pwm, en, rpm, reverts)) => {
            let live = super::hwmon_ctl::read_header_state(&pwm, &en, &rpm);
            (true, writable, Some(live), reverts)
        }
        None => (false, false, None, 0),
    };

    let effective_floor_pct = if pump_protected {
        crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8
    } else {
        0
    };

    // [SAFETY] The staleness predicate, from the ONE producer of it
    // (`calibration::cache_temperature_freshness`). This was inlined here until
    // DEC-336; the write path needs the same rule, and a second inlined copy
    // there would have been two gating shapes for one safety rule.
    let temperature = crate::api::calibration::cache_temperature_freshness(&state.cache);

    let too_hot = match crate::api::calibration::check_thermal_safety(&state.cache) {
        Err(crate::api::calibration::CalibrationError::ThermalAbort {
            sensor_id,
            temp_c,
            limit_c,
        }) => Some((sensor_id, temp_c, limit_c)),
        _ => None,
    };

    pf::PreflightInputs {
        header_id: header_id.to_string(),
        diagnostic,
        header_known,
        is_writable,
        readback_pct: live.as_ref().and_then(|l| l.pwm_percent),
        pwm_enable: live.as_ref().and_then(|l| l.pwm_enable),
        role,
        pump_protected,
        effective_floor_pct,
        slot_busy: state.cache.verify_active(),
        enable_revert_count,
        temperature,
        thermal_forcing: crate::api::calibration::thermal_force_state(&state.cache),
        too_hot,
        supporting: supporting_cooling(state, header_id),
    }
}

/// [SAFETY-adjacent] Describe, but never touch, the cooling that keeps running
/// while this header is tested (Overview § "Supporting-device rule", scope Q13).
///
/// **Reads only.** The engine's write phase is paused for the diagnostic's
/// lifetime, so every sibling holds its last commanded duty; this reports whether
/// that held state is one an operator should be happy with. Nothing here commands
/// a duty, and the discovery sweep remains a single-header writer.
fn supporting_cooling(state: &Arc<AppState>, header_id: &str) -> pf::SupportingCooling {
    let devices = state.cooling_devices();
    let Some(device) = devices
        .iter()
        .find(|d| d.all_members().contains(&header_id))
    else {
        return pf::SupportingCooling::default();
    };
    let siblings: Vec<String> = device
        .all_members()
        .into_iter()
        .filter(|m| **m != *header_id)
        .map(|m| m.to_string())
        .collect();

    let snap = state.cache.snapshot();
    let mut running = 0usize;
    let mut unknown = 0usize;
    for member in &siblings {
        match observe_sibling(&snap, member) {
            pf::SiblingObservation::Running => running += 1,
            pf::SiblingObservation::Unknown => unknown += 1,
            // Read, and not moving. Counted as neither — the report distinguishes
            // "nothing is running" from "nothing could be read", and this is the
            // first of those.
            pf::SiblingObservation::Stopped => {}
        }
    }

    pf::SupportingCooling {
        applicable: true,
        device_id: Some(device.id.clone()),
        siblings: siblings.len(),
        siblings_running: running,
        siblings_unknown: unknown,
    }
}

/// The OpenFan channel a member id names, or `None` if it names anything else.
///
/// The id shape is `openfan:ch{NN}` and the cache is keyed by the bare channel,
/// so a member cannot be looked up without this transform — which is the whole
/// reason `supporting_cooling` used to miss every OpenFan sibling.
///
/// A thin local alias for [`crate::serial::openfan_channel_of`], which is the
/// single producer/parser pair for this id since `G36` closed `P8-bq`. Local
/// because this module wants the `Option` shape; the shared parser returns a
/// `Result` so the engine can log its two failure modes apart.
fn openfan_channel_of(member_id: &str) -> Option<u8> {
    crate::serial::openfan_channel_of(member_id).ok()
}

/// Observe one sibling member, in whichever cache holds its source.
///
/// **Every source, not just hwmon** (register row `P8-t`). This resolved members
/// through `hwmon_fans` alone, so on an AIO whose radiator fans hang off an
/// OpenFan controller — the project's canonical configuration — every sibling
/// fell through to "unknown" and the preflight reported "No sibling member's
/// state could be read" while `/poll` was concurrently publishing their RPM.
/// Stating an absence of evidence the daemon actually holds is worse than
/// saying nothing, because the operator reads it as a fault in the cooling.
///
/// Each arm passes the source's **measured** pair to [`pf::classify_sibling`],
/// never a commanded one — see that function's safety note.
fn observe_sibling(
    snap: &crate::health::state::DaemonState,
    member: &str,
) -> pf::SiblingObservation {
    if let Some(channel) = openfan_channel_of(member) {
        // `OpenFanState::rpm` is a plain `u16`, so "no reading" is not
        // representable in the value — `rpm_polled` is what carries it, and it is
        // load-bearing here: `force_all_with_floor` mints an entry for every
        // channel the firmware does not report, which would otherwise read as a
        // confident 0 RPM. The controller publishes no duty readback at all
        // (`last_commanded_pwm` is the daemon's own command), so there is no
        // second reading to fall back on.
        return match snap.openfan_fans.get(&channel) {
            Some(f) if f.rpm_polled => pf::classify_sibling(Some(f.rpm), None),
            _ => pf::SiblingObservation::Unknown,
        };
    }
    if let Some(f) = snap.hwmon_fans.get(member) {
        return pf::classify_sibling(f.rpm, f.pwm_readback_pct);
    }
    // Looked up by id rather than by prefix, so this covers every vendor's
    // `<vendor>_gpu:<bdf>` without a list of prefixes to keep in step. A GPU fan
    // is not a plausible AIO member — `cooling_device::unknown_member` rejects
    // one wherever hwmon has been discovered at all — but a member that IS
    // resolvable should never be reported unreadable, which is this row's rule
    // rather than a claim about GPUs.
    if let Some(g) = snap.gpu_fans.get(member) {
        return pf::classify_sibling(g.rpm, g.duty_pct);
    }
    pf::SiblingObservation::Unknown
}

/// Bound the observation set, keeping the **target header's own channel**.
///
/// [SAFETY] The bound itself is DEC-320: every observation is copied into the
/// run, into a session's `evidence[]`, and into every export, so the set has to
/// be finite. What this function adds is `P8-an` — *which* channels the bound
/// keeps.
///
/// Headers are enumerated before monitor-only fans, so on a board with more
/// tach-carrying channels than the cap, the target header's own channel can sit
/// past the cut. `run_discovery` derives `target_idx` from `is_target_header`
/// **after** this runs, so dropping it makes `had_target_tach` false — and
/// `pump_tach_lost`, the one abort separating "perturbing a healthy pump" from
/// "the pump has stopped", is gated on having had a tach at start and can then
/// never fire.
///
/// The target is swapped into the last retained slot rather than the list being
/// reordered: exactly one non-target channel is displaced — the one that was at
/// `keep - 1`, which the old bound WOULD have retained, so the swap does cost one
/// observation. That is the price of the trade and it is worth naming: one
/// arbitrary chassis tach for the one channel the diagnostic is actually about.
/// Every other retained channel keeps its position, and both vectors get the same
/// swap so the lockstep pairing holds — a channel without its path would read as
/// permanently unavailable rather than absent.
///
/// Extracted from the handler so the rule has a test that runs it; inline, the
/// only available check was a source scan (`CLAUDE.md`: extracting a rule does
/// not test the call site, so the call site is guarded too).
fn bound_tach_channels(channels: &mut Vec<disc::TachChannel>, tach_paths: &mut Vec<String>) {
    let keep = crate::constants::DISCOVERY_MAX_TACH_CHANNELS;
    if channels.len() <= keep {
        return;
    }
    log::warn!(
        "control-path discovery: {} tach channels found, observing {keep} \
         (the target header's own channel is kept when it has one)",
        channels.len()
    );
    if let Some(t) = channels.iter().position(|c| c.is_target_header) {
        if t >= keep {
            channels.swap(t, keep - 1);
            tach_paths.swap(t, keep - 1);
        }
    }
    channels.truncate(keep);
    tach_paths.truncate(keep);
}

// ── POST /hwmon/{header_id}/discover-control-path ────────────────────

/// Start a control-path discovery run. Returns **202** with the run snapshot;
/// the sweep runs detached and the client polls `GET /diagnostics/control-path`.
///
/// [SAFETY] The task is detached, so it is NOT in `main::shutdown_sequence`'s
/// `task_handles`. What makes that safe is the shutdown check inside the shared
/// `characterization::RestoreOnDrop` — read its docs before changing anything
/// here, and note that `run_discovery` additionally checks shutdown at the top of
/// every cycle and inside every observation window, because the guard covers only
/// the restore.
pub async fn discover_control_path_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(header_id): axum::extract::Path<String>,
    Json(body): Json<disc::DiscoveryRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // [SAFETY] Refuse once the daemon is going down (DEC-317). Same first guard,
    // same reason, as verify and characterise: a diagnostic that starts after
    // `hand_back_hwmon` has run would re-assert `pwm_enable=1` through
    // `set_pwm`'s reclaim watchdog and then skip its own restore, leaving the
    // header latched in manual with no daemon left to drive it.
    if *state.openfan_runtime.shutdown.borrow() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("the daemon is shutting down"),
        );
    }
    if let Some(resp) = super::verify_thermal_guard(&state.cache) {
        return resp;
    }
    // [SAFETY] DEC-336 (`P8-p`): perform the refusal the preflight publishes.
    // The two guards above compare `value_c` and have no view of how old it is,
    // so a wedged poll loop presents its last-known-good temperatures forever
    // and both of them pass. This is the third thermal gate and the only one
    // that can see that, and it is keyed on the SAME predicate the published
    // verdict is derived from, so the two cannot disagree.
    //
    // Placed before the controller lookup, with the other thermal guards, so it
    // is reached on every request rather than only on requests that get as far
    // as resolving hardware. `validation_error` + `retryable`, deliberately not
    // `thermal_abort`: the machine may be perfectly cool and the honest
    // statement is that the daemon cannot tell — the same shape, and the same
    // client-side "soft refusal" taxonomy, as the forcing branch of
    // `verify_thermal_guard`.
    if let Some(resp) = super::stale_temperature_guard(
        &state.cache,
        disc::DISCOVERY_DIAGNOSTIC,
        "control-path discovery",
    ) {
        return resp;
    }
    let Some(controller) = state.hwmon_controller.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    // Header paths, and every OTHER header's tach, under one controller lock.
    let (pwm_path, enable_path, rpm_path, is_writable, mut channels, mut tach_paths) = {
        let ctrl = controller.lock();
        let Some(target) = ctrl.header(&header_id) else {
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorEnvelope::validation(format!("unknown header: {header_id}")),
            );
        };
        let (pwm, en, rpm, writable) = (
            target.pwm_path.clone(),
            target.enable_path.clone(),
            target.rpm_path.clone(),
            target.is_writable,
        );
        let mut channels: Vec<disc::TachChannel> = Vec::new();
        let mut paths: Vec<String> = Vec::new();
        for h in ctrl.headers() {
            let Some(path) = h.rpm_path.as_ref() else {
                continue;
            };
            channels.push(disc::TachChannel {
                tach_id: h.id.clone(),
                label: h.label.clone(),
                monitor_only: false,
                is_target_header: h.id == header_id,
            });
            paths.push(path.clone());
        }
        (pwm, en, rpm, writable, channels, paths)
    };

    if !is_writable {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::feature_unavailable(format!(
                "header {header_id} is read-only; control-path discovery cannot run on it"
            )),
        );
    }

    // Monitor-only tachs (scope Q5). These are invisible to `/hwmon/headers` and
    // are NOT on the 1 Hz poll — deliberately, so the poll's cost stays flat.
    // They are read directly for the duration of this run and nowhere else, which
    // is what makes §2's "tach signal with no discovered controllable PWM"
    // outcome representable at all.
    match crate::hwmon::inventory::discover_monitor_only_fans(std::path::Path::new(
        crate::hwmon::HWMON_SYSFS_ROOT,
    )) {
        Ok(fans) => {
            for fan in fans {
                channels.push(disc::TachChannel {
                    tach_id: fan.id.clone(),
                    label: fan.label.clone(),
                    monitor_only: true,
                    is_target_header: false,
                });
                tach_paths.push(fan.input_path.display().to_string());
            }
        }
        Err(e) => {
            // Degrades to header-attached tachs only, exactly as the inventory
            // handler does. A missing sysfs root under a sandbox must not fail
            // the diagnostic.
            log::warn!("control-path discovery: monitor-only fan scan failed: {e}");
        }
    }

    bound_tach_channels(&mut channels, &mut tach_paths);

    // Claim the SAME single-flight slot verify, calibrate and characterise use,
    // so at most one of the four ever drives hardware.
    let Some(verify_guard) =
        super::begin_verify_pause(&state.cache, crate::constants::VERIFY_PAUSE_DEADMAN)
    else {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation("a hardware verify or calibration is already in progress"),
        );
    };

    // [SAFETY] The UNION predicate, never the wire `role` (DEC-312).
    let pump_protected = state.header_is_pump_protected(&header_id);
    let floor = if pump_protected {
        crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8
    } else {
        0
    };

    let live = super::hwmon_ctl::read_header_state(&pwm_path, &enable_path, &rpm_path);
    let delta = disc::resolve_delta(body.delta_pct);
    let cycles = disc::resolve_cycles(body.cycles);
    let window = crate::api::characterization::resolve_settle(body.window_seconds);
    let baseline = disc::resolve_baseline(live.pwm_percent, floor);
    let (perturbed, direction) = disc::perturbation_target(baseline, delta, floor);

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

    let run = disc::ControlPathRun {
        run_id: disc::next_run_id(),
        header_id: header_id.clone(),
        state: disc::STATE_RUNNING.to_string(),
        delta_pct: delta,
        requested_cycles: cycles,
        window_seconds: window.as_secs(),
        baseline_pct: baseline,
        perturbed_pct: perturbed,
        direction: direction.to_string(),
        channels: channels.clone(),
        cycles: vec![],
        summary: None,
        original_pct: live.pwm_percent,
        restore_failed: false,
        restore_outcome: crate::api::characterization::RestoreOutcome::Pending
            .token()
            .to_string(),
        detail: None,
        completed_unix_ms: None,
    };
    // Cancel flag cleared and run installed under ONE lock, and the cancel
    // handler takes the same lock across its check-and-set — without that
    // pairing a DELETE aimed at a finishing run could abort the run that
    // replaced it.
    {
        let mut slot_guard = state.control_path.lock();
        state.control_path_cancel.store(false, Ordering::SeqCst);
        *slot_guard = Some(run.clone());
    }

    let slot = state.control_path.clone();
    let my_run_id = run.run_id.clone();
    let cancel = state.control_path_cancel.clone();
    let cache = state.cache.clone();
    let ctrl_arc = controller.clone();
    let shutdown_rx = state.openfan_runtime.shutdown.clone();
    let hid = header_id.clone();
    let state_for_persist = state.clone();
    let driver_interval = read_update_interval(&pwm_path);

    tokio::spawn(async move {
        let report = crate::api::characterization::RestoreReport::new();

        // Guard drop order is load-bearing, and is the same order the
        // characterisation handler documents: `run_discovery` declares its own
        // `RestoreOnDrop` internally, so that guard drops when the sweep future
        // completes — i.e. BEFORE `pause` and `_lease` below, which is the only
        // order in which the restore write can still succeed.
        {
            let pause = verify_guard;
            let _lease = verify_lease;

            // [SAFETY] Renews BOTH the engine pause and the hwmon lease, once per
            // cycle, so each deadline measures liveness rather than total
            // duration. Renewing only the pause is the DEC-296 defect: nothing
            // else renews a Verify lease and `set_pwm` merely validates it, so a
            // long run would write fine until the 60 s TTL and then fail every
            // write — including the drop guard's restore.
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
            // Every sample is `DISCOVERY_MAX_TACH_CHANNELS + 3 = 35` blocking
            // `std::fs` reads, taken every 500 ms for the whole run (~186
            // samples), over a chip set drawn from a full `/sys/class/hwmon`
            // walk. `AIO3-d` accepted this shape on the characterisation sweep
            // at **three** reads per sample; discovery multiplies it ~11× and
            // widens the chip set, which is what makes it worth the dispatch
            // here (`P8-am`).
            //
            // On the blocking pool, per the DEC-290 precedent: a tach `open(2)`
            // wedged in a driver then parks a pool thread — sized for exactly
            // that — instead of parking a tokio worker uncancellably, where it
            // would starve unrelated tasks on the same worker. It does not make
            // a wedged read *fast*; `keepalive()` still cannot be reached with
            // data that never arrives. It stops one stuck header taking a slice
            // of the runtime down with it.
            //
            // `Arc`, so each sample's dispatch clones a pointer rather than four
            // strings, and so the closure stays `Fn` rather than `FnOnce`.
            let sample_paths = std::sync::Arc::new(SamplePaths {
                pwm: pwm_path.clone(),
                enable: enable_path.clone(),
                rpm: rpm_path.clone(),
                tachs: tach_paths.clone(),
            });
            let read_fn = move || sample_off_runtime(sample_paths.clone());
            // Fenced on `run_id`: a run whose deadman elapsed can be superseded
            // (`try_begin_verify` deliberately permits the steal), and without
            // the fence the loser would append its cycles into the winner's list
            // and then mark it terminal.
            let publish = |cycle: disc::DiscoveryCycle| {
                if let Some(r) = slot.lock().as_mut() {
                    if r.run_id == my_run_id && r.state == disc::STATE_RUNNING {
                        r.cycles.push(cycle);
                    }
                }
            };

            let outcome = disc::run_discovery(
                &cache,
                &hid,
                &channels,
                baseline,
                perturbed,
                direction,
                cycles,
                // [SAFETY] `AUD3-l`: the header's own floor, reused for the
                // RESTORE — 30% for a pump-protected header, 0 for everything
                // else, because putting an ordinary fan back at its captured 0
                // is a restore rather than a command.
                floor,
                pump_protected,
                window,
                write_fn,
                read_fn,
                &cancel,
                shutting_down,
                keepalive,
                &report,
                publish,
            )
            .await;

            let summary = disc::summarise(
                &channels,
                &outcome.cycles,
                driver_interval,
                outcome.observed_resolution_ms,
                outcome.sample_count,
            );
            let completed = crate::control_paths::unix_ms();

            // Terminal publish, INSIDE the guarded scope and fenced on `run_id`.
            // Inside, because the single-flight slot is released the moment this
            // block ends — a terminal write placed after it could legally land
            // on a run that had already started in the gap.
            let mut persist: Option<ControlPathRecord> = None;
            if let Some(r) = slot.lock().as_mut() {
                if r.run_id == my_run_id {
                    r.cycles = outcome.cycles;
                    r.state = outcome.state.to_string();
                    r.detail = outcome.detail;
                    r.completed_unix_ms = Some(completed);
                    // ONE source of truth for both fields (`AUD2-c`).
                    let restore = report.get();
                    r.restore_failed = restore.header_left_moved();
                    r.restore_outcome = restore.token().to_string();
                    // Only a run that actually finished its cycles describes the
                    // hardware. A cancelled or aborted run measured a partial
                    // window, and recording it as "last validated" would be the
                    // §5 error of turning absent evidence into a result.
                    if outcome.state == disc::STATE_COMPLETE {
                        persist = Some(record_for(r, &summary, completed));
                    }
                    r.summary = Some(summary);
                }
            }
            if let Some(record) = persist {
                persist_record(&state_for_persist, record).await;
            }
        };
    });

    json_ok(StatusCode::ACCEPTED, run)
}

/// Build the durable record from a finished run.
fn record_for(
    run: &disc::ControlPathRun,
    summary: &disc::DiscoverySummary,
    completed_unix_ms: u64,
) -> ControlPathRecord {
    let best = summary.candidates.first();
    ControlPathRecord {
        header_id: run.header_id.clone(),
        relationship: summary.relationship.clone(),
        confidence: summary.confidence.clone(),
        tach_ids: summary
            .candidates
            .iter()
            .map(|c| c.tach_id.clone())
            .collect(),
        tach_labels: summary.candidates.iter().map(|c| c.label.clone()).collect(),
        direction: best.map(|c| c.direction.clone()).unwrap_or_default(),
        baseline_rpm: best.and_then(|c| c.baseline_rpm),
        perturbed_rpm: best.and_then(|c| c.perturbed_rpm),
        change_pct: best.and_then(|c| c.change_pct),
        run_id: run.run_id.clone(),
        validated_unix_ms: completed_unix_ms,
    }
}

/// Merge one record into the store and write it out.
///
/// The write goes through `persist_off_runtime` for the same reason every other
/// persistence call does (DEC-252): `write_atomic` fsyncs, and that is unbounded
/// wall-clock time on a tokio worker the 1 Hz engine also runs on.
async fn persist_record(state: &Arc<AppState>, record: ControlPathRecord) {
    let to_write = {
        let guard = state.control_paths.read();
        let mut store = (**guard).clone();
        store.upsert(record.clone());
        store
    };
    let result = super::persist_off_runtime(move || {
        crate::control_paths::save_to(&control_paths_dir(), &to_write)
    })
    .await;
    match result {
        Ok(()) => {
            // Re-read and re-apply UNDER the write lock rather than committing a
            // snapshot computed before the fsync.
            //
            // The `await` above is a suspension point. The single-flight slot
            // normally makes two persists impossible, but the DEC-296 deadman
            // steal means a wedged run CAN be superseded — at which point both
            // runs hold a pre-fsync clone and the later commit would silently
            // drop the earlier record. Re-upserting costs the same and has no
            // such window.
            let mut guard = state.control_paths.write();
            let mut store = (**guard).clone();
            store.upsert(record);
            *guard = Arc::new(store);
        }
        Err(e) => {
            // Persist-first, commit-second: a failed write leaves the in-memory
            // map exactly as it was, so the two can never disagree about what is
            // on disk. The run itself already succeeded and is reported; only the
            // durable "last validated" row is lost.
            log::warn!("could not persist the control-path store: {e}");
        }
    }
}

/// State directory holding the control-path store.
fn control_paths_dir() -> std::path::PathBuf {
    crate::daemon_state::state_dir_path()
}

/// The four sysfs paths one discovery observation reads.
///
/// A struct rather than four separately captured strings because the sample is
/// dispatched to the blocking pool (`P8-am`), so it must OWN what it reads. It
/// is shared behind an `Arc`, which is what keeps the per-sample cost a pointer
/// clone instead of four string allocations, ~186 times a run.
struct SamplePaths {
    pwm: String,
    enable: Option<String>,
    rpm: Option<String>,
    tachs: Vec<String>,
}

/// One observation, taken on the blocking pool.
///
/// A named function rather than an inline closure so this dispatch has a call
/// site a test can actually reach: DEC-324's rule is that an extracted rule
/// nothing asserts about the production path is an untested rule, and "the
/// sample runs off the runtime" is precisely such a rule. Its only caller is the
/// `read_fn` handed to `run_discovery` below.
async fn sample_off_runtime(paths: std::sync::Arc<SamplePaths>) -> disc::DiscoverySample {
    let channels = paths.tachs.len();
    sample_or_unreadable(
        tokio::task::spawn_blocking(move || paths.sample()).await,
        channels,
    )
}

/// Map a finished sample task onto a sample, or onto absence.
///
/// Split out from [`sample_off_runtime`] so the failure arm has a call site a
/// test can reach: `SamplePaths::sample` contains no panicking path, so the arm
/// is otherwise unreachable from any test and would ship unexercised — DEC-324's
/// rule, raised against the first draft by `ofc:concurrency-reviewer`.
fn sample_or_unreadable(
    joined: Result<disc::DiscoverySample, tokio::task::JoinError>,
    channels: usize,
) -> disc::DiscoverySample {
    joined.unwrap_or_else(|e| {
        // The join failed, so the read never completed — the closure panicked, or
        // the runtime is going down. Reported as unreadable rather than as a
        // value: that is the same shape an unreadable chip already produces, and
        // `observe`'s next `shutting_down()` check is what turns a teardown into
        // an abort.
        //
        // **One consumer does read it differently, and it is worth naming.** On
        // the PRE-RUN sample — `run_discovery`'s `first` — an unreadable header
        // makes `original_pct` `None`, so `RestoreOnDrop` takes its
        // `NoOriginalDuty` branch and leaves the header alone instead of
        // restoring it. That needs this arm to fire on the very first sample,
        // before any duty has been written: the teardown case bails at the
        // `shutting_down()` check before `wrote_any` is set, and leaving an
        // as-yet-unperturbed header alone is the right answer anyway. Narrow,
        // not absent — the first draft of this comment claimed no consumer
        // needed a new branch, which was too broad.
        log::warn!(
            "control-path discovery: sample task failed ({e}); \
             recording this sample as unreadable"
        );
        SamplePaths::unreadable(channels)
    })
}

impl SamplePaths {
    /// One observation.
    ///
    /// **Blocking**, deliberately: `DISCOVERY_MAX_TACH_CHANNELS + 3` `std::fs`
    /// reads. Never call this on the async runtime — that is the whole point of
    /// the `spawn_blocking` at its only call site.
    fn sample(&self) -> disc::DiscoverySample {
        disc::DiscoverySample {
            header: super::hwmon_ctl::read_header_state(&self.pwm, &self.enable, &self.rpm),
            tachs: self
                .tachs
                .iter()
                .map(|p| {
                    std::fs::read_to_string(p)
                        .ok()
                        .and_then(|s| s.trim().parse::<u16>().ok())
                })
                .collect(),
        }
    }

    /// What an observation that could not run at all reports: every field
    /// unreadable.
    ///
    /// Not a new state — it is exactly the sample an unreadable chip already
    /// produces, so no consumer needs a new branch (with one narrow exception,
    /// named at [`sample_or_unreadable`]).
    ///
    /// `tachs` is sized to the channel count for legibility, **not** because
    /// anything depends on it: every reader in `api/discovery.rs` goes through
    /// `tachs.get(i)`, so a short vector is already indistinguishable from one
    /// full of `None`. Stated explicitly because the first draft claimed the
    /// count was load-bearing — which would have invited a test asserting a
    /// property no consumer can observe, i.e. one that passes by construction.
    fn unreadable(channels: usize) -> disc::DiscoverySample {
        disc::DiscoverySample {
            header: crate::api::responses::HwmonVerifyState {
                pwm_enable: None,
                pwm_raw: None,
                pwm_percent: None,
                rpm: None,
            },
            tachs: vec![None; channels],
        }
    }
}

/// The driver's declared telemetry cadence, in ms, if it publishes one (§4).
///
/// hwmon's `update_interval` is a chip-level attribute beside the `pwmN` files.
/// Absent on most Super-I/O drivers, which is exactly why §4 requires UNKNOWN
/// rather than a guess — this returns `None` and the summary falls back to what
/// the run actually observed.
fn read_update_interval(pwm_path: &str) -> Option<u64> {
    let dir = std::path::Path::new(pwm_path).parent()?;
    std::fs::read_to_string(dir.join("update_interval"))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

// ── GET / DELETE /diagnostics/control-path ───────────────────────────

/// The current or most recent run, plus every persisted relationship.
#[derive(serde::Serialize)]
struct ControlPathResponse {
    api_version: u32,
    run: Option<disc::ControlPathRun>,
    /// The durable map (§6.3). Present even when no run has happened this boot,
    /// which is the whole reason it is persisted.
    records: Vec<ControlPathRecord>,
}

pub async fn control_path_status_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let run = state.control_path.lock().clone();
    let records: Vec<ControlPathRecord> = state
        .control_paths
        .read()
        .records
        .values()
        .cloned()
        .collect();
    if run.is_none() && records.is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no control-path discovery has run"),
        );
    }
    json_ok(
        StatusCode::OK,
        ControlPathResponse {
            api_version: crate::api::responses::API_VERSION,
            run,
            records,
        },
    )
}

/// Cancel a running discovery. Cooperative: the sweep checks the flag at the top
/// of every cycle, restores through its drop guard, and reports `cancelled`.
pub async fn control_path_cancel_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Check-and-set under ONE lock, paired with the installer above.
    let snapshot = {
        let slot = state.control_path.lock();
        match slot.as_ref() {
            Some(run) if run.is_running() => {
                state.control_path_cancel.store(true, Ordering::SeqCst);
                run.clone()
            }
            _ => {
                return error_response(
                    StatusCode::CONFLICT,
                    &ErrorEnvelope::validation("no control-path discovery is running"),
                )
            }
        }
    };
    json_ok(StatusCode::ACCEPTED, snapshot)
}

/// Drop persisted records whose header is no longer discoverable (§6.3), at boot.
///
/// Returns the pruned store. Called from `main` once discovery has run, so a
/// board or driver change invalidates stale mappings before anything reads them.
pub fn prune_store_to_live(
    store: &ControlPathStore,
    live_header_ids: &[String],
) -> ControlPathStore {
    let mut next = store.clone();
    let dropped = next.prune_to_live(live_header_ids);
    if dropped > 0 {
        log::info!(
            "control-path store: dropped {dropped} record(s) whose header is no longer present"
        );
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A FIFO nothing writes to for `delay`, so a reader blocks in `open(2)`.
    ///
    /// A genuine kernel-level block, not a `sleep`: `CLAUDE.md` records that
    /// DEC-278's three tests all passed while modelling a wedge the way their
    /// author imagined it rather than the way it happens. A tach `open(2)` stuck
    /// in a driver blocks in the kernel, and so does this. The writer arriving on
    /// a timer is what stops a failure becoming a hung CI job.
    fn wedged_fifo(dir: &std::path::Path, delay: Duration) -> String {
        let path = dir.join("pwm1");
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
        let releaser = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            // Opening the write end releases the reader from `open`; dropping it
            // immediately gives it EOF, so the read returns an empty string.
            let _ = std::fs::OpenOptions::new().write(true).open(&releaser);
        });
        path.display().to_string()
    }

    /// [P8-am] The sample runs on the blocking pool, so a wedged read cannot
    /// starve the runtime.
    ///
    /// Each sample is up to `DISCOVERY_MAX_TACH_CHANNELS + 3` blocking `std::fs`
    /// reads, taken every 500 ms for the whole run. Issued inline from the
    /// `tokio::spawn`ed sweep they park a worker uncancellably; the register's
    /// consequence is that `keepalive()` is never reached, the run is superseded,
    /// and its restore fails `InvalidLease` with the header left perturbed.
    ///
    /// One worker thread is what makes the difference observable rather than
    /// probabilistic: an inline blocking read has nowhere else to put the other
    /// tasks. The counter is read on the `block_on` thread — which is not a
    /// worker — **while the read is still wedged**, so the reading cannot be
    /// contaminated by the tasks that run once it completes.
    #[test]
    fn the_sample_runs_off_the_runtime_so_a_wedged_read_cannot_starve_it() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let paths = Arc::new(SamplePaths {
            pwm: wedged_fifo(tmp.path(), Duration::from_millis(600)),
            enable: None,
            rpm: None,
            tachs: vec![],
        });

        rt.block_on(async {
            let progressed = Arc::new(AtomicUsize::new(0));
            let counter = progressed.clone();

            let sample = tokio::spawn(sample_off_runtime(paths));
            let ticker = tokio::spawn(async move {
                loop {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            });

            // Mid-wedge, on the caller thread.
            std::thread::sleep(Duration::from_millis(200));
            let during = progressed.load(Ordering::SeqCst);

            let out = sample.await.expect("the sample task");
            ticker.abort();

            // Not a tuned threshold. Under the defect the ticker can complete AT
            // MOST one iteration — it is spawned second, and its second poll
            // needs the very worker the inline read is sitting on — so 2 is the
            // structural boundary between "the sample yielded the worker" and
            // "it did not". With the fix this is in the hundreds.
            assert!(
                during >= 2,
                "the runtime advanced {during} times while one sample was wedged; \
                 the sample is running on a worker instead of the blocking pool \
                 (`P8-am`)"
            );
            // The wedge really was a wedge: an unreadable header, not a value.
            assert!(
                out.header.pwm_percent.is_none(),
                "the fixture returned a duty, so the read was not wedged and the \
                 progress count above proves nothing"
            );
        });
    }

    /// [P8-am] A sample task that FAILED is reported as absence, not as a value.
    ///
    /// The arm is unreachable from production input — `SamplePaths::sample` has
    /// no panicking path — so it is exercised through `sample_or_unreadable`
    /// with a **real** `JoinError`, obtained by aborting a task rather than
    /// hand-rolling one. An aborted join is the shutdown case the arm names, and
    /// it keeps the test output free of a panic backtrace.
    ///
    /// Both arms, deliberately: with only the `Err` case a `sample_or_unreadable`
    /// that ignored its argument and always returned `unreadable` would pass.
    #[test]
    fn a_failed_sample_task_is_reported_as_unreadable_not_as_a_value() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            let h = tokio::spawn(std::future::pending::<disc::DiscoverySample>());
            h.abort();
            h.await.expect_err("an aborted task must yield a JoinError")
        });

        let failed = sample_or_unreadable(Err(err), 4);
        assert!(
            failed.header.pwm_percent.is_none() && failed.header.rpm.is_none(),
            "a failed join must report absence, never a duty or an RPM"
        );
        assert_eq!(failed.tachs.len(), 4);
        assert!(failed.tachs.iter().all(Option::is_none));

        // The success arm passes the reading through untouched.
        let read = sample_or_unreadable(
            Ok(disc::DiscoverySample {
                header: crate::api::responses::HwmonVerifyState {
                    pwm_enable: Some(1),
                    pwm_raw: Some(128),
                    pwm_percent: Some(50),
                    rpm: Some(900),
                },
                tachs: vec![Some(1200)],
            }),
            4,
        );
        assert_eq!(read.header.pwm_percent, Some(50));
        assert_eq!(read.tachs, vec![Some(1200)]);
    }

    /// [P8-am] `unreadable` is sized from the channel count.
    ///
    /// A legibility property, **not** a behavioural one: every consumer in
    /// `api/discovery.rs` reads through `tachs.get(i)`, so a short vector is
    /// indistinguishable from one full of `None`. Asserted so the constructor
    /// keeps saying what it means, and labelled so nobody later mistakes it for
    /// a regression guard over something a consumer can observe.
    #[test]
    fn an_unreadable_sample_is_sized_from_the_channel_count() {
        let s = SamplePaths::unreadable(7);
        assert_eq!(s.tachs.len(), 7);
        assert!(s.tachs.iter().all(Option::is_none));
        assert!(s.header.pwm_percent.is_none());
        assert!(s.header.rpm.is_none());
    }

    // ── `P8-an`: the target header's channel survives the bound ──

    fn ch(i: usize, target: bool) -> disc::TachChannel {
        disc::TachChannel {
            tach_id: format!("hwmon:c:d:fan{i}:L{i}"),
            label: format!("L{i}"),
            monitor_only: !target,
            is_target_header: target,
        }
    }

    /// Build `n` channels with the target at `target_at`, plus lockstep paths.
    fn channels_with_target(n: usize, target_at: usize) -> (Vec<disc::TachChannel>, Vec<String>) {
        let chans: Vec<_> = (0..n).map(|i| ch(i, i == target_at)).collect();
        let paths: Vec<_> = (0..n).map(|i| format!("/p/fan{i}")).collect();
        (chans, paths)
    }

    #[test]
    fn the_target_channel_survives_a_bound_that_would_have_dropped_it() {
        // THE discriminating arm (DEC-340): a test that only asserted "no target
        // present" would be the PRE-FIX answer by construction. The observation
        // that the old code cannot produce is the target being *found* after a
        // truncation that put it past the cut.
        let keep = crate::constants::DISCOVERY_MAX_TACH_CHANNELS;
        let over = keep + 5;
        let target_at = over - 1; // last — well past the cut
        let (mut chans, mut paths) = channels_with_target(over, target_at);

        // Precondition: the bound must actually engage, or this asserts nothing.
        assert!(chans.len() > keep, "fixture must exceed the cap");
        assert!(target_at >= keep, "target must start past the cut");

        bound_tach_channels(&mut chans, &mut paths);

        assert_eq!(chans.len(), keep, "the bound must still bind");
        assert_eq!(paths.len(), keep, "lockstep: paths bound with channels");
        let idx = chans
            .iter()
            .position(|c| c.is_target_header)
            .expect("the target header's channel must survive the bound");
        // ...and its PATH must be the target's path, not the displaced one's —
        // a swap applied to one vector and not the other would pass the line
        // above and silently read the wrong sysfs file.
        assert_eq!(
            paths[idx],
            format!("/p/fan{target_at}"),
            "the swap must be applied in lockstep to both vectors"
        );
        assert_eq!(
            chans[idx].tach_id,
            format!("hwmon:c:d:fan{target_at}:L{target_at}")
        );
    }

    #[test]
    fn a_target_already_inside_the_bound_is_not_moved() {
        // The complement: without it, a rule that unconditionally swapped would
        // pass the test above while scrambling every ordinary board.
        let keep = crate::constants::DISCOVERY_MAX_TACH_CHANNELS;
        let (mut chans, mut paths) = channels_with_target(keep + 3, 2);
        bound_tach_channels(&mut chans, &mut paths);

        assert_eq!(chans.len(), keep);
        assert!(
            chans[2].is_target_header,
            "an in-bounds target must not move"
        );
        assert_eq!(paths[2], "/p/fan2");
        // Every other retained channel keeps its position too.
        assert_eq!(chans[0].tach_id, "hwmon:c:d:fan0:L0");
        assert_eq!(paths[keep - 2], format!("/p/fan{}", keep - 2));
    }

    #[test]
    fn a_set_within_the_bound_is_left_alone_entirely() {
        let keep = crate::constants::DISCOVERY_MAX_TACH_CHANNELS;
        let (mut chans, mut paths) = channels_with_target(keep, keep - 1);
        let before = chans.clone();
        bound_tach_channels(&mut chans, &mut paths);
        assert_eq!(chans.len(), before.len());
        assert!(chans[keep - 1].is_target_header);
    }

    #[test]
    fn a_bound_with_no_target_channel_still_binds() {
        // A header with no tach of its own: nothing to preserve, but the bound
        // must still apply or DEC-320's reservation is breached.
        let keep = crate::constants::DISCOVERY_MAX_TACH_CHANNELS;
        let n = keep + 4;
        let mut chans: Vec<_> = (0..n).map(|i| ch(i, false)).collect();
        let mut paths: Vec<_> = (0..n).map(|i| format!("/p/fan{i}")).collect();
        bound_tach_channels(&mut chans, &mut paths);
        assert_eq!(chans.len(), keep);
        assert_eq!(paths.len(), keep);
        assert!(!chans.iter().any(|c| c.is_target_header));
    }
}
