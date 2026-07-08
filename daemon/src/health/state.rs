//! Canonical state model for the daemon cache.
//!
//! All IPC responses and safety logic draw from these types.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::hwmon::types::{SensorKind, SensorThresholds};

/// Device/source label identifying where a reading originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceLabel {
    /// OpenFanController USB serial device.
    OpenFan,
    /// Kernel hwmon sysfs device (motherboard sensors/fans).
    Hwmon,
    /// AMD discrete GPU via amdgpu hwmon/PMFW.
    AmdGpu,
    /// Intel discrete GPU (Arc) via the `xe`/`i915` hwmon node. Read-only
    /// telemetry — no fan write path exists in the kernel (DEC-121).
    IntelGpu,
    /// NVIDIA discrete GPU via the `nouveau` hwmon node. Read-only telemetry;
    /// the writable nouveau `pwm1` is excluded from hwmon discovery (DEC-204).
    NvidiaGpu,
    /// AIO cooler exposed via hwmon (future).
    AioHwmon,
    /// AIO cooler exposed via USB/HID (future).
    AioUsb,
}

impl std::fmt::Display for DeviceLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenFan => write!(f, "openfan"),
            Self::Hwmon => write!(f, "hwmon"),
            Self::AmdGpu => write!(f, "amd_gpu"),
            Self::IntelGpu => write!(f, "intel_gpu"),
            Self::NvidiaGpu => write!(f, "nvidia_gpu"),
            Self::AioHwmon => write!(f, "aio_hwmon"),
            Self::AioUsb => write!(f, "aio_usb"),
        }
    }
}

/// Cached state for a single OpenFanController fan channel.
#[derive(Debug, Clone)]
pub struct OpenFanState {
    /// Channel index (0–9).
    pub channel: u8,
    /// Last known RPM reading from the device.
    pub rpm: u16,
    /// Last PWM value commanded by the daemon (firmware doesn't report this).
    pub last_commanded_pwm: Option<u8>,
    /// When this reading was taken.
    pub updated_at: Instant,
    /// True after the first real RPM poll. Prevents false stall alerts
    /// when a PWM write creates the entry before any RPM data arrives.
    pub rpm_polled: bool,
}

/// Cached state for a motherboard hwmon fan header.
#[derive(Debug, Clone)]
pub struct HwmonFanState {
    /// Stable fan ID (e.g. `it8696:fan1`).
    pub id: String,
    /// RPM reading if available.
    pub rpm: Option<u16>,
    /// Last PWM value commanded by the daemon (if controlled).
    pub last_commanded_pwm: Option<u8>,
    /// When this reading was taken.
    pub updated_at: Instant,
}

/// Cached state for a temperature sensor.
#[derive(Debug, Clone)]
pub struct CachedSensorReading {
    /// Stable sensor ID.
    pub id: String,
    /// Sensor classification.
    pub kind: SensorKind,
    /// Human-friendly label.
    pub label: String,
    /// Temperature in degrees Celsius.
    pub value_c: f64,
    /// Source device label.
    pub source: DeviceLabel,
    /// When this reading was taken.
    pub updated_at: Instant,
    /// Temperature rate of change (°C/s), smoothed.
    pub rate_c_per_s: Option<f64>,
    /// Session minimum temperature since daemon start.
    pub session_min_c: Option<f64>,
    /// Session maximum temperature since daemon start.
    pub session_max_c: Option<f64>,
    /// Hwmon chip name (e.g. "k10temp", "nct6683", "it8696").
    pub chip_name: String,
    /// Sysfs `tempN_type` value if present (3=diode, 4=thermistor, 5=AMD TSI, 6=Intel PECI).
    pub temp_type: Option<u8>,
    /// Curated hwmon threshold attribute snapshot from discovery (DEC-117).
    pub thresholds: Option<SensorThresholds>,
}

/// Cached state for a discrete GPU fan (one per GPU — hardware exposes a
/// single aggregate fan).
///
/// Shared by AMD, Intel, and NVIDIA discrete GPUs, distinguished by the ID
/// prefix: `amd_gpu:` / `intel_gpu:` / `nvidia_gpu:<PCI_BDF>`. For read-only
/// sources (Intel DEC-121, NVIDIA DEC-204) `last_commanded_pct` is always
/// `None` — fan control is firmware-managed with no userspace write path.
#[derive(Debug, Clone)]
pub struct AmdGpuFanState {
    /// Stable fan ID: `<vendor>_gpu:<PCI_BDF>`.
    pub id: String,
    /// Current fan RPM if available (hwmon `fan1_input`, or NVML per driver R565+).
    pub rpm: Option<u16>,
    /// Last speed percentage commanded by the daemon via PMFW flat curve.
    /// Always `None` for read-only sources (Intel, NVIDIA).
    pub last_commanded_pct: Option<u8>,
    /// Firmware-**reported** current fan duty %, when the source exposes it
    /// (NVML `nvmlDeviceGetFanSpeed_v2`, DEC-204). A *measured* value distinct
    /// from `last_commanded_pct` — never conflate the two. `None` for AMD/Intel
    /// (which report RPM, not a duty readback). Per NVML this may exceed 100 (it
    /// is a % of the product's max-noise-tolerance fan speed).
    pub duty_pct: Option<u8>,
    /// When this reading was taken.
    pub updated_at: Instant,
}

