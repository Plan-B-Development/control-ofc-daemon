//! Sensor data types for the hwmon subsystem.

use std::time::SystemTime;

/// Curated subset of hwmon temperature-threshold sysfs attributes (DEC-117).
///
/// Every field is optional because driver coverage varies: k10temp exposes
/// effectively none, coretemp exposes `max`/`crit`, amdgpu exposes
/// `crit`/`emergency`, and the nct6775/nct6683 families expose
/// `max`/`crit`/`alarm`. Missing files mean the kernel driver does not
/// expose that attribute for this sensor.
///
/// Values are stored as Celsius (the sysfs interface reports millidegrees
/// — discovery divides by 1000 before populating this struct).
///
/// Alarm flags (`alarm`, `max_alarm`, `crit_alarm`, `fault`) are sampled
/// at discovery time only — real-time tracking would require per-poll
/// sysfs reads. The daemon's hardcoded 105 °C thermal-safety override
/// (`safety.rs`) is independent of these and remains authoritative for
/// emergency fan response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SensorThresholds {
    /// `tempN_max` — typical upper warning point.
    pub max_c: Option<f64>,
    /// `tempN_min` — typical lower warning point.
    pub min_c: Option<f64>,
    /// `tempN_crit` — critical threshold above which damage may occur.
    pub crit_c: Option<f64>,
    /// `tempN_crit_hyst` — hysteresis below `crit_c` for clearing the alarm.
    pub crit_hyst_c: Option<f64>,
    /// `tempN_emergency` — emergency threshold; chip-driven shutdown above.
    pub emergency_c: Option<f64>,
    /// `tempN_emergency_hyst` — hysteresis below `emergency_c`.
    pub emergency_hyst_c: Option<f64>,
    /// `tempN_lcrit` — lower critical threshold (cold-side).
    pub lcrit_c: Option<f64>,
    /// `tempN_offset` — userspace-applied offset/calibration.
    pub offset_c: Option<f64>,
    /// `tempN_alarm` — generic alarm bit (1 = asserted).
    pub alarm: Option<bool>,
    /// `tempN_max_alarm` — alarm bit specific to the `max` threshold.
    pub max_alarm: Option<bool>,
    /// `tempN_crit_alarm` — alarm bit specific to the `crit` threshold.
    pub crit_alarm: Option<bool>,
    /// `tempN_fault` — chip-reported sensor fault (1 = sensor unreliable).
    pub fault: Option<bool>,
}

impl SensorThresholds {
    /// True when no attribute was readable — used by the API layer to skip
    /// emitting an empty `thresholds` object on the wire.
    pub fn is_empty(&self) -> bool {
        self.max_c.is_none()
            && self.min_c.is_none()
            && self.crit_c.is_none()
            && self.crit_hyst_c.is_none()
            && self.emergency_c.is_none()
            && self.emergency_hyst_c.is_none()
            && self.lcrit_c.is_none()
            && self.offset_c.is_none()
            && self.alarm.is_none()
            && self.max_alarm.is_none()
            && self.crit_alarm.is_none()
            && self.fault.is_none()
    }
}

/// Kind of temperature sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SensorKind {
    CpuTemp,
    MbTemp,
    DiskTemp,
    GpuTemp,
}

impl std::fmt::Display for SensorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CpuTemp => write!(f, "cpu_temp"),
            Self::MbTemp => write!(f, "mb_temp"),
            Self::DiskTemp => write!(f, "disk_temp"),
            Self::GpuTemp => write!(f, "gpu_temp"),
        }
    }
}

/// Source of the sensor data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SensorSource {
    Hwmon,
    AmdGpu,
    /// Intel discrete GPU (Arc), via the `xe` or `i915` hwmon node. Both
    /// kernel drivers register their hwmon device only for discrete GPUs
    /// (`if (!IS_DGFX) return;`), so this source is always a discrete card.
    /// Read-only: temperature + fan RPM only, no PWM/write path (DEC-121).
    IntelGpu,
}

impl std::fmt::Display for SensorSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hwmon => write!(f, "hwmon"),
            Self::AmdGpu => write!(f, "amd_gpu"),
            Self::IntelGpu => write!(f, "intel_gpu"),
        }
    }
}

/// A single temperature sensor reading.
#[derive(Debug, Clone)]
pub struct SensorReading {
    /// Stable identifier (e.g. `hwmon:k10temp:Tctl`).
    pub id: String,
    /// Classification of this sensor.
    pub kind: SensorKind,
    /// Human-friendly label.
    pub label: String,
    /// Temperature in degrees Celsius.
    pub value_c: f64,
    /// When this reading was taken.
    pub timestamp: SystemTime,
    /// Where the reading came from.
    pub source: SensorSource,
    /// Hwmon chip name (e.g. "k10temp", "nct6683", "it8696").
    pub chip_name: String,
    /// Sysfs `tempN_type` value if present (3=diode, 4=thermistor, 5=AMD TSI, 6=Intel PECI).
    pub temp_type: Option<u8>,
    /// Curated hwmon threshold sysfs attributes (DEC-117). `None` when the
    /// driver exposes nothing of interest; populated once at discovery time
    /// and cloned through every poll cycle.
    pub thresholds: Option<SensorThresholds>,
}

/// Metadata about a discovered temperature sensor (before reading a value).
#[derive(Debug, Clone)]
pub struct SensorDescriptor {
    /// Stable identifier.
    pub id: String,
    /// Classification.
    pub kind: SensorKind,
    /// Human-friendly label.
    pub label: String,
    /// Source subsystem.
    pub source: SensorSource,
    /// Path to the sysfs `temp*_input` file for reading.
    pub input_path: String,
    /// Hwmon chip name (e.g. "k10temp", "nct6683", "it8696").
    pub chip_name: String,
    /// Sysfs `tempN_type` value if present (3=diode, 4=thermistor, 5=AMD TSI, 6=Intel PECI).
    pub temp_type: Option<u8>,
    /// Curated hwmon threshold sysfs attributes captured at discovery
    /// time (DEC-117). `None` when nothing was readable for this sensor.
    pub thresholds: Option<SensorThresholds>,
}
