//! Configuration scaffold for the OnlyFans daemon.

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
    "/run/onlyfans/onlyfans.sock".into()
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
    "/var/lib/onlyfans".into()
}

/// Profile search directory configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesConfig {
    /// Directories where the daemon looks for profile JSON files.
    /// The GUI stores profiles at `~/.config/onlyfans/profiles/` by default.
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
    let mut dirs = vec!["/etc/onlyfans/profiles".to_string()];
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        dirs.push(format!("{xdg}/onlyfans/profiles"));
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(format!("{home}/.config/onlyfans/profiles"));
    } else {
        // Fallback for systemd services where HOME is not set.
        // The daemon typically runs as root; /root is the standard home.
        dirs.push("/root/.config/onlyfans/profiles".to_string());
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

/// Persist profile search directories to daemon.toml.
///
/// Reads the existing file, updates the `[profiles]` section, and writes back
/// atomically (temp file + rename). Preserves all other sections.
pub fn persist_profile_search_dirs(config_path: &str, dirs: &[String]) -> Result<(), String> {
    let existing = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("failed to read {config_path}: {e}")),
    };

    // Parse existing TOML as a generic table so we preserve all sections.
    // If the file is malformed, warn and start from an empty table rather
    // than propagating the error (the new value will overwrite cleanly).
    let mut table: toml::Table = match toml::from_str(&existing) {
        Ok(t) => t,
        Err(e) => {
            if !existing.is_empty() {
                log::warn!("existing config at {config_path} failed to parse, will overwrite: {e}");
            }
            toml::Table::new()
        }
    };

    // Build the search_dirs array as TOML values
    let dirs_array: toml::Value = toml::Value::Array(
        dirs.iter()
            .map(|d| toml::Value::String(d.clone()))
            .collect(),
    );

    // Update or create [profiles] section
    let profiles = table
        .entry("profiles")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(ref mut t) = profiles {
        t.insert("search_dirs".to_string(), dirs_array);
    }

    // Serialize and write atomically
    let content =
        toml::to_string_pretty(&table).map_err(|e| format!("failed to serialize config: {e}"))?;

    let path = std::path::Path::new(config_path);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &content).map_err(|e| format!("failed to write temp config: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("failed to rename config: {e}"))?;

    log::info!("Persisted profile search dirs to {config_path}");
    Ok(())
}

/// Persist startup delay to daemon.toml.
pub fn persist_startup_delay(config_path: &str, delay_secs: u64) -> Result<(), String> {
    let existing = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("failed to read {config_path}: {e}")),
    };

    let mut table: toml::Table = match toml::from_str(&existing) {
        Ok(t) => t,
        Err(e) => {
            if !existing.is_empty() {
                log::warn!("existing config at {config_path} failed to parse, will overwrite: {e}");
            }
            toml::Table::new()
        }
    };

    let startup = table
        .entry("startup")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(ref mut t) = startup {
        t.insert(
            "delay_secs".to_string(),
            toml::Value::Integer(delay_secs as i64),
        );
    }

    let content =
        toml::to_string_pretty(&table).map_err(|e| format!("failed to serialize config: {e}"))?;

    let path = std::path::Path::new(config_path);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &content).map_err(|e| format!("failed to write temp config: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("failed to rename config: {e}"))?;

    log::info!("Persisted startup delay {delay_secs}s to {config_path}");
    Ok(())
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
        assert_eq!(config.ipc.socket_path, "/run/onlyfans/onlyfans.sock");
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
socket_path = "/tmp/onlyfans.sock"
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        assert_eq!(config.serial.port.as_deref(), Some("/dev/ttyACM0"));
        assert_eq!(config.serial.timeout_ms, 1000);
        assert_eq!(config.polling.poll_interval_ms, 500);
        assert_eq!(config.ipc.socket_path, "/tmp/onlyfans.sock");
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
search_dirs = ["/etc/onlyfans/profiles", "/home/user/.config/onlyfans/profiles"]
"#;
        let config = DaemonConfig::from_toml(toml).unwrap();
        assert_eq!(config.profiles.search_dirs.len(), 2);
        assert_eq!(config.profiles.search_dirs[0], "/etc/onlyfans/profiles");
        assert_eq!(
            config.profiles.search_dirs[1],
            "/home/user/.config/onlyfans/profiles"
        );
    }

    #[test]
    fn profiles_default_includes_etc() {
        let config = DaemonConfig::from_toml("").unwrap();
        assert!(
            config
                .profiles
                .search_dirs
                .contains(&"/etc/onlyfans/profiles".to_string()),
            "default search_dirs must include /etc/onlyfans/profiles"
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
                .contains(&"/etc/onlyfans/profiles".to_string()),
            "omitting [profiles] must still produce default search_dirs"
        );
    }

    #[test]
    fn defaults_include_root_fallback_when_home_unset() {
        // Temporarily remove HOME and XDG_CONFIG_HOME to simulate systemd service
        let saved_home = std::env::var("HOME").ok();
        let saved_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::remove_var("HOME");
        std::env::remove_var("XDG_CONFIG_HOME");

        let dirs = default_profile_search_dirs();

        // Restore
        if let Some(h) = saved_home {
            std::env::set_var("HOME", h);
        }
        if let Some(x) = saved_xdg {
            std::env::set_var("XDG_CONFIG_HOME", x);
        }

        assert!(
            dirs.contains(&"/etc/onlyfans/profiles".to_string()),
            "must always include /etc/onlyfans/profiles"
        );
        assert!(
            dirs.contains(&"/root/.config/onlyfans/profiles".to_string()),
            "must include /root fallback when HOME is unset"
        );
    }

    #[test]
    fn defaults_use_home_when_set() {
        let saved = std::env::var("HOME").ok();
        let saved_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("HOME", "/home/testuser");
        std::env::remove_var("XDG_CONFIG_HOME");

        let dirs = default_profile_search_dirs();

        // Restore
        match saved {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        if let Some(x) = saved_xdg {
            std::env::set_var("XDG_CONFIG_HOME", x);
        }

        assert!(dirs.contains(&"/home/testuser/.config/onlyfans/profiles".to_string()));
        assert!(
            !dirs.contains(&"/root/.config/onlyfans/profiles".to_string()),
            "/root fallback must not appear when HOME is set"
        );
    }

    #[test]
    fn persist_profile_search_dirs_creates_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.toml");
        std::fs::write(&path, "[state]\nstate_dir = \"/var/lib/onlyfans\"\n").unwrap();

        let dirs = vec![
            "/etc/onlyfans/profiles".to_string(),
            "/home/user/.config/onlyfans/profiles".to_string(),
        ];
        persist_profile_search_dirs(path.to_str().unwrap(), &dirs).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = toml::from_str(&content).unwrap();

        // [state] preserved
        assert!(parsed.contains_key("state"));
        // [profiles] created with search_dirs
        let profiles = parsed["profiles"].as_table().unwrap();
        let search_dirs = profiles["search_dirs"].as_array().unwrap();
        assert_eq!(search_dirs.len(), 2);
        assert_eq!(search_dirs[0].as_str().unwrap(), "/etc/onlyfans/profiles");
    }

    #[test]
    fn persist_profile_search_dirs_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("roundtrip.toml");
        std::fs::write(
            &path,
            "[serial]\nport = \"/dev/ttyACM0\"\n\n[polling]\npoll_interval_ms = 500\n",
        )
        .unwrap();

        let dirs = vec!["/etc/onlyfans/profiles".to_string()];
        persist_profile_search_dirs(path.to_str().unwrap(), &dirs).unwrap();

        // Should still parse as valid DaemonConfig
        let config = DaemonConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(config.serial.port.as_deref(), Some("/dev/ttyACM0"));
        assert_eq!(config.polling.poll_interval_ms, 500);
        assert!(config
            .profiles
            .search_dirs
            .contains(&"/etc/onlyfans/profiles".to_string()));
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
    fn persist_profile_search_dirs_overwrites_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.toml");
        // Write deliberately malformed TOML
        std::fs::write(&path, "this is not valid toml {{{{").unwrap();

        let dirs = vec!["/etc/onlyfans/profiles".to_string()];
        // Should succeed — malformed TOML gets overwritten with a clean table
        persist_profile_search_dirs(path.to_str().unwrap(), &dirs).unwrap();

        // The new file should be valid and contain our directory
        let config = DaemonConfig::load(path.to_str().unwrap()).unwrap();
        assert!(config
            .profiles
            .search_dirs
            .contains(&"/etc/onlyfans/profiles".to_string()));
    }
}
