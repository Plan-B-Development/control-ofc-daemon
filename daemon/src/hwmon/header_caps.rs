//! Header capability audit — read-only driver introspection (AIO-MB Phase 4,
//! DEC-316).
//!
//! What a PWM header's *driver* exposes, as opposed to what policy says it may
//! do. Those are two different meanings of the word "capability" and they are
//! deliberately in two different modules: nothing here is a safety input, and
//! nothing in [`crate::hwmon::device_policy`] is read from hardware.
//!
//! # Read-only, by decision
//!
//! Every value here comes from a `read(2)`. There is **no probe**: nothing in
//! this module writes to sysfs, so it adds no hardware-write path and needs no
//! verify slot, lease, deadman or pump-floor protection. That is what keeps
//! Phase 4 out of the `[SAFETY]` set.
//!
//! # Absent is not zero
//!
//! Every field is an `Option` and stays `None` when the attribute is missing.
//! This matters more than it looks: `fanN_pulses` is an `nct6775` attribute
//! that `it87` does not implement, and the validation board for this programme
//! is an `it8696`. Reporting `0` pulses per revolution there would be a
//! fabricated measurement; reporting nothing is the truth.
//!
//! Measured on the validation board (`it8696`, 2026-09-03): `pwmN_freq`,
//! `fanN_min` and `fanN_alarm` are present; `fanN_max`, `fanN_pulses` and
//! `pwmN_mode` are not.

use std::path::Path;

/// Read-only capability facts about one PWM header.
///
/// All static — read once at discovery. The one genuinely *dynamic* signal a
/// header carries, `fanN_alarm`, is deliberately not here: it rides the 1 Hz
/// poll instead (see [`alarm_path`]), because a value refreshed only when the
/// GUI happens to re-fetch headers would read "clear" while a fan is failing.
///
/// `pwmN_enable`'s current *value* is not here either, for a sharper version of
/// the same reason: the daemon writes that attribute itself when it takes a
/// header over, so a discovery-time snapshot would report the pre-takeover mode
/// for the whole process lifetime. It rides the poll as
/// `FanEntry::pwm_enable_mode`. Whether the attribute *exists* is static and
/// stays on the descriptor as `supports_enable`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeaderCaps {
    /// PWM base frequency in Hz, from `pwmN_freq`.
    ///
    /// Low-frequency PWM is the usual cause of a header that only responds over
    /// a narrow duty band — the `it87` docs recommend lowering it when a fan is
    /// controllable only at very small PWM values.
    pub pwm_freq_hz: Option<u32>,
    /// Low RPM alarm threshold, from `fanN_min`.
    pub rpm_min_threshold: Option<u16>,
    /// High RPM threshold, from `fanN_max`. Absent on most Super-I/O chips.
    pub rpm_max_threshold: Option<u16>,
    /// Tachometer pulses per revolution, from `fanN_pulses`.
    ///
    /// Absent on `it87`. Where present it is what makes a reported RPM
    /// interpretable — a pump reporting half or double its real speed is
    /// usually a pulses mismatch, not a fault.
    pub tach_pulses_per_rev: Option<u8>,
}

/// Read one `hwmon` attribute and parse it, returning `None` for absent,
/// unreadable, or unparseable — the three cases a caller must treat alike.
fn read_attr<T: std::str::FromStr>(hwmon_dir: &Path, name: &str) -> Option<T> {
    let path = hwmon_dir.join(name);
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().parse::<T>().ok(),
        Err(e) => {
            log::debug!(
                "header capability audit: {} unreadable: {e}",
                path.display()
            );
            None
        }
    }
}

/// Audit one header's driver-reported capabilities. Pure reads; never writes.
pub fn read_header_caps(hwmon_dir: &Path, pwm_index: u8) -> HeaderCaps {
    HeaderCaps {
        pwm_freq_hz: read_attr(hwmon_dir, &format!("pwm{pwm_index}_freq")),
        rpm_min_threshold: read_attr(hwmon_dir, &format!("fan{pwm_index}_min")),
        rpm_max_threshold: read_attr(hwmon_dir, &format!("fan{pwm_index}_max")),
        tach_pulses_per_rev: read_attr(hwmon_dir, &format!("fan{pwm_index}_pulses")),
    }
}

/// Path to this header's `fanN_alarm`, when the driver exposes one.
///
/// Returned as a path rather than a value because the alarm is *state*: it is
/// sampled on the 1 Hz poll alongside RPM and published on `/poll`, not frozen
/// into the discovery snapshot.
pub fn alarm_path(hwmon_dir: &Path, pwm_index: u8) -> Option<String> {
    let p = hwmon_dir.join(format!("fan{pwm_index}_alarm"));
    p.exists().then(|| p.display().to_string())
}