/// A sensor that was discovered but currently fails every read (DEC-193).
///
/// Distinct from a *stale* reading (a sensor that read recently but not this
/// instant) and from a *vanished* descriptor (a device unbound from sysfs):
/// this is a descriptor that is still present but whose `temp*_input` read
/// fails persistently — e.g. an `ath12k` WiFi-radio temperature while the radio
/// is soft-blocked (`ENETDOWN`). The polling loop quarantines such sensors so
/// they stop spamming the journal, evicts their stale cached reading, and
/// surfaces them here for display-only (Diagnostics) visibility.
#[derive(Debug, Clone)]
pub struct UnavailableSensor {
    /// Stable sensor ID (matches what it would have on `/sensors` when readable).
    pub id: String,
    /// Human-friendly label from discovery.
    pub label: String,
    /// Human-readable cause — the hwmon read error (e.g.
    /// "read error: /sys/.../temp1_input: Network is down (os error 100)").
    pub reason: String,
    /// When the sensor was quarantined as unreadable.
    pub since: Instant,
}

/// Placeholder for AIO pump state (future implementation).
#[derive(Debug, Clone, Default)]
pub struct AioPumpState {
    /// Whether an AIO device is detected.
    pub detected: bool,
    /// Pump RPM if available.
    pub pump_rpm: Option<u16>,
    /// Coolant temperature in °C if available.
    pub coolant_temp_c: Option<f64>,
    /// Pump duty percentage if available.
    pub pump_duty_pct: Option<f64>,
    /// Last commanded pump percentage.
    pub last_commanded_pct: Option<f64>,
    /// When this was last updated.
    pub updated_at: Option<Instant>,
}

/// Per-subsystem update timestamp tracking.
#[derive(Debug, Clone, Default)]
pub struct SubsystemTimestamps {
    /// Last time OpenFanController data was updated.
    pub openfan: Option<Instant>,
    /// Last time hwmon sensor data was updated.
    pub hwmon: Option<Instant>,
    /// Last time AIO data was updated.
    pub aio: Option<Instant>,
}

/// The complete daemon state snapshot.
#[derive(Debug, Clone)]
pub struct DaemonState {
    /// When this snapshot was created.
    pub snapshot_at: Instant,
    /// OpenFanController fan states, keyed by channel index.
    pub openfan_fans: HashMap<u8, OpenFanState>,
    /// Motherboard hwmon fan states, keyed by stable fan ID.
    pub hwmon_fans: HashMap<String, HwmonFanState>,
    /// AMD GPU fan states, keyed by `amd_gpu:<PCI_BDF>`.
    pub gpu_fans: HashMap<String, AmdGpuFanState>,
    /// Temperature sensor readings, keyed by stable sensor ID.
    pub sensors: HashMap<String, CachedSensorReading>,
    /// AIO pump state (placeholder).
    pub aio: AioPumpState,
    /// Per-subsystem last-update timestamps.
    pub subsystem_timestamps: SubsystemTimestamps,
    /// Thermal safety override state: "normal", "emergency", or "recovery".
    pub thermal_override_state: Option<String>,
    /// True while a hardware verify (hwmon or GPU) is in progress. Held for the
    /// verify's entire lifetime by the handler's RAII guard, so the engine pause
    /// outlasts a slow verify rather than expiring on a fixed timer. Single-
    /// flight: a second verify is rejected (409) while this is set (DEC-165).
    pub verify_in_progress: bool,
    /// Generous deadman backing `verify_in_progress`: the RAII guard always
    /// clears the flag on drop/panic/cancel, but if it somehow does not, the
    /// pause self-clears after this instant so a verify can never strand control.
    pub verify_active_until: Option<Instant>,
    /// GPU fan ids the daemon has relinquished to firmware-auto via
    /// `POST /gpu/{id}/fan/reset` (DEC-165). The engine skips writing these so a
    /// reset is durable under an active profile; cleared on profile activation.
    pub relinquished_gpu_fans: HashSet<String>,
    /// Sensors discovered but currently unreadable (DEC-193). Maintained by the
    /// hwmon poll loop's `SensorFailureTracker`; surfaced on `/status` + `/poll`
    /// for display. Sensors listed here are evicted from `sensors` so a stale
    /// value is never served.
    pub unavailable_sensors: Vec<UnavailableSensor>,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            snapshot_at: Instant::now(),
            openfan_fans: HashMap::new(),
            hwmon_fans: HashMap::new(),
            gpu_fans: HashMap::new(),
            sensors: HashMap::new(),
            aio: AioPumpState::default(),
            subsystem_timestamps: SubsystemTimestamps::default(),
            thermal_override_state: None,
            verify_in_progress: false,
            verify_active_until: None,
            relinquished_gpu_fans: HashSet::new(),
            unavailable_sensors: Vec::new(),
        }
    }
}
