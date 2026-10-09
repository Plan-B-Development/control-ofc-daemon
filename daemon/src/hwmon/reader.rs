//! Read temperature values from hwmon sysfs.

use std::path::Path;
use std::time::SystemTime;

use crate::error::HwmonError;
use crate::hwmon::types::{SensorDescriptor, SensorReading};
use crate::hwmon::util::sanitize_f64;

/// Lower bound of a plausible sensor reading. Below this the value is treated as
/// a hardware/driver fault rather than a temperature (DEC-288).
const PLAUSIBLE_MIN_C: f64 = -50.0;
/// Upper bound of a plausible sensor reading. No CPU survives to this
/// temperature — hardware thermal protection fires well below it on every
/// platform — so anything above it is a fault, not a measurement (DEC-288).
///
/// **Do not restate a THERMTRIP number here (D1-l).** This comment used to say
/// "hardware `THERMTRIP` fires around 125°C", and that figure cannot be sourced:
/// the public Zen 4 (55901) and Zen 5 (57238) PPRs contain **no** numeric
/// THERMTRIP value — AMD defers it to a non-public thermal datasheet — and the
/// figure Intel *does* publish is **higher**, ~130°C ("the processor will stop
/// all executions when the junction temperature exceeds approximately 130 C",
/// 655258 Rev 011 / 743844 Rev 015). The constant below is 250°C, far above
/// either, so nothing downstream was ever wrong — this was a wrong *reason*
/// attached to a right value, which is the shape that survives review for years
/// and then gets cited as evidence. It was, once.
///
/// Deliberately WIDER than `discovery::THRESHOLD_MAX_C` (200°C) and not to be
/// "unified" with it: that constant bounds a declared *threshold* attribute
/// (`tempN_crit`), this one bounds a live *reading*. They answer different
/// questions and their values are independent.
///
/// **This bound cannot catch every fault, and still does not.** Garbage that
/// lands between [`crate::constants::THERMAL_EMERGENCY_TRIGGER_C`] and this
/// bound — e.g. a saturated 8-bit thermistor reading 127°C — is
/// indistinguishable from a real over-temperature *here* and would still latch
/// the emergency. Widening the check is not the answer: temperatures at and
/// above the trigger are legitimate readings on real hardware, so no
/// reader-level bound can separate a real over-temperature from a stuck one.
///
/// DEC-294 removed the one instance of this that is kernel-documented and
/// reachable — an ASUS NCT6776F `CPUTIN`, which is frequently unconnected and
/// reports a plausible-looking constant — but it did so **at classification,
/// not here**: that sensor is no longer a `CpuTemp`, so it never reaches the
/// ladder. The general class is an accepted posture, not an open gap (`TS-s`,
/// DEC-400, the user's decision). The ladder does not bound its latch: it has
/// no plausibility gate, no maximum latch time and no sibling cross-check,
/// because a safety function that has tripped stays tripped until its reset
/// (IEC 61511-1 11.2.7). Here the reset is a fresh reading at or below release.
/// A stuck reading therefore fails loud, at 100 %. The remedy for a sensor
/// known to do that is at classification, as DEC-294's was.
const PLAUSIBLE_MAX_C: f64 = 250.0;

