use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// What the panic hook gives back if the daemon panics. Populated after hardware
/// discovery, read by the panic hook.
struct PanicRestoreTargets {
    gpu_curves: Vec<(PathBuf, Option<PathBuf>)>,
    /// The hwmon hand-back ledger (DEC-382): which headers the daemon holds, and
    /// what each one gets back. Lock-free of the controller mutex by design.
    hwmon_handback: Option<Arc<HandBackLedger>>,
}

static PANIC_RESTORE: OnceLock<PanicRestoreTargets> = OnceLock::new();

/// The process's main thread — i.e. the one whose death ends the process. Used
/// to tell a fatal panic from a contained one (DEC-265).
///
/// **Captured in `fn main`, before the runtime is built** — deliberately NOT in
/// `install_panic_hook`, which is the first statement of `async_main` and so
/// runs on whatever thread happens to be polling the future. `install_panic_hook`
/// keeps a `set` of its own, but as a no-op backstop for a future direct caller;
/// `fn main` always wins the `OnceLock`. Do not "tidy" the `fn main` capture
/// away on the strength of the one in the hook.
///
/// An unset value fails **safe**: `panic_is_fatal` treats `None` as fatal, so the
/// hardware restore runs rather than being skipped.
static MAIN_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();

/// Does a panic on `current` end the process?
///
/// Only a panic on the main thread does; tokio catches one on a worker or
/// blocking thread and hands the caller a `JoinError`. Split out from the hook
/// so the decision is unit-testable — it decides whether fans are handed back
/// to firmware control, and an inverted condition here is silent (DEC-266).
///
/// Fails **safe**: an unset `main` (the hook somehow firing before
/// `install_panic_hook` finished) is treated as fatal, so the restore runs.
fn panic_is_fatal(main: Option<&std::thread::ThreadId>, current: std::thread::ThreadId) -> bool {
    main.is_none_or(|main| current == *main)
}

/// Reports the profile engine's task ending, however it ended (DEC-266).
///
/// A `Drop` guard rather than a send after the `.await`, because the case that
/// matters most — a panic inside the engine's own tick body — unwinds past any
/// such send. Dropping the future runs this; returning normally runs it too.
struct EngineDeathSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for EngineDeathSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn `fut` on the runtime and hand back a receiver that fires when it ends,
/// however it ends (DEC-266).
///
/// Exists as a function so the binding can be tested. The guard must be held in a
/// **named** local across the `.await`: written as `let _ = EngineDeathSignal(..)`
/// it would drop at construction, the receiver would be ready before the main
/// loop even started, and the daemon would restore-and-exit on its first tick —
/// a boot crash-loop that compiles and passes every other test.
fn spawn_supervised<F>(
    fut: F,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<()>,
)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _death = EngineDeathSignal(Some(tx));
        fut.await;
    });
    (handle, rx)
}

/// Why the main loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StopReason {
    /// A signal asked us to stop. Carries the signal name for the log.
    Signal(&'static str),
    /// The IPC server task ended; the daemon is useless without it.
    IpcDead,
    /// The profile engine — the sole PWM writer — ended (DEC-266).
    EngineDead,
    /// The hwmon poll loop — the sole writer of the sensor map the thermal-emergency rule
    /// reads — ended (DEC-267).
    HwmonDead,
}

/// The main loop's verdict: why it stopped, and whether systemd must restart us.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StopOutcome {
    reason: StopReason,
    must_restart: bool,
}

/// Wait until something says the daemon should stop, and decide whether that
/// warrants a restart.
///
/// [SAFETY] DEC-272 (register row 01-f). This was an inline `select!` in `main`,
/// which meant the supervision properties every ADR from DEC-266 onward depends
/// on were established by READING the code, never by test — the purest form of
/// the recurring failure CLAUDE.md names. Extracted whole, including the
/// post-loop sweep, so the decision is one testable unit rather than a shape a
/// reviewer has to re-derive.
///
/// The post-loop `try_recv` sweep is INSIDE this function on purpose. It is
/// DEC-269's own fix for a silent lost-restart bug: `select!` reports one arm,
/// but a shared root cause (blocking-pool exhaustion, OOM pressure) can end
/// several tasks in the same instant, and if the IPC arm won the race while the
/// engine had also died, the engine's death went unlogged AND `must_restart`
/// stayed false — so the process exited 0 and `Restart=on-failure` never fired.
/// Leaving that sweep at the call site would have left the one property most
/// worth pinning outside the tested unit.
///
/// `sighup`/`sigterm` are `Option` because registration is fail-soft on the real
/// path; tests pass `None` and drive the death channels directly. `ctrl_c` has no
/// such switch, and simply never fires under test.
async fn wait_for_stop(
    mut sighup: Option<tokio::signal::unix::Signal>,
    mut sigterm: Option<tokio::signal::unix::Signal>,
    ipc_dead_rx: tokio::sync::oneshot::Receiver<String>,
    engine_dead_rx: tokio::sync::oneshot::Receiver<()>,
    hwmon_dead_rx: tokio::sync::oneshot::Receiver<()>,
    mut on_reload: impl FnMut(),
) -> StopOutcome {
    tokio::pin!(ipc_dead_rx);
    tokio::pin!(engine_dead_rx);
    tokio::pin!(hwmon_dead_rx);

    let reason;
    let mut must_restart = false;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                log::info!("Received SIGINT — shutting down");
                reason = StopReason::Signal("SIGINT");
                break;
            }
            _ = async { sigterm.as_mut().expect("guarded by if predicate").recv().await }, if sigterm.is_some() => {
                log::info!("Received SIGTERM — shutting down");
                reason = StopReason::Signal("SIGTERM");
                break;
            }
            _ = async { sighup.as_mut().expect("guarded by if predicate").recv().await }, if sighup.is_some() => {
                log::info!("Received SIGHUP — reloading config");
                on_reload();
            }
            res = &mut ipc_dead_rx => {
                match res {
                    Ok(msg) => log::error!(
                        "IPC server task died unexpectedly ({msg}) — shutting down"
                    ),
                    Err(_) => log::error!(
                        "IPC server task dropped its dead-signal channel — shutting down"
                    ),
                }
                reason = StopReason::IpcDead;
                break;
            }
            // DEC-266. Reached only while the main loop is still running, and
            // shutdown is not requested until after it breaks — so the engine
            // ending here is always unexpected, never the clean-exit path.
            _ = &mut engine_dead_rx => {
                log::error!(
                    "SAFETY: the profile engine task exited unexpectedly — it is the sole \
                     PWM writer, so fan control and the thermal emergency are \
                     both gone. Restoring fans to firmware control and exiting so systemd \
                     restarts the daemon."
                );
                must_restart = true;
                reason = StopReason::EngineDead;
                break;
            }
            // DEC-267. Same reasoning one level upstream: this task is the
            // only writer of the sensor map the thermal-emergency rule reads.
            _ = &mut hwmon_dead_rx => {
                log::error!(
                    "SAFETY: the hwmon poll task exited unexpectedly — the sensor feed the \
                     thermal-emergency rule reads is frozen, so the daemon is running on \
                     readings that can no longer change. Restoring fans to firmware \
                     control and exiting so systemd restarts the daemon."
                );
                must_restart = true;
                reason = StopReason::HwmonDead;
                break;
            }
        }
    }

    // DEC-269: checked UNCONDITIONALLY, not behind `must_restart`. See the doc
    // comment — gating it there silently lost the restart DEC-266 exists to
    // produce whenever another arm won the race.
    if engine_dead_rx.try_recv().is_ok() {
        log::error!(
            "SAFETY: the profile engine task had also exited — restarting rather \
             than stopping cleanly"
        );
        must_restart = true;
    }
    if hwmon_dead_rx.try_recv().is_ok() {
        log::error!(
            "SAFETY: the hwmon poll task had also exited — restarting rather than \
             stopping cleanly"
        );
        must_restart = true;
    }

    StopOutcome {
        reason,
        must_restart,
    }
}

fn install_panic_hook() {
    // MAIN_THREAD is captured by `fn main` BEFORE `block_on`, not here. This
    // function is the first statement of `async_main`, so "the current thread" is
    // whatever thread happens to be polling the future. That is the main thread
    // today only because `block_on` polls on its caller and nothing `.await`s
    // ahead of this call — neither of which is pinned by anything. The captured id
    // gates `panic_is_fatal`, which gates the whole panic-time hardware restore:
    // capture it on a worker and a fatal main-thread panic is classified
    // "contained", so the fans are never restored. Belt-and-braces `set` here is
    // a no-op once `main` has run (OnceLock), and covers any future direct caller.
    let _ = MAIN_THREAD.set(std::thread::current().id());
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // DEC-265: only a panic that actually takes the daemon down restores
        // fans to firmware control.
        //
        // The hook fires for EVERY panic on EVERY thread, and tokio catches a
        // panicking task and hands the caller a `JoinError` — the daemon keeps
        // running. So one contained panic in a blocking write task used to reset
        // every GPU curve and every hwmon `pwm*_enable` to automatic underneath a
        // profile engine that was still alive and would re-assert its curve on
        // the next tick. Fans lurched to firmware defaults and back for no
        // reason, while the message claimed the process was aborting.
        //
        // A panic on the main thread ends the process; one on a tokio worker or
        // blocking thread does not. That is the distinction, and it is drawn on
        // thread identity rather than the thread's *name*, which tokio is free to
        // change. The trade is accepted and deliberate: a contained panic now
        // leaves the fans under daemon control.
        //
        // "Under daemon control" is only true while the daemon still has a
        // writer, so it is not this hook that makes it true — the profile engine
        // is supervised (DEC-266, see `engine_dead_rx`). A panic that kills the
        // engine task is contained by the runtime but fatal to fan control, and
        // the supervisor turns it back into a restore-and-exit. Removing that
        // supervision silently re-arms the regression this branch would
        // otherwise introduce.
        let fatal = panic_is_fatal(MAIN_THREAD.get(), std::thread::current().id());
        if !fatal {
            eprintln!(
                "PANIC on a non-main thread: contained by the runtime, so fans are \
                 left under daemon control and NOT reset to automatic"
            );
            default_hook(info);
            return;
        }
        if let Some(targets) = PANIC_RESTORE.get() {
            eprintln!("PANIC: giving fans back to firmware control before aborting");
            // 278-a: bounded. These are bare sysfs writes, so a chip that has
            // stopped acknowledging them used to block the hook and the process
            // never reached `abort()` — a panicking daemon that neither controls
            // fans nor dies. Proceed on the deadline; aborting with fans latched
            // is strictly better than hanging with fans latched, because systemd
            // can restart the former.
            if !restore_panic_targets(targets, SHUTDOWN_TASK_TIMEOUT) {
                eprintln!(
                    "  WARNING: the restore did not finish within {}s — a chip is not \
                     responding to writes. Aborting anyway so the process cannot hang; \
                     fans may be left under daemon control until something owns them again.",
                    SHUTDOWN_TASK_TIMEOUT.as_secs()
                );
            }
        }
        default_hook(info);
    }));
}

use control_ofc_daemon::api::handlers::AppState;
use control_ofc_daemon::api::server;
use control_ofc_daemon::config::DaemonConfig;
use control_ofc_daemon::daemon_state;
use control_ofc_daemon::health::cache::{StateCache, MAX_SUPERVISABLE_POLL_INTERVAL_MS};
use control_ofc_daemon::health::history::HistoryRing;
use control_ofc_daemon::health::staleness::StalenessConfig;
use control_ofc_daemon::hwmon::handback::{self, HandBackLedger, HandBackOutcome};
use control_ofc_daemon::hwmon::lease::LeaseManager;
use control_ofc_daemon::hwmon::pwm_control::{HwmonPwmController, RealSysfsWriter};
use control_ofc_daemon::hwmon::pwm_discovery::discover_pwm_headers;
use control_ofc_daemon::hwmon::HWMON_SYSFS_ROOT;
use control_ofc_daemon::profile::{self, DaemonProfile};
use control_ofc_daemon::runtime_config::{
    record_degraded, LoadPhase, RuntimeConfig, RuntimeConfigDegraded, RUNTIME_CONFIG_FILE,
};
use control_ofc_daemon::safety::ThermalSafetyRule;
use control_ofc_daemon::serial::controller::FanController;
use control_ofc_daemon::serial::real_transport::{
    enumerate_serial_candidates, RealSerialTransport,
};
use tokio::net::UnixListener;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG_PATH: &str = "/etc/control-ofc/daemon.toml";

/// Hidden dev-only flag. When passed, the daemon skips its "must run as root"
/// check. It does NOT skip any file/socket access checks — those still run
/// and will fail with an actionable error if the dev hasn't also overridden
/// the socket/state paths to user-writable locations. Not publicly documented.
const ALLOW_NON_ROOT_FLAG: &str = "--allow-non-root";

/// Return `true` if the current process is running as effective UID 0.
fn running_as_root() -> bool {
    // SAFETY: `geteuid` is thread-safe, reentrant, signal-safe, and always
    // defined on Unix targets. It reads immutable per-process kernel state
    // (effective UID) with no memory safety concerns — no pointers, no
    // allocations, no mutable references involved.
    unsafe { libc::geteuid() == 0 }
}

/// CLI flag parser for `--allow-non-root`. Separated from `parse_profile_arg`
/// so preflight can consult it before any config/profile plumbing runs.
fn parse_allow_non_root_flag() -> bool {
    std::env::args().any(|a| a == ALLOW_NON_ROOT_FLAG)
}

/// Pre-flight validation that the daemon has the permissions it needs.
///
/// Runs *before* any subsystem (polling, profile engine, hardware probes)
/// starts, so that a permission failure surfaces as one clear error instead
/// of a half-started zombie daemon with silently-broken IPC.
///
/// Performs three checks, in order:
/// 1. **EUID check** — bail out if not root, unless `--allow-non-root`.
///    hwmon / GPU / serial writes all require root regardless of file
///    permissions, so running as a regular user can't succeed anyway.
/// 2. **State directory writability** — try to create a `.writable_probe`
///    file inside `state_dir`. Catches the case where the daemon is running
///    as root but without systemd having prepared `/var/lib/control-ofc`.
/// 3. **IPC socket bind** — create the parent directory, remove any stale
///    socket from a prior crash, bind a `UnixListener`, and chmod it to
///    0o666 (DEC-049). The returned listener is handed straight to
///    `server::serve`, so there is no bind/unbind/re-bind race.
///
/// Any failure prints an actionable error to stderr and exits(1). The hint
/// always points back to `sudo systemctl enable --now control-ofc-daemon`,
/// which is the only supported way to run the daemon.
fn preflight_check(config: &DaemonConfig, allow_non_root: bool) -> UnixListener {
    // ── 1. EUID check ───────────────────────────────────────────────────
    if !running_as_root() && !allow_non_root {
        eprintln!("error: control-ofc-daemon must be run as root.");
        eprintln!();
        eprintln!("The daemon writes PWM values to /sys/class/hwmon/ and GPU fan");
        eprintln!("curves, and binds a Unix socket under /run/control-ofc/. All");
        eprintln!("of these require root privileges and the systemd-managed");
        eprintln!("runtime and state directories.");
        eprintln!();
        eprintln!("Start the daemon via systemd instead:");
        eprintln!();
        eprintln!("    sudo systemctl enable --now control-ofc-daemon");
        eprintln!();
        eprintln!("(Developers: pass {ALLOW_NON_ROOT_FLAG} and override");
        eprintln!("ipc.socket_path / state.state_dir in your config to run the");
        eprintln!("binary directly. This is not supported for end users.)");
        std::process::exit(1);
    }

    // ── 2. State directory writability ─────────────────────────────────
    let state_dir = Path::new(&config.state.state_dir);
    if let Err(e) = std::fs::create_dir_all(state_dir) {
        eprintln!(
            "error: cannot create state directory '{}': {e}",
            state_dir.display()
        );
        eprintln!();
        eprintln!("This directory is normally created by systemd via");
        eprintln!("StateDirectory=control-ofc in the unit file. Start the");
        eprintln!("daemon via:");
        eprintln!();
        eprintln!("    sudo systemctl enable --now control-ofc-daemon");
        std::process::exit(1);
    }
    let probe = state_dir.join(".writable_probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "error: permission denied writing to state directory '{}'.",
                state_dir.display()
            );
            eprintln!();
            eprintln!("The daemon must be able to persist its state file and");
            eprintln!("runtime.toml. If you started the binary directly as a");
            eprintln!("regular user, use systemd instead:");
            eprintln!();
            eprintln!("    sudo systemctl enable --now control-ofc-daemon");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "error: state directory '{}' is not writable: {e}",
                state_dir.display()
            );
            std::process::exit(1);
        }
    }

    // ── 3. IPC socket bind ─────────────────────────────────────────────
    let socket_path = Path::new(&config.ipc.socket_path);
    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "error: cannot create IPC socket directory '{}': {e}",
                parent.display()
            );
            eprintln!();
            eprintln!("This directory is normally created by systemd via");
            eprintln!("RuntimeDirectory=control-ofc. Start the daemon via:");
            eprintln!();
            eprintln!("    sudo systemctl enable --now control-ofc-daemon");
            std::process::exit(1);
        }
    }
    if socket_path.exists() {
        if let Err(e) = std::fs::remove_file(socket_path) {
            eprintln!(
                "error: failed to remove stale IPC socket '{}': {e}",
                socket_path.display()
            );
            std::process::exit(1);
        }
        log::info!("Removed stale socket: {}", socket_path.display());
    }
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            let kind = e.kind();
            eprintln!(
                "error: failed to bind IPC socket '{}': {e}",
                socket_path.display()
            );
            if kind == std::io::ErrorKind::PermissionDenied {
                eprintln!();
                eprintln!("The daemon cannot bind its IPC socket. Start it via");
                eprintln!("systemd, which prepares the runtime directory:");
                eprintln!();
                eprintln!("    sudo systemctl enable --now control-ofc-daemon");
            } else if kind == std::io::ErrorKind::AddrInUse {
                eprintln!();
                eprintln!("Another instance of control-ofc-daemon may already be");
                eprintln!("running. Check with:");
                eprintln!();
                eprintln!("    systemctl status control-ofc-daemon");
            }
            std::process::exit(1);
        }
    };
    // DEC-049: world-writable socket so non-root GUI clients can connect.
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o666))
        {
            eprintln!(
                "error: failed to chmod 0o666 on IPC socket '{}': {e}",
                socket_path.display()
            );
            std::process::exit(1);
        }
    }

    log::info!(
        "Preflight OK — state dir '{}' writable, IPC bound at '{}'",
        state_dir.display(),
        socket_path.display()
    );
    listener
}

/// The startup delay actually slept: `configured_secs`, capped at
/// [`control_ofc_daemon::constants::MAX_STARTUP_DELAY_SECS`] (DEC-387).
///
/// Both setters already refuse more — `daemon.toml` validation and
/// `POST /config/startup-delay` — but `runtime.toml` is re-read on every start
/// and a hand-edit bypasses both. Since the unit became `Type=notify` the delay
/// runs inside `TimeoutStartSec=`, whose derivation assumes this cap: an uncapped
/// delay longer than that would have systemd kill the daemon before it ever
/// reported ready, and restart it into the same delay, on every boot.
fn effective_startup_delay(configured_secs: u64) -> u64 {
    let max = control_ofc_daemon::constants::MAX_STARTUP_DELAY_SECS;
    if configured_secs > max {
        log::warn!(
            "startup.delay_secs = {configured_secs} exceeds the {max}s maximum — \
             sleeping {max}s"
        );
        max
    } else {
        configured_secs
    }
}

/// Apply runtime.toml overrides onto the in-memory `DaemonConfig`.
/// Any key present in runtime.toml shadows the admin-owned daemon.toml value.
fn apply_runtime_overlay(config: &mut DaemonConfig, runtime: &RuntimeConfig, admin_path: &str) {
    if let Some(dirs) = runtime.profile_search_dirs() {
        log::info!(
            "runtime.toml overrides [profiles] search_dirs ({} dirs)",
            dirs.len()
        );
        config.profiles.search_dirs = dirs.to_vec();
    }
    if let Some(delay) = runtime.startup_delay_secs() {
        log::info!("runtime.toml overrides [startup] delay_secs = {delay}");
        config.startup.delay_secs = delay;
    }

    // DEC-243 admin keys. These are consumed once at process start, so the API
    // that sets them reports "takes effect on restart" — this overlay is what
    // makes that true. Without it the value would persist and never apply.
    if let Some(port) = runtime.serial_port() {
        log::info!("runtime.toml overrides [serial] port = {port}");
        config.serial.port = Some(port.to_string());
    }
    if let Some(timeout) = runtime.serial_timeout_ms() {
        log::info!("runtime.toml overrides [serial] timeout_ms = {timeout}");
        config.serial.timeout_ms = timeout;
    }
    if let Some(interval) = runtime.poll_interval_ms() {
        log::info!("runtime.toml overrides [polling] poll_interval_ms = {interval}");
        config.polling.poll_interval_ms = interval;
    }
    if let Some(allow) = runtime.allow_port_probe() {
        log::info!("runtime.toml overrides [detection] allow_port_probe = {allow}");
        config.detection.allow_port_probe = allow;
    }
    if let Some(enable) = runtime.enable_nvidia_telemetry() {
        log::info!("runtime.toml overrides [detection] enable_nvidia_telemetry = {enable}");
        config.detection.enable_nvidia_telemetry = enable;
    }
    if let Some(pct) = runtime.exit_floor_pct() {
        log::info!("runtime.toml overrides [shutdown] exit_floor_pct = {pct}");
        config.shutdown.exit_floor_pct = pct;
    }

    // Sanity: if the admin config *also* has non-default runtime-mutable keys,
    // the runtime values still win — but warn so the admin knows their edits
    // are being shadowed. This catches the "admin edits daemon.toml but the
    // daemon keeps using runtime.toml" failure mode.
    if runtime.profile_search_dirs().is_some() || runtime.startup_delay_secs().is_some() {
        log::info!(
            "Runtime-mutable keys live in runtime.toml now; \
             edits to [profiles]/[startup] in {admin_path} are ignored \
             while runtime.toml exists. See docs/ADRs/002-runtime-config-split.md."
        );
    }

    // [SAFETY] DEC-270: last word on the poll cadence, after both the admin file
    // and the runtime overlay have had theirs. This is the single point where the
    // effective interval is settled — every consumer below reads the field — so
    // the clamp belongs here rather than at the six read sites.
    //
    // `daemon.toml` bounds this only as `>= 100`; the 250–2000 ms clamp lives on
    // the API route. Past `MAX_SUPERVISABLE_POLL_INTERVAL_MS` the thermal-emergency rule's
    // staleness budget stops tracking the cadence (it is capped at
    // `CPU_TEMP_STALE_CEILING_MS`), so the 5x headroom erodes towards 1x and a
    // single missed poll starts reading as stale; past the 30 s ceiling it
    // inverts and EVERY reading is stale on arrival, which silently disables the
    // ladder — it runs only on a `Fresh` reading. Clamp rather than reject:
    // refusing to boot over a config typo leaves the fans with no controller at
    // all, which is strictly worse than polling faster than the admin asked for.
    if config.polling.poll_interval_ms > MAX_SUPERVISABLE_POLL_INTERVAL_MS {
        log::warn!(
            "[polling] poll_interval_ms = {} is slower than the {} ms the \
             thermal-safety rule can supervise; clamping. Past that the \
             ladder's staleness budget stops tracking the poll cadence, so \
             ordinary readings begin to look stale and the ladder stops firing.",
            config.polling.poll_interval_ms,
            MAX_SUPERVISABLE_POLL_INTERVAL_MS,
        );
        config.polling.poll_interval_ms = MAX_SUPERVISABLE_POLL_INTERVAL_MS;
    }

    // DEC-388: `daemon.toml` validation refuses more than 100, but `runtime.toml`
    // is not re-validated. A duty above 100 % means nothing to either backend.
    if config.shutdown.exit_floor_pct > 100 {
        log::warn!(
            "[shutdown] exit_floor_pct = {} is above 100 — using 100",
            config.shutdown.exit_floor_pct
        );
        config.shutdown.exit_floor_pct = 100;
    }
}

/// Reload the daemon config and runtime overlay, updating the shared
/// profile search dirs. Extracted from the SIGHUP handler so it can be
/// unit-tested without a full AppState.
///
/// Returns the new search dirs on success, or an error string on failure.
/// Prepend the daemon-owned profile store (`{state_dir}/profiles`, DEC-160) to
/// the configured search dirs so CRUD-created profiles are always discoverable
/// by id and the store is the primary location — regardless of admin config or
/// a SIGHUP reload. Dedup-safe; otherwise order-preserving.
fn with_store_dir(mut dirs: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let store = daemon_state::profiles_dir();
    if !dirs.contains(&store) {
        dirs.insert(0, store);
    }
    dirs
}

