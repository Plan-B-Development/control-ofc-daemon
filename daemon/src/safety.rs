//! CPU Tctl emergency thermal safety rule.
//!
//! Single latched rule: at [`crate::constants::THERMAL_EMERGENCY_TRIGGER_C`],
//! force every OpenFan channel and writable hwmon header the machine HAS to
//! 100% — on most machines that is hwmon alone. Hold until a FRESH Tctl reading
//! at or below [`crate::constants::THERMAL_EMERGENCY_RELEASE_C`], then return
//! control to the active profile. The thresholds are deliberately not restated
//! here — DEC-292 reduced them to one definition each precisely because a doc
//! that spells a threshold out drifts from it.
//!
//! **There is no recovery rung since DEC-386 (`TS-l`).** Release used to hold
//! 60 % for two 1 Hz ticks before handing back — two seconds, which is
//! thermally meaningless, and since DEC-307 a floor the curve already exceeds
//! at that temperature. It also gave the tick a third rule state whose
//! interactions with a stale or absent reading were the source of most of the
//! ladder's special cases. The rule is now only idle or latched; the tick's
//! decision table lives in `profile_engine::safety_tick`.
//!
//! **Every value this rule returns is a FLOOR, and since DEC-307 the engine
//! implements it as one.** Each reaches the engine as `decision.forced_pct`,
//! and the forced branch calls `force_present_backends(..)`, which drives every
//! OpenFan channel and writable hwmon header present through
//! `force_all_with_floor(pct, &commands)` — including the ones no control
//! commands, which is what preserves the emergency's reach — and a commanded
//! output gets `max(commanded, pct)`.
//!
//! **This enumeration describes the DESIGN; it is not a message (DEC-371).**
//! What an operator is told is derived per tick from `ForcedScope`, which
//! `force_present_backends` sets from inside each write arm. `evaluate` below
//! cannot know the reach — it returns a duty — so its own log line names one
//! deliberately. Do not restate the enumeration there: this file said
//! "all OpenFan+hwmon fans" unconditionally for the project's whole life, and on
//! most machines that overstated the highest-stakes line the daemon emits.
//!
//! It was not always so, and the history is the point (`D1-j`). Until DEC-307
//! the branch called `force_all(pct)` and `continue`d, skipping profile
//! evaluation entirely, so the returned value **replaced** the profile's output
//! instead of flooring it. For the 100% emergency that is invisible, because
//! 100 is the maximum. For the other two rungs it was not: on release the fans
//! were driven **to** 60%, so a curve asking for more at that temperature was
//! overridden *downward* for two ticks immediately after an excursion while the
//! CPU was still hot; and the no-CPU-sensor duty did the same to a control
//! driven by a still-healthy GPU or coolant sensor. This doc called the 60%
//! step a "floor" throughout, which is how the gap survived — name and
//! behaviour disagreed on a safety path, and only the name was ever read.
//!
//! The fix is monotone by construction: no output is ever driven lower than the
//! old `force_all(pct)` would have driven it.
//!
//! GPU fans are deliberately excluded from this rule (DEC-130): there is no
//! GPU emergency threshold. AMD PMFW protects the GPU by throttling its clocks
//! on junction temperature, independently of OS fan control; it does not ramp
//! a fan past a curve the daemon has committed (`TS-i`).

/// Emergency thermal safety override for CPU temperature.
///
/// Uses hysteresis to prevent flapping — see
/// [`crate::constants::THERMAL_EMERGENCY_TRIGGER_C`] and
/// [`crate::constants::THERMAL_EMERGENCY_RELEASE_C`] for the values, which are
/// deliberately not restated here (DEC-292: this doc used to name them, and a doc
/// that restates a threshold drifts from it exactly like a duplicated literal).
/// Edge-triggered logging — only logs on state transitions.
pub struct ThermalSafetyRule {
    trigger_temp_c: f64,
    release_temp_c: f64,
    forced_output_pct: u8,
    active: bool,
}

impl ThermalSafetyRule {
    /// Create the default CPU Tctl emergency rule.
    pub fn new() -> Self {
        Self {
            trigger_temp_c: crate::constants::THERMAL_EMERGENCY_TRIGGER_C,
            release_temp_c: crate::constants::THERMAL_EMERGENCY_RELEASE_C,
            forced_output_pct: 100,
            active: false,
        }
    }

    /// Apply a FRESH CPU Tctl reading — the only thing that may move the latch.
    ///
    /// Returns `Some(forced_pct)` while the emergency is latched after this
    /// reading, `None` when profile control should proceed. Latches at the
    /// trigger; releases at or below the release temperature and hands control
    /// straight back (DEC-386 — no recovery rung).
    ///
    /// [SAFETY] Never call it with a stale reading: a stale value is evidence of
    /// what WAS true, and must neither release the latch nor raise it (DEC-269).
    /// `safety_tick` calls this only from its fresh-reading arm.
    pub fn evaluate(&mut self, tctl_c: f64) -> Option<u8> {
        if !self.active && tctl_c >= self.trigger_temp_c {
            self.active = true;
            log::warn!(
                "THERMAL EMERGENCY: CPU Tctl {:.1}°C >= {}°C — forcing fans to {}%",
                tctl_c,
                self.trigger_temp_c,
                self.forced_output_pct
            );
        } else if self.active && tctl_c <= self.release_temp_c {
            self.active = false;
            log::info!(
                "Thermal emergency released: CPU Tctl {:.1}°C <= {}°C — control returns \
                 to the profile",
                tctl_c,
                self.release_temp_c
            );
        }
        self.active.then_some(self.forced_output_pct)
    }

