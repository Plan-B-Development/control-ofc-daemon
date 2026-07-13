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
    /// Seconds since daemon process started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    /// Thermal safety override state: `"normal"` | `"recovery"` | `"emergency"`
    /// | `"no_sensor_fallback"` (forced 40% when no CPU sensor is reachable).
    /// Mirrors the value the profile engine reports each tick (the same string
    /// `/diagnostics/hardware` exposes) so the GUI can stand its control loop
    /// down while the daemon is forcing safety PWM (DEC-132). Additive field —
    /// API_VERSION unchanged.
    pub thermal_state: String,
    /// Active manual overrides (DEC-163), each with remaining TTL. Omitted when
    /// none are active so the common-case `/status` wire shape is unchanged
    /// (additive). Poll-authoritative surface for the GUI's override UI.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<OverrideStatusEntry>,
    /// Active fan-identify holds (DEC-166), each with remaining deadman TTL.
    /// Omitted when none are active.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fan_identify: Vec<IdentifyStatusEntry>,
    /// Sensors discovered but currently unreadable (DEC-193) — e.g. an `ath12k`
    /// WiFi temperature while the radio is soft-blocked. Display-only: they are
    /// evicted from the `sensors` array so a stale value is never served. Omitted
    /// when none, so the common-case wire shape is unchanged (additive).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable_sensors: Vec<UnavailableSensorEntry>,
    /// Active profile id + display name, mirrored onto the `/status` + `/poll`
    /// surface so an external activation (CLI `--profile`, another client,
    /// systemd) is reflected within one 1 Hz poll instead of the GUI's slow
    /// `/profile/active` refresh (DEC-194). Both omitted when no profile is
    /// active, so the common-case wire shape is unchanged (additive) — a client
    /// treats an absent key (old daemon, or genuinely no profile) as "unknown"
    /// and falls back to `/profile/active`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile_name: Option<String>,
    /// Compact hardware-readiness rollup (DEC-206) for the GUI Dashboard health
    /// chip: overall severity + per-severity counts + the most-severe item's
    /// summary/code (for a deep-link). Cached in `AppState` and mirrored here so
    /// the 1 Hz poll stays cheap — the full item list stays on
    /// `/inventory/readiness`. Omitted by daemons predating DEC-206 (and until the
    /// startup refresh runs), so a client treats an absent key as "no rollup" and
    /// hides the chip (additive — API_VERSION unchanged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<crate::hwmon::readiness::ReadinessRollup>,
}

/// One present-but-unreadable sensor on the `/status` + `/poll` surface
/// (DEC-193).
#[derive(Debug, Clone, Serialize)]
pub struct UnavailableSensorEntry {
    pub id: String,
    pub label: String,
    /// Human-readable cause — the daemon's hwmon read error.
    pub reason: String,
    /// Milliseconds since the sensor was quarantined as unreadable.
    pub unavailable_for_ms: u64,
}

/// One active manual override on the `/status` poll surface (DEC-163).
#[derive(Debug, Clone, Serialize)]
pub struct OverrideStatusEntry {
    pub control_id: String,
    pub pwm_percent: u8,
    pub expires_in_secs: u64,
}

/// One active fan-identify hold on the `/status` poll surface (DEC-166).
#[derive(Debug, Clone, Serialize)]
pub struct IdentifyStatusEntry {
    pub fan_id: String,
    pub expires_in_secs: u64,
}

/// Per-subsystem health status.
#[derive(Debug, Clone, Serialize)]
pub struct SubsystemStatus {
    pub name: String,
    pub status: String,
    pub age_ms: Option<u64>,
    pub reason: String,
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
    /// Hwmon chip name (e.g. "k10temp", "nct6683", "it8696"). Always present —
    /// discovery requires a readable `name` attribute or the device is skipped
    /// ("amdgpu" for AMD GPU sources, "xe"/"i915" for Intel GPU). (audit P2-D)
    pub chip_name: String,
    /// Sysfs `tempN_type` value if present (3=diode, 4=thermistor, 5=AMD TSI, 6=Intel PECI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_type: Option<u8>,
    /// Curated hwmon temperature-threshold sysfs attributes (DEC-117). Omitted
    /// from the JSON when the driver exposes none of the attributes for this
    /// sensor. Alarm flags are sampled at discovery time only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<SensorThresholdsResponse>,
    /// False when this temperature must not be offered as a fan-curve source
    /// (DEC-193) — currently set only for wireless-radio PHY temps (e.g.
    /// `ath12k` WiFi), which read `ENETDOWN` whenever the radio is down and
    /// would strand a curve. Display is unaffected. Always present here; clients
    /// talking to a pre-2.3.0 daemon that omits it must default to `true`.
    pub control_eligible: bool,
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
    /// Firmware-reported current fan duty % (measured, not commanded) — present
    /// only for sources that expose a duty readback (NVIDIA via NVML, DEC-204).
    /// Distinct from `last_commanded_pwm`. May exceed 100 (NVML expresses it as
    /// a % of max noise tolerance). Additive/optional (API v1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duty_pct: Option<u8>,
    pub age_ms: u64,
    /// True when RPM is 0 but last_commanded_pwm is above the safety floor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_detected: Option<bool>,
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
    /// Whether this header belongs to a liquid cooler (NZXT Kraken /
    /// Aquacomputer) — daemon-authoritative AIO hint for the GUI (AIO Phase 1).
    /// Pre-1.18.0 daemons omit this; consumers default it to `false`.
    pub is_aio: bool,
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
            is_aio: h.is_aio,
        }
    }
}

