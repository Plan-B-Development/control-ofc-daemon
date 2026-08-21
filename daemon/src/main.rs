use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Hardware paths that must be restored to automatic mode if the daemon panics.
/// Populated after hardware discovery, read by the panic hook.
struct PanicRestoreTargets {
    gpu_curves: Vec<(PathBuf, Option<PathBuf>)>,
    hwmon_enable_paths: Vec<String>,
}

static PANIC_RESTORE: OnceLock<PanicRestoreTargets> = OnceLock::new();

/// The thread `install_panic_hook` ran on — i.e. the one whose death ends the
/// process. Used to tell a fatal panic from a contained one (DEC-265).
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

fn install_panic_hook() {
    // Called as the first statement of `main`, so this IS the main thread.
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
            eprintln!("PANIC: restoring fans to automatic mode before aborting");
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
            for enable_path in &targets.hwmon_enable_paths {
                if let Err(e) = std::fs::write(enable_path, "2\n") {
                    eprintln!("  WARNING: failed to restore hwmon auto mode {enable_path}: {e}");
                }
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
use control_ofc_daemon::hwmon::lease::LeaseManager;
use control_ofc_daemon::hwmon::pwm_control::{HwmonPwmController, RealSysfsWriter};
use control_ofc_daemon::hwmon::pwm_discovery::discover_pwm_headers;
use control_ofc_daemon::hwmon::HWMON_SYSFS_ROOT;
use control_ofc_daemon::profile::{self, DaemonProfile};
use control_ofc_daemon::runtime_config::{RuntimeConfig, RUNTIME_CONFIG_FILE};
use control_ofc_daemon::safety::ThermalSafetyRule;
use control_ofc_daemon::serial::controller::FanController;
use control_ofc_daemon::serial::real_transport::{auto_detect_port, RealSerialTransport};
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
    // the API route. Past `MAX_SUPERVISABLE_POLL_INTERVAL_MS` the 105 °C rule's
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
             thermal-safety rule can supervise; clamping. Past that the 105 C \
             ladder's staleness budget stops tracking the poll cadence, so \
             ordinary readings begin to look stale and the ladder stops firing.",
            config.polling.poll_interval_ms,
            MAX_SUPERVISABLE_POLL_INTERVAL_MS,
        );
        config.polling.poll_interval_ms = MAX_SUPERVISABLE_POLL_INTERVAL_MS;
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
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut new_config =
        DaemonConfig::load(config_path).map_err(|e| format!("config reload failed: {e}"))?;
    let new_runtime = RuntimeConfig::load_from(runtime_config_path);
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
/// mode (no autonomous writes) and waits for a valid profile to be activated.
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

/// Ordered graceful shutdown (DEC-146 P3-9 + audit P1-A).
///
/// Stops accepting IPC connections and drains in-flight requests FIRST, then
/// drains the poll/engine tasks, then restores hardware to automatic — so
/// neither a late client write (via the IPC server) nor an in-flight engine
/// write can land after the restore and leave fans stuck in manual mode. Every
/// await is bounded by `task_timeout` so a hung task or a lingering connection
/// (e.g. a slow client holding a request open) can never block the safety restore; on timeout
/// we log and proceed, and `ExecStopPost=control-ofc-restore-auto` backstops
/// production regardless.
///
/// The engine task `.await`-joins every `spawn_blocking` backend write before
/// its loop iteration ends, so draining its task handle here also drains those
/// writes — a blocking write cannot be left in flight once the handle resolves.
/// The only residual window is a single sysfs/serial write that hangs past
/// `task_timeout` (a running `spawn_blocking` cannot be cancelled); the
/// `ExecStopPost` restore backstops that pathological case.
async fn shutdown_sequence<F>(
    poll_shutdown_tx: &tokio::sync::watch::Sender<bool>,
    server_shutdown_tx: tokio::sync::oneshot::Sender<()>,
    server_handle: tokio::task::JoinHandle<()>,
    task_handles: Vec<(&'static str, tokio::task::JoinHandle<()>)>,
    task_timeout: Duration,
    restore_hardware: F,
) where
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

    // Drain the poll/engine tasks (DEC-146 P3-9) so an in-flight engine
    // spawn_blocking write cannot land after the restore.
    for (name, handle) in task_handles {
        if tokio::time::timeout(task_timeout, handle).await.is_err() {
            log::warn!(
                "{name} task did not stop within {}s; proceeding with hardware restore",
                task_timeout.as_secs()
            );
        }
    }

    // Restore hardware to automatic — guaranteed last writer.
    restore_hardware();
}

#[tokio::main]
async fn main() {
    install_panic_hook();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("control-ofc-daemon v{VERSION} starting");

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
    log::info!("State directory: {}", config.state.state_dir);

    // Load runtime.toml from state_dir and merge. Keys present in runtime.toml
    // shadow the admin-owned daemon.toml (NetworkManager-intern pattern — ADR-002).
    let runtime_config_path =
        std::path::PathBuf::from(&config.state.state_dir).join(RUNTIME_CONFIG_FILE);
    let runtime_cfg = RuntimeConfig::load_from(&runtime_config_path);
    apply_runtime_overlay(&mut config, &runtime_cfg, &config_path);

    // Pre-flight: verify we can bind the IPC socket and write to state_dir
    // *before* starting any subsystem. A failure here is fatal — the daemon
    // is useless without IPC, and a half-started daemon only confuses
    // operators. preflight_check exits(1) itself on failure.
    let allow_non_root = parse_allow_non_root_flag();
    let listener = preflight_check(&config, allow_non_root);

    // Configurable startup delay — wait for hardware to appear after boot
    if config.startup.delay_secs > 0 {
        log::info!("Startup delay: {}s", config.startup.delay_secs);
        std::thread::sleep(Duration::from_secs(config.startup.delay_secs));
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
    let serial_timeout = Duration::from_millis(config.serial.timeout_ms);

    // ── Initialize OpenFanController (with retry for USB enumeration timing) ──
    let fan_controller: Option<Arc<Mutex<FanController>>>;
    let openfan_transport: Option<
        Arc<Mutex<Box<dyn control_ofc_daemon::serial::transport::SerialTransport + Send>>>,
    >;

    let max_serial_retries = 5;
    let mut serial_connected = false;
    let mut fc: Option<Arc<Mutex<FanController>>> = None;
    let mut ot: Option<
        Arc<Mutex<Box<dyn control_ofc_daemon::serial::transport::SerialTransport + Send>>>,
    > = None;

    for attempt in 0..=max_serial_retries {
        if attempt > 0 {
            let delay = Duration::from_secs(1 << (attempt - 1).min(4)); // 1s, 2s, 4s, 8s, 16s
            log::info!(
                "Serial retry {attempt}/{max_serial_retries}: waiting {delay:?} for device..."
            );
            // std::thread::sleep is acceptable here — no async tasks running yet during init (P2-R2)
            std::thread::sleep(delay);
        }

        // [SAFETY] Try the configured port first, then auto-detection. The
        // ordering rule lives in `serial_port_candidates` so it is unit-testable
        // without a serial device — see its doc comment for why a configured
        // port must never be the only candidate.
        let candidates = control_ofc_daemon::serial::adoption::serial_port_candidates(
            config.serial.port.as_deref(),
            || {
                log::info!("Auto-detecting OpenFanController serial port...");
                auto_detect_port(serial_timeout)
            },
        );

        if candidates.is_empty() && attempt == 0 {
            log::info!("No serial port configured and none detected");
        }

        // [SAFETY] Accept only a candidate that also *identifies* as an
        // OpenFanController — see `first_openfan_port`.
        if let Some((port, transport)) = control_ofc_daemon::serial::adoption::first_openfan_port(
            &candidates,
            serial_timeout,
            |p| {
                log::info!("Opening OpenFanController on {p}");
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
            break;
        }
    }

    if !serial_connected {
        log::warn!(
            "No OpenFanController found after {} attempts — running without serial fan control",
            max_serial_retries + 1
        );
    }

    fan_controller = fc;
    openfan_transport = ot;

    // ── Initialize hwmon PWM controller ─────────────────────────────
    let hwmon_root = Path::new(HWMON_SYSFS_ROOT);
    let mut hwmon_headers_for_poll = Vec::new();
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
    log::info!("Thermal safety rule active: hottest CpuTemp emergency at 105°C");

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
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
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
        allow_port_probe: config.detection.allow_port_probe,
        running_config: config.clone(),
        // DEC-206/207: seeded by the assessment task below once the poll cache is
        // warm. The rollup Arc is shared with the AssessmentCache (its store keeps
        // this poll mirror in lockstep with the full snapshot).
        readiness_rollup: readiness_rollup.clone(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
        )),
    });

    // Silence "assigned but not read" — runtime_cfg is consumed by the
    // overlay/migration above; the variable itself is no longer needed.
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
        let hwmon_enable_paths: Vec<_> = app_state
            .hwmon_controller
            .as_ref()
            .map(|ctrl| {
                ctrl.lock()
                    .headers()
                    .iter()
                    .filter_map(|h| h.enable_path.clone())
                    .collect()
            })
            .unwrap_or_default();
        let _ = PANIC_RESTORE.set(PanicRestoreTargets {
            gpu_curves,
            hwmon_enable_paths,
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
    // loop is the ONLY writer of the sensor map the 105 °C rule reads, so its
    // death used to blind that rule silently.
    //
    // DEC-269 corrects what this comment used to claim. Stale readings do NOT
    // simply "present as absent": a stale reading last seen at or above the
    // release temperature keeps fan curves running on it, and one seen while an
    // emergency or recovery floor was active holds that output. Only a stale-
    // and-cool reading reaches NO_SENSOR_SAFE_PCT. Either way, none of those is
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
    // nothing autonomous runs — the daemon only writes in response to explicit
    // API intent (manual override, fan identify); the GUI never writes PWM.
    //
    // DEC-266: the engine is SUPERVISED. Its task dying is not a contained
    // failure — it is the loss of the only PWM writer, and with it the 105 °C
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
                engine_shutdown,
            )
            .await;
        })
    };

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
    // poll loop (sole writer of the sensor map the 105 C rule reads). Drives a
    // non-zero exit AFTER the ordered restore has run, so systemd restarts us.
    let mut must_restart = false;
    {
        use tokio::signal::unix::SignalKind;

        let mut sighup = match tokio::signal::unix::signal(SignalKind::hangup()) {
            Ok(stream) => Some(stream),
            Err(e) => {
                log::warn!("Failed to register SIGHUP handler, config reload unavailable: {e}");
                None
            }
        };
        let mut sigterm = match tokio::signal::unix::signal(SignalKind::terminate()) {
            Ok(stream) => Some(stream),
            Err(e) => {
                log::warn!(
                    "Failed to register SIGTERM handler, only SIGINT will trigger graceful \
                     shutdown: {e}"
                );
                None
            }
        };

        tokio::pin!(ipc_dead_rx);
        tokio::pin!(engine_dead_rx);
        tokio::pin!(hwmon_dead_rx);

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    log::info!("Received SIGINT — shutting down");
                    break;
                }
                _ = async { sigterm.as_mut().expect("guarded by if predicate").recv().await }, if sigterm.is_some() => {
                    log::info!("Received SIGTERM — shutting down");
                    break;
                }
                _ = async { sighup.as_mut().expect("guarded by if predicate").recv().await }, if sighup.is_some() => {
                    log::info!("Received SIGHUP — reloading config");
                    if let Err(e) = apply_config_reload(
                        &config_path,
                        &runtime_config_path,
                        &app_state.profile_search_dirs,
                    ) {
                        log::error!("{e}");
                    }
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
                    break;
                }
                // DEC-266. Reached only while the main loop is still running, and
                // shutdown is not requested until after it breaks — so the engine
                // ending here is always unexpected, never the clean-exit path.
                _ = &mut engine_dead_rx => {
                    log::error!(
                        "SAFETY: the profile engine task exited unexpectedly — it is the sole \
                         PWM writer, so fan control and the 105 \u{b0}C thermal emergency are \
                         both gone. Restoring fans to firmware control and exiting so systemd \
                         restarts the daemon."
                    );
                    must_restart = true;
                    break;
                }
                // DEC-267. Same reasoning one level upstream: this task is the
                // only writer of the sensor map the 105 C rule reads.
                _ = &mut hwmon_dead_rx => {
                    log::error!(
                        "SAFETY: the hwmon poll task exited unexpectedly — the sensor feed the \
                         105 \u{b0}C rule reads is frozen, so the daemon is running on \
                         readings that can no longer change. Restoring fans to firmware \
                         control and exiting so systemd restarts the daemon."
                    );
                    must_restart = true;
                    break;
                }
            }
        }

        // DEC-269: `select!` reports one arm, but a shared root cause (blocking
        // pool exhaustion, OOM pressure) can end both tasks in the same instant.
        // Reporting only the winner sends an operator after the wrong subsystem.
        //
        // Checked UNCONDITIONALLY, not behind `must_restart`. Gating it there
        // was a bug: if the IPC arm won the race while the engine had also died,
        // the engine's death went unlogged AND `must_restart` stayed false, so
        // the process exited 0 and `Restart=on-failure` never fired — silently
        // losing the restart DEC-266 exists to produce.
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
    }

    // Ordered graceful shutdown (DEC-146 P3-9 + audit P1-A) — see
    // `shutdown_sequence`: stop the IPC server and drain the poll/engine tasks
    // BEFORE restoring hardware to automatic, so neither a late client write nor
    // an in-flight engine write can land after the restore.
    let task_handles: Vec<(&'static str, tokio::task::JoinHandle<()>)> = [
        ("hwmon-poll", Some(hwmon_poll_handle)),
        ("openfan-poll", openfan_poll_handle),
        ("profile-engine", Some(engine_handle)),
    ]
    .into_iter()
    .filter_map(|(name, handle)| handle.map(|h| (name, h)))
    .collect();

    shutdown_sequence(
        &poll_shutdown_tx,
        shutdown_tx,
        server_handle,
        task_handles,
        SHUTDOWN_TASK_TIMEOUT,
        || {
            // Reset GPU fans to automatic before shutting down (re-enables zero-RPM)
            for gpu in &app_state.amd_gpus {
                if let Some(ref fan_curve_path) = gpu.fan_curve_path {
                    match control_ofc_daemon::hwmon::gpu_fan::reset_to_auto(
                        fan_curve_path,
                        gpu.fan_zero_rpm_path.as_deref(),
                    ) {
                        Ok(()) => log::info!("GPU {} fan reset to auto", gpu.pci_bdf),
                        Err(e) => log::warn!("GPU {} fan reset failed: {e}", gpu.pci_bdf),
                    }
                }
            }

            // Restore hwmon headers to automatic mode (pwm_enable=2) so BIOS
            // regains thermal control. Without this, a daemon crash leaves
            // motherboard fans stuck in manual mode with no thermal management.
            if let Some(ref hwmon_ctrl) = app_state.hwmon_controller {
                let ctrl = hwmon_ctrl.lock();
                for header in ctrl.headers() {
                    if let Some(ref enable_path) = header.enable_path {
                        match std::fs::write(enable_path, "2\n") {
                            Ok(()) => log::info!("hwmon {} restored to auto mode", header.id),
                            Err(e) => log::warn!("hwmon {} auto restore failed: {e}", header.id),
                        }
                    }
                }
            }
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
    if must_restart {
        std::process::exit(1);
    }
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
        // running with no PWM writer and no 105 °C emergency.
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

        apply_runtime_overlay(&mut config, &runtime, "/etc/control-ofc/daemon.toml");

        assert_eq!(config.serial.port.as_deref(), Some("/dev/ttyACM7"));
        assert_eq!(config.serial.timeout_ms, 750);
        assert_eq!(config.polling.poll_interval_ms, 1500);
        assert!(config.detection.allow_port_probe);
        assert!(config.detection.enable_nvidia_telemetry);
    }

    #[test]
    fn overlay_clamps_a_poll_cadence_the_safety_rule_cannot_supervise() {
        // DEC-270. `daemon.toml` bounds poll_interval_ms only as >= 100, so a
        // hand-edited `poll_interval_ms = 3600000` used to reach the engine
        // intact. The 105 C rule's staleness budget is capped at 30 s, so every
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
        // durably removed OpenFan control — and with it the 105 C emergency's
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
        // return Ok, so nothing surfaced: the 105 C emergency's `force_all`
        // reported success while driving nothing.
        let chosen = control_ofc_daemon::serial::adoption::first_openfan_port(
            &ports(&["/dev/ttyACM9", "/dev/ttyACM0"]),
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
        // hardware is restored to automatic, else a late client write re-enters
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

        let order_restore = order.clone();
        shutdown_sequence(
            &poll_tx,
            server_tx,
            server_handle,
            vec![],
            Duration::from_secs(3),
            move || order_restore.lock().unwrap().push("hardware_restored"),
        )
        .await;

        assert_eq!(
            *order.lock().unwrap(),
            vec!["server_stopped", "hardware_restored"],
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
            move || *restored_c.lock().unwrap() = true,
        )
        .await;

        assert!(
            *restored.lock().unwrap(),
            "the hardware restore must run even if the IPC server fails to stop in time"
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

        let result =
            apply_config_reload(config_path.to_str().unwrap(), &runtime_path, &search_dirs);
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

        let result =
            apply_config_reload(config_path.to_str().unwrap(), &runtime_path, &search_dirs);
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

        let result =
            apply_config_reload(config_path.to_str().unwrap(), &runtime_path, &search_dirs);
        assert!(result.is_err());

        // Original dirs should be untouched
        let dirs = search_dirs.read().clone();
        assert_eq!(dirs, vec![PathBuf::from("/should/stay")]);
    }
}
