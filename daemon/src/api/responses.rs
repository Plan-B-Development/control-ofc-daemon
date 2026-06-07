//! JSON response types for the IPC API.
//!
//! All types derive `Serialize` for JSON output. Field names are stable
//! within API v1 — changes must be additive only.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const API_VERSION: u32 = 1;

/// Response for `/status` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub api_version: u32,
    pub daemon_version: String,
    pub overall_status: String,
    pub subsystems: Vec<SubsystemStatus>,
    pub counters: Counters,
    /// Seconds since daemon process started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    /// Seconds since last GUI write command (None if no writes received).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gui_last_seen_seconds_ago: Option<u64>,
    /// Thermal safety override state: `"normal"` | `"recovery"` | `"emergency"`.
    /// Mirrors the value the profile engine reports each tick (the same string
    /// `/diagnostics/hardware` exposes) so the GUI can stand its control loop
    /// down while the daemon is forcing safety PWM (DEC-132). Additive field —
    /// API_VERSION unchanged.
    pub thermal_state: String,
}

/// Per-subsystem health status.
#[derive(Debug, Clone, Serialize)]
pub struct SubsystemStatus {
    pub name: String,
    pub status: String,
    pub age_ms: Option<u64>,
    pub reason: String,
}

/// Operational counters.
#[derive(Debug, Clone, Serialize)]
pub struct Counters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_summary: Option<String>,
}

/// Response for `/sensors` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct SensorsResponse {
    pub api_version: u32,
    pub sensors: Vec<SensorEntry>,
}

/// A single sensor reading in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct SensorEntry {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub value_c: f64,
    pub source: String,
    pub age_ms: u64,
    /// Temperature change rate in degrees C per second (smoothed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_c_per_s: Option<f64>,
    /// Session minimum temperature since daemon start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_min_c: Option<f64>,
    /// Session maximum temperature since daemon start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_max_c: Option<f64>,
    /// Hwmon chip name (e.g. "k10temp", "nct6683", "it8696").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chip_name: Option<String>,
    /// Sysfs `tempN_type` value if present (3=diode, 4=thermistor, 5=AMD TSI, 6=Intel PECI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_type: Option<u8>,
    /// Curated hwmon temperature-threshold sysfs attributes (DEC-117). Omitted
    /// from the JSON when the driver exposes none of the attributes for this
    /// sensor. Alarm flags are sampled at discovery time only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<SensorThresholdsResponse>,
}

/// JSON serialization shape for the curated hwmon threshold attributes
/// exposed on `SensorEntry` (DEC-117).
///
/// Every field is omitted from the JSON when `None`, so a sensor with only
/// `crit` available emits `{"thresholds": {"crit_c": 105.0}}` rather than
/// twelve null fields. Mirrors `crate::hwmon::types::SensorThresholds`.
#[derive(Debug, Clone, Serialize)]
pub struct SensorThresholdsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit_hyst_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emergency_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emergency_hyst_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcrit_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alarm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_alarm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit_alarm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault: Option<bool>,
}

impl From<&crate::hwmon::types::SensorThresholds> for SensorThresholdsResponse {
    fn from(t: &crate::hwmon::types::SensorThresholds) -> Self {
        Self {
            max_c: t.max_c,
            min_c: t.min_c,
            crit_c: t.crit_c,
            crit_hyst_c: t.crit_hyst_c,
            emergency_c: t.emergency_c,
            emergency_hyst_c: t.emergency_hyst_c,
            lcrit_c: t.lcrit_c,
            offset_c: t.offset_c,
            alarm: t.alarm,
            max_alarm: t.max_alarm,
            crit_alarm: t.crit_alarm,
            fault: t.fault,
        }
    }
}

/// Response for `/fans` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct FansResponse {
    pub api_version: u32,
    pub fans: Vec<FanEntry>,
}

/// A single fan in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct FanEntry {
    pub id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_commanded_pwm: Option<u8>,
    pub age_ms: u64,
    /// True when RPM is 0 but last_commanded_pwm is above the safety floor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_detected: Option<bool>,
}

/// Request body for `POST /fans/openfan/{channel}/pwm` and `POST /fans/openfan/pwm`.
#[derive(Debug, Clone, Deserialize)]
pub struct SetPwmRequest {
    pub pwm_percent: u8,
}

/// Request body for `POST /fans/openfan/{channel}/target_rpm`.
#[derive(Debug, Clone, Deserialize)]
pub struct SetRpmRequest {
    pub target_rpm: u16,
}

/// Response for successful per-channel PWM set.
#[derive(Debug, Clone, Serialize)]
pub struct SetPwmResponse {
    pub api_version: u32,
    pub channel: u8,
    pub pwm_percent: u8,
    pub coalesced: bool,
}

/// Response for successful all-channel PWM set.
#[derive(Debug, Clone, Serialize)]
pub struct SetPwmAllResponse {
    pub api_version: u32,
    pub pwm_percent: u8,
    pub channels_affected: u8,
    /// True when the controller short-circuited the serial command because
    /// every channel already held this value. Lets the GUI distinguish
    /// "no-op" from "wrote and cache fresh".
    pub coalesced: bool,
}

/// Response for successful target RPM set.
#[derive(Debug, Clone, Serialize)]
pub struct SetRpmResponse {
    pub api_version: u32,
    pub channel: u8,
    pub target_rpm: u16,
}