fn apply_config_reload(
    config_path: &str,
    runtime_config_path: &Path,
    profile_search_dirs: &parking_lot::RwLock<Vec<std::path::PathBuf>>,
    degraded: &parking_lot::RwLock<Option<RuntimeConfigDegraded>>,
    cache: &StateCache,
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut new_config =
        DaemonConfig::load(config_path).map_err(|e| format!("config reload failed: {e}"))?;
    // `AUD3-m`: a reload that cannot parse `runtime.toml` re-applies DEFAULTS to
    // the running config, exactly as the boot load does. Narrower in effect —
    // only `profile_search_dirs` is committed below, so header roles keep
    // whatever boot established — but it is the same silent degradation on the
    // same surface, so it is reported rather than left in the journal.
    //
    // [SAFETY] `WIRE-ao`: **most-severe wins, not latest-wins.** This used to
    // overwrite the slot unconditionally, and the two phases do not cost the
    // same. A `startup` degradation drops every `header_roles` assignment — on a
    // board with no `pwmN_label` files that is the only evidence a header drives
    // a pump, so its 30% floor, stop exemption and pump-safe identify are all
    // gone. A `reload` degradation drops nothing: boot's roles are still in
    // force. Letting the cheaper record overwrite the expensive one made
    // `phase` under-report, so a client reading `reload` would reassure the user
    // while a hand-assigned pump was unprotected — reachable by editing a broken
    // `runtime.toml` and sending SIGHUP. GUI v2.58.0 works around it by never
    // reassuring on `reload`; this fixes it at source.
    //
    // A startup record therefore stands — and since `TS-r` so does an `update`
    // record, which says a setter replaced the file. Latest-wins is kept
    // *within* a phase, so a second failed reload still refreshes `detail` with
    // the current error rather than serving a stale one. The rule itself is
    // `runtime_config::record_degraded`.
    let (new_runtime, problem) =
        RuntimeConfig::load_from_reporting(runtime_config_path, LoadPhase::Reload);
    if let Some(problem) = problem {
        // Since `TS-r` the rule has a third phase (`update`, which also stands
        // against a reload) and a second writer, so it lives in one function.
        record_degraded(degraded, problem);
    }
    apply_runtime_overlay(&mut new_config, &new_runtime, config_path);
    let new_dirs = with_store_dir(
        new_config
            .profiles
            .search_dirs
            .iter()
            .map(std::path::PathBuf::from)
            .collect(),
    );
    log::info!("Config reloaded — profile search dirs: {:?}", new_dirs);
    *profile_search_dirs.write() = new_dirs.clone();
    // DEC-388: the exit floor applies live, so a reload re-applies it as it does
    // the search dirs.
    cache.set_exit_floor_pct(new_config.shutdown.exit_floor_pct);
    Ok(new_dirs)
}

/// Resolve the config file path.
///
/// Precedence: `--config` CLI arg > `$CONTROL_OFC_CONFIG` env var > default.
fn resolve_config_path() -> String {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--config" && i + 1 < args.len() {
            return args[i + 1].clone();
        }
        i += 1;
    }
    if let Ok(val) = std::env::var("CONTROL_OFC_CONFIG") {
        if !val.is_empty() {
            return val;
        }
    }
    DEFAULT_CONFIG_PATH.to_string()
}

/// Parse CLI arguments: --profile <name> or --profile-file <path>
fn parse_profile_arg(search_dirs: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" if i + 1 < args.len() => {
                i += 2; // skip --config and its value
                continue;
            }
            "--allow-non-root" => {
                // Handled by `parse_allow_non_root_flag` at preflight; skip here.
                i += 1;
                continue;
            }
            "--profile" if i + 1 < args.len() => {
                let name = &args[i + 1];
                return profile::find_profile(name, search_dirs).or_else(|| {
                    log::error!("Profile '{name}' not found in search paths");
                    None
                });
            }
            "--profile-file" if i + 1 < args.len() => {
                let path = std::path::PathBuf::from(&args[i + 1]);
                if path.exists() {
                    return Some(path);
                }
                log::error!("Profile file '{}' not found", path.display());
                return None;
            }
            _ => {}
        }
        i += 1;
    }

    // Check OPENFAN_PROFILE env var
    if let Ok(name) = std::env::var("OPENFAN_PROFILE") {
        if !name.is_empty() {
            return profile::find_profile(&name, search_dirs).or_else(|| {
                log::warn!("OPENFAN_PROFILE='{name}' not found in search paths");
                None
            });
        }
    }

    None
}

/// Resolve a profile from persisted daemon state, mapping **any** load failure
/// (no pointer, missing file, corrupt/invalid/hand-edited JSON) to `None`.
///
/// This is the boot-time fail-safe (DEC-165): a persisted profile that has gone
/// bad on disk must never crash startup — the daemon falls back to imperative
/// mode (no curve evaluation; only the thermal ladder writes on its own) and
/// waits for a valid profile to be activated.
/// Pure over an injected `load` fn so the fail-safe is unit-testable without the
/// real state file. The caller logs the success case (it owns the "restored"
/// message); this fn logs the warn-level failure cases.
fn resolve_persisted_profile(
    state: &daemon_state::DaemonState,
    load: impl Fn(&Path) -> Result<DaemonProfile, String>,
) -> Option<DaemonProfile> {
    let path_str = state.active_profile_path.as_ref()?;
    let path = PathBuf::from(path_str);
    if !path.exists() {
        log::warn!("Persisted profile path no longer exists: {path_str}");
        return None;
    }
    match load(&path) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("Persisted profile invalid: {e}");
            None
        }
    }
}

/// Load the initial profile from CLI, env, or persisted state.
fn resolve_initial_profile(search_dirs: &[std::path::PathBuf]) -> Option<DaemonProfile> {
    // Priority 1: CLI / env override
    if let Some(path) = parse_profile_arg(search_dirs) {
        return match profile::load_profile(&path) {
            Ok(p) => {
                // Persist the CLI choice so it survives reboot
                if let Err(e) = daemon_state::save_state(&daemon_state::DaemonState {
                    version: 1,
                    active_profile_id: Some(p.id.clone()),
                    active_profile_path: Some(path.display().to_string()),
                }) {
                    log::error!("Failed to persist CLI profile selection: {e}");
                }
                Some(p)
            }
            Err(e) => {
                log::error!("Failed to load CLI profile: {e}");
                None
            }
        };
    }

    // Priority 2: Persisted state. A corrupt/missing/hand-edited persisted
    // profile must fail SAFE to no-profile, never crash startup — see
    // `resolve_persisted_profile`.
    let state = daemon_state::load_state();
    if let Some(p) = resolve_persisted_profile(&state, profile::load_profile) {
        log::info!("Restored persisted profile: '{}'", p.name);
        return Some(p);
    }

    // Priority 3: No profile — run in pure imperative mode
    log::info!("No profile loaded — running in imperative mode (GUI-driven)");
    None
}

/// Maximum time to wait for the IPC server or a poll/engine task to stop during
/// shutdown before proceeding with the hardware restore anyway.
const SHUTDOWN_TASK_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the runtime teardown waits for outstanding blocking work before
/// giving up on it and letting the process exit (273-b).
///
/// DEC-272 made an outstanding wedged `spawn_blocking` sensor read a **designed
/// steady state** — the read is single-flighted and re-awaited, never
/// re-spawned, precisely so a wedged chip cannot exhaust the blocking pool. The
/// cost is that such a read can still be outstanding when `main` returns, and
/// tokio's `Runtime::drop` waits for spawned work *forever*
/// (<https://docs.rs/tokio/1.50.0/tokio/runtime/struct.Runtime.html>). Under
/// systemd that meant hanging until `TimeoutStopSec` SIGKILL; run from a
/// terminal it meant hanging with no backstop at all.
///
/// `shutdown_sequence` has already *attempted* the hardware restore by the time
/// this is reached, so for the case this bound exists to cover — a wedged sensor
/// READ — nothing about safety rides on the wait and only process exit does.
///
/// That is the whole of the claim, and it is narrower than it first reads. It is
/// no longer narrowed by the restore itself, though: until 2.21.1 the restore
/// took the hwmon controller lock **unbounded**, and the engine write path holds
/// that same lock across an uncancellable `spawn_blocking` sysfs write — so a
/// chip wedging mid-WRITE stalled the restore *before* control ever reached here,
/// and no timeout below it could help. `hand_back_hwmon` now bounds **both**
/// the lock acquisition and the restore writes themselves (277-b) — bounding only
/// the lock would have moved the hang to the kernel driver lock rather than
/// removing it — so by the time this timeout applies the restore has been
/// attempted and abandoned either way.
///
/// Bound the read case and let the leaked thread die with the process.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Start-up time asked of systemd on top of each boot-time serial probe's own
/// bound (DEC-387): enough for everything that follows the last probe before
/// `READY=1` — sysfs discovery, GPU detection, the opt-in NVML load. The
/// extension only ever moves the start deadline later, so a generous value
/// costs nothing but a slower kill of a start that has genuinely stopped.
const BOOT_PROBE_EXTENSION_SLACK: Duration = Duration::from_secs(30);

/// What the exit floor did (DEC-388). Returned, like [`HwmonRestore`], so the
/// outcome is testable without log capture.
#[derive(Debug, PartialEq, Eq)]
enum ExitFloor {
    /// `exit_floor_pct` is 0: every output keeps the duty it holds.
    Off,
    /// Both steps ran. `raised` outputs were written up to their exit duty and
    /// `failed` writes did not land; outputs already at or above it are in
    /// neither count.
    Done { raised: usize, failed: usize },
    /// A controller was still locked — a wedged write holds it — or a step
    /// outlived its deadline, so some outputs keep their last duty.
    Incomplete,
}

/// The exit floor (DEC-388, `TS-j`, `TS-y`): on a clean stop, every output the
/// daemon cannot give back to firmware is left at `max(its last duty, the
/// floor)`, or at full speed where that duty is unknown. That is each OpenFan
/// channel the daemon has written — serial, no firmware curve, so it holds
/// whatever a stop leaves it at — and each hwmon header with no `pwmN_enable`.
/// Outputs the daemon never wrote are left alone.
///
/// **Runs FIRST in the restore closure, and that is load-bearing.** Since
/// DEC-388 a watchdog timeout sends SIGTERM rather than SIGABRT, so a hung loop
/// takes this path too — but under `TimeoutAbortSec=10`, not the ordinary
/// `TimeoutStopSec`.
/// The engine drain (3 s) plus these two steps (at most 3 s each) fit inside
/// that when the hung engine is the only task that will not drain; a deadlock
/// that also stalls the IPC server or a poll task costs up to 3 s more per
/// stuck drain, and SIGKILL can then land before this runs. The GPU reset and
/// hwmon hand-back that follow may be cut too, but `ExecStopPost` repeats both.
/// It cannot repeat this: serial is out of its reach, and it has no record of a
/// no-mode header's duty.
///
/// **Taking a controller's lock is not proof the engine is done with it.** A
/// final batch that outlived the drains locks per channel or header (DEC-099,
/// DEC-154), so this step can run between two of its writes and the rest of the
/// batch lands after it. That is harmless because the floor latches:
/// `FanController::set_pwm` raises anything lower to it — which also covers a
/// calibration sweep still running inside an HTTP request that outlived the
/// server drain — and a header with no mode switch is latched the same way in
/// `HwmonPwmController` (DEC-392), so an engine write or a verify restore that
/// outlived the drains cannot lower it either.
///
/// Bounded like its siblings: each backend's lock wait and its writes run on a
/// detached thread under `step_timeout`, one step per backend so a wedged serial
/// link cannot keep the headers from being reached.
fn apply_exit_floor(
    openfan: Option<Arc<Mutex<FanController>>>,
    hwmon: Option<Arc<Mutex<HwmonPwmController>>>,
    floor_pct: u8,
    step_timeout: Duration,
) -> ExitFloor {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    if floor_pct == 0 {
        log::info!(
            "exit floor is 0 — OpenFan channels and headers with no mode switch keep \
             the duty they hold"
        );
        return ExitFloor::Off;
    }
    let raised = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let complete = Arc::new(AtomicBool::new(true));

    if let Some(ctrl) = openfan {
        let (r, f, c) = (raised.clone(), failed.clone(), complete.clone());
        let finished = run_bounded("exit-floor-openfan", step_timeout, move || {
            let Some(mut guard) = ctrl.try_lock_for(step_timeout) else {
                c.store(false, SeqCst);
                log::error!(
                    "OpenFan controller still locked after {}s — its channels keep their \
                     last duty",
                    step_timeout.as_secs()
                );
                return;
            };
            for w in guard.apply_exit_floor(floor_pct) {
                match &w.result {
                    Ok(done) if done.coalesced => {}
                    Ok(_) => {
                        r.fetch_add(1, SeqCst);
                        log::info!(
                            "OpenFan channel {}: left at {} % on stop (was {})",
                            w.channel,
                            w.target_pct,
                            w.was_pct
                                .map_or("unknown".to_string(), |p| format!("{p} %"))
                        );
                    }
                    Err(e) => {
                        f.fetch_add(1, SeqCst);
                        log::error!(
                            "OpenFan channel {}: the {} % exit floor did not land ({e}) — it \
                             keeps its last duty",
                            w.channel,
                            w.target_pct
                        );
                    }
                }
            }
        });
        if !finished {
            complete.store(false, SeqCst);
            log::error!(
                "OpenFan exit floor did not finish within {}s — the link is not \
                 responding; channels not yet written keep their last duty",
                step_timeout.as_secs()
            );
        }
    }

    if let Some(ctrl) = hwmon {
        let (r, f, c) = (raised.clone(), failed.clone(), complete.clone());
        let finished = run_bounded("exit-floor-hwmon", step_timeout, move || {
            let Some(mut guard) = ctrl.try_lock_for(step_timeout) else {
                c.store(false, SeqCst);
                log::error!(
                    "hwmon controller still locked after {}s — headers with no mode switch \
                     keep their last duty",
                    step_timeout.as_secs()
                );
                return;
            };
            for w in guard.apply_exit_floor(floor_pct) {
                match &w.result {
                    None => {}
                    Some(Ok(())) => {
                        r.fetch_add(1, SeqCst);
                        log::info!(
                            "hwmon {}: left at {} % on stop (was {}) — it has no mode to be \
                             given back",
                            w.header_id,
                            w.target_pct,
                            w.was_pct
                                .map_or("unknown".to_string(), |p| format!("{p} %"))
                        );
                    }
                    Some(Err(e)) => {
                        f.fetch_add(1, SeqCst);
                        log::error!(
                            "hwmon {}: the {} % exit floor did not land ({e}) — it keeps its \
                             last duty",
                            w.header_id,
                            w.target_pct
                        );
                    }
                }
            }
        });
        if !finished {
            complete.store(false, SeqCst);
            log::error!(
                "hwmon exit floor did not finish within {}s — a chip is not responding \
                 to writes",
                step_timeout.as_secs()
            );
        }
    }

    if !complete.load(SeqCst) {
        return ExitFloor::Incomplete;
    }
    ExitFloor::Done {
        raised: raised.load(SeqCst),
        failed: failed.load(SeqCst),
    }
}

/// What the hwmon hand-back actually did.
///
/// Returned rather than only logged so the benign/real distinction below is
/// testable as an **outcome**: this repo has no log capture, and installing a
/// global logger to assert a level would be process-wide and fragile.
#[derive(Debug, PartialEq, Eq)]
enum HwmonRestore {
    /// No hwmon controller on this machine — nothing to do.
    NoController,
    /// Gave this many headers back — to their recorded mode, or to full speed
    /// where that could not be done — and could write nothing to `failed` more.
    HandedBack { released: usize, failed: usize },
    /// The daemon holds no header. **Benign**, and the common case at a clean
    /// stop after a force or profile ended: DEC-382 gives headers back as soon as
    /// nothing holds them. It also covers a board whose headers have no
    /// `pwmN_enable` (legacy nct67xx revisions), which the daemon never switches
    /// out of firmware control in the first place.
    NothingTaken,
    /// The ledger's lock could not be taken in time. Nothing in the ledger does
    /// I/O beyond a tmpfs write, so this should not happen — and `ExecStopPost`
    /// replays the on-disk record either way.
    Unresolvable,
    /// The writes did not finish within the deadline — a chip is not responding.
    WritesTimedOut(usize),
}

/// Run `f` on a detached thread and wait at most `timeout` for it to finish.
/// `true` = it completed; `false` = the deadline passed (or the thread could not
/// be started). The thread is left running and dies with the process.
///
/// **This is the only way to bound a sysfs write.** A `std::fs::write` that has
/// wedged in the kernel cannot be cancelled or interrupted, so the sole remaining
/// lever is to stop *waiting* for it — the same trade `shutdown_timeout` makes
/// for the wedged-read case (DEC-275).
///
/// Extracted (DEC-279) because three hardware-restore paths need exactly this
/// shape and only one of them had it: `hand_back_hwmon` (277-b, fixed in
/// 2.21.1), the shutdown closure's GPU reset (278-c) and the panic hook (278-a).
/// Two near-copies of a safety bound is how the second one ends up missing.
///
/// `Builder::spawn` rather than `thread::spawn`, deliberately: the panic hook
/// calls this **during a panic**, and `thread::spawn` *panics* if the OS refuses
/// the thread — a panic inside a panic hook aborts immediately, turning a
/// recoverable restore failure into a hard abort with no hardware handed back.
/// A refused spawn reports `false` here instead, which lands the caller on the
/// same "did not complete" branch as a timeout. The two are not distinguished
/// because the operator's situation is identical: the writes did not land, and
/// the process must proceed anyway.
fn run_bounded<F>(name: &str, timeout: Duration, f: F) -> bool
where
    F: FnOnce() + Send + 'static,
{
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("restore-{name}"))
        .spawn(move || {
            f();
            let _ = done_tx.send(());
        });
    if spawned.is_err() {
        return false;
    }
    done_rx.recv_timeout(timeout).is_ok()
}

/// Reset every AMD GPU fan curve to automatic (PMFW `fan_curve` `r` then `c`) —
/// **bounded**, so a wedged PMFW write cannot stall shutdown (278-c).
///
/// The same hazard and the same remedy as `hand_back_hwmon`, one device
/// class over. `gpu_fan::reset_to_auto` is two bare `std::fs::write` calls, and
/// amdgpu serialises PMFW attribute stores on the device — so a card that has
/// stopped acknowledging them blocks the write for as long as it stays wedged.
///
/// This runs **first** in the shutdown closure, which is what made it matter:
/// until 2.22.0 it could block before the already-bounded hwmon restore was ever
/// reached, so bounding that one alone left the process just as stuck. Exactly
/// 277-b's harm on a different device.
///
/// Bounded here at the call site rather than inside `gpu_fan::reset_to_auto`,
/// which stays a plain synchronous write. That function has a non-shutdown
/// caller (`POST /gpu/{id}/fan/reset`) for which a deadline would be wrong: a
/// slow reset there must report an error to the client, not return `Ok` while
/// the write is still outstanding.
///
/// **Promises exactly what its hwmon sibling promises**, and no more: this STEP
/// returns. It does not promise the GPU is back under PMFW control — if the card
/// is unresponsive nothing can restore it, and no doc may tighten that.
fn restore_gpu_fans_to_auto(
    curves: Vec<(PathBuf, Option<PathBuf>)>,
    write_timeout: Duration,
) -> bool {
    if curves.is_empty() {
        return true;
    }
    let count = curves.len();
    let completed = run_bounded("gpu", write_timeout, move || {
        for (curve_path, zero_rpm_path) in &curves {
            match control_ofc_daemon::hwmon::gpu_fan::reset_to_auto(
                curve_path,
                zero_rpm_path.as_deref(),
            ) {
                Ok(()) => log::info!("GPU fan curve {} reset to auto", curve_path.display()),
                Err(e) => log::warn!("GPU fan curve {} reset failed: {e}", curve_path.display()),
            }
        }
    });
    if !completed {
        log::error!(
            "GPU fan reset did not finish within {}s — a GPU is not responding to PMFW \
             writes. Proceeding with shutdown so the process can exit; up to {count} GPU \
             fan curve(s) may be left under daemon control until something owns them again.",
            write_timeout.as_secs()
        );
    }
    completed
}

/// The panic hook's last-resort hardware restore — **bounded** (278-a).
///
/// Carries the same unbounded-write shape 277-b fixed in the shutdown path and
/// 278-c fixed for GPUs: bare `std::fs::write` calls to GPU `fan_curve` and
/// hwmon `pwm*_enable`, no deadline. A chip that has stopped acknowledging
/// writes blocked the hook, so the process never reached `abort()` — a panicking
/// daemon that neither controls fans nor dies.
///
/// **Lower severity than 277-b, which is why this is a deadline and not a
/// restructure**, and all three reasons were verified rather than assumed: this
/// runs only on a *fatal* panic (`panic_is_fatal` returns early otherwise), it is
/// already the last-resort path rather than the routine one, and it correctly
/// takes no lock at all. There is nothing here to move out from under a mutex —
/// only a wait to bound.
///
/// `eprintln!` rather than `log::`, matching the rest of the hook: a panic hook
/// must not depend on a logger that may itself be mid-panic, or not yet
/// installed.
fn restore_panic_targets(targets: &'static PanicRestoreTargets, timeout: Duration) -> bool {
    run_bounded("panic", timeout, move || {
        for (curve_path, zero_rpm_path) in &targets.gpu_curves {
            if let Err(e) = std::fs::write(curve_path, "r\n") {
                eprintln!(
                    "  WARNING: failed to reset GPU curve {}: {e}",
                    curve_path.display()
                );
            }
            if let Err(e) = std::fs::write(curve_path, "c\n") {
                eprintln!(
                    "  WARNING: failed to commit GPU curve {}: {e}",
                    curve_path.display()
                );
            }
            if let Some(zrp) = zero_rpm_path {
                if let Err(e) = std::fs::write(zrp, "1\n") {
                    eprintln!(
                        "  WARNING: failed to re-enable zero-RPM {}: {e}",
                        zrp.display()
                    );
                }
                if let Err(e) = std::fs::write(zrp, "c\n") {
                    eprintln!(
                        "  WARNING: failed to commit zero-RPM {}: {e}",
                        zrp.display()
                    );
                }
            }
        }
        // DEC-382: give back exactly what was taken, never a hardcoded `2`. A
        // bounded wait on the ledger — this thread may be the one holding it, and
        // parking_lot's mutex is not re-entrant — and on a miss, nothing: the
        // process is about to abort, and `ExecStopPost` replays the record.
        let Some(ledger) = &targets.hwmon_handback else {
            return;
        };
        let Some(taken) = ledger.try_taken(Duration::from_millis(200)) else {
            eprintln!(
                "  WARNING: hwmon hand-back ledger is locked; leaving the headers to \
                 ExecStopPost, which replays its record"
            );
            return;
        };
        for header in taken {
            let outcome = handback::hand_back(
                &mut RealSysfsWriter,
                &header.enable_path,
                &header.pwm_path,
                header.action,
            );
            if outcome != HandBackOutcome::Restored {
                eprintln!("  WARNING: hwmon {} handed back as {outcome:?}", header.id);
            }
        }
    })
}

/// Give every hwmon header the daemon holds back to what it was doing before the
/// daemon first took it (DEC-382) — **bounded**, so a wedged chip cannot stall
/// shutdown (277-b).
///
/// Until DEC-382 this wrote a hardcoded `pwm_enable=2` to every discovered
/// header, taken or not. `2` is automatic only on `it87`: it selects Thermal
/// Cruise on `nct6775` (the BIOS usually leaves Smart Fan IV, `5`) and puts an
/// `nzxt-kraken3` pump on a 0 % curve. It now replays the hand-back ledger:
/// only headers the daemon took, each to its recorded value, confirmed by
/// reading it back, with `fancontrol`'s full-speed fallback where that fails.
///
/// **Both halves of 277-b stay bounded.** The ledger is read with a deadline —
/// it has its own lock, deliberately apart from the controller mutex a wedged
/// write can hold for good, so this no longer needs that mutex at all — and the
/// writes run on a detached thread under `run_bounded`, because a restore write
/// to a wedged Super-I/O chip blocks on the same kernel driver lock the stuck
/// write holds. The leaked thread dies with the process (DEC-275).
///
/// **What this does and does not promise.** It guarantees *this step* returns.
/// It does not guarantee the hardware is back: a chip that accepts nothing is
/// reported as `failed`, and a chip that never answers times the step out. The
/// on-disk record still names those headers, so `ExecStopPost` tries again.
/// Keep the hand-back record in the unit's runtime directory (DEC-382), where
/// `control-ofc-restore-auto` — `ExecStopPost` — replays it after a crash.
///
/// `$RUNTIME_DIRECTORY` is what systemd sets for `RuntimeDirectory=control-ofc`;
/// the literal is the same directory for a daemon run by hand. Without one the
/// daemon still gives headers back itself on every exit it survives — only the
/// crash backstop is lost, and the log says so once, here.
fn keep_handback_record(ledger: &Arc<HandBackLedger>) {
    let dir = runtime_dir();
    if dir.is_dir() {
        ledger.set_record_path(dir.join(handback::RECORD_FILE_NAME));
    } else {
        log::warn!(
            "no runtime directory at {} — the hwmon hand-back record is not kept, so \
             after a crash ExecStopPost cannot give back the headers the daemon holds",
            dir.display()
        );
    }
}

