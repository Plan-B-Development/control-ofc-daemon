//! Read-only inventory of monitor-only fan tachometers (Phase 1).
//!
//! The existing PWM discovery (`pwm_discovery`) enumerates `pwmN` control
//! headers and pairs each with its same-index `fanN_input` for RPM. A fan
//! header that exposes only `fanN_input` with **no** matching `pwmN` (a
//! monitor-only tachometer) is therefore invisible to `/hwmon/headers`. This
//! module discovers those orphan tachometers so the inventory can surface every
//! visible fan RPM input.
//!
//! Read-only: never writes hardware. Mirrors the injected-`&Path` root and the
//! per-directory error tolerance of `discovery` / `pwm_discovery`, so tests
//! point it at a `tempfile::tempdir()` instead of the real `/sys/class/hwmon`.

use std::path::{Path, PathBuf};

use crate::error::HwmonError;

use super::util::{device_id_from_path, read_sysfs_string};

/// A monitor-only fan tachometer: an hwmon `fanN_input` with no matching
/// `pwmN` control. RPM-readable but not controllable.
#[derive(Debug, Clone)]
pub struct FanInputDescriptor {
    /// Stable identifier `hwmon:<chip>:<device_id>:fan<N>:<label>` — the hwmon
    /// index is deliberately absent, matching the sensor/PWM id scheme.
    pub id: String,
    /// Hwmon chip name (e.g. `nct6798`).
    pub chip_name: String,
    /// The N in `fanN_input`.
    pub fan_index: u8,
    /// Human-readable label from `fanN_label`, else `fan{N}`.
    pub label: String,
    /// Absolute path of the `fanN_input` file.
    ///
    /// Added for AIO Phase 8 Batch 1: control-path discovery observes
    /// monitor-only tachs during its own measurement window, and without a path
    /// it would have to re-resolve one from `chip_name` + `fan_index`, which is
    /// how a second, drifting copy of the discovery rules gets created (DEC-276).
    ///
    /// **Not on the wire.** `FanInputEntry::from` does not carry it, deliberately
    /// — a sysfs path is a daemon-internal handle, and the GUI has no business
    /// receiving one when the architecture forbids it touching sysfs at all.
    pub input_path: PathBuf,
}

/// Discover monitor-only fan tachometers under a given sysfs hwmon root.
///
/// The `hwmon_root` parameter allows injecting a test fixture directory instead
/// of the real `/sys/class/hwmon`. Read-only; never writes hardware. Results are
/// sorted by id for a deterministic wire order (matching the sensor/PWM
/// builders). A single unreadable device directory is skipped, not fatal.
pub fn discover_monitor_only_fans(
    hwmon_root: &Path,
) -> Result<Vec<FanInputDescriptor>, HwmonError> {
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
        match discover_device_monitor_only_fans(&hwmon_dir) {
            Ok(fans) => descriptors.extend(fans),
            Err(e) => {
                log::warn!(
                    "Skipping monitor-only fan discovery for {}: {e}",
                    hwmon_dir.display()
                );
            }
        }
    }

    descriptors.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(descriptors)
}

