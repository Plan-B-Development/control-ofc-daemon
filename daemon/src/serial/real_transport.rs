//! Real serial port transport using the `serialport` crate.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use crate::constants;
use crate::error::SerialError;
use crate::serial::transport::SerialTransport;

/// Real serial transport backed by the `serialport` crate.
pub struct RealSerialTransport {
    reader: BufReader<Box<dyn serialport::SerialPort>>,
    writer: Box<dyn serialport::SerialPort>,
    /// Configured timeout — stored for accurate error reporting.
    timeout: Duration,
}

/// Allowed serial port path prefixes. Paths not matching any of these are
/// rejected to prevent accidentally opening arbitrary device nodes.
const ALLOWED_SERIAL_PREFIXES: &[&str] = &[
    "/dev/ttyS",
    "/dev/ttyUSB",
    "/dev/ttyACM",
    "/dev/ttyAMA",
    "/dev/serial/",
];

/// Check whether a serial port path starts with an allowed prefix.
///
/// Also rejects paths containing traversal components (`..`) or null bytes
/// to prevent CWE-22 path traversal even if the prefix matches.
fn is_allowed_serial_path(path: &str) -> bool {
    if path.contains("..") || path.contains('\0') {
        return false;
    }
    ALLOWED_SERIAL_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

impl RealSerialTransport {
    /// Open a serial port at the given path.
    pub fn open(path: &str, timeout: Duration) -> Result<Self, SerialError> {
        if !is_allowed_serial_path(path) {
            return Err(SerialError::Protocol {
                message: format!(
                    "serial path '{path}' does not match any allowed prefix ({ALLOWED_SERIAL_PREFIXES:?})"
                ),
            });
        }

        let port = serialport::new(path, constants::SERIAL_BAUD_RATE)
            .timeout(timeout)
            .open()
            .map_err(|e| SerialError::Protocol {
                message: format!("failed to open serial port '{path}': {e}"),
            })?;

        let reader = BufReader::new(port.try_clone().map_err(|e| SerialError::Protocol {
            message: format!("failed to clone serial port: {e}"),
        })?);

        Ok(Self {
            reader,
            writer: port,
            timeout,
        })
    }
}

impl SerialTransport for RealSerialTransport {
    fn write_line(&mut self, data: &str) -> Result<(), SerialError> {
        self.writer
            .write_all(data.as_bytes())
            .map_err(|e| SerialError::Protocol {
                message: format!("serial write failed: {e}"),
            })?;
        self.writer.flush().map_err(|e| SerialError::Protocol {
            message: format!("serial flush failed: {e}"),
        })?;
        Ok(())
    }

    fn read_line(&mut self, _timeout: Duration) -> Result<String, SerialError> {
        use std::io::Read;
        let mut line = String::new();
        let timeout_ms = self.timeout.as_millis() as u64;
        let n = self
            .reader
            .by_ref()
            .take(constants::MAX_SERIAL_LINE_BYTES)
            .read_line(&mut line)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::TimedOut => SerialError::Timeout { timeout_ms },
                _ => SerialError::Protocol {
                    message: format!("serial read failed: {e}"),
                },
            })?;

        if n == 0 {
            return Err(SerialError::Timeout { timeout_ms });
        }

        Ok(line)
    }
}

/// Auto-detect the OpenFanController serial port.
///
/// First tries `serialport::available_ports()` (libudev). If that fails
/// (e.g. in a sandboxed systemd unit), falls back to probing
/// `/dev/ttyACM0` through `/dev/ttyACM9` directly.
pub fn auto_detect_port(timeout: Duration) -> Option<String> {
    // Try libudev enumeration first
    match serialport::available_ports() {
        Ok(ports) => {
            let candidates: Vec<_> = ports
                .iter()
                .filter(|p| p.port_name.contains("ttyACM") || p.port_name.contains("ttyUSB"))
                .collect();
            log::info!(
                "serialport enumeration found {} candidate(s)",
                candidates.len()
            );

            for port_info in &candidates {
                if let Some(found) = probe_port(&port_info.port_name, timeout) {
                    return Some(found);
                }
            }
        }
        Err(e) => {
            log::warn!("serialport::available_ports() failed: {e} — falling back to direct probe");
        }
    }

    // Fallback: probe /dev/ttyACM0..9 and /dev/ttyUSB0..9 directly
    // (works even without libudev; covers both CDC-ACM and FTDI/CH340 adapters)
    for prefix in &["/dev/ttyACM", "/dev/ttyUSB"] {
        log::info!("Probing {prefix}0..9 directly");
        for i in constants::SERIAL_PROBE_RANGE {
            let path = format!("{prefix}{i}");
            if Path::new(&path).exists() {
                if let Some(found) = probe_port(&path, timeout) {
                    return Some(found);
                }
            }
        }
    }

    None
}

/// Try to open a port and send ReadAllRpm to see if it's an OpenFanController.
///
/// The path has already been validated by `is_allowed_serial_path` inside
/// `RealSerialTransport::open`, but auto_detect_port only generates
/// hard-coded `/dev/ttyACM*` and `/dev/ttyUSB*` paths anyway.
fn probe_port(path: &str, timeout: Duration) -> Option<String> {
    log::info!("Probing {path}...");

    match RealSerialTransport::open(path, timeout) {
        Ok(mut transport) => {
            let cmd = crate::serial::protocol::Command::ReadAllRpm;
            match crate::serial::transport::send_command(&mut transport, &cmd, timeout) {
                Ok(response) => {
                    log::info!("OpenFanController detected on {path}: {response:?}");
                    Some(path.to_string())
                }
                Err(e) => {
                    log::info!("Port {path} opened but did not respond as OpenFanController: {e}");
                    None
                }
            }
        }
        Err(e) => {
            log::info!("Could not open {path}: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_serial_paths() {
        assert!(is_allowed_serial_path("/dev/ttyACM0"));
        assert!(is_allowed_serial_path("/dev/ttyUSB0"));
        assert!(is_allowed_serial_path("/dev/ttyS0"));
        assert!(is_allowed_serial_path("/dev/ttyAMA0"));
        assert!(is_allowed_serial_path("/dev/serial/by-id/usb-foo"));
    }

    #[test]
    fn disallowed_serial_paths() {
        assert!(!is_allowed_serial_path("/dev/sda1"));
        assert!(!is_allowed_serial_path("/etc/passwd"));
        assert!(!is_allowed_serial_path("/dev/null"));
        assert!(!is_allowed_serial_path("ttyACM0")); // no leading /dev/
        assert!(!is_allowed_serial_path(""));
        // Path traversal attempts
        assert!(!is_allowed_serial_path("/dev/ttyACM0/../sda1"));
        assert!(!is_allowed_serial_path("/dev/ttyUSB0/../../etc/passwd"));
        assert!(!is_allowed_serial_path("/dev/ttyACM0\0"));
    }

    #[test]
    fn open_rejects_invalid_path_prefix() {
        let result = RealSerialTransport::open("/etc/passwd", Duration::from_millis(100));
        assert!(result.is_err());
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("does not match any allowed prefix"),
                    "unexpected error: {msg}"
                );
            }
            Ok(_) => panic!("expected error for disallowed path"),
        }
    }
}
