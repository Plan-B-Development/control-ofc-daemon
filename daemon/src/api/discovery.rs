//! Safe PWM ↔ tach control-path discovery (AIO Phase 8 Batch 1 §2).
//!
//! # The question this answers
//!
//! `hwmon/pwm_discovery.rs` pairs `pwmN` with `fanN_input` **by index**. That is
//! a naming convention, not a measurement, and on real boards it is routinely
//! wrong — a splitter puts two fans on one tach, a Y-cable puts one fan's tach
//! on a channel with no PWM at all, and some vendors simply do not line the
//! indices up. §2 asks for the relationship to be *established* instead: perturb
//! one PWM output, watch every tach, and report which ones moved with what
//! confidence.
//!
//! # What makes it safe
//!
//! This is conceptually `pwmconfig`'s correlation trick, and §2 says outright it
//! "must not copy its stop-the-fan safety model". `pwmconfig` stops each fan in
//! turn and watches for the tach that hits zero. That is the single most
//! dangerous thing you can do to a liquid cooler, so none of it is reused:
//!
//! * [`perturbation_target`] moves the header **away from the nearer rail**, so
//!   there is always headroom and the swing is never toward a stall.
//! * Every commanded duty is clamped into
//!   `[max(DISCOVERY_MIN_PCT, header floor) .. 100]`, so **0 % is unreachable**
//!   for any header and a pump-protected one never crosses its 30 % floor.
//! * The header is returned to its baseline between cycles, and to its captured
//!   pre-run duty on every exit path, by the same
//!   [`RestoreOnDrop`](crate::api::characterization) guard the characterisation
//!   sweep uses — including its two deliberate skips (shutdown, thermal force)
//!   and its load-bearing drop order.
//! * A pump whose tach **disappears** mid-run aborts immediately
//!   ([`pump_tach_lost`]) — §1 lists that as an abort trigger, and it is the one
//!   signal that distinguishes "the pump is fine and we are perturbing it" from
//!   "the pump has stopped".
//!
//! # Two cycles, not one
//!
//! [`crate::constants::DISCOVERY_DEFAULT_CYCLES`] is 2 because §2 lists
//! repeatability as a confidence input. One cycle cannot tell a tach that
//! responded from a tach that happened to drift while we were looking; two can,
//! and the difference is the whole gap between `confirmed` and `ambiguous`.
//!
//! # The 3× rule is a confidence input, not a gate
//!
//! An obvious design is "the target must move 3× more than any other channel".
//! It is wrong, and the way it fails matters: two fans on a splitter, or a pump
//! and its own second tach lead, both respond to the same header — and the 3×
//! test would reject **both**, reporting `no_tach_response` for a header that
//! demonstrably drives two tachs. §2 requires "one PWM → multiple responding
//! tach signals" to be representable, so response is decided per channel against
//! that channel's own measured noise floor, and the cross-channel margin only
//! grades the *confidence* of the result.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::api::calibration::{
    check_thermal_safety, stale_temperature_refusal, thermal_force_state,
};
use crate::api::characterization::{RestoreOnDrop, RestoreReport, RunStep};
use crate::api::diagnostic_gates::{pump_protected_mid_run_detail, PumpWatch};
use crate::api::responses::HwmonVerifyState;
use crate::constants;
use crate::health::cache::StateCache;

/// The diagnostic this module IS, named once (DEC-336, `P8-p`).
///
/// The POST handler's entry guard and [`run_discovery`]'s per-cycle guard both
/// key their staleness refusal on this, so neither can end up gated on a
/// different diagnostic than the preflight the operator was shown. Consuming
/// `Diagnostic::blocks_on_stale_temperature` rather than restating the rule is
/// what keeps the published verdict and the enforced behaviour in step.
pub const DISCOVERY_DIAGNOSTIC: crate::api::preflight::Diagnostic =
    crate::api::preflight::Diagnostic::ControlPathDiscovery;

// ── Vocabulary ───────────────────────────────────────────────────────

pub const STATE_RUNNING: &str = "running";
pub const STATE_COMPLETE: &str = "complete";
pub const STATE_CANCELLED: &str = "cancelled";
pub const STATE_ABORTED: &str = "aborted";
pub const STATE_FAILED: &str = "failed";

/// Relationship outcomes required by §2.
pub const REL_CONFIRMED: &str = "confirmed";
pub const REL_PROBABLE: &str = "probable";
pub const REL_AMBIGUOUS: &str = "ambiguous";
pub const REL_NO_RESPONSE: &str = "no_tach_response";
pub const REL_MULTIPLE: &str = "multiple_responses";

/// Confidence vocabulary from §4.
pub const CONF_HIGH: &str = "high";
pub const CONF_MEDIUM: &str = "medium";
pub const CONF_LOW: &str = "low";
pub const CONF_UNKNOWN: &str = "unknown";

// ── Wire types ───────────────────────────────────────────────────────

/// Body of `POST /hwmon/{header_id}/discover-control-path`. All fields optional;
/// every one of them is clamped server-side.
#[derive(Debug, Default, Deserialize)]
pub struct DiscoveryRequest {
    pub delta_pct: Option<u8>,
    pub cycles: Option<u8>,
    pub window_seconds: Option<u64>,
}

/// A tach channel this run watches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TachChannel {
    /// Stable id — a header id for a header-attached tach, a
    /// `hwmon:chip:device:fanN:label` id for a monitor-only one.
    pub tach_id: String,
    pub label: String,
    /// True for a `fanN_input` with no matching `pwmN`. These are invisible to
    /// `/hwmon/headers` and are not on the 1 Hz poll — this diagnostic reads
    /// them directly for the duration of its own window, and nowhere else.
    pub monitor_only: bool,
    /// True for the header being perturbed. Exactly one channel carries this,
    /// and only when the target header has a tach of its own.
    pub is_target_header: bool,
}

/// One channel's behaviour across one perturbation cycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TachObservation {
    pub tach_id: String,
    /// Settled reading at the baseline duty.
    pub baseline_rpm: Option<u16>,
    /// Settled reading at the perturbed duty.
    pub perturbed_rpm: Option<u16>,
    /// `perturbed - baseline`. Signed: §2 asks for direction.
    pub delta_rpm: Option<i32>,
    /// Peak-to-peak spread measured on THIS channel during THIS cycle's baseline
    /// window, floored at [`constants::DISCOVERY_MIN_NOISE_FLOOR_RPM`]. Measured
    /// rather than assumed — a noisy channel earns a higher bar.
    pub noise_floor_rpm: u16,
    /// Did this channel move beyond both its own noise floor and the relative
    /// threshold?
    pub responded: bool,
    /// DEC-405 (`PTR-c`). True when this channel had not settled before this
    /// cycle's baseline window, so `noise_floor_rpm` is cycle 1's measurement
    /// for it rather than this window's — a recovery ramp is not noise.
    #[serde(default)]
    pub noise_floor_from_cycle_1: bool,
}

/// One perturbation cycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveryCycle {
    /// 1-based.
    pub cycle: u8,
    pub baseline_pct: u8,
    pub perturbed_pct: u8,
    /// `up` | `down` — which way [`perturbation_target`] went.
    pub direction: String,
    pub observations: Vec<TachObservation>,
    /// DEC-405 (`PTR-c`). Whether every channel that could move had settled
    /// before the baseline window opened. `None` when no wait ran, because the
    /// baseline write did not move the duty (cycle 1 at the header's own duty).
    #[serde(default)]
    pub baseline_settled: Option<bool>,
    /// DEC-405. How long this cycle waited for that settle, bounded by
    /// [`constants::DISCOVERY_SETTLE_WAIT_MAX`]. `0` when no wait ran.
    #[serde(default)]
    pub settle_wait_ms: u64,
}

