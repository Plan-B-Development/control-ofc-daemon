//! OpenFan firmware update (DEC-481, DEC-483).
//!
//! The daemon takes the board through its bootloader and back. The daemon
//! checks before starting, parks every channel at 100 %, borrows the serial
//! port from the poll loop ([`crate::serial::port_loan`]), sends the board into
//! its bootloader and watches the USB device in sysfs
//! ([`crate::serial::usb_identity`]). In between, the firmware is written: by
//! the daemon itself when the start asked for it and the file is a published
//! release it knows ([`firmware`], [`crate::serial::picoboot`]) — after it has
//! read the bootloader's flash id and found the board's own serial — or else
//! by the user, copying the file onto the `RPI-RP2` drive. A daemon write that
//! cannot finish falls back to that copy. Then the daemon confirms the board
//! that comes back is the same one, gathers evidence of which firmware it now
//! runs, hands the port back and waits until control has landed. It opens no
//! device but the board's own serial interface and, to write, the PICOBOOT
//! interface of the bootloader on the board's USB port.
//!
//! **Ownership.** A run holds the controller from its claim in the shared state
//! ([`crate::health::cache::StateCache::try_begin_openfan_maintenance`]) until it
//! ends. Every diagnostic that takes the write pause is refused meanwhile, and
//! OpenFan writes are suspended — skipped, never failed — until the port is
//! back. Every exit writes the journal, returns the port or nothing, and lets
//! the claim go, in that order; a [`ClaimGuard`] and the loan's own drop
//! semantics make the last two hold through a panic as well.
//!
//! **Nothing is ever repeated.** The journal is written before each action, and
//! a daemon that starts with an unfinished run marks it interrupted
//! ([`journal::recover`]); it may watch for the board, read-only, but never
//! resumes an action.
//!
//! **A board that does not answer** (DEC-484, [`silent`]) — firmware that does
//! not speak Control-OFC's protocol, or has hung — is updated too. Nothing is
//! parked, because the board takes no commands; the daemon sends the 1200-baud
//! signal alone, on the board's own serial interface, and asks the user to
//! press BOOT and RESET when that does not take it to its bootloader. The run
//! then goes on as any other, and hands the board that comes back to the poll
//! loop, or adopts it when there is none.

pub mod evidence;
pub mod firmware;
pub mod journal;
pub mod run;
pub mod silent;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::health::cache::StateCache;
use crate::serial::port_loan::LoanSender;
use crate::serial::usb_identity::UsbDevice;

/// Stage tokens: `stage` on the run record and on `status.openfan_maintenance`.
pub mod stage {
    pub const PREPARING: &str = "preparing";
    pub const PARKING: &str = "parking";
    pub const ENTERING_BOOTLOADER: &str = "entering_bootloader";
    /// A silent board did not leave for its bootloader on the 1200-baud signal:
    /// the user holds BOOT and presses RESET (DEC-484). Cancellable until the
    /// bootloader appears.
    pub const WAITING_FOR_BOOT_BUTTON: &str = "waiting_for_boot_button";
    /// The daemon writes the firmware itself (DEC-483).
    pub const WRITING_FIRMWARE: &str = "writing_firmware";
    pub const WAITING_FOR_FILE: &str = "waiting_for_file";
    pub const WAITING_FOR_RETURN: &str = "waiting_for_return";
    pub const CHECKING: &str = "checking";
    pub const RESTORING_CONTROL: &str = "restoring_control";
    pub const FINISHED: &str = "finished";
}

