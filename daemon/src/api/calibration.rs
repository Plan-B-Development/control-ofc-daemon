//! [SAFETY] OpenFan calibration: where a fan stops as its duty falls, where it
//! starts again as the duty rises, and its RPM across the range (DEC-452,
//! `W-OFAN`, register rows `WIRE-l`, `PTR-i`, `TS-bh`).
//!
//! # The walk
//!
//! - **Descent** from 100 % in [`constants::OPENFAN_CAL_COARSE_STEP_PCT`] steps
//!   to [`constants::OPENFAN_CAL_FINE_BELOW_PCT`], then in
//!   [`constants::OPENFAN_CAL_FINE_STEP_PCT`] steps towards 0 %, until the fan
//!   is confirmed stopped: the **stall duty**. A fan that stops at 100 % is
//!   `no_fan_detected`; one still spinning at 0 % is `no_stall_down_to_0`.
//! - **Ascent** from the stall duty in fine steps, up to
//!   [`constants::OPENFAN_CAL_FINE_BELOW_PCT`] (or one coarse step above a stall
//!   found in the coarse band), until the fan is confirmed spinning: the
//!   **restart duty**. A fan that has not restarted by then gets the recovery
//!   kick and the outcome `did_not_restart`.
//!
//! Until DEC-452 the sweep only climbed from 0 %, so its `stop_pwm` was always
//! the step below `start_pwm` and measured nothing (`WIRE-l`). A stop needs a
//! descent, which is lm-sensors `fancontrol`'s MINSTOP as against MINSTART.
//!
//! # Every hold is gated on every sample
//!
//! Each step holds for `hold_seconds` and samples the cached RPM every
//! [`constants::OPENFAN_CAL_SAMPLE_INTERVAL`]. Before every write and on every
//! sample the run checks, in order: shutdown, cancel, the 85 °C limit, the
//! ladder forcing, stale temperatures, the hottest fresh CPU reading rising
//! [`constants::STALL_PROBE_RISE_LIMIT_C`] above its start (the stall probe's
//! gate, DEC-407), and the engine-pause keepalive (DEC-296). So a cancel is
//! honoured within one sample rather than at the end of a hold — during the
//! descent and ascent. The recovery kick and the restore are not cancellable.
//!
//! A hold's **verdict** needs [`constants::OPENFAN_CAL_CONFIRM_SAMPLES`] fresh
//! samples, all zero (stopped) or all non-zero (spinning); fewer, or a mix, is
//! `unconfirmed`, and the walk moves on as if nothing was confirmed.
//!
//! # Pumps (`PTR-i`)
//!
//! The daemon holds **no pump evidence for an OpenFan channel** — every pump
//! predicate is hwmon-only — so this walk cannot refuse a pump the way the stall
//! probe does. Instead the request must carry `acknowledge_below_floor: true`,
//! which the GUI sends only after the user confirms the channel does not power a
//! pump (DEC-452, the user's Q3-A).
//!
//! # Ending
//!
//! An abort or cancel that may have left the fan stopped ends with a 100 %
//! **recovery kick** before the restore, because a restore to a duty between
//! the stall and restart duties would leave it stopped. The kick is owed when
//! the fan was not last confirmed spinning — or, with no verdict at all (a
//! failed write, no fresh reading), when the walk had reached
//! [`constants::OPENFAN_CAL_FINE_BELOW_PCT`] or found a stall. It is not
//! gated: it runs after a cancel or an abort, and stops only on shutdown. No
//! kick while thermal safety is forcing (the force already holds 100 %).
//!
//! The **restore** writes the pre-calibration duty, read from the controller
//! under its lock after the engine pause was claimed (`TS-bh`), or full speed
//! where that duty is unknown (DEC-412). Where a kick was owed and could not run
//! to its end — the daemon is shutting down — it writes **100 %** instead
//! (`restored_full_speed`): the exit floor that follows is not a restart
//! either. It is skipped only while thermal safety is forcing (DEC-295). It
//! still runs while shutting down: an OpenFan channel has no mode to hand back,
//! and once the exit floor has run it latches `max(last, floor)` as the
//! channel's minimum (DEC-388), so whichever of the two lands last the channel
//! ends at or above the floor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::characterization::{
    RestoreOutcome, STATE_ABORTED, STATE_CANCELLED, STATE_COMPLETE, STATE_RUNNING,
};
use crate::api::diagnostic_gates::ThermalRefusal;
use crate::api::preflight::TemperatureFreshness;
use crate::constants;
use crate::health::cache::StateCache;

// ── Vocabulary ───────────────────────────────────────────────────────
// Stable tokens. The client owns the wording and must render an unrecognised
// token rather than dropping it (the 273-i rule).

/// Stopped on the way down and started again on the way up.
pub const OUTCOME_STALL_AND_RESTART_FOUND: &str = "stall_and_restart_found";
/// Still spinning at 0 %.
pub const OUTCOME_NO_STALL_DOWN_TO_0: &str = "no_stall_down_to_0";
/// Stopped, and still stopped at the top of the ascent; the kick followed.
pub const OUTCOME_DID_NOT_RESTART: &str = "did_not_restart";
/// 0 rpm at 100 %: nothing on the channel, or no tach wire.
pub const OUTCOME_NO_FAN_DETECTED: &str = "no_fan_detected";
/// The run stopped early; `abort_reason` says why.
pub const OUTCOME_ABORTED: &str = "aborted";
/// A `DELETE /diagnostics/openfan-calibration` stopped the run.
pub const OUTCOME_CANCELLED: &str = "cancelled";

/// `abort_reason` tokens. The three thermal ones are [`ThermalRefusal::token`]'s
/// (`thermal_limit` | `thermal_force` | `stale_temperature`).
pub const ABORT_SHUTTING_DOWN: &str = "shutting_down";
pub const ABORT_SUPERSEDED: &str = "superseded";
pub const ABORT_THERMAL_RISE: &str = "thermal_rise";
pub const ABORT_NO_CPU_TEMPERATURE: &str = "no_cpu_temperature";
pub const ABORT_WRITE_FAILED: &str = "write_failed";
pub const ABORT_RPM_UNREADABLE: &str = "rpm_unreadable";
/// The channel was assigned the `pump` role while the run was walking it
/// (`ROLE-f`). The walk stops within one sample, and the restore holds the
/// pump floor.
pub const ABORT_PUMP_PROTECTED: &str = "pump_protected";
/// The calibration task ended without a result (a panic). Its
/// `restore_outcome` stays `pending`: the walk's backstop attempted the
/// restore, and nothing recorded how that went.
pub const ABORT_TASK_FAILED: &str = "task_failed";

/// `CalPoint.phase` and `OpenFanCalibrationRun.phase`.
pub const PHASE_DESCENT: &str = "descent";
pub const PHASE_ASCENT: &str = "ascent";
pub const PHASE_KICK: &str = "kick";
pub const PHASE_RESTORE: &str = "restore";

/// `CalPoint.observation`.
pub const OBS_SPINNING: &str = "spinning";
pub const OBS_STOPPED: &str = "stopped";
/// The confirming samples disagreed; the walk moves on as if not confirmed.
pub const OBS_UNCONFIRMED: &str = "unconfirmed";
/// The hold was cut short by an abort or a cancel.
pub const OBS_INTERRUPTED: &str = "interrupted";

/// `restore_outcome` when the run never wrote the channel, so there was
/// nothing to put back. The other tokens are [`RestoreOutcome::token`]'s.
pub const RESTORE_NOT_NEEDED: &str = "not_needed";
/// `restore_outcome` when a recovery kick was owed and could not run — the
/// daemon was shutting down — so the channel was left at 100 % rather than at
/// an original duty that may not restart a stopped fan. `restore_failed` is
/// true: the channel is not at its original duty.
pub const RESTORE_FULL_SPEED: &str = "restored_full_speed";

// ── Wire types ───────────────────────────────────────────────────────

/// One held duty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CalPoint {
    pub pwm_percent: u8,
    /// The last fresh reading of the hold.
    pub rpm: u16,
    /// `descent` | `ascent` | `kick`. Added by DEC-452; the deprecated sync
    /// route carries it too.
    #[serde(default)]
    pub phase: String,
    /// `spinning` | `stopped` | `unconfirmed` | `interrupted`.
    #[serde(default)]
    pub observation: String,
}

/// Result of the deprecated synchronous route (`POST .../calibrate`).
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationResult {
    pub fan_id: String,
    pub points: Vec<CalPoint>,
    pub start_pwm: Option<u8>,
    pub stop_pwm: Option<u8>,
    pub min_rpm: u16,
    pub max_rpm: u16,
}

/// Body of `POST /fans/openfan/{channel}/calibration` (DEC-452).
///
/// [SAFETY] `acknowledge_below_floor: true` is required: the walk goes down to
/// 0 % and the daemon cannot tell whether the channel powers a pump (`PTR-i`).
/// Unknown fields are rejected, so a crafted request cannot name a duty or a
/// step.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenFanCalibrationRequest {
    pub acknowledge_below_floor: Option<bool>,
    /// Seconds each step is held; clamped to
    /// [`constants::OPENFAN_CAL_MIN_HOLD_S`]..=[`constants::OPENFAN_CAL_MAX_HOLD_S`].
    pub hold_seconds: Option<u64>,
}

/// Body of the deprecated `POST /fans/openfan/{channel}/calibrate`.
///
/// `steps` is still accepted so an existing client's body parses, and is
/// ignored: the walk's steps are fixed (DEC-452). Unlike the new route this one
/// does not reject unknown fields, as it never did.
#[derive(Debug, Default, Deserialize)]
pub struct CalibrationRequest {
    #[serde(default)]
    pub steps: Option<u8>,
    #[serde(default)]
    pub hold_seconds: Option<u64>,
    #[serde(default)]
    pub acknowledge_below_floor: Option<bool>,
}