/// A candidate PWM → tach relationship.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlPathCandidate {
    pub tach_id: String,
    pub label: String,
    pub monitor_only: bool,
    /// `high` | `medium` | `low`.
    pub confidence: String,
    /// `positive` | `negative` — does RPM follow duty, or oppose it?
    pub direction: String,
    pub baseline_rpm: Option<u16>,
    pub perturbed_rpm: Option<u16>,
    /// Change as a percentage of baseline, when a baseline was readable.
    pub change_pct: Option<f64>,
    /// How many cycles this channel responded in, out of how many ran.
    pub cycles_responded: u8,
    pub cycles_total: u8,
}

/// Derived result over the whole run. Produced by [`summarise`], which is pure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoverySummary {
    /// `confirmed` | `probable` | `ambiguous` | `no_tach_response` |
    /// `multiple_responses`.
    pub relationship: String,
    /// `high` | `medium` | `low` | `unknown`.
    pub confidence: String,
    pub candidates: Vec<ControlPathCandidate>,
    /// Effective telemetry update cadence, when it could be established: the
    /// driver's own `update_interval` if it publishes one, else the **median**
    /// interval between changes this run observed (DEC-405) — on the header's
    /// own tach where it has one, else the fastest channel.
    /// `None` means UNKNOWN, which §4 requires in preference to a guess.
    pub measurement_resolution_ms: Option<u64>,
    /// How this run sub-sampled.
    pub sample_interval_ms: u64,
    pub sample_count: u32,
    /// Why the confidence landed where it did. Stable-ish prose; the client
    /// renders it verbatim.
    pub confidence_notes: Vec<String>,
}

/// A discovery run, and the body of `GET /diagnostics/control-path`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlPathRun {
    pub run_id: String,
    pub header_id: String,
    /// `running` | `complete` | `cancelled` | `aborted` | `failed`.
    pub state: String,
    /// The clamped perturbation size actually used.
    pub delta_pct: u8,
    /// The clamped cycle count this run will walk. `cycles.len()` against this
    /// is the client's progress indicator.
    pub requested_cycles: u8,
    pub window_seconds: u64,
    /// The duty the run perturbs around.
    pub baseline_pct: u8,
    /// The duty it perturbs to.
    pub perturbed_pct: u8,
    pub direction: String,
    /// Every channel watched, in a stable order.
    pub channels: Vec<TachChannel>,
    pub cycles: Vec<DiscoveryCycle>,
    /// `None` while running.
    pub summary: Option<DiscoverySummary>,
    /// The duty the header held before the run. `None` means it could not be
    /// read, in which case there is nothing to put back.
    pub original_pct: Option<u8>,
    /// **The header was NOT put back.** Derived from
    /// [`RestoreOutcome::header_left_moved`](crate::api::characterization::RestoreOutcome),
    /// so it cannot drift from `restore_outcome`.
    pub restore_failed: bool,
    /// `pending` | `restored` | `write_failed` | `skipped_shutting_down` |
    /// `skipped_thermal_force` | `no_original_duty`.
    pub restore_outcome: String,
    /// Why the run ended, when it did not simply complete.
    pub detail: Option<String>,
    /// Wall-clock completion stamp, for §6.3's "Last validated" row.
    pub completed_unix_ms: Option<u64>,
    /// `P8-bg` (daemon >= 2.55.0): the phase being held right now —
    /// `settle_wait`, `baseline` or `perturbed` — which cycle, at what duty,
    /// and for how long at most. A cycle is published only after both of its
    /// windows, so before this a client saw one intermediate update in a whole
    /// default run. `None` before the first write and once terminal.
    #[serde(default)]
    pub current_step: Option<RunStep>,
}

pub const STEP_PHASE_SETTLE_WAIT: &str = "settle_wait";
pub const STEP_PHASE_BASELINE: &str = "baseline";
pub const STEP_PHASE_PERTURBED: &str = "perturbed";

impl ControlPathRun {
    pub fn is_running(&self) -> bool {
        self.state == STATE_RUNNING
    }
}

// ── Input resolution (pure) ──────────────────────────────────────────

/// Clamp a caller-supplied perturbation size.
pub fn resolve_delta(requested: Option<u8>) -> u8 {
    requested.unwrap_or(constants::DISCOVERY_DELTA_PCT).clamp(
        constants::DISCOVERY_DELTA_MIN_PCT,
        constants::DISCOVERY_DELTA_MAX_PCT,
    )
}

/// Clamp a caller-supplied cycle count. The floor is 2, not 1: repeatability is
/// a confidence input, and a one-cycle run could not produce `confirmed` while
/// still claiming to have tested for it.
pub fn resolve_cycles(requested: Option<u8>) -> u8 {
    requested
        .unwrap_or(constants::DISCOVERY_DEFAULT_CYCLES)
        .clamp(
            constants::DISCOVERY_DEFAULT_CYCLES,
            constants::DISCOVERY_MAX_CYCLES,
        )
}

/// [SAFETY] Choose the duty to perturb to, and which way.
///
/// The direction is chosen **away from the nearer rail**: whichever of the floor
/// and 100 % the baseline sits closer to, the swing goes the other way. That
/// guarantees headroom, so the clamp below can never collapse the swing to
/// nothing, and it means a pump idling near its floor is perturbed *upward* —
/// never walked toward a stall.
///
/// The returned duty is always inside `[max(DISCOVERY_MIN_PCT, floor) .. 100]`.
/// **No input can produce 0 %**, whatever the caller sends and whatever the
/// header's role resolves to — the same flat rule, for the same reason, as
/// `characterization::resolve_points`.
pub fn perturbation_target(baseline_pct: u8, delta: u8, floor: u8) -> (u8, &'static str) {
    let lo = floor.max(constants::DISCOVERY_MIN_PCT);
    let hi = 100u8;
    // A baseline below the floor is possible (a header sitting where firmware
    // left it) and must not drag the perturbation down with it.
    let base = baseline_pct.clamp(lo, hi);
    let room_up = hi.saturating_sub(base);
    let room_down = base.saturating_sub(lo);
    if room_up >= room_down {
        (base.saturating_add(delta).min(hi), "up")
    } else {
        (base.saturating_sub(delta).max(lo), "down")
    }
}

/// [SAFETY] The duty the run perturbs *around*.
///
/// Clamped into the same safe range as the perturbation itself, so the
/// between-cycle return write cannot put a pump below its floor either. When the
/// pre-run duty is unreadable this falls back to
/// [`constants::IDENTIFY_PUMP_BASELINE_FALLBACK_PCT`] — the same fallback, for
/// the same reason, that pump-safe identify uses (DEC-311): a mid-range duty is
/// the one guess that is safe in both directions.
pub fn resolve_baseline(readback_pct: Option<u8>, floor: u8) -> u8 {
    let lo = floor.max(constants::DISCOVERY_MIN_PCT);
    readback_pct
        .unwrap_or(constants::IDENTIFY_PUMP_BASELINE_FALLBACK_PCT)
        .clamp(lo, 100)
}

// ── Abort predicates (pure) ──────────────────────────────────────────

