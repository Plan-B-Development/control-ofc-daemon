//! Runtime-mutable daemon configuration — the "intern" file.
//!
//! Holds the subset of settings that the daemon itself may rewrite at runtime
//! in response to API calls (`POST /config/profile-search-dirs`,
//! `POST /config/startup-delay`). Stored at `{state_dir}/runtime.toml`,
//! never in `/etc/control-ofc/daemon.toml` — that file stays admin-owned.
//!
//! This split mirrors the NetworkManager pattern of `/etc/NetworkManager/
//! NetworkManager.conf` (admin) + `/var/lib/NetworkManager/NetworkManager-
//! intern.conf` (daemon-owned, read last, shadows admin). See ADR-002.
//!
//! Precedence at startup:
//!   1. `DaemonConfig` is loaded from `/etc/control-ofc/daemon.toml`
//!   2. `RuntimeConfig` is loaded from `{state_dir}/runtime.toml`
//!   3. Any key present in both is resolved to the runtime value
//!
//! Writes go through [`crate::atomic_io::write_atomic`], which does
//! tmp + fsync + rename + parent-dir fsync at 0o600 permissions — so a
//! process crash, kernel panic, or power loss mid-write leaves either the
//! previous complete file or the new complete file, never a zero-length file.

use crate::atomic_io::{create_dir_private, write_atomic};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Filename used inside the state directory.
pub const RUNTIME_CONFIG_FILE: &str = "runtime.toml";

/// Runtime-mutable subset of daemon configuration.
///
/// All fields are `Option<...>` so "not present in runtime.toml" is distinct
/// from "explicitly set to the default". Only fields that are `Some` shadow
/// the admin config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<RuntimeProfiles>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<RuntimeStartup>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<RuntimeHardware>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeProfiles {
    pub search_dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStartup {
    pub delay_secs: u64,
}

/// User-approved hardware selections (Phase 5). Persisted by stable sensor id
/// (never a volatile `hwmonN` path). Advisory only — the daemon's thermal safety
/// still uses the hottest CpuTemp; these drive the inventory's `default_cpu`
/// recommendation + the readiness "selected sensor missing" items.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeHardware {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_cpu_sensor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_mb_sensor: Option<String>,
}

impl RuntimeHardware {
    fn is_empty(&self) -> bool {
        self.preferred_cpu_sensor.is_none() && self.preferred_mb_sensor.is_none()
    }
}

impl RuntimeConfig {
    /// Load runtime.toml from a specific file path.
    ///
    /// Returns `RuntimeConfig::default()` if the file does not exist. A
    /// malformed file logs a warning and also returns defaults — runtime
    /// config is regenerated on the next successful write, so a one-off
    /// corruption should not prevent the daemon from starting.
    pub fn load_from(path: &Path) -> Self {
        match crate::atomic_io::read_to_string_capped(path) {
            Ok(content) => match toml::from_str::<RuntimeConfig>(&content) {
                Ok(cfg) => {
                    log::info!("Loaded runtime config from {}", path.display());
                    cfg
                }
                Err(e) => {
                    log::warn!(
                        "Malformed runtime config at {}: {e} — ignoring, will regenerate",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!(
                    "Failed to read runtime config at {}: {e} — using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Atomically persist runtime.toml. Creates the parent directory if needed.
    /// Sets owner-only (0o600) permissions before rename, matching daemon_state.json.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            create_dir_private(parent)?;
        }

        let content =
            toml::to_string_pretty(self).map_err(|e| format!("serialize runtime config: {e}"))?;

        // Prepend a header so anyone opening the file sees its purpose.
        let body = format!(
            "# Control-OFC runtime config — managed by the daemon.\n\
             # DO NOT edit while the daemon is running; use the API instead.\n\
             # Source of truth for keys that the daemon rewrites at runtime.\n\
             # Admin-owned config lives at /etc/control-ofc/daemon.toml.\n\
             \n\
             {content}"
        );

        write_atomic(path, body.as_bytes())?;

        log::info!("Persisted runtime config to {}", path.display());
        Ok(())
    }

    /// Return the `profiles.search_dirs` value if present.
    pub fn profile_search_dirs(&self) -> Option<&[String]> {
        self.profiles.as_ref().map(|p| p.search_dirs.as_slice())
    }

    /// Return the `startup.delay_secs` value if present.
    pub fn startup_delay_secs(&self) -> Option<u64> {
        self.startup.as_ref().map(|s| s.delay_secs)
    }

    /// Set `profiles.search_dirs`, creating the section if absent.
    pub fn set_profile_search_dirs(&mut self, dirs: Vec<String>) {
        self.profiles = Some(RuntimeProfiles { search_dirs: dirs });
    }

    /// Set `startup.delay_secs`, creating the section if absent.
    pub fn set_startup_delay_secs(&mut self, delay: u64) {
        self.startup = Some(RuntimeStartup { delay_secs: delay });
    }

    /// Preferred CPU temperature sensor (stable id), if set.
    pub fn preferred_cpu_sensor(&self) -> Option<&str> {
        self.hardware
            .as_ref()
            .and_then(|h| h.preferred_cpu_sensor.as_deref())
    }

    /// Preferred case/motherboard temperature sensor (stable id), if set.
    pub fn preferred_mb_sensor(&self) -> Option<&str> {
        self.hardware
            .as_ref()
            .and_then(|h| h.preferred_mb_sensor.as_deref())
    }

    /// Set (or clear, with `None`) the preferred CPU sensor. Preserves the mb
    /// selection; drops the whole `[hardware]` section when both are cleared.
    pub fn set_preferred_cpu_sensor(&mut self, id: Option<String>) {
        let mut hw = self.hardware.take().unwrap_or_default();
        hw.preferred_cpu_sensor = id;
        self.hardware = if hw.is_empty() { None } else { Some(hw) };
    }

    /// Set (or clear, with `None`) the preferred motherboard sensor. Preserves
    /// the CPU selection; drops the `[hardware]` section when both are cleared.
    pub fn set_preferred_mb_sensor(&mut self, id: Option<String>) {
        let mut hw = self.hardware.take().unwrap_or_default();
        hw.preferred_mb_sensor = id;
        self.hardware = if hw.is_empty() { None } else { Some(hw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let cfg = RuntimeConfig::default();
        assert!(cfg.profiles.is_none());
        assert!(cfg.startup.is_none());
        assert!(cfg.profile_search_dirs().is_none());
        assert!(cfg.startup_delay_secs().is_none());
    }

    #[test]
    fn load_from_nonexistent_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("absent.toml");
        let cfg = RuntimeConfig::load_from(&path);
        assert!(cfg.profiles.is_none());
    }

    #[test]
    fn load_from_malformed_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.toml");
        std::fs::write(&path, "not = valid = toml === {{{{").unwrap();
        let cfg = RuntimeConfig::load_from(&path);
        assert!(cfg.profiles.is_none());
    }

    #[test]
    fn roundtrip_profile_search_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_profile_search_dirs(vec![
            "/etc/control-ofc/profiles".into(),
            "/home/alice/.config/control-ofc/profiles".into(),
        ]);
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(
            loaded.profile_search_dirs().unwrap(),
            &[
                "/etc/control-ofc/profiles".to_string(),
                "/home/alice/.config/control-ofc/profiles".to_string(),
            ]
        );
        assert!(loaded.startup_delay_secs().is_none());
    }

    #[test]
    fn roundtrip_startup_delay() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_startup_delay_secs(7);
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.startup_delay_secs(), Some(7));
        assert!(loaded.profile_search_dirs().is_none());
    }

    #[test]
    fn both_fields_coexist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_profile_search_dirs(vec!["/p".into()]);
        cfg.set_startup_delay_secs(5);
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.profile_search_dirs().unwrap(), &["/p".to_string()]);
        assert_eq!(loaded.startup_delay_secs(), Some(5));
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("runtime.toml");
        assert!(!path.parent().unwrap().exists());

