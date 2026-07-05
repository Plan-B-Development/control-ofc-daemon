//! Hwmon sysfs discovery, sensor reading, and PWM control.
//!
//! - Discovery of temperature sensors with stable IDs
//! - Reading temperatures from hwmon sysfs
//! - Discovery of controllable PWM outputs with stable IDs
//! - Lease-protected PWM writes with safety floors

pub mod aio;
pub mod discovery;
pub mod gpu_detect;
pub mod gpu_fan;
pub mod intel_gpu_detect;
pub mod inventory;
pub mod kernel_warnings;
pub mod lease;
pub mod pwm_control;
pub mod pwm_discovery;
pub mod reader;
pub mod types;
pub mod util;

use types::{SensorDescriptor, SensorReading};

/// Default sysfs hwmon root.
pub const HWMON_SYSFS_ROOT: &str = "/sys/class/hwmon";

/// Outcome of reading a descriptor set: successful readings plus the per-sensor
/// failures (DEC-193).
///
/// [`read_sensor_values`] deliberately does **not** log failures — that policy
/// (throttling, quarantine, and surfacing as "unavailable") belongs to the poll
/// loop's `SensorFailureTracker`, so a sensor that is present but unreadable
/// (e.g. an `ath12k` WiFi temp returning `ENETDOWN` while the radio is
/// soft-blocked) can no longer spam the journal at 1 Hz.
#[derive(Debug, Default)]
pub struct SensorReadOutcome {
    /// Sensors that read successfully this pass.
    pub readings: Vec<SensorReading>,
    /// Sensors whose `temp*_input` read failed this pass.
    pub failures: Vec<SensorReadFailure>,
}

/// A single sensor that failed to read. Carries enough to surface it as an
/// "unavailable" sensor on `/status` + `/poll` without re-reading sysfs.
#[derive(Debug, Clone)]
pub struct SensorReadFailure {
    /// Stable sensor ID.
    pub id: String,
    /// Human-friendly label from discovery.
    pub label: String,
    /// Human-readable cause — the [`HwmonError`] display (e.g.
    /// "read error: /sys/.../temp1_input: Network is down (os error 100)").
    pub reason: String,
}

/// Read current values for an already-discovered descriptor set.
///
/// The per-tick hot path (DEC-133): touches only each sensor's
/// `temp*_input` file — no directory enumeration, no label/type reads, no
/// threshold/alarm snapshot. Returns the successful readings *and* the
/// per-sensor failures; the caller owns all logging/quarantine policy
/// (DEC-193) so this function is silent.
pub fn read_sensor_values(descriptors: &[SensorDescriptor]) -> SensorReadOutcome {
    let mut outcome = SensorReadOutcome::default();
    for d in descriptors {
        match reader::read_temp(d) {
            Ok(r) => outcome.readings.push(r),
            Err(e) => outcome.failures.push(SensorReadFailure {
                id: d.id.clone(),
                label: d.label.clone(),
                reason: e.to_string(),
            }),
        }
    }
    outcome
}

/// True when an hwmon chip is a wireless-radio PHY temperature sensor — e.g.
/// Qualcomm Atheros `ath1{0,1,2}k_hwmon` or Intel `iwlwifi` (DEC-193).
///
/// Such a sensor reads `ENETDOWN` whenever the radio is down (rfkill, interface
/// down, firmware not started — kernel-by-design, shared across ath10k/11k/12k),
/// so it must never be offered as a fan-curve source: a WiFi temperature would
/// strand a curve the moment WiFi is switched off. The API marks these
/// `control_eligible = false`; display is unaffected (they still appear on
/// `/sensors` when readable). The list is intentionally conservative — extend it
/// as other wireless thermal-hwmon drivers are confirmed.
pub fn is_wireless_phy_chip(chip_name: &str) -> bool {
    (chip_name.starts_with("ath") && chip_name.ends_with("_hwmon"))
        || chip_name.starts_with("iwlwifi")
        || chip_name.starts_with("iwlmvm")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// DEC-193: `read_sensor_values` surfaces failures instead of logging, so a
    /// present-but-unreadable sensor (bad data here) is reported as a failure
    /// carrying its id/label/reason — the input the quarantine tracker needs.
    #[test]
    fn read_sensor_values_reports_failures_without_dropping_them() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "k10temp\n").unwrap();
        fs::write(hwmon0.join("temp1_input"), "55000\n").unwrap();
        fs::write(hwmon0.join("temp1_label"), "Tctl\n").unwrap();
        fs::write(hwmon0.join("temp2_input"), "garbage\n").unwrap();
        fs::write(hwmon0.join("temp2_label"), "Bad\n").unwrap();

        let descriptors = discovery::discover_sensors(tmp.path()).unwrap();
        let outcome = read_sensor_values(&descriptors);

        assert_eq!(outcome.readings.len(), 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].label, "Bad");
        assert!(outcome.failures[0].reason.contains("invalid temperature"));
    }

    #[test]
    fn is_wireless_phy_chip_recognises_radio_thermals() {
        // Qualcomm Atheros WiFi (the reported case) + Intel.
        assert!(is_wireless_phy_chip("ath12k_hwmon"));
        assert!(is_wireless_phy_chip("ath11k_hwmon"));
        assert!(is_wireless_phy_chip("ath10k_hwmon"));
        assert!(is_wireless_phy_chip("iwlwifi_1"));
        // Real fan-control-eligible sensors are not wireless.
        assert!(!is_wireless_phy_chip("k10temp"));
        assert!(!is_wireless_phy_chip("nct6798"));
        assert!(!is_wireless_phy_chip("amdgpu"));
        assert!(!is_wireless_phy_chip("z53")); // NZXT Kraken coolant
    }
}
