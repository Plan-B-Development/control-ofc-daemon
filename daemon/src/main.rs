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

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(targets) = PANIC_RESTORE.get() {
            eprintln!("PANIC: restoring fans to automatic mode before aborting");
            for (curve_path, zero_rpm_path) in &targets.gpu_curves {
                let _ = std::fs::write(curve_path, "r\n");
                let _ = std::fs::write(curve_path, "c\n");
                if let Some(zrp) = zero_rpm_path {
                    let _ = std::fs::write(zrp, "1\n");
                    let _ = std::fs::write(zrp, "c\n");
                }
            }
            for enable_path in &targets.hwmon_enable_paths {
                let _ = std::fs::write(enable_path, "2\n");
            }
        }
        default_hook(info);
    }));
}

use control_ofc_daemon::api::handlers::AppState;
use control_ofc_daemon::api::server;
use control_ofc_daemon::config::DaemonConfig;
use control_ofc_daemon::daemon_state;
use control_ofc_daemon::health::cache::StateCache;
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
    // SAFETY: `geteuid` is thread-safe and always defined on Unix targets.
    // It simply reads the calling process's EUID from the kernel.
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
}

/// Reload the daemon config and runtime overlay, updating the shared
/// profile search dirs. Extracted from the SIGHUP handler so it can be
/// unit-tested without a full AppState.
///
/// Returns the new search dirs on success, or an error string on failure.
fn apply_config_reload(
    config_path: &str,
    runtime_config_path: &Path,
    profile_search_dirs: &parking_lot::RwLock<Vec<std::path::PathBuf>>,
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut new_config =
        DaemonConfig::load(config_path).map_err(|e| format!("config reload failed: {e}"))?;
    let new_runtime = RuntimeConfig::load_from(runtime_config_path);
    apply_runtime_overlay(&mut new_config, &new_runtime, config_path);
    let new_dirs: Vec<std::path::PathBuf> = new_config
        .profiles
        .search_dirs
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
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

/// Load the initial profile from CLI, env, or persisted state.
fn resolve_initial_profile(search_dirs: &[std::path::PathBuf]) -> Option<DaemonProfile> {
    // Priority 1: CLI / env override
    if let Some(path) = parse_profile_arg(search_dirs) {
        return match profile::load_profile(&path) {
            Ok(p) => {
                // Persist the CLI choice so it survives reboot
                let _ = daemon_state::save_state(&daemon_state::DaemonState {
                    version: 1,
                    active_profile_id: Some(p.id.clone()),
                    active_profile_path: Some(path.display().to_string()),
                });
                Some(p)
            }
            Err(e) => {
                log::error!("Failed to load CLI profile: {e}");
                None
            }
        };
    }

    // Priority 2: Persisted state
    let state = daemon_state::load_state();
    if let Some(ref path_str) = state.active_profile_path {
        let path = std::path::PathBuf::from(path_str);
        if path.exists() {
            match profile::load_profile(&path) {
                Ok(p) => {
                    log::info!("Restored persisted profile: '{}'", p.name);
                    return Some(p);
                }
                Err(e) => {
                    log::warn!("Persisted profile invalid: {e}");
                }
            }
        } else {
            log::warn!("Persisted profile path no longer exists: {path_str}");
        }
    }

    // Priority 3: No profile — run in pure imperative mode
    log::info!("No profile loaded — running in imperative mode (GUI-driven)");
    None
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

    // Build profile search dirs from config
    let profile_search_dirs: Vec<std::path::PathBuf> = config
        .profiles
        .search_dirs
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
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

        let serial_port_path = config.serial.port.clone().or_else(|| {
            log::info!("Auto-detecting OpenFanController serial port...");
            auto_detect_port(serial_timeout)
        });

        if let Some(ref port) = serial_port_path {
            log::info!("Opening OpenFanController on {port}");
            match RealSerialTransport::open(port, serial_timeout) {
                Ok(transport) => {
                    log::info!("OpenFanController connected on {port}");
                    let boxed: Box<
                        dyn control_ofc_daemon::serial::transport::SerialTransport + Send,
                    > = Box::new(transport);
                    let shared = Arc::new(Mutex::new(boxed));

                    let ctrl =
                        FanController::new_shared(shared.clone(), cache.clone(), serial_timeout);
                    fc = Some(Arc::new(Mutex::new(ctrl)));
                    ot = Some(shared);
                    serial_connected = true;
                    break;
                }
                Err(e) => {
                    log::warn!("Failed to open OpenFanController on {port}: {e}");
                }
            }
        } else if attempt == 0 {
            log::info!("No serial port in config — trying auto-detect on retries");
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
    };

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

    let app_state = Arc::new(AppState {
        cache: cache.clone(),
        staleness_config,
        daemon_version: VERSION.to_string(),
        fan_controller,
        hwmon_controller,
        start_time: Instant::now(),
        history: history.clone(),
        active_profile: active_profile.clone(),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus,
        profile_search_dirs: parking_lot::RwLock::new(profile_search_dirs),
        config_path: config_path.clone(),
        runtime_config_path: runtime_config_path.clone(),
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
    let (poll_shutdown_tx, poll_shutdown_rx) = tokio::sync::watch::channel(false);

    // ── Spawn hwmon sensor + fan polling loop ──────────────────────
    let hwmon_cache = cache.clone();
    let hwmon_history = history.clone();
    let hwmon_interval = Duration::from_millis(config.polling.poll_interval_ms);
    let hwmon_shutdown = poll_shutdown_rx.clone();
    let gpu_infos_for_poll = app_state.amd_gpus.clone();
    tokio::spawn(async move {
        control_ofc_daemon::polling::hwmon_poll_loop(
            hwmon_cache,
            hwmon_history,
            hwmon_headers_for_poll,
            gpu_infos_for_poll,
            hwmon_root,
            hwmon_interval,
            hwmon_shutdown,
        )
        .await;
    });

    // ── Spawn OpenFanController polling loop ────────────────────────
    if let Some(transport) = openfan_transport {
        let openfan_cache = cache.clone();
        let openfan_interval = Duration::from_millis(config.polling.poll_interval_ms);
        let openfan_shutdown = poll_shutdown_rx.clone();
        tokio::spawn(async move {
            control_ofc_daemon::polling::openfan_poll_loop(
                openfan_cache,
                transport,
                serial_timeout,
                openfan_interval,
                openfan_shutdown,
            )
            .await;
        });
    }

    // ── Spawn profile engine ─────────────────────────────────────────
    // Evaluates curves and writes PWM headlessly at 1Hz.
    // In imperative mode (no profile), the GUI drives PWM writes instead.
    {
        let engine_cache = cache.clone();
        let engine_profile = active_profile.clone();
        let engine_safety = safety_rule.clone();
        let engine_fc = app_state.fan_controller.clone();
        let engine_hwmon = app_state.hwmon_controller.clone();
        let engine_gpus = app_state.amd_gpus.clone();
        let engine_shutdown = poll_shutdown_rx;

        tokio::spawn(async move {
            control_ofc_daemon::profile_engine::profile_engine_loop(
                engine_cache,
                engine_profile,
                engine_fc,
                engine_hwmon,
                engine_gpus,
                engine_safety,
                engine_shutdown,
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

    log::info!("Daemon ready — waiting for shutdown signal");

    // Handle SIGHUP (config reload), SIGINT/SIGTERM (shutdown), and IPC task
    // death (shutdown — daemon is useless without IPC).
    {
        use tokio::signal::unix::SignalKind;

        match tokio::signal::unix::signal(SignalKind::hangup()) {
            Ok(mut sighup) => {
                tokio::pin!(ipc_dead_rx);

                loop {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            log::info!("Received SIGINT — shutting down");
                            break;
                        }
                        _ = sighup.recv() => {
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
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to register SIGHUP handler, config reload unavailable: {e}");
                tokio::pin!(ipc_dead_rx);

                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        log::info!("Received SIGINT — shutting down");
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
                    }
                }
            }
        }
    }

    // Signal all tasks to stop
    let _ = poll_shutdown_tx.send(true);

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

    // Signal the IPC server to stop
    let _ = shutdown_tx.send(());
    let _ = server_handle.await;

    log::info!("control-ofc-daemon v{VERSION} stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0], PathBuf::from("/custom/profiles"));
        assert_eq!(dirs[1], PathBuf::from("/other/profiles"));
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
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0], PathBuf::from("/runtime/profiles"));
        assert_eq!(dirs[1], PathBuf::from("/user/profiles"));
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