/// Read a temperature value from a `temp*_input` sysfs file.
///
/// The kernel reports temperatures in millidegrees Celsius (e.g. 45000 = 45.0°C).
///
/// An implausible value is an **error**, not a clamped reading (DEC-288) — see the
/// rejection below for why that distinction is safety-critical.
pub fn read_temp(descriptor: &SensorDescriptor) -> Result<SensorReading, HwmonError> {
    if let Some(enable_path) = disabled_sensor_enable_path(descriptor) {
        return Err(HwmonError::ReadError {
            path: enable_path,
            message: "temperature sensor disabled (enable = 0); the driver keeps returning \
                      its last value, so the reading is not used"
                .into(),
        });
    }

    let path = Path::new(&descriptor.input_path);
    let raw = std::fs::read_to_string(path).map_err(|e| HwmonError::ReadError {
        path: descriptor.input_path.clone(),
        message: e.to_string(),
    })?;

    let millidegrees: i64 =
        raw.trim()
            .parse()
            .map_err(|e: std::num::ParseIntError| HwmonError::ReadError {
                path: descriptor.input_path.clone(),
                message: format!("invalid temperature value '{raw}': {e}"),
            })?;

    let mut value_c = millidegrees as f64 / 1000.0;

    // Sanity bounds: a value outside [-50, 250]°C is almost certainly garbage.
    //
    // REJECT it rather than clamping it (DEC-288). Clamping produced a
    // valid-looking 250.0°C, and every consumer downstream believed it:
    // `hottest_cpu_reading` max-reduces across CpuTemp sensors, so one broken
    // sensor outranked every healthy one, and `ThermalSafetyRule` latches at
    // `THERMAL_EMERGENCY_TRIGGER_C` but only releases at
    // `THERMAL_EMERGENCY_RELEASE_C` — which 250 never reaches. The result
    // was a permanent, unrecoverable thermal emergency: every fan forced to 100%
    // until reboot, from a reading this very code had already identified as
    // garbage. It was also unquarantinable, because DEC-193 evicts a sensor that
    // fails to *read*, and a clamped read is a success.
    //
    // An `Err` routes the sensor into that DEC-193 quarantine instead: streak ->
    // one re-discovery probe -> quarantined and logged once, surfaced as
    // `unavailable_sensors[]` on /status + /poll, evicted from the live set, and
    // un-quarantined automatically the moment it reads sanely again. So a
    // transient glitch costs nothing and a persistently broken sensor becomes
    // *visible* rather than silently deafening. With no CpuTemp sensor left, the
    // adjudicated absent-sensor path (DEC-132/190) applies its 40% floor, which
    // is recoverable; the old behaviour was not.
    //
    // This mirrors `discovery::read_temp_attr_c`, which already drops implausible
    // threshold values instead of clamping them.
    //
    // No `log::warn!` here: at 1 Hz a per-tick log is exactly the spam DEC-193
    // was built to collapse. The tracker owns the logging, once per transition.
    if !(PLAUSIBLE_MIN_C..=PLAUSIBLE_MAX_C).contains(&value_c) {
        return Err(HwmonError::ReadError {
            path: descriptor.input_path.clone(),
            message: format!(
                "implausible temperature {value_c:.1}°C outside [{PLAUSIBLE_MIN_C:.0}, {PLAUSIBLE_MAX_C:.0}]°C"
            ),
        });
    }

    // Guard against NaN/Infinity from upstream calculation errors
    value_c = sanitize_f64(value_c);

    Ok(SensorReading {
        id: descriptor.id.clone(),
        kind: descriptor.kind,
        label: descriptor.label.clone(),
        value_c,
        timestamp: SystemTime::now(),
        source: descriptor.source,
        chip_name: descriptor.chip_name.clone(),
        temp_type: descriptor.temp_type,
        thresholds: descriptor.thresholds.clone(),
    })
}

/// Chips whose `tempN_input` keeps answering after the sensor is switched off.
///
/// `spd5118` (DDR5 SPD hub): the read path never checks the TS_DISABLE bit, so
/// with `temp1_enable = 0` the hub stops converting and `temp1_input` returns
/// the last register value with no error — a silently frozen reading (DEC-491).
/// `temp1_enable` reads the regmap cache (TEMP_CONFIG is not a volatile
/// register), so checking it every tick costs no SMBus traffic.
const ENABLE_UNCHECKED_CHIPS: &[&str] = &["spd5118"];

