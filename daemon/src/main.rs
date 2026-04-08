use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use onlyfans_daemon::api::handlers::AppState;
use onlyfans_daemon::api::server;
use onlyfans_daemon::config::DaemonConfig;
use onlyfans_daemon::daemon_state;
use onlyfans_daemon::health::cache::StateCache;
use onlyfans_daemon::health::history::HistoryRing;
use onlyfans_daemon::health::staleness::StalenessConfig;
use onlyfans_daemon::hwmon::lease::LeaseManager;
use onlyfans_daemon::hwmon::pwm_control::{HwmonPwmController, RealSysfsWriter};
use onlyfans_daemon::hwmon::pwm_discovery::discover_pwm_headers;
use onlyfans_daemon::hwmon::HWMON_SYSFS_ROOT;
use onlyfans_daemon::profile::{self, DaemonProfile};
use onlyfans_daemon::safety::ThermalSafetyRule;
use onlyfans_daemon::serial::controller::FanController;
use onlyfans_daemon::serial::real_transport::{auto_detect_port, RealSerialTransport};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG_PATH: &str = "/etc/onlyfans/daemon.toml";

/// Resolve the config file path.
///
/// Precedence: `--config` CLI arg > `$ONLYFANS_CONFIG` env var > default.
fn resolve_config_path() -> String {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--config" && i + 1 < args.len() {
            return args[i + 1].clone();
        }
        i += 1;
    }
    if let Ok(val) = std::env::var("ONLYFANS_CONFIG") {
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("onlyfans-daemon v{VERSION} starting");

    let config_path = resolve_config_path();
    log::info!("Config path: {config_path}");

    let config = match DaemonConfig::load(&config_path) {
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

    // Configurable startup delay — wait for hardware to appear after boot
    if config.startup.delay_secs > 0 {
        log::info!(
            "Startup delay: {}s (from daemon.toml)",
            config.startup.delay_secs
        );
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
        Arc<Mutex<Box<dyn onlyfans_daemon::serial::transport::SerialTransport + Send>>>,
    >;

    let max_serial_retries = 5;
    let mut serial_connected = false;
    let mut fc: Option<Arc<Mutex<FanController>>> = None;
    let mut ot: Option<
        Arc<Mutex<Box<dyn onlyfans_daemon::serial::transport::SerialTransport + Send>>>,
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
                    let boxed: Box<dyn onlyfans_daemon::serial::transport::SerialTransport + Send> =
                        Box::new(transport);
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
    let amd_gpus =
        onlyfans_daemon::hwmon::gpu_detect::detect_amd_gpus(std::path::Path::new(HWMON_SYSFS_ROOT));
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
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (poll_shutdown_tx, poll_shutdown_rx) = tokio::sync::watch::channel(false);

    // ── Spawn hwmon sensor + fan polling loop ──────────────────────
    let hwmon_cache = cache.clone();
    let hwmon_history = history.clone();
    let hwmon_interval = Duration::from_millis(config.polling.poll_interval_ms);
    let hwmon_shutdown = poll_shutdown_rx.clone();
    let gpu_infos_for_poll = app_state.amd_gpus.clone();
    tokio::spawn(async move {
        onlyfans_daemon::polling::hwmon_poll_loop(
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
            onlyfans_daemon::polling::openfan_poll_loop(
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
            onlyfans_daemon::profile_engine::profile_engine_loop(
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
    let socket_path = config.ipc.socket_path.clone();
    let server_state = app_state.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server::serve(&socket_path, server_state, shutdown_rx).await {
            log::error!("IPC server error: {e}");
        }
    });

    log::info!("Daemon ready — waiting for shutdown signal");

    // Handle SIGHUP (config reload) and SIGINT/SIGTERM (shutdown)
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sighup = signal(SignalKind::hangup()).expect("Failed to register SIGHUP handler");

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    log::info!("Received SIGINT — shutting down");
                    break;
                }
                _ = sighup.recv() => {
                    log::info!("Received SIGHUP — reloading config");
                    match DaemonConfig::load(&config_path) {
                        Ok(new_config) => {
                            let new_dirs: Vec<std::path::PathBuf> = new_config
                                .profiles
                                .search_dirs
                                .iter()
                                .map(std::path::PathBuf::from)
                                .collect();
                            log::info!("Config reloaded — profile search dirs: {:?}", new_dirs);
                            *app_state.profile_search_dirs.write() = new_dirs;
                        }
                        Err(e) => log::error!("Config reload failed: {e}"),
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
            match onlyfans_daemon::hwmon::gpu_fan::reset_to_auto(
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

    log::info!("onlyfans-daemon v{VERSION} stopped");
}
