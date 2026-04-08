//! CPU Tctl emergency thermal safety rule.
//!
//! Single latched rule: if CPU Tctl reaches 105°C, force all fans to 100%.
//! Hold until Tctl drops to 80°C, then apply a one-cycle 60% recovery floor
//! before returning control to the active profile.

/// Emergency thermal safety override for CPU temperature.
///
/// Uses hysteresis (trigger at 105°C, release at 80°C) to prevent flapping.
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
            trigger_temp_c: 105.0,
            release_temp_c: 80.0,
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
                "THERMAL EMERGENCY: CPU Tctl {:.1}°C >= {}°C — forcing all fans to {}%",
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

        // One-cycle recovery floor after emergency release
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
}

impl Default for ThermalSafetyRule {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_temp_no_override() {
        let mut rule = ThermalSafetyRule::new();
        assert_eq!(rule.evaluate(60.0), None);
        assert!(!rule.is_active());
    }

    #[test]
    fn trigger_at_105() {
        let mut rule = ThermalSafetyRule::new();
        assert_eq!(rule.evaluate(105.0), Some(100));
        assert!(rule.is_active());
    }

    #[test]
    fn holds_at_100_while_above_release() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(105.0); // trigger
        assert_eq!(rule.evaluate(90.0), Some(100)); // still hot
        assert!(rule.is_active());
    }

    #[test]
    fn releases_at_80_with_recovery() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(105.0); // trigger
        assert_eq!(rule.evaluate(80.0), Some(60)); // release + recovery
        assert!(!rule.is_active());
    }

    #[test]
    fn recovery_lasts_one_cycle() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(105.0); // trigger
        rule.evaluate(80.0); // release → recovery
        assert_eq!(rule.evaluate(70.0), Some(60)); // one-cycle recovery floor
        assert_eq!(rule.evaluate(70.0), None); // back to normal
    }

    #[test]
    fn retrigger_after_recovery() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(105.0); // trigger
        rule.evaluate(80.0); // release
        rule.evaluate(70.0); // recovery
        rule.evaluate(70.0); // normal

        // Heat up again
        assert_eq!(rule.evaluate(106.0), Some(100));
        assert!(rule.is_active());
    }

    #[test]
    fn does_not_trigger_at_104() {
        let mut rule = ThermalSafetyRule::new();
        assert_eq!(rule.evaluate(104.9), None);
        assert!(!rule.is_active());
    }

    #[test]
    fn does_not_release_at_81() {
        let mut rule = ThermalSafetyRule::new();
        rule.evaluate(105.0); // trigger
        assert_eq!(rule.evaluate(81.0), Some(100)); // still above 80
        assert!(rule.is_active());
    }

    #[test]
    fn oscillation_at_trigger_boundary_stays_active() {
        // Once triggered, temp oscillating near the trigger boundary (104.9–105.1)
        // must NOT release — the 25°C hysteresis gap (trigger=105, release=80)
        // keeps the override locked until temp actually drops to 80°C.
        let mut rule = ThermalSafetyRule::new();

        // Cross the trigger threshold
        assert_eq!(rule.evaluate(105.0), Some(100));
        assert!(rule.is_active());

        // Oscillate just below trigger — still far above release (80°C)
        assert_eq!(rule.evaluate(104.9), Some(100));
        assert!(rule.is_active());
        assert_eq!(rule.evaluate(105.1), Some(100));
        assert!(rule.is_active());
        assert_eq!(rule.evaluate(104.9), Some(100));
        assert!(rule.is_active());

        // Only releases when temp actually drops to the release threshold
        assert_eq!(rule.evaluate(80.0), Some(60)); // release → recovery
        assert!(!rule.is_active());
    }
}