/// Discover monitor-only fans for a single hwmon device directory.
fn discover_device_monitor_only_fans(
    hwmon_dir: &Path,
) -> Result<Vec<FanInputDescriptor>, HwmonError> {
    let chip_name = read_sysfs_string(&hwmon_dir.join("name"))?
        .trim()
        .to_string();

    // GPU-owned fans are never surfaced in the hwmon inventory — consistent
    // with `pwm_discovery`. amdgpu (DEC-102) and nouveau (DEC-204) fan RPM is
    // surfaced via the `amd_gpu:` / `nvidia_gpu:` prefixes + the GPU endpoints,
    // not the hwmon monitor-only-fan inventory.
    if crate::hwmon::is_gpu_owned_hwmon_chip(&chip_name) {
        return Ok(Vec::new());
    }

    let device_id = resolve_device_id(hwmon_dir);

    // Enumerate fanN_input files (fan1_input, fan2_input, ...).
    let entries = std::fs::read_dir(hwmon_dir).map_err(|e| HwmonError::ReadError {
        path: hwmon_dir.display().to_string(),
        message: e.to_string(),
    })?;

    let mut fan_indices: Vec<u8> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_prefix("fan")
                .and_then(|s| s.strip_suffix("_input"))
                .and_then(|n| n.parse::<u8>().ok())
        })
        .collect();

    fan_indices.sort_unstable();

    let mut fans = Vec::new();
    for fan_index in fan_indices {
        // Same-index pairing (mirrors `pwm_discovery`, which reads
        // `fan{pwm_index}_input`): a `fanN_input` is "controllable" iff `pwmN`
        // exists. Skip those — they are already surfaced as PWM headers with
        // `rpm_available = true`. What remains is monitor-only.
        if hwmon_dir.join(format!("pwm{fan_index}")).exists() {
            continue;
        }

        let label = read_fan_label(hwmon_dir, fan_index);
        let id = build_stable_id(&chip_name, &device_id, fan_index, &label);

        fans.push(FanInputDescriptor {
            id,
            chip_name: chip_name.clone(),
            fan_index,
            label,
            input_path: hwmon_dir.join(format!("fan{fan_index}_input")),
        });
    }

    Ok(fans)
}