/// Request body for `POST /hwmon/lease/take`.
#[derive(Debug, Clone, Deserialize)]
pub struct TakeLeaseRequest {
    #[serde(default)]
    pub owner_hint: String,
}

/// Response for successful lease take.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseResponse {
    pub api_version: u32,
    pub lease_id: String,
    pub owner_hint: String,
    pub ttl_seconds: u64,
}

/// Request body for `POST /hwmon/lease/release`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseLeaseRequest {
    pub lease_id: String,
}

/// Response for successful lease release.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseReleasedResponse {
    pub api_version: u32,
    pub released: bool,
}

/// Request body for `POST /hwmon/{header_id}/pwm`.
#[derive(Debug, Clone, Deserialize)]
pub struct HwmonSetPwmRequest {
    pub pwm_percent: u8,
    pub lease_id: String,
}

/// Response for successful hwmon PWM set.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonSetPwmResponse {
    pub api_version: u32,
    pub header_id: String,
    pub pwm_percent: u8,
    pub raw_value: u8,
}

/// A single PWM header in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct PwmHeaderEntry {
    pub id: String,
    pub label: String,
    pub chip_name: String,
    /// Device identifier (PCI BDF or platform device name).
    pub device_id: String,
    pub pwm_index: u8,
    pub supports_enable: bool,
    pub rpm_available: bool,
    pub min_pwm_percent: u8,
    pub max_pwm_percent: u8,
    /// Whether the pwmN file is writable (checked at discovery).
    pub is_writable: bool,
    /// PWM/DC mode from pwmN_mode (0=DC, 1=PWM), absent if file not exposed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pwm_mode: Option<u8>,
}

impl From<&crate::hwmon::pwm_discovery::PwmHeaderDescriptor> for PwmHeaderEntry {
    /// Single source of truth for the descriptor → wire-entry mapping
    /// (DEC-146 P3-12). Previously duplicated field-for-field in the
    /// headers and rescan handlers, which drifts when fields are added —
    /// this contract recently grew `pwm_mode` and `is_writable`.
    fn from(h: &crate::hwmon::pwm_discovery::PwmHeaderDescriptor) -> Self {
        PwmHeaderEntry {
            id: h.id.clone(),
            label: h.label.clone(),
            chip_name: h.chip_name.clone(),
            device_id: h.device_id.clone(),
            pwm_index: h.pwm_index,
            supports_enable: h.supports_enable,
            rpm_available: h.rpm_available,
            min_pwm_percent: h.min_pwm_percent,
            max_pwm_percent: h.max_pwm_percent,
            is_writable: h.is_writable,
            pwm_mode: h.pwm_mode,
        }
    }
}

/// Response for `GET /hwmon/headers`.
#[derive(Debug, Clone, Serialize)]
pub struct PwmHeadersResponse {
    pub api_version: u32,
    pub headers: Vec<PwmHeaderEntry>,
}

/// Response for `GET /capabilities`.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilitiesResponse {
    pub api_version: u32,
    pub daemon_version: String,
    pub ipc_transport: &'static str,
    pub devices: DeviceCapabilities,
    pub features: FeatureFlags,
    pub limits: Limits,
}

/// Per-device-group capability info.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCapabilities {
    pub openfan: OpenfanCapability,
    pub hwmon: HwmonCapability,
    pub amd_gpu: AmdGpuCapability,
    /// Intel discrete GPU (Arc) capability. Additive field (DEC-121) — older
    /// GUIs ignore it; read-only monitoring only, never fan-writable.
    pub intel_gpu: IntelGpuCapability,
    pub aio_hwmon: UnsupportedCapability,
    pub aio_usb: UnsupportedCapability,
}

/// AMD discrete GPU capability details.
#[derive(Debug, Clone, Serialize)]
pub struct AmdGpuCapability {
    pub present: bool,
    /// Marketing name (e.g. "RX 9070 XT") or null if unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Compact display label (e.g. "9070XT" or "AMD D-GPU").
    pub display_label: String,
    /// PCI Bus:Device.Function address (legacy field name).
    ///
    /// Deprecated alias for `pci_bdf` — both fields carry the same value
    /// during the transition to canonical naming (M11 contract remediation,
    /// see GUI `CHANGELOG.md` v1.6.0 and `DECISIONS.md` DEC-042). New callers
    /// should prefer `pci_bdf`; this field will be removed in a future
    /// major version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<String>,
    /// PCI Bus:Device.Function address (canonical).
    ///
    /// Matches the field name already used by `GpuDiagnostics`, eliminating
    /// the `/capabilities` vs `/diagnostics/hardware` naming mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_bdf: Option<String>,
    /// PCI device ID (e.g. 0x7550 for Navi 48).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_device_id: Option<u16>,
    /// PCI revision (e.g. 0xC0 for XT variant).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_revision: Option<u8>,
    /// Fan control method: "pmfw_curve", "hwmon_pwm", or "none".
    pub fan_control_method: String,
    /// Whether PMFW fan curve is supported (RDNA3+).
    pub pmfw_supported: bool,
    /// Whether fan RPM reading is available.
    pub fan_rpm_available: bool,
    /// Whether this GPU has fan write capability (PMFW curve or hwmon pwm1+enable).
    pub fan_write_supported: bool,
    /// Whether this is a discrete (VGA) GPU vs render-only.
    pub is_discrete: bool,
    /// Whether the amdgpu overdrive feature is enabled (ppfeaturemask bit 14).
    pub overdrive_enabled: bool,
    /// Whether the PMFW zero-RPM sysfs file exists.
    pub gpu_zero_rpm_available: bool,
    /// Kernel-version advisories applicable to this GPU.
    ///
    /// Empty in normal operation. Populated when the running kernel matches a
    /// known amdgpu regression (e.g. RDNA3/RDNA4 hard-hang on 6.19, R9700 SMU
    /// mismatch on 7.0). The GUI surfaces high/critical entries as a one-time
    /// popup. See `crate::hwmon::kernel_warnings` for the catalog.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_warnings: Vec<crate::hwmon::kernel_warnings::KernelWarning>,
}

