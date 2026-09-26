//! Kernel-version awareness for known amdgpu regressions.
//!
//! Surfaces warnings to GUI clients when the running kernel matches a
//! published amdgpu regression that the daemon cannot fix at runtime. One rule
//! is live (DEC-422):
//!
//! **drm/amd #4765 — the MES eviction hang on RDNA3/RDNA4.** `079ae5118e1f`
//! ("drm/amdkfd: fix suspend/resume all calls in mes based eviction path") made
//! evicting a process on a MES GPU suspend the whole MES. That also stops the
//! kernel-mode queues, so a compute job running beside a 3D workload times out
//! and the GPU hangs. The bug entered mainline in 6.18 and was backported to
//! 6.17.9. The fix (`3fd20580b96a`, upstream `18dbcfb46f69`, "drm/amdkfd: No need
//! to suspend whole MES to evict process", `Closes:` #4765) is in 6.19.0 and
//! 6.18.7. 6.17 reached end of life at 6.17.13 without it, and 6.12.y and 6.6.y
//! never took the bug. Every GC 11.x and 12.x part runs MES
//! (`amdgpu_discovery_set_mes_ip_blocks`, v6.18), hence the RDNA3/RDNA4 scope.
//! Sources: kernel.org `ChangeLog-6.18.7` (the fix) and `ChangeLog-6.17.9` (the
//! backport of the bug), and the stable tree's branch logs (no fix on 6.17.y;
//! neither commit on 6.12.y or 6.6.y).
//!
//! **Retired by DEC-422.** DEC-421's review found both earlier rules wrong:
//! - `rdna_hang_kernel_6_18_6_19` flagged every 6.18.x / 6.19.x kernel on
//!   RDNA3/4 as Critical and told users to pin 6.15–6.17. None of those was ever
//!   a longterm kernel, and 6.17.9 onward carries the bug above. Its evidence
//!   was an unbisected Phoronix report (December 2025) that no follow-up tied to
//!   a fix or a later kernel. The one bisected 6.18 hang is the rule above.
//! - `smu_mismatch_navi48_r9700` rested on the SMU driver-interface version
//!   message. That message appears on every Navi 48 card, is benign, and was
//!   removed in kernel 7.0 (`e471627d5627`: "It just leads to user confusion").
//!   The rule told every R9700 owner the fan curve could not work, but the PMFW
//!   `fan_curve` path does work on R9700s. The per-unit fan faults on ROCm #6101
//!   are what a GPU fan verify detects.
//!
//! The GUI keeps its guidance for both retired ids, because older daemons still
//! emit them.
//!
//! These are *advisory* warnings. The daemon does not refuse writes. The GUI
//! surfaces a popup (once per id per session, until the user dismisses it for
//! good), and the support bundle records the kernel release.
//! See DEC-098.
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
/// The GUI uses severity to decide whether to surface a popup
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
///   [`MES_HANG_4765_ID`]). Stable across releases, EXCEPT when the
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

/// Stable id of the drm/amd #4765 advisory (DEC-422).
///
/// Named for the upstream issue rather than a kernel range, so the id does not
/// have to change if the range is ever corrected. `rdna_hang_kernel_6_19_x`
/// had to be renamed to `rdna_hang_kernel_6_18_6_19` (DEC-114) precisely
/// because its name carried a range. Renaming away from that id is deliberate
/// too: the advice changed, so a user who dismissed the old popup must see
/// this one.
pub const MES_HANG_4765_ID: &str = "rdna_mes_hang_drm_amd_4765";

/// Whether a kernel release carries drm/amd #4765 without its fix (DEC-422).
///
/// 6.17.9 took the bug as a stable backport, and no 6.17.y release took the fix
/// (6.17 ended at 6.17.13), so every 6.17.y from 9 on carries it. 6.18.0
/// carries it and 6.18.7 fixed it. Nothing else has it: 6.19 and later
/// shipped with the fix, and the 6.12.y and 6.6.y longterm lines never took the
/// bug.
///
/// A distribution kernel that backports the fix without changing its version
/// number is reported as affected. One that carries the bug under a `.0` patch
/// level (for example Ubuntu's `6.17.0-NN`) cannot be detected at all. Both
/// limits come from reading a version string, and the message says so.
fn carries_mes_eviction_hang(major: u32, minor: u32, patch: u32) -> bool {
    match (major, minor) {
        (6, 17) => patch >= 9,
        (6, 18) => patch <= 6,
        _ => false,
    }
}