/// Outcome tokens. The firmware has no build identifier, so only a run in
/// which the daemon wrote the file and read it back can report the exact
/// build ([`outcome::EXACT_BUILD_VERIFIED`]).
pub mod outcome {
    /// Refused, cancelled, or the board never left normal mode: nothing changed
    /// and the fans are back under profile control.
    pub const NO_FIRMWARE_CHANGE: &str = "no_firmware_change";
    /// The board is (or may be) in its bootloader with no firmware copied: copy
    /// a file, press RESET, or power-cycle — but after the daemon's own write
    /// stopped part-way (`flash_changed` without `verified`) only a copied file
    /// brings it back.
    pub const NEEDS_RECOVERY: &str = "needs_recovery";
    /// The drive went away but the board did not come back answering.
    pub const FIRMWARE_COPIED_BOARD_NOT_BACK: &str = "firmware_copied_board_not_back";
    /// The board answers, but the fan settings did not land in time.
    pub const BOARD_BACK_CONTROL_NOT_RESTORED: &str = "board_back_control_not_restored";
    /// Control restored; the evidence is shown, the exact build unconfirmed.
    pub const COMPLETED_BUILD_NOT_CONFIRMED: &str = "completed_build_not_confirmed";
    /// Control restored, and the board runs the file: the daemon wrote it,
    /// read every byte back, and the board restarted from it straight away
    /// (DEC-483).
    pub const EXACT_BUILD_VERIFIED: &str = "exact_build_verified";
    /// Control restored, but the evidence shows the previous firmware.
    pub const BACK_ON_PREVIOUS_FIRMWARE: &str = "back_on_previous_firmware";

    /// Outcomes that leave the board outside normal control: OpenFan writes
    /// stay suspended, and `openfan` health is critical, until the link reports
    /// connected again.
    pub fn needs_recovery(outcome: &str) -> bool {
        recovery_token(outcome).is_some()
    }

    /// How a run that stopped before it finished — the daemon stopping, a
    /// panic — ended, by how far it got. One rule for every place that has to
    /// decide it: the run, its claim guard, its supervisor and the journal.
    pub fn when_interrupted(bootloader_requested: bool, board_answered: bool) -> &'static str {
        match (bootloader_requested, board_answered) {
            (false, _) => NO_FIRMWARE_CHANGE,
            (true, false) => NEEDS_RECOVERY,
            (true, true) => BOARD_BACK_CONTROL_NOT_RESTORED,
        }
    }

    /// The token for an outcome read back from the journal, when it is one that
    /// leaves the board outside normal control.
    pub fn recovery_token(outcome: &str) -> Option<&'static str> {
        [NEEDS_RECOVERY, FIRMWARE_COPIED_BOARD_NOT_BACK]
            .into_iter()
            .find(|t| *t == outcome)
    }
}

/// `state` on the run record.
pub const STATE_RUNNING: &str = "running";
pub const STATE_FINISHED: &str = "finished";

/// `board` on the run record and the start request (DEC-484).
pub mod board {
    /// The board answers Control-OFC: parked, then sent `>07`.
    pub const CONNECTED: &str = "connected";
    /// The board is on USB but does not answer: the 1200-baud signal alone,
    /// then the BOOT button.
    pub const SILENT: &str = "silent";
}

/// `bootloader_trigger` on the run record.
pub mod trigger {
    pub const JUMP: &str = ">07";
    pub const BAUD_1200: &str = "1200_baud";
    /// The user held BOOT and pressed RESET (DEC-484).
    pub const BOOT_BUTTON: &str = "boot_button";
}

/// `phase` on the run record's `firmware_write` (DEC-483).
pub mod write_phase {
    /// Asked for at the start; it begins once the bootloader appears.
    pub const PENDING: &str = "pending";
    /// Opening the bootloader and reading its flash id.
    pub const IDENTIFYING: &str = "identifying";
    /// Erasing and programming the sectors the image covers.
    pub const WRITING: &str = "writing";
    /// Reading every byte back.
    pub const VERIFYING: &str = "verifying";
    /// The restart was asked for; waiting for the bootloader to leave.
    pub const REBOOTING: &str = "rebooting";
    /// Written, read back, and the board restarted.
    pub const WRITTEN: &str = "written";
    /// The daemon stopped writing; the file is copied by hand.
    pub const FELL_BACK: &str = "fell_back";
}

/// `fallback_reason` on the run record's `firmware_write`: why the run went on to the
/// copy by hand (DEC-483).
pub mod fallback {
    /// Opening the bootloader was refused: the opt-in drop-in is missing.
    pub const NO_USB_ACCESS: &str = "no_usb_access";
    /// The bootloader could not be opened, or shows no PICOBOOT interface.
    pub const USB_UNAVAILABLE: &str = "usb_unavailable";
    /// The bootloader's flash is not the board the update was started for.
    /// Nothing was written.
    pub const FLASH_ID_MISMATCH: &str = "flash_id_mismatch";
    /// A transfer failed, the bootloader refused a command, or the write ran
    /// past its limit.
    pub const TRANSFER_FAILED: &str = "transfer_failed";
    /// A byte read back was not the file's.
    pub const READBACK_MISMATCH: &str = "readback_mismatch";
    /// Written and read back, but the board did not restart.
    pub const NO_RESTART: &str = "no_restart";
}