/// [SAFETY] Has a pump's tach vanished mid-run?
///
/// §1 lists "pump tach unexpectedly disappears during a test" as an abort
/// trigger, and it is the only signal available that separates "we are
/// perturbing a healthy pump" from "the pump has stopped and we are still
/// writing to it". Deliberately conditional on the tach having been readable at
/// the START of the run: a pump with no tach at all is a normal configuration
/// and must not abort every run on that board.
///
/// Not gated on a *low* RPM, only on an absent one. A perturbation stays inside
/// the safe range by construction, so a pump that is merely slower is expected;
/// a pump whose tach stops reporting is not.
pub fn pump_tach_lost(pump_protected: bool, had_tach_at_start: bool, current: Option<u16>) -> bool {
    pump_protected && had_tach_at_start && current.is_none()
}

// ── Measurement derivation (pure) ────────────────────────────────────

/// Peak-to-peak spread of a channel's samples, floored at
/// [`constants::DISCOVERY_MIN_NOISE_FLOOR_RPM`].
///
/// Measured per channel rather than assumed globally, because a 400 RPM pump and
/// a 2000 RPM radiator fan do not have the same jitter — and the whole point of
/// §2's "tach noise floor" confidence input is that a noisy channel must clear a
/// higher bar before it counts as having responded.
pub fn noise_floor(samples: &[Option<u16>]) -> u16 {
    let readable: Vec<u16> = samples.iter().filter_map(|s| *s).collect();
    let spread = match (readable.iter().min(), readable.iter().max()) {
        (Some(lo), Some(hi)) => hi.saturating_sub(*lo),
        _ => 0,
    };
    spread.max(constants::DISCOVERY_MIN_NOISE_FLOOR_RPM)
}

/// Did this channel move enough to count as a response?
///
/// Two tests, both of which must pass: the change must clear the channel's own
/// **measured** noise floor, and it must be at least
/// [`constants::DISCOVERY_RESPONSE_MIN_PCT`] of the channel's baseline. The
/// relative test is what stops a 60 RPM wobble on a 2000 RPM fan reading as a
/// response; the absolute one is what stops a 55 RPM change on a 300 RPM pump
/// being dismissed as noise.
pub fn responded(baseline: Option<u16>, perturbed: Option<u16>, noise: u16) -> bool {
    let (Some(b), Some(p)) = (baseline, perturbed) else {
        return false;
    };
    let delta = b.abs_diff(p);
    let relative = (u32::from(b) * u32::from(constants::DISCOVERY_RESPONSE_MIN_PCT) / 100) as u16;
    delta >= noise && delta >= relative
}

/// The tach refresh interval one channel's samples show: the median gap between
/// changes of value. The rule lives in [`crate::api::stats::update_interval_ms`]
/// so characterisation and discovery cannot disagree about it (DEC-405); this
/// name is kept for the callers and tests that already use it.
///
/// `None` when fewer than two changes were seen — reported as UNKNOWN rather
/// than guessed.
pub fn measurement_resolution_ms(samples: &[(u64, Option<u16>)]) -> Option<u64> {
    crate::api::stats::update_interval_ms(samples)
}

/// Derive the whole result from the measured cycles. Pure — the handler must
/// call this rather than deriving any verdict inline.
pub fn summarise(
    channels: &[TachChannel],
    cycles: &[DiscoveryCycle],
    driver_update_interval_ms: Option<u64>,
    observed_resolution_ms: Option<u64>,
    sample_count: u32,
) -> DiscoverySummary {
    let cycles_total = cycles.len() as u8;
    let mut candidates: Vec<ControlPathCandidate> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // Was ANY tach readable at all? Distinguishes "nothing responded" from
    // "nothing could be measured", which §5 forbids collapsing together.
    let any_readable = cycles
        .iter()
        .flat_map(|c| c.observations.iter())
        .any(|o| o.baseline_rpm.is_some() || o.perturbed_rpm.is_some());

    for ch in channels {
        let obs: Vec<&TachObservation> = cycles
            .iter()
            .filter_map(|c| c.observations.iter().find(|o| o.tach_id == ch.tach_id))
            .collect();
        let responded_count = obs.iter().filter(|o| o.responded).count() as u8;
        if responded_count == 0 {
            continue;
        }
        // Representative figures come from the first cycle that responded, so
        // the reported before/after pair is one really-measured pair rather than
        // an average across cycles that never happened together.
        let first = obs
            .iter()
            .find(|o| o.responded)
            .expect("responded_count > 0");
        let change_pct = match (first.baseline_rpm, first.delta_rpm) {
            (Some(b), Some(d)) if b > 0 => Some((f64::from(d) / f64::from(b)) * 100.0),
            _ => None,
        };
        // Direction is measured, not assumed: a duty rise that LOWERS a reading
        // is real (a mis-wired tach, or a channel reporting a period rather than
        // a rate) and §2 asks for direction rather than a fixed expectation.
        let direction = match first.delta_rpm {
            Some(d) if d < 0 => "negative",
            _ => "positive",
        };
        candidates.push(ControlPathCandidate {
            tach_id: ch.tach_id.clone(),
            label: ch.label.clone(),
            monitor_only: ch.monitor_only,
            // Filled in below, once the cross-channel margin is known.
            confidence: CONF_LOW.to_string(),
            direction: direction.to_string(),
            baseline_rpm: first.baseline_rpm,
            perturbed_rpm: first.perturbed_rpm,
            change_pct,
            cycles_responded: responded_count,
            cycles_total,
        });
    }

    // Cross-channel margin: the biggest change seen on any channel that did NOT
    // respond. This is §2's "non-target tach stability" input, and it grades
    // confidence — it never decides response, or a genuine two-tach header would
    // report as no-response (see the module docs).
    let quietest_margin: u32 = cycles
        .iter()
        .flat_map(|c| c.observations.iter())
        .filter(|o| !o.responded)
        .filter_map(|o| o.delta_rpm.map(|d| d.unsigned_abs()))
        .max()
        .unwrap_or(0);

    for cand in &mut candidates {
        let consistent = cand.cycles_responded == cycles_total && cycles_total > 0;
        let own_delta: u32 = cand
            .baseline_rpm
            .zip(cand.perturbed_rpm)
            .map(|(b, p)| u32::from(b.abs_diff(p)))
            .unwrap_or(0);
        let clear_of_others = quietest_margin == 0
            || own_delta >= quietest_margin * u32::from(constants::DISCOVERY_TARGET_OVER_NOISE);
        cand.confidence = match (consistent, clear_of_others) {
            (true, true) => CONF_HIGH,
            (true, false) => CONF_MEDIUM,
            (false, _) => CONF_LOW,
        }
        .to_string();
    }

    // Strongest first, so the client's first row is the best candidate.
    candidates.sort_by(|a, b| {
        rank(&b.confidence)
            .cmp(&rank(&a.confidence))
            .then(b.cycles_responded.cmp(&a.cycles_responded))
    });

    let relationship = if !any_readable {
        notes.push(
            "No tach channel produced a readable RPM during this run, so no relationship \
             could be tested."
                .into(),
        );
        REL_NO_RESPONSE
    } else if candidates.is_empty() {
        notes.push(
            "Every tach channel stayed within its own noise floor. This header may drive \
             no tach-reporting device, or its device may be running under its own internal \
             control."
                .into(),
        );
        REL_NO_RESPONSE
    } else if candidates.len() > 1 {
        notes.push(format!(
            "{} tach channels responded together. That is expected for a splitter or a \
             shared header, and means the mapping is not one-to-one.",
            candidates.len()
        ));
        REL_MULTIPLE
    } else {
        let only = &candidates[0];
        if only.confidence == CONF_HIGH {
            REL_CONFIRMED
        } else if only.confidence == CONF_MEDIUM {
            notes.push(
                "One channel responded in every cycle, but another channel moved by a \
                 comparable amount, so the mapping is probable rather than confirmed."
                    .into(),
            );
            REL_PROBABLE
        } else {
            notes.push(format!(
                "The responding channel answered in only {} of {} cycles, so the result is \
                 not repeatable enough to rely on.",
                only.cycles_responded, only.cycles_total
            ));
            REL_AMBIGUOUS
        }
    };

    // Overall confidence: the best candidate's, or UNKNOWN when nothing was
    // measurable. A clean no-response on readable tachs is a real, LOW-confidence
    // observation — not an unknown, and explicitly not a pass (§5).
    let confidence = if !any_readable {
        CONF_UNKNOWN
    } else if let Some(best) = candidates.first() {
        match relationship {
            REL_MULTIPLE if best.confidence == CONF_HIGH => CONF_MEDIUM,
            _ => best.confidence.as_str(),
        }
    } else {
        CONF_LOW
    };

    // DEC-405 (`PTR-c`): say when a baseline could not settle, and which noise
    // floors it therefore borrowed, so a verdict never silently rests on a
    // window that was still recovering.
    let unsettled: Vec<String> = cycles
        .iter()
        .filter(|c| c.baseline_settled == Some(false))
        .map(|c| c.cycle.to_string())
        .collect();
    if !unsettled.is_empty() {
        let borrowed = cycles
            .iter()
            .flat_map(|c| c.observations.iter())
            .filter(|o| o.noise_floor_from_cycle_1)
            .count();
        notes.push(format!(
            "The tachs had not all settled within {} s before the baseline of cycle(s) {}. \
             {borrowed} channel reading(s) used cycle 1's noise floor instead of one \
             measured while the fan was still recovering.",
            constants::DISCOVERY_SETTLE_WAIT_MAX.as_secs(),
            unsettled.join(", ")
        ));
    }

    // §4: prefer the driver's own declared cadence; fall back to what this run
    // observed; report UNKNOWN rather than guessing.
    let measurement_resolution_ms = driver_update_interval_ms.or(observed_resolution_ms);
    if measurement_resolution_ms.is_none() {
        notes.push(
            "Telemetry update cadence is unknown: this driver publishes no update_interval \
             and no reading changed during the run."
                .into(),
        );
    } else if let Some(res) = measurement_resolution_ms {
        if res >= constants::DISCOVERY_SAMPLE_INTERVAL.as_millis() as u64 * 2 {
            notes.push(format!(
                "Telemetry updates roughly every {res} ms, so timings finer than that are \
                 not meaningful."
            ));
        }
    }

    DiscoverySummary {
        relationship: relationship.to_string(),
        confidence: confidence.to_string(),
        candidates,
        measurement_resolution_ms,
        sample_interval_ms: constants::DISCOVERY_SAMPLE_INTERVAL.as_millis() as u64,
        sample_count,
        confidence_notes: notes,
    }
}

