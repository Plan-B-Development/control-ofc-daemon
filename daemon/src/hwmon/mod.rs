//! Hwmon sysfs discovery, sensor reading, and PWM control.
//!
//! - Discovery of temperature sensors with stable IDs
//! - Reading temperatures from hwmon sysfs
//! - Discovery of controllable PWM outputs with stable IDs
//! - Lease-protected PWM writes with safety floors

pub mod discovery;
pub mod gpu_detect;
pub mod gpu_fan;
pub mod lease;
pub mod pwm_control;
pub mod pwm_discovery;
pub mod reader;
pub mod types;
pub mod util;

use std::path::Path;

use crate::error::HwmonError;
use types::{SensorDescriptor, SensorReading};

/// Default sysfs hwmon root.
pub const HWMON_SYSFS_ROOT: &str = "/sys/class/hwmon";

/// Discover all temperature sensors and read their current values.
///
/// This is the main entry point for sensor collection. It performs
/// fresh discovery on each call (no caching — that's Milestone 3).
pub fn collect_sensors(
    hwmon_root: &Path,
) -> Result<(Vec<SensorDescriptor>, Vec<SensorReading>), HwmonError> {
    let descriptors = discovery::discover_sensors(hwmon_root)?;

    let readings: Vec<SensorReading> = descriptors
        .iter()
        .filter_map(|d| match reader::read_temp(d) {
            Ok(r) => Some(r),
            Err(e) => {
                log::warn!("Failed to read sensor {}: {e}", d.id);
                None
            }
        })
        .collect();

    Ok((descriptors, readings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn collect_sensors_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();

        // Create k10temp device
        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "k10temp\n").unwrap();
        fs::write(hwmon0.join("temp1_input"), "55000\n").unwrap();
        fs::write(hwmon0.join("temp1_label"), "Tctl\n").unwrap();

        // Create amdgpu device
        let hwmon1 = tmp.path().join("hwmon1");
        fs::create_dir_all(&hwmon1).unwrap();
        fs::write(hwmon1.join("name"), "amdgpu\n").unwrap();
        fs::write(hwmon1.join("temp1_input"), "42000\n").unwrap();
        fs::write(hwmon1.join("temp1_label"), "edge\n").unwrap();

        let (descriptors, readings) = collect_sensors(tmp.path()).unwrap();

        assert_eq!(descriptors.len(), 2);
        assert_eq!(readings.len(), 2);

        // Verify readings match descriptors
        assert!((readings[0].value_c - 55.0).abs() < f64::EPSILON);
        assert!((readings[1].value_c - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn collect_sensors_skips_unreadable() {
        let tmp = tempfile::tempdir().unwrap();

        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "k10temp\n").unwrap();
        fs::write(hwmon0.join("temp1_input"), "55000\n").unwrap();
        fs::write(hwmon0.join("temp1_label"), "Tctl\n").unwrap();
        // temp2 exists in discovery but has bad data
        fs::write(hwmon0.join("temp2_input"), "garbage\n").unwrap();

        let (descriptors, readings) = collect_sensors(tmp.path()).unwrap();

        assert_eq!(descriptors.len(), 2);
        // Only the valid reading comes through
        assert_eq!(readings.len(), 1);
        assert!((readings[0].value_c - 55.0).abs() < f64::EPSILON);
    }
}
