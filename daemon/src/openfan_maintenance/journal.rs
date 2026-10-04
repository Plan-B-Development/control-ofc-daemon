//! The firmware-update journal, `{state_dir}/openfan-maintenance.json` (DEC-481).
//!
//! One document: the current or most recent run's [`MaintenanceRecord`],
//! written with [`crate::atomic_io::write_atomic`] (owner-only, fsynced)
//! **before** each action, so a crash leaves a record of what was about to
//! happen. It holds the firmware's fingerprint and the board's identity, never
//! a local file path.

use std::path::{Path, PathBuf};

use super::{outcome, MaintenanceRecord, STATE_FINISHED};

/// The journal's file name inside the state directory.
pub const JOURNAL_FILE: &str = "openfan-maintenance.json";

/// A journal larger than this was not written by this daemon.
const MAX_JOURNAL_BYTES: u64 = 256 * 1024;

/// The production journal path.
pub fn path() -> PathBuf {
    crate::daemon_state::state_dir_path().join(JOURNAL_FILE)
}

/// Write `record`. A failure is returned for the caller to log; the run goes
/// on, because refusing to restore control over a journal write would be the
/// wrong trade.
pub fn save(path: &Path, record: &MaintenanceRecord) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
    crate::atomic_io::write_atomic(path, &bytes)
}

/// Read the journal; `None` when there is none or it cannot be read.
pub fn load(path: &Path) -> Option<MaintenanceRecord> {
    let text = match crate::atomic_io::read_to_string_with_cap(path, MAX_JOURNAL_BYTES) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("OpenFan firmware-update journal could not be read: {e}");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(record) => Some(record),
        Err(e) => {
            log::warn!("OpenFan firmware-update journal is not a run record ({e}) — ignored");
            None
        }
    }
}

/// At startup: load the journal, and mark a run the daemon stopped in the
/// middle of as interrupted — never resumed, and nothing it did repeated.
///
/// A run that had already asked the board to enter its bootloader ends as
/// [`outcome::NEEDS_RECOVERY`]: the board may be sitting in it. One that had
/// not ends as [`outcome::NO_FIRMWARE_CHANGE`].
pub fn recover(path: &Path) -> Option<Recovered> {
    let mut record = load(path)?;
    if record.state == STATE_FINISHED {
        return Some(Recovered {
            record,
            interrupted_now: false,
        });
    }
    let now = crate::control_paths::unix_ms();
    let stopped_in = record.stage.clone();
    record.state = STATE_FINISHED.to_string();
    record.interrupted = true;
    record.cancellable = false;
    record.finished_unix_ms = Some(now);
    record.stage_deadline_unix_ms = None;
    if let Some(last) = record.stages.last_mut() {
        last.ended_unix_ms.get_or_insert(now);
    }
    let token = outcome::when_interrupted(record.bootloader_requested, record.board_answered);
    let how_far = if !record.bootloader_requested {
        "before the board was asked to enter its bootloader"
    } else if !record.board_answered {
        "after asking the board to enter its bootloader"
    } else {
        "after the board came back and answered"
    };
    let detail = format!("the daemon stopped during {stopped_in}, {how_far}; nothing was repeated");
    record.outcome = Some(token.to_string());
    record.outcome_detail = Some(detail);
    log::warn!(
        "OpenFan firmware update {} was interrupted by a daemon stop during {stopped_in} — \
         recorded as {token}",
        record.run_id
    );
    if let Err(e) = save(path, &record) {
        log::warn!("OpenFan firmware-update journal could not be updated: {e}");
    }
    Some(Recovered {
        record,
        interrupted_now: true,
    })
}

/// What [`recover`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub record: MaintenanceRecord,
    /// The run was unfinished, and this start marked it interrupted.
    pub interrupted_now: bool,
}

#[cfg(test)]
mod tests {
    use super::super::{stage, FirmwareClaim};
    use super::*;

    fn record() -> MaintenanceRecord {
        MaintenanceRecord::new(
            "r1".into(),
            "DE615CB14721492C".into(),
            FirmwareClaim {
                sha256: "a".repeat(64),
                size: 1024,
                usb_config_descriptor_hex: None,
                info: None,
            },
        )
    }

    #[test]
    fn a_saved_record_loads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(JOURNAL_FILE);
        let r = record();
        save(&p, &r).unwrap();
        assert_eq!(load(&p), Some(r));
        assert!(load(&dir.path().join("absent.json")).is_none());
    }

    #[test]
    fn a_run_stopped_after_the_bootloader_request_needs_recovery_and_is_not_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(JOURNAL_FILE);
        let mut r = record();
        r.stage = stage::WAITING_FOR_FILE.into();
        r.bootloader_requested = true;
        save(&p, &r).unwrap();

        let got = recover(&p).unwrap();
        assert!(got.interrupted_now);
        let got = got.record;
        assert!(got.interrupted);
        assert_eq!(got.state, STATE_FINISHED);
        assert_eq!(got.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        assert_eq!(
            got.stage,
            stage::WAITING_FOR_FILE,
            "where it stopped is kept"
        );
        // Recovering again changes nothing: it is finished now.
        let again = recover(&p).unwrap();
        assert!(!again.interrupted_now);
        assert_eq!(again.record, got);
        assert_eq!(load(&p), Some(got), "and the journal says so");
    }

    #[test]
    fn a_run_stopped_before_the_bootloader_request_changed_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(JOURNAL_FILE);
        let mut r = record();
        r.stage = stage::PARKING.into();
        save(&p, &r).unwrap();
        let got = recover(&p).unwrap().record;
        assert_eq!(got.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
    }

    #[test]
    fn a_foreign_document_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(JOURNAL_FILE);
        std::fs::write(&p, "{\"hello\": 1}").unwrap();
        assert!(recover(&p).is_none());
    }
}
