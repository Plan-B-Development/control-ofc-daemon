//! Minimal, hand-written FFI bindings to NVIDIA's NVML (`libnvidia-ml.so.1`),
//! loaded at runtime via `libloading` (DEC-204). **This is the ONLY module that
//! contains `unsafe` for NVIDIA support** — everything above it is safe Rust.
//!
//! ## Correctness basis
//!
//! Signatures, the `nvmlPciInfo_t` struct layout, enum values, and buffer sizes
//! are transcribed from NVIDIA's `nvml.h` (vendored in NVIDIA/go-nvml and
//! NVIDIA/nvidia-settings) and cross-verified byte-for-byte against
//! `nvml-wrapper-sys`'s bindgen output. `nvmlReturn_t` is modelled as a plain
//! 4-byte integer (`c_int`), never a Rust `enum`, because a C function returning
//! a value outside a Rust enum's declared discriminants is instant UB.
//!
//! ## Read-only + degrade-safe
//!
//! Only read-only symbols are bound; no fan-write or any state-changing symbol
//! is resolved. Every fallible step (missing library, missing symbol, non-zero
//! return) degrades to an error the caller turns into "telemetry absent" — the
//! daemon never panics and never depends on NVML being present. Field names are
//! Rust-conventional; the ABI is fixed by `#[repr(C)]` + field order/types.
//!
//! **Unverified-on-hardware caveat:** this code has been exercised only against
//! the [`FakeNvml`](super::nvml::FakeNvml) test double and the library-absent
//! degrade path — there is no NVIDIA GPU available to smoke-test the real calls.
//! It ships opt-in and default-off (`[detection] enable_nvidia_telemetry`).

use std::os::raw::{c_char, c_int, c_uint};

use libloading::Library;

// ── Opaque handle ───────────────────────────────────────────────────────────
// `typedef struct nvmlDevice_st* nvmlDevice_t;` — an opaque pointer (8 bytes on
// x86-64). NVML owns the pointee; we never dereference it.
#[repr(C)]
struct NvmlDeviceSt {
    _private: [u8; 0],
}
type NvmlDeviceHandle = *mut NvmlDeviceSt;

/// NVML status code. Modelled as an integer (see module docs) — compare against
/// the `NVML_*` constants below, never `match` it as a Rust enum.
pub type NvmlReturn = c_int;

pub const NVML_SUCCESS: NvmlReturn = 0;
pub const NVML_ERROR_NOT_SUPPORTED: NvmlReturn = 3;
/// A versioned-struct `version` field did not match the driver (see
/// [`fan_speed_info_version`]). Treated as "feature absent", never fatal.
pub const NVML_ERROR_ARGUMENT_VERSION_MISMATCH: NvmlReturn = 25;

/// `nvmlTemperatureSensors_t::NVML_TEMPERATURE_GPU` — the only sensor type.
const NVML_TEMPERATURE_GPU: c_int = 0;

/// Buffer sizes for NVML string getters (from nvml.h).
const NVML_DEVICE_NAME_V2_BUFFER_SIZE: usize = 96;
const NVML_SYSTEM_DRIVER_VERSION_BUFFER_SIZE: usize = 80;

// ── PCI info (highest-risk struct; layout cross-verified, sizeof == 68) ──────
#[repr(C)]
#[derive(Copy, Clone)]
struct NvmlPciInfo {
    bus_id_legacy: [c_char; 16], // NVML_DEVICE_PCI_BUS_ID_BUFFER_V2_SIZE
    domain: c_uint,
    bus: c_uint,
    device: c_uint,
    pci_device_id: c_uint,
    pci_subsystem_id: c_uint,
    /// The full "domain:bus:device.function" BDF string (NUL-terminated).
    bus_id: [c_char; 32], // NVML_DEVICE_PCI_BUS_ID_BUFFER_SIZE
}

impl NvmlPciInfo {
    fn zeroed() -> Self {
        Self {
            bus_id_legacy: [0; 16],
            domain: 0,
            bus: 0,
            device: 0,
            pci_device_id: 0,
            pci_subsystem_id: 0,
            bus_id: [0; 32],
        }
    }
}

// ── Versioned per-fan RPM struct (driver R565+; optional) ────────────────────
#[repr(C)]
#[derive(Copy, Clone)]
struct NvmlFanSpeedInfo {
    version: c_uint, // INPUT — must be set (see fan_speed_info_version)
    fan: c_uint,     // INPUT — fan index
    speed: c_uint,   // OUTPUT — RPM
}

