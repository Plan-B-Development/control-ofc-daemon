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
/// **No `deny_unknown_fields` at this level — deliberate (DEC-243).**
///
/// `load_from` treats any parse error as "malformed → use defaults". With
/// `deny_unknown_fields` here, an *older* daemon started against a
/// `runtime.toml` written by a newer one would fail to parse a section it does
/// not know, fall back to `default()`, and thereby silently discard **every**
/// runtime setting — profile search dirs and startup delay included — which the
/// next successful write would then make permanent. Ignoring unknown *sections*
/// keeps the settings that older daemon still understands. It is not fully
/// lossless: there is no `#[serde(flatten)]` catch-all, so the unknown section
/// itself is still dropped on that daemon's next `save_to`. The point is that a
/// downgrade costs you only the newer keys, not all of them. Each section below
/// keeps `deny_unknown_fields`, so a typo *within* a known section still fails
/// loudly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<RuntimeProfiles>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<RuntimeStartup>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<RuntimeHardware>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<RuntimeSerial>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polling: Option<RuntimePolling>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<RuntimeDetection>,
}

/// Runtime overrides for `[serial]` (DEC-243). Both take effect at next start.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSerial {
    /// Serial port path; `None` here means "not overridden" (admin value wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl RuntimeSerial {
    fn is_empty(&self) -> bool {
        self.port.is_none() && self.timeout_ms.is_none()
    }
}

/// Runtime override for `[polling]` (DEC-243). Takes effect at next start.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePolling {
    pub poll_interval_ms: u64,
}

/// Runtime overrides for `[detection]` (DEC-243).
///
/// Setting either of these is only *half* the requirement — each also needs a
/// root-installed systemd drop-in granting the capability (`CAP_SYS_RAWIO` for
/// the port probe, `/dev/nvidia* rw` for NVML). The flag alone does not make the
/// feature work, and callers must not present it as if it does.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDetection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_port_probe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_nvidia_telemetry: Option<bool>,
}

impl RuntimeDetection {
    fn is_empty(&self) -> bool {
        self.allow_port_probe.is_none() && self.enable_nvidia_telemetry.is_none()
    }
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

