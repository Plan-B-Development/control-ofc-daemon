//! Nouveau (open NVIDIA driver) discrete GPU detection — read-only (DEC-204).
//!
//! Scans hwmon devices for `name == "nouveau"` and resolves the PCI
//! Bus:Device.Function address for a stable fan identity. NVIDIA GPUs are
//! surfaced to the API under the vendor-level `nvidia_gpu:` source prefix
//! (shared with the proprietary NVML backend added in a later phase); this
//! module is the nouveau/hwmon detection leg.
//!
//! ## Read-only by design (Phase 1)
//!
//! nouveau *does* expose a writable `pwm1`/`pwm1_enable` on cards with
//! controllable fans, but Phase-1 NVIDIA support is telemetry-only: this
//! detector models only the readable `fan1_input` (RPM). The `nouveau` chip is
//! excluded from hwmon PWM-header and monitor-only-fan discovery via
//! [`crate::hwmon::is_gpu_owned_hwmon_chip`] (mirroring the `amdgpu` DEC-102
//! exclusion), so the profile engine can never drive that `pwm1`. GPU
//! temperatures flow through the normal sensor pipeline tagged
//! `SensorSource::NvidiaGpu` (see `discovery.rs`).
//!
//! Identity is deliberately minimal here — just enough to form a stable
//! `nvidia_gpu:<BDF>` fan id. PCI device-ID / marketing-name resolution and the
//! `/capabilities` + `/diagnostics/hardware` surfaces are added in the phase
//! that wires the NVIDIA capability (contrast `intel_gpu_detect.rs`).

use std::path::{Path, PathBuf};

use super::util::read_sysfs_string;

/// Detected nouveau-backed NVIDIA discrete GPU with a stable fan identity.
#[derive(Debug, Clone)]
pub struct NouveauGpuInfo {
    /// PCI Bus:Device.Function address (e.g. `0000:03:00.0`). Stable across reboots.
    pub pci_bdf: String,
    /// Path to the hwmon directory for this GPU.
    pub hwmon_path: PathBuf,
    /// Whether fan RPM reading is available (`fan1_input` exists).
    pub has_fan_rpm: bool,
}

/// Discover all nouveau-backed NVIDIA discrete GPUs by scanning hwmon devices.
///
/// `hwmon_root` allows test injection (defaults to `/sys/class/hwmon`). For each
/// hwmon device named `nouveau`, resolves the PCI identity and read-only fan
/// availability. Results are sorted by PCI BDF for deterministic ordering.
pub fn detect_nouveau_gpus(hwmon_root: &Path) -> Vec<NouveauGpuInfo> {
    let mut gpus = Vec::new();

    let entries = match std::fs::read_dir(hwmon_root) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("Cannot read hwmon root {}: {e}", hwmon_root.display());
            return gpus;
        }
    };

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
        if let Some(gpu) = detect_single_gpu(&hwmon_dir) {
            gpus.push(gpu);
        }
    }

    // Deterministic ordering by PCI BDF (mirrors the AMD/Intel detectors).
    gpus.sort_by(|a, b| a.pci_bdf.cmp(&b.pci_bdf));

    gpus
}

/// Attempt to detect a nouveau-backed NVIDIA discrete GPU from one hwmon dir.
fn detect_single_gpu(hwmon_dir: &Path) -> Option<NouveauGpuInfo> {
    let name = read_sysfs_string(&hwmon_dir.join("name")).ok()?;
    if name.trim() != "nouveau" {
        return None;
    }

    // Resolve the device symlink to a PCI path for a stable identity.
    let device_link = hwmon_dir.join("device");
    let pci_path = resolve_pci_path(&device_link)?;
    let pci_bdf = extract_pci_bdf(&pci_path)?;

    // nouveau exposes up to several tachometers; we surface only fan1 as the
    // GPU's aggregate fan, consistent with "one fan entity per GPU" (DEC-044)
    // and the read-only contract.
    let has_fan_rpm = hwmon_dir.join("fan1_input").exists();

    Some(NouveauGpuInfo {
        pci_bdf,
        hwmon_path: hwmon_dir.to_path_buf(),
        has_fan_rpm,
    })
}

