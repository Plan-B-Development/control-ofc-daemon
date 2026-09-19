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
//! | `WATCHDOG_USEC=` (wide, then configured) | [`watch_sleep`] | around a system sleep, on the hook's signals |
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
//! middle of a 5 s simulated restore (SIGABRT was systemd's default
//! `WatchdogSignal`; since DEC-388 the unit sends SIGTERM, which a stopping daemon
//! ignores, and SIGKILL follows `TimeoutAbortSec` later — the restore is still
//! cut, only later); with `WATCHDOG_USEC=0` in the same message
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
//! **System sleep (`TS-ao`, DEC-396).** The watchdog's clock is `CLOCK_MONOTONIC`.
//! It does not advance during the sleep itself, but user space — this daemon and
//! PID 1 alike — is frozen while devices suspend and resume, and that stretch does
//! count. A machine whose device suspend and resume together took more than about
//! ten seconds could therefore see the daemon restarted as it resumed. So the
//! package ships a `system-sleep` hook: it sends `SIGUSR1` before the sleep, and
//! [`watch_sleep`] answers with `WATCHDOG_USEC=` widened to [`SLEEP_WATCHDOG`] (never
//! narrowed below what the unit configured), then acknowledges in the runtime
//! directory so the hook — and with it the sleep — waits until the wide value is
//! queued to systemd. `SIGUSR2` after resume puts the configured value back. If no
//! resume signal arrives, [`watch_sleep`] puts it back by itself once
//! [`SLEEP_WATCHDOG`] has passed: a missing hook must never leave detection wide for
//! the rest of the process's life. Neither is ever sent once [`Notifier::stopping`]
//! has run, since any non-zero `WATCHDOG_USEC=` would re-arm what the stop disarmed.
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// How wide the watchdog is opened across a system sleep (`TS-ao`, DEC-396), and
/// how long [`watch_sleep`] waits for the resume signal before narrowing it again
/// by itself.
///
/// Eight times the unit's 15 s, to cover device suspend plus resume on slow
/// hardware — the stretch the frozen daemon cannot ping through. It is not a
/// hang budget in normal running: it applies only between the hook's two calls,
/// most of which the machine spends asleep with the clock stopped. A drop-in
/// that already configures a wider watchdog keeps its own value.
pub const SLEEP_WATCHDOG: Duration = Duration::from_secs(120);

/// How soon a restore that could not be delivered after resume is tried again.
const SLEEP_RESTORE_RETRY: Duration = Duration::from_secs(1);

/// The file the daemon writes its PID to once the sleep signals are handled, so
/// the hook signals only a daemon that understands them (`SIGUSR1`'s default
/// action is to terminate). Lives in the unit's runtime directory, which systemd
/// empties whenever the unit stops.
pub const SLEEP_HOOK_PID_FILE: &str = "sleep-hook.pid";
/// The acknowledgement the hook waits for before letting the sleep proceed.
pub const SLEEP_HOOK_ACK_FILE: &str = "sleep-hook.ack";

/// Which side of a system sleep a hook signal announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepTransition {
    /// `SIGUSR1`: the hook's `pre` call, just before the machine sleeps.
    Entering,
    /// `SIGUSR2`: the hook's `post` call, after it has resumed.
    Resumed,
}

/// What [`Notifier::sleep_transition`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepOutcome {
    /// The new `WATCHDOG_USEC=` reached systemd's queue.
    Sent,
    /// Nothing to send: no watchdog is configured, or the daemon is stopping.
    Skipped,
    /// It could not be sent, even after the one-shot retry.
    Failed,
}

impl SleepOutcome {
    fn as_ack(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

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
    /// Set only once the stop's `WATCHDOG_USEC=0` actually reached systemd's
    /// queue — by [`Self::stopping`] or by [`Self::resend_disarm_if_lost`]
    /// (`TS-ap`). `stopping` alone says the daemon *meant* to disarm.
    disarmed: AtomicBool,
    /// Serialises every message that sets `WATCHDOG_USEC=`: the stop's disarm and
    /// the sleep hook's widen/restore. Without it a sleep transition that had
    /// checked `stopping` could still send its non-zero value just AFTER the
    /// disarm and re-arm the watchdog over the hardware restore. Never taken by
    /// the engine's ping, which must not wait.
    watchdog_usec: Mutex<()>,
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
            disarmed: AtomicBool::new(false),
            watchdog_usec: Mutex::new(()),
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
        let _serial = self.watchdog_usec_lock();
        self.stopping.store(true, Ordering::Release);
        match self.send_once(STOP_MESSAGE) {
            Ok(()) => self.disarmed.store(true, Ordering::Release),
            Err(e) => log::warn!(
                "could not tell systemd the daemon is stopping ({e}); its watchdog may \
                 still be armed — trying again after the IPC server stops and before \
                 the hardware restore"
            ),
        }
    }

