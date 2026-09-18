//! Give hardware back exactly as it was found (DEC-382 — `TS-a`/`TS-b`/`TS-c`).
//!
//! The daemon takes an hwmon header away from firmware by writing
//! `pwmN_enable=1`. Until DEC-382 every route that gave one back — the clean
//! shutdown, the panic hook and `ExecStopPost` — wrote a hardcoded `2`, and the
//! routes that end a thermal force, a diagnostic or a profile gave nothing back
//! at all. `2` is "automatic" only on `it87`, the board this was validated on:
//! on `nct6775` it selects Thermal Cruise rather than the Smart Fan IV (`5`) the
//! BIOS left, and on `nzxt-kraken3` it uploads a curve buffer that is all zero
//! unless something wrote it, which puts the pump on a 0 % curve.
//!
//! The rule here is the one `fancontrol` (`PWM_ENABLE_ORIG_STATE`) and `fan2go`
//! (`onExit: restore`) both follow: read what a header was doing just before the
//! daemon first took it, and give back exactly that. Where that is impossible or
//! known to be unsafe, fall back to full speed, as `fancontrol` does.
//!
//! # The record
//!
//! A crashed daemon cannot give anything back itself, so every take is written
//! to a small record **before** the `pwm_enable=1` write, and `ExecStopPost`
//! (`control-ofc-restore-auto`) replays it. It lives in the unit's
//! `RuntimeDirectory` because that directory has exactly the lifetime the record
//! needs: systemd removes it once the unit has stopped — after `ExecStopPost` has
//! run — and a reboot, which returns every header to its BIOS mode anyway, empties
//! `/run`. It is tmpfs, so a take costs no disk I/O, and it needs no `fsync`: the
//! only failure it has to survive is the death of this process.
//!
//! One line per header the daemon holds right now, tab-separated:
//!
//! ```text
//! <pwmN_enable path>  <pwmN path>  mode    <value>   write pwm_enable = value
//! <pwmN_enable path>  <pwmN path>  manual  <raw>     write pwm_enable = 1, pwm = raw
//! <pwmN_enable path>  <pwmN path>  full    -         fancontrol's fallback
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;

use crate::hwmon::pwm_control::SysfsWriter;
use crate::hwmon::pwm_discovery::PwmHeaderDescriptor;

/// File name of the record inside the unit's runtime directory.
pub const RECORD_FILE_NAME: &str = "hwmon-handback";

/// First line of every record. `control-ofc-restore-auto` skips `#` lines.
const RECORD_HEADER: &str =
    "# control-ofc hwmon hand-back record v1: headers the daemon has taken \
     from firmware. Written by control-ofc-daemon, replayed by control-ofc-restore-auto \
     (ExecStopPost). Do not edit.";

/// Raw duty at or above which a full-speed fallback counts as having landed.
///
/// `fancontrol`'s own threshold (`pwmdisable`, `-ge 190`): some chips cap or round
/// the register below 255, so demanding exactly 255 would report a fallback that
/// worked as one that failed.
const FULL_SPEED_MIN_RAW: u8 = 190;

/// What giving a header back writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandBack {
    /// Write this `pwmN_enable` value back. Never `1`: a header found in manual
    /// mode is [`HandBack::Manual`], because its duty has to come back with it.
    Mode(u8),
    /// The header was already in manual mode, at this raw duty. Give back both.
    Manual(u8),
    /// The original could not be read, or writing it back is known to be unsafe:
    /// `fancontrol`'s fallback — `pwmN_enable=0` (no control, full speed), else
    /// manual at 255.
    FullSpeed,
}