/// A calibration run, and the body of `GET /diagnostics/openfan-calibration`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OpenFanCalibrationRun {
    pub run_id: String,
    /// `openfan:chNN`.
    pub fan_id: String,
    pub channel: u8,
    /// `running` | `complete` | `cancelled` | `aborted` — the characterisation
    /// vocabulary, so one client poller serves every diagnostic.
    pub state: String,
    /// What the run is doing now: `descent` | `ascent` | `kick` | `restore`.
    /// `None` before the first write.
    pub phase: Option<String>,
    /// The duty most recently commanded by the run.
    pub current_pct: Option<u8>,
    /// The hold per step actually used, after clamping.
    pub hold_ms: u64,
    /// `None` while running, then one of the `OUTCOME_*` tokens.
    pub outcome: Option<String>,
    /// Set when `outcome` is `aborted`.
    pub abort_reason: Option<String>,
    /// Why the run ended, in words, when it did not simply complete.
    pub detail: Option<String>,
    /// The highest duty at which the fan was confirmed stopped on the way down.
    pub stall_duty_pct: Option<u8>,
    /// The lowest duty at which it was confirmed spinning on the way back up.
    pub restart_duty_pct: Option<u8>,
    /// `restart_duty_pct - stall_duty_pct`, when both were found.
    pub hysteresis_pct: Option<u8>,
    /// The lowest non-zero and the highest RPM any point read.
    pub min_rpm: Option<u16>,
    pub max_rpm: Option<u16>,
    /// A recovery kick ran and the fan was not confirmed spinning within the
    /// kick's window — on the `did_not_restart` kick or on one after an early
    /// stop. The channel is restored regardless.
    pub restart_failed_at_full: bool,
    /// The hottest fresh CPU reading at start, the highest seen, and the rise
    /// that aborts the run.
    pub start_cpu_temp_c: Option<f64>,
    pub max_cpu_temp_c: Option<f64>,
    pub rise_limit_c: f64,
    pub points: Vec<CalPoint>,
    /// The duty the channel held before the run; `None` when unknown (then the
    /// restore writes 100 %) and in the POST's 202 snapshot, which is built
    /// before the task reads it.
    pub original_pct: Option<u8>,
    /// `restored` | `restored_full_speed` | `write_failed` |
    /// `skipped_thermal_force` | `not_needed`, or `pending` while running.
    pub restore_outcome: String,
    /// True when the channel was left somewhere other than its original duty.
    pub restore_failed: bool,
    pub started_unix_ms: u64,
    pub completed_unix_ms: Option<u64>,
}

impl OpenFanCalibrationRun {
    pub fn is_running(&self) -> bool {
        self.state == STATE_RUNNING
    }

    /// Copy everything the walk measured onto the run — the one place the two
    /// shapes meet, so a field cannot be published mid-run and forgotten at the
    /// end.
    pub fn apply(&mut self, p: &CalProgress) {
        if !p.state.is_empty() {
            self.state = p.state.to_string();
        }
        self.phase = p.phase.map(str::to_string);
        self.current_pct = p.current_pct;
        self.outcome = p.outcome.map(str::to_string);
        self.abort_reason = p.abort_reason.map(str::to_string);
        self.detail = p.detail.clone();
        self.stall_duty_pct = p.stall_duty_pct;
        self.restart_duty_pct = p.restart_duty_pct;
        self.hysteresis_pct = match (p.stall_duty_pct, p.restart_duty_pct) {
            (Some(s), Some(r)) => Some(r.saturating_sub(s)),
            _ => None,
        };
        self.min_rpm = p.points.iter().map(|c| c.rpm).filter(|&r| r > 0).min();
        self.max_rpm = p.points.iter().map(|c| c.rpm).max();
        self.restart_failed_at_full = p.restart_failed_at_full;
        self.max_cpu_temp_c = p.max_cpu_temp_c;
        self.points = p.points.clone();
        self.original_pct = p.original_pct;
        self.restore_outcome = p.restore_outcome.to_string();
        self.restore_failed = p.restore_failed;
    }

    /// The deprecated sync route's shape, from a finished run.
    ///
    /// `stop_pwm` is now a measured stop (the stall duty) and `start_pwm` a
    /// measured start (the restart duty). A fan still spinning at 0 % starts
    /// from 0 %, which is what the old "lowest duty that read any RPM" said.
    pub fn legacy_result(&self) -> CalibrationResult {
        let start_pwm = match self.outcome.as_deref() {
            Some(OUTCOME_NO_STALL_DOWN_TO_0) => Some(0),
            _ => self.restart_duty_pct,
        };
        CalibrationResult {
            fan_id: self.fan_id.clone(),
            points: self.points.clone(),
            start_pwm,
            stop_pwm: self.stall_duty_pct,
            min_rpm: self.min_rpm.unwrap_or(0),
            max_rpm: self.max_rpm.unwrap_or(0),
        }
    }
}

/// The OpenFan calibration slot: the current or most recent run (what
/// `GET /diagnostics/openfan-calibration` reads), the cancel flag, and whether
/// a calibration task is still alive.
///
/// [SAFETY] `alive` is what keeps a second calibration off the controller. The
/// engine pause (DEC-191) is not enough by itself: its deadman can lapse under
/// a run whose serial write is wedged, a second `POST` could then claim it and
/// walk the controller while the first run is still alive, and installing the
/// second run would clear the first run's cancel flag (DEC-452 review, F1).
#[derive(Debug, Default)]
pub struct OpenFanCalibrationSlot {
    pub run: parking_lot::Mutex<Option<OpenFanCalibrationRun>>,
    pub cancel: AtomicBool,
    alive: AtomicBool,
}

impl OpenFanCalibrationSlot {
    /// Claim the slot for a new calibration task, or `None` while one is alive.
    pub fn claim(self: &Arc<Self>) -> Option<AliveGuard> {
        self.alive
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(AliveGuard {
            slot: Arc::clone(self),
            run_id: None,
        })
    }

    /// True while a calibration task holds the slot.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

/// Held by a calibration task for its whole life; dropping it frees the slot.
///
/// A task that ends without publishing its terminal state — a panic — would
/// otherwise leave its run reading `running` for ever, so the drop marks such a
/// run aborted with [`ABORT_TASK_FAILED`]. The channel itself is restored by
/// the walk's own backstop.
#[derive(Debug)]
pub struct AliveGuard {
    slot: Arc<OpenFanCalibrationSlot>,
    run_id: Option<String>,
}

impl AliveGuard {
    /// Install `run` as the slot's current run with the cancel flag cleared,
    /// under ONE lock — paired with the cancel handler's check-and-set (the
    /// characterisation rule).
    pub fn install(&mut self, run: OpenFanCalibrationRun) {
        let mut slot = self.slot.run.lock();
        self.slot.cancel.store(false, Ordering::SeqCst);
        self.run_id = Some(run.run_id.clone());
        *slot = Some(run);
    }
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        if let Some(id) = &self.run_id {
            if let Some(run) = self.slot.run.lock().as_mut() {
                if run.run_id == *id && run.is_running() {
                    log::error!(
                        "OpenFan calibration {id} of ch{} ended without a result; marking it \
                         aborted",
                        run.channel
                    );
                    run.state = STATE_ABORTED.to_string();
                    run.phase = None;
                    run.outcome = Some(OUTCOME_ABORTED.to_string());
                    run.abort_reason = Some(ABORT_TASK_FAILED.to_string());
                    run.detail = Some("the calibration task ended without a result".into());
                    run.completed_unix_ms = Some(crate::control_paths::unix_ms());
                }
            }
        }
        self.slot.alive.store(false, Ordering::SeqCst);
    }
}

/// A process-unique run id, in the characterisation shape.
pub fn next_run_id() -> String {
    format!("ofcal-{}", crate::api::characterization::next_run_id())
}

/// The hold per step, clamped (DEC-452).
pub fn clamp_hold(hold_seconds: Option<u64>) -> Duration {
    let asked = hold_seconds.unwrap_or(constants::OPENFAN_CAL_DEFAULT_HOLD_S);
    let held = asked.clamp(
        constants::OPENFAN_CAL_MIN_HOLD_S,
        constants::OPENFAN_CAL_MAX_HOLD_S,
    );
    if held != asked {
        log::info!(
            "OpenFan calibration: hold_seconds clamped from {asked} to {held} (valid range: {}–{})",
            constants::OPENFAN_CAL_MIN_HOLD_S,
            constants::OPENFAN_CAL_MAX_HOLD_S
        );
    }
    Duration::from_secs(held)
}

/// Error from a gate or a write.
#[derive(Debug, thiserror::Error)]
pub enum CalibrationError {
    #[error("thermal abort: sensor {sensor_id} at {temp_c:.1}°C exceeds {limit_c}°C")]
    ThermalAbort {
        sensor_id: String,
        temp_c: f64,
        limit_c: f64,
    },
    #[error("validation: {0}")]
    Validation(String),
    #[error("hardware: {0}")]
    Hardware(String),
}
/// Check whether any sensor in the cache exceeds the thermal limit.
/// Returns `Ok(())` or `Err(CalibrationError::ThermalAbort)`.
pub fn check_thermal_safety(cache: &StateCache) -> Result<(), CalibrationError> {
    let snap = cache.snapshot();
    for sensor in snap.sensors.values() {
        if sensor.value_c > constants::CALIBRATION_MAX_TEMP_C {
            return Err(CalibrationError::ThermalAbort {
                sensor_id: sensor.id.clone(),
                temp_c: sensor.value_c,
                limit_c: constants::CALIBRATION_MAX_TEMP_C,
            });
        }
    }
    Ok(())
}

