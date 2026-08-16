//! Adopting an OpenFanController serial device — the one path that decides
//! which port becomes the fan controller.
//!
//! [SAFETY] Lives in the library rather than in `main.rs` so that boot adoption
//! and `POST /fans/openfan/rescan` (DEC-265) share it. Two copies of this logic
//! would be two chances to skip the DEC-250 identity handshake, and a device
//! that opens but is not an OpenFanController accepts every write with `Ok` —
//! `/status` would show OpenFan healthy while the 105 C emergency drove nothing.

use std::time::Duration;

/// Decide which serial port paths to try, in order, for one connect attempt.
///
/// [SAFETY] The configured port is tried FIRST but is never the only candidate:
/// auto-detection is always appended as a fallback. This used to be
/// `configured.or_else(auto_detect)`, so a configured port suppressed detection
/// outright. Since `serial.port` became settable over the 0666 socket
/// (DEC-243), any local user could persist a well-formed but dead path and, from
/// the next restart, leave `fan_controller` as `None` — which does not merely
/// disable fan control, because the profile engine's thermal-emergency
/// `force_all` is guarded by `if let Some(be) = openfan_be`. The 105 C rule
/// would lose its only path to every OpenFan-attached fan, with no failsafe.
///
/// Pure so the rule is unit-testable without a serial device: `detect` is
/// injected, and a detected path equal to the configured one is not retried.
pub fn serial_port_candidates(
    configured: Option<&str>,
    detect: impl FnOnce() -> Option<String>,
) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(c) = configured {
        candidates.push(c.to_string());
    }
    if let Some(detected) = detect() {
        if !candidates.contains(&detected) {
            candidates.push(detected);
        }
    }
    candidates
}

/// Connect to the first candidate that opens **and** identifies as an
/// OpenFanController.
///
/// [SAFETY] The other half of `serial_port_candidates` (DEC-250). That function
/// guarantees auto-detection still *runs* when a port is configured; this one
/// guarantees its result is still *reachable*. Acceptance used to be "the port
/// opened", and `RealSerialTransport::open` succeeds on any readable tty — so a
/// configured-but-wrong `/dev/ttyACM*` was adopted as the fan controller and the
/// loop stopped there, discarding the correctly detected port that was sitting
/// next in the candidate list. Because writes to an indifferent device return
/// `Ok`, nothing surfaced: no failure was logged, `/status` showed OpenFan
/// healthy, and the 105°C emergency's `force_all` reported success while driving
/// nothing. `serial.port` is settable by any local user over the 0666 socket
/// (DEC-243) and persists in `runtime.toml`, so this was durable across reboots.
///
/// A candidate that opens but fails the handshake is skipped, not fatal: the
/// next candidate — in practice the auto-detected one — is tried.
///
/// Pure over the injected `open` so the accept/reject rule is unit-testable
/// without a serial device, matching `serial_port_candidates`. The verification
/// deliberately lives *inside* this function rather than in the closure: it is
/// the property under test, and a caller cannot accidentally skip it.
pub fn first_openfan_port<T: crate::serial::transport::SerialTransport>(
    candidates: &[String],
    timeout: Duration,
    mut open: impl FnMut(&str) -> Result<T, crate::error::SerialError>,
) -> Option<(String, T)> {
    for port in candidates {
        match open(port) {
            Ok(mut transport) => {
                match crate::serial::transport::verify_openfan_identity(&mut transport, timeout) {
                    Ok(()) => return Some((port.clone(), transport)),
                    Err(e) => log::warn!(
                        "{port} opened but did not identify as an OpenFanController ({e}) \
                         — not using it"
                    ),
                }
            }
            Err(e) => log::warn!("Failed to open OpenFanController on {port}: {e}"),
        }
    }
    None
}
