//! Daemon persistent state — survives restarts.
//!
//! Stores the active profile selection at `{state_dir}/daemon_state.json`.
//! Default state_dir is `/var/lib/control-ofc` (configurable via `[state]` in
//! daemon.toml). Writes go through [`crate::atomic_io::write_atomic`], which
//! does tmp + fsync + rename + parent-dir fsync — so a process crash, kernel
//! panic, or power loss mid-write leaves either the previous complete file or
//! the new complete file, never a zero-length file.

use crate::atomic_io::{create_dir_private, write_atomic};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const DEFAULT_STATE_DIR: &str = "/var/lib/control-ofc";
const STATE_FILE: &str = "daemon_state.json";

/// Configurable state directory — set once at startup from config.
static STATE_DIR: OnceLock<String> = OnceLock::new();

/// Set the state directory from config. Must be called before any load/save.
/// Logs a warning if called more than once (OnceLock silently ignores subsequent sets).
pub fn init_state_dir(dir: &str) {
    if STATE_DIR.set(dir.to_string()).is_err() {
        log::warn!(
            "state_dir already initialized to '{}' — ignoring new value '{dir}'",
            state_dir()
        );
    }
}

fn state_dir() -> &'static str {
    STATE_DIR.get().map_or(DEFAULT_STATE_DIR, |s| s.as_str())
}

/// Persisted daemon state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    #[serde(default = "default_version")]
    pub version: u32,
    /// ID of the active profile (e.g., "quiet").
    #[serde(default)]
    pub active_profile_id: Option<String>,
    /// Path to the active profile file.
    #[serde(default)]
    pub active_profile_path: Option<String>,
}

fn default_version() -> u32 {
    1
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            version: 1,
            active_profile_id: None,
            active_profile_path: None,
        }
    }
}

/// Get the state file path.
pub fn state_file_path() -> PathBuf {
    PathBuf::from(state_dir()).join(STATE_FILE)
}

/// Directory where daemon-owned profiles are stored: `{state_dir}/profiles`
/// (DEC-160). This is the daemon's authoritative profile write location;
/// `main` prepends it to the profile search dirs (see `with_store_dir`) so it
/// is also the primary resolution source for activation-by-id.
pub fn profiles_dir() -> PathBuf {
    PathBuf::from(state_dir()).join("profiles")
}

/// Load persisted state from a specific directory (testable without global OnceLock).
pub fn load_state_from(dir: &Path) -> DaemonState {
    let path = dir.join(STATE_FILE);
    if !path.exists() {
        return DaemonState::default();
    }
    match crate::atomic_io::read_to_string_capped(&path) {
        Ok(content) => match serde_json::from_str::<DaemonState>(&content) {
            Ok(state) => state,
            Err(e) => {
                log::warn!("Corrupt state file {}, using defaults: {e}", path.display());
                DaemonState::default()
            }
        },
        Err(e) => {
            log::warn!("Could not read state file {}: {e}", path.display());
            DaemonState::default()
        }
    }
}

/// Load persisted state, returning default if file doesn't exist or is invalid.
pub fn load_state() -> DaemonState {
    let path = state_file_path();
    if !path.exists() {
        log::info!(
            "No persisted state file at {}, starting fresh",
            path.display()
        );
        return DaemonState::default();
    }
    match crate::atomic_io::read_to_string_capped(&path) {
        Ok(content) => match serde_json::from_str::<DaemonState>(&content) {
            Ok(state) => {
                log::info!(
                    "Loaded persisted state: profile={:?}",
                    state.active_profile_id
                );
                state
            }
            Err(e) => {
                log::warn!(
                    "Invalid state file '{}': {e}, using defaults",
                    path.display()
                );
                DaemonState::default()
            }
        },
        Err(e) => {
            log::warn!(
                "Failed to read state file '{}': {e}, using defaults",
                path.display()
            );
            DaemonState::default()
        }
    }
}

/// Persist state atomically (write to .tmp then rename).
pub fn save_state(state: &DaemonState) -> Result<(), String> {
    save_state_to(Path::new(state_dir()), state)
}

/// Persist state to a specific directory (testable without global OnceLock).
///
/// Confidentiality note: the state dir is created — and an existing dir
/// migrated — `0o700` via `create_dir_private` (DEC-173); the state file itself
/// is `0o600` (applied by `write_atomic`). systemd `StateDirectory=control-ofc`
/// may create `/var/lib/control-ofc` first at `StateDirectoryMode=` (default
/// 0o755); the next save tightens it to 0o700.
pub fn save_state_to(dir: &Path, state: &DaemonState) -> Result<(), String> {
    create_dir_private(dir)?;

    let path = dir.join(STATE_FILE);
    let content = serde_json::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize state: {e}"))?;
    write_atomic(&path, content.as_bytes())?;

    log::debug!(
        "State saved to {}: profile={:?}",
        path.display(),
        state.active_profile_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_has_no_profile() {
        let state = DaemonState::default();
        assert!(state.active_profile_id.is_none());
        assert!(state.active_profile_path.is_none());
        assert_eq!(state.version, 1);
    }

    #[test]
    fn roundtrip_serialize() {
        let state = DaemonState {
            version: 1,
            active_profile_id: Some("quiet".into()),
            active_profile_path: Some("/etc/control-ofc/profiles/quiet.json".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: DaemonState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.active_profile_id, Some("quiet".into()));
    }

    #[test]
    fn missing_fields_use_defaults() {
        let json = r#"{"version": 1}"#;
        let state: DaemonState = serde_json::from_str(json).unwrap();
        assert!(state.active_profile_id.is_none());
    }

    #[test]
    fn invalid_json_returns_default() {
        let json = "not json at all";
        let result = serde_json::from_str::<DaemonState>(json);
        assert!(result.is_err());
    }

    #[test]
    fn save_and_load_roundtrip() {
        // Uses save_state_to / load_state_from to avoid global OnceLock mutation,
        // preventing test isolation issues when tests run in parallel.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");

        let state = DaemonState {
            version: 1,
            active_profile_id: Some("balanced".into()),
            active_profile_path: Some("/tmp/balanced.json".into()),
        };
        save_state_to(&dir, &state).unwrap();

        let loaded = load_state_from(&dir);
        assert_eq!(loaded.active_profile_id, Some("balanced".into()));
        assert_eq!(
            loaded.active_profile_path,
            Some("/tmp/balanced.json".into())
        );
    }

    #[test]
    fn load_from_nonexistent_dir_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("does_not_exist");
        let loaded = load_state_from(&dir);
        assert!(loaded.active_profile_id.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn state_file_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("perms_test");

        let state = DaemonState {
            version: 1,
            active_profile_id: Some("test".into()),
            active_profile_path: None,
        };
        save_state_to(&dir, &state).unwrap();

        let path = dir.join(STATE_FILE);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file should be owner-only (0o600)");
    }
}