/// The thermal ladder's forcing state, or `None` when it is not forcing
/// (DEC-295).
///
/// Deliberately a SEPARATE predicate rather than a new arm inside
/// [`check_thermal_safety`]: that function is also the verify gate, and DEC-295
/// was scoped to calibration, so widening its meaning would have changed
/// `/hwmon/{id}/verify` and `/gpu/{id}/fan/verify` behaviour from a change that
/// had not reviewed them. **DEC-297 then used this predicate to close exactly
/// that gap** in `verify_thermal_guard` — which is why it was separated rather
/// than folded in: the two callers wanted the rule at different times.
///
/// Both non-normal states force a duty floor (DEC-307) — `emergency` 100%,
/// `no_sensor_fallback` 40% — so either means the engine is writing a value
/// this sweep must not fight. (`recovery`, 60%, was a third until DEC-386
/// removed it; any string other than `normal` still counts.) `None` is a cache
/// that has never published a state, which is normal.
pub fn thermal_force_state(cache: &StateCache) -> Option<String> {
    match cache.snapshot().thermal_override_state {
        None => None,
        Some(s) if s == "normal" => None,
        Some(s) => Some(s),
    }
}

/// [SAFETY] How fresh the temperature telemetry the thermal gates read actually
/// is (DEC-336, register row `P8-p`).
///
/// The CpuTemp-then-fallback selection lives HERE, once, and is the only
/// producer of a [`TemperatureFreshness`] from a cache in this daemon. It used
/// to be inlined in `handlers::discovery::gather_preflight`, which made the
/// preflight the sole reader of a rule the write path also needs; a second
/// inlined copy at the write path would have been two gating shapes for one
/// safety rule, which is the proximate cause `CLAUDE.md § Hard-won lessons`
/// records for the v2.63.1 defect.
///
/// A machine with no CPU sensor at all falls back to every temperature it has,
/// rather than reporting "no temperature sensors" while a dozen are readable.
/// Every age is taken from ONE `Instant`, so two readings cannot be compared
/// against two different "now"s.
///
/// # The budget is derived, not fixed
///
/// [`diagnostic_temp_max_age`] is the thermal ladder's own trust window, so a
/// diagnostic refuses on a reading exactly when the ladder has stopped acting on
/// it — never while the ladder still trusts it, and never after — see that
/// function.
pub fn cache_temperature_freshness(cache: &StateCache) -> TemperatureFreshness {
    // Snapshot FIRST, then stamp `now`. Reversed, any time spent waiting on the
    // read lock behind the 1 Hz `update_sensors` writer is silently subtracted
    // from every age, and it subtracts in the fail-OPEN direction: a reading
    // stamped after `now` saturates to age 0 and reads as fresh. Sub-millisecond
    // in practice, and this is a gate rather than a report, so it is taken in
    // the conservative order. (The reversed order was carried over verbatim from
    // the copy that used to be inlined in `gather_preflight`.)
    //
    // `read_with`, not `snapshot()`: a full snapshot clones all five maps of
    // `DaemonState` to read one, which `health::cache` documents against as
    // EFF-1. This runs once per POST, once per preflight and once per discovery
    // cycle, beside two existing full-snapshot callers.
    let readings: Vec<(String, std::time::Instant)> = cache.read_with(|state| {
        let cpu: Vec<_> = state
            .sensors
            .values()
            .filter(|s| matches!(s.kind, crate::hwmon::types::SensorKind::CpuTemp))
            .map(|s| (s.id.clone(), s.updated_at))
            .collect();
        if cpu.is_empty() {
            state
                .sensors
                .values()
                .map(|s| (s.id.clone(), s.updated_at))
                .collect()
        } else {
            cpu
        }
    });
    let now = std::time::Instant::now();
    crate::api::preflight::temperature_freshness(&readings, diagnostic_temp_max_age(cache), now)
}

/// [SAFETY] How old a temperature reading may be before a diagnostic treats it
/// as unusable (DEC-336, narrowed to equality by DEC-395).
///
/// **Exactly** [`StateCache::cpu_temp_stale_after`] — the thermal ladder's own
/// trust window, at every poll cadence.
///
/// # Why it is that window, in both directions
///
/// **Never wider (DEC-395, register row `TS-aj`).** DEC-336 floored this at a
/// flat 10 s so discovery "behaves exactly as before" at the default 1 s
/// cadence, whose ladder window is 5 s. That left a band — ages 5 s to 10 s —
/// in which the ladder had already stopped trusting the CPU reading, and so
/// could not force on it however hot it read, while every diagnostic still
/// passed its staleness gate and went on to drive a header. Readings refresh
/// every poll, so a reading that old means reads are failing, which is exactly
/// the case the gate exists for. The floor protected discovery's pre-DEC-336
/// behaviour, a reason that never applied to the verify, characterisation and
/// calibration gates DEC-385 added on the same budget.
///
/// **Never narrower (DEC-336).** `polling.poll_interval_ms` has no upper bound
/// in `DaemonConfig::validate`; the overlay clamps it to
/// `MAX_SUPERVISABLE_POLL_INTERVAL_MS` (6 s), a supported cadence whose ladder
/// window is 30 s. A fixed budget below that would refuse a diagnostic on a
/// reading the ladder is still acting on, on a perfectly healthy machine.
///
/// **So the invariant is a relationship rather than a number: a diagnostic
/// refuses on a reading exactly when the ladder has stopped acting on it.**
/// Both sides compare `age <= window` (`preflight::temperature_freshness` and
/// `profile_engine::hottest_cpu_reading`), so equality of the window is
/// equality of the verdict. `cpu_temp_stale_after` is the one place that window
/// is defined, and this consumes it rather than restating it.
pub fn diagnostic_temp_max_age(cache: &StateCache) -> std::time::Duration {
    cache.cpu_temp_stale_after()
}

/// [SAFETY] The refusal the preflight publishes, performed (DEC-336, `P8-p`).
///
/// `Some(message)` when `diagnostic` blocks on a stale temperature source AND
/// the cache holds no usable reading ([`temperature_refusal`]). `None` otherwise.
/// Since DEC-385 every diagnostic blocks; the predicate is still consulted so the
/// published verdict and this refusal cannot come apart.
///
/// # Why this exists
///
/// `GET /diagnostics/preflight` has published `verdict: "blocked"`,
/// `blocking: ["temperature_source"]` for `control_path_discovery` since
/// DEC-333, and **nothing performed it**: the POST gated on
/// [`check_thermal_safety`] and [`thermal_force_state`], both of which compare
/// `value_c` with no age term. A wedged poll loop therefore froze the cache at
/// its last-known-good temperatures, the preflight said `blocked`, and the POST
/// returned `202` and perturbed a header on a machine whose real temperature
/// was unknown — leaving the refusal to whichever client happened to honour the
/// verdict, which §6.1 forbids in exactly those words.
///
/// It is deliberately keyed on [`Diagnostic::blocks_on_stale_temperature`]
/// rather than on a `matches!` of its own: that predicate is what the published
/// report is derived from, so consuming it is what makes the wire verdict and
/// the daemon's behaviour incapable of disagreeing.
///
/// [`Diagnostic::blocks_on_stale_temperature`]: crate::api::preflight::Diagnostic::blocks_on_stale_temperature
pub fn stale_temperature_refusal(
    cache: &StateCache,
    diagnostic: crate::api::preflight::Diagnostic,
) -> Option<String> {
    if !diagnostic.blocks_on_stale_temperature() {
        return None;
    }
    temperature_refusal(cache)
}

/// [SAFETY] Why the thermal guards cannot be evaluated right now, or `None` when
/// at least one usable temperature reading exists ([`cache_temperature_freshness`]).
///
/// The unconditional half of [`stale_temperature_refusal`], for an operation that
/// publishes no preflight verdict to stay consistent with — the OpenFan
/// calibration sweep (DEC-385). A diagnostic that has a preflight entry must go
/// through [`stale_temperature_refusal`] instead.
pub fn temperature_refusal(cache: &StateCache) -> Option<String> {
    let freshness = cache_temperature_freshness(cache);
    if freshness.is_usable() {
        return None;
    }
    Some(match (freshness.total, freshness.newest_age_ms) {
        (0, _) => "no temperature readings are available, so the thermal guards \
                   cannot be evaluated"
            .to_string(),
        (_, Some(age)) => format!(
            "every temperature reading is stale — freshest is {age} ms old, limit \
             {} ms; the thermal guards cannot be evaluated",
            // The budget actually applied: it follows the poll cadence, and a
            // message naming a limit the daemon did not use sends the operator
            // looking for the wrong fault.
            diagnostic_temp_max_age(cache).as_millis()
        ),
        _ => "every temperature reading is stale, so the thermal guards cannot be \
              evaluated"
            .to_string(),
    })
}
// ── The walk ─────────────────────────────────────────────────────────

/// The descent's duties: 100 % down to [`constants::OPENFAN_CAL_FINE_BELOW_PCT`]
/// in coarse steps, then to 0 % in fine ones.
pub fn descent_duties() -> Vec<u8> {
    let mut out = Vec::new();
    let mut pct = 100u8;
    while pct > constants::OPENFAN_CAL_FINE_BELOW_PCT {
        out.push(pct);
        pct -= constants::OPENFAN_CAL_COARSE_STEP_PCT;
    }
    loop {
        out.push(pct);
        if pct == 0 {
            return out;
        }
        pct = pct.saturating_sub(constants::OPENFAN_CAL_FINE_STEP_PCT);
    }
}

/// The ascent's duties from a stall at `stall_pct`: every descent duty above
/// the stall, up to [`constants::OPENFAN_CAL_FINE_BELOW_PCT`] — or, for a stall
/// found in the coarse band, one coarse step above it — lowest first.
pub fn ascent_duties(stall_pct: u8) -> Vec<u8> {
    let top = constants::OPENFAN_CAL_FINE_BELOW_PCT.max(
        stall_pct
            .saturating_add(constants::OPENFAN_CAL_COARSE_STEP_PCT)
            .min(100),
    );
    let mut up: Vec<u8> = descent_duties()
        .into_iter()
        .filter(|&d| d > stall_pct && d <= top)
        .collect();
    up.reverse();
    up
}

