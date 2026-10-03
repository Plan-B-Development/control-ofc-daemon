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
//!   that nothing will revise. `RestoreGuard::restore_floor` now clamps it.
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
//!   completed run at the LOWEST duty, and `RestoreGuard` has five exits that
//!   leave the header where the sweep put it — the two deliberate skips, an
//!   unreadable pre-sweep duty, a read or a write that did not return (DEC-420,
//!   DEC-455), and a
//!   shutdown whose `hand_back_hwmon` found no `pwmN_enable` to hand back
//!   (`main.rs`, `NothingToRestore` / `WritesTimedOut` / `Unresolvable`).
//!   Ending high keeps all five benign.
//! - The invariant that holds in **both** modes, and the one to reason from:
//!   **no walked duty is ever below `max(CHARACTERIZATION_MIN_PCT, floor)`.**
//! - The pre-sweep duty is restored by [`RestoreGuard`] on every exit path on
//!   which nothing else owns the header and its driver still answers —
//!   completion, cancellation, a failed write, a reclaim, and a thermal abort
//!   below the forcing threshold. (The same narrowing DEC-295 applied to
//!   DEC-134's identical claim for calibrate.)
//! - The restore is skipped while the thermal ladder is forcing (DEC-295) and
//!   while the daemon is shutting down (DEC-290) — in both cases something with
//!   more authority owns the header — and after a read that did not return
//!   (DEC-420), because the restore's write path could hang holding the
//!   controller lock; a header that became a pump during the run is restored,
//!   floored, even then. After a WRITE that did not return (DEC-455) nothing
//!   more is written at all, pump or not: that write still holds the lock, and
//!   anything written after it would queue behind it and land whenever it did.
//! - The restore is an explicit step every exit awaits
//!   ([`RestoreGuard::restore`]), before the lease guard drops. A run dropped
//!   before it — a panic — writes nothing more and logs; the engine's next
//!   tick drives or hands back the header (DEC-382, DEC-455).
//! - **A skipped restore is reported, not silently reported as a success.**
//!   [`RestoreOutcome`] records which of the six exits the guard actually took,
//!   and `restore_failed` is derived from it, so "the header is back where it
//!   was" is answerable from the wire on every path.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::calibration::thermal_force_state;
use crate::api::diagnostic_gates::{
    note_write_unresponsive, pump_protected_mid_run_detail, step_gate, thermal_gate, GateStop,
    PumpWatch, WriteFailure,
};
use crate::api::responses::HwmonVerifyState;
use crate::constants;
use crate::health::cache::StateCache;

/// The diagnostic this module IS, named once (DEC-385) — the per-point staleness
/// refusals key on it, as the POST handler's does, so none of them can be gated
/// on a different diagnostic than the preflight the operator was shown. The
/// twin of `discovery::DISCOVERY_DIAGNOSTIC`.
pub const CHARACTERIZATION_DIAGNOSTIC: crate::api::preflight::Diagnostic =
    crate::api::preflight::Diagnostic::Characterization;

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
    /// `stable` | `variable` | `unstable` | `insufficient_data` | `unavailable`
    /// | `not_settled` (DEC-405, daemon >= 2.52.0).
    /// An opaque token: render an unrecognised one, never drop it (273-i).
    pub verdict: String,
    /// The cadence these readings were actually taken at. Published so no client
    /// has to assume one — `§5` forbids implying resolution the data lacks.
    pub sample_interval_ms: u64,
    /// How much of the hold was dwell rather than settle. `0` for a step the
    /// daemon did not select.
    pub dwell_ms: u64,
    /// DEC-405 (`PTR-a`). Where, measured from the write, the window these
    /// figures describe opened: the point's `settled_ms` when it settled, so the
    /// settling transient is excluded, and `0` when it never settled — the
    /// figures then cover the whole hold and `verdict` is `not_settled`.
    /// `samples` and `dropouts` always cover the whole hold — raw evidence is
    /// never trimmed (`§9`).
    #[serde(default)]
    pub window_start_ms: u64,
    /// DEC-405 (`PTR-b`). The tach register's refresh interval as this hold
    /// observed it — the median gap between changes of value. `None` when the
    /// value changed fewer than twice, which is UNKNOWN, not fast.
    #[serde(default)]
    pub update_interval_ms: Option<u64>,
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

/// Project one hold's statistics onto the wire, over its settled tail
/// (DEC-405): `settled_ms` is the same value the point publishes.
fn point_stability(
    samples: &[crate::api::stats::RpmSample],
    dwell: Duration,
    settled_ms: Option<u64>,
) -> PointStability {
    let (st, window_start_ms) = crate::api::stats::settled_rpm_stats(samples, settled_ms);
    let observed: Vec<(u64, Option<u16>)> = samples.iter().map(|s| (s.at_ms, s.rpm)).collect();
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
        window_start_ms,
        update_interval_ms: crate::api::stats::update_interval_ms(&observed),
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
    /// Time from the write to the first reading whose RPM had moved away from
    /// `rpm_before` by more than the threshold `rpm_verdict` uses (`PTR-y`) —
    /// a hold sub-sample, or the after-read at `settle_ms` — so a `changed`
    /// point carries one unless no hold sample was readable. `None` means it
    /// never moved (or the tach was unreadable) — not that it responded
    /// instantly.
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
    ///
    /// DEC-405 (`PTR-d`): in a bidirectional run this is judged per leg, each
    /// sorted by duty — `false` if any judged leg is, `true` if at least one leg
    /// was judged and none is `false`, `None` if no leg could be. Before, it was
    /// judged in walk order, and a walk that descends then climbs is `false` for
    /// every working fan. Unidirectional runs are judged in walk order, as ever.
    pub monotonic: Option<bool>,
    /// DEC-405. The falling leg alone, by duty. `None` for a unidirectional run
    /// or a leg with fewer than two usable readings.
    #[serde(default)]
    pub monotonic_falling: Option<bool>,
    /// DEC-405. The rising leg alone, by duty. Same `None` rule.
    #[serde(default)]
    pub monotonic_rising: Option<bool>,
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

/// What a running diagnostic is doing **right now** (`P8-bg`, daemon >= 2.55.0).
///
/// Shared by [`CharacterizationRun`] and
/// [`ControlPathRun`](crate::api::discovery::ControlPathRun). Both publish a
/// result only when a hold ENDS — a point after its settle and any dwell, a
/// cycle after two windows — so without this a healthy run was silent for up
/// to 26 s at a time, and indistinguishable from a wedged one. It changes at
/// every phase boundary, and is `None` before the first write and once the run
/// is terminal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RunStep {
    /// Characterisation: `settle` | `dwell`. Discovery: `settle_wait` |
    /// `baseline` | `perturbed`. An opaque token: render an unrecognised one,
    /// never drop it (273-i).
    pub phase: String,
    /// Characterisation: the 0-based `step_index` the held point will carry.
    /// Discovery: the 1-based cycle, as `DiscoveryCycle::cycle` numbers it.
    pub index: u16,
    /// The duty being held during this phase.
    pub duty_pct: u8,
    /// When this phase began, on the wall clock `completed_unix_ms` uses — so a
    /// client on the same host can show how long it has been held.
    pub started_unix_ms: u64,
    /// The phase's upper bound. A phase can end sooner (a settle-wait that
    /// settles, a cancel, an abort) but never later, save I/O overhead.
    pub max_ms: u64,
}

impl RunStep {
    pub fn now(phase: &str, index: u16, duty_pct: u8, max: Duration) -> Self {
        Self {
            phase: phase.to_string(),
            index,
            duty_pct,
            started_unix_ms: crate::control_paths::unix_ms(),
            max_ms: max.as_millis() as u64,
        }
    }
}

pub const STEP_PHASE_SETTLE: &str = "settle";
pub const STEP_PHASE_DWELL: &str = "dwell";

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
    /// read, in which case there is nothing to put back — **and, since DEC-420,
    /// also means "not yet known"**: the `202` and every `running` snapshot
    /// carry `None`, because the value is the sweep's own pre-sweep read and is
    /// published only on the terminal run.
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
    /// `P8-bg`: the phase being held right now. `None` before the first write
    /// and once terminal. `serde(default)` so a run persisted by an older
    /// daemon inside a validation session still loads.
    #[serde(default)]
    pub current_step: Option<RunStep>,
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

/// Which exit [`RestoreGuard`] took — the single source of truth for both
/// `restore_failed` and `restore_outcome` on the wire.
///
/// [SAFETY-adjacent] Three of these six are *deliberate* skips, not faults, and
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
    /// Skipped: a read of the header did not return within
    /// [`constants::DIAGNOSTIC_READ_BUDGET`], so the run writes nothing more to
    /// it (DEC-420, the user's choice at review). A restore goes through
    /// `set_pwm`, whose own sysfs reads are unbounded and run under the
    /// controller lock, so on a driver that has stopped answering it could hold
    /// that lock — the one the engine and the thermal force need — for as long
    /// as the driver does. The header is left at the last swept duty, which is
    /// never below `max(20, its floor)`.
    SkippedUnresponsive = 6,
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
            Self::SkippedUnresponsive => "skipped_unresponsive",
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
            6 => Self::SkippedUnresponsive,
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
    // `P8-g`: THIN, never truncate. Truncating kept the first N ascending values,
    // so a request for 20..100 in steps of 1 was served 20-39% and reported as
    // the sweep — the bottom quarter of the range, silently. `thin_to` keeps the
    // first and last and samples between them, so the cap and the RANGE hold
    // together.
    //
    // [SAFETY] Both invariants this function exists to guarantee survive the
    // swap, because `thin_to` only ever SELECTS elements of an already-clamped,
    // already-sorted list: every retained value was clamped to
    // `max(CHARACTERIZATION_MIN_PCT, floor)` before it got here, so no point can
    // be below the floor or zero; and its index map is monotonically
    // non-decreasing, so the list stays ascending — which is the property that
    // makes an aborted sweep leave the header at the highest duty it reached.
    thin_to(&out, constants::CHARACTERIZATION_MAX_POINTS)
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

/// How far a tach reading must move from `before` to count as movement — the
/// one rule behind both [`rpm_verdict`] and [`first_change_ms`] (`PTR-y`).
///
/// `PTR-m`: judged against the point's own measured noise where it has one —
/// `stddev_rpm` over a window that SETTLED — rather than the proportional
/// `before / 10`. The proportional rule scales with the reading, not with the
/// tach's spread, so a smooth high-RPM device's genuine ~265 rpm steps read
/// `unchanged` while the sweep-level `rpm_response` passed.
///
/// Falls back to the proportional rule when no trustworthy settled spread
/// exists: a point that did not settle computed its σ over its own step
/// transient, and using that would grade the step by itself — the bigger the
/// response, the larger the "noise". Too few readings, or none, is no spread at
/// all.
fn rpm_move_threshold(before: u16, stability: Option<&PointStability>) -> f64 {
    use crate::api::stats::{STABILITY_INSUFFICIENT, STABILITY_NOT_SETTLED, STABILITY_UNAVAILABLE};
    let settled_sigma = stability
        .filter(|st| {
            ![
                STABILITY_NOT_SETTLED,
                STABILITY_INSUFFICIENT,
                STABILITY_UNAVAILABLE,
            ]
            .contains(&st.verdict.as_str())
        })
        .and_then(|st| st.stddev_rpm);
    match settled_sigma {
        Some(sigma) => (sigma * constants::CHARACTERIZATION_RPM_VERDICT_SIGMA)
            .max(f64::from(constants::CHARACTERIZATION_RPM_NOISE_FLOOR)),
        None => f64::from(constants::CHARACTERIZATION_RPM_NOISE_FLOOR.max(before / 10)),
    }
}

/// When did the tach first move away from `before`, in ms from the write?
///
/// `PTR-y`: derived AFTER the hold, from the retained samples and then the
/// after-read (`after`, stamped with the point's `settle_ms`), against exactly
/// the threshold the verdict uses. It used to be detected live inside the hold
/// with the proportional rule `PTR-m` retired from the verdict, so a smooth
/// high-RPM step could read `changed` with no time at all — its Response column
/// empty, the member's median short a point, and a session's `response_latency`
/// `unavailable` where every step had moved. Now a `changed` verdict always
/// carries a time: the after-read that earns it is the last candidate here —
/// **provided at least one hold sample was readable** (DEC-454 review). With
/// none, the after-read's timestamp is only the hold's length (up to settle +
/// dwell), an upper bound rather than a reaction time, so the point reports
/// `None` as it always did.
///
/// `None` means nothing moved, the tach was unreadable throughout the hold, or
/// there was no reference reading — never that it responded instantly.
fn first_change_ms(
    samples: &[crate::api::stats::RpmSample],
    before: Option<u16>,
    after: Option<(u64, u16)>,
    stability: Option<&PointStability>,
) -> Option<u64> {
    let b = before?;
    let threshold = rpm_move_threshold(b, stability);
    let readable: Vec<(u64, u16)> = samples
        .iter()
        .filter_map(|s| s.rpm.map(|rpm| (s.at_ms, rpm)))
        .collect();
    let after = if readable.is_empty() { None } else { after };
    readable
        .into_iter()
        .chain(after)
        .find(|&(_, rpm)| f64::from(b.abs_diff(rpm)) > threshold)
        .map(|(at_ms, _)| at_ms)
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
            if got.abs_diff(requested_pct) <= constants::READBACK_TOLERANCE_PCT {
                "match".into()
            } else {
                "clamped".into()
            }
        }
    }
}

/// Did this point's fan physically respond? `changed` | `unchanged` |
/// `unavailable`. The threshold is [`rpm_move_threshold`]'s, shared with
/// [`first_change_ms`] so a `changed` point always has a response time.
fn rpm_verdict(
    before: Option<u16>,
    after: Option<u16>,
    stability: Option<&PointStability>,
) -> String {
    let (Some(b), Some(a)) = (before, after) else {
        return "unavailable".into();
    };
    if f64::from(b.abs_diff(a)) > rpm_move_threshold(b, stability) {
        "changed".into()
    } else {
        "unchanged".into()
    }
}

