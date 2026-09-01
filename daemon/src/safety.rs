//! CPU Tctl emergency thermal safety rule.
//!
//! Single latched rule: at [`crate::constants::THERMAL_EMERGENCY_TRIGGER_C`],
//! force all OpenFan channels and writable hwmon headers to 100%. Hold until
//! Tctl drops to [`crate::constants::THERMAL_EMERGENCY_RELEASE_C`], then hold
//! 60% for two cycles (the release cycle that drops out of emergency, plus one
//! more) before returning control to the active profile. The thresholds are
//! deliberately not restated here — DEC-292 reduced them to one definition each
//! precisely because a doc that spells a threshold out drifts from it.
//!
//! **The 60% step is a REPLACEMENT, not a floor — this doc used to say "floor"
//! and that was wrong (`D1-j`).** Every value this rule returns reaches the
//! engine as `decision.forced_pct`, and the engine's forced branch calls
//! `force_all(pct)` and then `continue`s, skipping profile evaluation entirely.
//! So on release the fans are driven **to** 60%, not held **at or above** it: a
//! curve asking for 100% at that temperature is overridden downward for two
//! ticks, immediately after an excursion, while the CPU is still hot. Same
//! shape for the no-CPU-sensor duty.
//!
//! That is a real defect and it is **not fixed here** — correcting it means the
//! engine must evaluate the profile during a forced tick and write
//! `max(computed, forced)`, which cannot simply clamp the computed commands:
//! `force_all` reaches every writable header and every OpenFan channel
//! regardless of whether any control commands it, so clamping commands alone
//! would shrink the emergency's reach to controlled fans only — the v2.38.0 P1
//! shape. The correct form is a `force_all_with_floor(pct, &commands)` on
//! `SafetyWriteBackend`, which is a trait change across three backends and
//! interacts with the shared `BoundedWrite` invariant (DEC-289/298/299). It is
//! tracked as `D1-j` in `DECISIONS_OPEN_ITEMS.md` and deferred to its own
//! change, deliberately, rather than bolted onto a diff that already moves the
//! trip point.
//!
//! GPU fans are deliberately excluded from this rule (DEC-130): there is no
//! GPU emergency threshold. AMD PMFW firmware owns GPU thermal protection
//! (junction-temp throttling and firmware fan ramp) independently of OS fan
//! control.

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
    recovery_output_pct: u8,
    active: bool,
    recovery: bool,
}

impl ThermalSafetyRule {
    /// Create the default CPU Tctl emergency rule.
    pub fn new() -> Self {
        Self {
            trigger_temp_c: crate::constants::THERMAL_EMERGENCY_TRIGGER_C,
            release_temp_c: crate::constants::THERMAL_EMERGENCY_RELEASE_C,
            forced_output_pct: 100,
            recovery_output_pct: 60,
            active: false,
            recovery: false,
        }
    }

