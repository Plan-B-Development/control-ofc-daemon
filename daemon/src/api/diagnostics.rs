//! Hardware diagnostics: kernel module detection and ACPI conflict scanning.

use std::collections::HashMap;
use std::path::Path;

use super::responses::{AcpiConflictInfo, KernelModuleInfo};

/// Known hwmon driver modules and whether they're in the mainline kernel.
const KNOWN_MODULES: &[(&str, bool)] = &[
    ("nct6775", true),
    ("nct6775_core", true),
    ("nct6775_platform", true),
    ("nct6683", true),
    ("nct6687", false),
    ("it87", true),
    ("f71882fg", true),
    ("asus_ec_sensors", true),
    ("asus_wmi_sensors", true),
    ("asus_wmi_ec_sensors", true),
    ("sch5627", true),
    ("sch5636", true),
    ("k10temp", true),
    ("coretemp", true),
    ("amdgpu", true),
];

/// Map chip_name prefix → expected kernel driver module name.
fn expected_driver_for_chip(chip_name: &str) -> &'static str {
    let lower = chip_name.to_lowercase();
    if lower.starts_with("nct6687") {
        "nct6687"
    } else if lower.starts_with("nct6") || lower.starts_with("nct5") {
        "nct6775"
    } else if lower.starts_with("it8") {
        "it87"
    } else if lower.starts_with("f718") || lower.starts_with("f8000") || lower.starts_with("f818") {
        "f71882fg"
    } else if lower.starts_with("sch5627") {
        "sch5627"
    } else if lower.starts_with("sch5636") {
        "sch5636"
    } else {
        "unknown"
    }
}

/// Whether a chip's driver is in the mainline kernel.
pub fn chip_driver_in_mainline(chip_name: &str) -> bool {
    let driver = expected_driver_for_chip(chip_name);
    // ITE chips IT8625E+ require out-of-tree frankcrawford/it87
    if driver == "it87" {
        let lower = chip_name.to_lowercase();
        let mainline_chips = [
            "it8603", "it8620", "it8623", "it8628", "it8705", "it8712", "it8716", "it8718",
            "it8720", "it8721", "it8726", "it8728", "it8732", "it8758", "it8771", "it8772",
            "it8781", "it8782", "it8783", "it8786", "it8790", "it8792", "it8795", "it87952",
        ];
        return mainline_chips.iter().any(|c| lower.starts_with(c));
    }
    KNOWN_MODULES
        .iter()
        .find(|(name, _)| *name == driver)
        .map(|(_, mainline)| *mainline)
        .unwrap_or(false)
}

/// Return the expected driver name for a chip.
pub fn expected_driver(chip_name: &str) -> &'static str {
    expected_driver_for_chip(chip_name)
}

/// Detect which known hwmon kernel modules are currently loaded.
pub fn detect_loaded_modules() -> Vec<KernelModuleInfo> {
    detect_loaded_modules_from(Path::new("/proc/modules"))
}

/// Testable variant with injectable path.
pub fn detect_loaded_modules_from(proc_modules: &Path) -> Vec<KernelModuleInfo> {
    let content = match std::fs::read_to_string(proc_modules) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Cannot read {}: {e}", proc_modules.display());
            return KNOWN_MODULES
                .iter()
                .map(|(name, mainline)| KernelModuleInfo {
                    name: name.to_string(),
                    loaded: false,
                    in_mainline: *mainline,
                })
                .collect();
        }
    };

    let loaded: HashMap<&str, bool> = content
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(|name| (name, true))
        .collect();

    KNOWN_MODULES
        .iter()
        .map(|(name, mainline)| KernelModuleInfo {
            name: name.to_string(),
            loaded: loaded.contains_key(name),
            in_mainline: *mainline,
        })
        .collect()
}

/// I/O port ranges used by common Super I/O chips.
const SIO_IO_RANGES: &[(&str, u16, u16)] = &[
    ("nct6775", 0x0290, 0x0299),
    ("nct6775", 0x04E0, 0x04EF),
    ("it87", 0x0290, 0x029F),
    ("it87", 0x0A20, 0x0A2F),
    ("it87", 0x0A40, 0x0A4F),
    ("it87", 0x0A60, 0x0A6F),
];

/// Detect ACPI I/O port conflicts with hwmon drivers.
pub fn detect_acpi_conflicts() -> Vec<AcpiConflictInfo> {
    detect_acpi_conflicts_from(Path::new("/proc/ioports"))
}

/// Testable variant with injectable path.
pub fn detect_acpi_conflicts_from(proc_ioports: &Path) -> Vec<AcpiConflictInfo> {
    let content = match std::fs::read_to_string(proc_ioports) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Cannot read {}: {e}", proc_ioports.display());
            return vec![];
        }
    };

    let mut conflicts = Vec::new();

    // Parse /proc/ioports lines like:
    //   0290-0299 : ACPI OpRegion AMW0.SHWM
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("ACPI") && !trimmed.contains("acpi") {
            continue;
        }

        // Parse the range: "0290-0299 : description"
        let parts: Vec<&str> = trimmed.splitn(2, " : ").collect();
        if parts.len() != 2 {
            continue;
        }
        let range_str = parts[0].trim();
        let description = parts[1].trim();

        let range_parts: Vec<&str> = range_str.split('-').collect();
        if range_parts.len() != 2 {
            continue;
        }

        let start = match u16::from_str_radix(range_parts[0].trim(), 16) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let end = match u16::from_str_radix(range_parts[1].trim(), 16) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check overlap with known SIO ranges
        for (driver, sio_start, sio_end) in SIO_IO_RANGES {
            if start <= *sio_end && end >= *sio_start {
                conflicts.push(AcpiConflictInfo {
                    io_range: range_str.to_string(),
                    claimed_by: description.to_string(),
                    conflicts_with_driver: driver.to_string(),
                });
            }
        }
    }

    conflicts
}