/// Intel discrete GPU (Arc) capability details (DEC-121).
///
/// Monitoring-only: Intel GPU fan control is firmware-managed and has no
/// userspace write path, so `fan_control_method` is always `"read_only"` (or
/// `"none"` when no fan is exposed) and there is no `fan_write_supported`
/// field — writes are never offered. Mirrors the read-only subset of
/// `AmdGpuCapability` without any of the PMFW/overdrive/zero-RPM fields.
#[derive(Debug, Clone, Serialize)]
pub struct IntelGpuCapability {
    pub present: bool,
    /// Marketing name (e.g. "Arc B580") or null if the device ID is unmapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Compact display label (e.g. "Arc B580" or "Intel D-GPU").
    pub display_label: String,
    /// PCI Bus:Device.Function address (legacy alias, kept symmetric with
    /// `amd_gpu` for the GUI's `_coalesce_pci_bdf` tolerance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<String>,
    /// PCI Bus:Device.Function address (canonical).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_bdf: Option<String>,
    /// PCI device ID (e.g. 0xE20B for Arc B580).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_device_id: Option<u16>,
    /// Kernel driver backing the GPU: "xe" or "i915".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Fan control method: always "read_only" (fan present) or "none".
    pub fan_control_method: String,
    /// Whether fan RPM reading is available (`fan1_input`).
    pub fan_rpm_available: bool,
    /// Whether this is a discrete (VGA) GPU. Always true for a detected Intel
    /// GPU (the hwmon node is DGFX-gated), emitted for symmetry.
    pub is_discrete: bool,
}

/// OpenFanController capability details.
#[derive(Debug, Clone, Serialize)]
pub struct OpenfanCapability {
    pub present: bool,
    pub channels: u8,
    pub rpm_support: bool,
    pub write_support: bool,
}

/// Hwmon PWM capability details.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonCapability {
    pub present: bool,
    pub pwm_header_count: usize,
    pub lease_required: bool,
    pub write_support: bool,
}

/// Placeholder for unsupported device groups.
#[derive(Debug, Clone, Serialize)]
pub struct UnsupportedCapability {
    pub present: bool,
    pub status: &'static str,
}

/// Feature flags for the GUI.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureFlags {
    pub openfan_write_supported: bool,
    pub hwmon_write_supported: bool,
    pub lease_required_for_hwmon_writes: bool,
}

/// Policy-level limits the GUI should respect.
#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    pub pwm_percent_min: u8,
    pub pwm_percent_max: u8,
    pub openfan_stop_timeout_s: u8,
}

/// Response for `GET /hwmon/lease/status`.
#[derive(Debug, Clone, Serialize)]
pub struct LeaseStatusResponse {
    pub api_version: u32,
    pub lease_required: bool,
    pub held: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds_remaining: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_hint: Option<String>,
}

/// Request body for `POST /hwmon/lease/renew`.
#[derive(Debug, Clone, Deserialize)]
pub struct RenewLeaseRequest {
    pub lease_id: String,
}

/// Response for `GET /sensors/history`.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryResponse {
    pub api_version: u32,
    pub entity_id: String,
    pub points: Vec<crate::health::history::HistorySample>,
}

/// Response for calibration sweep endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationResponse {
    pub api_version: u32,
    pub fan_id: String,
    pub points: Vec<crate::api::calibration::CalPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_pwm: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_pwm: Option<u8>,
    pub min_rpm: u16,
    pub max_rpm: u16,
}

/// Response for `GET /poll` — combined sensors, fans, and status in one call.
#[derive(Debug, Clone, Serialize)]
pub struct PollResponse {
    pub api_version: u32,
    pub status: StatusResponse,
    pub sensors: Vec<SensorEntry>,
    pub fans: Vec<FanEntry>,
}

// ── Hardware diagnostics ────────────────────────────────────────────