    /// Load runtime.toml for a **read-modify-write** setter, refusing to
    /// continue if a file exists that we could not understand (DEC-252).
    ///
    /// [SAFETY] `load_from` deliberately falls back to defaults so a corrupt
    /// file can never stop the daemon booting. That fallback is wrong for a
    /// setter: every `POST /config/*` is load → mutate one key → `save_to`, so
    /// loading defaults and then writing does not merely *ignore* the unreadable
    /// file — it **overwrites every other setting in it with a default**, and the
    /// loss is permanent from that moment. The warning `load_from` logs goes to
    /// the journal, where nobody looks until their configuration has already
    /// gone. This repo has shipped one settings-destruction bug already
    /// (DEC-244); a read that failed must not become a write that erases.
    ///
    /// A *missing* file is not an error — that is the first-write case, and
    /// defaults are exactly right.
    pub fn load_for_update(path: &Path) -> Result<Self, String> {
        match crate::atomic_io::read_to_string_capped(path) {
            Ok(content) => toml::from_str::<RuntimeConfig>(&content).map_err(|e| {
                format!(
                    "existing runtime config at {} is malformed ({e}); refusing to \
                     overwrite it — move it aside to start fresh",
                    path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!(
                "cannot read existing runtime config at {} ({e}); refusing to \
                 overwrite it",
                path.display()
            )),
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

    // ── DEC-243 runtime-mutable admin keys ──────────────────────────────
    // Each getter returns `None` when unset, so `apply_runtime_overlay` can tell
    // "not overridden" from "explicitly set to the default value".

    pub fn serial_port(&self) -> Option<&str> {
        self.serial.as_ref().and_then(|s| s.port.as_deref())
    }

    pub fn serial_timeout_ms(&self) -> Option<u64> {
        self.serial.as_ref().and_then(|s| s.timeout_ms)
    }

    pub fn poll_interval_ms(&self) -> Option<u64> {
        self.polling.as_ref().map(|p| p.poll_interval_ms)
    }

    pub fn allow_port_probe(&self) -> Option<bool> {
        self.detection.as_ref().and_then(|d| d.allow_port_probe)
    }

    pub fn enable_nvidia_telemetry(&self) -> Option<bool> {
        self.detection
            .as_ref()
            .and_then(|d| d.enable_nvidia_telemetry)
    }

    /// Set (or clear, with `None`) the serial port override. Preserves the
    /// timeout; drops the `[serial]` section when both are cleared.
    pub fn set_serial_port(&mut self, port: Option<String>) {
        let mut s = self.serial.take().unwrap_or_default();
        s.port = port;
        self.serial = if s.is_empty() { None } else { Some(s) };
    }

    /// Set (or clear) the serial read timeout override.
    pub fn set_serial_timeout_ms(&mut self, timeout: Option<u64>) {
        let mut s = self.serial.take().unwrap_or_default();
        s.timeout_ms = timeout;
        self.serial = if s.is_empty() { None } else { Some(s) };
    }

    /// Set (or clear, with `None`) the poll-interval override.
    pub fn set_poll_interval_ms(&mut self, interval: Option<u64>) {
        self.polling = interval.map(|poll_interval_ms| RuntimePolling { poll_interval_ms });
    }

    /// Set (or clear) the Super-I/O port-probe opt-in.
    pub fn set_allow_port_probe(&mut self, allow: Option<bool>) {
        let mut d = self.detection.take().unwrap_or_default();
        d.allow_port_probe = allow;
        self.detection = if d.is_empty() { None } else { Some(d) };
    }

    /// Set (or clear) the NVML telemetry opt-in.
    pub fn set_enable_nvidia_telemetry(&mut self, enable: Option<bool>) {
        let mut d = self.detection.take().unwrap_or_default();
        d.enable_nvidia_telemetry = enable;
        self.detection = if d.is_empty() { None } else { Some(d) };
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
        // Falls through to default (not a panic): the *section-level*
        // deny_unknown_fields on RuntimeProfiles makes this a parse failure →
        // warn + default. Note the unknown key is inside a known section — the
        // top-level struct deliberately no longer denies unknown *sections*, so
        // a downgrade stays lossless (see unknown_section_is_ignored_not_fatal).
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

    // ── DEC-243: new runtime-mutable admin keys ──────────────────────────

    #[test]
    fn dec243_keys_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        cfg.set_poll_interval_ms(Some(2000));
        cfg.set_serial_port(Some("/dev/ttyACM1".into()));
        cfg.set_serial_timeout_ms(Some(750));
        cfg.set_allow_port_probe(Some(true));
        cfg.set_enable_nvidia_telemetry(Some(true));
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.poll_interval_ms(), Some(2000));
        assert_eq!(loaded.serial_port(), Some("/dev/ttyACM1"));
        assert_eq!(loaded.serial_timeout_ms(), Some(750));
        assert_eq!(loaded.allow_port_probe(), Some(true));
        assert_eq!(loaded.enable_nvidia_telemetry(), Some(true));
    }

    #[test]
    fn unset_dec243_keys_are_none_not_defaults() {
        // The overlay must distinguish "not overridden" from "set to the
        // default", or an untouched key would shadow the admin config.
        let cfg = RuntimeConfig::default();
        assert!(cfg.poll_interval_ms().is_none());
        assert!(cfg.serial_port().is_none());
        assert!(cfg.serial_timeout_ms().is_none());
        assert!(cfg.allow_port_probe().is_none());
        assert!(cfg.enable_nvidia_telemetry().is_none());
    }

    #[test]
    fn clearing_one_serial_key_preserves_the_other() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_serial_port(Some("/dev/ttyACM0".into()));
        cfg.set_serial_timeout_ms(Some(600));
        cfg.set_serial_port(None);
        assert_eq!(cfg.serial_timeout_ms(), Some(600));
        assert!(cfg.serial_port().is_none());
    }

    #[test]
    fn clearing_both_serial_keys_drops_the_section() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_serial_port(Some("/dev/ttyACM0".into()));
        cfg.set_serial_timeout_ms(Some(600));
        cfg.set_serial_port(None);
        cfg.set_serial_timeout_ms(None);
        assert!(cfg.serial.is_none());
    }

    #[test]
    fn clearing_both_detection_keys_drops_the_section() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_allow_port_probe(Some(true));
        cfg.set_enable_nvidia_telemetry(Some(true));
        cfg.set_allow_port_probe(None);
        cfg.set_enable_nvidia_telemetry(None);
        assert!(cfg.detection.is_none());
    }

