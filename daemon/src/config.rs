//! Configuration scaffold for the Control-OFC daemon.

use crate::error::ConfigError;
use serde::Deserialize;

/// Top-level daemon configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
    let mut dirs = vec!["/etc/control-ofc/profiles".to_string()];
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
}

// Default: delay_secs = 0 (no startup delay).
// Derived rather than manual impl per clippy::derivable_impls.

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

        if self.startup.delay_secs > 30 {
            return Err(ConfigError::Validation {
                field: "startup.delay_secs".into(),
                message: "must be <= 30".into(),
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
}