impl HandBack {
    /// What a header gets back, from what it reported just before the daemon
    /// first took it.
    ///
    /// * An unreadable `pwmN_enable` is an unrecorded mode → [`Self::FullSpeed`].
    /// * `1` means something — a user, another tool, or a daemon that died
    ///   without its `ExecStopPost` running — already had it in manual mode. The
    ///   duty is part of that state, so an unreadable duty is unrecorded too.
    /// * `2` on an `nzxt-kraken3` device is never written back: that write
    ///   uploads a curve buffer that is all zero unless something wrote it
    ///   ([`crate::hwmon::aio::is_nzxt_kraken3_chip`]). A Kraken reports `0` after
    ///   probe, and `0` gives back fixed 100 %, so a Kraken reporting `2` is one
    ///   some other writer put there — an earlier daemon's hardcoded restore among
    ///   them — and replaying it is exactly the defect this module removes.
    pub fn from_reading(chip_name: &str, enable: Option<u8>, raw_pwm: Option<u8>) -> Self {
        match enable {
            None => Self::FullSpeed,
            Some(1) => raw_pwm.map_or(Self::FullSpeed, Self::Manual),
            Some(2) if crate::hwmon::aio::is_nzxt_kraken3_chip(chip_name) => Self::FullSpeed,
            Some(mode) => Self::Mode(mode),
        }
    }

    /// The `(kind, value)` columns of this action's record line.
    fn record_columns(self) -> (&'static str, String) {
        match self {
            Self::Mode(mode) => ("mode", mode.to_string()),
            Self::Manual(raw) => ("manual", raw.to_string()),
            Self::FullSpeed => ("full", "-".to_string()),
        }
    }
}

/// What one hand-back achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandBackOutcome {
    /// The recorded state is back, and reading it back confirmed it.
    Restored,
    /// The recorded state could not be given back — or none was recorded — and
    /// the header runs at full speed instead.
    FullSpeed,
    /// Nothing could be written: the header is still wherever the daemon left it.
    Failed,
}

impl HandBackOutcome {
    /// True when the daemon no longer holds the header: firmware has it back, or
    /// it runs at full speed. Only [`Self::Failed`] leaves it taken.
    pub fn released(self) -> bool {
        !matches!(self, Self::Failed)
    }
}

/// Give one header back: write `action`, read it back, and fall back to full
/// speed when the read-back disagrees.
///
/// Every write is confirmed by reading it back, because a write syscall that
/// succeeds is not a mode that took: a driver can accept a value and store
/// something else. A hand-back that is not confirmed is not assumed.
pub fn hand_back(
    writer: &mut dyn SysfsWriter,
    enable_path: &str,
    pwm_path: &str,
    action: HandBack,
) -> HandBackOutcome {
    let restored = match action {
        HandBack::Mode(mode) => {
            writer.write_file(enable_path, &mode.to_string()).is_ok()
                && read_u8(writer, enable_path) == Some(mode)
        }
        HandBack::Manual(raw) => {
            writer.write_file(enable_path, "1").is_ok()
                && writer.write_file(pwm_path, &raw.to_string()).is_ok()
                && manual_confirmed(writer, enable_path, pwm_path, raw)
        }
        HandBack::FullSpeed => false,
    };
    if restored {
        return HandBackOutcome::Restored;
    }
    full_speed(writer, enable_path, pwm_path)
}

/// `fancontrol`'s `pwmdisable`: `pwmN_enable=0` (no control, full speed); where
/// the driver refuses that, manual mode at 255.
fn full_speed(writer: &mut dyn SysfsWriter, enable_path: &str, pwm_path: &str) -> HandBackOutcome {
    if writer.write_file(enable_path, "0").is_ok() && read_u8(writer, enable_path) == Some(0) {
        return HandBackOutcome::FullSpeed;
    }
    if writer.write_file(enable_path, "1").is_ok()
        && writer.write_file(pwm_path, "255").is_ok()
        && read_u8(writer, pwm_path).is_some_and(|raw| raw >= FULL_SPEED_MIN_RAW)
    {
        return HandBackOutcome::FullSpeed;
    }
    HandBackOutcome::Failed
}

/// Is the header in manual mode at `raw`? Mode `1` is the plain answer; mode `0`
/// at full scale is the same state on drivers that report it that way
/// (`pwm::is_full_speed_alias`, DEC-326).
fn manual_confirmed(writer: &dyn SysfsWriter, enable_path: &str, pwm_path: &str, raw: u8) -> bool {
    match read_u8(writer, enable_path) {
        Some(1) => true,
        mode @ Some(0) => crate::pwm::is_full_speed_alias(
            crate::pwm::raw_to_percent(raw),
            read_u8(writer, pwm_path).map(crate::pwm::raw_to_percent),
            mode,
        ),
        _ => false,
    }
}

