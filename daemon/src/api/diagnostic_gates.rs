//! [SAFETY] The per-step gates a hardware diagnostic applies before it writes a
//! duty, defined once (DEC-407, Stage 3 of DEC-404).
//!
//! `characterization::run_sweep` spelled these out inline at two sites — the
//! top of every point and inside every hold — and the stall/restart probe
//! (`api::stall_probe`) needs the same ones at every write and on every sample.
//! Three copies of a safety rule is how one site ends up checking a subset
//! (DEC-336's finding), so the rule lives here and every loop calls it.
//!
//! **Behaviour-preserving by construction for characterisation.** The order is
//! the one `run_sweep` already used — shutdown, cancel, the three thermal
//! gates, keepalive — and the detail strings are produced from the same format
//! strings, parameterised only by the subject and the action the caller names.
//! Characterisation's own tests are the oracle for that claim.
//!
//! Control-path discovery keeps its own `thermal_gate` closure deliberately: its
//! stale-temperature detail carries no prefix, so folding it in here would change
//! a published string from a change that did not scope discovery.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::api::calibration::{
    check_thermal_safety, stale_temperature_refusal, thermal_force_state,
};
use crate::api::preflight::Diagnostic;
use crate::health::cache::StateCache;

/// Why [`step_gate`] stopped a run. The caller owns the wording of every arm
/// except [`GateStop::Thermal`], whose detail is already composed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GateStop {
    /// The daemon is going down; the caller must stop touching hardware.
    ShuttingDown,
    /// The caller's cancel flag is set.
    Cancelled,
    /// One of the three thermal gates refused; the detail says which.
    Thermal(ThermalRefusal, String),
    /// The keepalive failed: the deadman elapsed and a later diagnostic took
    /// this run's lease, so any further write would fail.
    Superseded,
}

/// Which of the three thermal gates refused, as a stable token — so a caller
/// that publishes a reason (the stall probe's `abort_reason`) never re-parses
/// the detail text or re-evaluates a gate whose answer may have moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThermalRefusal {
    /// A sensor is over `CALIBRATION_MAX_TEMP_C`.
    TooHot,
    /// The thermal ladder is forcing fan output.
    Forcing,
    /// No usable temperature reading: the guards cannot be evaluated.
    Stale,
}

impl ThermalRefusal {
    pub(crate) fn token(self) -> &'static str {
        match self {
            ThermalRefusal::TooHot => "thermal_limit",
            ThermalRefusal::Forcing => "thermal_force",
            ThermalRefusal::Stale => "stale_temperature",
        }
    }
}

/// [SAFETY] The three thermal gates, in their established order: the two cheap
/// `value_c` comparisons (a sensor over `CALIBRATION_MAX_TEMP_C`, the ladder
/// forcing), then the freshness refusal — the only one of the three that can
/// see a poll loop that has stopped (DEC-336 / DEC-385).
///
/// `subject` and `action` complete the caller's detail, e.g. `"characterisation"`
/// and `"cannot write"`. Returns the first gate that refused and its detail.
pub(crate) fn thermal_refusal(
    cache: &StateCache,
    diagnostic: Diagnostic,
    subject: &str,
    action: &str,
) -> Option<(ThermalRefusal, String)> {
    if let Err(e) = check_thermal_safety(cache) {
        return Some((ThermalRefusal::TooHot, e.to_string()));
    }
    if let Some(state) = thermal_force_state(cache) {
        return Some((
            ThermalRefusal::Forcing,
            format!("thermal safety is forcing fan output ({state}); {subject} {action}"),
        ));
    }
    stale_temperature_refusal(cache, diagnostic).map(|reason| {
        (
            ThermalRefusal::Stale,
            format!("{subject} {action}: {reason}"),
        )
    })
}

/// [`thermal_refusal`]'s detail alone, for a caller that publishes no token.
pub(crate) fn thermal_gate(
    cache: &StateCache,
    diagnostic: Diagnostic,
    subject: &str,
    action: &str,
) -> Option<String> {
    thermal_refusal(cache, diagnostic, subject, action).map(|(_, detail)| detail)
}

/// [SAFETY] Every gate a diagnostic applies before a write, in order: shutdown,
/// cancel, [`thermal_gate`], keepalive.
///
/// The shutdown check comes first because the diagnostic task is detached: it
/// keeps running through `shutdown_sequence`, and a write landing after
/// `hand_back_hwmon` would re-assert `pwm_enable=1` through `set_pwm`'s reclaim
/// watchdog on a header nothing will drive again (DEC-290 / 277-c). Keepalive
/// comes last so the deadman is renewed only once every other gate has passed —
/// the DEC-296 rule that it measures liveness, not duration.
pub(crate) fn step_gate<S, K>(
    cache: &StateCache,
    diagnostic: Diagnostic,
    subject: &str,
    action: &str,
    shutting_down: &S,
    cancel: &AtomicBool,
    keepalive: &K,
) -> Result<(), GateStop>
where
    S: Fn() -> bool,
    K: Fn() -> bool,
{
    if shutting_down() {
        return Err(GateStop::ShuttingDown);
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(GateStop::Cancelled);
    }
    if let Some((which, detail)) = thermal_refusal(cache, diagnostic, subject, action) {
        return Err(GateStop::Thermal(which, detail));
    }
    if !keepalive() {
        return Err(GateStop::Superseded);
    }
    Ok(())
}

