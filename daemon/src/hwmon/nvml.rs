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

/// Read-only NVIDIA telemetry source. `read_all` returns one reading per
/// detected GPU (empty when NVML is disabled or unavailable). Implementations
/// are `Send + Sync` so the backend can be shared into the poll loop's blocking
/// leg.
pub trait NvmlBackend: Send + Sync {
    /// Read current telemetry for every detected NVIDIA GPU. Called once per
    /// poll tick from a blocking thread; must not panic.
    fn read_all(&self) -> Vec<NvmlReading>;
}

/// NVML unavailable/disabled — the default. The library is never loaded.
pub struct DisabledNvml;

impl NvmlBackend for DisabledNvml {
    fn read_all(&self) -> Vec<NvmlReading> {
        Vec::new()
    }
}

/// Scripted backend for deterministic tests (no hardware, no FFI).
pub struct FakeNvml {
    readings: Vec<NvmlReading>,
}

impl FakeNvml {
    pub fn new(readings: Vec<NvmlReading>) -> Self {
        Self { readings }
    }
}

impl NvmlBackend for FakeNvml {
    fn read_all(&self) -> Vec<NvmlReading> {
        self.readings.clone()
    }
}

/// A GPU discovered at NVML init: its handle, BDF, and fan count.
struct RealDevice {
    handle: crate::hwmon::nvml_sys::NvmlHandle,
    bdf: String,
    num_fans: u32,
}

/// The live NVML backend. Owns the loaded library and the device handles for
/// the process lifetime; `Drop` calls `nvmlShutdown`.
pub struct RealNvml {
    lib: NvmlLib,
    devices: Vec<RealDevice>,
}

impl RealNvml {
    /// Load NVML, initialise it, and enumerate GPUs (reading each BDF + fan
    /// count once). Returns an error if the library/`nvmlInit` is unavailable;
    /// a per-device failure skips that device rather than failing the whole load.
    fn load_and_init() -> Result<Self, crate::hwmon::nvml_sys::NvmlError> {
        let lib = NvmlLib::load()?;
        lib.init()?;
        let count = lib.device_count()?;

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
            log::info!(
                "NVIDIA GPU detected via NVML: PCI {bdf} ({num_fans} fan(s)) \
                 [read-only, experimental]"
            );
            devices.push(RealDevice {
                handle,
                bdf,
                num_fans,
            });
        }
        Ok(Self { lib, devices })
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
}
