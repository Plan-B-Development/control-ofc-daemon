//! Read temperature values from hwmon sysfs.

use std::path::Path;
use std::time::SystemTime;

use crate::error::HwmonError;
use crate::hwmon::types::{SensorDescriptor, SensorReading};
use crate::hwmon::util::sanitize_f64;

/// Read a temperature value from a `temp*_input` sysfs file.
///
/// The kernel reports temperatures in millidegrees Celsius (e.g. 45000 = 45.0°C).
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

    // Sanity bounds: values outside -50°C to 250°C are almost certainly garbage
    if !(-50.0..=250.0).contains(&value_c) {
        log::warn!(
            "Sensor '{}' reported implausible temperature {:.1}°C, clamping to [-50, 250]",
            descriptor.id,
            value_c
        );
        value_c = value_c.clamp(-50.0, 250.0);
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