    /// `TS-ap`: send the stop's disarm again if no attempt has reached systemd
    /// yet. `finish_shutdown` calls it twice: once the IPC server has stopped,
    /// before the task drains (`TS-ay`, DEC-402), because those drains can
    /// together outlast what is left of the last keep-alive's deadline; and
    /// immediately before the hardware restore, after the drains have given a
    /// full queue more time to be read. Without it that deadline stays armed over
    /// a restore that can outlast it. A no-op once an attempt has landed, or
    /// before [`Self::stopping`] has run.
    pub fn resend_disarm_if_lost(&self) {
        let _serial = self.watchdog_usec_lock();
        if !self.stopping.load(Ordering::Acquire) || self.disarmed.load(Ordering::Acquire) {
            return;
        }
        match self.send_once(STOP_MESSAGE) {
            Ok(()) => {
                self.disarmed.store(true, Ordering::Release);
                log::info!("told systemd the daemon is stopping on a retry");
            }
            Err(e) => log::warn!(
                "could not tell systemd the daemon is stopping on a retry either ({e}); \
                 if no attempt lands before the hardware restore, a restore slower than \
                 the watchdog may be cut short, and ExecStopPost repeats the hwmon and \
                 GPU steps"
            ),
        }
    }

    /// Widen the watchdog for a system sleep, or put the configured value back
    /// after one (`TS-ao`, DEC-396) — see the module doc. Blocks for at most the
    /// one-shot retry budget, so [`watch_sleep`] calls it off the async workers.
    ///
    /// [SAFETY] Sends nothing when no watchdog is configured, because a non-zero
    /// `WATCHDOG_USEC=` would ENABLE one the unit never asked for; and nothing
    /// once [`Self::stopping`] has run, because it would re-arm the watchdog the
    /// stop disarmed. The check and the send share the lock `stopping` takes, so
    /// neither can slip in after the disarm.
    pub fn sleep_transition(&self, transition: SleepTransition) -> SleepOutcome {
        let _serial = self.watchdog_usec_lock();
        let Some(configured) = self.watchdog else {
            return SleepOutcome::Skipped;
        };
        if self.stopping.load(Ordering::Acquire) {
            return SleepOutcome::Skipped;
        }
        let usec = match transition {
            SleepTransition::Entering => configured.max(SLEEP_WATCHDOG),
            SleepTransition::Resumed => configured,
        };
        match self.send_once(&format!("WATCHDOG_USEC={}", usec.as_micros())) {
            Ok(()) => SleepOutcome::Sent,
            Err(e) => {
                log::warn!("could not change the systemd watchdog for {transition:?} ({e})");
                SleepOutcome::Failed
            }
        }
    }

