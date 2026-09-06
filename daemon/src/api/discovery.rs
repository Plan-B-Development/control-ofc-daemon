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
use crate::api::characterization::{RestoreOnDrop, RestoreReport};
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
    /// driver's own `update_interval` if it publishes one, else the smallest
    /// interval between two *differing* samples this run actually observed.
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
}

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

/// Smallest interval between two consecutive samples whose value differed.
///
/// §4: "Do not report sub-second timing precision if the underlying hwmon value
/// updates every ~1–2 seconds." This derives the cadence from samples the run
/// **already took**, which is why there is no second, faster polling loop: a
/// driver that only refreshes every 2 s produces runs of identical readings, and
/// the gap between changes is exactly the quantity §4 asks for.
///
/// `None` when nothing ever changed — reported as UNKNOWN rather than guessed.
pub fn measurement_resolution_ms(samples: &[(u64, Option<u16>)]) -> Option<u64> {
    let mut last_value: Option<u16> = None;
    // The timestamp of the previous OBSERVED CHANGE, not of the previous sample.
    //
    // The distinction is the whole correctness of this function. The first change
    // in a series has no known start: the value was already whatever it was when
    // sampling began, so the gap between sample 0 and the first change is a
    // measure of when we started looking, not of how often the driver updates.
    // Counting it under-reports the cadence — a 2 s driver first sampled 1 s
    // before its first refresh would be reported as 1 s, which is exactly the
    // false precision §4 exists to prevent. Two changes are therefore required
    // before any interval is reported, and one change alone yields UNKNOWN.
    let mut prev_change_at: Option<u64> = None;
    let mut smallest: Option<u64> = None;
    for (at_ms, value) in samples {
        let Some(v) = value else { continue };
        match last_value {
            None => last_value = Some(*v),
            Some(prev) if prev != *v => {
                if let Some(prev_ms) = prev_change_at {
                    let interval = at_ms.saturating_sub(prev_ms);
                    if interval > 0 {
                        smallest = Some(smallest.map_or(interval, |s: u64| s.min(interval)));
                    }
                }
                prev_change_at = Some(*at_ms);
                last_value = Some(*v);
            }
            Some(_) => {}
        }
    }
    smallest
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
pub async fn run_discovery<W, R, P, S, K>(
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
    window: Duration,
    write_fn: W,
    read_fn: R,
    cancel: &AtomicBool,
    shutting_down: S,
    keepalive: K,
    report: &RestoreReport,
    mut publish: P,
) -> DiscoveryOutcome
where
    W: Fn(u8) -> Result<(), String>,
    R: Fn() -> DiscoverySample,
    P: FnMut(DiscoveryCycle),
    S: Fn() -> bool,
    K: Fn() -> bool,
{
    let first = read_fn();
    let original_pct = first.header.pwm_percent;
    // Which tach belongs to the header being perturbed, if any. Captured before
    // anything is written so `pump_tach_lost` compares against the pre-run truth.
    let target_idx = channels.iter().position(|c| c.is_target_header);
    let had_target_tach = target_idx
        .and_then(|i| first.tachs.get(i).copied().flatten())
        .is_some();

    let mut measured: Vec<DiscoveryCycle> = Vec::with_capacity(cycle_count as usize);
    let mut sample_count: u32 = 0;
    let mut resolution_samples: Vec<Vec<(u64, Option<u16>)>> = vec![Vec::new(); channels.len()];
    let wrote_any = AtomicBool::new(false);
    let run_started = tokio::time::Instant::now();

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
    };

    macro_rules! bail {
        ($state:expr, $detail:expr) => {
            return DiscoveryOutcome {
                state: $state,
                detail: Some($detail),
                cycles: measured,
                sample_count,
                observed_resolution_ms: fold_resolution(&resolution_samples),
            }
        };
    }

    for cycle in 1..=cycle_count {
        // [SAFETY] The same four gates the characterisation sweep applies at the
        // top of every point, for the same reasons. The shutdown check is not
        // covered by the drop guard's own skip: this task is detached, so it
        // keeps running through `shutdown_sequence` and could otherwise land a
        // write after `restore_hwmon_to_auto` handed the header back to firmware.
        if shutting_down() {
            bail!(STATE_ABORTED, "the daemon is shutting down".into());
        }
        if cancel.load(Ordering::SeqCst) {
            bail!(
                STATE_CANCELLED,
                format!("cancelled after {} of {cycle_count} cycles", cycle - 1)
            );
        }
        if let Err(e) = check_thermal_safety(cache) {
            bail!(STATE_ABORTED, e.to_string());
        }
        if let Some(state) = thermal_force_state(cache) {
            bail!(
                STATE_ABORTED,
                format!(
                    "thermal safety is forcing fan output ({state}); control-path \
                     discovery cannot write"
                )
            );
        }
        // [SAFETY] DEC-336 (`P8-p`): the third thermal gate, and the only one
        // that can see a poll loop that has stopped. The two above compare
        // `value_c` against a limit; neither has a view of how OLD that value
        // is, so a reader wedged mid-run freezes the cache and both keep
        // passing on last-known-good readings while this sweep goes on writing.
        // Evaluated ONCE PER CYCLE, alongside its two siblings — **not** ahead
        // of every `keepalive()`, which an earlier draft of this comment
        // claimed. A cycle holds two observation windows and therefore two
        // keepalives, so a poll that wedges just after this check leaves the
        // header at the perturbed duty for up to two windows (~30 s at the
        // documented 15 s maximum) before the run aborts and restores. That is
        // the same cadence `check_thermal_safety` and `thermal_force_state`
        // have always run at, so this matches its siblings rather than
        // introducing a new gap — and tightening all three to per-window is
        // register row `P8-u`, deliberately out of scope here. The comment is
        // corrected rather than the code because a safety comment that
        // overstates its own cadence is how the next reader concludes the gap
        // is already covered.
        if let Some(reason) = stale_temperature_refusal(cache, DISCOVERY_DIAGNOSTIC) {
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
        // can land in that gap and a write issued after `restore_hwmon_to_auto`
        // would re-assert `pwm_enable=1` at a fixed duty with no writer left
        // (the DEC-290 / 277-c hazard the drop guard's own skip exists for).
        if shutting_down() {
            bail!(STATE_ABORTED, "the daemon is shutting down".into());
        }
        wrote_any.store(true, Ordering::SeqCst);
        if let Err(e) = write_fn(baseline_pct) {
            bail!(
                STATE_FAILED,
                format!("PWM write of {baseline_pct}% failed: {e}")
            );
        }
        let base = match observe(
            &read_fn,
            window,
            &shutting_down,
            run_started,
            &mut resolution_samples,
            &mut sample_count,
        )
        .await
        {
            Some(s) => s,
            None => bail!(STATE_ABORTED, "the daemon is shutting down".into()),
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
        if let Err(e) = write_fn(perturbed_pct) {
            bail!(
                STATE_FAILED,
                format!("PWM write of {perturbed_pct}% failed: {e}")
            );
        }
        let pert = match observe(
            &read_fn,
            window,
            &shutting_down,
            run_started,
            &mut resolution_samples,
            &mut sample_count,
        )
        .await
        {
            Some(s) => s,
            None => bail!(STATE_ABORTED, "the daemon is shutting down".into()),
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
                let noise = noise_floor(&base.per_channel[i]);
                TachObservation {
                    tach_id: ch.tach_id.clone(),
                    baseline_rpm,
                    perturbed_rpm,
                    delta_rpm: baseline_rpm
                        .zip(perturbed_rpm)
                        .map(|(b, p)| i32::from(p) - i32::from(b)),
                    noise_floor_rpm: noise,
                    responded: responded(baseline_rpm, perturbed_rpm, noise),
                }
            })
            .collect();

        let done = DiscoveryCycle {
            cycle,
            baseline_pct,
            perturbed_pct,
            direction: direction.to_string(),
            observations,
        };
        measured.push(done.clone());
        publish(done);
    }

    // Return to the baseline before the guard runs, so a run whose captured
    // original duty is unreadable still leaves the header somewhere deliberate
    // rather than at the perturbed duty. Guarded by the same shutdown re-check
    // as the two writes above, and for the same reason.
    if !shutting_down() {
        let _ = write_fn(baseline_pct);
    }

    DiscoveryOutcome {
        state: STATE_COMPLETE,
        detail: None,
        cycles: measured,
        sample_count,
        observed_resolution_ms: fold_resolution(&resolution_samples),
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

/// Hold for `window`, sub-sampling every channel. `None` means the daemon began
/// shutting down mid-window, which the caller turns into an abort.
async fn observe<R, S>(
    read_fn: &R,
    window: Duration,
    shutting_down: &S,
    run_started: tokio::time::Instant,
    resolution: &mut [Vec<(u64, Option<u16>)>],
    sample_count: &mut u32,
) -> Option<Observed>
where
    R: Fn() -> DiscoverySample,
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
        // what bounds how long a shutdown waits for this task to stop touching
        // hardware.
        if shutting_down() {
            return None;
        }
        let sample = read_fn();
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
    Some(Observed { last, per_channel })
}

/// Smallest observed update interval across every channel.
fn fold_resolution(per_channel: &[Vec<(u64, Option<u16>)>]) -> Option<u64> {
    per_channel
        .iter()
        .filter_map(|s| measurement_resolution_ms(s))
        .min()
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
