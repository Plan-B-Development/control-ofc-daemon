//! AMD GPU detection and identity resolution.
//!
//! Scans hwmon devices for `name == "amdgpu"`, resolves the PCI Bus:Device.Function
//! address for stable identity, reads PCI device/class/revision IDs, maps to a
//! marketing name, and detects PMFW fan_curve support for RDNA3+ GPUs.

use std::path::{Path, PathBuf};

use super::util::read_sysfs_string;

/// PCI base class for VGA compatible controller (discrete GPU).
const PCI_CLASS_VGA: u32 = 0x030000;

/// PCI vendor ID for AMD/ATI.
const PCI_VENDOR_AMD: u16 = 0x1002;

/// Path to the amdgpu ppfeaturemask kernel parameter.
const PPFEATUREMASK_PATH: &str = "/sys/module/amdgpu/parameters/ppfeaturemask";

/// Sysfs directory that exists when the amdgpu kernel module is loaded.
const AMDGPU_MODULE_PATH: &str = "/sys/module/amdgpu";

/// Sysfs root listing all PCI devices (each entry is a BDF symlink).
const PCI_DEVICES_ROOT: &str = "/sys/bus/pci/devices";

/// Bit 14 in ppfeaturemask enables overdrive (required for gpu_od/ sysfs tree).
const PP_OVERDRIVE_MASK: u32 = 0x4000;

/// Detected AMD GPU with stable identity and capabilities.
#[derive(Debug, Clone)]
pub struct AmdGpuInfo {
    /// PCI Bus:Device.Function address (e.g. `0000:03:00.0`). Stable across reboots.
    pub pci_bdf: String,
    /// PCI device ID (e.g. `0x7550` for Navi 48).
    pub pci_device_id: u16,
    /// PCI revision (e.g. `0xC0` for RX 9070 XT, `0xC3` for RX 9070).
    pub pci_revision: u8,
    /// PCI class code (e.g. `0x030000` for VGA).
    pub pci_class: u32,
    /// Marketing name (e.g. "RX 9070 XT") or None if unknown.
    pub marketing_name: Option<String>,
    /// Path to the hwmon directory for this GPU.
    pub hwmon_path: PathBuf,
    /// Path to the PMFW fan_curve file, if available (RDNA3+ with overdrive enabled).
    pub fan_curve_path: Option<PathBuf>,
    /// Path to the PMFW fan_zero_rpm_enable file, if available.
    pub fan_zero_rpm_path: Option<PathBuf>,
    /// Whether this is a discrete GPU (VGA class) vs render-only.
    pub is_discrete: bool,
    /// Whether fan RPM reading is available (fan1_input exists).
    pub has_fan_rpm: bool,
    /// Whether PWM value file exists (pwm1) — does NOT imply writable.
    pub has_pwm: bool,
    /// Whether pwm1_enable exists (required for legacy hwmon manual mode).
    pub has_pwm_enable: bool,
    /// Whether the amdgpu overdrive feature is enabled in ppfeaturemask.
    pub overdrive_enabled: bool,
}

impl AmdGpuInfo {
    /// User-facing display label following the product rule:
    /// specific model name if known (e.g. "9070XT"), otherwise "AMD D-GPU".
    pub fn display_label(&self) -> String {
        if let Some(ref name) = self.marketing_name {
            // Compact label: strip "RX " prefix, remove spaces
            name.replace("RX ", "").replace(' ', "")
        } else {
            "AMD D-GPU".to_string()
        }
    }

