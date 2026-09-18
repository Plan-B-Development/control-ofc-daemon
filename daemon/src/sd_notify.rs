//! Readiness and watchdog notifications to systemd (DEC-387, `TS-d`).
//!
//! # Why
//!
//! DEC-266 restarts the daemon when the profile engine task *dies*. Nothing
//! restarted it when the engine was *alive but no longer ticking* — a deadlock,
//! an await that never resolves, a starved runtime — and that is the worst state
//! the daemon has: every fan holds its last duty and nothing evaluates the thermal
//! ladder, while systemd sees a healthy process. The unit's watchdog closes it: a
//! loop that stops completing ticks stops pinging, and systemd kills it, runs
//! `ExecStopPost` (which hands the hwmon headers back, DEC-382) and restarts it.
//!
//! # What is sent, and from where
//!
//! | message | sent from | when |
//! | --- | --- | --- |
//! | `READY=1` | `main` | once the engine is ticking and the API is serving |
//! | `WATCHDOG=1` | `TickCompletion::drop` | every completed engine tick, and nowhere else |
//! | `RESTART_RESET=1` | the same ping | once, after [`RESTART_RESET_AFTER_TICKS`] completed ticks |
//! | `STOPPING=1` + `WATCHDOG_USEC=0` | `finish_shutdown` | the moment shutdown begins |
//! | `EXTEND_TIMEOUT_USEC=` | `main` | before each boot-time serial probe |
//!
//! The ping lives in the tick's completion guard because "a tick completed" is
//! the only liveness this watchdog should measure. A device write that is slow
//! or wedged does not stop it: DEC-289/298/299 bounded every backend join, so the
//! loop keeps ticking past such a write, and restarting could not unwedge a
//! kernel driver anyway. The watchdog therefore fires on a wedged LOOP, never on
//! a wedged device.
//!
//! **`WATCHDOG_USEC=0` on stop is load-bearing, not tidiness.** systemd re-arms
//! the watchdog on every `WATCHDOG=1` it receives, whatever state the unit is in
//! — `service_notify_message` calls `service_reset_watchdog` with no state check
//! (v261). A tick that completes after the stop has begun would therefore arm a
//! fresh timer over the bounded hardware restore, and a restore slower than
//! `WatchdogSec` would be killed part-way through handing the fans back. Measured
//! on systemd 261 with a transient unit, `WatchdogSec=2`: a ping sent 0.2 s after
//! `STOPPING=1` drew `Watchdog timeout (limit 2s)!` and SIGABRT 2 s later, in the
//! middle of a 5 s simulated restore; with `WATCHDOG_USEC=0` in the same message
//! as `STOPPING=1` the same late ping was harmless. A zero override disarms the
//! watchdog for this invocation whatever arrives after it, and systemd clears
//! the override on the next start (`service_start`).
//!
//! **Start-up is bounded by progress, not by a guess.** The boot-time serial probe
//! opens every enumerated candidate, each bounded by `serial.timeout_ms`, and
//! neither the number of candidates nor that timeout has a hard ceiling in the
//! admin file. So `main` asks for [`Notifier::extend_timeout`] before each probe:
//! a machine with many serial devices keeps extending its own start deadline, and
//! a probe that never returns stops extending it, which times the start out as
//! before.
//!
//! **A limit worth knowing (`TS-ao`).** The watchdog's clock is `CLOCK_MONOTONIC`.
//! It does not advance during the sleep itself, but user space — this daemon and
//! PID 1 alike — is frozen while devices suspend and resume, and that stretch does
//! count. A machine whose device suspend and resume together take more than about
//! ten seconds can therefore see the daemon restarted as it resumes.
//!
//! # No dependency
//!
//! The protocol is one datagram per message to the socket named in
//! `$NOTIFY_SOCKET`. `sd_notify(3)` publishes standalone reimplementations for
//! exactly this purpose, and systemd documents the interface as stable. Every
//! call here is a no-op when `$NOTIFY_SOCKET` is unset, so the daemon behaves
//! exactly as before when run by hand, in a test, or under a unit that does not
//! ask for notifications.

