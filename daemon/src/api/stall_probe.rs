//! [SAFETY] The stall/restart probe — the one diagnostic that writes below 20 %
//! on purpose (DEC-407, DEC-404 Stage 3, register row `PTR-h`).
//!
//! # What it measures
//!
//! Where a fan stops as its duty falls (the **stall duty**) and where it starts
//! again as the duty rises from a stop (the **restart duty**). The difference is
//! the hysteresis a profile needs to respect: a curve that commands a duty
//! between the two can stop a fan it cannot restart. Characterisation cannot
//! find either, by design — its flat clamp keeps every walked duty at or above
//! [`constants::CHARACTERIZATION_MIN_PCT`], and that clamp stays true of it.
//! This probe is a separate diagnostic with its own, narrower envelope.
//!
//! # The envelope (DEC-404 decision 2, clarified at Stage 3 as S3-1…S3-12)
//!
//! - **Eligible** only when the daemon-resolved role is `chassis_fan` or
//!   `radiator_fan`, the header is **not** [`header_is_pump_protected`] (the full
//!   union, DEC-384's profile term included), is writable and has a tach. A CPU
//!   temperature must be fresh, because the +5 °C rise gate cannot be evaluated
//!   without one. Re-checked before **every** write and on **every** sample, not
//!   only at entry — the `TS-aw` lesson: a run that decides the pump question
//!   once can be overtaken by an assignment made mid-run (S3-R2).
//! - **Baseline**: 20 %, held until settled on register updates (Stage 1's rule)
//!   for at most one settle window. It measures the tach refresh — the driver's
//!   `update_interval` preferred — and proves a fan is there. A 0 there is
//!   `stalled_at_or_above_20` for a fan that was spinning before (and kicks), or
//!   `no_fan_detected` (S3-R1).
//! - **Descent**: 18 % → 0 % in [`constants::STALL_PROBE_STEP_PCT`] steps, each
//!   held for `max(6 s, 3 × refresh)`. A stall is 0 rpm across two refreshes.
//! - **Ascent**: from the stall duty up in the same steps to 20 % inclusive. A
//!   restart is rpm > 0 across two refreshes.
//! - **Budget**: derived from the refresh as the worst-case walk and capped at
//!   [`constants::STALL_PROBE_BUDGET_CAP`]; a header too slow to fit is refused
//!   during the baseline, before any sub-floor duty is written.
//! - **Aborts** on any diagnostic gate (shutdown, 85 °C, ladder forcing, stale
//!   temperatures, a lost lease), on the hottest fresh CPU reading rising
//!   [`constants::STALL_PROBE_RISE_LIMIT_C`] above its start, on lost
//!   eligibility, on the budget, on a reclaim, on an unreadable sample or a read
//!   that does not return within [`constants::DIAGNOSTIC_READ_BUDGET`], and on
//!   cancel. Every abort and cancel ends with a **100 % recovery kick** held
//!   until the fan is seen spinning (bounded) — **never while shutting down**,
//!   when a write after the hand-back would re-take the header (S3-R3).
//! - **Restore** is the shared [`RestoreOnDrop`] with its DEC-315 tokens, and
//!   the mode is handed back by DEC-382 on the next engine tick.
//!
//! # Why the loop is its own and the gates are not
//!
//! S3-4: the gates are `diagnostic_gates`, shared with characterisation. The
//! loop is adaptive — the next duty depends on what the last one observed, a
//! step ends early once a stall or restart is confirmed, and a cancel is
//! honoured on every sample — which characterisation's fixed-plan hold is not.
//!
//! [`header_is_pump_protected`]: crate::api::handlers::AppState::header_is_pump_protected

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::characterization::{
    RestoreOnDrop, RestoreReport, STATE_ABORTED, STATE_CANCELLED, STATE_COMPLETE, STATE_FAILED,
    STATE_RUNNING,
};
use crate::api::diagnostic_gates::{step_gate, thermal_refusal, GateStop, ThermalRefusal};
use crate::api::preflight::Diagnostic;
use crate::api::responses::HwmonVerifyState;
use crate::api::stats::{self, RpmSample};
use crate::constants;
use crate::health::cache::StateCache;
use crate::hwmon::roles::HeaderRole;

/// The preflight a client requests before this probe (`pwm_stall_probe`).
pub const STALL_PROBE_DIAGNOSTIC: Diagnostic = Diagnostic::StallProbe;

// ── Vocabulary ───────────────────────────────────────────────────────
// Stable tokens. The client owns the wording and must render an unrecognised
// token rather than dropping it (the 273-i rule).

/// The fan stopped on the way down and started again on the way up.
pub const OUTCOME_STALL_AND_RESTART_FOUND: &str = "stall_and_restart_found";
/// The fan was still spinning at 0 %.
pub const OUTCOME_NO_STALL_DOWN_TO_0: &str = "no_stall_down_to_0";
/// The fan stopped and was still stopped at 20 %; the kick followed.
pub const OUTCOME_DID_NOT_RESTART_BELOW_20: &str = "did_not_restart_below_20";
/// The tach read 0 at the 20 % baseline, and 0 (or unreadable) before the probe
/// wrote anything — no fan was seen — so nothing below 20 % was written.
pub const OUTCOME_NO_FAN_DETECTED: &str = "no_fan_detected";
/// S3-R1: the fan was spinning before the probe and read 0 rpm at the 20 %
/// baseline, so its stall duty is 20 % or higher (a DC-mode header, typically).
/// Nothing below 20 % was written; the kick followed, then the restore.
pub const OUTCOME_STALLED_AT_OR_ABOVE_20: &str = "stalled_at_or_above_20";
/// The run stopped early; `abort_reason` says why.
pub const OUTCOME_ABORTED: &str = "aborted";
/// A `DELETE /diagnostics/stall-probe` stopped the run.
pub const OUTCOME_CANCELLED: &str = "cancelled";

/// `abort_reason` tokens. The three thermal ones come from
/// [`ThermalRefusal::token`].
pub const ABORT_SHUTTING_DOWN: &str = "shutting_down";
pub const ABORT_SUPERSEDED: &str = "superseded";
pub const ABORT_THERMAL_RISE: &str = "thermal_rise";
pub const ABORT_NO_CPU_TEMPERATURE: &str = "no_cpu_temperature";
pub const ABORT_ELIGIBILITY_LOST: &str = "eligibility_lost";
pub const ABORT_BUDGET_EXCEEDED: &str = "budget_exceeded";
pub const ABORT_RECLAIMED: &str = "reclaimed";
pub const ABORT_WRITE_FAILED: &str = "write_failed";
pub const ABORT_REFRESH_UNKNOWN: &str = "refresh_unknown";
pub const ABORT_REFRESH_TOO_SLOW: &str = "refresh_too_slow";
pub const ABORT_TACH_UNREADABLE: &str = "tach_unreadable";

/// Why a header cannot be probed. Shared by the POST's refusal, the preflight
/// row and the mid-run re-check, so the three cannot disagree.
pub const INELIGIBLE_PUMP_PROTECTED: &str = "pump_protected";
pub const INELIGIBLE_CPU_FAN: &str = "cpu_fan";
pub const INELIGIBLE_ROLE_UNKNOWN: &str = "role_unknown";
pub const INELIGIBLE_READ_ONLY: &str = "read_only";
pub const INELIGIBLE_NO_TACH: &str = "no_tach";

/// `ProbePoint.phase`.
pub const PHASE_BASELINE: &str = "baseline";
pub const PHASE_DESCENT: &str = "descent";
pub const PHASE_ASCENT: &str = "ascent";
pub const PHASE_KICK: &str = "kick";

/// `ProbePoint.observation`.
pub const OBS_SPINNING: &str = "spinning";
pub const OBS_STALLED: &str = "stalled";
pub const OBS_RESTARTED: &str = "restarted";
pub const OBS_STOPPED: &str = "stopped";
pub const OBS_NO_FAN: &str = "no_fan";
pub const OBS_UNREADABLE: &str = "unreadable";
/// The hold was cut short by an abort or a cancel; the samples it did take are
/// still counted.
pub const OBS_INTERRUPTED: &str = "interrupted";
/// The hold ended without a confirmation, and its last reading contradicts the
/// plain verdict (a 0 at the end of a descent step, a non-zero at the end of an
/// ascent step): the probe moves on, and says it could not tell.
pub const OBS_UNCONFIRMED: &str = "unconfirmed";

/// `StallProbeRun.refresh_source`.
pub const REFRESH_DRIVER: &str = "driver_update_interval";
pub const REFRESH_OBSERVED: &str = "observed";

// ── Wire types ───────────────────────────────────────────────────────

/// Body of `POST /hwmon/{header_id}/stall-probe`.
///
/// [SAFETY] **No tunables, and unknown fields are rejected.** Every duty, step,
/// dwell and budget is derived by the daemon from the header's own
/// measurements, so there is nothing a crafted request can widen. The one field
/// is the explicit acknowledgement S3-6 requires: without `true` the request is
/// refused, so a mistyped `curl` aimed at another endpoint's path cannot start
/// a run that goes to 0 %.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StallProbeRequest {
    pub acknowledge_below_floor: Option<bool>,
}

/// One held duty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProbePoint {
    /// `baseline` | `descent` | `ascent` | `kick`.
    pub phase: String,
    /// Position in the run's walk, from 0.
    pub step_index: u16,
    pub commanded_pct: u8,
    /// `false` when the write itself failed.
    pub command_accepted: bool,
    pub readback_pct: Option<u8>,
    pub pwm_enable: Option<u8>,
    /// The tach just before the write.
    pub rpm_before: Option<u16>,
    /// The last tach reading of the hold.
    pub rpm_after: Option<u16>,
    /// How long the duty was held.
    pub held_ms: u64,
    /// When, from the write, the stall or restart was confirmed; `None` when
    /// it was not.
    pub confirmed_at_ms: Option<u64>,
    /// Sub-samples taken, and how many of them read exactly 0 rpm.
    pub samples: u32,
    pub zero_samples: u32,
    /// `spinning` | `stalled` | `restarted` | `stopped` | `no_fan` |
    /// `unreadable` | `interrupted` | `unconfirmed`. An opaque token: render an
    /// unrecognised one, never drop it (273-i).
    pub observation: String,
}

/// Everything the loop measured. Copied onto [`StallProbeRun`] by the handler.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProbeResult {
    pub state: &'static str,
    /// The duty read before the probe's first write; `None` when unreadable.
    pub original_pct: Option<u8>,
    pub outcome: Option<&'static str>,
    pub abort_reason: Option<&'static str>,
    pub detail: Option<String>,
    pub stall_duty_pct: Option<u8>,
    pub restart_duty_pct: Option<u8>,
    pub lowest_commanded_pct: Option<u8>,
    pub time_below_floor_ms: u64,
    pub baseline_rpm: Option<u16>,
    pub baseline_settled: Option<bool>,
    pub refresh_ms: Option<u64>,
    pub refresh_source: Option<&'static str>,
    pub dwell_ms: Option<u64>,
    pub confirm_ms: Option<u64>,
    pub budget_ms: Option<u64>,
    pub start_cpu_temp_c: Option<f64>,
    pub max_cpu_temp_c: Option<f64>,
    pub restart_failed_at_full: bool,
    pub points: Vec<ProbePoint>,
}