/// [SAFETY] `TS-aw` (DEC-418): the pump-protection union, re-read while a
/// diagnostic runs rather than once before it.
///
/// Verify, characterisation and control-path discovery each decide at entry
/// whether the header is pump-protected, and plan their duties from that answer.
/// Pump evidence can arrive mid-run: a profile naming the header a pump is
/// activated (DEC-384's profile term), or the header is assigned the `pump`
/// role (DEC-311). Before this, that evidence was not reconsidered, and a run
/// planned for an ordinary fan could go on driving a now-protected pump below
/// the 30 % floor until it ended.
///
/// The user's rule (2026-09-23) is DEC-407's `eligibility_lost` precedent, not
/// clamp-and-continue: **when the header becomes pump-protected mid-run the run
/// stops, and its restore is floored at the pump floor.** Each run calls
/// [`PumpWatch::became_protected`] before every write and on every sample, and
/// `RestoreOnDrop` (or verify's own restore) consults
/// [`PumpWatch::restore_is_pump`] once more immediately before it writes.
///
/// A header protected at entry cannot *become* protected: its run is already a
/// pump run on pump-safe duties, so the watch never re-reads it. Evidence
/// going *away* mid-run is not acted on either — a pump run keeps its floor.
///
/// **The answer is sticky.** Once seen protected the watch says so for the rest
/// of the run, even if the evidence is withdrawn a moment later, so the restore
/// floor cannot be lowered again by a racing un-assignment. The stall probe's
/// `pump_seen` is the same rule.
///
/// **A hold whose time has elapsed is a completed measurement** (the user's
/// rule, DEC-418 review): each loop consults the watch only while its window is
/// still open, so evidence that arrives as a window ends does not discard it —
/// the next write's check or the restore's re-read acts on it instead, and no
/// write happens in between.
///
/// **It also remembers the last duty the run wrote** ([`PumpWatch::note_write`]),
/// for the one restore that has no captured original to floor: a header whose
/// pre-run duty could not be read is left where the run left it, as it always
/// was, but raised to the pump floor if it has become a pump (the user's
/// choice, DEC-418 review, `K1`). Never lower than before this change.
///
/// `check` must take no lock the caller holds. Production passes
/// `AppState::header_is_pump_protected`, which takes `active_profile`,
/// `header_roles` and `hwmon_controller` one at a time and releases each, so
/// every call site calls it with nothing held — never from inside a `set_pwm`
/// critical section. Nor is it called once shutdown has been seen: each run
/// checks shutdown first, and `RestoreOnDrop` skips before it reaches the watch.
/// Nothing a stopping run could do with the answer is left by then — it writes
/// nothing more — and the exit floor is contending for the controller lock the
/// lookup takes (bounded, `apply_exit_floor`'s `try_lock_for`).
pub struct PumpWatch<'a> {
    header_id: String,
    subject: &'static str,
    at_start: bool,
    check: Box<dyn Fn() -> bool + Send + Sync + 'a>,
    seen: AtomicBool,
    /// The last duty the run wrote; `None` until [`PumpWatch::note_write`].
    last_written: AtomicU8,
    wrote: AtomicBool,
}

impl<'a> PumpWatch<'a> {
    /// `at_start` is the union read when the run was planned; `check` re-reads
    /// it. `subject` names the diagnostic in the log line, e.g. `"verify"`.
    pub fn new(
        header_id: impl Into<String>,
        subject: &'static str,
        at_start: bool,
        check: impl Fn() -> bool + Send + Sync + 'a,
    ) -> Self {
        Self {
            header_id: header_id.into(),
            subject,
            at_start,
            check: Box::new(check),
            seen: AtomicBool::new(false),
            last_written: AtomicU8::new(0),
            wrote: AtomicBool::new(false),
        }
    }

    /// Record a duty the run is about to write. Stamped BEFORE the write, the
    /// `wrote_any` rule: a write that errors can still have moved the header.
    pub fn note_write(&self, pct: u8) {
        self.last_written.store(pct, Ordering::SeqCst);
        self.wrote.store(true, Ordering::SeqCst);
    }

    /// [SAFETY] `K1`: what to write when the run moved the header, its pre-run
    /// duty is unknown, and it must now be treated as a pump — the last duty the
    /// run wrote, raised to the pump floor. `None` when no restore is owed: the
    /// run wrote nothing, or the header is not a pump.
    pub fn floored_fallback(&self) -> Option<u8> {
        if !self.wrote.load(Ordering::SeqCst) || !self.restore_is_pump() {
            return None;
        }
        let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
        Some(self.last_written.load(Ordering::SeqCst).max(floor))
    }

