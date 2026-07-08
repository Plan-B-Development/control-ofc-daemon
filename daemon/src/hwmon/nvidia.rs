//! Unified NVIDIA discrete-GPU identity (DEC-204).
//!
//! NVIDIA is served by two mutually-exclusive per-GPU drivers — the open
//! `nouveau` (hwmon) leg and the proprietary NVML leg. This module folds both
//! into one [`NvidiaGpuIdentity`] list, gathered once at startup, that the
//! `/capabilities` + `/diagnostics/hardware` handlers read (mirroring the
//! AMD/Intel `*_gpus` AppState fields). Read-only: NVIDIA fans are never driven.

use crate::hwmon::nouveau_detect::NouveauGpuInfo;
use crate::hwmon::nvml::{NvmlBackend, NvmlDeviceIdentity};

/// A detected NVIDIA discrete GPU's static identity, from whichever driver leg
/// found it. `model_name`/`driver_version` are only available via NVML
/// (proprietary driver); the nouveau leg contributes just the BDF + fan
/// availability (there is no in-repo device-id → name table for NVIDIA).
#[derive(Debug, Clone, PartialEq)]
pub struct NvidiaGpuIdentity {
    pub pci_bdf: String,
    /// Backing kernel driver: `"nouveau"` (open) or `"nvidia"` (proprietary).
    /// Mirrors the Intel `driver` field semantics (the kernel module name). The
    /// proprietary GPU is *read* via the NVML userspace library, but the kernel
    /// module backing it is `nvidia` — so that (not "nvml") is the truthful
    /// driver name for the GUI to display.
    pub driver: &'static str,
    pub model_name: Option<String>,
    pub driver_version: Option<String>,
    /// Whether the GPU exposes a fan (nouveau: `fan1_input`; NVML: numFans >= 1).
    pub has_fan: bool,
    /// Whether fan RPM specifically can be read.
    pub fan_rpm_available: bool,
}

impl NvidiaGpuIdentity {
    /// User-facing label: the NVML model name if known, else the generic
    /// "NVIDIA D-GPU" (nouveau exposes no model name).
    pub fn display_label(&self) -> String {
        self.model_name
            .clone()
            .unwrap_or_else(|| "NVIDIA D-GPU".to_string())
    }

    /// Fan control method — `"read_only"` when a fan is present, else `"none"`.
    /// NVIDIA is never fan-writable in this daemon (DEC-204).
    pub fn fan_control_method(&self) -> &'static str {
        if self.has_fan {
            "read_only"
        } else {
            "none"
        }
    }

    fn from_nouveau(g: &NouveauGpuInfo) -> Self {
        Self {
            pci_bdf: g.pci_bdf.clone(),
            driver: "nouveau",
            model_name: None,
            driver_version: None,
            has_fan: g.has_fan_rpm,
            fan_rpm_available: g.has_fan_rpm,
        }
    }

    fn from_nvml(d: &NvmlDeviceIdentity) -> Self {
        Self {
            pci_bdf: d.pci_bdf.clone(),
            driver: "nvidia",
            model_name: d.model_name.clone(),
            driver_version: d.driver_version.clone(),
            has_fan: d.num_fans >= 1,
            fan_rpm_available: d.fan_rpm_available,
        }
    }
}

/// Gather the unified NVIDIA GPU identity list from both driver legs, once at
/// startup. nouveau and NVML are mutually exclusive per GPU (they cannot both
/// bind the same card), so a given BDF appears at most once. Sorted by BDF for
/// a deterministic primary selection.
pub fn gather_nvidia_gpus(
    nouveau: &[NouveauGpuInfo],
    nvml: &dyn NvmlBackend,
) -> Vec<NvidiaGpuIdentity> {
    let mut gpus: Vec<NvidiaGpuIdentity> = nouveau
        .iter()
        .map(NvidiaGpuIdentity::from_nouveau)
        .collect();
    gpus.extend(nvml.devices().iter().map(NvidiaGpuIdentity::from_nvml));
    gpus.sort_by(|a, b| a.pci_bdf.cmp(&b.pci_bdf));
    // nouveau + NVML are mutually exclusive per GPU, but enforce it structurally
    // (not just by comment) so a mid-transition/VM edge cannot double-list one
    // BDF. Keeps the first (NVML sorts ahead of nouveau only incidentally; either
    // read-only entry is equivalent for the capability/diagnostics surfaces).
    gpus.dedup_by(|a, b| a.pci_bdf == b.pci_bdf);
    gpus
}

