//! Canonical state model for the daemon cache.
//!
//! All IPC responses and safety logic draw from these types.

use std::collections::HashMap;
use std::time::Instant;

use crate::hwmon::types::SensorKind;

/// Device/source label identifying where a reading originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceLabel {
    /// OpenFanController USB serial device.
    OpenFan,
    /// Kernel hwmon sysfs device (motherboard sensors/fans).
    Hwmon,
    /// AMD discrete GPU via amdgpu hwmon/PMFW.
    AmdGpu,
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
}

/// Cached state for an AMD GPU fan (one per GPU — hardware exposes a single aggregate).
#[derive(Debug, Clone)]
pub struct AmdGpuFanState {
    /// Stable fan ID: `amd_gpu:<PCI_BDF>` (e.g. `amd_gpu:0000:2d:00.0`).
    pub id: String,
    /// Current fan RPM if available (from fan1_input).
    pub rpm: Option<u16>,
    /// Last speed percentage commanded by the daemon via PMFW flat curve.
    pub last_commanded_pct: Option<u8>,
    /// When this reading was taken.
    pub updated_at: Instant,
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
    /// Last time a GUI-initiated write command was processed.
    pub last_gui_write_at: Option<Instant>,
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
            last_gui_write_at: None,
        }
    }
}
