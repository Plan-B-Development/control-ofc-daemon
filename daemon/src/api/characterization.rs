//! PWM/RPM response characterisation sweep (AIO-MB Phase 3).
//!
//! A deeper diagnostic that sits **alongside** `POST /hwmon/{id}/verify`, not in
//! place of it: verify answers "does a PWM write do anything at all?" in ~6 s at
//! one test duty; this walks a header across several duties and reports what it
//! measured at each, keeping **command acceptance**, **PWM readback** and
//! **physical RPM response** as three independent verdicts.
//!
//! That separation is the point of the feature. A pump whose firmware overrides
//! PWM during its startup/self-bleeding period reports a *correct* readback with
//! RPM pinned high — three collapsed into one PASS/FAIL would call that a write
//! failure, which is exactly the wrong conclusion.
//!
//! # Safety
//!
//! - **0% is unreachable through this module.** [`resolve_points`] clamps every
//!   input into `[max(CHARACTERIZATION_MIN_PCT, floor) .. 100]`; a pump-protected
//!   header's floor is [`crate::profile::HARD_PUMP_CPU_FLOOR_PCT`].
//!
//!   **This claim was false for one duty until `AUD3-l`, and the exception was
//!   the restore.** `resolve_points` governs the duties written on the way *in*;
//!   the restore wrote the captured pre-sweep duty straight through, and the
//!   write path applies no floor of its own. A pump header whose duty read 0 was
//!   therefore swept correctly and then restored to 0 — with `pwm_enable=1`
//!   asserted, which is what turns a firmware-controlled 0 into a stopped pump
//!   that nothing will revise. `RestoreOnDrop::restore_floor` now clamps it.
//!
//!   **So state the claim precisely, because the loose version is what went
//!   wrong:** no *commanded sweep point* is ever 0, for any header; and a
//!   **pump-protected** header is never left below its floor by this module at
//!   all, restore included. A non-pump header's restore may still write 0 — that
//!   is putting the fan back exactly where it was found, which is deliberate and
//!   is asserted by `a_non_pump_header_is_restored_exactly_as_captured`.
//!   `HeaderRole::is_pump()` is `Pump` only, so a **CPU-labelled** header is
//!   outside this clamp even though the engine floors CPU members at the same
//!   30% (`profile::CPU_PUMP_LABEL_HINTS`). That gap is deliberate here and
//!   recorded as `322-b`; it is not an oversight of the clamp's predicate.
//! - **Unidirectional** sweeps are ascending, so an abort part-way leaves the
//!   header *high* rather than low. That is DEC-313 decision 5 and it is
//!   unchanged.
//! - **Bidirectional** sweeps (DEC-334) descend from the top and then climb
//!   back, so the run *ends* at the highest duty and the early part of a long
//!   run sits near maximum. The order was chosen for exactly this reason: the
//!   spec's illustrative rising-then-falling order would have ended every
//!   completed run at the LOWEST duty, and `RestoreOnDrop` has four exits that
//!   leave the header where the sweep put it — the two deliberate skips, an
//!   unreadable pre-sweep duty, and a shutdown whose `restore_hwmon_to_auto`
//!   found no `pwmN_enable` to hand back (`main.rs`, `NothingToRestore` /
//!   `WritesTimedOut` / `Unresolvable`). Ending high keeps all four benign.
//! - The invariant that holds in **both** modes, and the one to reason from:
//!   **no walked duty is ever below `max(CHARACTERIZATION_MIN_PCT, floor)`.**
//! - The pre-sweep duty is restored by [`RestoreOnDrop`] on every exit path on
//!   which nothing else owns the header — completion, cancellation, a failed
//!   write, a reclaim, and a thermal abort below the forcing threshold. (The
//!   same narrowing DEC-295 applied to DEC-134's identical claim for calibrate.)
//! - The restore is skipped while the thermal ladder is forcing (DEC-295) and
//!   while the daemon is shutting down (DEC-290) — in both cases something with
//!   more authority owns the header.
//! - **A skipped restore is reported, not silently reported as a success.**
//!   [`RestoreOutcome`] records which of the five exits the guard actually took,
//!   and `restore_failed` is derived from it, so "the header is back where it
//!   was" is answerable from the wire on every path.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::calibration::{check_thermal_safety, thermal_force_state};
use crate::api::responses::HwmonVerifyState;
use crate::constants;
use crate::health::cache::StateCache;

// ── Wire types ───────────────────────────────────────────────────────

/// Body of `POST /hwmon/{header_id}/characterize`. Both fields optional.
#[derive(Debug, Default, Deserialize)]
pub struct CharacterizationRequest {
    /// Duties to test. Clamped, deduped and sorted ascending by
    /// [`resolve_points`]; omitted means [`constants::CHARACTERIZATION_DEFAULT_POINTS`].
    pub points_pct: Option<Vec<u8>>,
    /// Seconds to hold each duty before reading back. Clamped into
    /// `[CHARACTERIZATION_SETTLE_MIN_S, CHARACTERIZATION_SETTLE_MAX_S]`.
    pub settle_seconds: Option<u64>,
    /// DEC-334. Walk the duties down from the top and back up, so `§2`
    /// hysteresis can be measured. Absent is `false`, i.e. the pre-2.40.0
    /// ascending sweep, so an older client's payload means exactly what it
    /// always did.
    pub bidirectional: Option<bool>,
    /// DEC-334. Extra hold, in seconds, at up to
    /// [`constants::STABILITY_MAX_POINTS`] daemon-chosen duties, for `§4`
    /// statistics. Absent or `0` means no dwell. Clamped into
    /// `[STABILITY_MIN_S, STABILITY_MAX_S]`.
    ///
    /// **Which** duties get it is deliberately not a client input: the run's
    /// cost has to be bounded by the daemon, not by the caller.
    pub stability_seconds: Option<u64>,
}

/// `§4` statistics over the tach samples retained during one step's hold.
///
/// `None` on a [`CharPoint`] means the step retained nothing at all — a failed
/// write, or an abort before the hold opened.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PointStability {
    /// Every retained reading, dropouts included.
    pub samples: u32,
    pub usable: u32,
    /// An unreadable reading, or a `0` recorded while the window proved the fan
    /// was turning. A steadily stopped fan reports no dropouts.
    pub dropouts: u32,
    /// Counted, never removed from the figures below.
    pub outliers: u32,
    pub mean_rpm: Option<f64>,
    pub median_rpm: Option<u16>,
    pub min_rpm: Option<u16>,
    pub max_rpm: Option<u16>,
    pub stddev_rpm: Option<f64>,
    /// `None` when the mean is zero; a stopped fan has no relative spread.
    pub cv_pct: Option<f64>,
    /// `stable` | `variable` | `unstable` | `insufficient_data` | `unavailable`.
    /// An opaque token: render an unrecognised one, never drop it (273-i).
    pub verdict: String,
    /// The cadence these readings were actually taken at. Published so no client
    /// has to assume one — `§5` forbids implying resolution the data lacks.
    pub sample_interval_ms: u64,
    /// How much of the hold was dwell rather than settle. `0` for a step the
    /// daemon did not select.
    pub dwell_ms: u64,
}

/// `§7`: a value the daemon derived from a *trusted* correction factor, carried
/// with its provenance so a client can never mistake it for an observation.
///
/// This is the wire's first `{value, provenance}` envelope, and it exists only
/// where the provenance genuinely varies. `rpm_after` is invariantly OBSERVED
/// and stays a bare field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EstimatedRpm {
    pub value: u16,
    /// `DERIVED` — the figure is computed, never measured.
    pub provenance: String,
    pub correction_factor: f64,
    /// Where the factor came from. Only ever compiled-in device metadata:
    /// `DevicePolicy` derives no `Deserialize`, so untrusted input cannot define
    /// one (`§7`: "never auto-infer a correction from approximate RPM range").
    pub correction_source: String,
}

/// Project one window's [`crate::api::stats::RpmStats`] onto the wire.
fn point_stability(samples: &[crate::api::stats::RpmSample], dwell: Duration) -> PointStability {
    let st = crate::api::stats::rpm_stats(samples);
    PointStability {
        samples: st.samples,
        usable: st.usable,
        dropouts: st.dropouts,
        outliers: st.outliers,
        mean_rpm: st.mean,
        median_rpm: st.median,
        min_rpm: st.min,
        max_rpm: st.max,
        stddev_rpm: st.stddev,
        cv_pct: st.cv_pct,
        verdict: st.verdict.to_string(),
        sample_interval_ms: constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64,
        dwell_ms: dwell.as_millis() as u64,
    }
}

/// One duty's learned RPM band, from a previous characterisation of this header
/// (`§6`). Supplied by the caller from the persisted store; `summarise` never
/// reads it from disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearnedPoint {
    pub duty_pct: u8,
    pub rpm_min: u16,
    pub rpm_max: u16,
}

/// A trusted tach correction, from compiled-in device metadata only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RpmCorrection {
    pub factor: f64,
    pub source: &'static str,
}

/// `§7`: apply a trusted correction, or report nothing.
///
/// Returns `None` when there is no correction — **never a copy of the reported
/// value stamped `DERIVED`**. A client showing "estimated physical RPM" that is
/// really just the reported figure relabelled is exactly the silent promotion
/// the Overview's provenance rule forbids.
pub fn estimate_physical_rpm(
    reported: Option<u16>,
    correction: Option<RpmCorrection>,
) -> Option<EstimatedRpm> {
    let (rpm, c) = (reported?, correction?);
    if !c.factor.is_finite() || c.factor <= 0.0 {
        return None;
    }
    let scaled = (f64::from(rpm) * c.factor).round();
    Some(EstimatedRpm {
        value: scaled.clamp(0.0, f64::from(u16::MAX)) as u16,
        provenance: "DERIVED".into(),
        correction_factor: c.factor,
        correction_source: c.source.to_string(),
    })
}

/// A contiguous span of duties over which reported RPM did not meaningfully
/// change. **Not a fault** — `§3` is explicit that a plateau must not be
/// reinterpreted as pump failure.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlateauSpan {
    pub from_pct: u8,
    pub to_pct: u8,
    pub rpm_min: u16,
    pub rpm_max: u16,
}

/// One measured point. The three axes stay separate on the wire — see the
/// module docs for why collapsing them is a defect, not a simplification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CharPoint {
    /// The duty this point asked for, **after** clamping.
    pub requested_pct: u8,
    /// Did the PWM write itself succeed? (Axis 1.)
    pub command_accepted: bool,
    /// What the header reported back after settling. (Axis 2.)
    pub readback_pct: Option<u8>,
    pub readback_raw: Option<u8>,
    /// `pwm_enable` after settling. Anything but `1` means something else took
    /// the header back — BIOS, EC, or firmware.
    pub pwm_enable: Option<u8>,
    /// Tach before the write and after the settle. (Axis 3.)
    pub rpm_before: Option<u16>,
    pub rpm_after: Option<u16>,
    /// How long this point actually held.
    pub settle_ms: u64,
    /// Time from the write to the first sub-sample whose RPM had moved beyond
    /// the noise floor, when one was seen. `None` means it never moved (or the
    /// tach is unreadable) — not that it responded instantly.
    pub first_change_ms: Option<u64>,
    /// `match` | `clamped` | `reverted` | `unavailable`
    pub readback_verdict: String,
    /// `changed` | `unchanged` | `unavailable`
    pub rpm_verdict: String,
    /// DEC-334. Which leg of the walk this step belongs to: `ramp` | `falling` |
    /// `rising`. `ramp` is the first step in either mode — the only one whose
    /// approach direction is unknown, because it is entered from the captured
    /// pre-sweep duty. A hysteresis comparison must exclude it.
    pub direction: String,
    /// DEC-334. 0-based position in the walked plan. A bidirectional walk visits
    /// some duties twice, so `requested_pct` alone does not order the points.
    pub step_index: u16,
    /// DEC-334, `§5`. When reported RPM entered its settled band, measured from
    /// the write. `None` means it never settled within the hold — **not** that it
    /// settled instantly, the same distinction `first_change_ms` carries.
    pub settled_ms: Option<u64>,
    /// DEC-334, `§4`. `None` when the step retained no samples at all.
    pub stability: Option<PointStability>,
    /// DEC-334, `§7`. Present only where trusted device metadata supplies a
    /// correction factor. **`rpm_after` is always the raw reported value and is
    /// never overwritten by this** (`§9`).
    pub estimated_physical_rpm: Option<EstimatedRpm>,
}

/// Derived diagnostics over a whole sweep. Produced by [`summarise`], which is
/// pure — the handler must call it rather than deriving any of this inline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CharSummary {
    /// `pass` | `partial` | `fail`
    pub command_acceptance: String,
    /// `pass` | `clamped` | `reverted` | `unavailable`
    pub pwm_readback: String,
    /// `responsive` | `no_response` | `unavailable`
    pub rpm_response: String,
    pub min_tested_pct: Option<u8>,
    pub max_tested_pct: Option<u8>,
    pub min_rpm: Option<u16>,
    pub max_rpm: Option<u16>,
    /// `None` when fewer than two points carried a usable tach reading.
    pub monotonic: Option<bool>,
    /// Top of a flat region at the bottom of the sweep, if one was measured.
    pub dead_zone_upper_pct: Option<u8>,
    /// The readback value the hardware appears to pin at, when clamping was seen.
    pub clamp_pct: Option<u8>,
    /// PWM was accepted and read back correctly, yet RPM never responded — the
    /// signature of firmware driving the pump itself. **Not** a fault verdict:
    /// `AIO-Phase3.md` is explicit that a device may legitimately override PWM
    /// during startup or internal thermal protection.
    pub possible_device_override: bool,
    /// Some point saw `pwm_enable != 1` — another controller has the header.
    pub interference_detected: bool,

    // ── DEC-334 (AIO Phase 8 Batch 2) ────────────────────────────────
    /// `§2`. Largest rising/falling gap as a percentage of the observed RPM
    /// span. `None` when nothing could be compared.
    pub hysteresis_pct: Option<f64>,
    /// `none` | `present` | `insufficient_data` | `not_tested`. **Never a fault
    /// verdict** — `§2` lists six legitimate explanations, starting with an
    /// internal device controller.
    pub hysteresis_verdict: String,
    pub hysteresis_worst_duty_pct: Option<u8>,
    pub hysteresis_worst_delta_rpm: Option<u16>,
    /// How many duties carried readings in **both** directions. The turn-around
    /// duty and the `ramp` step do not, and are excluded rather than paired with
    /// a neighbour.
    pub hysteresis_compared_points: u32,

    /// `§3`. Where PWM changes actually move reported RPM.
    pub min_responsive_pct: Option<u8>,
    pub max_responsive_pct: Option<u8>,
    pub low_plateau_to_pct: Option<u8>,
    pub saturation_from_pct: Option<u8>,
    pub plateaus: Vec<PlateauSpan>,

    /// `§4`. The **worst** per-point classification across the sweep, not an
    /// average: one unstable duty is the finding, and averaging would bury it.
    pub stability_verdict: String,
    pub worst_cv_pct: Option<f64>,
    pub total_dropouts: u32,
    pub total_outliers: u32,

    /// `§5`. The cadence the timings were actually measured at. A client must
    /// render the timings against **this**, never assume milliseconds.
    pub measurement_resolution_ms: Option<u64>,
    /// Median across points, in the resolution above. `None` when no point
    /// produced one.
    pub typical_response_ms: Option<u64>,
    pub typical_settling_ms: Option<u64>,

    /// `§6`. `Some(true)` when a reading sat outside the learned band, `Some(false)`
    /// when a band existed and every reading fell inside it, `None` when nothing
    /// has been learned for this header yet. **Three states on purpose:** "no
    /// model" must not read as "passed".
    pub outside_learned_range: Option<bool>,
    pub learned_range_note: Option<String>,
    /// `§6` interpretation states, e.g. `DEVICE_OVERRIDE_POSSIBLE`,
    /// `PWM_CLAMP_POSSIBLE`, `TACH_MAPPING_OR_SCALING_POSSIBLE`. **Possibilities,
    /// never conclusions** — `§6` forbids stating that an internal override
    /// definitely occurred without trusted metadata.
    pub interpretation_states: Vec<String>,
}