/// A stall/restart probe run, and the body of `GET /diagnostics/stall-probe`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StallProbeRun {
    pub run_id: String,
    pub header_id: String,
    /// `running` | `complete` | `cancelled` | `aborted` | `failed` — the same
    /// vocabulary as a characterisation run, so one client poller serves both.
    pub state: String,
    /// `None` while running, then one of the `OUTCOME_*` tokens.
    pub outcome: Option<String>,
    /// Set when `outcome` is `aborted`: one of the `ABORT_*` tokens or a
    /// thermal token (`thermal_limit` | `thermal_force` | `stale_temperature`).
    pub abort_reason: Option<String>,
    /// Why the run ended, in words, when it did not simply complete.
    pub detail: Option<String>,
    /// The highest duty at which the fan was confirmed stopped on the way down.
    /// `0` is a stall too (S3-9): an ordinary PWM fan that stops only at 0 %.
    pub stall_duty_pct: Option<u8>,
    /// The lowest duty at which the fan was confirmed spinning on the way back
    /// up from the stall. At most 20.
    pub restart_duty_pct: Option<u8>,
    /// `restart_duty_pct - stall_duty_pct`, when both were found.
    pub hysteresis_pct: Option<u8>,
    /// The lowest duty this run commanded.
    pub lowest_commanded_pct: Option<u8>,
    /// Time the probe spent commanding below 20 %, up to its last probe write.
    pub time_below_floor_ms: u64,
    /// The tach at the end of the 20 % baseline, and whether it settled.
    pub baseline_rpm: Option<u16>,
    pub baseline_settled: Option<bool>,
    /// The tach refresh the timing was derived from, and where it came from:
    /// `driver_update_interval` | `observed`.
    pub refresh_ms: Option<u64>,
    pub refresh_source: Option<String>,
    /// The derived per-step dwell, the confirm window (two refreshes) and the
    /// budget below 20 %. `None` until the baseline has measured the refresh.
    pub dwell_ms: Option<u64>,
    pub confirm_ms: Option<u64>,
    pub budget_ms: Option<u64>,
    /// The hottest fresh CPU reading at start, the highest seen during the run,
    /// and the rise that aborts it.
    pub start_cpu_temp_c: Option<f64>,
    pub max_cpu_temp_c: Option<f64>,
    pub rise_limit_c: f64,
    /// The recovery kick held 100 % for its whole window and the fan still read
    /// 0 rpm. The header is restored regardless; this says a fan may be
    /// physically stuck.
    pub restart_failed_at_full: bool,
    pub points: Vec<ProbePoint>,
    /// The duty the header held before the probe; `None` when unreadable, and
    /// in the POST's 202 snapshot, which is built before the task reads it.
    pub original_pct: Option<u8>,
    /// Same meaning and single source as on a characterisation run (DEC-315).
    pub restore_failed: bool,
    pub restore_outcome: String,
    pub completed_unix_ms: Option<u64>,
    /// `§9` provenance sidecar: field name → classification.
    pub provenance: BTreeMap<String, String>,
}

impl StallProbeRun {
    pub fn is_running(&self) -> bool {
        self.state == STATE_RUNNING
    }

    /// Copy everything the loop measured onto the run. The one place the two
    /// shapes meet, so a field cannot be published mid-run and forgotten at the
    /// end (or the reverse).
    pub fn apply(&mut self, r: &ProbeResult) {
        if !r.state.is_empty() {
            self.state = r.state.to_string();
        }
        self.outcome = r.outcome.map(str::to_string);
        self.abort_reason = r.abort_reason.map(str::to_string);
        self.detail = r.detail.clone();
        self.stall_duty_pct = r.stall_duty_pct;
        self.restart_duty_pct = r.restart_duty_pct;
        self.hysteresis_pct = match (r.stall_duty_pct, r.restart_duty_pct) {
            (Some(s), Some(u)) => Some(u.saturating_sub(s)),
            _ => None,
        };
        self.lowest_commanded_pct = r.lowest_commanded_pct;
        self.time_below_floor_ms = r.time_below_floor_ms;
        self.baseline_rpm = r.baseline_rpm;
        self.baseline_settled = r.baseline_settled;
        self.refresh_ms = r.refresh_ms;
        self.refresh_source = r.refresh_source.map(str::to_string);
        self.dwell_ms = r.dwell_ms;
        self.confirm_ms = r.confirm_ms;
        self.budget_ms = r.budget_ms;
        self.start_cpu_temp_c = r.start_cpu_temp_c;
        self.max_cpu_temp_c = r.max_cpu_temp_c;
        self.restart_failed_at_full = r.restart_failed_at_full;
        self.points = r.points.clone();
        self.original_pct = r.original_pct;
    }
}

/// The slot `GET`/`DELETE /diagnostics/stall-probe` read.
pub type StallProbeSlot = std::sync::Arc<parking_lot::Mutex<Option<StallProbeRun>>>;

/// A process-unique run id, in the characterisation shape.
pub fn next_run_id() -> String {
    format!("probe-{}", crate::api::characterization::next_run_id())
}

/// `§9` provenance for a probe result.
pub fn provenance_legend() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for k in ["commanded_pct", "lowest_commanded_pct", "rise_limit_c"] {
        m.insert(k.to_string(), "COMMANDED".to_string());
    }
    for k in [
        "readback_pct",
        "pwm_enable",
        "rpm_before",
        "rpm_after",
        "baseline_rpm",
        "start_cpu_temp_c",
        "max_cpu_temp_c",
        "original_pct",
    ] {
        m.insert(k.to_string(), "OBSERVED".to_string());
    }
    for k in [
        "stall_duty_pct",
        "restart_duty_pct",
        "hysteresis_pct",
        "observation",
        "outcome",
        "refresh_ms",
        "dwell_ms",
        "confirm_ms",
        "budget_ms",
        "time_below_floor_ms",
        "restart_failed_at_full",
    ] {
        m.insert(k.to_string(), "DERIVED".to_string());
    }
    m
}

// ── Pure rules ───────────────────────────────────────────────────────

/// [SAFETY] Why a header cannot be probed, or `None` when it can.
///
/// The display role, NOT the pump union, decides the role half (S3-5): a user
/// who assigns `radiator_fan` to a `CPU_FAN` header carrying radiator fans makes
/// it eligible. The pump union decides the other half and is checked FIRST, so a
/// header whose role reads `chassis_fan` while its label or the active profile
/// says pump is refused as `pump_protected` — the DEC-312 case.
pub fn ineligibility(
    role: HeaderRole,
    pump_protected: bool,
    writable: bool,
    has_tach: bool,
) -> Option<&'static str> {
    if pump_protected {
        return Some(INELIGIBLE_PUMP_PROTECTED);
    }
    if !writable {
        return Some(INELIGIBLE_READ_ONLY);
    }
    if !has_tach {
        return Some(INELIGIBLE_NO_TACH);
    }
    match role {
        HeaderRole::ChassisFan | HeaderRole::RadiatorFan => None,
        HeaderRole::CpuFan => Some(INELIGIBLE_CPU_FAN),
        HeaderRole::Pump => Some(INELIGIBLE_PUMP_PROTECTED),
        HeaderRole::Unknown => Some(INELIGIBLE_ROLE_UNKNOWN),
    }
}

/// Words for an ineligibility token.
pub fn ineligibility_detail(token: &str) -> &'static str {
    match token {
        INELIGIBLE_PUMP_PROTECTED => {
            "this header is pump-protected, and a pump is never driven below its floor"
        }
        INELIGIBLE_CPU_FAN => "this header's role is cpu_fan, which the stall probe never tests",
        INELIGIBLE_ROLE_UNKNOWN => {
            "this header's role is unknown; assign chassis_fan or radiator_fan to probe it"
        }
        INELIGIBLE_READ_ONLY => "this header is read-only",
        INELIGIBLE_NO_TACH => "this header has no tach, so a stall cannot be seen",
        _ => "this header is not eligible for the stall probe",
    }
}

/// What the eligibility re-check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eligibility {
    /// `None` when the header may still be probed.
    pub ineligible: Option<&'static str>,
    /// The pump union's answer, reported on its own so the restore floor can
    /// follow a pump assignment made mid-run even when the run is already over.
    pub pump_protected: bool,
}

/// [SAFETY] The hottest FRESH CPU reading, or `None`.
///
/// The same selection the thermal ladder acts on (`hottest_cpu_reading` over
/// `cpu_temp_stale_after`), so the rise gate trusts a reading exactly when the
/// ladder does. A stale reading is `None` here, never a value: the rise gate on
/// a frozen number would pass forever.
pub fn hottest_fresh_cpu_c(cache: &StateCache) -> Option<f64> {
    let stale_after = cache.cpu_temp_stale_after();
    cache.read_with(|s| {
        match crate::profile_engine::hottest_cpu_reading(
            &s.sensors,
            std::time::Instant::now(),
            stale_after,
        ) {
            crate::profile_engine::CpuReading::Fresh(t) => Some(t),
            _ => None,
        }
    })
}

/// The derived timing for one header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeTiming {
    pub refresh: Duration,
    /// How long each probe step is held.
    pub dwell: Duration,
    /// How long a reading must persist to count as two refreshes.
    pub confirm: Duration,
    /// The most time the probe may spend below 20 %.
    pub budget: Duration,
    /// The recovery kick's window.
    pub kick: Duration,
}

/// Probe steps below 20 % in the worst case: every descent step (18 → 0) and
/// every ascent step from a stall at 0 back up to 18. The ascent's 20 % step is
/// not below the floor, so it is not budgeted.
pub const fn worst_case_sub_floor_steps() -> u32 {
    let descent = (constants::STALL_PROBE_START_PCT / constants::STALL_PROBE_STEP_PCT) as u32;
    descent + (descent - 1)
}

/// [SAFETY] Derive the probe's timing from the header's tach refresh (S3-1).
///
/// - `dwell = max(STALL_PROBE_MIN_DWELL, 3 × refresh)`.
/// - `confirm = refresh + one sample interval`. "0 rpm for two refreshes": the
///   first sample showing 0 is the first refresh that says so, and the next
///   refresh lands at most one `refresh` later. The sample interval is the
///   margin for a driver whose refresh jitters around its median.
/// - `budget` is the worst-case walk — every sub-floor step held its full dwell
///   plus one sample of overhead, and the two steps that can run past their
///   dwell to finish a confirmation (the one that stalls and the one that
///   restarts) each doing so. Hard-capped at [`constants::STALL_PROBE_BUDGET_CAP`];
///   a refresh that would need more is [`ABORT_REFRESH_TOO_SLOW`].
/// - `kick = max(STALL_PROBE_MIN_DWELL, 3 × refresh)`, at most
///   [`constants::STALL_PROBE_KICK_MAX`].
pub fn derive_timing(refresh: Duration) -> Result<ProbeTiming, &'static str> {
    if refresh.is_zero() {
        return Err(ABORT_REFRESH_UNKNOWN);
    }
    let sample = constants::CHARACTERIZATION_SAMPLE_INTERVAL;
    let three = refresh * constants::STALL_PROBE_DWELL_REFRESHES;
    let dwell = three.max(constants::STALL_PROBE_MIN_DWELL);
    let confirm = refresh + sample;
    let budget = (dwell + sample) * worst_case_sub_floor_steps() + (confirm + sample) * 2;
    if budget > constants::STALL_PROBE_BUDGET_CAP {
        return Err(ABORT_REFRESH_TOO_SLOW);
    }
    let kick = three
        .max(constants::STALL_PROBE_MIN_DWELL)
        .min(constants::STALL_PROBE_KICK_MAX);
    Ok(ProbeTiming {
        refresh,
        dwell,
        confirm,
        budget,
        kick,
    })
}

/// The descent's duties: 18, 16, … 0.
pub fn descent_duties() -> Vec<u8> {
    (0..constants::STALL_PROBE_START_PCT)
        .rev()
        .filter(|d| d % constants::STALL_PROBE_STEP_PCT == 0)
        .collect()
}

/// The ascent's duties from a stall at `stall_pct`: one step up, then on to
/// 20 % inclusive (S3-9).
pub fn ascent_duties(stall_pct: u8) -> Vec<u8> {
    let step = constants::STALL_PROBE_STEP_PCT;
    (stall_pct.saturating_add(step)..=constants::STALL_PROBE_START_PCT)
        .step_by(step as usize)
        .collect()
}

// ── The loop ─────────────────────────────────────────────────────────

/// Why the loop stopped before an outcome.
#[derive(Debug, Clone, PartialEq)]
enum Stop {
    ShuttingDown,
    Cancelled,
    Thermal(ThermalRefusal, String),
    Rise {
        start_c: f64,
        now_c: f64,
    },
    NoCpuTemperature,
    Superseded,
    EligibilityLost(&'static str),
    Budget(Duration),
    Reclaimed {
        at_pct: u8,
        pwm_enable: u8,
    },
    WriteFailed {
        pct: u8,
        error: String,
    },
    RefreshUnknown,
    RefreshTooSlow(u64),
    /// A probe sample could not be read, or a read did not return at all
    /// within [`constants::DIAGNOSTIC_READ_BUDGET`] (`wedged`). Unknown is
    /// never a pass: without a reading a stall or restart cannot be judged, and
    /// walking on regardless would report a result nothing measured.
    TachUnreadable {
        wedged: bool,
    },
}

impl Stop {
    fn token(&self) -> &'static str {
        match self {
            Stop::ShuttingDown => ABORT_SHUTTING_DOWN,
            Stop::Cancelled => OUTCOME_CANCELLED,
            Stop::Thermal(which, _) => which.token(),
            Stop::Rise { .. } => ABORT_THERMAL_RISE,
            Stop::NoCpuTemperature => ABORT_NO_CPU_TEMPERATURE,
            Stop::Superseded => ABORT_SUPERSEDED,
            Stop::EligibilityLost(_) => ABORT_ELIGIBILITY_LOST,
            Stop::Budget(_) => ABORT_BUDGET_EXCEEDED,
            Stop::Reclaimed { .. } => ABORT_RECLAIMED,
            Stop::WriteFailed { .. } => ABORT_WRITE_FAILED,
            Stop::RefreshUnknown => ABORT_REFRESH_UNKNOWN,
            Stop::RefreshTooSlow(_) => ABORT_REFRESH_TOO_SLOW,
            Stop::TachUnreadable { .. } => ABORT_TACH_UNREADABLE,
        }
    }