fn rank(confidence: &str) -> u8 {
    match confidence {
        CONF_HIGH => 3,
        CONF_MEDIUM => 2,
        CONF_LOW => 1,
        _ => 0,
    }
}

// ── The sweep ────────────────────────────────────────────────────────

/// One sub-sample: the target header's state plus every watched tach, read as
/// close together as sysfs allows.
#[derive(Debug, Clone)]
pub struct DiscoverySample {
    pub header: HwmonVerifyState,
    /// Parallel to the `channels` slice handed to [`run_discovery`].
    pub tachs: Vec<Option<u16>>,
}

/// How a discovery run ended.
pub struct DiscoveryOutcome {
    pub state: &'static str,
    pub detail: Option<String>,
    pub cycles: Vec<DiscoveryCycle>,
    pub sample_count: u32,
    pub observed_resolution_ms: Option<u64>,
}

/// Walk `cycles` perturbation cycles on one header, watching every channel.
///
/// Generic over the write/read closures so every abort path and the restore are
/// testable without sysfs — the same shape, and for the same reason, as
/// `characterization::run_sweep`.
///
/// Aborts, all of which restore: `cancel` set (→ `cancelled`), a failed write
/// (→ `failed`), `pwm_enable != 1` (→ `aborted`, reclaim), a sensor over the
/// diagnostic limit or the ladder forcing (→ `aborted`), the daemon shutting
/// down (→ `aborted`), and **a pump-protected header whose tach disappears**
/// (→ `aborted`).
#[allow(clippy::too_many_arguments)]
pub async fn run_discovery<W, R, Fut, P, A, S, K>(
    cache: &StateCache,
    header_id: &str,
    channels: &[TachChannel],
    baseline_pct: u8,
    perturbed_pct: u8,
    direction: &str,
    cycle_count: u8,
    // [SAFETY] The lowest duty the RESTORE may write — `HARD_PUMP_CPU_FLOOR_PCT`
    // for a pump-protected header, 0 for everything else. Separate from the
    // sweep floor already baked into `baseline_pct`/`perturbed_pct`, for exactly
    // the reason `AUD3-l` records on the characterisation path: putting an
    // ordinary fan back at its own captured 0 is a restore, not a safety event.
    restore_floor: u8,
    pump_protected: bool,
    // [SAFETY] `TS-aw` (DEC-418): the pump union, re-read before every write and
    // on every sample. A header that becomes pump-protected mid-run stops the
    // run (`aborted`), and the restore guard re-reads it before it writes.
    // `pump_protected` above stays the entry answer: it is what `baseline_pct`
    // and `perturbed_pct` were planned from.
    pump_watch: &PumpWatch<'_>,
    window: Duration,
    // DEC-405: the bound on each settle-wait. Production passes
    // `constants::DISCOVERY_SETTLE_WAIT_MAX`; a parameter, like `window`, so a
    // harness's own clock decides how long "bounded" is.
    settle_wait_max: Duration,
    write_fn: W,
    // One observation, resolved as a **future** so the caller can put the
    // blocking `std::fs` reads it performs on the blocking pool (`P8-am`).
    // A test that has nothing to block on returns `std::future::ready`.
    read_fn: R,
    cancel: &AtomicBool,
    shutting_down: S,
    keepalive: K,
    report: &RestoreReport,
    mut publish: P,
    // `P8-bg`: told the phase at every window boundary. `publish` fires once
    // per cycle, after both windows, which is the silence this fills.
    mut announce: A,
) -> DiscoveryOutcome
where
    W: Fn(u8) -> Result<(), String>,
    R: Fn() -> Fut,
    Fut: std::future::Future<Output = DiscoverySample>,
    P: FnMut(DiscoveryCycle),
    A: FnMut(RunStep),
    S: Fn() -> bool,
    K: Fn() -> bool,
{
    let first = read_fn().await;
    let original_pct = first.header.pwm_percent;
    // Which tach belongs to the header being perturbed, if any. Captured before
    // anything is written so `pump_tach_lost` compares against the pre-run truth.
    let target_idx = channels.iter().position(|c| c.is_target_header);
    let had_target_tach = target_idx
        .and_then(|i| first.tachs.get(i).copied().flatten())
        .is_some();

    let mut measured: Vec<DiscoveryCycle> = Vec::with_capacity(cycle_count as usize);
    // DEC-405: each channel's reading just before the current window, the
    // reference a settle is judged against — a first sample still equal to it is
    // a register that has not refreshed, never a settle.
    let mut previous_tachs: Vec<Option<u16>> = first.tachs.clone();
    // Cycle 1's per-channel noise floor, and whether it was measured on a
    // settled baseline — the fallback when a later baseline cannot settle.
    let mut cycle_1_noise: Vec<Option<u16>> = vec![None; channels.len()];
    let mut sample_count: u32 = 0;
    let mut resolution_samples: Vec<Vec<(u64, Option<u16>)>> = vec![Vec::new(); channels.len()];
    let wrote_any = AtomicBool::new(false);
    let run_started = tokio::time::Instant::now();

    // [SAFETY] Bound BEFORE `_restore`, deliberately. `_restore` must remain the
    // last binding in this scope so it drops first (see its own comment below),
    // and while this closure is `Drop`-free — it captures one shared reference —
    // relying on that would leave the guard's stated invariant false at its own
    // site and would silently mis-order the next binding someone adds here.
    // Keep new bindings above `_restore`.
    let thermal_gate = || -> Option<String> {
        if let Err(e) = check_thermal_safety(cache) {
            return Some(e.to_string());
        }
        if let Some(state) = thermal_force_state(cache) {
            return Some(format!(
                "thermal safety is forcing fan output ({state}); control-path \
                 discovery cannot write"
            ));
        }
        stale_temperature_refusal(cache, DISCOVERY_DIAGNOSTIC)
    };

    // Declared LAST so it drops FIRST — while the caller's lease guard is still
    // held. Reversed, the restore write fails `InvalidLease` and the header is
    // parked at the last perturbed duty. Same invariant, same reason, as
    // `characterization::run_sweep`; see `RestoreOnDrop`'s docs before touching
    // this ordering.
    let _restore = RestoreOnDrop {
        header_id,
        original_pct,
        write_fn: &write_fn,
        cache,
        shutting_down: &shutting_down,
        wrote_any: &wrote_any,
        report,
        restore_floor,
        pump_watch: Some(pump_watch),
    };

    macro_rules! bail {
        ($state:expr, $detail:expr) => {
            return DiscoveryOutcome {
                state: $state,
                detail: Some($detail),
                cycles: measured,
                sample_count,
                observed_resolution_ms: fold_resolution(&resolution_samples, target_idx),
            }
        };
    }
    // Why a window ended early, as an abort: both arms are `aborted`.
    macro_rules! bail_window {
        ($stop:expr) => {
            bail!(STATE_ABORTED, $stop.detail())
        };
    }

    // [SAFETY] DEC-339 (`P8-u`): the three thermal gates, defined ONCE and
    // evaluated before EVERY duty-lowering write this function issues.
    //
    // The rule this closure exists to make unbreakable: **a write is never
    // issued on a cache that has not been re-read since the previous write.**
    // Before DEC-339 the three predicates were spelled out inline at the top of
    // the cycle and nowhere else, so they ran once per *cycle* while a cycle
    // issues *two* writes and holds two observation windows. The perturbed
    // write — the one that can command a header DOWN by up to
    // `DISCOVERY_DELTA_MAX_PCT` points — was therefore issued on a reading up
    // to one window old (15 s at the documented maximum), and the resulting
    // duty was then held for a second window before anything looked again. The
    // daemon could actively reduce cooling on a machine it already knew was
    // over `CALIBRATION_MAX_TEMP_C`, and take ~30 s to notice.
    //
    // Order matters and is the pre-existing one: the two cheap `value_c`
    // comparisons first, then the freshness refusal, which is the only one that
    // can see a poll loop that has STOPPED (DEC-336, `P8-p`) — the other two
    // keep passing on last-known-good readings against a frozen cache.
    //
    // Returns the abort detail, because all three produce `STATE_ABORTED`;
    // collapsing them to one shape is deliberate, per DEC-336's finding that
    // two gating shapes for one safety rule is how a site ends up checking a
    // subset. A fourth write site added later gets all three or none.
    //
    // The closure itself is defined above, before `_restore`, so the guard stays
    // the last binding in scope.

    for cycle in 1..=cycle_count {
        // [SAFETY] The same four gates the characterisation sweep applies at the
        // top of every point, for the same reasons. The shutdown check is not
        // covered by the drop guard's own skip: this task is detached, so it
        // keeps running through `shutdown_sequence` and could otherwise land a
        // write after `hand_back_hwmon` handed the header back to firmware.
        if shutting_down() {
            bail!(STATE_ABORTED, "the daemon is shutting down".into());
        }
        if cancel.load(Ordering::SeqCst) {
            bail!(
                STATE_CANCELLED,
                format!("cancelled after {} of {cycle_count} cycles", cycle - 1)
            );
        }
        // Gate 1 of 2 per cycle — guards the BASELINE write below.
        if let Some(reason) = thermal_gate() {
            bail!(STATE_ABORTED, reason);
        }
        // ── Baseline window ──
        // Written explicitly rather than assumed: cycle 2 arrives here straight
        // from cycle 1's perturbed duty, and an unwritten baseline would compare
        // a perturbed reading against another perturbed reading.
        //
        // [SAFETY] DEC-296: liveness is proved before **every observation
        // window**, not once per cycle. A cycle holds TWO windows, so renewing
        // per cycle makes the renewal interval `2 × window` — which at the
        // documented maximum (15 s) equals `VERIFY_PAUSE_DEADMAN` (30 s) before
        // any I/O overhead, i.e. the pause expires before it is re-armed. The
        // engine's write phase would then resume mid-run, and `try_begin_verify`
        // would enter its steal branch, letting a second diagnostic force-take
        // this run's lease so that even the restore write fails `InvalidLease`
        // and the header is parked at the perturbed duty. That is precisely the
        // defect DEC-296 recorded, and it is why the compile-time assertion in
        // `constants.rs` describes a ONE-window interval: this is the code that
        // has to make that true.
        if !keepalive() {
            bail!(
                STATE_ABORTED,
                "superseded by a later diagnostic; this run's lease is gone".into()
            );
        }
        // [SAFETY] Re-check immediately before the write. `observe` checks at the
        // top of each sample iteration, but returns after one more read — up to
        // `DISCOVERY_MAX_TACH_CHANNELS` blocking sysfs reads later — so shutdown
        // can land in that gap and a write issued after `hand_back_hwmon`
        // would re-assert `pwm_enable=1` at a fixed duty with no writer left
        // (the DEC-290 / 277-c hazard the drop guard's own skip exists for).
        if shutting_down() {
            bail!(STATE_ABORTED, "the daemon is shutting down".into());
        }
        // [SAFETY] `TS-aw`: after the shutdown check, so it is never read while
        // the exit path holds the controller, and immediately before the write.
        if pump_watch.became_protected() {
            bail_window!(WindowStop::PumpProtected);
        }
        wrote_any.store(true, Ordering::SeqCst);
        pump_watch.note_write(baseline_pct);
        if let Err(e) = write_fn(baseline_pct) {
            bail!(
                STATE_FAILED,
                format!("PWM write of {baseline_pct}% failed: {e}")
            );
        }
        // ── Settle-wait (DEC-405, `PTR-c`) ──
        // A baseline window that opens as the previous perturbation is reversed
        // measures the recovery ramp, and `noise_floor` then calls that ramp
        // noise: on 2026-09-08 a +796 rpm pump response was graded `ambiguous`
        // against an 804 rpm cycle-2 floor. So wait, bounded, for every channel
        // that can move to settle — before any baseline whose write moved the
        // duty, which is every later cycle and cycle 1 only when the header was
        // below the discovery floor.
        let needs_wait = cycle > 1 || original_pct != Some(baseline_pct);
        let (baseline_settled, settle_wait_ms, channel_settled) = if needs_wait {
            announce(RunStep::now(
                STEP_PHASE_SETTLE_WAIT,
                u16::from(cycle),
                baseline_pct,
                settle_wait_max,
            ));
            let waited = match settle_wait(
                &read_fn,
                settle_wait_max,
                &shutting_down,
                pump_watch,
                run_started,
                &mut resolution_samples,
                &mut sample_count,
                &previous_tachs,
            )
            .await
            {
                Ok(w) => w,
                Err(stop) => bail_window!(stop),
            };
            // [SAFETY] The wait held the baseline duty for up to a window, so its
            // last reading gets the same reclaim / lost-pump-tach check every
            // observation window's does. Without it a pump whose tach vanished
            // during the wait would be noticed only after the baseline window
            // too — two windows at the baseline duty instead of one.
            if let Some(reason) = reclaim_or_lost_pump(
                &waited.last,
                baseline_pct,
                pump_protected,
                had_target_tach,
                target_idx.and_then(|i| waited.last.tachs.get(i).copied().flatten()),
            ) {
                bail!(STATE_ABORTED, reason);
            }
            // DEC-405 (F2): a cancel is honoured at every window boundary, so it
            // lands when the window being held ends — the wait included, which
            // would otherwise add up to `DISCOVERY_SETTLE_WAIT_MAX` to it.
            if cancel.load(Ordering::SeqCst) {
                bail!(
                    STATE_CANCELLED,
                    format!("cancelled after {} of {cycle_count} cycles", cycle - 1)
                );
            }
            // [SAFETY] The wait is an observation window in its own right, so
            // the one-window renewal cadence (DEC-296) and the thermal gates
            // beside it (DEC-339) are applied again before the baseline window.
            // Without this the baseline window would run up to
            // `DISCOVERY_SETTLE_WAIT_MAX + window` (30 s) past the last renewal —
            // exactly the deadman — and a thermal condition that arose during
            // the wait would go unexamined for two windows. Same order as every
            // other gate site in this function: thermal, then keepalive.
            if let Some(reason) = thermal_gate() {
                bail!(STATE_ABORTED, reason);
            }
            if !keepalive() {
                bail!(
                    STATE_ABORTED,
                    "superseded by a later diagnostic; this run's lease is gone".into()
                );
            }
            let all = waited.settled.iter().all(|s| *s);
            (Some(all), waited.elapsed_ms, waited.settled)
        } else {
            (None, 0, vec![true; channels.len()])
        };
        announce(RunStep::now(
            STEP_PHASE_BASELINE,
            u16::from(cycle),
            baseline_pct,
            window,
        ));
        let base = match observe(
            &read_fn,
            window,
            &shutting_down,
            pump_watch,
            run_started,
            &mut resolution_samples,
            &mut sample_count,
        )
        .await
        {
            Ok(s) => s,
            Err(stop) => bail_window!(stop),
        };
        if let Some(reason) = reclaim_or_lost_pump(
            &base.last,
            baseline_pct,
            pump_protected,
            had_target_tach,
            target_idx.and_then(|i| base.last.tachs.get(i).copied().flatten()),
        ) {
            bail!(STATE_ABORTED, reason);
        }

        // ── Perturbed window ──
        // [SAFETY] Gate 2 of 2 per cycle (DEC-339, `P8-u`) — and the site the
        // register row was actually about. The baseline window has just elapsed,
        // so the reading the cycle-top gate passed on is up to `window` old; the
        // write below is the one that can lower the duty. Checking here makes the
        // thermal cadence exactly equal to the keepalive cadence — one evaluation
        // per observation window — which is the invariant the DEC-296 note below
        // already establishes for liveness. Read the two together: this function
        // proves liveness AND thermal safety before every window, never once per
        // cycle for two windows.
        //
        // **Ordered ABOVE `keepalive()`, matching the cycle top and
        // `characterization.rs`'s in-hold block — do not swap them.** A ladder
        // engagement force-takes the hwmon lease (`backends.rs`
        // `force_take_lease(ThermalSafety)`), so `keepalive()` fails on the very
        // same condition. Checked in the other order, a thermal trip reports
        // `"superseded by a later diagnostic; this run's lease is gone"` — false,
        // and it points the operator at a competing diagnostic instead of at the
        // heat. The hardware outcome is identical either way; the abort *detail*
        // is not, and it is what the UI shows. The characterisation sweep states
        // this reasoning at its own renewal site.
        //
        // It stays ABOVE the shutdown check too: `shutting_down()` is
        // load-bearing *immediately* before the write, because `observe` can
        // return one read after a shutdown began and a write landing after
        // `hand_back_hwmon` would re-assert `pwm_enable=1` on a header the
        // firmware has been handed back (the DEC-290 / 277-c hazard).
        //
        // DEC-405 (F2): a cancel pressed during the baseline window lands here,
        // when that window ends, rather than a whole perturbed window later.
        if cancel.load(Ordering::SeqCst) {
            bail!(
                STATE_CANCELLED,
                format!("cancelled after {} of {cycle_count} cycles", cycle - 1)
            );
        }
        if let Some(reason) = thermal_gate() {
            bail!(STATE_ABORTED, reason);
        }
        // Second renewal of the cycle — see the note above the first.
        if !keepalive() {
            bail!(
                STATE_ABORTED,
                "superseded by a later diagnostic; this run's lease is gone".into()
            );
        }
        if shutting_down() {
            bail!(STATE_ABORTED, "the daemon is shutting down".into());
        }
        // [SAFETY] `TS-aw`: the write that can take the header DOWN by up to
        // `DISCOVERY_DELTA_MAX_PCT`, on a duty planned for an ordinary fan.
        if pump_watch.became_protected() {
            bail_window!(WindowStop::PumpProtected);
        }
        pump_watch.note_write(perturbed_pct);
        if let Err(e) = write_fn(perturbed_pct) {
            bail!(
                STATE_FAILED,
                format!("PWM write of {perturbed_pct}% failed: {e}")
            );
        }
        announce(RunStep::now(
            STEP_PHASE_PERTURBED,
            u16::from(cycle),
            perturbed_pct,
            window,
        ));
        let pert = match observe(
            &read_fn,
            window,
            &shutting_down,
            pump_watch,
            run_started,
            &mut resolution_samples,
            &mut sample_count,
        )
        .await
        {
            Ok(s) => s,
            Err(stop) => bail_window!(stop),
        };
        if let Some(reason) = reclaim_or_lost_pump(
            &pert.last,
            perturbed_pct,
            pump_protected,
            had_target_tach,
            target_idx.and_then(|i| pert.last.tachs.get(i).copied().flatten()),
        ) {
            bail!(STATE_ABORTED, reason);
        }

        let observations: Vec<TachObservation> = channels
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                let baseline_rpm = base.last.tachs.get(i).copied().flatten();
                let perturbed_rpm = pert.last.tachs.get(i).copied().flatten();
                let own_noise = noise_floor(&base.per_channel[i]);
                // DEC-405: a channel that settled measured its noise on a steady
                // baseline. One that did not falls back to cycle 1's floor for it
                // when there is one; cycle 1 itself has nothing earlier to use,
                // and its `baseline_settled` says so.
                let fallback = if channel_settled[i] {
                    None
                } else {
                    cycle_1_noise[i]
                };
                let noise = fallback.unwrap_or(own_noise);
                if cycle == 1 {
                    cycle_1_noise[i] = channel_settled[i].then_some(own_noise);
                }
                TachObservation {
                    tach_id: ch.tach_id.clone(),
                    baseline_rpm,
                    perturbed_rpm,
                    delta_rpm: baseline_rpm
                        .zip(perturbed_rpm)
                        .map(|(b, p)| i32::from(p) - i32::from(b)),
                    noise_floor_rpm: noise,
                    responded: responded(baseline_rpm, perturbed_rpm, noise),
                    noise_floor_from_cycle_1: fallback.is_some(),
                }
            })
            .collect();

        let done = DiscoveryCycle {
            cycle,
            baseline_pct,
            perturbed_pct,
            direction: direction.to_string(),
            observations,
            baseline_settled,
            settle_wait_ms,
        };
        previous_tachs = pert.last.tachs.clone();
        measured.push(done.clone());
        publish(done);
    }

    // [SAFETY] DEC-339, second review round: the gate has to reach the VERDICT,
    // not just the writes.
    //
    // Every write above is now preceded by a fresh evaluation — but the last
    // observation window has no write after it, so without this nothing
    // re-examines the conditions under which that window's DATA was collected.
    // That matters here in a way it does not on the characterisation path,
    // because this run's output is persisted as a durable claim about the
    // hardware: `handlers::discovery` writes a `ControlPathRecord` if and only
    // if `state == STATE_COMPLETE`, on the stated reasoning that "a cancelled or
    // aborted run measured a partial window, and recording it as 'last
    // validated' would be the §5 error of turning absent evidence into a
    // result".
    //
    // A ladder engagement during the final window is exactly that error wearing
    // a `complete` label. `force_all_with_floor` force-takes the hwmon lease and
    // drives every writable header to `max(commanded, forced)` — and
    // `on_lease_released()` is called specifically so the force re-asserts
    // `pwm_enable=1`, which means `reclaim_or_lost_pump` **cannot** see it: that
    // predicate keys on `pwm_enable != 1`. So every watched tach jumps for a
    // reason unrelated to our perturbation, the deltas are meaningless, and the
    // run would otherwise persist them as a confirmed PWM→tach mapping that
    // later runs and the UI treat as measured fact. A wrong mapping is worse
    // than no mapping.
    //
    // Reported as an abort rather than silently suppressing the persist, so the
    // operator is told the measurement was abandoned instead of watching a run
    // succeed and quietly record nothing.
    if let Some(reason) = thermal_gate() {
        bail!(STATE_ABORTED, reason);
    }

    // Return to the baseline before the guard runs, so a run whose captured
    // original duty is unreadable still leaves the header somewhere deliberate
    // rather than at the perturbed duty. Guarded by the same shutdown re-check
    // as the two writes above, and for the same reason.
    //
    // [SAFETY] The thermal-force term is kept even though the gate above
    // subsumes it today: it is the TOCTOU backstop for a force that engages in
    // the microseconds between, and it is the condition `RestoreOnDrop` — which
    // runs on the very next line — uses for its own stand-down before logging
    // that the header was "left at the thermal-safety forced duty". Before
    // DEC-339 this write ran unconditionally, so it could move the header off
    // the forced duty and make the guard's own log line false: the one write in
    // this function that could fight the ladder, immediately above the code that
    // stood down from exactly that.
    //
    // **Direction note, corrected in review.** An earlier draft justified
    // exempting this write from the other two predicates by saying refusal
    // "would strand the header at the perturbed duty". That is backwards for the
    // common case: `perturbation_target` picks `up` whenever `room_up >=
    // room_down`, which is any baseline at or below ~60, so the perturbed duty is
    // usually HIGHER and returning to baseline is the duty-lowering move. The
    // exemption is now moot for the voluntary-abort limbs — the gate above bails
    // before reaching this line — and what remains true is only the narrow claim
    // about a force.
    //
    // `TS-aw` (DEC-418): a header that became pump-protected after the last
    // window returns to the baseline raised to the pump floor. Every window
    // completed, so the run still reports `complete`; the baseline was planned
    // for an ordinary fan and may sit below the floor. Written rather than
    // skipped (review `K1`): when the pre-run duty was unreadable, this is the
    // duty `RestoreOnDrop` leaves the header at, and skipping it left the header
    // at the perturbed duty — lower than before this change.
    if !shutting_down() && thermal_force_state(cache).is_none() {
        let pct = if pump_watch.became_protected() {
            baseline_pct.max(crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8)
        } else {
            baseline_pct
        };
        pump_watch.note_write(pct);
        let _ = write_fn(pct);
    }

    DiscoveryOutcome {
        state: STATE_COMPLETE,
        detail: None,
        cycles: measured,
        sample_count,
        observed_resolution_ms: fold_resolution(&resolution_samples, target_idx),
    }
}