/// A characterisation run, and the body of `GET /diagnostics/characterization`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CharacterizationRun {
    pub run_id: String,
    pub header_id: String,
    /// `running` | `complete` | `cancelled` | `aborted` | `failed`
    pub state: String,
    /// The clamped point list this run will walk. `points.len()` against this
    /// is the client's progress indicator.
    pub requested_points_pct: Vec<u8>,
    pub settle_seconds: u64,
    pub points: Vec<CharPoint>,
    /// `None` while running.
    pub summary: Option<CharSummary>,
    /// The duty the header held before the sweep. `None` means it could not be
    /// read, in which case there is nothing to put back.
    pub original_pct: Option<u8>,
    /// **The header was NOT put back.** True on every exit that leaves it parked
    /// at the last swept point — a failed restore write *and* the two deliberate
    /// skips *and* an unreadable pre-sweep duty. Derived from
    /// [`RestoreOutcome::header_left_moved`], so it cannot drift from the reason
    /// below.
    ///
    /// Before v2.30.0 this was `false` on the three non-write exits, which said
    /// "restored" about a header that had not been (`AUD2-c`).
    pub restore_failed: bool,
    /// *Why*, as a stable token: `pending` while the run is still going, then one
    /// of `restored` | `write_failed` | `skipped_shutting_down` |
    /// `skipped_thermal_force` | `no_original_duty`. The client owns the wording
    /// and must render an unrecognised token rather than dropping it (273-i).
    ///
    /// This exists because `restore_failed: true` alone would invite exactly the
    /// wrong action on `skipped_thermal_force`: the header is high because
    /// thermal safety put it there, and a client "writing its intent explicitly"
    /// is the one thing it must not do until the ladder releases.
    pub restore_outcome: String,
    /// Why the run ended, when it did not simply complete.
    pub detail: Option<String>,

    // ── DEC-334 (AIO Phase 8 Batch 2) ────────────────────────────────
    /// Whether this run walked both directions.
    pub bidirectional: bool,
    /// The clamped dwell actually used; `0` when none was requested.
    pub stability_seconds: u64,
    /// Wall clock, for `§6`'s learned-range provenance and the Hardware page's
    /// "last characterised" row. `ControlPathRun` has carried this since Batch 1.
    pub completed_unix_ms: Option<u64>,
    /// `§9` provenance legend for this result: field name → classification
    /// token. A **sidecar**, so the export needs no per-field wrapping and the
    /// fields whose provenance never varies stay bare on the wire.
    pub provenance: BTreeMap<String, String>,
}

impl CharacterizationRun {
    pub fn is_running(&self) -> bool {
        self.state == STATE_RUNNING
    }
}

pub const STATE_RUNNING: &str = "running";
pub const STATE_COMPLETE: &str = "complete";
pub const STATE_CANCELLED: &str = "cancelled";
pub const STATE_ABORTED: &str = "aborted";
pub const STATE_FAILED: &str = "failed";

// ── Restore reporting ────────────────────────────────────────────────

/// Which exit [`RestoreOnDrop`] took — the single source of truth for both
/// `restore_failed` and `restore_outcome` on the wire.
///
/// [SAFETY-adjacent] Two of these five are *deliberate* skips, not faults, and
/// conflating them with a success is what `AUD2-c` recorded: the guard returned
/// early under a thermal force or a shutdown and the run still published
/// `restore_failed: false`, i.e. "the header is back where it was" about a
/// header parked at the last swept duty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RestoreOutcome {
    /// The sweep has not finished, so the guard has not run yet.
    Pending = 0,
    /// The pre-sweep duty was written back successfully.
    Restored = 1,
    /// The restore write was attempted and failed.
    WriteFailed = 2,
    /// Skipped: the daemon is shutting down and `restore_hardware()` owns the
    /// header (DEC-290 / 277-c).
    SkippedShuttingDown = 3,
    /// Skipped: the thermal ladder is forcing and outranks a diagnostic
    /// (DEC-295).
    SkippedThermalForce = 4,
    /// The pre-sweep duty could not be read *and* the sweep moved the header, so
    /// there was nothing to put it back to.
    NoOriginalDuty = 5,
}

impl RestoreOutcome {
    pub fn token(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Restored => "restored",
            Self::WriteFailed => "write_failed",
            Self::SkippedShuttingDown => "skipped_shutting_down",
            Self::SkippedThermalForce => "skipped_thermal_force",
            Self::NoOriginalDuty => "no_original_duty",
        }
    }

    /// Is the header parked somewhere other than where the sweep found it?
    ///
    /// `Pending` is false because nothing has been swept back yet *and* the
    /// terminal publish only reads this after the guard has dropped, so it is
    /// unreachable there. `Restored` is the only other false.
    pub fn header_left_moved(self) -> bool {
        !matches!(self, Self::Pending | Self::Restored)
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Restored,
            2 => Self::WriteFailed,
            3 => Self::SkippedShuttingDown,
            4 => Self::SkippedThermalForce,
            5 => Self::NoOriginalDuty,
            _ => Self::Pending,
        }
    }
}

/// A write-once cell the drop guard stamps and the terminal publish reads.
///
/// An atomic rather than a mutex on purpose: the guard runs inside `Drop`, where
/// a poisoned or contended lock has nowhere to report a failure to.
#[derive(Debug, Default)]
pub struct RestoreReport(AtomicU8);

impl RestoreReport {
    pub fn new() -> Self {
        Self(AtomicU8::new(RestoreOutcome::Pending as u8))
    }
    pub fn set(&self, outcome: RestoreOutcome) {
        self.0.store(outcome as u8, Ordering::SeqCst);
    }
    pub fn get(&self) -> RestoreOutcome {
        RestoreOutcome::from_u8(self.0.load(Ordering::SeqCst))
    }
}

// ── Input resolution (pure) ──────────────────────────────────────────

/// [SAFETY] Clamp, dedupe and order the sweep points.
///
/// `floor` is the header's own floor — [`crate::profile::HARD_PUMP_CPU_FLOOR_PCT`]
/// for a pump-protected header, anything lower for the rest. The effective floor
/// is `max(CHARACTERIZATION_MIN_PCT, floor)`, so **no input can produce a point
/// below 20%, and none can produce 0%**, whatever the caller sends and whatever
/// the header's role resolves to.
///
/// Ascending order is a safety property, not presentation: a sweep aborted at
/// any point has left the header at the highest duty it reached.
pub fn resolve_points(requested: Option<&[u8]>, floor: u8) -> Vec<u8> {
    let effective_floor = floor.max(constants::CHARACTERIZATION_MIN_PCT);
    let source: Vec<u8> = match requested {
        Some(v) if !v.is_empty() => v.to_vec(),
        _ => constants::CHARACTERIZATION_DEFAULT_POINTS.to_vec(),
    };
    let mut out: Vec<u8> = source
        .into_iter()
        .map(|p| p.clamp(effective_floor, 100))
        .collect();
    out.sort_unstable();
    out.dedup();
    out.truncate(constants::CHARACTERIZATION_MAX_POINTS);
    out
}

/// Which leg of a walk a step belongs to (DEC-334).
///
/// `Ramp` is **always the first step of the walk, in both modes**, and it is not
/// a cosmetic label. Every other step is entered from its neighbour, so its
/// approach direction is known; the first is entered from the captured pre-sweep
/// duty, which may be above or below it. Calling that `Falling` (or `Rising`)
/// would feed a wrong-direction reading into the hysteresis comparison — a flag
/// describing a value must be derived from that value, not from the leg it
/// happens to sit in (DEC-325).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Ramp,
    Falling,
    Rising,
}

impl Direction {
    pub fn token(self) -> &'static str {
        match self {
            Self::Ramp => "ramp",
            Self::Falling => "falling",
            Self::Rising => "rising",
        }
    }
}

/// One step of a resolved walk: the duty, which leg it belongs to, and whether
/// it carries a stability dwell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepStep {
    pub pct: u8,
    pub direction: Direction,
    /// Extra hold beyond the settle window, for `§4` statistics. The **daemon**
    /// chooses which steps carry one; the client only asks for a duration.
    pub dwell: Option<Duration>,
}

/// Thin an ascending list to at most `max` entries, keeping the first and last.
///
/// Truncation would have been wrong: it drops the top of the range, and the
/// range is the thing being characterised.
fn thin_to(points: &[u8], max: usize) -> Vec<u8> {
    if points.len() <= max || max < 2 {
        return points.to_vec();
    }
    let n = points.len();
    let mut out: Vec<u8> = (0..max).map(|i| points[i * (n - 1) / (max - 1)]).collect();
    out.dedup();
    out
}

/// Turn a clamped ascending duty list into the walk the sweep will actually
/// perform.
///
/// [`resolve_points`] is untouched and still owns the clamp/dedup/sort — so its
/// exhaustive "never yields zero or below the floor for any input" proof still
/// covers every duty here, in both modes.
///
/// Bidirectional walks descend from the top (`Ramp`, then `Falling`) and climb
/// back (`Rising`), **skipping the turn-around duty on the way up** because the
/// header is already sitting on it and it cannot be approached from below
/// without breaching the floor. That makes the walk `2n - 1` steps, which is why
/// the unique-duty budget is [`constants::CHARACTERIZATION_MAX_UNIQUE_BIDIRECTIONAL`]
/// rather than the step cap itself.
pub fn resolve_sweep_plan(
    points: &[u8],
    bidirectional: bool,
    dwell: Option<Duration>,
) -> Vec<SweepStep> {
    if points.is_empty() {
        return Vec::new();
    }
    let base: Vec<u8> = if bidirectional {
        thin_to(points, constants::CHARACTERIZATION_MAX_UNIQUE_BIDIRECTIONAL)
    } else {
        points.to_vec()
    };

    let mut steps: Vec<SweepStep> = Vec::new();
    let push = |pct: u8, direction: Direction, steps: &mut Vec<SweepStep>| {
        steps.push(SweepStep {
            pct,
            direction,
            dwell: None,
        });
    };
    if bidirectional {
        for (i, &pct) in base.iter().rev().enumerate() {
            push(
                pct,
                if i == 0 {
                    Direction::Ramp
                } else {
                    Direction::Falling
                },
                &mut steps,
            );
        }
        for &pct in base.iter().skip(1) {
            push(pct, Direction::Rising, &mut steps);
        }
    } else {
        for (i, &pct) in base.iter().enumerate() {
            push(
                pct,
                if i == 0 {
                    Direction::Ramp
                } else {
                    Direction::Rising
                },
                &mut steps,
            );
        }
    }
    // Belt and braces: the arithmetic above cannot exceed the cap, and the
    // compile-time assert says so, but a walk is the thing that costs engine
    // write-pause and it is bounded here as well as by construction.
    steps.truncate(constants::CHARACTERIZATION_MAX_POINTS);
    assign_dwells(&mut steps, dwell);
    steps
}

/// Choose which steps carry a stability dwell — lowest, middle and highest of
/// the **final** leg, capped at [`constants::STABILITY_MAX_POINTS`].
///
/// Daemon-chosen rather than client-chosen so the run's cost is bounded here
/// regardless of what is requested. The final leg is preferred because those
/// readings are taken after the device has been through the whole walk.
fn assign_dwells(steps: &mut [SweepStep], dwell: Option<Duration>) {
    let Some(d) = dwell else {
        return;
    };
    if steps.is_empty() {
        return;
    }
    let last_dir = steps[steps.len() - 1].direction;
    let mut leg: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.direction == last_dir)
        .map(|(i, _)| i)
        .collect();
    if leg.len() < 2 {
        leg = (0..steps.len()).collect();
    }
    let n = leg.len();
    let k = constants::STABILITY_MAX_POINTS.min(n);
    let mut picks: Vec<usize> = if k <= 1 {
        vec![leg[n - 1]]
    } else {
        (0..k).map(|i| leg[i * (n - 1) / (k - 1)]).collect()
    };
    picks.sort_unstable();
    picks.dedup();
    for i in picks {
        steps[i].dwell = Some(d);
    }
}

/// Clamp the optional stability dwell. Absent **or zero** means no dwell at all:
/// the statistics are then derived from the samples the settle window already
/// takes, which is the always-on half of `§4`.
pub fn resolve_stability_dwell(requested: Option<u64>) -> Option<Duration> {
    match requested {
        None | Some(0) => None,
        Some(secs) => Some(Duration::from_secs(
            secs.clamp(constants::STABILITY_MIN_S, constants::STABILITY_MAX_S),
        )),
    }
}

/// Clamp the per-point settle window. See
/// [`constants::CHARACTERIZATION_SETTLE_MAX_S`] for why the ceiling is
/// load-bearing rather than cosmetic.
pub fn resolve_settle(requested: Option<u64>) -> Duration {
    let secs = requested
        .unwrap_or(constants::CHARACTERIZATION_DEFAULT_SETTLE_S)
        .clamp(
            constants::CHARACTERIZATION_SETTLE_MIN_S,
            constants::CHARACTERIZATION_SETTLE_MAX_S,
        );
    Duration::from_secs(secs)
}

// ── Per-point and summary derivation (pure) ──────────────────────────

/// Did this tach reading move enough to mean anything?
fn rpm_moved(before: u16, after: u16) -> bool {
    let delta = before.abs_diff(after);
    delta > constants::CHARACTERIZATION_RPM_NOISE_FLOOR.max(before / 10)
}

/// Classify one point's PWM readback. `reverted` outranks everything: if
/// `pwm_enable` is not 1, the value read back is not ours to interpret —
/// **unless** it is the driver's full-speed alias, which is our own write
/// reflected back (`pwm::is_full_speed_alias`, DEC-326 / `HOST-a`). Without
/// that exemption every sweep's 100% point scores `reverted` on an ITE chip
/// and the run aborts one point from the end.
fn readback_verdict(requested_pct: u8, readback_pct: Option<u8>, pwm_enable: Option<u8>) -> String {
    if let Some(en) = pwm_enable {
        if en != 1 && !crate::pwm::is_full_speed_alias(requested_pct, readback_pct, pwm_enable) {
            return "reverted".into();
        }
    }
    match readback_pct {
        None => "unavailable".into(),
        Some(got) => {
            if got.abs_diff(requested_pct) <= constants::CHARACTERIZATION_READBACK_TOLERANCE_PCT {
                "match".into()
            } else {
                "clamped".into()
            }
        }
    }
}

fn rpm_verdict(before: Option<u16>, after: Option<u16>) -> String {
    match (before, after) {
        (Some(b), Some(a)) => {
            if rpm_moved(b, a) {
                "changed".into()
            } else {
                "unchanged".into()
            }
        }
        _ => "unavailable".into(),
    }
}

