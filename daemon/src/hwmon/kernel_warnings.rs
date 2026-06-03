//! Kernel-version awareness for known amdgpu regressions.
//!
//! Surfaces warnings to GUI clients when the running kernel matches a
//! published amdgpu regression that the daemon cannot fix at runtime. Two
//! risks were called out by external research (Phoronix, ROCm GitHub) and
//! re-verified against primary sources during the DEC-114 docs audit:
//!
//! 1. **Linux 6.18/6.19 RDNA3/RDNA4 hard hang** (Phoronix EOY 2025). RDNA3 +
//!    RDNA4 GPUs hard-hang under load on **both** kernel 6.18.x and 6.19.x —
//!    not 6.19 alone. Not bisected and no upstream fix or revert confirmed.
//!    The verified-safe fallback is a 6.15–6.17 longterm kernel; rolling back
//!    to 6.18 is NOT safe because 6.18 is also affected (ROCm #6101 reports
//!    kernel panics on both 6.18.20 and 6.19.10).
//!    See <https://www.phoronix.com/review/old-amdgpu-eoy2025> and
//!    <https://github.com/ROCm/ROCm/issues/6101>.
//!
//! 2. **R9700 / Navi 48 (PCI 0x7551) SMU interface-version mismatch**
//!    (ROCm Issue #6101). The board firmware reports SMU interface v50 while
//!    the amdgpu driver supports v46, so there is no working fan-control path:
//!    `pwm1` is read-only, commanded fan changes have no effect, and the GPU
//!    can reach 109 °C under load with no dmesg "fan failed" line. Reported
//!    across every tested kernel (6.14, 6.17, 7.0), so it is scoped by PCI
//!    device ID rather than kernel version. The RX 9070 XT (PCI 0x7550) is
//!    unaffected. See <https://github.com/ROCm/ROCm/issues/6101>.
//!
//! These are *advisory* warnings. The daemon does not refuse writes — the
//! GUI surfaces a one-time popup, the support bundle records the kernel
//! release, and (in a future release) a post-write RPM readback will be
//! the actual safety net. See DEC-098.
//!
//! Detection runs at capabilities-build time and is cheap (a single sysfs
//! read of `/proc/sys/kernel/osrelease`). The kernel version is parsed once
//! per request; per-warning matching is a couple of integer comparisons.

use std::path::Path;

use crate::hwmon::gpu_detect::{is_rdna3_or_rdna4, AmdGpuInfo};

/// Path to the kernel release sysfs file. Override-able for testing.
pub const KERNEL_RELEASE_PATH: &str = "/proc/sys/kernel/osrelease";

/// Parsed `(major, minor, patch)` from a kernel release string.
///
/// Accepts `"7.0.3-1-cachyos"`, `"6.19.7"`, `"6.18.0"`, etc. Returns `None`
/// if the prefix doesn't parse to three dot-separated integers.
pub fn parse_kernel_version(release: &str) -> Option<(u32, u32, u32)> {
    let trimmed = release.trim();
    // Take the version prefix up to the first non-digit/non-dot character
    // (e.g. strip "-1-cachyos" off "7.0.3-1-cachyos").
    let prefix: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = prefix.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Severity of a kernel warning, ordered from informational to safety-critical.
///
/// The GUI uses severity to decide whether to surface a one-time popup
/// (`high`/`critical`) versus only logging it (`info`/`medium`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KernelWarningSeverity {
    Info,
    Medium,
    High,
    Critical,
}

/// A single kernel-related advisory tied to the running GPU.
///
/// Fields:
/// - `id`: stable identifier the GUI can key knowledge-base entries off (e.g.
///   `"rdna_hang_kernel_6_18_6_19"`). Stable across releases, EXCEPT when the
///   underlying advice materially changes — then the id is deliberately
///   renamed so acknowledged-warning state is invalidated and the GUI
///   re-prompts the user with the corrected guidance (see DEC-114).
/// - `severity`: drives whether the GUI shows a popup vs. logs only.
/// - `message`: pre-formatted user-visible text. The daemon owns the wording
///   so a single message update doesn't require coordinated GUI redeploys.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KernelWarning {
    pub id: String,
    pub severity: KernelWarningSeverity,
    pub message: String,
}

