//! Intel discrete GPU detection and identity resolution (DEC-121).
//!
//! Scans hwmon devices for `name == "xe"` (Battlemage / Xe2 and later) or
//! `name == "i915"` (Alchemist / Arc A-series), resolves the PCI
//! Bus:Device.Function address for stable identity, reads PCI device/class/
//! revision IDs, and maps known device IDs to a marketing name.
//!
//! ## Why chip-name detection is unambiguous
//!
//! Both the `xe` and `i915` kernel drivers register their hwmon device
//! **only for discrete GPUs** — the registration functions bail out early for
//! integrated graphics:
//!
//! ```c
//! // drivers/gpu/drm/xe/xe_hwmon.c and i915/i915_hwmon.c
//! /* hwmon is available only for dGfx */
//! if (!IS_DGFX(...))
//!     return;
//! ```
//!
//! So an hwmon chip named `xe`/`i915` is *definitionally* a discrete Intel
//! GPU; integrated Xe/UHD graphics expose no such node. (Verified against
//! torvalds/linux master.)
//!
//! ## Read-only by design
//!
//! Intel discrete GPU fan control is handled autonomously by on-card firmware
//! (the `fan_control_*.bin` blob in linux-firmware). The kernel exposes
//! `fanN_input` (RPM) as read-only — there is **no `pwm` attribute and no fan
//! write callback** anywhere in either driver. This detector therefore models
//! only what can be read; there is no `fan_curve_path`, `pwm`, `pwm_enable`,
//! overdrive, or shutdown-reset machinery (contrast `gpu_detect.rs` for AMD).

use std::path::{Path, PathBuf};

use super::util::read_sysfs_string;

/// PCI base class for VGA compatible controller.
///
/// Note: Intel integrated *and* discrete GPUs both report this class, so it is
/// NOT a discrete/integrated discriminator — the hwmon chip name is (see
/// module docs). It is recorded only for the `is_discrete` honesty flag.
const PCI_CLASS_VGA: u32 = 0x030000;

/// Detected Intel discrete GPU with stable identity and read-only fan state.
#[derive(Debug, Clone)]
pub struct IntelGpuInfo {
    /// PCI Bus:Device.Function address (e.g. `0000:03:00.0`). Stable across reboots.
    pub pci_bdf: String,
    /// PCI device ID (e.g. `0xE20B` for Arc B580).
    pub pci_device_id: u16,
    /// PCI revision.
    pub pci_revision: u8,
    /// PCI class code (e.g. `0x030000` for VGA).
    pub pci_class: u32,
    /// Marketing name (e.g. "Arc B580") or None if the device ID is unknown.
    pub marketing_name: Option<String>,
    /// Kernel driver backing this GPU: `"xe"` or `"i915"`.
    pub driver: String,
    /// Path to the hwmon directory for this GPU.
    pub hwmon_path: PathBuf,
    /// Whether this is a discrete GPU (VGA class). Always true in practice
    /// because the backing hwmon node is DGFX-gated, but read honestly from
    /// the PCI class for forward-proofing.
    pub is_discrete: bool,
    /// Whether fan RPM reading is available (`fan1_input` exists).
    pub has_fan_rpm: bool,
}

impl IntelGpuInfo {
    /// User-facing display label: specific model name if the PCI device ID is
    /// recognised (e.g. "Arc B580"), otherwise the generic "Intel D-GPU".
    pub fn display_label(&self) -> String {
        self.marketing_name
            .clone()
            .unwrap_or_else(|| "Intel D-GPU".to_string())
    }

    /// Fan control method available on this GPU.
    ///
    /// - `"read_only"`: fan RPM is readable but there is no write path (the
    ///   only outcome for an Intel GPU that exposes a fan).
    /// - `"none"`: no fan interface at all (`fan1_input` absent).
    ///
    /// Intel GPUs never return a writable method — fan control is firmware-
    /// managed (DEC-121).
    pub fn fan_control_method(&self) -> &'static str {
        if self.has_fan_rpm {
            "read_only"
        } else {
            "none"
        }
    }
}

/// Discover all Intel discrete GPUs by scanning hwmon devices.
///
/// The `hwmon_root` parameter allows test injection (defaults to
/// `/sys/class/hwmon`). For each hwmon device named `xe` or `i915`, resolves
/// PCI identity and read-only fan availability.
pub fn detect_intel_gpus(hwmon_root: &Path) -> Vec<IntelGpuInfo> {
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

    // Sort: GPUs with a fan interface first, then discrete, then by PCI BDF —
    // mirrors the AMD ordering so `select_primary_intel_gpu` prefers the most
    // useful card.
    gpus.sort_by(|a, b| {
        b.has_fan_rpm
            .cmp(&a.has_fan_rpm)
            .then(b.is_discrete.cmp(&a.is_discrete))
            .then(a.pci_bdf.cmp(&b.pci_bdf))
    });

    gpus
}

