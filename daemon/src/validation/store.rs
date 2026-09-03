//! On-disk persistence for validation sessions (§4, §15).
//!
//! One JSON document per session under `{state_dir}/validation/`, written with
//! the project's existing `atomic_io` helpers — §4 is explicit that persistent
//! storage should reuse the existing conventions rather than introduce a database
//! for validation alone.
//!
//! **Why persist at all.** §15 requires that "an interruption that cannot be
//! recovered must be represented explicitly as INTERRUPTED". A daemon restart is
//! exactly that interruption, and it is unrepresentable from memory: the process
//! that would have recorded it is gone. [`sweep_interrupted`] runs at boot,
//! finds any session still marked `recording`, and rewrites it as `interrupted`
//! with the timestamp of its last real sample. **No telemetry is fabricated for
//! the gap** — the record simply stops where the evidence stopped.
//!
//! Every function here takes its directory as an argument so the whole module is
//! testable against a temporary directory with no global state.

use super::session::{
    is_safe_session_id, unix_ms, ValidationSession, STATE_INTERRUPTED, STATE_RECORDING,
};
use crate::atomic_io;
use crate::constants;
use std::path::{Path, PathBuf};

/// `{state_dir}/validation` — sibling of `profiles/`, same ownership and mode.
pub fn validation_dir() -> PathBuf {
    crate::daemon_state::validation_dir()
}

fn session_path(dir: &Path, session_id: &str) -> Option<PathBuf> {
    // A session id reaches this from a URL path segment, so it is confined
    // before it is ever interpolated into a filename. `..` or a separator would
    // otherwise escape the session directory entirely.
    is_safe_session_id(session_id).then(|| dir.join(format!("{session_id}.json")))
}

/// Write a session, creating the directory if needed.
///
/// Errors are returned rather than logged-and-swallowed: a failed write on the
/// recording path is surfaced to the caller as `503 persistence_failed`, matching
/// every other `POST /config/*` handler.
pub fn save_to(dir: &Path, session: &ValidationSession) -> Result<(), String> {
    let path = session_path(dir, &session.session_id)
        .ok_or_else(|| format!("unsafe session id: {}", session.session_id))?;
    atomic_io::create_dir_private(dir)?;
    let bytes = serde_json::to_vec_pretty(session)
        .map_err(|e| format!("serialize session {}: {e}", session.session_id))?;
    atomic_io::write_atomic(&path, &bytes)
}

/// Read one session by id. `Ok(None)` when it simply is not there.
pub fn load_from(dir: &Path, session_id: &str) -> Result<Option<ValidationSession>, String> {
    let Some(path) = session_path(dir, session_id) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let text = atomic_io::read_to_string_capped(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Every session on disk, newest first.
///
/// A file that will not parse is logged and skipped rather than failing the
/// listing — one corrupt session must not hide the other four.
///
/// **This fully parses every retained session** (up to five, ~1 MB each at the
/// sample cap), so it is not free. Every async caller runs it through
/// `persist_off_runtime` rather than inline, for the same reason the profile
/// store's writes go off-runtime: blocking a tokio worker for tens of
/// milliseconds starves whatever else that thread was going to poll.
pub fn list_from(dir: &Path) -> Vec<ValidationSession> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match atomic_io::read_to_string_capped(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<ValidationSession>(&t).map_err(|e| e.to_string()))
        {
            Ok(s) => out.push(s),
            Err(e) => log::warn!(
                "Skipping unreadable validation session {}: {e}",
                path.display()
            ),
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.started_unix_ms));
    out
}

/// Delete the oldest sessions beyond the retention limit.
///
/// A session still `recording` is never pruned, however old — it is the live one,
/// and deleting it would lose the very record the sweep depends on.
pub fn prune(dir: &Path, keep: usize) {
    let sessions = list_from(dir);
    let mut kept = 0usize;
    for s in sessions {
        if s.state == STATE_RECORDING {
            continue;
        }
        kept += 1;
        if kept <= keep {
            continue;
        }
        if let Some(path) = session_path(dir, &s.session_id) {
            if let Err(e) = std::fs::remove_file(&path) {
                log::warn!("Could not prune {}: {e}", path.display());
            }
        }
    }
}

/// Rewrite any session left `recording` as `interrupted` (§15).
///
/// Called once at daemon startup. The only way a session is still marked
/// `recording` in a file is that the process holding it died — a crash, a
/// SIGKILL, a power loss, or an ordinary restart that beat the finaliser.
///
/// The rewrite records **what was actually captured**: `truncated_at_unix_ms` is
/// the last real sample's timestamp, and nothing is invented for the period after
/// it. The summary is then recomputed so its findings report `interrupted`
/// rather than `not_tested` for anything the session would have decided.
///
/// Returns the ids it repaired.
pub fn sweep_interrupted(dir: &Path, reason: &str) -> Vec<String> {
    let mut repaired = Vec::new();
    for mut session in list_from(dir) {
        if session.state != STATE_RECORDING {
            continue;
        }
        session.state = STATE_INTERRUPTED.to_string();
        session.interrupted_reason = Some(reason.to_string());
        session.truncated_at_unix_ms = session
            .samples
            .last()
            .map(|s| s.unix_ms)
            .or(Some(session.started_unix_ms));
        session.completed_unix_ms = Some(unix_ms());
        session.findings = super::summary::summarise(&session);
        match save_to(dir, &session) {
            Ok(()) => {
                log::info!(
                    "Validation session {} was interrupted ({reason}); {} sample(s) preserved",
                    session.session_id,
                    session.samples.len()
                );
                repaired.push(session.session_id.clone());
            }
            Err(e) => log::warn!(
                "Could not mark validation session {} interrupted: {e}",
                session.session_id
            ),
        }
    }
    repaired
}

/// Convenience wrappers over the process-wide state directory.
pub fn save(session: &ValidationSession) -> Result<(), String> {
    save_to(&validation_dir(), session)
}

pub fn load(session_id: &str) -> Result<Option<ValidationSession>, String> {
    load_from(&validation_dir(), session_id)
}

pub fn list() -> Vec<ValidationSession> {
    list_from(&validation_dir())
}

pub fn prune_default() {
    prune(
        &validation_dir(),
        constants::VALIDATION_MAX_RETAINED_SESSIONS,
    )
}
