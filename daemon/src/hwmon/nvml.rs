//! Opt-in, read-only NVIDIA telemetry backend over NVML (DEC-204).
//!
//! This is the safe layer over [`crate::hwmon::nvml_sys`]. It exposes a
//! [`NvmlBackend`] trait with three implementations:
//!
//! - [`RealNvml`] — dlopens `libnvidia-ml.so.1`, enumerates GPUs at startup, and
//!   reads temperature + fan telemetry per poll tick.
//! - [`FakeNvml`] — scripted readings for deterministic tests (no hardware).
//! - [`DisabledNvml`] — the default: NVML never loaded, always empty.
//!
//! ## Read-only + degrade-safe
//!
//! No fan-write path exists. Every per-tick read is best-effort: any NVML error
//! (including `NOT_SUPPORTED`) becomes an absent field, never a daemon fault, and
//! never per-tick log spam. NVIDIA GPUs surface under the vendor-level
//! `nvidia_gpu:` prefix shared with the nouveau leg.
//!
//! **Experimental / unverified:** the real path has never run against an NVIDIA
//! GPU (none available). It is gated behind `[detection] enable_nvidia_telemetry`
//! (default `false`); when disabled the library is never even loaded.

use std::sync::Arc;

use crate::hwmon::nvml_sys::NvmlLib;

/// One NVIDIA GPU's current telemetry snapshot. Every field is best-effort —
/// `None` means "not available this tick" (unsupported, transient error, or no
/// fan). `fan_duty_pct` is the firmware-reported current duty as a percentage of
/// max noise tolerance — distinct from any daemon-commanded value, and per NVML
/// it **may exceed 100** on some parts. `fan_rpm` needs driver R565+.
#[derive(Debug, Clone, PartialEq)]
pub struct NvmlReading {
    /// PCI BDF (e.g. `0000:03:00.0`), used to form the `nvidia_gpu:<BDF>` id.
    pub pci_bdf: String,
    pub temp_c: Option<f64>,
    pub fan_duty_pct: Option<u8>,
    pub fan_rpm: Option<u16>,
}

/// Static identity for one NVIDIA GPU, gathered once at NVML init — distinct
/// from the per-tick [`NvmlReading`] telemetry. Feeds the `/capabilities` +
/// `/diagnostics/hardware` surfaces (DEC-204). `model_name`/`driver_version` are
/// best-effort (`None` if the optional NVML getters are absent).
#[derive(Debug, Clone, PartialEq)]
pub struct NvmlDeviceIdentity {
    pub pci_bdf: String,
    pub model_name: Option<String>,
    pub driver_version: Option<String>,
    pub num_fans: u32,
    /// Whether per-fan RPM can be read (device has fans AND the R565+ symbol is present).
    pub fan_rpm_available: bool,
}

/// Read-only NVIDIA telemetry source. `read_all` returns one reading per
/// detected GPU (empty when NVML is disabled or unavailable). Implementations
/// are `Send + Sync` so the backend can be shared into the poll loop's blocking
/// leg.
pub trait NvmlBackend: Send + Sync {
    /// Read current telemetry for every detected NVIDIA GPU. Called once per
    /// poll tick from a blocking thread; must not panic.
    fn read_all(&self) -> Vec<NvmlReading>;

    /// Static per-GPU identity gathered at init (empty when disabled/absent).
    /// Read once at startup for the capability + diagnostics surfaces.
    fn devices(&self) -> Vec<NvmlDeviceIdentity>;
}

/// NVML unavailable/disabled — the default. The library is never loaded.
pub struct DisabledNvml;

impl NvmlBackend for DisabledNvml {
    fn read_all(&self) -> Vec<NvmlReading> {
        Vec::new()
    }

    fn devices(&self) -> Vec<NvmlDeviceIdentity> {
        Vec::new()
    }
}

/// Scripted backend for deterministic tests (no hardware, no FFI).
pub struct FakeNvml {
    readings: Vec<NvmlReading>,
    identities: Vec<NvmlDeviceIdentity>,
}