/// Derive the whole-sweep diagnostics. Pure, total, and the only place these
/// rules live — the handler calls this rather than deriving anything inline.
pub fn summarise(points: &[CharPoint], learned: &[LearnedPoint]) -> CharSummary {
    let accepted = points.iter().filter(|p| p.command_accepted).count();
    let command_acceptance = if points.is_empty() || accepted == 0 {
        "fail"
    } else if accepted == points.len() {
        "pass"
    } else {
        "partial"
    }
    .to_string();

    // Same exemption as `readback_verdict`: a full-speed alias is our own duty
    // read back through a driver that reports mode from the duty register, not
    // a second writer (DEC-326 / `HOST-a`).
    let interference_detected = points.iter().any(|p| {
        matches!(p.pwm_enable, Some(en) if en != 1)
            && !crate::pwm::is_full_speed_alias(p.requested_pct, p.readback_pct, p.pwm_enable)
    });

    let pwm_readback = if points.iter().any(|p| p.readback_verdict == "reverted") {
        "reverted"
    } else if points.is_empty() || points.iter().all(|p| p.readback_verdict == "unavailable") {
        "unavailable"
    } else if points.iter().any(|p| p.readback_verdict == "clamped") {
        "clamped"
    } else {
        "pass"
    }
    .to_string();

    // The readback the hardware appears to pin at: the value shared by the most
    // clamped points, lowest wins a tie. Reported as a candidate, never as a
    // proven device limit — one sweep is one noisy sample.
    let mut clamped: Vec<u8> = points
        .iter()
        .filter(|p| p.readback_verdict == "clamped")
        .filter_map(|p| p.readback_pct)
        .collect();
    clamped.sort_unstable();
    let clamp_pct = clamped
        .iter()
        .map(|v| (clamped.iter().filter(|o| *o == v).count(), *v))
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(_, v)| v);

    let rpms: Vec<(u8, u16)> = points
        .iter()
        .filter_map(|p| p.rpm_after.map(|r| (p.requested_pct, r)))
        .collect();
    let min_rpm = rpms.iter().map(|(_, r)| *r).min();
    let max_rpm = rpms.iter().map(|(_, r)| *r).max();

    let rpm_response = match (min_rpm, max_rpm) {
        (Some(lo), Some(hi)) => {
            let spread = hi - lo;
            let threshold = constants::CHARACTERIZATION_RESPONSIVE_MIN_DELTA_RPM.max(lo / 5);
            if spread > threshold {
                "responsive"
            } else {
                "no_response"
            }
        }
        _ => "unavailable",
    }
    .to_string();

    // Monotonic within tolerance: no reading may fall meaningfully below the one
    // before it. `AIO-Phase3.md` is explicit that a non-monotonic result is not
    // by itself a fault, so this is reported, never acted on.
    let monotonic = if rpms.len() < 2 {
        None
    } else {
        let mut ok = true;
        for w in rpms.windows(2) {
            let (prev, next) = (w[0].1, w[1].1);
            let tolerance = constants::CHARACTERIZATION_RPM_NOISE_FLOOR.max(prev / 33);
            if next + tolerance < prev {
                ok = false;
                break;
            }
        }
        Some(ok)
    };

    // Dead zone: a flat region at the BOTTOM of the sweep that something above it
    // eventually escapes. Requires a later point to have actually risen, so a
    // uniformly flat (unresponsive) sweep reports no dead zone — that is
    // `rpm_response: no_response`, a different finding.
    let dead_zone_upper_pct = match (min_rpm, rpms.len()) {
        (Some(lo), n) if n >= 2 => {
            let tolerance = constants::CHARACTERIZATION_RPM_NOISE_FLOOR.max(lo / 33);
            let flat_len = rpms
                .iter()
                .take_while(|(_, r)| r.abs_diff(lo) <= tolerance)
                .count();
            if flat_len >= 2 && flat_len < rpms.len() {
                Some(rpms[flat_len - 1].0)
            } else {
                None
            }
        }
        _ => None,
    };

    // ── DEC-334 derivations ──────────────────────────────────────────
    use crate::api::stats;

    // Derived from the POINTS, not from a request flag: a flag describing a
    // value must come from that value (DEC-325). A run that aborted before the
    // falling leg produced no falling readings and is honestly unidirectional.
    let bidirectional = points.iter().any(|p| p.direction == "falling");

    let series = |dir: &str| -> Vec<stats::DutyRpm> {
        stats::fold_direction(
            points
                .iter()
                .filter(|p| p.direction == dir)
                .filter_map(|p| {
                    p.rpm_after.map(|rpm| stats::DutyRpm {
                        duty_pct: p.requested_pct,
                        rpm,
                    })
                }),
        )
    };
    let rising = series("rising");
    let falling = series("falling");

    let hyst = stats::hysteresis(&rising, &falling, bidirectional);

    // Shape analysis runs over every usable reading regardless of leg — the
    // effective range is a property of the header, not of one direction.
    let all_points = stats::fold_direction(points.iter().filter_map(|p| {
        p.rpm_after.map(|rpm| stats::DutyRpm {
            duty_pct: p.requested_pct,
            rpm,
        })
    }));
    let range = stats::effective_range(&all_points);
    let plateaus: Vec<PlateauSpan> = stats::plateaus(&all_points)
        .into_iter()
        .map(|pl| PlateauSpan {
            from_pct: pl.from_pct,
            to_pct: pl.to_pct,
            rpm_min: pl.rpm_min,
            rpm_max: pl.rpm_max,
        })
        .collect();

    // `§4`: the WORST per-point classification, never an average — one unstable
    // duty is the finding, and averaging would bury it.
    let rank = |v: &str| match v {
        stats::STABILITY_UNSTABLE => 4,
        stats::STABILITY_VARIABLE => 3,
        stats::STABILITY_INSUFFICIENT => 2,
        stats::STABILITY_STABLE => 1,
        _ => 0,
    };
    let stability_verdict = points
        .iter()
        .filter_map(|p| p.stability.as_ref())
        .map(|st| st.verdict.as_str())
        .max_by_key(|v| rank(v))
        .unwrap_or(stats::STABILITY_UNAVAILABLE)
        .to_string();
    let worst_cv_pct = points
        .iter()
        .filter_map(|p| p.stability.as_ref())
        .filter_map(|st| st.cv_pct)
        .fold(None::<f64>, |acc, cv| {
            Some(acc.map_or(cv, |a: f64| a.max(cv)))
        });
    let total_dropouts: u32 = points
        .iter()
        .filter_map(|p| p.stability.as_ref())
        .map(|st| st.dropouts)
        .sum();
    let total_outliers: u32 = points
        .iter()
        .filter_map(|p| p.stability.as_ref())
        .map(|st| st.outliers)
        .sum();

    // `§5`: publish the cadence the timings were measured at, so no client has
    // to assume milliseconds. Taken from the samples actually retained rather
    // than restated as a constant.
    let measurement_resolution_ms = points
        .iter()
        .filter_map(|p| p.stability.as_ref())
        .map(|st| st.sample_interval_ms)
        .max();
    let median_u64 = |mut v: Vec<u64>| -> Option<u64> {
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        Some(v[v.len() / 2])
    };
    let typical_response_ms = median_u64(points.iter().filter_map(|p| p.first_change_ms).collect());
    let typical_settling_ms = median_u64(points.iter().filter_map(|p| p.settled_ms).collect());

    // `§6`: compare against the learned band, three-state.
    let (outside_learned_range, learned_range_note, above, below) =
        compare_to_learned(points, learned);

    // `§6` interpretation states. Every one is a POSSIBILITY: the section is
    // explicit that an internal override must never be stated as fact without
    // trusted metadata, and that unexpected RPM is not pump failure.
    let mut interpretation_states: Vec<String> = Vec::new();
    let readback_ok = pwm_readback == "pass";
    if readback_ok && rpm_response == "no_response" {
        interpretation_states.push("DEVICE_OVERRIDE_POSSIBLE".into());
    }
    if pwm_readback == "clamped" {
        interpretation_states.push("PWM_CLAMP_POSSIBLE".into());
    }
    if outside_learned_range == Some(true) && readback_ok {
        if above > below && above * 2 >= points.len() {
            // Consistently faster than learned across most of the sweep: the
            // device driving itself is a better explanation than a fault.
            interpretation_states.push("DEVICE_THERMAL_CONTROL_POSSIBLE".into());
        }
        if above > 0 && above <= 2 && points.len() > 3 {
            interpretation_states.push("STARTUP_OVERRIDE_POSSIBLE".into());
        }
        if let Some(ratio) = suspicious_tach_ratio(points, learned) {
            let _ = ratio;
            interpretation_states.push("TACH_MAPPING_OR_SCALING_POSSIBLE".into());
        }
        if !interpretation_states
            .iter()
            .any(|s| s == "DEVICE_OVERRIDE_POSSIBLE")
        {
            interpretation_states.push("DEVICE_OVERRIDE_POSSIBLE".into());
        }
    }

    CharSummary {
        possible_device_override: pwm_readback == "pass" && rpm_response == "no_response",
        command_acceptance,
        pwm_readback,
        rpm_response,
        min_tested_pct: points.iter().map(|p| p.requested_pct).min(),
        max_tested_pct: points.iter().map(|p| p.requested_pct).max(),
        min_rpm,
        max_rpm,
        monotonic,
        dead_zone_upper_pct,
        clamp_pct,
        interference_detected,

        hysteresis_pct: hyst.magnitude_pct,
        hysteresis_verdict: hyst.verdict.to_string(),
        hysteresis_worst_duty_pct: hyst.worst_duty_pct,
        hysteresis_worst_delta_rpm: hyst.worst_delta_rpm,
        hysteresis_compared_points: hyst.compared_points,

        min_responsive_pct: range.min_responsive_pct,
        max_responsive_pct: range.max_responsive_pct,
        low_plateau_to_pct: range.low_plateau_to_pct,
        saturation_from_pct: range.saturation_from_pct,
        plateaus,

        stability_verdict,
        worst_cv_pct,
        total_dropouts,
        total_outliers,

        measurement_resolution_ms,
        typical_response_ms,
        typical_settling_ms,

        outside_learned_range,
        learned_range_note,
        interpretation_states,
    }
}

/// `§6`: measure each reading against its learned band.
///
/// Returns `(outside, note, above_count, below_count)`. `outside` is **three
/// state**: `None` means nothing has been learned for this header yet, and must
/// not read as "passed" — the Overview's rule that lack of evidence never
/// becomes a PASS, applied to its own absence.
fn compare_to_learned(
    points: &[CharPoint],
    learned: &[LearnedPoint],
) -> (Option<bool>, Option<String>, usize, usize) {
    if learned.is_empty() {
        return (None, None, 0, 0);
    }
    let tol = constants::LEARNED_RANGE_TOLERANCE_PCT / 100.0;
    let mut above = 0usize;
    let mut below = 0usize;
    let mut compared = 0usize;
    let mut worst: Option<(u8, u16, u16, u16)> = None;
    for p in points {
        let (Some(rpm), Some(band)) = (
            p.rpm_after,
            learned.iter().find(|l| l.duty_pct == p.requested_pct),
        ) else {
            continue;
        };
        compared += 1;
        let hi = f64::from(band.rpm_max) * (1.0 + tol);
        let lo = f64::from(band.rpm_min) * (1.0 - tol);
        let v = f64::from(rpm);
        if v > hi {
            above += 1;
        } else if v < lo {
            below += 1;
        } else {
            continue;
        }
        let gap = if v > hi {
            rpm.saturating_sub(band.rpm_max)
        } else {
            band.rpm_min.saturating_sub(rpm)
        };
        if worst.is_none_or(|(_, _, _, g)| gap > g) {
            worst = Some((p.requested_pct, band.rpm_min, band.rpm_max, gap));
        }
    }
    if compared == 0 {
        return (None, None, 0, 0);
    }
    let note =
        worst.map(|(duty, lo, hi, _)| format!("at {duty}% the learned response is {lo}-{hi} RPM"));
    (Some(above + below > 0), note, above, below)
}

/// `§7`-adjacent: does the deviation look like a tach *scaling* difference
/// rather than a speed difference?
///
/// A scaled tach is off by a near-constant multiple across the whole sweep — 2x
/// and 0.5x being the common pulse-per-revolution mismatches. A device running
/// its own control is not. Reported only as a possibility, and never used to
/// infer a correction: `§7` forbids auto-inferring one from an approximate range.
fn suspicious_tach_ratio(points: &[CharPoint], learned: &[LearnedPoint]) -> Option<f64> {
    let mut ratios: Vec<f64> = Vec::new();
    for p in points {
        let (Some(rpm), Some(band)) = (
            p.rpm_after,
            learned.iter().find(|l| l.duty_pct == p.requested_pct),
        ) else {
            continue;
        };
        let mid = (f64::from(band.rpm_min) + f64::from(band.rpm_max)) / 2.0;
        if mid > 0.0 {
            ratios.push(f64::from(rpm) / mid);
        }
    }
    if ratios.len() < 3 {
        return None;
    }
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    // Consistent to within 10%, and near a 2x or 0.5x mismatch.
    let consistent = ratios.iter().all(|r| (r - mean).abs() <= mean * 0.10);
    let near = |target: f64| (mean - target).abs() <= 0.15;
    if consistent && (near(2.0) || near(0.5) || near(3.0) || near(1.0 / 3.0)) {
        Some(mean)
    } else {
        None
    }
}

// ── The sweep ────────────────────────────────────────────────────────

/// Restores the pre-sweep duty on drop — completion, cancellation, an early
/// return, and the runtime dropping the detached task at shutdown.
///
/// **The two skip rules live INSIDE `drop`, deliberately.** The calibrate
/// equivalent (`calibration::RestoreOnDrop`) only needed the thermal rule,
/// because its future is awaited by a handler that cannot outlive the process.
/// This sweep is detached: nothing in `main::shutdown_sequence`'s `task_handles`
/// joins it, so its guard genuinely can run *during* shutdown, after
/// `restore_hardware()` has handed the header back to firmware. A restore
/// written there would re-assert `pwm_enable=1` at a fixed duty with no writer
/// left to revise it — the exact DEC-290 / 277-c hazard. Checking before the
/// await instead of inside `drop` would not cover it.
///
/// **Second consumer, AIO Phase 8 Batch 1.** `api::discovery`'s control-path
/// sweep constructs one of these too, which is why the struct and its fields are
/// `pub(crate)` rather than private. Nothing about the guard changed for that —
/// the two skip rules, their order, and the `restore_floor` clamp are the same
/// code running for both diagnostics, which is the point. **The drop-order
/// invariant travels with it**: whoever builds one must declare it LAST in its
/// scope so it drops FIRST, while the caller's hwmon lease guard is still held.
pub(crate) struct RestoreOnDrop<'a, W: Fn(u8) -> Result<(), String>, S: Fn() -> bool> {
    pub(crate) header_id: &'a str,
    pub(crate) original_pct: Option<u8>,
    pub(crate) write_fn: &'a W,
    pub(crate) cache: &'a StateCache,
    pub(crate) shutting_down: &'a S,
    /// Set by the sweep loop before its first write. Distinguishes "there was no
    /// pre-sweep duty to restore and we never moved the header" (nothing to
    /// report) from "we moved it and cannot put it back" ([`RestoreOutcome::NoOriginalDuty`]).
    pub(crate) wrote_any: &'a AtomicBool,
    pub(crate) report: &'a RestoreReport,
    /// [SAFETY] `AUD3-l`. The lowest duty this header may be RESTORED to —
    /// `HARD_PUMP_CPU_FLOOR_PCT` for a pump-protected header, 0 for everything
    /// else. `resolve_points` has always floored the duties written on the way
    /// IN; the way out wrote `original_pct` straight through, and the write path
    /// applies no floor of its own. Restoring a captured 0 to a pump therefore
    /// converted "0 under firmware control" into "0 under `pwm_enable=1` with no
    /// writer" — a stopped pump. Same clamp as `hwmon_ctl::restore_duty`.
    pub(crate) restore_floor: u8,
}

impl<W: Fn(u8) -> Result<(), String>, S: Fn() -> bool> Drop for RestoreOnDrop<'_, W, S> {
    /// **Rewritten for `AUD2-c`:** every exit now stamps `self.report`. It
    /// previously recorded only the failed-write exit and left the other four at
    /// the caller's `false`, so three of them published "restored" about a header
    /// that was still at the last swept duty. The branch *order* and the two skip
    /// rules are unchanged — this is a reporting fix, not a behaviour change, and
    /// no write was added or removed on any path.
    fn drop(&mut self) {
        // A run that never wrote left the header exactly where it found it, so
        // none of the three non-restoring exits is a finding for it. Reporting
        // one would trade `AUD2-c`'s false "restored" for a false alarm — and the
        // ladder-aborts-at-point-0 case, which writes nothing, is the common one.
        let moved = self.wrote_any.load(Ordering::SeqCst);
        let left_behind = |o: RestoreOutcome| {
            if moved {
                o
            } else {
                RestoreOutcome::Restored
            }
        };

        // The two authority skips are checked BEFORE `original_pct`, deliberately
        // and unlike the original order. Both can coincide with an unreadable
        // pre-sweep duty, and when they do it is the *authority* the client needs
        // to hear about: `no_original_duty` invites "re-activate your profile",
        // which under a thermal force is the one thing it must not do. No write
        // moves as a result — all three of these exits only ever `return`.
        if (self.shutting_down)() {
            log::info!(
                "characterize: skipping restore of {} — the daemon is shutting down \
                 and the hardware restore owns the header",
                self.header_id
            );
            self.report
                .set(left_behind(RestoreOutcome::SkippedShuttingDown));
            return;
        }
        if let Some(state) = thermal_force_state(self.cache) {
            log::warn!(
                "characterize: {} left at the thermal-safety forced duty instead of \
                 restoring its pre-sweep duty — thermal safety is active ({state}) \
                 and outranks a diagnostic.",
                self.header_id
            );
            self.report
                .set(left_behind(RestoreOutcome::SkippedThermalForce));
            return;
        }
        let Some(restore) = self.original_pct else {
            if moved {
                log::warn!(
                    "characterize: {} was swept but its pre-sweep duty could not be \
                     read, so it is left at the last swept duty",
                    self.header_id
                );
            }
            self.report.set(left_behind(RestoreOutcome::NoOriginalDuty));
            return;
        };
        // [SAFETY] `AUD3-l` — clamp on the way out, as the sweep does on the way in.
        let restore = restore.max(self.restore_floor);
        match (self.write_fn)(restore) {
            Ok(()) => self.report.set(RestoreOutcome::Restored),
            Err(e) => {
                log::warn!(
                    "characterize: restore of {} to {restore}% failed; it is left at the \
                     last swept duty: {e}",
                    self.header_id
                );
                self.report.set(RestoreOutcome::WriteFailed);
            }
        }
    }
}