    /// Fan control method available on this GPU.
    ///
    /// - `"pmfw_curve"`: PMFW fan_curve exists (RDNA3+ with overdrive enabled)
    /// - `"hwmon_pwm"`: pwm1 AND pwm1_enable both exist (pre-RDNA3)
    /// - `"read_only"`: can read fan RPM but no write path available
    /// - `"none"`: no fan interface at all
    pub fn fan_control_method(&self) -> &'static str {
        if self.fan_curve_path.is_some() {
            "pmfw_curve"
        } else if self.has_pwm && self.has_pwm_enable {
            "hwmon_pwm"
        } else if self.has_fan_rpm || self.has_pwm {
            "read_only"
        } else {
            "none"
        }
    }

    /// Whether the legacy hwmon `pwm1` write path is functional on this GPU.
    ///
    /// `pwm1` existing alone is not sufficient — RDNA3/RDNA4 GPUs without
    /// `amdgpu.ppfeaturemask=0xffffffff` expose `pwm1` as read-only and lack
    /// `pwm1_enable`. Writing to `pwm1_enable` on those GPUs returns `ENOENT`
    /// and surfaces a misleading 503 hardware_unavailable. Capability scoring
    /// at `status.rs` and the GPU set/reset handlers must agree on this rule
    /// to avoid drift between `/capabilities` and the actual handler outcome.
    pub fn can_write_legacy_pwm(&self) -> bool {
        self.has_pwm && self.has_pwm_enable
    }

    /// Whether this GPU has any fan-related sysfs files (fan or PWM).
    pub fn has_any_fan_interface(&self) -> bool {
        self.has_fan_rpm || self.has_pwm || self.fan_curve_path.is_some()
    }

    /// Path to the PMFW `fan_minimum_pwm` sysfs attribute, if PMFW is present.
    ///
    /// Derived from `fan_curve_path` (both live in `gpu_od/fan_ctrl/`), so it
    /// is `Some` only when a PMFW fan_curve was found. The attribute is a
    /// diagnostics-only read — the daemon never writes it.
    pub fn fan_minimum_pwm_path(&self) -> Option<PathBuf> {
        self.fan_curve_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("fan_minimum_pwm"))
    }
}

/// Whether a PCI device ID is RDNA3 or RDNA4 (for kernel-regression warnings).
///
/// Used by `kernel_warnings::detect_kernel_warnings` to scope warnings to GPU
/// architectures actually affected by the regression. RDNA2 and earlier are
/// not affected by the 6.19 hard-hang or the SMU mismatch on 7.0.
pub fn is_rdna3_or_rdna4(device_id: u16) -> bool {
    matches!(
        device_id,
        // RDNA4 — Navi 48 and related
        0x7550 | 0x7551
        // RDNA3 — Navi 31 (RX 7900 series)
        | 0x744C | 0x7448 | 0x7480
        // RDNA3 — Navi 32 (RX 7800/7700)
        | 0x7470 | 0x747E
        // RDNA3 — Navi 33 (RX 7600)
        | 0x7460 | 0x7461
    )
}

/// Discover all AMD GPUs by scanning hwmon devices.
///
/// The `hwmon_root` parameter allows test injection (defaults to `/sys/class/hwmon`).
/// For each hwmon device with `name == "amdgpu"`, resolves PCI identity and capabilities.
pub fn detect_amd_gpus(hwmon_root: &Path) -> Vec<AmdGpuInfo> {
    detect_amd_gpus_with_ppfeaturemask(hwmon_root, Path::new(PPFEATUREMASK_PATH))
}

/// Internal: detect GPUs with injectable ppfeaturemask path for testing.
pub fn detect_amd_gpus_with_ppfeaturemask(
    hwmon_root: &Path,
    ppfeaturemask_path: &Path,
) -> Vec<AmdGpuInfo> {
    let mut gpus = Vec::new();
    let overdrive_enabled = read_overdrive_enabled(ppfeaturemask_path);

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
        if let Some(gpu) = detect_single_gpu(&hwmon_dir, overdrive_enabled) {
            gpus.push(gpu);
        }
    }

    // Sort: discrete GPUs with fan interfaces first, then by PCI BDF
    gpus.sort_by(|a, b| {
        b.has_any_fan_interface()
            .cmp(&a.has_any_fan_interface())
            .then(b.is_discrete.cmp(&a.is_discrete))
            .then(a.pci_bdf.cmp(&b.pci_bdf))
    });

    gpus
}

