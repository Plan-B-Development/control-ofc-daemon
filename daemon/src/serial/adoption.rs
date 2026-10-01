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
/// injected here was `auto_detect_port`, which OPENED each candidate to identify
/// it (retired by `DC-ae`). This function is kept for its DEC-250 ordering tests, which are the record
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

/// Which device node a path names right now: its `(st_dev, st_ino)`, following
/// symlinks (`DC-ae`).
///
/// A name is not an identity. When a USB-serial device goes away and another
/// arrives, the kernel hands the newcomer the lowest free minor, so
/// `/dev/ttyACM0` can name the OpenFanController at one reconnect attempt and an
/// unrelated Arduino at the next, 30 cycles later, with nothing in between for a
/// name comparison to see. devtmpfs removes a node with its device and creates a
/// fresh inode for the next one, so the pair changes whenever the device behind a
/// name does. A `/dev/serial/by-id/` link resolves to the node it points at.
///
/// `stat(2)` opens nothing, so this is safe to call on a stranger's device.
/// `None` means the path does not resolve to anything (or cannot be stat'ed) —
/// there is nothing to open there either way.
pub fn node_id(path: &str) -> Option<NodeId> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some(NodeId {
        dev: m.dev(),
        ino: m.ino(),
    })
}

/// A device node's identity — see [`node_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeId {
    pub dev: u64,
    pub ino: u64,
}

/// How long a newly appeared node is re-probed on EVERY reconnect attempt, from
/// the attempt that first saw it (`DC-ae`) — for a board whose tty enumerates
/// before its firmware answers the handshake, or while ModemManager is still
/// probing it. Counted in time, not attempts: the backoff packs its first
/// attempts into the seconds after a drop (cycles 0, 2, 4, 8), so an attempt
/// budget was spent in ~8 s and then stranded the controller for good.
pub const RECONNECT_NEW_NODE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// The fewest opens a new node gets before the window alone may end its
/// every-attempt probing — where `polling.poll_interval_ms` is raised, fewer
/// attempts fit in [`RECONNECT_NEW_NODE_WINDOW`].
pub const RECONNECT_NEW_NODE_MIN_OPENS: u32 = 4;

/// After its window, a new node that has not identified is re-probed at most
/// once per this long, for as long as the controller stays gone (`DC-ae`, the
/// user's choice at review). Never giving up is what keeps a slow controller
/// findable — the reconnect probe is the only way back, and so the thermal
/// emergency's only route to the OpenFan fans. The cost falls only on a stranger
/// plugged in after adoption: one DTR reset per interval while the controller is
/// away, where the retired sweep reset every tty about every 30 s.
pub const RECONNECT_NEW_NODE_SLOW_RETRY: std::time::Duration = std::time::Duration::from_secs(300);

/// What the OpenFan poll loop's reconnect probe may open, attempt by attempt
/// (`DC-ae`).
///
/// The probe used to call `auto_detect_port`, which OPENS every `ttyACM`/`ttyUSB`
/// node until one identifies — and it ran on every backoff cycle, one sweep every
/// 30 cycles for as long as the controller stayed gone. Opening a tty asserts DTR
/// and resets Arduino-class boards, so each unrelated one was reset on that cadence
/// indefinitely: the hazard `OFN-b` removed from boot and DEC-361 kept out of the
/// post-boot search, left standing on the one path that runs without end. It also
/// ignored the configured port.
///
/// Each attempt now opens, in order and each node at most once:
///
/// 1. **The configured port**, when it resolves. The user named that device.
/// 2. **The node the controller was adopted on**, while it is still the node it
///    was — same `(dev, ino)`: a controller that stopped answering without
///    re-enumerating. serialport opens with an exclusive `flock`, which root does
///    not bypass, so this open succeeds only because the poll loop closes the old
///    port before its first attempt (`release_adopted_port`, `DC-ct`). Re-opening
///    does not reset an OpenFanController: its firmware has no DTR handler. Once
///    the node is seen missing or re-created, it is never probed again for this
///    drop, because its name may now belong to someone else.
/// 3. **Every candidate node that appeared since the survey began** — a
///    `(path, NodeId)` pair not in the previous observation when first seen — on
///    every attempt for [`RECONNECT_NEW_NODE_WINDOW`] (and at least
///    [`RECONNECT_NEW_NODE_MIN_OPENS`] times), then once per
///    [`RECONNECT_NEW_NODE_SLOW_RETRY`] while it stays. A returning controller
///    always arrives as a new node, even when two devices are replugged and swap
///    names; a node present all along with the same identity is someone else's,
///    and is left alone.
///
/// The first observation is seeded from the candidate list the adoption itself was
/// made from, so a drop costs no sweep. What that seed cannot know about is a
/// device plugged in after adoption: it is new at the first attempt, and is opened
/// on the new-node schedule while the controller stays away.
///
/// Pure over the injected `observe` and `now` so the rule is testable without a
/// device or a clock; production passes [`node_id`] and `Instant::now()`.
#[derive(Debug, Clone)]
pub struct ReconnectSurvey {
    configured: Option<String>,
    /// `None` once the adopted node has been seen missing or re-created.
    adopted: Option<(String, NodeId)>,
    baseline: Vec<(String, NodeId)>,
    fresh: Vec<FreshNode>,
}

