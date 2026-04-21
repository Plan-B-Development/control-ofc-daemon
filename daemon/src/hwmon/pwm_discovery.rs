//! Discover controllable PWM outputs on motherboard fan headers.
//!
//! Scans hwmon sysfs for `pwmN` files and builds descriptors with
//! stable IDs that do not depend on the hwmon index.

use std::path::{Path, PathBuf};

use crate::error::HwmonError;

/// Descriptor for a controllable PWM fan header.
#[derive(Debug, Clone)]
pub struct PwmHeaderDescriptor {
    /// Stable identifier (e.g. `hwmon:it8696:it87.2624:pwm3:PUMP`).
    pub id: String,
    /// Human-readable label (from `fanN_label` / `pwmN_label`, or fallback).
    pub label: String,
    /// Chip name (e.g. `it8696`).
    pub chip_name: String,
    /// Device identifier (PCI BDF or platform device name).
    pub device_id: String,
    /// PWM index (the N in `pwmN`).
    pub pwm_index: u8,
    /// Whether `pwmN_enable` exists (needed for mode control).
    pub supports_enable: bool,
    /// Path to the `pwmN` file for writing.
    pub pwm_path: String,
    /// Path to the `pwmN_enable` file (if it exists).
    pub enable_path: Option<String>,
    /// Whether `fanN_input` exists (RPM reading available).
    pub rpm_available: bool,
    /// Path to `fanN_input` for RPM reading (if it exists).
    pub rpm_path: Option<String>,
    /// Minimum PWM percent (policy floor).
    pub min_pwm_percent: u8,
    /// Maximum PWM percent (policy ceiling).
    pub max_pwm_percent: u8,
    /// Whether the `pwmN` file is writable (checked at discovery time).
    pub is_writable: bool,
    /// PWM/DC mode from `pwmN_mode` (0=DC, 1=PWM, None if file absent).
    pub pwm_mode: Option<u8>,
}

/// Discover all controllable PWM outputs under a given hwmon root.
///
/// A header is considered controllable if it has a `pwmN` file.
pub fn discover_pwm_headers(hwmon_root: &Path) -> Result<Vec<PwmHeaderDescriptor>, HwmonError> {
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
        match discover_device_pwm(&hwmon_dir) {
            Ok(headers) => descriptors.extend(headers),
            Err(e) => {
                log::warn!("Skipping PWM discovery for {}: {e}", hwmon_dir.display());
            }
        }
    }

    Ok(descriptors)
}

/// Discover PWM outputs for a single hwmon device directory.
fn discover_device_pwm(hwmon_dir: &Path) -> Result<Vec<PwmHeaderDescriptor>, HwmonError> {
    let chip_name = read_sysfs_string(&hwmon_dir.join("name"))?
        .trim()
        .to_string();

    let device_id = resolve_device_id(hwmon_dir);

    // Find all pwmN files (pwm1, pwm2, ...)
    let entries = std::fs::read_dir(hwmon_dir).map_err(|e| HwmonError::ReadError {
        path: hwmon_dir.display().to_string(),
        message: e.to_string(),
    })?;

    let mut pwm_files: Vec<(u8, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            // Match "pwm1", "pwm2", etc. but NOT "pwm1_enable" etc.
            if let Some(rest) = name.strip_prefix("pwm") {
                if let Ok(index) = rest.parse::<u8>() {
                    return Some((index, e.path()));
                }
            }
            None
        })
        .collect();

    pwm_files.sort_by_key(|(idx, _)| *idx);

    let mut headers = Vec::new();
    for (pwm_index, pwm_path) in pwm_files {
        let enable_path = hwmon_dir.join(format!("pwm{pwm_index}_enable"));
        let supports_enable = enable_path.exists();

        let fan_input = hwmon_dir.join(format!("fan{pwm_index}_input"));
        let rpm_available = fan_input.exists();

        // Try label sources in priority order
        let label = read_label(hwmon_dir, pwm_index);

        let id = build_stable_id(&chip_name, &device_id, pwm_index, &label);

        // Check if pwmN is writable (probe file permissions)
        let is_writable = std::fs::metadata(&pwm_path)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false);

        // Read pwmN_mode if present (0=DC, 1=PWM)
        let mode_path = hwmon_dir.join(format!("pwm{pwm_index}_mode"));
        let pwm_mode = if mode_path.exists() {
            std::fs::read_to_string(&mode_path)
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
        } else {
            None
        };

        headers.push(PwmHeaderDescriptor {
            id,
            label,
            chip_name: chip_name.clone(),
            device_id: device_id.clone(),
            pwm_index,
            supports_enable,
            pwm_path: pwm_path.display().to_string(),
            enable_path: if supports_enable {
                Some(enable_path.display().to_string())
            } else {
                None
            },
            rpm_available,
            rpm_path: if rpm_available {
                Some(fan_input.display().to_string())
            } else {
                None
            },
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable,
            pwm_mode,
        });
    }

    Ok(headers)
}