use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Completed ticks after which the daemon asks systemd to reset its restart
/// counter (`RESTART_RESET=1`).
///
/// `RestartSteps=` grows the restart delay with systemd's restart counter, and
/// nothing resets that counter short of a manual start. Without this, the fifth
/// unrelated crash in a long uptime would wait the full `RestartMaxDelaySec=`,
/// exactly as a tight crash loop does, and a fan daemon's time to recover is a
/// safety property. Five minutes of completed ticks is five times the longest
/// delay the unit allows: a fault that recurs faster than that keeps backing
/// off, and one that does not is treated as new. The field needs systemd 258 or
/// later; older managers ignore unknown fields.
pub const RESTART_RESET_AFTER_TICKS: u64 = 300;

/// Retries of a one-shot message whose send found systemd's queue full.
const ONE_SHOT_RETRIES: u32 = 50;
/// The pause between those retries: 50 x 2 ms bounds the wait at ~100 ms.
const ONE_SHOT_RETRY_GAP: Duration = Duration::from_millis(2);

/// The daemon's connection to systemd's notification socket.
///
/// Built once in `main` from the environment systemd provides, and shared with
/// the profile engine through `StateCache::attach_notifier`.
pub struct Notifier {
    socket: UnixDatagram,
    addr: SocketAddr,
    /// `Some` when systemd expects keep-alives from this process. No
    /// `WATCHDOG=1` is ever sent when it is `None`.
    watchdog: Option<Duration>,
    /// Set by [`Self::stopping`]. No ping is sent after it, and systemd has been
    /// told to ignore any that are already in flight.
    stopping: AtomicBool,
    /// Completed ticks seen, for [`RESTART_RESET_AFTER_TICKS`] — counted whether
    /// or not their keep-alive could be sent. Only the engine calls in, so this
    /// never sees contention; it is atomic only so `Notifier` is `Sync`.
    ticks: AtomicU64,
    restart_reset_sent: AtomicBool,
    /// A failing ping is logged once, not once a second.
    ping_failure_logged: AtomicBool,
    /// [`ONE_SHOT_RETRIES`] and [`ONE_SHOT_RETRY_GAP`], held per notifier only so
    /// a test can widen them; production never changes them.
    one_shot_retries: u32,
    one_shot_gap: Duration,
}

impl Notifier {
    /// The notifier systemd asked for, or `None` when `$NOTIFY_SOCKET` is unset
    /// or empty (not running under a unit that wants notifications).
    ///
    /// A value that is set but cannot be used is logged as an error and also
    /// yields `None`: a `Type=notify` unit will then time out waiting for
    /// `READY=1` and restart the daemon, and the log line says why.
    pub fn from_env() -> Option<Self> {
        let socket = std::env::var("NOTIFY_SOCKET")
            .ok()
            .filter(|s| !s.is_empty())?;
        let watchdog = watchdog_timeout(
            std::env::var("WATCHDOG_USEC").ok().as_deref(),
            std::env::var("WATCHDOG_PID").ok().as_deref(),
            std::process::id(),
        );
        match Self::new(&socket, watchdog) {
            Ok(notifier) => Some(notifier),
            Err(e) => {
                log::error!(
                    "NOTIFY_SOCKET={socket:?} cannot be used ({e}) — systemd will not be \
                     told this daemon is ready, so a Type=notify unit will time out and \
                     restart it"
                );
                None
            }
        }
    }

