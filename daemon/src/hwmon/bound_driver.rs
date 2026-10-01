//! The kernel driver actually bound to each hwmon device (`BRD-g`, DEC-469).
//!
//! The chip name does not say which driver bound a chip. Mainline `nct6683` and
//! the out-of-tree `nct6687` both register their hwmon device as `nct6683`,
//! `nct6686` or `nct6687` by chip kind (`nct6683_device_names[]` /
//! `nct6687_device_names[]`, read 2026-10-01), so the name-keyed
//! [`crate::hwmon::chip_db::expected_driver`] is a guess for that family in
//! both directions. The two drivers register different *platform driver* names
//! (`DRVNAME` is `"nct6683"` in-kernel and `"nct6687"` out-of-tree), and that
//! name is what `/sys/class/hwmon/hwmonN/device/driver` links to — so the link
//! is the observation, and this module reads it.
//!
//! Read per scan, never stored at discovery: a rebind moves the chip to a new
//! driver. `/diagnostics/hardware` scans on every request; the Super-I/O and
//! readiness reports scan once per shared assessment, so their answer can be up
//! to `ASSESSMENT_TTL` old. Read-only sysfs — one `readlink` per hwmon device.

use std::path::Path;

use crate::hwmon::chip_name::read_chip_name;
use crate::hwmon::discovery::device_id_for_hwmon_dir;

/// One hwmon device and the driver bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedDriver {
    /// Canonical chip name (DEC-442) — the same spelling every id carries.
    pub chip_name: String,
    /// The `device_id` half of the chip's stable ids.
    pub device_id: String,
    /// The bound kernel driver's name, e.g. `"nct6683"`, `"it87"`.
    pub driver: String,
}

/// The kernel driver bound to one hwmon device: the last component of the
/// `device/driver` symlink's target. `None` when the device has no `driver`
/// link (virtual devices such as `nvme` and `ath12k` publish none) or it does
/// not read — never a guess.
pub fn read_bound_driver(hwmon_dir: &Path) -> Option<String> {
    let target = std::fs::read_link(hwmon_dir.join("device").join("driver")).ok()?;
    let name = target.file_name()?.to_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Every hwmon device under `hwmon_root` whose bound driver can be read.
///
/// A device whose `name` or driver link does not read is left out, so a caller
/// looking one up gets `None` — "not observed" — rather than a wrong answer.
/// Sorted by directory, so the first match for a chip name is deterministic.
pub fn scan_bound_drivers(hwmon_root: &Path) -> Vec<ObservedDriver> {
    let Ok(entries) = std::fs::read_dir(hwmon_root) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hwmon"))
        })
        .collect();
    dirs.sort();
    dirs.iter()
        .filter_map(|dir| {
            let chip_name = read_chip_name(dir).ok()?.canonical;
            let driver = read_bound_driver(dir)?;
            Some(ObservedDriver {
                chip_name,
                device_id: device_id_for_hwmon_dir(dir),
                driver,
            })
        })
        .collect()
}

/// The driver bound to the chip `(chip_name, device_id)`, from a scan.
pub fn driver_for_device<'a>(
    observed: &'a [ObservedDriver],
    chip_name: &str,
    device_id: &str,
) -> Option<&'a str> {
    observed
        .iter()
        .find(|o| o.chip_name == chip_name && o.device_id == device_id)
        .map(|o| o.driver.as_str())
}

/// The driver bound to the first chip named `chip_name` (case-insensitive),
/// from a scan. For callers keyed by name alone, such as the Super-I/O report.
pub fn driver_for_chip<'a>(observed: &'a [ObservedDriver], chip_name: &str) -> Option<&'a str> {
    let want = chip_name.trim().to_lowercase();
    observed
        .iter()
        .find(|o| o.chip_name.to_lowercase() == want)
        .map(|o| o.driver.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// A fake `/sys/class/hwmon/<dir>` with a `name`, a `device` directory and,
    /// when `driver` is given, a `device/driver` symlink to
    /// `<root>/bus/platform/drivers/<driver>` — the shape the kernel publishes.
    fn chip(root: &Path, dir: &str, name: &str, device: &str, driver: Option<&str>) {
        let hw = root.join("class").join(dir);
        let dev = root.join("devices").join(device);
        fs::create_dir_all(&hw).unwrap();
        fs::create_dir_all(&dev).unwrap();
        fs::write(hw.join("name"), format!("{name}\n")).unwrap();
        symlink(&dev, hw.join("device")).unwrap();
        if let Some(d) = driver {
            let drv = root.join("bus/platform/drivers").join(d);
            fs::create_dir_all(&drv).unwrap();
            symlink(&drv, dev.join("driver")).unwrap();
        }
    }

    #[test]
    fn reads_the_driver_link_not_the_chip_name() {
        // An MSI NCT6687D bound by the in-kernel nct6683 publishes hwmon name
        // "nct6687"; only the link says which driver it is.
        let tmp = tempfile::tempdir().unwrap();
        chip(
            tmp.path(),
            "hwmon3",
            "nct6687",
            "nct6683.2592",
            Some("nct6683"),
        );
        let got = scan_bound_drivers(&tmp.path().join("class"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].chip_name, "nct6687");
        assert_eq!(got[0].driver, "nct6683");
        assert_eq!(
            got[0].device_id,
            device_id_for_hwmon_dir(&tmp.path().join("class/hwmon3")),
            "keyed by the same device_id the header ids carry"
        );
    }

    #[test]
    fn a_device_with_no_driver_link_is_not_observed() {
        let tmp = tempfile::tempdir().unwrap();
        chip(tmp.path(), "hwmon0", "nvme", "nvme0", None);
        chip(tmp.path(), "hwmon4", "it8696", "it87.2624", Some("it87"));
        let got = scan_bound_drivers(&tmp.path().join("class"));
        assert_eq!(got.len(), 1, "nvme has no driver link: {got:?}");
        assert_eq!(got[0].chip_name, "it8696");
        assert_eq!(got[0].driver, "it87");
        assert!(read_bound_driver(&tmp.path().join("class/hwmon0")).is_none());
    }

    #[test]
    fn the_chip_name_is_canonicalised_like_every_id() {
        // it87 v2.0 suffix (DEC-442): looked up by the canonical spelling.
        let tmp = tempfile::tempdir().unwrap();
        chip(
            tmp.path(),
            "hwmon4",
            "it8696_a008090a",
            "it87.2624",
            Some("it87"),
        );
        let got = scan_bound_drivers(&tmp.path().join("class"));
        assert_eq!(driver_for_chip(&got, "it8696"), Some("it87"));
    }

    #[test]
    fn lookups_match_on_name_and_device() {
        let observed = vec![
            ObservedDriver {
                chip_name: "nct6686".into(),
                device_id: "nct6683.2592".into(),
                driver: "nct6683".into(),
            },
            ObservedDriver {
                chip_name: "nct6799".into(),
                device_id: "nct6775.656".into(),
                driver: "nct6775".into(),
            },
        ];
        assert_eq!(
            driver_for_device(&observed, "nct6686", "nct6683.2592"),
            Some("nct6683")
        );
        assert_eq!(driver_for_device(&observed, "nct6686", "nct6775.656"), None);
        assert_eq!(driver_for_chip(&observed, "NCT6799"), Some("nct6775"));
        assert_eq!(driver_for_chip(&observed, "it8696"), None);
    }

    #[test]
    fn a_missing_root_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan_bound_drivers(&tmp.path().join("absent")).is_empty());
    }
}
