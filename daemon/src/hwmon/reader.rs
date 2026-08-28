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
/// temperature — hardware `THERMTRIP` fires around 125°C — so anything above it
/// is a fault, not a measurement (DEC-288).
///
/// Deliberately WIDER than `discovery::THRESHOLD_MAX_C` (200°C) and not to be
/// "unified" with it: that constant bounds a declared *threshold* attribute
/// (`tempN_crit`), this one bounds a live *reading*. They answer different
/// questions and their values are independent.
///
/// **This bound cannot catch every fault.** Garbage that lands inside
/// [105, 250]°C — e.g. a saturated 8-bit thermistor reading 127°C — is
/// indistinguishable from a real over-temperature here and still latches the
/// emergency. Widening the check is not the answer: 105-125°C are legitimate
/// readings. Tracked as `AUD-x` in `DECISIONS_OPEN_ITEMS.md`.
const PLAUSIBLE_MAX_C: f64 = 250.0;

/// Read a temperature value from a `temp*_input` sysfs file.
///
/// The kernel reports temperatures in millidegrees Celsius (e.g. 45000 = 45.0°C).
///
/// An implausible value is an **error**, not a clamped reading (DEC-288) — see the
/// rejection below for why that distinction is safety-critical.
pub fn read_temp(descriptor: &SensorDescriptor) -> Result<SensorReading, HwmonError> {
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
    // >=105°C but only releases at <=80°C — which 250 never reaches. The result
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
}