/// What the walk has measured, published as it goes and copied onto the run
/// by [`OpenFanCalibrationRun::apply`].
#[derive(Debug, Clone, PartialEq)]
pub struct CalProgress {
    /// Empty while running; the terminal state once finished.
    pub state: &'static str,
    pub phase: Option<&'static str>,
    pub current_pct: Option<u8>,
    pub outcome: Option<&'static str>,
    pub abort_reason: Option<&'static str>,
    pub detail: Option<String>,
    pub stall_duty_pct: Option<u8>,
    pub restart_duty_pct: Option<u8>,
    pub restart_failed_at_full: bool,
    pub max_cpu_temp_c: Option<f64>,
    pub points: Vec<CalPoint>,
    pub original_pct: Option<u8>,
    pub restore_outcome: &'static str,
    pub restore_failed: bool,
}

/// A controller write, run on the blocking pool by the walk. Serial I/O blocks
/// for up to the transport timeout, so it must never run on a runtime worker
/// (the DEC-146 P3-8 rule the engine's OpenFan writes follow).
pub type CalWriteFn = Arc<dyn Fn(u8, u8) -> Result<(), CalibrationError> + Send + Sync>;

/// Why the walk stopped early.
#[derive(Debug, Clone, PartialEq)]
enum Stop {
    ShuttingDown,
    Cancelled,
    /// An `ABORT_*` or thermal token, and the words.
    Abort(&'static str, String),
}

/// A hold's verdict from its last confirming samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Spinning,
    Stopped,
    Unconfirmed,
}

impl Verdict {
    fn token(self) -> &'static str {
        match self {
            Verdict::Spinning => OBS_SPINNING,
            Verdict::Stopped => OBS_STOPPED,
            Verdict::Unconfirmed => OBS_UNCONFIRMED,
        }
    }
}

/// The verdict from the last [`constants::OPENFAN_CAL_CONFIRM_SAMPLES`] fresh
/// samples: all zero is stopped, all non-zero is spinning, anything else — and
/// fewer samples than that — is unconfirmed. Pure, so the rule is testable on
/// its own.
fn verdict(fresh: &[u16]) -> Verdict {
    let need = constants::OPENFAN_CAL_CONFIRM_SAMPLES;
    if fresh.len() < need {
        return Verdict::Unconfirmed;
    }
    let last = &fresh[fresh.len() - need..];
    if last.iter().all(|&r| r == 0) {
        Verdict::Stopped
    } else if last.iter().all(|&r| r > 0) {
        Verdict::Spinning
    } else {
        Verdict::Unconfirmed
    }
}

/// The channel's cached RPM and when it was read.
fn rpm_reading(cache: &StateCache, channel: u8) -> Option<(u16, std::time::Instant)> {
    cache.read_with(|s| s.openfan_fans.get(&channel).map(|f| (f.rpm, f.updated_at)))
}

/// Run a controller write on the blocking pool.
async fn write_off_runtime(write: &CalWriteFn, channel: u8, pct: u8) -> Result<(), String> {
    let w = Arc::clone(write);
    match tokio::task::spawn_blocking(move || w(channel, pct)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(format!("the write task failed: {e}")),
    }
}

/// The walk's state and its injected hooks.
struct Walk<'a, S, K, A, P> {
    cache: &'a StateCache,
    channel: u8,
    hold: Duration,
    write: CalWriteFn,
    cancel: &'a AtomicBool,
    shutting_down: S,
    keepalive: K,
    /// Re-reads the pump-protection union for this channel (`ROLE-f`).
    pump_protected: A,
    publish: P,
    start_cpu_c: f64,
    p: CalProgress,
    /// Set before the first write is issued: from then a restore is owed,
    /// whether or not that write reports success (`TS-o`).
    touched: bool,
    /// The lowest duty commanded, and the last verdict, which decide the kick.
    lowest: Option<u8>,
    last_verdict: Option<Verdict>,
    /// Armed with the first write, disarmed by [`Walk::restore`].
    backstop: RestoreBackstop<'a>,
    /// A kick was owed and did not run to its end, so the restore writes
    /// 100 % instead of the original duty.
    restore_full_speed: bool,
    /// The channel became a pump mid-run. Sticky, so a racing un-assignment
    /// cannot lower the restore again (the `PumpWatch` rule).
    pump_seen: bool,
}

