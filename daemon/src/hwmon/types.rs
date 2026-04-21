//! Sensor data types for the hwmon subsystem.

use std::time::SystemTime;

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
}

impl std::fmt::Display for SensorSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hwmon => write!(f, "hwmon"),
            Self::AmdGpu => write!(f, "amd_gpu"),
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
}
