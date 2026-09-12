//! Adopting an OpenFanController serial device — the one path that decides
//! which port becomes the fan controller.
//!
//! [SAFETY] Lives in the library rather than in `main.rs` so that boot adoption
//! and `POST /fans/openfan/rescan` (DEC-265) share it. Two copies of this logic
//! would be two chances to skip the DEC-250 identity handshake, and a device
//! that opens but is not an OpenFanController accepts every write with `Ok` —
//! `/status` would show OpenFan healthy while the thermal emergency drove nothing.

use std::time::Duration;

/// Decide which serial port paths to try, in order, for one connect attempt.
///
/// **No production caller since `OFN-b` (2026-09-12).** Boot adoption — its only
/// one — moved to [`serial_port_candidates_enumerated`], because the `detect`
/// injected here is `auto_detect_port`, which OPENS each candidate to identify
/// it. This function is kept for its DEC-250 ordering tests, which are the record
/// of why a configured port must never suppress detection; that rule now lives in
/// the enumerated variant, which carries equivalent tests. Do not reintroduce a
/// caller without re-reading that variant's doc comment first.
///
/// [SAFETY] The configured port is tried FIRST but is never the only candidate:
/// auto-detection is always appended as a fallback. This used to be
/// `configured.or_else(auto_detect)`, so a configured port suppressed detection
/// outright. Since `serial.port` became settable over the 0666 socket
/// (DEC-243), any local user could persist a well-formed but dead path and, from
/// the next restart, leave `fan_controller` as `None` — which does not merely
/// disable fan control, because the profile engine's thermal-emergency
/// `force_all_with_floor` is guarded by `if let Some(be) = openfan_be`. The thermal-emergency rule
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

/// As [`serial_port_candidates`], but for an enumerator that returns EVERY
/// candidate rather than the first identified one (DEC-291).
///
/// Same ordering rule — a configured port is tried first — kept here rather than
/// inline in the handler so it stays unit-testable, exactly as `main.rs` notes
/// for its sibling. The distinction that matters is in the enumerator: this one
/// must not open anything, because its result is what the rescan cooldown
/// compares, and opening is the act the cooldown exists to ration.
pub fn serial_port_candidates_enumerated(
    configured: Option<&str>,
    enumerate: impl FnOnce() -> Vec<String>,
) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(c) = configured {
        candidates.push(c.to_string());
    }
    for p in enumerate() {
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }
    candidates
}