/// Derive the whole-sweep diagnostics. Pure, total, and the only place these
/// rules live — the handler calls this rather than deriving anything inline.
///
/// `driver_update_interval_ms` is the chip's own `update_interval`, when it
/// publishes one; it outranks the interval the holds observed (§4, DEC-405).
pub fn summarise(
    points: &[CharPoint],
    learned: &[LearnedPoint],
    driver_update_interval_ms: Option<u64>,
) -> CharSummary {
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
    let non_decreasing = |seq: &[u16]| -> Option<bool> {
        if seq.len() < 2 {
            return None;
        }
        Some(seq.windows(2).all(|w| {
            let tolerance = constants::CHARACTERIZATION_RPM_NOISE_FLOOR.max(w[0] / 33);
            w[1] + tolerance >= w[0]
        }))
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

    // DEC-405 (`PTR-d`): a bidirectional walk descends and then climbs, so walk
    // order is not duty order. Each leg is judged on its own, by duty — the same
    // two series the hysteresis comparison uses, `ramp` excluded for the same
    // reason (its approach direction is unknown).
    let leg = |series: &[stats::DutyRpm]| -> Option<bool> {
        let seq: Vec<u16> = series.iter().map(|p| p.rpm).collect();
        non_decreasing(&seq)
    };
    let (monotonic, monotonic_falling, monotonic_rising) = if bidirectional {
        let (f, r) = (leg(&falling), leg(&rising));
        let overall = match (f, r) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (None, None) => None,
            _ => Some(true),
        };
        (overall, f, r)
    } else {
        let seq: Vec<u16> = rpms.iter().map(|(_, r)| *r).collect();
        (non_decreasing(&seq), None, None)
    };

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
    // `not_settled` (DEC-405) ranks above `insufficient_data`: there were
    // enough readings, and they were still moving — a stronger observation than
    // too few readings, and weaker than a settled window that varied.
    let rank = |v: &str| match v {
        stats::STABILITY_UNSTABLE => 5,
        stats::STABILITY_VARIABLE => 4,
        stats::STABILITY_NOT_SETTLED => 3,
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

    let median_u64 = |mut v: Vec<u64>| -> Option<u64> {
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        Some(v[v.len() / 2])
    };
    // `§5`: publish the cadence the timings were measured at, so no client has
    // to assume milliseconds. DEC-405 (`PTR-b`): that is the tach REGISTER's
    // refresh, not the sampler's — the chip's own `update_interval` where it
    // publishes one, else the median of what the holds observed, else UNKNOWN.
    // This used to report the 500 ms sample interval, against a measured ~2 s
    // refresh on it87.
    let measurement_resolution_ms = driver_update_interval_ms.or_else(|| {
        median_u64(
            points
                .iter()
                .filter_map(|p| p.stability.as_ref())
                .filter_map(|st| st.update_interval_ms)
                .collect(),
        )
    });
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
        monotonic_falling,
        monotonic_rising,
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

/// Restores the pre-run duty when a diagnostic ends — every exit of
/// `run_sweep`, `discovery::run_discovery` and `stall_probe::run_probe` ends
/// in an explicit [`RestoreGuard::restore`].
///
/// **Explicit and async since DEC-455 (`PTR-ab`), no longer a `Drop`.** The
/// restore used to be written from inside `Drop`, which cannot `.await`, so its
/// write ran `set_pwm` inline on the tokio worker — and a driver that stopped
/// answering parked the worker with the controller lock held. The write is now
/// the caller's bounded async write ([`crate::api::diagnostic_gates::bounded_hwmon_write`]
/// in production). `Drop` remains only as the fallback for a run that never
/// reached its restore — in practice a panic, since a shutdown skips the
/// restore anyway — and by the user's choice (2026-09-29) it writes nothing
/// and logs: once the caller's pause and lease guards drop, the engine's next
/// tick drives the header if a profile names it and otherwise hands it back as
/// it was found (DEC-382) — or, for a header with no mode switch, releases it
/// to the exit floor (DEC-451), which `Drop` arranges by recording it in the
/// state cache (`PTR-ae`).
///
/// **The skip rules live INSIDE `restore`, deliberately.** The calibrate
/// equivalent (`calibration::RestoreOnDrop`) only needed the thermal rule,
/// because its future is awaited by a handler that cannot outlive the process.
/// These runs are detached: nothing in `main::shutdown_sequence`'s
/// `task_handles` joins them, so a restore genuinely can run *during* shutdown,
/// after `hand_back_hwmon` has handed the header back to firmware. A restore
/// written there would re-assert `pwm_enable=1` at a fixed duty with no writer
/// left to revise it — the exact DEC-290 / 277-c hazard.
///
/// **Shared by three diagnostics** — characterisation, control-path discovery
/// (AIO Phase 8 Batch 1) and the stall probe (DEC-407) — which is why the struct
/// and its fields are `pub(crate)`. The skip rules, their order, and the
/// `restore_floor` clamp are the same code for all three, which is the point.
/// **The ordering invariant travels with it:** the restore must run while the
/// caller's hwmon lease guard is still held, or it fails `InvalidLease` — each
/// runner awaits it before it returns, inside the caller's guarded scope.
pub(crate) struct RestoreGuard<'a, W, WF, S>
where
    W: Fn(u8) -> WF,
    WF: std::future::Future<Output = Result<(), WriteFailure>>,
    S: Fn() -> bool,
{
    pub(crate) header_id: &'a str,
    pub(crate) original_pct: Option<u8>,
    pub(crate) write_fn: &'a W,
    pub(crate) cache: &'a StateCache,
    pub(crate) shutting_down: &'a S,
    /// Set by the run before its first write. Distinguishes "there was no
    /// pre-run duty to restore and we never moved the header" (nothing to
    /// report) from "we moved it and cannot put it back" ([`RestoreOutcome::NoOriginalDuty`]).
    pub(crate) wrote_any: &'a AtomicBool,
    pub(crate) report: &'a RestoreReport,
    /// [SAFETY] `AUD3-l`. The lowest duty this header may be RESTORED to —
    /// `HARD_PUMP_CPU_FLOOR_PCT` for a pump-protected header, 0 for everything
    /// else. The runs have always floored the duties written on the way IN; the
    /// way out wrote `original_pct` straight through, and the write path
    /// applies no floor of its own. Restoring a captured 0 to a pump therefore
    /// converted "0 under firmware control" into "0 under `pwm_enable=1` with no
    /// writer" — a stopped pump. Same clamp as `hwmon_ctl::restore_duty`.
    pub(crate) restore_floor: u8,
    /// [SAFETY] `TS-aw` (DEC-418). The run's pump watch, re-read once more
    /// immediately before the restore writes: a header that became
    /// pump-protected at any point in the run — after its last sample
    /// included — is restored no lower than the pump floor, whatever
    /// `restore_floor` was planned as. Consulted only after both authority
    /// skips and the stuck-write skip, so it is never read while shutting down
    /// or while a stuck write holds the controller lock its lookup takes.
    /// `None` for the stall probe, which refuses a pump outright and raises
    /// `restore_floor` itself from its own eligibility re-check (DEC-407).
    pub(crate) pump_watch: Option<&'a PumpWatch<'a>>,
    /// [SAFETY] DEC-420 (the user's choice at review). Set by the run when a
    /// read of the header did not return within its bound. The restore is then
    /// skipped ([`RestoreOutcome::SkippedUnresponsive`]) — unless the header has
    /// become pump-protected during the run, when the floored restore is still
    /// attempted, because DEC-418's 30 % floor outranks the risk of a write that
    /// hangs (and that write is bounded since DEC-455). `None` for the
    /// diagnostics whose reads are not bounded this way.
    pub(crate) unresponsive: Option<&'a AtomicBool>,
    /// [SAFETY] DEC-455 (`PTR-ab`). Set when one of the run's WRITES did not
    /// return within [`constants::DIAGNOSTIC_WRITE_BUDGET`] — by the run, or by
    /// this guard's own restore write. The stuck write still holds the
    /// controller lock, so the restore re-reads nothing that takes that lock —
    /// not even the pump union — and writes only what `after_stuck_write` says.
    pub(crate) write_stuck: &'a AtomicBool,
    /// [SAFETY] DEC-455: what the restore does after a write that did not
    /// return. Decided by whether anything else will ever put the header back.
    pub(crate) after_stuck_write: AfterStuckWrite,
    /// Cleared by [`RestoreGuard::restore`]; still set in `drop` only when the
    /// run never reached its restore.
    pub(crate) armed: bool,
}

impl<W, WF, S> RestoreGuard<'_, W, WF, S>
where
    W: Fn(u8) -> WF,
    WF: std::future::Future<Output = Result<(), WriteFailure>>,
    S: Fn() -> bool,
{
    /// Every exit stamps `self.report` (`AUD2-c`): three of the old exits used
    /// to publish "restored" about a header still at the last swept duty. The
    /// branch *order* is the pre-DEC-455 one, with the stuck-write skip added
    /// after the two authority skips.
    ///
    /// `armed` is cleared only once the body has returned (`PTR-ae` review):
    /// a panic INSIDE the restore — a pump-union lookup, the floor arithmetic —
    /// leaves it set, so `Drop` records the header as abandoned too. A record
    /// after a write that did land only ever costs a raise to the exit floor.
    pub(crate) async fn restore(&mut self) {
        self.restore_body().await;
        self.armed = false;
    }

    async fn restore_body(&mut self) {
        // A run that never wrote left the header exactly where it found it, so
        // none of the non-restoring exits is a finding for it. Reporting one
        // would trade `AUD2-c`'s false "restored" for a false alarm — and the
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
        // [SAFETY] DEC-455: a write that did not return still holds the
        // controller lock, so nothing more is written and nothing that takes the
        // lock is called — the pump watch's lookup included. Checked BEFORE the
        // stuck-read branch below, which consults that watch.
        if self.write_stuck.load(Ordering::SeqCst) {
            self.after_a_stuck_write(moved).await;
            return;
        }
        // [SAFETY] DEC-420: after the driver stopped answering a READ, no write
        // — unless the header became a pump. `became_protected`, not
        // `restore_is_pump`: a header that was a pump from the start was swept
        // on duties at or above the pump floor, so where the run left it is
        // already safe.
        if self.unresponsive.is_some_and(|u| u.load(Ordering::SeqCst)) {
            if !self.pump_watch.is_some_and(|w| w.became_protected()) {
                log::warn!(
                    "characterize: {} left at the last swept duty instead of restoring \
                     it — a read of it did not return, and a write to a driver that is \
                     not responding could hang",
                    self.header_id
                );
                self.report
                    .set(left_behind(RestoreOutcome::SkippedUnresponsive));
                return;
            }
            log::warn!(
                "characterize: {} stopped responding AND became pump-protected during \
                 the run, so its floored restore is attempted anyway",
                self.header_id
            );
        }
        let Some(restore) = self.original_pct else {
            if moved {
                // [SAFETY] `TS-aw` `K1` (DEC-418): left where the run left it, as
                // always — unless it has become a pump, when that duty is raised
                // to the pump floor. Still `no_original_duty`: the header was
                // moved and cannot be put back where it was found.
                match self.pump_watch.and_then(|w| w.floored_fallback()) {
                    Some(pct) => {
                        log::warn!(
                            "characterize: {} was swept but its pre-sweep duty could not \
                             be read, and it became pump-protected during the run, so it \
                             is raised to {pct}% (the last swept duty, floored)",
                            self.header_id
                        );
                        if let Err(e) = (self.write_fn)(pct).await {
                            if e == WriteFailure::Unresponsive {
                                self.write_stuck.store(true, Ordering::SeqCst);
                            }
                            log::warn!(
                                "characterize: raising {} to {pct}% failed; it is left at \
                                 the last swept duty: {e:?}",
                                self.header_id
                            );
                        }
                    }
                    None => log::warn!(
                        "characterize: {} was swept but its pre-sweep duty could not be \
                         read, so it is left at the last swept duty",
                        self.header_id
                    ),
                }
            }
            self.report.set(left_behind(RestoreOutcome::NoOriginalDuty));
            return;
        };
        // [SAFETY] `AUD3-l` — clamp on the way out, as the sweep does on the way in.
        // `TS-aw` — and re-read the pump union first, so evidence that arrived
        // mid-run raises the floor even when the run itself never saw it.
        // DEC-443: the watch carries the header's own pump floor — the DC pump
        // floor on a DC-mode header.
        let floor = match self.pump_watch {
            Some(w) if w.restore_is_pump() => self.restore_floor.max(w.pump_floor()),
            _ => self.restore_floor,
        };
        let restore = restore.max(floor);
        match (self.write_fn)(restore).await {
            Ok(()) => self.report.set(RestoreOutcome::Restored),
            // DEC-455: the restore write itself did not return. It may still
            // land when the driver answers; nothing more is written either way.
            Err(WriteFailure::Unresponsive) => {
                self.write_stuck.store(true, Ordering::SeqCst);
                log::warn!(
                    "characterize: the restore of {} to {restore}% did not return within \
                     {} s; it is left where the run left it unless that write lands later",
                    self.header_id,
                    constants::DIAGNOSTIC_WRITE_BUDGET.as_secs()
                );
                self.report.set(RestoreOutcome::SkippedUnresponsive);
            }
            Err(WriteFailure::Error(e)) => {
                log::warn!(
                    "characterize: restore of {} to {restore}% failed; it is left at the \
                     last swept duty: {e}",
                    self.header_id
                );
                self.report.set(RestoreOutcome::WriteFailed);
            }
        }
    }

    /// [SAFETY] DEC-455: the restore after one of the run's writes did not
    /// return — [`AfterStuckWrite`] says what, by whether anything else will
    /// ever put the header back. Calls nothing that takes the controller lock:
    /// the queued write's floor comes from what the pump watch has already
    /// seen. A queued write that is still waiting when its bound runs out stays
    /// queued and lands after the stuck one; it is reported
    /// `skipped_unresponsive`, like any write that may yet land.
    async fn after_a_stuck_write(&mut self, moved: bool) {
        let queued = match self.after_stuck_write {
            AfterStuckWrite::WriteNothing => None,
            AfterStuckWrite::QueueFullSpeed => Some(100),
            AfterStuckWrite::QueueRestore => match self.original_pct {
                Some(original) => {
                    let floor = match self.pump_watch {
                        Some(w) if w.seen_as_pump() => self.restore_floor.max(w.pump_floor()),
                        _ => self.restore_floor,
                    };
                    Some(original.max(floor))
                }
                // As the no-original-duty exit: left where the run left it,
                // raised to the pump floor if it is known to be a pump.
                None => self.pump_watch.and_then(|w| w.floored_fallback_as_seen()),
            },
        };
        let Some(pct) = queued else {
            log::warn!(
                "characterize: {} left where the run left it instead of restoring it — a \
                 write to it did not return within {} s and still holds the controller \
                 lock; the engine takes the header back once it lets go",
                self.header_id,
                constants::DIAGNOSTIC_WRITE_BUDGET.as_secs()
            );
            let outcome = if moved {
                RestoreOutcome::SkippedUnresponsive
            } else {
                RestoreOutcome::Restored
            };
            self.report.set(outcome);
            return;
        };
        log::warn!(
            "characterize: a write to {} did not return within {} s; it has no mode switch \
             for the engine to hand back, so {pct}% is queued behind that write and lands \
             when the driver answers",
            self.header_id,
            constants::DIAGNOSTIC_WRITE_BUDGET.as_secs()
        );
        let outcome = match (self.write_fn)(pct).await {
            Ok(()) if self.after_stuck_write == AfterStuckWrite::QueueRestore => {
                RestoreOutcome::Restored
            }
            // Full speed is not where the header was found; and a write still
            // waiting may yet land.
            Ok(()) | Err(WriteFailure::Unresponsive) => RestoreOutcome::SkippedUnresponsive,
            Err(WriteFailure::Error(e)) => {
                log::warn!(
                    "characterize: the queued write of {pct}% to {} failed: {e}",
                    self.header_id
                );
                RestoreOutcome::WriteFailed
            }
        };
        self.report.set(outcome);
    }
}

impl<W, WF, S> Drop for RestoreGuard<'_, W, WF, S>
where
    W: Fn(u8) -> WF,
    WF: std::future::Future<Output = Result<(), WriteFailure>>,
    S: Fn() -> bool,
{
    /// [SAFETY] The fallback only (DEC-455, the user's choice): a run dropped
    /// before its restore — a panic — writes nothing here. A write from `Drop`
    /// could not be bounded, and would race the lease guard that drops next.
    /// The engine's next tick drives or hands back a header with a mode switch
    /// (DEC-382). One with no mode switch is recorded here, in memory only
    /// (`PTR-ae`), so that once the write-pause ends DEC-451's release raises
    /// it to the exit floor if no profile names it — the engine holds only
    /// what IT wrote, so without the record it stayed at the run's last duty
    /// until the daemon stopped.
    fn drop(&mut self) {
        if self.armed && self.wrote_any.load(Ordering::SeqCst) {
            self.cache.note_abandoned_by_diagnostic(self.header_id);
            log::error!(
                "diagnostic on {} ended without running its restore; nothing is written \
                 from here — the engine's next tick drives the header, gives it back, or \
                 releases it to the exit floor",
                self.header_id
            );
        }
    }
}

/// [SAFETY] DEC-455 (`PTR-ab`, the user's choices, 2026-09-29): what a
/// diagnostic's restore does after one of its writes did not return. That
/// write still holds the controller lock; a write issued now waits behind it on
/// the blocking pool and lands after it, whenever the driver answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterStuckWrite {
    /// A header with a mode switch: write nothing more. The diagnostic's first
    /// write took it from the firmware and recorded that (`note_take`), so the
    /// engine's next tick drives it or hands it back as it was found (DEC-382).
    WriteNothing,
    /// A header with no mode switch (characterisation, discovery): nothing
    /// else will ever put it back — the engine holds (DEC-451) only what IT
    /// wrote — so the normal restore is queued behind the stuck write. Its
    /// floor is what the pump watch has already seen: a lookup would take the
    /// lock the stuck write holds.
    QueueRestore,
    /// A header with no mode switch (the stall probe): one full-speed write is
    /// queued behind the stuck write. A queued kick could not hold 100 % long
    /// enough to restart a fan the stuck step stalled, and a restore alone could
    /// leave it stopped, so the header is left at full speed.
    QueueFullSpeed,
}

impl AfterStuckWrite {
    /// The choice for a header: `has_mode_switch` is `supports_enable` with an
    /// enable path — exactly when `set_pwm`'s take is recorded for hand-back.
    pub fn for_header(has_mode_switch: bool, no_mode: AfterStuckWrite) -> Self {
        if has_mode_switch {
            Self::WriteNothing
        } else {
            no_mode
        }
    }
}

/// How a sweep ended.
pub struct SweepOutcome {
    pub state: &'static str,
    pub detail: Option<String>,
    pub points: Vec<CharPoint>,
    /// The pre-sweep duty the restore aims at — the sweep's own first read, the
    /// ONLY read of it (DEC-420, `PTR-v`). The handler publishes this rather
    /// than reading the header a second time, so the published duty and the
    /// restore target cannot disagree. `None` when that read found no duty or
    /// did not return.
    pub original_pct: Option<u8>,
}

/// A read of the header did not return within
/// [`constants::DIAGNOSTIC_READ_BUDGET`] (DEC-420, `PTR-v`): mark the run
/// unresponsive — which is what makes [`RestoreGuard::restore`] skip its
/// write — and return the run's `detail`. One function, so no wedge exit can report the
/// wedge without also arming the skip.
fn note_unresponsive(unresponsive: &AtomicBool) -> String {
    unresponsive.store(true, Ordering::SeqCst);
    format!(
        "a read of the header did not return within {} s (a driver that is not \
         responding), so the sweep stopped and does not read it again; \
         `restore_outcome` says whether its pre-sweep duty was written back",
        constants::DIAGNOSTIC_READ_BUDGET.as_secs()
    )
}

/// Walk `points` on one header, publishing each measured point as it lands.
///
/// Generic over the write/read closures so the whole sequence — including every
/// abort path and the restore — is testable without sysfs. `publish` is called
/// once per completed point and is what makes the run visible to
/// `GET /diagnostics/characterization` while it is still running.
///
/// Aborts, all of which restore: `cancel` set (→ `cancelled`), a failed write
/// (→ `failed`), `pwm_enable != 1` (→ `aborted`, reclaim), and a sensor over
/// the calibrate/verify limit or the ladder forcing (→ `aborted`). Two do NOT
/// restore: a read that did not return (→ `aborted`, DEC-420), after which
/// nothing more is written unless the header became a pump, and a write that
/// did not return (→ `failed`, DEC-455), after which nothing more is written.
///
/// [SAFETY] `write_fn` is BOUNDED in production (DEC-455, `PTR-ab`,
/// [`crate::api::diagnostic_gates::bounded_hwmon_write`]): `Err(Unresponsive)`
/// is a write that did not return within [`constants::DIAGNOSTIC_WRITE_BUDGET`].
/// Every exit — early returns included — ends in the explicit
/// [`RestoreGuard::restore`], awaited before this returns.
///
/// [SAFETY] `read_fn` is BOUNDED (DEC-420, `PTR-v`, the stall probe's DEC-407
/// shape): `None` is a read that did not return within
/// [`constants::DIAGNOSTIC_READ_BUDGET`]. While a read is outstanding no gate
/// runs, so an unbounded one left the header at a swept duty with the thermal,
/// cancel and pump gates blind. A wedge ends the run, and — `spawn_blocking`
/// being uncancellable — the header is never read again by it.
#[allow(clippy::too_many_arguments)]
pub async fn run_sweep<W, WF, R, Fut, P, A, S, K>(
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
    // [SAFETY] DEC-455: whether the header has a mode switch, i.e. whether the
    // engine will hand it back after a write that did not return
    // ([`AfterStuckWrite`]). Without one, the restore is queued behind it.
    has_mode_switch: bool,
    // [SAFETY] `TS-aw` (DEC-418): the pump union, re-read before every write and
    // on every sample. A header that becomes pump-protected mid-run stops the
    // sweep (`aborted`), and the restore guard re-reads it before it writes.
    pump_watch: &PumpWatch<'_>,
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
    // `P8-bg`: told the phase at every boundary — each step's settle, then its
    // dwell when it has one. Separate from `publish`, which fires only when a
    // hold completes and is therefore exactly the silence this exists to fill.
    mut announce: A,
) -> SweepOutcome
where
    W: Fn(u8) -> WF,
    WF: std::future::Future<Output = Result<(), WriteFailure>>,
    R: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<HwmonVerifyState>>,
    P: FnMut(CharPoint),
    A: FnMut(RunStep),
    S: Fn() -> bool,
    K: Fn() -> bool,
{
    let first = read_fn().await;
    let original_pct = first.as_ref().and_then(|s| s.pwm_percent);
    let mut measured: Vec<CharPoint> = Vec::with_capacity(plan.len());
    // Declared BEFORE the guard so they outlive it — the guard reads them.
    let wrote_any = AtomicBool::new(false);
    // DEC-420: set by `note_unresponsive` on every wedge exit.
    let unresponsive = AtomicBool::new(false);
    // DEC-455: set by `note_write_unresponsive` when a write does not return.
    let write_stuck = AtomicBool::new(false);

    // Awaited below, before this returns — i.e. while the caller's lease guard
    // is still held. After it, the restore write would fail `InvalidLease` and
    // the header would be parked at the final sweep point (invariant 6 of the
    // agreed scope).
    let mut restore = RestoreGuard {
        header_id,
        original_pct,
        write_fn: &write_fn,
        cache,
        shutting_down: &shutting_down,
        wrote_any: &wrote_any,
        report,
        restore_floor,
        pump_watch: Some(pump_watch),
        unresponsive: Some(&unresponsive),
        write_stuck: &write_stuck,
        after_stuck_write: AfterStuckWrite::for_header(
            has_mode_switch,
            AfterStuckWrite::QueueRestore,
        ),
        armed: true,
    };

    // DEC-455: every exit of the walk is a `return` from this block, so every
    // one of them reaches the explicit restore below.
    let outcome = async {
        // A pre-sweep read that did not return: nothing has been written, so the
        // guard reports the header as where it was found.
        if first.is_none() {
            return SweepOutcome {
                state: STATE_ABORTED,
                detail: Some(note_unresponsive(&unresponsive)),
                points: measured,
                original_pct,
            };
        }

        for (idx, step) in plan.iter().enumerate() {
            let pct = step.pct;
            // [SAFETY] DEC-420 (`PTR-v`, the stall probe's DEC-407 rule): the
            // reference read comes FIRST, before every gate, so the gates below are
            // the last thing between it and the write. It used to sit between the
            // step gate and the write, where a read wedged across
            // `shutdown_sequence`'s drains let the write land after
            // `hand_back_hwmon`.
            let Some(before) = read_fn().await else {
                return SweepOutcome {
                    state: STATE_ABORTED,
                    detail: Some(note_unresponsive(&unresponsive)),
                    points: measured,
                    original_pct,
                };
            };
            let rpm_before = before.rpm;

            // [SAFETY] Stop writing the moment the daemon starts going down. The
            // drop guard's shutdown skip covers the RESTORE, but not this loop: the
            // task is detached, so it keeps running through `shutdown_sequence` and
            // can land a `set_pwm` AFTER `hand_back_hwmon` has handed the
            // header back to firmware. `set_pwm`'s reclaim watchdog would then see
            // the firmware mode that hand-back just wrote, call it a BIOS reclaim and
            // re-assert `pwm_enable=1` at the swept duty — a header latched in manual
            // with no writer left. That is the DEC-290 / 277-c hazard, and checking
            // only in `Drop` does not close it.
            //
            // Then cancel, the three thermal gates — including DEC-385's staleness
            // refusal (`TS-q`), because the two `value_c` checks cannot see age and a
            // poll that wedges part way through a sweep leaves them passing on its
            // last reading while the ladder, blind to it, cannot force — and finally
            // DEC-296's keepalive, proving liveness once per point so the deadman
            // measures that rather than the sweep's total duration. One definition,
            // shared with the stall probe (DEC-407): `diagnostic_gates::step_gate`.
            if let Err(stop) = step_gate(
                cache,
                CHARACTERIZATION_DIAGNOSTIC,
                "characterisation",
                "cannot write",
                &shutting_down,
                cancel,
                &keepalive,
            ) {
                let (state, detail) = match stop {
                    GateStop::ShuttingDown => {
                        (STATE_ABORTED, "the daemon is shutting down".to_string())
                    }
                    GateStop::Cancelled => (
                        STATE_CANCELLED,
                        format!("cancelled after {idx} of {} steps", plan.len()),
                    ),
                    GateStop::Thermal(_, detail) => (STATE_ABORTED, detail),
                    GateStop::Superseded => (
                        STATE_ABORTED,
                        "superseded by a later diagnostic; this run's lease is gone".to_string(),
                    ),
                };
                return SweepOutcome {
                    state,
                    detail: Some(detail),
                    points: measured,
                    original_pct,
                };
            }
            // [SAFETY] `TS-aw`: after the step gate, so it is never read once
            // shutdown has been seen, and immediately before the write it guards.
            if pump_watch.became_protected() {
                return SweepOutcome {
                    state: STATE_ABORTED,
                    detail: Some(pump_protected_mid_run_detail(
                        "characterisation",
                        pump_watch.pump_floor(),
                    )),
                    points: measured,
                    original_pct,
                };
            }
            // [SAFETY] DEC-420 (`PTR-v`): shutdown once more, immediately before
            // the write — the stall probe's rule. The pump re-read above takes
            // locks, and this task is detached, so nothing else stops a write that
            // reaches here after the hand-back began. (`set_pwm` also refuses a
            // header the hand-back owns once it has begun, DEC-420's `PTR-s`; this
            // covers the no-mode headers it deliberately does not refuse.)
            if shutting_down() {
                return SweepOutcome {
                    state: STATE_ABORTED,
                    detail: Some("the daemon is shutting down".into()),
                    points: measured,
                    original_pct,
                };
            }

            // Stamped BEFORE the call, deliberately: `set_pwm` writes sysfs and then
            // reads back, so an `Err` can still have moved the header. Over-reporting
            // "the header moved" is the safe direction; under-reporting it is the
            // `AUD2-c` defect.
            wrote_any.store(true, Ordering::SeqCst);
            pump_watch.note_write(pct);
            let command_accepted = match write_fn(pct).await {
                Ok(()) => true,
                // [SAFETY] DEC-455: a write that did not return still holds the
                // controller lock. The run ends here, as a failed write (the user's
                // choice), with no point for it — whether the command was accepted is
                // unknown — and nothing more is written or re-read.
                Err(WriteFailure::Unresponsive) => {
                    return SweepOutcome {
                        state: STATE_FAILED,
                        detail: Some(note_write_unresponsive(&write_stuck, pct)),
                        points: measured,
                        original_pct,
                    };
                }
                Err(WriteFailure::Error(e)) => {
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
                        original_pct,
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
            announce(RunStep::now(STEP_PHASE_SETTLE, idx as u16, pct, settle));
            let mut dwell_announced = dwell.is_zero();
            let mut samples: Vec<crate::api::stats::RpmSample> = Vec::new();
            let mut last_renew = tokio::time::Instant::now();
            while started.elapsed() < hold {
                let remaining = hold.saturating_sub(started.elapsed());
                tokio::time::sleep(remaining.min(constants::CHARACTERIZATION_SAMPLE_INTERVAL))
                    .await;
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
                        // No after-read and no settled spread on a hold cut short:
                        // the retained samples against the proportional rule.
                        first_change_ms: first_change_ms(&samples, rpm_before, None, None),
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
                        original_pct,
                    };
                }
                // [SAFETY] `TS-aw`: per sample, not per renewal, so a flip during a
                // long dwell stops the hold within one sample interval (the stall
                // probe's S3-R2 rule). The point being held is not recorded: it was
                // measured on a header whose floor has just changed.
                //
                // Only while the hold is still open. Once its time has elapsed the
                // point is measured (the user's rule, DEC-418 review): the next
                // step's check, or the restore's re-read, acts on the flip instead,
                // and nothing is written in between.
                if started.elapsed() < hold && pump_watch.became_protected() {
                    return SweepOutcome {
                        state: STATE_ABORTED,
                        detail: Some(pump_protected_mid_run_detail(
                            "characterisation",
                            pump_watch.pump_floor(),
                        )),
                        points: measured,
                        original_pct,
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
                    //
                    // DEC-385's staleness refusal runs on the same cadence, for the
                    // same reason as the per-point check — a wedge inside one long
                    // dwell must stop it. The three gates are the shared definition
                    // (DEC-407); shutdown and cancel keep their in-hold rules above
                    // and below, which is why this is not `step_gate`.
                    if let Some(detail) = thermal_gate(
                        cache,
                        CHARACTERIZATION_DIAGNOSTIC,
                        "characterisation",
                        "cannot continue",
                    ) {
                        return SweepOutcome {
                            state: STATE_ABORTED,
                            detail: Some(detail),
                            points: measured,
                            original_pct,
                        };
                    }
                    if !keepalive() {
                        return SweepOutcome {
                            state: STATE_ABORTED,
                            detail: Some(
                                "superseded by a later diagnostic; this run's lease is gone".into(),
                            ),
                            points: measured,
                            original_pct,
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
                if step.dwell.is_some()
                    && started.elapsed() >= settle
                    && cancel.load(Ordering::SeqCst)
                {
                    return SweepOutcome {
                        state: STATE_CANCELLED,
                        detail: Some(format!(
                        "cancelled during the stability hold at {pct}%, after {idx} of {} steps",
                        plan.len()
                    )),
                        points: measured,
                        original_pct,
                    };
                }
                if !dwell_announced && started.elapsed() >= settle {
                    dwell_announced = true;
                    announce(RunStep::now(
                        STEP_PHASE_DWELL,
                        idx as u16,
                        pct,
                        hold.saturating_sub(started.elapsed()),
                    ));
                }
                let at_ms = started.elapsed().as_millis() as u64;
                let Some(sample) = read_fn().await else {
                    return SweepOutcome {
                        state: STATE_ABORTED,
                        detail: Some(note_unresponsive(&unresponsive)),
                        points: measured,
                        original_pct,
                    };
                };
                samples.push(crate::api::stats::RpmSample {
                    at_ms,
                    rpm: sample.rpm,
                });
            }

            let Some(after) = read_fn().await else {
                return SweepOutcome {
                    state: STATE_ABORTED,
                    detail: Some(note_unresponsive(&unresponsive)),
                    points: measured,
                    original_pct,
                };
            };
            // DEC-405: one settle point, used for both the published `settled_ms`
            // and the window the statistics describe, so the two cannot disagree.
            // The pre-write reading is the reference: a first sample still showing
            // it is the register not having refreshed yet, never a settle.
            let settled_ms = crate::api::stats::settling_ms(&samples, rpm_before);
            let stability = point_stability(&samples, dwell, settled_ms);
            let settle_ms = started.elapsed().as_millis() as u64;
            let point = CharPoint {
                requested_pct: pct,
                command_accepted,
                readback_pct: after.pwm_percent,
                readback_raw: after.pwm_raw,
                pwm_enable: after.pwm_enable,
                rpm_before,
                rpm_after: after.rpm,
                settle_ms,
                // `PTR-y`: after the hold, against the verdict's own threshold.
                first_change_ms: first_change_ms(
                    &samples,
                    rpm_before,
                    after.rpm.map(|rpm| (settle_ms, rpm)),
                    Some(&stability),
                ),
                readback_verdict: readback_verdict(pct, after.pwm_percent, after.pwm_enable),
                rpm_verdict: rpm_verdict(rpm_before, after.rpm, Some(&stability)),
                direction: step.direction.token().into(),
                step_index: idx as u16,
                settled_ms,
                stability: Some(stability),
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
            //
            // [SAFETY] DEC-420 (`PTR-v`): a mode change seen while shutting down is
            // the hand-back's own write, not a reclaim (DEC-407's rule), and is
            // reported as the shutdown it is.
            if reclaimed && shutting_down() {
                return SweepOutcome {
                    state: STATE_ABORTED,
                    detail: Some("the daemon is shutting down".into()),
                    points: measured,
                    original_pct,
                };
            }
            if reclaimed {
                return SweepOutcome {
                    state: STATE_ABORTED,
                    detail: Some(format!(
                        "another controller reclaimed the header at {pct}% \
                     (pwm_enable={}); the remaining points were not tested",
                        after.pwm_enable.unwrap_or(0)
                    )),
                    points: measured,
                    original_pct,
                };
            }
        }

        SweepOutcome {
            state: STATE_COMPLETE,
            detail: None,
            points: measured,
            original_pct,
        }
    }
    .await;
    restore.restore().await;
    outcome
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
        "monotonic_falling",
        "monotonic_rising",
        "window_start_ms",
        "update_interval_ms",
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
        summarise(points, &[], None)
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
    /// Tests' reads never wedge: adapt a plain read to the bounded shape
    /// `run_sweep` takes (DEC-420), where `None` is a read that did not return.
    fn sync_read<R: Fn() -> HwmonVerifyState>(
        r: R,
    ) -> impl Fn() -> std::future::Ready<Option<HwmonVerifyState>> {
        move || std::future::ready(Some(r()))
    }

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
            true,
            &PumpWatch::fixed(false),
            settle,
            None,
            crate::api::diagnostic_gates::sync_write(write_fn),
            sync_read(read_fn),
            cancel,
            shutting_down,
            keepalive,
            report,
            publish,
            |_| {},
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
            rpm_verdict: rpm_verdict(Some(0), rpm, None),
            ..Default::default()
        }
    }

    // ── rpm_verdict (`PTR-m`) ─────────────────────────────────────────

    fn stab(verdict: &str, stddev: Option<f64>) -> PointStability {
        PointStability {
            verdict: verdict.into(),
            stddev_rpm: stddev,
            ..Default::default()
        }
    }

    #[test]
    fn a_step_beyond_three_settled_sigmas_is_changed_and_one_inside_is_not() {
        let quiet = stab(crate::api::stats::STABILITY_STABLE, Some(20.0));
        // 3σ = 60, above the 50 floor: 61 moved, 60 did not.
        assert_eq!(rpm_verdict(Some(3000), Some(3061), Some(&quiet)), "changed");
        assert_eq!(
            rpm_verdict(Some(3000), Some(3060), Some(&quiet)),
            "unchanged"
        );
        // A noisy point needs a proportionally bigger move.
        let noisy = stab(crate::api::stats::STABILITY_VARIABLE, Some(100.0));
        assert_eq!(
            rpm_verdict(Some(1000), Some(1250), Some(&noisy)),
            "unchanged"
        );
    }

    #[test]
    fn the_absolute_noise_floor_still_applies_to_a_perfectly_steady_point() {
        let dead_steady = stab(crate::api::stats::STABILITY_STABLE, Some(0.0));
        assert_eq!(
            rpm_verdict(Some(800), Some(850), Some(&dead_steady)),
            "unchanged"
        );
        assert_eq!(
            rpm_verdict(Some(800), Some(851), Some(&dead_steady)),
            "changed"
        );
    }

    /// A point that never settled has a σ inflated by its own step transient —
    /// grading the step against that would make a bigger response look like
    /// bigger noise. It keeps the proportional rule, as do too few readings.
    #[test]
    fn an_unsettled_or_thin_point_keeps_the_proportional_rule() {
        for verdict in [
            crate::api::stats::STABILITY_NOT_SETTLED,
            crate::api::stats::STABILITY_INSUFFICIENT,
            crate::api::stats::STABILITY_UNAVAILABLE,
        ] {
            let st = stab(verdict, Some(5.0));
            // 3σ would say changed; the proportional rule (300) says not.
            assert_eq!(
                rpm_verdict(Some(3000), Some(3200), Some(&st)),
                "unchanged",
                "{verdict}"
            );
        }
        assert_eq!(rpm_verdict(Some(3000), Some(3200), None), "unchanged");
        assert_eq!(rpm_verdict(None, Some(3200), None), "unavailable");
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
        // A fresh, cool reading — not an empty cache. Since DEC-385 a sweep with
        // no usable temperature refuses at its first point, so an empty cache
        // would test nothing but the refusal.
        let cache = cache_at(40.0, None);
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
        // Precondition: the sweep itself ran, not only its restore — the restore
        // alone would satisfy every assertion below.
        assert!(
            log.contains(&50),
            "precondition: the sweep reached 50%: {log:?}"
        );
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

    // ── TS-aw / DEC-418: a header that becomes a pump mid-sweep ──────────────

    /// Run a sweep whose pump union flips when `flip_on` says so. `flip_on` sees
    /// the union flag and is handed to both `write_fn` (`Written(pct)`) and
    /// `publish` (`Published(pct)`), so a test picks the exact moment evidence
    /// arrives. The header reads `original` before the sweep — 10 %, below the
    /// pump floor, so the restore's floor is observable, or `None` for a pre-run
    /// duty that could not be read (review `K1`).
    enum Moment {
        Written(u8),
        Published(u8),
    }

    async fn sweep_with_flip(
        plan: &[u8],
        settle: Duration,
        flip_on: impl Fn(&Moment) -> bool,
    ) -> (SweepOutcome, Vec<u8>, &'static str) {
        sweep_with_flip_from(Some(10), plan, settle, flip_on).await
    }

    async fn sweep_with_flip_from(
        original: Option<u8>,
        plan: &[u8],
        settle: Duration,
        flip_on: impl Fn(&Moment) -> bool,
    ) -> (SweepOutcome, Vec<u8>, &'static str) {
        let cache = cache_at(40.0, None);
        let rig = Rig::new();
        let union = Arc::new(AtomicBool::new(false));
        let u = union.clone();
        let watch = PumpWatch::new(
            "hwmon:test:pwm1",
            "characterisation",
            false,
            30,
            move || u.load(Ordering::SeqCst),
        );
        let writes = rig.writes.clone();
        let flip = |m: Moment| {
            if flip_on(&m) {
                union.store(true, Ordering::SeqCst);
            }
        };
        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &plan_of(plan),
            0,
            true,
            &watch,
            settle,
            None,
            crate::api::diagnostic_gates::sync_write(|p: u8| {
                writes.lock().unwrap().push(p);
                flip(Moment::Written(p));
                Ok(())
            }),
            sync_read(|| sample(original, Some(1), Some(900))),
            &rig.cancel,
            || false,
            || true,
            &rig.report,
            |pt: CharPoint| flip(Moment::Published(pt.requested_pct)),
            |_| {},
        )
        .await;
        let (_, token) = rig.restore();
        (out, rig.written(), token)
    }

    /// [SAFETY] `TS-aw`: evidence arriving while a sub-floor point is being held
    /// stops the sweep within the hold — the point is not recorded and no
    /// further point is written — and the restore is the captured 10 % raised
    /// to the pump floor, not the captured 10 %.
    #[tokio::test(start_paused = true)]
    async fn a_pump_flip_mid_hold_stops_the_sweep_and_floors_the_restore() {
        let (out, log, token) = sweep_with_flip(&[20, 60, 80], Duration::from_secs(2), |m| {
            matches!(m, Moment::Written(20))
        })
        .await;
        assert_eq!(out.state, STATE_ABORTED, "detail: {:?}", out.detail);
        assert!(
            out.detail
                .as_deref()
                .is_some_and(|d| d.contains("became pump-protected")),
            "detail: {:?}",
            out.detail
        );
        // Stopped INSIDE the hold: the step gate alone would have recorded the
        // 20 % point before stopping at the 60 % one.
        assert!(out.points.is_empty(), "points: {:?}", out.points);
        assert_eq!(
            log,
            vec![20, PUMP_FLOOR],
            "after the flip the only write must be the floored restore"
        );
        assert_eq!(token, RestoreOutcome::Restored.token());
    }

    /// [SAFETY] `TS-aw`: evidence arriving between points is caught by the check
    /// before the next write, so the next — planned for an ordinary fan — is
    /// never written.
    #[tokio::test(start_paused = true)]
    async fn a_pump_flip_between_points_stops_before_the_next_write() {
        let (out, log, _) = sweep_with_flip(&[20, 25, 80], Duration::from_secs(1), |m| {
            matches!(m, Moment::Published(20))
        })
        .await;
        assert_eq!(out.state, STATE_ABORTED, "detail: {:?}", out.detail);
        assert_eq!(
            out.points.len(),
            1,
            "precondition: the 20 % point completed"
        );
        assert_eq!(log, vec![20, PUMP_FLOOR]);
    }

    /// [SAFETY] `TS-aw`: the restore guard re-reads the union itself. Evidence
    /// that arrives after the last point — when no loop check remains — still
    /// floors the restore; the sweep measured every point, so it stays
    /// `complete`. The opposite arm, with nothing arriving, restores the
    /// captured 10 % exactly: the floor is not applied to every sweep.
    #[tokio::test(start_paused = true)]
    async fn the_restore_rereads_the_union_after_the_last_point() {
        let (out, log, _) = sweep_with_flip(&[60], Duration::from_secs(1), |m| {
            matches!(m, Moment::Published(60))
        })
        .await;
        assert_eq!(out.state, STATE_COMPLETE, "detail: {:?}", out.detail);
        assert_eq!(log, vec![60, PUMP_FLOOR]);

        let (out, log, _) = sweep_with_flip(&[60], Duration::from_secs(1), |_| false).await;
        assert_eq!(out.state, STATE_COMPLETE);
        assert_eq!(log, vec![60, 10], "an ordinary fan is restored as found");
    }

    /// [SAFETY] Review `K1`: the pre-run duty could not be read, so there is no
    /// original to floor. The header is left where the run left it, as it always
    /// was — raised to the pump floor when that was below it, never lowered to it.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_original_is_left_at_the_last_duty_raised_to_the_floor() {
        // Stopped at a sub-floor point: the last duty, 20 %, is raised to 30 %.
        let (out, log, token) =
            sweep_with_flip_from(None, &[20, 60, 80], Duration::from_secs(2), |m| {
                matches!(m, Moment::Written(20))
            })
            .await;
        assert_eq!(out.state, STATE_ABORTED, "detail: {:?}", out.detail);
        assert_eq!(log, vec![20, PUMP_FLOOR]);
        assert_eq!(token, RestoreOutcome::NoOriginalDuty.token());

        // Stopped above the floor: left at 60 %, not lowered to 30 %.
        let (_, log, _) = sweep_with_flip_from(None, &[20, 60, 80], Duration::from_secs(2), |m| {
            matches!(m, Moment::Written(60))
        })
        .await;
        assert_eq!(log, vec![20, 60, 60]);

        // The opposite arm: an ordinary fan with no original is left exactly
        // where the run left it, with no write added.
        let (out, log, _) =
            sweep_with_flip_from(None, &[20, 60], Duration::from_secs(1), |_| false).await;
        assert_eq!(out.state, STATE_COMPLETE);
        assert_eq!(log, vec![20, 60]);
    }

    /// [SAFETY] The user's rule (DEC-418 review, `C3`): a hold whose time has
    /// elapsed is measured. Evidence that becomes visible exactly as the last
    /// hold ends — before its closing sample — must not discard the point: the
    /// run is `complete`, every point recorded, and the restore still floored.
    /// Paused time makes "exactly" exact: the watch answers yes from the instant
    /// the last hold's settle has fully elapsed.
    #[tokio::test(start_paused = true)]
    async fn a_flip_as_the_last_hold_ends_leaves_the_sweep_complete_and_floored() {
        let cache = cache_at(40.0, None);
        let rig = Rig::new();
        let settle = Duration::from_secs(2);
        let flip_at: Arc<Mutex<Option<tokio::time::Instant>>> = Arc::new(Mutex::new(None));
        let fa = flip_at.clone();
        let watch = PumpWatch::new(
            "hwmon:test:pwm1",
            "characterisation",
            false,
            30,
            move || {
                fa.lock()
                    .unwrap()
                    .is_some_and(|t| tokio::time::Instant::now() >= t)
            },
        );
        let writes = rig.writes.clone();
        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &plan_of(&[60]),
            0,
            true,
            &watch,
            settle,
            None,
            crate::api::diagnostic_gates::sync_write(|p: u8| {
                writes.lock().unwrap().push(p);
                if p == 60 {
                    *flip_at.lock().unwrap() = Some(tokio::time::Instant::now() + settle);
                }
                Ok(())
            }),
            sync_read(|| sample(Some(10), Some(1), Some(900))),
            &rig.cancel,
            || false,
            || true,
            &rig.report,
            |_| {},
            |_| {},
        )
        .await;
        assert_eq!(out.state, STATE_COMPLETE, "detail: {:?}", out.detail);
        assert_eq!(out.points.len(), 1, "the completed point was discarded");
        assert_eq!(
            rig.written(),
            vec![60, PUMP_FLOOR],
            "the restore must be floored"
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

    /// [SAFETY] DEC-455 (`PTR-ab`), the user's choice for the fallback: a run
    /// whose future is dropped before its restore — a panic, or the runtime
    /// dropping the detached task at teardown — writes NOTHING more. The
    /// restore is an explicit awaited step now, and `RestoreGuard::drop` only
    /// logs: a write from `Drop` could not be bounded and would race the lease
    /// guard dropping next; the engine's next tick drives or hands back the
    /// header (DEC-382). Not shutting down, and with a point already written,
    /// so the old drop-time restore would have fired here.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_run_before_its_restore_writes_nothing_more() {
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let report = RestoreReport::new();
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
                0,
                Duration::from_secs(6),
                move |p: u8| {
                    writes_w.lock().unwrap().push(p);
                    *last_w.lock().unwrap() = p;
                    Ok(())
                },
                move || sample(Some(*last_r.lock().unwrap()), Some(1), Some(900)),
                &cancel,
                || false,
                || true,
                &report,
                |_| {},
            );
            tokio::pin!(fut);
            // Into the first point's settle, then abandoned.
            let _ = tokio::time::timeout(Duration::from_millis(50), &mut fut).await;
            assert_eq!(
                *writes.lock().unwrap(),
                vec![30],
                "precondition: the header was moved, so a restore was owed"
            );
        } // the future — and its RestoreGuard — drop here

        assert_eq!(
            *writes.lock().unwrap(),
            vec![30],
            "the drop wrote a restore; the fallback writes nothing"
        );
        assert_eq!(
            report.get(),
            RestoreOutcome::Pending,
            "no restore ran, so none may be reported"
        );
        // `PTR-ae`: recorded instead, so DEC-451's release can reach a header
        // with no mode switch once the pause ends.
        assert_eq!(
            cache.abandoned_by_diagnostic(),
            vec!["hwmon:test:pwm1".to_string()]
        );
    }

    /// `PTR-ae` review: a panic INSIDE the restore — here the restore write
    /// itself — is a run that never put the header back, and is recorded too.
    #[tokio::test(start_paused = true)]
    async fn a_panic_inside_the_restore_still_records_the_header() {
        let cache = Arc::new(cache_at(45.0, Some("normal")));
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (c, w) = (cache.clone(), writes.clone());
        let joined = tokio::spawn(async move {
            let cancel = AtomicBool::new(false);
            let report = RestoreReport::new();
            let last = Arc::new(Mutex::new(42u8));
            let (last_w, last_r) = (last.clone(), last.clone());
            run_sweep_uni(
                &c,
                "hwmon:test:pwm1",
                &[30],
                0,
                Duration::from_secs(1),
                move |p: u8| {
                    assert_ne!(p, 42, "the restore write panics");
                    w.lock().unwrap().push(p);
                    *last_w.lock().unwrap() = p;
                    Ok(())
                },
                move || sample(Some(*last_r.lock().unwrap()), Some(1), Some(900)),
                &cancel,
                || false,
                || true,
                &report,
                |_| {},
            )
            .await;
        })
        .await;

        assert!(joined.unwrap_err().is_panic(), "precondition: it panicked");
        assert_eq!(*writes.lock().unwrap(), vec![30], "precondition: moved");
        assert_eq!(
            cache.abandoned_by_diagnostic(),
            vec!["hwmon:test:pwm1".to_string()]
        );
    }

    /// `PTR-ae`, the opposite branch: a run that reached its restore records
    /// nothing, so DEC-451's release never overrides a restore that stood.
    #[tokio::test(start_paused = true)]
    async fn a_run_that_restored_records_no_abandoned_header() {
        let cache = cache_at(45.0, Some("normal"));
        let cancel = AtomicBool::new(false);
        let report = RestoreReport::new();
        let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writes_w = writes.clone();
        let last = Arc::new(Mutex::new(42u8));
        let last_w = last.clone();
        let last_r = last.clone();

        let out = run_sweep_uni(
            &cache,
            "hwmon:test:pwm1",
            &[30],
            0,
            Duration::from_secs(1),
            move |p: u8| {
                writes_w.lock().unwrap().push(p);
                *last_w.lock().unwrap() = p;
                Ok(())
            },
            move || sample(Some(*last_r.lock().unwrap()), Some(1), Some(900)),
            &cancel,
            || false,
            || true,
            &report,
            |_| {},
        )
        .await;

        assert_eq!(out.state, STATE_COMPLETE, "detail: {:?}", out.detail);
        assert_eq!(*writes.lock().unwrap(), vec![30, 42], "precondition");
        assert_eq!(report.get(), RestoreOutcome::Restored);
        assert!(cache.abandoned_by_diagnostic().is_empty());
    }

    /// A sweep over 30/60/90 % whose 60 % write does not return, from a pre-run
    /// duty of `original`, logging every write and every pump lookup in order.
    /// The union flips to pump AT the stuck write. Returns the outcome, the
    /// restore token, and the events after the stuck write — having checked
    /// the stuck write was attempted.
    async fn stuck_sweep(
        has_mode_switch: bool,
        pump_at_start: bool,
        original: u8,
    ) -> (SweepOutcome, &'static str, Vec<String>, Vec<String>) {
        let cache = cache_at(40.0, None);
        let rig = Rig::new();
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let union = Arc::new(AtomicBool::new(false));
        let (u, ev) = (union.clone(), events.clone());
        let watch = PumpWatch::new(
            "hwmon:test:pwm1",
            "characterisation",
            pump_at_start,
            30,
            move || {
                ev.lock().unwrap().push("pump-lookup".into());
                u.load(Ordering::SeqCst)
            },
        );
        let (u, ev) = (union.clone(), events.clone());
        let write_fn = move |p: u8| {
            ev.lock().unwrap().push(format!("write {p}"));
            if p == 60 {
                u.store(true, Ordering::SeqCst);
                return std::future::ready(Err(WriteFailure::Unresponsive));
            }
            std::future::ready(Ok(()))
        };
        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &plan_of(&[30, 60, 90]),
            0,
            has_mode_switch,
            &watch,
            Duration::from_secs(2),
            None,
            write_fn,
            sync_read(move || sample(Some(original), Some(1), Some(900))),
            &rig.cancel,
            || false,
            || true,
            &rig.report,
            |_| {},
            |_| {},
        )
        .await;
        assert_eq!(out.state, STATE_FAILED, "{:?}", out.detail);
        assert!(
            out.detail
                .as_deref()
                .is_some_and(|d| d.contains("PWM write of 60%") && d.contains("did not return")),
            "{:?}",
            out.detail
        );
        assert_eq!(out.points.len(), 1, "the 30 % point, and no other");
        let (_, token) = rig.restore();
        let events = events.lock().unwrap().clone();
        let stuck = events
            .iter()
            .position(|e| e == "write 60")
            .expect("precondition: the stuck write was attempted");
        let (before, after) = events.split_at(stuck + 1);
        (out, token, before.to_vec(), after.to_vec())
    }

    /// [SAFETY] DEC-455 (`PTR-ab`): a point write that does not return ends the
    /// sweep `failed`, and on a header with a mode switch nothing more touches
    /// it — no further point, no restore (`skipped_unresponsive`), and no
    /// pump-watch lookup, which takes the controller lock the parked write
    /// still holds. The union flips to pump AT the stuck write, so this also
    /// pins 4A: no floored restore after a stuck write either. The engine's
    /// next tick hands the header back (DEC-382).
    #[tokio::test(start_paused = true)]
    async fn a_write_that_does_not_return_ends_the_sweep_and_touches_nothing_more() {
        let (_, token, before, after) = stuck_sweep(true, false, 42).await;
        assert_eq!(token, "skipped_unresponsive");
        assert!(
            before.iter().any(|e| e == "pump-lookup"),
            "precondition: the watch is read while the run is live: {before:?}"
        );
        assert_eq!(after, Vec::<String>::new(), "after the stuck write");
    }

    /// [SAFETY] DEC-455 review F1 (the user's choice): on a header with NO mode
    /// switch the engine never hands the header back — it holds only what it
    /// wrote (DEC-451) — so the normal restore is queued behind the stuck
    /// write, and still with no pump lookup. The union flipped at the stuck
    /// write was never read, so this restore is not floored: the documented
    /// cost of taking no lock.
    #[tokio::test(start_paused = true)]
    async fn a_stuck_write_on_a_header_with_no_mode_switch_queues_the_restore() {
        let (_, token, _, after) = stuck_sweep(false, false, 42).await;
        assert_eq!(after, vec!["write 42".to_string()], "only the restore");
        assert_eq!(token, "restored", "the fake's queued write returned");
    }

    /// DEC-455 review F1: the queued restore keeps the pump floor the watch
    /// already knows — here a pump from the start, restored from a pre-run 10 %
    /// to the 30 % floor — without a lookup.
    #[tokio::test(start_paused = true)]
    async fn a_queued_restore_keeps_a_known_pump_floor() {
        let (_, _, _, after) = stuck_sweep(false, true, 10).await;
        assert_eq!(after, vec!["write 30".to_string()]);
    }

    /// DEC-455: the RESTORE write not returning is reported as
    /// `skipped_unresponsive` — it may still land, so neither `restored` nor
    /// `write_failed` is true — and the completed sweep stays `complete`.
    #[tokio::test(start_paused = true)]
    async fn a_restore_write_that_does_not_return_is_reported_unresponsive() {
        let cache = cache_at(40.0, None);
        let rig = Rig::new();
        let writes = rig.writes.clone();
        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &plan_of(&[30]),
            0,
            true,
            &PumpWatch::fixed(false),
            Duration::from_secs(2),
            None,
            move |p: u8| {
                writes.lock().unwrap().push(p);
                std::future::ready(if p == 42 {
                    Err(WriteFailure::Unresponsive)
                } else {
                    Ok(())
                })
            },
            sync_read(|| sample(Some(42), Some(1), Some(900))),
            &rig.cancel,
            || false,
            || true,
            &rig.report,
            |_| {},
            |_| {},
        )
        .await;
        assert_eq!(out.state, STATE_COMPLETE, "{:?}", out.detail);
        assert_eq!(rig.written(), vec![30, 42], "the restore was attempted");
        assert_eq!(rig.restore(), (true, "skipped_unresponsive"));
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
    /// restore. The task is detached, so it outlives `hand_back_hwmon`.
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
                true,
                &PumpWatch::fixed(false),
                Duration::from_millis(1),
                None,
                crate::api::diagnostic_gates::sync_write(move |pct: u8| {
                    writes.lock().unwrap().push(pct);
                    *last_w.lock().unwrap() = pct;
                    Ok(())
                }),
                sync_read(move || {
                    let p = *last.lock().unwrap();
                    sample(Some(p), Some(1), rpm_for(p))
                }),
                &rig.cancel,
                || false,
                || true,
                &rig.report,
                |_| {},
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
        /// ENDS at the highest duty. `RestoreGuard` has exits that leave the header
        /// where the sweep put it (the module docs list them), and ending high
        /// keeps them all benign.
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
            let cache = cache_at(40.0, None);
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
                true,
                &PumpWatch::fixed(false),
                Duration::from_secs(constants::CHARACTERIZATION_SETTLE_MIN_S),
                None,
                crate::api::diagnostic_gates::sync_write(|_p: u8| Ok(())),
                sync_read(|| sample(Some(50), Some(1), Some(1200))),
                &cancel,
                || false,
                move || {
                    stamps_k.lock().unwrap().push(start.elapsed());
                    true
                },
                &report,
                |_| {},
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
            let cache = cache_at(40.0, None);
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
            let cache = cache_at(40.0, None);
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
                true,
                &PumpWatch::fixed(false),
                Duration::from_millis(1),
                None,
                crate::api::diagnostic_gates::sync_write(move |p: u8| {
                    writes.lock().unwrap().push(p);
                    *last_w.lock().unwrap() = p;
                    let mut n = seen.lock().unwrap();
                    *n += 1;
                    Ok(())
                }),
                sync_read(move || {
                    let p = *last.lock().unwrap();
                    sample(Some(p), Some(1), Some(500 + u16::from(p) * 20))
                }),
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
                |_| {},
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
            let cache = cache_at(40.0, None);
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
                true,
                &PumpWatch::fixed(false),
                Duration::from_millis(1),
                None,
                crate::api::diagnostic_gates::sync_write(move |p: u8| {
                    writes.lock().unwrap().push(p);
                    Ok(())
                }),
                sync_read(|| sample(Some(0), Some(1), Some(900))),
                &rig.cancel,
                || false,
                || true,
                &report,
                |_| {},
                |_| {},
            )
            .await;
            // Precondition: the sweep ran to its top point. Without it, a sweep
            // refused at its first point passes the loop below on the restore alone.
            assert!(
                rig.written().contains(&100),
                "precondition: the sweep reached 100%: {:?}",
                rig.written()
            );
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
            let cache = cache_at(40.0, None);
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
                true,
                &PumpWatch::fixed(false),
                Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
                None,
                crate::api::diagnostic_gates::sync_write(|_p: u8| Ok(())),
                sync_read(move || {
                    let mut n = reads_r.lock().unwrap();
                    *n += 1;
                    // Mid-settle, not before it and not between steps.
                    if *n == 3 {
                        flag.store(true, Ordering::SeqCst);
                    }
                    sample(Some(50), Some(1), Some(1200))
                }),
                &cancel,
                || false,
                || true,
                &report,
                |_| {},
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
            let cache = cache_at(40.0, None);
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
                true,
                &PumpWatch::fixed(false),
                settle,
                None,
                crate::api::diagnostic_gates::sync_write(|_p: u8| Ok(())),
                sync_read(move || {
                    let mut n = reads_r.lock().unwrap();
                    *n += 1;
                    if *n == 2 {
                        // Well inside the hold, and above CALIBRATION_MAX_TEMP_C.
                        cache_w
                            .update_sensors(vec![hot_cpu(constants::CALIBRATION_MAX_TEMP_C + 5.0)]);
                    }
                    sample(Some(50), Some(1), Some(1200))
                }),
                &cancel,
                || false,
                || true,
                &report,
                |_| {},
                |_| {},
            )
            .await;
            assert_eq!(out.state, STATE_ABORTED, "detail: {:?}", out.detail);
            // The HOT reading must be what aborted it. With no usable reading at
            // the start, the DEC-385 staleness refusal aborts first and this test
            // passes without ever reaching the dwell.
            assert!(
                out.detail
                    .as_deref()
                    .is_some_and(|d| d.contains("thermal abort")),
                "aborted for the wrong reason: {:?}",
                out.detail
            );
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

        /// A CPU reading older than every budget the cache can apply — aged by
        /// construction, since paused time does not advance `Instant`.
        fn stale_cpu(cache: &StateCache) -> CachedSensorReading {
            let mut r = hot_cpu(40.0);
            r.updated_at = std::time::Instant::now()
                - (crate::api::calibration::diagnostic_temp_max_age(cache)
                    + Duration::from_secs(5));
            r
        }

        /// [SAFETY] TS-q / DEC-385, per POINT: a poll that wedges after the first
        /// point stops the sweep at the next one, with the staleness named as the
        /// reason — the checks beside it read `value_c` and would carry on.
        #[tokio::test(start_paused = true)]
        async fn a_poll_that_wedges_between_points_aborts_the_sweep() {
            let rig = Rig::new();
            let cache = cache_at(40.0, None);
            let cache_w = &cache;
            let writes = rig.writes.clone();
            let out = run_sweep_uni(
                &cache,
                "hwmon:test:pwm1",
                &[40, 60, 80],
                0,
                Duration::from_millis(1),
                move |p: u8| {
                    writes.lock().unwrap().push(p);
                    if p == 40 {
                        cache_w.update_sensors(vec![stale_cpu(cache_w)]);
                    }
                    Ok(())
                },
                || sample(Some(40), Some(1), Some(900)),
                &rig.cancel,
                || false,
                || true,
                &rig.report,
                |_| {},
            )
            .await;
            assert_eq!(out.state, STATE_ABORTED, "detail: {:?}", out.detail);
            assert!(
                out.detail.as_deref().is_some_and(|d| d.contains("stale")),
                "aborted for the wrong reason: {:?}",
                out.detail
            );
            assert!(
                !rig.written().contains(&60),
                "a point past the wedge was written: {:?}",
                rig.written()
            );
        }

        /// DEC-385, within a DWELL: the renewal-cadence check sees a wedge inside
        /// one long hold, as it sees a hot reading — without it the hold would run
        /// to its end on frozen numbers.
        #[tokio::test(start_paused = true)]
        async fn a_poll_that_wedges_during_a_dwell_aborts_before_the_hold_ends() {
            let cache = cache_at(40.0, None);
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
                true,
                &PumpWatch::fixed(false),
                settle,
                None,
                crate::api::diagnostic_gates::sync_write(|_p: u8| Ok(())),
                sync_read(move || {
                    let mut n = reads_r.lock().unwrap();
                    *n += 1;
                    if *n == 2 {
                        cache_w.update_sensors(vec![stale_cpu(cache_w)]);
                    }
                    sample(Some(50), Some(1), Some(1200))
                }),
                &cancel,
                || false,
                || true,
                &report,
                |_| {},
                |_| {},
            )
            .await;
            assert_eq!(out.state, STATE_ABORTED, "detail: {:?}", out.detail);
            assert!(
                out.detail.as_deref().is_some_and(|d| d.contains("stale")),
                "aborted for the wrong reason: {:?}",
                out.detail
            );
            let observed = *reads.lock().unwrap() as u64;
            let worst_ticks = (constants::STABILITY_RENEW_INTERVAL_S * 1000
                / constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64)
                + 2;
            assert!(
                observed <= worst_ticks,
                "aborted after {observed} samples; the renewal-cadence check bounds it at \
                 {worst_ticks}"
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

        /// A tach register that refreshes every [`SLOW_REFRESH_MS`] and a fan that
        /// approaches each commanded duty first-order (τ = 1.5 s) — the it87 shape
        /// behind `PTR-a`/`PTR-b`, driven through the real `run_sweep` so the call
        /// site, not only the pure helpers, is what is under test (DEC-324).
        const SLOW_REFRESH_MS: u64 = 2000;

        struct SlowTach {
            started: tokio::time::Instant,
            /// (write instant, rpm at that instant, target rpm)
            segments: Vec<(tokio::time::Instant, f64, f64)>,
            last_pct: u8,
        }

        impl SlowTach {
            fn target(pct: u8) -> f64 {
                500.0 + f64::from(pct) * 20.0
            }
            fn continuous(&self, at: tokio::time::Instant) -> f64 {
                let seg = self
                    .segments
                    .iter()
                    .rev()
                    .find(|(t, _, _)| *t <= at)
                    .copied();
                match seg {
                    Some((t, from, to)) => {
                        let dt = at.duration_since(t).as_secs_f64();
                        to + (from - to) * (-dt / 1.5).exp()
                    }
                    None => Self::target(self.last_pct),
                }
            }
            /// The value the register holds now: the fan's speed at the latest
            /// refresh, plus a deterministic ±2 so every refresh is an update.
            fn register(&self) -> u16 {
                let since = tokio::time::Instant::now().duration_since(self.started);
                let k = since.as_millis() as u64 / SLOW_REFRESH_MS;
                let refreshed = self.started + Duration::from_millis(k * SLOW_REFRESH_MS);
                let jitter = [0.0, 2.0, -2.0][(k % 3) as usize];
                (self.continuous(refreshed) + jitter).round() as u16
            }
        }

        async fn sweep_slow_tach(rig: &Rig, cache: &StateCache, settle: Duration) -> SweepOutcome {
            let plan = resolve_sweep_plan(&[30, 60, 100], true, None);
            let writes = rig.writes.clone();
            let tach = Arc::new(Mutex::new(SlowTach {
                started: tokio::time::Instant::now(),
                segments: Vec::new(),
                last_pct: 42,
            }));
            let tach_w = tach.clone();
            run_sweep(
                cache,
                "hwmon:test:pwm1",
                &plan,
                0,
                true,
                &PumpWatch::fixed(false),
                settle,
                None,
                crate::api::diagnostic_gates::sync_write(move |pct: u8| {
                    writes.lock().unwrap().push(pct);
                    let mut t = tach_w.lock().unwrap();
                    let now = tokio::time::Instant::now();
                    let from = t.continuous(now);
                    t.segments.push((now, from, SlowTach::target(pct)));
                    t.last_pct = pct;
                    Ok(())
                }),
                sync_read(move || {
                    let t = tach.lock().unwrap();
                    sample(Some(t.last_pct), Some(1), Some(t.register()))
                }),
                &rig.cancel,
                || false,
                || true,
                &rig.report,
                |_| {},
                |_| {},
            )
            .await
        }

        /// DEC-405 through the real sweep: `PTR-b` (no settle before the
        /// register's first refresh), `PTR-a` (statistics over the settled tail
        /// only), `PTR-b` again (the published resolution is the register's
        /// cadence, not the sampler's) and `PTR-d` (per-leg monotonic).
        #[tokio::test(start_paused = true)]
        async fn a_slow_tach_sweep_reports_settling_stability_and_resolution_honestly() {
            let rig = Rig::new();
            let cache = cache_at(40.0, None);
            let out = sweep_slow_tach(
                &rig,
                &cache,
                Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
            )
            .await;
            assert_eq!(out.state, STATE_COMPLETE, "{:?}", out.detail);
            assert_eq!(
                out.points.len(),
                5,
                "100 ramp, 60/30 falling, 60/100 rising"
            );

            let mut settled_points = 0;
            for p in &out.points {
                let st = p.stability.as_ref().expect("every held point has stats");
                let (before, after) = (p.rpm_before.unwrap(), p.rpm_after.unwrap());
                if before.abs_diff(after) < 100 {
                    continue; // the fan barely moved; nothing to wait out
                }
                let settled = p.settled_ms.unwrap_or_else(|| {
                    panic!("{}% should settle inside a 12 s hold", p.requested_pct)
                });
                settled_points += 1;
                // PTR-b: the first register refresh after each write is 2000 ms
                // later (steps start on refresh boundaries), so nothing earlier
                // can be a settle. The old sample-counting rule said 500.
                assert!(
                    settled >= SLOW_REFRESH_MS,
                    "{}% settled at {settled} ms, before the first refresh",
                    p.requested_pct
                );
                // PTR-a: the window the figures describe opens at the settle, and
                // no reading from the ramp before it survives into them.
                assert_eq!(st.window_start_ms, settled);
                let band = f64::from(after) * constants::SETTLING_BAND_PCT / 100.0;
                for extreme in [st.min_rpm.unwrap(), st.max_rpm.unwrap()] {
                    assert!(
                        f64::from(extreme.abs_diff(after)) <= band,
                        "{}%: {extreme} rpm is a transient reading inside the window",
                        p.requested_pct
                    );
                }
                assert_eq!(st.verdict, crate::api::stats::STABILITY_STABLE);
                assert_eq!(st.update_interval_ms, Some(SLOW_REFRESH_MS));
            }
            assert!(
                settled_points >= 3,
                "precondition: the sweep really moved the fan"
            );

            let s = summarise(&out.points, &[], None);
            assert_eq!(
                s.measurement_resolution_ms,
                Some(SLOW_REFRESH_MS),
                "the register's cadence, never the 500 ms sampler's"
            );
            assert_eq!(s.stability_verdict, crate::api::stats::STABILITY_STABLE);
            // PTR-d: a working fan walked down then up is monotonic on each leg.
            assert_eq!(s.monotonic_falling, Some(true));
            assert_eq!(s.monotonic_rising, Some(true));
            assert_eq!(s.monotonic, Some(true));
            // A declared driver cadence outranks the observed one.
            assert_eq!(
                summarise(&out.points, &[], Some(1000)).measurement_resolution_ms,
                Some(1000)
            );
        }

        /// `P8-bg`: every step announces its settle before the hold, and a
        /// dwell step announces the dwell once the settle has elapsed — each
        /// naming the step index and duty the point it becomes will carry.
        /// Asserted against the plan and the published points, not a list.
        #[tokio::test(start_paused = true)]
        async fn every_phase_is_announced_with_the_point_it_becomes() {
            let rig = Rig::new();
            let cache = cache_at(40.0, None);
            let settle = Duration::from_secs(2);
            let dwell = Duration::from_secs(6);
            let plan = resolve_sweep_plan(&[30, 60, 100], true, Some(dwell));
            // Precondition: the plan really has dwell and non-dwell steps.
            assert!(plan.iter().any(|s| s.dwell.is_some()));
            assert!(plan.iter().any(|s| s.dwell.is_none()));
            let writes = rig.writes.clone();
            let last = Arc::new(Mutex::new(42u8));
            let last_w = last.clone();
            let steps: Arc<Mutex<Vec<RunStep>>> = Arc::default();
            let steps_a = steps.clone();
            let published: Arc<Mutex<Vec<(u16, u8, usize)>>> = Arc::default();
            let published_p = published.clone();
            let out = run_sweep(
                &cache,
                "hwmon:test:pwm1",
                &plan,
                0,
                true,
                &PumpWatch::fixed(false),
                settle,
                None,
                crate::api::diagnostic_gates::sync_write(move |pct: u8| {
                    writes.lock().unwrap().push(pct);
                    *last_w.lock().unwrap() = pct;
                    Ok(())
                }),
                sync_read(move || {
                    let p = *last.lock().unwrap();
                    sample(Some(p), Some(1), Some(600 + u16::from(p) * 10))
                }),
                &rig.cancel,
                || false,
                || true,
                &rig.report,
                // Record how many phases had been announced when each point
                // landed, so the order is asserted, not just the contents.
                move |pt: CharPoint| {
                    let n = steps_a.lock().unwrap().len();
                    published_p
                        .lock()
                        .unwrap()
                        .push((pt.step_index, pt.requested_pct, n));
                },
                {
                    let steps = steps.clone();
                    move |st: RunStep| steps.lock().unwrap().push(st)
                },
            )
            .await;
            assert_eq!(out.state, STATE_COMPLETE, "{:?}", out.detail);
            let steps = steps.lock().unwrap().clone();

            let mut expected: Vec<(&str, u16, u8)> = Vec::new();
            for (idx, step) in plan.iter().enumerate() {
                expected.push((STEP_PHASE_SETTLE, idx as u16, step.pct));
                if step.dwell.is_some() {
                    expected.push((STEP_PHASE_DWELL, idx as u16, step.pct));
                }
            }
            let got: Vec<(&str, u16, u8)> = steps
                .iter()
                .map(|s| (s.phase.as_str(), s.index, s.duty_pct))
                .collect();
            assert_eq!(got, expected);
            for s in &steps {
                let bound = if s.phase == STEP_PHASE_SETTLE {
                    settle
                } else {
                    dwell
                };
                assert!(
                    s.max_ms > 0 && s.max_ms <= bound.as_millis() as u64,
                    "{s:?} against a bound of {bound:?}"
                );
                assert!(s.started_unix_ms > 0);
            }
            // Each point lands after its own phases were announced and before
            // the next step's.
            for (step_index, pct, announced) in published.lock().unwrap().iter() {
                let last = &steps[announced - 1];
                assert_eq!((last.index, last.duty_pct), (*step_index, *pct));
            }
        }

        /// `PTR-m` through the real sweep. A smooth high-RPM device — a few rpm
        /// of jitter at ~2700 — steps 125 rpm per point. The proportional rule
        /// needed more than a tenth of the reading (~265 rpm here) and called
        /// every one of those genuine responses `unchanged`; judged against the
        /// point's own settled spread they are unmistakable.
        #[tokio::test(start_paused = true)]
        async fn a_smooth_high_rpm_step_is_changed_against_its_own_noise() {
            let rig = Rig::new();
            let cache = cache_at(40.0, None);
            let writes = rig.writes.clone();
            let level = Arc::new(Mutex::new(90u8));
            let level_w = level.clone();
            let reads = Arc::new(Mutex::new(0u32));
            let rpm_for = |pct: u8| 2650 + (u16::from(pct) - 90) * 25;
            let out = run_sweep_uni(
                &cache,
                "hwmon:test:pwm1",
                &[95, 100],
                0,
                Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
                move |pct: u8| {
                    writes.lock().unwrap().push(pct);
                    *level_w.lock().unwrap() = pct;
                    Ok(())
                },
                move || {
                    let mut n = reads.lock().unwrap();
                    *n += 1;
                    // Changes direction every read, so it settles at once.
                    let jitter: i32 = [0, 3, -2, 1][(*n % 4) as usize];
                    let p = *level.lock().unwrap();
                    let rpm = (i32::from(rpm_for(p)) + jitter) as u16;
                    sample(Some(p), Some(1), Some(rpm))
                },
                &rig.cancel,
                || false,
                || true,
                &rig.report,
                |_| {},
            )
            .await;
            assert_eq!(out.state, STATE_COMPLETE, "{:?}", out.detail);
            assert_eq!(out.points.len(), 2);
            for p in &out.points {
                let (before, after) = (p.rpm_before.unwrap(), p.rpm_after.unwrap());
                // Precondition: the old proportional rule really did call this
                // step unchanged, so a `changed` below is the new rule's doing.
                assert!(
                    f64::from(before.abs_diff(after)) <= rpm_move_threshold(before, None),
                    "precondition: {before} -> {after} is under the proportional threshold"
                );
                let st = p.stability.as_ref().unwrap();
                assert_eq!(st.verdict, crate::api::stats::STABILITY_STABLE);
                assert_eq!(
                    p.rpm_verdict, "changed",
                    "{}%: {before} -> {after} rpm against a σ of {:?}",
                    p.requested_pct, st.stddev_rpm
                );
            }
        }

        /// `PTR-y`, the pure half. A smooth ~2650 rpm tach stepping 125 rpm: the
        /// verdict's settled-σ threshold sees it, so the response time must too.
        /// The old live detector needed more than a tenth of the reading.
        #[test]
        fn first_change_uses_the_verdicts_threshold() {
            let st = PointStability {
                verdict: crate::api::stats::STABILITY_STABLE.into(),
                stddev_rpm: Some(2.0),
                ..Default::default()
            };
            let samples: Vec<crate::api::stats::RpmSample> = [2651, 2776, 2773, 2775]
                .iter()
                .enumerate()
                .map(|(i, &rpm)| crate::api::stats::RpmSample {
                    at_ms: 500 * (i as u64 + 1),
                    rpm: Some(rpm),
                })
                .collect();
            // Precondition: the proportional rule calls the step unmoved.
            assert!(f64::from(2650u16.abs_diff(2775)) <= rpm_move_threshold(2650, None));
            assert_eq!(first_change_ms(&samples, Some(2650), None, None), None);
            // The first sample is jitter, the second is the step.
            assert_eq!(
                first_change_ms(&samples, Some(2650), Some((2500, 2775)), Some(&st)),
                Some(1000)
            );
            assert_eq!(rpm_verdict(Some(2650), Some(2775), Some(&st)), "changed");
        }

        /// DEC-454 review (P3): a hold whose every tach read failed has no
        /// reaction time to report. The after-read's stamp would be the hold's
        /// length — an upper bound — so the point stays `None`, as before, even
        /// though the verdict reads `changed`.
        #[test]
        fn an_unreadable_hold_publishes_no_first_change() {
            let unreadable = [
                crate::api::stats::RpmSample {
                    at_ms: 500,
                    rpm: None,
                },
                crate::api::stats::RpmSample {
                    at_ms: 1000,
                    rpm: None,
                },
            ];
            // Precondition: the verdict does read the step as moved.
            assert_eq!(rpm_verdict(Some(1000), Some(1300), None), "changed");
            assert_eq!(
                first_change_ms(&unreadable, Some(1000), Some((26_000, 1300)), None),
                None
            );
            // One readable sample is enough to let the after-read stand.
            let one = [
                unreadable[0],
                crate::api::stats::RpmSample {
                    at_ms: 1000,
                    rpm: Some(1000),
                },
            ];
            assert_eq!(
                first_change_ms(&one, Some(1000), Some((26_000, 1300)), None),
                Some(26_000)
            );
        }

        /// `PTR-y`: the after-read is the last candidate, so a verdict that reads
        /// `changed` has a time even when no readable hold sample had moved yet.
        /// And nothing moved is `None`, never an instant response.
        #[test]
        fn a_changed_verdict_always_has_a_first_change() {
            let unmoved = [crate::api::stats::RpmSample {
                at_ms: 500,
                rpm: Some(1000),
            }];
            assert_eq!(rpm_verdict(Some(1000), Some(1300), None), "changed");
            assert_eq!(
                first_change_ms(&unmoved, Some(1000), Some((6000, 1300)), None),
                Some(6000)
            );
            assert_eq!(rpm_verdict(Some(1000), Some(1010), None), "unchanged");
            assert_eq!(
                first_change_ms(&unmoved, Some(1000), Some((6000, 1010)), None),
                None
            );
            // No reference reading: nothing to have moved from.
            assert_eq!(
                first_change_ms(&unmoved, None, Some((6000, 1300)), None),
                None
            );
        }

        /// `PTR-y` through the real sweep and on into the session summary. The
        /// smooth device of `a_smooth_high_rpm_step_is_changed_against_its_own_noise`
        /// reads `changed` at every step; before the fix none of those steps had a
        /// response time, so the member's `response_latency` read `unavailable`.
        #[tokio::test(start_paused = true)]
        async fn a_changed_step_is_timed_and_the_session_reads_its_latency() {
            use crate::validation::session::{
                DevicePolicySnapshot, EvidenceRef, SessionMetadata, ValidationSession,
                DIAG_CHARACTERIZATION, F_RESPONSE_LATENCY, KIND_VALIDATION, RESULT_OBSERVED,
                STATE_COMPLETED,
            };
            let rig = Rig::new();
            let cache = cache_at(40.0, None);
            let level = Arc::new(Mutex::new(90u8));
            let level_w = level.clone();
            let reads = Arc::new(Mutex::new(0u32));
            let rpm_for = |pct: u8| 2650 + (u16::from(pct) - 90) * 25;
            let out = run_sweep_uni(
                &cache,
                "hwmon:test:pwm1",
                &[95, 100],
                0,
                Duration::from_secs(constants::CHARACTERIZATION_DEFAULT_SETTLE_S),
                move |pct: u8| {
                    *level_w.lock().unwrap() = pct;
                    Ok(())
                },
                move || {
                    let mut n = reads.lock().unwrap();
                    *n += 1;
                    let jitter: i32 = [0, 3, -2, 1][(*n % 4) as usize];
                    let p = *level.lock().unwrap();
                    sample(
                        Some(p),
                        Some(1),
                        Some((i32::from(rpm_for(p)) + jitter) as u16),
                    )
                },
                &rig.cancel,
                || false,
                || true,
                &rig.report,
                |_| {},
            )
            .await;
            assert_eq!(out.state, STATE_COMPLETE, "{:?}", out.detail);
            assert_eq!(out.points.len(), 2);
            for p in &out.points {
                assert_eq!(p.rpm_verdict, "changed", "precondition: {p:?}");
                // The tach steps with the write, so the first hold sample moved.
                assert_eq!(
                    p.first_change_ms,
                    Some(constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64),
                    "{}% read changed with no response time",
                    p.requested_pct
                );
            }

            let run = CharacterizationRun {
                run_id: "char-1".into(),
                header_id: "hwmon:test:pwm1".into(),
                state: STATE_COMPLETE.into(),
                points: out.points,
                ..Default::default()
            };
            let session = ValidationSession {
                session_id: "val-1".into(),
                kind: KIND_VALIDATION.into(),
                state: STATE_COMPLETED.into(),
                started_unix_ms: 1,
                completed_unix_ms: Some(2),
                metadata: SessionMetadata {
                    cooling_device_id: "dev-1".into(),
                    device_name: "Test AIO".into(),
                    device_kind: "aio_liquid".into(),
                    pump_member: None,
                    radiator_members: vec![],
                    auxiliary_members: vec![],
                    temperature_sensor: None,
                    coolant_sensor: None,
                    coolant_telemetry: "unavailable".into(),
                    device_policy: DevicePolicySnapshot {
                        id: "generic_pump".into(),
                        display_name: "Generic pump".into(),
                        minimum_safe_pwm_pct: 30.0,
                        supports_stop: false,
                        startup_override_seconds: None,
                        expected_rpm_min: None,
                        expected_rpm_max: None,
                        internal_control_possible: true,
                    },
                    members: vec![],
                    active_profile_id: None,
                    active_profile_name: None,
                    daemon_version: "0.0.0-test".into(),
                    user_metadata: Default::default(),
                },
                requested_diagnostics: vec![],
                sweep_members: vec![],
                samples: vec![],
                events: vec![],
                evidence: vec![EvidenceRef {
                    kind: DIAG_CHARACTERIZATION.into(),
                    member_id: "hwmon:test:pwm1".into(),
                    run_id: Some("char-1".into()),
                    started_unix_ms: 1,
                    completed_unix_ms: Some(2),
                    outcome: RESULT_OBSERVED.into(),
                    detail: None,
                    characterization: Some(run),
                    verify: None,
                    control_path: None,
                }],
                external_measurements: vec![],
                findings: vec![],
                sample_limit_reached: false,
                interrupted_reason: None,
                truncated_at_unix_ms: None,
                auto_started: false,
                stop_when_diagnostics_complete: false,
                startup_fingerprints: vec![],
                steady_state: None,
            };
            let findings = crate::validation::summary::summarise(&session);
            let latency: Vec<_> = findings
                .iter()
                .filter(|f| f.id == F_RESPONSE_LATENCY)
                .collect();
            assert_eq!(latency.len(), 1, "{latency:?}");
            assert_eq!(latency[0].member_id.as_deref(), Some("hwmon:test:pwm1"));
            assert_eq!(latency[0].state, RESULT_OBSERVED, "{:?}", latency[0].detail);
        }

        /// A tach that never changes value establishes no cadence: UNKNOWN, not
        /// the sampler's 500 ms (§5 — the figure this used to publish).
        #[tokio::test(start_paused = true)]
        async fn a_tach_that_never_changes_publishes_no_resolution() {
            let rig = Rig::new();
            let cache = cache_at(40.0, None);
            let out = sweep_bidi(&rig, &cache, &[30, 100], 42, |p| {
                Some(500 + u16::from(p) * 20)
            })
            .await;
            assert!(!out.points.is_empty());
            assert_eq!(
                summarise(&out.points, &[], None).measurement_resolution_ms,
                None
            );
        }

        /// PTR-d, the other arm: a genuinely non-monotonic LEG is still `false`,
        /// and says which leg.
        #[test]
        fn a_non_monotonic_leg_is_reported_and_named() {
            let pts = vec![
                point_dir(100, Some(100), Some(1), Some(2500), "ramp"),
                point_dir(60, Some(60), Some(1), Some(1700), "falling"),
                point_dir(30, Some(30), Some(1), Some(1100), "falling"),
                // Rising leg: 60% reads FASTER than 100% — a real dip at the top.
                point_dir(60, Some(60), Some(1), Some(1700), "rising"),
                point_dir(100, Some(100), Some(1), Some(1200), "rising"),
            ];
            let s = summarise(&pts, &[], None);
            assert_eq!(s.monotonic_falling, Some(true));
            assert_eq!(s.monotonic_rising, Some(false));
            assert_eq!(s.monotonic, Some(false));
            // And a unidirectional walk is still judged in walk order.
            let uni = vec![
                point_dir(30, Some(30), Some(1), Some(900), "ramp"),
                point_dir(60, Some(60), Some(1), Some(1500), "rising"),
                point_dir(100, Some(100), Some(1), Some(2400), "rising"),
            ];
            let u = summarise(&uni, &[], None);
            assert_eq!(
                (u.monotonic, u.monotonic_falling, u.monotonic_rising),
                (Some(true), None, None)
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
                summarise(&uni, &[], None).hysteresis_verdict,
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
            let s = summarise(&pts, &[], None);
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
            assert_eq!(summarise(&pts, &[], None).outside_learned_range, None);
        }

        #[test]
        fn a_reading_inside_the_learned_band_is_false_not_none() {
            let pts = vec![point_dir(50, Some(50), Some(1), Some(2000), "rising")];
            let learned = [LearnedPoint {
                duty_pct: 50,
                rpm_min: 1900,
                rpm_max: 2100,
            }];
            assert_eq!(
                summarise(&pts, &learned, None).outside_learned_range,
                Some(false)
            );
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
            let s = summarise(&pts, &learned, None);
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
            let s = summarise(&[p], &learned, None);
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

    // ── `P8-g`: the cap thins, it does not truncate ──

    #[test]
    fn a_fine_grained_request_is_thinned_across_the_range_not_truncated_to_its_bottom() {
        // The defect: `truncate` kept the first 20 ascending values, so 20..100
        // in steps of 1 was served 20-39% and reported as the sweep. Assert the
        // RANGE, which is what truncation destroys and thinning preserves —
        // asserting the length alone passes with the defect in place.
        let requested: Vec<u8> = (20..=100).collect();
        let pts = resolve_points(Some(&requested), 20);

        assert!(
            pts.len() <= constants::CHARACTERIZATION_MAX_POINTS,
            "the cap must still bind: {pts:?}"
        );
        assert_eq!(
            pts.first().copied(),
            Some(20),
            "the bottom of the range is kept"
        );
        assert_eq!(
            pts.last().copied(),
            Some(100),
            "the TOP of the range is kept — this is the assertion truncation fails"
        );
        // Precondition that the input really was over the cap, or the test is
        // asserting nothing about thinning at all.
        assert!(requested.len() > constants::CHARACTERIZATION_MAX_POINTS);
    }

    #[test]
    fn thinning_preserves_the_floor_and_the_ascending_order() {
        // [SAFETY] The two invariants `resolve_points` exists to guarantee. They
        // must hold for a thinned list exactly as they did for a truncated one:
        // no point below the effective floor, none at zero, and ascending — the
        // property that makes an aborted sweep leave the header at the highest
        // duty it reached.
        for floor in [0u8, 20, PUMP_FLOOR, 55] {
            let requested: Vec<u8> = (0..=100).collect();
            let pts = resolve_points(Some(&requested), floor);
            let effective = floor.max(constants::CHARACTERIZATION_MIN_PCT);

            assert!(!pts.is_empty(), "floor {floor}: a sweep must have points");
            assert!(
                pts.iter().all(|&p| p >= effective),
                "floor {floor}: every point must be at or above {effective}: {pts:?}"
            );
            assert!(
                pts.iter().all(|&p| p > 0),
                "floor {floor}: no point may be zero: {pts:?}"
            );
            assert!(
                pts.windows(2).all(|w| w[0] < w[1]),
                "floor {floor}: points must stay strictly ascending: {pts:?}"
            );
            assert!(
                pts.len() <= constants::CHARACTERIZATION_MAX_POINTS,
                "floor {floor}: cap must bind: {pts:?}"
            );
        }
    }

    #[test]
    fn thinning_twice_still_keeps_both_ends_at_every_size() {
        // A bidirectional request is thinned TWICE — `resolve_points` to
        // `MAX_POINTS`, then `resolve_sweep_plan` to
        // `MAX_UNIQUE_BIDIRECTIONAL`. Both reviewers reached "first and last
        // survive" algebraically; this checks it instead of arguing it, across
        // every input size that can reach the two-stage path.
        for n in 2..=101usize {
            let src: Vec<u8> = (0..n).map(|i| (i % 101) as u8).collect();
            let mut src: Vec<u8> = src.into_iter().collect();
            src.sort_unstable();
            src.dedup();
            if src.len() < 2 {
                continue;
            }
            let (lo, hi) = (src[0], src[src.len() - 1]);

            let once = thin_to(&src, constants::CHARACTERIZATION_MAX_POINTS);
            let twice = thin_to(&once, constants::CHARACTERIZATION_MAX_UNIQUE_BIDIRECTIONAL);

            assert_eq!(
                once.first().copied(),
                Some(lo),
                "n={n}: first lost at stage 1"
            );
            assert_eq!(
                once.last().copied(),
                Some(hi),
                "n={n}: last lost at stage 1"
            );
            assert_eq!(
                twice.first().copied(),
                Some(lo),
                "n={n}: first lost at stage 2"
            );
            assert_eq!(
                twice.last().copied(),
                Some(hi),
                "n={n}: last lost at stage 2"
            );
            assert!(
                twice.windows(2).all(|w| w[0] < w[1]),
                "n={n}: two-stage thinning must stay strictly ascending: {twice:?}"
            );
            assert!(twice.len() <= constants::CHARACTERIZATION_MAX_UNIQUE_BIDIRECTIONAL);
        }
    }

    #[test]
    fn a_request_within_the_cap_is_returned_unchanged() {
        // The complement: thinning must not disturb a list that already fits,
        // or every ordinary request silently changes shape.
        let pts = resolve_points(Some(&[20, 40, 60, 80, 100]), 20);
        assert_eq!(pts, vec![20, 40, 60, 80, 100]);
    }

    // ── DEC-420 (`PTR-v`): bounded reads, and the stall probe's write rules ──

    type ReadFut = std::future::Ready<Option<HwmonVerifyState>>;

    /// A read that counts itself and answers from `script(n)` for the n-th read
    /// (1-based). `None` is a read that did not return within its bound.
    fn scripted_read(
        reads: Arc<Mutex<usize>>,
        script: impl Fn(usize) -> Option<HwmonVerifyState>,
    ) -> impl Fn() -> ReadFut {
        move || {
            let mut n = reads.lock().unwrap();
            *n += 1;
            std::future::ready(script(*n))
        }
    }

    /// Run a two-step (30 %, 60 %) sweep with a 2 s settle — four in-hold
    /// samples a step, so the reads are: 1 pre-sweep; 2 step 0's reference;
    /// 3-6 its samples; 7 its after-read; 8 step 1's reference.
    async fn bounded_sweep(
        rig: &Rig,
        read_fn: impl Fn() -> ReadFut,
        pump_check: impl Fn() -> bool + Send + Sync,
        shutting_down: impl Fn() -> bool,
    ) -> SweepOutcome {
        let cache = cache_at(45.0, Some("normal"));
        let watch = PumpWatch::new("hwmon:test:pwm1", "characterisation", false, 30, pump_check);
        let writes = rig.writes.clone();
        run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &plan_of(&[30, 60]),
            0,
            true,
            &watch,
            Duration::from_secs(2),
            None,
            crate::api::diagnostic_gates::sync_write(move |p: u8| {
                writes.lock().unwrap().push(p);
                Ok(())
            }),
            read_fn,
            &rig.cancel,
            shutting_down,
            || true,
            &rig.report,
            |_| {},
            |_| {},
        )
        .await
    }

    /// [SAFETY] A pre-sweep read that does not return ends the run before any
    /// write, and the header is not read again.
    #[tokio::test(start_paused = true)]
    async fn a_pre_sweep_read_that_never_returns_ends_the_run_unwritten() {
        let rig = Rig::new();
        let reads = Arc::new(Mutex::new(0));
        let out = bounded_sweep(
            &rig,
            scripted_read(reads.clone(), |_| None),
            || false,
            || false,
        )
        .await;
        assert_eq!(out.state, STATE_ABORTED);
        assert!(out.detail.as_deref().unwrap().contains("did not return"));
        assert!(rig.written().is_empty());
        assert_eq!(*reads.lock().unwrap(), 1, "never read again");
        assert_eq!(out.original_pct, None);
        assert_eq!(rig.restore(), (false, "restored"), "the header never moved");
    }

    /// [SAFETY] A sample that does not return mid-hold ends the run at once —
    /// the gates were blind for as long as it was outstanding — the header is
    /// not read again, and (the user's choice, DEC-420 review) nothing more is
    /// WRITTEN to it either: a restore goes through `set_pwm`, whose own reads
    /// are unbounded and run under the controller lock. It stays at the swept
    /// duty, and the run says so (`skipped_unresponsive`, header left moved).
    #[tokio::test(start_paused = true)]
    async fn a_sample_that_never_returns_mid_hold_ends_the_run_and_writes_nothing_more() {
        let rig = Rig::new();
        let reads = Arc::new(Mutex::new(0));
        let out = bounded_sweep(
            &rig,
            scripted_read(reads.clone(), |n| {
                (n != 3).then(|| sample(Some(50), Some(1), Some(900)))
            }),
            || false,
            || false,
        )
        .await;
        assert_eq!(out.state, STATE_ABORTED);
        assert!(out.detail.as_deref().unwrap().contains("did not return"));
        assert_eq!(
            *reads.lock().unwrap(),
            3,
            "never read again after the wedge"
        );
        assert_eq!(out.original_pct, Some(50));
        assert_eq!(rig.written(), vec![30], "the sweep point, and no restore");
        assert_eq!(rig.restore(), (true, "skipped_unresponsive"));
    }

    /// [SAFETY] The same before the first write: a reference read that does not
    /// return leaves the header untouched — no restore even of a known duty,
    /// which the guard writes back for an unmoved run otherwise — and reports it
    /// as where it was found.
    #[tokio::test(start_paused = true)]
    async fn a_reference_read_that_never_returns_writes_nothing_at_all() {
        let rig = Rig::new();
        let reads = Arc::new(Mutex::new(0));
        let out = bounded_sweep(
            &rig,
            scripted_read(reads.clone(), |n| {
                (n != 2).then(|| sample(Some(50), Some(1), Some(900)))
            }),
            || false,
            || false,
        )
        .await;
        assert_eq!(out.state, STATE_ABORTED);
        assert_eq!(*reads.lock().unwrap(), 2);
        assert!(rig.written().is_empty(), "{:?}", rig.written());
        assert_eq!(rig.restore(), (false, "restored"), "the header never moved");
    }

    /// [SAFETY] The pump exception (DEC-420 review, the user's choice): a header
    /// that became pump-protected during the run — here, seen only by the
    /// restore's re-read, after the hung read — still gets its restore floored
    /// at the pump floor, because DEC-418's 30 % outranks the risk of a write
    /// that hangs. Its pre-sweep 10 % is written back as 30 %.
    #[tokio::test(start_paused = true)]
    async fn a_header_that_became_a_pump_is_still_floored_after_a_hung_read() {
        let rig = Rig::new();
        let reads = Arc::new(Mutex::new(0));
        let pump = Arc::new(AtomicBool::new(false));
        let p = pump.clone();
        let out = bounded_sweep(
            &rig,
            scripted_read(reads.clone(), move |n| {
                if n == 3 {
                    p.store(true, Ordering::SeqCst);
                    return None;
                }
                Some(sample(Some(10), Some(1), Some(900)))
            }),
            move || pump.load(Ordering::SeqCst),
            || false,
        )
        .await;
        assert_eq!(out.state, STATE_ABORTED);
        assert!(out.detail.as_deref().unwrap().contains("did not return"));
        assert_eq!(
            rig.written(),
            vec![30, PUMP_FLOOR],
            "the point, then the floor"
        );
        assert_eq!(rig.restore(), (false, "restored"));
    }

    /// [SAFETY] The reference read comes BEFORE the gates (the stall probe's
    /// rule). A cancel that arrives while it is outstanding — a read wedged
    /// across the request — must stop the step's write. With the read after
    /// the gates, as it was, the cancel is seen only after the write lands.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_during_the_reference_read_stops_the_write() {
        let rig = Arc::new(Rig::new());
        let reads = Arc::new(Mutex::new(0));
        let r = rig.clone();
        let out = bounded_sweep(
            &rig,
            scripted_read(reads.clone(), move |n| {
                if n == 2 {
                    r.cancel.store(true, Ordering::SeqCst);
                }
                Some(sample(Some(50), Some(1), Some(900)))
            }),
            || false,
            || false,
        )
        .await;
        // `>= 2`, not `== 2`: how many reads follow depends on whether the
        // write landed, which is what this test is asking (DEC-348).
        assert!(
            *reads.lock().unwrap() >= 2,
            "precondition: the reference read ran"
        );
        assert_eq!(out.state, STATE_CANCELLED, "{:?}", out.detail);
        // Only the restore, which writes the captured duty back even for a run
        // that never moved the header (pre-existing, DEC-407's Consequences).
        assert_eq!(rig.written(), vec![50], "the 30 % point must not land");
    }

    /// [SAFETY] Shutdown is re-checked immediately before the write. Here it
    /// begins during the pump re-read, which sits after the step gate: without
    /// the re-check the step's duty is written after the hand-back started.
    #[tokio::test(start_paused = true)]
    async fn a_shutdown_after_the_step_gate_stops_the_write() {
        let rig = Rig::new();
        let down = Arc::new(AtomicBool::new(false));
        let (d, seen) = (down.clone(), Arc::new(AtomicBool::new(false)));
        let seen_c = seen.clone();
        let out = bounded_sweep(
            &rig,
            sync_read(|| sample(Some(50), Some(1), Some(900))),
            move || {
                seen_c.store(true, Ordering::SeqCst);
                d.store(true, Ordering::SeqCst);
                false
            },
            move || down.load(Ordering::SeqCst),
        )
        .await;
        assert!(
            seen.load(Ordering::SeqCst),
            "precondition: the pump re-read ran"
        );
        assert_eq!(out.state, STATE_ABORTED);
        assert!(out.detail.as_deref().unwrap().contains("shutting down"));
        assert!(rig.written().is_empty(), "{:?}", rig.written());
    }

    /// [SAFETY] A mode change seen while shutting down is the hand-back's own
    /// write, reported as the shutdown — never as another controller's reclaim.
    #[tokio::test(start_paused = true)]
    async fn a_mode_change_seen_while_shutting_down_is_the_shutdown() {
        let rig = Rig::new();
        let down = Arc::new(AtomicBool::new(false));
        let d = down.clone();
        let reads = Arc::new(Mutex::new(0));
        let out = bounded_sweep(
            &rig,
            scripted_read(reads.clone(), move |n| {
                if n == 7 {
                    // Step 0's after-read: the hand-back has begun and given
                    // the header back to mode 2.
                    d.store(true, Ordering::SeqCst);
                    return Some(sample(Some(30), Some(2), Some(900)));
                }
                Some(sample(Some(50), Some(1), Some(900)))
            }),
            || false,
            move || down.load(Ordering::SeqCst),
        )
        .await;
        assert_eq!(
            *reads.lock().unwrap(),
            7,
            "precondition: the after-read ran"
        );
        assert_eq!(out.state, STATE_ABORTED);
        let detail = out.detail.unwrap();
        assert!(detail.contains("shutting down"), "{detail}");
        assert!(!detail.contains("reclaimed"), "{detail}");
        assert_eq!(
            rig.written(),
            vec![30],
            "and no restore while shutting down"
        );
    }
}