/// Response for `GET /diagnostics/hardware`.
#[derive(Debug, Clone, Serialize)]
pub struct HardwareDiagnosticsResponse {
    pub api_version: u32,
    pub hwmon: HwmonDiagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuDiagnostics>,
    /// Intel discrete GPU diagnostics (DEC-121). Additive — omitted when no
    /// Intel GPU is present; older clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intel_gpu: Option<IntelGpuDiagnostics>,
    pub thermal_safety: ThermalSafetyInfo,
    pub kernel_modules: Vec<KernelModuleInfo>,
    pub acpi_conflicts: Vec<AcpiConflictInfo>,
    pub board: BoardInfo,
    /// Chip names this DMI board is *expected* to expose, sourced from a
    /// curated dual-chip board lookup (`it87.c` DMI table + community
    /// reports). Empty when the board is not in the lookup. The GUI
    /// compares this against `hwmon.chips_detected[].chip_name` to detect
    /// missing chips that the driver failed to enumerate (DEC-101 — most
    /// commonly: the secondary IT87952E on Gigabyte X670/X870/Z790 boards
    /// that needs explicit `mmio=on` modparam to bind reliably).
    /// Older clients that don't know this field default to an empty list
    /// because the field is `skip_serializing_if = "Vec::is_empty"` on
    /// the wire and the GUI's `_filter_fields` parser tolerates it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_chips: Vec<String>,
    /// Best-effort kernel-level chip detection — chip names parsed out of
    /// `/dev/kmsg` `it87:` log lines (DEC-101). Populated when the daemon
    /// can read the kernel ring buffer (Arch default: `dmesg_restrict=0`).
    /// Empty when kmsg is not readable or no matches were found. Useful
    /// for the "kernel found chip but driver did not bind" diagnostic;
    /// not authoritative — the hwmon-bound chips in `chips_detected` are
    /// the source of truth for which PWM headers actually work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_detected_chips: Vec<String>,
    /// Pairs of currently loaded driver modules that are known to race for
    /// the same chip — distinct from `acpi_conflicts` (which is about I/O
    /// port ranges) and from the GUI's `CONFLICTING_MODULE_SETS` (which is
    /// a static name-pair table the GUI applies after the fact). Reported
    /// as CRITICAL severity for the canonical `(nct6687, nct6775)` case
    /// because writing PWM can corrupt the chip's non-volatile state — see
    /// `ModuleCollisionInfo` doc comment for the upstream incident.
    /// Empty (and omitted from the wire) on healthy systems.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_collisions: Vec<ModuleCollisionInfo>,
    /// CPU vendor read from `/proc/cpuinfo` `vendor_id` — `"Intel"`,
    /// `"AMD"`, or `""` (empty) when unknown. DEC-110: lets the GUI scope
    /// vendor-platform quirks (e.g. "this BIOS quirk applies on MSI Intel
    /// Z890 boards, not on MSI AMD X870E") without having to infer the
    /// platform from board name. Empty on hypervisors or when `/proc/cpuinfo`
    /// is unreadable; older clients that don't know the field skip it via
    /// `skip_serializing_if = "String::is_empty"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cpu_vendor: String,
    /// AMD VGA-class PCI devices and their driver binding (DEC-119). Detected
    /// independently of hwmon so a GPU whose `amdgpu` driver failed to bind
    /// (blacklist, KMS failure, vfio-pci passthrough) is still reported —
    /// such a device produces no hwmon node and is absent from `gpu`. Empty
    /// (and omitted) when no AMD VGA device exists; older clients skip it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub amd_pci_devices: Vec<AmdPciDeviceInfo>,
    /// Whether the `amdgpu` kernel module is loaded (`/sys/module/amdgpu`).
    /// Paired with `amd_pci_devices`: a device present with the module *not*
    /// loaded points at a blacklist or missing module; module loaded but
    /// device unbound points at a bind failure. Defaults to `false` for
    /// older clients that don't send the field.
    #[serde(default)]
    pub amdgpu_module_loaded: bool,
}

/// Hwmon chip diagnostics.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonDiagnostics {
    pub chips_detected: Vec<HwmonChipInfo>,
    pub total_headers: usize,
    pub writable_headers: usize,
    /// Cumulative BIOS pwm_enable reclaim events per header ID.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub enable_revert_counts: HashMap<String, u64>,
}

/// Per-chip identification and driver info.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonChipInfo {
    pub chip_name: String,
    pub device_id: String,
    pub expected_driver: String,
    pub in_mainline_kernel: bool,
    pub header_count: usize,
}

/// GPU-specific diagnostics.
#[derive(Debug, Clone, Serialize)]
pub struct GpuDiagnostics {
    /// PCI Bus:Device.Function address (canonical).
    pub pci_bdf: String,
    /// Alias for `pci_bdf` — emitted so callers that consumed
    /// `/capabilities.amd_gpu.pci_id` can use the same field name here
    /// during the transition window (M11). Same string as `pci_bdf`.
    pub pci_id: String,
    pub pci_device_id: u16,
    pub pci_revision: u8,
    pub model_name: Option<String>,
    pub fan_control_method: String,
    pub overdrive_enabled: bool,
    pub ppfeaturemask: Option<String>,
    pub ppfeaturemask_bit14_set: bool,
    pub zero_rpm_available: bool,
    /// PMFW OD_RANGE fan-speed minimum (percent), parsed from the device's
    /// `fan_curve` `FAN_CURVE(fan speed)` range. The amdgpu driver rejects
    /// curve points below this with `EINVAL`, so it is the firmware-enforced
    /// reason a PMFW GPU fan cannot be driven to 0% via the curve (typically
    /// 15% on RDNA3+). `None` for non-PMFW GPUs. Additive field — older
    /// clients skip it (`skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_speed_min_pct: Option<u8>,
    /// PMFW OD_RANGE fan-speed maximum (percent), companion to
    /// `fan_speed_min_pct` (typically 100%). `None` for non-PMFW GPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_speed_max_pct: Option<u8>,
    /// PMFW `fan_minimum_pwm` setting (percent), best-effort parse of the
    /// `gpu_od/fan_ctrl/fan_minimum_pwm` sysfs attribute. `None` when the
    /// attribute is absent or unparseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_minimum_pwm: Option<u8>,
    /// Whether the `amdgpu` driver is bound to this GPU's PCI device. Always
    /// `true` here in practice (this struct is only built from an hwmon node,
    /// which requires a bound driver), but emitted for symmetry with
    /// `HardwareDiagnosticsResponse.amd_pci_devices` and forward-proofing.
    #[serde(default)]
    pub amdgpu_driver_bound: bool,
    /// Kernel-regression advisories for this GPU — the same catalog surfaced
    /// in `/capabilities.amd_gpu.kernel_warnings`, duplicated here so the
    /// diagnostics support bundle is self-contained. Empty (and omitted) when
    /// nothing applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_warnings: Vec<crate::hwmon::kernel_warnings::KernelWarning>,
}

