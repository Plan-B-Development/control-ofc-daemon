//! Configuration scaffold for the Control-OFC daemon.

use crate::error::ConfigError;
use serde::Deserialize;

/// Top-level daemon configuration.
///
/// **No `deny_unknown_fields` at this level — deliberate (`TS-bl`, the user's
/// decision `U8`).** A section this daemon does not know is collected into
/// [`Self::unknown_sections`] and reported as a warning at boot and on reload
/// instead of failing the load, so a section added by a newer daemon (as
/// `[shutdown]` and `[safety]` were) no longer stops this one starting after a
/// downgrade. Every known section below keeps `deny_unknown_fields`, so a typo
/// *within* a section still fails loudly. Accepted risk: a misspelt section
/// name is ignored with a warning and that section's defaults apply.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub serial: SerialConfig,

    #[serde(default)]
    pub polling: PollingConfig,

    #[serde(default)]
    pub ipc: IpcConfig,

    #[serde(default)]
    pub state: StateConfig,

    #[serde(default)]
    pub profiles: ProfilesConfig,

    #[serde(default)]
    pub startup: StartupConfig,

    #[serde(default)]
    pub detection: DetectionConfig,

    #[serde(default)]
    pub shutdown: ShutdownConfig,

    /// Cooling-failure detection (DEC-443).
    #[serde(default)]
    pub safety: SafetyConfig,

    /// Top-level keys this daemon does not recognise, kept only so they can be
    /// named in a warning (see [`Self::unknown_section_names`]). Never read for
    /// configuration.
    #[serde(flatten)]
    pub unknown_sections: toml::Table,
}

/// Serial port configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialConfig {
    /// Serial port path. `None` = auto-detect.
    pub port: Option<String>,

    /// Read timeout in milliseconds.
    #[serde(default = "default_serial_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            port: None,
            timeout_ms: default_serial_timeout_ms(),
        }
    }
}

fn default_serial_timeout_ms() -> u64 {
    500
}

/// Polling interval configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollingConfig {
    /// How often to poll sensors/fans (milliseconds).
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: default_poll_interval_ms(),
        }
    }
}

fn default_poll_interval_ms() -> u64 {
    1000
}

/// IPC server configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcConfig {
    /// Unix socket path for the IPC server.
    #[serde(default = "default_socket_path")]
    pub socket_path: String,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
        }
    }
}

fn default_socket_path() -> String {
    "/run/control-ofc/control-ofc.sock".into()
}

/// Persistent state configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    /// Directory for daemon-owned persistent state (daemon_state.json).
    #[serde(default = "default_state_dir")]
    pub state_dir: String,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
        }
    }
}

fn default_state_dir() -> String {
    "/var/lib/control-ofc".into()
}

/// The admin-owned system profile directory.
///
/// Always present in the search path: the defaults include it, and
/// `POST /config/profile-search-dirs` refuses to prune it, so an unprivileged
/// client cannot orphan admin-installed profiles. Kept here as a constant
/// because two modules now depend on the exact string — the defaults below and
/// the search-dir editor in `api::handlers::config` — and a second literal is
/// how the two would drift.
pub const SYSTEM_PROFILE_DIR: &str = "/etc/control-ofc/profiles";

/// Profile search directory configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesConfig {
    /// Directories where the daemon looks for profile JSON files.
    /// The GUI stores profiles at `~/.config/control-ofc/profiles/` by default.
    #[serde(default = "default_profile_search_dirs")]
    pub search_dirs: Vec<String>,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            search_dirs: default_profile_search_dirs(),
        }
    }
}

fn default_profile_search_dirs() -> Vec<String> {
    profile_search_dirs_for(
        std::env::var("HOME").ok().as_deref(),
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
    )
}