/// The `pwmN_enable` values a chip's driver accepts.
///
/// **Nothing in sysfs reports this** — the kernel hwmon ABI exposes only the
/// current value, and establishing the supported *set* by experiment would mean
/// writing to `pwmN_enable`, which this phase deliberately does not do. So it
/// comes from driver knowledge, and an empty slice means "unknown": a caller
/// must render that as unknown and must never present it as "no modes
/// supported".
///
/// Family resolution reuses [`crate::hwmon::chip_db::expected_driver_for_chip`]
/// rather than re-deriving chip-name prefixes here. That matters for Nuvoton:
/// `nct6683`/`nct6686`/`nct6687` are a different driver from the `nct6775` line
/// with different semantics, and a local `starts_with("nct6")` would silently
/// lump them together.
///
/// Sources:
/// - `it87` — `drivers/hwmon/it87.c::set_pwm_enable` rejects anything outside
///   `0..=2` with `-EINVAL` (read 2026-09-03). 0 = on/off or full speed,
///   1 = manual, 2 = automatic (SmartGuardian).
/// - `nct6775` — `Documentation/hwmon/nct6775.rst`: 0 = disabled (full speed),
///   1 = manual, 2 = Thermal Cruise, 3 = Fan Speed Cruise, 4 = Smart Fan III
///   (NCT6775F only), 5 = Smart Fan IV.
pub fn supported_pwm_enable_modes(chip_name: &str) -> &'static [u8] {
    match crate::hwmon::chip_db::expected_driver_for_chip(chip_name) {
        "it87" => &[0, 1, 2],
        "nct6775" => &[0, 1, 2, 3, 4, 5],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("pwm1"), "128\n").unwrap();
        fs::write(dir.join("pwm1_freq"), "23437\n").unwrap();
        fs::write(dir.join("fan1_min"), "300\n").unwrap();
        fs::write(dir.join("fan1_alarm"), "0\n").unwrap();
        tmp
    }

    /// The whole point of the audit: read what is there.
    #[test]
    fn reads_the_attributes_a_driver_exposes() {
        let tmp = fixture();
        let caps = read_header_caps(tmp.path(), 1);
        assert_eq!(caps.pwm_freq_hz, Some(23437));
        assert_eq!(caps.rpm_min_threshold, Some(300));
    }

    /// **Absent is `None`, never `0`.** This is the `it8696` case: the board
    /// this programme validates against exposes no `fanN_pulses` and no
    /// `fanN_max`, and a fabricated zero there would be a measurement the
    /// hardware never made.
    #[test]
    fn an_attribute_the_driver_lacks_reports_none_not_zero() {
        let tmp = fixture();
        let caps = read_header_caps(tmp.path(), 1);
        assert_eq!(
            caps.tach_pulses_per_rev, None,
            "it87 exposes no fanN_pulses"
        );
        assert_eq!(caps.rpm_max_threshold, None, "it87 exposes no fanN_max");
        assert!(alarm_path(tmp.path(), 1).is_some());
        assert_eq!(
            alarm_path(tmp.path(), 2),
            None,
            "no fan2_alarm in the fixture"
        );
    }

    /// A garbage attribute is indistinguishable from an absent one to a caller,
    /// and must not panic or produce a wrong number.
    #[test]
    fn an_unparseable_attribute_reports_none() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("pwm1_freq"), "not-a-number\n").unwrap();
        fs::write(tmp.path().join("fan1_min"), "\n").unwrap();
        let caps = read_header_caps(tmp.path(), 1);
        assert_eq!(caps.pwm_freq_hz, None);
        assert_eq!(caps.rpm_min_threshold, None);
    }

    /// An empty header directory audits cleanly to "nothing known".
    #[test]
    fn a_driver_exposing_nothing_audits_to_all_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_header_caps(tmp.path(), 1), HeaderCaps::default());
    }

    /// Cited values, and the Nuvoton family split that a local prefix match
    /// would have got wrong.
    #[test]
    fn supported_modes_come_from_the_driver_family() {
        // it87.c rejects > 2.
        assert_eq!(supported_pwm_enable_modes("it8696"), &[0, 1, 2]);
        assert_eq!(supported_pwm_enable_modes("it8620"), &[0, 1, 2]);
        // nct6775.rst documents 0..=5.
        assert_eq!(supported_pwm_enable_modes("nct6798"), &[0, 1, 2, 3, 4, 5]);
        assert_eq!(supported_pwm_enable_modes("nct6775"), &[0, 1, 2, 3, 4, 5]);
        // A DIFFERENT driver despite the nct6 prefix — monitoring-only mainline
        // and the out-of-tree nct6687d. Claiming Smart Fan IV here would be a
        // lie about hardware the daemon cannot control that way.
        assert!(supported_pwm_enable_modes("nct6683").is_empty());
        assert!(supported_pwm_enable_modes("nct6687").is_empty());
        // Unknown chips report unknown, not "none supported".
        assert!(supported_pwm_enable_modes("k10temp").is_empty());
        assert!(supported_pwm_enable_modes("").is_empty());
    }
}