    /// The `WATCHDOG_USEC=` lock. Poisoning is ignored: it guards no data, only
    /// the ordering of two sends, and a stop must never be refused over a panic.
    fn watchdog_usec_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.watchdog_usec
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// [`Self::send`] for a message that has no second chance — `READY=1` and the
    /// stop. A keep-alive can afford to be dropped, because another follows in a
    /// second; these cannot. A lost `READY=1` costs a restart when
    /// `TimeoutStartSec=` expires, and a lost stop leaves the watchdog armed over
    /// the hardware restore — the hazard it exists to remove. So a full receive
    /// queue is retried briefly, and each wait is still bounded (at most
    /// [`ONE_SHOT_RETRIES`] x [`ONE_SHOT_RETRY_GAP`], ~100 ms) because the stop is
    /// sent ahead of the restore and must not delay it by more than that. A stop
    /// makes at most three attempts (DEC-402), so ~300 ms in all, and only while
    /// systemd's queue stays full. Called
    /// only from `main`'s thread and from [`watch_sleep`]'s blocking task — never
    /// from the engine, whose tick must not wait.
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

/// The stop announcement, with the disarm in the same datagram (see the module doc).
const STOP_MESSAGE: &str = "STOPPING=1\nWATCHDOG_USEC=0";

/// Act on the sleep hook's signals for the life of the process (`TS-ao`, DEC-396).
///
/// Each [`SleepTransition`] is applied through [`Notifier::sleep_transition`] and,
/// when `ack_dir` is given, acknowledged in [`SLEEP_HOOK_ACK_FILE`] there — the
/// hook deletes that file, signals, and holds the sleep until it reappears. After
/// a widen, a resume signal that has not arrived within `fallback` is treated as
/// having arrived; a restore that could not be delivered is retried every
/// [`SLEEP_RESTORE_RETRY`]. Ends when `events` closes.
pub async fn watch_sleep(
    notifier: Arc<Notifier>,
    mut events: tokio::sync::mpsc::Receiver<SleepTransition>,
    fallback: Duration,
    ack_dir: Option<PathBuf>,
) {
    let mut narrow_at: Option<tokio::time::Instant> = None;
    loop {
        let transition = match narrow_at {
            Some(deadline) => tokio::select! {
                event = events.recv() => event,
                () = tokio::time::sleep_until(deadline) => Some(SleepTransition::Resumed),
            },
            None => events.recv().await,
        };
        let Some(transition) = transition else {
            return;
        };
        let n = Arc::clone(&notifier);
        let ack = ack_dir.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let outcome = n.sleep_transition(transition);
            if let Some(dir) = ack {
                write_sleep_ack(&dir, transition, outcome);
            }
            outcome
        })
        .await
        .unwrap_or(SleepOutcome::Failed);
        let now = tokio::time::Instant::now();
        narrow_at = match (transition, outcome) {
            (SleepTransition::Entering, SleepOutcome::Sent) => Some(now + fallback),
            // A widen that did not land changed nothing at systemd, so whatever
            // narrowing was already due — an earlier widen's fallback, or a
            // restore retry — is still due. Dropping it could leave a watchdog
            // that DID widen earlier wide for good.
            (SleepTransition::Entering, SleepOutcome::Failed) => narrow_at,
            (SleepTransition::Resumed, SleepOutcome::Failed) => Some(now + SLEEP_RESTORE_RETRY),
            (SleepTransition::Resumed, SleepOutcome::Sent) | (_, SleepOutcome::Skipped) => None,
        };
        match (transition, outcome) {
            (SleepTransition::Entering, SleepOutcome::Sent) => {
                log::info!("system sleep: systemd watchdog widened for the suspend and resume")
            }
            (SleepTransition::Resumed, SleepOutcome::Sent) => {
                log::info!("system sleep: systemd watchdog restored")
            }
            _ => {}
        }
    }
}