/// Intel discrete GPU diagnostics (DEC-121).
///
/// Read-only by nature — there is no fan write path, ppfeaturemask, overdrive,
/// or PMFW curve, so this carries only identity, the backing driver, and a
/// fixed truthful explanation of why fan control is unavailable.
#[derive(Debug, Clone, Serialize)]
pub struct IntelGpuDiagnostics {
    /// PCI Bus:Device.Function address (canonical).
    pub pci_bdf: String,
    /// Alias for `pci_bdf`, emitted for symmetry with `GpuDiagnostics`.
    pub pci_id: String,
    pub pci_device_id: u16,
    pub pci_revision: u8,
    pub model_name: Option<String>,
    /// Kernel driver backing the GPU: "xe" or "i915".
    pub driver: String,
    /// Always "read_only" (fan present) or "none" — Intel never writable.
    pub fan_control_method: String,
    /// Whether `fan1_input` RPM reading is available.
    pub fan_rpm_available: bool,
    /// Truthful, user-facing explanation of why fan control is unavailable,
    /// suitable for direct display in the GUI Diagnostics page.
    pub fan_control_note: String,
}

/// An AMD VGA-class PCI device and the driver bound to it (DEC-119).
///
/// Detected by scanning PCI space directly, independent of hwmon. Lets the
/// GUI distinguish "no AMD GPU installed" from "AMD GPU present but the
/// amdgpu driver is not bound" (blacklist, failed KMS, or vfio-pci
/// passthrough) — the latter produces no hwmon node, so the `gpu` field is
/// `None` and the GPU would otherwise be invisible.
#[derive(Debug, Clone, Serialize)]
pub struct AmdPciDeviceInfo {
    pub pci_bdf: String,
    pub pci_device_id: u16,
    /// Basename of the bound kernel driver, or `None` when unbound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Whether `amdgpu` is the bound driver.
    pub amdgpu_bound: bool,
    /// Whether this PCI device also surfaced an `amdgpu` hwmon node (i.e. it
    /// appears in the `gpu` field / fan-control path). `false` here while
    /// `amdgpu_bound` is `true` can indicate a very early-boot race or a
    /// render-only node without sensors.
    pub hwmon_present: bool,
}

/// Thermal safety rule status.
#[derive(Debug, Clone, Serialize)]
pub struct ThermalSafetyInfo {
    pub state: String,
    pub cpu_sensor_found: bool,
    pub emergency_threshold_c: f64,
    pub release_threshold_c: f64,
}

/// Kernel module load status.
#[derive(Debug, Clone, Serialize)]
pub struct KernelModuleInfo {
    pub name: String,
    pub loaded: bool,
    pub in_mainline: bool,
}

/// Detected ACPI I/O port conflict.
#[derive(Debug, Clone, Serialize)]
pub struct AcpiConflictInfo {
    pub io_range: String,
    pub claimed_by: String,
    pub conflicts_with_driver: String,
}

/// Detected pair of simultaneously loaded driver modules that are known to
/// race for the same chip and can corrupt the chip's non-volatile fan
/// control state.
///
/// The flagship case (DEC-105) is `(nct6687, nct6775)` — older out-of-tree
/// `nct6687` builds declare chip ID 0xd450, which is also the legitimate
/// chip ID of the upstream-supported NCT6797D. (The 0xd450 claim was removed
/// upstream in Fred78290/nct6687d PR #164, 2026 — see DEC-114 — but
/// already-loaded modules and not-yet-updated packages remain at risk.)
/// When both modules load,
/// whichever binds first claims the chip and the other may scribble into
/// the wrong registers. The original Bazzite report (ublue-os/bazzite
/// #4498) documents a CPU fan header being bricked by this exact load
/// ordering on MSI MAG X570 TOMAHAWK WIFI. The same chip family (NCT6797D)
/// appears on AM4 400-series MSI boards (e.g. B450M MORTAR, X470 GAMING
/// PRO CARBON) so the trap is not 500-series-only.
///
/// Severity is reported as a string (`"critical" | "high" | "medium"`)
/// so the GUI can render the appropriate banner without translating
/// numeric levels.
#[derive(Debug, Clone, Serialize)]
pub struct ModuleCollisionInfo {
    pub module_a: String,
    pub module_b: String,
    pub severity: String,
    pub summary: String,
    pub remediation: String,
}