/// Response for `GET /hwmon/headers`.
#[derive(Debug, Clone, Serialize)]
pub struct PwmHeadersResponse {
    pub api_version: u32,
    pub headers: Vec<PwmHeaderEntry>,
}

/// A monitor-only fan tachometer in the hwmon inventory (Phase 1) — an
/// `fanN_input` with no matching `pwmN` control, i.e. RPM-readable but not
/// controllable. Controllable fans appear under `pwm_controls` instead (with
/// `rpm_available = true`). Structural only: the live RPM value is served via
/// `/fans` + `/poll`, not here.
#[derive(Debug, Clone, Serialize)]
pub struct FanInputEntry {
    pub id: String,
    /// Always `"hwmon"` — monitor-only fans are an hwmon-subsystem concept.
    pub source: String,
    pub chip_name: String,
    pub label: String,
    /// The N in `fanN_input`.
    pub fan_index: u8,
}

impl From<&crate::hwmon::inventory::FanInputDescriptor> for FanInputEntry {
    fn from(f: &crate::hwmon::inventory::FanInputDescriptor) -> Self {
        FanInputEntry {
            id: f.id.clone(),
            source: "hwmon".into(),
            chip_name: f.chip_name.clone(),
            label: f.label.clone(),
            fan_index: f.fan_index,
        }
    }
}

/// An inventory temperature sensor (Phase 2): the standard `SensorEntry` fields
/// plus a fine-grained classification refinement. The three extra fields are
/// flattened alongside the `SensorEntry` fields, so each `temp_sensors` object
/// carries `id`/`kind`/`value_c`/… *and* `classification`/`confidence`/
/// `rationale`. The refinement is advisory — `kind` and the daemon's thermal
/// safety are unchanged.
#[derive(Debug, Clone, Serialize)]
pub struct InventoryTempSensor {
    #[serde(flatten)]
    pub sensor: SensorEntry,
    /// Fine class: `cpu_package` | `cpu_core` | `cpu_tctl` | `cpu_tdie` |
    /// `motherboard_temp` | `vrm_temp` | `chipset_temp` | `gpu_temp` |
    /// `disk_temp` | `coolant_temp` | `unknown_temp`. A refinement of `kind`.
    pub classification: String,
    /// Classifier confidence: `high` | `medium` | `low` | `unknown`.
    pub confidence: String,
    /// Plain-English reason for the classification (daemon-owned wording).
    pub rationale: String,
}

/// The daemon's default-CPU-sensor recommendation. Advisory only: thermal safety
/// still uses the hottest CpuTemp, and this never silently replaces a user's
/// stored choice. Omitted from the response when no CPU temperature sensor was
/// found. `source` is `"user"` when it echoes the persisted preferred CPU sensor
/// (Phase 5), else `"auto"` for the deterministic auto-pick (Phase 2).
#[derive(Debug, Clone, Serialize)]
pub struct DefaultCpuEntry {
    pub sensor_id: String,
    pub confidence: String,
    pub rationale: String,
    pub source: String,
}

/// The user's persisted hardware selections echoed on the inventory (Phase 5).
/// Each id is the raw stored preference and may be stale — check the sensor list
/// or the readiness `selected_*_sensor_missing` items. The whole object is
/// omitted when nothing is persisted (additive).
#[derive(Debug, Clone, Serialize)]
pub struct InventoryPreferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_sensor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mb_sensor_id: Option<String>,
}

/// Response for `GET /inventory/hwmon` — a structured, read-only inventory of
/// hwmon-visible hardware for the GUI. Composed from existing discovery plus the
/// live cache; the daemon never writes hardware to build it.
///
/// - `temp_sensors`: the live sensor set (the standard `SensorEntry` fields plus
///   the Phase-2 `classification`/`confidence`/`rationale` refinement).
/// - `pwm_controls`: discovered controllable PWM headers (same as
///   `/hwmon/headers`).
/// - `monitor_only_fans`: RPM tachometers with no controllable `pwmN`
///   (previously invisible to the API). Omitted when empty (additive).
/// - `default_cpu`: the deterministic default-CPU recommendation, omitted when
///   no CPU sensor is present (additive).
#[derive(Debug, Clone, Serialize)]
pub struct HwmonInventoryResponse {
    pub api_version: u32,
    pub temp_sensors: Vec<InventoryTempSensor>,
    pub pwm_controls: Vec<PwmHeaderEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub monitor_only_fans: Vec<FanInputEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_cpu: Option<DefaultCpuEntry>,
    /// The user's persisted preferred CPU/motherboard sensors (Phase 5), omitted
    /// when none are set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferences: Option<InventoryPreferences>,
}

/// Response for `GET /inventory/readiness` — the structured hardware-readiness
/// list (Phase 3). Read-only diagnose-and-guide; the daemon never mutates the
/// system to produce it. `overall` is the most severe item's severity.
#[derive(Debug, Clone, Serialize)]
pub struct ReadinessResponse {
    pub api_version: u32,
    pub overall: crate::hwmon::readiness::ReadinessSeverity,
    pub items: Vec<crate::hwmon::readiness::ReadinessItem>,
}