/// Select the primary (preferred) NVIDIA GPU — first by sorted BDF. Returns
/// `None` when none are detected. Mirrors `select_primary_intel_gpu`.
pub fn select_primary_nvidia_gpu(gpus: &[NvidiaGpuIdentity]) -> Option<&NvidiaGpuIdentity> {
    gpus.first()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::nvml::FakeNvml;

    fn nouveau(bdf: &str, has_fan: bool) -> NouveauGpuInfo {
        NouveauGpuInfo {
            pci_bdf: bdf.to_string(),
            hwmon_path: std::path::PathBuf::from("/tmp"),
            has_fan_rpm: has_fan,
        }
    }

    #[test]
    fn from_nouveau_is_generic_label_read_only() {
        let id = NvidiaGpuIdentity::from_nouveau(&nouveau("0000:03:00.0", true));
        assert_eq!(id.driver, "nouveau");
        assert_eq!(id.model_name, None);
        assert_eq!(id.display_label(), "NVIDIA D-GPU");
        assert_eq!(id.fan_control_method(), "read_only");
        assert!(id.has_fan);
    }

    #[test]
    fn from_nvml_carries_model_and_driver() {
        let d = NvmlDeviceIdentity {
            pci_bdf: "0000:03:00.0".into(),
            model_name: Some("NVIDIA GeForce RTX 4080".into()),
            driver_version: Some("565.77".into()),
            num_fans: 2,
            fan_rpm_available: true,
        };
        let id = NvidiaGpuIdentity::from_nvml(&d);
        // Proprietary leg → kernel driver "nvidia" (not the "nvml" library name).
        assert_eq!(id.driver, "nvidia");
        assert_eq!(id.model_name.as_deref(), Some("NVIDIA GeForce RTX 4080"));
        assert_eq!(id.display_label(), "NVIDIA GeForce RTX 4080");
        assert_eq!(id.driver_version.as_deref(), Some("565.77"));
        assert_eq!(id.fan_control_method(), "read_only");
    }

    #[test]
    fn fanless_gpu_is_none_method() {
        // Both legs: no fan → "none" method.
        let nouveau_id = NvidiaGpuIdentity::from_nouveau(&nouveau("0000:03:00.0", false));
        assert!(!nouveau_id.has_fan);
        assert_eq!(nouveau_id.fan_control_method(), "none");

        let nvml_id = NvidiaGpuIdentity::from_nvml(&NvmlDeviceIdentity {
            pci_bdf: "0000:03:00.0".into(),
            model_name: Some("NVIDIA A40".into()),
            driver_version: Some("565.77".into()),
            num_fans: 0,
            fan_rpm_available: false,
        });
        assert!(!nvml_id.has_fan);
        assert_eq!(nvml_id.fan_control_method(), "none");
    }

    #[test]
    fn gather_merges_both_legs_sorted_by_bdf() {
        let nouveau_gpus = [nouveau("0000:0a:00.0", true)];
        let nvml = FakeNvml::with_identities(
            vec![],
            vec![NvmlDeviceIdentity {
                pci_bdf: "0000:03:00.0".into(),
                model_name: Some("RTX 4080".into()),
                driver_version: Some("565.77".into()),
                num_fans: 1,
                fan_rpm_available: false,
            }],
        );
        let gpus = gather_nvidia_gpus(&nouveau_gpus, &nvml);
        assert_eq!(gpus.len(), 2);
        // Sorted by BDF: NVML 0000:03 first, nouveau 0000:0a second.
        assert_eq!(gpus[0].pci_bdf, "0000:03:00.0");
        assert_eq!(gpus[0].driver, "nvidia");
        assert_eq!(gpus[1].pci_bdf, "0000:0a:00.0");
        assert_eq!(gpus[1].driver, "nouveau");
        assert_eq!(
            select_primary_nvidia_gpu(&gpus).unwrap().pci_bdf,
            "0000:03:00.0"
        );
    }

    #[test]
    fn gather_two_nvml_gpus_sorted_deterministic_primary() {
        // Multi-GPU workstation: primary selection must be the lowest BDF
        // regardless of enumeration order (guards a sort/first() regression).
        let ident = |bdf: &str| NvmlDeviceIdentity {
            pci_bdf: bdf.into(),
            model_name: Some("RTX 4080".into()),
            driver_version: Some("565.77".into()),
            num_fans: 1,
            fan_rpm_available: false,
        };
        let nvml =
            FakeNvml::with_identities(vec![], vec![ident("0000:65:00.0"), ident("0000:03:00.0")]);
        let gpus = gather_nvidia_gpus(&[], &nvml);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].pci_bdf, "0000:03:00.0");
        assert_eq!(gpus[1].pci_bdf, "0000:65:00.0");
        assert_eq!(
            select_primary_nvidia_gpu(&gpus).unwrap().pci_bdf,
            "0000:03:00.0"
        );
    }

    #[test]
    fn gather_empty_when_neither_leg() {
        let nvml = FakeNvml::new(vec![]);
        assert!(gather_nvidia_gpus(&[], &nvml).is_empty());
        assert!(select_primary_nvidia_gpu(&[]).is_none());
    }

    #[test]
    fn gather_dedups_same_bdf_across_legs() {
        // Defensive: the two legs cannot report the same BDF on real hardware
        // (mutually exclusive drivers), but if they ever did the GPU is listed
        // once, not twice.
        let nouveau_gpus = [nouveau("0000:03:00.0", true)];
        let nvml = FakeNvml::with_identities(
            vec![],
            vec![NvmlDeviceIdentity {
                pci_bdf: "0000:03:00.0".into(),
                model_name: Some("RTX 4080".into()),
                driver_version: Some("565.77".into()),
                num_fans: 1,
                fan_rpm_available: false,
            }],
        );
        let gpus = gather_nvidia_gpus(&nouveau_gpus, &nvml);
        assert_eq!(gpus.len(), 1, "same BDF must not double-list: {gpus:#?}");
    }
}