/// [SAFETY] The two mid-run abort predicates that depend on a fresh reading.
///
/// Kept together and pure so both limbs are exercised by one test each, and so
/// the pump limb cannot be dropped from one call site while surviving in the
/// other — this is checked after BOTH windows, deliberately.
fn reclaim_or_lost_pump(
    sample: &DiscoverySample,
    commanded_pct: u8,
    pump_protected: bool,
    had_target_tach: bool,
    target_rpm: Option<u16>,
) -> Option<String> {
    // The DEC-326 full-speed-alias exemption: some drivers report `pwm_enable=0`
    // to mean "full speed", which is our own 100 % write reflected back rather
    // than somebody else's reclaim.
    let reclaimed = matches!(sample.header.pwm_enable, Some(en) if en != 1)
        && !crate::pwm::is_full_speed_alias(
            commanded_pct,
            sample.header.pwm_percent,
            sample.header.pwm_enable,
        );
    if reclaimed {
        return Some(format!(
            "another controller reclaimed the header at {commanded_pct}% (pwm_enable={}); \
             discovery stopped",
            sample.header.pwm_enable.unwrap_or(0)
        ));
    }
    if pump_tach_lost(pump_protected, had_target_tach, target_rpm) {
        return Some(
            "the pump's tachometer stopped reporting during the test; discovery stopped \
             and the header is being restored"
                .into(),
        );
    }
    None
}