/// Read `fanN_label`, falling back to `fan{N}`.
fn read_fan_label(hwmon_dir: &Path, fan_index: u8) -> String {
    let label_path = hwmon_dir.join(format!("fan{fan_index}_label"));
    if let Ok(label) = read_sysfs_string(&label_path) {
        let trimmed = label.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    format!("fan{fan_index}")
}

/// Build a stable ID for a monitor-only fan — matches the PWM header scheme
/// (`hwmon:<chip>:<device_id>:fan<N>:<label>`), with no hwmon index.
fn build_stable_id(chip_name: &str, device_id: &str, fan_index: u8, label: &str) -> String {
    format!("hwmon:{chip_name}:{device_id}:fan{fan_index}:{label}")
}

/// Resolve the stable device ID from the sysfs `device` symlink. Kept local
/// (mirrors the private `pwm_discovery::resolve_device_id` / the inline logic in
/// `discovery`) rather than widening a private helper's visibility for one
/// caller — the existing code already carries two copies of this pattern.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a hwmon fixture with fan tachometers and optional matching pwm
    /// controls. Each entry: `(index, label, has_matching_pwm)`.
    fn create_fan_fixture(
        base: &Path,
        dir_name: &str,
        chip_name: &str,
        fans: &[(u8, Option<&str>, bool)],
    ) -> PathBuf {
        let hwmon_dir = base.join(dir_name);
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), chip_name).unwrap();
        for &(index, label, has_pwm) in fans {
            fs::write(hwmon_dir.join(format!("fan{index}_input")), "1200\n").unwrap();
            if let Some(lbl) = label {
                fs::write(
                    hwmon_dir.join(format!("fan{index}_label")),
                    format!("{lbl}\n"),
                )
                .unwrap();
            }
            if has_pwm {
                fs::write(hwmon_dir.join(format!("pwm{index}")), "128\n").unwrap();
            }
        }
        hwmon_dir
    }

    #[test]
    fn discovers_orphan_fan_with_no_pwm() {
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(
            tmp.path(),
            "hwmon0",
            "nct6798",
            &[(2, Some("AUX_FAN"), false)],
        );

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].fan_index, 2);
        assert_eq!(fans[0].label, "AUX_FAN");
        assert_eq!(fans[0].chip_name, "nct6798");
        assert!(fans[0].id.starts_with("hwmon:nct6798:"));
        assert!(fans[0].id.contains("fan2"));
        assert!(fans[0].id.contains("AUX_FAN"));
    }

    #[test]
    fn skips_fan_with_matching_pwm() {
        // fan1_input + pwm1 → controllable; surfaced as a PWM header elsewhere.
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(
            tmp.path(),
            "hwmon0",
            "it8696",
            &[(1, Some("CPU_FAN"), true)],
        );

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert!(
            fans.is_empty(),
            "a fan with a matching pwm must be excluded: {fans:#?}"
        );
    }

    #[test]
    fn mixed_pairing_returns_only_orphans() {
        // fan1 has pwm1 (controllable → excluded); fan3 has no pwm3 (orphan).
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(
            tmp.path(),
            "hwmon0",
            "nct6798",
            &[(1, Some("CPU_FAN"), true), (3, Some("PUMP_TACH"), false)],
        );

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].fan_index, 3);
        assert_eq!(fans[0].label, "PUMP_TACH");
    }

    #[test]
    fn different_index_pwm_does_not_cover_fan() {
        // fan3_input with only pwm1 present → fan3 is orphan (same-index rule).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), "nct6798").unwrap();
        fs::write(dir.join("pwm1"), "128\n").unwrap();
        fs::write(dir.join("fan1_input"), "1000\n").unwrap();
        fs::write(dir.join("fan3_input"), "800\n").unwrap();

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].fan_index, 3);
    }

    #[test]
    fn missing_label_falls_back_to_fan_index() {
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(tmp.path(), "hwmon0", "nct6798", &[(2, None, false)]);

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].label, "fan2");
    }

    #[test]
    fn amdgpu_fans_excluded() {
        // DEC-102: GPU fans are owned by the GPU subsystem, never by hwmon.
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(tmp.path(), "hwmon0", "amdgpu", &[(1, None, false)]);

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert!(fans.is_empty(), "amdgpu fans must be excluded: {fans:#?}");
    }

    #[test]
    fn nouveau_fans_excluded() {
        // DEC-204: nouveau GPU fans are owned by the GPU subsystem (surfaced via
        // the `nvidia_gpu:` prefix), never by the hwmon monitor-only inventory.
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(tmp.path(), "hwmon0", "nouveau", &[(1, None, false)]);

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert!(fans.is_empty(), "nouveau fans must be excluded: {fans:#?}");
    }

    #[test]
    fn malformed_name_skips_directory_not_fatal() {
        // A hwmon dir with no readable `name` is skipped; a sibling valid dir
        // still yields its orphan fan (per-directory error tolerance).
        let tmp = tempfile::tempdir().unwrap();
        // hwmon0: a fan but no `name` file → skipped.
        let bad = tmp.path().join("hwmon0");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("fan1_input"), "1000\n").unwrap();
        // hwmon1: valid.
        create_fan_fixture(
            tmp.path(),
            "hwmon1",
            "nct6798",
            &[(2, Some("SYS_FAN"), false)],
        );

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].chip_name, "nct6798");
    }

    #[test]
    fn results_sorted_by_id_across_chips() {
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(tmp.path(), "hwmon0", "nct6798", &[(2, Some("zzz"), false)]);
        create_fan_fixture(tmp.path(), "hwmon1", "it8696", &[(2, Some("aaa"), false)]);

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert_eq!(fans.len(), 2);
        // Deterministic wire order: sorted by id ("hwmon:it8696…" < "hwmon:nct6798…").
        assert!(fans[0].id < fans[1].id);
        assert_eq!(fans[0].chip_name, "it8696");
    }

    #[test]
    fn stable_id_omits_hwmon_index() {
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(
            tmp.path(),
            "hwmon7",
            "nct6798",
            &[(2, Some("SYS_FAN"), false)],
        );

        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert_eq!(fans.len(), 1);
        assert!(!fans[0].id.contains("hwmon7"));
        assert!(fans[0].id.starts_with("hwmon:nct6798:"));
    }

    #[test]
    fn empty_root_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert!(fans.is_empty());
    }

    #[test]
    fn all_fans_pwm_paired_yields_nothing() {
        // Every fan has a matching pwm → nothing monitor-only.
        let tmp = tempfile::tempdir().unwrap();
        create_fan_fixture(
            tmp.path(),
            "hwmon0",
            "it8696",
            &[(1, Some("CPU_FAN"), true), (2, Some("SYS_FAN"), true)],
        );
        let fans = discover_monitor_only_fans(tmp.path()).unwrap();
        assert!(fans.is_empty());
    }
}