/// The `tempN_enable` path of a sensor that reports itself disabled, if any.
///
/// Only an explicit `0` disables. A missing or unreadable attribute leaves the
/// reading as it was before this check existed: the value is still read, and a
/// real fault there fails on its own.
fn disabled_sensor_enable_path(descriptor: &SensorDescriptor) -> Option<String> {
    if !ENABLE_UNCHECKED_CHIPS.contains(&descriptor.chip_name.as_str()) {
        return None;
    }
    let enable_path = format!("{}_enable", descriptor.input_path.strip_suffix("_input")?);
    let raw = std::fs::read_to_string(&enable_path).ok()?;
    (raw.trim() == "0").then_some(enable_path)
}

/// Read all sensors from a list of descriptors.
///
/// Sensors that fail to read are logged and skipped (not fatal).
pub fn read_all(descriptors: &[SensorDescriptor]) -> Vec<Result<SensorReading, HwmonError>> {
    descriptors.iter().map(read_temp).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::types::{SensorKind, SensorSource};
    use std::fs;

    fn make_descriptor(input_path: &str) -> SensorDescriptor {
        SensorDescriptor {
            id: "hwmon:test:nodev:temp1".into(),
            kind: SensorKind::CpuTemp,
            label: "Tctl".into(),
            source: SensorSource::Hwmon,
            input_path: input_path.into(),
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }
    }

    #[test]
    fn read_temp_normal() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("temp1_input");
        fs::write(&input, "45000\n").unwrap();

        let desc = make_descriptor(input.to_str().unwrap());
        let reading = read_temp(&desc).unwrap();

        assert_eq!(reading.id, "hwmon:test:nodev:temp1");
        assert!((reading.value_c - 45.0).abs() < f64::EPSILON);
        assert_eq!(reading.kind, SensorKind::CpuTemp);
        assert_eq!(reading.label, "Tctl");
    }

    #[test]
    fn read_temp_negative() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("temp1_input");
        fs::write(&input, "-5000\n").unwrap();

        let desc = make_descriptor(input.to_str().unwrap());
        let reading = read_temp(&desc).unwrap();

        assert!((reading.value_c - (-5.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn read_temp_fractional() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("temp1_input");
        fs::write(&input, "45500\n").unwrap();

        let desc = make_descriptor(input.to_str().unwrap());
        let reading = read_temp(&desc).unwrap();

        assert!((reading.value_c - 45.5).abs() < f64::EPSILON);
    }

    #[test]
    fn read_temp_missing_file() {
        let desc = make_descriptor("/nonexistent/temp1_input");
        let result = read_temp(&desc);
        assert!(result.is_err());
        match result.unwrap_err() {
            HwmonError::ReadError { path, .. } => {
                assert_eq!(path, "/nonexistent/temp1_input");
            }
            other => panic!("expected ReadError, got {other:?}"),
        }
    }

    #[test]
    fn read_temp_non_numeric() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("temp1_input");
        fs::write(&input, "not_a_number\n").unwrap();

        let desc = make_descriptor(input.to_str().unwrap());
        let result = read_temp(&desc);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid temperature"));
    }

    /// DEC-288: an implausibly HIGH reading is an error, not a clamped 250.0°C.
    /// `i32::MAX` millidegrees is the canonical misprobed-chip value.
    #[test]
    fn read_temp_rejects_an_implausibly_high_value() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("temp1_input");
        fs::write(&input, "2147483647\n").unwrap();

        let desc = make_descriptor(input.to_str().unwrap());
        let result = read_temp(&desc);

        assert!(
            result.is_err(),
            "an implausible reading must not be clamped into a valid-looking value"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("implausible"), "{msg}");
        // The message reaches users as `unavailable_sensors[].reason`, so it must
        // read cleanly — no doubled spaces from a wrapped format literal.
        assert!(!msg.contains("  "), "reason has stray padding: {msg:?}");
        assert!(msg.contains("[-50, 250]\u{b0}C"), "{msg}");
    }

    /// DEC-288: the low bound rejects too — a sub -50°C reading is equally a fault.
    #[test]
    fn read_temp_rejects_an_implausibly_low_value() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("temp1_input");
        fs::write(&input, "-60000\n").unwrap();

        let desc = make_descriptor(input.to_str().unwrap());
        assert!(read_temp(&desc).is_err());
    }

    /// DEC-288: the bounds are INCLUSIVE. Without this, "reject implausible
    /// values" could quietly become "reject anything near the edge", which would
    /// discard legitimate readings — a real risk for the -50°C end on a cold
    /// boot, and for chips that genuinely report high case temperatures.
    #[test]
    fn read_temp_accepts_both_plausible_bounds_exactly() {
        for (milli, expected) in [("250000", 250.0_f64), ("-50000", -50.0_f64)] {
            let tmp = tempfile::tempdir().unwrap();
            let input = tmp.path().join("temp1_input");
            fs::write(&input, format!("{milli}\n")).unwrap();

            let desc = make_descriptor(input.to_str().unwrap());
            let reading = read_temp(&desc)
                .unwrap_or_else(|e| panic!("{expected}°C is in range but was rejected: {e}"));
            assert!((reading.value_c - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn read_all_mixed_results() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("temp1_input");
        fs::write(&good, "50000\n").unwrap();

        let descs = vec![
            make_descriptor(good.to_str().unwrap()),
            make_descriptor("/nonexistent/temp2_input"),
        ];

        let results = read_all(&descs);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
    }

    /// A DDR5 SPD-hub descriptor reading `temp1_input` in `dir`.
    fn spd5118_descriptor(dir: &Path) -> SensorDescriptor {
        SensorDescriptor {
            id: "hwmon:spd5118:21-0051:temp1".into(),
            kind: SensorKind::MbTemp,
            label: "temp1".into(),
            source: SensorSource::Hwmon,
            input_path: dir.join("temp1_input").to_str().unwrap().into(),
            chip_name: "spd5118".into(),
            temp_type: None,
            thresholds: None,
        }
    }

    /// DEC-491: an spd5118 sensor with `temp1_enable = 0` keeps answering with
    /// its last value. It must read as a failure (so DEC-193 quarantines it and
    /// it shows unavailable), never as a frozen temperature.
    #[test]
    fn disabled_spd5118_sensor_is_a_read_failure_not_a_frozen_value() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("temp1_input"), "37500\n").unwrap();
        fs::write(tmp.path().join("temp1_enable"), "0\n").unwrap();

        let err = read_temp(&spd5118_descriptor(tmp.path())).unwrap_err();
        let HwmonError::ReadError { path, message } = &err else {
            panic!("expected ReadError, got {err:?}");
        };
        assert!(path.ends_with("temp1_enable"), "{path}");
        assert!(message.contains("disabled"), "{message}");
    }

    /// The opposite branch: enabled, or no enable attribute at all, reads the
    /// value exactly as before.
    #[test]
    fn enabled_or_unreported_spd5118_sensor_reads_normally() {
        for enable in [Some("1\n"), None] {
            let tmp = tempfile::tempdir().unwrap();
            fs::write(tmp.path().join("temp1_input"), "37500\n").unwrap();
            if let Some(v) = enable {
                fs::write(tmp.path().join("temp1_enable"), v).unwrap();
            }
            let reading = read_temp(&spd5118_descriptor(tmp.path()))
                .unwrap_or_else(|e| panic!("enable={enable:?} must read: {e}"));
            assert!((reading.value_c - 37.5).abs() < f64::EPSILON);
        }
    }

    /// Only the chips known to ignore their enable bit are checked: another
    /// driver's `temp1_enable = 0` is that driver's business (it reports the
    /// state itself), and the value is read as before.
    #[test]
    fn enable_attribute_is_checked_only_for_listed_chips() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("temp1_input");
        fs::write(&input, "45000\n").unwrap();
        fs::write(tmp.path().join("temp1_enable"), "0\n").unwrap();
        assert!(read_temp(&make_descriptor(input.to_str().unwrap())).is_ok());
    }
}