    fn detail(&self) -> String {
        match self {
            Stop::ShuttingDown => "the daemon is shutting down".into(),
            Stop::Cancelled => "cancelled".into(),
            Stop::Thermal(_, d) => d.clone(),
            Stop::Rise { start_c, now_c } => format!(
                "the hottest CPU reading rose from {start_c:.1}°C to {now_c:.1}°C, more than \
                 the {:.0}°C the stall probe allows",
                constants::STALL_PROBE_RISE_LIMIT_C
            ),
            Stop::NoCpuTemperature => "no fresh CPU temperature reading, so the rise gate \
                                       cannot be evaluated"
                .into(),
            Stop::Superseded => "superseded by a later diagnostic; this run's lease is gone".into(),
            Stop::EligibilityLost(t) => {
                format!(
                    "the header stopped being eligible: {}",
                    ineligibility_detail(t)
                )
            }
            Stop::Budget(b) => format!(
                "the probe spent longer than its {} ms budget below {}%",
                b.as_millis(),
                constants::STALL_PROBE_START_PCT
            ),
            Stop::Reclaimed { at_pct, pwm_enable } => format!(
                "another controller reclaimed the header at {at_pct}% (pwm_enable={pwm_enable})"
            ),
            Stop::WriteFailed { pct, error } => format!("PWM write of {pct}% failed: {error}"),
            Stop::RefreshUnknown => "the tach refresh could not be measured at 20% and the \
                                     driver publishes no update_interval"
                .into(),
            Stop::RefreshTooSlow(ms) => format!(
                "the tach refreshes every {ms} ms, too slowly for the probe to fit its {} s cap",
                constants::STALL_PROBE_BUDGET_CAP.as_secs()
            ),
            Stop::TachUnreadable { wedged: true } => format!(
                "a read of the header did not return within {} ms; the probe stopped reading it",
                constants::DIAGNOSTIC_READ_BUDGET.as_millis()
            ),
            Stop::TachUnreadable { wedged: false } => "the tach could not be read during the \
                                                       probe, so a stall or restart cannot be \
                                                       judged"
                .into(),
        }
    }

    /// [SAFETY] Whether this stop ends with the 100 % recovery kick — every
    /// stop except shutting down (S3-7 as written; S3-R3). While shutting down
    /// the exit floor and the hand-back own the header, and a write after the
    /// hand-back is the DEC-290 hazard. Under a ladder force or after a lost
    /// lease the kick is still attempted: its write either raises the duty,
    /// which can lower no floor, or fails harmlessly because the ladder or the
    /// successor holds the lease.
    fn kicks(&self) -> bool {
        !matches!(self, Stop::ShuttingDown)
    }
}

/// What a hold watches for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// The baseline: end early once settled on register updates.
    Settle,
    /// 0 rpm across the confirm window.
    Stall,
    /// rpm > 0 across the confirm window.
    Spin,
}

/// Which gates a hold's samples are checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Every probe gate on every sample (S3-3, S3-R2), and an unreadable
    /// sample ends the run.
    Probe,
    /// The recovery kick is itself the safe action, so only what makes a write
    /// wrong can end it: shutdown, a ladder force, a lost lease, a reclaim. An
    /// unreadable sample only breaks a confirmation.
    Kick,
}

struct Held {
    samples: Vec<RpmSample>,
    confirmed_at_ms: Option<u64>,
    last: Option<HwmonVerifyState>,
    held_ms: u64,
    zero_samples: u32,
}

impl Held {
    fn empty(last: Option<HwmonVerifyState>) -> Self {
        Held {
            samples: vec![],
            confirmed_at_ms: None,
            last,
            held_ms: 0,
            zero_samples: 0,
        }
    }
}

const UNREAD: HwmonVerifyState = HwmonVerifyState {
    pwm_enable: None,
    pwm_raw: None,
    pwm_percent: None,
    rpm: None,
};

struct Probe<'a, W, R, E, S, K, P> {
    cache: &'a StateCache,
    write_fn: &'a W,
    read_fn: &'a R,
    eligible: &'a E,
    cancel: &'a AtomicBool,
    shutting_down: &'a S,
    keepalive: &'a K,
    wrote_any: &'a AtomicBool,
    publish: P,
    start_temp_c: f64,
    last_renew: tokio::time::Instant,
    below_since: Option<tokio::time::Instant>,
    below_accum: Duration,
    timing: Option<ProbeTiming>,
    pump_seen: bool,
    /// A read never returned. `spawn_blocking` cannot be cancelled, so from here
    /// on the header is not read again — each new read would park another thread
    /// on the same wedged driver (DEC-289).
    wedged: bool,
    step: u16,
    baseline_samples: Vec<RpmSample>,
    res: ProbeResult,
}