        let mut cfg = RuntimeConfig::default();
        cfg.set_startup_delay_secs(1);
        cfg.save_to(&path).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn save_rejects_unknown_fields_on_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            "[profiles]\nsearch_dirs = [\"/p\"]\nextra_field = 1\n",
        )
        .unwrap();
        // Should fall through to default (not panic), since deny_unknown_fields
        // causes parse failure → warn + default.
        let loaded = RuntimeConfig::load_from(&path);
        assert!(loaded.profiles.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_startup_delay_secs(3);
        cfg.save_to(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "runtime config must be owner-only");
    }

    #[test]
    fn save_to_readonly_path_returns_err() {
        // Use a path whose parent is a regular file — every plausible failure
        // mode (mkdir, tmp-file create) must surface as an Err rather than
        // silently succeeding or panicking.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_startup_delay_secs(1);
        let err = cfg.save_to(&path).unwrap_err();
        assert!(
            err.contains("create dir") // create_dir_private mkdir failure (DEC-173)
                || err.contains("write tmp")
                || err.contains("create tmp file"),
            "expected mkdir/write error, got: {err}"
        );
    }

    #[test]
    fn load_preserves_fields_written_by_previous_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_profile_search_dirs(vec!["/one".into(), "/two".into()]);
        cfg.save_to(&path).unwrap();

        let mut loaded = RuntimeConfig::load_from(&path);
        loaded.set_startup_delay_secs(2);
        loaded.save_to(&path).unwrap();

        let reloaded = RuntimeConfig::load_from(&path);
        assert_eq!(
            reloaded.profile_search_dirs().unwrap(),
            &["/one".to_string(), "/two".to_string()]
        );
        assert_eq!(reloaded.startup_delay_secs(), Some(2));
    }

    #[test]
    fn roundtrip_preferred_sensors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        cfg.set_preferred_cpu_sensor(Some("hwmon:k10temp:x:Tctl".into()));
        cfg.set_preferred_mb_sensor(Some("hwmon:nct6798:x:SYSTIN".into()));
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.preferred_cpu_sensor(), Some("hwmon:k10temp:x:Tctl"));
        assert_eq!(loaded.preferred_mb_sensor(), Some("hwmon:nct6798:x:SYSTIN"));
    }

    #[test]
    fn clearing_one_preferred_keeps_the_other() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_preferred_cpu_sensor(Some("cpu".into()));
        cfg.set_preferred_mb_sensor(Some("mb".into()));
        cfg.set_preferred_cpu_sensor(None);
        assert_eq!(cfg.preferred_cpu_sensor(), None);
        assert_eq!(cfg.preferred_mb_sensor(), Some("mb"));
    }

    #[test]
    fn clearing_both_preferred_drops_hardware_section() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_preferred_cpu_sensor(Some("cpu".into()));
        cfg.set_preferred_cpu_sensor(None);
        assert!(cfg.hardware.is_none());
    }

    #[test]
    fn preferred_sensors_coexist_with_other_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        cfg.set_profile_search_dirs(vec!["/p".into()]);
        cfg.set_preferred_cpu_sensor(Some("cpu".into()));
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.profile_search_dirs().unwrap(), &["/p".to_string()]);
        assert_eq!(loaded.preferred_cpu_sensor(), Some("cpu"));
    }
}