/// `<transition> <outcome>`, written whole by rename so the hook never reads half
/// of it. Failure is logged and ignored: the hook's wait is bounded, and the
/// watchdog change itself has already been sent or not by the time this runs.
fn write_sleep_ack(dir: &Path, transition: SleepTransition, outcome: SleepOutcome) {
    let word = match transition {
        SleepTransition::Entering => "pre",
        SleepTransition::Resumed => "post",
    };
    let tmp = dir.join(format!("{SLEEP_HOOK_ACK_FILE}.tmp"));
    let result = std::fs::write(&tmp, format!("{word} {}\n", outcome.as_ack()))
        .and_then(|()| std::fs::rename(&tmp, dir.join(SLEEP_HOOK_ACK_FILE)));
    if let Err(e) = result {
        log::warn!(
            "could not acknowledge the sleep hook in {} ({e})",
            dir.display()
        );
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

    /// [SAFETY] `TS-ap`: a stop that could not be delivered is sent again before
    /// the restore. The queue is full across the FIRST attempt only — filled with
    /// pings, which never wait, then read — so only a second attempt can land it.
    #[test]
    fn a_lost_disarm_is_sent_again_before_the_restore() {
        let m = Manager::new();
        // No retries, so the first attempt fails on the full queue at once; the
        // production budget would only make the test slower.
        let n = m.notifier(WATCHDOG).with_one_shot_budget(0, Duration::ZERO);
        for _ in 0..5_000 {
            n.watchdog_tick();
        }
        assert!(
            n.ping_failure_logged.load(Ordering::Relaxed),
            "precondition: the queue really is full"
        );
        n.stopping();
        assert!(
            !n.disarmed.load(Ordering::Acquire),
            "precondition: the first disarm was lost to the full queue"
        );
        let before = m.drain();
        assert!(
            !before.iter().any(|m| m.starts_with("STOPPING")),
            "precondition: the stop never reached the queue"
        );

        n.resend_disarm_if_lost();
        assert_eq!(m.drain(), [STOP_MESSAGE], "the second attempt must land");
        assert!(n.disarmed.load(Ordering::Acquire));

        n.resend_disarm_if_lost();
        assert!(m.drain().is_empty(), "a landed disarm is never repeated");
    }

    /// The retry is a no-op when the first attempt landed, and before any stop.
    #[test]
    fn a_disarm_that_landed_is_not_sent_again() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        n.resend_disarm_if_lost();
        assert!(m.drain().is_empty(), "nothing to resend before a stop");
        n.stopping();
        n.resend_disarm_if_lost();
        assert_eq!(m.drain(), [STOP_MESSAGE]);
    }

    /// `TS-ao`: widened to [`SLEEP_WATCHDOG`] for the sleep, back to the unit's own
    /// value after it.
    #[test]
    fn a_sleep_widens_the_watchdog_and_resume_restores_it() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        assert_eq!(
            n.sleep_transition(SleepTransition::Entering),
            SleepOutcome::Sent
        );
        assert_eq!(
            n.sleep_transition(SleepTransition::Resumed),
            SleepOutcome::Sent
        );
        assert_eq!(
            m.drain(),
            [
                format!("WATCHDOG_USEC={}", SLEEP_WATCHDOG.as_micros()),
                "WATCHDOG_USEC=15000000".to_owned(),
            ]
        );
        assert!(SLEEP_WATCHDOG > WATCHDOG.unwrap());
    }

    /// A drop-in that already configured a wider watchdog is never narrowed by a
    /// sleep.
    #[test]
    fn a_sleep_never_narrows_a_wider_configured_watchdog() {
        let m = Manager::new();
        let wide = SLEEP_WATCHDOG * 3;
        let n = m.notifier(Some(wide));
        n.sleep_transition(SleepTransition::Entering);
        assert_eq!(m.drain(), [format!("WATCHDOG_USEC={}", wide.as_micros())]);
    }

    /// [SAFETY] Without a configured watchdog a non-zero `WATCHDOG_USEC=` would
    /// ENABLE one, so nothing is sent.
    #[test]
    fn a_sleep_never_enables_a_watchdog_the_unit_did_not_configure() {
        let m = Manager::new();
        let n = m.notifier(None);
        for t in [SleepTransition::Entering, SleepTransition::Resumed] {
            assert_eq!(n.sleep_transition(t), SleepOutcome::Skipped);
        }
        assert!(m.drain().is_empty());
    }

    /// [SAFETY] Once the stop has disarmed the watchdog, a sleep transition must
    /// not re-arm it over the hardware restore.
    #[test]
    fn a_sleep_after_the_stop_does_not_re_arm_the_watchdog() {
        let m = Manager::new();
        let n = m.notifier(WATCHDOG);
        n.sleep_transition(SleepTransition::Entering);
        assert_eq!(m.drain().len(), 1, "precondition: transitions are sent");
        n.stopping();
        for t in [SleepTransition::Entering, SleepTransition::Resumed] {
            assert_eq!(n.sleep_transition(t), SleepOutcome::Skipped);
        }
        assert_eq!(m.drain(), [STOP_MESSAGE]);
    }

    /// Poll `m` until `want` has arrived, or fail after 5 s. Returns everything
    /// read, in order.
    async fn recv_until(m: &Manager, want: &str) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while std::time::Instant::now() < deadline {
            got.extend(m.drain());
            if got.iter().any(|g| g == want) {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{want:?} never arrived; got {got:?}");
    }

    /// Poll the ack file until it reads `want` (it is written just after the
    /// send), or fail after 5 s.
    async fn wait_for_ack(ack: &Path, want: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = std::fs::read_to_string(ack).unwrap_or_default();
            if got == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "ack never read {want:?}; last read {got:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The hook's round trip: each signal is applied and acknowledged in the
    /// runtime directory, which is what the `pre` hook waits for.
    #[tokio::test]
    async fn watch_sleep_applies_and_acknowledges_each_transition() {
        let m = Manager::new();
        let ack_dir = tempfile::tempdir().expect("ack dir");
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let task = tokio::spawn(watch_sleep(
            Arc::new(m.notifier(WATCHDOG)),
            rx,
            SLEEP_WATCHDOG,
            Some(ack_dir.path().to_owned()),
        ));
        let ack = ack_dir.path().join(SLEEP_HOOK_ACK_FILE);

        tx.send(SleepTransition::Entering).await.expect("send");
        recv_until(&m, &format!("WATCHDOG_USEC={}", SLEEP_WATCHDOG.as_micros())).await;
        wait_for_ack(&ack, "pre sent\n").await;

        tx.send(SleepTransition::Resumed).await.expect("send");
        recv_until(&m, "WATCHDOG_USEC=15000000").await;
        wait_for_ack(&ack, "post sent\n").await;

        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("watch_sleep ends when its channel closes")
            .expect("task");
    }

    /// [SAFETY] A widen whose resume signal never comes is narrowed by the daemon
    /// itself: a missing `post` must not leave detection wide for good.
    #[tokio::test]
    async fn a_widen_with_no_resume_is_narrowed_after_the_fallback() {
        let m = Manager::new();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let _task = tokio::spawn(watch_sleep(
            Arc::new(m.notifier(WATCHDOG)),
            rx,
            Duration::from_millis(100),
            None,
        ));
        tx.send(SleepTransition::Entering).await.expect("send");
        let got = recv_until(&m, "WATCHDOG_USEC=15000000").await;
        assert_eq!(
            got,
            [
                format!("WATCHDOG_USEC={}", SLEEP_WATCHDOG.as_micros()),
                "WATCHDOG_USEC=15000000".to_owned(),
            ],
            "widened, then narrowed with no Resumed ever sent"
        );
    }

    /// [SAFETY] A widen that fails to send must not cancel the narrowing an
    /// earlier widen that DID land is owed — or systemd keeps the wide value for
    /// good. The first widen lands; the second is refused (nothing listening);
    /// the first one's fallback must still narrow.
    #[tokio::test]
    async fn a_failed_widen_keeps_the_narrowing_already_due() {
        let m = Manager::new();
        let ack_dir = tempfile::tempdir().expect("ack dir");
        let ack = ack_dir.path().join(SLEEP_HOOK_ACK_FILE);
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let _task = tokio::spawn(watch_sleep(
            Arc::new(m.notifier(WATCHDOG)),
            rx,
            Duration::from_secs(1),
            Some(ack_dir.path().to_owned()),
        ));
        tx.send(SleepTransition::Entering).await.expect("send");
        recv_until(&m, &format!("WATCHDOG_USEC={}", SLEEP_WATCHDOG.as_micros())).await;

        std::fs::remove_file(&m.path).expect("unbind");
        tx.send(SleepTransition::Entering).await.expect("send");
        wait_for_ack(&ack, "pre failed\n").await;
        let rx2 = UnixDatagram::bind(&m.path).expect("rebind");
        rx2.set_nonblocking(true).expect("nonblocking");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 64];
        loop {
            match rx2.recv(&mut buf) {
                Ok(k) => {
                    assert_eq!(&buf[..k], b"WATCHDOG_USEC=15000000");
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the failed widen cancelled the narrowing the first one was owed"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("recv: {e}"),
            }
        }
    }

    /// A restore that could not be delivered is retried until it lands.
    #[tokio::test]
    async fn a_lost_restore_is_retried() {
        let m = Manager::new();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let _task = tokio::spawn(watch_sleep(
            Arc::new(m.notifier(WATCHDOG)),
            rx,
            SLEEP_WATCHDOG,
            None,
        ));
        tx.send(SleepTransition::Entering).await.expect("send");
        recv_until(&m, &format!("WATCHDOG_USEC={}", SLEEP_WATCHDOG.as_micros())).await;

        // Nothing listening: the restore fails at once rather than waiting.
        std::fs::remove_file(&m.path).expect("unbind");
        tx.send(SleepTransition::Resumed).await.expect("send");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let rx2 = UnixDatagram::bind(&m.path).expect("rebind");
        rx2.set_nonblocking(true).expect("nonblocking");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 64];
        loop {
            match rx2.recv(&mut buf) {
                Ok(k) => {
                    assert_eq!(&buf[..k], b"WATCHDOG_USEC=15000000");
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the lost restore was never retried"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("recv: {e}"),
            }
        }
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
