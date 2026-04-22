//! Structured error types for the Control-OFC daemon.

use thiserror::Error;

/// Configuration loading and validation errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("parse error: {message}")]
    Parse { message: String },

    #[error("validation error: {field}: {message}")]
    Validation { field: String, message: String },
}

/// Serial protocol and transport errors.
#[derive(Debug, Error)]
pub enum SerialError {
    #[error("port unavailable: {path}")]
    PortUnavailable { path: String },

    #[error("timeout after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },

    #[error("protocol error: {message}")]
    Protocol { message: String },
}

/// Hwmon sysfs errors.
///
/// Display output intentionally includes sysfs paths (e.g. `/sys/class/hwmon/...`).
/// These are public kernel-exported paths, not secrets. The daemon communicates
/// over a local-only Unix socket, so information disclosure risk is negligible,
/// and the paths provide significant diagnostic value for troubleshooting.
#[derive(Debug, Error)]
pub enum HwmonError {
    #[error("sensor not found: {id}")]
    SensorNotFound { id: String },

    #[error("read error: {path}: {message}")]
    ReadError { path: String, message: String },

    #[error("write error: {path}: {message}")]
    WriteError { path: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_display() {
        let err = ConfigError::Validation {
            field: "poll_interval_ms".into(),
            message: "must be >= 100".into(),
        };
        assert_eq!(
            err.to_string(),
            "validation error: poll_interval_ms: must be >= 100"
        );
    }

    #[test]
    fn serial_timeout_display() {
        let err = SerialError::Timeout { timeout_ms: 500 };
        assert_eq!(err.to_string(), "timeout after 500ms");
    }
}
