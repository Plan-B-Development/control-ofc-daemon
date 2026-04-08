//! Hwmon device discovery and stable ID generation.
//!
//! Enumerates `/sys/class/hwmon/hwmon*`, reads chip names and labels,
//! and produces `SensorDescriptor`s with stable IDs that do not depend
//! on the hwmon index.

use std::path::{Path, PathBuf};

use crate::error::HwmonError;
use crate::hwmon::types::{SensorDescriptor, SensorKind, SensorSource};

/// Known chip name → sensor kind classification.
fn classify_chip(chip_name: &str, label: &str) -> SensorKind {
    match chip_name {
        "k10temp" => SensorKind::CpuTemp,
        "coretemp" => SensorKind::CpuTemp,
        "amdgpu" => SensorKind::GpuTemp,
        "nvme" => SensorKind::DiskTemp,
        _ if chip_name.starts_with("it87") => SensorKind::MbTemp,
        _ => {
            // Fallback: try to guess from the label
            let lower = label.to_lowercase();
            if lower.contains("cpu") || lower.contains("tctl") || lower.contains("tccd") {
                SensorKind::CpuTemp
            } else if lower.contains("gpu") || lower.contains("edge") || lower.contains("junction")
            {
                SensorKind::GpuTemp
            } else {
                SensorKind::MbTemp
            }
        }
    }
}

/// Build a stable ID for an hwmon sensor.
///
/// Format: `hwmon:<chip_name>:<device_id>:<label_or_index>`
/// where `device_id` is derived from the device path to distinguish
/// multiple chips with the same name (e.g. two NVMe drives).
fn build_stable_id(chip_name: &str, device_id: &str, label: &str) -> String {
    format!("hwmon:{chip_name}:{device_id}:{label}")
}

/// Extract a short device identifier from the sysfs device path.
///
/// For PCI devices: extracts the PCI address (e.g. `0000:03:00.0`).
use super::util::device_id_from_path;

/// Discover all temperature sensors under a given sysfs hwmon root.
///
/// The `hwmon_root` parameter allows injecting a test fixture directory
/// instead of the real `/sys/class/hwmon`.
pub fn discover_sensors(hwmon_root: &Path) -> Result<Vec<SensorDescriptor>, HwmonError> {
    let mut descriptors = Vec::new();

    let entries = std::fs::read_dir(hwmon_root).map_err(|e| HwmonError::ReadError {
        path: hwmon_root.display().to_string(),
        message: e.to_string(),
    })?;

    let mut hwmon_dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hwmon"))
        })
        .collect();

    hwmon_dirs.sort();

    for hwmon_dir in hwmon_dirs {
        match discover_device_sensors(&hwmon_dir) {
            Ok(sensors) => descriptors.extend(sensors),
            Err(e) => {
                log::warn!("Skipping {}: {e}", hwmon_dir.display());
            }
        }
    }

    Ok(descriptors)
}

/// Discover temperature sensors for a single hwmon device directory.
fn discover_device_sensors(hwmon_dir: &Path) -> Result<Vec<SensorDescriptor>, HwmonError> {
    let chip_name = read_sysfs_string(&hwmon_dir.join("name"))?
        .trim()
        .to_string();

    // Read device symlink for stable ID
    let device_link = hwmon_dir.join("device");
    let device_id = if device_link.exists() {
        let resolved = std::fs::read_link(&device_link)
            .or_else(|_| std::fs::canonicalize(&device_link))
            .unwrap_or_else(|e| {
                log::warn!(
                    "Could not resolve device symlink for {}: {}",
                    hwmon_dir.display(),
                    e
                );
                std::path::PathBuf::from("unknown")
            });
        device_id_from_path(&resolved)
    } else {
        "nodev".to_string()
    };

    // Find all temp*_input files
    let mut sensors = Vec::new();
    let entries = std::fs::read_dir(hwmon_dir).map_err(|e| HwmonError::ReadError {
        path: hwmon_dir.display().to_string(),
        message: e.to_string(),
    })?;

    let mut temp_inputs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("temp") && n.ends_with("_input"))
        })
        .collect();

    temp_inputs.sort();

    for input_path in temp_inputs {
        let filename = input_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        // Extract index: temp1_input → 1
        let index = filename
            .strip_prefix("temp")
            .and_then(|s| s.strip_suffix("_input"))
            .unwrap_or("0");

        // Try to read the label file (temp1_label, etc.)
        let label_path = input_path.with_file_name(format!("temp{index}_label"));
        let label = if label_path.exists() {
            read_sysfs_string(&label_path)
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            format!("temp{index}")
        };

        let kind = classify_chip(&chip_name, &label);
        let id = build_stable_id(&chip_name, &device_id, &label);
        let source = if chip_name == "amdgpu" {
            SensorSource::AmdGpu
        } else {
            SensorSource::Hwmon
        };

        sensors.push(SensorDescriptor {
            id,
            kind,
            label: label.clone(),
            source,
            input_path: input_path.display().to_string(),
        });
    }

    Ok(sensors)
}