/// Motherboard identification from DMI/SMBIOS.
#[derive(Debug, Clone, Serialize)]
pub struct BoardInfo {
    pub vendor: String,
    pub name: String,
    pub bios_version: String,
}

// ── PWM verification ──────────────────────────────────────────────

/// Request body for `POST /hwmon/{header_id}/verify`.
#[derive(Debug, Deserialize)]
pub struct HwmonVerifyRequest {
    pub lease_id: String,
}

/// Response for `POST /hwmon/{header_id}/verify`.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonVerifyResponse {
    pub header_id: String,
    /// "effective", "pwm_enable_reverted", "pwm_value_clamped",
    /// "no_rpm_effect", or "rpm_unavailable"
    pub result: String,
    pub initial_state: HwmonVerifyState,
    pub final_state: HwmonVerifyState,
    pub test_pwm_percent: u8,
    pub wait_seconds: u8,
    pub details: String,
    /// True if the post-verify restore-to-original-PWM write failed. Older
    /// clients that don't know the field default to ``false`` on the GUI side.
    /// When true, the header is left at the verify test value rather than the
    /// caller's prior PWM — the caller may want to write the desired PWM
    /// explicitly instead of trusting the verify endpoint to have done so.
    #[serde(default, skip_serializing_if = "is_false")]
    pub restore_failed: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Snapshot of sysfs state during a PWM verify operation.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonVerifyState {
    pub pwm_enable: Option<u8>,
    pub pwm_raw: Option<u8>,
    pub pwm_percent: Option<u8>,
    pub rpm: Option<u16>,
}

// ── GPU fan verification ──────────────────────────────────────────

/// Response for `POST /gpu/{gpu_id}/fan/verify`. No `api_version` field —
/// symmetric with [`HwmonVerifyResponse`], its sibling verify endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct GpuVerifyResponse {
    pub gpu_id: String,
    /// One of: "effective", "curve_not_applied", "no_rpm_effect",
    /// "zero_rpm_suppressed", "rpm_unavailable", "write_failed",
    /// "pwm_enable_reverted" (legacy pwm1 path only).
    pub result: String,
    pub initial_state: GpuVerifyState,
    pub final_state: GpuVerifyState,
    /// The (OD_RANGE-clamped) speed the verify drove the fan to.
    pub test_speed_pct: u8,
    pub wait_seconds: u8,
    /// "pmfw_curve" or "hwmon_pwm" — which write path was exercised.
    pub fan_control_method: String,
    pub details: String,
    /// True if restoring the pre-verify fan state failed (the fan may be left
    /// at the test speed). Older clients default to `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub restore_failed: bool,
}

/// Snapshot of GPU fan state during a verify operation. Fields are optional and
/// path-dependent: `zero_rpm_enabled` is populated on the PMFW path, `pwm_enable`
/// on the legacy `pwm1` path. `applied_speed_pct` is the read-back commanded
/// speed (flat curve value for PMFW, `pwm1` percent for legacy).
#[derive(Debug, Clone, Serialize)]
pub struct GpuVerifyState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_speed_pct: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pwm_enable: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zero_rpm_enabled: Option<bool>,
}

/// Standard error envelope for all error responses.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

/// Error body within the envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub retryable: bool,
    pub source: String,
}

impl ErrorEnvelope {
    pub fn not_found(path: &str) -> Self {
        Self {
            error: ErrorBody {
                code: "not_found".into(),
                message: format!("endpoint not found: {path}"),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "validation_error".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    pub fn hardware_unavailable(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "hardware_unavailable".into(),
                message: message.into(),
                details: None,
                retryable: true,
                source: "hardware".into(),
            },
        }
    }

    /// The endpoint exists and the addressed device exists, but this device
    /// lacks the capability the caller is trying to exercise (e.g. a GPU with
    /// no PMFW `fan_curve` and no legacy `pwm1` write path). Distinct from
    /// `hardware_unavailable` (transient / retryable hardware failure) and
    /// `validation_error` (malformed request shape). Returned with HTTP 400
    /// and `retryable: false` — the condition is permanent for this device.
    pub fn feature_unavailable(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "feature_unavailable".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    pub fn lease_error(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "lease_required".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    pub fn lease_already_held(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "lease_already_held".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "internal_error".into(),
                message: message.into(),
                details: None,
                retryable: true,
                source: "internal".into(),
            },
        }
    }

    /// A runtime config write failed. Returned with HTTP 503 by
    /// `POST /config/*` handlers so the caller knows the change did not
    /// persist and can retry. See ADR-002.
    pub fn persistence_failed(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "persistence_failed".into(),
                message: message.into(),
                details: None,
                retryable: true,
                source: "internal".into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_envelope_serializes() {
        let env = ErrorEnvelope::not_found("/nonexistent");
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["error"]["code"], "not_found");
        assert_eq!(json["error"]["retryable"], false);
        assert_eq!(json["error"]["source"], "validation");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/nonexistent"));
        // details should be absent (skip_serializing_if)
        assert!(json["error"].get("details").is_none());
    }

    #[test]
    fn internal_error_is_retryable() {
        let env = ErrorEnvelope::internal("something broke");
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["error"]["code"], "internal_error");
        assert_eq!(json["error"]["retryable"], true);
    }