fn profile_search_dirs_for(home: Option<&str>, xdg_config: Option<&str>) -> Vec<String> {
    let mut dirs = vec![SYSTEM_PROFILE_DIR.to_string()];
    if let Some(xdg) = xdg_config {
        dirs.push(format!("{xdg}/control-ofc/profiles"));
    } else if let Some(h) = home {
        dirs.push(format!("{h}/.config/control-ofc/profiles"));
    } else {
        // Fallback for systemd services where HOME is not set.
        // The daemon typically runs as root; /root is the standard home.
        dirs.push("/root/.config/control-ofc/profiles".to_string());
    }
    dirs
}

/// Startup configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupConfig {
    /// Delay in seconds before the daemon begins device detection.
    /// Useful for waiting for USB or hwmon devices to appear after boot.
    #[serde(default)]
    pub delay_secs: u64,

    /// Record a short lifecycle session automatically at daemon start
    /// (AIO Phase 8 Batch 3a §1, DEC-335). **Defaults to `false`.**
    ///
    /// # Why this exists
    ///
    /// §1's headline example is a startup override observed for ~50 s. A
    /// session must be started by a human, and by the time anyone opens the GUI
    /// that window is long over — so without this the feature can only ever
    /// capture daemon restarts and resumes an operator happens to be present
    /// for, and the example it was specified from is unreachable.
    ///
    /// # Why it is off by default
    ///
    /// It is the only autonomous behaviour this batch adds. It writes no
    /// hardware, claims no verify slot and holds no lease — it is a recorder —
    /// but it does open a session with nobody watching, and this project's bar
    /// for the daemon acting on its own is deliberately high. An operator who
    /// wants the startup fingerprint opts in.
    ///
    /// # What it will not do
    ///
    /// It never blocks an operator. A session started through the API cancels
    /// the auto-record and takes the slot; the partial recording is still saved.
    /// The reverse — a background diagnostic making `POST /validation/session`
    /// return `409 already_recording` on a freshly booted machine — is not
    /// acceptable behaviour and is asserted against.
    #[serde(default)]
    pub record_startup: bool,
}

// Default: delay_secs = 0 (no startup delay).
// Derived rather than manual impl per clippy::derivable_impls.

/// Hardware-detection configuration (DEC-203).
///
/// Governs the opt-in active Super-I/O port probe. **`allow_port_probe`
/// defaults to `false`** — the daemon never touches an I/O port unless the
/// operator explicitly opts in here *and* installs the `CAP_SYS_RAWIO` systemd
/// drop-in (both are required; see `packaging/`). Even when enabled the probe is
/// one-shot, read-only, and refuses a port claimed by a bound driver or ACPI.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectionConfig {
    /// Allow the active `/dev/port` Super-I/O probe (`POST /inventory/superio/probe`).
    /// `false` (default) = the probe endpoint refuses with `port_probe_available:
    /// false`. Requires the opt-in `CAP_SYS_RAWIO` drop-in to actually function.
    #[serde(default)]
    pub allow_port_probe: bool,

    /// Enable opt-in, **read-only** NVIDIA telemetry via NVML (DEC-204).
    /// `false` (default) = `libnvidia-ml.so.1` is never loaded. When `true` the
    /// daemon dlopens NVML and reads GPU temperature + fan telemetry; it also
    /// needs the opt-in `/dev/nvidia*` systemd drop-in to actually function (see
    /// `packaging/nvidia-telemetry.conf.example`). **Experimental — the NVML
    /// path is unverified on real hardware.** Never writes to any GPU.
    #[serde(default)]
    pub enable_nvidia_telemetry: bool,
}

/// What the daemon leaves behind when it stops (DEC-388).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    /// The lowest duty, in percent, that a clean stop leaves an output at when
    /// the output has no firmware mode to be given back to — an OpenFan channel,
    /// or an hwmon header with no `pwmN_enable`. Each gets `max(its last duty,
    /// this)`; `0` holds the last duty exactly, as before DEC-388. Applies live:
    /// the value in force at the moment of the stop is the one used.
    #[serde(default = "default_exit_floor_pct")]
    pub exit_floor_pct: u8,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            exit_floor_pct: default_exit_floor_pct(),
        }
    }
}