/// How a sweep ended.
pub struct SweepOutcome {
    pub state: &'static str,
    pub detail: Option<String>,
    pub points: Vec<CharPoint>,
}

/// Walk `points` on one header, publishing each measured point as it lands.
///
/// Generic over the write/read closures so the whole sequence — including every
/// abort path and the restore — is testable without sysfs. `publish` is called
/// once per completed point and is what makes the run visible to
/// `GET /diagnostics/characterization` while it is still running.
///
/// Aborts, all of which restore: `cancel` set (→ `cancelled`), a failed write
/// (→ `failed`), `pwm_enable != 1` (→ `aborted`, reclaim), a sensor over the
/// calibrate/verify limit or the ladder forcing (→ `aborted`).
#[allow(clippy::too_many_arguments)]
pub async fn run_sweep<W, R, P, S, K>(
    cache: &StateCache,
    header_id: &str,
    plan: &[SweepStep],
    // [SAFETY] `AUD3-l`: the lowest duty the RESTORE may write. Separate from
    // the sweep floor already baked into `points` — for a non-pump header the
    // sweep floor is `CHARACTERIZATION_MIN_PCT` while the correct restore floor
    // is 0, because putting an ordinary fan back where it was found is not a
    // safety event. Passed explicitly rather than derived from `points[0]` for
    // exactly that reason.
    restore_floor: u8,
    settle: Duration,
    // `§7`. `None` on every shipped machine — no `DevicePolicy` entry sets one.
    correction: Option<RpmCorrection>,
    write_fn: W,
    read_fn: R,
    cancel: &AtomicBool,
    shutting_down: S,
    keepalive: K,
    report: &RestoreReport,
    mut publish: P,
) -> SweepOutcome
where
    W: Fn(u8) -> Result<(), String>,
    R: Fn() -> HwmonVerifyState,
    P: FnMut(CharPoint),
    S: Fn() -> bool,
    K: Fn() -> bool,
{
    let original_pct = read_fn().pwm_percent;
    let mut measured: Vec<CharPoint> = Vec::with_capacity(plan.len());
    // Declared BEFORE the guard so it outlives it — the guard reads it in `drop`.
    let wrote_any = AtomicBool::new(false);

    // Declared LAST so it drops FIRST — while the caller's lease guard is still
    // held. Reversed, the restore write fails `InvalidLease` and the header is
    // parked at the final sweep point (invariant 6 of the agreed scope).
    let _restore = RestoreOnDrop {
        header_id,
        original_pct,
        write_fn: &write_fn,
        cache,
        shutting_down: &shutting_down,
        wrote_any: &wrote_any,
        report,
        restore_floor,
    };

    for (idx, step) in plan.iter().enumerate() {
        let pct = step.pct;
        // [SAFETY] Stop writing the moment the daemon starts going down. The
        // drop guard's shutdown skip covers the RESTORE, but not this loop: the
        // task is detached, so it keeps running through `shutdown_sequence` and
        // can land a `set_pwm` AFTER `restore_hwmon_to_auto` has handed the
        // header back to firmware. `set_pwm`'s reclaim watchdog would then see
        // the `pwm_enable=2` that restore just wrote, call it a BIOS reclaim and
        // re-assert `pwm_enable=1` at the swept duty — a header latched in manual
        // with no writer left. That is the DEC-290 / 277-c hazard, and checking
        // only in `Drop` does not close it.
        if shutting_down() {
            return SweepOutcome {
                state: STATE_ABORTED,
                detail: Some("the daemon is shutting down".into()),
                points: measured,
            };
        }
        if cancel.load(Ordering::SeqCst) {
            return SweepOutcome {
                state: STATE_CANCELLED,
                detail: Some(format!("cancelled after {idx} of {} steps", plan.len())),
                points: measured,
            };
        }
        if let Err(e) = check_thermal_safety(cache) {
            return SweepOutcome {
                state: STATE_ABORTED,
                detail: Some(e.to_string()),
                points: measured,
            };
        }
        if let Some(state) = thermal_force_state(cache) {
            return SweepOutcome {
                state: STATE_ABORTED,
                detail: Some(format!(
                    "thermal safety is forcing fan output ({state}); \
                     characterisation cannot write"
                )),
                points: measured,
            };
        }
        // DEC-296: prove liveness once per point so the deadman measures that
        // rather than the sweep's total duration.
        if !keepalive() {
            return SweepOutcome {
                state: STATE_ABORTED,
                detail: Some("superseded by a later diagnostic; this run's lease is gone".into()),
                points: measured,
            };
        }

        let before = read_fn();
        let rpm_before = before.rpm;

        // Stamped BEFORE the call, deliberately: `set_pwm` writes sysfs and then
        // reads back, so an `Err` can still have moved the header. Over-reporting
        // "the header moved" is the safe direction; under-reporting it is the
        // `AUD2-c` defect.
        wrote_any.store(true, Ordering::SeqCst);
        let command_accepted = match write_fn(pct) {
            Ok(()) => true,
            Err(e) => {
                measured.push(CharPoint {
                    requested_pct: pct,
                    command_accepted: false,
                    readback_pct: before.pwm_percent,
                    readback_raw: before.pwm_raw,
                    pwm_enable: before.pwm_enable,
                    rpm_before,
                    rpm_after: None,
                    settle_ms: 0,
                    first_change_ms: None,
                    readback_verdict: "unavailable".into(),
                    rpm_verdict: "unavailable".into(),
                    direction: step.direction.token().into(),
                    step_index: idx as u16,
                    ..Default::default()
                });
                let last = measured.last().expect("just pushed").clone();
                publish(last);
                return SweepOutcome {
                    state: STATE_FAILED,
                    detail: Some(format!("PWM write of {pct}% failed: {e}")),
                    points: measured,
                };
            }
        };

        // Hold the settle, then any stability dwell, sub-sampling throughout.
        // No early exit: a deterministic window keeps the pause budget an upper
        // bound. `tokio::time::Instant`, NOT `std::time::Instant`: the latter
        // does not advance under `#[tokio::test(start_paused)]`, so this loop's
        // exit condition would never be reached and the test would hang rather
        // than fail (CLAUDE.md, tokio-test trap 1). Identical in production.
        //
        // [SAFETY] DEC-334. The lease and the engine-pause deadman are renewed
        // **inside this loop** on their own cadence, not once per step. That is
        // not a refinement, it is what makes a dwell legal at all: the per-step
        // renewal that served the bare settle is bounded by
        // `CHARACTERIZATION_SETTLE_MAX_S * 2 <= VERIFY_PAUSE_DEADMAN`, which
        // holds at exactly 30 == 30 — zero headroom — so a hold longer than a
        // settle overruns the deadman at ANY dwell length, and at
        // `STABILITY_MAX_S` it also outlives the 60 s lease TTL. `constants.rs`
        // records what that costs: a sweep that blew its lease could not even
        // restore the header. The assert that guards this is derived from
        // `STABILITY_RENEW_INTERVAL_S`, deliberately NOT copied from the settle
        // one — copying it would have kept the arithmetic and changed its
        // meaning (DEC-333).
        let dwell = step.dwell.unwrap_or(Duration::ZERO);
        let hold = settle + dwell;
        let renew_every = Duration::from_secs(constants::STABILITY_RENEW_INTERVAL_S);
        let started = tokio::time::Instant::now();
        let mut first_change_ms: Option<u64> = None;
        let mut samples: Vec<crate::api::stats::RpmSample> = Vec::new();
        let mut last_renew = tokio::time::Instant::now();
        while started.elapsed() < hold {
            let remaining = hold.saturating_sub(started.elapsed());
            tokio::time::sleep(remaining.min(constants::CHARACTERIZATION_SAMPLE_INTERVAL)).await;
            // Same rule mid-hold: the sub-sample cadence is what bounds how long
            // a shutdown waits for this task to stop touching hardware.
            if shutting_down() {
                measured.push(CharPoint {
                    requested_pct: pct,
                    command_accepted,
                    readback_pct: None,
                    readback_raw: None,
                    pwm_enable: None,
                    rpm_before,
                    rpm_after: None,
                    settle_ms: started.elapsed().as_millis() as u64,
                    first_change_ms,
                    readback_verdict: "unavailable".into(),
                    rpm_verdict: "unavailable".into(),
                    direction: step.direction.token().into(),
                    step_index: idx as u16,
                    ..Default::default()
                });
                return SweepOutcome {
                    state: STATE_ABORTED,
                    detail: Some("the daemon is shutting down".into()),
                    points: measured,
                };
            }
            if last_renew.elapsed() >= renew_every {
                // [SAFETY] The thermal abort has to be re-evaluated INSIDE the
                // hold, not only at the top of the step. A step used to be at
                // most one settle (15 s); with a dwell it is up to 75 s, so
                // checking only at entry stretched the worst-case latency on the
                // `CALIBRATION_MAX_TEMP_C` (85 °C) abort five-fold — and that
                // threshold exists precisely because a sweep is *voluntary* and
                // should give up with more headroom than the emergency ladder.
                //
                // The >=105 °C ladder was never the exposure: it force-takes the
                // hwmon lease, so `keepalive()` below fails within one renewal
                // interval. The 85-105 °C band had nothing backstopping it.
                //
                // Evaluated on the renewal cadence rather than per sample: that
                // bounds the latency at ~5.5 s, which is *better* than the 15 s
                // this path allowed before the dwell existed, without paying for
                // a cache snapshot twice a second.
                if let Err(e) = check_thermal_safety(cache) {
                    return SweepOutcome {
                        state: STATE_ABORTED,
                        detail: Some(e.to_string()),
                        points: measured,
                    };
                }
                if let Some(state) = thermal_force_state(cache) {
                    return SweepOutcome {
                        state: STATE_ABORTED,
                        detail: Some(format!(
                            "thermal safety is forcing fan output ({state}); \
                             characterisation cannot continue"
                        )),
                        points: measured,
                    };
                }
                if !keepalive() {
                    return SweepOutcome {
                        state: STATE_ABORTED,
                        detail: Some(
                            "superseded by a later diagnostic; this run's lease is gone".into(),
                        ),
                        points: measured,
                    };
                }
                last_renew = tokio::time::Instant::now();
            }
            // A dwell can be an order of magnitude longer than a settle, so it
            // honours a cancel rather than making the user wait it out. The
            // SETTLE keeps its documented semantics exactly — "the window
            // currently being held finishes" — because shortening that would
            // change behaviour older clients already depend on.
            //
            // **Gated on `step.dwell`, and the first draft was not.** With no
            // dwell `hold == settle`, and the final iteration sleeps exactly the
            // remainder — so `elapsed() >= settle` is true on the last tick of
            // EVERY plain settle. A cancel pressed at any point during that
            // window therefore returned here before `read_fn()`, discarding a
            // point that had completed its full settle and reporting it as
            // "cancelled during the stability hold" on a run that requested no
            // hold. Pinned by `a_cancel_during_a_plain_settle_still_records_the_point`.
            if step.dwell.is_some() && started.elapsed() >= settle && cancel.load(Ordering::SeqCst)
            {
                return SweepOutcome {
                    state: STATE_CANCELLED,
                    detail: Some(format!(
                        "cancelled during the stability hold at {pct}%, after {idx} of {} steps",
                        plan.len()
                    )),
                    points: measured,
                };
            }
            let at_ms = started.elapsed().as_millis() as u64;
            let sampled = read_fn().rpm;
            samples.push(crate::api::stats::RpmSample {
                at_ms,
                rpm: sampled,
            });
            if first_change_ms.is_none() {
                if let (Some(b), Some(now)) = (rpm_before, sampled) {
                    if rpm_moved(b, now) {
                        first_change_ms = Some(at_ms);
                    }
                }
            }
        }

        let after = read_fn();
        let point = CharPoint {
            requested_pct: pct,
            command_accepted,
            readback_pct: after.pwm_percent,
            readback_raw: after.pwm_raw,
            pwm_enable: after.pwm_enable,
            rpm_before,
            rpm_after: after.rpm,
            settle_ms: started.elapsed().as_millis() as u64,
            first_change_ms,
            readback_verdict: readback_verdict(pct, after.pwm_percent, after.pwm_enable),
            rpm_verdict: rpm_verdict(rpm_before, after.rpm),
            direction: step.direction.token().into(),
            step_index: idx as u16,
            settled_ms: crate::api::stats::settling_ms(&samples),
            stability: Some(point_stability(&samples, dwell)),
            estimated_physical_rpm: estimate_physical_rpm(after.rpm, correction),
        };
        // The abort predicate gets the same exemption (DEC-326 / `HOST-a`).
        // This is the limb that actually ends the run: without it, a sweep whose
        // last point is 100% aborts on a header that accepted every write.
        let reclaimed = matches!(after.pwm_enable, Some(en) if en != 1)
            && !crate::pwm::is_full_speed_alias(pct, after.pwm_percent, after.pwm_enable);
        measured.push(point.clone());
        publish(point);

        // A reclaim ends the sweep and is reported, per the brief: continuing
        // would measure a header somebody else is driving.
        if reclaimed {
            return SweepOutcome {
                state: STATE_ABORTED,
                detail: Some(format!(
                    "another controller reclaimed the header at {pct}% \
                     (pwm_enable={}); the remaining points were not tested",
                    after.pwm_enable.unwrap_or(0)
                )),
                points: measured,
            };
        }
    }

    SweepOutcome {
        state: STATE_COMPLETE,
        detail: None,
        points: measured,
    }
}

/// `§9` provenance legend for a characterisation result.
///
/// A **sidecar**, not per-field envelopes: the Overview requires that every
/// result preserve the COMMANDED / OBSERVED / DERIVED / DEVICE_METADATA /
/// UNVERIFIED distinction, but for almost every field here the classification is
/// fixed by definition and wrapping each one would restate a constant on the wire
/// once per point. Only `estimated_physical_rpm` genuinely varies, and that one
/// carries a real envelope.
///
/// Fields absent from this map are unclassified and a client must render them as
/// such rather than assuming OBSERVED — silently promoting a derived value into a
/// hardware observation is the one thing the Overview forbids outright.
pub fn provenance_legend() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| {
        m.insert(k.to_string(), v.to_string());
    };
    // What Control-OFC asked for.
    put("requested_pct", "COMMANDED");
    put("requested_points_pct", "COMMANDED");
    put("settle_seconds", "COMMANDED");
    put("stability_seconds", "COMMANDED");
    put("bidirectional", "COMMANDED");
    put("original_pct", "OBSERVED");
    // What hwmon actually reported.
    put("readback_pct", "OBSERVED");
    put("readback_raw", "OBSERVED");
    put("pwm_enable", "OBSERVED");
    put("rpm_before", "OBSERVED");
    put("rpm_after", "OBSERVED");
    put("settle_ms", "OBSERVED");
    put("sample_interval_ms", "OBSERVED");
    put("samples", "OBSERVED");
    put("usable", "OBSERVED");
    put("min_rpm", "OBSERVED");
    put("max_rpm", "OBSERVED");
    put("median_rpm", "OBSERVED");
    // What Control-OFC inferred from those observations.
    for k in [
        "first_change_ms",
        "settled_ms",
        "mean_rpm",
        "stddev_rpm",
        "cv_pct",
        "dropouts",
        "outliers",
        "verdict",
        "readback_verdict",
        "rpm_verdict",
        "command_acceptance",
        "pwm_readback",
        "rpm_response",
        "monotonic",
        "dead_zone_upper_pct",
        "clamp_pct",
        "possible_device_override",
        "interference_detected",
        "hysteresis_pct",
        "hysteresis_verdict",
        "min_responsive_pct",
        "max_responsive_pct",
        "low_plateau_to_pct",
        "saturation_from_pct",
        "plateaus",
        "stability_verdict",
        "worst_cv_pct",
        "measurement_resolution_ms",
        "typical_response_ms",
        "typical_settling_ms",
        "outside_learned_range",
        "interpretation_states",
        "estimated_physical_rpm",
        "direction",
    ] {
        put(k, "DERIVED");
    }
    // Supplied by a trusted, compiled-in device definition.
    put("correction_factor", "DEVICE_METADATA");
    put("correction_source", "DEVICE_METADATA");
    m
}