/// Result of holding one duty for a window.
struct Observed {
    last: DiscoverySample,
    /// Per channel, every sub-sample taken during this window.
    per_channel: Vec<Vec<Option<u16>>>,
}

/// Why a window ([`observe`], [`settle_wait`]) ended before its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowStop {
    /// The daemon began shutting down.
    ShuttingDown,
    /// `TS-aw`: the header became pump-protected mid-window.
    PumpProtected,
}

impl WindowStop {
    /// The abort detail; both arms end the run as `aborted`.
    fn detail(self) -> String {
        match self {
            WindowStop::ShuttingDown => "the daemon is shutting down".into(),
            WindowStop::PumpProtected => pump_protected_mid_run_detail("control-path discovery"),
        }
    }
}

/// The per-sample stop check both windows run first: shutdown, then — never
/// while shutting down, and only while the window is still `open` — the pump
/// watch (`TS-aw`). A window whose time has elapsed is measured (the user's
/// rule, DEC-418 review); the next write's check or the restore acts on a flip
/// seen after that, and nothing is written in between.
fn window_stop<S: Fn() -> bool>(
    shutting_down: &S,
    pump_watch: &PumpWatch<'_>,
    open: bool,
) -> Result<(), WindowStop> {
    if shutting_down() {
        return Err(WindowStop::ShuttingDown);
    }
    if open && pump_watch.became_protected() {
        return Err(WindowStop::PumpProtected);
    }
    Ok(())
}