/// Read a sysfs attribute as a `u8`. Shared with the take in
/// `HwmonPwmController::set_pwm`, so the value an original is recorded from and
/// the value a hand-back is confirmed against are parsed the same way.
pub(crate) fn read_u8(writer: &dyn SysfsWriter, path: &str) -> Option<u8> {
    writer
        .read_file(path)
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
}

/// One header the daemon holds right now, and what giving it back writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenHeader {
    pub id: String,
    pub enable_path: String,
    pub pwm_path: String,
    pub action: HandBack,
}

#[derive(Debug)]
struct Entry {
    enable_path: String,
    pwm_path: String,
    /// What the header was doing before the daemon first took it.
    ///
    /// Set on the first take and **never overwritten**. A firmware reclaim, a
    /// resume, a lease hand-over and a re-take after a hand-back all switch the
    /// header to manual again, but each of those starts from state the daemon
    /// itself left or firmware re-asserted, so none of them is a better original
    /// than the one read before the daemon touched the header at all.
    original: Option<HandBack>,
    /// The daemon has the header in manual mode now.
    taken: bool,
    /// A failed hand-back has been reported since the last success, so a header
    /// that cannot be written is logged once rather than at 1 Hz.
    failure_logged: bool,
}

#[derive(Debug, Default)]
struct LedgerState {
    entries: HashMap<String, Entry>,
    record_path: Option<PathBuf>,
    record_error_logged: bool,
}

/// Which hwmon headers the daemon has taken, and what each one gets back.
///
/// [SAFETY] DEC-382. Held behind its own lock rather than inside
/// `HwmonPwmController`'s, deliberately: the shutdown restore and the panic hook
/// both have to read it while a wedged sysfs write may be holding the controller
/// mutex for good. Nothing under this lock touches hardware — the reads that
/// decide an original happen before it is taken — so it cannot be held by a
/// wedged chip.
#[derive(Debug)]
pub struct HandBackLedger {
    state: Mutex<LedgerState>,
}

impl HandBackLedger {
    /// One entry per header that exposes `pwmN_enable` — the only headers the
    /// daemon ever switches out of firmware control. A header with no enable file
    /// has no mode to take or give back, and `set_pwm` never writes one.
    pub fn new(headers: &[PwmHeaderDescriptor]) -> Self {
        let entries = headers
            .iter()
            .filter(|h| h.supports_enable)
            .filter_map(|h| {
                let enable_path = h.enable_path.clone()?;
                Some((
                    h.id.clone(),
                    Entry {
                        enable_path,
                        pwm_path: h.pwm_path.clone(),
                        original: None,
                        taken: false,
                        failure_logged: false,
                    },
                ))
            })
            .collect();
        Self {
            state: Mutex::new(LedgerState {
                entries,
                ..LedgerState::default()
            }),
        }
    }

    /// Keep the on-disk record at `path` from now on, writing it once
    /// immediately so it always describes this process.
    pub fn set_record_path(&self, path: PathBuf) {
        let mut state = self.state.lock();
        state.record_path = Some(path);
        persist(&mut state);
    }

    /// True when `id` is tracked and has no recorded original yet: the next take
    /// is the first, and the caller must read the header before switching it.
    pub fn needs_original(&self, id: &str) -> bool {
        self.state
            .lock()
            .entries
            .get(id)
            .is_some_and(|e| e.original.is_none())
    }

    /// Record that the daemon is about to take `id`.
    ///
    /// [SAFETY] Call **before** writing `pwm_enable=1`. The record is write-ahead
    /// for the same reason a journal is: if the process dies between the two, a
    /// record naming a header that never reached manual mode replays as a no-op,
    /// while a header in manual mode that no record names is stranded there.
    /// `original` is stored only when none is recorded yet.
    pub fn note_take(&self, id: &str, original: Option<HandBack>) {
        let mut state = self.state.lock();
        let changed = match state.entries.get_mut(id) {
            None => false,
            Some(entry) => {
                let mut changed = false;
                if entry.original.is_none() && original.is_some() {
                    entry.original = original;
                    changed = true;
                }
                if !entry.taken {
                    entry.taken = true;
                    changed = true;
                }
                changed
            }
        };
        if changed {
            persist(&mut state);
        }
    }

