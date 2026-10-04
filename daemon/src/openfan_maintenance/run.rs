//! A firmware update's stages (DEC-481, DEC-483).
//!
//! 1. **Preparing** — read the board's USB identity and its `>05`/`>06` blocks.
//! 2. **Parking** — every channel to 100 % through the normal write path; any
//!    failure ends the run with no firmware change. Last point a cancel lands.
//! 3. **Entering the bootloader** — borrow the port, send `>07` (no reply); if
//!    the board is still in normal mode after a while, switch the same port to
//!    1200 baud. The bootloader must then appear on the same USB port.
//! 4. **Writing the firmware** — only when the start asked the daemon to: open
//!    the PICOBOOT interface of the bootloader on that port, read its flash id
//!    and find the board's serial in it, erase and program each sector the
//!    image covers, read every byte back, restart the board. Anything that
//!    stops it first gives the drive back and goes on to stage 5.
//! 5. **Waiting for the file** — the user copies it onto the board's drive.
//!    Skipped after a write the board restarted from.
//! 6. **Waiting for the board** — the same serial on the same USB port.
//! 7. **Checking** — open only the interface the daemon used, `>00`,
//!    `>05`/`>06`, the evidence.
//! 8. **Restoring control** — hand the port back, lift the write suspension,
//!    wait for fresh polls of every channel and for the settings to land.
//!
//! **A silent board** (DEC-484, [`Target::Silent`]) takes another way to its
//! bootloader, and the same one on from there:
//! 1. **Preparing** — confirm in sysfs that the board is still on its USB port
//!    running its firmware; take the adoption probes' single-flight flag, so no
//!    probe opens the board from here until it is handed over; borrow the poll
//!    loop's port, whatever the link, when a loop runs, so its reconnect search
//!    stops. Nothing is parked: the board takes no commands.
//! 2. **Entering the bootloader** — open the board's first serial interface
//!    (or use the borrowed port, while it is one) and run the handshake once
//!    more: a board that answers is handed back unchanged. Otherwise the
//!    1200-baud signal alone — never `>07`, which firmware that does not speak
//!    the protocol may read as something else.
//! 3. **Waiting for the BOOT button** — when the bootloader has not appeared,
//!    the user holds BOOT and presses RESET. Cancellable until it appears.
//!
//! Then stages 4–8 as above. A board that comes back where no poll loop runs
//! is adopted, which starts one.
//!
//! Every wait has a limit and watches the stop signal; every blocking call
//! (serial, sysfs, USB) runs on the blocking pool.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::watch;

use super::{
    evidence, fallback, firmware, journal, outcome, stage, trigger, write_phase, AliveGuard,
    BoardSnapshot, ClaimGuard, FirmwareWrite, MaintenanceRecord, MaintenanceSlot, StageTiming,
    STATE_FINISHED,
};
use crate::constants;
use crate::error::SerialError;
use crate::health::cache::StateCache;
use crate::serial::controller::FanController;
use crate::serial::picoboot::{self, PicobootError};
use crate::serial::port_loan::{self, LoanHandle, LoanReturn, LoanSender};
use crate::serial::protocol::{
    encode_bare, FW_INFO_OPCODE, HW_INFO_OPCODE, JUMP_TO_BOOTLOADER_OPCODE, NUM_CHANNELS,
};
use crate::serial::transport::{read_info_block, verify_openfan_identity, SerialTransport};
use crate::serial::usb_identity as usb;

/// Opens the board's serial interface after it comes back.
pub type Opener = Arc<
    dyn Fn(&str, Duration) -> Result<Box<dyn SerialTransport + Send>, SerialError> + Send + Sync,
>;

/// The channels whose settings must have landed before control counts as
/// restored: the active profile's OpenFan channels, or every channel while
/// the thermal force is active.
pub type ExpectedChannels = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

/// Adopt the board a silent board's update brought back, where no poll loop
/// runs (DEC-484): install it as the controller and start its loop, on the
/// adoption path every probe uses. Blocking pool. `false` when nothing was
/// installed.
pub type AdoptFn = Arc<dyn Fn(Box<dyn SerialTransport + Send>, String) -> bool + Send + Sync>;

/// The board a run is for, and what reaching it takes.
pub enum Target {
    /// A board that answers (DEC-481): read, parked, sent `>07`.
    Connected {
        controller: Arc<Mutex<FanController>>,
        lender: LoanSender,
    },
    /// A board on USB that does not answer (DEC-484).
    Silent(SilentTarget),
}

/// A silent board, and the handles its update needs.
pub struct SilentTarget {
    /// The board's USB port (`8-8`).
    pub usb_port: String,
    /// The serial interface the signal goes to, and the one reopened after.
    pub interface_number: u8,
    /// The poll loop's lender, when a controller was adopted and its loop runs.
    pub lender: Option<LoanSender>,
    /// The adoption probes' single-flight flag (`AppState::openfan_rescanning`).
    pub probe_gate: Arc<AtomicBool>,
    /// Adopts the returned board where no poll loop runs.
    pub adopt: AdoptFn,
}

/// The probes' single-flight flag, held by a silent board's update; dropping
/// it lets probes run again.
struct GateHold(Arc<AtomicBool>);

impl Drop for GateHold {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Each stage's limits. Production uses [`StageLimits::production`]; tests
/// shrink them.
#[derive(Debug, Clone, Copy)]
pub struct StageLimits {
    pub borrow_wait: Duration,
    pub prepare: Duration,
    /// A silent board's update: how long an adoption probe already running
    /// has to finish (DEC-484).
    pub probe_wait: Duration,
    /// A silent board's update: how long the user has to press BOOT and RESET.
    pub boot_button_wait: Duration,
    pub park: Duration,
    pub trigger_wait: Duration,
    pub bootloader_wait: Duration,
    pub enter: Duration,
    pub file_wait: Duration,
    pub return_wait: Duration,
    pub check_wait: Duration,
    pub restore_wait: Duration,
    pub sysfs_poll: Duration,
    /// Pause between handshake attempts on the returned board.
    pub check_retry: Duration,
    /// The daemon's own write: this, plus `write_per_sector` for each sector.
    pub write_base: Duration,
    pub write_per_sector: Duration,
    /// After the restart is asked for, how long the bootloader has to leave.
    pub reboot_wait: Duration,
}

impl StageLimits {
    pub fn production() -> Self {
        Self {
            borrow_wait: constants::OPENFAN_MAINT_BORROW_WAIT,
            prepare: constants::OPENFAN_MAINT_PREPARE_LIMIT,
            probe_wait: constants::OPENFAN_MAINT_PROBE_WAIT,
            boot_button_wait: constants::OPENFAN_MAINT_BOOT_BUTTON_WAIT,
            park: constants::OPENFAN_MAINT_PARK_LIMIT,
            trigger_wait: constants::OPENFAN_MAINT_TRIGGER_WAIT,
            bootloader_wait: constants::OPENFAN_MAINT_BOOTLOADER_WAIT,
            enter: constants::OPENFAN_MAINT_ENTER_LIMIT,
            file_wait: constants::OPENFAN_MAINT_FILE_WAIT,
            return_wait: constants::OPENFAN_MAINT_RETURN_WAIT,
            check_wait: constants::OPENFAN_MAINT_CHECK_WAIT,
            restore_wait: constants::OPENFAN_MAINT_RESTORE_WAIT,
            sysfs_poll: constants::OPENFAN_MAINT_SYSFS_POLL,
            check_retry: Duration::from_millis(500),
            write_base: constants::OPENFAN_MAINT_WRITE_BASE,
            write_per_sector: constants::OPENFAN_MAINT_WRITE_PER_SECTOR,
            reboot_wait: constants::OPENFAN_MAINT_REBOOT_WAIT,
        }
    }
}

/// Everything a run needs, injected so a test can drive it without hardware.
pub struct RunEnv {
    pub cache: Arc<StateCache>,
    pub slot: Arc<MaintenanceSlot>,
    pub target: Target,
    pub sys_root: PathBuf,
    pub journal_path: PathBuf,
    pub limits: StageLimits,
    pub serial_timeout: Duration,
    pub open: Opener,
    pub expected_channels: ExpectedChannels,
    pub shutdown: watch::Receiver<bool>,
    /// The file the daemon writes itself, when the start asked it to
    /// (DEC-483); `None` when the user copies it.
    pub write: Option<firmware::Staged>,
    /// Opens the PICOBOOT interface of the bootloader on a USB port.
    pub picoboot: picoboot::Opener,
}

/// Start a run and the supervisor that records an internal error if it
/// panics. Returns the supervisor's handle, which is what the shutdown drain
/// waits on.
pub fn spawn(env: RunEnv, claim: ClaimGuard, alive: AliveGuard) -> tokio::task::JoinHandle<()> {
    let slot = env.slot.clone();
    let journal_path = env.journal_path.clone();
    let run_id = claim.run_id().to_string();
    let task = tokio::spawn(run(env, claim));
    tokio::spawn(async move {
        // Held until the record is final, so no second run starts before it.
        let _alive = alive;
        let Err(e) = task.await else {
            return;
        };
        if !e.is_panic() {
            return;
        }
        // The claim and the loan went with the unwinding task: the claim
        // guard released it (as needing recovery once the bootloader was
        // asked for), and the dropped loan handle reads as "nothing back" to
        // the poll loop. What is left is the record.
        let at = slot.record().map(|r| r.stage).unwrap_or_default();
        log::error!("OpenFan firmware update {run_id}: internal error during {at}: {e}");
        let finished = slot.update_record(|r| {
            if r.is_running() {
                let token = outcome::when_interrupted(r.bootloader_requested, r.board_answered);
                finish_record(r, token, format!("internal error during {at}"), false);
            }
        });
        if let Some(record) = finished {
            save_journal(&journal_path, &run_id, &record).await;
        }
    })
}

/// Write the journal. It fsyncs, so it runs on the blocking pool — the
/// runtime's workers are shared with the engine — and the caller awaits it
/// before acting. A failure is logged and the run goes on ([`journal::save`]).
async fn save_journal(path: &Path, run_id: &str, record: &MaintenanceRecord) {
    let (path, record) = (path.to_path_buf(), record.clone());
    if let Some(Err(e)) = blocking(move || journal::save(&path, &record)).await {
        log::warn!("OpenFan firmware update {run_id}: journal could not be written: {e}");
    }
}

/// Close `record` with `token`.
pub(crate) fn finish_record(
    record: &mut MaintenanceRecord,
    token: &str,
    detail: String,
    interrupted: bool,
) {
    let now = crate::control_paths::unix_ms();
    record.state = STATE_FINISHED.to_string();
    record.cancellable = false;
    record.finished_unix_ms = Some(now);
    record.stage_deadline_unix_ms = None;
    record.outcome = Some(token.to_string());
    record.outcome_detail = Some(detail);
    record.interrupted |= interrupted;
    if let Some(last) = record.stages.last_mut() {
        last.ended_unix_ms.get_or_insert(now);
    }
}

/// How a run ended.
struct End {
    outcome: &'static str,
    detail: String,
    interrupted: bool,
    cancelled: bool,
}

impl End {
    fn new(outcome: &'static str, detail: impl Into<String>) -> Self {
        Self {
            outcome,
            detail: detail.into(),
            interrupted: false,
            cancelled: false,
        }
    }

    /// A `DELETE` that landed before the port was borrowed: nothing changed.
    fn cancelled(detail: impl Into<String>) -> Self {
        Self {
            cancelled: true,
            ..Self::new(outcome::NO_FIRMWARE_CHANGE, detail)
        }
    }
}

/// What a sysfs or cache watch came to.
enum Watched<T> {
    Found(T),
    Deadline,
    Shutdown,
}

/// What the board's USB port holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortHolds {
    /// The board, running firmware: the expected serial.
    Board,
    Bootloader,
    /// A different device.
    Other,
    Empty,
}

fn port_holds(sys: &Path, port: &str, serial: &str) -> PortHolds {
    match usb::device_at(sys, port) {
        None => PortHolds::Empty,
        Some(d) if d.is_rp2040_bootloader() => PortHolds::Bootloader,
        Some(d) if d.vendor_id == usb::RP2040_VENDOR_ID && d.serial.as_deref() == Some(serial) => {
            PortHolds::Board
        }
        Some(_) => PortHolds::Other,
    }
}

/// What appeared on the port after the board left normal mode.
enum Landed {
    Bootloader,
    /// The board's firmware again, with the serial device to reopen.
    Firmware(String),
}

/// What came back on the port after the drive went away.
enum Back {
    /// The board, with the serial device to reopen.
    Board(String),
    /// The bootloader again: the copied file did not start.
    Bootloader,
}

/// The `RPI-RP2` drives sysfs shows: the board's, and any other board's in its
/// bootloader. `disks` is the `/sys/block` listing they were read against, so a
/// watch reads them again only when a disk comes or goes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Drives {
    disks: Vec<String>,
    ours: Option<String>,
    others: Vec<String>,
}

impl Drives {
    fn read(sys: &Path, port: &str, disks: Vec<String>) -> Self {
        let ours = usb::block_devices_of(sys, port).into_iter().next();
        let others = usb::bootloaders(sys)
            .into_iter()
            .filter(|d| d.port != port)
            .flat_map(|d| usb::block_devices_of(sys, &d.port))
            .collect();
        Self {
            disks,
            ours,
            others,
        }
    }
}

/// What the wait for the file saw.
enum FileWait {
    /// The bootloader left the port: the copy finished, or the board was reset.
    Left,
    /// A disk came or went while the bootloader stayed.
    Drives(Drives),
}

const NO_FILE_IN_TIME: &str =
    "no firmware file was copied in time — the board is still in its bootloader";

/// How the daemon's own write ended.
enum Written {
    /// Written, read back, and the board restarted from it.
    Done,
    /// The run goes on to the copy by hand.
    FellBack,
    /// The daemon is stopping.
    Stopped,
}

/// Run `f` on the bootloader's PICOBOOT client on the blocking pool, and get
/// the client back with its result; `None` if the task panicked, which takes
/// the client with it.
async fn on_usb<T: Send + 'static>(
    mut client: picoboot::Client,
    f: impl FnOnce(&mut picoboot::Client) -> Result<T, PicobootError> + Send + 'static,
) -> Option<(picoboot::Client, Result<T, PicobootError>)> {
    blocking(move || {
        let result = f(&mut client);
        (client, result)
    })
    .await
}

/// Give the bootloader's drive back to the user — exclusive access off, so a
/// copy onto it is accepted again — and close the client. picotool's way out:
/// ask, and reset the interface if the asking fails.
async fn release_drive(client: picoboot::Client) -> bool {
    blocking(move || {
        let mut c = client;
        c.exclusive_access(picoboot::NOT_EXCLUSIVE).is_ok()
            || (c.reset().is_ok() && c.exclusive_access(picoboot::NOT_EXCLUSIVE).is_ok())
    })
    .await
    .unwrap_or(false)
}

fn to_map(pairs: Result<Vec<(String, String)>, SerialError>) -> Option<BTreeMap<String, String>> {
    pairs.ok().map(|p| p.into_iter().collect())
}

/// Run `f` on the blocking pool; `None` if it panicked.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => Some(v),
        Err(e) => {
            log::error!("OpenFan firmware update: a blocking step panicked: {e}");
            None
        }
    }
}

struct Runner {
    env: RunEnv,
    run_id: String,
    claim: Option<ClaimGuard>,
    /// The borrowed port while the run holds it; `None` after the 1200-baud
    /// signal consumed it, and once it is given back.
    port: Option<Box<dyn SerialTransport + Send>>,
    /// The loan, until it is settled.
    handle: Option<LoanHandle>,
    /// A silent board's update holds the probes' flag until it ends (DEC-484).
    gate: Option<GateHold>,
    /// A board that answers was handed to the poll loop, or adopted.
    handed_over: bool,
}

/// Where a run reached the bootloader: the board's USB port and the serial
/// interface reopened after.
struct Entered {
    port: String,
    ifnum: u8,
}

/// What the silent board said when it was asked once more.
enum Asked {
    Answers(Box<dyn SerialTransport + Send>),
    Silent(Box<dyn SerialTransport + Send>, String),
}

async fn run(env: RunEnv, claim: ClaimGuard) {
    let run_id = claim.run_id().to_string();
    let mut runner = Runner {
        env,
        run_id,
        claim: Some(claim),
        port: None,
        handle: None,
        gate: None,
        handed_over: false,
    };
    let end = runner.drive().await;
    runner.conclude(end).await;
}

impl Runner {
    fn shutting_down(&self) -> bool {
        *self.env.shutdown.borrow()
    }

    /// Change the record and write the journal before anything else happens.
    /// `&mut self` because the future must be `Send`: the run holds its port,
    /// which is not `Sync`.
    async fn record(
        &mut self,
        f: impl FnOnce(&mut MaintenanceRecord),
    ) -> Option<MaintenanceRecord> {
        let updated = self.env.slot.update_record(f)?;
        save_journal(&self.env.journal_path, &self.run_id, &updated).await;
        Some(updated)
    }