/// `NVML_STRUCT_VERSION(FanSpeedInfo, 1)` = `sizeof(v1) | (1 << 24)`.
///
/// **Caveat (DEC-204):** the exact macro could not be read verbatim from the
/// header during research, so this is derived from NVML's documented
/// versioned-struct convention. A wrong value yields a clean
/// `NVML_ERROR_ARGUMENT_VERSION_MISMATCH` (handled as "RPM absent"), never UB.
fn fan_speed_info_version() -> c_uint {
    (std::mem::size_of::<NvmlFanSpeedInfo>() as c_uint) | (1u32 << 24)
}

// ── Function-pointer signatures (exact C prototypes) ─────────────────────────
type PfnInit = unsafe extern "C" fn() -> NvmlReturn;
type PfnShutdown = unsafe extern "C" fn() -> NvmlReturn;
type PfnCount = unsafe extern "C" fn(*mut c_uint) -> NvmlReturn;
type PfnHandleByIndex = unsafe extern "C" fn(c_uint, *mut NvmlDeviceHandle) -> NvmlReturn;
type PfnPciInfo = unsafe extern "C" fn(NvmlDeviceHandle, *mut NvmlPciInfo) -> NvmlReturn;
type PfnTemp = unsafe extern "C" fn(NvmlDeviceHandle, c_int, *mut c_uint) -> NvmlReturn;
type PfnNumFans = unsafe extern "C" fn(NvmlDeviceHandle, *mut c_uint) -> NvmlReturn;
type PfnFanSpeedV2 = unsafe extern "C" fn(NvmlDeviceHandle, c_uint, *mut c_uint) -> NvmlReturn;
type PfnFanSpeedRpm = unsafe extern "C" fn(NvmlDeviceHandle, *mut NvmlFanSpeedInfo) -> NvmlReturn;
type PfnName = unsafe extern "C" fn(NvmlDeviceHandle, *mut c_char, c_uint) -> NvmlReturn;
type PfnDriverVer = unsafe extern "C" fn(*mut c_char, c_uint) -> NvmlReturn;

/// A resolved, opaque NVML device handle. Safe to move/share across threads: it
/// is an opaque process-lifetime identifier owned by NVML, we only ever read
/// through it, and NVML is documented thread-safe. We never dereference it.
#[derive(Copy, Clone)]
pub struct NvmlHandle(NvmlDeviceHandle);
// SAFETY: see doc comment — opaque read-only ID; NVML is thread-safe.
unsafe impl Send for NvmlHandle {}
// SAFETY: see doc comment — opaque read-only ID; NVML is thread-safe.
unsafe impl Sync for NvmlHandle {}

/// Error from an NVML load or call.
#[derive(Debug)]
pub enum NvmlError {
    /// `libnvidia-ml.so.1` could not be loaded (absent / not an NVIDIA system).
    Load(libloading::Error),
    /// A required symbol was missing (driver too old for a required function).
    Symbol(libloading::Error),
    /// An NVML call returned a non-success status code.
    Call { op: &'static str, code: NvmlReturn },
}

impl std::fmt::Display for NvmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load(e) => write!(f, "libnvidia-ml.so.1 not loadable: {e}"),
            Self::Symbol(e) => write!(f, "NVML symbol missing: {e}"),
            Self::Call { op, code } => write!(f, "{op} failed (NVML code {code})"),
        }
    }
}

/// Resolve one NVML symbol from `lib` as a bare, `'static`-usable fn pointer.
///
/// # Safety
/// The caller asserts `T` is the exact C signature exported under `name`, and
/// that `lib` outlives every call made through the returned pointer.
unsafe fn resolve<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, NvmlError> {
    lib.get::<T>(name).map(|s| *s).map_err(NvmlError::Symbol)
}

fn check(ret: NvmlReturn, op: &'static str) -> Result<(), NvmlError> {
    if ret == NVML_SUCCESS {
        Ok(())
    } else {
        Err(NvmlError::Call { op, code: ret })
    }
}