/// `firmware_write` on the run record: the daemon's own write (DEC-483).
/// Present only on a run that asked the daemon to write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareWrite {
    /// The published release being written.
    pub release: String,
    /// A [`write_phase`] token.
    pub phase: String,
    /// Bytes programmed while writing, and read back while verifying.
    pub done_bytes: u64,
    /// The bytes the image writes.
    pub total_bytes: u64,
    /// The bootloader's flash id, as the firmware shows it for its USB serial.
    pub flash_id: Option<String>,
    /// Set just before the first erase is sent: from here the flash may no
    /// longer hold the old firmware whole.
    pub flash_changed: bool,
    /// Every byte was read back as the file's.
    pub verified: bool,
    /// A [`fallback`] token, once the daemon stopped writing.
    pub fallback_reason: Option<String>,
    pub fallback_detail: Option<String>,
}

impl FirmwareWrite {
    pub fn new(release: &str, total_bytes: u64) -> Self {
        Self {
            release: release.to_string(),
            phase: write_phase::PENDING.to_string(),
            done_bytes: 0,
            total_bytes,
            flash_id: None,
            flash_changed: false,
            verified: false,
            fallback_reason: None,
            fallback_detail: None,
        }
    }
}

/// What the GUI says about the file it prepared — sent with the start request
/// and kept in the journal. Never a file path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareClaim {
    /// SHA-256 of the prepared `.uf2`, lower-case hex.
    pub sha256: String,
    /// Its size in bytes.
    pub size: u64,
    /// The USB configuration descriptor the image contains, lower-case hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_config_descriptor_hex: Option<String>,
    /// The `KEY:VALUE` strings the image answers `>05`/`>06` with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<BTreeMap<String, String>>,
}

/// What the board looked like on one side of the update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSnapshot {
    pub usb: Option<UsbDevice>,
    /// The `>05` block, or `None` when the board did not answer it.
    pub hw_info: Option<BTreeMap<String, String>>,
    /// The `>06` block, or `None` when the board did not answer it.
    pub fw_info: Option<BTreeMap<String, String>>,
}

impl BoardSnapshot {
    /// Both blocks as one map, or `None` when neither was answered.
    pub fn info(&self) -> Option<BTreeMap<String, String>> {
        if self.hw_info.is_none() && self.fw_info.is_none() {
            return None;
        }
        let mut all = self.hw_info.clone().unwrap_or_default();
        all.extend(self.fw_info.clone().unwrap_or_default());
        Some(all)
    }
}

/// When one stage ran, for the record and the user's validation notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: String,
    pub started_unix_ms: u64,
    pub ended_unix_ms: Option<u64>,
}