    async fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::info!("OpenFan firmware update {}: {text}", self.run_id);
        self.record(|r| r.notes.push(text)).await;
    }

    /// Move to `next`, whose limit is `limit` from now.
    async fn enter(&mut self, next: &'static str, limit: Duration) {
        self.enter_until(next, Instant::now() + limit).await;
    }

    /// Move to `next`, which ends at `deadline`.
    async fn enter_until(&mut self, next: &'static str, deadline: Instant) {
        let now_ms = crate::control_paths::unix_ms();
        let left = deadline.saturating_duration_since(Instant::now());
        self.record(|r| {
            if let Some(last) = r.stages.last_mut() {
                last.ended_unix_ms.get_or_insert(now_ms);
            }
            r.stages.push(StageTiming {
                stage: next.to_string(),
                started_unix_ms: now_ms,
                ended_unix_ms: None,
            });
            r.stage = next.to_string();
            r.stage_started_unix_ms = now_ms;
            r.stage_deadline_unix_ms = Some(now_ms + left.as_millis() as u64);
        })
        .await;
        self.env
            .cache
            .set_openfan_maintenance_stage(&self.run_id, next, Some(deadline));
        log::info!("OpenFan firmware update {}: {next}", self.run_id);
    }

    /// How a run the daemon is stopping ends.
    fn interrupted(&self) -> End {
        let record = self.env.slot.record();
        let (requested, answered, at) = record
            .map(|r| (r.bootloader_requested, r.board_answered, r.stage))
            .unwrap_or_default();
        End {
            interrupted: true,
            ..End::new(
                outcome::when_interrupted(requested, answered),
                format!("interrupted by a daemon stop during {at}"),
            )
        }
    }

    /// Re-read sysfs until `check` finds something, `limit` passes, or stop.
    async fn watch_sysfs<T: Send + 'static>(
        &mut self,
        limit: Duration,
        check: impl Fn(&Path) -> Option<T> + Send + Sync + 'static,
    ) -> Watched<T> {
        let deadline = Instant::now() + limit;
        let check = Arc::new(check);
        loop {
            if self.shutting_down() {
                return Watched::Shutdown;
            }
            let (c, root) = (check.clone(), self.env.sys_root.clone());
            if let Some(Some(found)) = blocking(move || c(&root)).await {
                return Watched::Found(found);
            }
            let now = Instant::now();
            if now >= deadline {
                return Watched::Deadline;
            }
            let nap = self.env.limits.sysfs_poll.min(deadline - now);
            tokio::select! {
                biased;
                _ = self.env.shutdown.changed() => return Watched::Shutdown,
                _ = tokio::time::sleep(nap) => {}
            }
        }
    }

    /// Sleep `nap`, or return `false` on a stop.
    async fn pause(&mut self, nap: Duration) -> bool {
        if self.shutting_down() {
            return false;
        }
        tokio::select! {
            biased;
            _ = self.env.shutdown.changed() => false,
            _ = tokio::time::sleep(nap) => true,
        }
    }

    async fn drive(&mut self) -> End {
        let serial = self
            .env
            .slot
            .record()
            .map(|r| r.expected_usb_serial)
            .unwrap_or_default();
        let entered = if matches!(self.env.target, Target::Silent(_)) {
            self.enter_silent(&serial).await
        } else {
            self.enter_connected(&serial).await
        };
        match entered {
            Ok(entered) => self.onward_from_bootloader(entered, serial).await,
            Err(end) => end,
        }
    }

    /// Stages 1–3 for a board that answers (DEC-481): read it, park every
    /// channel, borrow the port and send `>07`, then the 1200-baud signal on
    /// the same port. `Ok` once the bootloader is on the board's USB port.
    async fn enter_connected(&mut self, serial: &str) -> Result<Entered, End> {
        let limits = self.env.limits;
        let serial = serial.to_string();
        let (controller, lender) = match &self.env.target {
            Target::Connected { controller, lender } => (controller.clone(), lender.clone()),
            Target::Silent(_) => {
                return Err(End::new(
                    outcome::NO_FIRMWARE_CHANGE,
                    "the update was started for a board that does not answer",
                ))
            }
        };

        // ── 1. Preparing ──────────────────────────────────────────────────
        self.enter(stage::PREPARING, limits.prepare).await;
        let Some(adopted) = self.env.cache.openfan_port() else {
            return Err(End::new(
                outcome::NO_FIRMWARE_CHANGE,
                "the controller's serial device is not known",
            ));
        };
        let sys = self.env.sys_root.clone();
        let identity = blocking(move || usb::tty_identity(&sys, &adopted))
            .await
            .flatten();
        let Some(identity) = identity else {
            return Err(End::new(
                outcome::NO_FIRMWARE_CHANGE,
                "the board's USB identity could not be read",
            ));
        };
        if identity.device.serial.as_deref() != Some(serial.as_str()) {
            return Err(End::new(
                outcome::NO_FIRMWARE_CHANGE,
                "the connected board is not the one the update was started for",
            ));
        }
        let port = identity.device.port.clone();
        let ifnum = identity.interface_number;
        let tty = format!("/dev/{}", identity.tty);
        let ctrl = controller.clone();
        let (hw, fw) = blocking(move || {
            let mut c = ctrl.lock();
            (
                to_map(c.read_info(HW_INFO_OPCODE)),
                to_map(c.read_info(FW_INFO_OPCODE)),
            )
        })
        .await
        .unwrap_or((None, None));
        self.record(|r| {
            r.usb_port = Some(port.clone());
            r.interface_number = Some(ifnum);
            r.tty = Some(tty.clone());
            r.before = BoardSnapshot {
                usb: Some(identity.device.clone()),
                hw_info: hw,
                fw_info: fw,
            };
        })
        .await;
        if self.env.slot.cancel_requested() {
            return Err(End::cancelled("cancelled before the fans were touched"));
        }
        if self.shutting_down() {
            return Err(self.interrupted());
        }

        // ── 2. Parking ────────────────────────────────────────────────────
        self.enter(stage::PARKING, limits.park).await;
        // [SAFETY] From here the engine and the thermal force leave the
        // channels alone, or the profile would lower them again before the
        // bootloader request. 100 % is the most either could ask for.
        if !self.env.cache.suspend_openfan_writes(&self.run_id) {
            return Err(End::new(
                outcome::NO_FIRMWARE_CHANGE,
                "the update lost its claim on the controller",
            ));
        }
        for ch in 0..NUM_CHANNELS {
            if self.env.slot.cancel_requested() {
                return Err(End::cancelled(
                    "cancelled while parking the fans — they return to profile control",
                ));
            }
            let ctrl = controller.clone();
            match blocking(move || ctrl.lock().set_pwm(ch, 100)).await {
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    return Err(End::new(
                        outcome::NO_FIRMWARE_CHANGE,
                        format!("parking channel {ch} at 100 % failed ({e})"),
                    ))
                }
                None => {
                    return Err(End::new(
                        outcome::NO_FIRMWARE_CHANGE,
                        format!("parking channel {ch} at 100 % failed"),
                    ))
                }
            }
        }
        // The last point a cancel lands, decided with the request under one lock.
        if self.env.slot.close_cancel_window() {
            return Err(End::cancelled(
                "cancelled after parking the fans — they return to profile control",
            ));
        }
        if self.shutting_down() {
            return Err(self.interrupted());
        }

        // ── 3. Entering the bootloader ────────────────────────────────────
        self.enter(stage::ENTERING_BOOTLOADER, limits.enter).await;
        let loan = match port_loan::borrow(&lender, limits.borrow_wait).await {
            Ok(loan) => loan,
            Err(why) => {
                return Err(End::new(
                    outcome::NO_FIRMWARE_CHANGE,
                    format!("the port could not be borrowed: {}", why.describe()),
                ))
            }
        };
        self.handle = Some(loan.handle);
        // Journaled before the command goes out: from here the board may be in
        // its bootloader whatever happens next.
        self.record(|r| {
            r.bootloader_requested = true;
            r.bootloader_trigger = Some(trigger::JUMP.into());
        })
        .await;
        if let Some(claim) = &self.claim {
            claim.mark_bootloader_requested();
        }
        let mut lent = loan.transport;
        let (lent, sent) = blocking(move || {
            let sent = lent.write_line(&encode_bare(JUMP_TO_BOOTLOADER_OPCODE));
            (lent, sent)
        })
        .await
        .map_or(
            (
                None,
                Err(SerialError::Protocol {
                    message: "the write task failed".into(),
                }),
            ),
            |(t, s)| (Some(t), s),
        );
        self.port = lent;
        if let Err(e) = sent {
            self.note(format!("sending >07 failed ({e})")).await;
        }

        let (p, s) = (port.clone(), serial.clone());
        let left = move |sys: &Path| (port_holds(sys, &p, &s) != PortHolds::Board).then_some(());
        let mut gone = match self.watch_sysfs(limits.trigger_wait, left.clone()).await {
            Watched::Found(()) => true,
            Watched::Deadline => false,
            Watched::Shutdown => return Err(self.interrupted()),
        };
        if !gone {
            // Still in normal mode: the 1200-baud signal, on the same port.
            self.record(|r| r.bootloader_trigger = Some(trigger::BAUD_1200.into()))
                .await;
            if let Some(held) = self.port.take() {
                match blocking(move || held.touch_1200_baud()).await {
                    Some(Ok(())) => {}
                    Some(Err(e)) => {
                        self.note(format!("the 1200-baud signal failed ({e})"))
                            .await
                    }
                    None => self.note("the 1200-baud signal failed").await,
                }
            }
            gone = match self.watch_sysfs(limits.trigger_wait, left).await {
                Watched::Found(()) => true,
                Watched::Deadline => false,
                Watched::Shutdown => return Err(self.interrupted()),
            };
        }
        if !gone {
            return Err(self
                .in_firmware(&tty, "the board did not leave normal mode")
                .await);
        }
        let (p, s) = (port.clone(), serial.clone());
        let appeared = move |sys: &Path| match port_holds(sys, &p, &s) {
            PortHolds::Bootloader => Some(Landed::Bootloader),
            PortHolds::Board => usb::tty_for(sys, &p, ifnum).map(Landed::Firmware),
            PortHolds::Other | PortHolds::Empty => None,
        };
        match self.watch_sysfs(limits.bootloader_wait, appeared).await {
            Watched::Found(Landed::Bootloader) => {}
            Watched::Found(Landed::Firmware(tty)) => {
                // It restarted, but into its firmware. The port the run held
                // went with the device it was opened on: reopen.
                self.drop_port().await;
                return Err(self
                    .in_firmware(
                        &tty,
                        "the board restarted into its firmware instead of its bootloader",
                    )
                    .await);
            }
            Watched::Deadline => {
                return Err(End::new(
                    outcome::NEEDS_RECOVERY,
                    "the board left normal mode, but no bootloader appeared on its USB port",
                ))
            }
            Watched::Shutdown => return Err(self.interrupted()),
        }
        Ok(Entered { port, ifnum })
    }

    /// Stages 1–3 for a silent board (DEC-484). `Ok` once the bootloader is on
    /// the board's USB port.
    async fn enter_silent(&mut self, serial: &str) -> Result<Entered, End> {
        let limits = self.env.limits;
        let serial = serial.to_string();
        let (port, ifnum, lender, gate) = match &self.env.target {
            Target::Silent(t) => (
                t.usb_port.clone(),
                t.interface_number,
                t.lender.clone(),
                t.probe_gate.clone(),
            ),
            Target::Connected { .. } => {
                return Err(End::new(
                    outcome::NO_FIRMWARE_CHANGE,
                    "the update was started for a board that answers",
                ))
            }
        };

        // ── 1. Preparing ──────────────────────────────────────────────────
        self.enter(
            stage::PREPARING,
            limits.prepare + limits.probe_wait + limits.borrow_wait,
        )
        .await;
        let (sys, p, s) = (self.env.sys_root.clone(), port.clone(), serial.clone());
        let found = blocking(move || {
            let device = usb::device_at(&sys, &p)?;
            if !device.is_openfan_board() || device.serial.as_deref() != Some(s.as_str()) {
                return None;
            }
            let tty = usb::tty_for(&sys, &p, ifnum)?;
            Some((device, tty))
        })
        .await
        .flatten();
        let Some((device, tty)) = found else {
            return Err(End::new(
                outcome::NO_FIRMWARE_CHANGE,
                "the board is no longer on its USB port running its firmware",
            ));
        };
        self.record(|r| {
            r.usb_port = Some(port.clone());
            r.interface_number = Some(ifnum);
            r.tty = Some(tty.clone());
            r.before = BoardSnapshot {
                usb: Some(device),
                hw_info: None,
                fw_info: None,
            };
        })
        .await;
        // [SAFETY] From here no adoption probe opens the board until it has
        // been handed over: every probe after boot runs under this flag, and
        // one already running is waited for. The poll loop's reconnect search
        // is parked by the loan below instead.
        let deadline = Instant::now() + limits.probe_wait;
        while self.gate.is_none() {
            if gate
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                self.gate = Some(GateHold(gate.clone()));
                break;
            }
            if self.env.slot.cancel_requested() {
                return Err(End::cancelled("cancelled before the board was signalled"));
            }
            if Instant::now() >= deadline {
                return Err(End::new(
                    outcome::NO_FIRMWARE_CHANGE,
                    "an OpenFan probe that was already running did not finish in time",
                ));
            }
            if !self.pause(limits.sysfs_poll).await {
                return Err(self.interrupted());
            }
        }
        if let Some(lender) = lender {
            match port_loan::borrow_any(&lender, limits.borrow_wait).await {
                Ok(loan) => {
                    self.handle = Some(loan.handle);
                    self.port = Some(loan.transport);
                }
                Err(why) => {
                    return Err(End::new(
                        outcome::NO_FIRMWARE_CHANGE,
                        format!(
                            "the OpenFan poll loop could not be paused: {}",
                            why.describe()
                        ),
                    ))
                }
            }
        }
        if self.env.slot.cancel_requested() {
            return Err(End::cancelled("cancelled before the board was signalled"));
        }
        if self.shutting_down() {
            return Err(self.interrupted());
        }

        // ── 2. Entering the bootloader ────────────────────────────────────
        self.enter(stage::ENTERING_BOOTLOADER, limits.enter).await;
        // Asked once more before the signal: a board that answers now is not
        // silent, and is never sent a signal it was not parked for.
        // [SAFETY] The borrowed port is asked only while it is open on this
        // board's node. A poll loop that has not yet let go of the port of the
        // board it was adopted for lends that one, and the signal must never
        // reach another board — one that may be driving fans.
        let lent_on_target = self.env.cache.openfan_port().as_deref() == Some(tty.as_str());
        let (held, open, path) = (self.port.take(), self.env.open.clone(), tty.clone());
        let timeout = self.env.serial_timeout;
        let asked = blocking(move || {
            // The borrowed port while it is this board's — its lock would
            // refuse a fresh open of the node — and otherwise the node itself.
            let mut t = match held {
                Some(t) if !t.is_placeholder() && lent_on_target => t,
                Some(mut other) => {
                    // Closed without waiting for output a board that is not
                    // reading would never take (`DC-ct`).
                    other.discard_pending_output();
                    drop(other);
                    open(&path, timeout)?
                }
                None => open(&path, timeout)?,
            };
            Ok::<_, SerialError>(match verify_openfan_identity(&mut *t, timeout) {
                Ok(()) => Asked::Answers(t),
                Err(e) => Asked::Silent(t, e.to_string()),
            })
        })
        .await;
        let signalled = match asked {
            Some(Ok(Asked::Answers(t))) => {
                self.hand_over(t, tty.clone()).await?;
                return Err(End::new(
                    outcome::NO_FIRMWARE_CHANGE,
                    "the board answers Control-OFC — it is not silent, nothing was changed, and \
                     control resumes",
                ));
            }
            Some(Ok(Asked::Silent(t, why))) => {
                self.note(format!("the board does not answer ({why})"))
                    .await;
                // Journaled before the signal: from here the board may be in
                // its bootloader whatever happens next.
                self.record(|r| {
                    r.bootloader_requested = true;
                    r.bootloader_trigger = Some(trigger::BAUD_1200.into());
                })
                .await;
                if let Some(claim) = &self.claim {
                    claim.mark_bootloader_requested();
                }
                match blocking(move || t.touch_1200_baud()).await {
                    Some(Ok(())) => {}
                    Some(Err(e)) => {
                        self.note(format!("the 1200-baud signal failed ({e})"))
                            .await
                    }
                    None => self.note("the 1200-baud signal failed").await,
                }
                true
            }
            Some(Err(e)) => {
                self.note(format!(
                    "the board's serial device could not be opened ({e}) — no signal was sent"
                ))
                .await;
                false
            }
            None => {
                self.note("opening the board's serial device failed — no signal was sent")
                    .await;
                false
            }
        };
        let mut landed = false;
        if signalled {
            let (p, s) = (port.clone(), serial.clone());
            let left =
                move |sys: &Path| (port_holds(sys, &p, &s) != PortHolds::Board).then_some(());
            let gone = match self.watch_sysfs(limits.trigger_wait, left).await {
                Watched::Found(()) => true,
                Watched::Deadline => false,
                Watched::Shutdown => return Err(self.interrupted()),
            };
            if gone {
                let (p, s) = (port.clone(), serial.clone());
                let appeared = move |sys: &Path| {
                    (port_holds(sys, &p, &s) == PortHolds::Bootloader).then_some(())
                };
                landed = match self.watch_sysfs(limits.bootloader_wait, appeared).await {
                    Watched::Found(()) => true,
                    Watched::Deadline => false,
                    Watched::Shutdown => return Err(self.interrupted()),
                };
            }
        }
        if !landed {
            self.wait_for_boot_button(&port, &serial).await?;
        }
        // Past this point a cancel would leave the board in its bootloader
        // with nothing to bring it back: the run goes on to the file.
        if self.env.slot.close_cancel_window() {
            self.note(
                "a cancel arrived as the board entered its bootloader — the update goes on, \
                 since stopping now would leave the board there",
            )
            .await;
        }
        Ok(Entered { port, ifnum })
    }

    /// Stage 3 for a silent board the signal did not take to its bootloader
    /// (DEC-484): the user holds BOOT and presses RESET. `Ok` once the
    /// bootloader is on the board's USB port; a cancel, or the time running
    /// out, ends the run while it is not.
    async fn wait_for_boot_button(&mut self, port: &str, serial: &str) -> Result<(), End> {
        let limits = self.env.limits;
        // From here the user may put the board in its bootloader, signal or no.
        self.record(|r| {
            r.bootloader_requested = true;
            r.bootloader_trigger = Some(trigger::BOOT_BUTTON.into());
        })
        .await;
        if let Some(claim) = &self.claim {
            claim.mark_bootloader_requested();
        }
        // Published with the time a bootloader takes to appear: a board the
        // buttons restart just before the wait runs out is waited for (below),
        // and that wait must not read as a stage overrunning its limit.
        self.enter(
            stage::WAITING_FOR_BOOT_BUTTON,
            limits.boot_button_wait + limits.bootloader_wait,
        )
        .await;
        // The wait for the buttons itself.
        let deadline = Instant::now() + limits.boot_button_wait;
        self.note(
            "the board did not enter its bootloader — hold its BOOT button, press and release \
             RESET, then release BOOT",
        )
        .await;
        let mut empty_since: Option<Instant> = None;
        // How the wait ends, once one more look at the port has confirmed it.
        let mut ending: Option<End> = None;
        loop {
            let (sys, p, s) = (
                self.env.sys_root.clone(),
                port.to_string(),
                serial.to_string(),
            );
            let holds = blocking(move || port_holds(&sys, &p, &s))
                .await
                .unwrap_or(PortHolds::Empty);
            if holds == PortHolds::Bootloader {
                return Ok(());
            }
            // The buttons empty the port for a moment as the board restarts: a
            // cancel — and the time running out — then waits as long as a
            // bootloader takes to appear, so a board on its way into one is not
            // left there with the run ended.
            let settling = if holds == PortHolds::Empty {
                empty_since.get_or_insert_with(Instant::now).elapsed() < limits.bootloader_wait
            } else {
                empty_since = None;
                false
            };
            // A cancel, or the time running out, ends the wait only once the
            // next look still finds the board where it was: the buttons may
            // have been pressed just after this one.
            if settling {
                ending = None;
            } else if let Some(end) = ending.take() {
                return Err(end);
            } else if self.env.slot.cancel_requested() {
                ending = Some(End::cancelled(
                    "cancelled while waiting for the BOOT button — the board was not changed",
                ));
            } else if Instant::now() >= deadline {
                ending = Some(End::new(
                    outcome::NO_FIRMWARE_CHANGE,
                    "the board did not enter its bootloader in time — nothing was changed",
                ));
            }
            let nap = match deadline.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() && ending.is_none() => limits.sysfs_poll.min(left),
                _ => limits.sysfs_poll,
            };
            if !self.pause(nap).await {
                return Err(self.interrupted());
            }
        }
    }

    /// Stages 4–8, from the bootloader on the board's USB port: the same for
    /// every board.
    async fn onward_from_bootloader(&mut self, entered: Entered, serial: String) -> End {
        let limits = self.env.limits;
        let Entered { port, ifnum } = entered;
        // [SAFETY] From here the fans' settings are unknown to the daemon.
        self.env.cache.invalidate_openfan_writes();
        self.record(|r| r.bootloader_seen = true).await;
        self.drop_port().await;

        // ── 4. Writing the firmware, when the start asked the daemon to ───
        let mut written = false;
        if let Some(job) = self.env.write.take() {
            match self.write_firmware(&port, &serial, job).await {
                Written::Done => written = true,
                Written::FellBack => {}
                Written::Stopped => return self.interrupted(),
            }
        }

        // ── 5–6. Waiting for the file, then for the board ─────────────────
        // One file wait however often the board goes back to its bootloader:
        // a return resumes it rather than starting it again, and only so many
        // returns are waited through. A board the daemon wrote and restarted
        // needs no file — unless it comes back to its bootloader.
        let file_deadline = Instant::now() + limits.file_wait;
        let mut returns: u32 = 0;
        let mut wait_for_file = !written;
        let returned_tty = loop {
            if std::mem::replace(&mut wait_for_file, true) {
                self.enter_until(stage::WAITING_FOR_FILE, file_deadline)
                    .await;
                // The drive is named when it attaches — on real hardware about a
                // second after the bootloader — and again whenever a disk comes or
                // goes, so the window can say which drive to copy onto.
                let mut seen: Option<Drives> = None;
                loop {
                    let (p, s) = (port.clone(), serial.clone());
                    let known = seen.as_ref().map(|d| d.disks.clone());
                    let event = move |sys: &Path| {
                        if port_holds(sys, &p, &s) != PortHolds::Bootloader {
                            return Some(FileWait::Left);
                        }
                        let disks = usb::block_device_names(sys);
                        (known.as_ref() != Some(&disks))
                            .then(|| FileWait::Drives(Drives::read(sys, &p, disks)))
                    };
                    let file_left = file_deadline.saturating_duration_since(Instant::now());
                    match self.watch_sysfs(file_left, event).await {
                        Watched::Found(FileWait::Left) => break,
                        Watched::Found(FileWait::Drives(now)) => {
                            let changed = seen.as_ref().is_none_or(|was| {
                                (&was.ours, &was.others) != (&now.ours, &now.others)
                            });
                            if changed {
                                let (ours, others) = (now.ours.clone(), now.others.clone());
                                self.record(|r| {
                                    r.bootloader_drive = ours;
                                    r.other_bootloader_drives = others;
                                })
                                .await;
                            }
                            seen = Some(now);
                            // Disks that keep coming and going cannot hold the wait open.
                            if Instant::now() >= file_deadline {
                                return End::new(outcome::NEEDS_RECOVERY, NO_FILE_IN_TIME);
                            }
                        }
                        Watched::Deadline => {
                            return End::new(outcome::NEEDS_RECOVERY, NO_FILE_IN_TIME)
                        }
                        Watched::Shutdown => return self.interrupted(),
                    }
                }
            }

            self.enter(stage::WAITING_FOR_RETURN, limits.return_wait)
                .await;
            let (p, s) = (port.clone(), serial.clone());
            let back = move |sys: &Path| match port_holds(sys, &p, &s) {
                PortHolds::Board => usb::tty_for(sys, &p, ifnum).map(Back::Board),
                PortHolds::Bootloader => Some(Back::Bootloader),
                PortHolds::Other | PortHolds::Empty => None,
            };
            match self.watch_sysfs(limits.return_wait, back).await {
                Watched::Found(Back::Board(tty)) => break tty,
                Watched::Found(Back::Bootloader) => {
                    returns += 1;
                    if returns >= constants::OPENFAN_MAINT_MAX_BOOTLOADER_RETURNS {
                        return End::new(
                            outcome::NEEDS_RECOVERY,
                            format!(
                                "the board went back to its bootloader {returns} times after its \
                                 drive went away — it is still in its bootloader"
                            ),
                        );
                    }
                    self.note(
                        "the board went back to its bootloader — the copied file may not have \
                         started; waiting for a file again",
                    )
                    .await;
                }
                Watched::Deadline => {
                    return End::new(
                        outcome::FIRMWARE_COPIED_BOARD_NOT_BACK,
                        "the board did not come back after its drive went away",
                    )
                }
                Watched::Shutdown => return self.interrupted(),
            }
        };

        // ── 6. Checking ───────────────────────────────────────────────────
        self.enter(stage::CHECKING, limits.check_wait).await;
        let (p, sys) = (port.clone(), self.env.sys_root.clone());
        let after_usb = blocking(move || usb::device_at(&sys, &p)).await.flatten();
        self.record(|r| r.tty = Some(returned_tty.clone())).await;
        let deadline = Instant::now() + limits.check_wait;
        let timeout = self.env.serial_timeout;
        let transport = loop {
            let (open, path) = (self.env.open.clone(), returned_tty.clone());
            let attempt = blocking(move || {
                let mut t = open(&path, timeout)?;
                verify_openfan_identity(&mut *t, timeout)?;
                Ok::<_, SerialError>(t)
            })
            .await;
            let last_error = match attempt {
                Some(Ok(t)) => break t,
                Some(Err(e)) => e.to_string(),
                None => "the open task failed".to_string(),
            };
            if Instant::now() >= deadline {
                return End::new(
                    outcome::FIRMWARE_COPIED_BOARD_NOT_BACK,
                    format!(
                        "the board came back (its USB identity matches) but does not answer \
                         Control-OFC's commands ({last_error})"
                    ),
                );
            }
            if !self.pause(limits.check_retry).await {
                return self.interrupted();
            }
        };
        // Journaled before anything else: from here an interruption leaves a
        // board that answers, not one that needs recovering.
        self.record(|r| r.board_answered = true).await;
        if let Some(claim) = &self.claim {
            claim.mark_board_answered();
        }
        let read = blocking(move || {
            let mut t = transport;
            let hw = to_map(read_info_block(&mut *t, HW_INFO_OPCODE, timeout));
            let fw = to_map(read_info_block(&mut *t, FW_INFO_OPCODE, timeout));
            (t, hw, fw)
        })
        .await;
        let Some((transport, hw, fw)) = read else {
            return End::new(
                outcome::FIRMWARE_COPIED_BOARD_NOT_BACK,
                "reading the returned board's information failed",
            );
        };
        let after = BoardSnapshot {
            usb: after_usb,
            hw_info: hw,
            fw_info: fw,
        };
        let verdict = self
            .record(|r| {
                r.evidence = Some(evidence::compare(&r.before, &after, &r.firmware));
                r.after = Some(after);
            })
            .await
            .and_then(|r| r.evidence)
            .map(|e| e.verdict)
            .unwrap_or_else(|| evidence::INCONCLUSIVE.to_string());

        // ── 7. Restoring control ──────────────────────────────────────────
        self.enter(stage::RESTORING_CONTROL, limits.restore_wait)
            .await;
        // The board restarted: nothing it held before is known to it now.
        self.env.cache.invalidate_openfan_writes();
        let known: Vec<u8> = self.env.cache.read_with(|s| {
            s.openfan_fans
                .values()
                .filter(|f| f.rpm_polled)
                .map(|f| f.channel)
                .collect()
        });
        let polls_before = self.env.cache.openfan_polls_started();
        if let Err(end) = self.hand_over(transport, returned_tty.clone()).await {
            return end;
        }
        // Only now: the port is in the slot, so a write lands instead of failing.
        self.env.cache.resume_openfan_writes(&self.run_id);
        let expected = (self.env.expected_channels)();
        let deadline = Instant::now() + limits.restore_wait;
        loop {
            let landed = self.env.cache.read_with(|s| {
                expected.iter().all(|ch| {
                    s.openfan_fans
                        .get(ch)
                        .is_some_and(|f| f.last_commanded_pwm.is_some())
                }) && known.iter().all(|ch| {
                    s.openfan_fans
                        .get(ch)
                        .is_some_and(|f| f.poll_seq > polls_before)
                })
            });
            if landed {
                break;
            }
            if Instant::now() >= deadline {
                return End::new(
                    outcome::BOARD_BACK_CONTROL_NOT_RESTORED,
                    "the board answers, but the fan settings did not land in time — the engine \
                     keeps retrying",
                );
            }
            if !self.pause(limits.sysfs_poll).await {
                return self.interrupted();
            }
        }
        // The exact build is known only when the board restarted straight
        // from the bytes the daemon read back: a board that went back to its
        // bootloader may since have been given another file by hand.
        let verified = written && returns == 0;
        match (verified, verdict == evidence::PREVIOUS_FIRMWARE) {
            (true, false) => End::new(
                outcome::EXACT_BUILD_VERIFIED,
                "control is restored; Control-OFC wrote the file, read every byte back, and the \
                 board restarted from it",
            ),
            (true, true) => End::new(
                outcome::COMPLETED_BUILD_NOT_CONFIRMED,
                "control is restored; Control-OFC wrote the file and read every byte back, but \
                 the board's own reports match the firmware it ran before, so the exact build \
                 is not confirmed",
            ),
            (false, true) => End::new(
                outcome::BACK_ON_PREVIOUS_FIRMWARE,
                "control is restored, but the board reports the firmware it ran before — the \
                 update was not applied",
            ),
            (false, false) => End::new(
                outcome::COMPLETED_BUILD_NOT_CONFIRMED,
                "control is restored; the evidence is shown, but no check can prove which exact \
                 build is running",
            ),
        }
    }

    /// Hand a board that answers to the poll loop: back through the loan when
    /// one is out, or — for a silent board no poll loop was running for — by
    /// adopting it, which starts one (DEC-484). `Err` with how the run ends when
    /// neither took it.
    async fn hand_over(
        &mut self,
        transport: Box<dyn SerialTransport + Send>,
        path: String,
    ) -> Result<(), End> {
        let wait = self.env.limits.borrow_wait;
        let (taken, refused) = if let Some(handle) = self.handle.take() {
            (
                handle
                    .give_back(LoanReturn::Port { transport, path }, wait)
                    .await,
                "the poll loop did not take the port back",
            )
        } else if let Target::Silent(target) = &self.env.target {
            let adopt = target.adopt.clone();
            (
                blocking(move || adopt(transport, path))
                    .await
                    .unwrap_or(false),
                "the board answers, but it could not be adopted as the controller",
            )
        } else {
            return Err(End::new(
                outcome::BOARD_BACK_CONTROL_NOT_RESTORED,
                "the loan was lost before the hand-back",
            ));
        };
        if !taken {
            if self.shutting_down() {
                return Err(self.interrupted());
            }
            return Err(End::new(outcome::BOARD_BACK_CONTROL_NOT_RESTORED, refused));
        }
        self.handed_over = true;
        Ok(())
    }

    /// Change the daemon write's progress in memory, for the window. The
    /// journal is written at each phase instead: a run that stops is judged
    /// by how far it got, not by the byte.
    fn progress(&self, f: impl FnOnce(&mut FirmwareWrite)) {
        self.env.slot.update_record(|r| {
            if let Some(w) = r.firmware_write.as_mut() {
                f(w);
            }
        });
    }

    /// Change the daemon write in the record, and write the journal.
    async fn record_write(&mut self, f: impl FnOnce(&mut FirmwareWrite)) {
        self.record(|r| {
            if let Some(w) = r.firmware_write.as_mut() {
                f(w);
            }
        })
        .await;
    }

    /// Stage 4 (DEC-483): write `job` through the PICOBOOT interface of the
    /// bootloader on `port`. Nothing is erased until the bootloader's flash id
    /// is found to be `serial`; every sector is read back before the board is
    /// restarted. Anything that stops it before the restart gives the drive
    /// back and the run goes on to the copy by hand.
    async fn write_firmware(&mut self, port: &str, serial: &str, job: firmware::Staged) -> Written {
        let limits = self.env.limits;
        let sectors = job.image.sectors();
        let deadline =
            Instant::now() + limits.write_base + limits.write_per_sector * sectors.len() as u32;
        self.enter_until(stage::WRITING_FIRMWARE, deadline).await;
        let total = job.image.byte_count();
        self.record(|r| {
            r.firmware_write
                .get_or_insert_with(|| FirmwareWrite::new(job.release.name, total))
                .phase = write_phase::IDENTIFYING.to_string();
        })
        .await;

        // The bootloader on the board's own USB port, and nothing else.
        let (open, p) = (self.env.picoboot.clone(), port.to_string());
        let client = match blocking(move || open(&p)).await {
            Some(Ok(client)) => client,
            Some(Err(PicobootError::NoAccess(e))) => {
                let detail = format!(
                    "the daemon may not open USB devices ({e}) — the openfan-firmware-write \
                     drop-in is not installed"
                );
                return self.fall_back(None, fallback::NO_USB_ACCESS, detail).await;
            }
            Some(Err(e)) => {
                return self
                    .fall_back(None, fallback::USB_UNAVAILABLE, e.to_string())
                    .await
            }
            None => {
                let detail = "opening the bootloader failed".to_string();
                return self
                    .fall_back(None, fallback::USB_UNAVAILABLE, detail)
                    .await;
            }
        };

        // Whose flash is it? The firmware builds its USB serial from the
        // flash chip's unique id, so the board's serial must be in it.
        let identified = on_usb(client, |c| {
            c.reset()?;
            c.exclusive_access(picoboot::EXCLUSIVE)?;
            c.exit_xip()?;
            c.flash_id()
        })
        .await;
        let (client, id) = match identified {
            Some((client, Ok(id))) => (client, id),
            Some((client, Err(e))) => {
                let detail = format!("reading the flash id failed: {e}");
                return self
                    .fall_back(Some(client), fallback::TRANSFER_FAILED, detail)
                    .await;
            }
            None => {
                let detail = "reading the flash id failed".to_string();
                return self
                    .fall_back_held(port, fallback::TRANSFER_FAILED, detail)
                    .await;
            }
        };
        let shown = picoboot::serial_of(&id);
        self.record_write(|w| w.flash_id = Some(shown)).await;
        if !picoboot::id_matches_serial(&id, serial) {
            // No serial in the sentence: it is logged, and the log goes whole
            // into a support bundle. The record's own fields hold both.
            let detail = format!(
                "the bootloader on USB port {port} has another board's flash, not this \
                 controller's — nothing was written"
            );
            return self
                .fall_back(Some(client), fallback::FLASH_ID_MISMATCH, detail)
                .await;
        }

        let mut client = client;
        let mut done: u64 = 0;
        for (index, sector) in sectors.iter().enumerate() {
            if self.shutting_down() {
                return self.stop_writing(client).await;
            }
            if Instant::now() >= deadline {
                let detail = "the write did not finish in time".to_string();
                return self
                    .fall_back(Some(client), fallback::TRANSFER_FAILED, detail)
                    .await;
            }
            if index == 0 {
                // [SAFETY] Journaled before the first erase, and after the last
                // check that could end the write first: from here the old
                // firmware is no longer whole, and only a write that finishes —
                // the daemon's or a copy by hand — leaves the board a firmware
                // to run.
                self.record_write(|w| {
                    w.flash_changed = true;
                    w.phase = write_phase::WRITING.to_string();
                })
                .await;
            }
            let (base, this) = (sector.base, sector.clone());
            match on_usb(client, move |c| picoboot::write_sector(c, &this)).await {
                Some((c, Ok(()))) => client = c,
                Some((c, Err(e))) => {
                    let detail = format!("writing the sector at {base:#010x} failed: {e}");
                    return self
                        .fall_back(Some(c), fallback::TRANSFER_FAILED, detail)
                        .await;
                }
                None => {
                    let detail = format!("writing the sector at {base:#010x} failed");
                    return self
                        .fall_back_held(port, fallback::TRANSFER_FAILED, detail)
                        .await;
                }
            }
            done += sector.bytes();
            self.progress(|w| w.done_bytes = done);
        }

        self.record_write(|w| {
            w.phase = write_phase::VERIFYING.to_string();
            w.done_bytes = 0;
        })
        .await;
        done = 0;
        for sector in &sectors {
            if self.shutting_down() {
                return self.stop_writing(client).await;
            }
            if Instant::now() >= deadline {
                let detail = "reading the firmware back did not finish in time".to_string();
                return self
                    .fall_back(Some(client), fallback::TRANSFER_FAILED, detail)
                    .await;
            }
            let (base, this) = (sector.base, sector.clone());
            match on_usb(client, move |c| picoboot::verify_sector(c, &this)).await {
                Some((c, Ok(None))) => client = c,
                Some((c, Ok(Some(at)))) => {
                    let detail = format!("the flash at {at:#010x} does not hold the file's bytes");
                    return self
                        .fall_back(Some(c), fallback::READBACK_MISMATCH, detail)
                        .await;
                }
                Some((c, Err(e))) => {
                    let detail = format!("reading back the sector at {base:#010x} failed: {e}");
                    return self
                        .fall_back(Some(c), fallback::TRANSFER_FAILED, detail)
                        .await;
                }
                None => {
                    let detail = format!("reading back the sector at {base:#010x} failed");
                    return self
                        .fall_back_held(port, fallback::TRANSFER_FAILED, detail)
                        .await;
                }
            }
            done += sector.bytes();
            self.progress(|w| w.done_bytes = done);
        }

        self.record_write(|w| {
            w.verified = true;
            w.phase = write_phase::REBOOTING.to_string();
        })
        .await;
        if self.shutting_down() {
            return self.stop_writing(client).await;
        }
        match on_usb(client, |c| c.reboot()).await {
            // Closed on the blocking pool, where the client was.
            Some((c, Ok(()))) => {
                blocking(move || drop(c)).await;
            }
            Some((c, Err(e))) => {
                let detail = format!(
                    "the firmware was written and read back, but the restart failed ({e}) — \
                     press the board's RESET button, or copy the file onto its drive"
                );
                return self.fall_back(Some(c), fallback::NO_RESTART, detail).await;
            }
            None => {
                let detail = "the firmware was written and read back, but the restart failed — \
                              press the board's RESET button, or copy the file onto its drive"
                    .to_string();
                return self
                    .fall_back_held(port, fallback::NO_RESTART, detail)
                    .await;
            }
        }
        // The bootloader leaves the port half a second after the acknowledgement.
        let (p, s) = (port.to_string(), serial.to_string());
        let left =
            move |sys: &Path| (port_holds(sys, &p, &s) != PortHolds::Bootloader).then_some(());
        match self.watch_sysfs(limits.reboot_wait, left).await {
            Watched::Found(()) => {
                self.record_write(|w| w.phase = write_phase::WRITTEN.to_string())
                    .await;
                Written::Done
            }
            Watched::Deadline => {
                let detail = "the firmware was written and read back, but the board did not \
                              restart — press its RESET button, or copy the file onto its drive"
                    .to_string();
                self.fall_back_held(port, fallback::NO_RESTART, detail)
                    .await
            }
            Watched::Shutdown => Written::Stopped,
        }
    }

    /// The daemon write stops before the restart: the drive is given back,
    /// the reason recorded, and the run goes on to the copy by hand. `client`
    /// is the open bootloader, if any — `None` when it was never opened.
    async fn fall_back(
        &mut self,
        client: Option<picoboot::Client>,
        reason: &'static str,
        detail: String,
    ) -> Written {
        let released = match client {
            Some(c) => release_drive(c).await,
            None => true,
        };
        self.fell_back(released, reason, detail).await
    }

    /// As [`Self::fall_back`], after the client was lost while the bootloader
    /// may still hold the drive to itself: it is opened again to give it back.
    async fn fall_back_held(
        &mut self,
        port: &str,
        reason: &'static str,
        detail: String,
    ) -> Written {
        let (open, p) = (self.env.picoboot.clone(), port.to_string());
        let released = match blocking(move || open(&p)).await {
            Some(Ok(c)) => release_drive(c).await,
            _ => false,
        };
        self.fell_back(released, reason, detail).await
    }

    async fn fell_back(&mut self, released: bool, reason: &'static str, detail: String) -> Written {
        let shown = detail.clone();
        self.record_write(|w| {
            w.phase = write_phase::FELL_BACK.to_string();
            w.fallback_reason = Some(reason.to_string());
            w.fallback_detail = Some(shown);
        })
        .await;
        // A flash that is not the board's may be another board's drive.
        let copy = if reason == fallback::FLASH_ID_MISMATCH {
            "copy the file onto the drive only if you are sure it is the OpenFAN board's"
        } else {
            "copy the file onto the board's drive"
        };
        self.note(format!(
            "Control-OFC did not write the firmware itself ({detail}) — {copy}"
        ))
        .await;
        if !released {
            self.note(
                "the bootloader did not confirm its drive takes a copy again — if the copy fails, \
                 see the recovery steps",
            )
            .await;
        }
        Written::FellBack
    }

    /// The daemon is stopping mid-write: give the drive back so the user can
    /// copy the file while it is down.
    async fn stop_writing(&mut self, client: picoboot::Client) -> Written {
        release_drive(client).await;
        Written::Stopped
    }

    /// The board is in its firmware without having reached its bootloader:
    /// confirm it answers — on the port the run holds, or reopened on `tty` —
    /// and hand that back, or nothing. Either way no firmware changed.
    async fn in_firmware(&mut self, tty: &str, what: &str) -> End {
        let (held, open, path) = (self.port.take(), self.env.open.clone(), tty.to_string());
        let timeout = self.env.serial_timeout;
        let checked = blocking(move || {
            let mut t = match held {
                Some(t) => t,
                None => open(&path, timeout)?,
            };
            verify_openfan_identity(&mut *t, timeout)?;
            Ok::<_, SerialError>(t)
        })
        .await;
        let Some(handle) = self.handle.take() else {
            return End::new(outcome::NO_FIRMWARE_CHANGE, what);
        };
        let wait = self.env.limits.borrow_wait;
        match checked {
            Some(Ok(t)) => {
                handle
                    .give_back(
                        LoanReturn::Port {
                            transport: t,
                            path: tty.to_string(),
                        },
                        wait,
                    )
                    .await;
                End::new(
                    outcome::NO_FIRMWARE_CHANGE,
                    format!("{what}; it answers, and control resumes"),
                )
            }
            _ => {
                handle.give_back(LoanReturn::Nothing, wait).await;
                End::new(
                    outcome::NO_FIRMWARE_CHANGE,
                    format!("{what}, and it does not answer — the daemon is reconnecting to it"),
                )
            }
        }
    }

    /// Close the port the run holds, if any, without waiting for its output.
    /// (The PICOBOOT client is closed by [`release_drive`] or after the restart.)
    async fn drop_port(&mut self) {
        if let Some(mut old) = self.port.take() {
            blocking(move || {
                old.discard_pending_output();
                drop(old);
            })
            .await;
        }
    }

    /// Every exit: the journal, then the port or nothing, then the claim.
    async fn conclude(mut self, end: End) {
        let detail = end.detail.clone();
        let finished = self
            .record(|r| {
                finish_record(r, end.outcome, detail, end.interrupted);
                r.cancelled = end.cancelled;
            })
            .await;
        let at = finished.map(|r| r.stage).unwrap_or_default();
        if outcome::needs_recovery(end.outcome) {
            log::warn!(
                "OpenFan firmware update {} ended during {at}: {} — {}",
                self.run_id,
                end.outcome,
                end.detail
            );
        } else {
            log::info!(
                "OpenFan firmware update {} ended during {at}: {} — {}",
                self.run_id,
                end.outcome,
                end.detail
            );
        }
        if let Some(handle) = self.handle.take() {
            self.drop_port().await;
            handle
                .give_back(LoanReturn::Nothing, self.env.limits.borrow_wait)
                .await;
        }
        if let Some(claim) = self.claim.take() {
            if end.outcome == outcome::NO_FIRMWARE_CHANGE && !self.handed_over {
                // Nothing changed and nothing was handed over: the board is as
                // it was, so a recovery a silent board's claim replaced is put
                // back (DEC-484).
                claim.release_unchanged();
            } else {
                claim.release(outcome::needs_recovery(end.outcome).then_some(end.outcome));
            }
        }
        // Probes may open the board again.
        drop(self.gate.take());
    }
}