impl<S, K, A, P> Walk<'_, S, K, A, P>
where
    S: Fn() -> bool,
    K: Fn() -> bool,
    A: Fn() -> bool,
    P: Fn(&CalProgress),
{
    /// [SAFETY] Every gate, in order, before every write and on every sample.
    fn gate(&mut self) -> Result<(), Stop> {
        if (self.shutting_down)() {
            return Err(Stop::ShuttingDown);
        }
        if self.cancel.load(Ordering::SeqCst) {
            return Err(Stop::Cancelled);
        }
        // [SAFETY] `ROLE-f`: the entry refused a pump; this catches one assigned
        // since, read live on every sample. The write follows this check across
        // the controller lock, so an assignment landing in that gap lets at most
        // one step reach the wire; the next sample stops the walk. The restore —
        // and the drop backstop, should the task die first — then hold the pump
        // floor, and the restore re-reads the union itself (`refresh_pump_seen`).
        if self.refresh_pump_seen() {
            return Err(Stop::Abort(
                ABORT_PUMP_PROTECTED,
                "this channel was assigned the pump role; a pump is never walked toward 0%".into(),
            ));
        }
        if let Err(e) = check_thermal_safety(self.cache) {
            return Err(Stop::Abort(ThermalRefusal::TooHot.token(), e.to_string()));
        }
        if let Some(state) = thermal_force_state(self.cache) {
            return Err(Stop::Abort(
                ThermalRefusal::Forcing.token(),
                format!("thermal safety is forcing fan output ({state}); calibration cannot write"),
            ));
        }
        if let Some(reason) = temperature_refusal(self.cache) {
            return Err(Stop::Abort(
                ThermalRefusal::Stale.token(),
                format!("calibration cannot write: {reason}"),
            ));
        }
        let Some(now_c) = crate::api::stall_probe::hottest_fresh_cpu_c(self.cache) else {
            return Err(Stop::Abort(
                ABORT_NO_CPU_TEMPERATURE,
                "no fresh CPU temperature reading, so the rise gate cannot be evaluated".into(),
            ));
        };
        let max = self.p.max_cpu_temp_c.map_or(now_c, |m| m.max(now_c));
        self.p.max_cpu_temp_c = Some(max);
        if now_c > self.start_cpu_c + constants::STALL_PROBE_RISE_LIMIT_C {
            return Err(Stop::Abort(
                ABORT_THERMAL_RISE,
                format!(
                    "the hottest CPU reading rose from {:.1}°C to {now_c:.1}°C, more than the \
                     {:.0}°C calibration allows",
                    self.start_cpu_c,
                    constants::STALL_PROBE_RISE_LIMIT_C
                ),
            ));
        }
        // Last, so the pause is renewed only once every other gate has passed —
        // DEC-296's rule that it measures liveness, not duration.
        if !(self.keepalive)() {
            return Err(Stop::Abort(
                ABORT_SUPERSEDED,
                "superseded by a later diagnostic; the engine pause is no longer this run's".into(),
            ));
        }
        Ok(())
    }

    /// Re-read the pump union unless it is already known, recording a pump and
    /// raising the drop backstop to the pump floor. Never read once shutdown
    /// has been seen — the `PumpWatch` contract: the exit path owns the channel.
    fn refresh_pump_seen(&mut self) -> bool {
        if !self.pump_seen && !(self.shutting_down)() && (self.pump_protected)() {
            self.pump_seen = true;
        }
        if self.pump_seen {
            self.backstop.target = self.backstop.target.max(pump_floor_duty());
        }
        self.pump_seen
    }

    fn set_phase(&mut self, phase: &'static str, pct: Option<u8>) {
        self.p.phase = Some(phase);
        if pct.is_some() {
            self.p.current_pct = pct;
        }
        (self.publish)(&self.p);
    }

    /// Write `pct` and hold it, gated on every sample. `gated` is false only for
    /// the recovery kick, which must run after a cancel or an abort; it still
    /// stops on shutdown. `until_spinning` ends the hold early once spinning is
    /// confirmed (the kick).
    async fn step(
        &mut self,
        pct: u8,
        phase: &'static str,
        hold: Duration,
        gated: bool,
        until_spinning: bool,
    ) -> Result<Verdict, Stop> {
        if gated {
            self.gate()?;
        } else if (self.shutting_down)() {
            return Err(Stop::ShuttingDown);
        }
        self.set_phase(phase, Some(pct));
        self.touched = true;
        self.backstop.armed = true;
        self.lowest = Some(self.lowest.map_or(pct, |l| l.min(pct)));
        // Stamped BEFORE the write: only a reading taken after it describes
        // this duty.
        let written_at = std::time::Instant::now();
        if let Err(e) = write_off_runtime(&self.write, self.channel, pct).await {
            self.last_verdict = None;
            return Err(Stop::Abort(
                ABORT_WRITE_FAILED,
                format!("writing {pct}% to channel {} failed: {e}", self.channel),
            ));
        }

        let deadline = tokio::time::Instant::now() + hold;
        let mut fresh: Vec<u16> = Vec::new();
        let mut stopped_by: Option<Stop> = None;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep((deadline - now).min(constants::OPENFAN_CAL_SAMPLE_INTERVAL)).await;
            let checked = if gated {
                self.gate()
            } else if (self.shutting_down)() {
                Err(Stop::ShuttingDown)
            } else {
                // The kick still keeps the engine pause alive; its answer is
                // not a reason to stop writing 100 %.
                let _ = (self.keepalive)();
                Ok(())
            };
            if let Err(stop) = checked {
                stopped_by = Some(stop);
                break;
            }
            if let Some((rpm, at)) = rpm_reading(self.cache, self.channel) {
                if at >= written_at {
                    fresh.push(rpm);
                }
            }
            if until_spinning && verdict(&fresh) == Verdict::Spinning {
                break;
            }
        }

        let last = fresh.last().copied();
        if let Some(stop) = stopped_by {
            if let Some(rpm) = last {
                self.push(pct, rpm, phase, OBS_INTERRUPTED);
            }
            self.last_verdict = last.map(|r| {
                if r > 0 {
                    Verdict::Spinning
                } else {
                    Verdict::Stopped
                }
            });
            return Err(stop);
        }
        let Some(rpm) = last else {
            self.last_verdict = None;
            return Err(Stop::Abort(
                ABORT_RPM_UNREADABLE,
                format!(
                    "no RPM reading for channel {} arrived within the {} s hold at {pct}%",
                    self.channel,
                    hold.as_secs()
                ),
            ));
        };
        let v = verdict(&fresh);
        self.last_verdict = Some(v);
        self.push(pct, rpm, phase, v.token());
        Ok(v)
    }

    fn push(&mut self, pct: u8, rpm: u16, phase: &'static str, observation: &'static str) {
        self.p.points.push(CalPoint {
            pwm_percent: pct,
            rpm,
            phase: phase.to_string(),
            observation: observation.to_string(),
        });
        (self.publish)(&self.p);
    }

    /// The descent, then the ascent. Sets `outcome` on every path that
    /// completes; an early stop returns the reason.
    async fn walk(&mut self) -> Result<(), Stop> {
        let hold = self.hold;
        let mut stall = None;
        for pct in descent_duties() {
            match self.step(pct, PHASE_DESCENT, hold, true, false).await? {
                Verdict::Stopped if pct == 100 => {
                    self.p.outcome = Some(OUTCOME_NO_FAN_DETECTED);
                    return Ok(());
                }
                Verdict::Stopped => {
                    stall = Some(pct);
                    break;
                }
                Verdict::Spinning | Verdict::Unconfirmed => {}
            }
        }
        let Some(stall) = stall else {
            self.p.outcome = Some(OUTCOME_NO_STALL_DOWN_TO_0);
            return Ok(());
        };
        self.p.stall_duty_pct = Some(stall);
        (self.publish)(&self.p);

        for pct in ascent_duties(stall) {
            if self.step(pct, PHASE_ASCENT, hold, true, false).await? == Verdict::Spinning {
                self.p.restart_duty_pct = Some(pct);
                self.p.outcome = Some(OUTCOME_STALL_AND_RESTART_FOUND);
                return Ok(());
            }
        }
        // Still stopped at the top of the ascent: kick it, and say so.
        self.p.outcome = Some(OUTCOME_DID_NOT_RESTART);
        self.kick().await
    }

    /// [SAFETY] The 100 % recovery kick, held until the fan is seen spinning or
    /// for [`constants::OPENFAN_CAL_KICK_MAX`].
    async fn kick(&mut self) -> Result<(), Stop> {
        // Until the kick has run, a restore on drop must not leave the fan
        // at a duty that may not restart it.
        self.backstop.target = 100;
        let v = self
            .step(
                100,
                PHASE_KICK,
                constants::OPENFAN_CAL_KICK_MAX,
                false,
                true,
            )
            .await?;
        self.p.restart_failed_at_full = v != Verdict::Spinning;
        Ok(())
    }

    /// Whether an early stop may have left the fan stopped, so a kick is owed
    /// before the restore: the channel was written and the fan was not last
    /// confirmed spinning. With no verdict at all (a failed write, no fresh
    /// reading) the fan can be stopped only if the walk reached the fine band
    /// or found a stall — a stall can sit above the fine band, so the duty
    /// alone does not decide it (DEC-452 review, contract P2).
    fn kick_owed(&self) -> bool {
        self.touched
            && match self.last_verdict {
                Some(Verdict::Spinning) => false,
                Some(Verdict::Stopped | Verdict::Unconfirmed) => true,
                None => {
                    self.lowest
                        .is_some_and(|l| l < constants::OPENFAN_CAL_FINE_BELOW_PCT)
                        || self.p.stall_duty_pct.is_some()
                }
            }
    }

    /// [SAFETY] Put the pre-calibration duty back — or full speed where it is
    /// unknown (DEC-412) — unless thermal safety is forcing (DEC-295).
    async fn restore(&mut self) {
        // From here the restore is this function's, whatever it decides.
        self.backstop.armed = false;
        if !self.touched {
            self.p.restore_outcome = RESTORE_NOT_NEEDED;
            return;
        }
        // [SAFETY] `ROLE-f`: the gate does not run during the kick, and a cancel
        // or shutdown returns before it reaches the pump check, so a `pump`
        // assignment can land after the last gated sample. Read it once more
        // before choosing what to write.
        self.refresh_pump_seen();
        let target = if self.restore_full_speed {
            100
        } else if self.pump_seen {
            // `ROLE-f`: never back below the pump floor the engine now holds it at.
            crate::pwm::exit_duty(self.p.original_pct, 0).max(pump_floor_duty())
        } else {
            crate::pwm::exit_duty(self.p.original_pct, 0)
        };
        if let Some(state) = thermal_force_state(self.cache) {
            log::warn!(
                "OpenFan ch{} left at the thermal-safety forced duty instead of restoring \
                 {target}% — thermal safety is active ({state}) and outranks calibration",
                self.channel
            );
            self.p.restore_outcome = RestoreOutcome::SkippedThermalForce.token();
            self.p.restore_failed = true;
            return;
        }
        if self.restore_full_speed {
            log::warn!(
                "OpenFan ch{}: a recovery kick was owed and could not run — leaving it at \
                 100% rather than a duty that may not restart it",
                self.channel
            );
        } else if self.p.original_pct.is_none() {
            log::info!(
                "OpenFan ch{}: the pre-calibration duty is unknown (never commanded, or lost to \
                 a failed reply, a reconnect or a resume) — restoring {target}%",
                self.channel
            );
        }
        self.set_phase(PHASE_RESTORE, Some(target));
        match write_off_runtime(&self.write, self.channel, target).await {
            Ok(()) if self.restore_full_speed => {
                self.p.restore_outcome = RESTORE_FULL_SPEED;
                // Not the original duty, so not a restore in the run's sense.
                self.p.restore_failed = crate::pwm::exit_duty(self.p.original_pct, 0) != 100;
            }
            Ok(()) => {
                self.p.restore_outcome = RestoreOutcome::Restored.token();
                // `ROLE-f`: a pump floored above its pre-run duty is not back
                // where the run found it, and `restore_failed` says so.
                self.p.restore_failed = target != crate::pwm::exit_duty(self.p.original_pct, 0);
            }
            Err(e) => {
                log::warn!(
                    "OpenFan ch{}: restoring {target}% after calibration failed: {e}",
                    self.channel
                );
                self.p.restore_outcome = RestoreOutcome::WriteFailed.token();
                self.p.restore_failed = true;
            }
        }
    }
}

/// The pump floor as a duty. An OpenFan channel reports no `pwmN_mode`, so it
/// is never the DC pump floor (DEC-443).
fn pump_floor_duty() -> u8 {
    crate::profile::pump_floor_pct(None) as u8
}

/// Restores on drop if the walk wrote the channel and never reached its own
/// restore — a panic, or the runtime dropping the task at exit. Armed by the
/// first write, disarmed by [`Walk::restore`].
///
/// The one place a controller write runs synchronously, and only on a path that
/// is already abnormal: a blocking call in `Drop` is the lesser harm than a fan
/// left at a sweep step, which can be 0 % (DEC-297).
struct RestoreBackstop<'a> {
    armed: bool,
    channel: u8,
    target: u8,
    write: CalWriteFn,
    cache: &'a StateCache,
}

impl Drop for RestoreBackstop<'_> {
    fn drop(&mut self) {
        if !self.armed || thermal_force_state(self.cache).is_some() {
            return;
        }
        if let Err(e) = (self.write)(self.channel, self.target) {
            log::warn!(
                "OpenFan ch{}: backstop restore of {}% failed: {e}",
                self.channel,
                self.target
            );
        }
    }
}