/// Resolve the `device` symlink to the actual PCI device path.
fn resolve_pci_path(device_link: &Path) -> Option<PathBuf> {
    if !device_link.exists() {
        return None;
    }
    std::fs::canonicalize(device_link).ok()
}

/// Extract PCI BDF (Bus:Device.Function) from a resolved sysfs path.
fn extract_pci_bdf(path: &Path) -> Option<String> {
    for component in path.iter().rev() {
        if let Some(s) = component.to_str() {
            if is_pci_bdf(s) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Check if a string matches PCI BDF format: DDDD:BB:DD.F
fn is_pci_bdf(s: &str) -> bool {
    s.len() >= 12
        && s.chars().nth(4) == Some(':')
        && s.chars().nth(7) == Some(':')
        && s.chars().nth(10) == Some('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a fake nouveau GPU hwmon device with a PCI sysfs tree.
    fn create_fake_nouveau_gpu(
        base: &Path,
        hwmon_name: &str,
        pci_bdf: &str,
        fan_rpm: bool,
    ) -> PathBuf {
        let hwmon_dir = base.join(hwmon_name);
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "nouveau\n").unwrap();

        let pci_dir = base.join("pci_devices").join(pci_bdf);
        fs::create_dir_all(&pci_dir).unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&pci_dir, hwmon_dir.join("device")).unwrap();

        fs::write(hwmon_dir.join("temp1_input"), "45000\n").unwrap();
        if fan_rpm {
            fs::write(hwmon_dir.join("fan1_input"), "0\n").unwrap();
        }

        hwmon_dir
    }

    #[test]
    fn detect_nouveau_with_fan() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = create_fake_nouveau_gpu(tmp.path(), "hwmon4", "0000:03:00.0", true);

        let gpus = detect_nouveau_gpus(tmp.path());
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.pci_bdf, "0000:03:00.0");
        assert_eq!(gpu.hwmon_path, hwmon);
        assert!(gpu.has_fan_rpm);
    }

    #[test]
    fn detect_nouveau_without_fan() {
        // A passive/fanless card: detected as a GPU (for temps), but no fan.
        let tmp = tempfile::tempdir().unwrap();
        create_fake_nouveau_gpu(tmp.path(), "hwmon4", "0000:03:00.0", false);

        let gpus = detect_nouveau_gpus(tmp.path());
        assert_eq!(gpus.len(), 1);
        assert!(!gpus[0].has_fan_rpm);
    }

    #[test]
    fn skips_non_nouveau_devices() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, chip) in [
            ("hwmon0", "amdgpu"),
            ("hwmon1", "xe"),
            ("hwmon2", "k10temp"),
        ] {
            let dir = tmp.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("name"), format!("{chip}\n")).unwrap();
        }

        let gpus = detect_nouveau_gpus(tmp.path());
        assert!(gpus.is_empty());
    }

    #[test]
    fn no_device_symlink_skipped() {
        // A nouveau chip whose device symlink can't be resolved yields no BDF,
        // so it is skipped rather than producing a fan with an unstable id.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "nouveau\n").unwrap();

        let gpus = detect_nouveau_gpus(tmp.path());
        assert!(gpus.is_empty());
    }

    #[test]
    fn multiple_sorted_by_bdf() {
        let tmp = tempfile::tempdir().unwrap();
        // Higher BDF discovered first by hwmon index...
        create_fake_nouveau_gpu(tmp.path(), "hwmon0", "0000:0a:00.0", true);
        // ...lower BDF second.
        create_fake_nouveau_gpu(tmp.path(), "hwmon1", "0000:03:00.0", true);

        let gpus = detect_nouveau_gpus(tmp.path());
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].pci_bdf, "0000:03:00.0");
        assert_eq!(gpus[1].pci_bdf, "0000:0a:00.0");
    }

    #[test]
    fn pci_bdf_format_check() {
        assert!(is_pci_bdf("0000:03:00.0"));
        assert!(!is_pci_bdf("short"));
    }
}