/// Read a NUL-terminated C string buffer into a Rust `String`, handling the
/// platform-dependent signedness of `c_char` by reinterpreting each byte.
fn cstr_to_string(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A loaded NVML library with its read-only symbols resolved once.
///
/// Holds the `Library` for the process lifetime: dropping it calls `dlclose`,
/// which would invalidate every resolved function pointer, so `_lib` must
/// outlive all calls.
pub struct NvmlLib {
    // Owns the dlopen'd library so it outlives every call made through the
    // resolved fn pointers below; dropping it (dlclose) would leave them
    // dangling. `Drop for RealNvml` calls nvmlShutdown before this struct drops,
    // so the teardown order is always nvmlShutdown -> dlclose. Never read directly.
    #[allow(dead_code)]
    _lib: Library,
    init: PfnInit,
    shutdown: PfnShutdown,
    count: PfnCount,
    handle_by_index: PfnHandleByIndex,
    pci_info: PfnPciInfo,
    temperature: PfnTemp,
    num_fans: PfnNumFans,
    fan_speed_v2: PfnFanSpeedV2,
    // Optional — added in driver R565; `None` on older drivers.
    fan_speed_rpm: Option<PfnFanSpeedRpm>,
    // Optional identity getters (ancient, stable symbols — but resolved
    // best-effort so a name/version mismatch degrades to `None` rather than
    // disabling the whole NVML backend).
    name: Option<PfnName>,
    driver_version: Option<PfnDriverVer>,
}

impl NvmlLib {
    /// Load `libnvidia-ml.so.1` and resolve the read-only symbol set.
    ///
    /// Returns `NvmlError::Load` when the library is absent (the normal case on
    /// a non-NVIDIA system) and `NvmlError::Symbol` when a *required* symbol is
    /// missing. The optional per-fan-RPM symbol is resolved best-effort.
    pub fn load() -> Result<Self, NvmlError> {
        // SAFETY: loading a shared library runs its initializers. `libnvidia-ml.so.1`
        // is the stable versioned SONAME of NVIDIA's own runtime library; we load
        // it by fixed name only when the operator has opted in.
        let lib = unsafe { Library::new("libnvidia-ml.so.1") }.map_err(NvmlError::Load)?;

        // SAFETY: each symbol is resolved (via `resolve`) with the exact C
        // signature from nvml.h, cross-verified against nvml-wrapper-sys
        // (DEC-204). The resolved bare fn pointers stay valid because `lib` is
        // moved into the returned struct below and kept alive as long as they
        // are callable.
        unsafe {
            let init: PfnInit = resolve(&lib, b"nvmlInit_v2\0")?;
            let shutdown: PfnShutdown = resolve(&lib, b"nvmlShutdown\0")?;
            let count: PfnCount = resolve(&lib, b"nvmlDeviceGetCount_v2\0")?;
            let handle_by_index: PfnHandleByIndex =
                resolve(&lib, b"nvmlDeviceGetHandleByIndex_v2\0")?;
            let pci_info: PfnPciInfo = resolve(&lib, b"nvmlDeviceGetPciInfo_v3\0")?;
            let temperature: PfnTemp = resolve(&lib, b"nvmlDeviceGetTemperature\0")?;
            let num_fans: PfnNumFans = resolve(&lib, b"nvmlDeviceGetNumFans\0")?;
            let fan_speed_v2: PfnFanSpeedV2 = resolve(&lib, b"nvmlDeviceGetFanSpeed_v2\0")?;
            // Optional (driver R565+): absent -> no per-fan RPM, not an error.
            let fan_speed_rpm: Option<PfnFanSpeedRpm> =
                resolve::<PfnFanSpeedRpm>(&lib, b"nvmlDeviceGetFanSpeedRPM\0").ok();
            // Optional identity getters — best-effort (see field docs).
            let name: Option<PfnName> = resolve::<PfnName>(&lib, b"nvmlDeviceGetName\0").ok();
            let driver_version: Option<PfnDriverVer> =
                resolve::<PfnDriverVer>(&lib, b"nvmlSystemGetDriverVersion\0").ok();

            Ok(Self {
                init,
                shutdown,
                count,
                handle_by_index,
                pci_info,
                temperature,
                num_fans,
                fan_speed_v2,
                fan_speed_rpm,
                name,
                driver_version,
                _lib: lib,
            })
        }
    }

    /// `nvmlInit_v2()` — must be called once before any device query.
    pub fn init(&self) -> Result<(), NvmlError> {
        // SAFETY: takes no arguments; returns a status code.
        check(unsafe { (self.init)() }, "nvmlInit_v2")
    }

    /// `nvmlShutdown()` — best-effort cleanup; errors are ignored.
    pub fn shutdown(&self) {
        // SAFETY: takes no arguments; return value intentionally discarded.
        let _ = unsafe { (self.shutdown)() };
    }

    /// `nvmlDeviceGetCount_v2()`.
    pub fn device_count(&self) -> Result<u32, NvmlError> {
        let mut n: c_uint = 0;
        // SAFETY: `&mut n` is a valid, aligned out-pointer NVML writes the count into.
        check(unsafe { (self.count)(&mut n) }, "nvmlDeviceGetCount_v2")?;
        Ok(n)
    }

    /// `nvmlDeviceGetHandleByIndex_v2()`.
    pub fn handle_by_index(&self, index: u32) -> Result<NvmlHandle, NvmlError> {
        let mut h: NvmlDeviceHandle = std::ptr::null_mut();
        // SAFETY: `&mut h` is a valid out-pointer; NVML writes an opaque handle.
        check(
            unsafe { (self.handle_by_index)(index, &mut h) },
            "nvmlDeviceGetHandleByIndex_v2",
        )?;
        // Defensive: a conforming driver never returns SUCCESS with a null
        // handle, but guard anyway so a defective driver yields an error (the
        // device is then skipped at enumeration) rather than a null handle
        // flowing into later reads.
        if h.is_null() {
            return Err(NvmlError::Call {
                op: "nvmlDeviceGetHandleByIndex_v2 (null handle on success)",
                code: -1,
            });
        }
        Ok(NvmlHandle(h))
    }

    /// `nvmlDeviceGetPciInfo_v3()` — returns the BDF string (`bus_id`).
    pub fn pci_bdf(&self, dev: NvmlHandle) -> Result<String, NvmlError> {
        let mut info = NvmlPciInfo::zeroed();
        // SAFETY: `dev.0` is an NVML-issued handle; `&mut info` is a valid,
        // correctly-laid-out out-pointer NVML fills in.
        check(
            unsafe { (self.pci_info)(dev.0, &mut info) },
            "nvmlDeviceGetPciInfo_v3",
        )?;
        Ok(cstr_to_string(&info.bus_id))
    }

    /// `nvmlDeviceGetTemperature(NVML_TEMPERATURE_GPU)` — degrees Celsius.
    pub fn temperature_c(&self, dev: NvmlHandle) -> Result<u32, NvmlError> {
        let mut t: c_uint = 0;
        // SAFETY: valid handle + out-pointer; sensor type is the documented GPU enum.
        check(
            unsafe { (self.temperature)(dev.0, NVML_TEMPERATURE_GPU, &mut t) },
            "nvmlDeviceGetTemperature",
        )?;
        Ok(t)
    }

    /// `nvmlDeviceGetNumFans()`.
    pub fn num_fans(&self, dev: NvmlHandle) -> Result<u32, NvmlError> {
        let mut n: c_uint = 0;
        // SAFETY: valid handle + out-pointer.
        check(
            unsafe { (self.num_fans)(dev.0, &mut n) },
            "nvmlDeviceGetNumFans",
        )?;
        Ok(n)
    }

    /// `nvmlDeviceGetFanSpeed_v2()` — duty as a percentage of max noise
    /// tolerance (can exceed 100 on some parts).
    pub fn fan_duty_pct(&self, dev: NvmlHandle, fan: u32) -> Result<u32, NvmlError> {
        let mut pct: c_uint = 0;
        // SAFETY: valid handle + out-pointer; `fan` is a fan index < num_fans.
        check(
            unsafe { (self.fan_speed_v2)(dev.0, fan, &mut pct) },
            "nvmlDeviceGetFanSpeed_v2",
        )?;
        Ok(pct)
    }

    /// `nvmlDeviceGetFanSpeedRPM()` — per-fan RPM (driver R565+). Returns
    /// `Ok(None)` when the symbol is absent or the driver reports the versioned
    /// struct / feature is unsupported.
    pub fn fan_rpm(&self, dev: NvmlHandle, fan: u32) -> Result<Option<u32>, NvmlError> {
        let Some(rpm_fn) = self.fan_speed_rpm else {
            return Ok(None);
        };
        let mut info = NvmlFanSpeedInfo {
            version: fan_speed_info_version(),
            fan,
            speed: 0,
        };
        // SAFETY: valid handle; `info.version`/`info.fan` are set as required
        // inputs and `&mut info` is a valid out-pointer for the speed field.
        let ret = unsafe { (rpm_fn)(dev.0, &mut info) };
        if ret == NVML_ERROR_NOT_SUPPORTED || ret == NVML_ERROR_ARGUMENT_VERSION_MISMATCH {
            return Ok(None);
        }
        check(ret, "nvmlDeviceGetFanSpeedRPM")?;
        Ok(Some(info.speed))
    }

    /// Whether the optional per-fan RPM symbol (driver R565+) is available.
    pub fn has_fan_rpm(&self) -> bool {
        self.fan_speed_rpm.is_some()
    }

    /// `nvmlDeviceGetName()` — product name (e.g. "NVIDIA GeForce RTX 4080").
    /// Best-effort: `None` if the symbol is absent or the call fails.
    pub fn device_name(&self, dev: NvmlHandle) -> Option<String> {
        let name_fn = self.name?;
        let mut buf = [0 as c_char; NVML_DEVICE_NAME_V2_BUFFER_SIZE];
        // SAFETY: valid handle; `buf` is a valid, correctly-sized out-buffer.
        let ret = unsafe { name_fn(dev.0, buf.as_mut_ptr(), buf.len() as c_uint) };
        // Treat an empty string as absent so `display_label()` falls back.
        (ret == NVML_SUCCESS)
            .then(|| cstr_to_string(&buf))
            .filter(|s| !s.is_empty())
    }

    /// `nvmlSystemGetDriverVersion()` — NVIDIA driver version (system-wide, e.g.
    /// "565.77"). Best-effort: `None` if the symbol is absent or the call fails.
    pub fn driver_version(&self) -> Option<String> {
        let ver_fn = self.driver_version?;
        let mut buf = [0 as c_char; NVML_SYSTEM_DRIVER_VERSION_BUFFER_SIZE];
        // SAFETY: `buf` is a valid, correctly-sized out-buffer.
        let ret = unsafe { ver_fn(buf.as_mut_ptr(), buf.len() as c_uint) };
        // Treat an empty string as absent so `display_label()` falls back.
        (ret == NVML_SUCCESS)
            .then(|| cstr_to_string(&buf))
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_info_layout_is_68_bytes_repr_c() {
        // Guards the highest-risk ABI struct against accidental field-order or
        // type drift (cross-verified vs nvml-wrapper-sys / nvml.h, DEC-204).
        assert_eq!(std::mem::size_of::<NvmlPciInfo>(), 68);
        assert_eq!(std::mem::align_of::<NvmlPciInfo>(), 4);
        // Field OFFSETS — a size-only guard would MISS a bus_id/bus_id_legacy
        // swap (both keep sizeof 68), which would make `pci_bdf()` read the
        // wrong 16 bytes and return a garbled BDF. The BDF string is the 32-byte
        // field at offset 36.
        assert_eq!(std::mem::offset_of!(NvmlPciInfo, bus_id_legacy), 0);
        assert_eq!(std::mem::offset_of!(NvmlPciInfo, bus_id), 36);
    }

    #[test]
    fn fan_speed_info_layout_and_version() {
        assert_eq!(std::mem::size_of::<NvmlFanSpeedInfo>(), 12);
        // `speed` (the RPM we read back) must be the LAST field — guards a
        // fan/speed swap that would read the fan index back as the RPM.
        assert_eq!(std::mem::offset_of!(NvmlFanSpeedInfo, speed), 8);
        // sizeof(12) | (1 << 24) == 0x0100_000C
        assert_eq!(fan_speed_info_version(), 0x0100_000C);
    }

    #[test]
    fn cstr_to_string_stops_at_nul_and_handles_signedness() {
        // "0000:03:00.0" then NUL then garbage — only the BDF survives.
        let mut buf = [0 as c_char; 32];
        for (i, b) in b"0000:03:00.0".iter().enumerate() {
            buf[i] = *b as c_char;
        }
        buf[12] = 0;
        buf[13] = -1i8 as c_char; // high-bit byte after the NUL must be ignored
        assert_eq!(cstr_to_string(&buf), "0000:03:00.0");
    }

    #[test]
    fn library_absent_degrades_to_load_error() {
        // On this (non-NVIDIA) build host the real load must fail cleanly with a
        // Load error, never a panic — the degrade path we actually rely on.
        match NvmlLib::load() {
            Err(NvmlError::Load(_)) => {}
            Err(other) => panic!("expected Load error, got {other}"),
            Ok(_) => { /* an NVIDIA host — also fine, just no assertion */ }
        }
    }
}