/// Response for `GET /inventory/superio` (DEC-202) — the passive Super-I/O
/// detection report. Read-only; the daemon never probes I/O ports, loads
/// modules, or writes hardware to produce it. `arch_supported` is false on
/// non-x86 (with an empty `chips` list). Absent route ⇒ daemon predates the
/// feature (clients gate on 404, mirroring the other `/inventory/*` endpoints).
#[derive(Debug, Clone, Serialize)]
pub struct SuperIoResponse {
    pub api_version: u32,
    pub arch_supported: bool,
    pub chips: Vec<SuperIoChipEntry>,
    /// Driver names whose ISA I/O range collides with an ACPI OperationRegion.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub acpi_conflict_drivers: Vec<String>,
    /// Report-level notes (always carries the "present ≠ controllable" caveat).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// DEC-203: whether the opt-in active `/dev/port` probe
    /// (`POST /inventory/superio/probe`) can run right now. Off by default; the
    /// GUI gates its "probe" affordance on this.
    pub port_probe_available: bool,
    /// Plain-English reason for `port_probe_available` (`"available"`, or why not
    /// — flag off / no CAP_SYS_RAWIO / kernel lockdown / no `/dev/port`).
    pub port_probe_reason: String,
}

/// One detected Super-I/O chip in a [`SuperIoResponse`].
#[derive(Debug, Clone, Serialize)]
pub struct SuperIoChipEntry {
    pub chip_name: String,
    pub vendor: String,
    /// Evidence sources: `dmi_board_table` | `kernel_log` | `bound_hwmon`.
    pub evidence: Vec<String>,
    /// Presence confidence: `high` | `medium` | `low` | `unknown`.
    pub confidence: String,
    /// The module inferred to have bound this chip (present only when bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_driver: Option<String>,
    pub expected_module: String,
    pub module_loaded: bool,
    pub hwmon_present: bool,
    /// A load recommendation, present only for an unbound, allowlisted chip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<SuperIoRecommendationEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

/// A "load this driver" recommendation for an unbound chip.
#[derive(Debug, Clone, Serialize)]
pub struct SuperIoRecommendationEntry {
    pub module: String,
    pub in_mainline: bool,
    pub load_hint: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risk_notes: Vec<String>,
}

/// Response for `GET /inventory/hardware-readiness` (DEC-207) — the combined
/// readiness + Super-I/O snapshot the merged "Cooling Hardware Readiness" GUI page
/// fetches in ONE atomic request, so both halves come from a single shared passive
/// scan (no cross-endpoint drift, no redundant detection). Read-only; the daemon
/// never mutates the system to produce it. Absent route ⇒ daemon predates the
/// feature (clients gate on 404, mirroring the other `/inventory/*` endpoints).
/// The compact `rollup` mirrors the one on `/status` + `/poll`; `items` matches
/// `GET /inventory/readiness`; `superio` matches `GET /inventory/superio`.
#[derive(Debug, Clone, Serialize)]
pub struct HardwareReadinessResponse {
    pub api_version: u32,
    /// Compact rollup (same shape mirrored onto `/status` + `/poll`).
    pub rollup: crate::hwmon::readiness::ReadinessRollup,
    /// The overall severity (== `rollup.overall`), for convenience.
    pub overall: crate::hwmon::readiness::ReadinessSeverity,
    /// The full readiness list (== `GET /inventory/readiness`).
    pub items: Vec<crate::hwmon::readiness::ReadinessItem>,
    /// The passive Super-I/O report (== `GET /inventory/superio`).
    pub superio: SuperIoResponse,
    /// Milliseconds since the underlying passive scan completed (matches the API's
    /// `age_ms` freshness convention; the GUI renders a "last scanned" time).
    pub scanned_age_ms: u64,
    /// Monotonic scan id; changes exactly when a new scan is served, so the GUI
    /// can detect a fresh assessment without diffing.
    pub generation: u64,
}

/// One entry in the `GET /profiles` listing — a lightweight summary parsed from
/// each stored/preset profile (the full document is fetched via
/// `GET /profiles/{id}`).
#[derive(Debug, Clone, Serialize)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Response for `GET /profiles`.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileListResponse {
    pub api_version: u32,
    pub profiles: Vec<ProfileSummary>,
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
    /// Control-execution capability (GUI→daemon control migration). Additive
    /// top-level field (DEC-159/160) — pre-1.20 daemons omit it, and the GUI
    /// must treat its absence as "all false" (old behaviour). The GUI keys its
    /// daemon-owned-control startup gate on this block.
    pub control: ControlCapability,
}