/// A node that appeared after the survey began, and its probe history.
#[derive(Debug, Clone)]
struct FreshNode {
    node: (String, NodeId),
    first_seen: std::time::Instant,
    last_open: std::time::Instant,
    opens: u32,
}

impl FreshNode {
    fn due(&self, now: std::time::Instant) -> bool {
        self.opens < RECONNECT_NEW_NODE_MIN_OPENS
            || now.saturating_duration_since(self.first_seen) < RECONNECT_NEW_NODE_WINDOW
            || now.saturating_duration_since(self.last_open) >= RECONNECT_NEW_NODE_SLOW_RETRY
    }
}

impl ReconnectSurvey {
    /// Start a survey for a controller just adopted on `adopted`, seeded with the
    /// candidates that adoption was chosen from. Build it at the adoption, not
    /// later: a node read after the controller re-enumerated would seed the new
    /// node as "present all along" and the controller would never be probed.
    pub fn new(
        configured: Option<String>,
        adopted: &str,
        seed: &[String],
        observe: impl Fn(&str) -> Option<NodeId>,
    ) -> Self {
        Self {
            configured,
            adopted: observe(adopted).map(|id| (adopted.to_string(), id)),
            baseline: observed(seed, &observe),
            fresh: Vec::new(),
        }
    }

    /// The configured port this survey tries first — for `first_openfan_port`'s
    /// log level, which must agree with the plan about which port the user named.
    pub fn configured(&self) -> Option<&str> {
        self.configured.as_deref()
    }

    /// The ports this attempt may open, in order, given the candidates enumerated
    /// now (without opening them). Advances the survey: the observation becomes
    /// the next attempt's baseline, and each new node's schedule moves on.
    pub fn plan(
        &mut self,
        candidates: &[String],
        observe: impl Fn(&str) -> Option<NodeId>,
        now: std::time::Instant,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut ids: Vec<NodeId> = Vec::new();
        let mut push = |path: &str, id: NodeId| {
            if !out.iter().any(|p| p == path) && !ids.contains(&id) {
                out.push(path.to_string());
                ids.push(id);
            }
        };

        if let Some(c) = self.configured.as_deref() {
            if let Some(id) = observe(c) {
                push(c, id);
            }
        }

        if let Some((path, id)) = self.adopted.clone() {
            if observe(&path) == Some(id) {
                push(&path, id);
            } else {
                log::info!(
                    "OpenFan reconnect: {path} is gone or now names a different device — \
                     no longer re-probing it"
                );
                self.adopted = None;
            }
        }

        let observed_now = observed(candidates, &observe);
        // A node that left, or came back as a different device, is forgotten.
        self.fresh.retain(|f| observed_now.contains(&f.node));
        for entry in &observed_now {
            if !self.baseline.contains(entry) && !self.fresh.iter().any(|f| &f.node == entry) {
                self.fresh.push(FreshNode {
                    node: entry.clone(),
                    first_seen: now,
                    last_open: now,
                    opens: 0,
                });
            }
            if let Some(f) = self.fresh.iter_mut().find(|f| &f.node == entry) {
                if f.opens == 0 || f.due(now) {
                    f.opens = f.opens.saturating_add(1);
                    f.last_open = now;
                    push(&entry.0, entry.1);
                }
            }
        }
        self.baseline = observed_now;
        out
    }
}