#[cfg(test)]
mod tests {
    //! A run end to end against a board on the bench: its USB identity moves
    //! in a sysfs fixture the way the kernel's does, the real poll loop lends
    //! and takes back its port, the board answers what the firmware answers,
    //! and in its bootloader a boot ROM answers PICOBOOT. Real time, short
    //! limits; every wait is bounded.
    use super::*;
    use crate::health::state::OpenFanLink;
    use crate::openfan_maintenance::journal::JOURNAL_FILE;
    use crate::openfan_maintenance::{CancelOutcome, FirmwareClaim};
    use crate::serial::picoboot::fake::Rom;
    use crate::serial::port_loan::loan_channel;
    use crate::serial::uf2::{self, Image};
    use crate::serial::usb_identity::fixture::{descriptors, Sysfs};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

    const USB_PORT: &str = "8-8";
    const SERIAL: &str = "DE615CB14721492C";
    const TTY: &str = "/dev/ttyACM91";
    /// The descriptor tags of the firmware the board runs, and of the file.
    const OLD: u8 = 0x80;
    const NEW: u8 = 0x81;
    const SERIAL_TIMEOUT: Duration = Duration::from_millis(50);
    const POLL: Duration = Duration::from_millis(20);
    /// How long the bench takes to re-enumerate, as the kernel does.
    const REENUMERATE: Duration = Duration::from_millis(40);
    const RUN: &str = "ofmaint-test";
    /// The board's flash id: the bytes its firmware shows as `SERIAL`.
    const FLASH_UID: [u8; 8] = [0xde, 0x61, 0x5c, 0xb1, 0x47, 0x21, 0x49, 0x2c];
    /// The release the daemon writes on the bench.
    static BENCH_RELEASE: firmware::Release = firmware::Release {
        sha256: "not read by the run",
        size: 0,
        name: "bench release",
        broken: false,
    };
    /// Forty pages: two whole sectors and half of a third.
    const RELEASE_PAGES: u32 = 40;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Firmware,
        Bootloader,
        Away,
    }

    /// What a trigger (`>07`, or the 1200-baud signal) does to the board.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum OnTrigger {
        Bootloader,
        /// Restarts into its firmware.
        Restart,
        /// Leaves USB and never comes back.
        Vanish,
        Nothing,
    }

    struct Board {
        sys: Sysfs,
        mode: parking_lot::Mutex<Mode>,
        /// Bumped by every re-enumeration; a port opened before is dead.
        generation: AtomicUsize,
        /// The firmware it runs: descriptor tag and `FW_REV`.
        firmware: parking_lot::Mutex<(u8, &'static str)>,
        on_jump: parking_lot::Mutex<OnTrigger>,
        on_1200: parking_lot::Mutex<OnTrigger>,
        answers: AtomicBool,
        /// A channel whose `>02` the firmware never acknowledges.
        deaf_channel: parking_lot::Mutex<Option<u8>>,
        frames: parking_lot::Mutex<Vec<String>>,
        touches: AtomicUsize,
        opened: parking_lot::Mutex<Vec<String>>,
        /// The bootloader enumerates without its drive, which `attach_drive`
        /// adds later — as on real hardware, where the kernel's storage scan
        /// follows the device by about a second.
        drive_late: AtomicBool,
        /// Its boot ROM, behind the PICOBOOT interface its bootloader shows.
        rom: Arc<parking_lot::Mutex<Rom>>,
        /// What the board does when its ROM restarts it after a write.
        on_reboot: parking_lot::Mutex<OnTrigger>,
        /// Every USB port the daemon asked to open PICOBOOT on.
        picoboot_opened: parking_lot::Mutex<Vec<String>>,
        /// The daemon may not open USB devices: no drop-in.
        usb_denied: AtomicBool,
        /// Every reconnect attempt the poll loop made.
        reconnects: AtomicUsize,
        /// Its serial device cannot be opened (another program holds it).
        refuse_open: AtomicBool,
    }

    impl Board {
        fn new() -> Arc<Self> {
            let board = Arc::new(Self {
                sys: Sysfs::new(),
                mode: parking_lot::Mutex::new(Mode::Away),
                generation: AtomicUsize::new(0),
                firmware: parking_lot::Mutex::new((OLD, "01")),
                on_jump: parking_lot::Mutex::new(OnTrigger::Bootloader),
                on_1200: parking_lot::Mutex::new(OnTrigger::Bootloader),
                answers: AtomicBool::new(true),
                deaf_channel: parking_lot::Mutex::new(None),
                frames: parking_lot::Mutex::new(Vec::new()),
                touches: AtomicUsize::new(0),
                opened: parking_lot::Mutex::new(Vec::new()),
                drive_late: AtomicBool::new(false),
                rom: Rom::new(FLASH_UID),
                on_reboot: parking_lot::Mutex::new(OnTrigger::Restart),
                picoboot_opened: parking_lot::Mutex::new(Vec::new()),
                usb_denied: AtomicBool::new(false),
                reconnects: AtomicUsize::new(0),
                refuse_open: AtomicBool::new(false),
            });
            // A restart from the ROM runs what was just written. Weak: the
            // board holds the ROM.
            let weak = Arc::downgrade(&board);
            board.rom.lock().on_reboot = Some(Box::new(move || {
                if let Some(b) = weak.upgrade() {
                    *b.firmware.lock() = (NEW, "02");
                    let what = *b.on_reboot.lock();
                    b.trigger(what);
                }
            }));
            board.boot();
            board
        }

        /// What the production PICOBOOT opener does: the bootloader on that
        /// port, if it is there and the daemon may open it.
        fn open_picoboot(&self, port: &str) -> Result<picoboot::Client, PicobootError> {
            self.picoboot_opened.lock().push(port.to_string());
            if self.usb_denied.load(SeqCst) {
                return Err(PicobootError::NoAccess(
                    "permission denied (os error 1)".into(),
                ));
            }
            if port == USB_PORT && self.mode() == Mode::Bootloader {
                Ok(Rom::client(&self.rom))
            } else {
                Err(PicobootError::NotFound(format!(
                    "no RP2040 bootloader on USB port {port}"
                )))
            }
        }

        /// The flash bytes the release covers, as the board holds them.
        fn flash_holds_the_release(&self) -> bool {
            let image = the_release().image;
            image
                .sectors()
                .iter()
                .flat_map(|s| &s.runs)
                .all(|run| self.rom.lock().flash_at(run.addr, run.data.len()) == run.data)
        }

        fn mode(&self) -> Mode {
            *self.mode.lock()
        }

        fn leave(&self) {
            self.generation.fetch_add(1, SeqCst);
            self.sys.remove_device(USB_PORT);
            *self.mode.lock() = Mode::Away;
        }

        /// Enumerate running its firmware.
        fn boot(&self) {
            self.leave();
            let tag = self.firmware.lock().0;
            self.sys
                .add_openfan(USB_PORT, SERIAL, tag, "ttyACM91", "ttyACM92");
            *self.mode.lock() = Mode::Firmware;
        }

        fn enter_bootloader(&self) {
            self.leave();
            self.rom.lock().power_on();
            if self.drive_late.load(SeqCst) {
                self.sys.add_bootloader_without_drive(USB_PORT);
            } else {
                self.sys.add_bootloader(USB_PORT, "sdx");
            }
            *self.mode.lock() = Mode::Bootloader;
        }

        fn attach_drive(&self) {
            self.sys.add_disk(USB_PORT, "sdx");
        }

        /// The user copies a file: the bootloader writes it and restarts.
        fn copy_file(&self, tag: u8, fw_rev: &'static str) {
            assert_eq!(self.mode(), Mode::Bootloader, "the drive must be there");
            *self.firmware.lock() = (tag, fw_rev);
            self.boot();
        }

        /// Leave USB now and come back as `what` once the kernel has caught up.
        fn trigger(self: &Arc<Self>, what: OnTrigger) {
            if what == OnTrigger::Nothing {
                return;
            }
            self.leave();
            let board = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(REENUMERATE);
                match what {
                    OnTrigger::Bootloader => board.enter_bootloader(),
                    OnTrigger::Restart => board.boot(),
                    OnTrigger::Vanish | OnTrigger::Nothing => {}
                }
            });
        }

        fn port(self: &Arc<Self>) -> Box<dyn SerialTransport + Send> {
            Box::new(BenchPort {
                board: self.clone(),
                generation: self.generation.load(SeqCst),
                replies: VecDeque::new(),
            })
        }

        /// What the production opener does: open the node, if it is there.
        fn open(
            self: &Arc<Self>,
            path: &str,
        ) -> Result<Box<dyn SerialTransport + Send>, SerialError> {
            self.opened.lock().push(path.to_string());
            if self.mode() == Mode::Firmware && path == TTY && !self.refuse_open.load(SeqCst) {
                Ok(self.port())
            } else {
                Err(SerialError::Protocol {
                    message: format!("{path}: no such device"),
                })
            }
        }

        /// The poll loop's reconnect: open and identify, as production does.
        fn reconnect(
            self: &Arc<Self>,
            timeout: Duration,
        ) -> Option<Box<dyn SerialTransport + Send>> {
            self.reconnects.fetch_add(1, SeqCst);
            if self.mode() != Mode::Firmware {
                return None;
            }
            let mut port = self.port();
            verify_openfan_identity(&mut *port, timeout).ok()?;
            Some(port)
        }

        fn sent(&self, frame: &str) -> usize {
            self.frames
                .lock()
                .iter()
                .filter(|f| f.as_str() == frame)
                .count()
        }
    }

    struct BenchPort {
        board: Arc<Board>,
        generation: usize,
        replies: VecDeque<String>,
    }

    impl BenchPort {
        fn live(&self) -> bool {
            self.board.generation.load(SeqCst) == self.generation
                && self.board.mode() == Mode::Firmware
        }
    }

    impl SerialTransport for BenchPort {
        fn write_line(&mut self, data: &str) -> Result<(), SerialError> {
            if !self.live() {
                return Err(SerialError::Protocol {
                    message: "the device has gone".into(),
                });
            }
            self.board.frames.lock().push(data.trim_end().to_string());
            if !self.board.answers.load(SeqCst) {
                return Ok(());
            }
            let body = data.trim_start_matches('>').trim_end();
            match &body[..2] {
                "07" => {
                    let what = *self.board.on_jump.lock();
                    self.board.trigger(what);
                }
                "05" => self
                    .replies
                    .extend(["<05|", "HW_REV:03", "MCU:PICO2040", ""].map(|l| format!("{l}\r\n"))),
                "06" => {
                    let rev = self.board.firmware.lock().1;
                    self.replies.extend([
                        format!("<06|FW_REV:{rev}\r\n"),
                        "PROTOCOL_VERSION:01\r\n".into(),
                        "\r\n".into(),
                    ]);
                }
                "02" if u8::from_str_radix(&body[2..4], 16).ok()
                    == *self.board.deaf_channel.lock() => {}
                _ => self
                    .replies
                    .push_back(crate::serial::protocol::firmware_echo_for(data)),
            }
            Ok(())
        }

        fn read_line(&mut self, _timeout: Duration) -> Result<String, SerialError> {
            if !self.live() {
                return Err(SerialError::Protocol {
                    message: "the device has gone".into(),
                });
            }
            self.replies
                .pop_front()
                .ok_or(SerialError::Timeout { timeout_ms: 1 })
        }

        fn touch_1200_baud(self: Box<Self>) -> Result<(), SerialError> {
            self.board.touches.fetch_add(1, SeqCst);
            if !self.live() {
                return Err(SerialError::Protocol {
                    message: "the device has gone".into(),
                });
            }
            let what = *self.board.on_1200.lock();
            self.board.trigger(what);
            Ok(())
        }
    }

    fn quick() -> StageLimits {
        StageLimits {
            borrow_wait: Duration::from_secs(2),
            prepare: Duration::from_secs(5),
            probe_wait: Duration::from_secs(2),
            boot_button_wait: Duration::from_secs(3),
            park: Duration::from_secs(5),
            trigger_wait: Duration::from_millis(300),
            bootloader_wait: Duration::from_secs(2),
            enter: Duration::from_secs(10),
            file_wait: Duration::from_secs(10),
            return_wait: Duration::from_secs(3),
            check_wait: Duration::from_secs(1),
            restore_wait: Duration::from_secs(3),
            sysfs_poll: Duration::from_millis(10),
            check_retry: Duration::from_millis(20),
            write_base: Duration::from_secs(5),
            write_per_sector: Duration::from_millis(500),
            reboot_wait: Duration::from_millis(500),
        }
    }

    /// The release the daemon writes on the bench.
    fn the_release() -> firmware::Staged {
        let data = uf2::fixture::image_file(RELEASE_PAGES, 7);
        firmware::Staged {
            sha256: firmware::sha256_hex(&data),
            release: &BENCH_RELEASE,
            image: Image::parse(&data).expect("a valid image"),
        }
    }

    /// The file the GUI described: the new build's descriptor and `FW_REV`.
    fn the_file() -> FirmwareClaim {
        let config: String = descriptors(NEW)[18..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        FirmwareClaim {
            sha256: "ab".repeat(32),
            size: 107_520,
            usb_config_descriptor_hex: Some(config),
            info: Some(BTreeMap::from([("FW_REV".to_string(), "02".to_string())])),
        }
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    struct Bench {
        board: Arc<Board>,
        cache: Arc<StateCache>,
        slot: Arc<MaintenanceSlot>,
        ctrl: Arc<Mutex<FanController>>,
        lender: LoanSender,
        stop: Arc<watch::Sender<bool>>,
        poll: tokio::task::JoinHandle<()>,
        journal: tempfile::TempDir,
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            let _ = self.stop.send(true);
            self.poll.abort();
        }
    }

    impl Bench {
        /// A board adopted on `TTY`, its poll loop running.
        async fn new(board: Arc<Board>) -> Self {
            let cache = Arc::new(StateCache::new());
            cache.set_openfan_port(TTY);
            let shared = Arc::new(Mutex::new(board.port()));
            let ctrl = Arc::new(Mutex::new(FanController::new_shared(
                shared.clone(),
                cache.clone(),
                SERIAL_TIMEOUT,
            )));
            let (lender, loans) = loan_channel();
            let (stop, stop_rx) = watch::channel(false);
            let b = board.clone();
            let poll = tokio::spawn(crate::polling::openfan_poll_loop_with(
                cache.clone(),
                shared,
                SERIAL_TIMEOUT,
                POLL,
                stop_rx,
                move |_: &Arc<StateCache>, t: Duration| b.reconnect(t),
                loans,
                |_: &str| {},
            ));
            let c = cache.clone();
            wait_until("the first polls", || {
                c.openfan_link() == Some(OpenFanLink::Connected)
                    && c.read_with(|s| s.openfan_fans.len()) == NUM_CHANNELS as usize
            })
            .await;
            Self {
                board,
                cache,
                slot: Arc::new(MaintenanceSlot::default()),
                ctrl,
                lender,
                stop: Arc::new(stop),
                poll,
                journal: tempfile::tempdir().unwrap(),
            }
        }

        fn journal_path(&self) -> PathBuf {
            self.journal.path().join(JOURNAL_FILE)
        }

        fn env(&self, limits: StageLimits, expected: ExpectedChannels) -> RunEnv {
            let board = self.board.clone();
            RunEnv {
                cache: self.cache.clone(),
                slot: self.slot.clone(),
                target: Target::Connected {
                    controller: self.ctrl.clone(),
                    lender: self.lender.clone(),
                },
                sys_root: self.board.sys.root().to_path_buf(),
                journal_path: self.journal_path(),
                limits,
                serial_timeout: SERIAL_TIMEOUT,
                open: Arc::new(move |path: &str, _: Duration| board.open(path)),
                expected_channels: expected,
                shutdown: self.stop.subscribe(),
                write: None,
                picoboot: {
                    let board = self.board.clone();
                    Arc::new(move |port: &str| board.open_picoboot(port))
                },
            }
        }

        /// Claim and record as the handler does — but not yet spawn.
        fn claim(&self) -> (ClaimGuard, AliveGuard) {
            self.cache
                .try_begin_openfan_maintenance(RUN, stage::PREPARING)
                .expect("the bench's controller is free and connected");
            let claim = ClaimGuard::new(self.cache.clone(), RUN.into());
            let alive = self.slot.claim().expect("no run yet");
            self.slot.set_record(MaintenanceRecord::new(
                RUN.into(),
                SERIAL.into(),
                the_file(),
            ));
            (claim, alive)
        }

        fn start_with(
            &self,
            limits: StageLimits,
            expected: ExpectedChannels,
        ) -> tokio::task::JoinHandle<()> {
            let (claim, alive) = self.claim();
            spawn(self.env(limits, expected), claim, alive)
        }

        fn start(&self) -> tokio::task::JoinHandle<()> {
            self.start_with(quick(), Arc::new(Vec::new))
        }

        /// A run that asks the daemon to write [`the_release`].
        fn start_writing(&self) -> tokio::task::JoinHandle<()> {
            self.start_writing_with(quick())
        }

        fn start_writing_with(&self, limits: StageLimits) -> tokio::task::JoinHandle<()> {
            let (claim, alive) = self.claim();
            let env = RunEnv {
                write: Some(the_release()),
                ..self.env(limits, Arc::new(Vec::new))
            };
            spawn(env, claim, alive)
        }

        fn record(&self) -> MaintenanceRecord {
            self.slot.record().expect("a run was started")
        }

        async fn reached(&self, stage: &str) {
            let slot = self.slot.clone();
            wait_until(&format!("stage {stage}"), || {
                slot.record().is_some_and(|r| r.stage == stage)
            })
            .await;
        }

        async fn finish(&self, run: tokio::task::JoinHandle<()>) -> MaintenanceRecord {
            tokio::time::timeout(Duration::from_secs(20), run)
                .await
                .expect("the run must end")
                .expect("the supervisor must not fail");
            let record = self.record();
            assert_eq!(record.state, STATE_FINISHED);
            assert_eq!(
                journal::load(&self.journal_path()).as_ref(),
                Some(&record),
                "the journal holds what the run ended with"
            );
            record
        }

        fn polls_since(&self, before: u64) -> bool {
            self.cache
                .read_with(|s| s.openfan_fans.values().all(|f| f.poll_seq > before))
        }
    }

    fn park_frame(ch: u8) -> String {
        format!(">02{ch:02X}{:02X}", crate::pwm::percent_to_raw(100))
    }

    #[tokio::test]
    async fn a_full_update_parks_hands_the_port_out_and_back_and_restores_control() {
        let bench = Bench::new(Board::new()).await;
        let run = bench.start();
        bench.reached(stage::WAITING_FOR_FILE).await;

        // Parked, then the bootloader asked for — once, and with `>07` alone.
        for ch in 0..NUM_CHANNELS {
            assert_eq!(
                bench.board.sent(&park_frame(ch)),
                1,
                "channel {ch} parked at 100 %"
            );
        }
        assert_eq!(bench.board.sent(">07"), 1);
        assert_eq!(
            bench.board.touches.load(SeqCst),
            0,
            "the 1200-baud signal is a fallback only"
        );
        let r = bench.record();
        assert!(r.bootloader_requested && r.bootloader_seen);
        assert_eq!(r.bootloader_trigger.as_deref(), Some(">07"));
        assert_eq!(r.bootloader_drive.as_deref(), Some("sdx"));
        assert_eq!(r.usb_port.as_deref(), Some(USB_PORT));
        assert_eq!(r.interface_number, Some(0));
        assert!(!r.cancellable);
        // [SAFETY] Writes are suspended, the port is out, the force stays off it.
        assert!(bench.cache.openfan_writes_suspended());
        let err = bench.ctrl.lock().set_pwm(0, 50).unwrap_err().to_string();
        assert!(err.contains("firmware update"), "{err}");
        assert_eq!(
            bench.slot.request_cancel(),
            CancelOutcome::TooLate,
            "no cancel once the board was asked to leave"
        );

        // The user copies the file.
        let polls_before = bench.cache.openfan_polls_started();
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;

        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert!(r.board_answered && !r.interrupted && !r.cancelled);
        assert!(r.firmware_write.is_none(), "the file was copied by hand");
        assert!(
            bench.board.picoboot_opened.lock().is_empty(),
            "a copy by hand opens no USB device"
        );
        assert!(!stages(&r).contains(&stage::WRITING_FIRMWARE));
        let ev = r.evidence.expect("evidence");
        assert_eq!(ev.verdict, evidence::CONSISTENT_WITH_FILE);
        assert_eq!(ev.descriptor_changed, Some(true));
        assert_eq!(ev.info_matches_file, Some(true));
        assert_eq!(
            r.after
                .and_then(|a| a.fw_info)
                .and_then(|i| i.get("FW_REV").cloned()),
            Some("02".to_string())
        );
        assert_eq!(
            *bench.board.opened.lock(),
            vec![TTY.to_string()],
            "one open, of the interface the daemon was using"
        );
        // Control is back: the claim is gone, writes land, polls run on the new port.
        assert!(bench.cache.openfan_maintenance().is_none());
        assert!(!bench.cache.openfan_writes_suspended());
        assert_eq!(bench.cache.openfan_link(), Some(OpenFanLink::Connected));
        assert!(bench.polls_since(polls_before));
        bench
            .ctrl
            .lock()
            .set_pwm(0, 50)
            .expect("a write lands again");
    }

    #[tokio::test]
    async fn a_drive_that_attaches_after_the_bootloader_is_named_when_it_appears() {
        // On real hardware the drive follows the bootloader by about a second,
        // so a look taken as the wait begins finds no drive.
        let board = Board::new();
        board.drive_late.store(true, SeqCst);
        let bench = Bench::new(board).await;
        let run = bench.start();
        bench.reached(stage::WAITING_FOR_FILE).await;
        assert_eq!(bench.record().bootloader_drive, None, "no drive yet");

        bench.board.attach_drive();
        let slot = bench.slot.clone();
        wait_until("the drive to be named", || {
            slot.record().and_then(|r| r.bootloader_drive).as_deref() == Some("sdx")
        })
        .await;

        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    #[tokio::test]
    async fn a_board_that_ignores_07_gets_the_1200_baud_signal_on_the_same_port() {
        let board = Board::new();
        *board.on_jump.lock() = OnTrigger::Nothing;
        let bench = Bench::new(board).await;
        let opened_before = bench.board.opened.lock().len();
        let run = bench.start();
        bench.reached(stage::WAITING_FOR_FILE).await;
        assert_eq!(bench.board.sent(">07"), 1);
        assert_eq!(bench.board.touches.load(SeqCst), 1);
        assert_eq!(
            bench.board.opened.lock().len(),
            opened_before,
            "the signal goes down the port it borrowed — nothing is opened for it"
        );
        assert_eq!(
            bench.record().bootloader_trigger.as_deref(),
            Some("1200_baud")
        );
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    #[tokio::test]
    async fn a_board_that_never_leaves_normal_mode_is_handed_back_unchanged() {
        let board = Board::new();
        *board.on_jump.lock() = OnTrigger::Nothing;
        *board.on_1200.lock() = OnTrigger::Nothing;
        let bench = Bench::new(board).await;
        let r = bench.finish(bench.start()).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert!(r.bootloader_requested && !r.bootloader_seen);
        let detail = r.outcome_detail.unwrap();
        assert!(
            detail.contains("did not leave normal mode") && detail.contains("it answers"),
            "{detail}"
        );
        assert_eq!(
            *bench.board.opened.lock(),
            vec![TTY.to_string()],
            "the 1200-baud signal closed the port, so it is reopened once"
        );
        assert!(bench.cache.openfan_maintenance().is_none());
        assert!(!bench.cache.openfan_writes_suspended());
        let polls = bench.cache.openfan_polls_started();
        let b = &bench;
        wait_until("polls on the returned port", || b.polls_since(polls)).await;
    }

    #[tokio::test]
    async fn a_board_that_restarts_into_its_firmware_is_reopened_and_handed_back() {
        let board = Board::new();
        *board.on_jump.lock() = OnTrigger::Restart;
        let bench = Bench::new(board).await;
        let r = bench.finish(bench.start()).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert!(!r.bootloader_seen);
        assert_eq!(bench.board.touches.load(SeqCst), 0, "it left: no fallback");
        assert!(r
            .outcome_detail
            .unwrap()
            .contains("restarted into its firmware"));
        assert_eq!(*bench.board.opened.lock(), vec![TTY.to_string()]);
        assert!(bench.cache.openfan_maintenance().is_none());
        assert_eq!(bench.cache.openfan_link(), Some(OpenFanLink::Connected));
    }

    #[tokio::test]
    async fn a_board_that_vanishes_needs_recovery_until_it_answers_again() {
        let board = Board::new();
        *board.on_jump.lock() = OnTrigger::Vanish;
        let bench = Bench::new(board).await;
        let r = bench.finish(bench.start()).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        // [SAFETY] Writes stay off and the claim is a recovery, not a release.
        assert!(bench.cache.openfan_writes_suspended());
        assert!(matches!(
            bench.cache.openfan_maintenance(),
            Some(crate::health::state::OpenFanMaintenance::NeedsRecovery { .. })
        ));
        assert!(
            bench.ctrl.lock().set_pwm(0, 50).is_err(),
            "the port was handed back as nothing"
        );

        // The user power-cycles it: the poll loop finds it, and that ends it.
        bench.board.boot();
        let c = bench.cache.clone();
        wait_until("the reconnect", || c.openfan_maintenance().is_none()).await;
        assert!(!bench.cache.openfan_writes_suspended());
        assert_eq!(bench.cache.openfan_link(), Some(OpenFanLink::Connected));
    }

    #[tokio::test]
    async fn no_file_copied_in_time_leaves_the_board_needing_recovery() {
        let bench = Bench::new(Board::new()).await;
        let limits = StageLimits {
            file_wait: Duration::from_millis(300),
            ..quick()
        };
        let r = bench
            .finish(bench.start_with(limits, Arc::new(Vec::new)))
            .await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        assert_eq!(r.stage, stage::WAITING_FOR_FILE);
        assert!(r.bootloader_seen && !r.board_answered);
        assert!(bench.cache.openfan_writes_suspended());
    }

    #[tokio::test]
    async fn a_board_back_on_usb_that_does_not_answer_is_not_back() {
        let bench = Bench::new(Board::new()).await;
        let run = bench.start();
        bench.reached(stage::WAITING_FOR_FILE).await;
        bench.board.answers.store(false, SeqCst);
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::FIRMWARE_COPIED_BOARD_NOT_BACK)
        );
        assert!(!r.board_answered);
        let opened = bench.board.opened.lock().clone();
        assert!(!opened.is_empty());
        assert!(
            opened.iter().all(|p| p == TTY),
            "only the interface the daemon used is ever opened: {opened:?}"
        );
        let limits = quick();
        let most = (limits.check_wait.as_millis() / limits.check_retry.as_millis()) as usize + 2;
        assert!(
            opened.len() <= most,
            "{} opens, at most {most}",
            opened.len()
        );
        assert!(bench.cache.openfan_writes_suspended());
    }

    #[tokio::test]
    async fn a_board_that_goes_back_to_its_bootloader_is_noted_and_waited_for() {
        let bench = Bench::new(Board::new()).await;
        let run = bench.start();
        bench.reached(stage::WAITING_FOR_FILE).await;
        // The drive goes, and the bootloader comes back.
        bench.board.leave();
        bench.reached(stage::WAITING_FOR_RETURN).await;
        bench.board.enter_bootloader();
        let slot = bench.slot.clone();
        wait_until("the note", || {
            slot.record()
                .is_some_and(|r| r.stage == stage::WAITING_FOR_FILE && !r.notes.is_empty())
        })
        .await;
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("went back to its bootloader")));
        assert_eq!(
            r.stages
                .iter()
                .filter(|s| s.stage == stage::WAITING_FOR_FILE)
                .count(),
            2
        );
    }

    /// Returns to the bootloader are capped. Uncapped, a board (or a hub)
    /// that kept re-enumerating held the controller, and grew the record and
    /// its journal, for as long as it kept doing it.
    #[tokio::test]
    async fn a_board_that_keeps_going_back_to_its_bootloader_is_left_needing_recovery() {
        let bench = Bench::new(Board::new()).await;
        let run = bench.start();
        let max = constants::OPENFAN_MAINT_MAX_BOOTLOADER_RETURNS as usize;
        let waits = |r: &MaintenanceRecord| {
            r.stages
                .iter()
                .filter(|s| s.stage == stage::WAITING_FOR_FILE)
                .count()
        };
        for pass in 1..=max {
            let slot = bench.slot.clone();
            wait_until(&format!("file wait {pass}"), || {
                slot.record()
                    .is_some_and(|r| r.stage == stage::WAITING_FOR_FILE && waits(&r) == pass)
            })
            .await;
            bench.board.leave();
            bench.reached(stage::WAITING_FOR_RETURN).await;
            bench.board.enter_bootloader();
        }
        let r = bench.finish(run).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        assert!(
            r.outcome_detail
                .as_deref()
                .is_some_and(|d| d.contains(&format!("{max} times"))),
            "{:?}",
            r.outcome_detail
        );
        assert_eq!(waits(&r), max, "no file wait after the last return");
        assert!(
            bench.cache.openfan_writes_suspended(),
            "left needing recovery"
        );
    }

    /// A return to the bootloader resumes the file wait rather than starting
    /// it again — or every return bought the board a fresh limit.
    #[tokio::test]
    async fn a_return_to_the_bootloader_does_not_restart_the_file_wait() {
        let bench = Bench::new(Board::new()).await;
        let limits = StageLimits {
            file_wait: Duration::from_millis(1500),
            ..quick()
        };
        let run = bench.start_with(limits, Arc::new(Vec::new));
        bench.reached(stage::WAITING_FOR_FILE).await;
        let first = bench.record().stage_deadline_unix_ms.expect("a limit");
        // Much of the wait goes by before the board goes back to its bootloader.
        tokio::time::sleep(Duration::from_millis(600)).await;
        bench.board.leave();
        bench.reached(stage::WAITING_FOR_RETURN).await;
        bench.board.enter_bootloader();
        let slot = bench.slot.clone();
        wait_until("the second file wait", || {
            slot.record()
                .is_some_and(|r| r.stage == stage::WAITING_FOR_FILE && !r.notes.is_empty())
        })
        .await;
        let second = bench.record().stage_deadline_unix_ms.expect("a limit");
        assert!(
            second <= first + 50,
            "the file wait started again: its limit moved {} ms",
            second.saturating_sub(first)
        );
        // And it runs out at that first limit, no file having been copied.
        let r = bench.finish(run).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        let ended = r.finished_unix_ms.expect("finished");
        assert!(
            ended < first + 500,
            "the wait ran {} ms past its limit",
            ended.saturating_sub(first)
        );
    }

    #[tokio::test]
    async fn the_same_firmware_back_again_is_reported_as_the_previous_one() {
        let bench = Bench::new(Board::new()).await;
        let run = bench.start();
        bench.reached(stage::WAITING_FOR_FILE).await;
        bench.board.copy_file(OLD, "01");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::BACK_ON_PREVIOUS_FIRMWARE)
        );
        assert_eq!(r.evidence.unwrap().verdict, evidence::PREVIOUS_FIRMWARE);
    }

    #[tokio::test]
    async fn a_cancel_before_the_port_is_borrowed_changes_nothing() {
        let bench = Bench::new(Board::new()).await;
        let (claim, alive) = bench.claim();
        assert_eq!(bench.slot.request_cancel(), CancelOutcome::Requested);
        let r = bench
            .finish(spawn(bench.env(quick(), Arc::new(Vec::new)), claim, alive))
            .await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert!(r.cancelled && !r.bootloader_requested);
        assert_eq!(
            bench.board.sent(&park_frame(0)),
            0,
            "cancelled before parking"
        );
        assert_eq!(bench.board.sent(">07"), 0);
        assert!(bench.cache.openfan_maintenance().is_none());
        assert!(!bench.cache.openfan_writes_suspended());
    }

    #[tokio::test]
    async fn a_failed_park_changes_nothing_and_never_asks_for_the_bootloader() {
        let board = Board::new();
        *board.deaf_channel.lock() = Some(5);
        let bench = Bench::new(board).await;
        let r = bench.finish(bench.start()).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert!(r.outcome_detail.unwrap().contains("channel 5"));
        assert_eq!(bench.board.sent(">07"), 0);
        assert!(!r.bootloader_requested);
        assert!(bench.cache.openfan_maintenance().is_none());
        assert!(
            !bench.cache.openfan_writes_suspended(),
            "the engine takes the channels back"
        );
    }

    #[tokio::test]
    async fn a_stop_while_waiting_for_the_file_is_recorded_as_needing_recovery() {
        let bench = Bench::new(Board::new()).await;
        let run = bench.start();
        bench.reached(stage::WAITING_FOR_FILE).await;
        bench.stop.send(true).unwrap();
        let r = bench.finish(run).await;
        assert!(r.interrupted);
        assert_eq!(r.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        assert_eq!(r.stage, stage::WAITING_FOR_FILE);
        assert!(bench.cache.openfan_writes_suspended());
    }

    #[tokio::test]
    async fn settings_that_do_not_land_leave_control_not_restored_but_writes_on() {
        let bench = Bench::new(Board::new()).await;
        let limits = StageLimits {
            restore_wait: Duration::from_millis(300),
            ..quick()
        };
        // Channel 3 is the profile's, and nothing writes it on the bench.
        let run = bench.start_with(limits, Arc::new(|| vec![3]));
        bench.reached(stage::WAITING_FOR_FILE).await;
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::BOARD_BACK_CONTROL_NOT_RESTORED)
        );
        assert!(
            !bench.cache.openfan_writes_suspended() && bench.cache.openfan_maintenance().is_none(),
            "the board answers: the engine keeps trying"
        );
    }

    #[tokio::test]
    async fn the_restore_waits_for_the_profile_settings_to_land() {
        let bench = Bench::new(Board::new()).await;
        let run = bench.start_with(quick(), Arc::new(|| vec![3]));
        bench.reached(stage::WAITING_FOR_FILE).await;
        bench.board.copy_file(NEW, "02");
        bench.reached(stage::RESTORING_CONTROL).await;
        // The engine's next tick, once writes are back on.
        let c = bench.cache.clone();
        wait_until("writes back on", || !c.openfan_writes_suspended()).await;
        assert!(!run.is_finished(), "it waits for channel 3");
        bench.ctrl.lock().set_pwm(3, 40).unwrap();
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    #[tokio::test]
    async fn a_run_that_panics_is_recorded_and_its_claim_released() {
        let bench = Bench::new(Board::new()).await;
        let run = bench.start_with(quick(), Arc::new(|| panic!("injected")));
        bench.reached(stage::WAITING_FOR_FILE).await;
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::BOARD_BACK_CONTROL_NOT_RESTORED),
            "it had answered: an internal error there is not a board to recover"
        );
        assert!(r
            .outcome_detail
            .unwrap()
            .contains("internal error during restoring_control"));
        assert!(bench.cache.openfan_maintenance().is_none());
        assert!(!bench.cache.openfan_writes_suspended());
        assert!(!bench.slot.is_alive(), "the next run may start");
    }

    // ── The daemon writes the firmware itself (DEC-483) ──────────────

    /// The stages a run went through, in order.
    fn stages(r: &MaintenanceRecord) -> Vec<&str> {
        r.stages.iter().map(|t| t.stage.as_str()).collect()
    }

    /// Every command that changed the board's flash: erases, and writes to
    /// flash (the flash-id helper goes to XIP SRAM, at `0x15000000`).
    fn flash_changes(rom: &Rom) -> Vec<String> {
        rom.log
            .iter()
            .filter(|l| l.starts_with("FLASH_ERASE") || l.starts_with("WRITE 0x10"))
            .cloned()
            .collect()
    }

    /// The write record once the write has ended, either way.
    async fn left_the_write(bench: &Bench) -> FirmwareWrite {
        let slot = bench.slot.clone();
        wait_until("the write to end", || {
            slot.record()
                .and_then(|r| r.firmware_write)
                .is_some_and(|w| {
                    [write_phase::FELL_BACK, write_phase::WRITTEN].contains(&w.phase.as_str())
                })
        })
        .await;
        bench
            .record()
            .firmware_write
            .expect("the write is recorded")
    }

    /// The write record once the run has fallen back to the copy by hand.
    async fn fell_back_to_the_copy(bench: &Bench) -> FirmwareWrite {
        bench.reached(stage::WAITING_FOR_FILE).await;
        let w = bench
            .record()
            .firmware_write
            .expect("the write is recorded");
        assert_eq!(w.phase, write_phase::FELL_BACK);
        w
    }

    #[tokio::test]
    async fn the_daemon_writes_a_known_release_reads_it_back_and_the_board_runs_it() {
        let bench = Bench::new(Board::new()).await;
        // Another board in its bootloader, on another port.
        bench.board.sys.add_bootloader("9-1", "sdy");
        let r = bench.finish(bench.start_writing()).await;

        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::EXACT_BUILD_VERIFIED),
            "{:?}",
            r.outcome_detail
        );
        let w = r.firmware_write.clone().expect("the write is recorded");
        assert_eq!(w.phase, write_phase::WRITTEN);
        assert!(w.verified && w.flash_changed);
        assert_eq!(w.flash_id.as_deref(), Some(SERIAL), "the board's own flash");
        assert_eq!(w.release, BENCH_RELEASE.name);
        assert_eq!(w.total_bytes, the_release().image.byte_count());
        assert_eq!(w.done_bytes, w.total_bytes, "every byte read back");
        assert_eq!(w.fallback_reason, None);
        assert_eq!(
            stages(&r),
            [
                stage::PREPARING,
                stage::PARKING,
                stage::ENTERING_BOOTLOADER,
                stage::WRITING_FIRMWARE,
                stage::WAITING_FOR_RETURN,
                stage::CHECKING,
                stage::RESTORING_CONTROL,
            ],
            "no wait for a file"
        );
        assert!(bench.board.flash_holds_the_release());
        {
            let rom = bench.board.rom.lock();
            assert_eq!(
                rom.logged("FLASH_ERASE"),
                [
                    "FLASH_ERASE 0x10000000+0x1000",
                    "FLASH_ERASE 0x10001000+0x1000",
                    "FLASH_ERASE 0x10002000+0x1000",
                ],
                "the three sectors the release covers, once each"
            );
            let at = |entry: &str| rom.log.iter().position(|l| l.starts_with(entry));
            assert!(
                at("EXEC").expect("the flash id was read") < at("FLASH_ERASE").expect("erased"),
                "the board is identified before anything is erased: {:?}",
                rom.log
            );
            assert_eq!(rom.log.last().map(String::as_str), Some("REBOOT"));
        }
        assert_eq!(
            *bench.board.picoboot_opened.lock(),
            [USB_PORT],
            "the bootloader on the board's own port, once"
        );
        assert_eq!(
            r.after
                .and_then(|a| a.fw_info)
                .and_then(|i| i.get("FW_REV").cloned())
                .as_deref(),
            Some("02")
        );
        assert!(bench.cache.openfan_maintenance().is_none());
        assert!(!bench.cache.openfan_writes_suspended());
    }

    #[tokio::test]
    async fn a_bootloader_whose_flash_is_another_boards_is_never_erased() {
        let board = Board::new();
        board.rom.lock().uid = [0x11; 8];
        let bench = Bench::new(board).await;
        let run = bench.start_writing();
        // However the write ends, the other board's flash is untouched.
        let w = left_the_write(&bench).await;
        {
            let rom = bench.board.rom.lock();
            assert_eq!(
                rom.logged("EXEC"),
                ["EXEC 0x15000000 flash-id"],
                "the id was read"
            );
            assert!(
                flash_changes(&rom).is_empty(),
                "nothing erased or programmed: {:?}",
                rom.log
            );
        }
        assert_eq!(w.phase, write_phase::FELL_BACK);
        assert_eq!(
            w.fallback_reason.as_deref(),
            Some(fallback::FLASH_ID_MISMATCH)
        );
        assert_eq!(w.flash_id.as_deref(), Some("1111111111111111"));
        assert!(!w.flash_changed);
        // The serials stay in the record's own fields: the note is logged too,
        // and the log is shared whole in a support bundle.
        let detail = w.fallback_detail.expect("the mismatch is explained");
        assert!(detail.contains("another board"), "{detail}");
        for id in ["1111111111111111", SERIAL] {
            assert!(!detail.to_uppercase().contains(id), "{detail}");
        }
        bench.reached(stage::WAITING_FOR_FILE).await;
        assert_eq!(
            bench.board.rom.lock().exclusive,
            picoboot::NOT_EXCLUSIVE,
            "the drive takes a copy again"
        );
        // The copy by hand finishes it, and nothing claims the exact build.
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert!(r.notes.iter().any(|n| n.contains("copy the file")));
        assert_eq!(r.expected_usb_serial, SERIAL);
        for id in ["1111111111111111", SERIAL] {
            assert!(
                r.notes.iter().all(|n| !n.to_uppercase().contains(id)),
                "{:?}",
                r.notes
            );
        }
    }

    #[tokio::test]
    async fn a_write_out_of_time_before_its_first_erase_leaves_the_flash_whole() {
        let bench = Bench::new(Board::new()).await;
        // No time at all: the write ends at its first sector's deadline check.
        let run = bench.start_writing_with(StageLimits {
            write_base: Duration::ZERO,
            write_per_sector: Duration::ZERO,
            ..quick()
        });
        let w = fell_back_to_the_copy(&bench).await;
        assert_eq!(
            w.fallback_reason.as_deref(),
            Some(fallback::TRANSFER_FAILED)
        );
        {
            let rom = bench.board.rom.lock();
            assert_eq!(
                rom.logged("EXEC"),
                ["EXEC 0x15000000 flash-id"],
                "the board was identified"
            );
            assert!(
                flash_changes(&rom).is_empty(),
                "nothing erased: {:?}",
                rom.log
            );
            assert_eq!(rom.exclusive, picoboot::NOT_EXCLUSIVE);
        }
        assert!(
            !w.flash_changed,
            "nothing was erased, so the old firmware is whole"
        );
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    #[tokio::test]
    async fn without_usb_access_the_file_is_copied_by_hand() {
        let board = Board::new();
        board.usb_denied.store(true, SeqCst);
        let bench = Bench::new(board).await;
        let run = bench.start_writing();
        let w = fell_back_to_the_copy(&bench).await;
        assert_eq!(w.fallback_reason.as_deref(), Some(fallback::NO_USB_ACCESS));
        assert!(w
            .fallback_detail
            .unwrap()
            .contains("openfan-firmware-write"));
        assert!(!w.flash_changed);
        assert_eq!(*bench.board.picoboot_opened.lock(), [USB_PORT]);
        assert!(
            bench.board.rom.lock().log.is_empty(),
            "nothing reached the bootloader"
        );
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    #[tokio::test]
    async fn a_transfer_that_fails_mid_write_gives_the_drive_back() {
        let board = Board::new();
        board.rom.lock().fail = Some((picoboot::CMD_FLASH_ERASE, 2, picoboot::LinkError::Timeout));
        let bench = Bench::new(board).await;
        let run = bench.start_writing();
        let w = fell_back_to_the_copy(&bench).await;
        assert_eq!(
            w.fallback_reason.as_deref(),
            Some(fallback::TRANSFER_FAILED)
        );
        assert!(w.flash_changed, "the first sector had been written");
        assert!(!w.verified);
        let detail = w.fallback_detail.unwrap();
        assert!(detail.contains("0x10001000"), "{detail}");
        {
            let rom = bench.board.rom.lock();
            assert_eq!(rom.logged("FLASH_ERASE").len(), 1);
            assert_eq!(rom.exclusive, picoboot::NOT_EXCLUSIVE);
            assert!(rom.logged("REBOOT").is_empty(), "never restarted");
        }
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    #[tokio::test]
    async fn a_read_back_that_differs_is_never_restarted_from() {
        let board = Board::new();
        board.rom.lock().corrupt_writes = true;
        let bench = Bench::new(board).await;
        let run = bench.start_writing();
        // However the write ends, a board whose flash reads back wrong is
        // never restarted.
        let w = left_the_write(&bench).await;
        assert!(
            bench.board.rom.lock().logged("REBOOT").is_empty(),
            "never restarted from a bad read-back"
        );
        assert_eq!(w.phase, write_phase::FELL_BACK);
        assert_eq!(
            w.fallback_reason.as_deref(),
            Some(fallback::READBACK_MISMATCH)
        );
        assert!(w.flash_changed && !w.verified);
        assert!(w.fallback_detail.unwrap().contains("0x10000000"));
        assert_eq!(bench.board.rom.lock().exclusive, picoboot::NOT_EXCLUSIVE);
        assert!(!bench.board.flash_holds_the_release());
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    #[tokio::test]
    async fn a_stop_mid_write_gives_the_drive_back_and_leaves_the_board_needing_recovery() {
        let bench = Bench::new(Board::new()).await;
        let stop = bench.stop.clone();
        bench.board.rom.lock().on_command = Some(Box::new(move |id, nth| {
            if id == picoboot::CMD_FLASH_ERASE && nth == 2 {
                let _ = stop.send(true);
            }
        }));
        let r = bench.finish(bench.start_writing()).await;
        assert!(r.interrupted);
        assert_eq!(r.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        assert_eq!(r.stage, stage::WRITING_FIRMWARE);
        let w = r.firmware_write.unwrap();
        assert!(w.flash_changed && !w.verified);
        let rom = bench.board.rom.lock();
        assert_eq!(
            rom.logged("FLASH_ERASE").len(),
            2,
            "the sector in hand is finished, and no other begun"
        );
        assert_eq!(
            rom.exclusive,
            picoboot::NOT_EXCLUSIVE,
            "the drive takes a copy while the daemon is down"
        );
        assert!(rom.logged("REBOOT").is_empty());
    }

    #[tokio::test]
    async fn a_stop_while_identifying_the_board_leaves_the_flash_whole() {
        let bench = Bench::new(Board::new()).await;
        let stop = bench.stop.clone();
        bench.board.rom.lock().on_command = Some(Box::new(move |id, _| {
            if id == picoboot::CMD_EXEC {
                let _ = stop.send(true);
            }
        }));
        let r = bench.finish(bench.start_writing()).await;
        assert!(r.interrupted);
        assert_eq!(r.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        let w = r.firmware_write.unwrap();
        assert_eq!(
            w.flash_id.as_deref(),
            Some(SERIAL),
            "the board was identified"
        );
        assert!(!w.flash_changed, "nothing was erased: {:?}", w);
        let rom = bench.board.rom.lock();
        assert!(flash_changes(&rom).is_empty(), "{:?}", rom.log);
        assert_eq!(rom.exclusive, picoboot::NOT_EXCLUSIVE);
        assert!(rom.logged("REBOOT").is_empty());
    }

    #[tokio::test]
    async fn a_board_that_does_not_restart_after_the_write_is_copied_to_by_hand() {
        let board = Board::new();
        board.rom.lock().restarts = false;
        let bench = Bench::new(board).await;
        let run = bench.start_writing();
        let w = fell_back_to_the_copy(&bench).await;
        assert_eq!(w.fallback_reason.as_deref(), Some(fallback::NO_RESTART));
        assert!(w.verified, "written and read back before the restart");
        assert_eq!(
            *bench.board.picoboot_opened.lock(),
            [USB_PORT, USB_PORT],
            "opened again to give the drive back"
        );
        assert_eq!(bench.board.rom.lock().exclusive, picoboot::NOT_EXCLUSIVE);
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED),
            "the copy may have been another file"
        );
    }

    #[tokio::test]
    async fn a_board_back_in_its_bootloader_after_the_write_waits_for_a_copy() {
        let board = Board::new();
        *board.on_reboot.lock() = OnTrigger::Bootloader;
        let bench = Bench::new(board).await;
        let run = bench.start_writing();
        bench.reached(stage::WAITING_FOR_FILE).await;
        let r = bench.record();
        assert_eq!(
            r.firmware_write.as_ref().map(|w| w.phase.as_str()),
            Some(write_phase::WRITTEN)
        );
        assert!(stages(&r).contains(&stage::WAITING_FOR_RETURN));
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("went back to its bootloader")));
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED),
            "a board that came back to its bootloader may have been given another file"
        );
    }

    // ── A silent board (DEC-484) ─────────────────────────────────────

    /// A silent board's update on the bench: the board does not answer, and
    /// either was never adopted or its poll loop has given up on it.
    struct Silent {
        board: Arc<Board>,
        cache: Arc<StateCache>,
        slot: Arc<MaintenanceSlot>,
        lender: Option<LoanSender>,
        /// The adoption probes' single-flight flag.
        gate: Arc<AtomicBool>,
        /// Every device path a board was adopted on.
        adopted: Arc<parking_lot::Mutex<Vec<String>>>,
        /// The poll loops those adoptions started.
        loops: Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
        stop: Arc<watch::Sender<bool>>,
        journal: tempfile::TempDir,
        /// The bench whose poll loop runs for the adopted controller.
        _bench: Option<Bench>,
    }

    impl Drop for Silent {
        fn drop(&mut self) {
            let _ = self.stop.send(true);
            for h in self.loops.lock().drain(..) {
                h.abort();
            }
        }
    }

    impl Silent {
        /// A board that never answered: no controller, no poll loop.
        fn never_adopted(board: Arc<Board>) -> Self {
            board.answers.store(false, SeqCst);
            let (stop, _) = watch::channel(false);
            Self {
                board,
                cache: Arc::new(StateCache::new()),
                slot: Arc::new(MaintenanceSlot::default()),
                lender: None,
                gate: Arc::default(),
                adopted: Arc::default(),
                loops: Arc::default(),
                stop: Arc::new(stop),
                journal: tempfile::tempdir().unwrap(),
                _bench: None,
            }
        }

        /// A board adopted, then gone silent: its poll loop has given up on
        /// the port and is searching for the controller.
        async fn gone_silent(board: Arc<Board>) -> Self {
            let bench = Bench::new(board).await;
            bench.board.answers.store(false, SeqCst);
            Self::over(bench).await
        }

        /// `bench`'s board, once its poll loop is searching for it.
        async fn over(bench: Bench) -> Self {
            let c = bench.cache.clone();
            wait_until("the loop to give up on the port", || {
                c.openfan_link() == Some(OpenFanLink::Reconnecting)
            })
            .await;
            Self {
                board: bench.board.clone(),
                cache: bench.cache.clone(),
                slot: bench.slot.clone(),
                lender: Some(bench.lender.clone()),
                gate: Arc::default(),
                adopted: Arc::default(),
                loops: Arc::default(),
                stop: bench.stop.clone(),
                journal: tempfile::tempdir().unwrap(),
                _bench: Some(bench),
            }
        }

        fn journal_path(&self) -> PathBuf {
            self.journal.path().join(JOURNAL_FILE)
        }

        fn env(&self, limits: StageLimits) -> RunEnv {
            let (cache, stop) = (self.cache.clone(), self.stop.subscribe());
            let (adopted, loops, b) =
                (self.adopted.clone(), self.loops.clone(), self.board.clone());
            // What production's adoption does: install the board and start
            // its poll loop.
            let adopt: AdoptFn = Arc::new(move |transport, path: String| {
                adopted.lock().push(path);
                let shared = Arc::new(Mutex::new(transport));
                let (_lender, loans) = loan_channel();
                let b = b.clone();
                loops
                    .lock()
                    .push(tokio::spawn(crate::polling::openfan_poll_loop_with(
                        cache.clone(),
                        shared,
                        SERIAL_TIMEOUT,
                        POLL,
                        stop.clone(),
                        move |_: &Arc<StateCache>, t: Duration| b.reconnect(t),
                        loans,
                        |_: &str| {},
                    )));
                true
            });
            let board = self.board.clone();
            RunEnv {
                cache: self.cache.clone(),
                slot: self.slot.clone(),
                target: Target::Silent(SilentTarget {
                    usb_port: USB_PORT.into(),
                    interface_number: 0,
                    lender: self.lender.clone(),
                    probe_gate: self.gate.clone(),
                    adopt,
                }),
                sys_root: self.board.sys.root().to_path_buf(),
                journal_path: self.journal_path(),
                limits,
                serial_timeout: SERIAL_TIMEOUT,
                open: Arc::new(move |path: &str, _: Duration| board.open(path)),
                expected_channels: Arc::new(Vec::new),
                shutdown: self.stop.subscribe(),
                write: None,
                picoboot: {
                    let board = self.board.clone();
                    Arc::new(move |port: &str| board.open_picoboot(port))
                },
            }
        }

        /// Claim and record as the handler does.
        fn start_with(&self, limits: StageLimits) -> tokio::task::JoinHandle<()> {
            let prior = self
                .cache
                .try_begin_silent_openfan_maintenance(RUN, stage::PREPARING)
                .expect("nothing answers on the bench");
            let claim = ClaimGuard::with_prior(self.cache.clone(), RUN.into(), prior);
            let alive = self.slot.claim().expect("no run yet");
            let mut record = MaintenanceRecord::new(RUN.into(), SERIAL.into(), the_file());
            record.board = crate::openfan_maintenance::board::SILENT.into();
            self.slot.set_record(record);
            spawn(self.env(limits), claim, alive)
        }

        fn start(&self) -> tokio::task::JoinHandle<()> {
            self.start_with(quick())
        }

        fn record(&self) -> MaintenanceRecord {
            self.slot.record().expect("a run was started")
        }

        async fn reached(&self, stage: &str) {
            let slot = self.slot.clone();
            wait_until(&format!("stage {stage}"), || {
                slot.record().is_some_and(|r| r.stage == stage)
            })
            .await;
        }

        async fn finish(&self, run: tokio::task::JoinHandle<()>) -> MaintenanceRecord {
            tokio::time::timeout(Duration::from_secs(20), run)
                .await
                .expect("the run must end")
                .expect("the supervisor must not fail");
            let record = self.record();
            assert_eq!(record.state, STATE_FINISHED);
            assert_eq!(
                journal::load(&self.journal_path()).as_ref(),
                Some(&record),
                "the journal holds what the run ended with"
            );
            record
        }

        /// The update's new firmware answers Control-OFC; the user copies it.
        fn copy_answering_file(&self) {
            self.board.answers.store(true, SeqCst);
            self.board.copy_file(NEW, "02");
        }

        fn recovery_of(&self) -> Option<String> {
            match self.cache.openfan_maintenance() {
                Some(crate::health::state::OpenFanMaintenance::NeedsRecovery {
                    run_id, ..
                }) => Some(run_id),
                _ => None,
            }
        }
    }

    #[tokio::test]
    async fn a_silent_board_never_adopted_gets_the_1200_baud_signal_alone_and_is_adopted_after() {
        let silent = Silent::never_adopted(Board::new());
        let run = silent.start();
        silent.reached(stage::WAITING_FOR_FILE).await;

        // [SAFETY] Nothing parked and no `>07`: the board takes no commands,
        // and firmware that does not speak the protocol may read it otherwise.
        assert_eq!(silent.board.sent(">07"), 0);
        for ch in 0..NUM_CHANNELS {
            assert_eq!(silent.board.sent(&park_frame(ch)), 0, "channel {ch}");
        }
        assert_eq!(silent.board.touches.load(SeqCst), 1);
        let r = silent.record();
        assert_eq!(r.board, crate::openfan_maintenance::board::SILENT);
        assert_eq!(r.bootloader_trigger.as_deref(), Some(trigger::BAUD_1200));
        assert!(r.bootloader_requested && r.bootloader_seen && !r.cancellable);
        assert!(!stages(&r).contains(&stage::PARKING));
        assert!(
            silent.gate.load(SeqCst),
            "no adoption probe opens the board while the update holds it"
        );
        assert!(silent.cache.openfan_writes_suspended());
        assert!(silent.adopted.lock().is_empty());

        silent.copy_answering_file();
        let r = silent.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert!(r.board_answered);
        assert_eq!(
            r.evidence.map(|e| e.verdict).as_deref(),
            Some(evidence::CONSISTENT_WITH_FILE)
        );
        assert_eq!(
            *silent.adopted.lock(),
            [TTY.to_string()],
            "adopted once, on the interface it was signalled on"
        );
        assert_eq!(
            *silent.board.opened.lock(),
            [TTY.to_string(), TTY.to_string()],
            "opened to be asked and signalled, and once more to be checked"
        );
        assert!(!silent.gate.load(SeqCst), "probes may run again");
        assert!(silent.cache.openfan_maintenance().is_none());
        assert!(!silent.cache.openfan_writes_suspended());
        let c = silent.cache.clone();
        wait_until("the adopted board's polls", || {
            c.openfan_link() == Some(OpenFanLink::Connected)
                && c.read_with(|s| s.openfan_fans.len()) == NUM_CHANNELS as usize
        })
        .await;
    }

    #[tokio::test]
    async fn a_board_gone_silent_parks_its_poll_loop_and_is_handed_back_to_it() {
        let silent = Silent::gone_silent(Board::new()).await;
        // Precondition: the loop is searching for the board, and reopens it.
        let before = silent.board.reconnects.load(SeqCst);
        let b = silent.board.clone();
        wait_until("a reconnect attempt", || b.reconnects.load(SeqCst) > before).await;
        let run = silent.start();
        silent.reached(stage::WAITING_FOR_FILE).await;
        assert_eq!(silent.board.sent(">07"), 0);
        assert_eq!(silent.board.touches.load(SeqCst), 1);
        // [SAFETY] The loop is parked on the loan: its reconnect search makes
        // no attempt. Watched for twice its longest backoff, 30 polls.
        let attempts = silent.board.reconnects.load(SeqCst);
        tokio::time::sleep(POLL * 60).await;
        assert_eq!(
            silent.board.reconnects.load(SeqCst),
            attempts,
            "no reconnect attempt while the update holds the port"
        );

        let polls_before = silent.cache.openfan_polls_started();
        silent.copy_answering_file();
        let r = silent.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert!(
            silent.adopted.lock().is_empty(),
            "the poll loop took the board back: nothing was adopted"
        );
        assert_eq!(silent.cache.openfan_link(), Some(OpenFanLink::Connected));
        assert!(silent.cache.openfan_maintenance().is_none());
        assert!(!silent.cache.openfan_writes_suspended());
        let c = silent.cache.clone();
        wait_until("polls on the returned port", || {
            c.read_with(|s| s.openfan_fans.values().all(|f| f.poll_seq > polls_before))
        })
        .await;
    }

    /// The board a connected update left back on USB but silent: its update
    /// as a silent board takes the place of the recovery, and ends it.
    #[tokio::test]
    async fn a_board_an_update_left_silent_is_updated_again_and_its_recovery_ends() {
        let bench = Bench::new(Board::new()).await;
        let first = bench.start();
        bench.reached(stage::WAITING_FOR_FILE).await;
        bench.board.answers.store(false, SeqCst);
        bench.board.copy_file(NEW, "02");
        let r = bench.finish(first).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::FIRMWARE_COPIED_BOARD_NOT_BACK)
        );
        let silent = Silent::over(bench).await;
        assert_eq!(silent.recovery_of().as_deref(), Some(RUN), "precondition");
        let second = silent.start();
        silent.reached(stage::WAITING_FOR_FILE).await;
        silent.copy_answering_file();
        let r = silent.finish(second).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert_eq!(silent.recovery_of(), None);
        assert!(!silent.cache.openfan_writes_suspended());
        assert_eq!(silent.cache.openfan_link(), Some(OpenFanLink::Connected));
    }

    #[tokio::test]
    async fn a_silent_board_the_signal_does_not_move_waits_for_its_boot_button() {
        let board = Board::new();
        *board.on_1200.lock() = OnTrigger::Nothing;
        let silent = Silent::never_adopted(board);
        let run = silent.start();
        silent.reached(stage::WAITING_FOR_BOOT_BUTTON).await;
        let r = silent.record();
        assert_eq!(r.bootloader_trigger.as_deref(), Some(trigger::BOOT_BUTTON));
        assert!(r.cancellable, "cancellable while the board has not left");
        assert_eq!(silent.board.touches.load(SeqCst), 1);
        assert_eq!(silent.board.mode(), Mode::Firmware);

        // The user holds BOOT and presses RESET.
        silent.board.trigger(OnTrigger::Bootloader);
        silent.reached(stage::WAITING_FOR_FILE).await;
        assert!(!silent.record().cancellable);
        assert_eq!(silent.slot.request_cancel(), CancelOutcome::TooLate);
        silent.copy_answering_file();
        let r = silent.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert!(stages(&r).contains(&stage::WAITING_FOR_BOOT_BUTTON));
        assert_eq!(*silent.adopted.lock(), [TTY.to_string()]);
    }

    #[tokio::test]
    async fn a_cancel_while_waiting_for_the_boot_button_changes_nothing_and_keeps_the_recovery() {
        let board = Board::new();
        *board.on_1200.lock() = OnTrigger::Nothing;
        let silent = Silent::never_adopted(board);
        // The last run, before a restart, left the board back but silent.
        silent
            .cache
            .restore_openfan_recovery("r0", outcome::FIRMWARE_COPIED_BOARD_NOT_BACK);
        let run = silent.start();
        silent.reached(stage::WAITING_FOR_BOOT_BUTTON).await;
        assert_eq!(
            silent.recovery_of(),
            None,
            "the run took the recovery's place"
        );
        assert_eq!(silent.slot.request_cancel(), CancelOutcome::Requested);
        let r = silent.finish(run).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert!(r.cancelled);
        assert_eq!(
            silent.board.mode(),
            Mode::Firmware,
            "the board was not changed"
        );
        assert!(silent.adopted.lock().is_empty());
        assert!(!silent.gate.load(SeqCst));
        // [SAFETY] The board is as the last run left it: so is its recovery.
        assert_eq!(silent.recovery_of().as_deref(), Some("r0"));
        assert!(silent.cache.openfan_writes_suspended());
    }

    /// The user presses the buttons and Cancel together: the board is on its
    /// way into its bootloader, so the update goes on rather than leave it there.
    #[tokio::test]
    async fn a_cancel_as_the_buttons_restart_the_board_does_not_strand_it() {
        let board = Board::new();
        *board.on_1200.lock() = OnTrigger::Nothing;
        let silent = Silent::never_adopted(board);
        let run = silent.start();
        silent.reached(stage::WAITING_FOR_BOOT_BUTTON).await;
        // The board leaves its port now, and comes back in its bootloader.
        silent.board.trigger(OnTrigger::Bootloader);
        assert_eq!(silent.slot.request_cancel(), CancelOutcome::Requested);
        silent.reached(stage::WAITING_FOR_FILE).await;
        let r = silent.record();
        assert!(
            r.notes.iter().any(|n| n.contains("a cancel arrived")),
            "{:?}",
            r.notes
        );
        silent.copy_answering_file();
        let r = silent.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert!(!r.cancelled);
    }

    /// A board unplugged while the buttons are awaited: a cancel lands once a
    /// bootloader would have appeared, not at the end of the wait.
    #[tokio::test]
    async fn a_cancel_with_the_board_gone_from_usb_lands_once_nothing_came_back() {
        let board = Board::new();
        *board.on_1200.lock() = OnTrigger::Nothing;
        let silent = Silent::never_adopted(board);
        let limits = StageLimits {
            bootloader_wait: Duration::from_millis(300),
            boot_button_wait: Duration::from_secs(15),
            ..quick()
        };
        let run = silent.start_with(limits);
        silent.reached(stage::WAITING_FOR_BOOT_BUTTON).await;
        silent.board.leave();
        assert_eq!(silent.slot.request_cancel(), CancelOutcome::Requested);
        let started = Instant::now();
        let r = silent.finish(run).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert!(r.cancelled);
        assert!(
            started.elapsed() < limits.boot_button_wait / 3,
            "the cancel landed after {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn no_boot_button_in_time_ends_with_nothing_changed() {
        let board = Board::new();
        *board.on_1200.lock() = OnTrigger::Nothing;
        let silent = Silent::never_adopted(board);
        let limits = StageLimits {
            boot_button_wait: Duration::from_millis(300),
            ..quick()
        };
        let r = silent.finish(silent.start_with(limits)).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert_eq!(r.stage, stage::WAITING_FOR_BOOT_BUTTON);
        assert!(r.outcome_detail.unwrap().contains("in time"));
        assert!(!r.cancelled);
        assert!(silent.cache.openfan_maintenance().is_none());
        assert!(!silent.gate.load(SeqCst));
    }

    /// The buttons pressed in the wait's last moments: the board is on its
    /// way into its bootloader as the time runs out, so the update goes on
    /// rather than call the board unchanged and leave it there.
    #[tokio::test]
    async fn buttons_pressed_as_the_wait_runs_out_still_bring_the_update() {
        let board = Board::new();
        *board.on_1200.lock() = OnTrigger::Nothing;
        let silent = Silent::never_adopted(board);
        let limits = StageLimits {
            boot_button_wait: Duration::from_secs(1),
            bootloader_wait: Duration::from_secs(3),
            ..quick()
        };
        let run = silent.start_with(limits);
        silent.reached(stage::WAITING_FOR_BOOT_BUTTON).await;
        let entered = Instant::now();
        // The board leaves its port before the time runs out...
        tokio::time::sleep(Duration::from_millis(300)).await;
        silent.board.leave();
        // ...and its bootloader appears only after it has.
        tokio::time::sleep(
            limits.boot_button_wait.saturating_sub(entered.elapsed()) + Duration::from_millis(500),
        )
        .await;
        assert_ne!(
            silent.record().state,
            STATE_FINISHED,
            "the run must not end while the board may be on its way into its bootloader"
        );
        silent.board.enter_bootloader();
        silent.reached(stage::WAITING_FOR_FILE).await;
        silent.copy_answering_file();
        let r = silent.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    #[tokio::test]
    async fn a_daemon_stop_while_waiting_for_the_boot_button_leaves_the_board_needing_recovery() {
        let board = Board::new();
        *board.on_1200.lock() = OnTrigger::Nothing;
        let silent = Silent::never_adopted(board);
        let run = silent.start();
        silent.reached(stage::WAITING_FOR_BOOT_BUTTON).await;
        silent.stop.send(true).unwrap();
        let r = silent.finish(run).await;
        // The user may already be pressing the buttons.
        assert_eq!(r.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        assert!(r.interrupted);
        assert!(silent.recovery_of().is_some());
        assert!(silent.cache.openfan_writes_suspended());
    }

    /// The board answers when it is asked once more: it is not silent, so it
    /// is never signalled — the update hands it over unchanged.
    #[tokio::test]
    async fn a_board_that_answers_after_all_is_never_signalled() {
        let silent = Silent::never_adopted(Board::new());
        silent.board.answers.store(true, SeqCst);
        let r = silent.finish(silent.start()).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert!(r.outcome_detail.unwrap().contains("not silent"));
        assert_eq!(silent.board.touches.load(SeqCst), 0);
        assert_eq!(silent.board.sent(">07"), 0);
        assert!(!r.bootloader_requested);
        assert_eq!(
            *silent.adopted.lock(),
            [TTY.to_string()],
            "adopted on the port that answered"
        );
        assert!(silent.cache.openfan_maintenance().is_none());
        assert!(!silent.cache.openfan_writes_suspended());
    }

    /// A probe already running when the update starts is waited for: the
    /// board is signalled only once the probe has let it go.
    #[tokio::test]
    async fn a_probe_already_running_is_waited_for_before_the_board_is_signalled() {
        let silent = Silent::never_adopted(Board::new());
        silent.gate.store(true, SeqCst);
        let run = silent.start();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(silent.record().stage, stage::PREPARING);
        assert!(silent.board.opened.lock().is_empty(), "nothing opened yet");
        silent.gate.store(false, SeqCst);
        silent.reached(stage::WAITING_FOR_FILE).await;
        assert_eq!(silent.board.touches.load(SeqCst), 1);
        silent.copy_answering_file();
        silent.finish(run).await;

        // One that never ends: the update gives up, having touched nothing.
        let silent = Silent::never_adopted(Board::new());
        silent.gate.store(true, SeqCst);
        let limits = StageLimits {
            probe_wait: Duration::from_millis(200),
            ..quick()
        };
        let r = silent.finish(silent.start_with(limits)).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
        assert!(silent.board.opened.lock().is_empty());
        assert!(
            silent.gate.load(SeqCst),
            "the probe's flag is the probe's to clear, not the update's"
        );
    }

    /// Another program holds the board's serial device: no signal can be
    /// sent, so the buttons are asked for straight away.
    #[tokio::test]
    async fn a_board_whose_serial_device_will_not_open_goes_straight_to_the_boot_button() {
        let silent = Silent::never_adopted(Board::new());
        silent.board.refuse_open.store(true, SeqCst);
        let run = silent.start();
        silent.reached(stage::WAITING_FOR_BOOT_BUTTON).await;
        assert_eq!(silent.board.touches.load(SeqCst), 0);
        let r = silent.record();
        assert!(
            r.notes.iter().any(|n| n.contains("could not be opened")),
            "{:?}",
            r.notes
        );
        assert_eq!(silent.slot.request_cancel(), CancelOutcome::Requested);
        let r = silent.finish(run).await;
        assert_eq!(r.outcome.as_deref(), Some(outcome::NO_FIRMWARE_CHANGE));
    }

    /// A poll loop that has not yet let go of the port it holds — its board
    /// has just stopped answering, and no reconnect attempt has closed the
    /// port yet: it lends whatever its slot holds, once, and settles whatever
    /// comes back.
    fn lend_once(
        slot: port_loan::PortSlot,
        mut loans: port_loan::LoanReceiver,
        mut stop: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let Some(request) = loans.recv().await else {
                return;
            };
            match port_loan::lend(&slot, request, false, &mut stop).await {
                port_loan::LoanEnd::Returned { settled, .. }
                | port_loan::LoanEnd::Lost { settled } => {
                    let _ = settled.send(());
                }
                _ => {}
            }
        })
    }

    /// [SAFETY] The loop lends the port of the board it was adopted for, open
    /// on another node: the update neither asks nor signals that board — it
    /// may be driving fans — and opens the silent board's own node instead.
    #[tokio::test]
    async fn a_lent_port_on_another_boards_node_is_never_signalled() {
        let other = Board::new();
        other.answers.store(false, SeqCst);
        let mut silent = Silent::never_adopted(Board::new());
        silent.cache.set_openfan_port("/dev/ttyACM90");
        let (lender, loans) = loan_channel();
        let _loop = lend_once(
            Arc::new(Mutex::new(other.port())),
            loans,
            silent.stop.subscribe(),
        );
        silent.lender = Some(lender);
        let run = silent.start();
        let (b, o) = (silent.board.clone(), other.clone());
        wait_until("the signal", || {
            b.touches.load(SeqCst) + o.touches.load(SeqCst) > 0
        })
        .await;
        assert_eq!(
            other.touches.load(SeqCst),
            0,
            "the board on the lent port was signalled"
        );
        assert!(
            other.frames.lock().is_empty(),
            "the board on the lent port was asked"
        );
        assert_eq!(silent.board.touches.load(SeqCst), 1);
        silent.reached(stage::WAITING_FOR_FILE).await;
        assert_eq!(
            *silent.board.opened.lock(),
            [TTY.to_string()],
            "its own node, opened to be asked"
        );
        silent.copy_answering_file();
        let r = silent.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert_eq!(other.mode(), Mode::Firmware, "the other board never left");
    }

    /// The lent port is the silent board's own, still open: the board is asked
    /// and signalled through it — a fresh open would meet its lock.
    #[tokio::test]
    async fn a_lent_port_on_the_boards_own_node_is_the_one_signalled() {
        let mut silent = Silent::never_adopted(Board::new());
        silent.cache.set_openfan_port(TTY);
        let (lender, loans) = loan_channel();
        let _loop = lend_once(
            Arc::new(Mutex::new(silent.board.port())),
            loans,
            silent.stop.subscribe(),
        );
        silent.lender = Some(lender);
        let run = silent.start();
        silent.reached(stage::WAITING_FOR_FILE).await;
        assert_eq!(silent.board.touches.load(SeqCst), 1);
        assert!(
            silent.board.opened.lock().is_empty(),
            "asked and signalled through the lent port"
        );
        silent.copy_answering_file();
        let r = silent.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
    }

    /// A cancel the wait sees just before the buttons restart the board: the
    /// next look finds the board on its way into its bootloader, so the update
    /// goes on rather than call the board unchanged and leave it there.
    #[tokio::test]
    async fn a_cancel_seen_just_before_the_buttons_restart_the_board_does_not_strand_it() {
        let board = Board::new();
        *board.on_1200.lock() = OnTrigger::Nothing;
        let silent = Silent::never_adopted(board);
        // One look at the port a second: the cancel is seen by the first look
        // after it, and the buttons are pressed before the next.
        let look = Duration::from_secs(1);
        let limits = StageLimits {
            sysfs_poll: look,
            boot_button_wait: Duration::from_secs(60),
            ..quick()
        };
        let run = silent.start_with(limits);
        silent.reached(stage::WAITING_FOR_BOOT_BUTTON).await;
        let entered = Instant::now();
        tokio::time::sleep(look / 5).await;
        assert_eq!(silent.slot.request_cancel(), CancelOutcome::Requested);
        tokio::time::sleep_until((entered + look * 3 / 2).into()).await;
        assert_ne!(
            silent.record().state,
            STATE_FINISHED,
            "the look that first sees a cancel must not end the wait"
        );
        silent.board.trigger(OnTrigger::Bootloader);
        silent.reached(stage::WAITING_FOR_FILE).await;
        let r = silent.record();
        assert!(
            r.notes.iter().any(|n| n.contains("a cancel arrived")),
            "{:?}",
            r.notes
        );
        silent.copy_answering_file();
        let r = silent.finish(run).await;
        assert_eq!(
            r.outcome.as_deref(),
            Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED)
        );
        assert!(!r.cancelled);
    }
}