    /// A watch whose entry answer is final — for a caller with no union to
    /// consult: the pure loops' own tests, unit and integration alike (a
    /// `#[cfg(test)]` item is invisible to `daemon/tests/`).
    pub fn fixed(at_start: bool) -> PumpWatch<'static> {
        PumpWatch::new("", "test", at_start, || false)
    }

    /// Whether the header was NOT pump-protected when the run was planned and
    /// has been seen protected since. Re-reads the union until it answers yes,
    /// then answers yes without reading again. Logs the first sighting once.
    pub fn became_protected(&self) -> bool {
        if self.at_start {
            return false;
        }
        if self.seen.load(Ordering::SeqCst) {
            return true;
        }
        if !(self.check)() {
            return false;
        }
        if !self.seen.swap(true, Ordering::SeqCst) {
            // Worded for both cases: a flip mid-measurement stops the run, and
            // one after it only floors the restore. Either way nothing below
            // the floor is written again.
            log::warn!(
                "{}: became pump-protected while a {} planned for an ordinary fan was \
                 running (a profile naming it a pump was activated, or it was assigned \
                 the pump role); nothing below the {}% pump floor is written again, and \
                 its restore is floored there",
                self.header_id,
                self.subject,
                crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8
            );
        }
        true
    }

    /// Whether a restore must be floored at the pump floor: protected at entry,
    /// or seen protected at any point since — this call included.
    pub fn restore_is_pump(&self) -> bool {
        self.at_start || self.became_protected()
    }
}

/// What a run says when [`PumpWatch::became_protected`] stops it. One wording
/// for all three diagnostics; `subject` names which, e.g. `"characterisation"`.
///
/// It says the restore is *floored*, not that it happened: whether it landed is
/// the run's `restore_outcome` / `restore_failed`, which a shutdown or a thermal
/// force can legitimately skip.
pub(crate) fn pump_protected_mid_run_detail(subject: &str) -> String {
    format!(
        "the header became pump-protected during the {subject} (a profile naming it a \
         pump was activated, or it was assigned the pump role), so the {subject} stopped \
         and its restore is floored at the {}% pump floor",
        crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// A watch over a union the test flips, counting how often it is read.
    fn flipping() -> (PumpWatch<'static>, Arc<AtomicBool>, Arc<AtomicUsize>) {
        let union = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let (u, r) = (union.clone(), reads.clone());
        let watch = PumpWatch::new("hwmon:t:d:pwm1", "verify", false, move || {
            r.fetch_add(1, Ordering::SeqCst);
            u.load(Ordering::SeqCst)
        });
        (watch, union, reads)
    }

    #[test]
    fn a_watch_answers_the_live_union_until_it_flips() {
        let (watch, union, reads) = flipping();
        assert!(!watch.became_protected());
        assert!(!watch.restore_is_pump());
        assert_eq!(reads.load(Ordering::SeqCst), 2, "each call re-reads");
        union.store(true, Ordering::SeqCst);
        assert!(watch.became_protected());
        assert!(watch.restore_is_pump());
    }

    /// [SAFETY] Sticky: evidence withdrawn after it was seen must not lower the
    /// restore floor again, and the union is not read once it has said yes.
    #[test]
    fn once_seen_protected_the_answer_does_not_go_back() {
        let (watch, union, reads) = flipping();
        union.store(true, Ordering::SeqCst);
        assert!(watch.became_protected());
        let after_flip = reads.load(Ordering::SeqCst);
        union.store(false, Ordering::SeqCst);
        assert!(watch.became_protected());
        assert!(watch.restore_is_pump());
        assert_eq!(reads.load(Ordering::SeqCst), after_flip);
    }

    /// A header protected at entry is already a pump run on pump-safe duties:
    /// it never "becomes" protected (so it never aborts), its restore is always
    /// floored, and the union is never read.
    #[test]
    fn a_pump_run_never_aborts_and_always_floors_its_restore() {
        let reads = Arc::new(AtomicUsize::new(0));
        let r = reads.clone();
        let watch = PumpWatch::new("h", "verify", true, move || {
            r.fetch_add(1, Ordering::SeqCst);
            false
        });
        assert!(!watch.became_protected());
        assert!(watch.restore_is_pump());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    /// [SAFETY] `K1`: the fallback is the last written duty raised to the floor
    /// — never lowered to it — and there is none without a write or a pump.
    #[test]
    fn the_fallback_raises_the_last_duty_to_the_floor_and_never_lowers_it() {
        let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
        let (watch, union, _) = flipping();
        union.store(true, Ordering::SeqCst);
        assert_eq!(watch.floored_fallback(), None, "no write, no restore owed");
        watch.note_write(20);
        assert_eq!(watch.floored_fallback(), Some(floor));
        watch.note_write(80);
        assert_eq!(watch.floored_fallback(), Some(80), "raised, never lowered");

        let (ordinary, _, _) = flipping();
        ordinary.note_write(20);
        assert_eq!(
            ordinary.floored_fallback(),
            None,
            "not a pump: nothing owed"
        );
    }

    #[test]
    fn the_detail_names_the_diagnostic_and_the_floor() {
        let d = pump_protected_mid_run_detail("characterisation");
        assert!(d.contains("during the characterisation"), "{d}");
        assert!(
            d.contains(&format!(
                "{}%",
                crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8
            )),
            "{d}"
        );
    }
}
