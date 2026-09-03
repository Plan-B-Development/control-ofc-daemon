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
    read_session(&path).map(Some).map_err(|e| e.to_string())
}

/// Why a session file on disk could not be turned into a [`ValidationSession`].
///
/// The distinction is load-bearing, and having only one bucket was half of
/// `AUD3-i`: an over-cap file is unreadable *by construction* and will be for
/// ever, so stepping over it leaks a file retention can never reclaim, whereas an
/// unparseable one may be a transient error or a serde slip and must not be
/// destroyed on a guess.
#[derive(Debug)]
enum SessionReadError {
    /// Larger than [`constants::VALIDATION_MAX_SESSION_BYTES`]. Only a daemon
    /// predating the write-side byte budget could have produced this. It is
    /// deleted by [`prune`], because nothing will ever read it again.
    TooLarge(u64),
    /// Within the cap but unreadable or unparseable. Logged and skipped, never
    /// deleted.
    Unreadable(String),
}

impl std::fmt::Display for SessionReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(
                f,
                "session file is {n} bytes, over the {}-byte cap",
                constants::VALIDATION_MAX_SESSION_BYTES
            ),
            Self::Unreadable(e) => write!(f, "{e}"),
        }
    }
}

/// Read and parse one session file, classifying failure.
///
/// The length is taken from the file's metadata *before* reading, so "too large"
/// is decided by a stat rather than inferred from the read helper's error text.
fn read_session(path: &Path) -> Result<ValidationSession, SessionReadError> {
    let len = std::fs::metadata(path)
        .map_err(|e| SessionReadError::Unreadable(format!("stat {}: {e}", path.display())))?
        .len();
    if len > constants::VALIDATION_MAX_SESSION_BYTES {
        return Err(SessionReadError::TooLarge(len));
    }
    let text = atomic_io::read_to_string_with_cap(path, constants::VALIDATION_MAX_SESSION_BYTES)
        .map_err(|e| SessionReadError::Unreadable(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| SessionReadError::Unreadable(format!("parse {}: {e}", path.display())))
}

/// Every session on disk, newest first.
///
/// A file that will not parse is logged and skipped rather than failing the
/// listing — one corrupt session must not hide the other four.
///
/// **This fully parses every retained session** (up to five, each bounded by
/// `VALIDATION_MAX_SAMPLE_BYTES` — 3.6 MiB at one member, 5.7 MiB at two; the
/// "~1 MB" this said until 2026-09-04 was wrong by up to an order of magnitude,
/// which is `AUD3-i`), so it is not free. Every async caller runs it through
/// `persist_off_runtime` rather than inline, for the same reason the profile
/// store's writes go off-runtime: blocking a tokio worker for tens of
/// milliseconds starves whatever else that thread was going to poll.
pub fn list_from(dir: &Path) -> Vec<ValidationSession> {
    scan(dir).0
}

/// Every readable session (newest first) plus the paths of any that are over the
/// store's cap.
///
/// [`list_from`] wants only the first half; [`prune`] needs the second, because a
/// file it cannot read is a file it can otherwise never reclaim.
fn scan(dir: &Path) -> (Vec<ValidationSession>, Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut out = Vec::new();
    let mut oversized = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match read_session(&path) {
            Ok(s) => out.push(s),
            Err(e @ SessionReadError::TooLarge(_)) => {
                log::warn!(
                    "Validation session {} cannot be read back: {e}. It will be pruned.",
                    path.display()
                );
                oversized.push(path);
            }
            Err(e) => log::warn!(
                "Skipping unreadable validation session {}: {e}",
                path.display()
            ),
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.started_unix_ms));
    (out, oversized)
}

/// Delete the oldest sessions beyond the retention limit.
///
/// A session still `recording` is never pruned, however old — it is the live one,
/// and deleting it would lose the very record the sweep depends on.
pub fn prune(dir: &Path, keep: usize) {
    let (sessions, oversized) = scan(dir);
    // An over-cap file is unreadable for ever, so retention can never reach it
    // through the normal path — it is not listed, not served, and not counted.
    // Deleting it here is the only thing that stops it occupying the state
    // directory permanently. Unparseable-but-within-cap files are deliberately
    // left alone: see `SessionReadError`.
    for path in oversized {
        // Re-stat immediately before removing. `scan` measured this file some
        // moments ago, and a flush landing in between can legitimately have
        // replaced it with a smaller, readable one — deleting *that* would take
        // a live recording with it and leave `sweep_interrupted` nothing to mark
        // `interrupted`, which is the one property this store exists to
        // guarantee (§15). Raised by `ofc:security-reviewer`.
        match std::fs::metadata(&path) {
            Ok(m) if m.len() <= constants::VALIDATION_MAX_SESSION_BYTES => {
                log::info!(
                    "Not pruning {}: it is readable again since the scan",
                    path.display()
                );
                continue;
            }
            Err(_) => continue,
            Ok(_) => {}
        }
        match std::fs::remove_file(&path) {
            Ok(()) => log::warn!("Pruned unreadable oversized session {}", path.display()),
            Err(e) => log::warn!("Could not prune {}: {e}", path.display()),
        }
    }
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