    /// A notifier for the socket `notify_socket`, in `$NOTIFY_SOCKET` syntax: an
    /// absolute path, or `@name` for a Linux abstract socket.
    pub fn new(notify_socket: &str, watchdog: Option<Duration>) -> io::Result<Self> {
        let addr = socket_address(notify_socket)?;
        let socket = UnixDatagram::unbound()?;
        // [SAFETY] Non-blocking, because the ping is sent from the profile
        // engine's tick. A datagram to a receiver whose queue is full BLOCKS a
        // blocking sender, so a service manager that stopped reading would
        // otherwise freeze the sole PWM writer — the exact failure the watchdog
        // is here to catch. A ping that cannot be queued is dropped instead; one
        // miss in a timeout that spans many ticks costs nothing.
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            addr,
            watchdog,
            stopping: AtomicBool::new(false),
            ticks: AtomicU64::new(0),
            restart_reset_sent: AtomicBool::new(false),
            ping_failure_logged: AtomicBool::new(false),
            one_shot_retries: ONE_SHOT_RETRIES,
            one_shot_gap: ONE_SHOT_RETRY_GAP,
        })
    }

    /// Widen the one-shot retry budget, so a test that frees the queue from
    /// another thread is not racing the ~100 ms production budget.
    #[cfg(test)]
    fn with_one_shot_budget(mut self, retries: u32, gap: Duration) -> Self {
        self.one_shot_retries = retries;
        self.one_shot_gap = gap;
        self
    }

    /// The watchdog timeout systemd configured, if it expects keep-alives.
    pub fn watchdog(&self) -> Option<Duration> {
        self.watchdog
    }

    /// Tell systemd start-up is complete. Starts the watchdog clock.
    pub fn ready(&self) {
        if let Err(e) = self.send_once("READY=1") {
            log::error!(
                "could not tell systemd the daemon is ready ({e}) — a Type=notify unit \
                 will time out and restart it"
            );
        }
    }

    /// Ask systemd for at least `extra` more time to finish starting up
    /// (`EXTEND_TIMEOUT_USEC`). `main` sends it before each boot-time serial
    /// probe, sized to that probe's own bound — see the module doc. systemd only
    /// ever moves a deadline LATER with this (v261
    /// `service_extend_event_source_timeout`), so it cannot shorten the start
    /// window the unit sets.
    pub fn extend_timeout(&self, extra: Duration) {
        let message = format!("EXTEND_TIMEOUT_USEC={}", extra.as_micros());
        if let Err(e) = self.send_once(&message) {
            log::warn!("could not ask systemd for more start-up time ({e})");
        }
    }

    /// One keep-alive — called from `TickCompletion::drop`, and only from there.
    ///
    /// Never blocks and never fails: a ping that cannot be sent is logged once
    /// and dropped. Sends nothing once [`Self::stopping`] has run. Without a
    /// configured watchdog it sends only the one-off `RESTART_RESET=1`, because
    /// the restart backoff that reset serves applies to crashes as well.
    pub fn watchdog_tick(&self) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let ticks = self.ticks.fetch_add(1, Ordering::Relaxed) + 1;
        let reset_due =
            ticks >= RESTART_RESET_AFTER_TICKS && !self.restart_reset_sent.load(Ordering::Relaxed);
        let message = match (self.watchdog.is_some(), reset_due) {
            (true, true) => "WATCHDOG=1\nRESTART_RESET=1",
            (true, false) => "WATCHDOG=1",
            (false, true) => "RESTART_RESET=1",
            (false, false) => return,
        };
        match self.send(message) {
            Ok(()) if reset_due => {
                self.restart_reset_sent.store(true, Ordering::Relaxed);
                log::info!(
                    "Healthy for {ticks} engine ticks — asked systemd to reset its restart \
                     counter, so a later fault restarts after RestartSec= again"
                );
            }
            Ok(()) => {}
            Err(e) => {
                if !self.ping_failure_logged.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "systemd watchdog ping failed ({e}); if this persists systemd will \
                         restart the daemon. Logged once."
                    );
                }
            }
        }
    }

    /// Tell systemd shutdown has begun, and disarm the watchdog for the rest of
    /// this process's life. See the module doc for why `WATCHDOG_USEC=0` is
    /// part of the same message.
    pub fn stopping(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Err(e) = self.send_once("STOPPING=1\nWATCHDOG_USEC=0") {
            log::warn!(
                "could not tell systemd the daemon is stopping ({e}); its watchdog may \
                 still be armed during the hardware restore"
            );
        }
    }

    /// [`Self::send`] for a message that has no second chance — `READY=1` and the
    /// stop. A keep-alive can afford to be dropped, because another follows in a
    /// second; these cannot. A lost `READY=1` costs a restart when
    /// `TimeoutStartSec=` expires, and a lost stop leaves the watchdog armed over
    /// the hardware restore — the hazard it exists to remove. So a full receive
    /// queue is retried briefly, and the wait is still bounded (at most
    /// [`ONE_SHOT_RETRIES`] x [`ONE_SHOT_RETRY_GAP`], ~100 ms) because the stop is
    /// sent ahead of the restore and must not delay it by more than that. Called
    /// only from `main`'s thread — never from the engine, whose tick must not wait.
    fn send_once(&self, message: &str) -> io::Result<()> {
        let mut attempt = 0;
        loop {
            match self.send(message) {
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock && attempt < self.one_shot_retries =>
                {
                    attempt += 1;
                    std::thread::sleep(self.one_shot_gap);
                }
                result => return result,
            }
        }
    }

    fn send(&self, message: &str) -> io::Result<()> {
        let sent = self.socket.send_to_addr(message.as_bytes(), &self.addr)?;
        if sent == message.len() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("sent {sent} of {} bytes", message.len()),
            ))
        }
    }
}