// ── DMI board identification ──────────────────────────────────────

use super::responses::BoardInfo;

/// Read motherboard identification from DMI sysfs (world-readable, no root required).
pub fn read_board_info() -> BoardInfo {
    read_board_info_from(Path::new("/sys/class/dmi/id"))
}

/// Testable variant with injectable path.
pub fn read_board_info_from(dmi_dir: &Path) -> BoardInfo {
    let read_field = |field: &str| -> String {
        std::fs::read_to_string(dmi_dir.join(field))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    BoardInfo {
        vendor: read_field("board_vendor"),
        name: read_field("board_name"),
        bios_version: read_field("bios_version"),
    }
}

/// Read the raw ppfeaturemask value as a hex string.
pub fn read_ppfeaturemask() -> Option<String> {
    read_ppfeaturemask_from(Path::new("/sys/module/amdgpu/parameters/ppfeaturemask"))
}

/// Testable variant.
pub fn read_ppfeaturemask_from(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    // Normalize to hex format
    if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        Some(trimmed.to_string())
    } else if let Ok(dec) = trimmed.parse::<u32>() {
        Some(format!("0x{dec:08x}"))
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn expected_driver_mapping() {
        assert_eq!(expected_driver("nct6798"), "nct6775");
        assert_eq!(expected_driver("nct6687"), "nct6687");
        assert_eq!(expected_driver("it8696"), "it87");
        assert_eq!(expected_driver("it8688"), "it87");
        assert_eq!(expected_driver("f71882fg"), "f71882fg");
        assert_eq!(expected_driver("unknown_chip"), "unknown");
    }

    #[test]
    fn mainline_detection() {
        assert!(chip_driver_in_mainline("nct6798"));
        assert!(!chip_driver_in_mainline("nct6687"));
        // IT8688E is NOT in mainline
        assert!(!chip_driver_in_mainline("it8688"));
        // IT8628E IS in mainline
        assert!(chip_driver_in_mainline("it8628"));
    }

    #[test]
    fn detect_modules_from_proc() {
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6775 28672 0 - Live 0xffffffffc0a00000\n\
             k10temp 16384 0 - Live 0xffffffffc0980000\n\
             amdgpu 8388608 12 - Live 0xffffffffc1000000\n",
        )
        .unwrap();

        let modules = detect_loaded_modules_from(&modules_path);
        let nct = modules.iter().find(|m| m.name == "nct6775").unwrap();
        assert!(nct.loaded);
        assert!(nct.in_mainline);

        let it87 = modules.iter().find(|m| m.name == "it87").unwrap();
        assert!(!it87.loaded);

        let nct6687 = modules.iter().find(|m| m.name == "nct6687").unwrap();
        assert!(!nct6687.loaded);
        assert!(!nct6687.in_mainline);
    }

    #[test]
    fn detect_acpi_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let ioports_path = tmp.path().join("ioports");
        fs::write(
            &ioports_path,
            "0000-0cf7 : PCI Bus 0000:00\n\
             0290-0299 : ACPI OpRegion AMW0.SHWM\n\
             0cf8-0cff : PCI conf1\n",
        )
        .unwrap();

        let conflicts = detect_acpi_conflicts_from(&ioports_path);
        assert_eq!(conflicts.len(), 2); // Overlaps with both nct6775 and it87 ranges
        assert!(conflicts
            .iter()
            .any(|c| c.conflicts_with_driver == "nct6775"));
    }

    #[test]
    fn no_acpi_conflict_when_no_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let ioports_path = tmp.path().join("ioports");
        fs::write(
            &ioports_path,
            "0000-001f : ACPI something\n\
             0400-040f : ACPI PM_TMR\n",
        )
        .unwrap();

        let conflicts = detect_acpi_conflicts_from(&ioports_path);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn ppfeaturemask_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ppfeaturemask");
        fs::write(&path, "0xffffffff\n").unwrap();
        assert_eq!(read_ppfeaturemask_from(&path), Some("0xffffffff".into()));
    }

    #[test]
    fn ppfeaturemask_decimal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ppfeaturemask");
        fs::write(&path, "4294967295\n").unwrap();
        assert_eq!(read_ppfeaturemask_from(&path), Some("0xffffffff".into()));
    }

    #[test]
    fn ppfeaturemask_missing() {
        assert_eq!(read_ppfeaturemask_from(Path::new("/nonexistent")), None);
    }

    #[test]
    fn read_board_info_from_sysfs() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("board_vendor"),
            "Gigabyte Technology Co., Ltd.\n",
        )
        .unwrap();
        fs::write(tmp.path().join("board_name"), "X870E AORUS MASTER\n").unwrap();
        fs::write(tmp.path().join("bios_version"), "F13a\n").unwrap();

        let info = read_board_info_from(tmp.path());
        assert_eq!(info.vendor, "Gigabyte Technology Co., Ltd.");
        assert_eq!(info.name, "X870E AORUS MASTER");
        assert_eq!(info.bios_version, "F13a");
    }

    #[test]
    fn read_board_info_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let info = read_board_info_from(tmp.path());
        assert_eq!(info.vendor, "");
        assert_eq!(info.name, "");
        assert_eq!(info.bios_version, "");
    }
}