/// Detect kernel-version warnings applicable to a single GPU.
///
/// `kernel_release` is the contents of `/proc/sys/kernel/osrelease` (or an
/// equivalent test injection). Returns an empty Vec when nothing is wrong
/// or when the kernel version can't be parsed (fail-soft — better to omit
/// a warning than to surface a wrong one).
///
/// DEC-422 rewrote this from two rules to one: see the module docs for the
/// rule and for why `rdna_hang_kernel_6_18_6_19` and
/// `smu_mismatch_navi48_r9700` are no longer raised.
pub fn detect_kernel_warnings(kernel_release: &str, gpu: &AmdGpuInfo) -> Vec<KernelWarning> {
    let mut warnings = Vec::new();
    let Some((major, minor, patch)) = parse_kernel_version(kernel_release) else {
        return warnings;
    };

    if carries_mes_eviction_hang(major, minor, patch) && is_rdna3_or_rdna4(gpu.pci_device_id) {
        warnings.push(KernelWarning {
            id: MES_HANG_4765_ID.into(),
            severity: KernelWarningSeverity::Critical,
            message: format!(
                "Kernel {kernel_release} carries a known amdgpu hang for RDNA3/RDNA4 GPUs \
                 (drm/amd #4765): a compute job running alongside a 3D workload can hang \
                 the GPU, and while the system is hung no fan speed can change. It is \
                 fixed in 6.18.7 and 6.19. Update to the latest 6.18 longterm point \
                 release or a current 7.x kernel; 6.17 is end-of-life and was never fixed. \
                 (This is matched on the version number, so a distribution kernel that \
                 backported the fix may be flagged anyway.)"
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

    // ── detect_kernel_warnings: drm/amd #4765 (DEC-422) ─────────────

    /// The ids DEC-422 retired. No release this daemon can see may raise them.
    const RETIRED: [&str; 2] = ["rdna_hang_kernel_6_18_6_19", "smu_mismatch_navi48_r9700"];

    fn ids(release: &str, gpu: &AmdGpuInfo) -> Vec<String> {
        detect_kernel_warnings(release, gpu)
            .into_iter()
            .map(|w| w.id)
            .collect()
    }

    #[test]
    fn mes_hang_fires_across_6_18_0_to_6_18_6_and_clears_at_6_18_7() {
        let gpu = make_gpu(0x7550, true); // RX 9070 XT
        for release in ["6.18.0", "6.18.3-2-cachyos", "6.18.6"] {
            let warnings = detect_kernel_warnings(release, &gpu);
            assert_eq!(warnings.len(), 1, "{release}");
            assert_eq!(warnings[0].id, MES_HANG_4765_ID, "{release}");
            assert_eq!(warnings[0].severity, KernelWarningSeverity::Critical);
        }
        // The fix landed in 6.18.7 — the old rule flagged every 6.18 kernel.
        for release in ["6.18.7", "6.18.46", "6.18.53-1-cachyos"] {
            assert!(ids(release, &gpu).is_empty(), "{release} carries the fix");
        }
    }

    #[test]
    fn mes_hang_fires_on_the_6_17_backport_and_not_before_it() {
        // 079ae5118e1f was backported into 6.17.9 and the fix never followed —
        // the old advice "pin 6.15–6.17" sent people straight into this.
        let gpu = make_gpu(0x744C, true); // RX 7900 XTX
        for release in ["6.17.9", "6.17.13"] {
            assert_eq!(
                ids(release, &gpu),
                vec![MES_HANG_4765_ID.to_string()],
                "{release}"
            );
        }
        for release in ["6.17.0", "6.17.8", "6.16.12", "6.15.11"] {
            assert!(
                ids(release, &gpu).is_empty(),
                "{release} predates the backport"
            );
        }
    }

    #[test]
    fn fixed_and_never_affected_lines_do_not_warn() {
        let gpu = make_gpu(0x7550, true);
        for release in [
            "6.19.0",
            "6.19.10",
            "7.0.3-1-cachyos",
            "7.2.6-1-cachyos",
            "6.12.105",
            "6.6.153",
        ] {
            assert!(ids(release, &gpu).is_empty(), "{release}");
        }
    }

    #[test]
    fn mes_hang_reaches_every_mes_era_gpu_and_no_rdna2() {
        for (id, what) in [
            (0x7590, "RX 9060 XT"),
            (0x7551, "R9700"),
            (0x7480, "RX 7600"),
            (0x15BF, "Radeon 780M iGPU"),
            (0x150E, "Radeon 890M iGPU"),
        ] {
            assert_eq!(
                ids("6.18.2", &make_gpu(id, false)),
                vec![MES_HANG_4765_ID.to_string()],
                "{what}"
            );
        }
        assert!(
            ids("6.18.2", &make_gpu(0x73BF, false)).is_empty(),
            "RX 6900 XT has no MES"
        );
    }

    #[test]
    fn retired_ids_are_never_emitted() {
        // Presence before absence: the matrix does raise the live advisory, so
        // an empty result below cannot be a detector that never runs.
        let mut raised = 0;
        for release in [
            "6.17.9", "6.18.5", "6.18.7", "6.19.10", "7.0.3", "7.1.0", "7.2.6",
        ] {
            for (id, curve) in [
                (0x7551, true),
                (0x7551, false),
                (0x7550, true),
                (0x744C, true),
            ] {
                for got in ids(release, &make_gpu(id, curve)) {
                    assert!(
                        !RETIRED.contains(&got.as_str()),
                        "{release} {id:#06x} raised {got}"
                    );
                    raised += 1;
                }
            }
        }
        assert!(
            raised > 0,
            "the matrix must raise the live advisory at least once"
        );
    }

    #[test]
    fn r9700_with_a_fan_curve_is_not_told_its_curve_cannot_work() {
        // The retired SMU rule fired here on every kernel. A healthy 7.x R9700
        // now gets nothing; on an affected kernel it gets only the hang.
        let gpu = make_gpu(0x7551, true);
        assert!(ids("7.0.3-1-cachyos", &gpu).is_empty());
        assert!(ids("7.1.0", &gpu).is_empty());
        assert_eq!(ids("6.17.9", &gpu), vec![MES_HANG_4765_ID.to_string()]);
    }

    #[test]
    fn the_message_gives_the_fixed_releases_and_never_the_eol_ones() {
        let w = &detect_kernel_warnings("6.18.4", &make_gpu(0x7550, true))[0];
        assert!(w.message.contains("#4765"));
        assert!(w.message.contains("6.18.7") && w.message.contains("6.19"));
        assert!(w.message.contains("6.18.4"), "names the running release");
        // Never again an instruction to move to a kernel that was never
        // longterm (the retired rule's advice).
        assert!(!w.message.contains("6.15"));
        assert!(!w.message.to_lowercase().contains("pin to"));
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