/// Read the best available label for a PWM header.
fn read_label(hwmon_dir: &Path, pwm_index: u8) -> String {
    // Try pwmN_label first, then fanN_label, then fallback
    let pwm_label_path = hwmon_dir.join(format!("pwm{pwm_index}_label"));
    if let Ok(label) = read_sysfs_string(&pwm_label_path) {
        let trimmed = label.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }

    let fan_label_path = hwmon_dir.join(format!("fan{pwm_index}_label"));
    if let Ok(label) = read_sysfs_string(&fan_label_path) {
        let trimmed = label.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }

    format!("pwm{pwm_index}")
}

// classify_header_floor() removed — thermal safety is centralized in ThermalSafetyRule.

/// Build a stable ID for a PWM header.
fn build_stable_id(chip_name: &str, device_id: &str, pwm_index: u8, label: &str) -> String {
    format!("hwmon:{chip_name}:{device_id}:pwm{pwm_index}:{label}")
}

/// Resolve the stable device ID from the sysfs device symlink.
fn resolve_device_id(hwmon_dir: &Path) -> String {
    let device_link = hwmon_dir.join("device");
    if device_link.exists() {
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
    }
}

use super::util::{device_id_from_path, read_sysfs_string};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a hwmon fixture with PWM outputs.
    fn create_pwm_fixture(
        base: &Path,
        dir_name: &str,
        chip_name: &str,
        pwm_outputs: &[(u8, Option<&str>, bool, bool)], // (index, label, has_enable, has_fan_input)
    ) -> PathBuf {
        let hwmon_dir = base.join(dir_name);
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), chip_name).unwrap();

        for &(index, label, has_enable, has_fan_input) in pwm_outputs {
            fs::write(hwmon_dir.join(format!("pwm{index}")), "128\n").unwrap();
            if has_enable {
                fs::write(hwmon_dir.join(format!("pwm{index}_enable")), "2\n").unwrap();
            }
            if has_fan_input {
                fs::write(hwmon_dir.join(format!("fan{index}_input")), "1200\n").unwrap();
            }
            if let Some(lbl) = label {
                fs::write(
                    hwmon_dir.join(format!("fan{index}_label")),
                    format!("{lbl}\n"),
                )
                .unwrap();
            }
        }

        hwmon_dir
    }

    #[test]
    fn discover_single_pwm_output() {
        let tmp = tempfile::tempdir().unwrap();
        create_pwm_fixture(tmp.path(), "hwmon0", "it8696", &[(1, None, true, true)]);

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].chip_name, "it8696");
        assert_eq!(headers[0].pwm_index, 1);
        assert!(headers[0].supports_enable);
        assert!(headers[0].rpm_available);
        assert_eq!(headers[0].label, "pwm1");
    }

    #[test]
    fn discover_multiple_pwm_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        create_pwm_fixture(
            tmp.path(),
            "hwmon0",
            "it8696",
            &[
                (1, Some("CPU_FAN"), true, true),
                (2, Some("CHA_FAN1"), true, true),
                (3, Some("PUMP"), true, false),
            ],
        );

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[0].label, "CPU_FAN");
        assert_eq!(headers[1].label, "CHA_FAN1");
        assert_eq!(headers[2].label, "PUMP");
    }

    #[test]
    fn stable_id_format() {
        let tmp = tempfile::tempdir().unwrap();
        create_pwm_fixture(
            tmp.path(),
            "hwmon0",
            "it8696",
            &[(1, Some("CPU_FAN"), true, true)],
        );

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert!(headers[0].id.starts_with("hwmon:it8696:"));
        assert!(headers[0].id.contains("pwm1"));
        assert!(headers[0].id.contains("CPU_FAN"));
        // Must not contain hwmon index
        assert!(!headers[0].id.contains("hwmon0"));
    }

    #[test]
    fn discover_without_enable_file() {
        let tmp = tempfile::tempdir().unwrap();
        create_pwm_fixture(tmp.path(), "hwmon0", "amdgpu", &[(1, None, false, true)]);

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert_eq!(headers.len(), 1);
        assert!(!headers[0].supports_enable);
        assert!(headers[0].enable_path.is_none());
    }

    #[test]
    fn discover_without_fan_input() {
        let tmp = tempfile::tempdir().unwrap();
        create_pwm_fixture(
            tmp.path(),
            "hwmon0",
            "it8696",
            &[(3, Some("PUMP"), true, false)],
        );

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert_eq!(headers.len(), 1);
        assert!(!headers[0].rpm_available);
        assert!(headers[0].rpm_path.is_none());
    }

    #[test]
    fn all_headers_have_zero_floor() {
        // No per-header floors — thermal safety handled centrally
        let tmp = tempfile::tempdir().unwrap();
        create_pwm_fixture(
            tmp.path(),
            "hwmon0",
            "it8696",
            &[
                (1, Some("CPU_FAN"), true, true),
                (2, Some("CHA_FAN1"), true, true),
            ],
        );

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert_eq!(headers[0].min_pwm_percent, 0);
        assert_eq!(headers[1].min_pwm_percent, 0);
    }

    #[test]
    fn pump_header_has_zero_floor() {
        let tmp = tempfile::tempdir().unwrap();
        create_pwm_fixture(
            tmp.path(),
            "hwmon0",
            "it8696",
            &[(3, Some("PUMP"), true, false)],
        );

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert_eq!(headers[0].min_pwm_percent, 0);
    }

    #[test]
    fn discover_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert!(headers.is_empty());
    }

    #[test]
    fn discover_skips_chip_without_pwm() {
        let tmp = tempfile::tempdir().unwrap();
        // Create hwmon device with sensors but no PWM
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "k10temp").unwrap();
        fs::write(hwmon_dir.join("temp1_input"), "55000\n").unwrap();

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert!(headers.is_empty());
    }

    #[test]
    fn discover_multiple_devices() {
        let tmp = tempfile::tempdir().unwrap();
        create_pwm_fixture(
            tmp.path(),
            "hwmon0",
            "it8696",
            &[(1, Some("CPU_FAN"), true, true)],
        );
        create_pwm_fixture(
            tmp.path(),
            "hwmon1",
            "nct6798",
            &[(1, Some("SYS_FAN1"), true, true)],
        );

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert_eq!(headers.len(), 2);
    }

    #[test]
    fn pwm_label_preferred_over_fan_label() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "it8696").unwrap();
        fs::write(hwmon_dir.join("pwm1"), "128\n").unwrap();
        fs::write(hwmon_dir.join("pwm1_enable"), "2\n").unwrap();
        fs::write(hwmon_dir.join("fan1_label"), "FAN_LABEL\n").unwrap();
        fs::write(hwmon_dir.join("pwm1_label"), "PWM_LABEL\n").unwrap();

        let headers = discover_pwm_headers(tmp.path()).unwrap();
        assert_eq!(headers[0].label, "PWM_LABEL");
    }
}