/// Read a sysfs file as a trimmed string.
use super::util::read_sysfs_string;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_fixture_with_chip_name(
        base: &Path,
        dir_name: &str,
        chip_name: &str,
        temps: &[(&str, Option<&str>)],
    ) -> PathBuf {
        let hwmon_dir = base.join(dir_name);
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), chip_name).unwrap();

        for (index, label) in temps {
            fs::write(hwmon_dir.join(format!("temp{index}_input")), "45000\n").unwrap();
            if let Some(lbl) = label {
                fs::write(
                    hwmon_dir.join(format!("temp{index}_label")),
                    format!("{lbl}\n"),
                )
                .unwrap();
            }
        }

        hwmon_dir
    }

    #[test]
    fn discover_cpu_sensor_k10temp() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "k10temp",
            &[("1", Some("Tctl")), ("3", Some("Tccd1"))],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
        assert_eq!(sensors[0].label, "Tctl");
        assert!(sensors[0].id.contains("k10temp"));
        assert!(sensors[0].id.contains("Tctl"));
        assert_eq!(sensors[1].label, "Tccd1");
    }

    #[test]
    fn discover_gpu_sensor_amdgpu() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "amdgpu",
            &[
                ("1", Some("edge")),
                ("2", Some("junction")),
                ("3", Some("mem")),
            ],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 3);
        for s in &sensors {
            assert_eq!(s.kind, SensorKind::GpuTemp);
        }
        assert_eq!(sensors[0].label, "edge");
    }

    #[test]
    fn discover_nvme_disk_sensor() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "nvme", &[("1", Some("Composite"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].kind, SensorKind::DiskTemp);
        assert!(sensors[0].id.contains("nvme"));
    }

    #[test]
    fn discover_motherboard_sensor_ite() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "it8696", &[("1", None), ("2", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        for s in &sensors {
            assert_eq!(s.kind, SensorKind::MbTemp);
        }
        // Without labels, fallback name used
        assert_eq!(sensors[0].label, "temp1");
    }

    #[test]
    fn discover_missing_label_uses_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "k10temp",
            &[("1", None)], // no label file
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].label, "temp1");
        // Fallback label doesn't match CPU heuristics, but chip name does
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
    }

    #[test]
    fn discover_multiple_devices() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "k10temp", &[("1", Some("Tctl"))]);
        create_fixture_with_chip_name(tmp.path(), "hwmon1", "amdgpu", &[("1", Some("edge"))]);
        create_fixture_with_chip_name(tmp.path(), "hwmon2", "it8696", &[("1", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 3);

        let kinds: Vec<_> = sensors.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&SensorKind::CpuTemp));
        assert!(kinds.contains(&SensorKind::GpuTemp));
        assert!(kinds.contains(&SensorKind::MbTemp));
    }

    #[test]
    fn discover_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let sensors = discover_sensors(tmp.path()).unwrap();
        assert!(sensors.is_empty());
    }

    #[test]
    fn discover_skips_non_hwmon_entries() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a non-hwmon directory
        fs::create_dir_all(tmp.path().join("notahwmon")).unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "k10temp", &[("1", Some("Tctl"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
    }

    #[test]
    fn stable_id_does_not_contain_hwmon_index() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon5", "k10temp", &[("1", Some("Tctl"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert!(!sensors[0].id.contains("hwmon5"));
        assert!(sensors[0].id.starts_with("hwmon:k10temp:"));
    }

    #[test]
    fn device_id_extracts_pci_address() {
        let path = Path::new(
            "/sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0/0000:02:00.0/0000:03:00.0",
        );
        let id = device_id_from_path(path);
        assert_eq!(id, "0000:03:00.0");
    }

    #[test]
    fn device_id_extracts_platform_id() {
        let path = Path::new("/sys/devices/platform/it87.2624");
        let id = device_id_from_path(path);
        assert_eq!(id, "it87.2624");
    }
}