/// Hold for `window`, sub-sampling every channel. An `Err` means the window
/// ended early — shutdown, or a header that became pump-protected — which the
/// caller turns into an abort.
async fn observe<R, Fut, S>(
    read_fn: &R,
    window: Duration,
    shutting_down: &S,
    pump_watch: &PumpWatch<'_>,
    run_started: tokio::time::Instant,
    resolution: &mut [Vec<(u64, Option<u16>)>],
    sample_count: &mut u32,
) -> Result<Observed, WindowStop>
where
    R: Fn() -> Fut,
    Fut: std::future::Future<Output = DiscoverySample>,
    S: Fn() -> bool,
{
    // `tokio::time::Instant`, NOT `std::time::Instant`: the latter does not
    // advance under `#[tokio::test(start_paused)]`, so this loop's exit
    // condition would never be reached and a test would hang instead of failing
    // (CLAUDE.md, tokio-test trap 1). Identical behaviour in production.
    let started = tokio::time::Instant::now();
    let mut last;
    let mut per_channel: Vec<Vec<Option<u16>>> = vec![Vec::new(); resolution.len()];
    loop {
        // Same rule as the characterisation settle: the sub-sample cadence is
        // what bounds how long a shutdown — or a mid-run pump flip — waits for
        // this task to stop touching hardware.
        window_stop(shutting_down, pump_watch, started.elapsed() < window)?;
        // `.await`, not a call: the production `read_fn` dispatches this
        // sample's ~35 blocking `std::fs` reads to the blocking pool, so a tach
        // `open(2)` wedged in the driver parks a pool thread instead of a tokio
        // worker (`P8-am`). The cadence below is unchanged.
        let sample = read_fn().await;
        *sample_count = sample_count.saturating_add(1);
        let at_ms = run_started.elapsed().as_millis() as u64;
        for (i, slot) in per_channel.iter_mut().enumerate() {
            let v = sample.tachs.get(i).copied().flatten();
            slot.push(v);
            if let Some(res) = resolution.get_mut(i) {
                res.push((at_ms, v));
            }
        }
        last = sample;
        if started.elapsed() >= window {
            break;
        }
        let remaining = window.saturating_sub(started.elapsed());
        tokio::time::sleep(remaining.min(constants::DISCOVERY_SAMPLE_INTERVAL)).await;
    }
    Ok(Observed { last, per_channel })
}