/// Control-execution capability flags. The daemon advertises which control
/// responsibilities it can own. Each flag defaults to the pre-migration
/// behaviour when read by a client that doesn't understand it (AIP-180).
#[derive(Debug, Clone, Serialize)]
pub struct ControlCapability {
    /// Daemon can store, validate, list, and delete GUI-authored profiles via
    /// the `/profiles` CRUD API. True since 1.19.0 (DEC-160).
    pub profile_storage: bool,
    /// Daemon can evaluate fan curves headlessly (always true — the engine has
    /// done this since the profile engine landed; see DEC-096).
    pub curve_evaluation: bool,
    /// Daemon exposes a manual-override API. False until the override API lands
    /// (DEC-163).
    pub manual_override: bool,
    /// Daemon exposes a fan-identify API. False until that API lands (DEC-166).
    pub fan_identify: bool,
    /// True only when the daemon engine is the sole authoritative fan writer —
    /// the 2.0.0 cutover (DEC-165) deleted the `gui_active` defer machinery, so
    /// the engine writes every tick a profile is active. A loop-less GUI may run
    /// against the daemon ONLY when this is true; a pre-2.0 daemon omits the
    /// field, so a client defaults it to `false` (AIP-180) and refuses to take
    /// control rather than silently leaving fans uncontrolled. The
    /// safety-critical version gate.
    pub autonomous_control: bool,
    /// Minimum GUI version this daemon supports for daemon-owned control, or
    /// empty when no floor is enforced yet (set at the 2.0.0 cutover).
    pub min_supported_gui: String,
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
    /// NVIDIA discrete GPU capability. Additive field (DEC-204) — older GUIs
    /// ignore it; read-only monitoring only, never fan-writable.
    pub nvidia_gpu: NvidiaGpuCapability,
    /// Liquid-cooler (AIO) hwmon capability — dynamic since 1.18.0 (DEC-156).
    pub aio_hwmon: AioHwmonCapability,
    /// USB-only coolers (liquidctl/USB-HID) are out of scope — always
    /// unsupported.
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

/// NVIDIA discrete GPU capability details (DEC-204).
///
/// Read-only, like the Intel Arc capability (DEC-121) — NVIDIA fan control is
/// never offered (nouveau's writable `pwm1` is excluded from discovery; the
/// NVML backend is telemetry-only), so there is no `fan_write_supported` field.
/// `model_name`/`driver_version` are populated only via the proprietary NVML
/// driver; the open `nouveau` leg yields a generic label.
#[derive(Debug, Clone, Serialize)]
pub struct NvidiaGpuCapability {
    pub present: bool,
    /// Product name (e.g. "NVIDIA GeForce RTX 4080") — NVML only, else null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Compact display label (model name, or "NVIDIA D-GPU").
    pub display_label: String,
    /// PCI Bus:Device.Function (legacy alias, kept symmetric with amd/intel_gpu
    /// for the GUI's `_coalesce_pci_bdf` tolerance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<String>,
    /// PCI Bus:Device.Function (canonical).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_bdf: Option<String>,
    /// Backing kernel driver: "nouveau" (open) or "nvidia" (proprietary). The
    /// proprietary GPU is read via the NVML library, but the kernel module — and
    /// so this field's value — is "nvidia" (mirrors Intel's `driver` semantics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// NVIDIA driver version (e.g. "565.77") — NVML only, else null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    /// Fan control method: "read_only" (fan present) or "none". Never writable.
    pub fan_control_method: String,
    /// Whether fan RPM reading is available.
    pub fan_rpm_available: bool,
    /// Whether this is a discrete GPU. Always true for a detected NVIDIA GPU.
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
    pub write_support: bool,
}

/// AIO (all-in-one / liquid cooler) hwmon capability (AIO Phase 1, DEC-156).
///
/// A backward-compatible superset of [`UnsupportedCapability`]: `present` and
/// `status` are retained (pre-1.18.0 GUIs read only those), with
/// `pump_writable` / `coolant_available` added. Scope is hwmon-only — USB-only
/// coolers are out of scope and reported via the separate `aio_usb`, which
/// stays `unsupported` permanently.
///
/// `status` is honest about controllability: `"supported"` (a writable AIO
/// pump/fan header is present), `"monitor_only"` (a liquid cooler or coolant
/// sensor is detected but no writable AIO header exists — e.g. NZXT Kraken2, or
/// coolant sensing only; never presented as controllable), or `"unsupported"`
/// (nothing detected).
#[derive(Debug, Clone, Serialize)]
pub struct AioHwmonCapability {
    pub present: bool,
    pub status: &'static str,
    /// Whether at least one AIO header is writable (pump/fan controllable).
    pub pump_writable: bool,
    /// Whether a coolant-temperature sensor is available.
    pub coolant_available: bool,
}

impl AioHwmonCapability {
    /// Build the dynamic capability from discovered AIO headers + coolant
    /// sensing. `aio_total_headers` counts `is_aio` headers; `aio_writable`
    /// counts those that are also `is_writable`; `coolant_available` is whether
    /// any `CoolantTemp` sensor is cached.
    pub fn from_discovery(
        aio_total_headers: usize,
        aio_writable: usize,
        coolant_available: bool,
    ) -> Self {
        let present = aio_total_headers > 0 || coolant_available;
        let pump_writable = aio_writable > 0;
        let status = if !present {
            "unsupported"
        } else if pump_writable {
            "supported"
        } else {
            "monitor_only"
        };
        AioHwmonCapability {
            present,
            status,
            pump_writable,
            coolant_available,
        }
    }

