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
///
/// `pub` so `POST /config/serial-port` validates against this exact list rather
/// than its own looser `/dev/` test — two copies of a security check drift, and
/// the API's copy accepted paths (`/dev/shm/...`) that `open()` then rejected.
pub fn is_allowed_serial_path(path: &str) -> bool {
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

/// List candidate serial ports without opening any `ttyACM`/`ttyUSB` (DEC-291).
///
/// The other half of [`auto_detect_port`], split out because the difference
/// matters: this enumerates, that one *identifies*, and identifying means
/// `open(2)`, which asserts DTR and resets Arduino-class boards.
///
/// The rescan endpoint's cooldown exists to ration exactly that reset. It could
/// not, because the candidate list it compares was built by `auto_detect_port` —
/// so every refused rescan had already reset the board before the refusal was
/// decided, and the handler's own comment asserted the opposite. Enumeration is a
/// libudev/sysfs read plus `Path::exists`.
///
/// **Precisely what "opens nothing" means here**, because the over-broad version
/// of this claim is what caused the defect: `available_ports()` does open the
/// devnode of any tty whose parent driver is `serial8250`, before this function's
/// `ttyACM`/`ttyUSB` filter ever runs. No candidate this function *returns* is
/// opened, which is what the cooldown needs; but do not restate this as "touches
/// no hardware". The shipped unit blocks those opens anyway
/// (`DeviceAllow=char-ttyACM/ttyUSB`), so they are reachable only in a dev or
/// container run.
pub fn enumerate_serial_candidates() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match serialport::available_ports() {
        Ok(ports) => out.extend(
            ports
                .into_iter()
                .filter(|p| p.port_name.contains("ttyACM") || p.port_name.contains("ttyUSB"))
                .map(|p| p.port_name),
        ),
        Err(e) => {
            log::warn!("serialport::available_ports() failed: {e} — falling back to path scan");
        }
    }
    // Same fallback shape as `auto_detect_port`, and for the same reason: it works
    // without libudev. `exists()` stats; it does not open.
    for prefix in &["/dev/ttyACM", "/dev/ttyUSB"] {
        for i in constants::SERIAL_PROBE_RANGE {
            let path = format!("{prefix}{i}");
            if Path::new(&path).exists() && !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Auto-detect the OpenFanController serial port **by opening and identifying**
/// each candidate.
///
/// First tries `serialport::available_ports()` (libudev). If that fails
/// (e.g. in a sandboxed systemd unit), falls back to probing
/// `/dev/ttyACM0` through `/dev/ttyACM9` directly.
///
/// Opening asserts DTR, which resets Arduino-class boards — so this is NOT the
/// function to call merely to learn what ports exist. Use
/// [`enumerate_serial_candidates`] for that (DEC-291). Boot adoption no longer
/// calls this at all for that reason (OFN-b); the remaining caller is the
/// OpenFan poll loop's reconnect probe, which runs only after a controller that
/// was already adopted has dropped off.
///
/// **Each candidate is opened at most once per call.** It did not used to be:
/// the libudev pass returns early only on *success*, so a machine where nothing
/// identified fell through to the direct path scan below and opened every one of
/// the same nodes a second time — two DTR resets per unrelated device per call,
/// where the whole point of the split with [`enumerate_serial_candidates`] is to
/// ration exactly that. The `probed` set below is the same guard
/// `enumerate_serial_candidates` already applies to its own two passes.
pub fn auto_detect_port(timeout: Duration) -> Option<String> {
    // libudev first; an enumeration failure is not fatal, it just leaves the
    // path scan in `probe_order` as the only source (see its doc comment).
    let enumerated: Vec<String> = match serialport::available_ports() {
        Ok(ports) => {
            let found: Vec<String> = ports
                .into_iter()
                .map(|p| p.port_name)
                .filter(|n| n.contains("ttyACM") || n.contains("ttyUSB"))
                .collect();
            // Only on the Ok path. "found 0 candidate(s)" after an enumeration
            // that FAILED is a different fact from one that succeeded and matched
            // nothing, and the failure branch already says so above — this is the
            // one path where an operator needs the two kept apart.
            log::info!("serialport enumeration found {} candidate(s)", found.len());
            found
        }
        Err(e) => {
            log::warn!("serialport::available_ports() failed: {e} — falling back to direct probe");
            Vec::new()
        }
    };

    for path in probe_order(&enumerated, |p| Path::new(p).exists()) {
        if let Some(found) = probe_port(&path, timeout) {
            return Some(found);
        }
    }

    None
}

/// The de-duplicated order in which [`auto_detect_port`] opens candidates.
///
/// Split out and made pure so "each candidate is opened AT MOST ONCE" is a
/// testable property rather than a claim (`OFN-b`). It did not hold: the two
/// passes below used to be two loops, the first of which returned early only on
/// *success*, so on a machine where nothing identified every node was opened
/// twice per call — and opening a tty asserts DTR, which resets Arduino-class
/// boards. That is the precise cost `enumerate_serial_candidates` exists to
/// ration, and this function is where the rationing has to be true.
///
/// The path scan still runs when enumeration SUCCEEDED but matched nothing:
/// `available_ports()` can return `Ok(vec![])` where the device nodes exist,
/// which is the case the fallback was written for. Re-probing nodes already
/// covered was the bug, not the second pass itself.
///
/// `node_exists` is injected for the same reason `open` is injected into
/// `first_openfan_port`: it is the only part that touches the filesystem.
pub fn probe_order(enumerated: &[String], node_exists: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in enumerated {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    // Covers both CDC-ACM and FTDI/CH340 adapters, and works without libudev.
    for prefix in &["/dev/ttyACM", "/dev/ttyUSB"] {
        for i in constants::SERIAL_PROBE_RANGE {
            let path = format!("{prefix}{i}");
            if !out.contains(&path) && node_exists(&path) {
                out.push(path);
            }
        }
    }
    out
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
            // Shared with the configured-port path in `main` (DEC-250) so both
            // agree on what counts as an OpenFanController.
            match crate::serial::transport::verify_openfan_identity(&mut transport, timeout) {
                Ok(()) => {
                    log::info!("OpenFanController detected on {path}");
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

    // ── `OFN-b`: probe order and de-duplication ───────────────────────────────

    #[test]
    fn a_node_reported_by_libudev_is_not_scanned_again() {
        // The DISCRIMINATING arm: before the fix, `auto_detect_port` returned
        // from its libudev pass only on SUCCESS, so a machine where nothing
        // identified fell through to the path scan and opened every one of the
        // same nodes a second time. Each of those opens asserts DTR and resets an
        // Arduino-class board.
        let enumerated = vec!["/dev/ttyACM0".to_string(), "/dev/ttyUSB0".to_string()];
        let order = probe_order(&enumerated, |p| {
            // Both enumerated nodes also exist on disk — exactly the arrangement
            // that produced the double probe.
            p == "/dev/ttyACM0" || p == "/dev/ttyUSB0"
        });
        assert_eq!(
            order,
            vec!["/dev/ttyACM0", "/dev/ttyUSB0"],
            "a node already enumerated must not be re-probed by the path scan"
        );
        for p in &enumerated {
            assert_eq!(
                order.iter().filter(|o| *o == p).count(),
                1,
                "{p} appears twice in the probe order"
            );
        }
    }

    #[test]
    fn the_path_scan_still_covers_nodes_libudev_did_not_report() {
        // The opposite arm, and the reason the second pass was not simply
        // deleted: `available_ports()` returns `Ok(vec![])` where the device
        // nodes exist (no libudev in a sandboxed unit), which is the case the
        // fallback was written for. Without this, a "fix" that dropped the scan
        // whenever enumeration succeeded would pass the test above.
        let order = probe_order(&[], |p| p == "/dev/ttyACM3");
        assert_eq!(order, vec!["/dev/ttyACM3"]);
    }

    #[test]
    fn a_node_that_does_not_exist_is_never_probed() {
        assert!(probe_order(&[], |_| false).is_empty());
    }

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
