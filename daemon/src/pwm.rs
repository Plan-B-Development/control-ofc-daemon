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
}