/// Attempt to detect an AMD GPU from a single hwmon directory.
fn detect_single_gpu(hwmon_dir: &Path, overdrive_enabled: bool) -> Option<AmdGpuInfo> {
    let name = read_sysfs_string(&hwmon_dir.join("name")).ok()?;
    if name.trim() != "amdgpu" {
        return None;
    }

    // Resolve device symlink to PCI path
    let device_link = hwmon_dir.join("device");
    let pci_path = resolve_pci_path(&device_link)?;
    let pci_bdf = extract_pci_bdf(&pci_path)?;

    // Read PCI identity
    let pci_device_id = read_pci_hex16(&pci_path.join("device")).unwrap_or(0);
    let pci_revision = read_pci_hex_u8(&pci_path.join("revision")).unwrap_or(0);
    let pci_class = read_pci_hex32(&pci_path.join("class")).unwrap_or(0);

    let is_discrete = (pci_class & 0xFFFF00) == PCI_CLASS_VGA;
    let marketing_name = lookup_marketing_name(pci_device_id, pci_revision);

    // Check fan capabilities in hwmon
    let has_fan_rpm = hwmon_dir.join("fan1_input").exists();
    let has_pwm = hwmon_dir.join("pwm1").exists();
    let has_pwm_enable = hwmon_dir.join("pwm1_enable").exists();

    // Check for PMFW fan_curve (RDNA3+ with overdrive enabled)
    let fan_curve_path = find_fan_curve_path(&pci_path);
    let fan_zero_rpm_path = find_fan_zero_rpm_path(&pci_path);

    if fan_curve_path.is_none() && !has_pwm_enable && has_fan_rpm && overdrive_enabled {
        log::info!(
            "GPU {pci_bdf}: overdrive enabled but gpu_od/fan_ctrl/fan_curve not found. \
             Kernel/firmware may not support PMFW fan control for this GPU yet."
        );
    } else if fan_curve_path.is_none() && !has_pwm_enable && has_fan_rpm && !overdrive_enabled {
        log::info!(
            "GPU {pci_bdf}: RDNA3+ GPU detected without pwm1_enable. \
             PMFW fan control requires amdgpu.ppfeaturemask with bit 14 (0x4000) set. \
             Current ppfeaturemask does not include overdrive. \
             Add 'amdgpu.ppfeaturemask=0xffffffff' to kernel parameters to enable."
        );
    }

    Some(AmdGpuInfo {
        pci_bdf,
        pci_device_id,
        pci_revision,
        pci_class,
        marketing_name,
        hwmon_path: hwmon_dir.to_path_buf(),
        fan_curve_path,
        fan_zero_rpm_path,
        is_discrete,
        has_fan_rpm,
        has_pwm,
        has_pwm_enable,
        overdrive_enabled,
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

/// Read a PCI sysfs hex attribute as u16 (e.g. `0x7550`).
fn read_pci_hex16(path: &Path) -> Option<u16> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    u16::from_str_radix(trimmed, 16).ok()
}

/// Read a PCI sysfs hex attribute as u8 (e.g. `0xc0` for revision).
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

/// Read whether overdrive is enabled in the amdgpu ppfeaturemask.
fn read_overdrive_enabled(ppfeaturemask_path: &Path) -> bool {
    let raw = match std::fs::read_to_string(ppfeaturemask_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let trimmed = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    match u32::from_str_radix(trimmed, 16) {
        Ok(mask) => (mask & PP_OVERDRIVE_MASK) != 0,
        Err(_) => {
            // ppfeaturemask may be reported as decimal on some kernels
            match raw.trim().parse::<u32>() {
                Ok(mask) => (mask & PP_OVERDRIVE_MASK) != 0,
                Err(_) => false,
            }
        }
    }
}

/// Find the PMFW fan_curve sysfs file for a GPU.
fn find_fan_curve_path(pci_device_path: &Path) -> Option<PathBuf> {
    let fan_curve = pci_device_path.join("gpu_od/fan_ctrl/fan_curve");
    if fan_curve.exists() {
        Some(fan_curve)
    } else {
        None
    }
}

/// Find the PMFW fan_zero_rpm_enable sysfs file for a GPU.
fn find_fan_zero_rpm_path(pci_device_path: &Path) -> Option<PathBuf> {
    let zero_rpm = pci_device_path.join("gpu_od/fan_ctrl/fan_zero_rpm_enable");
    if zero_rpm.exists() {
        Some(zero_rpm)
    } else {
        None
    }
}

/// Map a PCI device ID (and optional revision) to an AMD GPU marketing name.
///
/// Verified against pci.ids database and lspci output.
fn lookup_marketing_name(device_id: u16, revision: u8) -> Option<String> {
    match device_id {
        // RDNA4 — Navi 48 (RX 9070 series)
        // Same device ID 0x7550, distinguished by PCI revision
        0x7550 => match revision {
            0xC0 => Some("RX 9070 XT".into()),
            0xC3 => Some("RX 9070".into()),
            _ => Some("RX 9070 Series".into()),
        },

        // RDNA3 — Navi 31 (RX 7900 series)
        0x744C => Some("RX 7900 XTX".into()),
        0x7448 => Some("RX 7900 XT".into()),
        0x7480 => Some("RX 7900 GRE".into()),
        // RDNA3 — Navi 32 (RX 7800/7700 series)
        0x7470 => Some("RX 7800 XT".into()),
        0x747E => Some("RX 7700 XT".into()),
        // RDNA3 — Navi 33 (RX 7600 series)
        0x7460 => Some("RX 7600 XT".into()),
        0x7461 => Some("RX 7600".into()),

        // RDNA2 — Navi 21 (RX 6900/6800 series)
        0x73BF => Some("RX 6900 XT".into()),
        0x73A5 => Some("RX 6950 XT".into()),
        0x73EF => Some("RX 6800 XT".into()),
        0x73E3 => Some("RX 6800".into()),
        // RDNA2 — Navi 22 (RX 6700 series)
        0x73DF => Some("RX 6700 XT".into()),
        0x73D1 => Some("RX 6700".into()),
        // RDNA2 — Navi 23 (RX 6600 series)
        0x73FF => Some("RX 6600 XT".into()),
        0x73E9 => Some("RX 6600".into()),

        // Known iGPUs — return None (they get "AMD D-GPU" fallback but are filtered by
        // has_any_fan_interface in practice)
        0x13C0 => None, // Granite Ridge iGPU (Ryzen 9000 series)
        0x1681 => None, // Rembrandt iGPU (Ryzen 6000/7000 mobile)
        0x164E => None, // Raphael iGPU (Ryzen 7000 desktop)
        0x15BF => None, // Phoenix iGPU (Ryzen 7040/8040)

        _ => {
            log::debug!("Unknown AMD GPU device ID: {device_id:#06x} rev {revision:#04x}");
            None
        }
    }
}

/// Select the primary (preferred) AMD GPU from detected GPUs.
///
/// Preference: GPUs with fan interfaces first, then discrete VGA > render-only,
/// then first by PCI BDF. Returns None if no AMD GPUs are detected.
pub fn select_primary_gpu(gpus: &[AmdGpuInfo]) -> Option<&AmdGpuInfo> {
    // Already sorted by detect_amd_gpus: fan interface > discrete > PCI BDF
    gpus.first()
}

/// An AMD VGA-class PCI device and the driver currently bound to it.
///
/// This is detected independently of the hwmon scan in [`detect_amd_gpus`].
/// Hwmon-based detection only sees a GPU once `amdgpu` has bound and created
/// an `hwmon` node; if the module is blacklisted, failed early KMS, or the
/// device is bound to `vfio-pci` for passthrough, no hwmon node exists and the
/// GPU is otherwise invisible. Scanning PCI space and reading the `driver`
/// symlink lets the daemon report "GPU present but amdgpu not bound" — the
/// single most common reason GPU fan control silently does nothing.
#[derive(Debug, Clone)]
pub struct AmdPciDevice {
    /// PCI Bus:Device.Function address (e.g. `0000:03:00.0`).
    pub pci_bdf: String,
    /// PCI device ID (e.g. `0x7550`).
    pub pci_device_id: u16,
    /// Basename of the bound kernel driver (e.g. `"amdgpu"`, `"vfio-pci"`),
    /// or `None` when the device has no `driver` symlink (unbound).
    pub driver: Option<String>,
}

impl AmdPciDevice {
    /// Whether the `amdgpu` driver is bound to this device.
    pub fn amdgpu_bound(&self) -> bool {
        self.driver.as_deref() == Some("amdgpu")
    }
}

/// Whether the `amdgpu` kernel module is loaded (`/sys/module/amdgpu` exists).
pub fn amdgpu_module_loaded() -> bool {
    amdgpu_module_loaded_at(Path::new(AMDGPU_MODULE_PATH))
}

/// Internal: module-loaded check against an injectable path (test seam).
pub fn amdgpu_module_loaded_at(module_path: &Path) -> bool {
    module_path.exists()
}

/// Scan PCI space for AMD VGA-class devices and report their driver binding.
///
/// Uses the production `/sys/bus/pci/devices` root. See
/// [`detect_amd_pci_devices_at`] for the test-injectable variant.
pub fn detect_amd_pci_devices() -> Vec<AmdPciDevice> {
    detect_amd_pci_devices_at(Path::new(PCI_DEVICES_ROOT))
}

/// Internal: PCI scan against an injectable devices root (test seam).
///
/// Each entry in the devices root is a BDF-named symlink to the device dir.
/// We read `vendor`/`class`/`device` (following the symlink) and the optional
/// `driver` symlink to determine binding. Non-AMD and non-VGA devices are
/// skipped. Fail-soft: an unreadable root yields an empty Vec.
pub fn detect_amd_pci_devices_at(pci_devices_root: &Path) -> Vec<AmdPciDevice> {
    let mut devices = Vec::new();
    let entries = match std::fs::read_dir(pci_devices_root) {
        Ok(e) => e,
        Err(_) => return devices,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let dev_path = entry.path();
        let Some(bdf) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_pci_bdf(&bdf) {
            continue;
        }
        // Vendor + class gate: AMD VGA-class controllers only.
        if read_pci_hex16(&dev_path.join("vendor")) != Some(PCI_VENDOR_AMD) {
            continue;
        }
        let class = read_pci_hex32(&dev_path.join("class")).unwrap_or(0);
        if (class & 0xFFFF00) != PCI_CLASS_VGA {
            continue;
        }
        let pci_device_id = read_pci_hex16(&dev_path.join("device")).unwrap_or(0);
        // The `driver` symlink basename names the bound driver, if any.
        let driver = std::fs::read_link(dev_path.join("driver"))
            .ok()
            .and_then(|target| {
                target
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
            });
        devices.push(AmdPciDevice {
            pci_bdf: bdf,
            pci_device_id,
            driver,
        });
    }

    devices.sort_by(|a, b| a.pci_bdf.cmp(&b.pci_bdf));
    devices
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a fake amdgpu hwmon device with optional PCI sysfs tree.
    /// Now supports optional revision and pwm_enable control.
    #[allow(clippy::too_many_arguments)]
    fn create_fake_gpu_ext(
        base: &Path,
        hwmon_name: &str,
        pci_bdf: &str,
        device_id: &str,
        revision: &str,
        class: &str,
        fan_rpm: bool,
        pwm: bool,
        pwm_enable: bool,
        pmfw: bool,
    ) -> PathBuf {
        let hwmon_dir = base.join(hwmon_name);
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "amdgpu\n").unwrap();

        let pci_dir = base.join("pci_devices").join(pci_bdf);
        fs::create_dir_all(&pci_dir).unwrap();
        fs::write(pci_dir.join("device"), format!("{device_id}\n")).unwrap();
        fs::write(pci_dir.join("revision"), format!("{revision}\n")).unwrap();
        fs::write(pci_dir.join("class"), format!("{class}\n")).unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&pci_dir, hwmon_dir.join("device")).unwrap();

        fs::write(hwmon_dir.join("temp1_input"), "45000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_label"), "edge\n").unwrap();

        if fan_rpm {
            fs::write(hwmon_dir.join("fan1_input"), "0\n").unwrap();
        }
        if pwm {
            fs::write(hwmon_dir.join("pwm1"), "0\n").unwrap();
        }
        if pwm_enable {
            fs::write(hwmon_dir.join("pwm1_enable"), "2\n").unwrap();
        }
        if pmfw {
            let fan_ctrl = pci_dir.join("gpu_od/fan_ctrl");
            fs::create_dir_all(&fan_ctrl).unwrap();
            fs::write(
                fan_ctrl.join("fan_curve"),
                "OD_FAN_CURVE:\n0: 40C 30%\n1: 50C 35%\n",
            )
            .unwrap();
            fs::write(fan_ctrl.join("fan_zero_rpm_enable"), "1\n").unwrap();
        }

        hwmon_dir
    }

    /// Legacy helper — creates with pwm_enable=true when pwm=true.
    #[allow(clippy::too_many_arguments)]
    fn create_fake_gpu(
        base: &Path,
        hwmon_name: &str,
        pci_bdf: &str,
        device_id: &str,
        class: &str,
        fan_rpm: bool,
        pwm: bool,
        pmfw: bool,
    ) -> PathBuf {
        create_fake_gpu_ext(
            base, hwmon_name, pci_bdf, device_id, "0xc0", class, fan_rpm, pwm, pwm, pmfw,
        )
    }

    fn fake_ppfeaturemask(base: &Path, value: &str) -> PathBuf {
        let path = base.join("ppfeaturemask");
        fs::write(&path, format!("{value}\n")).unwrap();
        path
    }

    // ── PCI ID / marketing name tests ──────────────────────────────

    #[test]
    fn navi48_xt_detected_by_revision() {
        assert_eq!(
            lookup_marketing_name(0x7550, 0xC0),
            Some("RX 9070 XT".to_string())
        );
    }

    #[test]
    fn navi48_non_xt_detected_by_revision() {
        assert_eq!(
            lookup_marketing_name(0x7550, 0xC3),
            Some("RX 9070".to_string())
        );
    }

    #[test]
    fn navi48_unknown_revision_is_series() {
        assert_eq!(
            lookup_marketing_name(0x7550, 0xFF),
            Some("RX 9070 Series".to_string())
        );
    }

    #[test]
    fn igpu_returns_none() {
        assert_eq!(lookup_marketing_name(0x13C0, 0xCB), None);
    }

    #[test]
    fn rdna3_lookup() {
        assert_eq!(
            lookup_marketing_name(0x744C, 0x00),
            Some("RX 7900 XTX".to_string())
        );
    }

    #[test]
    fn unknown_id_returns_none() {
        assert_eq!(lookup_marketing_name(0xFFFF, 0x00), None);
    }

    // ── Detection integration tests ────────────────────────────────

    #[test]
    fn detect_navi48_with_correct_id() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0xfff7bfff");
        create_fake_gpu_ext(
            tmp.path(),
            "hwmon0",
            "0000:03:00.0",
            "0x7550",
            "0xc0",
            "0x030000",
            true,  // fan_rpm
            true,  // pwm
            false, // pwm_enable — RDNA4 does NOT have this
            false, // pmfw — overdrive not enabled
        );

        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.pci_device_id, 0x7550);
        assert_eq!(gpu.pci_revision, 0xC0);
        assert_eq!(gpu.marketing_name.as_deref(), Some("RX 9070 XT"));
        assert_eq!(gpu.display_label(), "9070XT");
        assert!(gpu.is_discrete);
        assert!(gpu.has_fan_rpm);
        assert!(gpu.has_pwm);
        assert!(!gpu.has_pwm_enable);
        assert!(gpu.fan_curve_path.is_none());
        assert_eq!(gpu.fan_control_method(), "read_only");
    }

    #[test]
    fn detect_navi48_with_pmfw_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0xffffffff");
        create_fake_gpu_ext(
            tmp.path(),
            "hwmon0",
            "0000:03:00.0",
            "0x7550",
            "0xc0",
            "0x030000",
            true,  // fan_rpm
            true,  // pwm
            false, // pwm_enable
            true,  // pmfw — overdrive enabled, fan_curve exists
        );

        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        let gpu = &gpus[0];
        assert_eq!(gpu.fan_control_method(), "pmfw_curve");
        assert!(gpu.fan_curve_path.is_some());
        assert!(gpu.overdrive_enabled);
    }

    #[test]
    fn detect_rdna2_with_pwm_enable() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0x0");
        create_fake_gpu_ext(
            tmp.path(),
            "hwmon0",
            "0000:03:00.0",
            "0x73BF",
            "0xc3",
            "0x030000",
            true,
            true,
            true, // pwm_enable exists on RDNA2
            false,
        );

        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        let gpu = &gpus[0];
        assert_eq!(gpu.fan_control_method(), "hwmon_pwm");
        assert!(gpu.has_pwm_enable);
        assert_eq!(gpu.marketing_name.as_deref(), Some("RX 6900 XT"));
    }

    #[test]
    fn fanless_igpu_detected_but_no_fan_interface() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0x0");
        create_fake_gpu_ext(
            tmp.path(),
            "hwmon0",
            "0000:7b:00.0",
            "0x13c0",
            "0xcb",
            "0x030000",
            false,
            false,
            false,
            false,
        );

        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert!(!gpu.has_any_fan_interface());
        assert_eq!(gpu.fan_control_method(), "none");
        assert!(gpu.marketing_name.is_none());
    }

    #[test]
    fn discrete_with_fans_preferred_over_igpu() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0x0");
        // iGPU — no fan
        create_fake_gpu_ext(
            tmp.path(),
            "hwmon0",
            "0000:7b:00.0",
            "0x13c0",
            "0xcb",
            "0x030000",
            false,
            false,
            false,
            false,
        );
        // dGPU — has fan
        create_fake_gpu_ext(
            tmp.path(),
            "hwmon1",
            "0000:03:00.0",
            "0x7550",
            "0xc0",
            "0x030000",
            true,
            true,
            false,
            false,
        );

        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        assert_eq!(gpus.len(), 2);
        // dGPU with fans should be first (primary)
        assert!(gpus[0].has_any_fan_interface());
        assert_eq!(gpus[0].pci_bdf, "0000:03:00.0");
        assert!(!gpus[1].has_any_fan_interface());
    }

    // ── ppfeaturemask tests ────────────────────────────────────────

    #[test]
    fn overdrive_enabled_with_all_bits() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0xffffffff");
        assert!(read_overdrive_enabled(&pp));
    }

    #[test]
    fn overdrive_disabled_missing_bit14() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0xfff7bfff");
        assert!(!read_overdrive_enabled(&pp));
    }

    #[test]
    fn overdrive_enabled_only_bit14() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0x4000");
        assert!(read_overdrive_enabled(&pp));
    }

    #[test]
    fn overdrive_decimal_format() {
        let tmp = tempfile::tempdir().unwrap();
        // 0xfff7bfff in decimal (bit 14 NOT set)
        let pp = fake_ppfeaturemask(tmp.path(), "4294557695");
        assert!(!read_overdrive_enabled(&pp));
    }

    #[test]
    fn overdrive_missing_file() {
        assert!(!read_overdrive_enabled(Path::new(
            "/nonexistent/ppfeaturemask"
        )));
    }

    // ── Legacy compat tests ────────────────────────────────────────

    #[test]
    fn display_label_known_model() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0x0");
        create_fake_gpu(
            tmp.path(),
            "hwmon0",
            "0000:2d:00.0",
            "0x7550",
            "0x030000",
            true,
            true,
            false,
        );
        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        assert_eq!(gpus[0].display_label(), "9070XT");
    }

    #[test]
    fn display_label_unknown_model() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0x0");
        create_fake_gpu(
            tmp.path(),
            "hwmon0",
            "0000:2d:00.0",
            "0x9999",
            "0x030000",
            false,
            false,
            false,
        );
        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        assert_eq!(gpus[0].display_label(), "AMD D-GPU");
    }

    #[test]
    fn skips_non_amdgpu_devices() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0x0");
        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "k10temp\n").unwrap();

        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        assert!(gpus.is_empty());
    }

    #[test]
    fn no_device_symlink_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = fake_ppfeaturemask(tmp.path(), "0x0");
        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "amdgpu\n").unwrap();

        let gpus = detect_amd_gpus_with_ppfeaturemask(tmp.path(), &pp);
        assert!(gpus.is_empty());
    }

    #[test]
    fn pci_bdf_format_check() {
        assert!(is_pci_bdf("0000:2d:00.0"));
        assert!(is_pci_bdf("0000:00:18.3"));
        assert!(!is_pci_bdf("it87.2624"));
        assert!(!is_pci_bdf("short"));
    }

    // ── DEC-119: PCI driver-bound scan ──────────────────────────────

    /// Create a fake `/sys/bus/pci/devices/<bdf>` entry with vendor/device/
    /// class attributes and an optional `driver` symlink.
    fn create_fake_pci_dev(
        root: &Path,
        bdf: &str,
        vendor: &str,
        device: &str,
        class: &str,
        driver: Option<&str>,
    ) {
        let dev = root.join(bdf);
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("vendor"), format!("{vendor}\n")).unwrap();
        fs::write(dev.join("device"), format!("{device}\n")).unwrap();
        fs::write(dev.join("class"), format!("{class}\n")).unwrap();
        if let Some(drv) = driver {
            let drv_dir = root.join("drivers").join(drv);
            fs::create_dir_all(&drv_dir).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&drv_dir, dev.join("driver")).unwrap();
        }
    }

    #[test]
    fn pci_scan_reports_amdgpu_bound() {
        let tmp = tempfile::tempdir().unwrap();
        create_fake_pci_dev(
            tmp.path(),
            "0000:03:00.0",
            "0x1002",
            "0x7550",
            "0x030000",
            Some("amdgpu"),
        );
        let devs = detect_amd_pci_devices_at(tmp.path());
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].pci_bdf, "0000:03:00.0");
        assert_eq!(devs[0].pci_device_id, 0x7550);
        assert_eq!(devs[0].driver.as_deref(), Some("amdgpu"));
        assert!(devs[0].amdgpu_bound());
    }

    #[test]
    fn pci_scan_reports_unbound_gpu() {
        // The flagship gap-(a) case: AMD VGA present, no driver symlink → the
        // driver did not bind, so no hwmon node exists and the GPU would
        // otherwise be invisible.
        let tmp = tempfile::tempdir().unwrap();
        create_fake_pci_dev(
            tmp.path(),
            "0000:03:00.0",
            "0x1002",
            "0x7550",
            "0x030000",
            None,
        );
        let devs = detect_amd_pci_devices_at(tmp.path());
        assert_eq!(devs.len(), 1);
        assert!(devs[0].driver.is_none());
        assert!(!devs[0].amdgpu_bound());
    }

    #[test]
    fn pci_scan_reports_vfio_passthrough() {
        let tmp = tempfile::tempdir().unwrap();
        create_fake_pci_dev(
            tmp.path(),
            "0000:03:00.0",
            "0x1002",
            "0x7550",
            "0x030000",
            Some("vfio-pci"),
        );
        let devs = detect_amd_pci_devices_at(tmp.path());
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].driver.as_deref(), Some("vfio-pci"));
        assert!(!devs[0].amdgpu_bound());
    }

    #[test]
    fn pci_scan_skips_non_amd_and_non_vga() {
        let tmp = tempfile::tempdir().unwrap();
        // NVIDIA VGA — wrong vendor.
        create_fake_pci_dev(
            tmp.path(),
            "0000:01:00.0",
            "0x10de",
            "0x2684",
            "0x030000",
            Some("nvidia"),
        );
        // AMD HD-audio function — right vendor, wrong class.
        create_fake_pci_dev(
            tmp.path(),
            "0000:03:00.1",
            "0x1002",
            "0xab30",
            "0x040300",
            Some("snd_hda_intel"),
        );
        // AMD VGA — the only one that should be reported.
        create_fake_pci_dev(
            tmp.path(),
            "0000:03:00.0",
            "0x1002",
            "0x7550",
            "0x030000",
            Some("amdgpu"),
        );
        let devs = detect_amd_pci_devices_at(tmp.path());
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].pci_bdf, "0000:03:00.0");
    }

    #[test]
    fn pci_scan_missing_root_is_empty() {
        assert!(detect_amd_pci_devices_at(Path::new("/nonexistent/pci/devices")).is_empty());
    }

    #[test]
    fn amdgpu_module_loaded_detects_presence() {
        let tmp = tempfile::tempdir().unwrap();
        let module = tmp.path().join("amdgpu");
        assert!(!amdgpu_module_loaded_at(&module));
        fs::create_dir_all(&module).unwrap();
        assert!(amdgpu_module_loaded_at(&module));
    }
}