/// One run, as `GET /fans/openfan/maintenance` reports it and the journal
/// keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceRecord {
    pub run_id: String,
    /// [`STATE_RUNNING`] or [`STATE_FINISHED`].
    pub state: String,
    /// A [`stage`] token; the stage it ended in once finished.
    pub stage: String,
    pub stage_started_unix_ms: u64,
    pub stage_deadline_unix_ms: Option<u64>,
    /// Whether `DELETE` can still stop it — only before the port is borrowed.
    pub cancellable: bool,
    pub started_unix_ms: u64,
    pub finished_unix_ms: Option<u64>,
    /// An [`outcome`] token once finished.
    pub outcome: Option<String>,
    pub outcome_detail: Option<String>,
    /// The daemon stopped before the run finished.
    #[serde(default)]
    pub interrupted: bool,
    /// A `DELETE` ended the run before the port was borrowed.
    #[serde(default)]
    pub cancelled: bool,
    /// `>07` was sent: from here the board may be in its bootloader.
    #[serde(default)]
    pub bootloader_requested: bool,
    /// The bootloader appeared on the board's USB port.
    #[serde(default)]
    pub bootloader_seen: bool,
    /// The returned board answered the identity handshake: from here it is out
    /// of its bootloader and talking to the daemon.
    #[serde(default)]
    pub board_answered: bool,
    /// A [`trigger`] token: `>07`, `1200_baud` once that was tried too, or
    /// `boot_button` once a silent board's update asked for the buttons.
    pub bootloader_trigger: Option<String>,
    /// The board's `RPI-RP2` drive (`sdb`).
    pub bootloader_drive: Option<String>,
    /// Other `RPI-RP2` drives present — the user must not copy onto those.
    #[serde(default)]
    pub other_bootloader_drives: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub stages: Vec<StageTiming>,
    pub expected_usb_serial: String,
    /// The board's USB port (`8-8`).
    pub usb_port: Option<String>,
    /// The serial interface the daemon uses, reopened after the update.
    pub interface_number: Option<u8>,
    /// The serial device the daemon was using, and after the update the one it
    /// reopened.
    pub tty: Option<String>,
    pub firmware: FirmwareClaim,
    #[serde(default)]
    pub before: BoardSnapshot,
    pub after: Option<BoardSnapshot>,
    pub evidence: Option<evidence::Evidence>,
    /// The daemon's own write; `None` when the file is copied by hand.
    #[serde(default)]
    pub firmware_write: Option<FirmwareWrite>,
    /// A [`board`] token: whether the run was for a board that answered or
    /// for a silent one (DEC-484). A journal written before has none: those
    /// runs were all for a connected board.
    #[serde(default = "connected_board")]
    pub board: String,
}

fn connected_board() -> String {
    board::CONNECTED.to_string()
}

impl MaintenanceRecord {
    /// A fresh record for a run about to start.
    pub fn new(run_id: String, expected_usb_serial: String, firmware: FirmwareClaim) -> Self {
        let now = crate::control_paths::unix_ms();
        Self {
            run_id,
            state: STATE_RUNNING.to_string(),
            stage: stage::PREPARING.to_string(),
            stage_started_unix_ms: now,
            stage_deadline_unix_ms: None,
            cancellable: true,
            started_unix_ms: now,
            finished_unix_ms: None,
            outcome: None,
            outcome_detail: None,
            interrupted: false,
            cancelled: false,
            bootloader_requested: false,
            bootloader_seen: false,
            board_answered: false,
            bootloader_trigger: None,
            bootloader_drive: None,
            other_bootloader_drives: Vec::new(),
            notes: Vec::new(),
            stages: Vec::new(),
            expected_usb_serial,
            usb_port: None,
            interface_number: None,
            tty: None,
            firmware,
            before: BoardSnapshot::default(),
            after: None,
            evidence: None,
            firmware_write: None,
            board: connected_board(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.state == STATE_RUNNING
    }
}

/// A process-unique run id.
pub fn next_run_id() -> String {
    format!(
        "ofmaint-{}-{}",
        crate::control_paths::unix_ms(),
        crate::api::characterization::next_run_id()
    )
}

/// Everything the daemon keeps about firmware updates, shared through
/// `AppState`.
#[derive(Default)]
pub struct MaintenanceSlot {
    /// The poll loop's lending channel; set when a poll loop starts, and there
    /// is at most one per process.
    lender: parking_lot::Mutex<Option<LoanSender>>,
    /// The current or most recent run.
    record: parking_lot::Mutex<Option<MaintenanceRecord>>,
    /// Set by `DELETE` to stop a run before the port is borrowed.
    cancel: AtomicBool,
    /// A run's task is alive. Held for the task's whole life, so a second run
    /// cannot start while the first is still writing its record.
    alive: AtomicBool,
    /// The running task, for the shutdown drain, and the flag that closes
    /// registration once shutdown has taken it (the `OFN-t` shape).
    tasks: parking_lot::Mutex<Tasks>,
    /// The last live `>05`/`>06` read `GET /fans/openfan/device` made, so a
    /// client polling that route cannot keep the serial link busy.
    info: parking_lot::Mutex<Option<CachedInfo>>,
    /// The last uploaded file the daemon would write (DEC-483).
    staged: parking_lot::Mutex<Option<firmware::Staged>>,
    /// The boards a recovery watch is watching for, by USB port and serial
    /// (DEC-484): one watch each, however many runs left one outside normal
    /// control.
    recovery_watches: parking_lot::Mutex<BTreeSet<(String, String)>>,
}

/// A recovery watch's hold on the board it watches for; dropping it lets
/// another watch for that board start.
pub struct RecoveryWatchHold {
    slot: Arc<MaintenanceSlot>,
    board: (String, String),
}

impl Drop for RecoveryWatchHold {
    fn drop(&mut self) {
        self.slot.recovery_watches.lock().remove(&self.board);
    }
}

/// One `>05`/`>06` read, and when and on which device it was made.
#[derive(Debug, Clone)]
pub struct CachedInfo {
    pub at: std::time::Instant,
    pub port: Option<String>,
    pub hw_info: Option<BTreeMap<String, String>>,
    pub fw_info: Option<BTreeMap<String, String>>,
}

#[derive(Default)]
struct Tasks {
    handles: Vec<tokio::task::JoinHandle<()>>,
    closed: bool,
}

/// What `DELETE /fans/openfan/maintenance` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Asked to stop; it ends with no firmware change.
    Requested,
    /// Past the point where it can be stopped.
    TooLate,
    /// No run is in progress.
    NotRunning,
}

impl MaintenanceSlot {
    /// Record the poll loop's lending channel.
    pub fn set_lender(&self, lender: LoanSender) {
        *self.lender.lock() = Some(lender);
    }