impl<W, R, Fut, E, S, K, P> Probe<'_, W, R, E, S, K, P>
where
    W: Fn(u8) -> Result<(), String>,
    R: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<HwmonVerifyState>>,
    E: Fn() -> Eligibility,
    S: Fn() -> bool,
    K: Fn() -> bool,
    P: FnMut(&ProbeResult),
{
    fn below_floor(&self) -> Duration {
        self.below_accum + self.below_since.map(|t| t.elapsed()).unwrap_or_default()
    }

    fn close_below_floor(&mut self) {
        if let Some(t) = self.below_since.take() {
            self.below_accum += t.elapsed();
        }
        self.res.time_below_floor_ms = self.below_accum.as_millis() as u64;
    }

    fn publish(&mut self) {
        self.res.time_below_floor_ms = self.below_floor().as_millis() as u64;
        (self.publish)(&self.res);
    }

    /// One read of the header. `None` is a read that did not return within the
    /// caller's bound — the header is then marked wedged and never read again.
    async fn read(&mut self) -> Option<HwmonVerifyState> {
        if self.wedged {
            return None;
        }
        let s = (self.read_fn)().await;
        if s.is_none() {
            self.wedged = true;
        }
        s
    }

    /// [SAFETY] The rise gate: the hottest FRESH CPU reading against its value
    /// at start. No fresh reading is itself a stop — a frozen value would pass
    /// the comparison forever.
    fn rise_gate(&mut self) -> Result<(), Stop> {
        let Some(now_c) = hottest_fresh_cpu_c(self.cache) else {
            return Err(Stop::NoCpuTemperature);
        };
        let max = self.res.max_cpu_temp_c.map_or(now_c, |m| m.max(now_c));
        self.res.max_cpu_temp_c = Some(max);
        if now_c > self.start_temp_c + constants::STALL_PROBE_RISE_LIMIT_C {
            return Err(Stop::Rise {
                start_c: self.start_temp_c,
                now_c,
            });
        }
        Ok(())
    }

    /// [SAFETY] The time-below-the-floor budget (S3-1). Unset until the
    /// baseline has measured the refresh, i.e. before any sub-floor write.
    fn budget_gate(&self) -> Result<(), Stop> {
        match self.timing {
            Some(t) if self.below_floor() > t.budget => Err(Stop::Budget(t.budget)),
            _ => Ok(()),
        }
    }

    /// [SAFETY] The eligibility re-check — the pump union first — before every
    /// write and on every probe sample (S3-R2). A pump answer is remembered even
    /// when the run is already stopping, because it raises the restore floor.
    fn eligibility_gate(&mut self) -> Result<(), Stop> {
        let e = (self.eligible)();
        self.pump_seen |= e.pump_protected;
        match e.ineligible {
            Some(t) => Err(Stop::EligibilityLost(t)),
            None => Ok(()),
        }
    }

    /// [SAFETY] Every gate before a write: the shared step gate (shutdown,
    /// cancel, the three thermal gates, keepalive), then the rise gate, the
    /// budget and the eligibility re-check. Nothing here awaits, so the
    /// eligibility answer is the one in force when the write is issued.
    fn before_write(&mut self) -> Result<(), Stop> {
        step_gate(
            self.cache,
            STALL_PROBE_DIAGNOSTIC,
            "the stall probe",
            "cannot write",
            self.shutting_down,
            self.cancel,
            self.keepalive,
        )
        .map_err(|g| match g {
            GateStop::ShuttingDown => Stop::ShuttingDown,
            GateStop::Cancelled => Stop::Cancelled,
            GateStop::Thermal(which, d) => Stop::Thermal(which, d),
            GateStop::Superseded => Stop::Superseded,
        })?;
        self.last_renew = tokio::time::Instant::now();
        self.rise_gate()?;
        // The budget before every write too, not only inside holds: between a
        // hold's last sample and the next write sit two sysfs reads, and no new
        // sub-floor duty may be written once the budget is spent.
        self.budget_gate()?;
        self.eligibility_gate()
    }

    /// [SAFETY] The per-sample gates of a hold.
    fn sample_gates(&mut self, mode: Mode) -> Result<(), Stop> {
        if (self.shutting_down)() {
            return Err(Stop::ShuttingDown);
        }
        let renew_due =
            self.last_renew.elapsed() >= Duration::from_secs(constants::STABILITY_RENEW_INTERVAL_S);
        match mode {
            Mode::Kick => {
                if let Some(state) = crate::api::calibration::thermal_force_state(self.cache) {
                    return Err(Stop::Thermal(
                        ThermalRefusal::Forcing,
                        format!("thermal safety is forcing fan output ({state})"),
                    ));
                }
            }
            Mode::Probe => {
                if self.cancel.load(Ordering::SeqCst) {
                    return Err(Stop::Cancelled);
                }
                if let Some((which, d)) = thermal_refusal(
                    self.cache,
                    STALL_PROBE_DIAGNOSTIC,
                    "the stall probe",
                    "cannot continue",
                ) {
                    return Err(Stop::Thermal(which, d));
                }
                self.rise_gate()?;
                self.budget_gate()?;
                self.eligibility_gate()?;
            }
        }
        // DEC-296 / DEC-334: renew inside the hold on its own cadence, so no hold
        // length can outrun the lease TTL or the engine-pause deadman.
        if renew_due {
            if !(self.keepalive)() {
                return Err(Stop::Superseded);
            }
            self.last_renew = tokio::time::Instant::now();
        }
        Ok(())
    }

    /// Write one duty and account for time below the floor.
    ///
    /// [SAFETY] Shutdown is checked here, immediately before the write — the
    /// kick's included — and not only by the gates that precede it. The task is
    /// detached, and a write landing after `hand_back_hwmon` would re-assert
    /// `pwm_enable=1` through `set_pwm`'s reclaim watchdog on a header nothing
    /// will drive again (DEC-290); at a probe duty that can be a stopped fan.
    ///
    /// The header is counted as moved BEFORE the call, because `set_pwm` can
    /// write sysfs and then fail its readback — the `AUD2-c` direction.
    fn write(&mut self, pct: u8) -> Result<(), Stop> {
        if (self.shutting_down)() {
            return Err(Stop::ShuttingDown);
        }
        self.wrote_any.store(true, Ordering::SeqCst);
        if pct < constants::STALL_PROBE_START_PCT {
            if self.below_since.is_none() {
                self.below_since = Some(tokio::time::Instant::now());
            }
            self.res.lowest_commanded_pct =
                Some(self.res.lowest_commanded_pct.map_or(pct, |l| l.min(pct)));
        } else {
            self.close_below_floor();
        }
        (self.write_fn)(pct).map_err(|error| Stop::WriteFailed { pct, error })
    }

    #[allow(clippy::too_many_arguments)]
    async fn hold(
        &mut self,
        max: Duration,
        watch: Watch,
        confirm: Option<Duration>,
        reference: Option<u16>,
        commanded: u8,
        mode: Mode,
    ) -> Result<Held, (Stop, Held)> {
        let started = tokio::time::Instant::now();
        let mut held = Held::empty(None);
        let mut streak_from: Option<u64> = None;
        loop {
            let elapsed = started.elapsed();
            // A confirmation already under way may finish past the dwell, by at
            // most one confirm window — never an open-ended extension.
            let limit = match (streak_from, confirm) {
                (Some(_), Some(c)) if watch != Watch::Settle => max + c,
                _ => max,
            };
            if elapsed >= limit {
                break;
            }
            tokio::time::sleep((limit - elapsed).min(constants::CHARACTERIZATION_SAMPLE_INTERVAL))
                .await;
            held.held_ms = started.elapsed().as_millis() as u64;
            if let Err(stop) = self.sample_gates(mode) {
                return Err((stop, held));
            }
            let Some(s) = self.read().await else {
                held.held_ms = started.elapsed().as_millis() as u64;
                if mode == Mode::Kick {
                    // A wedge during the kick does not end it: 100 % is still
                    // the right duty. It is held for its window, unobserved.
                    continue;
                }
                return Err((Stop::TachUnreadable { wedged: true }, held));
            };
            held.held_ms = started.elapsed().as_millis() as u64;
            let at_ms = held.held_ms;
            held.samples.push(RpmSample { at_ms, rpm: s.rpm });
            if s.rpm == Some(0) {
                held.zero_samples += 1;
            }
            // A reclaim ends the run, with the same full-speed exemption the
            // characterisation sweep applies (DEC-326 / `HOST-a`).
            if let Some(en) = s.pwm_enable {
                if en != 1 && !crate::pwm::is_full_speed_alias(commanded, s.pwm_percent, Some(en)) {
                    held.last = Some(s);
                    // [SAFETY] A mode change seen while shutting down is the
                    // hand-back's own write, not a BIOS reclaim — and a reclaim
                    // kicks, which would re-take the header it just gave back.
                    let stop = if (self.shutting_down)() {
                        Stop::ShuttingDown
                    } else {
                        Stop::Reclaimed {
                            at_pct: commanded,
                            pwm_enable: en,
                        }
                    };
                    return Err((stop, held));
                }
            }
            if s.rpm.is_none() && mode == Mode::Probe {
                held.last = Some(s);
                return Err((Stop::TachUnreadable { wedged: false }, held));
            }
            let hit = match watch {
                Watch::Settle => false,
                Watch::Stall => s.rpm == Some(0),
                Watch::Spin => matches!(s.rpm, Some(r) if r > 0),
            };
            held.last = Some(s);
            if watch == Watch::Settle {
                if stats::settled_on_updates(&held.samples, reference).is_some() {
                    break;
                }
                continue;
            }
            // In the kick an unreadable sample breaks a streak: it is neither
            // evidence of a stop nor of a spin.
            if hit {
                let from = *streak_from.get_or_insert(at_ms);
                if let Some(c) = confirm {
                    if at_ms.saturating_sub(from) >= c.as_millis() as u64 {
                        held.confirmed_at_ms = Some(at_ms);
                        break;
                    }
                }
            } else {
                streak_from = None;
            }
        }
        Ok(held)
    }

    fn push_point(
        &mut self,
        phase: &str,
        pct: u8,
        accepted: bool,
        before: &HwmonVerifyState,
        held: &Held,
        observation: &str,
    ) {
        let last = held.last.clone().unwrap_or(UNREAD);
        self.res.points.push(ProbePoint {
            phase: phase.to_string(),
            step_index: self.step,
            commanded_pct: pct,
            command_accepted: accepted,
            readback_pct: last.pwm_percent,
            pwm_enable: last.pwm_enable,
            rpm_before: before.rpm,
            rpm_after: last.rpm,
            held_ms: held.held_ms,
            confirmed_at_ms: held.confirmed_at_ms,
            samples: held.samples.len() as u32,
            zero_samples: held.zero_samples,
            observation: observation.to_string(),
        });
        self.step = self.step.saturating_add(1);
        self.publish();
    }

    /// One probe step: read, gate, write, hold, record. `Ok(confirmed)`.
    async fn step(
        &mut self,
        phase: &str,
        pct: u8,
        max: Duration,
        watch: Watch,
    ) -> Result<bool, Stop> {
        // The reference read FIRST, then every gate, then the write — so the
        // eligibility and shutdown checks are the last things before the write,
        // with no await between them. Gated first and read second, a pump
        // assignment or a shutdown landing during this read would not be seen
        // before the write it should have stopped.
        let Some(before) = self.read().await else {
            return Err(Stop::TachUnreadable { wedged: true });
        };
        self.before_write()?;
        if let Err(stop) = self.write(pct) {
            if matches!(stop, Stop::WriteFailed { .. }) {
                let held = Held::empty(Some(before.clone()));
                self.push_point(phase, pct, false, &before, &held, OBS_INTERRUPTED);
            }
            return Err(stop);
        }
        let confirm = self.timing.map(|t| t.confirm);
        match self
            .hold(max, watch, confirm, before.rpm, pct, Mode::Probe)
            .await
        {
            Ok(held) => {
                let confirmed = held.confirmed_at_ms.is_some();
                let rpm = held.last.as_ref().and_then(|s| s.rpm);
                let observation = match (watch, confirmed, rpm) {
                    (_, _, None) => OBS_UNREADABLE,
                    (Watch::Settle, _, Some(0)) => OBS_NO_FAN,
                    (Watch::Settle, _, Some(_)) => OBS_SPINNING,
                    (Watch::Stall, true, _) => OBS_STALLED,
                    (Watch::Stall, false, Some(0)) => OBS_UNCONFIRMED,
                    (Watch::Stall, false, Some(_)) => OBS_SPINNING,
                    (Watch::Spin, true, _) => OBS_RESTARTED,
                    (Watch::Spin, false, Some(0)) => OBS_STOPPED,
                    (Watch::Spin, false, Some(_)) => OBS_UNCONFIRMED,
                };
                if watch == Watch::Settle {
                    self.res.baseline_rpm = rpm;
                    self.res.baseline_settled =
                        Some(stats::settling_ms(&held.samples, before.rpm).is_some());
                    self.baseline_samples = held.samples.clone();
                }
                self.push_point(phase, pct, true, &before, &held, observation);
                Ok(confirmed)
            }
            Err((stop, held)) => {
                self.push_point(phase, pct, true, &before, &held, OBS_INTERRUPTED);
                Err(stop)
            }
        }
    }

    /// Baseline → descent → ascent. `Ok((outcome, kick))` or the stop that
    /// ended it; `kick` says whether the outcome itself ends with the kick.
    async fn body(
        &mut self,
        baseline_max: Duration,
        driver_refresh_ms: Option<u64>,
    ) -> Result<(&'static str, bool), Stop> {
        let start = constants::STALL_PROBE_START_PCT;

        // ── Baseline: 20 %, settled on register updates, refresh measured ──
        self.step(PHASE_BASELINE, start, baseline_max, Watch::Settle)
            .await?;
        let before_rpm = self.res.points.last().and_then(|p| p.rpm_before);
        match self.res.baseline_rpm {
            None => return Err(Stop::TachUnreadable { wedged: false }),
            // [SAFETY] S3-R1. A fan that was spinning before the probe and reads
            // 0 at 20 % stalls at 20 % or above — it is not absent. The restore
            // could put it back below its restart duty and leave it stopped, so
            // it gets the kick. With no reading from before the write, nothing
            // proves a fan is there: `no_fan_detected`, and the kick regardless.
            Some(0) => {
                return Ok(match before_rpm {
                    Some(r) if r > 0 => (OUTCOME_STALLED_AT_OR_ABOVE_20, true),
                    Some(_) => (OUTCOME_NO_FAN_DETECTED, false),
                    None => (OUTCOME_NO_FAN_DETECTED, true),
                });
            }
            Some(_) => {}
        }
        let observed = stats::update_interval_ms(
            &self
                .baseline_samples
                .iter()
                .map(|s| (s.at_ms, s.rpm))
                .collect::<Vec<_>>(),
        );
        let (refresh_ms, source) = match (driver_refresh_ms.filter(|&d| d > 0), observed) {
            (Some(d), _) => (d, REFRESH_DRIVER),
            (None, Some(o)) => (o, REFRESH_OBSERVED),
            (None, None) => return Err(Stop::RefreshUnknown),
        };
        self.res.refresh_ms = Some(refresh_ms);
        self.res.refresh_source = Some(source);
        let timing = match derive_timing(Duration::from_millis(refresh_ms)) {
            Ok(t) => t,
            Err(ABORT_REFRESH_TOO_SLOW) => return Err(Stop::RefreshTooSlow(refresh_ms)),
            Err(_) => return Err(Stop::RefreshUnknown),
        };
        self.timing = Some(timing);
        self.res.dwell_ms = Some(timing.dwell.as_millis() as u64);
        self.res.confirm_ms = Some(timing.confirm.as_millis() as u64);
        self.res.budget_ms = Some(timing.budget.as_millis() as u64);
        self.publish();

        // ── Descent: 18 % → 0 %, until a stall is confirmed ──
        let mut stall = None;
        for d in descent_duties() {
            if self
                .step(PHASE_DESCENT, d, timing.dwell, Watch::Stall)
                .await?
            {
                stall = Some(d);
                break;
            }
        }
        let Some(stall) = stall else {
            return Ok((OUTCOME_NO_STALL_DOWN_TO_0, false));
        };
        self.res.stall_duty_pct = Some(stall);
        self.publish();

        // ── Ascent: one step above the stall, up to 20 % inclusive ──
        for u in ascent_duties(stall) {
            if self
                .step(PHASE_ASCENT, u, timing.dwell, Watch::Spin)
                .await?
            {
                self.res.restart_duty_pct = Some(u);
                return Ok((OUTCOME_STALL_AND_RESTART_FOUND, false));
            }
        }
        Ok((OUTCOME_DID_NOT_RESTART_BELOW_20, true))
    }

    /// [SAFETY] The 100 % recovery kick (S3-7): held until the fan is seen
    /// spinning for the confirm window, bounded by the kick window. A fan whose
    /// last reading is still 0 rpm when the window ends sets
    /// `restart_failed_at_full`. After a wedged read the kick is held for its
    /// window without reading, and claims nothing either way; so does a kick
    /// cut short by a shutdown, a ladder force, a lost lease or a reclaim.
    async fn kick(&mut self) {
        let pct = constants::STALL_PROBE_KICK_PCT;
        // Before the baseline has measured the refresh, the kick uses the
        // slowest cadence the minimum dwell is sized for (a third of it, i.e.
        // the ~2 s it87 register) — the conservative confirm, never a shorter one.
        let (window, confirm) = match self.timing {
            Some(t) => (t.kick, t.confirm),
            None => (
                constants::STALL_PROBE_MIN_DWELL,
                constants::STALL_PROBE_MIN_DWELL / constants::STALL_PROBE_DWELL_REFRESHES
                    + constants::CHARACTERIZATION_SAMPLE_INTERVAL,
            ),
        };
        // [SAFETY] Write FIRST. The kick is the recovery, so nothing — not even
        // the read that would give its `rpm_before` — may delay it; on a slow
        // chip that read is a whole extra sysfs round-trip below the floor. The
        // last reading the probe took stands in for "before".
        let before = self
            .res
            .points
            .last()
            .map(|p| HwmonVerifyState {
                pwm_enable: p.pwm_enable,
                pwm_raw: None,
                pwm_percent: p.readback_pct,
                rpm: p.rpm_after,
            })
            .unwrap_or(UNREAD);
        match self.write(pct) {
            Ok(()) => {}
            // The exit path owns the header now; not even a record of an attempt.
            Err(Stop::ShuttingDown) => return,
            Err(stop) => {
                log::warn!(
                    "stall probe: the {pct}% recovery kick failed: {}",
                    stop.detail()
                );
                let held = Held::empty(Some(before.clone()));
                self.push_point(PHASE_KICK, pct, false, &before, &held, OBS_INTERRUPTED);
                return;
            }
        }
        match self
            .hold(
                window,
                Watch::Spin,
                Some(confirm),
                before.rpm,
                pct,
                Mode::Kick,
            )
            .await
        {
            Ok(held) => {
                let last = held.last.as_ref().and_then(|s| s.rpm);
                let obs = match (held.confirmed_at_ms.is_some(), last) {
                    (true, _) => OBS_RESTARTED,
                    (false, _) if self.wedged => OBS_UNREADABLE,
                    (false, None) => OBS_UNREADABLE,
                    (false, Some(0)) => OBS_STOPPED,
                    (false, Some(_)) => OBS_UNCONFIRMED,
                };
                if obs == OBS_STOPPED {
                    self.res.restart_failed_at_full = true;
                    log::warn!(
                        "stall probe: the fan read 0 rpm for the whole {} ms {pct}% recovery \
                         kick — it may be physically stuck",
                        window.as_millis()
                    );
                }
                self.push_point(PHASE_KICK, pct, true, &before, &held, obs);
            }
            Err((_, held)) => {
                self.push_point(PHASE_KICK, pct, true, &before, &held, OBS_INTERRUPTED);
            }
        }
    }
}

