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
    /// during the transition to canonical naming (M11 in
    /// `docs/23_Contract_Mismatch_Backlog.md` on the GUI side). New callers
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
    pub thermal_safety: ThermalSafetyInfo,
    pub kernel_modules: Vec<KernelModuleInfo>,
    pub acpi_conflicts: Vec<AcpiConflictInfo>,
    pub board: BoardInfo,
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
}

/// Snapshot of sysfs state during a PWM verify operation.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonVerifyState {
    pub pwm_enable: Option<u8>,
    pub pwm_raw: Option<u8>,
    pub pwm_percent: Option<u8>,
    pub rpm: Option<u16>,
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
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["overall_status"], "ok");
        assert_eq!(json["subsystems"][0]["name"], "openfan");
        assert_eq!(json["subsystems"][0]["age_ms"], 500);
        // last_error_summary absent when None
        assert!(json["counters"].get("last_error_summary").is_none());
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
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["id"], "hwmon:k10temp:0000:00:18.3:Tctl");
        assert_eq!(json["kind"], "cpu_temp");
        assert_eq!(json["value_c"], 55.0);
        assert_eq!(json["chip_name"], "k10temp");
        // temp_type absent when None
        assert!(json.get("temp_type").is_none());
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
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["chip_name"], "nct6683");
        assert_eq!(json["temp_type"], 5);
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
        };
        let json = serde_json::to_value(&cap).unwrap();
        assert!(json.get("pci_id").is_none());
        assert!(json.get("pci_bdf").is_none());
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
        };
        let json = serde_json::to_value(&diag).unwrap();
        assert_eq!(json["pci_bdf"], "0000:03:00.0");
        assert_eq!(json["pci_id"], "0000:03:00.0");
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