impl FakeNvml {
    pub fn new(readings: Vec<NvmlReading>) -> Self {
        Self {
            readings,
            identities: Vec::new(),
        }
    }

    pub fn with_identities(
        readings: Vec<NvmlReading>,
        identities: Vec<NvmlDeviceIdentity>,
    ) -> Self {
        Self {
            readings,
            identities,
        }
    }
}

impl NvmlBackend for FakeNvml {
    fn read_all(&self) -> Vec<NvmlReading> {
        self.readings.clone()
    }

    fn devices(&self) -> Vec<NvmlDeviceIdentity> {
        self.identities.clone()
    }
}

/// A GPU discovered at NVML init: its handle, BDF, and fan count.
struct RealDevice {
    handle: crate::hwmon::nvml_sys::NvmlHandle,
    bdf: String,
    num_fans: u32,
    model_name: Option<String>,
}

/// The live NVML backend. Owns the loaded library and the device handles for
/// the process lifetime; `Drop` calls `nvmlShutdown`.
pub struct RealNvml {
    lib: NvmlLib,
    devices: Vec<RealDevice>,
    /// NVIDIA driver version (system-wide), read once at init. Best-effort.
    driver_version: Option<String>,
}

impl RealNvml {
    /// Load NVML, initialise it, and enumerate GPUs (reading each BDF, fan
    /// count, and model name once). Returns an error if the library/`nvmlInit`
    /// is unavailable; a per-device failure skips that device rather than
    /// failing the whole load.
    fn load_and_init() -> Result<Self, crate::hwmon::nvml_sys::NvmlError> {
        let lib = NvmlLib::load()?;
        lib.init()?;
        let count = lib.device_count()?;
        let driver_version = lib.driver_version();

        let mut devices = Vec::new();
        for i in 0..count {
            let handle = match lib.handle_by_index(i) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("NVML: cannot get handle for device {i}: {e}");
                    continue;
                }
            };
            let bdf = match lib.pci_bdf(handle) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("NVML: cannot read PCI info for device {i}: {e}");
                    continue;
                }
            };
            let num_fans = lib.num_fans(handle).unwrap_or(0);
            let model_name = lib.device_name(handle);
            log::info!(
                "NVIDIA GPU detected via NVML: {} PCI {bdf} ({num_fans} fan(s)) \
                 [read-only, experimental]",
                model_name.as_deref().unwrap_or("(unknown model)")
            );
            devices.push(RealDevice {
                handle,
                bdf,
                num_fans,
                model_name,
            });
        }
        Ok(Self {
            lib,
            devices,
            driver_version,
        })
    }
}

impl NvmlBackend for RealNvml {
    fn read_all(&self) -> Vec<NvmlReading> {
        self.devices
            .iter()
            .map(|d| {
                let temp_c = self.lib.temperature_c(d.handle).ok().map(|t| t as f64);
                // One aggregate fan entity per GPU (DEC-044): read fan 0 as the
                // representative. Duty % is available on any GPU that has fans
                // (no driver-version gate); RPM needs driver R565+.
                let (fan_duty_pct, fan_rpm) = if d.num_fans >= 1 {
                    let duty = self
                        .lib
                        .fan_duty_pct(d.handle, 0)
                        .ok()
                        .map(|p| p.min(u8::MAX as u32) as u8);
                    let rpm = self
                        .lib
                        .fan_rpm(d.handle, 0)
                        .ok()
                        .flatten()
                        .map(|r| r.min(u16::MAX as u32) as u16);
                    (duty, rpm)
                } else {
                    (None, None)
                };
                NvmlReading {
                    pci_bdf: d.bdf.clone(),
                    temp_c,
                    fan_duty_pct,
                    fan_rpm,
                }
            })
            .collect()
    }