/// Detect kernel-version warnings applicable to a single GPU.
///
/// `kernel_release` is the contents of `/proc/sys/kernel/osrelease` (or an
/// equivalent test injection). Returns an empty Vec when nothing is wrong
/// or when the kernel version can't be parsed (fail-soft — better to omit
/// a warning than to surface a wrong one).
pub fn detect_kernel_warnings(kernel_release: &str, gpu: &AmdGpuInfo) -> Vec<KernelWarning> {
    let mut warnings = Vec::new();
    let Some((major, minor, _patch)) = parse_kernel_version(kernel_release) else {
        return warnings;
    };

    // Risk 1: Linux 6.18.x / 6.19.x hard-hang on RDNA3/RDNA4
    // (Phoronix EOY 2025). Both 6.18 and 6.19 are affected — re-verified
    // against the Phoronix article and ROCm #6101 (panics on 6.18.20 and
    // 6.19.10). The id was renamed from `rdna_hang_kernel_6_19_x` so the
    // GUI re-prompts users who acknowledged the earlier, narrower (and
    // unsafe — it recommended 6.18) advice. See DEC-114.
    if major == 6 && (minor == 18 || minor == 19) && is_rdna3_or_rdna4(gpu.pci_device_id) {
        warnings.push(KernelWarning {
            id: "rdna_hang_kernel_6_18_6_19".into(),
            severity: KernelWarningSeverity::Critical,
            message: format!(
                "Kernel {kernel_release} is affected by an RDNA3/RDNA4 hard-hang \
                 regression that hits both 6.18.x and 6.19.x under load \
                 (Phoronix EOY 2025; ROCm #6101). Pin to a 6.15–6.17 longterm \
                 kernel before continuing fan control on this GPU — do NOT roll \
                 back to 6.18, which is also affected."
            ),
        });
    }

    // Risk 2: R9700 / Navi 48 (PCI 0x7551) SMU interface-version mismatch
    // (ROCm Issue #6101). Firmware SMU iface v50 vs driver v46 → no working
    // fan-control path: pwm1 is read-only and commanded changes have no
    // effect, while the GPU can reach 109 °C with no dmesg error. Reported
    // across 6.14 / 6.17 / 7.0, so it is scoped by PCI device ID, not kernel
    // version. Suppressed inside the 6.18/6.19 hang range, where Risk 1 is
    // the dominant warning. The RX 9070 XT (0x7550) is unaffected. Narrow
    // this once the amdgpu driver ships the matching SMU interface.
    let in_hang_range = major == 6 && (minor == 18 || minor == 19);
    if gpu.pci_device_id == 0x7551 && gpu.fan_curve_path.is_some() && !in_hang_range {
        warnings.push(KernelWarning {
            id: "smu_mismatch_navi48_r9700".into(),
            severity: KernelWarningSeverity::Critical,
            message: format!(
                "Kernel {kernel_release} on the R9700 (Navi 48 0x7551) has no \
                 working PMFW fan-control path: an SMU interface-version mismatch \
                 (firmware v50 vs driver v46, ROCm #6101) means commanded fan \
                 changes have no effect and the GPU can overheat. Use automatic \
                 mode (POST /gpu/{{bdf}}/fan/reset) until the amdgpu driver ships \
                 the matching SMU interface."
            ),
        });
    }

    warnings
}

/// Read the running kernel release from `/proc/sys/kernel/osrelease`.
///
/// Returns `None` on error so callers can fail-soft (no warnings vs.
/// incorrect warnings). Production callers should use `read_kernel_release`;
/// tests inject their own release string into `detect_kernel_warnings`.
pub fn read_kernel_release() -> Option<String> {
    read_kernel_release_at(Path::new(KERNEL_RELEASE_PATH))
}