/// A monotonically increasing run id. Opaque to clients; only used so a polling
/// GUI can tell "my run" from "a later one".
pub fn next_run_id() -> String {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(1);
    format!("char-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// The shared slot holding the current or most recent run.
pub type RunSlot = Arc<parking_lot::Mutex<Option<CharacterizationRun>>>;

#[cfg(test)]
mod tests {
    use super::*;

    /// `summarise` with no learned band — the pre-DEC-334 behaviour, which is
    /// what every test written before §6 existed is asserting about.
    fn sum(points: &[CharPoint]) -> CharSummary {
        summarise(points, &[])
    }

    /// A unidirectional plan over `points`, i.e. exactly the walk these tests
    /// have always driven.
    fn plan_of(points: &[u8]) -> Vec<SweepStep> {
        resolve_sweep_plan(points, false, None)
    }

    /// `run_sweep` in its pre-DEC-334 shape: a unidirectional walk and no tach
    /// correction. Every test written before §1/§7 existed is asserting about
    /// exactly that walk, so routing them through one shim keeps their meaning
    /// identical instead of restating the new arguments 11 times.
    #[allow(clippy::too_many_arguments)]
    async fn run_sweep_uni<W, R, P, S, K>(
        cache: &StateCache,
        header_id: &str,
        points: &[u8],
        restore_floor: u8,
        settle: Duration,
        write_fn: W,
        read_fn: R,
        cancel: &AtomicBool,
        shutting_down: S,
        keepalive: K,
        report: &RestoreReport,
        publish: P,
    ) -> SweepOutcome
    where
        W: Fn(u8) -> Result<(), String>,
        R: Fn() -> HwmonVerifyState,
        P: FnMut(CharPoint),
        S: Fn() -> bool,
        K: Fn() -> bool,
    {
        run_sweep(
            cache,
            header_id,
            &plan_of(points),
            restore_floor,
            settle,
            None,
            write_fn,
            read_fn,
            cancel,
            shutting_down,
            keepalive,
            report,
            publish,
        )
        .await
    }
    use crate::health::state::{CachedSensorReading, DeviceLabel};
    use crate::hwmon::types::SensorKind;
    use std::sync::Mutex;

    const PUMP_FLOOR: u8 = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;

    /// One hot CPU reading, for tests that heat the cache mid-run rather than
    /// starting from a hot one.
    fn hot_cpu(temp_c: f64) -> CachedSensorReading {
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

    fn cache_at(temp_c: f64, thermal_state: Option<&str>) -> StateCache {
        let cache = StateCache::new();
        cache.update_sensors(vec![CachedSensorReading {
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
        }]);
        if let Some(s) = thermal_state {
            cache.record_engine_tick(s, constants::THERMAL_EMERGENCY_TRIGGER_C);
        }
        cache
    }

    fn sample(pct: Option<u8>, enable: Option<u8>, rpm: Option<u16>) -> HwmonVerifyState {
        HwmonVerifyState {
            pwm_enable: enable,
            pwm_raw: pct.map(|p| ((p as u16 * 255) / 100) as u8),
            pwm_percent: pct,
            rpm,
        }
    }

    fn point(pct: u8, readback: Option<u8>, enable: Option<u8>, rpm: Option<u16>) -> CharPoint {
        CharPoint {
            requested_pct: pct,
            command_accepted: true,
            readback_pct: readback,
            readback_raw: readback.map(|p| ((p as u16 * 255) / 100) as u8),
            pwm_enable: enable,
            rpm_before: Some(0),
            rpm_after: rpm,
            settle_ms: 6000,
            first_change_ms: None,
            readback_verdict: readback_verdict(pct, readback, enable),
            rpm_verdict: rpm_verdict(Some(0), rpm),
            ..Default::default()
        }
    }

    // ── resolve_points: the central safety invariant ─────────────────

    /// [SAFETY] The whole diagnostic rests on this: **no input reaches 0%**,
    /// for any header, any role, any caller-supplied list. Exhaustive over every
    /// `u8` rather than sampled, because a sampled check cannot prove "never".
    #[test]
    fn resolve_points_never_yields_zero_or_below_the_floor_for_any_input() {
        for floor in [0u8, 20, PUMP_FLOOR, 100] {
            let effective = floor.max(constants::CHARACTERIZATION_MIN_PCT);
            let all: Vec<u8> = (0..=255u8).collect();
            for pts in [
                resolve_points(Some(&all), floor),
                resolve_points(None, floor),
                resolve_points(Some(&[0]), floor),
                resolve_points(Some(&[]), floor),
                resolve_points(Some(&[0, 0, 0, 1, 2]), floor),
            ] {
                assert!(!pts.is_empty(), "floor {floor} produced no points");
                for p in pts {
                    assert!(p > 0, "floor {floor} produced a 0% point");
                    assert!(
                        p >= effective,
                        "floor {floor} produced {p}%, below {effective}%"
                    );
                    assert!(p <= 100, "floor {floor} produced {p}%, above 100");
                }
            }
        }
    }

    /// A pump-protected header is clamped to the daemon's own pump floor, and
    /// the default sweep's 30% first point is exactly it — so the documented
    /// default list is already legal for a pump and is not silently rewritten.
    #[test]
    fn resolve_points_clamps_a_pump_to_the_hard_floor() {
        let pts = resolve_points(Some(&[5, 10, 20, 25, 30, 50]), PUMP_FLOOR);
        assert_eq!(*pts.first().unwrap(), PUMP_FLOOR);
        assert!(pts.iter().all(|p| *p >= PUMP_FLOOR));
        assert_eq!(
            resolve_points(None, PUMP_FLOOR),
            constants::CHARACTERIZATION_DEFAULT_POINTS.to_vec()
        );
    }

    /// Ascending order is a safety property, not presentation: an abort part-way
    /// through must leave the header HIGH.
    #[test]
    fn resolve_points_sorts_ascending_dedupes_and_caps() {
        let pts = resolve_points(Some(&[100, 30, 50, 30, 100, 40]), 0);
        assert_eq!(pts, vec![30, 40, 50, 100]);
        let many: Vec<u8> = (20..=100).collect();
        let capped = resolve_points(Some(&many), 0);
        assert_eq!(capped.len(), constants::CHARACTERIZATION_MAX_POINTS);
        assert!(capped.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn resolve_settle_clamps_into_the_deadman_safe_window() {
        assert_eq!(
            resolve_settle(None).as_secs(),
            constants::CHARACTERIZATION_DEFAULT_SETTLE_S
        );
        assert_eq!(
            resolve_settle(Some(0)).as_secs(),
            constants::CHARACTERIZATION_SETTLE_MIN_S
        );
        assert_eq!(
            resolve_settle(Some(9_999)).as_secs(),
            constants::CHARACTERIZATION_SETTLE_MAX_S
        );
        // The renewal interval must stay inside the pause deadman (DEC-296).
        assert!(
            resolve_settle(Some(9_999)) * 2 <= constants::VERIFY_PAUSE_DEADMAN,
            "a maximal settle must leave the once-per-point renew inside the deadman"
        );
    }

    // ── summarise: the three axes stay independent ───────────────────

    /// The brief's core requirement: PWM accepted + read back correctly, RPM
    /// flat. This must NOT report a write failure — it is the device-override
    /// signature, and calling it a fault is the exact wrong conclusion.
    #[test]
    fn a_flat_rpm_with_a_good_readback_is_an_override_not_a_write_failure() {
        let pts: Vec<CharPoint> = [30u8, 50, 70, 100]
            .iter()
            .map(|p| point(*p, Some(*p), Some(1), Some(2800)))
            .collect();
        let s = sum(&pts);
        assert_eq!(s.command_acceptance, "pass");
        assert_eq!(s.pwm_readback, "pass");
        assert_eq!(s.rpm_response, "no_response");
        assert!(s.possible_device_override);
        assert!(!s.interference_detected);
    }

    /// A healthy pump: all three axes pass, and the summary reports the range.
    #[test]
    fn a_responsive_header_reports_all_three_axes_and_its_range() {
        let pts = vec![
            point(30, Some(30), Some(1), Some(920)),
            point(50, Some(50), Some(1), Some(1460)),
            point(70, Some(70), Some(1), Some(2140)),
            point(100, Some(100), Some(1), Some(3380)),
        ];
        let s = sum(&pts);
        assert_eq!(s.command_acceptance, "pass");
        assert_eq!(s.pwm_readback, "pass");
        assert_eq!(s.rpm_response, "responsive");
        assert_eq!(s.min_rpm, Some(920));
        assert_eq!(s.max_rpm, Some(3380));
        assert_eq!(s.min_tested_pct, Some(30));
        assert_eq!(s.max_tested_pct, Some(100));
        assert_eq!(s.monotonic, Some(true));
        assert!(!s.possible_device_override);
        assert_eq!(s.dead_zone_upper_pct, None);
    }

    /// Non-monotonic is reported as such and is NOT conflated with a PWM
    /// failure — `AIO-Phase3.md`: "Do not claim a device is faulty merely
    /// because RPM does not exactly follow PWM."
    #[test]
    fn non_monotonic_rpm_is_reported_without_implying_a_write_failure() {
        let pts = vec![
            point(30, Some(30), Some(1), Some(900)),
            point(50, Some(50), Some(1), Some(1800)),
            point(70, Some(70), Some(1), Some(1200)),
            point(100, Some(100), Some(1), Some(3000)),
        ];
        let s = sum(&pts);
        assert_eq!(s.monotonic, Some(false));
        assert_eq!(s.command_acceptance, "pass", "writes all succeeded");
        assert_eq!(
            s.pwm_readback, "pass",
            "readback was correct at every point"
        );
        assert_eq!(s.rpm_response, "responsive");
        assert!(!s.possible_device_override);
    }

    #[test]
    fn a_reverted_pwm_enable_outranks_every_other_readback_verdict() {
        let pts = vec![
            point(30, Some(30), Some(1), Some(900)),
            point(50, Some(88), Some(2), Some(2500)),
        ];
        let s = sum(&pts);
        assert_eq!(s.pwm_readback, "reverted");
        assert!(s.interference_detected);
        assert!(
            !s.possible_device_override,
            "a reclaim is interference, not a device override"
        );
    }

    #[test]
    fn a_pinned_readback_reports_the_clamp_value() {
        let pts = vec![
            point(30, Some(30), Some(1), Some(900)),
            point(60, Some(60), Some(1), Some(1800)),
            point(90, Some(75), Some(1), Some(2200)),
            point(100, Some(75), Some(1), Some(2200)),
        ];
        let s = sum(&pts);
        assert_eq!(s.pwm_readback, "clamped");
        assert_eq!(s.clamp_pct, Some(75));
    }

    #[test]
    fn a_flat_bottom_that_later_rises_reports_a_dead_zone() {
        let pts = vec![
            point(30, Some(30), Some(1), Some(800)),
            point(40, Some(40), Some(1), Some(810)),
            point(50, Some(50), Some(1), Some(805)),
            point(70, Some(70), Some(1), Some(1900)),
            point(100, Some(100), Some(1), Some(3000)),
        ];
        let s = sum(&pts).dead_zone_upper_pct;
        assert_eq!(s, Some(50));
    }

    /// A uniformly flat sweep is `no_response`, NOT a dead zone — they are
    /// different findings and conflating them would hide the important one.
    #[test]
    fn a_uniformly_flat_sweep_is_no_response_and_not_a_dead_zone() {
        let pts: Vec<CharPoint> = [30u8, 50, 70, 100]
            .iter()
            .map(|p| point(*p, Some(*p), Some(1), Some(2000)))
            .collect();
        let s = sum(&pts);
        assert_eq!(s.rpm_response, "no_response");
        assert_eq!(s.dead_zone_upper_pct, None);
    }

    #[test]
    fn a_slow_pump_is_not_a_false_no_response() {
        // 20% of 300 rpm is 60 — under tach noise. The absolute floor is what
        // stops a slow device reading as unresponsive.
        let quiet = vec![
            point(30, Some(30), Some(1), Some(300)),
            point(100, Some(100), Some(1), Some(360)),
        ];
        assert_eq!(sum(&quiet).rpm_response, "no_response");
        let real = vec![
            point(30, Some(30), Some(1), Some(300)),
            point(100, Some(100), Some(1), Some(700)),
        ];
        assert_eq!(sum(&real).rpm_response, "responsive");
    }

    #[test]
    fn no_tach_is_unavailable_rather_than_no_response() {
        let pts = vec![
            point(30, Some(30), Some(1), None),
            point(100, Some(100), Some(1), None),
        ];
        let s = sum(&pts);
        assert_eq!(s.rpm_response, "unavailable");
        assert_eq!(s.monotonic, None);
        assert!(
            !s.possible_device_override,
            "an unreadable tach proves nothing about device override"
        );
    }

    #[test]
    fn a_partly_failed_sweep_reports_partial_acceptance() {
        let mut pts = vec![point(30, Some(30), Some(1), Some(900))];
        let mut bad = point(50, None, Some(1), None);
        bad.command_accepted = false;
        pts.push(bad);
        assert_eq!(sum(&pts).command_acceptance, "partial");
        assert_eq!(sum(&[]).command_acceptance, "fail");
    }

    // ── the sweep ────────────────────────────────────────────────────

    struct Rig {
        writes: Arc<Mutex<Vec<u8>>>,
        keepalives: Arc<Mutex<usize>>,
        report: RestoreReport,
        cancel: AtomicBool,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                writes: Arc::new(Mutex::new(Vec::new())),
                keepalives: Arc::new(Mutex::new(0)),
                report: RestoreReport::new(),
                cancel: AtomicBool::new(false),
            }
        }
        fn written(&self) -> Vec<u8> {
            self.writes.lock().unwrap().clone()
        }
        /// What the run would publish. Asserting the pair together is the point:
        /// `AUD2-c` was a boolean that disagreed with the reason beside it.
        fn restore(&self) -> (bool, &'static str) {
            let outcome = self.report.get();
            (outcome.header_left_moved(), outcome.token())
        }
    }
    /// [SAFETY] `AUD3-l`: the sweep's RESTORE obeys the pump floor, not just its
    /// points.
    ///
    /// `resolve_points` has always clamped the duties written on the way in, and
    /// the module doc claimed on that basis that "0% is unreachable through this
    /// module". It was not: the restore wrote `original_pct` straight through the
    /// write path, which applies no floor. A pump header whose pre-sweep duty
    /// read 0 was swept correctly and then put back to 0 — with `pwm_enable=1`
    /// asserted by the write, which is what turns a firmware-controlled 0 into a
    /// stopped pump nothing will revise.
    ///
    /// Asserts the REALISED write log, not a re-derivation of the clamp.
    #[tokio::test]
    async fn a_pump_sweep_never_restores_to_a_stop() {
        let cache = StateCache::new();
        let rig = Rig::new();
        let writes = rig.writes.clone();
        let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;

        let writes_w = writes.clone();
        let _ = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
            floor, // restore_floor: this header IS pump-protected
            Duration::from_millis(1),
            move |p: u8| {
                writes_w.lock().unwrap().push(p);
                Ok(())
            },
            // Pre-sweep duty reads 0 — the case the row could not verify against
            // hardware, and the one the code path is unguarded for either way.
            move || sample(Some(0), Some(1), Some(0)),
            &rig.cancel,
            || false,
            || true,
            &rig.report,
            |_| {},
        )
        .await;

        let log = writes.lock().unwrap().clone();
        assert!(!log.is_empty(), "the sweep must have written something");
        for (i, &w) in log.iter().enumerate() {
            assert!(
                w >= floor,
                "write #{i} of {log:?} drove a pump-protected header to {w}%, \
                 below the {floor}% floor; the LAST entry is the restore, which \
                 is the one that used to be 0"
            );
        }
        // Name the restore explicitly, so a future change that stops restoring
        // at all cannot satisfy this test by writing nothing on the way out.
        assert_eq!(
            *log.last().unwrap(),
            floor,
            "the restore should be the captured 0 raised to the floor"
        );
    }

    /// Drive the sweep with fake hardware. `rpm_for` maps the last written duty
    /// to a tach reading; `fail_at` makes that one write fail.
    #[allow(clippy::too_many_arguments)]
    async fn sweep(
        rig: &Rig,
        cache: &StateCache,
        points: &[u8],
        settle: Duration,
        initial_pct: u8,
        enable: Option<u8>,
        rpm_for: impl Fn(u8) -> Option<u16>,
        fail_at: Option<u8>,
        shutting_down: bool,
    ) -> SweepOutcome {
        let writes = rig.writes.clone();
        let last = Arc::new(Mutex::new(initial_pct));
        let last_w = last.clone();
        let write_fn = move |pct: u8| -> Result<(), String> {
            if Some(pct) == fail_at {
                return Err("simulated write failure".into());
            }
            writes.lock().unwrap().push(pct);
            *last_w.lock().unwrap() = pct;
            Ok(())
        };
        let read_fn = move || {
            let p = *last.lock().unwrap();
            sample(Some(p), enable, rpm_for(p))
        };
        let ka = rig.keepalives.clone();
        run_sweep_uni(
            cache,
            "hwmon:test:pwm1",
            points,
            0, // restore_floor: tests exercise the non-pump path
            settle,
            write_fn,
            read_fn,
            &rig.cancel,
            move || shutting_down,
            move || {
                *ka.lock().unwrap() += 1;
                true
            },
            &rig.report,
            |_| {},
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_completed_sweep_restores_the_original_duty_last() {
        let rig = Rig::new();
        let cache = cache_at(45.0, Some("normal"));
        let out = sweep(
            &rig,
            &cache,
            &[30, 50, 100],
            Duration::from_secs(6),
            42,
            Some(1),
            |p| Some(500 + p as u16 * 20),
            None,
            false,
        )
        .await;
        assert_eq!(out.state, STATE_COMPLETE);
        assert_eq!(out.points.len(), 3);
        assert_eq!(
            rig.written(),
            vec![30, 50, 100, 42],
            "the sweep must end by writing the pre-sweep duty back"
        );
        assert_eq!(rig.restore(), (false, "restored"));
        // Liveness renewal, asserted as two RELATIONSHIPS rather than a count.
        // It was `== 3` ("one per point, not one for the whole run", DEC-296)
        // and that literal silently encoded the per-step cadence — which DEC-334
        // had to change, because a hold longer than a settle overruns the pause
        // deadman at any dwell length.
        let ka = *rig.keepalives.lock().unwrap();
        let steps = out.points.len();
        assert!(
            ka >= steps,
            "DEC-296: at least one liveness renewal per step; got {ka} for {steps}"
        );
        // DEC-334: renewal also fires INSIDE a step's hold on its own cadence.
        // This 6 s settle exceeds STABILITY_RENEW_INTERVAL_S, so a correct
        // implementation renews more than once per step; deleting the in-hold
        // renewal makes this fail while the line above still passes.
        assert!(
            Duration::from_secs(6) > Duration::from_secs(constants::STABILITY_RENEW_INTERVAL_S),
            "precondition: the settle must exceed the renewal cadence, or this \
             assertion proves nothing"
        );
        assert!(
            ka > steps,
            "a 6 s hold at a {} s renewal cadence must renew more than once per \
             step; got {ka} for {steps}",
            constants::STABILITY_RENEW_INTERVAL_S
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_sweep_stops_between_points_and_still_restores() {
        let rig = Rig::new();
        rig.cancel.store(true, Ordering::SeqCst);
        let cache = cache_at(45.0, Some("normal"));
        let out = sweep(
            &rig,
            &cache,
            &[30, 50, 100],
            Duration::from_secs(6),
            42,
            Some(1),
            |_| Some(1000),
            None,
            false,
        )
        .await;
        assert_eq!(out.state, STATE_CANCELLED);
        assert!(out.points.is_empty());
        assert_eq!(rig.written(), vec![42], "restore must run on cancellation");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_write_ends_the_sweep_records_the_point_and_restores() {
        let rig = Rig::new();
        let cache = cache_at(45.0, Some("normal"));
        let out = sweep(
            &rig,
            &cache,
            &[30, 50, 100],
            Duration::from_secs(6),
            42,
            Some(1),
            |_| Some(1000),
            Some(50),
            false,
        )
        .await;
        assert_eq!(out.state, STATE_FAILED);
        assert_eq!(out.points.len(), 2);
        assert!(!out.points[1].command_accepted);
        assert_eq!(sum(&out.points).command_acceptance, "partial");
        assert_eq!(
            rig.written(),
            vec![30, 42],
            "the failed duty was never written; the original still goes back"
        );
    }

    /// A reclaim must interrupt and be reported — not silently sweep on
    /// measuring a header somebody else is driving.
    #[tokio::test(start_paused = true)]
    async fn a_reclaimed_header_aborts_after_recording_the_point() {
        let rig = Rig::new();
        let cache = cache_at(45.0, Some("normal"));
        let out = sweep(
            &rig,
            &cache,
            &[30, 50, 100],
            Duration::from_secs(6),
            42,
            Some(2),
            |_| Some(1000),
            None,
            false,
        )
        .await;
        assert_eq!(out.state, STATE_ABORTED);
        assert_eq!(out.points.len(), 1, "stops at the first reclaimed point");
        assert!(out.detail.unwrap().contains("reclaimed"));
        assert_eq!(sum(&out.points).pwm_readback, "reverted");
        assert!(sum(&out.points).interference_detected);
        assert_eq!(rig.written(), vec![30, 42]);
    }

    // ── [HOST-a / DEC-326] the driver's full-speed alias ─────────────

    #[test]
    fn the_full_speed_alias_is_not_scored_as_reverted() {
        // enable=0 at the 100% point, duty reading back what we asked for.
        assert_eq!(readback_verdict(100, Some(100), Some(0)), "match");
        // ...and the opposite branch: a real reclaim to automatic still wins.
        assert_eq!(readback_verdict(100, Some(100), Some(2)), "reverted");
        // ...as does enable=0 at any duty that is not the one we commanded.
        assert_eq!(readback_verdict(60, Some(60), Some(0)), "reverted");
    }

    #[test]
    fn the_full_speed_alias_is_not_counted_as_interference() {
        let alias = sum(&[
            point(30, Some(30), Some(1), Some(600)),
            point(100, Some(100), Some(0), Some(1436)),
        ]);
        assert!(
            !alias.interference_detected,
            "our own duty read back is not a second writer"
        );
        assert_eq!(alias.pwm_readback, "pass");

        // Opposite branch — an actual reclaim is still reported as one.
        let real = sum(&[
            point(30, Some(30), Some(1), Some(600)),
            point(100, Some(100), Some(2), Some(1436)),
        ]);
        assert!(real.interference_detected);
        assert_eq!(real.pwm_readback, "reverted");
    }

    /// The limb that actually ends the run. `sweep`'s fixed `enable` cannot
    /// express the driver behaviour, so this rig makes the mode a FUNCTION of
    /// the duty — which is precisely what `it87.c:3612` does.
    async fn sweep_with_it87_enable(
        rig: &Rig,
        cache: &StateCache,
        points: &[u8],
        initial_pct: u8,
    ) -> SweepOutcome {
        let writes = rig.writes.clone();
        let last = Arc::new(Mutex::new(initial_pct));
        let last_w = last.clone();
        let write_fn = move |pct: u8| -> Result<(), String> {
            writes.lock().unwrap().push(pct);
            *last_w.lock().unwrap() = pct;
            Ok(())
        };
        let read_fn = move || {
            let p = *last.lock().unwrap();
            // The kernel's rule, verbatim: full scale reports mode 0.
            sample(Some(p), Some(if p == 100 { 0 } else { 1 }), Some(1000))
        };
        let ka = rig.keepalives.clone();
        run_sweep_uni(
            cache,
            "hwmon:test:pwm1",
            points,
            0,
            Duration::from_secs(6),
            write_fn,
            read_fn,
            &rig.cancel,
            move || false,
            move || {
                *ka.lock().unwrap() += 1;
                true
            },
            &rig.report,
            |_| {},
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_sweep_reaching_100_percent_completes_on_an_it87_header() {
        let rig = Rig::new();
        let cache = cache_at(45.0, Some("normal"));
        let out = sweep_with_it87_enable(&rig, &cache, &[30, 50, 100], 42).await;

        assert_eq!(
            out.state, STATE_COMPLETE,
            "every write landed; the run must not abort at its own last point"
        );
        assert_eq!(out.points.len(), 3, "all three points measured");
        let s = sum(&out.points);
        assert!(!s.interference_detected);
        assert_eq!(s.pwm_readback, "pass");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hot_sensor_aborts_the_sweep_before_writing() {
        let rig = Rig::new();
        let cache = cache_at(95.0, Some("normal"));
        let out = sweep(
            &rig,
            &cache,
            &[30, 50],
            Duration::from_secs(6),
            42,
            Some(1),
            |_| Some(1000),
            None,
            false,
        )
        .await;
        assert_eq!(out.state, STATE_ABORTED);
        assert!(out.points.is_empty());
        assert_eq!(rig.written(), vec![42], "restore still runs");
    }

    /// The 80-85 °C band: cool enough for `check_thermal_safety`, but the ladder
    /// is still forcing. The sweep must refuse, and must NOT lower the header
    /// back under the forced duty on its way out (DEC-295).
    #[tokio::test(start_paused = true)]
    async fn a_forcing_ladder_aborts_the_sweep_and_suppresses_the_restore() {
        let rig = Rig::new();
        let cache = cache_at(82.0, Some("emergency"));
        let out = sweep(
            &rig,
            &cache,
            &[30, 50],
            Duration::from_secs(6),
            42,
            Some(1),
            |_| Some(1000),
            None,
            false,
        )
        .await;
        assert_eq!(out.state, STATE_ABORTED);
        assert!(out.detail.unwrap().contains("thermal safety"));
        assert!(
            rig.written().is_empty(),
            "must not write at all — including the restore, which would lower \
             the header back under the ladder's forced duty"
        );
        // `AUD2-c`, the no-false-alarm direction: this sweep never wrote, so it
        // left the header exactly where it found it and must NOT claim otherwise.
        // The skip-is-reported direction needs a sweep that actually moved the
        // header first — `a_thermal_force_after_a_write_reports_the_skip` below.
        assert_eq!(rig.restore(), (false, "restored"));
    }

    /// `AUD2-c`: the ladder starts forcing AFTER the sweep has moved the header,
    /// which is the interleaving where the skip actually strands something. The
    /// old code published `restore_failed: false` here — "the header is back
    /// where it was" about a header parked at 50%.
    #[tokio::test(start_paused = true)]
    async fn a_thermal_force_after_a_write_reports_the_skip() {
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let report = RestoreReport::new();
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writes_w = writes.clone();
        let hot: &StateCache = &cache;

        let out = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
            0, // restore_floor: tests exercise the non-pump path
            Duration::from_secs(6),
            move |p: u8| {
                writes_w.lock().unwrap().push(p);
                // The ladder starts forcing once the header has been moved.
                hot.record_engine_tick("emergency", constants::THERMAL_EMERGENCY_TRIGGER_C);
                Ok(())
            },
            move || sample(Some(42), Some(1), Some(900)),
            &cancel,
            || false,
            || true,
            &report,
            |_| {},
        )
        .await;

        assert_eq!(out.state, STATE_ABORTED);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![30],
            "precondition: the header WAS moved, and no restore write followed it"
        );
        assert_eq!(rig_free_restore(&report), (true, "skipped_thermal_force"));
    }

    /// The same for the shutdown skip, and distinct from the drop-the-future test
    /// above: here the sweep's own loop check returns `aborted`, the future
    /// completes, and the terminal publish therefore RUNS — which is what makes
    /// the mis-report reachable by a client at all.
    #[tokio::test(start_paused = true)]
    async fn a_shutdown_after_a_write_reports_the_skip() {
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let report = RestoreReport::new();
        let going_down = Arc::new(AtomicBool::new(false));
        let flip = going_down.clone();
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writes_w = writes.clone();

        let out = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
            0, // restore_floor: tests exercise the non-pump path
            Duration::from_secs(6),
            move |p: u8| {
                writes_w.lock().unwrap().push(p);
                flip.store(true, Ordering::SeqCst);
                Ok(())
            },
            move || sample(Some(42), Some(1), Some(900)),
            &cancel,
            move || going_down.load(Ordering::SeqCst),
            || true,
            &report,
            |_| {},
        )
        .await;

        assert_eq!(out.state, STATE_ABORTED);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![30],
            "precondition: the header WAS moved, and no restore write followed it"
        );
        assert_eq!(rig_free_restore(&report), (true, "skipped_shutting_down"));
    }

    /// The third silent exit: the pre-sweep duty could not be read, so there is
    /// nothing to put the header back to — after the sweep has already moved it.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_pre_sweep_duty_is_reported_once_the_header_has_moved() {
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let report = RestoreReport::new();
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writes_w = writes.clone();

        let out = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
            0, // restore_floor: tests exercise the non-pump path
            Duration::from_secs(6),
            move |p: u8| {
                writes_w.lock().unwrap().push(p);
                Ok(())
            },
            // `pwm_percent: None` — the chip publishes a pwm file it will not read.
            || sample(None, Some(1), Some(900)),
            &cancel,
            || false,
            || true,
            &report,
            |_| {},
        )
        .await;

        assert_eq!(out.state, STATE_COMPLETE);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![30, 50],
            "precondition: the sweep really did move the header, and no restore \
             write could follow it"
        );
        assert_eq!(rig_free_restore(&report), (true, "no_original_duty"));
    }

    /// …and the same unreadable duty must NOT be reported when the sweep never
    /// moved the header. Without this the fix above would trade one false
    /// statement for another — a run that touched nothing claiming the header
    /// was left somewhere else.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_pre_sweep_duty_is_silent_when_nothing_was_written() {
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(true); // aborts before the first write
        let report = RestoreReport::new();
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writes_w = writes.clone();

        let out = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
            0, // restore_floor: tests exercise the non-pump path
            Duration::from_secs(6),
            move |p: u8| {
                writes_w.lock().unwrap().push(p);
                Ok(())
            },
            || sample(None, Some(1), Some(900)),
            &cancel,
            || false,
            || true,
            &report,
            |_| {},
        )
        .await;

        assert_eq!(out.state, STATE_CANCELLED);
        assert!(
            writes.lock().unwrap().is_empty(),
            "precondition: the header was never moved"
        );
        assert_eq!(rig_free_restore(&report), (false, "restored"));
    }

    /// `Rig::restore` for the tests that drive `run_sweep` directly.
    fn rig_free_restore(report: &RestoreReport) -> (bool, &'static str) {
        let outcome = report.get();
        (outcome.header_left_moved(), outcome.token())
    }

    /// [SAFETY] DEC-290 / 277-c, the path the loop check CANNOT cover: the
    /// runtime dropping the detached task mid-`await` at process teardown.
    ///
    /// The loop's own shutdown check stops further writes, but it only runs
    /// while the future is still being polled. When the runtime is dropped the
    /// future is dropped where it stands, and the ONLY thing left is
    /// `RestoreOnDrop` — which is exactly why its shutdown skip lives inside
    /// `Drop` rather than before the await. Modelled by dropping the future,
    /// because that is what actually happens.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_task_during_shutdown_does_not_write_a_restore() {
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let failed = RestoreReport::new();
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writes_w = writes.clone();
        let last = Arc::new(Mutex::new(42u8));
        let last_w = last.clone();
        let last_r = last.clone();

        {
            let fut = run_sweep_uni(
                &cache,
                "hwmon:test:pwm1",
                &[30, 60, 90],
                0, // restore_floor: tests exercise the non-pump path
                Duration::from_secs(6),
                move |p: u8| {
                    writes_w.lock().unwrap().push(p);
                    *last_w.lock().unwrap() = p;
                    Ok(())
                },
                move || sample(Some(*last_r.lock().unwrap()), Some(1), Some(900)),
                &cancel,
                || true, // the daemon is going down
                || true,
                &failed,
                |_| {},
            );
            tokio::pin!(fut);
            // Poll it, then abandon it — the runtime-teardown shape.
            let _ = tokio::time::timeout(Duration::from_millis(50), &mut fut).await;
        } // the future drops here, and with it RestoreOnDrop

        assert!(
            writes.lock().unwrap().is_empty(),
            "nothing may be written once shutdown has begun — the restore least \
             of all, since it would re-assert manual mode on a header the \
             shutdown restore has already handed back to firmware: {:?}",
            writes.lock().unwrap()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_restore_is_reported_rather_than_swallowed() {
        let rig = Rig::new();
        let cache = cache_at(45.0, Some("normal"));
        let out = sweep(
            &rig,
            &cache,
            &[30],
            Duration::from_secs(6),
            42,
            Some(1),
            |_| Some(1000),
            Some(42), // the restore write is the one that fails
            false,
        )
        .await;
        assert_eq!(out.state, STATE_COMPLETE);
        assert_eq!(rig.restore(), (true, "write_failed"));
    }

    #[tokio::test(start_paused = true)]
    async fn each_point_holds_its_full_settle_and_reports_first_movement() {
        let rig = Rig::new();
        let cache = cache_at(45.0, Some("normal"));
        let out = sweep(
            &rig,
            &cache,
            &[30, 100],
            Duration::from_secs(6),
            30,
            Some(1),
            |p| Some(500 + p as u16 * 20),
            None,
            false,
        )
        .await;
        assert_eq!(out.state, STATE_COMPLETE);
        for p in &out.points {
            assert!(
                p.settle_ms >= 6000,
                "point {}% held only {}ms; the settle must not exit early",
                p.requested_pct,
                p.settle_ms
            );
        }
        // The 100% step moves RPM from 1100 to 2500, so movement is detected on
        // the first sub-sample.
        assert_eq!(out.points[1].first_change_ms, Some(500));
        // The 30% step is a no-op (already at 30), so nothing ever moves.
        assert_eq!(out.points[0].first_change_ms, None);
    }

    /// [SAFETY] The regression test for the lease-expiry P1.
    ///
    /// Models the wedge **the way it actually happens** — a TTL that lapses in
    /// wall-clock time unless something renews it — rather than the way it is
    /// easy to imagine (a write that just starts failing). That distinction is
    /// DEC-278's lesson: three tests written against an imagined mechanism all
    /// passed while the real one was untouched.
    ///
    /// The header is stranded not by the sweep's writes failing but by the
    /// **restore** failing with them, so that is what this asserts.
    #[tokio::test(start_paused = true)]
    async fn the_restore_write_lands_while_the_lease_is_still_valid() {
        const TTL: Duration = Duration::from_secs(60);
        let expiry = Arc::new(Mutex::new(tokio::time::Instant::now() + TTL));
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let last = Arc::new(Mutex::new(77u8));

        let expiry_w = expiry.clone();
        let writes_w = writes.clone();
        let last_w = last.clone();
        let write_fn = move |pct: u8| -> Result<(), String> {
            if tokio::time::Instant::now() >= *expiry_w.lock().unwrap() {
                return Err("lease expired".into());
            }
            writes_w.lock().unwrap().push(pct);
            *last_w.lock().unwrap() = pct;
            Ok(())
        };
        let last_r = last.clone();
        let read_fn = move || sample(Some(*last_r.lock().unwrap()), Some(1), Some(1500));
        let expiry_k = expiry.clone();
        let keepalive = move || {
            *expiry_k.lock().unwrap() = tokio::time::Instant::now() + TTL;
            true
        };

        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let failed = RestoreReport::new();
        // A documented-legal worst case: the full 20 points at the maximum
        // settle — 300 s, five times the lease TTL.
        let points: Vec<u8> = (0..constants::CHARACTERIZATION_MAX_POINTS)
            .map(|i| 30 + i as u8)
            .collect();
        let out = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &points,
            0, // restore_floor: tests exercise the non-pump path
            Duration::from_secs(constants::CHARACTERIZATION_SETTLE_MAX_S),
            write_fn,
            read_fn,
            &cancel,
            || false,
            keepalive,
            &failed,
            |_| {},
        )
        .await;

        assert_eq!(
            out.state, STATE_COMPLETE,
            "a legal 300 s sweep must not die of an unrenewed lease: {:?}",
            out.detail
        );
        assert_eq!(
            failed.get(),
            RestoreOutcome::Restored,
            "the restore must still be able to write after 5x the lease TTL"
        );
        assert_eq!(
            *writes.lock().unwrap().last().unwrap(),
            77,
            "the LAST write must be the pre-sweep duty — a header stranded at a \
             sweep point is the actual harm this guards"
        );
    }

    /// [SAFETY] Shutdown must stop the sweep WRITING, not merely skip the
    /// restore. The task is detached, so it outlives `restore_hwmon_to_auto`.
    #[tokio::test(start_paused = true)]
    async fn a_shutdown_part_way_through_stops_writing_immediately() {
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let failed = RestoreReport::new();
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writes_w = writes.clone();
        let last = Arc::new(Mutex::new(50u8));
        let last_w = last.clone();
        let going_down = Arc::new(AtomicBool::new(false));
        let flag = going_down.clone();
        let write_fn = move |p: u8| {
            writes_w.lock().unwrap().push(p);
            *last_w.lock().unwrap() = p;
            // The daemon starts shutting down right after the first write.
            flag.store(true, Ordering::SeqCst);
            Ok(())
        };
        let last_r = last.clone();
        let out = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &[30, 60, 90],
            0, // restore_floor: tests exercise the non-pump path
            Duration::from_secs(2),
            write_fn,
            move || sample(Some(*last_r.lock().unwrap()), Some(1), Some(900)),
            &cancel,
            move || going_down.load(Ordering::SeqCst),
            || true,
            &failed,
            |_| {},
        )
        .await;
        assert_eq!(out.state, STATE_ABORTED);
        assert!(out.detail.unwrap().contains("shutting down"));
        assert_eq!(
            *writes.lock().unwrap(),
            vec![30],
            "no write may land after shutdown begins — including the restore, \
             which would re-assert manual mode on a header firmware now owns"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn every_point_published_while_running_also_appears_in_the_outcome() {
        let published: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let pub2 = published.clone();
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let failed = RestoreReport::new();
        let last = Arc::new(Mutex::new(50u8));
        let last_w = last.clone();
        let out = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &[30, 60, 90],
            0, // restore_floor: tests exercise the non-pump path
            Duration::from_secs(2),
            move |p| {
                *last_w.lock().unwrap() = p;
                Ok(())
            },
            move || sample(Some(*last.lock().unwrap()), Some(1), Some(1000)),
            &cancel,
            || false,
            || true,
            &failed,
            |pt| pub2.lock().unwrap().push(pt.requested_pct),
        )
        .await;
        assert_eq!(*published.lock().unwrap(), vec![30, 60, 90]);
        assert_eq!(
            out.points
                .iter()
                .map(|p| p.requested_pct)
                .collect::<Vec<_>>(),
            vec![30, 60, 90],
            "the progressive view and the final result must not diverge"
        );
    }

    // ── AIO Phase 8 Batch 2 (DEC-334) ────────────────────────────
    mod behaviour {
        use super::*;

        /// A point with an explicit walk direction, for the §2/§6 derivations.
        fn point_dir(
            pct: u8,
            readback: Option<u8>,
            enable: Option<u8>,
            rpm: Option<u16>,
            direction: &str,
        ) -> CharPoint {
            CharPoint {
                direction: direction.to_string(),
                ..point(pct, readback, enable, rpm)
            }
        }

        /// Drive a **bidirectional** walk with fake hardware.
        async fn sweep_bidi(
            rig: &Rig,
            cache: &StateCache,
            points: &[u8],
            initial_pct: u8,
            rpm_for: impl Fn(u8) -> Option<u16> + Send + 'static,
        ) -> SweepOutcome {
            let plan = resolve_sweep_plan(points, true, None);
            let writes = rig.writes.clone();
            let last = Arc::new(Mutex::new(initial_pct));
            let last_w = last.clone();
            run_sweep(
                cache,
                "hwmon:test:pwm1",
                &plan,
                0,
                Duration::from_millis(1),
                None,
                move |pct: u8| {
                    writes.lock().unwrap().push(pct);
                    *last_w.lock().unwrap() = pct;
                    Ok(())
                },
                move || {
                    let p = *last.lock().unwrap();
                    sample(Some(p), Some(1), rpm_for(p))
                },
                &rig.cancel,
                || false,
                || true,
                &rig.report,
                |_| {},
            )
            .await
        }

        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};

        fn duties(plan: &[SweepStep]) -> Vec<u8> {
            plan.iter().map(|s| s.pct).collect()
        }
        fn dirs(plan: &[SweepStep]) -> Vec<&'static str> {
            plan.iter().map(|s| s.direction.token()).collect()
        }

        // ── §1 plan resolution ───────────────────────────────────────────

        #[test]
        fn a_unidirectional_plan_is_the_ascending_list_it_always_was() {
            let plan = resolve_sweep_plan(&[30, 50, 100], false, None);
            assert_eq!(duties(&plan), vec![30, 50, 100]);
            // DEC-313 decision 5 is unchanged for this mode.
            assert!(duties(&plan).windows(2).all(|w| w[0] < w[1]));
        }

        /// [SAFETY] Q1. The walk descends from the top and climbs back, so the run
        /// ENDS at the highest duty. `RestoreOnDrop` has four exits that leave the
        /// header where the sweep put it, and ending high keeps all four benign.
        #[test]
        fn a_bidirectional_walk_ends_at_the_highest_duty() {
            let plan = resolve_sweep_plan(&[30, 40, 50], true, None);
            assert_eq!(duties(&plan), vec![50, 40, 30, 40, 50]);
            assert_eq!(
                *duties(&plan).last().expect("non-empty"),
                *duties(&plan).iter().max().expect("non-empty"),
                "the walk must end at its maximum, or an aborted restore leaves the \
             header low"
            );
        }

        /// The first step is entered from the captured pre-sweep duty, so its
        /// approach direction is unknown. Labelling it `falling` would put a
        /// wrong-direction reading into the hysteresis comparison (DEC-325).
        #[test]
        fn the_first_step_of_either_walk_is_a_ramp() {
            assert_eq!(
                dirs(&resolve_sweep_plan(&[30, 40, 50], true, None))[0],
                "ramp"
            );
            assert_eq!(
                dirs(&resolve_sweep_plan(&[30, 40, 50], false, None))[0],
                "ramp"
            );
        }

        #[test]
        fn the_two_legs_are_labelled_by_the_direction_they_are_walked() {
            let plan = resolve_sweep_plan(&[30, 40, 50], true, None);
            assert_eq!(
                dirs(&plan),
                vec!["ramp", "falling", "falling", "rising", "rising"]
            );
        }

        /// The turn-around duty is walked once, not twice: the header is already
        /// sitting on it and it cannot be approached from below without breaching
        /// the floor.
        #[test]
        fn the_turnaround_duty_is_not_repeated() {
            let plan = resolve_sweep_plan(&[30, 40, 50, 60], true, None);
            assert_eq!(plan.iter().filter(|s| s.pct == 30).count(), 1);
            assert_eq!(plan.len(), 2 * 4 - 1);
        }

        /// [SAFETY] §10: "rising and falling sweeps respect safe minimum" and "pump
        /// sweeps never include 0%". Exhaustive over every floor and every u8 the
        /// caller could ask for, in BOTH directions.
        #[test]
        fn no_walked_duty_is_ever_zero_or_below_the_floor_in_either_direction() {
            for floor in [0u8, 20, 30, 100] {
                for bidi in [false, true] {
                    for raw in 0u8..=255 {
                        let points = resolve_points(Some(&[raw, raw / 2, 100]), floor);
                        let plan = resolve_sweep_plan(&points, bidi, None);
                        let effective = floor.max(constants::CHARACTERIZATION_MIN_PCT);
                        for step in &plan {
                            assert!(
                                step.pct >= effective && step.pct > 0 && step.pct <= 100,
                                "floor {floor} bidi {bidi} raw {raw}: walked {}",
                                step.pct
                            );
                        }
                    }
                }
            }
        }

        /// Q4: the cap is on WALKED STEPS, so the worst case and the engine
        /// write-pause it budgets do not move when a walk doubles back.
        #[test]
        fn a_bidirectional_walk_never_exceeds_the_total_step_cap() {
            // 17 duties: enough that a bidirectional walk must thin them, but inside
            // `resolve_points`' own pre-existing cap so this test measures the
            // thinning rather than that truncation.
            let many: Vec<u8> = (20..=100).step_by(5).collect();
            let points = resolve_points(Some(&many), 0);
            assert_eq!(
                points.len(),
                many.len(),
                "precondition: resolve_points must not have truncated, or this test \
             measures the wrong cap"
            );
            let plan = resolve_sweep_plan(&points, true, None);
            assert!(
                plan.len() <= constants::CHARACTERIZATION_MAX_POINTS,
                "walked {} steps, cap is {}",
                plan.len(),
                constants::CHARACTERIZATION_MAX_POINTS
            );
            assert!(
                plan.len() > constants::CHARACTERIZATION_MAX_UNIQUE_BIDIRECTIONAL,
                "precondition: the walk must actually double back"
            );
            // Thinning keeps the RANGE — truncating to the first N would have
            // dropped the top of the sweep, which is the part being characterised.
            assert_eq!(duties(&plan)[0], 100, "the walk starts at the maximum");
            assert_eq!(*duties(&plan).last().expect("non-empty"), 100);
            assert!(duties(&plan).contains(&20), "and still reaches the minimum");
        }

        #[test]
        fn a_dwell_is_requested_by_duration_and_placed_by_the_daemon() {
            let plan =
                resolve_sweep_plan(&[30, 40, 50, 60, 70], true, Some(Duration::from_secs(20)));
            let dwelled: Vec<u8> = plan
                .iter()
                .filter(|s| s.dwell.is_some())
                .map(|s| s.pct)
                .collect();
            assert!(
                dwelled.len() <= constants::STABILITY_MAX_POINTS,
                "the daemon bounds how many steps dwell, got {dwelled:?}"
            );
            assert!(!dwelled.is_empty());
        }

        #[test]
        fn no_dwell_is_assigned_when_none_is_requested() {
            let plan = resolve_sweep_plan(&[30, 40, 50], true, None);
            assert!(plan.iter().all(|s| s.dwell.is_none()));
            assert_eq!(resolve_stability_dwell(None), None);
            assert_eq!(
                resolve_stability_dwell(Some(0)),
                None,
                "0 means off, not 0 s"
            );
        }

        #[test]
        fn a_requested_dwell_is_clamped_both_ways() {
            assert_eq!(
                resolve_stability_dwell(Some(u64::MAX)),
                Some(Duration::from_secs(constants::STABILITY_MAX_S))
            );
            assert_eq!(
                resolve_stability_dwell(Some(1)),
                Some(Duration::from_secs(constants::STABILITY_MIN_S))
            );
        }

        // ── [SAFETY] the dwell's deadman/lease cadence ───────────────────

        /// **The highest-value test in DEC-334.** A dwell renewing once per step
        /// overruns the engine-pause deadman at *any* dwell length — the settle
        /// invariant holds at exactly `15 * 2 == 30`, with zero headroom — and at
        /// `STABILITY_MAX_S` it also outlives the 60 s hwmon lease, which
        /// `constants.rs` records as having once left a header un-restorable.
        ///
        /// So assert the REALISED gap between renewals, not a re-derivation of the
        /// rule's arithmetic (DEC-320), and assert a precondition that the dwell was
        /// genuinely long enough to break it — otherwise a short dwell would pass
        /// this while proving nothing (DEC-314).
        #[tokio::test(start_paused = true)]
        async fn the_longest_dwell_never_lets_a_renewal_gap_reach_the_deadman() {
            let cache = StateCache::new();
            let cancel = AtomicBool::new(false);
            let report = RestoreReport::new();
            let stamps: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
            let start = tokio::time::Instant::now();

            let dwell = Duration::from_secs(constants::STABILITY_MAX_S);
            let deadman = constants::VERIFY_PAUSE_DEADMAN;
            let lease = crate::hwmon::lease::DEFAULT_LEASE_TTL;
            // Precondition: without it, a dwell shorter than the deadman would pass
            // this test with the in-hold renewal deleted.
            assert!(
                dwell > deadman,
                "precondition: the longest dwell ({dwell:?}) must exceed the pause \
             deadman ({deadman:?}), or this test cannot detect the defect"
            );

            let plan = vec![SweepStep {
                pct: 50,
                direction: Direction::Ramp,
                dwell: Some(dwell),
            }];
            let stamps_k = stamps.clone();
            let _ = run_sweep(
                &cache,
                "hwmon:test:pwm1",
                &plan,
                0,
                Duration::from_secs(constants::CHARACTERIZATION_SETTLE_MIN_S),
                None,
                |_p: u8| Ok(()),
                || sample(Some(50), Some(1), Some(1200)),
                &cancel,
                || false,
                move || {
                    stamps_k.lock().unwrap().push(start.elapsed());
                    true
                },
                &report,
                |_| {},
            )
            .await;

            let mut marks = stamps.lock().unwrap().clone();
            assert!(marks.len() > 2, "expected repeated renewal, got {marks:?}");
            // The window closes at the end of the hold, so the final gap is measured
            // to the run's end rather than to another renewal.
            marks.push(Duration::from_secs(constants::CHARACTERIZATION_SETTLE_MIN_S) + dwell);
            let worst = marks
                .windows(2)
                .map(|w| w[1].saturating_sub(w[0]))
                .max()
                .expect("at least one gap");
            assert!(
                worst < deadman,
                "a renewal gap of {worst:?} reaches the {deadman:?} pause deadman"
            );
            assert!(
                worst < lease,
                "a renewal gap of {worst:?} reaches the {lease:?} hwmon lease TTL"
            );
        }

        // ── §1 restore across a two-direction sequence ───────────────────

        /// §1: "Restore original state after the full sequence or any interruption."
        #[tokio::test(start_paused = true)]
        async fn a_completed_two_direction_sweep_restores_the_original_duty_last() {
            let rig = Rig::new();
            let cache = StateCache::new();
            let out = sweep_bidi(&rig, &cache, &[30, 50, 100], 42, |p| {
                Some(500 + u16::from(p) * 20)
            })
            .await;
            assert_eq!(out.state, STATE_COMPLETE);
            let w = rig.written();
            assert_eq!(
                w,
                vec![100, 50, 30, 50, 100, 42],
                "down from the top, back up, then the pre-sweep duty"
            );
            assert_eq!(rig.restore(), (false, "restored"));
        }

        /// §10: "cancellation between sweep directions restores state."
        #[tokio::test(start_paused = true)]
        async fn cancelling_between_the_two_legs_still_restores() {
            let rig = Rig::new();
            let cache = StateCache::new();
            let cancel_after = Arc::new(Mutex::new(0usize));
            let seen = cancel_after.clone();
            let flag = &rig.cancel;
            let writes = rig.writes.clone();
            let last = Arc::new(Mutex::new(42u8));
            let last_w = last.clone();
            let plan = resolve_sweep_plan(&[30, 50, 100], true, None);
            let turn = plan
                .iter()
                .position(|s| s.direction == Direction::Rising)
                .expect("a bidirectional plan has a rising leg");
            let report = RestoreReport::new();
            let out = run_sweep(
                &cache,
                "hwmon:test:pwm1",
                &plan,
                0,
                Duration::from_millis(1),
                None,
                move |p: u8| {
                    writes.lock().unwrap().push(p);
                    *last_w.lock().unwrap() = p;
                    let mut n = seen.lock().unwrap();
                    *n += 1;
                    Ok(())
                },
                move || {
                    let p = *last.lock().unwrap();
                    sample(Some(p), Some(1), Some(500 + u16::from(p) * 20))
                },
                flag,
                || false,
                || true,
                &report,
                {
                    let flag2 = &rig.cancel;
                    let counter = cancel_after.clone();
                    move |_pt: CharPoint| {
                        // Trip the cancel exactly at the turn-around, i.e. between
                        // the falling and rising legs.
                        if *counter.lock().unwrap() == turn {
                            flag2.store(true, Ordering::SeqCst);
                        }
                    }
                },
            )
            .await;
            assert_eq!(out.state, STATE_CANCELLED);
            assert!(
                out.points.len() < plan.len(),
                "precondition: the cancel must land mid-walk, not after it"
            );
            let w = rig.written();
            assert_eq!(
                *w.last().expect("wrote something"),
                42,
                "a cancel between the legs still restores the pre-sweep duty: {w:?}"
            );
        }

        /// [SAFETY] The pump floor holds on the way DOWN too — which is the leg that
        /// did not exist before DEC-334.
        #[tokio::test(start_paused = true)]
        async fn a_bidirectional_pump_sweep_never_writes_below_its_floor() {
            let rig = Rig::new();
            let cache = StateCache::new();
            let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
            let points = resolve_points(Some(&[0, 5, 10, 50, 100]), floor);
            let plan = resolve_sweep_plan(&points, true, None);
            let writes = rig.writes.clone();
            let report = RestoreReport::new();
            let _ = run_sweep(
                &cache,
                "hwmon:test:pwm1",
                &plan,
                floor,
                Duration::from_millis(1),
                None,
                move |p: u8| {
                    writes.lock().unwrap().push(p);
                    Ok(())
                },
                || sample(Some(0), Some(1), Some(900)),
                &rig.cancel,
                || false,
                || true,
                &report,
                |_| {},
            )
            .await;
            // Asserts the REALISED write log, not a re-derivation of the clamp.
            for w in rig.written() {
                assert!(
                    w >= floor,
                    "wrote {w}% to a pump-protected header: {:?}",
                    rig.written()
                );
            }
        }

        // ── review remediation: the two P2s the concurrency pass found ───

        /// [C1] **A cancel during a plain settle must not discard the point.**
        ///
        /// With no dwell `hold == settle`, and the last loop iteration sleeps exactly
        /// the remainder — so `elapsed() >= settle` is true on the final tick of
        /// EVERY settle. The first draft of the mid-hold cancel check was not gated
        /// on `step.dwell`, so it returned there before `read_fn()`, dropping a
        /// point that had completed its full window and labelling the run
        /// "cancelled during the stability hold" on a run with no hold at all.
        ///
        /// Deleting `step.dwell.is_some() &&` from the guard makes this fail.
        #[tokio::test(start_paused = true)]
        async fn a_cancel_during_a_plain_settle_still_records_the_point() {
            let cache = StateCache::new();
            let cancel = AtomicBool::new(false);
            let report = RestoreReport::new();
            let plan = plan_of(&[50, 80]);
            assert!(
                plan.iter().all(|s| s.dwell.is_none()),
                "precondition: this test only means something on a plan with NO dwell"
            );
            let reads = Arc::new(Mutex::new(0usize));
            let reads_r = reads.clone();
            let flag = &cancel;
            let out = run_sweep(
                &cache,
                "hwmon:test:pwm1",
                &plan,
                0,
                Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
                None,
                |_p: u8| Ok(()),
                move || {
                    let mut n = reads_r.lock().unwrap();
                    *n += 1;
                    // Mid-settle, not before it and not between steps.
                    if *n == 3 {
                        flag.store(true, Ordering::SeqCst);
                    }
                    sample(Some(50), Some(1), Some(1200))
                },
                &cancel,
                || false,
                || true,
                &report,
                |_| {},
            )
            .await;
            assert_eq!(out.state, STATE_CANCELLED);
            assert_eq!(
                out.points.len(),
                1,
                "the step whose settle completed must still be recorded; got {:?}",
                out.points
            );
            assert!(
                !out.detail
                    .clone()
                    .unwrap_or_default()
                    .contains("stability hold"),
                "a run with no dwell must not report a stability hold: {:?}",
                out.detail
            );
        }

        /// [C2] **[SAFETY] The thermal abort is re-evaluated INSIDE the hold.**
        ///
        /// A step used to be at most one settle; with a dwell it is up to 75 s, so
        /// checking only at the top of the step stretched the worst-case latency on
        /// the 85 °C voluntary-operation abort five-fold. The >=105 °C ladder was
        /// never the exposure — it force-takes the lease, so `keepalive()` catches
        /// it — but nothing backstopped the band between the two.
        ///
        /// Sets the sensor hot only AFTER the sweep has entered the hold, so the
        /// pre-existing entry check cannot be what catches it.
        #[tokio::test(start_paused = true)]
        async fn a_sensor_that_goes_hot_during_a_dwell_aborts_before_the_hold_ends() {
            let cache = StateCache::new();
            let cancel = AtomicBool::new(false);
            let report = RestoreReport::new();
            let dwell = Duration::from_secs(constants::STABILITY_MAX_S);
            let settle = Duration::from_secs(constants::CHARACTERIZATION_SETTLE_MIN_S);
            let plan = vec![SweepStep {
                pct: 50,
                direction: Direction::Ramp,
                dwell: Some(dwell),
            }];
            let reads = Arc::new(Mutex::new(0usize));
            let reads_r = reads.clone();
            let cache_w = &cache;
            let out = run_sweep(
                &cache,
                "hwmon:test:pwm1",
                &plan,
                0,
                settle,
                None,
                |_p: u8| Ok(()),
                move || {
                    let mut n = reads_r.lock().unwrap();
                    *n += 1;
                    if *n == 2 {
                        // Well inside the hold, and above CALIBRATION_MAX_TEMP_C.
                        cache_w
                            .update_sensors(vec![hot_cpu(constants::CALIBRATION_MAX_TEMP_C + 5.0)]);
                    }
                    sample(Some(50), Some(1), Some(1200))
                },
                &cancel,
                || false,
                || true,
                &report,
                |_| {},
            )
            .await;
            assert_eq!(out.state, STATE_ABORTED, "detail: {:?}", out.detail);
            // The REALISED bound, not a re-derivation: the abort must land inside a
            // renewal interval plus a sample, not at the end of the 60 s dwell.
            let observed = *reads.lock().unwrap() as u64;
            let worst_ticks = (constants::STABILITY_RENEW_INTERVAL_S * 1000
                / constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64)
                + 2;
            assert!(
                observed <= worst_ticks,
                "aborted after {observed} samples; a renewal-cadence check bounds it at \
             {worst_ticks}, and the whole dwell would be {}",
                dwell.as_millis() as u64
                    / constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64
            );
        }

        // ── §7 correction ────────────────────────────────────────────────

        /// §7: "keep raw reported RPM in all exports". The correction is additive
        /// evidence, never a replacement.
        #[test]
        fn a_correction_never_overwrites_the_reported_rpm() {
            let est = estimate_physical_rpm(
                Some(1500),
                Some(RpmCorrection {
                    // A 3-pulse tach reported as 1.5x actual: exact in binary, so the
                    // test asserts the correction rather than a rounding mode.
                    factor: 2.0 / 3.0,
                    source: "test cooler",
                }),
            )
            .expect("a factor produces an estimate");
            assert_eq!(est.value, 1000);
            assert_eq!(est.provenance, "DERIVED");
            assert_eq!(est.correction_source, "test cooler");
        }

        /// §7: with no trusted metadata there is NO estimate — not a relabelled copy
        /// of the reported figure. Promoting an observation into a derived value is
        /// exactly what the Overview's provenance rule forbids.
        #[test]
        fn no_correction_means_no_estimated_value_at_all() {
            assert!(estimate_physical_rpm(Some(1500), None).is_none());
            assert!(estimate_physical_rpm(
                None,
                Some(RpmCorrection {
                    factor: 2.0,
                    source: "x"
                })
            )
            .is_none());
        }

        #[test]
        fn a_nonsense_correction_factor_is_refused_rather_than_applied() {
            for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
                assert!(
                    estimate_physical_rpm(
                        Some(1500),
                        Some(RpmCorrection {
                            factor: bad,
                            source: "x"
                        })
                    )
                    .is_none(),
                    "factor {bad} must not produce an estimate"
                );
            }
        }

        /// §10: "untrusted profile/user input cannot ... silently define tach
        /// correction." Enforced by the TYPE, not by a runtime check: `DevicePolicy`
        /// derives no `Deserialize`, so there is no path from a payload to a factor.
        #[test]
        fn no_shipped_device_policy_defines_a_tach_correction() {
            for policy in crate::hwmon::device_policy::all_policies() {
                assert!(
                    policy.rpm_correction_factor.is_none(),
                    "{} ships a correction factor; §7 requires validated per-device \
                 evidence before one is added, and the GUI must show it as \
                 DEVICE_METADATA rather than an observation",
                    policy.id
                );
            }
        }

        // ── §5 / §6 summary derivations ──────────────────────────────────

        /// §5: "Do not publish unrealistic millisecond precision." The summary must
        /// name the resolution its timings were measured at.
        #[tokio::test(start_paused = true)]
        async fn the_summary_publishes_the_resolution_its_timings_were_measured_at() {
            let rig = Rig::new();
            let cache = StateCache::new();
            let out = sweep_bidi(&rig, &cache, &[30, 100], 42, |p| {
                Some(500 + u16::from(p) * 20)
            })
            .await;
            let s = summarise(&out.points, &[]);
            assert_eq!(
                s.measurement_resolution_ms,
                Some(constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64),
                "the timings can only be multiples of the sub-sample cadence"
            );
        }

        /// The flag is derived from the POINTS, not from the request (DEC-325): a run
        /// that aborted before its rising leg is honestly unidirectional.
        #[test]
        fn hysteresis_is_not_tested_when_the_walk_produced_one_direction() {
            let uni = vec![
                point_dir(30, Some(30), Some(1), Some(900), "ramp"),
                point_dir(50, Some(50), Some(1), Some(1500), "rising"),
            ];
            assert_eq!(
                summarise(&uni, &[]).hysteresis_verdict,
                crate::api::stats::HYSTERESIS_NOT_TESTED
            );
        }

        #[test]
        fn hysteresis_is_measured_at_duties_walked_in_both_directions() {
            let pts = vec![
                point_dir(100, Some(100), Some(1), Some(3000), "ramp"),
                point_dir(50, Some(50), Some(1), Some(2000), "falling"),
                point_dir(30, Some(30), Some(1), Some(900), "falling"),
                point_dir(50, Some(50), Some(1), Some(1200), "rising"),
                point_dir(100, Some(100), Some(1), Some(3000), "rising"),
            ];
            let s = summarise(&pts, &[]);
            assert_eq!(s.hysteresis_compared_points, 1, "only 50% has both legs");
            assert_eq!(s.hysteresis_worst_duty_pct, Some(50));
            assert_eq!(s.hysteresis_worst_delta_rpm, Some(800));
            assert_eq!(s.hysteresis_verdict, crate::api::stats::HYSTERESIS_PRESENT);
        }

        /// §6: three states, and "no model yet" is the one that must not read as a
        /// pass. The Overview: "Do not turn lack of evidence into PASS."
        #[test]
        fn an_unlearned_header_reports_no_comparison_rather_than_agreement() {
            let pts = vec![point_dir(50, Some(50), Some(1), Some(2000), "rising")];
            assert_eq!(summarise(&pts, &[]).outside_learned_range, None);
        }

        #[test]
        fn a_reading_inside_the_learned_band_is_false_not_none() {
            let pts = vec![point_dir(50, Some(50), Some(1), Some(2000), "rising")];
            let learned = [LearnedPoint {
                duty_pct: 50,
                rpm_min: 1900,
                rpm_max: 2100,
            }];
            assert_eq!(summarise(&pts, &learned).outside_learned_range, Some(false));
        }

        /// §6's worked example, and §8.5's rule that this must never render as a
        /// hardware failure.
        #[test]
        fn a_reading_far_outside_the_learned_band_is_observed_with_possibilities() {
            let pts = vec![point_dir(35, Some(35), Some(1), Some(3350), "rising")];
            let learned = [LearnedPoint {
                duty_pct: 35,
                rpm_min: 900,
                rpm_max: 1150,
            }];
            let s = summarise(&pts, &learned);
            assert_eq!(s.outside_learned_range, Some(true));
            assert!(
                s.learned_range_note
                    .as_deref()
                    .is_some_and(|n| n.contains("900") && n.contains("1150")),
                "the note must name the learned band: {:?}",
                s.learned_range_note
            );
            assert!(
                s.interpretation_states
                    .iter()
                    .all(|t| t.ends_with("_POSSIBLE")),
                "§6 states are possibilities, never conclusions: {:?}",
                s.interpretation_states
            );
        }

        /// §10: "override detection requires command/readback success before
        /// suggesting device override."
        #[test]
        fn a_failed_readback_does_not_suggest_a_device_override() {
            let mut p = point_dir(35, Some(80), Some(1), Some(3350), "rising");
            p.readback_verdict = "clamped".into();
            let learned = [LearnedPoint {
                duty_pct: 35,
                rpm_min: 900,
                rpm_max: 1150,
            }];
            let s = summarise(&[p], &learned);
            assert!(
                !s.interpretation_states
                    .iter()
                    .any(|t| t == "DEVICE_OVERRIDE_POSSIBLE"),
                "readback did not succeed, so an internal override is not the \
             indicated explanation: {:?}",
                s.interpretation_states
            );
        }

        // ── §9 provenance ────────────────────────────────────────────────

        #[test]
        fn the_provenance_legend_classifies_the_commanded_observed_and_derived_split() {
            let m = provenance_legend();
            assert_eq!(
                m.get("requested_pct").map(String::as_str),
                Some("COMMANDED")
            );
            assert_eq!(m.get("rpm_after").map(String::as_str), Some("OBSERVED"));
            assert_eq!(
                m.get("estimated_physical_rpm").map(String::as_str),
                Some("DERIVED")
            );
            assert_eq!(
                m.get("correction_source").map(String::as_str),
                Some("DEVICE_METADATA")
            );
        }

        /// A raw observation must never be classified as derived, or the legend
        /// would license exactly the silent promotion the Overview forbids.
        #[test]
        fn no_raw_tach_or_readback_field_is_classified_as_derived() {
            let m = provenance_legend();
            for raw in [
                "rpm_before",
                "rpm_after",
                "readback_pct",
                "readback_raw",
                "pwm_enable",
            ] {
                assert_eq!(
                    m.get(raw).map(String::as_str),
                    Some("OBSERVED"),
                    "{raw} is a direct hwmon reading"
                );
            }
        }
    }
}