    /// The "nothing detected" baseline (also the pre-discovery default).
    pub fn unsupported() -> Self {
        AioHwmonCapability {
            present: false,
            status: "unsupported",
            pump_writable: false,
            coolant_available: false,
        }
    }
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
}

/// Policy-level limits the GUI should respect.
#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    pub pwm_percent_min: u8,
    pub pwm_percent_max: u8,
    pub openfan_stop_timeout_s: u8,
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
    /// NVIDIA discrete GPU diagnostics (DEC-204). Additive — omitted when no
    /// NVIDIA GPU is present; older clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvidia_gpu: Option<NvidiaGpuDiagnostics>,
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

/// NVIDIA discrete GPU diagnostics (DEC-204).
///
/// Read-only by nature — no fan write path exists (nouveau's `pwm1` is excluded
/// from discovery; the NVML backend is telemetry-only), so this carries only
/// identity, the backing driver leg, and a truthful explanation of why fan
/// control is unavailable.
#[derive(Debug, Clone, Serialize)]
pub struct NvidiaGpuDiagnostics {
    /// PCI Bus:Device.Function address (canonical).
    pub pci_bdf: String,
    /// Alias for `pci_bdf`, emitted for symmetry with the other GPU diagnostics.
    pub pci_id: String,
    /// Product name (NVML only) or null (nouveau).
    pub model_name: Option<String>,
    /// Backing kernel driver: "nouveau" (open) or "nvidia" (proprietary). The
    /// proprietary GPU is read via the NVML library, but the kernel module — and
    /// so this field's value — is "nvidia" (mirrors Intel's `driver` semantics).
    pub driver: String,
    /// NVIDIA driver version (NVML only) or null.
    pub driver_version: Option<String>,
    /// Always "read_only" (fan present) or "none" — NVIDIA never writable.
    pub fan_control_method: String,
    /// Whether fan RPM reading is available.
    pub fan_rpm_available: bool,
    /// Truthful, user-facing explanation of why fan control is unavailable.
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

/// Body of `POST /control/{id}/override` (DEC-163). `ttl_secs` is an optional
/// per-grant deadman, clamped server-side to `[1, OVERRIDE_TTL_SECS]` so a
/// client cannot request a long single TTL that would defeat the deadman — it
/// extends an override by renewing, not by a long initial grant.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideTakeRequest {
    pub pwm_percent: u8,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// Body of `POST /control/{id}/override/renew` and `DELETE /control/{id}/override`.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideTokenRequest {
    pub override_token: u64,
}

/// Body of `POST /fans/{fan_id}/identify` (DEC-166). `action` is `"stop"` or
/// `"restore"`; `ttl_secs` (stop only) is the deadman, clamped as above.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentifyRequest {
    pub action: String,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// Response to `POST /control/{id}/override` — a fresh grant. `override_token`
/// is the monotonic identity+fence the caller presents on renew/release
/// (DEC-163). `renew_secs` is the advisory interval at which the GUI should
/// renew (~⅓ TTL).
#[derive(Debug, Clone, Serialize)]
pub struct OverrideGrantResponse {
    pub api_version: u32,
    pub control_id: String,
    pub override_token: u64,
    pub pwm_percent: u8,
    pub ttl_secs: u64,
    pub renew_secs: u64,
    pub expires_in_secs: u64,
}

/// Response to `POST /control/{id}/override/renew` — extended TTL.
#[derive(Debug, Clone, Serialize)]
pub struct OverrideRenewResponse {
    pub api_version: u32,
    pub control_id: String,
    pub override_token: u64,
    pub ttl_secs: u64,
    pub expires_in_secs: u64,
}

/// Response to `DELETE /control/{id}/override` — reverted to curve control.
/// `released` is `false` when nothing live was held (idempotent no-op).
#[derive(Debug, Clone, Serialize)]
pub struct OverrideReleaseResponse {
    pub api_version: u32,
    pub control_id: String,
    pub released: bool,
}

/// Response to `POST /fans/{fan_id}/identify` (DEC-166). `expires_in_secs` is
/// present only for `action: "stop"` (the deadman); `"restore"` omits it.
#[derive(Debug, Clone, Serialize)]
pub struct IdentifyResponse {
    pub api_version: u32,
    pub fan_id: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
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