/// Internal: read the kernel release from a specific path (test injection).
pub fn read_kernel_release_at(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_gpu(device_id: u16, fan_curve: bool) -> AmdGpuInfo {
        AmdGpuInfo {
            pci_bdf: "0000:03:00.0".into(),
            pci_device_id: device_id,
            pci_revision: 0xC0,
            pci_class: 0x030000,
            marketing_name: None,
            hwmon_path: PathBuf::from("/tmp"),
            fan_curve_path: if fan_curve {
                Some(PathBuf::from("/tmp/fan_curve"))
            } else {
                None
            },
            fan_zero_rpm_path: None,
            is_discrete: true,
            has_fan_rpm: true,
            has_pwm: true,
            has_pwm_enable: false,
            overdrive_enabled: fan_curve,
        }
    }

    // ── parse_kernel_version ────────────────────────────────────────

    #[test]
    fn parse_cachyos_release() {
        assert_eq!(parse_kernel_version("7.0.3-1-cachyos"), Some((7, 0, 3)));
    }

    #[test]
    fn parse_simple_release() {
        assert_eq!(parse_kernel_version("6.19.7"), Some((6, 19, 7)));
    }

    #[test]
    fn parse_two_part_release_implies_zero_patch() {
        assert_eq!(parse_kernel_version("7.0"), Some((7, 0, 0)));
    }

    #[test]
    fn parse_with_trailing_newline() {
        assert_eq!(parse_kernel_version("6.18.0-1-cachyos\n"), Some((6, 18, 0)));
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse_kernel_version("not-a-kernel").is_none());
        assert!(parse_kernel_version("").is_none());
        assert!(parse_kernel_version("6").is_none()); // single component
    }

    // ── detect_kernel_warnings: 6.19 RDNA hang ──────────────────────

    #[test]
    fn rdna4_on_6_19_warns() {
        let gpu = make_gpu(0x7550, true);
        let warnings = detect_kernel_warnings("6.19.7", &gpu);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "rdna_hang_kernel_6_18_6_19");
        assert_eq!(warnings[0].severity, KernelWarningSeverity::Critical);
    }

    #[test]
    fn rdna3_on_6_19_warns() {
        let gpu = make_gpu(0x744C, true); // RX 7900 XTX
        let warnings = detect_kernel_warnings("6.19.0-2-cachyos", &gpu);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "rdna_hang_kernel_6_18_6_19");
    }

    #[test]
    fn rdna2_on_6_19_does_not_warn() {
        let gpu = make_gpu(0x73BF, false); // RX 6900 XT
        let warnings = detect_kernel_warnings("6.19.7", &gpu);
        assert!(warnings.is_empty());
    }

    #[test]
    fn rdna4_on_6_18_warns() {
        // 6.18 is ALSO affected by the hard-hang regression (DEC-114). The
        // earlier code wrongly cleared 6.18 and even recommended it as the
        // rollback target; this test guards against that unsafe regression.
        let gpu = make_gpu(0x7550, true);
        let warnings = detect_kernel_warnings("6.18.0", &gpu);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "rdna_hang_kernel_6_18_6_19");
        assert_eq!(warnings[0].severity, KernelWarningSeverity::Critical);
    }

    #[test]
    fn rdna4_on_7_0_does_not_warn_for_6_19() {
        let gpu = make_gpu(0x7550, true);
        let warnings = detect_kernel_warnings("7.0.3-1-cachyos", &gpu);
        // Should not warn for 6.19 hang — but also should not warn for SMU
        // mismatch because 0x7550 is not affected (only 0x7551).
        assert!(
            warnings.is_empty(),
            "0x7550 on 7.0 should produce no warnings, got: {warnings:?}"
        );
    }

    // ── detect_kernel_warnings: R9700 SMU mismatch ──────────────────

    #[test]
    fn r9700_on_7_0_warns_smu_mismatch() {
        let gpu = make_gpu(0x7551, true);
        let warnings = detect_kernel_warnings("7.0.3-1-cachyos", &gpu);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "smu_mismatch_navi48_r9700");
        assert_eq!(warnings[0].severity, KernelWarningSeverity::Critical);
    }

    #[test]
    fn r9700_without_fan_curve_does_not_warn_smu() {
        // No fan_curve path means PMFW isn't engaged; the SMU mismatch is
        // only relevant when the daemon would be writing to fan_curve.
        let gpu = make_gpu(0x7551, false);
        let warnings = detect_kernel_warnings("7.0.3", &gpu);
        assert!(warnings.is_empty());
    }

    #[test]
    fn r9700_on_7_1_still_warns_smu() {
        // The SMU mismatch is device-scoped, not kernel-7.0-scoped (DEC-114):
        // ROCm #6101 reports it persisting across every tested kernel, so we
        // keep warning until the amdgpu driver ships the matching SMU iface.
        let gpu = make_gpu(0x7551, true);
        let warnings = detect_kernel_warnings("7.1.0", &gpu);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "smu_mismatch_navi48_r9700");
    }

    #[test]
    fn rx_9070_xt_on_7_0_does_not_warn_smu() {
        // The user's actual hardware: 0x7550 (XT), not 0x7551 (R9700).
        // Same kernel, but fan_curve works on 0x7550.
        let gpu = make_gpu(0x7550, true);
        let warnings = detect_kernel_warnings("7.0.3-1-cachyos", &gpu);
        assert!(warnings.is_empty());
    }

    #[test]
    fn r9700_on_6_18_warns_hang_only() {
        // On the hang range the R9700 (RDNA4) gets the dominant hang warning
        // and the SMU warning is suppressed — exactly one warning, the hang.
        let gpu = make_gpu(0x7551, true);
        let warnings = detect_kernel_warnings("6.18.5", &gpu);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "rdna_hang_kernel_6_18_6_19");
    }

    #[test]
    fn r9700_on_6_17_warns_smu() {
        // 6.17 is outside the hang range but still has the SMU mismatch.
        let gpu = make_gpu(0x7551, true);
        let warnings = detect_kernel_warnings("6.17.9", &gpu);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "smu_mismatch_navi48_r9700");
    }

    // ── read_kernel_release_at ──────────────────────────────────────

    #[test]
    fn read_kernel_release_strips_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("osrelease");
        std::fs::write(&path, "7.0.3-1-cachyos\n").unwrap();
        assert_eq!(
            read_kernel_release_at(&path),
            Some("7.0.3-1-cachyos".to_string())
        );
    }

    #[test]
    fn read_kernel_release_missing_returns_none() {
        assert!(read_kernel_release_at(Path::new("/nonexistent/osrelease")).is_none());
    }

    #[test]
    fn read_kernel_release_empty_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("osrelease");
        std::fs::write(&path, "\n").unwrap();
        assert!(read_kernel_release_at(&path).is_none());
    }
}