/// [SAFETY] Run the stall/restart probe on one header.
///
/// Generic over every hardware and state touch-point so the whole sequence —
/// each abort path, the kick and the restore — is testable without sysfs, the
/// same shape as `characterization::run_sweep`. `read_fn` resolves as a future
/// so the caller can put the blocking reads on the blocking pool (`P8-am`), and
/// resolves to `None` when a read did not return within the caller's bound
/// ([`constants::DIAGNOSTIC_READ_BUDGET`] in production): a wedge.
///
/// `eligible` is called before every write, on every probe sample, and once
/// more at the end: its pump answer at any point raises the restore floor, so a
/// pump role assigned while the probe ran is never restored below the pump
/// floor (`AUD3-l`'s rule).
#[allow(clippy::too_many_arguments)]
pub async fn run_probe<W, R, Fut, E, S, K, P>(
    cache: &StateCache,
    header_id: &str,
    restore_floor: u8,
    baseline_max: Duration,
    driver_refresh_ms: Option<u64>,
    write_fn: W,
    read_fn: R,
    eligible: E,
    cancel: &AtomicBool,
    shutting_down: S,
    keepalive: K,
    report: &RestoreReport,
    publish: P,
) -> ProbeResult
where
    W: Fn(u8) -> Result<(), String>,
    R: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<HwmonVerifyState>>,
    E: Fn() -> Eligibility,
    S: Fn() -> bool,
    K: Fn() -> bool,
    P: FnMut(&ProbeResult),
{
    let first = read_fn().await;
    let original_pct = first.as_ref().and_then(|s| s.pwm_percent);
    let wrote_any = AtomicBool::new(false);
    let start_temp_c = hottest_fresh_cpu_c(cache);
    let mut probe = Probe {
        cache,
        write_fn: &write_fn,
        read_fn: &read_fn,
        eligible: &eligible,
        cancel,
        shutting_down: &shutting_down,
        keepalive: &keepalive,
        wrote_any: &wrote_any,
        publish,
        start_temp_c: start_temp_c.unwrap_or(f64::NAN),
        last_renew: tokio::time::Instant::now(),
        below_since: None,
        below_accum: Duration::ZERO,
        timing: None,
        pump_seen: false,
        wedged: first.is_none(),
        step: 0,
        baseline_samples: Vec::new(),
        res: ProbeResult {
            state: STATE_RUNNING,
            original_pct,
            start_cpu_temp_c: start_temp_c,
            max_cpu_temp_c: start_temp_c,
            ..Default::default()
        },
    };

    // Declared LAST so it drops FIRST — while the caller's lease guard is still
    // held. Same invariant, same reason, as `characterization::run_sweep`; read
    // `RestoreOnDrop`'s docs before touching this ordering, and keep every new
    // binding above it.
    let mut restore = RestoreOnDrop {
        header_id,
        original_pct,
        write_fn: &write_fn,
        cache,
        shutting_down: &shutting_down,
        wrote_any: &wrote_any,
        report,
        restore_floor,
        // DEC-420's skip-after-a-hung-read is characterisation's; the probe
        // keeps DEC-407's kick and restore after a wedge (`PTR-ab`).
        unresponsive: None,
        // DEC-407's own re-check raises `restore_floor` below; see its field doc.
        pump_watch: None,
    };

    let ended = if probe.wedged {
        // The very first read never returned: nothing has been written, and
        // nothing will be.
        Err(Stop::TachUnreadable { wedged: true })
    } else if start_temp_c.is_none() {
        // Refused before any write: the handler checks this too, but the cache
        // can go stale between the POST and the task.
        Err(Stop::NoCpuTemperature)
    } else {
        probe.body(baseline_max, driver_refresh_ms).await
    };
    // The ending is worked out now and RECORDED only after the kick: the kick
    // publishes its point, and a published result must never carry an ending
    // while the run is still holding 100 % (concurrency F4).
    let (state, outcome, abort_reason, detail, kick) = match ended {
        Ok((outcome, kick)) => (STATE_COMPLETE, outcome, None, None, kick),
        Err(stop) => {
            let cancelled = stop == Stop::Cancelled;
            let state = match stop {
                Stop::Cancelled => STATE_CANCELLED,
                Stop::WriteFailed { .. } => STATE_FAILED,
                _ => STATE_ABORTED,
            };
            let outcome = if cancelled {
                OUTCOME_CANCELLED
            } else {
                OUTCOME_ABORTED
            };
            log::info!("stall probe on {header_id} ended: {}", stop.detail());
            (
                state,
                outcome,
                (!cancelled).then(|| stop.token()),
                Some(stop.detail()),
                stop.kicks(),
            )
        }
    };
    // [SAFETY] Never while shutting down — re-read here, because the stop that
    // ended the run may have been seen before the shutdown began — and never on
    // a header the run never moved.
    if kick && probe.wrote_any.load(Ordering::SeqCst) && !shutting_down() {
        probe.kick().await;
    }
    probe.res.state = state;
    probe.res.outcome = Some(outcome);
    probe.res.abort_reason = abort_reason;
    probe.res.detail = detail;
    probe.close_below_floor();

    // [SAFETY] The final re-check. A pump role assigned after the last sample —
    // during the kick, say — must still raise the restore floor; the guard reads
    // the field when it drops, just below. Skipped while shutting down: the
    // guard skips the restore then, and the lookup takes the controller lock the
    // exit path may be holding.
    let pump_now = probe.pump_seen || (!shutting_down() && (probe.eligible)().pump_protected);
    if pump_now {
        restore.restore_floor = restore
            .restore_floor
            .max(crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8);
    }
    std::mem::take(&mut probe.res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::{CachedSensorReading, DeviceLabel};
    use crate::hwmon::types::SensorKind;
    use std::sync::{Arc, Mutex};

    const ID: &str = "hwmon:nct6798:isa:pwm3:SYS_FAN1";
    const PUMP_FLOOR: u8 = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;

    fn cpu(temp_c: f64) -> CachedSensorReading {
        CachedSensorReading {
            id: "cpu".into(),
            kind: SensorKind::CpuTemp,
            label: "Tctl".into(),
            value_c: temp_c,
            source: DeviceLabel::Hwmon,
            updated_at: std::time::Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }
    }

    fn cache_at(temp_c: f64) -> StateCache {
        let cache = StateCache::new();
        cache.update_sensors(vec![cpu(temp_c)]);
        cache.record_engine_tick("normal", constants::THERMAL_EMERGENCY_TRIGGER_C);
        cache
    }

    /// A fan with real stall/restart hysteresis behind a tach register that
    /// refreshes only every `refresh_ms` — the shape of the 2026-09-08 it87
    /// hardware (stall 6 %, restart 10 %, ~2 s refresh).
    struct FanSim {
        t0: tokio::time::Instant,
        duty: u8,
        spinning: bool,
        /// Stops when the duty falls to this or below. `None`: never stops.
        stall_at: Option<u8>,
        /// Starts again when the duty rises to this or above.
        restart_at: u8,
        refresh_ms: u64,
        /// A tach that always reads 0 — an empty header.
        no_fan: bool,
        pwm_enable: u8,
        reg: Option<(u64, u16)>,
    }

    impl FanSim {
        fn new(stall_at: Option<u8>, restart_at: u8) -> Self {
            FanSim {
                t0: tokio::time::Instant::now(),
                duty: 40,
                spinning: true,
                stall_at,
                restart_at,
                refresh_ms: 2000,
                no_fan: false,
                pwm_enable: 1,
                reg: None,
            }
        }

        fn write(&mut self, d: u8) {
            self.duty = d;
            if self.spinning {
                if matches!(self.stall_at, Some(s) if d <= s) {
                    self.spinning = false;
                }
            } else if d >= self.restart_at {
                self.spinning = true;
            }
        }

        fn read(&mut self) -> HwmonVerifyState {
            let t = self.t0.elapsed().as_millis() as u64;
            let boundary = t / self.refresh_ms * self.refresh_ms;
            let stale = match self.reg {
                Some((b, _)) => b != boundary,
                None => true,
            };
            if stale {
                let v = if self.no_fan || !self.spinning {
                    0
                } else {
                    // A few rpm of jitter per refresh, so each refresh is an
                    // observable register update.
                    300 + u16::from(self.duty) * 20 + ((boundary / self.refresh_ms) % 3) as u16 * 3
                };
                self.reg = Some((boundary, v));
            }
            HwmonVerifyState {
                pwm_enable: Some(self.pwm_enable),
                pwm_raw: Some(((u16::from(self.duty) * 255) / 100) as u8),
                pwm_percent: Some(self.duty),
                rpm: self.reg.map(|(_, v)| v),
            }
        }
    }

    type Hook = Box<dyn Fn(usize, u8)>;

    /// What one read returns, decided by a read hook.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Read {
        Normal,
        /// The read finished, but the tach value could not be parsed.
        Unreadable,
        /// The read never returned within the bound: `None`.
        Wedged,
    }

    /// Sees each read's 1-based number, the fan, and the writes so far.
    type ReadHook = Box<dyn Fn(usize, &mut FanSim, &[u8]) -> Read>;

    struct Rig {
        fan: Mutex<FanSim>,
        writes: Mutex<Vec<u8>>,
        hook: Mutex<Option<Hook>>,
        read_hook: Mutex<Option<ReadHook>>,
        reads: Mutex<usize>,
        read_delay: Duration,
        fail_write_at: Option<u8>,
    }

    impl Rig {
        fn new(fan: FanSim) -> Self {
            Rig {
                fan: Mutex::new(fan),
                writes: Mutex::new(Vec::new()),
                hook: Mutex::new(None),
                read_hook: Mutex::new(None),
                reads: Mutex::new(0),
                read_delay: Duration::ZERO,
                fail_write_at: None,
            }
        }
        fn on_write(&self, h: impl Fn(usize, u8) + 'static) {
            *self.hook.lock().unwrap() = Some(Box::new(h));
        }
        fn on_read(&self, h: impl Fn(usize, &mut FanSim, &[u8]) -> Read + 'static) {
            *self.read_hook.lock().unwrap() = Some(Box::new(h));
        }
        fn written(&self) -> Vec<u8> {
            self.writes.lock().unwrap().clone()
        }
        fn reads(&self) -> usize {
            *self.reads.lock().unwrap()
        }
        /// One read, through the hook.
        fn read(&self) -> Option<HwmonVerifyState> {
            let n = {
                let mut r = self.reads.lock().unwrap();
                *r += 1;
                *r
            };
            let written = self.written();
            let mut fan = self.fan.lock().unwrap();
            let act = match self.read_hook.lock().unwrap().as_ref() {
                Some(h) => h(n, &mut fan, &written),
                None => Read::Normal,
            };
            let mut s = fan.read();
            match act {
                Read::Normal => Some(s),
                Read::Unreadable => {
                    s.rpm = None;
                    Some(s)
                }
                Read::Wedged => None,
            }
        }
    }

    struct Opts<'a> {
        driver_refresh_ms: Option<u64>,
        eligible: &'a dyn Fn() -> Eligibility,
        cancel: &'a AtomicBool,
        shutting_down: &'a dyn Fn() -> bool,
    }

    fn eligible_always() -> Eligibility {
        Eligibility {
            ineligible: None,
            pump_protected: false,
        }
    }

    async fn run(rig: &Rig, cache: &StateCache, o: Opts<'_>) -> (ProbeResult, RestoreReport) {
        let report = RestoreReport::new();
        let res = run_probe(
            cache,
            ID,
            0,
            Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
            o.driver_refresh_ms,
            |p: u8| {
                if rig.fail_write_at == Some(p) {
                    rig.writes.lock().unwrap().push(p);
                    return Err("EIO".into());
                }
                rig.fan.lock().unwrap().write(p);
                let n = {
                    let mut w = rig.writes.lock().unwrap();
                    w.push(p);
                    w.len()
                };
                if let Some(h) = rig.hook.lock().unwrap().as_ref() {
                    h(n, p);
                }
                Ok(())
            },
            || {
                let delay = rig.read_delay;
                let s = rig.read();
                async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    s
                }
            },
            o.eligible,
            o.cancel,
            o.shutting_down,
            || true,
            &report,
            |_: &ProbeResult| {},
        )
        .await;
        (res, report)
    }

    async fn run_default(rig: &Rig, cache: &StateCache) -> (ProbeResult, RestoreReport) {
        let cancel = AtomicBool::new(false);
        run(
            rig,
            cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible_always,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await
    }

    fn kicked(writes: &[u8]) -> bool {
        writes.contains(&constants::STALL_PROBE_KICK_PCT)
    }

    // ── Pure rules ──────────────────────────────────────────────────

    #[test]
    fn the_walk_is_18_to_0_down_and_stall_plus_2_to_20_up() {
        assert_eq!(descent_duties(), vec![18, 16, 14, 12, 10, 8, 6, 4, 2, 0]);
        assert_eq!(ascent_duties(6), vec![8, 10, 12, 14, 16, 18, 20]);
        assert_eq!(ascent_duties(18), vec![20]);
        assert_eq!(ascent_duties(0).first(), Some(&2));
        // The budgeted steps are exactly the sub-floor ones of the worst walk.
        let sub = descent_duties().len() + ascent_duties(0).iter().filter(|&&u| u < 20).count();
        assert_eq!(worst_case_sub_floor_steps() as usize, sub);
    }

    #[test]
    fn timing_is_derived_from_the_refresh_and_capped() {
        let t = derive_timing(Duration::from_millis(2000)).unwrap();
        assert_eq!(t.dwell, Duration::from_secs(6));
        assert_eq!(t.confirm, Duration::from_millis(2500));
        assert_eq!(t.kick, Duration::from_secs(6));
        let steps = worst_case_sub_floor_steps();
        assert_eq!(
            t.budget,
            (t.dwell + constants::CHARACTERIZATION_SAMPLE_INTERVAL) * steps
                + (t.confirm + constants::CHARACTERIZATION_SAMPLE_INTERVAL) * 2
        );
        assert!(t.budget <= constants::STALL_PROBE_BUDGET_CAP);
        // A slow register lengthens the dwell with it …
        let slow = derive_timing(Duration::from_millis(2800)).unwrap();
        assert_eq!(slow.dwell, Duration::from_millis(8400));
        // … until the worst-case walk no longer fits the cap.
        assert_eq!(
            derive_timing(Duration::from_millis(4000)),
            Err(ABORT_REFRESH_TOO_SLOW)
        );
        assert_eq!(derive_timing(Duration::ZERO), Err(ABORT_REFRESH_UNKNOWN));
        // The kick window never exceeds its bound.
        assert!(slow.kick <= constants::STALL_PROBE_KICK_MAX);
    }

    #[test]
    fn eligibility_is_the_pump_union_first_then_the_display_role() {
        use HeaderRole::*;
        // The pump union outranks a role that reads eligible (DEC-312).
        assert_eq!(
            ineligibility(ChassisFan, true, true, true),
            Some(INELIGIBLE_PUMP_PROTECTED)
        );
        assert_eq!(ineligibility(ChassisFan, false, true, true), None);
        assert_eq!(ineligibility(RadiatorFan, false, true, true), None);
        assert_eq!(
            ineligibility(CpuFan, false, true, true),
            Some(INELIGIBLE_CPU_FAN)
        );
        assert_eq!(
            ineligibility(Pump, false, true, true),
            Some(INELIGIBLE_PUMP_PROTECTED)
        );
        assert_eq!(
            ineligibility(Unknown, false, true, true),
            Some(INELIGIBLE_ROLE_UNKNOWN)
        );
        assert_eq!(
            ineligibility(ChassisFan, false, false, true),
            Some(INELIGIBLE_READ_ONLY)
        );
        assert_eq!(
            ineligibility(ChassisFan, false, true, false),
            Some(INELIGIBLE_NO_TACH)
        );
    }

    // ── The loop ────────────────────────────────────────────────────

    /// The reference hardware: stall at 6 %, restart at 10 %, 2 s refresh.
    #[tokio::test(start_paused = true)]
    async fn a_fan_with_hysteresis_reports_its_stall_and_restart_duties() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = cache_at(45.0);
        let (res, report) = run_default(&rig, &cache).await;

        assert_eq!(res.state, STATE_COMPLETE);
        assert_eq!(res.outcome, Some(OUTCOME_STALL_AND_RESTART_FOUND));
        assert_eq!(res.stall_duty_pct, Some(6));
        assert_eq!(res.restart_duty_pct, Some(10));
        assert_eq!(res.lowest_commanded_pct, Some(6));
        assert_eq!(res.refresh_ms, Some(2000));
        assert_eq!(res.refresh_source, Some(REFRESH_OBSERVED));
        assert_eq!(res.dwell_ms, Some(6000));
        // The walk, then the restore to the pre-probe duty — and no kick.
        assert_eq!(rig.written(), vec![20, 18, 16, 14, 12, 10, 8, 6, 8, 10, 40]);
        assert_eq!(report.get().token(), "restored");
        // The stalled step ended early, once confirmed; a spinning one did not.
        let stalled = res
            .points
            .iter()
            .find(|p| p.observation == OBS_STALLED)
            .unwrap();
        assert_eq!(stalled.commanded_pct, 6);
        assert!(stalled.confirmed_at_ms.unwrap() < 6000);
        let restarted = res
            .points
            .iter()
            .find(|p| p.observation == OBS_RESTARTED)
            .unwrap();
        assert_eq!(restarted.commanded_pct, 10);
        assert!(res.points.iter().any(|p| p.commanded_pct == 8
            && p.phase == PHASE_ASCENT
            && p.observation == OBS_STOPPED));
        assert!(res.time_below_floor_ms > 0);
        assert!(res.time_below_floor_ms <= res.budget_ms.unwrap());
    }

    /// The driver's declared cadence outranks the observed one (S3-2).
    #[tokio::test(start_paused = true)]
    async fn a_driver_update_interval_outranks_the_observed_refresh() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = cache_at(45.0);
        let cancel = AtomicBool::new(false);
        let (res, _) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: Some(1000),
                eligible: &eligible_always,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await;
        assert_eq!(res.refresh_ms, Some(1000));
        assert_eq!(res.refresh_source, Some(REFRESH_DRIVER));
        assert_eq!(res.confirm_ms, Some(1500));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fan_that_spins_at_0_reports_no_stall() {
        let rig = Rig::new(FanSim::new(None, 0));
        let cache = cache_at(45.0);
        let (res, _) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_NO_STALL_DOWN_TO_0));
        assert_eq!(res.stall_duty_pct, None);
        assert_eq!(res.lowest_commanded_pct, Some(0));
        let w = rig.written();
        assert_eq!(w.last(), Some(&40), "restored to the pre-probe duty");
        assert!(!kicked(&w));
    }

    /// S3-9: stopping only at 0 % is a stall, and the restart is measured.
    #[tokio::test(start_paused = true)]
    async fn stopping_only_at_0_is_a_stall_and_the_restart_is_measured() {
        let rig = Rig::new(FanSim::new(Some(0), 4));
        let cache = cache_at(45.0);
        let (res, _) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_STALL_AND_RESTART_FOUND));
        assert_eq!(res.stall_duty_pct, Some(0));
        assert_eq!(res.restart_duty_pct, Some(4));
    }

    /// The worst-case walk — a stall at 0 and a restart only at 20 % — fits its
    /// own derived budget. The flat 90 s the plan first proposed did not.
    #[tokio::test(start_paused = true)]
    async fn the_worst_case_walk_fits_its_derived_budget() {
        let rig = Rig::new(FanSim::new(Some(0), 20));
        let cache = cache_at(45.0);
        let (res, _) = run_default(&rig, &cache).await;
        assert_eq!(
            res.outcome,
            Some(OUTCOME_STALL_AND_RESTART_FOUND),
            "{res:?}"
        );
        assert_eq!(res.restart_duty_pct, Some(20));
        assert!(
            res.time_below_floor_ms > 90_000,
            "the case a 90 s budget aborts"
        );
        assert!(res.time_below_floor_ms <= res.budget_ms.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn no_restart_by_20_ends_with_the_kick_and_a_restore() {
        let rig = Rig::new(FanSim::new(Some(6), 50));
        let cache = cache_at(45.0);
        let (res, report) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_DID_NOT_RESTART_BELOW_20));
        assert_eq!(res.stall_duty_pct, Some(6));
        assert_eq!(res.restart_duty_pct, None);
        let w = rig.written();
        assert_eq!(
            &w[w.len() - 3..],
            &[20, 100, 40],
            "20 %, the kick, then the restore"
        );
        assert!(!res.restart_failed_at_full, "the fan restarted at 100 %");
        assert_eq!(report.get().token(), "restored");
    }

    #[tokio::test(start_paused = true)]
    async fn a_fan_stuck_even_at_full_speed_is_flagged() {
        let rig = Rig::new(FanSim::new(Some(6), 255));
        let cache = cache_at(45.0);
        let (res, _) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_DID_NOT_RESTART_BELOW_20));
        assert!(res.restart_failed_at_full);
        let kick = res.points.iter().find(|p| p.phase == PHASE_KICK).unwrap();
        assert_eq!(kick.observation, OBS_STOPPED);
        // Held for the whole kick window, not cut short.
        assert!(kick.held_ms >= constants::STALL_PROBE_MIN_DWELL.as_millis() as u64);
    }

    /// An empty header: nothing below 20 % is ever written.
    #[tokio::test(start_paused = true)]
    async fn no_fan_at_20_writes_nothing_below_it() {
        let mut fan = FanSim::new(Some(6), 10);
        fan.no_fan = true;
        let rig = Rig::new(fan);
        let cache = cache_at(45.0);
        let (res, _) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_NO_FAN_DETECTED));
        assert_eq!(rig.written(), vec![20, 40]);
        assert_eq!(res.lowest_commanded_pct, None);
    }

    /// S3-10: a refresh that cannot be measured refuses in the baseline.
    #[tokio::test(start_paused = true)]
    async fn an_unmeasurable_refresh_refuses_before_any_sub_floor_write() {
        let mut fan = FanSim::new(Some(6), 10);
        fan.refresh_ms = 60_000; // one update in the whole baseline
        let rig = Rig::new(fan);
        let cache = cache_at(45.0);
        let (res, _) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_ABORTED));
        assert_eq!(res.abort_reason, Some(ABORT_REFRESH_UNKNOWN));
        assert!(
            rig.written().iter().all(|&p| p >= 20),
            "{:?}",
            rig.written()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_too_slow_for_the_cap_refuses_before_any_sub_floor_write() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = cache_at(45.0);
        let cancel = AtomicBool::new(false);
        let (res, _) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: Some(4000),
                eligible: &eligible_always,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_REFRESH_TOO_SLOW));
        let w = rig.written();
        assert!(w.iter().all(|&p| p >= 20), "{w:?}");
        assert_eq!(
            w,
            vec![20, 100, 40],
            "baseline, the kick (S3-7), the restore"
        );
    }

    /// [SAFETY] A pump assignment made mid-probe aborts before the NEXT
    /// sub-floor write, and the restore is raised to the pump floor. Asserted
    /// across the whole window: no duty below 20 % after the flip, ever.
    #[tokio::test(start_paused = true)]
    async fn a_mid_probe_pump_assignment_aborts_before_the_next_sub_floor_write() {
        let mut fan = FanSim::new(Some(6), 10);
        fan.duty = 10; // the pre-probe duty, below the pump floor
        let rig = Rig::new(fan);
        let cache = cache_at(45.0);
        let pump = Arc::new(AtomicBool::new(false));
        let flip = pump.clone();
        // The user assigns `pump` while the probe holds 14 %.
        rig.on_write(move |_, p| {
            if p == 14 {
                flip.store(true, Ordering::SeqCst);
            }
        });
        let eligible = move || {
            let p = pump.load(Ordering::SeqCst);
            Eligibility {
                ineligible: p.then_some(INELIGIBLE_PUMP_PROTECTED),
                pump_protected: p,
            }
        };
        let cancel = AtomicBool::new(false);
        let (res, report) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_ELIGIBILITY_LOST));
        let w = rig.written();
        let flip_at = w
            .iter()
            .position(|&p| p == 14)
            .expect("the flip write happened");
        assert!(
            w[flip_at + 1..].iter().all(|&p| p >= PUMP_FLOOR),
            "a duty below the pump floor after the pump assignment: {w:?}"
        );
        assert_eq!(
            &w[flip_at + 1..],
            &[100, PUMP_FLOOR],
            "kick, then a floored restore"
        );
        assert_eq!(report.get().token(), "restored");
    }

    /// [SAFETY] The FINAL re-check, on its own: a pump assigned during the kick
    /// — after the last probe write, so no `before_write` can see it — still
    /// raises the restore to the pump floor.
    #[tokio::test(start_paused = true)]
    async fn a_pump_assigned_during_the_kick_still_floors_the_restore() {
        let mut fan = FanSim::new(Some(6), 50);
        fan.duty = 10;
        let rig = Rig::new(fan);
        let cache = cache_at(45.0);
        let pump = Arc::new(AtomicBool::new(false));
        let flip = pump.clone();
        rig.on_write(move |_, p| {
            if p == constants::STALL_PROBE_KICK_PCT {
                flip.store(true, Ordering::SeqCst);
            }
        });
        let eligible = move || {
            let p = pump.load(Ordering::SeqCst);
            Eligibility {
                ineligible: p.then_some(INELIGIBLE_PUMP_PROTECTED),
                pump_protected: p,
            }
        };
        let cancel = AtomicBool::new(false);
        let (res, _) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await;
        assert_eq!(res.outcome, Some(OUTCOME_DID_NOT_RESTART_BELOW_20));
        let w = rig.written();
        assert_eq!(&w[w.len() - 2..], &[100, PUMP_FLOOR], "{w:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_5c_rise_aborts_with_the_kick_then_the_restore() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = Arc::new(cache_at(45.0));
        let hot = cache.clone();
        rig.on_write(move |_, p| {
            if p == 12 {
                hot.update_sensors(vec![cpu(50.5)]);
            }
        });
        let (res, report) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_ABORTED));
        assert_eq!(res.abort_reason, Some(ABORT_THERMAL_RISE));
        assert_eq!(res.max_cpu_temp_c, Some(50.5));
        let w = rig.written();
        assert_eq!(&w[w.len() - 3..], &[12, 100, 40]);
        assert_eq!(report.get().token(), "restored");
    }

    #[tokio::test(start_paused = true)]
    async fn a_rise_within_the_limit_does_not_abort() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = Arc::new(cache_at(45.0));
        let warm = cache.clone();
        rig.on_write(move |_, p| {
            if p == 12 {
                warm.update_sensors(vec![cpu(49.5)]);
            }
        });
        let (res, _) = run_default(&rig, &cache).await;
        assert_eq!(
            res.outcome,
            Some(OUTCOME_STALL_AND_RESTART_FOUND),
            "{res:?}"
        );
    }

    /// A cancel while the fan is stopped ends with the kick, not a bare restore
    /// that could leave it stopped below its restart duty.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_while_stalled_gives_the_kick() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = cache_at(45.0);
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        // Cancel on the first ascent write, i.e. while the fan is stopped.
        rig.on_write(move |n, p| {
            if n > 8 && p == 8 {
                c.store(true, Ordering::SeqCst);
            }
        });
        let (res, report) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible_always,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await;
        assert_eq!(res.state, STATE_CANCELLED);
        assert_eq!(res.outcome, Some(OUTCOME_CANCELLED));
        assert_eq!(res.abort_reason, None);
        let w = rig.written();
        assert_eq!(&w[w.len() - 3..], &[8, 100, 40]);
        assert_eq!(report.get().token(), "restored");
        assert!(!res.restart_failed_at_full);
        // Honoured on the NEXT SAMPLE, not at the next write: the held step is
        // cut short within one sample interval rather than waiting out its dwell.
        let cut = res
            .points
            .iter()
            .rev()
            .find(|p| p.phase == PHASE_ASCENT)
            .expect("the ascent step being held");
        assert_eq!(cut.observation, OBS_INTERRUPTED);
        assert!(
            cut.held_ms <= constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64,
            "a cancel waited {} ms",
            cut.held_ms
        );
    }

    /// S3-R3: under a ladder force the kick is still attempted — it can lower no
    /// floor, and where the ladder holds the lease it simply fails — but the
    /// restore is skipped, because the ladder owns the header.
    #[tokio::test(start_paused = true)]
    async fn a_ladder_force_still_gets_the_kick_but_no_restore() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = Arc::new(cache_at(45.0));
        let hot = cache.clone();
        rig.on_write(move |_, p| {
            if p == 12 {
                hot.record_engine_tick("emergency", constants::THERMAL_EMERGENCY_TRIGGER_C);
            }
        });
        let (res, report) = run_default(&rig, &cache).await;
        assert_eq!(res.abort_reason, Some("thermal_force"));
        let w = rig.written();
        assert_eq!(
            &w[w.len() - 2..],
            &[12, 100],
            "the kick, and no restore: {w:?}"
        );
        assert_eq!(report.get().token(), "skipped_thermal_force");
    }

    /// Shutting down: stop touching the header, no kick, and the restore is
    /// skipped (the exit path owns it).
    #[tokio::test(start_paused = true)]
    async fn shutting_down_mid_probe_writes_nothing_more() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = cache_at(45.0);
        let down = Arc::new(AtomicBool::new(false));
        let d = down.clone();
        rig.on_write(move |_, p| {
            if p == 14 {
                d.store(true, Ordering::SeqCst);
            }
        });
        let cancel = AtomicBool::new(false);
        let (res, report) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible_always,
                cancel: &cancel,
                shutting_down: &|| down.load(Ordering::SeqCst),
            },
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_SHUTTING_DOWN));
        assert_eq!(rig.written().last(), Some(&14));
        assert_eq!(report.get().token(), "skipped_shutting_down");
    }

    /// The BIOS takes the header back mid-descent: the run ends as `reclaimed`
    /// and still kicks, then restores.
    #[tokio::test(start_paused = true)]
    async fn a_reclaim_mid_probe_aborts_with_the_kick() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = cache_at(45.0);
        let reclaimed = Arc::new(AtomicBool::new(false));
        let flag = reclaimed.clone();
        rig.on_write(move |_, p| {
            if p == 16 {
                flag.store(true, Ordering::SeqCst);
            }
        });
        let cancel = AtomicBool::new(false);
        let report = RestoreReport::new();
        let res = run_probe(
            &cache,
            ID,
            0,
            Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
            None,
            |p: u8| {
                rig.fan.lock().unwrap().write(p);
                rig.writes.lock().unwrap().push(p);
                if let Some(h) = rig.hook.lock().unwrap().as_ref() {
                    h(0, p);
                }
                Ok(())
            },
            || {
                let mut f = rig.fan.lock().unwrap();
                // After the 16 % write the firmware owns the mode — until the
                // kick's own write re-takes it, as `set_pwm`'s watchdog would.
                if reclaimed.load(Ordering::SeqCst) && f.duty == 16 {
                    f.pwm_enable = 2;
                } else {
                    f.pwm_enable = 1;
                }
                std::future::ready(Some(f.read()))
            },
            &eligible_always,
            &cancel,
            &|| false,
            || true,
            &report,
            |_: &ProbeResult| {},
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_RECLAIMED));
        let w = rig.written();
        assert_eq!(&w[w.len() - 3..], &[16, 100, 40]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_write_is_a_failed_run_that_still_kicks_and_restores() {
        let mut rig = Rig::new(FanSim::new(Some(6), 10));
        rig.fail_write_at = Some(12);
        let cache = cache_at(45.0);
        let (res, report) = run_default(&rig, &cache).await;
        assert_eq!(res.state, STATE_FAILED);
        assert_eq!(res.abort_reason, Some(ABORT_WRITE_FAILED));
        let w = rig.written();
        assert_eq!(&w[w.len() - 3..], &[12, 100, 40]);
        assert_eq!(report.get().token(), "restored");
    }

    /// A slow chip: each read takes 800 ms, so every hold overruns its dwell and
    /// the derived budget — sized for the nominal walk — is what stops it.
    #[tokio::test(start_paused = true)]
    async fn the_budget_stops_a_run_that_overruns_it() {
        let mut rig = Rig::new(FanSim::new(Some(0), 20));
        rig.read_delay = Duration::from_millis(800);
        let cache = cache_at(45.0);
        let cancel = AtomicBool::new(false);
        // The driver declares 1 s, so the budget is sized for a fast chip; the
        // reads are slow, so the walk overruns it. (Observed, the refresh would
        // stretch with the reads and the budget with it — which is correct.)
        let (res, _) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: Some(1000),
                eligible: &eligible_always,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_BUDGET_EXCEEDED), "{res:?}");
        let budget = res.budget_ms.unwrap();
        // Stopped, and kicked, within one sample and one slow read of the budget:
        // the gap between two budget checks, and nothing after it.
        let slack = constants::CHARACTERIZATION_SAMPLE_INTERVAL + rig.read_delay;
        assert!(
            res.time_below_floor_ms <= budget + slack.as_millis() as u64,
            "{} > {budget} + {slack:?}",
            res.time_below_floor_ms
        );
        let w = rig.written();
        assert_eq!(&w[w.len() - 2..], &[100, 40]);
    }

    /// S3-R3: a lost lease still gets the kick attempt (it fails harmlessly in
    /// production, where the successor holds the lease).
    #[tokio::test(start_paused = true)]
    async fn a_superseded_run_still_attempts_the_kick() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = cache_at(45.0);
        let alive = Arc::new(AtomicBool::new(true));
        let a = alive.clone();
        rig.on_write(move |_, p| {
            if p == 12 {
                a.store(false, Ordering::SeqCst);
            }
        });
        let cancel = AtomicBool::new(false);
        let report = RestoreReport::new();
        let res = run_probe(
            &cache,
            ID,
            0,
            Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
            None,
            |p: u8| {
                rig.fan.lock().unwrap().write(p);
                rig.writes.lock().unwrap().push(p);
                if let Some(h) = rig.hook.lock().unwrap().as_ref() {
                    h(0, p);
                }
                Ok(())
            },
            || std::future::ready(rig.read()),
            &eligible_always,
            &cancel,
            &|| false,
            || alive.load(Ordering::SeqCst),
            &report,
            |_: &ProbeResult| {},
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_SUPERSEDED));
        assert!(kicked(&rig.written()), "{:?}", rig.written());
    }

    /// S3-R1: a fan spinning before the probe that reads 0 at the 20 % baseline
    /// stalls at 20 % or above. It is NOT absent, and it gets the kick — the
    /// restore alone could leave it stopped below its restart duty.
    #[tokio::test(start_paused = true)]
    async fn a_fan_that_stops_at_the_20_baseline_is_a_stall_and_gets_the_kick() {
        let rig = Rig::new(FanSim::new(Some(20), 30));
        let cache = cache_at(45.0);
        let (res, report) = run_default(&rig, &cache).await;
        assert_eq!(res.state, STATE_COMPLETE);
        assert_eq!(res.outcome, Some(OUTCOME_STALLED_AT_OR_ABOVE_20), "{res:?}");
        assert_eq!(res.stall_duty_pct, None, "only >= 20 % is known");
        assert_eq!(res.restart_duty_pct, None);
        assert_eq!(
            res.lowest_commanded_pct, None,
            "nothing below 20 % was written"
        );
        assert_eq!(rig.written(), vec![20, 100, 40]);
        let kick = res.points.iter().find(|p| p.phase == PHASE_KICK).unwrap();
        assert_eq!(kick.observation, OBS_RESTARTED);
        assert_eq!(report.get().token(), "restored");
    }

    /// S3-R1's other half: with no reading from before the write, nothing proves
    /// a fan is there — `no_fan_detected` — and the kick runs regardless.
    #[tokio::test(start_paused = true)]
    async fn no_fan_with_an_unreadable_prior_reading_still_kicks() {
        let mut fan = FanSim::new(Some(6), 10);
        fan.no_fan = true;
        let rig = Rig::new(fan);
        // Read 2 is the baseline step's pre-write reference read.
        rig.on_read(|n, _, _| {
            if n == 2 {
                Read::Unreadable
            } else {
                Read::Normal
            }
        });
        let cache = cache_at(45.0);
        let (res, _) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_NO_FAN_DETECTED));
        assert_eq!(
            res.points[0].rpm_before, None,
            "precondition: no prior reading"
        );
        assert_eq!(rig.written(), vec![20, 100, 40]);
    }

    /// An unreadable sample in a probe hold is never a pass: the run ends as
    /// `tach_unreadable` and kicks, rather than walking on blind and reporting
    /// a stall or a spin nothing measured.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_sample_ends_the_run_with_the_kick() {
        let rig = Rig::new(FanSim::new(None, 0));
        rig.on_read(|_, _, w| {
            if w.last() == Some(&12) {
                Read::Unreadable
            } else {
                Read::Normal
            }
        });
        let cache = cache_at(45.0);
        let (res, report) = run_default(&rig, &cache).await;
        assert_eq!(res.outcome, Some(OUTCOME_ABORTED), "{res:?}");
        assert_eq!(res.abort_reason, Some(ABORT_TACH_UNREADABLE));
        let w = rig.written();
        assert_eq!(&w[w.len() - 3..], &[12, 100, 40], "{w:?}");
        assert!(
            !w.contains(&10),
            "walked on below the unreadable step: {w:?}"
        );
        assert_eq!(report.get().token(), "restored");
    }

    /// Concurrency F2 / S3-R4: a read that never returns is a wedge. The run
    /// ends, the header is never read again (each read would park another
    /// thread on the wedged driver), and the kick is held unobserved.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_read_ends_the_run_and_the_kick_holds_unobserved() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let wedged_at = Arc::new(Mutex::new(None::<usize>));
        let at = wedged_at.clone();
        rig.on_read(move |n, _, w| {
            if w.last() == Some(&12) {
                at.lock().unwrap().get_or_insert(n);
                Read::Wedged
            } else {
                Read::Normal
            }
        });
        let cache = cache_at(45.0);
        let (res, report) = run_default(&rig, &cache).await;
        assert_eq!(res.abort_reason, Some(ABORT_TACH_UNREADABLE), "{res:?}");
        let wedged_at = wedged_at
            .lock()
            .unwrap()
            .expect("precondition: a read wedged");
        assert_eq!(
            rig.reads(),
            wedged_at,
            "the header was read again after a wedge (DEC-289: each read parks a thread)"
        );
        assert!(res.detail.as_deref().unwrap().contains("did not return"));
        let w = rig.written();
        assert_eq!(&w[w.len() - 3..], &[12, 100, 40], "{w:?}");
        let kick = res.points.iter().find(|p| p.phase == PHASE_KICK).unwrap();
        assert_eq!(kick.samples, 0, "the kick must not read a wedged header");
        assert_eq!(kick.observation, OBS_UNREADABLE);
        assert!(kick.held_ms >= constants::STALL_PROBE_MIN_DWELL.as_millis() as u64);
        assert!(!res.restart_failed_at_full, "a wedge is not a stuck fan");
        // Exactly one read reached the wedged header after the 12 % write.
        let wedged_reads = res
            .points
            .iter()
            .find(|p| p.commanded_pct == 12)
            .map(|p| p.samples)
            .unwrap();
        assert_eq!(wedged_reads, 0, "the wedged read is not a sample");
        // Ended AT the wedged read — not at the end of the step's dwell, with the
        // gates blind for the rest of it.
        let held_12 = res.points.iter().find(|p| p.commanded_pct == 12).unwrap();
        assert!(
            held_12.held_ms <= constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64,
            "the probe kept holding after a wedged read: {} ms",
            held_12.held_ms
        );
        assert_eq!(report.get().token(), "restored");
    }

    /// S3-R2: a pump assigned in the middle of a sub-floor HOLD is honoured on
    /// the next sample, not when the hold ends.
    #[tokio::test(start_paused = true)]
    async fn a_pump_assigned_mid_hold_is_honoured_on_the_next_sample() {
        let mut fan = FanSim::new(Some(6), 10);
        fan.duty = 10;
        let rig = Rig::new(fan);
        let pump = Arc::new(AtomicBool::new(false));
        let flip = pump.clone();
        let since_14 = Arc::new(Mutex::new(0usize));
        let s14 = since_14.clone();
        // The third sample of the 14 % hold: well inside it.
        rig.on_read(move |_, _, w| {
            if w.last() == Some(&14) {
                let mut k = s14.lock().unwrap();
                *k += 1;
                if *k == 3 {
                    flip.store(true, Ordering::SeqCst);
                }
            }
            Read::Normal
        });
        let eligible = move || {
            let p = pump.load(Ordering::SeqCst);
            Eligibility {
                ineligible: p.then_some(INELIGIBLE_PUMP_PROTECTED),
                pump_protected: p,
            }
        };
        let cancel = AtomicBool::new(false);
        let cache = cache_at(45.0);
        let (res, _) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_ELIGIBILITY_LOST));
        let held_14 = res.points.iter().find(|p| p.commanded_pct == 14).unwrap();
        assert_eq!(held_14.observation, OBS_INTERRUPTED);
        assert!(
            held_14.held_ms <= 4 * constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64,
            "a pump assignment waited out the hold: {} ms",
            held_14.held_ms
        );
        let w = rig.written();
        assert_eq!(&w[w.len() - 3..], &[14, 100, PUMP_FLOOR], "{w:?}");
    }

    /// The reference read happens BEFORE the gates, so an assignment landing
    /// during that read is seen before the write — gated first and read second,
    /// the write went through.
    #[tokio::test(start_paused = true)]
    async fn an_assignment_during_the_pre_write_read_blocks_that_write() {
        let mut fan = FanSim::new(Some(6), 10);
        fan.duty = 10;
        let rig = Rig::new(fan);
        let pump = Arc::new(AtomicBool::new(false));
        let flip = pump.clone();
        let since_14 = Arc::new(Mutex::new(0usize));
        let s14 = since_14.clone();
        let hold_samples =
            (6000 / constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis()) as usize;
        // The read after the 14 % hold's last sample is the 12 % step's
        // reference read.
        rig.on_read(move |_, _, w| {
            if w.last() == Some(&14) {
                let mut k = s14.lock().unwrap();
                *k += 1;
                if *k == hold_samples + 1 {
                    flip.store(true, Ordering::SeqCst);
                }
            }
            Read::Normal
        });
        let eligible = move || {
            let p = pump.load(Ordering::SeqCst);
            Eligibility {
                ineligible: p.then_some(INELIGIBLE_PUMP_PROTECTED),
                pump_protected: p,
            }
        };
        let cancel = AtomicBool::new(false);
        let cache = cache_at(45.0);
        let (res, _) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible,
                cancel: &cancel,
                shutting_down: &|| false,
            },
        )
        .await;
        let held_14 = res.points.iter().find(|p| p.commanded_pct == 14).unwrap();
        assert_eq!(
            held_14.samples as usize, hold_samples,
            "precondition: the flip landed on the reference read, not a hold sample"
        );
        assert_eq!(res.abort_reason, Some(ABORT_ELIGIBILITY_LOST));
        let w = rig.written();
        assert!(!w.contains(&12), "the 12 % write went through: {w:?}");
        assert_eq!(&w[w.len() - 3..], &[14, 100, PUMP_FLOOR], "{w:?}");
    }

    /// Concurrency F1: a shutdown that begins during the reference read stops
    /// the write it precedes — no kick, and the restore is the exit path's.
    #[tokio::test(start_paused = true)]
    async fn a_shutdown_during_the_pre_write_read_blocks_that_write() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let down = Arc::new(AtomicBool::new(false));
        let d = down.clone();
        let since_14 = Arc::new(Mutex::new(0usize));
        let s14 = since_14.clone();
        let hold_samples =
            (6000 / constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis()) as usize;
        rig.on_read(move |_, _, w| {
            if w.last() == Some(&14) {
                let mut k = s14.lock().unwrap();
                *k += 1;
                if *k == hold_samples + 1 {
                    d.store(true, Ordering::SeqCst);
                }
            }
            Read::Normal
        });
        let cancel = AtomicBool::new(false);
        let cache = cache_at(45.0);
        let (res, report) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible_always,
                cancel: &cancel,
                shutting_down: &|| down.load(Ordering::SeqCst),
            },
        )
        .await;
        let held_14 = res.points.iter().find(|p| p.commanded_pct == 14).unwrap();
        assert_eq!(held_14.samples as usize, hold_samples, "precondition");
        assert_eq!(res.abort_reason, Some(ABORT_SHUTTING_DOWN));
        assert_eq!(rig.written().last(), Some(&14), "{:?}", rig.written());
        assert_eq!(report.get().token(), "skipped_shutting_down");
    }

    /// Concurrency F1's second half: the hand-back's own mode write, seen while
    /// shutting down, is not a BIOS reclaim — a reclaim kicks, and that kick
    /// would re-take the header the hand-back just gave back.
    #[tokio::test(start_paused = true)]
    async fn a_mode_change_seen_while_shutting_down_is_not_a_reclaim() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let down = Arc::new(AtomicBool::new(false));
        let d = down.clone();
        rig.on_read(move |_, fan, w| {
            if w.last() == Some(&14) && !d.load(Ordering::SeqCst) {
                // The hand-back runs during this read: firmware mode, and the
                // shutdown flag already set.
                fan.pwm_enable = 2;
                d.store(true, Ordering::SeqCst);
            }
            Read::Normal
        });
        let cancel = AtomicBool::new(false);
        let cache = cache_at(45.0);
        let (res, _) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible_always,
                cancel: &cancel,
                shutting_down: &|| down.load(Ordering::SeqCst),
            },
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_SHUTTING_DOWN), "{res:?}");
        assert!(!kicked(&rig.written()), "{:?}", rig.written());
    }

    /// A stop that kicks, seen at the moment a shutdown began: the kick must
    /// still not be written once the flag is up.
    #[tokio::test(start_paused = true)]
    async fn no_kick_is_written_once_shutdown_has_begun() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let down = Arc::new(AtomicBool::new(false));
        let d = down.clone();
        // The eligibility re-check that loses eligibility is also the moment the
        // daemon starts going down.
        let calls = Arc::new(Mutex::new(0usize));
        let c = calls.clone();
        let eligible = move || {
            let mut n = c.lock().unwrap();
            *n += 1;
            if *n == 20 {
                d.store(true, Ordering::SeqCst);
                return Eligibility {
                    ineligible: Some(INELIGIBLE_PUMP_PROTECTED),
                    pump_protected: true,
                };
            }
            eligible_always()
        };
        let cancel = AtomicBool::new(false);
        let cache = cache_at(45.0);
        let (res, report) = run(
            &rig,
            &cache,
            Opts {
                driver_refresh_ms: None,
                eligible: &eligible,
                cancel: &cancel,
                shutting_down: &|| down.load(Ordering::SeqCst),
            },
        )
        .await;
        assert_eq!(res.abort_reason, Some(ABORT_ELIGIBILITY_LOST), "{res:?}");
        assert!(!kicked(&rig.written()), "{:?}", rig.written());
        assert_eq!(report.get().token(), "skipped_shutting_down");
    }

    /// Concurrency F4, at its source: no published result carries an ending while
    /// the run is still going — the kick's own point included. Asserted on EVERY
    /// publish, across the whole run, and for each kind of ending that kicks.
    #[tokio::test(start_paused = true)]
    async fn no_publish_carries_an_ending_before_the_run_returns() {
        for (stall_at, restart_at, cancel_on) in [
            (Some(6), 10, Some(8)), // cancelled while stalled → the kick
            (Some(6), 50, None),    // no restart by 20 → the kick
            (Some(20), 30, None),   // stalled at the baseline → the kick
        ] {
            let rig = Rig::new(FanSim::new(stall_at, restart_at));
            let cancel = Arc::new(AtomicBool::new(false));
            let c = cancel.clone();
            rig.on_write(move |n, p| {
                if cancel_on == Some(p) && n > 8 {
                    c.store(true, Ordering::SeqCst);
                }
            });
            let cache = cache_at(45.0);
            let report = RestoreReport::new();
            let publishes = Mutex::new(0usize);
            let res = run_probe(
                &cache,
                ID,
                0,
                Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
                None,
                |p: u8| {
                    rig.fan.lock().unwrap().write(p);
                    let n = {
                        let mut w = rig.writes.lock().unwrap();
                        w.push(p);
                        w.len()
                    };
                    if let Some(h) = rig.hook.lock().unwrap().as_ref() {
                        h(n, p);
                    }
                    Ok(())
                },
                || std::future::ready(rig.read()),
                &eligible_always,
                &cancel,
                &|| false,
                || true,
                &report,
                |r: &ProbeResult| {
                    *publishes.lock().unwrap() += 1;
                    assert_eq!(r.outcome, None, "published an ending mid-run: {r:?}");
                    assert_eq!(r.abort_reason, None);
                    assert_eq!(r.detail, None);
                    assert_eq!(r.state, STATE_RUNNING);
                },
            )
            .await;
            assert!(
                kicked(&rig.written()),
                "precondition: the run kicked ({res:?})"
            );
            assert!(
                *publishes.lock().unwrap() > 1,
                "precondition: the run published"
            );
            assert!(
                res.outcome.is_some(),
                "the ending is on the RETURNED result"
            );
        }
    }

    /// No fresh CPU reading at start: nothing is written at all.
    #[tokio::test(start_paused = true)]
    async fn no_fresh_cpu_temperature_writes_nothing() {
        let rig = Rig::new(FanSim::new(Some(6), 10));
        let cache = StateCache::new();
        cache.record_engine_tick("normal", constants::THERMAL_EMERGENCY_TRIGGER_C);
        let (res, report) = run_default(&rig, &cache).await;
        assert_eq!(res.abort_reason, Some(ABORT_NO_CPU_TEMPERATURE), "{res:?}");
        // Only the shared guard's restore of the captured duty — which
        // `RestoreOnDrop` performs for every run — and no probe write at all.
        assert_eq!(rig.written(), vec![40]);
        assert_eq!(report.get().token(), "restored");
    }
}
