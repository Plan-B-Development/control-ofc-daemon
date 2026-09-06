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
//! guard → controller present → header known → claim the single-flight slot →
//! resolve the pump floor → force-take the Verify lease → install the run →
//! spawn. Every step of it is the existing shared function, so this is a fourth
//! consumer of one implementation rather than a fourth copy of a sequence.

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

    // [SAFETY] The new staleness predicate. Ages are taken from the same
    // `Instant`, once, so two readings cannot be compared against two different
    // "now"s.
    let now = std::time::Instant::now();
    let snap = state.cache.snapshot();
    let readings: Vec<(String, std::time::Instant)> = snap
        .sensors
        .values()
        .filter(|s| matches!(s.kind, crate::hwmon::types::SensorKind::CpuTemp))
        .map(|s| (s.id.clone(), s.updated_at))
        .collect();
    // A machine with no CPU sensor at all falls back to every temperature it has,
    // rather than reporting "no temperature sensors" while a dozen are readable.
    let readings = if readings.is_empty() {
        snap.sensors
            .values()
            .map(|s| (s.id.clone(), s.updated_at))
            .collect()
    } else {
        readings
    };
    let temperature =
        pf::temperature_freshness(&readings, crate::constants::DIAGNOSTIC_TEMP_MAX_AGE, now);

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
        match snap.hwmon_fans.get(member) {
            // "Observably moving": a non-zero tach, or a non-zero readback where
            // the header reports no tach at all. A zero readback with no tach is
            // NOT counted as running — that is the case the operator needs told.
            Some(f) => {
                let moving = f.rpm.is_some_and(|r| r > 0)
                    || (f.rpm.is_none() && f.pwm_readback_pct.is_some_and(|p| p > 0));
                if moving {
                    running += 1;
                } else if f.rpm.is_none() && f.pwm_readback_pct.is_none() {
                    unknown += 1;
                }
            }
            None => unknown += 1,
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
    // `restore_hwmon_to_auto` has run would re-assert `pwm_enable=1` through
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

    // [SAFETY] Bound the observation set (DEC-320): every observation is copied
    // into the run, into a session's `evidence[]`, and into every export.
    if channels.len() > crate::constants::DISCOVERY_MAX_TACH_CHANNELS {
        log::warn!(
            "control-path discovery: {} tach channels found, observing the first {}",
            channels.len(),
            crate::constants::DISCOVERY_MAX_TACH_CHANNELS
        );
        // Truncate in lockstep — a channel without its path would read as
        // permanently unavailable rather than being absent.
        channels.truncate(crate::constants::DISCOVERY_MAX_TACH_CHANNELS);
        tach_paths.truncate(crate::constants::DISCOVERY_MAX_TACH_CHANNELS);
    }

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
            // Same known limitation as the characterisation sweep (`AIO3-d`):
            // these are blocking `std::fs` reads issued from the async runtime.
            // The channel count makes this run's per-sample cost higher than
            // characterisation's, which is exactly why
            // `DISCOVERY_MAX_TACH_CHANNELS` bounds it.
            let read_fn = || disc::DiscoverySample {
                header: super::hwmon_ctl::read_header_state(&pwm_path, &enable_path, &rpm_path),
                tachs: tach_paths
                    .iter()
                    .map(|p| {
                        std::fs::read_to_string(p)
                            .ok()
                            .and_then(|s| s.trim().parse::<u16>().ok())
                    })
                    .collect(),
            };
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