/// The run's observed update interval (DEC-405): the header's own tach's, when
/// it has one that established a cadence — that is the chip the timings are
/// about — else the fastest channel's, as before.
fn fold_resolution(
    per_channel: &[Vec<(u64, Option<u16>)>],
    target_idx: Option<usize>,
) -> Option<u64> {
    target_idx
        .and_then(|i| per_channel.get(i))
        .and_then(|s| measurement_resolution_ms(s))
        .or_else(|| {
            per_channel
                .iter()
                .filter_map(|s| measurement_resolution_ms(s))
                .min()
        })
}

/// What a settle-wait established.
struct SettleWait {
    /// Per channel: settled, or had nothing to settle.
    settled: Vec<bool>,
    elapsed_ms: u64,
    /// The wait's final reading, for the reclaim / lost-pump-tach check every
    /// window's last reading gets.
    last: DiscoverySample,
}

/// Can this channel be released from a settle-wait without settling on updates?
///
/// Only when it has produced nothing that could be settling: every readable
/// value it has shown this run — `reference` included — is the same, over at
/// least [`constants::DISCOVERY_UNCHANGED_SPAN`] of observation, or it has never
/// been readable at all. The span is the point: two seconds of one value is
/// what a slow register looks like before it refreshes.
fn nothing_to_settle(history: &[(u64, Option<u16>)], reference: Option<u16>) -> bool {
    let mut readable = history.iter().filter_map(|(at, v)| v.map(|v| (*at, v)));
    let Some((first_at, first_v)) = readable.next() else {
        return reference.is_none();
    };
    if reference.is_some_and(|r| r != first_v) {
        return false;
    }
    let mut last_at = first_at;
    for (at, v) in readable {
        if v != first_v {
            return false;
        }
        last_at = at;
    }
    last_at.saturating_sub(first_at) >= constants::DISCOVERY_UNCHANGED_SPAN.as_millis() as u64
}

/// DEC-405 (`PTR-c`): hold the just-written duty until every channel that can
/// move has settled, or `bound` elapses. An `Err` means shutdown began or the
/// header became pump-protected (`TS-aw`).
///
/// Settling is judged with [`stats::settled_on_updates`] while the wait is open
/// and only with the full [`stats::settling_ms`] once it has closed: online, the
/// constant-window rule would release a slow register before its first refresh.
/// Samples are appended to `resolution`, so the wait also informs the cadence.
#[allow(clippy::too_many_arguments)]
async fn settle_wait<R, Fut, S>(
    read_fn: &R,
    bound: Duration,
    shutting_down: &S,
    pump_watch: &PumpWatch<'_>,
    run_started: tokio::time::Instant,
    resolution: &mut [Vec<(u64, Option<u16>)>],
    sample_count: &mut u32,
    reference: &[Option<u16>],
) -> Result<SettleWait, WindowStop>
where
    R: Fn() -> Fut,
    Fut: std::future::Future<Output = DiscoverySample>,
    S: Fn() -> bool,
{
    use crate::api::stats::{self, RpmSample};
    // `tokio::time::Instant` for the same reason as `observe` (tokio trap 1).
    let started = tokio::time::Instant::now();
    let n = resolution.len();
    let mut per_channel: Vec<Vec<RpmSample>> = vec![Vec::new(); n];
    let reference_of = |i: usize| reference.get(i).copied().flatten();
    let mut last;
    loop {
        // Always open: the wait is not a measurement, and the baseline window
        // that follows it would catch the flip at its first sample anyway.
        window_stop(shutting_down, pump_watch, true)?;
        let sample = read_fn().await;
        *sample_count = sample_count.saturating_add(1);
        let at_run = run_started.elapsed().as_millis() as u64;
        let at = started.elapsed().as_millis() as u64;
        for (i, slot) in per_channel.iter_mut().enumerate() {
            let v = sample.tachs.get(i).copied().flatten();
            slot.push(RpmSample { at_ms: at, rpm: v });
            if let Some(res) = resolution.get_mut(i) {
                res.push((at_run, v));
            }
        }
        last = sample;
        let open_done = (0..n).all(|i| {
            nothing_to_settle(&resolution[i], reference_of(i))
                || stats::settled_on_updates(&per_channel[i], reference_of(i)).is_some()
        });
        if open_done || started.elapsed() >= bound {
            break;
        }
        let remaining = bound.saturating_sub(started.elapsed());
        tokio::time::sleep(remaining.min(constants::DISCOVERY_SAMPLE_INTERVAL)).await;
    }
    let settled = (0..n)
        .map(|i| {
            nothing_to_settle(&resolution[i], reference_of(i))
                || stats::settling_ms(&per_channel[i], reference_of(i)).is_some()
        })
        .collect();
    Ok(SettleWait {
        settled,
        elapsed_ms: started.elapsed().as_millis() as u64,
        last,
    })
}

/// A monotonically increasing run id. Opaque to clients; only used so a polling
/// GUI can tell "my run" from "a later one".
pub fn next_run_id() -> String {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(1);
    format!("path-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// The shared slot holding the current or most recent run.
pub type ControlPathSlot = std::sync::Arc<parking_lot::Mutex<Option<ControlPathRun>>>;