    /// A renew/release presented a stale or superseded override token — a
    /// thawed/resumed GUI cannot re-pin fans it no longer owns (Kleppmann
    /// fencing, DEC-163). HTTP 409 Conflict, `retryable: false`.
    pub fn stale_fencing_token(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "stale_fencing_token".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    /// A renew/release targeted an override that has expired (its deadman fired)
    /// or was never taken — the caller should re-take, not renew (DEC-163).
    /// HTTP 404 Not Found, `retryable: false`.
    pub fn override_expired(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "override_expired".into(),
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

    /// A profile with the requested id already exists in the daemon store.
    /// Returned with HTTP 409 by `POST /profiles` so the caller distinguishes
    /// a duplicate create from a malformed request (DEC-160). The check is
    /// store-scoped — a read-only preset of the same id is shadowable, not a
    /// conflict.
    pub fn already_exists(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "already_exists".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    /// The profile is currently active and so cannot be deleted. Returned with
    /// HTTP 409 by `DELETE /profiles/{id}` (DEC-160). Deactivate or activate a
    /// different profile first.
    pub fn profile_in_use(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "profile_in_use".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    /// A `409 thermal_abort` — a hardware fan action (verify / calibrate) was
    /// refused because a sensor is too hot to run it safely. Retryable once the
    /// system cools. Phase 6 / DEC-201 wires this into the verify handlers;
    /// mirrors the calibrate sweep's abort (DEC-134).
    pub fn thermal_abort(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "thermal_abort".into(),
                message: message.into(),
                details: None,
                retryable: true,
                source: "hardware".into(),
            },
        }
    }

    /// A `validation_error` carrying structured per-field violations in
    /// `details` (DEC-160). The envelope shape is unchanged — `details` is the
    /// existing free-form field — so older clients that read only
    /// `code`/`message` keep working while newer clients read
    /// `details.field_violations[]`.
    pub fn validation_with_details(message: impl Into<String>, details: serde_json::Value) -> Self {
        Self {
            error: ErrorBody {
                code: "validation_error".into(),
                message: message.into(),
                details: Some(details),
                retryable: false,
                source: "validation".into(),
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
    fn override_token_request_requires_token() {
        // `override_token` has no serde default — a token-less body must fail to
        // deserialize, so a renew/release handler can never treat a missing token
        // as 0. Pins the DEC-163 fencing contract at the type level (cheaper and
        // more direct than asserting axum's plain-text extractor rejection).
        assert!(serde_json::from_str::<OverrideTokenRequest>("{}").is_err());
        let ok: OverrideTokenRequest = serde_json::from_str(r#"{"override_token": 7}"#).unwrap();
        assert_eq!(ok.override_token, 7);
    }

    #[test]
    fn hwmon_inventory_response_schema() {
        // Phase 2: temp_sensors flatten the standard SensorEntry fields with the
        // classification refinement; default_cpu + monitor_only_fans are omitted
        // when absent (additive).
        let empty = HwmonInventoryResponse {
            api_version: API_VERSION,
            temp_sensors: Vec::new(),
            pwm_controls: Vec::new(),
            monitor_only_fans: Vec::new(),
            default_cpu: None,
            preferences: None,
        };
        let json = serde_json::to_value(&empty).unwrap();
        assert_eq!(json["api_version"], 1);
        assert!(json["temp_sensors"].is_array());
        assert!(json["pwm_controls"].is_array());
        assert!(
            json.get("monitor_only_fans").is_none(),
            "monitor_only_fans must be omitted when empty (additive)"
        );
        assert!(
            json.get("default_cpu").is_none(),
            "default_cpu must be omitted when no CPU sensor (additive)"
        );

        let populated = HwmonInventoryResponse {
            api_version: API_VERSION,
            temp_sensors: vec![InventoryTempSensor {
                sensor: SensorEntry {
                    id: "hwmon:k10temp:0000:00:18.3:Tctl".into(),
                    kind: "cpu_temp".into(),
                    label: "Tctl".into(),
                    value_c: 55.0,
                    source: "hwmon".into(),
                    age_ms: 100,
                    rate_c_per_s: None,
                    session_min_c: None,
                    session_max_c: None,
                    chip_name: "k10temp".into(),
                    temp_type: None,
                    thresholds: None,
                    control_eligible: true,
                },
                classification: "cpu_tctl".into(),
                confidence: "high".into(),
                rationale: "k10temp Tctl control temperature".into(),
            }],
            pwm_controls: Vec::new(),
            monitor_only_fans: vec![FanInputEntry {
                id: "hwmon:nct6798:isa:fan3:PUMP_TACH".into(),
                source: "hwmon".into(),
                chip_name: "nct6798".into(),
                label: "PUMP_TACH".into(),
                fan_index: 3,
            }],
            default_cpu: Some(DefaultCpuEntry {
                sensor_id: "hwmon:k10temp:0000:00:18.3:Tctl".into(),
                confidence: "high".into(),
                rationale: "only CPU candidate".into(),
                source: "auto".into(),
            }),
            preferences: Some(InventoryPreferences {
                cpu_sensor_id: Some("hwmon:k10temp:0000:00:18.3:Tctl".into()),
                mb_sensor_id: None,
            }),
        };
        let json = serde_json::to_value(&populated).unwrap();
        // Flatten: the SensorEntry fields sit at the same level as the
        // classification refinement on each temp_sensors entry.
        assert_eq!(
            json["temp_sensors"][0]["id"],
            "hwmon:k10temp:0000:00:18.3:Tctl"
        );
        assert_eq!(json["temp_sensors"][0]["kind"], "cpu_temp");
        assert_eq!(json["temp_sensors"][0]["classification"], "cpu_tctl");
        assert_eq!(json["temp_sensors"][0]["confidence"], "high");
        // default_cpu present with the recommended sensor id.
        assert_eq!(
            json["default_cpu"]["sensor_id"],
            "hwmon:k10temp:0000:00:18.3:Tctl"
        );
        assert_eq!(json["default_cpu"]["confidence"], "high");
        assert_eq!(json["default_cpu"]["source"], "auto");
        // Phase 5: persisted preferences echoed (mb omitted when unset).
        assert_eq!(
            json["preferences"]["cpu_sensor_id"],
            "hwmon:k10temp:0000:00:18.3:Tctl"
        );
        assert!(json["preferences"].get("mb_sensor_id").is_none());
        // monitor_only_fans still surfaces unchanged.
        assert_eq!(json["monitor_only_fans"][0]["fan_index"], 3);
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
            uptime_seconds: Some(3600),
            thermal_state: "normal".into(),
            overrides: Vec::new(),
            fan_identify: Vec::new(),
            unavailable_sensors: Vec::new(),
            active_profile_id: None,
            active_profile_name: None,
            readiness: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["overall_status"], "ok");
        assert_eq!(json["subsystems"][0]["name"], "openfan");
        assert_eq!(json["subsystems"][0]["age_ms"], 500);
        // DEC-170: the counters envelope was removed entirely (its only field,
        // last_error_summary, was permanently dead).
        assert!(json.get("counters").is_none());
        // DEC-132: thermal_state is always serialized (additive field).
        assert_eq!(json["thermal_state"], "normal");
        // DEC-163/166: override + identify arrays omitted when empty (additive).
        assert!(json.get("overrides").is_none());
        assert!(json.get("fan_identify").is_none());
        // DEC-193: unavailable_sensors omitted when empty (additive).
        assert!(json.get("unavailable_sensors").is_none());
        // DEC-194: active_profile_* omitted when no profile is active (additive).
        assert!(json.get("active_profile_id").is_none());
        assert!(json.get("active_profile_name").is_none());
        // DEC-206: readiness rollup omitted when None (old daemon / pre-startup).
        assert!(json.get("readiness").is_none());
    }

    #[test]
    fn status_response_serializes_readiness_rollup_when_present() {
        // DEC-206: the cached rollup rides /status (and /poll) for the Dashboard
        // health chip. Present ⇒ overall + counts + top_* serialize.
        use crate::hwmon::readiness::{ReadinessRollup, ReadinessSeverity};
        let resp = StatusResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            overall_status: "ok".into(),
            subsystems: Vec::new(),
            uptime_seconds: Some(1),
            thermal_state: "normal".into(),
            overrides: Vec::new(),
            fan_identify: Vec::new(),
            unavailable_sensors: Vec::new(),
            active_profile_id: None,
            active_profile_name: None,
            readiness: Some(ReadinessRollup {
                overall: ReadinessSeverity::Warning,
                critical: 0,
                warning: 2,
                info: 1,
                top_summary: Some("No motherboard PWM fan controls detected".into()),
                top_code: Some("no_pwm_controls".into()),
            }),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["readiness"]["overall"], "warning");
        assert_eq!(json["readiness"]["warning"], 2);
        assert_eq!(json["readiness"]["info"], 1);
        assert_eq!(json["readiness"]["top_code"], "no_pwm_controls");
        assert_eq!(
            json["readiness"]["top_summary"],
            "No motherboard PWM fan controls detected"
        );
    }

    #[test]
    fn status_response_serializes_unavailable_sensors_when_present() {
        // DEC-193: a quarantined sensor surfaces on /status (and /poll via the
        // embedded status) with its cause and how long it has been unreadable.
        let resp = StatusResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            overall_status: "ok".into(),
            subsystems: Vec::new(),
            uptime_seconds: Some(1),
            thermal_state: "normal".into(),
            overrides: Vec::new(),
            fan_identify: Vec::new(),
            unavailable_sensors: vec![UnavailableSensorEntry {
                id: "hwmon:ath12k_hwmon:phy0:temp1".into(),
                label: "temp1".into(),
                reason: "read error: Network is down (os error 100)".into(),
                unavailable_for_ms: 4200,
            }],
            active_profile_id: None,
            active_profile_name: None,
            readiness: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json["unavailable_sensors"][0]["id"],
            "hwmon:ath12k_hwmon:phy0:temp1"
        );
        assert_eq!(json["unavailable_sensors"][0]["label"], "temp1");
        assert!(json["unavailable_sensors"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("Network is down"));
        assert_eq!(json["unavailable_sensors"][0]["unavailable_for_ms"], 4200);
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
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
            control_eligible: true,
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
        // DEC-193: control_eligible is always serialized (a real sensor is true).
        assert_eq!(json["control_eligible"], true);
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
            chip_name: "nct6683".into(),
            temp_type: Some(5),
            thresholds: None,
            control_eligible: true,
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
            chip_name: "amdgpu".into(),
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
            control_eligible: true,
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
    fn hardware_readiness_response_serializes_combined_snapshot() {
        // DEC-207: the combined endpoint's DTO carries the rollup + readiness items
        // + the Super-I/O report + freshness/generation, all additive. Nested
        // skip_serializing_if is honored (rollup.top_* when None; empty superio
        // vecs), and `api_version` stays 1.
        use crate::hwmon::readiness::{ReadinessRollup, ReadinessSeverity};
        let resp = HardwareReadinessResponse {
            api_version: API_VERSION,
            rollup: ReadinessRollup {
                overall: ReadinessSeverity::Ok,
                critical: 0,
                warning: 0,
                info: 0,
                top_summary: None,
                top_code: None,
            },
            overall: ReadinessSeverity::Ok,
            items: Vec::new(),
            superio: SuperIoResponse {
                api_version: API_VERSION,
                arch_supported: true,
                chips: Vec::new(),
                acpi_conflict_drivers: Vec::new(),
                notes: Vec::new(),
                port_probe_available: false,
                port_probe_reason: "flag off".into(),
            },
            scanned_age_ms: 42,
            generation: 7,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["overall"], "ok");
        assert_eq!(json["rollup"]["overall"], "ok");
        // An all-ok rollup omits top_* (skip_serializing_if).
        assert!(json["rollup"].get("top_summary").is_none());
        assert!(json["rollup"].get("top_code").is_none());
        assert!(json["items"].as_array().unwrap().is_empty());
        // The nested SuperIoResponse: empty vecs omitted; chips is an empty array.
        assert_eq!(json["superio"]["arch_supported"], true);
        assert!(json["superio"].get("acpi_conflict_drivers").is_none());
        assert!(json["superio"].get("notes").is_none());
        assert_eq!(json["superio"]["chips"].as_array().unwrap().len(), 0);
        assert_eq!(json["scanned_age_ms"], 42);
        assert_eq!(json["generation"], 7);
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
                nvidia_gpu: NvidiaGpuCapability {
                    present: true,
                    model_name: Some("NVIDIA GeForce RTX 4080".into()),
                    display_label: "NVIDIA GeForce RTX 4080".into(),
                    pci_id: Some("0000:03:00.0".into()),
                    pci_bdf: Some("0000:03:00.0".into()),
                    driver: Some("nvidia".into()),
                    driver_version: Some("565.77".into()),
                    fan_control_method: "read_only".into(),
                    fan_rpm_available: false,
                    is_discrete: true,
                },
                aio_hwmon: AioHwmonCapability::unsupported(),
                aio_usb: UnsupportedCapability {
                    present: false,
                    status: "unsupported",
                },
            },
            features: FeatureFlags {
                openfan_write_supported: true,
                hwmon_write_supported: true,
            },
            limits: Limits {
                pwm_percent_min: 0,
                pwm_percent_max: 100,
                openfan_stop_timeout_s: 8,
            },
            control: ControlCapability {
                profile_storage: true,
                curve_evaluation: true,
                manual_override: false,
                fan_identify: false,
                autonomous_control: false,
                min_supported_gui: String::new(),
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["ipc_transport"], "uds/http");
        // Control-execution capability block (DEC-159/160).
        assert_eq!(json["control"]["profile_storage"], true);
        assert_eq!(json["control"]["curve_evaluation"], true);
        assert_eq!(json["control"]["manual_override"], false);
        assert_eq!(json["devices"]["openfan"]["present"], true);
        assert_eq!(json["devices"]["openfan"]["channels"], 10);
        assert_eq!(json["devices"]["hwmon"]["pwm_header_count"], 3);
        // DEC-170: the lease capability surface is gone — neither the per-header
        // flag nor the feature flag is emitted any more.
        assert!(json["devices"]["hwmon"].get("lease_required").is_none());
        assert!(json["features"]
            .get("lease_required_for_hwmon_writes")
            .is_none());
        // M11: both pci_id (legacy) and pci_bdf (canonical) must be emitted
        // with the same BDF string so clients on either name keep working.
        assert_eq!(json["devices"]["amd_gpu"]["pci_id"], "0000:2d:00.0");
        assert_eq!(json["devices"]["amd_gpu"]["pci_bdf"], "0000:2d:00.0");
        // NVIDIA capability (DEC-204): additive, read-only (no fan_write_supported).
        assert_eq!(json["devices"]["nvidia_gpu"]["present"], true);
        assert_eq!(
            json["devices"]["nvidia_gpu"]["display_label"],
            "NVIDIA GeForce RTX 4080"
        );
        // `driver` is the kernel module name ("nvidia"), not the NVML library.
        assert_eq!(json["devices"]["nvidia_gpu"]["driver"], "nvidia");
        assert_eq!(json["devices"]["nvidia_gpu"]["driver_version"], "565.77");
        assert_eq!(
            json["devices"]["nvidia_gpu"]["fan_control_method"],
            "read_only"
        );
        assert!(json["devices"]["nvidia_gpu"]
            .get("fan_write_supported")
            .is_none());
        // AIO Phase 1 (DEC-156): aio_hwmon is the dynamic capability — the
        // back-compat present+status plus pump_writable/coolant_available.
        assert_eq!(json["devices"]["aio_hwmon"]["present"], false);
        assert_eq!(json["devices"]["aio_hwmon"]["status"], "unsupported");
        assert_eq!(json["devices"]["aio_hwmon"]["pump_writable"], false);
        assert_eq!(json["devices"]["aio_hwmon"]["coolant_available"], false);
        // USB-only coolers remain out of scope.
        assert_eq!(json["devices"]["aio_usb"]["status"], "unsupported");
    }

    #[test]
    fn aio_hwmon_capability_from_discovery_status() {
        // Nothing detected → unsupported.
        let none = AioHwmonCapability::from_discovery(0, 0, false);
        assert!(!none.present);
        assert_eq!(none.status, "unsupported");

        // Coolant sensing only, no writable AIO header → monitor_only.
        let mon = AioHwmonCapability::from_discovery(0, 0, true);
        assert!(mon.present);
        assert!(!mon.pump_writable);
        assert!(mon.coolant_available);
        assert_eq!(mon.status, "monitor_only");

        // AIO header present but none writable → monitor_only (honest).
        let ro = AioHwmonCapability::from_discovery(1, 0, true);
        assert_eq!(ro.status, "monitor_only");
        assert!(!ro.pump_writable);

        // A writable AIO pump/fan header → supported.
        let sup = AioHwmonCapability::from_discovery(2, 2, true);
        assert!(sup.present);
        assert!(sup.pump_writable);
        assert_eq!(sup.status, "supported");
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
    fn fan_entry_optional_fields() {
        let entry = FanEntry {
            id: "openfan:ch00".into(),
            source: "openfan".into(),
            rpm: Some(1200),
            last_commanded_pwm: None,
            duty_pct: None,
            age_ms: 50,
            stall_detected: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["rpm"], 1200);
        // last_commanded_pwm + duty_pct absent when None — old GUIs see no new nulls.
        assert!(json.get("last_commanded_pwm").is_none());
        assert!(json.get("duty_pct").is_none());

        // A read-only NVIDIA fan surfaces its measured duty % (DEC-204).
        let nvidia = FanEntry {
            id: "nvidia_gpu:0000:03:00.0".into(),
            source: "nvidia_gpu".into(),
            rpm: None,
            last_commanded_pwm: None,
            duty_pct: Some(47),
            age_ms: 10,
            stall_detected: None,
        };
        let json = serde_json::to_value(&nvidia).unwrap();
        assert_eq!(json["duty_pct"], 47);
        assert_eq!(json["source"], "nvidia_gpu");
        // Read-only NVIDIA fan: never a commanded PWM on the wire.
        assert!(json.get("last_commanded_pwm").is_none());
    }
}