/// Attempt to detect an Intel discrete GPU from a single hwmon directory.
fn detect_single_gpu(hwmon_dir: &Path) -> Option<IntelGpuInfo> {
    let name = read_sysfs_string(&hwmon_dir.join("name")).ok()?;
    let driver = name.trim();
    if driver != "xe" && driver != "i915" {
        return None;
    }

    // Resolve device symlink to PCI path for stable identity.
    let device_link = hwmon_dir.join("device");
    let pci_path = resolve_pci_path(&device_link)?;
    let pci_bdf = extract_pci_bdf(&pci_path)?;

    let pci_device_id = read_pci_hex16(&pci_path.join("device")).unwrap_or(0);
    let pci_revision = read_pci_hex_u8(&pci_path.join("revision")).unwrap_or(0);
    let pci_class = read_pci_hex32(&pci_path.join("class")).unwrap_or(0);

    let is_discrete = (pci_class & 0xFFFF00) == PCI_CLASS_VGA;
    let marketing_name = lookup_marketing_name(pci_device_id);

    // Intel exposes up to three tachometers (fan1/2/3_input); we surface only
    // fan1 as the GPU's aggregate fan, consistent with "one fan entity per
    // GPU" (DEC-044) and the read-only contract.
    let has_fan_rpm = hwmon_dir.join("fan1_input").exists();

    Some(IntelGpuInfo {
        pci_bdf,
        pci_device_id,
        pci_revision,
        pci_class,
        marketing_name,
        driver: driver.to_string(),
        hwmon_path: hwmon_dir.to_path_buf(),
        is_discrete,
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

/// Read a PCI sysfs hex attribute as u16 (e.g. `0xE20B`).
fn read_pci_hex16(path: &Path) -> Option<u16> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    u16::from_str_radix(trimmed, 16).ok()
}

/// Read a PCI sysfs hex attribute as u8 (revision).
fn read_pci_hex_u8(path: &Path) -> Option<u8> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    u8::from_str_radix(trimmed, 16).ok()
}

/// Read a PCI sysfs hex attribute as u32 (e.g. `0x030000`).
fn read_pci_hex32(path: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    u32::from_str_radix(trimmed, 16).ok()
}

/// Map a PCI device ID (vendor 0x8086) to an Intel discrete GPU marketing name.
///
/// Conservative on purpose: the kernel ID headers carry raw device IDs only,
/// not per-SKU marketing names. Only `0xE20B` → "Arc B580" is authoritatively
/// verifiable (lspci `8086:e20b` reports "Battlemage G21 [Arc B580]", and the
/// linux-firmware fan-control blob is named `fan_control_8086_e20b_*.bin`).
///
/// All other Battlemage IDs (e.g. B570 and the workstation `0xE22x` group) and
/// Alchemist (DG2) IDs deliberately return `None` and fall back to the generic
/// "Intel D-GPU" display label, rather than guess an unverified SKU name. This
/// honours the project rule "do not claim … unless … truthful". REVIEW: extend
/// the table only with device-ID→name pairs confirmed against an authoritative
/// source (e.g. systemd/hwdata `pci.ids`).
///
/// Verified Battlemage (BMG) discrete device IDs from
/// `include/drm/intel/pciids.h` (`INTEL_BMG_IDS`): 0xE202, 0xE209, 0xE20B,
/// 0xE20C, 0xE20D, 0xE210, 0xE211, 0xE212, 0xE216, 0xE220, 0xE221, 0xE222,
/// 0xE223.
fn lookup_marketing_name(device_id: u16) -> Option<String> {
    match device_id {
        // Battlemage G21 — Arc B580 (verified). The only per-SKU mapping we
        // can ground in an authoritative source.
        0xE20B => Some("Arc B580".into()),
        _ => {
            log::debug!("Unknown/unmapped Intel GPU device ID: {device_id:#06x}");
            None
        }
    }
}

/// Select the primary (preferred) Intel GPU from detected GPUs.
///
/// Already sorted by `detect_intel_gpus`: fan interface > discrete > PCI BDF.
/// Returns `None` if no Intel discrete GPUs are detected.
pub fn select_primary_intel_gpu(gpus: &[IntelGpuInfo]) -> Option<&IntelGpuInfo> {
    gpus.first()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a fake Intel GPU hwmon device with a PCI sysfs tree.
    #[allow(clippy::too_many_arguments)]
    fn create_fake_intel_gpu(
        base: &Path,
        hwmon_name: &str,
        driver: &str,
        pci_bdf: &str,
        device_id: &str,
        revision: &str,
        class: &str,
        fan_rpm: bool,
    ) -> PathBuf {
        let hwmon_dir = base.join(hwmon_name);
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), format!("{driver}\n")).unwrap();

        let pci_dir = base.join("pci_devices").join(pci_bdf);
        fs::create_dir_all(&pci_dir).unwrap();
        fs::write(pci_dir.join("device"), format!("{device_id}\n")).unwrap();
        fs::write(pci_dir.join("revision"), format!("{revision}\n")).unwrap();
        fs::write(pci_dir.join("class"), format!("{class}\n")).unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&pci_dir, hwmon_dir.join("device")).unwrap();

        // xe exposes temps starting at temp2 (no temp1); i915 at temp1.
        if driver == "xe" {
            fs::write(hwmon_dir.join("temp2_input"), "45000\n").unwrap();
        } else {
            fs::write(hwmon_dir.join("temp1_input"), "45000\n").unwrap();
        }

        if fan_rpm {
            fs::write(hwmon_dir.join("fan1_input"), "0\n").unwrap();
        }

        hwmon_dir
    }

    // ── Marketing-name table ────────────────────────────────────────────

    #[test]
    fn b580_device_id_maps_to_name() {
        assert_eq!(lookup_marketing_name(0xE20B), Some("Arc B580".to_string()));
    }

    #[test]
    fn other_battlemage_ids_fall_back_to_generic() {
        // Verified BMG IDs that we deliberately do not name per-SKU.
        for id in [0xE202u16, 0xE209, 0xE20C, 0xE210, 0xE216, 0xE220, 0xE223] {
            assert_eq!(lookup_marketing_name(id), None, "id {id:#06x}");
        }
    }

    #[test]
    fn unknown_id_returns_none() {
        assert_eq!(lookup_marketing_name(0x1234), None);
    }

    // ── Detection integration ───────────────────────────────────────────

    #[test]
    fn detect_battlemage_b580_via_xe() {
        let tmp = tempfile::tempdir().unwrap();
        create_fake_intel_gpu(
            tmp.path(),
            "hwmon3",
            "xe",
            "0000:03:00.0",
            "0xe20b",
            "0x00",
            "0x030000",
            true, // fan_rpm
        );

        let gpus = detect_intel_gpus(tmp.path());
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.pci_device_id, 0xE20B);
        assert_eq!(gpu.driver, "xe");
        assert_eq!(gpu.marketing_name.as_deref(), Some("Arc B580"));
        assert_eq!(gpu.display_label(), "Arc B580");
        assert!(gpu.is_discrete);
        assert!(gpu.has_fan_rpm);
        assert_eq!(gpu.fan_control_method(), "read_only");
        assert_eq!(gpu.pci_bdf, "0000:03:00.0");
    }

    #[test]
    fn detect_alchemist_via_i915() {
        let tmp = tempfile::tempdir().unwrap();
        create_fake_intel_gpu(
            tmp.path(),
            "hwmon2",
            "i915",
            "0000:03:00.0",
            "0x56a0", // Arc A770 (unmapped → generic label)
            "0x08",
            "0x030000",
            true,
        );

        let gpus = detect_intel_gpus(tmp.path());
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.driver, "i915");
        // Unmapped device ID → generic, truthful label.
        assert!(gpu.marketing_name.is_none());
        assert_eq!(gpu.display_label(), "Intel D-GPU");
        assert!(gpu.has_fan_rpm);
        assert_eq!(gpu.fan_control_method(), "read_only");
    }

    #[test]
    fn detect_intel_gpu_without_fan_is_none_method() {
        // A discrete card whose platform/firmware reports no fans: detected as
        // a GPU (for temps), but fan_control_method is "none".
        let tmp = tempfile::tempdir().unwrap();
        create_fake_intel_gpu(
            tmp.path(),
            "hwmon3",
            "xe",
            "0000:03:00.0",
            "0xe20b",
            "0x00",
            "0x030000",
            false, // no fan1_input
        );

        let gpus = detect_intel_gpus(tmp.path());
        assert_eq!(gpus.len(), 1);
        assert!(!gpus[0].has_fan_rpm);
        assert_eq!(gpus[0].fan_control_method(), "none");
    }

    #[test]
    fn skips_non_intel_devices() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "amdgpu\n").unwrap();
        let hwmon1 = tmp.path().join("hwmon1");
        fs::create_dir_all(&hwmon1).unwrap();
        fs::write(hwmon1.join("name"), "k10temp\n").unwrap();

        let gpus = detect_intel_gpus(tmp.path());
        assert!(gpus.is_empty());
    }

    #[test]
    fn no_device_symlink_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "xe\n").unwrap();

        let gpus = detect_intel_gpus(tmp.path());
        assert!(gpus.is_empty());
    }

    #[test]
    fn fan_bearing_gpu_preferred_as_primary() {
        let tmp = tempfile::tempdir().unwrap();
        // Fanless first by name order...
        create_fake_intel_gpu(
            tmp.path(),
            "hwmon0",
            "xe",
            "0000:01:00.0",
            "0xe202",
            "0x00",
            "0x030000",
            false,
        );
        // ...fan-bearing card second.
        create_fake_intel_gpu(
            tmp.path(),
            "hwmon1",
            "xe",
            "0000:03:00.0",
            "0xe20b",
            "0x00",
            "0x030000",
            true,
        );

        let gpus = detect_intel_gpus(tmp.path());
        assert_eq!(gpus.len(), 2);
        let primary = select_primary_intel_gpu(&gpus).unwrap();
        assert!(primary.has_fan_rpm);
        assert_eq!(primary.pci_bdf, "0000:03:00.0");
    }

    #[test]
    fn select_primary_empty_is_none() {
        assert!(select_primary_intel_gpu(&[]).is_none());
    }

    #[test]
    fn pci_bdf_format_check() {
        assert!(is_pci_bdf("0000:03:00.0"));
        assert!(!is_pci_bdf("short"));
    }
}