    fn devices(&self) -> Vec<NvmlDeviceIdentity> {
        let has_rpm = self.lib.has_fan_rpm();
        self.devices
            .iter()
            .map(|d| NvmlDeviceIdentity {
                pci_bdf: d.bdf.clone(),
                model_name: d.model_name.clone(),
                driver_version: self.driver_version.clone(),
                num_fans: d.num_fans,
                fan_rpm_available: d.num_fans >= 1 && has_rpm,
            })
            .collect()
    }
}

impl Drop for RealNvml {
    fn drop(&mut self) {
        self.lib.shutdown();
    }
}

/// Construct the NVIDIA telemetry backend for this daemon run.
///
/// Returns [`DisabledNvml`] (the library is never loaded) when `enabled` is
/// `false` — the default — or when NVML cannot be loaded/initialised. Only when
/// the operator opts in *and* NVML is present does this return a live
/// [`RealNvml`]. Never fails: NVML absence is a normal degraded state.
pub fn init_nvml_backend(enabled: bool) -> Arc<dyn NvmlBackend> {
    if !enabled {
        log::debug!("NVIDIA NVML telemetry disabled ([detection] enable_nvidia_telemetry = false)");
        return Arc::new(DisabledNvml);
    }
    match RealNvml::load_and_init() {
        Ok(real) => {
            log::info!(
                "NVIDIA NVML telemetry enabled: {} GPU(s) [experimental — unverified on hardware]",
                real.devices.len()
            );
            Arc::new(real)
        }
        Err(e) => {
            log::info!("NVIDIA NVML telemetry unavailable: {e} (continuing without it)");
            Arc::new(DisabledNvml)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(bdf: &str) -> NvmlReading {
        NvmlReading {
            pci_bdf: bdf.into(),
            temp_c: Some(55.0),
            fan_duty_pct: Some(42),
            fan_rpm: Some(1400),
        }
    }

    #[test]
    fn disabled_backend_is_empty() {
        assert!(DisabledNvml.read_all().is_empty());
    }

    #[test]
    fn fake_backend_returns_scripted_readings() {
        let fake = FakeNvml::new(vec![sample("0000:03:00.0"), sample("0000:0a:00.0")]);
        let out = fake.read_all();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].pci_bdf, "0000:03:00.0");
        assert_eq!(out[0].temp_c, Some(55.0));
        assert_eq!(out[0].fan_duty_pct, Some(42));
        assert_eq!(out[0].fan_rpm, Some(1400));
        // The second GPU must survive too (guards a silently-dropped tail).
        assert_eq!(out[1].pci_bdf, "0000:0a:00.0");
        assert_eq!(out[1].fan_rpm, Some(1400));
    }

    #[test]
    fn factory_disabled_when_flag_off() {
        // Flag off => DisabledNvml, library never loaded.
        let backend = init_nvml_backend(false);
        assert!(backend.read_all().is_empty());
    }

    #[test]
    fn factory_degrades_to_disabled_when_lib_absent() {
        // On this non-NVIDIA build host, opting in still yields an empty backend
        // (RealNvml::load fails -> DisabledNvml) — never a panic. This is the
        // real degrade path we rely on; the only path verifiable without a GPU.
        let backend = init_nvml_backend(true);
        assert!(backend.read_all().is_empty());
    }

    #[test]
    fn backend_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn NvmlBackend>>();
    }

    #[test]
    fn fake_backend_devices_returns_identities() {
        let ident = NvmlDeviceIdentity {
            pci_bdf: "0000:03:00.0".into(),
            model_name: Some("NVIDIA GeForce RTX 4080".into()),
            driver_version: Some("565.77".into()),
            num_fans: 2,
            fan_rpm_available: true,
        };
        let fake = FakeNvml::with_identities(vec![], vec![ident.clone()]);
        assert_eq!(fake.devices(), vec![ident]);
        // Telemetry and identity are independent channels.
        assert!(fake.read_all().is_empty());
    }

    #[test]
    fn disabled_backend_devices_is_empty() {
        assert!(DisabledNvml.devices().is_empty());
    }
}