/// [SAFETY] Run one calibration on `channel` and restore it. Returns the final
/// progress with `state` set.
///
/// `original` is the duty the controller held for the channel, read by the
/// caller under the controller lock AFTER claiming the engine pause (`TS-bh`).
/// `start_cpu_c` is the hottest fresh CPU reading the POST refused to start
/// without. `publish` is called on every point and phase change; the caller
/// fences it on its run id.
#[allow(clippy::too_many_arguments)]
pub async fn run_calibration<S, K, A, P>(
    cache: &StateCache,
    channel: u8,
    hold: Duration,
    original: Option<u8>,
    start_cpu_c: f64,
    write: CalWriteFn,
    cancel: &AtomicBool,
    shutting_down: S,
    keepalive: K,
    pump_protected: A,
    publish: P,
) -> CalProgress
where
    S: Fn() -> bool,
    K: Fn() -> bool,
    A: Fn() -> bool,
    P: Fn(&CalProgress),
{
    let mut w = Walk {
        cache,
        channel,
        hold,
        write: Arc::clone(&write),
        cancel,
        shutting_down,
        keepalive,
        pump_protected,
        publish,
        start_cpu_c,
        p: CalProgress {
            state: "",
            phase: None,
            current_pct: None,
            outcome: None,
            abort_reason: None,
            detail: None,
            stall_duty_pct: None,
            restart_duty_pct: None,
            restart_failed_at_full: false,
            max_cpu_temp_c: Some(start_cpu_c),
            points: Vec::new(),
            original_pct: original,
            restore_outcome: RestoreOutcome::Pending.token(),
            restore_failed: false,
        },
        touched: false,
        lowest: None,
        last_verdict: None,
        restore_full_speed: false,
        pump_seen: false,
        backstop: RestoreBackstop {
            armed: false,
            channel,
            target: crate::pwm::exit_duty(original, 0),
            write,
            cache,
        },
    };

    let ended = w.walk().await;
    match &ended {
        Ok(()) => w.p.state = STATE_COMPLETE,
        Err(Stop::Cancelled) => {
            w.p.state = STATE_CANCELLED;
            w.p.outcome = Some(OUTCOME_CANCELLED);
        }
        Err(Stop::ShuttingDown) => {
            w.p.state = STATE_ABORTED;
            w.p.outcome = Some(OUTCOME_ABORTED);
            w.p.abort_reason = Some(ABORT_SHUTTING_DOWN);
            w.p.detail = Some("the daemon is shutting down".into());
        }
        Err(Stop::Abort(token, detail)) => {
            w.p.state = STATE_ABORTED;
            w.p.outcome = Some(OUTCOME_ABORTED);
            w.p.abort_reason = Some(token);
            w.p.detail = Some(detail.clone());
        }
    }
    if ended.is_err() {
        log::info!(
            "OpenFan calibration of ch{channel} ended early: {}",
            w.p.detail.as_deref().unwrap_or("cancelled")
        );
    }

    // [SAFETY] The recovery kick, when an early stop may have left the fan
    // stopped — never while forcing (the force already holds 100 %). A kick
    // that cannot run to its end — shutting down, or it stopped early — leaves
    // the channel at 100 % instead of the original duty (module docs).
    // `&&` short-circuits, so the kick runs only when owed, not forcing and
    // not already shutting down.
    if ended.is_err()
        && w.kick_owed()
        && thermal_force_state(cache).is_none()
        && ((w.shutting_down)() || w.kick().await.is_err())
    {
        w.restore_full_speed = true;
    }

    w.restore().await;
    // The terminal publish belongs to the caller; the phase is over.
    w.p.phase = None;
    w.p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::{CachedSensorReading, DeviceLabel, OpenFanState};
    use crate::hwmon::types::SensorKind;
    use std::time::Instant;

    fn cpu(temp_c: f64, age: Duration) -> CachedSensorReading {
        CachedSensorReading {
            id: "cpu".into(),
            kind: SensorKind::CpuTemp,
            label: "Tctl".into(),
            value_c: temp_c,
            source: DeviceLabel::Hwmon,
            // Aged BY CONSTRUCTION: paused time does not advance `Instant`.
            updated_at: Instant::now() - age,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }
    }

    fn make_cache(temp_c: f64) -> Arc<StateCache> {
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![cpu(temp_c, Duration::ZERO)]);
        cache.update_openfan_fans(vec![OpenFanState {
            channel: 0,
            rpm: 900,
            last_commanded_pwm: Some(50),
            updated_at: Instant::now(),
            rpm_polled: true,
        }]);
        cache
    }

    type WriteLog = Arc<std::sync::Mutex<Vec<u8>>>;

    /// A fan that stops at or below `stall` on the way down and needs `restart`
    /// to start again: each write publishes the RPM the fan would then read,
    /// as a fresh poll would. `hook` runs after every write.
    fn fan(
        cache: &Arc<StateCache>,
        stall: u8,
        restart: u8,
        hook: impl Fn(u8) + Send + Sync + 'static,
    ) -> (CalWriteFn, WriteLog) {
        let log: WriteLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let spinning = Arc::new(AtomicBool::new(true));
        let (c, l) = (cache.clone(), log.clone());
        let f: CalWriteFn = Arc::new(move |_ch, pct| {
            l.lock().unwrap().push(pct);
            let now_spinning = if spinning.load(Ordering::SeqCst) {
                pct > stall
            } else {
                pct >= restart
            };
            spinning.store(now_spinning, Ordering::SeqCst);
            c.update_openfan_fans(vec![OpenFanState {
                channel: 0,
                rpm: if now_spinning {
                    300 + pct as u16 * 15
                } else {
                    0
                },
                last_commanded_pwm: Some(pct),
                updated_at: Instant::now(),
                rpm_polled: true,
            }]);
            hook(pct);
            Ok(())
        });
        (f, log)
    }

    async fn run(
        cache: &Arc<StateCache>,
        write: CalWriteFn,
        original: Option<u8>,
        cancel: &AtomicBool,
    ) -> CalProgress {
        run_calibration(
            cache,
            0,
            Duration::from_secs(2),
            original,
            50.0,
            write,
            cancel,
            || false,
            || true,
            || false,
            |_| {},
        )
        .await
    }

    #[test]
    fn the_descent_is_coarse_then_fine_down_to_zero() {
        let d = descent_duties();
        assert_eq!(&d[..8], &[100, 90, 80, 70, 60, 50, 40, 30]);
        assert_eq!(&d[8..10], &[28, 26]);
        assert_eq!(d.last(), Some(&0));
        assert!(d.windows(2).all(|w| w[0] > w[1]), "strictly descending");
    }

    #[test]
    fn the_ascent_climbs_fine_steps_to_the_fine_band_top() {
        assert_eq!(
            ascent_duties(10),
            vec![12, 14, 16, 18, 20, 22, 24, 26, 28, 30]
        );
        assert_eq!(ascent_duties(28), vec![30]);
        // A stall in the coarse band climbs one coarse step.
        assert_eq!(ascent_duties(40), vec![50]);
        assert_eq!(ascent_duties(100), Vec::<u8>::new());
    }

    #[test]
    fn a_verdict_needs_every_confirming_sample_to_agree() {
        assert_eq!(verdict(&[500, 0, 0, 0]), Verdict::Stopped);
        assert_eq!(verdict(&[0, 500, 500, 500]), Verdict::Spinning);
        assert_eq!(verdict(&[0, 0, 500]), Verdict::Unconfirmed);
        assert_eq!(verdict(&[]), Verdict::Unconfirmed);
        // Fewer samples than the rule needs confirm nothing, however they read.
        let short = constants::OPENFAN_CAL_CONFIRM_SAMPLES - 1;
        assert_eq!(verdict(&vec![0; short]), Verdict::Unconfirmed);
        assert_eq!(verdict(&vec![900; short]), Verdict::Unconfirmed);
        assert_eq!(
            verdict(&[0; constants::OPENFAN_CAL_CONFIRM_SAMPLES]),
            Verdict::Stopped
        );
    }

    /// The slot admits one task at a time, and a task that ends without a
    /// result — its guard dropped with the run still `running` — leaves the run
    /// aborted, not running for ever.
    #[test]
    fn the_slot_admits_one_task_and_a_dead_task_aborts_its_run() {
        let slot = Arc::new(OpenFanCalibrationSlot::default());
        let mut guard = slot.claim().expect("free");
        assert!(slot.claim().is_none(), "a second task is refused");
        slot.cancel.store(true, Ordering::SeqCst);
        guard.install(OpenFanCalibrationRun {
            run_id: "ofcal-char-1".into(),
            state: STATE_RUNNING.into(),
            ..Default::default()
        });
        assert!(
            !slot.cancel.load(Ordering::SeqCst),
            "installing clears the cancel flag"
        );
        drop(guard);
        assert!(!slot.is_alive());
        let run = slot.run.lock().clone().expect("the run stays");
        assert_eq!(run.state, STATE_ABORTED);
        assert_eq!(run.outcome.as_deref(), Some(OUTCOME_ABORTED));
        assert_eq!(run.abort_reason.as_deref(), Some(ABORT_TASK_FAILED));
        assert!(run.completed_unix_ms.is_some());

        // A run that finished is left alone.
        let mut guard = slot.claim().expect("free again");
        guard.install(OpenFanCalibrationRun {
            run_id: "ofcal-char-2".into(),
            state: STATE_RUNNING.into(),
            ..Default::default()
        });
        slot.run.lock().as_mut().unwrap().state = STATE_COMPLETE.into();
        drop(guard);
        assert_eq!(slot.run.lock().as_ref().unwrap().state, STATE_COMPLETE);
        assert_eq!(slot.run.lock().as_ref().unwrap().abort_reason, None);
    }

    /// The whole walk: down to the stall, back up to the restart, then the
    /// original duty restored — and both measured duties are real ones.
    #[tokio::test(start_paused = true)]
    async fn a_fan_is_walked_down_to_its_stall_and_back_up_to_its_restart() {
        let cache = make_cache(50.0);
        let (write, log) = fan(&cache, 10, 16, |_| {});
        let cancel = AtomicBool::new(false);

        let p = run(&cache, write, Some(50), &cancel).await;

        assert_eq!(p.state, STATE_COMPLETE);
        assert_eq!(p.outcome, Some(OUTCOME_STALL_AND_RESTART_FOUND));
        assert_eq!(p.stall_duty_pct, Some(10));
        assert_eq!(p.restart_duty_pct, Some(16));
        assert_eq!(p.restore_outcome, "restored");
        let w = log.lock().unwrap();
        assert_eq!(w.first(), Some(&100), "the walk starts at full speed");
        assert!(
            w.contains(&10) && !w.contains(&8),
            "stops descending at the stall: {w:?}"
        );
        assert_eq!(
            &w[w.len() - 4..],
            &[12, 14, 16, 50],
            "ascent, then restore: {w:?}"
        );
        assert!(p.points.iter().any(|c| c.phase == PHASE_DESCENT
            && c.pwm_percent == 10
            && c.observation == OBS_STOPPED));
    }

    /// `WIRE-l`: the old sweep's `stop_pwm` was always the step below
    /// `start_pwm`. A fan with hysteresis must report two duties that differ by
    /// more than one step, which only a descent can measure.
    #[tokio::test(start_paused = true)]
    async fn hysteresis_is_measured_not_inferred() {
        let cache = make_cache(50.0);
        let (write, _log) = fan(&cache, 6, 20, |_| {});
        let p = run(&cache, write, Some(50), &AtomicBool::new(false)).await;
        let mut r = OpenFanCalibrationRun::default();
        r.apply(&p);
        assert_eq!((r.stall_duty_pct, r.restart_duty_pct), (Some(6), Some(20)));
        assert_eq!(r.hysteresis_pct, Some(14));
        let legacy = r.legacy_result();
        assert_eq!((legacy.stop_pwm, legacy.start_pwm), (Some(6), Some(20)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fan_still_spinning_at_zero_and_an_empty_channel() {
        let cache = make_cache(50.0);
        let never_stops: CalWriteFn = {
            let c = cache.clone();
            Arc::new(move |_ch, pct| {
                c.update_openfan_fans(vec![OpenFanState {
                    channel: 0,
                    rpm: 200 + pct as u16 * 10,
                    last_commanded_pwm: Some(pct),
                    updated_at: Instant::now(),
                    rpm_polled: true,
                }]);
                Ok(())
            })
        };
        let p = run(&cache, never_stops, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.outcome, Some(OUTCOME_NO_STALL_DOWN_TO_0));
        assert_eq!(p.stall_duty_pct, None);
        let mut r = OpenFanCalibrationRun::default();
        r.apply(&p);
        assert_eq!(r.legacy_result().start_pwm, Some(0));

        let cache = make_cache(50.0);
        let (write, log) = fan(&cache, 100, 101, |_| {});
        let p = run(&cache, write, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.outcome, Some(OUTCOME_NO_FAN_DETECTED));
        assert_eq!(
            *log.lock().unwrap(),
            vec![100, 50],
            "one step, then the restore"
        );
    }

    /// A fan that does not restart within the ascent is kicked at 100 %, and the
    /// run says whether the kick worked, before the original duty goes back.
    #[tokio::test(start_paused = true)]
    async fn a_fan_that_does_not_restart_is_kicked_before_the_restore() {
        let cache = make_cache(50.0);
        let (write, log) = fan(&cache, 10, 40, |_| {});
        let p = run(&cache, write, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.outcome, Some(OUTCOME_DID_NOT_RESTART));
        assert_eq!(p.restart_duty_pct, None);
        assert!(!p.restart_failed_at_full, "it spins at 100 %");
        let w = log.lock().unwrap();
        assert_eq!(
            &w[w.len() - 3..],
            &[30, 100, 50],
            "top of ascent, kick, restore: {w:?}"
        );
    }

    /// [SAFETY] DEC-452 review (contract P2): a stall can sit above the fine
    /// band, so an early stop after one is kicked even though no duty below the
    /// band was ever written. Both arms: a hold cut short with no fresh reading
    /// (no verdict), and a cancel after a hold confirmed the fan stopped.
    #[tokio::test(start_paused = true)]
    async fn a_fan_that_stalls_above_the_fine_band_is_still_kicked() {
        // A stall at 30 % climbs one coarse step, to 40 %, which the descent
        // wrote too — so it is the SECOND 40 % that is the ascent's.
        assert_eq!(ascent_duties(30), vec![40]);
        // No verdict: the rise lands as the ascent step is written.
        let cache = make_cache(50.0);
        let c2 = cache.clone();
        let hot = 50.0 + constants::STALL_PROBE_RISE_LIMIT_C + 1.0;
        let forties = std::sync::atomic::AtomicU8::new(0);
        let (write, log) = fan(&cache, 30, 45, move |pct| {
            if pct == 40 && forties.fetch_add(1, Ordering::SeqCst) == 1 {
                c2.update_sensors(vec![cpu(hot, Duration::ZERO)]);
            }
        });
        let p = run(&cache, write, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.stall_duty_pct, Some(30), "precondition: {p:?}");
        assert_eq!(p.abort_reason, Some(ABORT_THERMAL_RISE));
        let w = log.lock().unwrap().clone();
        assert!(!w.iter().any(|&d| d < 30), "precondition: {w:?}");
        assert_eq!(
            &w[w.len() - 4..],
            &[30, 40, 100, 50],
            "kick, restore: {w:?}"
        );

        // Stopped: a cancel lands between samples of the ascent's 40 % hold.
        let cache = make_cache(50.0);
        let cancel = Arc::new(AtomicBool::new(false));
        let (write, log) = fan(&cache, 30, 45, |_| {});
        let (c2, cache2) = (cancel.clone(), cache.clone());
        let task = tokio::spawn(async move {
            run_calibration(
                &cache2,
                0,
                Duration::from_secs(15),
                Some(50),
                50.0,
                write,
                &c2,
                || false,
                || true,
                || false,
                |_| {},
            )
            .await
        });
        let mut waited = 0;
        while log.lock().unwrap().iter().filter(|&&d| d == 40).count() < 2 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += 1;
            assert!(waited < 10_000, "the walk never reached the ascent");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        cancel.store(true, Ordering::SeqCst);
        let p = task.await.unwrap();
        assert!(
            p.points
                .iter()
                .any(|c| c.pwm_percent == 40 && c.observation == OBS_INTERRUPTED && c.rpm == 0),
            "precondition: the cut hold read the fan stopped: {:?}",
            p.points
        );
        let w = log.lock().unwrap().clone();
        assert_eq!(
            &w[w.len() - 4..],
            &[30, 40, 100, 50],
            "kick, restore: {w:?}"
        );
    }

    /// [SAFETY] `TS-q`'s rule, now on every SAMPLE: a poll that wedges during a
    /// hold stops the run at the next sample, and it restores.
    #[tokio::test(start_paused = true)]
    async fn stale_readings_mid_hold_abort_the_run_and_it_restores() {
        let cache = make_cache(50.0);
        let stale = diagnostic_temp_max_age(&cache) + Duration::from_secs(5);
        let c2 = cache.clone();
        let (write, log) = fan(&cache, 10, 16, move |pct| {
            if pct == 80 {
                c2.update_sensors(vec![cpu(50.0, stale)]);
            }
        });
        let p = run(&cache, write, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.state, STATE_ABORTED);
        assert_eq!(p.abort_reason, Some("stale_temperature"));
        assert_eq!(
            *log.lock().unwrap(),
            vec![100, 90, 80, 50],
            "no step past the wedge"
        );
    }

    /// [SAFETY] `ROLE-f`: a channel assigned `pump` mid-run is walked no
    /// further, and the restore never puts it back below the pump floor — even
    /// when the duty it held before the run was lower.
    #[tokio::test(start_paused = true)]
    async fn a_pump_assigned_mid_run_stops_the_walk_and_restores_at_the_floor() {
        let cache = make_cache(50.0);
        let pump = Arc::new(AtomicBool::new(false));
        let p2 = pump.clone();
        let (write, log) = fan(&cache, 10, 16, move |pct| {
            if pct == 70 {
                p2.store(true, Ordering::SeqCst);
            }
        });
        let original = 20;
        assert!(
            original < pump_floor_duty(),
            "precondition: below the floor"
        );
        let p = run_calibration(
            &cache,
            0,
            Duration::from_secs(2),
            Some(original),
            50.0,
            write,
            &AtomicBool::new(false),
            || false,
            || true,
            move || pump.load(Ordering::SeqCst),
            |_| {},
        )
        .await;

        assert_eq!(p.state, STATE_ABORTED);
        assert_eq!(p.abort_reason, Some(ABORT_PUMP_PROTECTED));
        assert!(
            p.restore_failed,
            "a restore floored above the pre-run duty is not the original"
        );
        let w = log.lock().unwrap();
        let at_70 = w
            .iter()
            .position(|&d| d == 70)
            .expect("the walk reached 70%");
        assert_eq!(
            &w[at_70 + 1..],
            &[pump_floor_duty()],
            "nothing more is walked once the channel is a pump, and the restore \
             holds the pump floor: {w:?}"
        );
    }

    /// [SAFETY] `ROLE-f` review: a `pump` assignment landing during the
    /// ungated recovery kick, after a cancel, must still keep the restore at
    /// or above the pump floor — the restore re-reads the union itself.
    #[tokio::test(start_paused = true)]
    async fn a_pump_assigned_during_the_kick_still_restores_at_the_floor() {
        let cache = make_cache(50.0);
        let cancel = Arc::new(AtomicBool::new(false));
        let pump = Arc::new(AtomicBool::new(false));
        let (c2, p2) = (cancel.clone(), pump.clone());
        let (write, log) = fan(&cache, 10, 16, move |pct| {
            if pct == 10 {
                c2.store(true, Ordering::SeqCst);
            } else if pct == 100 && c2.load(Ordering::SeqCst) {
                p2.store(true, Ordering::SeqCst);
            }
        });
        let original = 20;
        assert!(
            original < pump_floor_duty(),
            "precondition: below the floor"
        );
        let p = run_calibration(
            &cache,
            0,
            Duration::from_secs(2),
            Some(original),
            50.0,
            write,
            &cancel,
            || false,
            || true,
            move || pump.load(Ordering::SeqCst),
            |_| {},
        )
        .await;

        assert_eq!(p.state, STATE_CANCELLED);
        let w = log.lock().unwrap();
        let kicked = w.iter().rposition(|&d| d == 100).expect("a kick ran");
        assert!(
            kicked > 0,
            "precondition: the kick followed the cancel: {w:?}"
        );
        assert_eq!(
            w.last(),
            Some(&pump_floor_duty()),
            "the restore must hold the pump floor: {w:?}"
        );
    }

    /// The pump check reads live, so it also refuses the very first write when
    /// the assignment landed between the entry check and the walk.
    #[tokio::test(start_paused = true)]
    async fn a_pump_assigned_before_the_first_write_writes_nothing() {
        let cache = make_cache(50.0);
        let (write, log) = fan(&cache, 10, 16, |_| {});
        let p = run_calibration(
            &cache,
            0,
            Duration::from_secs(2),
            Some(50),
            50.0,
            write,
            &AtomicBool::new(false),
            || false,
            || true,
            || true,
            |_| {},
        )
        .await;
        assert_eq!(p.abort_reason, Some(ABORT_PUMP_PROTECTED));
        assert_eq!(p.restore_outcome, RESTORE_NOT_NEEDED);
        assert!(log.lock().unwrap().is_empty(), "nothing may be written");
    }

    /// [SAFETY] The rise gate: the hottest fresh CPU reading rising more than
    /// the limit above its start aborts the run.
    #[tokio::test(start_paused = true)]
    async fn a_cpu_rise_aborts_the_run() {
        let cache = make_cache(50.0);
        let c2 = cache.clone();
        let hot = 50.0 + constants::STALL_PROBE_RISE_LIMIT_C + 1.0;
        let (write, _log) = fan(&cache, 10, 16, move |pct| {
            if pct == 70 {
                c2.update_sensors(vec![cpu(hot, Duration::ZERO)]);
            }
        });
        let p = run(&cache, write, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.abort_reason, Some(ABORT_THERMAL_RISE));
        assert_eq!(p.max_cpu_temp_c, Some(hot));
    }

    /// [SAFETY] DEC-295: once thermal safety is forcing, calibration writes
    /// nothing more — no step, no kick and no restore.
    #[tokio::test(start_paused = true)]
    async fn a_latched_emergency_stops_every_further_write() {
        let cache = make_cache(50.0);
        let c2 = cache.clone();
        let (write, log) = fan(&cache, 10, 16, move |pct| {
            if pct == 20 {
                c2.record_engine_tick("emergency", constants::THERMAL_EMERGENCY_TRIGGER_C);
            }
        });
        let p = run(&cache, write, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.abort_reason, Some("thermal_force"));
        assert_eq!(p.restore_outcome, "skipped_thermal_force");
        assert!(p.restore_failed);
        assert_eq!(
            log.lock().unwrap().last(),
            Some(&20),
            "{:?}",
            log.lock().unwrap()
        );
    }

    /// A cancel is honoured within one sample — not at the end of the hold —
    /// and a fan the walk had stopped is kicked before the restore.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_is_honoured_within_a_sample_and_a_stopped_fan_is_kicked() {
        let cache = make_cache(50.0);
        let cancel = Arc::new(AtomicBool::new(false));
        let (write, log) = fan(&cache, 20, 30, |_| {});
        let c2 = cancel.clone();
        let cache2 = cache.clone();
        let task = tokio::spawn(async move {
            run_calibration(
                &cache2,
                0,
                Duration::from_secs(15),
                Some(50),
                50.0,
                write,
                &c2,
                || false,
                || true,
                || false,
                |_| {},
            )
            .await
        });
        // Wait until the walk has written the stalling 20 %, then cancel.
        let mut waited = 0;
        while !log.lock().unwrap().contains(&20) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += 1;
            assert!(waited < 10_000, "the walk never reached 20 %");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let asked = tokio::time::Instant::now();
        cancel.store(true, Ordering::SeqCst);
        let p = task.await.unwrap();
        assert_eq!(p.state, STATE_CANCELLED);
        assert_eq!(p.outcome, Some(OUTCOME_CANCELLED));
        let w = log.lock().unwrap().clone();
        assert_eq!(&w[w.len() - 2..], &[100, 50], "kick, then restore: {w:?}");
        assert!(
            asked.elapsed() < Duration::from_secs(1) + constants::OPENFAN_CAL_KICK_MAX,
            "the cancel waited for the hold: {:?}",
            asked.elapsed()
        );
        assert!(
            p.points.iter().any(|c| c.observation == OBS_INTERRUPTED),
            "the cut-short hold is recorded as interrupted"
        );
    }

    /// Run until shutdown is signalled on the write of `down_at`.
    async fn run_until_shutdown_at(down_at: u8, original: Option<u8>) -> (CalProgress, Vec<u8>) {
        let cache = make_cache(50.0);
        let down = Arc::new(AtomicBool::new(false));
        let d2 = down.clone();
        let (write, log) = fan(&cache, 10, 16, move |pct| {
            if pct == down_at {
                d2.store(true, Ordering::SeqCst);
            }
        });
        let p = run_calibration(
            &cache,
            0,
            Duration::from_secs(2),
            original,
            50.0,
            write,
            &AtomicBool::new(false),
            move || down.load(Ordering::SeqCst),
            || true,
            || false,
            |_| {},
        )
        .await;
        let w = log.lock().unwrap().clone();
        (p, w)
    }

    /// [SAFETY] Shutdown with a kick owed (the fan was stopped): no kick can run,
    /// so the restore writes 100 % rather than an original duty that may not
    /// restart it (DEC-452 review, F3).
    #[tokio::test(start_paused = true)]
    async fn shutdown_with_a_kick_owed_restores_full_speed_not_the_original() {
        let (p, w) = run_until_shutdown_at(12, Some(50)).await;
        assert_eq!(p.abort_reason, Some(ABORT_SHUTTING_DOWN));
        assert_eq!(w.last(), Some(&100), "full speed, not 50: {w:?}");
        assert_eq!(
            w.iter().filter(|&&d| d == 100).count(),
            2,
            "first step + restore, no kick: {w:?}"
        );
        assert_eq!(p.restore_outcome, RESTORE_FULL_SPEED);
        assert!(p.restore_failed, "not the original duty");
    }

    /// [SAFETY] Shutdown with no kick owed (the fan was last seen spinning)
    /// restores the original; an unknown original restores full speed
    /// (DEC-412).
    #[tokio::test(start_paused = true)]
    async fn shutdown_with_no_kick_owed_restores_the_original_or_full_speed() {
        let (p, w) = run_until_shutdown_at(80, Some(50)).await;
        assert_eq!(p.abort_reason, Some(ABORT_SHUTTING_DOWN));
        assert_eq!(&w[w.len() - 2..], &[80, 50], "{w:?}");
        assert_eq!(p.restore_outcome, "restored");
        assert!(!p.restore_failed);

        let (p, w) = run_until_shutdown_at(80, None).await;
        assert_eq!(&w[w.len() - 2..], &[80, 100], "{w:?}");
        assert_eq!(p.restore_outcome, "restored");
        assert!(!p.restore_failed);
    }

    /// A reading taken before the write describes the old duty; with no fresh
    /// one the run aborts rather than recording it.
    #[tokio::test(start_paused = true)]
    async fn no_fresh_rpm_after_a_write_aborts_as_unreadable() {
        let cache = make_cache(50.0);
        // The cached reading must predate every write stamp strictly.
        std::thread::sleep(Duration::from_millis(2));
        let log: WriteLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = log.clone();
        let deaf: CalWriteFn = Arc::new(move |_ch, pct| {
            l.lock().unwrap().push(pct);
            Ok(())
        });
        let p = run(&cache, deaf, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.abort_reason, Some(ABORT_RPM_UNREADABLE));
        assert_eq!(*log.lock().unwrap(), vec![100, 50]);
        assert!(p.points.is_empty());
    }

    /// A write that fails is `write_failed`, and the restore is still tried.
    #[tokio::test(start_paused = true)]
    async fn a_failed_write_aborts_and_still_restores() {
        let cache = make_cache(50.0);
        let (inner, log) = fan(&cache, 10, 16, |_| {});
        let failing: CalWriteFn = Arc::new(move |ch, pct| {
            if pct == 60 {
                return Err(CalibrationError::Hardware("mock".into()));
            }
            inner(ch, pct)
        });
        let p = run(&cache, failing, Some(50), &AtomicBool::new(false)).await;
        assert_eq!(p.abort_reason, Some(ABORT_WRITE_FAILED));
        assert_eq!(log.lock().unwrap().last(), Some(&50));
        assert_eq!(p.restore_outcome, "restored");
    }

    /// DEC-297's case, now the backstop's: a run whose future is dropped
    /// mid-walk (a runtime teardown) still restores the channel; one dropped
    /// before its first write writes nothing.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_run_still_restores_through_the_backstop() {
        let cache = make_cache(50.0);
        let (write, log) = fan(&cache, 10, 16, |_| {});
        {
            let cancel = AtomicBool::new(false);
            let fut = run(&cache, write, Some(50), &cancel);
            tokio::pin!(fut);
            tokio::select! {
                _ = &mut fut => panic!("the walk completed too fast to model a drop"),
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
        let w = log.lock().unwrap();
        assert!(
            w.len() >= 2,
            "precondition: the walk wrote before the drop: {w:?}"
        );
        assert_eq!(w.last(), Some(&50), "restored on drop: {w:?}");

        let cache = make_cache(50.0);
        let (write, log) = fan(&cache, 10, 16, |_| {});
        drop(run(&cache, write, Some(50), &AtomicBool::new(false)));
        assert!(
            log.lock().unwrap().is_empty(),
            "never polled, never written"
        );
    }

    /// The keepalive is the last gate; a failed one ends the run as superseded.
    #[tokio::test(start_paused = true)]
    async fn a_lost_engine_pause_is_superseded() {
        let cache = make_cache(50.0);
        let (write, _log) = fan(&cache, 10, 16, |_| {});
        let calls = std::sync::atomic::AtomicU32::new(0);
        let p = run_calibration(
            &cache,
            0,
            Duration::from_secs(2),
            Some(50),
            50.0,
            write,
            &AtomicBool::new(false),
            || false,
            || calls.fetch_add(1, Ordering::SeqCst) < 6,
            || false,
            |_| {},
        )
        .await;
        assert_eq!(p.abort_reason, Some(ABORT_SUPERSEDED));
    }
}