    /// Evaluate the rule against the current CPU Tctl temperature.
    ///
    /// Returns `Some(forced_pct)` if the override is active (fans should be forced),
    /// or `None` if normal profile control should proceed.
    pub fn evaluate(&mut self, tctl_c: f64) -> Option<u8> {
        // Check for trigger (not yet active)
        if !self.active && tctl_c >= self.trigger_temp_c {
            self.active = true;
            self.recovery = false;
            log::warn!(
                "THERMAL EMERGENCY: CPU Tctl {:.1}°C >= {}°C — forcing all OpenFan+hwmon fans to {}%",
                tctl_c,
                self.trigger_temp_c,
                self.forced_output_pct
            );
            return Some(self.forced_output_pct);
        }

        // While emergency is active
        if self.active {
            if tctl_c <= self.release_temp_c {
                // Temperature dropped below release threshold — exit emergency
                self.active = false;
                self.recovery = true;
                log::info!(
                    "Thermal emergency released: CPU Tctl {:.1}°C <= {}°C — recovery at {}%",
                    tctl_c,
                    self.release_temp_c,
                    self.recovery_output_pct
                );
                return Some(self.recovery_output_pct);
            }
            // Still above release threshold — hold at 100%
            return Some(self.forced_output_pct);
        }

        // Second of the two recovery-floor cycles: the first 60% tick was the
        // release return above (the release_temp crossing), this one completes
        // the floor before control returns to the profile — pinned by
        // `recovery_floor_spans_exactly_two_60pct_cycles`.
        if self.recovery {
            self.recovery = false;
            return Some(self.recovery_output_pct);
        }

        // Normal operation — no override
        None
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

    /// The output this rule is already holding, read **without** a temperature.
    ///
    /// [SAFETY] DEC-269. For the case where the CPU reading is *stale* rather
    /// than *absent*: the poll loop has stopped updating, but the last thing it
    /// told us still stands as evidence. A latched emergency means we saw at
    /// least the trigger and have never since seen the release point or below, and that remains
    /// true while we are blind — so the safe response to losing sight is to keep
    /// forcing what we were already forcing, not to fall back to a lower floor.
    ///
    /// Deliberately **non-mutating**, unlike [`Self::evaluate`]. A stale reading
    /// must not clear the latch, advance the two-cycle recovery counter, or
    /// trigger a new emergency — it is not evidence of anything *current*, only
    /// of what was last true.
    pub fn held_output_pct(&self) -> Option<u8> {
        if self.active {
            Some(self.forced_output_pct)
        } else if self.recovery {
            Some(self.recovery_output_pct)
        } else {
            None
        }
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

    #[test]
    fn releases_at_80_with_recovery() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(TRIGGER); // trigger
        assert_eq!(rule.evaluate(80.0), Some(60)); // release + recovery
        assert!(!rule.is_active());
    }

    #[test]
    fn recovery_lasts_one_cycle() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(TRIGGER); // trigger
        rule.evaluate(80.0); // release → recovery
        assert_eq!(rule.evaluate(70.0), Some(60)); // one-cycle recovery floor
        assert_eq!(rule.evaluate(70.0), None); // back to normal
    }

    #[test]
    fn recovery_floor_spans_exactly_two_60pct_cycles() {
        // Pins the module-doc invariant: after release, the 60% floor is held
        // for TWO cycles — the release cycle plus one recovery-floor cycle —
        // before control returns to the active profile.
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(TRIGGER); // trigger → emergency
        assert_eq!(rule.evaluate(80.0), Some(60)); // cycle 1: release at 60%
        assert_eq!(rule.evaluate(70.0), Some(60)); // cycle 2: recovery floor at 60%
        assert_eq!(rule.evaluate(70.0), None); // cycle 3: back to profile control
    }

    #[test]
    fn held_output_reports_what_the_rule_is_forcing_without_a_reading() {
        // DEC-269. This is what a stale reading holds onto, so it must track the
        // rule's real state — and must not mutate it.
        let mut rule = ThermalSafetyRule::new();
        assert_eq!(rule.held_output_pct(), None, "nothing forced at rest");

        rule.evaluate(TRIGGER + 1.0);
        assert_eq!(rule.held_output_pct(), Some(100), "latched");
        assert_eq!(rule.held_output_pct(), Some(100), "and it is idempotent");
        assert!(rule.is_active(), "reading it must not clear the latch");

        rule.evaluate(70.0); // release -> recovery
        assert_eq!(rule.held_output_pct(), Some(60), "recovery floor");
        // Reading it repeatedly must not consume the two-cycle recovery window.
        assert_eq!(rule.held_output_pct(), Some(60));
        assert_eq!(
            rule.evaluate(70.0),
            Some(60),
            "the second recovery cycle must still be owed — held_output_pct \
             advanced the state machine"
        );
        assert_eq!(rule.evaluate(70.0), None);
        assert_eq!(rule.held_output_pct(), None);
    }

    #[test]
    fn retrigger_after_recovery() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(TRIGGER); // trigger
        rule.evaluate(80.0); // release
        rule.evaluate(70.0); // recovery
        rule.evaluate(70.0); // normal

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
        assert_eq!(rule.evaluate(80.0), Some(60)); // release → recovery
        assert!(!rule.is_active());
    }
}