/// The unit's runtime directory: `$RUNTIME_DIRECTORY`, which systemd sets for
/// `RuntimeDirectory=control-ofc`, or the same literal for a daemon run by hand.
fn runtime_dir() -> PathBuf {
    std::env::var_os("RUNTIME_DIRECTORY")
        .and_then(|v| {
            // systemd separates several directories with ':'; this unit has one.
            v.to_str()
                .and_then(|s| s.split(':').next())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/run/control-ofc"))
}

/// Answer the `system-sleep` hook (`TS-ao`, DEC-396): `SIGUSR1` before a sleep,
/// `SIGUSR2` after it, each turned into a [`SleepTransition`] for
/// `sd_notify::watch_sleep`, which widens and restores the watchdog.
///
/// Only once both handlers are registered is the PID file written, and the hook
/// signals only a PID that file names — `SIGUSR1`'s default action terminates the
/// process, so a daemon that cannot handle it (an older one still running after
/// an upgrade, or this one mid-start) must never receive it. Registration is
/// fail-soft like SIGHUP's: without it the hook finds no file and does nothing,
/// which is the pre-DEC-396 behaviour.
fn start_sleep_watch(notifier: &Arc<control_ofc_daemon::sd_notify::Notifier>) {
    use control_ofc_daemon::sd_notify::{self, SleepTransition};
    use tokio::signal::unix::{signal, SignalKind};

    let (usr1, usr2) = match (
        signal(SignalKind::user_defined1()),
        signal(SignalKind::user_defined2()),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            log::warn!(
                "could not register the sleep hook's signals ({e}); the watchdog will not \
                 be widened across a system sleep"
            );
            return;
        }
    };
    let dir = runtime_dir();
    let (tx, rx) = tokio::sync::mpsc::channel::<SleepTransition>(4);
    tokio::spawn(forward_sleep_signals(usr1, usr2, tx));
    tokio::spawn(sd_notify::watch_sleep(
        Arc::clone(notifier),
        rx,
        sd_notify::SLEEP_WATCHDOG,
        Some(dir.clone()),
    ));
    let pid_file = dir.join(sd_notify::SLEEP_HOOK_PID_FILE);
    if let Err(e) = std::fs::write(&pid_file, format!("{}\n", std::process::id())) {
        log::warn!(
            "could not write {} ({e}); the sleep hook will not widen the watchdog",
            pid_file.display()
        );
    }
}

/// Turn the sleep hook's two signals into transitions — in arrival order, except
/// that when both are pending at once the resume goes first (`TS-az`). Ends
/// when `watch_sleep` has gone, or when both signal streams have.
async fn forward_sleep_signals(
    mut usr1: tokio::signal::unix::Signal,
    mut usr2: tokio::signal::unix::Signal,
    tx: tokio::sync::mpsc::Sender<control_ofc_daemon::sd_notify::SleepTransition>,
) {
    use control_ofc_daemon::sd_notify::SleepTransition;
    loop {
        // `TS-az`, DEC-402: `biased;`, resume arm FIRST. When both signals are
        // pending, the daemon cannot tell which came first: tokio merges repeats
        // and keeps no order across signals. A pre and a post from one sleep are
        // then applied post-then-pre, which leaves the watchdog wide until
        // `watch_sleep`'s fallback narrows it (bounded, it only delays hang
        // detection). A post followed by the NEXT sleep's pre is applied in its
        // true order and stays wide across that sleep. Sleep-first would get the
        // first case right and leave the watchdog narrow across the next sleep
        // in the second — `TS-ao`'s failure, which the hook exists to prevent.
        let transition = tokio::select! {
            biased;
            Some(()) = usr2.recv() => SleepTransition::Resumed,
            Some(()) = usr1.recv() => SleepTransition::Entering,
            else => return,
        };
        if tx.send(transition).await.is_err() {
            return;
        }
    }
}

fn hand_back_hwmon(
    ledger: Option<&Arc<HandBackLedger>>,
    lock_timeout: Duration,
    write_timeout: Duration,
) -> HwmonRestore {
    let Some(ledger) = ledger else {
        return HwmonRestore::NoController;
    };
    let Some(taken) = ledger.try_taken(lock_timeout) else {
        log::error!(
            "hwmon hand-back ledger was still locked after {}s — leaving the headers \
             the daemon holds to ExecStopPost",
            lock_timeout.as_secs()
        );
        return HwmonRestore::Unresolvable;
    };
    if taken.is_empty() {
        log::debug!("the daemon holds no hwmon header — nothing to give back");
        return HwmonRestore::NothingTaken;
    }

    let count = taken.len();
    let released = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (released_w, failed_w) = (released.clone(), failed.clone());
    let completed = run_bounded("hwmon", write_timeout, move || {
        for header in taken {
            let outcome = handback::hand_back(
                &mut RealSysfsWriter,
                &header.enable_path,
                &header.pwm_path,
                header.action,
            );
            match outcome {
                HandBackOutcome::Restored => {
                    log::info!("hwmon {} handed back to its recorded mode", header.id)
                }
                HandBackOutcome::FullSpeed => log::warn!(
                    "hwmon {}: its recorded mode could not be given back, so it was left \
                     at FULL SPEED",
                    header.id
                ),
                HandBackOutcome::Failed => log::error!(
                    "hwmon {}: could not be handed back — nothing could be written; it \
                     stays at its last duty in manual mode",
                    header.id
                ),
            }
            // [SAFETY] Deliberately NOT struck from the record (DEC-382 review,
            // concurrency F1). This path cannot take the controller mutex, so a
            // write can still land after it — an undrained engine write, a
            // detached sweep past its shutdown check — and one to a header given
            // back as `Manual(raw)` or the 1+255 fallback reads `pwm_enable=1`,
            // which the watchdog does not call a reclaim: it moves the duty with no
            // new take. On the record, ExecStopPost gives it back again; replaying
            // a header already given back is idempotent. Only the runtime
            // give-back, which holds the controller lock, strikes a header.
            if outcome.released() {
                released_w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            } else {
                failed_w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    });

    if !completed {
        log::error!(
            "hwmon hand-back did not finish within {}s — a chip is not responding to \
             writes. Proceeding with shutdown so the process can exit; ExecStopPost \
             will try the {} header(s) still on the record.",
            write_timeout.as_secs(),
            count
        );
        return HwmonRestore::WritesTimedOut(count);
    }
    HwmonRestore::HandedBack {
        released: released.load(std::sync::atomic::Ordering::SeqCst),
        failed: failed.load(std::sync::atomic::Ordering::SeqCst),
    }
}

/// Ordered graceful shutdown (DEC-146 P3-9 + audit P1-A).
///
/// Stops accepting IPC connections and drains in-flight requests FIRST, then
/// runs `after_server_stop` (DEC-402), then drains the poll/engine tasks, then
/// gives the hardware back — so
/// neither a late client write (via the IPC server) nor an in-flight engine
/// write can land after the restore and leave fans stuck in manual mode. Every
/// await is bounded by `task_timeout` so a hung task or a lingering connection
/// (e.g. a slow client holding a request open) can never block the safety restore; on timeout
/// we log and proceed. `ExecStopPost=control-ofc-restore-auto` backstops
/// production once the process has exited, whatever ended it — but only once it
/// HAS exited; see the note at the foot of this comment.
///
/// The engine task drains its backend writes before it ends, so draining its
/// task handle here also drains those writes — a blocking write cannot be left
/// in flight once the handle resolves. **That is no longer free.** Until
/// DEC-289 it held because the loop `.await`-joined every `spawn_blocking` write
/// unconditionally and so could not end a tick with one outstanding; bounding
/// those joins made it possible, and the guarantee is now restored explicitly by
/// the post-loop `drain_writes` in `profile_engine_loop`. If that drain is ever
/// removed, this paragraph becomes false and the restore below starts racing a
/// detached write that still holds the controller lock.
/// The only residual window is a single sysfs/serial write that hangs past
/// `task_timeout` (a running `spawn_blocking` cannot be cancelled). The restore
/// no longer *blocks* on that case: **both** of its steps are bounded — the GPU
/// `fan_curve` reset (278-c) and the hwmon hand-back, the latter on its lock
/// acquisition as well as its writes (277-b) — so the restore is attempted, and
/// abandoned on a deadline, whether or not the wedged write ever returns.
/// Bounding only one of the two achieved nothing while the other ran first,
/// which is why they now share `run_bounded` rather than each carrying a
/// hand-rolled deadline (DEC-279).
///
/// The `ExecStopPost` backstop remains, and it runs after every exit — the
/// `Restart=on-failure` path included: systemd runs it before scheduling the
/// restart (278-b said otherwise; measured false on systemd 261, `TS-k`, DEC-387).
/// What it cannot do is end a stall. It runs only once the process has exited,
/// so a restore that never returned would hold it off for good — which is what
/// the two bounded steps above prevent. Since DEC-387 a self-stop also sends
/// `STOPPING=1`, so systemd's `TimeoutStopSec=` bounds that path as well.
async fn shutdown_sequence<G, F>(
    poll_shutdown_tx: &tokio::sync::watch::Sender<bool>,
    server_shutdown_tx: tokio::sync::oneshot::Sender<()>,
    server_handle: tokio::task::JoinHandle<()>,
    task_handles: Vec<(&'static str, tokio::task::JoinHandle<()>)>,
    task_timeout: Duration,
    after_server_stop: G,
    restore_hardware: F,
) where
    G: FnOnce(),
    F: FnOnce(),
{
    // Tell the poll/engine tasks to stop.
    let _ = poll_shutdown_tx.send(true);

    // Stop the IPC server FIRST (audit P1-A). axum's graceful shutdown stops
    // accepting new connections immediately and drains in-flight requests, so no
    // client write can re-enter manual mode after the restore below. Bounded so a
    // lingering long-lived connection cannot block the safety restore.
    let _ = server_shutdown_tx.send(());
    if tokio::time::timeout(task_timeout, server_handle)
        .await
        .is_err()
    {
        log::warn!(
            "IPC server did not stop within {}s; proceeding with hardware restore",
            task_timeout.as_secs()
        );
    }

    // After the server, before the drains (DEC-402, `TS-ay`): the drains can run
    // out several task timeouts between them, so anything that must not wait
    // for them goes here.
    after_server_stop();

    // Drain the poll/engine tasks (DEC-146 P3-9) so an in-flight engine
    // spawn_blocking write cannot land after the restore — in the UNCONTENDED
    // case. When a write is wedged this drain times out rather than draining it
    // (a `spawn_blocking` cannot be cancelled), so the guarantee below is
    // conditional, not absolute. See `hand_back_hwmon` for the residual.
    for (name, handle) in task_handles {
        if tokio::time::timeout(task_timeout, handle).await.is_err() {
            log::warn!(
                "{name} task did not stop within {}s; proceeding with hardware restore",
                task_timeout.as_secs()
            );
        }
    }

    // Restore hardware to automatic — the last writer whenever the drain above
    // actually drained. If it timed out, a wedged engine write is still
    // outstanding and can land after this; that residual is documented on
    // `hand_back_hwmon` and in DEC-278, and must not be re-stated here as a
    // guarantee.
    restore_hardware();
}

/// `shutdown_sequence`, then the restart-forcing exit — deliberately ONE unit.
///
/// The ordering is the safety property (DEC-266/267): the hardware must be back
/// under firmware control *before* the process goes away, because exiting first
/// leaves fans latched at whatever duty the dead engine last wrote. Splitting
/// that across two statements in `async_main` left it unpinnable —
/// `std::process::exit` cannot be observed in-process, so deleting the exit (or
/// hoisting it above the restore) kept the whole suite green and silently
/// discarded DEC-266/267's point. Bundling both halves here gives the ordering a
/// single testable unit; `must_restart_exits_nonzero_after_restoring_hardware`
/// re-executes this very function in a child process and observes both.
///
/// Diverges (never returns) when `must_restart` is set.
// Every argument is a distinct piece of the stop the caller owns; bundling them
// into a struct would only add indirection (same call as `profile_engine_loop`).
#[allow(clippy::too_many_arguments)]
async fn finish_shutdown<F>(
    notifier: Option<&control_ofc_daemon::sd_notify::Notifier>,
    poll_shutdown_tx: &tokio::sync::watch::Sender<bool>,
    server_shutdown_tx: tokio::sync::oneshot::Sender<()>,
    server_handle: tokio::task::JoinHandle<()>,
    task_handles: Vec<(&'static str, tokio::task::JoinHandle<()>)>,
    task_timeout: Duration,
    must_restart: bool,
    restore_hardware: F,
) where
    F: FnOnce(),
{
    // [SAFETY] DEC-387: FIRST, before the engine is told to stop. systemd re-arms
    // its watchdog on any keep-alive whatever the unit's state, and the engine
    // completes ticks until `shutdown_sequence` stops it — so without this, one
    // late tick would arm a fresh `WatchdogSec` timer over the hardware restore
    // below, and a restore slower than that would be killed part-way through
    // handing the fans back (`WatchdogSignal`, then SIGKILL after
    // `TimeoutAbortSec`). `stopping` disarms the watchdog for good.
    if let Some(n) = notifier {
        n.stopping();
    }

    shutdown_sequence(
        poll_shutdown_tx,
        server_shutdown_tx,
        server_handle,
        task_handles,
        task_timeout,
        move || {
            // [SAFETY] `TS-ay`, DEC-402: if the disarm above never reached
            // systemd, the last keep-alive's deadline is still armed, and the
            // task drains that follow can run out several `task_timeout`s
            // between them — together longer than what is left of that deadline.
            // Send it again now, before any of them. It lands if systemd has read
            // its queue since the first attempt. On a normal stop the server
            // stops within milliseconds, so this is only ~100 ms later: it
            // narrows the window rather than closing it (`TS-bd`). A no-op once
            // it has landed.
            if let Some(n) = notifier {
                n.resend_disarm_if_lost();
            }
        },
        move || {
            // [SAFETY] `TS-ap`: and once more as late as possible before the
            // restore it protects, in case the queue was still full just now.
            // The drains give it more time to be read. A no-op when an earlier
            // attempt landed.
            if let Some(n) = notifier {
                n.resend_disarm_if_lost();
            }
            restore_hardware();
        },
    )
    .await;

    log::info!("control-ofc-daemon v{VERSION} stopped");

    // DEC-266/267. Deliberately after `shutdown_sequence`, so the hardware is
    // back under firmware control before the process goes away: exiting first
    // would leave fans latched at whatever duty the dead engine last wrote.
    // Non-zero so `Restart=on-failure` brings the daemon back with a live engine
    // and a live sensor feed — a clean exit here would look like a requested
    // stop and systemd would leave the machine with no fan control.
    //
    // Exiting from inside the async body is also what skips the runtime drop, so
    // a restart is never delayed by RUNTIME_SHUTDOWN_TIMEOUT (273-b).
    if must_restart {
        std::process::exit(1);
    }
}

/// Real entry point: owns the runtime so its teardown can be **bounded**.
///
/// Deliberately not `#[tokio::main]`. That attribute expands to a runtime held
/// as a temporary, dropped when `main` returns — and that drop blocks forever on
/// an outstanding blocking task, which DEC-272 makes a designed steady state.
/// See `RUNTIME_SHUTDOWN_TIMEOUT`.
///
/// The `must_restart` path in `finish_shutdown` calls `std::process::exit(1)`
/// from inside the async body, so it never reaches this teardown at all. That is
/// intentional: a restart must not be delayed by waiting on the very read that
/// wedged, and exiting from inside the async body is what skips the drop.
fn main() {
    // Capture the main thread's identity HERE, before the runtime exists, so it
    // cannot be recorded on a tokio worker. `panic_is_fatal` compares against it
    // to decide whether a panic ends the process — and therefore whether the
    // hardware restore runs at all.
    let _ = MAIN_THREAD.set(std::thread::current().id());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime");

    // Caught, not propagated, so the teardown below runs on the panic path too.
    // Without this a panic on the main thread unwinds straight past
    // `shutdown_timeout` and DROPS the runtime instead — the unbounded wait this
    // whole function exists to remove, restored precisely when things are already
    // going wrong. Reachable from `wait_for_stop`'s expects, `apply_config_reload`
    // and the restore closure; neither crate sets `panic = "abort"`, so unwinding
    // is live. The panic hook has already handed the hardware back by this point.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async_main());
    }));

    // Unblocks this thread after at most RUNTIME_SHUTDOWN_TIMEOUT, leaking any
    // still-running blocking task rather than waiting on it. The leaked thread
    // dies with the process. See that constant's doc comment for what this does
    // and does NOT bound — deliberately not restated here, because the sentence
    // that used to live on this line ("the hardware was restored before we got
    // here") is the claim the constant was corrected to stop making: the restore
    // is *attempted* before this point, it can fail, and on a mid-write wedge it
    // may not have completed at all (DEC-278: the restore guarantees the process
    // exits, not that the hardware was handed back).
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);

    // Re-raise so the process still exits non-zero and systemd still restarts it.
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

async fn async_main() {
    install_panic_hook();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("control-ofc-daemon v{VERSION} starting");

    // DEC-387 (`TS-d`): systemd's notification channel — `None` when not run by
    // a unit that asked for one, which makes every use of it below a no-op.
    let notifier = control_ofc_daemon::sd_notify::Notifier::from_env().map(Arc::new);
    if let Some(n) = notifier.as_deref() {
        match n.watchdog() {
            Some(timeout) => log::info!(
                "systemd watchdog: {}s, kept alive by each completed engine tick",
                timeout.as_secs_f64()
            ),
            None => log::info!("systemd notification socket present; no watchdog configured"),
        }
    }

    let config_path = resolve_config_path();
    log::info!("Config path: {config_path}");

    let mut config = match DaemonConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to load config: {e}");
            std::process::exit(1);
        }
    };

    log::info!(
        "Config loaded — poll {}ms, serial {:?}",
        config.polling.poll_interval_ms,
        config.serial.port.as_deref().unwrap_or("auto-detect"),
    );

    // Init state directory from config (must happen before any state load/save)
    daemon_state::init_state_dir(&config.state.state_dir);

    // AIO-MB Phase 5 (§15): any validation session still marked `recording` in a
    // file belongs to a process that died — a crash, a SIGKILL, or an ordinary
    // restart that beat the finaliser. Mark it `interrupted` and record where the
    // evidence actually stopped. **Nothing is fabricated for the gap**; the
    // record simply ends at the last sample that was really taken.
    {
        let repaired = control_ofc_daemon::validation::store::sweep_interrupted(
            &control_ofc_daemon::daemon_state::validation_dir(),
            "daemon_restart",
        );
        if !repaired.is_empty() {
            log::info!(
                "Marked {} interrupted validation session(s) from a previous run",
                repaired.len()
            );
        }
        // Reconcile retention at boot too, not only when a session stops
        // (`prune_default`'s other caller). A session written by a daemon that
        // predates the DEC-320 byte budget is over the store's read cap, so it
        // is invisible to every normal path and retention can never reach it —
        // and stopping a *new* session was the only thing that would have
        // reclaimed it. A user who simply upgrades and never runs another
        // validation would have kept the orphaned file for ever, which is the
        // half of `AUD3-i` that the store-side fix alone does not close.
        control_ofc_daemon::validation::store::prune_default();
    }
    log::info!("State directory: {}", config.state.state_dir);

    // Load runtime.toml from state_dir and merge. Keys present in runtime.toml
    // shadow the admin-owned daemon.toml (NetworkManager-intern pattern — ADR-002).
    let runtime_config_path =
        std::path::PathBuf::from(&config.state.state_dir).join(RUNTIME_CONFIG_FILE);
    // [SAFETY] `AUD3-m`: capture *why* a load fell back to defaults, not just
    // the defaults. This is the load that seeds `header_roles`, so a silent
    // failure here removes every user-assigned pump role's 30% floor — and until
    // this was reported on `/status`, a single `warn!` was the only trace.
    let (runtime_cfg, runtime_cfg_degraded) =
        RuntimeConfig::load_from_reporting(&runtime_config_path, LoadPhase::Startup);
    if let Some(ref d) = runtime_cfg_degraded {
        log::error!(
            "Runtime config at {} is {} ({}) — the daemon is running on DEFAULTS. \
             User-assigned header roles are NOT in effect, so any pump 30% floor \
             set via POST /config/header-role is not applied. Reported on /status \
             as runtime_config_degraded.",
            d.path,
            d.reason,
            d.detail
        );
    }
    apply_runtime_overlay(&mut config, &runtime_cfg, &config_path);

    // Pre-flight: verify we can bind the IPC socket and write to state_dir
    // *before* starting any subsystem. A failure here is fatal — the daemon
    // is useless without IPC, and a half-started daemon only confuses
    // operators. preflight_check exits(1) itself on failure.
    let allow_non_root = parse_allow_non_root_flag();
    let listener = preflight_check(&config, allow_non_root);

    // Configurable startup delay — wait for hardware to appear after boot
    let startup_delay = effective_startup_delay(config.startup.delay_secs);
    if startup_delay > 0 {
        log::info!("Startup delay: {startup_delay}s");
        std::thread::sleep(Duration::from_secs(startup_delay));
    }

    // Build profile search dirs from config, with the daemon-owned profile
    // store ({state_dir}/profiles) prepended as the primary location (DEC-160).
    let profile_search_dirs = with_store_dir(
        config
            .profiles
            .search_dirs
            .iter()
            .map(std::path::PathBuf::from)
            .collect(),
    );
    // Ensure the store dir exists so listing/activation work immediately — the
    // systemd StateDirectory= creates {state_dir} but not the profiles/ subdir.
    let profile_store_dir = daemon_state::profiles_dir();
    if let Err(e) = control_ofc_daemon::atomic_io::create_dir_private(&profile_store_dir) {
        // Non-fatal: save_raw recreates it on demand. 0o700 owner-only (DEC-173).
        log::warn!("could not create profile store dir: {e}");
    }

    log::info!("Profile search dirs: {:?}", profile_search_dirs);

    let cache = Arc::new(StateCache::new());
    if let Some(n) = &notifier {
        cache.attach_notifier(Arc::clone(n));
    }
    // DEC-388: the exit floor in force until an API write or a SIGHUP changes it.
    cache.set_exit_floor_pct(config.shutdown.exit_floor_pct);
    let serial_timeout = Duration::from_millis(config.serial.timeout_ms);

    // ── Initialize OpenFanController ─────────────────────────────────────────
    let fan_controller: Option<Arc<Mutex<FanController>>>;
    let openfan_transport: Option<
        Arc<Mutex<Box<dyn control_ofc_daemon::serial::transport::SerialTransport + Send>>>,
    >;

    // [SAFETY] OpenFan adoption, and the critical path is the whole point
    // (`OFN-a`/`OFN-r`/`OFN-s`). The controller is OPTIONAL hardware, so a
    // machine without one must pay neither a boot stall nor a warning that reads
    // like a fault. What it must still pay is the DEC-250 identity handshake on
    // anything it does adopt.
    //
    // **Exactly ONE attempt here.** This used to be a ladder of up to six
    // attempts sleeping 1+2+4+8+16 s, and it ran ahead of `axum::serve`, both
    // poll loops and the profile engine — so the daemon answered nothing, and
    // evaluated no thermal safety, for the whole of it. Everything past this one
    // attempt now runs in `post_boot_adoption_loop`, spawned once the server is
    // up, which is strictly better in both directions: no stall for a machine
    // without a controller, and a far LONGER search for one whose device
    // enumerates late than the ladder ever gave it.
    let serial_configured = config.serial.port.is_some();
    let mut serial_connected = false;
    let mut fc: Option<Arc<Mutex<FanController>>> = None;
    let mut ot: Option<
        Arc<Mutex<Box<dyn control_ofc_daemon::serial::transport::SerialTransport + Send>>>,
    > = None;

    // Hoisted rather than block-scoped: `post_boot_adoption_loop` is seeded with
    // this list so it can tell "the bus changed" from "the bus is the same", and
    // therefore never re-probes hardware boot already tried (`OFN-r`).
    let boot_candidates;
    {
        // [SAFETY] Try the configured port first, then every enumerated
        // candidate. The ordering rule lives in
        // `serial_port_candidates_enumerated` so it is unit-testable without a
        // serial device — see its doc comment for why a configured port must
        // never be the only candidate.
        //
        // ENUMERATE rather than auto-detect (`OFN-b`). `auto_detect_port` *opens*
        // each candidate in order to identify it, and on Linux opening a tty
        // asserts DTR — which resets Arduino-class boards. Worse, it probed the
        // libudev list and then fell through to its own `/dev/ttyACM0..9` +
        // `/dev/ttyUSB0..9` scan without deduplicating, so every candidate was
        // opened TWICE per attempt. DEC-291 built this non-opening path for
        // `POST /fans/openfan/rescan` and boot was never moved onto it.
        // `first_openfan_port` below still opens — it must, to run the DEC-250
        // handshake — but now exactly once per candidate.
        let candidates = control_ofc_daemon::serial::adoption::serial_port_candidates_enumerated(
            config.serial.port.as_deref(),
            enumerate_serial_candidates,
        );

        if candidates.is_empty() {
            log::info!("No serial port configured and no serial candidates present");
        }

        // [SAFETY] Accept only a candidate that also *identifies* as an
        // OpenFanController — see `first_openfan_port`.
        if let Some((port, transport)) = control_ofc_daemon::serial::adoption::first_openfan_port(
            &candidates,
            config.serial.port.as_deref(),
            serial_timeout,
            |p| {
                // `OFN-c`: at the SAME level as the outcome in `first_openfan_port`,
                // which is `debug` for an auto-enumerated stranger. Logging the
                // attempt at info while its result is debug is worse than the
                // single line it replaced — the hwmon-only user this change exists
                // to protect would see a probe start and never a probe finish.
                if config.serial.port.as_deref() == Some(p) {
                    log::info!("Opening configured serial port {p}");
                } else {
                    log::debug!("Probing serial candidate {p} for an OpenFanController");
                }
                // DEC-387: under `Type=notify` this probe runs inside the unit's
                // start window, and neither the number of candidates nor
                // `serial.timeout_ms` has a hard ceiling in the admin file. So
                // start-up is bounded by progress: ask for this probe's own bound
                // — the open, then an identity exchange whose final read can run
                // a full timeout past its deadline — plus the slack for the rest.
                // A probe that never returns stops extending, and the start times
                // out as it always did.
                if let Some(n) = notifier.as_deref() {
                    n.extend_timeout(serial_timeout * 2 + BOOT_PROBE_EXTENSION_SLACK);
                }
                RealSerialTransport::open(p, serial_timeout)
            },
        ) {
            log::info!("OpenFanController connected on {port}");
            let boxed: Box<dyn control_ofc_daemon::serial::transport::SerialTransport + Send> =
                Box::new(transport);
            let shared = Arc::new(Mutex::new(boxed));

            let ctrl = FanController::new_shared(shared.clone(), cache.clone(), serial_timeout);
            fc = Some(Arc::new(Mutex::new(ctrl)));
            ot = Some(shared);
            serial_connected = true;
        }
        boot_candidates = candidates;
    }

    if !serial_connected {
        // OFN-c: absence of OPTIONAL hardware is not a fault, and this line ships
        // to journald at info, so a `warn!` here put "No OpenFanController found"
        // into `systemctl status` and every support bundle for every hwmon-only
        // user — the canonical "is my install broken?" surface.
        //
        // A CONFIGURED port that did not yield a controller is different: the
        // user named a device and the daemon could not adopt it. That stays a
        // warning. The per-candidate identity rejection in `first_openfan_port`
        // is likewise still a warning, and deliberately so (DEC-250) — a tty that
        // opens but is not an OpenFanController accepts every write with `Ok`.
        if serial_configured {
            log::warn!(
                "No OpenFanController adopted at startup — the configured serial port did not \
                 identify as one, and no detected candidate did either. Still looking in the \
                 background; running without serial fan control until one appears"
            );
        } else {
            log::info!(
                "No OpenFanController detected — motherboard and GPU fan control are unaffected"
            );
        }
    }

    fan_controller = fc;
    openfan_transport = ot;

    // ── Initialize hwmon PWM controller ─────────────────────────────
    let hwmon_root = Path::new(HWMON_SYSFS_ROOT);
    let mut hwmon_headers_for_poll = Vec::new();
    // DEC-382: the hand-back ledger, cloned out before the controller goes behind
    // its mutex — the shutdown restore and the panic hook must reach it without
    // that lock, which a wedged sysfs write can hold for good.
    let mut hwmon_handback: Option<Arc<HandBackLedger>> = None;
    let hwmon_controller = match discover_pwm_headers(hwmon_root) {
        Ok(headers) if !headers.is_empty() => {
            log::info!("Discovered {} hwmon PWM header(s)", headers.len());
            for h in &headers {
                log::info!(
                    "  {} — {} (writable={}, mode={:?})",
                    h.id,
                    h.label,
                    h.is_writable,
                    h.pwm_mode
                );
            }
            // Keep a copy for the polling loop (needs paths for RPM/PWM reads)
            hwmon_headers_for_poll = headers.clone();
            let ctrl = HwmonPwmController::new(
                headers,
                LeaseManager::new(),
                Box::new(RealSysfsWriter),
                cache.clone(),
            );
            keep_handback_record(ctrl.handback());
            hwmon_handback = Some(ctrl.handback().clone());
            Some(Arc::new(Mutex::new(ctrl)))
        }
        Ok(_) => {
            log::info!("No hwmon PWM headers found");
            None
        }
        Err(e) => {
            log::warn!("hwmon PWM discovery failed: {e}");
            None
        }
    };

    // AIO Phase 8 Batch 1 (§6.3): load the persisted PWM to tach relationships
    // and drop any whose header discovery can no longer see.
    //
    // The invalidation is structural, not a policy anyone has to remember: a
    // record is keyed by the header's stable id, which embeds chip, device,
    // `pwmN` and label. Change the board, the driver, or start publishing labels
    // and the id changes with it, so a record that SURVIVES this prune is a
    // record whose hardware did not change. §6.3's warning against persisting a
    // mapping "as unquestioned truth" is satisfied here rather than by a
    // freshness heuristic.
    let control_paths_at_boot = {
        let dir = control_ofc_daemon::daemon_state::state_dir_path();
        let loaded = control_ofc_daemon::control_paths::load_from(&dir);
        let live: Vec<String> = hwmon_headers_for_poll
            .iter()
            .map(|h| h.id.clone())
            .collect();
        let pruned =
            control_ofc_daemon::api::handlers::discovery::prune_store_to_live(&loaded, &live);
        if pruned != loaded {
            if let Err(e) = control_ofc_daemon::control_paths::save_to(&dir, &pruned) {
                log::warn!("could not rewrite the pruned control-path store: {e}");
            }
        }
        pruned
    };

    // DEC-334 §6: the learned-response store, pruned by the same stable-header-id
    // rule for the same reason — a record that survives is one whose hardware did.
    let pwm_baselines_at_boot = {
        let dir = control_ofc_daemon::daemon_state::state_dir_path();
        let mut loaded = control_ofc_daemon::pwm_baselines::load_from(&dir);
        let live: Vec<String> = hwmon_headers_for_poll
            .iter()
            .map(|h| h.id.clone())
            .collect();
        if loaded.prune_to_live(&live) > 0 {
            if let Err(e) = control_ofc_daemon::pwm_baselines::save_to(&dir, &loaded) {
                log::warn!("could not rewrite the pruned PWM baseline store: {e}");
            }
        }
        loaded
    };

    let staleness_config = StalenessConfig {
        openfan_interval_ms: config.polling.poll_interval_ms,
        hwmon_interval_ms: config.polling.poll_interval_ms,
        // Fixed 1 Hz — the engine's tick period is hardcoded in
        // `profile_engine_loop`, not derived from `poll_interval_ms`, so raising
        // the poll interval must not widen what counts as a live engine.
        engine_interval_ms: 1000,
    };

    // DEC-267/269: the engine's CPU-staleness budget also derives from
    // `poll_interval_ms`, but `hwmon_poll_loop` publishes it rather than this
    // function — the loop owns the interval, and a wiring line here was
    // unpinnable by any test (deleting it left the whole suite green while the
    // budget silently reverted to its 1 s default, understating it on a slower
    // daemon and judging a healthy loop dead). See `polling::hwmon_poll_loop`.

    let history = Arc::new(HistoryRing::new(250));

    // ── Thermal safety rule ─────────────────────────────────────────
    let safety_rule = Arc::new(Mutex::new(ThermalSafetyRule::new()));
    // DEC-308: this is the FLOOR, not necessarily the trip point this machine
    // will use. The engine derives a higher one per tick where the CPU reports
    // its own design ceiling, and publishes what it acted on — a startup line
    // cannot know it yet, because no sensor has been read. Worded so it does not
    // become a fifth copy of a threshold that varies.
    log::info!(
        "Thermal safety rule active: hottest CpuTemp emergency at {}°C or above \
         (raised per-machine where the CPU reports its own ceiling)",
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C
    );

    // ── Profile loading (CLI > env > persisted state > none) ────────
    let initial_profile = resolve_initial_profile(&profile_search_dirs);
    let active_profile: Arc<Mutex<Option<DaemonProfile>>> = Arc::new(Mutex::new(initial_profile));

    // Detect AMD GPUs
    let amd_gpus = control_ofc_daemon::hwmon::gpu_detect::detect_amd_gpus(std::path::Path::new(
        HWMON_SYSFS_ROOT,
    ));
    if !amd_gpus.is_empty() {
        for gpu in &amd_gpus {
            log::info!(
                "AMD GPU detected: {} (PCI {}, fan control: {})",
                gpu.display_label(),
                gpu.pci_bdf,
                gpu.fan_control_method(),
            );
        }
    }

    // Detect Intel discrete GPUs (DEC-121). Read-only monitoring — temps +
    // fan RPM; no fan write path exists in the kernel.
    let intel_gpus = control_ofc_daemon::hwmon::intel_gpu_detect::detect_intel_gpus(
        std::path::Path::new(HWMON_SYSFS_ROOT),
    );
    for gpu in &intel_gpus {
        log::info!(
            "Intel GPU detected: {} (driver {}, PCI {}, fan control: {} [firmware-managed])",
            gpu.display_label(),
            gpu.driver,
            gpu.pci_bdf,
            gpu.fan_control_method(),
        );
    }
    let intel_gpus_for_poll = intel_gpus.clone();

    // Detect nouveau-backed NVIDIA discrete GPUs (DEC-204). Read-only telemetry —
    // temps flow through the sensor pipeline; fan RPM is polled here. The writable
    // nouveau `pwm1` is excluded from hwmon discovery (`is_gpu_owned_hwmon_chip`)
    // so the engine never drives it. Passed straight to the poll loop (no AppState
    // store yet — the `/capabilities` + `/diagnostics` surfaces land in a later phase).
    let nouveau_gpus_for_poll = control_ofc_daemon::hwmon::nouveau_detect::detect_nouveau_gpus(
        std::path::Path::new(HWMON_SYSFS_ROOT),
    );
    for gpu in &nouveau_gpus_for_poll {
        log::info!(
            "NVIDIA GPU detected (nouveau): PCI {} (fan RPM: {} [read-only])",
            gpu.pci_bdf,
            if gpu.has_fan_rpm { "available" } else { "none" },
        );
    }

    // Initialise the opt-in, read-only NVIDIA NVML telemetry backend (DEC-204).
    // Default: disabled — `libnvidia-ml.so.1` is never loaded. When enabled but
    // NVML is absent or fails to init, this degrades to a no-op backend (never
    // fatal). EXPERIMENTAL: the real NVML path is unverified on hardware.
    let nvml_backend = control_ofc_daemon::hwmon::nvml::init_nvml_backend(
        config.detection.enable_nvidia_telemetry,
    );

    // Unified NVIDIA GPU identity (nouveau + NVML legs), gathered once for the
    // `/capabilities` + `/diagnostics/hardware` surfaces (DEC-204). Read-only.
    // Gathered before `nouveau_gpus_for_poll` / `nvml_backend` are moved into
    // the poll loop below.
    let nvidia_gpus = control_ofc_daemon::hwmon::nvidia::gather_nvidia_gpus(
        &nouveau_gpus_for_poll,
        &*nvml_backend,
    );

    // DEC-206/207: share ONE rollup Arc between the AppState poll mirror and the
    // AssessmentCache — the cache's store() writes both in lockstep so the poll
    // path stays a cheap clone and the two never drift.
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    // DEC-265: created here rather than just before the poll loops, because
    // AppState carries a receiver so a loop started later by
    // `POST /fans/openfan/rescan` shuts down with the ones started at boot.
    let (poll_shutdown_tx, poll_shutdown_rx) = tokio::sync::watch::channel(false);
    let app_state = Arc::new(AppState {
        cache: cache.clone(),
        staleness_config,
        daemon_version: VERSION.to_string(),
        fan_controller: Arc::new(parking_lot::RwLock::new(fan_controller)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: serial_timeout,
            interval: Duration::from_millis(config.polling.poll_interval_ms),
            shutdown: poll_shutdown_rx.clone(),
        },
        hwmon_controller,
        start_time: Instant::now(),
        history: history.clone(),
        active_profile: active_profile.clone(),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        // AIO Phase 8 Batch 1: control-path discovery. Same slot-plus-cancel
        // shape as characterisation above, and the same single verify slot, so
        // this is a fourth claimant rather than a fourth concurrent writer.
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        stall_probe: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        stall_probe_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        // Persisted PWM to tach relationships, pruned at boot to whatever
        // discovery can still see. The header id embeds chip, device, pwmN and
        // label, so a board or driver change invalidates a stale record by
        // construction rather than by anyone remembering to check.
        control_paths: Arc::new(parking_lot::RwLock::new(Arc::new(control_paths_at_boot))),
        pwm_baselines: Arc::new(parking_lot::RwLock::new(Arc::new(pwm_baselines_at_boot))),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_tasks: std::sync::Arc::new(parking_lot::Mutex::new(Default::default())),
        amd_gpus,
        intel_gpus,
        nvidia_gpus,
        profile_search_dirs: parking_lot::RwLock::new(profile_search_dirs),
        config_path: config_path.clone(),
        runtime_config_path: runtime_config_path.clone(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        // DEC-311: user-assigned header roles, restored from runtime.toml at
        // boot. A `pump` assignment is a safety floor, so it must survive a
        // restart — an unrecognised token is dropped with a warning rather than
        // failing the load (see `header_roles_parsed`).
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            runtime_cfg.header_roles_parsed(),
        ))),
        // AIO-MB Phase 4: topology from the top-level `[[cooling_devices]]`
        // array. Sanitised on read, so one bad hand-edited device costs only
        // itself. Effective immediately, like `header_roles`.
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(
            runtime_cfg.cooling_devices(),
        ))),
        allow_port_probe: config.detection.allow_port_probe,
        running_config: config.clone(),
        // DEC-206/207: seeded by the assessment task below once the poll cache is
        // warm. The rollup Arc is shared with the AssessmentCache (its store keeps
        // this poll mirror in lockstep with the full snapshot).
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        // `AUD3-m`: seeded from the boot load above; after that written only
        // through `runtime_config::record_degraded`, by the SIGHUP reload path and
        // the `/config/*` setters (`TS-r`). Never cleared — see the field's doc.
        runtime_config_degraded: Arc::new(parking_lot::RwLock::new(runtime_cfg_degraded)),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
        )),
    });

    // runtime_cfg is consumed by the overlay/migration above and by the
    // DEC-311 header-role restore in `AppState`; the variable itself is no
    // longer needed.
    drop(runtime_cfg);

    // Populate panic hook targets now that hardware is discovered.
    {
        let gpu_curves: Vec<_> = app_state
            .amd_gpus
            .iter()
            .filter_map(|g| {
                g.fan_curve_path
                    .clone()
                    .map(|p| (p, g.fan_zero_rpm_path.clone()))
            })
            .collect();
        let _ = PANIC_RESTORE.set(PanicRestoreTargets {
            gpu_curves,
            hwmon_handback: hwmon_handback.clone(),
        });
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    // ── Spawn hwmon sensor + fan polling loop ──────────────────────
    let hwmon_cache = cache.clone();
    let hwmon_history = history.clone();
    let hwmon_interval = Duration::from_millis(config.polling.poll_interval_ms);
    let hwmon_shutdown = poll_shutdown_rx.clone();
    let gpu_infos_for_poll = app_state.amd_gpus.clone();
    let sensor_rescan_for_poll = app_state.sensor_rescan_requested.clone();
    // DEC-146 P3-9: keep the JoinHandles for the poll/engine tasks so
    // shutdown can await them before restoring hardware to automatic.
    // DEC-267: supervised, for the same reason the engine is (DEC-266). This
    // loop is the ONLY writer of the sensor map the thermal-emergency rule reads, so its
    // death used to blind that rule silently.
    //
    // DEC-269 corrects what this comment used to claim. Stale readings do NOT
    // simply "present as absent": a stale reading last seen at or above the
    // release temperature keeps fan curves running on it, and one seen while an
    // emergency is latched holds it (as, since DEC-386, does a vanished one).
    // Only a stale-and-cool or absent reading with nothing latched reaches
    // NO_SENSOR_SAFE_PCT. Either way, none of those is
    // a resting state to leave a machine in with no path back — hence the
    // restore-and-exit, so systemd brings the daemon back with a live loop.
    //
    // This catches the loop *dying*. A wedged blocking read leaves the task
    // alive, so supervision never fires — that case is covered one level down by
    // DEC-272, which bounds the blocking join with the freshness budget and holds
    // the outstanding handle instead of stacking a new read behind it. The loop
    // keeps ticking through a wedge; its readings age out and the freshness
    // filters act on that.
    let (hwmon_poll_handle, hwmon_dead_rx) = spawn_supervised(async move {
        control_ofc_daemon::polling::hwmon_poll_loop(
            hwmon_cache,
            hwmon_history,
            hwmon_headers_for_poll,
            gpu_infos_for_poll,
            intel_gpus_for_poll,
            nouveau_gpus_for_poll,
            nvml_backend,
            hwmon_root,
            // DEC-294: the real DMI root. The loop reads the board vendor from
            // it once, to gate the bogus-sensor demotion.
            std::path::Path::new("/sys/class/dmi/id"),
            hwmon_interval,
            sensor_rescan_for_poll,
            hwmon_shutdown,
        )
        .await;
    });

    // ── DEC-206: seed the readiness rollup for the Dashboard health chip ──
    // Recompute the compact rollup once the poll loop's first tick has filled the
    // sensor cache, so the chip reflects real hardware (not a false "no CPU
    // sensor" against an empty cache) from the user's first poll. Decoupled from
    // the hot poll loop; later refreshes ride the preferred-sensor and
    // readiness-GET handlers (a rescan-driven update rides the GUI's post-rescan
    // readiness GET). Bounded wait, then compute regardless — a genuinely
    // sensorless host still gets a (critical) rollup.
    {
        let seed_state = app_state.clone();
        let seed_interval = hwmon_interval;
        tokio::spawn(async move {
            for _ in 0..30u32 {
                if !seed_state.cache.snapshot().sensors.is_empty() {
                    break;
                }
                tokio::time::sleep(seed_interval).await;
            }
            // DEC-207: seed the shared hardware assessment (its store also mirrors
            // the rollup for the Dashboard chip). Coalesced, off the poll path,
            // and logs its own failure; `force` so the seed always runs one scan.
            let _ = control_ofc_daemon::api::handlers::ensure_assessment(seed_state, true).await;
        });
    }

    // ── Spawn OpenFanController polling loop ────────────────────────
    let openfan_poll_handle = if let Some(transport) = openfan_transport {
        let openfan_cache = cache.clone();
        let openfan_interval = Duration::from_millis(config.polling.poll_interval_ms);
        let openfan_shutdown = poll_shutdown_rx.clone();
        Some(tokio::spawn(async move {
            control_ofc_daemon::polling::openfan_poll_loop(
                openfan_cache,
                transport,
                serial_timeout,
                openfan_interval,
                openfan_shutdown,
            )
            .await;
        }))
    } else {
        None
    };

    // ── Spawn profile engine ─────────────────────────────────────────
    // Evaluates curves and writes PWM headlessly at 1Hz. The engine is the
    // sole PWM writer (DEC-159/DEC-165). In imperative mode (no active profile)
    // no curve is evaluated — the daemon writes for explicit API intent (manual
    // override, fan identify) and for the thermal ladder, which acts with or
    // without a profile (`TS-k`); the GUI never writes PWM.
    //
    // DEC-266: the engine is SUPERVISED. Its task dying is not a contained
    // failure — it is the loss of the only PWM writer, and with it the thermal
    // emergency, while the process stays up and `/status` keeps answering. The
    // panic hook cannot cover this (the engine runs on a tokio worker thread, so
    // its panic is "contained" by construction), and `Restart=on-failure` cannot
    // either, because nothing exits. So the task signals its own death on drop —
    // which a panic-unwind triggers just as a normal return does — and the main
    // loop turns that into the same restore-to-automatic shutdown a SIGTERM
    // would, then exits non-zero so systemd restarts us with a live engine.
    let (engine_handle, engine_dead_rx) = {
        let engine_cache = cache.clone();
        let engine_profile = active_profile.clone();
        let engine_safety = safety_rule.clone();
        let engine_fc = app_state.fan_controller.clone();
        let engine_hwmon = app_state.hwmon_controller.clone();
        let engine_gpus = app_state.amd_gpus.clone();
        let engine_overrides = app_state.override_table.clone();
        let engine_roles = app_state.header_roles.clone();
        let engine_shutdown = poll_shutdown_rx;

        spawn_supervised(async move {
            control_ofc_daemon::profile_engine::profile_engine_loop(
                engine_cache,
                engine_profile,
                engine_fc,
                engine_hwmon,
                engine_gpus,
                engine_safety,
                engine_overrides,
                engine_roles,
                engine_shutdown,
            )
            .await;
        })
    };

    // ── Spawn the validation recorder (AIO-MB Phase 5) ──────────────
    //
    // Always alive, idle until a session records. A PURE OBSERVER in the
    // narrowed sense `validation/recorder.rs` documents: it reads the state
    // cache the poll already fills, and the sysfs reads DEC-335 added for power
    // sampling are read-only and taken outside the session slot guard — so it
    // still cannot perturb a control decision, and a fault in it cannot take
    // down the sensor feed (§15). "Performs no sysfs I/O" was the original claim
    // and is retracted (`P8-ao`); the narrowed one only became true at `start`
    // with DEC-342 (`P8-v`), which is why it is worth stating precisely.
    //
    // A plain `tokio::spawn`, NOT `spawn_supervised`: a dead recorder loses
    // evidence, which is not a reason to kill a daemon that is still controlling
    // fans correctly. It IS joined at shutdown (see `task_handles` below) — the
    // 277-c lesson about an orphaned handle that stops but is never awaited.
    //
    // Deliberately different from the Phase 3 sweep, which is a bare detached
    // spawn: that task is short-lived and self-bounding, this one is not.
    let validation_handle = {
        let engine = app_state.validation.clone();
        let (hwmon_root, powercap_root) =
            control_ofc_daemon::validation::recorder::RecorderContext::sysfs_roots();
        let ctx = control_ofc_daemon::validation::recorder::RecorderContext {
            cache: cache.clone(),
            hwmon_controller: app_state.hwmon_controller.clone(),
            override_table: app_state.override_table.clone(),
            characterization: app_state.characterization.clone(),
            hwmon_root,
            powercap_root,
        };
        let mut shutdown = poll_shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(control_ofc_daemon::constants::VALIDATION_SAMPLE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    _ = interval.tick() => {
                        if *shutdown.borrow() {
                            break;
                        }
                        // `AUD3-n`: off the async runtime. One tick in 30 flushes
                        // the whole session document — `write` + `fsync` +
                        // `rename` + a directory `fsync`, over ~5.7 MiB for a
                        // realistic two-member session (28 MiB bound)
                        // (`AUD3-i`) — and every tick can wait on the hwmon
                        // controller lock. Neither belongs on the worker threads
                        // the 1 Hz profile engine, and therefore the
                        // thermal-safety decision, is scheduled on.
                        //
                        // Awaited rather than detached: two ticks must never run
                        // concurrently against one session, and `MissedTickBehavior
                        // ::Skip` above is already the policy for a tick that ran
                        // long.
                        let (e, c) = (engine.clone(), ctx.clone());
                        if tokio::task::spawn_blocking(move || e.tick(&c)).await.is_err() {
                            // Panicked, or the runtime is going down. A dead
                            // recorder loses evidence, which is not a reason to
                            // stop a daemon that is still controlling fans — the
                            // same judgement as the plain `spawn` above.
                            log::warn!("Validation recorder tick failed");
                        }
                    }
                }
            }
            // Flush whatever was captured. The session stays `recording` on
            // disk on purpose: the next boot's sweep is what turns it into
            // `interrupted`, which is the honest representation of a restart
            // (§15) and is unavailable to us here — we cannot know whether the
            // daemon is coming back.
            //
            // Deliberately still inline, unlike the tick above (`AUD3-n`): this
            // is the shutdown path, the engine loop is already stopping, and
            // handing the last write to a pool whose runtime is being torn down
            // trades a certain flush for a possible one.
            //
            // But through `flush_recording`, NOT `store::save` — this shares the
            // engine's `save_lock` and stale-write guard, so an in-flight
            // `POST /validation/session/stop` racing this shutdown cannot have
            // its `completed` document overwritten by this `recording` snapshot
            // and be "repaired" to `interrupted` on the next boot.
            engine.flush_recording();
        })
    };

    // ── Opt-in startup lifecycle recording (DEC-335 §1) ─────────────
    //
    // OFF unless `[startup] record_startup = true`. This is the only autonomous
    // behaviour AIO Phase 8 Batch 3a adds, and the bar for the daemon acting on
    // its own is deliberately high — so it is opt-in, bounded, and incapable of
    // costing an operator anything:
    //
    //   * it writes no hardware, claims no verify slot and takes no hwmon lease;
    //   * `ValidationEngine::start` PRE-EMPTS it the moment an operator starts a
    //     session, so it can never turn `POST /validation/session` into a 409;
    //   * `store::prune` retains auto-records in their own slot, so a machine
    //     that reboots repeatedly cannot evict hand-made sessions;
    //   * the stop is fenced on the session id, so a window that elapses after
    //     an operator took over does not finalise *their* session.
    //
    // Detached rather than joined at shutdown: it holds no resource worth
    // draining, and a shutdown inside the window leaves the recording marked
    // `recording`, which the next boot's `sweep_interrupted` repairs to
    // `interrupted` — the honest outcome, and the one §15 already specifies.
    if config.startup.record_startup {
        let state = app_state.clone();
        tokio::spawn(async move {
            let Some(session_id) =
                control_ofc_daemon::api::handlers::validation::start_auto_startup_record(&state)
                    .await
            else {
                return;
            };
            tokio::time::sleep(std::time::Duration::from_secs(
                control_ofc_daemon::constants::STARTUP_RECORD_WINDOW_S,
            ))
            .await;
            control_ofc_daemon::api::handlers::validation::stop_auto_startup_record(
                &state,
                &session_id,
            )
            .await;
        });
    }

    // ── Spawn IPC server ────────────────────────────────────────────
    // Listener was bound in preflight_check, so we know IPC is healthy
    // before any subsystem started. If the server task exits unexpectedly
    // after this point, ipc_dead_rx fires and the main loop breaks so the
    // daemon shuts down cleanly instead of running headless.
    let socket_path = config.ipc.socket_path.clone();
    let server_state = app_state.clone();
    let (ipc_dead_tx, ipc_dead_rx) = tokio::sync::oneshot::channel::<String>();
    let server_handle = tokio::spawn(async move {
        match server::serve(listener, socket_path, server_state, shutdown_rx).await {
            Ok(()) => {
                log::info!("IPC server exited cleanly");
            }
            Err(e) => {
                log::error!("IPC server error: {e}");
                let _ = ipc_dead_tx.send(e.to_string());
            }
        }
    });

    // ── Keep looking for an OpenFanController, off the critical path ────────
    // `OFN-r`/`OFN-s`. Boot made ONE adoption attempt; this is the rest of the
    // search, and it runs only if that attempt found nothing. Spawned AFTER the
    // IPC server so the daemon is already answering — which is the whole reason
    // the ladder could be taken off the critical path.
    //
    // [SAFETY] It drives `POST /fans/openfan/rescan`'s own handler rather than
    // probing directly, so it cannot skip the DEC-250 handshake, the DEC-266
    // conditional install, the poll-loop spawn or the 277-c handle registration,
    // and it shares the single-flight guard with a user-triggered rescan. A
    // controller it adopts registers its poll loop in `adopted_poll_tasks`,
    // which `task_handles` drains below on the same terms as a boot-adopted one
    // — atomically with closing registration, so an adoption racing shutdown is
    // either drained or refused (`OFN-t`).
    let post_boot_adoption_handle = if serial_connected {
        None
    } else {
        let adopt_state = app_state.clone();
        let adopt_shutdown = poll_shutdown_tx.subscribe();
        let window =
            control_ofc_daemon::serial::adoption::post_boot_adoption_window(serial_configured);
        Some(tokio::spawn(async move {
            control_ofc_daemon::api::handlers::post_boot_adoption_loop(
                adopt_state,
                window,
                control_ofc_daemon::api::handlers::POST_BOOT_ADOPTION_INTERVAL,
                adopt_shutdown,
                boot_candidates,
            )
            .await;
        }))
    };

    log::info!("Daemon ready — waiting for shutdown signal");

    // Handle SIGHUP (config reload), SIGINT/SIGTERM (shutdown), and IPC task
    // death (shutdown — daemon is useless without IPC).
    //
    // SIGTERM is what systemd sends on `systemctl stop` by default. Without
    // a handler the kernel terminates the process before the in-process
    // graceful path below (`shutdown_tx.send`, GPU reset, hwmon restore,
    // server join) can run; external safety still works via the
    // ExecStopPost restore script, but the in-line cleanup is silently
    // skipped. SIGHUP and SIGTERM registrations are both fail-soft: if the
    // kernel refuses (rare — typically only happens under unusual sandbox
    // policies), the daemon still terminates cleanly on SIGINT.
    // DEC-266/267: set when the loop breaks because a task the daemon cannot
    // function without ended — the profile engine (sole PWM writer) or the hwmon
    // poll loop (sole writer of the sensor map the thermal-emergency rule reads). Drives a
    // non-zero exit AFTER the ordered restore has run, so systemd restarts us.
    let stop = {
        use tokio::signal::unix::SignalKind;

        let sighup = match tokio::signal::unix::signal(SignalKind::hangup()) {
            Ok(stream) => Some(stream),
            Err(e) => {
                log::warn!("Failed to register SIGHUP handler, config reload unavailable: {e}");
                None
            }
        };
        let sigterm = match tokio::signal::unix::signal(SignalKind::terminate()) {
            Ok(stream) => Some(stream),
            Err(e) => {
                log::warn!(
                    "Failed to register SIGTERM handler, only SIGINT will trigger graceful \
                     shutdown: {e}"
                );
                None
            }
        };

        // TS-ao (DEC-396): the sleep hook's signals, before READY=1 so an active
        // unit always answers them. Only under systemd — the hook reaches the
        // daemon through the unit's MainPID and runtime directory.
        if let Some(n) = &notifier {
            start_sleep_watch(n);
        }

        // DEC-387 (`TS-d`): start-up is complete — the engine is ticking, the API
        // is serving, and SIGTERM now reaches the graceful path. systemd holds
        // the start job (and so `multi-user.target`) until this, and starts the
        // watchdog clock from it. Sent after the signal handlers are registered
        // so that any stop arriving after readiness takes the graceful path.
        if let Some(n) = notifier.as_deref() {
            n.ready();
        }

        wait_for_stop(
            sighup,
            sigterm,
            ipc_dead_rx,
            engine_dead_rx,
            hwmon_dead_rx,
            || {
                if let Err(e) = apply_config_reload(
                    &config_path,
                    &runtime_config_path,
                    &app_state.profile_search_dirs,
                    &app_state.runtime_config_degraded,
                    &app_state.cache,
                ) {
                    log::error!("{e}");
                }
            },
        )
        .await
    };
    let must_restart = stop.must_restart;
    log::debug!("Main loop stopped: {:?}", stop.reason);

    // Ordered graceful shutdown (DEC-146 P3-9 + audit P1-A) — see
    // `shutdown_sequence`: stop the IPC server and drain the poll/engine tasks
    // BEFORE restoring hardware to automatic, so neither a late client write nor
    // an in-flight engine write can land after the restore.
    let mut task_handles: Vec<(&'static str, tokio::task::JoinHandle<()>)> = [
        ("hwmon-poll", Some(hwmon_poll_handle)),
        ("openfan-poll", openfan_poll_handle),
        ("profile-engine", Some(engine_handle)),
        ("validation-recorder", Some(validation_handle)),
        ("openfan-post-boot-adoption", post_boot_adoption_handle),
    ]
    .into_iter()
    .filter_map(|(name, handle)| handle.map(|h| (name, h)))
    .collect();

    // 277-c: an OpenFanController adopted after boot (DEC-265) spawned its own
    // poll loop, and this list was built at startup — so that loop stopped via
    // the shared shutdown watch but was never *joined*. Drain those handles in
    // here, so "the restore is the guaranteed last writer" holds for a
    // rescan-adopted controller on the same terms as a boot-time one.
    //
    // `close_and_drain`, not a bare drain (`OFN-t`). A plain drain left a window
    // this comment implicitly denied: it runs HERE, and the shutdown watch is not
    // set until `finish_shutdown` calls `shutdown_sequence` below — so an
    // adoption completing in between registered a handle into a list nothing
    // would read again, and one completing later could install a controller after
    // `restore_hardware()` had run. Closing and taking in one critical section is
    // what makes the drain a guarantee rather than a snapshot.
    task_handles.extend(
        app_state
            .adopted_poll_tasks
            .lock()
            .close_and_drain()
            .into_iter()
            .map(|h| ("openfan-poll (adopted)", h)),
    );

    finish_shutdown(
        notifier.as_deref(),
        &poll_shutdown_tx,
        shutdown_tx,
        server_handle,
        task_handles,
        SHUTDOWN_TASK_TIMEOUT,
        must_restart,
        || {
            // [SAFETY] DEC-388: the exit floor FIRST — see `apply_exit_floor` for
            // why the order is load-bearing under a watchdog stop's shorter abort
            // timeout. Nothing after it in this closure touches what it reaches,
            // `ExecStopPost` cannot redo it, and it latches in `FanController`
            // and `HwmonPwmController` (DEC-392), so no write that outlives the
            // drains can lower an OpenFan channel or a no-mode header below it.
            let _ = apply_exit_floor(
                app_state.fan_controller.read().clone(),
                app_state.hwmon_controller.clone(),
                app_state.cache.exit_floor_pct(),
                SHUTDOWN_TASK_TIMEOUT,
            );

            // Reset GPU fans to automatic before shutting down (re-enables
            // zero-RPM). Bounded since 278-c: this step runs FIRST, so leaving it
            // unbounded meant a wedged PMFW write blocked here and the bounded
            // hwmon restore below was never reached — the process stayed just as
            // stuck as before 277-b. Paths are collected here and moved into the
            // bounded thread; nothing borrows `app_state` across the deadline.
            let gpu_curves: Vec<(PathBuf, Option<PathBuf>)> = app_state
                .amd_gpus
                .iter()
                .filter_map(|gpu| {
                    gpu.fan_curve_path
                        .clone()
                        .map(|c| (c, gpu.fan_zero_rpm_path.clone()))
                })
                .collect();
            let _ = restore_gpu_fans_to_auto(gpu_curves, SHUTDOWN_TASK_TIMEOUT);

            // Give every hwmon header the daemon holds back to what it was doing
            // before the daemon took it (DEC-382) — never a hardcoded mode.
            // Bounded since 277-b, because everything that could otherwise
            // backstop a stall here runs *after* it. See `hand_back_hwmon`.
            let _ = hand_back_hwmon(
                hwmon_handback.as_ref(),
                SHUTDOWN_TASK_TIMEOUT,
                SHUTDOWN_TASK_TIMEOUT,
            );
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    // ── DEC-243 runtime overlay ──────────────────────────────────────────
    // `apply_runtime_overlay` is the half that makes "takes effect on restart"
    // TRUE: the setters persist to runtime.toml, and only this function moves
    // those values into the config the process actually runs on. It is also a
    // SECOND copy of the same merge — `api::handlers::config::effective_on_disk`
    // computes it independently for GET /config. Nothing tied the two together,
    // and deleting all five branches below left the entire suite green.
    //
    // If they drift, settings persist and silently never apply, while GET /config
    // keeps reporting restart_pending after every restart — a permanently
    // unclearable banner over a setting that does nothing. That exact shape had
    // to be fixed once already for profiles.search_dirs.

    // ── DEC-272 (01-f): the main loop's stop decision ────────────────────
    // These properties used to be established by READING `main`'s inline
    // `select!`. Every ADR from DEC-266 on depends on them, and nothing pinned
    // any of them.

    /// Build the three death channels. The SENDERS must stay bound: dropping a
    /// oneshot sender resolves its receiver with `Err`, which fires that arm — so
    /// an unused `let _ = ` here would silently make every test race.
    #[allow(clippy::type_complexity)]
    fn stop_channels() -> (
        (
            tokio::sync::oneshot::Sender<String>,
            tokio::sync::oneshot::Receiver<String>,
        ),
        (
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        ),
        (
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        ),
    ) {
        (
            tokio::sync::oneshot::channel::<String>(),
            tokio::sync::oneshot::channel::<()>(),
            tokio::sync::oneshot::channel::<()>(),
        )
    }

    /// [SAFETY] DEC-266. The engine is the sole PWM writer; its death must exit
    /// non-zero so `Restart=on-failure` brings the daemon back with a live one.
    #[tokio::test]
    async fn engine_death_stops_the_loop_and_demands_a_restart() {
        let ((_ipc_tx, ipc_rx), (engine_tx, engine_rx), (_hw_tx, hw_rx)) = stop_channels();
        engine_tx.send(()).unwrap();

        let out = wait_for_stop(None, None, ipc_rx, engine_rx, hw_rx, || {}).await;

        assert_eq!(out.reason, StopReason::EngineDead);
        assert!(out.must_restart, "a dead engine must force a restart");
    }

    /// [SAFETY] DEC-267. Same one rung upstream: the poll loop is the sole writer
    /// of the sensor map the thermal-emergency rule reads.
    #[tokio::test]
    async fn hwmon_death_stops_the_loop_and_demands_a_restart() {
        let ((_ipc_tx, ipc_rx), (_engine_tx, engine_rx), (hw_tx, hw_rx)) = stop_channels();
        hw_tx.send(()).unwrap();

        let out = wait_for_stop(None, None, ipc_rx, engine_rx, hw_rx, || {}).await;

        assert_eq!(out.reason, StopReason::HwmonDead);
        assert!(out.must_restart, "a dead poll loop must force a restart");
    }

    /// A dead IPC server is a clean stop, not a safety event: the fans are still
    /// being driven correctly, there is just nobody to talk to. Exit 0.
    #[tokio::test]
    async fn ipc_death_stops_the_loop_without_demanding_a_restart() {
        let ((ipc_tx, ipc_rx), (_engine_tx, engine_rx), (_hw_tx, hw_rx)) = stop_channels();
        ipc_tx.send("socket closed".into()).unwrap();

        let out = wait_for_stop(None, None, ipc_rx, engine_rx, hw_rx, || {}).await;

        assert_eq!(out.reason, StopReason::IpcDead);
        assert!(
            !out.must_restart,
            "losing IPC alone is a clean stop — fans are still under control"
        );
    }

    /// [SAFETY] DEC-269's regression, pinned. A shared root cause (blocking-pool
    /// exhaustion, OOM pressure) can end several tasks in the same instant.
    /// `select!` reports ONE arm, and it chooses at random among ready arms — so
    /// when IPC wins that race, only the unconditional post-loop sweep is left to
    /// notice the engine also died. Without it the process exited 0 and
    /// `Restart=on-failure` never fired, silently losing the restart DEC-266
    /// exists to produce.
    ///
    /// Repeated because the arm is chosen randomly: one run cannot show that BOTH
    /// the winning-arm path and the sweep path reach the same verdict.
    #[tokio::test]
    async fn a_simultaneous_engine_death_forces_a_restart_whichever_arm_wins() {
        for i in 0..25 {
            let ((ipc_tx, ipc_rx), (engine_tx, engine_rx), (_hw_tx, hw_rx)) = stop_channels();
            ipc_tx.send("socket closed".into()).unwrap();
            engine_tx.send(()).unwrap();

            let out = wait_for_stop(None, None, ipc_rx, engine_rx, hw_rx, || {}).await;

            assert!(
                out.must_restart,
                "run {i}: the engine had also died, so this must restart — got {out:?}"
            );
        }
    }

    /// SIGHUP reloads and keeps waiting. A regression that treated it like the
    /// other arms would turn every config reload into a daemon shutdown.
    #[tokio::test]
    async fn sighup_reloads_the_config_without_stopping_the_loop() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("SIGHUP must be registerable");
        let ((ipc_tx, ipc_rx), (_engine_tx, engine_rx), (_hw_tx, hw_rx)) = stop_channels();

        let reloads = Arc::new(AtomicUsize::new(0));
        let counter = reloads.clone();
        let task = tokio::spawn(async move {
            wait_for_stop(Some(sighup), None, ipc_rx, engine_rx, hw_rx, move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .await
        });

        // Safe to raise: the handler above is installed process-wide by tokio, so
        // this is caught rather than terminating the test binary.
        unsafe { libc::raise(libc::SIGHUP) };
        let mut seen = false;
        for _ in 0..200 {
            if reloads.load(Ordering::SeqCst) >= 1 {
                seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(seen, "SIGHUP must trigger a config reload");

        // Still waiting — so a real stop signal still works afterwards.
        ipc_tx.send("done".into()).unwrap();
        let out = task.await.unwrap();
        assert_eq!(
            out.reason,
            StopReason::IpcDead,
            "SIGHUP must not have ended the loop"
        );
        assert!(!out.must_restart);
    }

    // ── DEC-266 panic classification + engine supervision ────────────────
    // Both halves decide whether fans go back to firmware control, and both
    // used to be untestable: the classification was inline in the hook, and the
    // engine's death had no consequence at all to observe. Inverting either is
    // silent — the daemon keeps running and `/status` keeps answering — so
    // these pin the decision itself rather than any downstream effect.

    #[test]
    fn a_panic_on_the_main_thread_is_fatal() {
        let main = std::thread::current().id();
        assert!(panic_is_fatal(Some(&main), main));
    }

    #[test]
    fn a_panic_on_a_worker_thread_is_contained() {
        let main = std::thread::current().id();
        let worker = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("worker thread panicked");
        assert_ne!(main, worker, "test needs two genuinely distinct threads");
        assert!(!panic_is_fatal(Some(&main), worker));
    }

    #[test]
    fn an_unknown_main_thread_fails_safe_to_fatal() {
        // If the hook somehow fires before MAIN_THREAD is set, restoring fans is
        // the safe guess: a needless reset beats leaving them latched in manual.
        assert!(panic_is_fatal(None, std::thread::current().id()));
    }

    #[test]
    fn a_running_engine_is_not_reported_as_dead() {
        // The binding test. `spawn_supervised` must hold the guard in a NAMED
        // local across the `.await` — written `let _ = EngineDeathSignal(..)` it
        // drops at construction, the receiver is ready before the main loop even
        // starts, and the daemon restore-and-exits on its first tick. That is a
        // boot crash-loop until systemd's StartLimitBurst gives up, and it
        // compiles and passes every other test in the suite.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let (handle, mut dead_rx) = spawn_supervised(async move {
                let _ = release_rx.await;
            });

            // Give the task a chance to be polled at least once.
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            assert!(
                dead_rx.try_recv().is_err(),
                "a still-running engine must NOT report itself dead"
            );

            let _ = release_tx.send(());
            handle.await.expect("task should not panic");
            assert!(
                dead_rx.await.is_ok(),
                "once the engine ends, its death must be reported"
            );
        });
    }

    #[test]
    fn a_supervised_task_that_panics_still_reports_its_death() {
        // Same wiring, via the panic path — the case the drop guard exists for.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let (handle, dead_rx) = spawn_supervised(async {
                panic!("engine tick body blew up");
            });
            assert!(handle.await.is_err(), "the task should have panicked");
            assert!(
                dead_rx.await.is_ok(),
                "a panicking engine task must report its death"
            );
        });
    }

    #[test]
    fn the_engine_reports_its_death_when_its_task_unwinds() {
        // The case that matters: a panic inside the engine's own tick body. It
        // unwinds past any send placed after the `.await`, so the signal has to
        // ride on Drop. Without it the task dies silently and the daemon keeps
        // running with no PWM writer and no thermal emergency.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let fired = rt.block_on(async {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                let _death = EngineDeathSignal(Some(tx));
                panic!("engine tick body blew up");
            });
            assert!(handle.await.is_err(), "the task should have panicked");
            rx.await.is_ok()
        });
        assert!(fired, "a panicking engine task must report its death");
    }

    #[test]
    fn the_engine_reports_its_death_when_its_task_returns() {
        // A clean return is equally a loss of the writer while the daemon is up.
        // The main loop only listens for this before shutdown is requested, so
        // reporting both ways costs nothing and misses nothing.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let fired = rt.block_on(async {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            tokio::spawn(async move {
                let _death = EngineDeathSignal(Some(tx));
            })
            .await
            .expect("task should not panic");
            rx.await.is_ok()
        });
        assert!(fired, "an engine task that returns must report its death");
    }

    #[test]
    fn overlay_applies_every_dec243_key() {
        let mut config = DaemonConfig::default();
        let mut runtime = RuntimeConfig::default();
        runtime.set_serial_port(Some("/dev/ttyACM7".into()));
        runtime.set_serial_timeout_ms(Some(750));
        runtime.set_poll_interval_ms(Some(1500));
        runtime.set_allow_port_probe(Some(true));
        runtime.set_enable_nvidia_telemetry(Some(true));
        runtime.set_exit_floor_pct(65);

        apply_runtime_overlay(&mut config, &runtime, "/etc/control-ofc/daemon.toml");

        assert_eq!(config.serial.port.as_deref(), Some("/dev/ttyACM7"));
        assert_eq!(config.serial.timeout_ms, 750);
        assert_eq!(config.polling.poll_interval_ms, 1500);
        assert!(config.detection.allow_port_probe);
        assert!(config.detection.enable_nvidia_telemetry);
        assert_eq!(config.shutdown.exit_floor_pct, 65);
    }

    /// DEC-388: `runtime.toml` is not re-validated, and a duty above 100 %
    /// means nothing to either backend — clamped by BOTH copies of the merge.
    #[test]
    fn an_exit_floor_above_100_is_clamped_by_both_copies_of_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        let admin_path = dir.path().join("daemon.toml");
        std::fs::write(&admin_path, "").unwrap();
        let runtime_path = dir.path().join("runtime.toml");
        let mut runtime = RuntimeConfig::default();
        runtime.set_exit_floor_pct(150);
        runtime.save_to(&runtime_path).unwrap();

        let mut via_overlay = DaemonConfig::load(admin_path.to_str().unwrap()).unwrap();
        apply_runtime_overlay(
            &mut via_overlay,
            &RuntimeConfig::load_from(&runtime_path),
            admin_path.to_str().unwrap(),
        );
        let (via_api, _) = control_ofc_daemon::api::handlers::config::effective_on_disk_paths(
            admin_path.to_str().unwrap(),
            &runtime_path,
        );

        assert_eq!(via_overlay.shutdown.exit_floor_pct, 100);
        assert_eq!(via_api.shutdown.exit_floor_pct, 100);
    }

    #[test]
    fn overlay_clamps_a_poll_cadence_the_safety_rule_cannot_supervise() {
        // DEC-270. `daemon.toml` bounds poll_interval_ms only as >= 100, so a
        // hand-edited `poll_interval_ms = 3600000` used to reach the engine
        // intact. The thermal-emergency rule's staleness budget is capped at 30 s, so every
        // reading arrived already older than its budget: `hottest_cpu_reading`
        // never returned `Fresh`, the emergency ladder never ran, and the fans
        // sat at NO_SENSOR_SAFE_PCT — with `/status` reporting a healthy engine.
        let mut config = DaemonConfig::default();
        config.polling.poll_interval_ms = 3_600_000;

        apply_runtime_overlay(&mut config, &RuntimeConfig::default(), "/etc/x.toml");

        assert_eq!(
            config.polling.poll_interval_ms, MAX_SUPERVISABLE_POLL_INTERVAL_MS,
            "an unsupervisable cadence must be clamped, not honoured"
        );
    }

    #[test]
    fn overlay_clamps_an_unsupervisable_cadence_from_the_runtime_overlay_too() {
        // The overlay wins over the admin file, so the clamp has to run after it
        // — not on the loaded config before the merge.
        let mut config = DaemonConfig::default();
        let mut runtime = RuntimeConfig::default();
        runtime.set_poll_interval_ms(Some(600_000));

        apply_runtime_overlay(&mut config, &runtime, "/etc/x.toml");

        assert_eq!(
            config.polling.poll_interval_ms,
            MAX_SUPERVISABLE_POLL_INTERVAL_MS
        );
    }

    #[test]
    fn overlay_leaves_a_supervisable_cadence_untouched() {
        // The clamp must not quietly speed up a legitimate slow-poll setup.
        let mut config = DaemonConfig::default();
        config.polling.poll_interval_ms = MAX_SUPERVISABLE_POLL_INTERVAL_MS;

        apply_runtime_overlay(&mut config, &RuntimeConfig::default(), "/etc/x.toml");

        assert_eq!(
            config.polling.poll_interval_ms, MAX_SUPERVISABLE_POLL_INTERVAL_MS,
            "exactly at the maximum is supervisable and must be honoured"
        );
    }

    #[test]
    fn overlay_leaves_admin_values_alone_when_runtime_is_empty() {
        // "Not overridden" must be distinguishable from "set to the default",
        // or an untouched runtime.toml would silently shadow the admin file.
        let mut config = DaemonConfig::default();
        config.serial.port = Some("/dev/ttyUSB3".into());
        config.serial.timeout_ms = 321;
        config.polling.poll_interval_ms = 4321;
        config.detection.allow_port_probe = true;
        config.detection.enable_nvidia_telemetry = true;

        apply_runtime_overlay(&mut config, &RuntimeConfig::default(), "/etc/x.toml");

        assert_eq!(config.serial.port.as_deref(), Some("/dev/ttyUSB3"));
        assert_eq!(config.serial.timeout_ms, 321);
        assert_eq!(config.polling.poll_interval_ms, 4321);
        assert!(config.detection.allow_port_probe);
        assert!(config.detection.enable_nvidia_telemetry);
    }

    #[test]
    fn overlay_can_turn_a_detection_opt_in_back_off() {
        // `false` is a real override, not "absent" — an operator must be able to
        // revoke an opt-in the admin file enabled.
        let mut config = DaemonConfig::default();
        config.detection.allow_port_probe = true;
        let mut runtime = RuntimeConfig::default();
        runtime.set_allow_port_probe(Some(false));

        apply_runtime_overlay(&mut config, &runtime, "/etc/x.toml");
        assert!(!config.detection.allow_port_probe);
    }

    #[test]
    fn the_get_config_copy_clamps_an_unsupervisable_cadence_the_same_way() {
        // DEC-270. The parity test below cannot catch this — its fixture is
        // 1750 ms, well inside the supervisable range. Clamping in
        // `apply_runtime_overlay` alone made `GET /config` report the
        // hand-edited value while the process ran the clamped one, so
        // `config_key`'s `pending = requires_restart && value != running`
        // latched true forever and the GUI advised a restart that could never
        // clear it: exactly the drift `effective_on_disk_paths`' own doc
        // comment warns about.
        let dir = tempfile::tempdir().unwrap();
        let admin_path = dir.path().join("daemon.toml");
        std::fs::write(&admin_path, "[polling]\npoll_interval_ms = 3600000\n").unwrap();
        let runtime_path = dir.path().join("runtime.toml");

        let mut via_overlay = DaemonConfig::load(admin_path.to_str().unwrap()).unwrap();
        apply_runtime_overlay(
            &mut via_overlay,
            &RuntimeConfig::load_from(&runtime_path),
            admin_path.to_str().unwrap(),
        );

        let (via_api, _) = control_ofc_daemon::api::handlers::config::effective_on_disk_paths(
            admin_path.to_str().unwrap(),
            &runtime_path,
        );

        assert_eq!(
            via_overlay.polling.poll_interval_ms, MAX_SUPERVISABLE_POLL_INTERVAL_MS,
            "the running config must be clamped"
        );
        assert_eq!(
            via_api.polling.poll_interval_ms, via_overlay.polling.poll_interval_ms,
            "GET /config must report what the daemon actually runs, or restart_pending never clears"
        );
    }

    #[test]
    fn overlay_matches_the_get_config_copy_of_the_same_merge() {
        // Pins the two independent implementations to each other. If a sixth key
        // is added to one and not the other, this fails rather than shipping a
        // setting that persists, reports pending, and never applies.
        let dir = tempfile::tempdir().unwrap();
        let admin_path = dir.path().join("daemon.toml");
        std::fs::write(
            &admin_path,
            "[serial]\ntimeout_ms = 400\n\n[polling]\npoll_interval_ms = 900\n",
        )
        .unwrap();
        let runtime_path = dir.path().join("runtime.toml");

        let mut runtime = RuntimeConfig::default();
        runtime.set_serial_port(Some("/dev/ttyACM2".into()));
        runtime.set_poll_interval_ms(Some(1750));
        runtime.set_allow_port_probe(Some(true));
        runtime.set_exit_floor_pct(70);
        runtime.save_to(&runtime_path).unwrap();

        let mut via_overlay = DaemonConfig::load(admin_path.to_str().unwrap()).unwrap();
        apply_runtime_overlay(
            &mut via_overlay,
            &RuntimeConfig::load_from(&runtime_path),
            admin_path.to_str().unwrap(),
        );

        let (via_api, _) = control_ofc_daemon::api::handlers::config::effective_on_disk_paths(
            admin_path.to_str().unwrap(),
            &runtime_path,
        );

        assert_eq!(via_overlay.serial.port, via_api.serial.port);
        assert_eq!(via_overlay.serial.timeout_ms, via_api.serial.timeout_ms);
        assert_eq!(
            via_overlay.polling.poll_interval_ms,
            via_api.polling.poll_interval_ms
        );
        assert_eq!(
            via_overlay.detection.allow_port_probe,
            via_api.detection.allow_port_probe
        );
        assert_eq!(
            via_overlay.detection.enable_nvidia_telemetry,
            via_api.detection.enable_nvidia_telemetry
        );
        assert_eq!(
            via_overlay.profiles.search_dirs,
            via_api.profiles.search_dirs
        );
        assert_eq!(via_overlay.startup.delay_secs, via_api.startup.delay_secs);
        assert_eq!(via_overlay.shutdown.exit_floor_pct, 70, "precondition");
        assert_eq!(
            via_overlay.shutdown.exit_floor_pct,
            via_api.shutdown.exit_floor_pct
        );
    }

    // ── [SAFETY] DEC-388: the exit floor ─────────────────────────────────

    #[test]
    fn a_zero_exit_floor_is_off() {
        assert_eq!(
            apply_exit_floor(None, None, 0, Duration::from_secs(1)),
            ExitFloor::Off
        );
    }

    /// [SAFETY] Through the REAL controller over real files: a header with no
    /// mode switch that the daemon left below the floor is raised to it, and a
    /// header WITH one is left alone — its mode is DEC-382's hand-back to give.
    #[test]
    fn the_exit_floor_raises_a_no_mode_header_and_leaves_moded_ones_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (ctrl, _ledger) = controller_with_taken(
            vec![
                handback_header(d, 1, None),
                handback_header(d, 2, Some("2")),
            ],
            &["h1", "h2"],
        );
        let raw_60 = control_ofc_daemon::pwm::percent_to_raw(60).to_string();
        assert_eq!(
            read_trimmed(&d.join("pwm1")),
            raw_60,
            "precondition: taken at 60 %"
        );

        let outcome = apply_exit_floor(None, Some(ctrl), 80, Duration::from_secs(3));

        assert_eq!(
            outcome,
            ExitFloor::Done {
                raised: 1,
                failed: 0
            }
        );
        assert_eq!(
            read_trimmed(&d.join("pwm1")),
            control_ofc_daemon::pwm::percent_to_raw(80).to_string(),
            "the no-mode header is raised to the floor"
        );
        assert_eq!(
            read_trimmed(&d.join("pwm2")),
            raw_60,
            "the moded header's duty is not the exit floor's to write"
        );
        assert_eq!(read_trimmed(&d.join("pwm2_enable")), "1");
    }

    /// A controller a wedged write still holds cannot be read, so the step gives
    /// up at its deadline rather than holding the stop — and says so.
    #[test]
    fn a_locked_controller_makes_the_exit_floor_incomplete_within_its_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctrl, _ledger) =
            controller_with_taken(vec![handback_header(tmp.path(), 1, None)], &["h1"]);
        let held = ctrl.lock();
        let started = Instant::now();

        let outcome = apply_exit_floor(
            None,
            Some(Arc::clone(&ctrl)),
            80,
            Duration::from_millis(150),
        );

        assert_eq!(outcome, ExitFloor::Incomplete);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the step must give up at its deadline, took {:?}",
            started.elapsed()
        );
        drop(held);
    }

    /// [SAFETY] The exit floor runs FIRST in the restore. Under a watchdog stop
    /// the abort window is 10 s, not `TimeoutStopSec`'s 40: the engine drain
    /// plus these two steps fit inside it, and what follows may not.
    /// `ExecStopPost` can redo the GPU reset and the hwmon hand-back; it cannot
    /// redo this.
    #[test]
    fn the_exit_floor_runs_first_in_the_restore() {
        let whole = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));
        let src = whole
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("main.rs has a #[cfg(test)] module");
        let call = src
            .find("    finish_shutdown(\n")
            .expect("async_main calls finish_shutdown");
        let restore = &src[call..];
        let floor = restore
            .find("apply_exit_floor(")
            .expect("the restore applies the exit floor");
        let gpu = restore
            .find("restore_gpu_fans_to_auto(")
            .expect("GPU reset");
        let hwmon = restore.find("hand_back_hwmon(").expect("hwmon hand-back");
        assert!(
            floor < gpu && floor < hwmon,
            "the exit floor must run before the GPU reset and the hwmon hand-back"
        );
    }

    /// [SAFETY] A watchdog stop is SIGTERM under `TimeoutAbortSec`, which must
    /// cover the engine drain (the hung engine — assumed to be the only task that
    /// will not drain; the unit's comment gives the limit of that assumption)
    /// and both exit-floor steps — each bounded by `SHUTDOWN_TASK_TIMEOUT` — and
    /// must be shorter than the ordinary `TimeoutStopSec`, or it buys nothing.
    #[test]
    fn the_watchdog_abort_window_covers_the_drain_and_the_exit_floor() {
        let unit = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../packaging/control-ofc-daemon.service"
        ))
        .expect("read the daemon unit");
        let secs = |key: &str| -> u64 {
            unit.lines()
                .map(str::trim)
                .filter(|l| !l.starts_with('#'))
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .unwrap_or_else(|| panic!("the unit sets {key}"))
                .parse()
                .unwrap_or_else(|_| panic!("{key} is a bare number of seconds"))
        };
        let abort = Duration::from_secs(secs("TimeoutAbortSec"));
        let needed = SHUTDOWN_TASK_TIMEOUT * 3;
        assert!(
            abort >= needed,
            "TimeoutAbortSec {abort:?} cannot fit the engine drain and the two exit-floor \
             steps ({needed:?})"
        );
        assert!(abort < Duration::from_secs(secs("TimeoutStopSec")));
    }

    /// [SAFETY] The ordinary stop window must cover the bounded stop's worst
    /// case, as the unit's `TimeoutStopSec` comment derives it: four task drains
    /// and the runtime teardown, plus every restore step at its deadline — the two
    /// exit-floor steps (DEC-388), the GPU reset, the hwmon lock and its writes.
    /// DEC-388's two steps took the old 30 s to within 1 s of that; this fails
    /// the next time a step is added without the window growing with it.
    #[test]
    fn the_stop_window_covers_the_bounded_stop() {
        let unit = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../packaging/control-ofc-daemon.service"
        ))
        .expect("read the daemon unit");
        let stop: u64 = unit
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .find_map(|l| l.strip_prefix("TimeoutStopSec="))
            .expect("the unit sets TimeoutStopSec")
            .parse()
            .expect("TimeoutStopSec is a bare number of seconds");
        let drains = SHUTDOWN_TASK_TIMEOUT * 4 + RUNTIME_SHUTDOWN_TIMEOUT;
        let restore = SHUTDOWN_TASK_TIMEOUT * 5;
        assert!(
            Duration::from_secs(stop) > drains + restore,
            "TimeoutStopSec={stop} does not cover the bounded stop ({:?})",
            drains + restore
        );
    }

    /// DEC-388: the exit floor applies live, so a SIGHUP re-applies it from the
    /// files, as it does the search dirs.
    #[test]
    fn a_config_reload_reapplies_the_exit_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("daemon.toml");
        std::fs::write(&config_path, "[shutdown]\nexit_floor_pct = 40\n").unwrap();
        let runtime_path = tmp.path().join("runtime.toml");
        let cache = StateCache::new();
        assert_ne!(cache.exit_floor_pct(), 40, "precondition");

        apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &parking_lot::RwLock::new(Vec::new()),
            &parking_lot::RwLock::new(None),
            &cache,
        )
        .unwrap();

        assert_eq!(cache.exit_floor_pct(), 40);
    }

    // ── [SAFETY] serial port fallback (DEC-243) ──────────────────────────

    #[test]
    fn configured_port_is_tried_first_but_detection_still_follows() {
        // The whole point: a configured port must not be the ONLY candidate.
        let c = control_ofc_daemon::serial::adoption::serial_port_candidates(
            Some("/dev/ttyACM9"),
            || Some("/dev/ttyACM0".into()),
        );
        assert_eq!(c, vec!["/dev/ttyACM9", "/dev/ttyACM0"]);
    }

    #[test]
    fn no_configured_port_falls_back_to_detection() {
        let c = control_ofc_daemon::serial::adoption::serial_port_candidates(None, || {
            Some("/dev/ttyACM0".into())
        });
        assert_eq!(c, vec!["/dev/ttyACM0"]);
    }

    #[test]
    fn detected_port_equal_to_configured_is_not_retried() {
        let c = control_ofc_daemon::serial::adoption::serial_port_candidates(
            Some("/dev/ttyACM0"),
            || Some("/dev/ttyACM0".into()),
        );
        assert_eq!(
            c,
            vec!["/dev/ttyACM0"],
            "no point opening the same path twice"
        );
    }

    #[test]
    fn nothing_configured_and_nothing_detected_yields_no_candidates() {
        let c = control_ofc_daemon::serial::adoption::serial_port_candidates(None, || None);
        assert!(c.is_empty());
    }

    #[test]
    fn a_dead_configured_port_cannot_suppress_detection() {
        // REGRESSION: the pre-fix `configured.or_else(detect)` returned exactly
        // one candidate here, so an unprivileged user who persisted a dead path
        // durably removed OpenFan control — and with it the thermal emergency's
        // only path to those fans. Detection must still be reachable.
        let detect_called = std::cell::Cell::new(false);
        let c = control_ofc_daemon::serial::adoption::serial_port_candidates(
            Some("/dev/ttyACM9"),
            || {
                detect_called.set(true);
                Some("/dev/ttyACM0".into())
            },
        );
        assert!(
            detect_called.get(),
            "detection must run even with a port configured"
        );
        assert!(c.contains(&"/dev/ttyACM0".to_string()));
    }

    // ── DEC-250: openability is not identity ─────────────────────────────

    /// A serial device that replies with a fixed script, then times out.
    ///
    /// `ok()` speaks the OpenFanController protocol; `wrong_device()` opens
    /// cleanly and chatters but never answers `ReadAllRpm` — a modem, a printer,
    /// an Arduino, anything else on a `/dev/ttyACM*`.
    use control_ofc_daemon::error::SerialError;

    struct ScriptedPort(std::collections::VecDeque<String>);

    impl ScriptedPort {
        fn ok() -> Self {
            Self(
                vec![concat!(
                    "<00|00:04B0;01:044C;02:0000;03:0000;04:0000;",
                    "05:0000;06:0000;07:0000;08:0000;09:0000;>\r\n"
                )
                .to_string()]
                .into(),
            )
        }
        fn wrong_device() -> Self {
            Self(vec!["ok\r\n".to_string(), "READY\r\n".to_string()].into())
        }
    }

    impl control_ofc_daemon::serial::transport::SerialTransport for ScriptedPort {
        fn write_line(&mut self, _data: &str) -> Result<(), SerialError> {
            Ok(())
        }
        fn read_line(&mut self, _timeout: Duration) -> Result<String, SerialError> {
            self.0
                .pop_front()
                .ok_or(SerialError::Timeout { timeout_ms: 1 })
        }
    }

    /// `OFN-b`: opening asserts DTR, which resets Arduino-class boards, so the
    /// number of opens IS the property — not an efficiency note.
    ///
    /// Boot adoption used to reach hardware through `auto_detect_port`, which
    /// probed the libudev list and then fell through to its own
    /// `/dev/ttyACM0..9` + `/dev/ttyUSB0..9` scan without deduplicating, so every
    /// candidate was opened TWICE per attempt — six attempts deep. Boot now
    /// enumerates without opening and hands the list here, which is the only
    /// place that opens.
    #[test]
    fn each_candidate_is_opened_at_most_once_per_attempt() {
        let candidates = ports(&["/dev/ttyACM0", "/dev/ttyUSB0", "/dev/ttyACM1"]);
        let mut opened: Vec<String> = Vec::new();
        let chosen = control_ofc_daemon::serial::adoption::first_openfan_port(
            &candidates,
            None,
            Duration::from_millis(50),
            |p| {
                opened.push(p.to_string());
                Ok(ScriptedPort::wrong_device())
            },
        );

        assert!(chosen.is_none(), "no candidate identifies in this fixture");
        assert_eq!(
            opened,
            vec!["/dev/ttyACM0", "/dev/ttyUSB0", "/dev/ttyACM1"],
            "every candidate is opened exactly once, in order"
        );
        for c in &candidates {
            assert_eq!(
                opened.iter().filter(|o| *o == c).count(),
                1,
                "{c} was opened more than once — each open is a DTR reset"
            );
        }
    }

    /// `OFN-b`, the CALL SITE — the half a unit test cannot reach.
    ///
    /// `probe_order` and `first_openfan_port` pin "each candidate is opened at
    /// most once", but neither proves BOOT goes through them. The defect was
    /// precisely a call site: boot called `auto_detect_port`, whose own doc
    /// comment says it is not the function to call merely to learn what ports
    /// exist. DEC-291 built the non-opening path for the rescan endpoint and
    /// boot was never moved onto it. This is `CLAUDE.md`'s most-recorded failure
    /// mode — an extracted rule with thorough tests and an untested caller.
    ///
    /// Comment lines are stripped before matching. A source-scanning guard that
    /// matches its own explanation is its own recorded trap: this file's
    /// adoption block *names* `auto_detect_port` in prose to explain why it is
    /// no longer called, and a substring scan would fire on that.
    #[test]
    fn boot_adoption_does_not_open_ports_merely_to_enumerate_them() {
        let src = include_str!("main.rs");
        // Production source only. Scanning the whole file matches THIS TEST's own
        // assertion strings, which name the forbidden function in order to
        // explain it — measured, on the first run of this guard. Stripping
        // comments is not enough for that; the strings are code.
        let production = src
            .split_once("#[cfg(test)]")
            .expect("main.rs has a test module")
            .0;
        let code: String = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !code.contains("auto_detect_port"),
            "boot adoption must not call auto_detect_port — it OPENS every candidate to \
             identify it, and opening asserts DTR, which resets Arduino-class boards. Use \
             enumerate_serial_candidates + first_openfan_port (DEC-291)."
        );
        // A CALL, not a bare identifier: the first draft asserted
        // `contains("enumerate_serial_candidates")`, which the `use` line at the
        // top of this file satisfies on its own — so the guard stayed green for a
        // boot path that had stopped calling it and merely kept the import.
        assert!(
            code.contains("serial_port_candidates_enumerated("),
            "boot adoption must build its candidate list with the non-opening \
             enumerator; asserting the absence above alone would pass against a boot \
             path that adopted nothing at all"
        );
    }

    fn ports(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_port_that_opens_but_is_not_an_openfan_is_not_adopted() {
        // REGRESSION: acceptance used to be "the port opened", and
        // `RealSerialTransport::open` succeeds on any readable tty. A
        // configured-but-wrong port was therefore adopted as the fan controller
        // and the loop stopped there — discarding the correctly detected port
        // sitting next in the candidate list. Writes to an indifferent device
        // return Ok, so nothing surfaced: the thermal emergency's `force_all_with_floor`
        // reported success while driving nothing.
        let chosen = control_ofc_daemon::serial::adoption::first_openfan_port(
            &ports(&["/dev/ttyACM9", "/dev/ttyACM0"]),
            None,
            Duration::from_millis(50),
            |p| {
                Ok(if p == "/dev/ttyACM9" {
                    ScriptedPort::wrong_device()
                } else {
                    ScriptedPort::ok()
                })
            },
        );

        assert_eq!(
            chosen.map(|(p, _)| p),
            Some("/dev/ttyACM0".to_string()),
            "a port that opens but does not answer ReadAllRpm must be skipped, \
             not adopted — and must not stop later candidates being tried"
        );
    }

    #[test]
    fn no_identifying_port_yields_no_controller() {
        // Failing to identify is not fatal, but it must not be papered over
        // either: with nothing that answers, the daemon runs without serial fan
        // control (and says so) rather than holding a handle to the wrong tty.
        let chosen = control_ofc_daemon::serial::adoption::first_openfan_port(
            &ports(&["/dev/ttyACM9"]),
            None,
            Duration::from_millis(50),
            |_| Ok(ScriptedPort::wrong_device()),
        );
        assert!(chosen.is_none());
    }

    #[test]
    fn an_unopenable_candidate_does_not_stop_the_search() {
        // Pre-existing behaviour, pinned: a port that cannot be opened at all is
        // skipped and the next candidate is still tried.
        let chosen = control_ofc_daemon::serial::adoption::first_openfan_port(
            &ports(&["/dev/ttyACM9", "/dev/ttyACM0"]),
            None,
            Duration::from_millis(50),
            |p| {
                if p == "/dev/ttyACM9" {
                    Err(SerialError::Protocol {
                        message: "no such device".into(),
                    })
                } else {
                    Ok(ScriptedPort::ok())
                }
            },
        );
        assert_eq!(chosen.map(|(p, _)| p), Some("/dev/ttyACM0".to_string()));
    }

    // ── Boot-time profile resolution fail-safe (DEC-165) ─────────────────

    #[test]
    fn persisted_profile_resolves_to_none_when_corrupt() {
        // A persisted profile that is corrupt/hand-edited-invalid on disk must
        // resolve to None (imperative mode), never crash startup. This is the
        // boot variant of "profile invalid" — the boot path skips validate(),
        // so load_profile failing safe is the load-bearing net.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active.json");
        std::fs::write(&path, "{ this is not valid json").unwrap();
        let state = daemon_state::DaemonState {
            version: 1,
            active_profile_id: Some("x".into()),
            active_profile_path: Some(path.display().to_string()),
        };
        assert!(
            resolve_persisted_profile(&state, profile::load_profile).is_none(),
            "a corrupt persisted profile must fail safe to no-profile"
        );
    }

    #[test]
    fn persisted_profile_resolves_to_none_without_a_pointer() {
        // No persisted pointer → None, and the loader is never consulted.
        let state = daemon_state::DaemonState {
            version: 1,
            active_profile_id: None,
            active_profile_path: None,
        };
        assert!(resolve_persisted_profile(&state, |_| panic!("loader must not run")).is_none());
    }

    #[test]
    fn persisted_profile_resolves_to_none_when_file_missing() {
        // A pointer to a path that no longer exists → None; loader not run.
        let state = daemon_state::DaemonState {
            version: 1,
            active_profile_id: Some("x".into()),
            active_profile_path: Some("/nonexistent/control-ofc/profile.json".into()),
        };
        assert!(resolve_persisted_profile(&state, |_| panic!("loader must not run")).is_none());
    }

    #[tokio::test]
    async fn shutdown_stops_ipc_server_before_restoring_hardware() {
        // audit P1-A: the IPC server must stop accepting writes before the
        // hardware is given back, else a late client write re-enters
        // manual mode after the restore.
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let (poll_tx, _poll_rx) = tokio::sync::watch::channel(false);
        let (server_tx, server_rx) = tokio::sync::oneshot::channel::<()>();

        // Fake IPC server: records when it stops, only after the signal arrives.
        let order_srv = order.clone();
        let server_handle = tokio::spawn(async move {
            let _ = server_rx.await;
            order_srv.lock().unwrap().push("server_stopped");
        });

        let order_after = order.clone();
        let order_restore = order.clone();
        shutdown_sequence(
            &poll_tx,
            server_tx,
            server_handle,
            vec![],
            Duration::from_secs(3),
            move || order_after.lock().unwrap().push("after_server_stop"),
            move || order_restore.lock().unwrap().push("hardware_restored"),
        )
        .await;

        assert_eq!(
            *order.lock().unwrap(),
            vec!["server_stopped", "after_server_stop", "hardware_restored"],
            "the IPC server must stop before hardware is restored to auto"
        );
    }

    #[tokio::test]
    async fn shutdown_restores_hardware_even_if_ipc_server_hangs() {
        // The bounded wait must elapse and the restore must still run, so a
        // lingering connection (e.g. an SSE stream) can never block the safety
        // restore.
        let restored = Arc::new(Mutex::new(false));
        let (poll_tx, _poll_rx) = tokio::sync::watch::channel(false);
        let (server_tx, server_rx) = tokio::sync::oneshot::channel::<()>();

        // Fake IPC server that never finishes: it ignores the shutdown signal.
        let server_handle = tokio::spawn(async move {
            let _hold = server_rx;
            std::future::pending::<()>().await;
        });

        let restored_c = restored.clone();
        shutdown_sequence(
            &poll_tx,
            server_tx,
            server_handle,
            vec![],
            Duration::from_millis(50),
            || {},
            move || *restored_c.lock().unwrap() = true,
        )
        .await;

        assert!(
            *restored.lock().unwrap(),
            "the hardware restore must run even if the IPC server fails to stop in time"
        );
    }

    // ── DEC-387 (`TS-d`): systemd readiness and watchdog ─────────────────

    /// [SAFETY] The stop is announced — and the watchdog disarmed — BEFORE the
    /// hardware restore runs. systemd re-arms its watchdog on any keep-alive
    /// whatever the unit's state, and the engine keeps completing ticks until
    /// `shutdown_sequence` stops it, so a disarm that came after the restore
    /// began would leave a slow restore exposed to a watchdog kill part-way through.
    /// Measured against systemd 261 in DEC-387; this pins the ordering here.
    #[tokio::test]
    async fn stopping_is_announced_before_the_hardware_restore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notify");
        let rx = std::os::unix::net::UnixDatagram::bind(&path).expect("bind the fake socket");
        rx.set_nonblocking(true).expect("nonblocking receiver");
        let notifier = control_ofc_daemon::sd_notify::Notifier::new(
            path.to_str().expect("utf-8 path"),
            Some(Duration::from_secs(15)),
        )
        .expect("notifier");

        let (poll_tx, _poll_rx) = tokio::sync::watch::channel(false);
        let (server_tx, server_rx) = tokio::sync::oneshot::channel::<()>();
        let server_handle = tokio::spawn(async move {
            let _ = server_rx.await;
        });
        let at_restore: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen = at_restore.clone();
        finish_shutdown(
            Some(&notifier),
            &poll_tx,
            server_tx,
            server_handle,
            vec![],
            Duration::from_secs(3),
            false,
            move || {
                let mut buf = [0u8; 64];
                let got = rx.recv(&mut buf).map_or_else(
                    |e| format!("<nothing: {e}>"),
                    |n| String::from_utf8_lossy(&buf[..n]).into_owned(),
                );
                *seen.lock().unwrap() = Some(got);
            },
        )
        .await;

        assert_eq!(
            at_restore.lock().unwrap().as_deref(),
            Some("STOPPING=1\nWATCHDOG_USEC=0"),
            "by the time the restore runs, systemd must already have been told the \
             daemon is stopping and that its watchdog no longer applies"
        );
    }

    /// A notifier whose queue is FULL: the fake socket is never read, so every
    /// one-shot send after this finds no room until a test task reads it.
    fn notifier_with_a_full_queue(
        dir: &tempfile::TempDir,
    ) -> (
        control_ofc_daemon::sd_notify::Notifier,
        Arc<std::os::unix::net::UnixDatagram>,
    ) {
        let path = dir.path().join("notify");
        let rx = std::os::unix::net::UnixDatagram::bind(&path).expect("bind the fake socket");
        rx.set_nonblocking(true).expect("nonblocking receiver");
        let notifier = control_ofc_daemon::sd_notify::Notifier::new(
            path.to_str().expect("utf-8 path"),
            Some(Duration::from_secs(15)),
        )
        .expect("notifier");
        for _ in 0..5_000 {
            notifier.watchdog_tick();
        }
        (notifier, Arc::new(rx))
    }

    /// Everything queued on `rx` right now.
    fn read_queue(rx: &std::os::unix::net::UnixDatagram) -> Vec<String> {
        let mut buf = [0u8; 64];
        let mut got = Vec::new();
        while let Ok(n) = rx.recv(&mut buf) {
            got.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        got
    }

    /// A task for the drain list that reads the queue once, IN the drain phase.
    /// Time is paused, so its sleep can only elapse when the runtime is idle —
    /// which first happens while `shutdown_sequence` awaits this task. It never
    /// runs while the IPC server is being awaited, however the scheduler orders
    /// the ready tasks.
    fn read_in_the_drain_phase(
        mut poll_rx: tokio::sync::watch::Receiver<bool>,
        rx: Arc<std::os::unix::net::UnixDatagram>,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let _ = poll_rx.wait_for(|stop| *stop).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            seen.lock().unwrap().extend(read_queue(&rx));
        })
    }

    /// [SAFETY] `TS-ay`, DEC-402, at the call site: a disarm lost when the stop
    /// began is sent again once the IPC server has stopped, BEFORE the task
    /// drains, which can run out several task timeouts between them. The queue
    /// is read only by the fake server as it stops, so the first attempt finds
    /// it full and the resend finds room. The drain-phase reader must already
    /// see the disarm; the resend before the restore comes after it and cannot
    /// satisfy this.
    #[tokio::test(start_paused = true)]
    async fn a_lost_disarm_is_resent_before_the_task_drains() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (notifier, rx) = notifier_with_a_full_queue(&dir);

        let (poll_tx, poll_rx) = tokio::sync::watch::channel(false);
        let (server_tx, server_rx) = tokio::sync::oneshot::channel::<()>();
        let at_server_stop: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let server_handle = {
            let rx = Arc::clone(&rx);
            let at_server_stop = Arc::clone(&at_server_stop);
            tokio::spawn(async move {
                let _ = server_rx.await;
                at_server_stop.lock().unwrap().extend(read_queue(&rx));
            })
        };
        let in_drains: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let reader = read_in_the_drain_phase(poll_rx, Arc::clone(&rx), Arc::clone(&in_drains));
        finish_shutdown(
            Some(&notifier),
            &poll_tx,
            server_tx,
            server_handle,
            vec![("reader", reader)],
            Duration::from_secs(3),
            false,
            || {},
        )
        .await;

        let at_server_stop = at_server_stop.lock().unwrap();
        assert!(
            !at_server_stop.is_empty() && !at_server_stop.iter().any(|m| m.starts_with("STOPPING")),
            "precondition: the queue was full and the first disarm was lost; \
             read at the server stop: {} message(s)",
            at_server_stop.len()
        );
        assert_eq!(
            in_drains.lock().unwrap().as_slice(),
            ["STOPPING=1\nWATCHDOG_USEC=0"],
            "the disarm must be sent again after the server stop and land before the drains"
        );
    }

    /// [SAFETY] `TS-ap`, at the call site: a disarm lost to a queue that stays
    /// full through BOTH earlier attempts — the stop's own and the one after the
    /// server stop (DEC-402) — is delivered before the restore runs. The queue is
    /// read only in the drain phase, so only the resend inside the restore
    /// closure can land the disarm in time.
    #[tokio::test(start_paused = true)]
    async fn a_lost_disarm_is_resent_before_the_hardware_restore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (notifier, rx) = notifier_with_a_full_queue(&dir);

        let (poll_tx, poll_rx) = tokio::sync::watch::channel(false);
        let (server_tx, server_rx) = tokio::sync::oneshot::channel::<()>();
        let server_handle = tokio::spawn(async move {
            let _ = server_rx.await;
        });
        let drained: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let drainer = read_in_the_drain_phase(poll_rx, Arc::clone(&rx), Arc::clone(&drained));
        let at_restore: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
        let seen = at_restore.clone();
        finish_shutdown(
            Some(&notifier),
            &poll_tx,
            server_tx,
            server_handle,
            vec![("drainer", drainer)],
            Duration::from_secs(3),
            false,
            move || *seen.lock().unwrap() = Some(read_queue(&rx)),
        )
        .await;

        let drained = drained.lock().unwrap();
        assert!(
            !drained.is_empty() && !drained.iter().any(|m| m.starts_with("STOPPING")),
            "precondition: the first two disarms were lost to the full queue; \
             drained {} message(s)",
            drained.len()
        );
        assert_eq!(
            at_restore.lock().unwrap().as_deref(),
            Some(["STOPPING=1\nWATCHDOG_USEC=0".to_string()].as_slice()),
            "the disarm must be sent again, and land, before the restore runs"
        );
    }

    /// Held by every test that raises SIGUSR1 or SIGUSR2. A signal is delivered
    /// to the whole process, and tokio hands it to every stream registered for
    /// it, whichever test's runtime owns the stream — so two of these tests
    /// running at once would each see the other's signals. Taken BEFORE the
    /// streams are registered, since a stream only sees signals raised after it.
    static SLEEP_SIGNALS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// `TS-ao`: the hook's two signals reach `watch_sleep` as the two transitions.
    /// Raised for real, so a swapped or dropped arm fails here.
    #[tokio::test]
    async fn the_sleep_hook_signals_become_sleep_transitions() {
        use control_ofc_daemon::sd_notify::SleepTransition;
        use tokio::signal::unix::{signal, SignalKind};
        let _only_us = SLEEP_SIGNALS.lock().await;
        let usr1 = signal(SignalKind::user_defined1()).expect("SIGUSR1 must be registerable");
        let usr2 = signal(SignalKind::user_defined2()).expect("SIGUSR2 must be registerable");
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let task = tokio::spawn(forward_sleep_signals(usr1, usr2, tx));

        for (sig, want) in [
            (libc::SIGUSR1, SleepTransition::Entering),
            (libc::SIGUSR2, SleepTransition::Resumed),
        ] {
            // SAFETY: raising a signal whose handler tokio installed above.
            unsafe { libc::raise(sig) };
            let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a transition within 5 s");
            assert_eq!(got, Some(want));
        }
        task.abort();
    }

    /// `TS-az`, DEC-402: when a sleep signal and a resume signal are BOTH pending,
    /// the resume is forwarded first. The daemon cannot tell which arrived first
    /// (tokio keeps no order across signals), and resume-first is the order whose
    /// wrong guess is bounded: a pre and a post from one sleep end wide until
    /// `watch_sleep`'s fallback narrows it, whereas sleep-first would leave the
    /// watchdog narrow across the next sleep when a post and the next pre meet.
    ///
    /// Raised pre-then-post, the order of one sleep, and the forwarder is only
    /// spawned after a sleep that parks this runtime, so its driver has handed
    /// both signals to the streams first. The select then sees both at once, and
    /// only the arm order decides.
    #[tokio::test]
    async fn a_pending_resume_is_forwarded_before_a_pending_sleep() {
        use control_ofc_daemon::sd_notify::SleepTransition;
        use tokio::signal::unix::{signal, SignalKind};
        let _only_us = SLEEP_SIGNALS.lock().await;
        let usr1 = signal(SignalKind::user_defined1()).expect("SIGUSR1 must be registerable");
        let usr2 = signal(SignalKind::user_defined2()).expect("SIGUSR2 must be registerable");
        // SAFETY: raising signals whose handlers tokio installed above.
        unsafe {
            libc::raise(libc::SIGUSR1);
            libc::raise(libc::SIGUSR2);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let task = tokio::spawn(forward_sleep_signals(usr1, usr2, tx));
        let mut got = Vec::new();
        for _ in 0..2 {
            got.push(
                tokio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("a transition within 5 s"),
            );
        }
        task.abort();
        assert_eq!(
            got,
            [
                Some(SleepTransition::Resumed),
                Some(SleepTransition::Entering)
            ]
        );
    }

    /// The CALL SITES of both DEC-387 start-up rules, which no in-process test
    /// can reach: `async_main` is never run by the suite. Same tool and reasoning
    /// as `the_shutdown_restore_goes_through_the_bounded_helper`.
    ///
    /// READY=1 must follow everything it vouches for — the engine spawned, the
    /// API server spawned, SIGTERM's handler registered — and precede the wait.
    /// Sent early, systemd would start the watchdog clock (and release
    /// `multi-user.target`) for a daemon that is not yet controlling anything; a
    /// stop arriving in between would kill it without the graceful restore.
    #[test]
    fn readiness_is_reported_after_what_it_vouches_for_and_the_delay_is_capped() {
        let whole = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));
        let src = whole
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("main.rs has a #[cfg(test)] module");

        let at = |needle: &str| {
            src.find(needle)
                .unwrap_or_else(|| panic!("async_main no longer contains `{needle}`"))
        };
        assert_eq!(
            src.matches(".ready()").count(),
            1,
            "READY=1 must be sent from exactly one place"
        );
        let ready = at(".ready()");
        for before in [
            "profile_engine::profile_engine_loop(",
            "server::serve(",
            "SignalKind::terminate()",
            "start_sleep_watch(n);",
        ] {
            assert!(
                at(before) < ready,
                "READY=1 is sent before `{before}` — systemd would call the daemon \
                 ready before it is"
            );
        }
        assert!(
            ready < at("wait_for_stop(\n            sighup,"),
            "READY=1 must be sent before the main loop starts waiting"
        );

        // Each boot-time serial probe extends the start deadline by its own
        // bound, so start-up is limited by progress rather than by an assumed
        // number of candidates — the extension must sit INSIDE the probe closure,
        // ahead of the open it pays for.
        let probe = at("serial::adoption::first_openfan_port(");
        let extend = at(".extend_timeout(");
        let open = at("RealSerialTransport::open(p, serial_timeout)");
        assert!(
            probe < extend && extend < open,
            "the start-timeout extension must be requested inside the boot probe \
             closure, before each candidate is opened"
        );

        assert!(
            src.contains("effective_startup_delay(config.startup.delay_secs)"),
            "the startup delay must go through the cap"
        );
        assert!(
            !src.contains("from_secs(config.startup.delay_secs)"),
            "the uncapped delay is being slept directly again"
        );
    }

    /// `runtime.toml` bypasses both setters' validation, and under `Type=notify`
    /// an uncapped delay would outlast `TimeoutStartSec=` on every boot.
    #[test]
    fn the_startup_delay_is_capped_at_the_documented_maximum() {
        let max = control_ofc_daemon::constants::MAX_STARTUP_DELAY_SECS;
        assert_eq!(effective_startup_delay(0), 0);
        assert_eq!(effective_startup_delay(max), max);
        assert_eq!(effective_startup_delay(max + 1), max);
        assert_eq!(effective_startup_delay(600), max);
    }

    // ── 273-b / 273-a: bounded process exit, and the restart exit's ordering ──

    /// Env var carrying the marker path to the re-executed child (273-a).
    /// Its presence is what puts the child on the probe branch.
    const RESTART_PROBE_MARKER: &str = "CONTROL_OFC_RESTART_PROBE_MARKER";
    /// Same, for the clean-stop control case.
    const CLEAN_PROBE_MARKER: &str = "CONTROL_OFC_CLEAN_PROBE_MARKER";

    /// Re-execute this test binary, running only `test_name`, with `var` set to
    /// `marker`. Returns the child's exit status.
    ///
    /// `std::process::exit` cannot be observed in-process, so the only way to
    /// assert on it is to be a different process. `current_exe()` here is this
    /// bin target's own libtest harness, so `--exact` re-enters exactly one test
    /// — which then takes its probe branch because the env var is set.
    fn run_probe_child(
        test_name: &str,
        var: &str,
        marker: &std::path::Path,
    ) -> std::process::ExitStatus {
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(exe)
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(var, marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("re-exec the test binary");

        // Bounded wait, not `output()`. A wedged child would otherwise hang CI
        // instead of failing it — the DEC-272 trap-3 shape, one process out.
        // Today the child is bounded only incidentally by its own
        // SHUTDOWN_TASK_TIMEOUT; nothing pinned that, so pin it here.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("probe child '{test_name}' did not exit within 30s");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Drive the real `finish_shutdown` with a restore closure that stamps
    /// `marker`. Shared by both probe children so they differ only in the flag.
    fn probe_finish_shutdown(marker: std::path::PathBuf, must_restart: bool) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("child runtime");
        runtime.block_on(async move {
            let (poll_tx, _poll_rx) = tokio::sync::watch::channel(false);
            let (server_tx, server_rx) = tokio::sync::oneshot::channel::<()>();
            let server_handle = tokio::spawn(async move {
                let _ = server_rx.await;
            });
            finish_shutdown(
                None,
                &poll_tx,
                server_tx,
                server_handle,
                vec![],
                Duration::from_secs(3),
                must_restart,
                move || std::fs::write(&marker, b"restored").expect("stamp the restore marker"),
            )
            .await;
        });
    }

    /// [SAFETY] 273-a — `must_restart` must exit non-zero, and must do so only
    /// AFTER the hardware has been restored.
    ///
    /// Both halves were unpinned: `std::process::exit` cannot be observed
    /// in-process, so deleting the exit — or hoisting it above the restore —
    /// left the entire suite green while silently discarding DEC-266/267.
    ///
    /// This runs the real `finish_shutdown` in a re-executed child and reads two
    /// independent signals off it:
    ///   * **exit code 1** — the exit ran at all. Delete it and the child falls
    ///     through to `exit(97)` instead, so the assertion reds rather than
    ///     passing vacuously (the DEC-272 "a test asserting an absence must first
    ///     assert the presence" trap, applied to a diverging call).
    ///   * **the marker file** — the restore ran BEFORE the exit. Hoist the exit
    ///     above `shutdown_sequence` and the code is still 1, but the marker is
    ///     gone. That file is the ordering assertion.
    #[test]
    fn must_restart_exits_nonzero_after_restoring_hardware() {
        if let Ok(marker) = std::env::var(RESTART_PROBE_MARKER) {
            probe_finish_shutdown(marker.into(), true);
            // `finish_shutdown` must diverge when must_restart is set. Reaching
            // here means the exit was removed; 97 is distinguishable from both
            // the expected 1 and libtest's own 0/101.
            std::process::exit(97);
        }

        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("restored");
        let status = run_probe_child(
            "tests::must_restart_exits_nonzero_after_restoring_hardware",
            RESTART_PROBE_MARKER,
            &marker,
        );

        assert_eq!(
            status.code(),
            Some(1),
            "a must_restart shutdown must exit(1) so Restart=on-failure revives the daemon \
             with a live engine; 97 means the exit was deleted, 0/101 that the child never \
             reached it"
        );
        assert!(
            marker.exists(),
            "the hardware restore must have run BEFORE the exit — an exit hoisted above \
             shutdown_sequence would leave fans latched at the dead engine's last duty"
        );
    }

    /// The control case: a clean stop must NOT exit non-zero, and must still
    /// restore. Without this, an unconditional `exit(1)` would satisfy the test
    /// above — systemd would then see every requested stop as a failure and
    /// restart the daemon forever.
    #[test]
    fn a_clean_stop_restores_hardware_and_does_not_force_a_restart() {
        if let Ok(marker) = std::env::var(CLEAN_PROBE_MARKER) {
            probe_finish_shutdown(marker.into(), false);
            return; // must return normally → libtest exits 0
        }

        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("restored");
        let status = run_probe_child(
            "tests::a_clean_stop_restores_hardware_and_does_not_force_a_restart",
            CLEAN_PROBE_MARKER,
            &marker,
        );

        assert_eq!(
            status.code(),
            Some(0),
            "a clean stop must not force a restart — an unconditional exit(1) would make \
             systemd restart the daemon after every requested stop"
        );
        assert!(
            marker.exists(),
            "a clean stop must still restore the hardware"
        );
    }

    /// [SAFETY] 273-b — the mechanism `main` relies on to bound its own exit.
    ///
    /// DEC-272 made an outstanding wedged `spawn_blocking` read a *designed*
    /// steady state, and tokio's `Runtime::drop` waits for spawned work forever
    /// (<https://docs.rs/tokio/1.50.0/tokio/runtime/struct.Runtime.html>). This
    /// pins that `shutdown_timeout` really does unblock the shutdown thread, so
    /// the version-specific behaviour the fix depends on is executable rather
    /// than remembered. That we *call* it is a separate assertion —
    /// `main_owns_its_runtime_so_teardown_is_bounded` below.
    ///
    /// The wedge carries a SELF-RELEASE DEADLINE. A failed assertion skips this
    /// test's own cleanup, and an unbounded blocked thread turns a red test into
    /// a hung CI job (DEC-272 trap 3). 30 s is far outside the 5 s the assertion
    /// allows, so it can never mask a real failure.
    #[test]
    fn a_wedged_blocking_task_cannot_stall_runtime_teardown() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let released = Arc::new(AtomicBool::new(false));
        let wedge = released.clone();
        runtime.spawn_blocking(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !wedge.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        // Let the blocking task actually reach the pool, so teardown has
        // something outstanding to wait on rather than racing an empty queue.
        std::thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        runtime.shutdown_timeout(Duration::from_millis(100));
        let elapsed = started.elapsed();
        released.store(true, Ordering::SeqCst);

        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown_timeout must abandon a wedged blocking task, not wait for it; \
             took {elapsed:?}"
        );
    }

    /// [SAFETY] 273-b — the call site, which behaviour cannot see.
    ///
    /// `main` returning from under `#[tokio::main]` drops the runtime, and that
    /// drop blocks forever on the wedged read DEC-272 designs for. The bound only
    /// exists if `main` owns the runtime and calls `shutdown_timeout` on it, and
    /// no in-process test can observe a `main` that is never called. Same tool
    /// and same reasoning as `polling.rs`'s biased-select pin.
    #[test]
    fn main_owns_its_runtime_so_teardown_is_bounded() {
        let whole = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));
        // Production code only — scanning the whole file makes this test match
        // its OWN string literals, which is how the polling.rs version of this
        // guard first passed while the production selects were unbiased.
        let src = whole
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("main.rs has a #[cfg(test)] module");

        // Matched in ATTRIBUTE POSITION, not as a substring: `fn main`'s own doc
        // comment names the attribute to explain why it is gone, and a bare
        // `contains` hit that comment and failed on the first run.
        assert!(
            !src.lines()
                .any(|l| l.trim_start().starts_with("#[tokio::main]")),
            "#[tokio::main] holds the runtime as a temporary and drops it when main \
             returns — an unbounded wait on any outstanding blocking task"
        );
        assert!(
            src.contains("runtime.block_on(async_main())"),
            "main must drive async_main on a runtime it owns"
        );
        assert!(
            src.contains("catch_unwind"),
            "main must catch a panic from async_main, or the unwind drops the \
             runtime and restores the unbounded wait on the very path where \
             things are already going wrong"
        );
        assert!(
            src.contains("runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT)"),
            "main must bound the runtime teardown; without this the process hangs \
             until systemd's TimeoutStopSec SIGKILL, and forever outside systemd"
        );
    }

    // ── 277-b: the safety restore must be bounded ───────────────────────
    // The engine write path holds the controller mutex across an uncancellable
    // `spawn_blocking` sysfs write, so a chip wedged mid-write holds it for as
    // long as it stays wedged. Everything that could otherwise backstop a stall
    // in the restore — SHUTDOWN_TASK_TIMEOUT, RUNTIME_SHUTDOWN_TIMEOUT, the
    // `must_restart` exit(1), TimeoutStopSec, ExecStopPost — runs AFTER it:
    // ExecStopPost only once the process has exited, and TimeoutStopSec only
    // where systemd has a stop in progress, which a self-stop lacked until
    // DEC-387 made it send STOPPING=1.

    use control_ofc_daemon::hwmon::pwm_discovery::PwmHeaderDescriptor;

    // ── DEC-382: give back exactly what was taken ────────────────────────
    // These go through the REAL take — `set_pwm` over real files — so each
    // original is recorded the way production records it, and a wedge is swapped
    // in only afterwards. A ledger primed by hand would test the replay and not
    // the thing it replays.

    /// A header over real files in `dir`: `pwmN`, and `pwmN_enable` starting at
    /// `mode` when there is one.
    fn handback_header(dir: &Path, n: u8, mode: Option<&str>) -> PwmHeaderDescriptor {
        let pwm = dir.join(format!("pwm{n}"));
        std::fs::write(&pwm, "90\n").unwrap();
        let enable = mode.map(|m| {
            let p = dir.join(format!("pwm{n}_enable"));
            std::fs::write(&p, format!("{m}\n")).unwrap();
            p.to_string_lossy().into_owned()
        });
        PwmHeaderDescriptor {
            id: format!("h{n}"),
            label: format!("h{n}"),
            chip_name: "testchip".to_string(),
            device_id: "testdev".to_string(),
            pwm_index: n,
            supports_enable: enable.is_some(),
            pwm_path: pwm.to_string_lossy().into_owned(),
            enable_path: enable,
            is_writable: true,
            ..Default::default()
        }
    }

    /// A real controller over `headers`, with every id in `take` taken through
    /// the real `set_pwm`.
    fn controller_with_taken(
        headers: Vec<PwmHeaderDescriptor>,
        take: &[&str],
    ) -> (
        Arc<parking_lot::Mutex<HwmonPwmController>>,
        Arc<HandBackLedger>,
    ) {
        let mut ctrl = HwmonPwmController::new(
            headers,
            LeaseManager::new(),
            Box::new(RealSysfsWriter),
            Arc::new(StateCache::new()),
        );
        let lease = ctrl
            .lease_manager_mut()
            .take_lease(control_ofc_daemon::hwmon::lease::HwmonWriter::Engine)
            .unwrap()
            .lease_id;
        for id in take {
            ctrl.set_pwm(id, 60, &lease).unwrap();
        }
        let ledger = ctrl.handback().clone();
        (Arc::new(parking_lot::Mutex::new(ctrl)), ledger)
    }

    fn read_trimmed(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap().trim().to_string()
    }

    /// [SAFETY] DEC-382 (`TS-a`): a stop gives each header the daemon took the
    /// mode it found, and writes nothing else. Before DEC-382 this wrote `2` to
    /// every discovered header — `h1` would read `2` here (Thermal Cruise on an
    /// nct6775, a zero curve on a Kraken), and `h3`, never taken, would have been
    /// rewritten too.
    #[test]
    fn a_stop_gives_each_taken_header_the_mode_it_found() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (_ctrl, ledger) = controller_with_taken(
            vec![
                handback_header(d, 1, Some("5")),
                handback_header(d, 2, Some("2")),
                handback_header(d, 3, Some("7")),
                // No pwmN_enable: `set_pwm` never switches it, so nothing to give back.
                handback_header(d, 4, None),
            ],
            &["h1", "h2", "h4"],
        );
        assert_eq!(
            read_trimmed(&d.join("pwm1_enable")),
            "1",
            "precondition: the take switched h1 to manual"
        );

        let outcome = hand_back_hwmon(
            Some(&ledger),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        assert_eq!(
            outcome,
            HwmonRestore::HandedBack {
                released: 2,
                failed: 0
            }
        );
        assert_eq!(read_trimmed(&d.join("pwm1_enable")), "5");
        assert_eq!(read_trimmed(&d.join("pwm2_enable")), "2");
        assert_eq!(
            read_trimmed(&d.join("pwm3_enable")),
            "7",
            "a header the daemon never took is not rewritten"
        );
        assert_eq!(
            ledger.taken_ids(),
            vec!["h1".to_string(), "h2".to_string()],
            "a stop leaves what it gave back ON the record: ExecStopPost replays it, \
             idempotently, and it is the backstop for a write that lands after the stop"
        );
    }

    /// [SAFETY] DEC-382 review (concurrency F1): a write that lands after the stop
    /// is still covered by `ExecStopPost`. Found in manual, `h1` is given back as
    /// `Manual(90)` and reads `pwm_enable=1`; a late `set_pwm` then moves its duty
    /// without a new take, because the watchdog does not call mode 1 a reclaim.
    /// The record must still name it, or the crash backstop skips it — the one
    /// thing the old glob over every `pwm*_enable` did cover.
    #[test]
    fn a_write_after_the_stop_is_still_on_the_record_for_exec_stop_post() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctrl, ledger) =
            controller_with_taken(vec![handback_header(tmp.path(), 1, Some("1"))], &["h1"]);
        let record = tmp.path().join(handback::RECORD_FILE_NAME);
        ledger.set_record_path(record.clone());

        let outcome = hand_back_hwmon(
            Some(&ledger),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert_eq!(
            outcome,
            HwmonRestore::HandedBack {
                released: 1,
                failed: 0
            }
        );

        let lease = ctrl
            .lock()
            .lease_manager()
            .active_lease()
            .map(|l| l.lease_id.clone())
            .expect("the take's lease is still live");
        ctrl.lock().set_pwm("h1", 30, &lease).unwrap();
        assert_eq!(
            read_trimmed(&tmp.path().join("pwm1")),
            control_ofc_daemon::pwm::percent_to_raw(30).to_string(),
            "precondition: the late write landed"
        );

        let body = std::fs::read_to_string(&record).unwrap();
        assert!(
            body.lines().any(|l| l.ends_with("\tmanual\t90")),
            "ExecStopPost's record must still name the header; got {body:?}"
        );
    }

    /// [SAFETY] 277-b, half one — now structural: the hand-back never takes the
    /// controller mutex at all, so a wedged engine write holding it cannot stall
    /// the stop. The wedge carries a self-release deadline (DEC-272 trap 3).
    #[test]
    fn a_held_controller_lock_cannot_stall_the_hand_back() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let (ctrl, ledger) =
            controller_with_taken(vec![handback_header(tmp.path(), 1, Some("5"))], &["h1"]);

        let released = Arc::new(AtomicBool::new(false));
        let (wedge, held) = (released.clone(), ctrl.clone());
        let wedger = std::thread::spawn(move || {
            let _guard = held.lock();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !wedge.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        std::thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        let outcome = hand_back_hwmon(
            Some(&ledger),
            Duration::from_millis(200),
            Duration::from_secs(5),
        );
        let elapsed = started.elapsed();
        released.store(true, Ordering::SeqCst);
        wedger.join().unwrap();

        assert_eq!(
            outcome,
            HwmonRestore::HandedBack {
                released: 1,
                failed: 0
            }
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the hand-back must not wait on the controller mutex; took {elapsed:?}"
        );
        assert_eq!(read_trimmed(&tmp.path().join("pwm1_enable")), "5");
    }

    /// Nothing held is not a failed restore. It is the common clean stop since
    /// DEC-382 — headers go back as soon as nothing holds them — and it is also a
    /// board whose headers have no `pwmN_enable`, which the daemon never takes.
    /// Reporting either as a failure would be the 277-b round-1 false error.
    #[test]
    fn nothing_held_is_not_reported_as_a_failed_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let (_ctrl, ledger) = controller_with_taken(
            vec![
                handback_header(tmp.path(), 1, None),
                handback_header(tmp.path(), 2, Some("5")),
            ],
            &["h1"],
        );
        assert_eq!(
            hand_back_hwmon(
                Some(&ledger),
                Duration::from_secs(5),
                Duration::from_secs(5)
            ),
            HwmonRestore::NothingTaken
        );
        assert_eq!(
            hand_back_hwmon(None, Duration::from_secs(5), Duration::from_secs(5)),
            HwmonRestore::NoController
        );
    }

    /// Turn a FIFO wedge into an ordinary file after `after`: link the FIFO aside,
    /// rename a regular file holding `content` over its path, then open the linked
    /// FIFO once — non-blocking, so this thread can never park itself — to release
    /// the ONE write parked on it. Every later open then hits the regular file.
    ///
    /// Why not `release_fifo_after`: the hand-back READS BACK what it wrote, and a
    /// read of a FIFO parks until a writer appears, so a plain reader-release can
    /// leave the hand-back thread parked on its confirmation instead. The test
    /// would still pass on its deadline — and a regression that made `run_bounded`
    /// wait for completion would then hang the suite rather than fail it.
    fn unwedge_after(fifo: &Path, after: Duration, content: &str) -> std::thread::JoinHandle<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let fifo = fifo.to_path_buf();
        let content = content.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(after);
            let aside = fifo.with_extension("wedge");
            std::fs::hard_link(&fifo, &aside).unwrap();
            let plain = fifo.with_extension("plain");
            std::fs::write(&plain, &content).unwrap();
            std::fs::rename(&plain, &fifo).unwrap();
            let _reader = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&aside);
            std::thread::sleep(Duration::from_millis(50));
        })
    }

    /// [SAFETY] 277-b, half two — the archetype: a write wedged in the kernel
    /// cannot stall the hand-back. The FIFO is swapped in AFTER the real take, and
    /// is the faithful model because opening one `O_WRONLY` blocks in `open(2)`.
    #[test]
    fn a_wedged_sysfs_write_cannot_stall_the_hand_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (_ctrl, ledger) =
            controller_with_taken(vec![handback_header(tmp.path(), 1, Some("5"))], &["h1"]);
        std::fs::remove_file(tmp.path().join("pwm1_enable")).unwrap();
        let fifo = wedged_fifo(tmp.path(), "pwm1_enable");
        let releaser = unwedge_after(&fifo, Duration::from_millis(400), "5\n");

        let started = Instant::now();
        let outcome = hand_back_hwmon(
            Some(&ledger),
            Duration::from_secs(5),
            Duration::from_millis(200),
        );
        let elapsed = started.elapsed();

        // Join BEFORE asserting: a panicking assertion skips everything after it.
        releaser.join().unwrap();

        assert_eq!(outcome, HwmonRestore::WritesTimedOut(1));
        assert!(
            elapsed < Duration::from_secs(2),
            "a sysfs write that never returns must not stall the stop — the process \
             has to be able to exit; took {elapsed:?}"
        );
    }

    /// Make a FIFO at `dir/name` and return it. Opening one `O_WRONLY` blocks in
    /// `open(2)` until a reader appears, which is the only faithful in-process
    /// model of a sysfs write wedged in a kernel driver: `std::fs::write` is
    /// `File::create` + `write_all`, so the block happens at the same syscall a
    /// real one does. A sleep, or a held userspace mutex, models the wedge the
    /// author imagined rather than the one that happens — DEC-278 shipped three
    /// passing tests that way against a fix which did not work.
    fn wedged_fifo(dir: &Path, name: &str) -> PathBuf {
        let fifo = dir.join(name);
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must be available (coreutils)");
        assert!(status.success(), "mkfifo failed for {}", fifo.display());
        fifo
    }

    /// Open `fifo` `opens` times after `after`, unblocking whatever is parked in
    /// `open(2)`.
    ///
    /// Self-release (DEC-272 trap 3): a failed assertion skips the test's own
    /// cleanup, so a writer with no reader would park for the life of the test
    /// binary and turn a red test into a hung CI job. Join the returned handle
    /// BEFORE asserting, and while the FIFO still exists — a reader that opens a
    /// path `tempdir` has already removed swallows the ENOENT and releases
    /// nothing.
    ///
    /// **`opens` must match how many times the code under test opens the path,
    /// not how many writes you think it does.** `gpu_fan::reset_to_auto` issues
    /// two `std::fs::write` calls to the same path, and each is a separate
    /// `File::create` — so one reader releases the first and leaves the writer
    /// parked on the second forever. The bound still holds and the test still
    /// passes, but the release becomes decorative: a regression that made
    /// `run_bounded` wait for completion would then HANG this test instead of
    /// failing it, which is the failure mode this helper exists to prevent. Each
    /// open here rendezvouses with the next writer open, so the count is exact.
    fn release_fifo_after(
        fifo: &Path,
        after: Duration,
        opens: usize,
    ) -> std::thread::JoinHandle<()> {
        let path = fifo.to_path_buf();
        std::thread::spawn(move || {
            std::thread::sleep(after);
            for _ in 0..opens {
                let _ = std::fs::File::open(&path);
            }
        })
    }

    /// [SAFETY] 278-c. The GPU half of the same rule 277-b established for hwmon.
    ///
    /// `gpu_fan::reset_to_auto` is two bare `std::fs::write` calls and it runs
    /// FIRST in the shutdown closure, so leaving it unbounded meant a wedged PMFW
    /// write blocked *before* the bounded hwmon restore was ever reached — the
    /// process stayed exactly as stuck as it was before 277-b, one device class
    /// over. Bounding half a sequence bounds nothing.
    #[test]
    fn a_wedged_gpu_curve_write_cannot_stall_the_shutdown_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = wedged_fifo(tmp.path(), "fan_curve");
        let releaser = release_fifo_after(&fifo, Duration::from_millis(400), 2);

        let started = Instant::now();
        let completed =
            restore_gpu_fans_to_auto(vec![(fifo.clone(), None)], Duration::from_millis(200));
        let elapsed = started.elapsed();

        releaser.join().unwrap();

        assert!(
            !completed,
            "a write parked in open(2) cannot have completed — if this reports \
             success the deadline is not being observed at all"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "a PMFW write that never returns must not stall shutdown: it runs \
             ahead of the hwmon restore, which it would keep from ever running, and \
             ExecStopPost cannot run until the process exits; took \
             {elapsed:?}"
        );
    }

    /// [SAFETY] DEC-382 — the panic hook gives back the recorded mode, never `2`.
    /// Before DEC-382 it was the third copy of the hardcoded write (with the
    /// shutdown restore and `ExecStopPost`), so fixing two would have left the
    /// crash path writing Thermal Cruise to an nct6775.
    #[test]
    fn the_panic_hook_gives_back_the_recorded_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let (_ctrl, ledger) =
            controller_with_taken(vec![handback_header(tmp.path(), 1, Some("5"))], &["h1"]);
        assert_eq!(
            read_trimmed(&tmp.path().join("pwm1_enable")),
            "1",
            "precondition"
        );
        let targets: &'static PanicRestoreTargets = Box::leak(Box::new(PanicRestoreTargets {
            gpu_curves: Vec::new(),
            hwmon_handback: Some(ledger),
        }));
        assert!(restore_panic_targets(targets, Duration::from_secs(5)));
        assert_eq!(read_trimmed(&tmp.path().join("pwm1_enable")), "5");
    }

    /// [SAFETY] 278-a. The panic hook carried the same unbounded shape.
    ///
    /// It runs only on a *fatal* panic and correctly takes no lock, which is why
    /// this is a deadline rather than a restructure — but an unresponsive chip
    /// still blocked the hook, so the process never reached `abort()`: a
    /// panicking daemon that neither controls fans nor dies. Aborting with fans
    /// latched is strictly better, because systemd can restart that.
    #[test]
    fn a_wedged_write_cannot_stall_the_panic_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let (_ctrl, ledger) =
            controller_with_taken(vec![handback_header(tmp.path(), 1, Some("5"))], &["h1"]);
        std::fs::remove_file(tmp.path().join("pwm1_enable")).unwrap();
        let fifo = wedged_fifo(tmp.path(), "pwm1_enable");
        let releaser = unwedge_after(&fifo, Duration::from_millis(400), "5\n");

        // `&'static` because `PANIC_RESTORE` is a static `OnceLock`, so the real
        // call site always has one.
        let targets: &'static PanicRestoreTargets = Box::leak(Box::new(PanicRestoreTargets {
            gpu_curves: Vec::new(),
            hwmon_handback: Some(ledger),
        }));

        let started = Instant::now();
        let completed = restore_panic_targets(targets, Duration::from_millis(200));
        let elapsed = started.elapsed();

        releaser.join().unwrap();

        assert!(
            !completed,
            "a write parked in open(2) cannot have completed"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the panic hook must not hang on an unresponsive chip — it sits \
             between the panic and abort(), so a block here is a process that \
             never dies; took {elapsed:?}"
        );
    }

    /// 277-c / `OFN-t` — the adopted-poll-task drain, which behaviour cannot see.
    ///
    /// The drain is inline in `async_main`, so no in-process test can invoke it;
    /// and it shipped with no test at all, which is how a shutdown guarantee
    /// quietly stops being one. Dropping the `.extend(...close_and_drain...)`
    /// line leaves the whole suite green — an adopted OpenFan poll loop is then
    /// signalled but never joined, and "the restore is the guaranteed last
    /// writer" stops being established for a rescan-adopted controller.
    ///
    /// Two assertions are load-bearing and neither is about the `extend`.
    ///
    /// **The ORDERING one**: draining after `finish_shutdown` would compile, read
    /// plausibly, and do nothing whatsoever — the handles would be collected
    /// after the drain that was supposed to consume them.
    ///
    /// **The `close_and_drain` one (`OFN-t`)**: a bare `drain(..)` compiles here
    /// too, passes the ordering assertion, and is exactly the defect. Because the
    /// drain runs BEFORE the shutdown watch is set, taking the list without
    /// closing it leaves an adoption completing a moment later free to register a
    /// handle that nothing will read again. `close_and_drain` is the only API on
    /// `AdoptedTasks` that does both, so asserting the call is asserting the
    /// atomicity — and asserting the ABSENCE of `.drain(..)` on this field is
    /// what stops the two-call spelling (`closed = true;` then `drain`) that
    /// reopens the window while reading as equivalent.
    ///
    /// Harmless today: that loop only reads status and RPM (verified — it sends
    /// `Command::ReadAllRpm` and its only mutation is a cache update), so there
    /// is no PWM hazard now. The guard is pre-emptive, and it says so rather than
    /// implying it is protecting live users.
    #[test]
    fn adopted_poll_tasks_are_closed_and_drained_before_shutdown() {
        let whole = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));
        let src = whole
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("main.rs has a #[cfg(test)] module");

        // Anchor on the FIELD ACCESS (`.adopted_poll_tasks`), not on the bare
        // name: the first bare occurrence in this file is the struct-construction
        // site a few hundred lines earlier, so anchoring there asserted a window
        // of source that never contains the drain, and the guard failed against
        // correct code the moment it was written.
        let drain_at = src
            .find(".adopted_poll_tasks")
            .expect("the adopted poll tasks must be drained into task_handles (277-c)");
        // Bounded: the drain sits mid-file today, but a guard that panics with
        // a slice-index message instead of its own assertion is a guard that
        // tells the next maintainer nothing.
        let window = &src[drain_at..(drain_at + 200).min(src.len())];
        assert!(
            window.contains("close_and_drain()"),
            "the tasks must be taken with close_and_drain() — a bare drain runs \
             BEFORE the shutdown watch is set, so an adoption completing a moment \
             later registers a handle nothing will ever join (`OFN-t`)"
        );
        assert!(
            !window.contains(".drain(.."),
            "closing and taking must be ONE call. Spelling it as two — set the \
             flag, then drain — reads as equivalent and is not: it reopens the \
             window between them"
        );
        assert!(
            src[..drain_at].contains("task_handles.extend("),
            "the drained handles must go INTO task_handles, which is the list \
             shutdown_sequence actually joins"
        );
        let finish_at = src
            .find("finish_shutdown(")
            .expect("async_main must call finish_shutdown");
        assert!(
            drain_at < finish_at,
            "the drain must run BEFORE finish_shutdown — draining afterwards \
             collects handles nothing will ever join, which compiles and reads \
             fine while doing nothing at all"
        );
    }

    /// [SAFETY] 277-b — the call site, which behaviour cannot see.
    ///
    /// The restore closure is inline in `async_main`, so no in-process test can
    /// invoke it. Extracting the rule into a tested function does NOT test the
    /// call site — the recurring failure named in `CLAUDE.md § Hard-won lessons`.
    /// Same tool and reasoning as `main_owns_its_runtime_so_teardown_is_bounded`.
    #[test]
    fn the_shutdown_restore_goes_through_the_bounded_helper() {
        let whole = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));
        // Production code only — scanning the whole file makes this test match
        // its own string literals, the trap the polling.rs guard fell into.
        let src = whole
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("main.rs has a #[cfg(test)] module");

        assert!(
            src.contains("hand_back_hwmon("),
            "the shutdown restore must call the bounded helper"
        );
        // [SAFETY] DEC-382 (`TS-a`): no production path writes a hardcoded mode
        // any more. A literal `"2\n"` was the write — three copies of it — and it
        // is Thermal Cruise on nct6775 and a 0 % pump curve on nzxt-kraken3.
        assert!(
            !src.contains("\"2\\n\""),
            "a hardcoded pwm_enable value is back in main.rs — hand headers back \
             through hwmon::handback, which restores what each one was doing"
        );
        assert!(
            src.contains("done_rx.recv_timeout(timeout)"),
            "the restore WRITES must be bounded, not just the lock acquisition — \
             bounding only the lock moves the hang from a userspace mutex to the \
             kernel driver lock the wedged write is already stuck on"
        );
        // 278-c / 278-a. Bounding the hwmon step alone was never sufficient: the
        // GPU reset runs FIRST in the same closure, and the panic hook is a third
        // copy of the same shape. Each must reach `run_bounded`, and each is
        // asserted here because behaviour cannot see any of these call sites —
        // the closure is inline in `async_main` and the hook body is inside
        // `set_hook`.
        //
        // Matched in CALL position, not as a bare substring. `contains("foo(")`
        // is satisfied by `fn foo(`, so the first version of this guard passed on
        // the function DEFINITIONS and would have stayed green with every call
        // site deleted — the exact "extracting a rule does not test the call site"
        // trap its own docstring claims to defend against, and a sibling of the
        // `polling.rs` self-matching guard. A call line is one that mentions the
        // name and does not declare it.
        let call_lines = |needle: &str| -> usize {
            src.lines()
                .filter(|l| {
                    let t = l.trim_start();
                    l.contains(needle) && !t.starts_with("fn ") && !t.starts_with("//")
                })
                .count()
        };
        for (needle, why) in [
            (
                "restore_gpu_fans_to_auto(",
                "the shutdown closure's GPU reset must be bounded — it runs BEFORE \
                 the hwmon restore, so an unbounded PMFW write blocks the whole \
                 closure and the bounded half below is never reached (278-c)",
            ),
            (
                "restore_panic_targets(",
                "the panic hook's restore must be bounded — an unresponsive chip \
                 otherwise blocks the hook and the process never reaches abort(), \
                 leaving a panicking daemon that neither controls fans nor dies \
                 (278-a)",
            ),
            (
                "hand_back_hwmon(",
                "the shutdown closure must still call the bounded hwmon hand-back \
                 (277-b)",
            ),
        ] {
            assert!(call_lines(needle) >= 1, "{why}");
        }
        // ORDERING, which nothing else pins. 278-c is that the GPU reset runs
        // first: bounding the second of two sequential steps bounds nothing while
        // the first can still block forever ahead of it. Swapping them would keep
        // every assertion above green and silently restore that shape, so assert
        // the relative position of the two call sites in the closure.
        let gpu_at = src
            .find("restore_gpu_fans_to_auto(app_state")
            .or_else(|| src.find("let _ = restore_gpu_fans_to_auto("))
            .expect("the shutdown closure must call restore_gpu_fans_to_auto");
        let hwmon_at = src
            .find("let _ = hand_back_hwmon(")
            .expect("the shutdown closure must call hand_back_hwmon");
        assert!(
            gpu_at < hwmon_at,
            "the GPU reset must stay AHEAD of the hwmon restore in the shutdown \
             closure — the ordering is what makes bounding both necessary, and a \
             reorder is invisible to every other assertion here"
        );
        // The helper the three of them share must still actually bound something.
        // Without this, the three assertions above pass against a `run_bounded`
        // that simply calls `f()` inline — the rule would be *named* everywhere
        // and *enforced* nowhere.
        assert!(
            src.contains("fn run_bounded") && src.contains("recv_timeout"),
            "run_bounded must impose a deadline — three restore paths now depend \
             on it, so a version that merely calls f() disarms all of them at once"
        );
        // Matched in STATEMENT position, not as a substring: the doc comments
        // above discuss the old unbounded lock in prose, and a bare `contains`
        // would hit that explanation rather than any code.
        assert!(
            !src.lines()
                .any(|l| l.trim_start().starts_with("let ctrl = hwmon_ctrl.lock();")),
            "the restore must not take the controller lock unbounded — the engine \
             holds it across an uncancellable sysfs write, and every backstop for \
             a stall here runs after it"
        );
    }

    #[test]
    fn config_reload_updates_profile_search_dirs() {
        let tmp = tempfile::tempdir().unwrap();

        // Write a daemon.toml with custom search dirs
        let config_path = tmp.path().join("daemon.toml");
        std::fs::write(
            &config_path,
            r#"
[profiles]
search_dirs = ["/custom/profiles", "/other/profiles"]
"#,
        )
        .unwrap();

        // No runtime.toml — only daemon.toml should be consulted
        let runtime_path = tmp.path().join("runtime.toml");

        let search_dirs = parking_lot::RwLock::new(vec![PathBuf::from("/old/path")]);

        let result = apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &search_dirs,
            &parking_lot::RwLock::new(None),
            &StateCache::new(),
        );
        assert!(result.is_ok());

        let dirs = search_dirs.read().clone();
        // The daemon-owned store ({state_dir}/profiles) is prepended first
        // (DEC-160); the configured dirs follow in order.
        assert_eq!(dirs[0], daemon_state::profiles_dir());
        assert_eq!(
            &dirs[1..],
            &[
                PathBuf::from("/custom/profiles"),
                PathBuf::from("/other/profiles"),
            ]
        );
    }

    #[test]
    fn a_reload_that_cannot_parse_runtime_toml_records_the_degradation() {
        // `AUD3-m`, at the CALL SITE rather than on the helper. `load_from_reporting`
        // is unit-tested in `runtime_config`, but a helper that returns a value
        // nothing stores is a rule with no consumer — `CLAUDE.md`'s #1 recurring
        // lesson. This drives the real `apply_config_reload` and asserts the
        // shared cell the `/status` builder reads.
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("daemon.toml");
        std::fs::write(&config_path, "").unwrap();
        let runtime_path = tmp.path().join("runtime.toml");
        std::fs::write(
            &runtime_path,
            "[polling]\npoll_interval_ms = 900\n[garbage\n",
        )
        .unwrap();

        let search_dirs = parking_lot::RwLock::new(vec![]);
        let degraded = parking_lot::RwLock::new(None);

        // The reload still SUCCEEDS — degrading to defaults rather than refusing
        // is deliberate, and this asserts the fix did not quietly change that.
        apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &search_dirs,
            &degraded,
            &StateCache::new(),
        )
        .expect("a corrupt runtime.toml must not fail the reload");

        let d = degraded
            .read()
            .clone()
            .expect("the degradation is recorded");
        assert_eq!(d.reason, "malformed");
        assert_eq!(d.phase, "reload");
    }

    #[test]
    fn a_clean_reload_does_not_clear_an_earlier_startup_degradation() {
        // Sticky, deliberately. A successful reload repairs nothing that a failed
        // STARTUP load cost: nearly every runtime-mutable key is consumed once at
        // boot, so the daemon is still running on defaults for those. Clearing
        // here would claim a recovery that did not happen.
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("daemon.toml");
        std::fs::write(&config_path, "").unwrap();
        let runtime_path = tmp.path().join("runtime.toml");
        RuntimeConfig::default().save_to(&runtime_path).unwrap();

        let search_dirs = parking_lot::RwLock::new(vec![]);
        let degraded = parking_lot::RwLock::new(Some(RuntimeConfigDegraded {
            reason: "malformed".into(),
            path: runtime_path.display().to_string(),
            detail: "expected `]`".into(),
            phase: "startup".into(),
        }));

        apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &search_dirs,
            &degraded,
            &StateCache::new(),
        )
        .unwrap();

        assert_eq!(
            degraded.read().as_ref().map(|d| d.phase.clone()),
            Some("startup".to_string()),
            "a clean reload must not erase what the startup load lost"
        );
    }

    #[test]
    fn a_failed_reload_does_not_overwrite_a_startup_degradation() {
        // [SAFETY] `WIRE-ao`. The sibling above proves a *clean* reload leaves the
        // startup record alone; this is the case that was actually broken — the
        // write was unconditional, so a FAILED reload replaced it. The two are not
        // equally severe: a startup failure drops every `header_roles` assignment
        // (no 30% floor, no stop exemption, no pump-safe identify), a reload
        // failure drops nothing because boot's roles are still in force. Letting
        // `reload` win made `/status` under-report, and a client reading `reload`
        // would reassure a user whose hand-assigned pump was unprotected.
        //
        // Asserted on `phase` AND `detail`: `phase` alone would pass if the
        // startup record were replaced by a *different* startup record, and the
        // detail is what identifies which one survived.
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("daemon.toml");
        std::fs::write(&config_path, "").unwrap();
        let runtime_path = tmp.path().join("runtime.toml");
        std::fs::write(&runtime_path, "[garbage\n").unwrap();

        let search_dirs = parking_lot::RwLock::new(vec![]);
        let degraded = parking_lot::RwLock::new(Some(RuntimeConfigDegraded {
            reason: "malformed".into(),
            path: runtime_path.display().to_string(),
            detail: "THE STARTUP ERROR".into(),
            phase: "startup".into(),
        }));

        apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &search_dirs,
            &degraded,
            &StateCache::new(),
        )
        .expect("a corrupt runtime.toml must still not fail the reload");

        let d = degraded.read().clone().expect("a record still stands");
        assert_eq!(
            d.phase, "startup",
            "the more severe record must survive a failed reload"
        );
        assert_eq!(
            d.detail, "THE STARTUP ERROR",
            "it must be the ORIGINAL startup record, not a fresh one wearing its phase"
        );
    }

    #[test]
    fn a_second_failed_reload_refreshes_the_reload_record() {
        // `WIRE-ao` kept latest-wins *within* the reload phase, and this pins that
        // half — without it the fix would be indistinguishable from "never
        // overwrite", which would serve a stale error while a different one was
        // live. The two writes carry different TOML errors so the assertion cannot
        // pass on a stale record.
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("daemon.toml");
        std::fs::write(&config_path, "").unwrap();
        let runtime_path = tmp.path().join("runtime.toml");
        let search_dirs = parking_lot::RwLock::new(vec![]);
        let degraded = parking_lot::RwLock::new(None);

        std::fs::write(&runtime_path, "[garbage\n").unwrap();
        apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &search_dirs,
            &degraded,
            &StateCache::new(),
        )
        .unwrap();
        let first = degraded.read().clone().expect("first failure recorded");
        assert_eq!(first.phase, "reload");

        // A *different* malformed file. The table name is echoed into the TOML
        // error's source snippet, so the two details are distinguishable by
        // content rather than by chance — an unknown-key file would parse
        // cleanly and this test would then assert nothing.
        std::fs::write(&runtime_path, "[the_second_failure\n").unwrap();
        apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &search_dirs,
            &degraded,
            &StateCache::new(),
        )
        .unwrap();
        let second = degraded.read().clone().expect("second failure recorded");
        assert_eq!(second.phase, "reload");
        assert!(
            second.detail.contains("the_second_failure"),
            "a later reload failure must refresh the detail, not serve the stale one; got {}",
            second.detail
        );
    }

    #[test]
    fn config_reload_with_runtime_overlay() {
        let tmp = tempfile::tempdir().unwrap();

        // daemon.toml with one set of search dirs
        let config_path = tmp.path().join("daemon.toml");
        std::fs::write(
            &config_path,
            r#"
[profiles]
search_dirs = ["/etc/control-ofc/profiles"]
"#,
        )
        .unwrap();

        // runtime.toml overrides search_dirs
        let runtime_path = tmp.path().join("runtime.toml");
        let mut runtime_cfg = RuntimeConfig::default();
        runtime_cfg
            .set_profile_search_dirs(vec!["/runtime/profiles".into(), "/user/profiles".into()]);
        runtime_cfg.save_to(&runtime_path).unwrap();

        let search_dirs = parking_lot::RwLock::new(vec![]);

        let result = apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &search_dirs,
            &parking_lot::RwLock::new(None),
            &StateCache::new(),
        );
        assert!(result.is_ok());

        let dirs = search_dirs.read().clone();
        assert_eq!(dirs[0], daemon_state::profiles_dir());
        assert_eq!(
            &dirs[1..],
            &[
                PathBuf::from("/runtime/profiles"),
                PathBuf::from("/user/profiles"),
            ]
        );
    }

    #[test]
    fn config_reload_invalid_config_returns_error() {
        let tmp = tempfile::tempdir().unwrap();

        // Write invalid TOML
        let config_path = tmp.path().join("bad.toml");
        std::fs::write(&config_path, "not = valid = toml === {{{{").unwrap();

        let runtime_path = tmp.path().join("runtime.toml");
        let search_dirs = parking_lot::RwLock::new(vec![PathBuf::from("/should/stay")]);

        let result = apply_config_reload(
            config_path.to_str().unwrap(),
            &runtime_path,
            &search_dirs,
            &parking_lot::RwLock::new(None),
            &StateCache::new(),
        );
        assert!(result.is_err());

        // Original dirs should be untouched
        let dirs = search_dirs.read().clone();
        assert_eq!(dirs, vec![PathBuf::from("/should/stay")]);
    }
}