    pub fn lender(&self) -> Option<LoanSender> {
        self.lender.lock().clone()
    }

    pub fn record(&self) -> Option<MaintenanceRecord> {
        self.record.lock().clone()
    }

    /// Install `record` — a new run's, or the journal's at startup.
    pub fn set_record(&self, record: MaintenanceRecord) {
        *self.record.lock() = Some(record);
    }

    /// Change the current record in place, returning a copy of the result.
    pub fn update_record(
        &self,
        f: impl FnOnce(&mut MaintenanceRecord),
    ) -> Option<MaintenanceRecord> {
        let mut slot = self.record.lock();
        let record = slot.as_mut()?;
        f(record);
        Some(record.clone())
    }

    /// Ask the running run to stop. Decided under the record lock, so a run
    /// moving past its last cancellable stage cannot race the answer.
    pub fn request_cancel(&self) -> CancelOutcome {
        let slot = self.record.lock();
        match slot.as_ref() {
            Some(r) if r.is_running() && r.cancellable => {
                self.cancel.store(true, Ordering::SeqCst);
                CancelOutcome::Requested
            }
            Some(r) if r.is_running() => CancelOutcome::TooLate,
            _ => CancelOutcome::NotRunning,
        }
    }

    /// Mark the run past its last cancellable point, and say whether a cancel
    /// arrived first — one decision under the record lock.
    pub fn close_cancel_window(&self) -> bool {
        let mut slot = self.record.lock();
        if let Some(r) = slot.as_mut() {
            r.cancellable = false;
        }
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Start watching for the board on USB port `port` with `serial` to come
    /// back, or `None` while a watch for it already runs (DEC-484).
    pub fn hold_recovery_watch(
        self: &Arc<Self>,
        port: &str,
        serial: &str,
    ) -> Option<RecoveryWatchHold> {
        let board = (port.to_string(), serial.to_string());
        if !self.recovery_watches.lock().insert(board.clone()) {
            return None;
        }
        Some(RecoveryWatchHold {
            slot: self.clone(),
            board,
        })
    }

    /// Whether a recovery watch is watching for that board.
    pub fn recovery_watched(&self, port: &str, serial: &str) -> bool {
        self.recovery_watches
            .lock()
            .contains(&(port.to_string(), serial.to_string()))
    }

    /// Claim the right to run, or `None` while a run's task is alive.
    pub fn claim(self: &Arc<Self>) -> Option<AliveGuard> {
        self.alive
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        self.cancel.store(false, Ordering::SeqCst);
        Some(AliveGuard(Arc::clone(self)))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Register a run's task — or refuse once shutdown has closed
    /// registration. `spawn` runs under the lock, so it must be a bare spawn.
    pub fn register(&self, spawn: impl FnOnce() -> tokio::task::JoinHandle<()>) -> bool {
        let mut tasks = self.tasks.lock();
        if tasks.closed {
            return false;
        }
        tasks.handles.retain(|h| !h.is_finished());
        tasks.handles.push(spawn());
        true
    }

    /// The last information read, if it is younger than `max_age` and was made
    /// on `port`.
    pub fn cached_info(
        &self,
        port: Option<&str>,
        max_age: std::time::Duration,
    ) -> Option<CachedInfo> {
        self.info
            .lock()
            .clone()
            .filter(|c| c.at.elapsed() < max_age && c.port.as_deref() == port)
    }

    pub fn store_info(&self, info: CachedInfo) {
        *self.info.lock() = Some(info);
    }

    /// Keep `staged` in place of the last upload — `None` when the last upload
    /// is not one the daemon would write.
    pub fn stage(&self, staged: Option<firmware::Staged>) {
        *self.staged.lock() = staged;
    }

    /// A copy of the staged file, if it is the one with `sha256`.
    pub fn staged(&self, sha256: &str) -> Option<firmware::Staged> {
        self.staged
            .lock()
            .as_ref()
            .filter(|s| s.sha256 == sha256)
            .cloned()
    }

    /// Whether shutdown has closed registration.
    pub fn is_closed(&self) -> bool {
        self.tasks.lock().closed
    }

    /// Close registration and take every registered task — the ONE call the
    /// shutdown path makes, so a run starting at the same moment is either
    /// drained or refused.
    pub fn close_and_drain(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let mut tasks = self.tasks.lock();
        tasks.closed = true;
        std::mem::take(&mut tasks.handles)
    }
}

/// Held by a run's task for its whole life; dropping it frees the slot.
pub struct AliveGuard(Arc<MaintenanceSlot>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::SeqCst);
    }
}

/// A run's claim on the controller in the shared state. Dropping it — on any
/// exit, a panic included — releases the claim; until [`Self::release`] says
/// otherwise, a run that has asked the board to enter its bootloader is
/// released as needing recovery, so OpenFan writes stay suspended.
pub struct ClaimGuard {
    cache: Arc<StateCache>,
    run_id: String,
    bootloader_requested: AtomicBool,
    board_answered: AtomicBool,
    /// The recovery a silent board's claim replaced (DEC-484): put back when
    /// the run changes nothing.
    prior: Option<(String, &'static str)>,
    released: bool,
}

impl ClaimGuard {
    /// Take ownership of a claim `cache` has already granted to `run_id`.
    pub fn new(cache: Arc<StateCache>, run_id: String) -> Self {
        Self {
            cache,
            run_id,
            bootloader_requested: AtomicBool::new(false),
            board_answered: AtomicBool::new(false),
            prior: None,
            released: false,
        }
    }