    #[test]
    fn feature_unavailable_is_not_retryable() {
        let env = ErrorEnvelope::feature_unavailable("GPU has no fan write path");
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["error"]["code"], "feature_unavailable");
        assert_eq!(json["error"]["retryable"], false);
        assert_eq!(json["error"]["source"], "validation");
    }

    #[test]
    fn status_response_schema() {
        let resp = StatusResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            overall_status: "ok".into(),
            subsystems: vec![SubsystemStatus {
                name: "openfan".into(),
                status: "ok".into(),
                age_ms: Some(500),
                reason: "readings fresh".into(),
            }],
            counters: Counters {
                last_error_summary: None,
            },
            uptime_seconds: Some(3600),
            gui_last_seen_seconds_ago: None,
            thermal_state: "normal".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["overall_status"], "ok");
        assert_eq!(json["subsystems"][0]["name"], "openfan");
        assert_eq!(json["subsystems"][0]["age_ms"], 500);
        // last_error_summary absent when None
        assert!(json["counters"].get("last_error_summary").is_none());
        // DEC-132: thermal_state is always serialized (additive field).
        assert_eq!(json["thermal_state"], "normal");
    }

    #[test]
    fn sensor_entry_schema() {
        let entry = SensorEntry {
            id: "hwmon:k10temp:0000:00:18.3:Tctl".into(),
            kind: "cpu_temp".into(),
            label: "Tctl".into(),
            value_c: 55.0,
            source: "hwmon".into(),
            age_ms: 123,
            rate_c_per_s: Some(0.5),
            session_min_c: Some(32.0),
            session_max_c: Some(78.5),
            chip_name: Some("k10temp".into()),
            temp_type: None,
            thresholds: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["id"], "hwmon:k10temp:0000:00:18.3:Tctl");
        assert_eq!(json["kind"], "cpu_temp");
        assert_eq!(json["value_c"], 55.0);
        assert_eq!(json["chip_name"], "k10temp");
        // temp_type absent when None
        assert!(json.get("temp_type").is_none());
        // DEC-117: thresholds omitted entirely when None
        assert!(json.get("thresholds").is_none());
    }

    #[test]
    fn sensor_entry_with_temp_type() {
        let entry = SensorEntry {
            id: "hwmon:nct6683:nodev:AMD TSI Addr 98h".into(),
            kind: "cpu_temp".into(),
            label: "AMD TSI Addr 98h".into(),
            value_c: 48.0,
            source: "hwmon".into(),
            age_ms: 50,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: Some("nct6683".into()),
            temp_type: Some(5),
            thresholds: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["chip_name"], "nct6683");
        assert_eq!(json["temp_type"], 5);
    }

    #[test]
    fn sensor_entry_serializes_thresholds_when_present() {
        // DEC-117: when the daemon reads at least one threshold attribute,
        // the JSON includes a `thresholds` object holding ONLY the
        // attributes that were actually readable (others omitted).
        let entry = SensorEntry {
            id: "hwmon:amdgpu:0000:03:00.0:edge".into(),
            kind: "gpu_temp".into(),
            label: "edge".into(),
            value_c: 42.0,
            source: "hwmon".into(),
            age_ms: 12,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: Some("amdgpu".into()),
            temp_type: None,
            thresholds: Some(SensorThresholdsResponse {
                max_c: None,
                min_c: None,
                crit_c: Some(110.0),
                crit_hyst_c: Some(100.0),
                emergency_c: Some(115.0),
                emergency_hyst_c: None,
                lcrit_c: None,
                offset_c: None,
                alarm: None,
                max_alarm: None,
                crit_alarm: Some(false),
                fault: None,
            }),
        };
        let json = serde_json::to_value(&entry).unwrap();
        let thresholds = &json["thresholds"];
        assert_eq!(thresholds["crit_c"], 110.0);
        assert_eq!(thresholds["crit_hyst_c"], 100.0);
        assert_eq!(thresholds["emergency_c"], 115.0);
        assert_eq!(thresholds["crit_alarm"], false);
        // Unset fields are omitted, not null.
        assert!(thresholds.get("max_c").is_none());
        assert!(thresholds.get("min_c").is_none());
        assert!(thresholds.get("alarm").is_none());
        assert!(thresholds.get("fault").is_none());
    }

    #[test]
    fn capabilities_response_schema() {
        let resp = CapabilitiesResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            ipc_transport: "uds/http",
            devices: DeviceCapabilities {
                openfan: OpenfanCapability {
                    present: true,
                    channels: 10,
                    rpm_support: true,
                    write_support: true,
                },
                hwmon: HwmonCapability {
                    present: true,
                    pwm_header_count: 3,
                    lease_required: true,
                    write_support: true,
                },
                amd_gpu: AmdGpuCapability {
                    present: true,
                    model_name: Some("RX 9070 XT".into()),
                    display_label: "9070XT".into(),
                    pci_id: Some("0000:2d:00.0".into()),
                    pci_bdf: Some("0000:2d:00.0".into()),
                    pci_device_id: Some(0x7550),
                    pci_revision: Some(0xC0),
                    fan_control_method: "pmfw_curve".into(),
                    pmfw_supported: true,
                    fan_rpm_available: true,
                    fan_write_supported: true,
                    is_discrete: true,
                    overdrive_enabled: true,
                    gpu_zero_rpm_available: true,
                    kernel_warnings: Vec::new(),
                },
                intel_gpu: IntelGpuCapability {
                    present: false,
                    model_name: None,
                    display_label: "Intel D-GPU".into(),
                    pci_id: None,
                    pci_bdf: None,
                    pci_device_id: None,
                    driver: None,
                    fan_control_method: "none".into(),
                    fan_rpm_available: false,
                    is_discrete: false,
                },
                aio_hwmon: UnsupportedCapability {
                    present: false,
                    status: "unsupported",
                },
                aio_usb: UnsupportedCapability {
                    present: false,
                    status: "unsupported",
                },
            },
            features: FeatureFlags {
                openfan_write_supported: true,
                hwmon_write_supported: true,
                lease_required_for_hwmon_writes: true,
            },
            limits: Limits {
                pwm_percent_min: 0,
                pwm_percent_max: 100,
                openfan_stop_timeout_s: 8,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["ipc_transport"], "uds/http");
        assert_eq!(json["devices"]["openfan"]["present"], true);
        assert_eq!(json["devices"]["openfan"]["channels"], 10);
        assert_eq!(json["devices"]["hwmon"]["pwm_header_count"], 3);
        assert_eq!(json["features"]["lease_required_for_hwmon_writes"], true);
        // M11: both pci_id (legacy) and pci_bdf (canonical) must be emitted
        // with the same BDF string so clients on either name keep working.
        assert_eq!(json["devices"]["amd_gpu"]["pci_id"], "0000:2d:00.0");
        assert_eq!(json["devices"]["amd_gpu"]["pci_bdf"], "0000:2d:00.0");
    }

    #[test]
    fn gpu_capability_absent_gpu_omits_both_pci_fields() {
        // M11: when no GPU is present, both pci_id and pci_bdf should be
        // absent from the JSON (skip_serializing_if = is_none).
        let cap = AmdGpuCapability {
            present: false,
            model_name: None,
            display_label: "AMD D-GPU".into(),
            pci_id: None,
            pci_bdf: None,
            pci_device_id: None,
            pci_revision: None,
            fan_control_method: "none".into(),
            pmfw_supported: false,
            fan_rpm_available: false,
            fan_write_supported: false,
            is_discrete: false,
            overdrive_enabled: false,
            gpu_zero_rpm_available: false,
            kernel_warnings: Vec::new(),
        };
        let json = serde_json::to_value(&cap).unwrap();
        assert!(json.get("pci_id").is_none());
        assert!(json.get("pci_bdf").is_none());
        // DEC-098: kernel_warnings is omitted when empty so older clients
        // without the field don't see an unexpected null/array.
        assert!(json.get("kernel_warnings").is_none());
    }

    #[test]
    fn gpu_diagnostics_emits_both_pci_names() {
        // M11: GpuDiagnostics emits both pci_bdf (canonical) and pci_id
        // (alias) with identical BDF strings during the transition.
        let diag = GpuDiagnostics {
            pci_bdf: "0000:03:00.0".into(),
            pci_id: "0000:03:00.0".into(),
            pci_device_id: 0x7550,
            pci_revision: 0xC0,
            model_name: Some("RX 9070 XT".into()),
            fan_control_method: "pmfw_curve".into(),
            overdrive_enabled: true,
            ppfeaturemask: Some("0x4000".into()),
            ppfeaturemask_bit14_set: true,
            zero_rpm_available: true,
            fan_speed_min_pct: Some(15),
            fan_speed_max_pct: Some(100),
            fan_minimum_pwm: None,
            amdgpu_driver_bound: true,
            kernel_warnings: Vec::new(),
        };
        let json = serde_json::to_value(&diag).unwrap();
        assert_eq!(json["pci_bdf"], "0000:03:00.0");
        assert_eq!(json["pci_id"], "0000:03:00.0");
        // DEC-119: OD_RANGE bounds are surfaced; the unparsed fan_minimum_pwm
        // and empty advisory list are omitted from the wire so older GUIs
        // don't see unexpected nulls.
        assert_eq!(json["fan_speed_min_pct"], 15);
        assert_eq!(json["amdgpu_driver_bound"], true);
        assert!(json.get("fan_minimum_pwm").is_none());
        assert!(json.get("kernel_warnings").is_none());
    }

    #[test]
    fn lease_status_response_schema() {
        let resp = LeaseStatusResponse {
            api_version: API_VERSION,
            lease_required: true,
            held: true,
            lease_id: Some("lease-1".into()),
            ttl_seconds_remaining: Some(55),
            owner_hint: Some("gui".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["held"], true);
        assert_eq!(json["lease_id"], "lease-1");
        assert_eq!(json["ttl_seconds_remaining"], 55);

        // No lease case: optional fields absent
        let resp2 = LeaseStatusResponse {
            api_version: API_VERSION,
            lease_required: true,
            held: false,
            lease_id: None,
            ttl_seconds_remaining: None,
            owner_hint: None,
        };
        let json2 = serde_json::to_value(&resp2).unwrap();
        assert_eq!(json2["held"], false);
        assert!(json2.get("lease_id").is_none());
    }

    #[test]
    fn fan_entry_optional_fields() {
        let entry = FanEntry {
            id: "openfan:ch00".into(),
            source: "openfan".into(),
            rpm: Some(1200),
            last_commanded_pwm: None,
            age_ms: 50,
            stall_detected: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["rpm"], 1200);
        // last_commanded_pwm absent when None
        assert!(json.get("last_commanded_pwm").is_none());
    }
}