fn default_exit_floor_pct() -> u8 {
    crate::constants::DEFAULT_EXIT_FLOOR_PCT
}

/// Cooling-failure detection settings (DEC-443, `W-SAFE`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyConfig {
    /// The coolant limit, in whole °C. Fresh coolant at or above it latches the
    /// thermal emergency's 100 % force; it releases at 5 °C below. Must be
    /// within `COOLANT_LIMIT_MIN_C..=COOLANT_LIMIT_MAX_C` (40–70); there is no
    /// off switch. Applies live, like the exit floor.
    #[serde(default = "default_coolant_limit_c")]
    pub coolant_limit_c: u8,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            coolant_limit_c: default_coolant_limit_c(),
        }
    }
}

fn default_coolant_limit_c() -> u8 {
    crate::constants::DEFAULT_COOLANT_LIMIT_C
}

impl DaemonConfig {
    /// Parse configuration from a TOML string.
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        toml::from_str(input).map_err(|e| ConfigError::Parse {
            message: e.to_string(),
        })
    }

    /// Load configuration from a file path. Returns defaults if the file does not exist.
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let config = Self::from_toml(&contents)?;
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("Config file not found at {path}, using defaults");
                Ok(Self::default())
            }
            Err(e) => Err(ConfigError::Parse {
                message: format!("cannot read {path}: {e}"),
            }),
        }
    }

    /// Names of the top-level keys this daemon ignored, sorted (`TS-bl`).
    pub fn unknown_section_names(&self) -> Vec<&str> {
        self.unknown_sections.keys().map(String::as_str).collect()
    }

    /// Warn once per ignored top-level key. Boot and SIGHUP reload call this;
    /// `GET /config` re-reads the file per request and stays quiet.
    pub fn warn_unknown_sections(&self, path: &str) {
        for name in self.unknown_section_names() {
            log::warn!(
                "{path}: ignoring unknown top-level section or key {name:?} \
                 (a newer daemon's setting, or a misspelling — its defaults apply)"
            );
        }
    }

    /// Validate configuration values.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.polling.poll_interval_ms < 100 {
            return Err(ConfigError::Validation {
                field: "polling.poll_interval_ms".into(),
                message: "must be >= 100".into(),
            });
        }

        if self.serial.timeout_ms < 50 {
            return Err(ConfigError::Validation {
                field: "serial.timeout_ms".into(),
                message: "must be >= 50".into(),
            });
        }

        if self.startup.delay_secs > crate::constants::MAX_STARTUP_DELAY_SECS {
            return Err(ConfigError::Validation {
                field: "startup.delay_secs".into(),
                message: format!("must be <= {}", crate::constants::MAX_STARTUP_DELAY_SECS),
            });
        }

        if self.shutdown.exit_floor_pct > 100 {
            return Err(ConfigError::Validation {
                field: "shutdown.exit_floor_pct".into(),
                message: "must be 0-100".into(),
            });
        }

        let (lo, hi) = (
            crate::constants::COOLANT_LIMIT_MIN_C,
            crate::constants::COOLANT_LIMIT_MAX_C,
        );
        if !(lo..=hi).contains(&self.safety.coolant_limit_c) {
            return Err(ConfigError::Validation {
                field: "safety.coolant_limit_c".into(),
                message: format!("must be {lo}-{hi}"),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let config = DaemonConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn parse_empty_toml_uses_defaults() {
        let config = DaemonConfig::from_toml("").unwrap();
        assert_eq!(config.polling.poll_interval_ms, 1000);
        assert_eq!(config.serial.timeout_ms, 500);
        assert_eq!(config.ipc.socket_path, "/run/control-ofc/control-ofc.sock");
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[serial]
port = "/dev/ttyACM0"
timeout_ms = 1000

[polling]
poll_interval_ms = 500

[ipc]
socket_path = "/tmp/control-ofc.sock"
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        assert_eq!(config.serial.port.as_deref(), Some("/dev/ttyACM0"));
        assert_eq!(config.serial.timeout_ms, 1000);
        assert_eq!(config.polling.poll_interval_ms, 500);
        assert_eq!(config.ipc.socket_path, "/tmp/control-ofc.sock");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_poll_interval_too_low() {
        let toml = r#"
[polling]
poll_interval_ms = 50
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("poll_interval_ms"));
        assert!(err.to_string().contains("must be >= 100"));
    }

    #[test]
    fn rejects_serial_timeout_too_low() {
        let toml = r#"
[serial]
timeout_ms = 10
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("serial.timeout_ms"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let toml = r#"
[serial]
baud_rate = 9600
"#;
        let result = DaemonConfig::from_toml(toml);
        assert!(result.is_err());
    }

    /// `TS-bl` (`U8`): a section this daemon does not know — e.g. one a newer
    /// daemon added — is ignored and named, not a load failure, so a downgrade
    /// with it hand-set still boots. The known sections beside it still apply.
    #[test]
    fn an_unknown_top_level_section_is_ignored_and_named() {
        let toml = r#"
stray_key = 1

[polling]
poll_interval_ms = 500

[future_section]
some_setting = true
at = 1979-05-27T07:32:00Z
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        assert_eq!(config.polling.poll_interval_ms, 500);
        assert_eq!(
            config.unknown_section_names(),
            vec!["future_section", "stray_key"]
        );
        config.validate().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.toml");
        std::fs::write(&path, toml).unwrap();
        let loaded = DaemonConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(loaded.polling.poll_interval_ms, 500);
    }

    /// The flip side of `U8`: leniency is top-level only. A typo inside a known
    /// section, and a wrongly typed known section, still fail the load.
    #[test]
    fn known_sections_stay_strict() {
        for bad in [
            "[safety]\ncoolant_limt_c = 50\n",
            "safety = 50\n",
            "[polling]\npoll_interval_ms = \"fast\"\n",
        ] {
            assert!(DaemonConfig::from_toml(bad).is_err(), "{bad:?} parsed");
        }
        assert!(DaemonConfig::from_toml("")
            .unwrap()
            .unknown_section_names()
            .is_empty());
    }

    #[test]
    fn missing_file_returns_defaults() {
        let config = DaemonConfig::load("/nonexistent/path/config.toml").unwrap();
        assert_eq!(config.polling.poll_interval_ms, 1000);
    }

    #[test]
    fn load_from_custom_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("custom.toml");
        std::fs::write(&path, "[polling]\npoll_interval_ms = 750\n").unwrap();
        let config = DaemonConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(config.polling.poll_interval_ms, 750);
    }

    #[test]
    fn parse_profiles_section() {
        let toml = r#"
[profiles]
search_dirs = ["/etc/control-ofc/profiles", "/home/user/.config/control-ofc/profiles"]
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        assert_eq!(config.profiles.search_dirs.len(), 2);
        assert_eq!(config.profiles.search_dirs[0], "/etc/control-ofc/profiles");
        assert_eq!(
            config.profiles.search_dirs[1],
            "/home/user/.config/control-ofc/profiles"
        );
    }

    #[test]
    fn profiles_default_includes_etc() {
        let config = DaemonConfig::from_toml("").unwrap();
        assert!(
            config
                .profiles
                .search_dirs
                .contains(&"/etc/control-ofc/profiles".to_string()),
            "default search_dirs must include /etc/control-ofc/profiles"
        );
    }

    #[test]
    fn profiles_section_optional() {
        let toml = r#"
[polling]
poll_interval_ms = 500
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        assert!(
            config
                .profiles
                .search_dirs
                .contains(&"/etc/control-ofc/profiles".to_string()),
            "omitting [profiles] must still produce default search_dirs"
        );
    }

    #[test]
    fn defaults_include_root_fallback_when_home_unset() {
        let dirs = profile_search_dirs_for(None, None);

        assert!(
            dirs.contains(&"/etc/control-ofc/profiles".to_string()),
            "must always include /etc/control-ofc/profiles"
        );
        assert!(
            dirs.contains(&"/root/.config/control-ofc/profiles".to_string()),
            "must include /root fallback when HOME is unset"
        );
    }

    #[test]
    fn defaults_use_home_when_set() {
        let dirs = profile_search_dirs_for(Some("/home/testuser"), None);

        assert!(dirs.contains(&"/home/testuser/.config/control-ofc/profiles".to_string()));
        assert!(
            !dirs.contains(&"/root/.config/control-ofc/profiles".to_string()),
            "/root fallback must not appear when HOME is set"
        );
    }

    #[test]
    fn parse_startup_delay_section() {
        let toml = r#"
[startup]
delay_secs = 5
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        assert_eq!(config.startup.delay_secs, 5);
    }

    #[test]
    fn startup_delay_default_zero() {
        let config = DaemonConfig::from_toml("").unwrap();
        assert_eq!(config.startup.delay_secs, 0);
    }

    #[test]
    fn startup_delay_rejects_over_30() {
        let toml = r#"
[startup]
delay_secs = 60
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("startup.delay_secs"));
    }

    #[test]
    fn the_exit_floor_defaults_to_fifty_and_parses() {
        let config = DaemonConfig::from_toml("").unwrap();
        assert_eq!(
            config.shutdown.exit_floor_pct,
            crate::constants::DEFAULT_EXIT_FLOOR_PCT
        );
        let config = DaemonConfig::from_toml("[shutdown]\nexit_floor_pct = 70\n").unwrap();
        assert_eq!(config.shutdown.exit_floor_pct, 70);
        assert!(config.validate().is_ok());
    }

    /// DEC-443: the shipped example documents the key, uncommentably, in the
    /// section that reads it — and uncommenting it yields a config that loads.
    #[test]
    fn the_example_config_documents_the_coolant_limit() {
        const EXAMPLE: &str = include_str!("../../packaging/daemon.toml.example");
        let section = EXAMPLE
            .split("\n[safety]\n")
            .nth(1)
            .expect("the example config must have a [safety] section");
        let section = section.split("\n[").next().unwrap();
        let line = format!(
            "# coolant_limit_c = {}",
            crate::constants::DEFAULT_COOLANT_LIMIT_C
        );
        assert!(
            section.contains(&line),
            "[safety] lacks `{line}`:\n{section}"
        );
        let cfg: DaemonConfig = toml::from_str(&format!("[safety]\n{}\n", &line[2..]))
            .expect("uncommented line parses");
        assert_eq!(
            cfg.safety.coolant_limit_c,
            crate::constants::DEFAULT_COOLANT_LIMIT_C
        );
    }

    #[test]
    fn the_coolant_limit_defaults_parses_and_is_range_checked() {
        let config = DaemonConfig::from_toml("").unwrap();
        assert_eq!(
            config.safety.coolant_limit_c,
            crate::constants::DEFAULT_COOLANT_LIMIT_C
        );
        let config = DaemonConfig::from_toml("[safety]\ncoolant_limit_c = 55\n").unwrap();
        assert_eq!(config.safety.coolant_limit_c, 55);
        config.validate().unwrap();
        for bad in [
            crate::constants::COOLANT_LIMIT_MIN_C - 1,
            crate::constants::COOLANT_LIMIT_MAX_C + 1,
        ] {
            let config =
                DaemonConfig::from_toml(&format!("[safety]\ncoolant_limit_c = {bad}\n")).unwrap();
            let err = config.validate().unwrap_err();
            assert!(err.to_string().contains("safety.coolant_limit_c"), "{bad}");
        }
    }

    #[test]
    fn an_exit_floor_above_100_is_rejected() {
        let config = DaemonConfig::from_toml("[shutdown]\nexit_floor_pct = 101\n").unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("shutdown.exit_floor_pct"));
    }
}
