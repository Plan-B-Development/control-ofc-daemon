//! PWM conversion utilities shared across subsystems.
//!
//! Standard sysfs PWM range is 0–255; the GUI and profiles use 0–100%.
//! These functions provide consistent rounding across all write and read paths.

/// Convert a PWM percent (0–100) to a raw PWM value (0–255).
pub fn percent_to_raw(percent: u8) -> u8 {
    ((percent as u16 * 255 + 50) / 100) as u8
}

/// Convert a raw PWM value (0–255) back to percent (0–100).
pub fn raw_to_percent(raw: u8) -> u8 {
    ((raw as u16 * 100 + 127) / 255) as u8
}

/// Is a `pwm_enable` of 0 here the driver's **"fan at full speed"** alias
/// rather than a firmware reclaim?
///
/// Standard hwmon semantics give `pwmN_enable == 0` the meaning *"no control —
/// fan runs at full speed"*. Several drivers therefore **synthesise** that value
/// from the duty register instead of reporting the mode the daemon actually set,
/// so a fully successful manual write of 100% reads back as `enable=0` and every
/// naive `enable != 1` test calls it a BIOS reclaim.
///
/// `it87` is the case this was found on (`it87.c:3612`, upstream 349.c567739):
///
/// ```text
/// if ((!has_fanctl_onoff(data) || nr >= 3) &&
///     data->pwm_duty[nr] == pwm_to_reg(data, 0xff))
///         return 0;                       /* Full speed */
/// ```
///
/// **Either half of that disjunction is sufficient**, which makes the reach
/// wider than the chip that exposed it:
///
/// * IT8696E has no `FEAT_FANCTL_ONOFF`, so **every** header is affected. This
///   is the X870E AORUS MASTER's primary chip.
/// * IT87952E *does* have the feature — and `nr >= 3` still catches its `pwm4`,
///   `pwm5` and `pwm6`.
/// * Consequently **any** ITE header at index ≥ 3 is affected regardless of the
///   chip's feature bits.
///
/// That is precisely why this discriminator carries **no chip table**: a table
/// keyed on `FEAT_FANCTL_ONOFF` would look correct and silently miss `pwm4+` on
/// every chip that has the feature. See DEC-326 and register row `HOST-a`.
///
/// # The test, and why it is safe in direction
///
/// Returns `true` only when the mode is exactly `0` **and** the duty register
/// still holds full scale (255 raw / 100%) **and** that is the duty we asked
/// for. A genuine reclaim to *automatic* reports mode `2`, so it is unaffected.
/// The one state this cannot distinguish is a genuine firmware reclaim to
/// **full speed while we were commanding full speed** — and there the fan is
/// already at maximum, i.e. exactly where the command wanted it, so suppressing
/// the reclaim response cannot reduce cooling. The moment anything commands a
/// duty below 100%, the readback no longer matches and reclaim detection
/// resumes on the very next observation.
///
/// Pass `readback_pct` as the value read back from `pwmN` *after* the write.
/// `None` for either optional argument means "not observed", which is never a
/// full-speed alias.
///
/// # Tolerance — this is percent, so it is ONE RAW STEP wider than the kernel
///
/// The kernel condition tests the duty register for exactly `0xff`. This
/// predicate takes a **percent**, and `raw_to_percent(254) == 100` as well as
/// `raw_to_percent(255) == 100`, so a duty of **254** also satisfies it. That is
/// deliberate rather than overlooked: percent is the only unit available at all
/// four call sites (`HwmonFanState` carries no raw duty), and tightening only
/// the sites that happen to have raw would give the same rule two different
/// meanings — the exact "two arguments derived from different sources" trap
/// recorded in `CLAUDE.md § Hard-won lessons`.
///
/// The residual case is a non-ITE driver reporting mode `0` at a duty of 254
/// while the daemon commanded 100%. It is exempted, and the safety argument is
/// unchanged: 254/255 is 99.6% — the fan is at maximum for every practical
/// purpose, so no cooling decision differs. What is lost is the same reclaim
/// *evidence* recorded in register row `326-a`.
pub fn is_full_speed_alias(
    requested_pct: u8,
    readback_pct: Option<u8>,
    pwm_enable: Option<u8>,
) -> bool {
    pwm_enable == Some(0) && requested_pct == 100 && readback_pct == Some(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_to_raw_boundaries() {
        assert_eq!(percent_to_raw(0), 0);
        assert_eq!(percent_to_raw(100), 255);
        assert_eq!(percent_to_raw(50), 128);
    }

    #[test]
    fn raw_to_percent_boundaries() {
        assert_eq!(raw_to_percent(0), 0);
        assert_eq!(raw_to_percent(255), 100);
        assert_eq!(raw_to_percent(128), 50);
    }

    #[test]
    fn roundtrip_percent() {
        for pct in 0..=100u8 {
            let raw = percent_to_raw(pct);
            let back = raw_to_percent(raw);
            assert!(
                back.abs_diff(pct) <= 1,
                "roundtrip failed for {pct}%: raw={raw}, back={back}%"
            );
        }
    }

    #[test]
    fn full_speed_alias_needs_all_three_conditions() {
        // The real signature: mode 0, we asked for 100%, duty still reads 100%.
        assert!(is_full_speed_alias(100, Some(100), Some(0)));

        // A genuine reclaim to AUTOMATIC reports mode 2 and is never suppressed.
        assert!(!is_full_speed_alias(100, Some(100), Some(2)));

        // Mode 0 at any duty we did not command is a real loss of control.
        assert!(!is_full_speed_alias(100, Some(40), Some(0)));
        assert!(!is_full_speed_alias(40, Some(40), Some(0)));
        assert!(!is_full_speed_alias(40, Some(100), Some(0)));

        // Not observed is never an alias.
        assert!(!is_full_speed_alias(100, None, Some(0)));
        assert!(!is_full_speed_alias(100, Some(100), None));
    }

    #[test]
    fn full_speed_alias_is_unreachable_below_full_scale() {
        // The kernel only synthesises mode 0 at `pwm_to_reg(0xff)`. Nothing
        // below full scale may be suppressed, whatever the readback says —
        // this is the half that keeps a real reclaim detectable.
        for pct in 0..100u8 {
            assert!(
                !is_full_speed_alias(pct, Some(pct), Some(0)),
                "{pct}% must never be treated as the full-speed alias"
            );
        }
        assert!(is_full_speed_alias(100, Some(100), Some(0)));
    }
}