    /// True when the daemon holds `id` in manual mode now.
    pub fn is_taken(&self, id: &str) -> bool {
        self.state.lock().entries.get(id).is_some_and(|e| e.taken)
    }

    /// Every header the daemon holds now, sorted.
    pub fn taken_ids(&self) -> Vec<String> {
        let state = self.state.lock();
        let mut ids: Vec<String> = state
            .entries
            .iter()
            .filter(|(_, e)| e.taken)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// `id` and what giving it back writes, if the daemon holds it.
    pub fn taken_header(&self, id: &str) -> Option<TakenHeader> {
        let state = self.state.lock();
        state
            .entries
            .get(id)
            .filter(|e| e.taken)
            .map(|e| taken(id, e))
    }

    /// Every header the daemon holds, waiting at most `timeout` for the lock.
    ///
    /// For the shutdown restore and the panic hook, which must not wait on a lock
    /// without a bound. `None` means the lock could not be taken in time — which
    /// nothing in this module should cause, because nothing under it does I/O
    /// beyond a tmpfs write; `ExecStopPost` replays the record either way.
    pub fn try_taken(&self, timeout: Duration) -> Option<Vec<TakenHeader>> {
        let state = self.state.try_lock_for(timeout)?;
        let mut out: Vec<TakenHeader> = state
            .entries
            .iter()
            .filter(|(_, e)| e.taken)
            .map(|(id, e)| taken(id, e))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Some(out)
    }

    /// Record that `id` was given back: firmware has it, or it runs at full speed.
    pub fn note_handed_back(&self, id: &str) {
        let mut state = self.state.lock();
        let changed = match state.entries.get_mut(id) {
            Some(entry) if entry.taken => {
                entry.taken = false;
                entry.failure_logged = false;
                true
            }
            _ => false,
        };
        if changed {
            persist(&mut state);
        }
    }

    /// Record a hand-back that wrote nothing. Returns `true` the first time since
    /// the header was last given back successfully, so the caller logs once.
    pub fn note_hand_back_failed(&self, id: &str) -> bool {
        let mut state = self.state.lock();
        match state.entries.get_mut(id) {
            Some(entry) if !entry.failure_logged => {
                entry.failure_logged = true;
                true
            }
            _ => false,
        }
    }
}

fn taken(id: &str, e: &Entry) -> TakenHeader {
    TakenHeader {
        id: id.to_string(),
        enable_path: e.enable_path.clone(),
        pwm_path: e.pwm_path.clone(),
        // A take with no original is one whose read failed before the switch —
        // unrecorded, so it gets the fallback, never a guess.
        action: e.original.unwrap_or(HandBack::FullSpeed),
    }
}

fn persist(state: &mut LedgerState) {
    let Some(path) = state.record_path.clone() else {
        return;
    };
    match write_replacing(&path, &render_record(&state.entries)) {
        Ok(()) => {
            if state.record_error_logged {
                log::info!(
                    "hwmon hand-back record {} is being written again",
                    path.display()
                );
            }
            state.record_error_logged = false;
        }
        Err(e) => {
            if !state.record_error_logged {
                log::warn!(
                    "could not write the hwmon hand-back record {}: {e} — if the daemon \
                     crashes now, ExecStopPost cannot give back the headers it holds",
                    path.display()
                );
            }
            state.record_error_logged = true;
        }
    }
}

fn render_record(entries: &HashMap<String, Entry>) -> String {
    let mut held: Vec<&Entry> = entries.values().filter(|e| e.taken).collect();
    held.sort_by(|a, b| a.enable_path.cmp(&b.enable_path));
    let mut out = String::from(RECORD_HEADER);
    out.push('\n');
    for e in held {
        // A separator inside a field could not be parsed back. sysfs paths never
        // contain one; a line that did would be skipped, never replayed wrong.
        if [&e.enable_path, &e.pwm_path]
            .iter()
            .any(|p| p.contains(['\t', '\n']))
        {
            continue;
        }
        let (kind, value) = e.original.unwrap_or(HandBack::FullSpeed).record_columns();
        out.push_str(&format!(
            "{}\t{}\t{kind}\t{value}\n",
            e.enable_path, e.pwm_path
        ));
    }
    out
}

/// Replace `path` in one step, so `ExecStopPost` can never read half a record.
/// No `fsync`: the record lives on tmpfs and only has to outlive this process.
fn write_replacing(path: &Path, body: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::HwmonError;

    /// An in-memory sysfs that can refuse a write (`EINVAL`) or store something
    /// other than what was written — the two ways a driver says "not that".
    #[derive(Default)]
    struct FakeSysfs {
        files: HashMap<String, String>,
        refused: Vec<(String, String)>,
        remapped: Vec<(String, String, String)>,
        log: Vec<(String, String)>,
    }

    impl FakeSysfs {
        fn with(mut self, path: &str, value: &str) -> Self {
            self.files.insert(path.into(), value.into());
            self
        }
        fn refusing(mut self, path: &str, value: &str) -> Self {
            self.refused.push((path.into(), value.into()));
            self
        }
        fn storing_instead(mut self, path: &str, written: &str, stored: &str) -> Self {
            self.remapped
                .push((path.into(), written.into(), stored.into()));
            self
        }
        fn get(&self, path: &str) -> Option<&str> {
            self.files.get(path).map(String::as_str)
        }
    }

    impl SysfsWriter for FakeSysfs {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.log.push((path.into(), value.into()));
            if self.refused.iter().any(|(p, v)| p == path && v == value) {
                return Err(HwmonError::WriteError {
                    path: path.into(),
                    message: "Invalid argument (os error 22)".into(),
                });
            }
            let stored = self
                .remapped
                .iter()
                .find(|(p, w, _)| p == path && w == value)
                .map_or(value.to_string(), |(_, _, s)| s.clone());
            self.files.insert(path.into(), stored);
            Ok(())
        }
        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            self.files
                .get(path)
                .map(|v| format!("{v}\n"))
                .ok_or_else(|| HwmonError::ReadError {
                    path: path.into(),
                    message: "No such file or directory (os error 2)".into(),
                })
        }
    }

    const EN: &str = "/sys/class/hwmon/hwmon3/pwm2_enable";
    const PWM: &str = "/sys/class/hwmon/hwmon3/pwm2";

    #[test]
    fn the_original_is_whatever_the_header_reported_before_the_take() {
        assert_eq!(
            HandBack::from_reading("it8696", Some(2), Some(90)),
            HandBack::Mode(2)
        );
        // nct6775's Smart Fan IV — the mode a hardcoded `2` used to replace with
        // Thermal Cruise.
        assert_eq!(
            HandBack::from_reading("nct6798", Some(5), Some(90)),
            HandBack::Mode(5)
        );
        // nct6687d's older firmware-control value survives verbatim.
        assert_eq!(
            HandBack::from_reading("nct6687", Some(99), None),
            HandBack::Mode(99)
        );
        assert_eq!(
            HandBack::from_reading("it8696", Some(1), Some(120)),
            HandBack::Manual(120)
        );
        // Manual without a readable duty is not a state we can give back.
        assert_eq!(
            HandBack::from_reading("it8696", Some(1), None),
            HandBack::FullSpeed
        );
        assert_eq!(
            HandBack::from_reading("it8696", None, Some(120)),
            HandBack::FullSpeed
        );
    }

    /// [SAFETY] `TS-a`: a Kraken is never handed `2`. Both arms, so a stuck
    /// predicate fails one of them: `0` (what the driver reports after probe) is
    /// given back verbatim — fixed 100 % — while `2` becomes the fallback.
    #[test]
    fn a_kraken_is_never_given_mode_2_back() {
        for chip in [
            "x53",
            "z53",
            "kraken2023",
            "kraken2023elite",
            "kraken2024elite",
        ] {
            assert_eq!(
                HandBack::from_reading(chip, Some(2), Some(128)),
                HandBack::FullSpeed,
                "{chip}: mode 2 uploads a curve buffer nothing filled"
            );
            assert_eq!(
                HandBack::from_reading(chip, Some(0), Some(128)),
                HandBack::Mode(0),
                "{chip}: the probe-time mode is given back as it was"
            );
        }
        // The same reading on a chip whose `2` really is automatic is kept.
        assert_eq!(
            HandBack::from_reading("it8696", Some(2), Some(128)),
            HandBack::Mode(2)
        );
    }

    #[test]
    fn a_recorded_mode_is_written_back_and_confirmed() {
        let mut sysfs = FakeSysfs::default().with(EN, "1").with(PWM, "255");
        let outcome = hand_back(&mut sysfs, EN, PWM, HandBack::Mode(5));
        assert_eq!(outcome, HandBackOutcome::Restored);
        assert_eq!(sysfs.get(EN), Some("5"));
    }

    /// A mode the driver stores as something else is not a mode that took: the
    /// header falls back to full speed rather than being reported restored.
    #[test]
    fn a_mode_that_does_not_read_back_falls_back_to_full_speed() {
        let mut sysfs = FakeSysfs::default()
            .with(EN, "1")
            .with(PWM, "80")
            .storing_instead(EN, "5", "2");
        let outcome = hand_back(&mut sysfs, EN, PWM, HandBack::Mode(5));
        assert_eq!(outcome, HandBackOutcome::FullSpeed);
        assert_eq!(sysfs.get(EN), Some("0"));
    }

    /// `fancontrol`'s second step: a driver that refuses `0` (as `nct6687d`
    /// does) gets manual mode at 255, and that counts as full speed.
    #[test]
    fn a_driver_that_refuses_0_gets_manual_at_255() {
        let mut sysfs = FakeSysfs::default()
            .with(EN, "1")
            .with(PWM, "80")
            .refusing(EN, "0");
        let outcome = hand_back(&mut sysfs, EN, PWM, HandBack::FullSpeed);
        assert_eq!(outcome, HandBackOutcome::FullSpeed);
        assert_eq!(sysfs.get(EN), Some("1"));
        assert_eq!(sysfs.get(PWM), Some("255"));
    }

    #[test]
    fn a_header_that_accepts_nothing_is_reported_failed() {
        let mut sysfs = FakeSysfs::default()
            .with(EN, "1")
            .with(PWM, "80")
            .refusing(EN, "5")
            .refusing(EN, "0")
            .refusing(EN, "1");
        let outcome = hand_back(&mut sysfs, EN, PWM, HandBack::Mode(5));
        assert_eq!(outcome, HandBackOutcome::Failed);
        assert!(!outcome.released());
        assert_eq!(sysfs.get(PWM), Some("80"), "nothing may be half-written");
    }

    #[test]
    fn a_manual_original_gets_its_duty_back() {
        let mut sysfs = FakeSysfs::default().with(EN, "1").with(PWM, "255");
        let outcome = hand_back(&mut sysfs, EN, PWM, HandBack::Manual(120));
        assert_eq!(outcome, HandBackOutcome::Restored);
        assert_eq!(sysfs.get(EN), Some("1"));
        assert_eq!(sysfs.get(PWM), Some("120"));
    }

    /// DEC-326's alias: a driver that reports mode 0 at full scale is in the
    /// manual state we asked for, not refusing it.
    #[test]
    fn a_manual_original_at_full_scale_accepts_the_mode_0_alias() {
        let mut sysfs = FakeSysfs::default()
            .with(EN, "2")
            .with(PWM, "40")
            .storing_instead(EN, "1", "0");
        let outcome = hand_back(&mut sysfs, EN, PWM, HandBack::Manual(255));
        assert_eq!(outcome, HandBackOutcome::Restored);
    }

    fn header(id: &str, enable: Option<&str>) -> PwmHeaderDescriptor {
        PwmHeaderDescriptor {
            id: id.into(),
            chip_name: "it8696".into(),
            supports_enable: enable.is_some(),
            enable_path: enable.map(Into::into),
            pwm_path: format!("/sys/class/hwmon/hwmon3/{id}"),
            is_writable: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_first_original_is_kept_across_every_later_take() {
        let ledger = HandBackLedger::new(&[header("pwm2", Some(EN))]);
        assert!(ledger.needs_original("pwm2"));
        ledger.note_take("pwm2", Some(HandBack::Mode(5)));
        assert!(!ledger.needs_original("pwm2"));
        ledger.note_handed_back("pwm2");
        // A re-take after a firmware reclaim or a hand-back reads a state the
        // daemon or the firmware left; it must not replace the original.
        ledger.note_take("pwm2", Some(HandBack::Mode(2)));
        assert_eq!(
            ledger.taken_header("pwm2").map(|t| t.action),
            Some(HandBack::Mode(5))
        );
    }

    #[test]
    fn a_header_without_an_enable_file_is_never_tracked() {
        let ledger = HandBackLedger::new(&[header("pwm1", None)]);
        assert!(!ledger.needs_original("pwm1"));
        ledger.note_take("pwm1", Some(HandBack::Mode(2)));
        assert!(!ledger.is_taken("pwm1"));
        assert!(ledger.taken_ids().is_empty());
    }

    #[test]
    fn a_take_whose_read_failed_gets_the_fallback() {
        let ledger = HandBackLedger::new(&[header("pwm2", Some(EN))]);
        ledger.note_take("pwm2", None);
        assert_eq!(
            ledger.taken_header("pwm2").map(|t| t.action),
            Some(HandBack::FullSpeed)
        );
    }

    #[test]
    fn a_failed_hand_back_is_reported_once_until_one_succeeds() {
        let ledger = HandBackLedger::new(&[header("pwm2", Some(EN))]);
        ledger.note_take("pwm2", Some(HandBack::Mode(5)));
        assert!(ledger.note_hand_back_failed("pwm2"));
        assert!(!ledger.note_hand_back_failed("pwm2"));
        ledger.note_handed_back("pwm2");
        ledger.note_take("pwm2", None);
        assert!(ledger.note_hand_back_failed("pwm2"));
    }

    /// The record is the only thing `ExecStopPost` has after a crash, so it is
    /// asserted as the file it is — read back from disk, line by line — rather
    /// than through the renderer that produced it.
    #[test]
    fn the_record_names_exactly_the_headers_held_now() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join(RECORD_FILE_NAME);
        let en1 = "/sys/class/hwmon/hwmon3/pwm1_enable";
        let ledger = HandBackLedger::new(&[header("pwm1", Some(en1)), header("pwm2", Some(EN))]);
        ledger.set_record_path(record.clone());

        let body = |p: &Path| std::fs::read_to_string(p).unwrap();
        let lines = |p: &Path| -> Vec<String> {
            body(p)
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(String::from)
                .collect()
        };
        assert!(body(&record).starts_with("# control-ofc hwmon hand-back record v1"));
        assert!(
            lines(&record).is_empty(),
            "nothing is held before the first take"
        );

        ledger.note_take("pwm2", Some(HandBack::Mode(5)));
        ledger.note_take("pwm1", Some(HandBack::Manual(77)));
        assert_eq!(
            lines(&record),
            vec![
                format!("{en1}\t/sys/class/hwmon/hwmon3/pwm1\tmanual\t77"),
                format!("{EN}\t/sys/class/hwmon/hwmon3/pwm2\tmode\t5"),
            ]
        );

        ledger.note_handed_back("pwm1");
        assert_eq!(
            lines(&record),
            vec![format!("{EN}\t/sys/class/hwmon/hwmon3/pwm2\tmode\t5")]
        );
        assert!(
            !dir.path().join(format!("{RECORD_FILE_NAME}.tmp")).exists(),
            "the record is replaced in one step, never left half-written"
        );
    }

    #[test]
    fn an_unrecorded_take_is_written_as_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join(RECORD_FILE_NAME);
        let ledger = HandBackLedger::new(&[header("pwm2", Some(EN))]);
        ledger.set_record_path(record.clone());
        ledger.note_take("pwm2", None);
        let body = std::fs::read_to_string(&record).unwrap();
        assert!(
            body.lines()
                .any(|l| l == format!("{EN}\t/sys/class/hwmon/hwmon3/pwm2\tfull\t-")),
            "got {body:?}"
        );
    }

    #[test]
    fn try_taken_gives_up_on_a_held_lock_instead_of_waiting() {
        let ledger = std::sync::Arc::new(HandBackLedger::new(&[header("pwm2", Some(EN))]));
        ledger.note_take("pwm2", Some(HandBack::Mode(5)));
        let held = ledger.state.lock();
        let other = std::sync::Arc::clone(&ledger);
        let got = std::thread::spawn(move || other.try_taken(Duration::from_millis(50)))
            .join()
            .unwrap();
        drop(held);
        assert!(got.is_none());
        assert_eq!(
            ledger.try_taken(Duration::from_millis(50)).map(|t| t.len()),
            Some(1)
        );
    }
}