/// Resolve `$NOTIFY_SOCKET` syntax: `/path` or `@abstract-name`.
///
/// `vsock:` addresses (for a VM reporting to its host) are not supported: this
/// daemon controls a physical machine's fans and is never that guest.
fn socket_address(notify_socket: &str) -> io::Result<SocketAddr> {
    if let Some(name) = notify_socket.strip_prefix('@') {
        SocketAddr::from_abstract_name(name.as_bytes())
    } else if notify_socket.starts_with('/') {
        SocketAddr::from_pathname(notify_socket)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "only AF_UNIX path and abstract sockets are supported",
        ))
    }
}

/// The keep-alive timeout systemd expects of THIS process, per
/// `sd_watchdog_enabled(3)`: `$WATCHDOG_USEC` must be a positive, finite count
/// of microseconds, and `$WATCHDOG_PID`, when set, must name this process — a
/// child that inherited the variables is not the one being watched.
fn watchdog_timeout(usec: Option<&str>, pid: Option<&str>, own_pid: u32) -> Option<Duration> {
    let usec: u64 = usec?.parse().ok()?;
    if usec == 0 || usec == u64::MAX {
        return None;
    }
    if let Some(pid) = pid {
        if pid.parse::<u32>().ok()? != own_pid {
            return None;
        }
    }
    Some(Duration::from_micros(usec))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A receiving socket standing in for systemd, and a notifier aimed at it.
    struct Manager {
        _dir: tempfile::TempDir,
        rx: UnixDatagram,
        path: std::path::PathBuf,
    }

    impl Manager {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("notify");
            let rx = UnixDatagram::bind(&path).expect("bind the fake notify socket");
            rx.set_nonblocking(true).expect("nonblocking receiver");
            Self {
                _dir: dir,
                rx,
                path,
            }
        }

        fn notifier(&self, watchdog: Option<Duration>) -> Notifier {
            Notifier::new(self.path.to_str().expect("utf-8 path"), watchdog).expect("notifier")
        }

        /// Every datagram queued so far, in order.
        fn drain(&self) -> Vec<String> {
            let mut out = Vec::new();
            let mut buf = [0u8; 256];
            loop {
                match self.rx.recv(&mut buf) {
                    Ok(n) => out.push(String::from_utf8_lossy(&buf[..n]).into_owned()),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return out,
                    Err(e) => panic!("recv from the fake notify socket: {e}"),
                }
            }
        }
    }

    const WATCHDOG: Option<Duration> = Some(Duration::from_secs(15));

    #[test]
    fn each_message_reaches_the_socket_exactly_as_systemd_parses_it() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        n.ready();
        n.watchdog_tick();
        n.stopping();
        assert_eq!(
            m.drain(),
            ["READY=1", "WATCHDOG=1", "STOPPING=1\nWATCHDOG_USEC=0"],
        );
    }

    /// [SAFETY] The late-ping hazard, from this side: once shutdown has begun, no
    /// tick may send a keep-alive, because systemd would re-arm the watchdog over
    /// the hardware restore. (`WATCHDOG_USEC=0` covers a ping already in flight;
    /// this covers every one after.)
    #[test]
    fn no_ping_is_sent_once_stopping_has_begun() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        n.watchdog_tick();
        assert_eq!(m.drain(), ["WATCHDOG=1"], "precondition: pings are sent");
        n.stopping();
        n.watchdog_tick();
        n.watchdog_tick();
        assert_eq!(m.drain(), ["STOPPING=1\nWATCHDOG_USEC=0"]);
    }

    #[test]
    fn without_a_configured_watchdog_nothing_pings_but_readiness_still_reports() {
        let m = Manager::new();
        let n = m.notifier(None);
        n.ready();
        n.watchdog_tick();
        n.stopping();
        assert_eq!(m.drain(), ["READY=1", "STOPPING=1\nWATCHDOG_USEC=0"]);
    }

    /// The restart counter is reset once, on the ping that reaches the
    /// threshold, and never again.
    #[test]
    fn the_restart_counter_is_reset_once_after_a_healthy_stretch() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        for tick in 1..RESTART_RESET_AFTER_TICKS {
            n.watchdog_tick();
            assert_eq!(m.drain(), ["WATCHDOG=1"], "tick {tick}");
        }
        n.watchdog_tick();
        assert_eq!(m.drain(), ["WATCHDOG=1\nRESTART_RESET=1"]);
        for _ in 0..3 {
            n.watchdog_tick();
        }
        assert_eq!(m.drain(), ["WATCHDOG=1"; 3]);
    }

    /// A reset that could not be sent is retried on the next ping rather than
    /// lost: `restart_reset_sent` is set only by a send that succeeded.
    #[test]
    fn a_restart_reset_that_failed_to_send_is_retried() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        n.ticks
            .store(RESTART_RESET_AFTER_TICKS - 1, Ordering::Relaxed);
        std::fs::remove_file(&m.path).expect("unbind the fake socket");
        n.watchdog_tick(); // fails: nothing is listening
        assert!(!n.restart_reset_sent.load(Ordering::Relaxed));

        let rx = UnixDatagram::bind(&m.path).expect("rebind");
        rx.set_nonblocking(true).expect("nonblocking");
        n.watchdog_tick();
        let mut buf = [0u8; 64];
        let got = rx.recv(&mut buf).expect("the retried ping arrives");
        assert_eq!(&buf[..got], b"WATCHDOG=1\nRESTART_RESET=1");
    }

    /// [SAFETY] A service manager that stops reading must not freeze the engine:
    /// the ping is sent from the tick, and a blocking datagram send to a full
    /// queue waits for the reader. With the socket non-blocking, the extra pings
    /// are simply dropped.
    ///
    /// The pings run on their own thread under a deadline, and on failure the
    /// test drains the queue itself so a blocked sender is released rather than
    /// hanging the suite (DEC-272 trap 3).
    #[test]
    fn a_full_socket_never_blocks_the_ping() {
        let m = Manager::new();
        let n = std::sync::Arc::new(m.notifier(WATCHDOG));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let pinger = {
            let n = std::sync::Arc::clone(&n);
            std::thread::spawn(move || {
                // Far past both `net.unix.max_dgram_qlen` (10 by default, 512
                // where systemd's sysctl.d raises it) and the byte budget of a
                // default receive buffer.
                for _ in 0..5_000 {
                    n.watchdog_tick();
                }
                let _ = done_tx.send(());
            })
        };
        let finished = done_rx.recv_timeout(Duration::from_secs(10)).is_ok();
        if !finished {
            // Release the blocked sender so the thread can exit, then fail.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !pinger.is_finished() && std::time::Instant::now() < deadline {
                m.drain();
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(
            finished,
            "5,000 pings into a socket nobody reads must not block"
        );
        pinger.join().expect("pinger thread");
        assert!(
            n.ping_failure_logged.load(Ordering::Relaxed),
            "precondition: the queue really did fill, or this proved nothing"
        );
    }

    /// The stop and `READY=1` have no second chance, so a queue that is full only
    /// for a moment must not lose them — while a keep-alive in the same moment is
    /// simply dropped. The queue is filled with pings (which never wait), then a
    /// reader drains it shortly after the stop is sent; the stop must arrive.
    #[test]
    fn a_one_shot_message_waits_out_a_briefly_full_queue() {
        let m = Manager::new();
        // A 5 s budget rather than production's ~100 ms, so the drainer thread
        // waking late on a loaded runner cannot fail the test; the same retry
        // loop runs either way, and removing it still loses the stop.
        let n = m
            .notifier(WATCHDOG)
            .with_one_shot_budget(5_000, Duration::from_millis(1));
        for _ in 0..5_000 {
            n.watchdog_tick();
        }
        assert!(
            n.ping_failure_logged.load(Ordering::Relaxed),
            "precondition: the queue really is full"
        );
        let drainer = std::thread::spawn({
            let rx = m.rx.try_clone().expect("clone the receiver");
            move || {
                std::thread::sleep(Duration::from_millis(20));
                let mut buf = [0u8; 256];
                let mut got = Vec::new();
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    match rx.recv(&mut buf) {
                        Ok(k) => got.push(String::from_utf8_lossy(&buf[..k]).into_owned()),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            if got.last().is_some_and(|m| m.starts_with("STOPPING")) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => panic!("recv: {e}"),
                    }
                }
                got
            }
        });
        n.stopping();
        let got = drainer.join().expect("drainer");
        assert_eq!(
            got.last().map(String::as_str),
            Some("STOPPING=1\nWATCHDOG_USEC=0"),
            "the stop was lost to a queue that was full for only ~20 ms"
        );
    }

    /// The production budget, pinned: the stop is sent ahead of the hardware
    /// restore, so the one-shot retry must never delay it by more than ~100 ms.
    #[test]
    fn the_production_one_shot_wait_is_bounded() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        assert_eq!(
            (n.one_shot_retries, n.one_shot_gap),
            (ONE_SHOT_RETRIES, ONE_SHOT_RETRY_GAP)
        );
        assert!(ONE_SHOT_RETRY_GAP * ONE_SHOT_RETRIES <= Duration::from_millis(100));
    }

    #[test]
    fn a_start_extension_is_sent_in_microseconds() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        n.extend_timeout(Duration::from_secs(32));
        assert_eq!(m.drain(), ["EXTEND_TIMEOUT_USEC=32000000"]);
    }

    /// The restart backoff applies to crashes as well as to watchdog timeouts, so
    /// the reset is owed even where the watchdog has been switched off.
    #[test]
    fn without_a_watchdog_the_restart_counter_is_still_reset_once() {
        let m = Manager::new();
        let n = m.notifier(None);
        for _ in 1..RESTART_RESET_AFTER_TICKS {
            n.watchdog_tick();
        }
        assert!(m.drain().is_empty(), "no keep-alive without a watchdog");
        n.watchdog_tick();
        n.watchdog_tick();
        assert_eq!(m.drain(), ["RESTART_RESET=1"]);
    }

    #[test]
    fn an_abstract_socket_is_reached_by_its_at_sign_name() {
        let name = format!("control-ofc-sd-notify-test-{}", std::process::id());
        let rx = UnixDatagram::bind_addr(
            &SocketAddr::from_abstract_name(name.as_bytes()).expect("abstract addr"),
        )
        .expect("bind an abstract socket");
        let n = Notifier::new(&format!("@{name}"), None).expect("notifier");
        n.ready();
        let mut buf = [0u8; 16];
        let got = rx.recv(&mut buf).expect("READY=1 arrives");
        assert_eq!(&buf[..got], b"READY=1");
    }

    #[test]
    fn only_path_and_abstract_addresses_are_accepted() {
        assert!(socket_address("/run/systemd/notify").is_ok());
        assert!(socket_address("@some/name").is_ok());
        for bad in ["relative/notify", "vsock:2:1234", ""] {
            assert_eq!(
                socket_address(bad).unwrap_err().kind(),
                io::ErrorKind::Unsupported,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_watchdog_timeout_follows_sd_watchdog_enabled() {
        let me = 4242;
        let secs = |s: u64| Some(Duration::from_secs(s));
        assert_eq!(watchdog_timeout(Some("15000000"), None, me), secs(15));
        assert_eq!(
            watchdog_timeout(Some("15000000"), Some("4242"), me),
            secs(15)
        );
        // Set for a different process: a child that inherited the variables.
        assert_eq!(watchdog_timeout(Some("15000000"), Some("1"), me), None);
        assert_eq!(watchdog_timeout(Some("15000000"), Some("x"), me), None);
        // Absent, zero, infinite, or garbage: not enabled.
        assert_eq!(watchdog_timeout(None, Some("4242"), me), None);
        assert_eq!(watchdog_timeout(Some("0"), None, me), None);
        assert_eq!(
            watchdog_timeout(Some(&u64::MAX.to_string()), None, me),
            None
        );
        assert_eq!(watchdog_timeout(Some("15s"), None, me), None);
    }
}
