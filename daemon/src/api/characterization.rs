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
//! - Points are swept **ascending**, so an abort part-way leaves the header
//!   *high* rather than low.
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
}

/// One measured point. The three axes stay separate on the wire — see the
/// module docs for why collapsing them is a defect, not a simplification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
}

/// Derived diagnostics over a whole sweep. Produced by [`summarise`], which is
/// pure — the handler must call it rather than deriving any of this inline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
}

/// A characterisation run, and the body of `GET /diagnostics/characterization`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
/// `pwm_enable` is not 1, the value read back is not ours to interpret.
fn readback_verdict(requested_pct: u8, readback_pct: Option<u8>, pwm_enable: Option<u8>) -> String {
    if let Some(en) = pwm_enable {
        if en != 1 {
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
pub fn summarise(points: &[CharPoint]) -> CharSummary {
    let accepted = points.iter().filter(|p| p.command_accepted).count();
    let command_acceptance = if points.is_empty() || accepted == 0 {
        "fail"
    } else if accepted == points.len() {
        "pass"
    } else {
        "partial"
    }
    .to_string();

    let interference_detected = points
        .iter()
        .any(|p| matches!(p.pwm_enable, Some(en) if en != 1));

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
struct RestoreOnDrop<'a, W: Fn(u8) -> Result<(), String>, S: Fn() -> bool> {
    header_id: &'a str,
    original_pct: Option<u8>,
    write_fn: &'a W,
    cache: &'a StateCache,
    shutting_down: &'a S,
    /// Set by the sweep loop before its first write. Distinguishes "there was no
    /// pre-sweep duty to restore and we never moved the header" (nothing to
    /// report) from "we moved it and cannot put it back" ([`RestoreOutcome::NoOriginalDuty`]).
    wrote_any: &'a AtomicBool,
    report: &'a RestoreReport,
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
    points: &[u8],
    settle: Duration,
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
    let mut measured: Vec<CharPoint> = Vec::with_capacity(points.len());
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
    };

    for (idx, &pct) in points.iter().enumerate() {
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
                detail: Some(format!("cancelled after {idx} of {} points", points.len())),
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

        // Hold the full settle, sub-sampling for the first movement. No early
        // exit: a deterministic window keeps the pause budget an upper bound.
        // `tokio::time::Instant`, NOT `std::time::Instant`: the latter does not
        // advance under `#[tokio::test(start_paused)]`, so this loop's exit
        // condition would never be reached and the test would hang rather than
        // fail (CLAUDE.md, tokio-test trap 1). Identical behaviour in production.
        let started = tokio::time::Instant::now();
        let mut first_change_ms: Option<u64> = None;
        while started.elapsed() < settle {
            let remaining = settle.saturating_sub(started.elapsed());
            tokio::time::sleep(remaining.min(constants::CHARACTERIZATION_SAMPLE_INTERVAL)).await;
            // Same rule mid-settle: the sub-sample cadence is what bounds how
            // long a shutdown waits for this task to stop touching hardware.
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
                });
                return SweepOutcome {
                    state: STATE_ABORTED,
                    detail: Some("the daemon is shutting down".into()),
                    points: measured,
                };
            }
            if first_change_ms.is_none() {
                if let (Some(b), Some(now)) = (rpm_before, read_fn().rpm) {
                    if rpm_moved(b, now) {
                        first_change_ms = Some(started.elapsed().as_millis() as u64);
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
        };
        let reclaimed = matches!(after.pwm_enable, Some(en) if en != 1);
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
    use crate::health::state::{CachedSensorReading, DeviceLabel};
    use crate::hwmon::types::SensorKind;
    use std::sync::Mutex;

    const PUMP_FLOOR: u8 = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;

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
        let s = summarise(&pts);
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
        let s = summarise(&pts);
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
        let s = summarise(&pts);
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
        let s = summarise(&pts);
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
        let s = summarise(&pts);
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
        let s = summarise(&pts).dead_zone_upper_pct;
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
        let s = summarise(&pts);
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
        assert_eq!(summarise(&quiet).rpm_response, "no_response");
        let real = vec![
            point(30, Some(30), Some(1), Some(300)),
            point(100, Some(100), Some(1), Some(700)),
        ];
        assert_eq!(summarise(&real).rpm_response, "responsive");
    }

    #[test]
    fn no_tach_is_unavailable_rather_than_no_response() {
        let pts = vec![
            point(30, Some(30), Some(1), None),
            point(100, Some(100), Some(1), None),
        ];
        let s = summarise(&pts);
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
        assert_eq!(summarise(&pts).command_acceptance, "partial");
        assert_eq!(summarise(&[]).command_acceptance, "fail");
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
        run_sweep(
            cache,
            "hwmon:test:pwm1",
            points,
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
        // One liveness renewal per point (DEC-296), not one for the whole run.
        assert_eq!(*rig.keepalives.lock().unwrap(), 3);
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
        assert_eq!(summarise(&out.points).command_acceptance, "partial");
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
        assert_eq!(summarise(&out.points).pwm_readback, "reverted");
        assert!(summarise(&out.points).interference_detected);
        assert_eq!(rig.written(), vec![30, 42]);
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

        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
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

        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
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

        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
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

        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &[30, 50],
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
            let fut = run_sweep(
                &cache,
                "hwmon:test:pwm1",
                &[30, 60, 90],
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
        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &points,
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
        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &[30, 60, 90],
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
        let out = run_sweep(
            &cache,
            "hwmon:test:pwm1",
            &[30, 60, 90],
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
}