    /// As [`Self::new`], for a silent board's claim that replaced the recovery
    /// `prior` left (DEC-484).
    pub fn with_prior(
        cache: Arc<StateCache>,
        run_id: String,
        prior: Option<(String, &'static str)>,
    ) -> Self {
        let mut guard = Self::new(cache, run_id);
        guard.prior = prior;
        guard
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// From here a panic leaves the board possibly in its bootloader.
    pub fn mark_bootloader_requested(&self) {
        self.bootloader_requested.store(true, Ordering::SeqCst);
    }

    /// From here the board is back and answering.
    pub fn mark_board_answered(&self) {
        self.board_answered.store(true, Ordering::SeqCst);
    }

    /// Release the claim; `needs_recovery` names an outcome that leaves the
    /// board outside normal control.
    pub fn release(mut self, needs_recovery: Option<&'static str>) {
        self.released = true;
        self.cache
            .end_openfan_maintenance(&self.run_id, needs_recovery);
    }

    /// Release the claim of a run that changed nothing and handed nothing
    /// back: the board is as it was, so a recovery the claim replaced is put
    /// back (DEC-484).
    pub fn release_unchanged(mut self) {
        self.released = true;
        match self.prior.take() {
            Some(prior) => self
                .cache
                .end_openfan_maintenance_restoring(&self.run_id, prior),
            None => self.cache.end_openfan_maintenance(&self.run_id, None),
        }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let token = outcome::when_interrupted(
            self.bootloader_requested.load(Ordering::SeqCst),
            self.board_answered.load(Ordering::SeqCst),
        );
        let recovery = outcome::recovery_token(token);
        log::error!(
            "OpenFan firmware update {}: ended without releasing its claim — released now{}",
            self.run_id,
            if recovery.is_some() {
                " as needing recovery"
            } else {
                ""
            }
        );
        match (recovery, self.prior.take()) {
            // Nothing was asked of the board: it is as the last run left it.
            (None, Some(prior)) if token == outcome::NO_FIRMWARE_CHANGE => self
                .cache
                .end_openfan_maintenance_restoring(&self.run_id, prior),
            _ => self.cache.end_openfan_maintenance(&self.run_id, recovery),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::OpenFanLink;

    #[test]
    fn a_cancel_lands_only_while_the_run_is_cancellable() {
        let slot = Arc::new(MaintenanceSlot::default());
        assert_eq!(slot.request_cancel(), CancelOutcome::NotRunning);
        let _alive = slot.claim().unwrap();
        slot.set_record(MaintenanceRecord::new(
            "r".into(),
            "S".into(),
            FirmwareClaim::default(),
        ));
        assert_eq!(slot.request_cancel(), CancelOutcome::Requested);
        assert!(
            slot.close_cancel_window(),
            "the cancel that came first is seen"
        );
        assert_eq!(slot.request_cancel(), CancelOutcome::TooLate);
    }

    #[test]
    fn only_one_run_task_lives_at_a_time() {
        let slot = Arc::new(MaintenanceSlot::default());
        let first = slot.claim().expect("free");
        assert!(slot.claim().is_none());
        drop(first);
        assert!(slot.claim().is_some());
    }

    #[test]
    fn a_dropped_claim_after_bootloader_entry_keeps_writes_suspended() {
        let cache = Arc::new(StateCache::new());
        cache.set_openfan_link(OpenFanLink::Connected);
        cache
            .try_begin_openfan_maintenance("r1", stage::PREPARING)
            .unwrap();
        let guard = ClaimGuard::new(cache.clone(), "r1".into());
        guard.mark_bootloader_requested();
        drop(guard);
        assert!(!cache.openfan_maintenance_running());
        assert!(
            cache.openfan_writes_suspended(),
            "a run that may have left the board in its bootloader keeps writes off"
        );
        cache.set_openfan_link(OpenFanLink::Connected);
        assert!(
            !cache.openfan_writes_suspended(),
            "the board answering again ends it"
        );
    }

    #[test]
    fn a_dropped_claim_before_bootloader_entry_resumes_writes() {
        let cache = Arc::new(StateCache::new());
        cache.set_openfan_link(OpenFanLink::Connected);
        cache
            .try_begin_openfan_maintenance("r1", stage::PREPARING)
            .unwrap();
        assert!(cache.suspend_openfan_writes("r1"));
        assert!(cache.openfan_writes_suspended());
        drop(ClaimGuard::new(cache.clone(), "r1".into()));
        assert!(!cache.openfan_writes_suspended());
        assert!(cache.openfan_maintenance().is_none());
    }

    #[test]
    fn writes_go_on_until_the_run_parks_the_channels() {
        let cache = Arc::new(StateCache::new());
        cache.set_openfan_link(OpenFanLink::Connected);
        cache
            .try_begin_openfan_maintenance("r1", stage::PREPARING)
            .unwrap();
        assert!(cache.openfan_maintenance_running());
        assert!(
            !cache.openfan_writes_suspended() && !cache.openfan_writes_paused(),
            "while the run only reads, the profile and the thermal force still reach the \
             channels"
        );
        assert!(
            !cache.suspend_openfan_writes("another"),
            "only the owner suspends"
        );
        assert!(!cache.openfan_writes_suspended());
        assert!(cache.suspend_openfan_writes("r1"));
        assert!(cache.openfan_writes_suspended() && cache.openfan_writes_paused());
    }

    /// DEC-484: a silent board's claim needs no working link, never takes a
    /// controller that answers, and suspends writes from the start.
    #[test]
    fn a_silent_boards_claim_needs_a_board_that_does_not_answer() {
        let cache = Arc::new(StateCache::new());
        // No poll loop at all: the board was never adopted.
        assert_eq!(
            cache.try_begin_silent_openfan_maintenance("r1", stage::PREPARING),
            Ok(None)
        );
        assert!(cache.openfan_maintenance_running());
        assert!(
            cache.openfan_writes_suspended(),
            "nothing answers, so nothing is lost by suspending at once"
        );
        assert_eq!(
            cache.try_begin_silent_openfan_maintenance("r2", stage::PREPARING),
            Err(crate::health::state::MaintenanceRefusal::MaintenanceActive)
        );
        drop(ClaimGuard::new(cache.clone(), "r1".into()));

        cache.set_openfan_link(OpenFanLink::Reconnecting);
        assert!(cache
            .try_begin_silent_openfan_maintenance("r3", stage::PREPARING)
            .is_ok());
        drop(ClaimGuard::new(cache.clone(), "r3".into()));

        cache.set_openfan_link(OpenFanLink::Connected);
        let refused = cache
            .try_begin_silent_openfan_maintenance("r4", stage::PREPARING)
            .unwrap_err();
        assert_eq!(refused.reason(), "board_not_silent");
        assert!(cache.openfan_maintenance().is_none(), "nothing was claimed");
    }

    /// DEC-484: a silent board's update over a board the last run left needing
    /// recovery takes that claim's place, and gives it back when it changes
    /// nothing — whether it ends cleanly or is dropped before asking anything
    /// of the board.
    #[test]
    fn a_silent_run_that_changes_nothing_puts_the_recovery_back() {
        let cache = Arc::new(StateCache::new());
        let leave_recovery = |run: &str| {
            cache.set_openfan_link(OpenFanLink::Connected);
            cache
                .try_begin_openfan_maintenance(run, stage::PREPARING)
                .unwrap();
            cache.end_openfan_maintenance(run, Some(outcome::FIRMWARE_COPIED_BOARD_NOT_BACK));
            cache.set_openfan_link(OpenFanLink::Reconnecting);
        };
        let recovering = |cache: &StateCache| match cache.openfan_maintenance() {
            Some(crate::health::state::OpenFanMaintenance::NeedsRecovery { run_id, outcome }) => {
                Some((run_id, outcome))
            }
            _ => None,
        };

        leave_recovery("r0");
        let prior = cache
            .try_begin_silent_openfan_maintenance("r1", stage::PREPARING)
            .unwrap();
        assert_eq!(
            prior,
            Some(("r0".to_string(), outcome::FIRMWARE_COPIED_BOARD_NOT_BACK))
        );
        assert!(cache.openfan_maintenance_running(), "the run replaced it");
        ClaimGuard::with_prior(cache.clone(), "r1".into(), prior).release_unchanged();
        assert_eq!(
            recovering(&cache),
            Some(("r0".to_string(), outcome::FIRMWARE_COPIED_BOARD_NOT_BACK))
        );
        assert!(cache.openfan_writes_suspended());

        // Dropped before the board was asked anything: the same.
        let prior = cache
            .try_begin_silent_openfan_maintenance("r2", stage::PREPARING)
            .unwrap();
        drop(ClaimGuard::with_prior(cache.clone(), "r2".into(), prior));
        assert_eq!(recovering(&cache).map(|r| r.0), Some("r0".to_string()));

        // Dropped after the signal: its own recovery, not the old one.
        let prior = cache
            .try_begin_silent_openfan_maintenance("r3", stage::PREPARING)
            .unwrap();
        let guard = ClaimGuard::with_prior(cache.clone(), "r3".into(), prior);
        guard.mark_bootloader_requested();
        drop(guard);
        assert_eq!(
            recovering(&cache),
            Some(("r3".to_string(), outcome::NEEDS_RECOVERY))
        );

        // A run that brought the board back ends the recovery.
        let prior = cache
            .try_begin_silent_openfan_maintenance("r4", stage::PREPARING)
            .unwrap();
        assert!(prior.is_some());
        ClaimGuard::with_prior(cache.clone(), "r4".into(), prior).release(None);
        assert!(cache.openfan_maintenance().is_none());
        assert!(!cache.openfan_writes_suspended());
    }

    #[test]
    fn registration_closes_at_shutdown() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let slot = MaintenanceSlot::default();
            assert!(slot.register(|| tokio::spawn(async {})));
            let drained = slot.close_and_drain();
            assert_eq!(drained.len(), 1);
            assert!(!slot.register(|| tokio::spawn(async {})));
            assert!(slot.is_closed());
        });
    }
}