    /// Whether the emergency override is currently active.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The temperature at or below which a latched emergency releases.
    ///
    /// Exposed so the tick can ask "was the last thing we knew hot enough to
    /// matter?" without duplicating the threshold (DEC-269).
    pub fn release_temp_c(&self) -> f64 {
        self.release_temp_c
    }

    /// The temperature at or above which the emergency latches.
    ///
    /// Sibling of [`Self::release_temp_c`], added by DEC-292 so a test can assert
    /// that what `/diagnostics/hardware` REPORTS equals what this rule ACTS on.
    /// Without it the two could only be compared against a literal, which is the
    /// duplication the ADR removes.
    pub fn trigger_temp_c(&self) -> f64 {
        self.trigger_temp_c
    }

    /// Set the trip point for this tick (DEC-308).
    ///
    /// [SAFETY] The engine derives this from the CPU's own reported design
    /// ceiling — see `profile_engine::effective_trigger_c`, which owns every
    /// guarantee about the value (raise-only, capped, authoritative chips only).
    /// This setter deliberately holds no policy of its own: duplicating the
    /// clamp here would be a second definition of the rule, which is what DEC-292
    /// exists to prevent.
    ///
    /// Safe to call while latched, and called unconditionally every tick. Moving
    /// the trip point cannot release an active emergency or extend one: `active`
    /// is cleared solely by a reading at or below `release_temp_c`, which this
    /// does not touch. So a sensor appearing or vanishing mid-emergency changes
    /// what it would take to *re-enter*, never what it takes to leave.
    pub fn set_trigger_temp_c(&mut self, trigger_c: f64) {
        self.trigger_temp_c = trigger_c;
    }

    /// The duty a latched emergency forces — what the tick holds while blind.
    ///
    /// [SAFETY] DEC-269 / DEC-386. A latched emergency means a fresh reading at
    /// or above the trigger was seen and none at or below release since, and that
    /// stays true while the sensor is stale OR gone — so going blind keeps the
    /// forced duty rather than falling to a lower floor. (DEC-190 dropped a
    /// VANISHED sensor to 40 %; DEC-386 retired that.) Non-mutating: only
    /// [`Self::evaluate`], with a fresh reading, moves the latch.
    pub fn forced_output_pct(&self) -> u8 {
        self.forced_output_pct
    }
}

impl Default for ThermalSafetyRule {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configured trip point, read rather than restated.
    ///
    /// These tests used to hardcode `105.0`. When a trip-point move was trialled
    /// (D1 batch) **27 tests went red at once**, none of which was testing the
    /// number — they were testing the ladder, and had merely spelled the trigger
    /// out. That is DEC-292's defect (a threshold written out in many places) in
    /// its test-suite form, so it gets DEC-292's fix: derive from the constant,
    /// and express "just above"/"just below" as offsets from it.
    const TRIGGER: f64 = crate::constants::THERMAL_EMERGENCY_TRIGGER_C;

    #[test]
    fn normal_temp_no_override() {
        let mut rule = ThermalSafetyRule::new();
        assert_eq!(rule.evaluate(60.0), None);
        assert!(!rule.is_active());
    }

    #[test]
    fn trigger_at_the_configured_trip_point() {
        let mut rule = ThermalSafetyRule::new();
        assert_eq!(rule.evaluate(TRIGGER), Some(100));
        assert!(rule.is_active());
    }

    #[test]
    fn holds_at_100_while_above_release() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(TRIGGER); // trigger
        assert_eq!(rule.evaluate(90.0), Some(100)); // still hot
        assert!(rule.is_active());
    }

    /// DEC-386: release hands control straight back — no recovery rung. The
    /// release reading itself returns `None`, and so does every one after it.
    #[test]
    fn release_returns_control_to_the_profile_at_once() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(TRIGGER); // trigger
        assert_eq!(
            rule.evaluate(80.0),
            None,
            "the release reading forces nothing"
        );
        assert!(!rule.is_active());
        assert_eq!(rule.evaluate(70.0), None, "and nothing is owed after it");
    }

    #[test]
    fn retrigger_after_release() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(TRIGGER); // trigger
        rule.evaluate(80.0); // release

        // Heat up again
        assert_eq!(rule.evaluate(TRIGGER + 1.0), Some(100));
        assert!(rule.is_active());
    }

    #[test]
    fn does_not_trigger_at_104() {
        let mut rule = ThermalSafetyRule::new();
        assert_eq!(rule.evaluate(TRIGGER - 0.1), None);
        assert!(!rule.is_active());
    }

    #[test]
    fn does_not_release_at_81() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(TRIGGER); // trigger
        assert_eq!(rule.evaluate(81.0), Some(100)); // still above 80
        assert!(rule.is_active());
    }

    #[test]
    fn oscillation_at_trigger_boundary_stays_active() {
        // Once triggered, temp oscillating near the trigger boundary
        // must NOT release — the hysteresis gap between trigger and release
        // keeps the override locked until temp actually drops to 80°C.
        let mut rule = ThermalSafetyRule::new();

        // Cross the trigger threshold
        assert_eq!(rule.evaluate(TRIGGER), Some(100));
        assert!(rule.is_active());

        // Oscillate just below trigger — still far above release (80°C)
        assert_eq!(rule.evaluate(TRIGGER - 0.1), Some(100));
        assert!(rule.is_active());
        assert_eq!(rule.evaluate(TRIGGER + 0.1), Some(100));
        assert!(rule.is_active());
        assert_eq!(rule.evaluate(TRIGGER - 0.1), Some(100));
        assert!(rule.is_active());

        // Only releases when temp actually drops to the release threshold
        assert_eq!(rule.evaluate(80.0), None); // release → profile control
        assert!(!rule.is_active());
    }
}