/// The candidates that resolve right now, each with its node identity.
fn observed(paths: &[String], observe: &impl Fn(&str) -> Option<NodeId>) -> Vec<(String, NodeId)> {
    paths
        .iter()
        .filter_map(|p| observe(p).map(|id| (p.clone(), id)))
        .collect()
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

    // ── `DC-ae`: what a reconnect attempt may open ──────────────────────────

    const OPENFAN: &str = "/dev/ttyACM0";
    const ARDUINO: &str = "/dev/ttyACM1";

    fn id(ino: u64) -> NodeId {
        NodeId { dev: 5, ino }
    }

    /// A fake `/dev`: path → the node it names right now. `observe` over it.
    fn bus<'a>(nodes: &'a [(&'a str, u64)]) -> impl Fn(&str) -> Option<NodeId> + 'a {
        move |p| nodes.iter().find(|(n, _)| *n == p).map(|(_, i)| id(*i))
    }

    fn names(nodes: &[(&str, u64)]) -> Vec<String> {
        nodes.iter().map(|(n, _)| n.to_string()).collect()
    }

    /// The controller adopted on ACM0 beside an Arduino on ACM1, both present
    /// at adoption.
    fn adopted_beside_an_arduino() -> ReconnectSurvey {
        let at_adoption = [(OPENFAN, 1), (ARDUINO, 2)];
        ReconnectSurvey::new(None, OPENFAN, &names(&at_adoption), bus(&at_adoption))
    }

    /// Run one attempt per entry of `secs` (seconds after the first) over an
    /// unchanging bus, returning how many times `path` was planned at each.
    fn opens_of(
        s: &mut ReconnectSurvey,
        now: &[(&str, u64)],
        secs: &[u64],
        path: &str,
    ) -> Vec<(u64, usize)> {
        let t0 = std::time::Instant::now();
        secs.iter()
            .map(|&t| {
                let plan = s.plan(&names(now), bus(now), t0 + Duration::from_secs(t));
                (t, plan.iter().filter(|p| *p == path).count())
            })
            .collect()
    }

    /// Attempts every 30 s for 20 minutes — the loop's settled cadence at the
    /// default poll interval — after the early ramp at 0, 2, 4, 8 and 16 s.
    fn schedule() -> Vec<u64> {
        let mut v = vec![0, 2, 4, 8, 16];
        v.extend((1..=40).map(|k| k * 30));
        v
    }

    #[test]
    fn a_stranger_present_all_along_is_never_opened() {
        // The DISCRIMINATING arm, and the defect: `auto_detect_port` opened ACM1
        // on every attempt — a DTR reset every 30 cycles, for as long as the
        // controller stayed gone. Here the controller has wedged on its own node.
        let mut s = adopted_beside_an_arduino();
        let now = [(OPENFAN, 1), (ARDUINO, 2)];
        for (t, n) in opens_of(&mut s, &now, &schedule(), ARDUINO) {
            assert_eq!(n, 0, "t={t}s: a tty present all along must never be opened");
        }
        let mut s = adopted_beside_an_arduino();
        for (t, n) in opens_of(&mut s, &now, &schedule(), OPENFAN) {
            assert_eq!(n, 1, "t={t}s: the adopted node is re-probed every attempt");
        }
    }

    #[test]
    fn a_new_node_is_never_given_up_on() {
        // The review's P1. The controller came back on ACM2 and does not answer
        // at first (slow firmware, ModemManager probing it). An ATTEMPT budget was
        // spent in the backoff's first ~8 s and never probed ACM2 again, so the
        // controller — and the thermal emergency's OpenFan leg — was lost until a
        // restart. It must still be probed after any length of absence.
        let mut s = adopted_beside_an_arduino();
        let now = [(ARDUINO, 2), ("/dev/ttyACM2", 7)];
        let opens = opens_of(&mut s, &now, &schedule(), "/dev/ttyACM2");
        let window = RECONNECT_NEW_NODE_WINDOW.as_secs();
        let slow = RECONNECT_NEW_NODE_SLOW_RETRY.as_secs();
        for &(t, n) in &opens {
            if t < window {
                assert_eq!(n, 1, "t={t}s: every attempt inside the window opens it");
            }
        }
        let late: Vec<u64> = opens
            .iter()
            .filter(|(t, n)| *t >= 600 && *n > 0)
            .map(|(t, _)| *t)
            .collect();
        assert!(!late.is_empty(), "never probed after 10 minutes: {opens:?}");
        // …and after the window, at most once per slow-retry interval.
        let after: Vec<u64> = opens
            .iter()
            .filter(|(t, n)| *t >= window && *n > 0)
            .map(|(t, _)| *t)
            .collect();
        for pair in after.windows(2) {
            assert!(
                pair[1] - pair[0] >= slow,
                "opened {}s apart: {after:?}",
                pair[1] - pair[0]
            );
        }
    }

    #[test]
    fn a_new_node_gets_its_minimum_opens_even_when_attempts_are_sparse() {
        // A raised poll interval puts fewer attempts inside the window.
        let mut s = adopted_beside_an_arduino();
        let now = [(ARDUINO, 2), ("/dev/ttyACM2", 7)];
        let opens = opens_of(&mut s, &now, &[0, 50, 100, 150, 200], "/dev/ttyACM2");
        let total: usize = opens.iter().map(|(_, n)| n).sum();
        assert_eq!(total, RECONNECT_NEW_NODE_MIN_OPENS as usize, "{opens:?}");
    }

    #[test]
    fn the_adopted_name_reused_by_another_device_is_not_the_adopted_node() {
        // Between two attempts the controller left and a stranger took its name
        // — same path, re-created node, new inode. A name comparison sees nothing
        // and would re-open the stranger on EVERY attempt as the adopted node; the
        // inode shows it is a new node, on the slow schedule once its window ends.
        let mut s = adopted_beside_an_arduino();
        let now = [(OPENFAN, 9), (ARDUINO, 2)];
        let opens = opens_of(
            &mut s,
            &now,
            &[0, 30, 60, 90, 120, 150, 180, 210, 240, 270],
            OPENFAN,
        );
        assert_eq!(opens[0].1, 1, "a new node is opened when first seen");
        let quiet: usize = opens
            .iter()
            .filter(|(t, _)| (120..=270).contains(t))
            .map(|(_, n)| n)
            .sum();
        assert_eq!(quiet, 0, "{opens:?}");
    }

    #[test]
    fn a_node_seen_gone_is_never_treated_as_the_adopted_node_again() {
        let mut s = adopted_beside_an_arduino();
        let gone = [(ARDUINO, 2)];
        let t0 = std::time::Instant::now();
        assert!(s.plan(&names(&gone), bus(&gone), t0).is_empty());
        // Even the SAME inode reappearing is only a new node from here on.
        let back = [(OPENFAN, 1), (ARDUINO, 2)];
        let opens = opens_of(
            &mut s,
            &back,
            &[0, 30, 60, 90, 120, 150, 180, 210, 240, 270],
            OPENFAN,
        );
        let quiet: usize = opens
            .iter()
            .filter(|(t, _)| (120..=270).contains(t))
            .map(|(_, n)| n)
            .sum();
        assert_eq!(opens[0].1, 1);
        assert_eq!(quiet, 0, "{opens:?}");
    }

    #[test]
    fn two_devices_that_swap_names_are_both_new() {
        // Both unplugged and replugged between attempts, the other way round. The
        // set of NAMES is unchanged, so a name comparison would never look — and
        // would keep re-opening ACM0, now the Arduino, as the "adopted node".
        let mut s = adopted_beside_an_arduino();
        let now = [(OPENFAN, 3), (ARDUINO, 4)];
        let plan = s.plan(&names(&now), bus(&now), std::time::Instant::now());
        assert_eq!(plan, vec![OPENFAN, ARDUINO]);
    }

    #[test]
    fn the_configured_port_is_tried_first_every_time_and_opened_once() {
        // The user named the device, by its by-id link. It resolves to the same
        // node as ACM0, so ACM0 must not be opened a second time under its other
        // name — each open is a DTR reset.
        let by_id = "/dev/serial/by-id/usb-Karanovic_Research_OpenFan-if00";
        let at_adoption = [(by_id, 1), (OPENFAN, 1), (ARDUINO, 2)];
        let mut s = ReconnectSurvey::new(
            Some(by_id.to_string()),
            by_id,
            &names(&at_adoption),
            bus(&at_adoption),
        );
        assert_eq!(s.configured(), Some(by_id));
        for _ in 0..5 {
            assert_eq!(
                s.plan(
                    &names(&at_adoption[1..]),
                    bus(&at_adoption),
                    std::time::Instant::now()
                ),
                vec![by_id]
            );
        }
    }

    #[test]
    fn a_configured_port_that_does_not_resolve_is_not_opened() {
        let at_adoption = [(OPENFAN, 1)];
        let mut s = ReconnectSurvey::new(
            Some("/dev/serial/by-id/absent".to_string()),
            OPENFAN,
            &names(&at_adoption),
            bus(&at_adoption),
        );
        assert_eq!(
            s.plan(
                &names(&at_adoption),
                bus(&at_adoption),
                std::time::Instant::now()
            ),
            vec![OPENFAN]
        );
    }
}
