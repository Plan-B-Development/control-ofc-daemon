//! Shared utilities for hwmon sysfs access.

use std::path::Path;

use crate::error::HwmonError;

/// Extract a short stable device identifier from the sysfs device path.
///
/// Walks the path components looking for recognisable patterns:
/// - PCI address: `DDDD:BB:DD.F` (12+ chars with colons/dot)
/// - Platform device: `it87*`, `isa*`, `nct*`
/// - Fallback: basename of the path, or `"unknown"`
pub fn device_id_from_path(device_path: &Path) -> String {
    for component in device_path.iter().rev() {
        if let Some(s) = component.to_str() {
            // PCI address pattern: DDDD:BB:DD.F
            if s.len() >= 12
                && s.chars().nth(4) == Some(':')
                && s.chars().nth(7) == Some(':')
                && s.chars().nth(10) == Some('.')
            {
                return s.to_string();
            }
            // Platform device pattern: name.NNNN
            if s.starts_with("it87") || s.starts_with("isa") || s.starts_with("nct") {
                return s.to_string();
            }
        }
    }

    device_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Replace NaN/Infinity with 0.0 to prevent silent null in JSON serialization.
pub fn sanitize_f64(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Read a sysfs attribute file, returning the raw content.
pub fn read_sysfs_string(path: &Path) -> Result<String, HwmonError> {
    std::fs::read_to_string(path).map_err(|e| HwmonError::ReadError {
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_f64_finite_passthrough() {
        assert_eq!(sanitize_f64(42.5), 42.5);
        assert_eq!(sanitize_f64(-10.0), -10.0);
        assert_eq!(sanitize_f64(0.0), 0.0);
    }

    #[test]
    fn sanitize_f64_nan_becomes_zero() {
        assert_eq!(sanitize_f64(f64::NAN), 0.0);
    }

    #[test]
    fn sanitize_f64_infinity_becomes_zero() {
        assert_eq!(sanitize_f64(f64::INFINITY), 0.0);
        assert_eq!(sanitize_f64(f64::NEG_INFINITY), 0.0);
    }
}