    #[test]
    fn unknown_section_is_ignored_not_fatal() {
        // THE DOWNGRADE GUARD (DEC-243). `load_from` treats any parse failure as
        // "malformed -> defaults", so with `deny_unknown_fields` at the top level
        // an older daemon reading a newer runtime.toml would discard EVERY
        // setting — profile search dirs and startup delay included — and the next
        // write would make that loss permanent. Unknown sections must be skipped
        // while the known ones survive intact. (The unknown section itself is
        // still dropped on that daemon's next save_to — there is no flatten
        // catch-all — so a downgrade costs the newer keys, not all of them.)
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            "[profiles]\nsearch_dirs = [\"/p\"]\n\n[startup]\ndelay_secs = 7\n\n\
             [from_the_future]\nsome_key = 1\n",
        )
        .unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(
            loaded.profile_search_dirs().unwrap(),
            &["/p".to_string()],
            "a future section must not wipe the known ones"
        );
        assert_eq!(loaded.startup_delay_secs(), Some(7));
    }

    #[test]
    fn unknown_key_inside_a_known_section_still_fails_loudly() {
        // The flip side: dropping deny_unknown_fields at the top level must not
        // also silence typos *within* a section, or a misspelled key would be
        // accepted and silently do nothing.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(&path, "[startup]\ndelay_sekonds = 7\n").unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert!(
            loaded.startup_delay_secs().is_none(),
            "a typo in a known section must not be silently accepted"
        );
    }

    // ── DEC-252: a failed read must never become a destructive write ──────

    #[test]
    fn load_for_update_refuses_a_malformed_file() {
        // load_from() falls back to defaults so a corrupt file cannot stop the
        // daemon booting. For a setter that fallback is destructive: load →
        // mutate one key → save would replace every other setting with a
        // default, permanently.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(&path, "this is not = valid toml [[[").unwrap();

        assert!(
            RuntimeConfig::load_for_update(&path).is_err(),
            "a malformed file must refuse the update"
        );
        // The boot path still tolerates it.
        let _booted = RuntimeConfig::load_from(&path);
    }

    #[test]
    fn load_for_update_accepts_a_missing_file() {
        // First write: defaults are exactly right, and refusing here would make
        // the very first setter call impossible.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        assert!(RuntimeConfig::load_for_update(&path).is_ok());
    }

    #[test]
    fn a_refused_update_leaves_the_existing_file_untouched() {
        // The property that actually matters to a user: their unreadable
        // settings file is still there afterwards, byte for byte, instead of
        // having been silently replaced by defaults plus one key.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        let original = "[polling]\npoll_interval_ms = 900\n[garbage\n";
        std::fs::write(&path, original).unwrap();

        let outcome = RuntimeConfig::load_for_update(&path);
        assert!(outcome.is_err());
        if let Ok(mut cfg) = outcome {
            // Would-be destructive path — only runs if the guard regressed.
            cfg.set_poll_interval_ms(Some(1000));
            cfg.save_to(&path).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the user's file must survive a refused update"
        );
    }
}