/// Whether two candidate lists name the same ports, ignoring order (DEC-291).
///
/// The rescan cooldown keys on "have the ports changed?", and the list is
/// assembled from two sources with different orderings — udev syspath order, then
/// a hard-coded ACM-before-USB path scan. `available_ports()` returns `Ok(vec![])`
/// rather than `Err` when libudev is unavailable, so the daemon can silently fall
/// through from one ordering to the other on the same hardware. Comparing the
/// `Vec`s directly would then read "the ports changed", skip the cooldown, and
/// allow the DTR sweep it exists to prevent.
pub fn same_port_set(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<&String> = a.iter().collect();
    let mut b: Vec<&String> = b.iter().collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

/// How long to keep looking for an OpenFanController AFTER boot.
///
/// Boot itself now makes exactly **one** adoption attempt and then gets out of
/// the way (`OFN-r`/`OFN-s`); everything after that runs detached, off the
/// critical path, driven by `post_boot_adoption_loop`. This returns how long
/// that detached search stays interested.
///
/// The window replaced a synchronous retry ladder that slept up to ~31 s before
/// the API server, both poll loops and the profile engine were started. Two
/// things were wrong with it: a machine with no controller paid the whole stall
/// for nothing, and a machine WITH one got no second chance at all if the device
/// enumerated late — the reconnect probe lives inside the OpenFan poll loop,
/// which is only spawned when boot already adopted something, so a miss was
/// terminal for the process lifetime.
///
/// * **Configured** (`daemon.toml` / `runtime.toml` / `POST /config/serial-port`) —
///   the user named a device, so its absence is worth staying interested in.
/// * **Not configured** — auto-detect. Shorter, but still far longer than the
///   ~31 s ladder it replaces, because waiting now costs nothing: the daemon is
///   fully serving throughout.
///
/// Cheap regardless of length, because the loop **compares the candidate set
/// itself** and probes only when it changes. Do not attribute that to DEC-291's
/// rescan cooldown: that predicate is `elapsed < COOLDOWN && same_port_set(..)`,
/// an AND, so it spaces repeat probes to one per ten seconds and never skips
/// one — relying on it would have re-opened every unrelated tty for the whole
/// window. See `post_boot_adoption_loop`.
pub fn post_boot_adoption_window(configured: bool) -> Duration {
    Duration::from_secs(if configured { 180 } else { 60 })
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
/// healthy, and the thermal emergency's `force_all_with_floor` reported success while driving
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
    configured: Option<&str>,
    timeout: Duration,
    mut open: impl FnMut(&str) -> Result<T, crate::error::SerialError>,
) -> Option<(String, T)> {
    for port in candidates {
        // OFN-c: a rejected CONFIGURED port is a fault — the user named that
        // device and the daemon could not adopt it — and stays a warning, which
        // is the DEC-250 signal above. A rejected *enumerated* candidate is not:
        // on a machine with no OpenFanController, every unrelated USB-serial
        // device on the bus lands here, and calling that a warning told every
        // hwmon-only user with an Arduino that something was broken. Note the
        // old wording made the same mistake in prose — "Failed to open
        // OpenFanController on /dev/ttyACM0" names a device that, in the case
        // that actually fires, is not one.
        let user_named = configured.is_some_and(|c| c == port.as_str());
        match open(port) {
            Ok(mut transport) => {
                match crate::serial::transport::verify_openfan_identity(&mut transport, timeout) {
                    Ok(()) => return Some((port.clone(), transport)),
                    Err(e) if user_named => log::warn!(
                        "Configured serial port {port} opened but did not identify as an \
                         OpenFanController ({e}) — not using it"
                    ),
                    Err(e) => log::debug!(
                        "{port} opened but did not identify as an OpenFanController ({e}) \
                         — not using it"
                    ),
                }
            }
            Err(e) if user_named => {
                log::warn!("Failed to open configured serial port {port}: {e}")
            }
            Err(e) => log::debug!("Could not open serial candidate {port}: {e}"),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── `OFN-a`/`OFN-r`: the post-boot adoption window ───────────────────────

    #[test]
    fn an_unconfigured_machine_still_gets_a_long_look_after_boot() {
        // The DISCRIMINATING arm. Boot pays ONE attempt now, so the thing that
        // decides whether a late-enumerating controller is ever found is this
        // window — and it must be generous, because it costs nothing: the daemon
        // is fully serving throughout, and a tick over unchanged hardware opens
        // no device at all.
        let w = post_boot_adoption_window(false);
        assert!(
            w >= Duration::from_secs(31),
            "the detached window must exceed the ~31 s synchronous ladder it replaced, \
             or a late controller is found LESS often than before; got {w:?}"
        );
    }

    #[test]
    fn a_configured_port_is_waited_on_longer() {
        // The opposite arm: without it, a constant window would pass above. Where
        // the user NAMED a device, its absence is worth staying interested in.
        assert!(post_boot_adoption_window(true) > post_boot_adoption_window(false));
    }

    /// The configured port is tried FIRST — the ordering rule this function
    /// exists to keep testable (DEC-250's sibling property, DEC-291).
    #[test]
    fn configured_port_leads_the_candidate_list() {
        let c = serial_port_candidates_enumerated(Some("/dev/ttyACM9"), || {
            vec!["/dev/ttyACM0".into(), "/dev/ttyUSB0".into()]
        });
        assert_eq!(c[0], "/dev/ttyACM9");
        assert_eq!(c.len(), 3);
    }

    /// A configured port that the enumerator also reports must not be probed
    /// twice: `first_openfan_port` opens each entry, and opening asserts DTR, so
    /// a duplicate is a second reset of the same board.
    #[test]
    fn a_configured_port_is_not_duplicated_by_the_enumerator() {
        let c = serial_port_candidates_enumerated(Some("/dev/ttyACM0"), || {
            vec!["/dev/ttyACM0".into(), "/dev/ttyACM1".into()]
        });
        assert_eq!(c, vec!["/dev/ttyACM0", "/dev/ttyACM1"]);
    }

    /// Same reason, for duplicates arising WITHIN the enumerator — the real one
    /// merges udev output with a path scan, which can name the same tty twice.
    #[test]
    fn duplicates_within_the_enumeration_are_dropped() {
        let c = serial_port_candidates_enumerated(None, || {
            vec![
                "/dev/ttyACM0".into(),
                "/dev/ttyACM0".into(),
                "/dev/ttyUSB0".into(),
            ]
        });
        assert_eq!(c, vec!["/dev/ttyACM0", "/dev/ttyUSB0"]);
    }

    #[test]
    fn no_configured_port_and_nothing_enumerated_yields_nothing() {
        assert!(serial_port_candidates_enumerated(None, Vec::new).is_empty());
        assert_eq!(
            serial_port_candidates_enumerated(Some("/dev/ttyACM0"), Vec::new),
            vec!["/dev/ttyACM0"]
        );
    }

    /// The cooldown keys on "have the ports changed?", and the list is assembled
    /// from two sources with different orderings. Comparing order-sensitively
    /// would read a reordering as a change, skip the cooldown, and allow the DTR
    /// sweep it exists to prevent.
    #[test]
    fn the_same_ports_in_a_different_order_are_the_same_set() {
        let a = vec!["/dev/ttyACM0".to_string(), "/dev/ttyUSB0".to_string()];
        let b = vec!["/dev/ttyUSB0".to_string(), "/dev/ttyACM0".to_string()];
        assert!(same_port_set(&a, &b));
    }

    #[test]
    fn a_genuinely_changed_port_set_is_not_the_same_set() {
        let a = vec!["/dev/ttyACM0".to_string()];
        let b = vec!["/dev/ttyACM0".to_string(), "/dev/ttyACM1".to_string()];
        assert!(
            !same_port_set(&a, &b),
            "attaching a device must lift the cooldown"
        );
        assert!(!same_port_set(&b, &a), "removing one must too");
        assert!(!same_port_set(
            &["/dev/ttyACM0".to_string()],
            &["/dev/ttyACM1".to_string()]
        ));
    }
}
