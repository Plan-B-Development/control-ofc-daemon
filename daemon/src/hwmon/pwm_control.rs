//! Hwmon PWM control with lease enforcement and safety floors.
//!
//! Writes `pwmN` sysfs files after validating lease, bounds, and mode.
//! All writes go through this module — no direct sysfs access elsewhere.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Periodic INFO summary cadence for the pwm_enable watchdog log throttle.
/// First reclaim per header is logged at WARN, subsequent reverts at DEBUG,
/// and a single INFO line is emitted every `WATCHDOG_SUMMARY_INTERVAL`
/// reporting the delta and cumulative count. See `decide_watchdog_log_action`.
const WATCHDOG_SUMMARY_INTERVAL: Duration = Duration::from_secs(60);

use crate::error::HwmonError;
use crate::health::cache::StateCache;
use crate::health::state::{DutyReconciliation, HwmonFanState};
use crate::hwmon::handback::{HandBack, HandBackLedger, HandBackOutcome};
use crate::hwmon::lease::{HwmonWriter, LeaseError, LeaseManager};
use crate::hwmon::pwm_discovery::PwmHeaderDescriptor;

/// PWM enable mode: manual (1) allows direct PWM writes.
///
/// Standard hwmon pwmX_enable values:
///   0 = no control (fan at full speed)
///   1 = manual PWM control via sysfs (universally supported)
///   2 = automatic/thermal cruise (driver-specific)
///   3+ = driver-specific (e.g., NCT6775 Speed Cruise, Smart Fan III/IV)
///
/// The daemon TAKES a header by writing 1 (manual), which every driver supports.
/// Giving one back writes whatever the header reported before the first take —
/// never an assumed "automatic" value, because 2 means something different on
/// every driver family (`hwmon::handback`, DEC-382).
const PWM_ENABLE_MANUAL: &str = "1";

use crate::pwm::{percent_to_raw, raw_to_percent};

/// Result of a successful PWM write.
#[derive(Debug, Clone)]
pub struct HwmonSetPwmResult {
    pub header_id: String,
    pub pwm_percent: u8,
    pub raw_value: u8,
}

/// Errors from hwmon PWM control operations.
#[derive(Debug)]
pub enum HwmonControlError {
    /// Lease not held or invalid.
    Lease(LeaseError),
    /// Input validation failure.
    Validation(String),
    /// Hardware/sysfs write failure.
    Hardware(HwmonError),
    /// Refused because the shutdown hand-back has begun and owns the header
    /// (DEC-420, `PTR-s`). Transient by nature — the daemon is stopping — so it
    /// maps to a retryable `503 hardware_unavailable`, like the handlers' own
    /// shutdown refusals, and is never a `Lease` error the thermal force would
    /// answer by re-taking the lease and retrying.
    ShuttingDown(String),
}

impl std::fmt::Display for HwmonControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lease(e) => write!(f, "lease error: {e}"),
            Self::Validation(msg) => write!(f, "validation error: {msg}"),
            Self::Hardware(e) => write!(f, "hardware error: {e}"),
            Self::ShuttingDown(msg) => write!(f, "shutting down: {msg}"),
        }
    }
}

/// Trait for writing sysfs files (allows mocking in tests).
pub trait SysfsWriter: Send {
    fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError>;
    fn read_file(&self, path: &str) -> Result<String, HwmonError>;
}

/// Real sysfs writer that writes to the filesystem.
pub struct RealSysfsWriter;

impl SysfsWriter for RealSysfsWriter {
    fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
        std::fs::write(path, value).map_err(|e| HwmonError::WriteError {
            path: path.to_string(),
            message: e.to_string(),
        })
    }

    fn read_file(&self, path: &str) -> Result<String, HwmonError> {
        std::fs::read_to_string(path).map_err(|e| HwmonError::ReadError {
            path: path.to_string(),
            message: e.to_string(),
        })
    }
}

/// Per-header write state for coalescing identical writes.
#[derive(Debug, Default)]
struct HeaderWriteState {
    /// Last PWM percent successfully written to this header.
    last_commanded_pct: Option<u8>,
    /// Whether manual mode (pwm_enable=1) has been written during the current lease.
    manual_mode_set: bool,
    /// What `pwmN` read immediately after the last successful write to this
    /// header, as a percent — the duty the header actually took (DEC-406, S2-5).
    /// `None` when that read failed, or after a write that did not read back.
    ///
    /// The drift check compares against THIS, not the command, because a driver
    /// may legitimately hold a different value: `dell_smm` has three duty levels
    /// (a write of 40 % reads back 50 %), `thinkpad_acpi` eight, and a clamping
    /// chip raises a low duty. Against the command every such header would be
    /// "drifting" after every write; against what it took, only a change made
    /// after the write is.
    held_pct: Option<u8>,
    /// The engine's duty-drift episode for this header (DEC-406).
    drift: DriftState,
}

/// One header's duty-drift episode (DEC-406, `PTR-g`).
///
/// An EPISODE opens when a coalesced tick reads `pwmN` back further than
/// `READBACK_TOLERANCE_PCT` from the duty the header took after the daemon's
/// last write (`HeaderWriteState::held_pct`, or the command where that read
/// failed), and closes when a coalesced tick reads it back within tolerance. Inside one episode the daemon logs at
/// most one WARN for the first correction and one for giving up, however many
/// times a changed command restarts the corrections.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DriftState {
    /// Corrections written at the current command that the next tick found had
    /// not held. Reset by any other write (a changed command, a re-take).
    corrections: u8,
    /// Gave up: `DUTY_CORRECTION_ATTEMPTS` corrections in a row did not hold.
    not_holding: bool,
    /// An episode is open.
    in_episode: bool,
    /// The give-up WARN has been logged in this episode.
    give_up_logged: bool,
}

impl DriftState {
    /// A write that is not a correction — a changed command, a re-take after a
    /// reclaim, resume or thermal force. It is a fresh attempt at a fresh duty,
    /// so the give-up is left and the correction count restarts; the episode
    /// (and so its log lines) is not closed, because nothing has yet read the
    /// duty back and found it holding.
    fn after_plain_write(self) -> Self {
        Self {
            corrections: 0,
            not_holding: false,
            ..self
        }
    }
}

/// What a coalesced engine tick does about its readback (DEC-406).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriftAction {
    /// The duty holds, or could not be read: skip the write, as before DEC-406.
    Coalesce,
    /// The duty holds again after an episode: skip the write; the episode ends.
    Recovered { was_not_holding: bool },
    /// Rewrite the duty. `first` = the episode's first correction (WARN).
    Correct { first: bool },
    /// Stop correcting: skip the write and flag the header. `log` = the
    /// episode's first give-up (WARN).
    GiveUp { log: bool },
    /// Already given up and still disagreeing: skip the write.
    Hold,
}

/// Decide a coalesced tick from its readback (DEC-406). Pure, so the state
/// machine is tested on its own; `set_pwm` is its one caller and is tested
/// through the engine as well.
///
/// `expected_pct` is the duty the header took after the daemon's last write
/// (S2-5), not necessarily the command. `readback_pct` of `None` is UNKNOWN,
/// never a mismatch: nothing is written, counted or changed. The comparison is
/// the same tolerance characterisation uses.
fn reconcile_decision(
    expected_pct: u8,
    readback_pct: Option<u8>,
    state: DriftState,
) -> (DriftAction, DriftState) {
    let Some(read) = readback_pct else {
        return (DriftAction::Coalesce, state);
    };
    if read.abs_diff(expected_pct) <= crate::constants::READBACK_TOLERANCE_PCT {
        if state.in_episode {
            return (
                DriftAction::Recovered {
                    was_not_holding: state.not_holding || state.give_up_logged,
                },
                DriftState::default(),
            );
        }
        return (DriftAction::Coalesce, state);
    }
    if state.not_holding {
        return (DriftAction::Hold, state);
    }
    if state.corrections >= crate::constants::DUTY_CORRECTION_ATTEMPTS {
        return (
            DriftAction::GiveUp {
                log: !state.give_up_logged,
            },
            DriftState {
                not_holding: true,
                give_up_logged: true,
                ..state
            },
        );
    }
    (
        DriftAction::Correct {
            first: !state.in_episode,
        },
        DriftState {
            corrections: state.corrections + 1,
            in_episode: true,
            ..state
        },
    )
}

/// Per-header throttle state for the pwm_enable watchdog log.
#[derive(Debug, Default, Clone)]
struct WatchdogLogState {
    /// Whether the first reclaim has been logged at WARN.
    first_warn_emitted: bool,
    /// Time of the last emitted WARN/INFO log line for this header.
    last_emit_at: Option<Instant>,
    /// Cumulative reclaim count at the last emitted summary.
    count_at_last_summary: u64,
}

/// Decision returned by [`decide_watchdog_log_action`] — what (if anything)
/// the watchdog should log on a given reclaim event.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum WatchdogLogAction {
    /// First reclaim per header — emit at WARN so the operator notices.
    Warn,
    /// Subsequent reclaim within the summary interval — emit at DEBUG only.
    Debug,
    /// Summary interval elapsed — emit a single INFO with delta + cumulative.
    Summary { delta: u64, cumulative: u64 },
}

/// Decide what the watchdog should log for this reclaim event.
///
/// Pure function over the per-header [`WatchdogLogState`]; mutates the state in
/// place so the caller does not need to track the previous emission time. The
/// throttle schedule:
///
/// * First reclaim per header → [`WatchdogLogAction::Warn`].
/// * Reclaims within `summary_interval` of the last emission → [`Debug`].
/// * Once the interval has elapsed → a single [`Summary`] with the delta count
///   since the last summary and the running total.
///
/// The cumulative `enable_revert_counts` map is updated separately by the
/// caller — this function only decides log-line emission, never gates the
/// watchdog's remediation behaviour.
fn decide_watchdog_log_action(
    state: &mut WatchdogLogState,
    now: Instant,
    summary_interval: Duration,
    count: u64,
) -> WatchdogLogAction {
    if !state.first_warn_emitted {
        state.first_warn_emitted = true;
        state.last_emit_at = Some(now);
        state.count_at_last_summary = count;
        return WatchdogLogAction::Warn;
    }

    let due_for_summary = state
        .last_emit_at
        .is_some_and(|t| now.duration_since(t) >= summary_interval);

    if due_for_summary {
        let delta = count.saturating_sub(state.count_at_last_summary);
        state.last_emit_at = Some(now);
        state.count_at_last_summary = count;
        return WatchdogLogAction::Summary {
            delta,
            cumulative: count,
        };
    }

    WatchdogLogAction::Debug
}

/// Controller for hwmon PWM writes with lease enforcement and write verification.
pub struct HwmonPwmController {
    headers: HashMap<String, PwmHeaderDescriptor>,
    lease_manager: LeaseManager,
    writer: Box<dyn SysfsWriter>,
    cache: Arc<StateCache>,
    /// Per-header write state for coalescing (reset on lease release).
    write_state: HashMap<String, HeaderWriteState>,
    /// Cumulative BIOS pwm_enable reclaim events per header. Persists across leases.
    enable_revert_counts: HashMap<String, u64>,
    /// When the most recent reclaim was *counted*, per header (DEC-360).
    ///
    /// Deliberately NOT `WatchdogLogState::last_emit_at`, which is what the GUI
    /// register row originally proposed reusing. That field records the last
    /// *log emission*, and emission is throttled — first reclaim WARN, the rest
    /// DEBUG, one summary per interval — so it stops advancing on exactly the
    /// busy headers whose activity matters most, and a client ranking recency
    /// from it would read a grinding contention as stale.
    ///
    /// `Instant`, not a wall clock: it is monotonic, and it shares a lifetime
    /// with `enable_revert_counts` (both live on the controller and both reset
    /// together when it is rebuilt), so an age derived from it can never
    /// describe a different daemon's counts.
    enable_revert_last_at: HashMap<String, Instant>,
    /// Per-header log throttle state for the pwm_enable watchdog. Persists
    /// across leases so the first WARN is emitted at most once per controller
    /// lifetime per header — subsequent reverts collapse into periodic INFO
    /// summaries instead of one WARN per second.
    watchdog_log_state: HashMap<String, WatchdogLogState>,
    /// Cumulative count of PWM verify-after-write mismatches per header.
    /// Persists across leases. A non-zero value here means the daemon wrote
    /// PWM=N but read back PWM≠N — a strong signal of BIOS/EC interference,
    /// clamping, or a concurrent in-process writer.
    verify_mismatch_counts: HashMap<String, u64>,
    /// Duty corrections the engine has written per header since the daemon
    /// started (DEC-406): coalesced ticks whose readback disagreed with the
    /// command, so the duty was rewritten. Persists across leases, like
    /// `verify_mismatch_counts`, and is published on `/fans` and `/poll`.
    duty_corrections: HashMap<String, u32>,
    /// Which headers this controller has taken from firmware, and what each one
    /// gets back (DEC-382). Shared, not owned: the shutdown restore and the panic
    /// hook read it without this controller's mutex, which a wedged sysfs write
    /// can hold for good.
    handback: Arc<HandBackLedger>,
    /// Headers with NO `pwmN_enable` this controller has written, and the duty
    /// each last took (DEC-388, `TS-y`); `None` = written, but whether the last
    /// write landed is unknown.
    ///
    /// Such a header has no mode to take or give back, so DEC-382's ledger does
    /// not track it — yet its duty register holds the daemon's last write after
    /// exit. The exit floor reads this. Deliberately NOT `write_state`, which
    /// `on_lease_released` clears on every profile deactivation: a header the
    /// deactivated profile left at 20 % would then be invisible at the stop that
    /// followed while still running at 20 %.
    exit_record: HashMap<String, Option<u8>>,
    /// The lowest duty each header with no `pwmN_enable` may be written at from
    /// now on (DEC-392, `TS-ar`): latched by [`Self::apply_exit_floor`] and never
    /// cleared — the hwmon half of `FanController`'s `exit_min`.
    ///
    /// The floor holds this controller's lock for its whole step, but the engine
    /// takes it per header, so the floor can run between two headers of a final
    /// batch that outlived the drains — and the rest of that batch then lands
    /// after it, a lower duty on a header the floor had already raised.
    /// [`Self::set_pwm`] raises any such command to this, under the same
    /// mutex the floor ran under, so there is no window between a check and a
    /// write. Raising is never refused: a forced 100 % still lands.
    exit_min: HashMap<String, u8>,
}

/// One header's exit-floor write (DEC-388), for the caller's report.
#[derive(Debug)]
pub struct HwmonExitFloorWrite {
    pub header_id: String,
    /// What the controller last knew the header held; `None` = unknown.
    pub was_pct: Option<u8>,
    pub target_pct: u8,
    /// `None` when the header already held the target, so nothing was written.
    pub result: Option<Result<(), HwmonError>>,
}

impl HwmonPwmController {
    pub fn new(
        headers: Vec<PwmHeaderDescriptor>,
        lease_manager: LeaseManager,
        writer: Box<dyn SysfsWriter>,
        cache: Arc<StateCache>,
    ) -> Self {
        let handback = Arc::new(HandBackLedger::new(&headers));
        let header_map: HashMap<String, PwmHeaderDescriptor> =
            headers.into_iter().map(|h| (h.id.clone(), h)).collect();

        Self {
            headers: header_map,
            lease_manager,
            writer,
            cache,
            write_state: HashMap::new(),
            enable_revert_counts: HashMap::new(),
            enable_revert_last_at: HashMap::new(),
            watchdog_log_state: HashMap::new(),
            verify_mismatch_counts: HashMap::new(),
            duty_corrections: HashMap::new(),
            handback,
            exit_record: HashMap::new(),
            exit_min: HashMap::new(),
        }
    }

    /// The exit floor for headers with no `pwmN_enable` (DEC-388, `TS-y`): leave
    /// each one this controller has written at `max(its last duty, floor_pct)`,
    /// or at 100 % where it no longer knows that duty. The same rule as the
    /// OpenFan channels (`FanController::apply_exit_floor`), because it is the
    /// same situation: nothing for the firmware to take back. A header this
    /// controller never wrote is left alone, and a `floor_pct` of 0 turns the
    /// step off.
    ///
    /// Headers WITH `pwmN_enable` are not touched here — DEC-382 hands them back
    /// to the mode they had. Written straight through the writer, without the
    /// lease: this is the shutdown, and the engine that held the lease is gone.
    ///
    /// It also LATCHES the floor ([`Self::exit_min`], DEC-392): each header with
    /// no mode switch at its exit duty if this controller wrote it, at
    /// `floor_pct` if not. From then on `set_pwm` raises any lower command for
    /// such a header to that — an engine batch that outlived the drains, or a
    /// verify's restore — so the duty the stop logs is the duty it leaves.
    pub fn apply_exit_floor(&mut self, floor_pct: u8) -> Vec<HwmonExitFloorWrite> {
        if floor_pct == 0 {
            return Vec::new();
        }
        for h in self.headers.values().filter(|h| !h.supports_enable) {
            let min = match self.exit_record.get(&h.id) {
                Some(was) => crate::pwm::exit_duty(*was, floor_pct),
                None => floor_pct,
            };
            self.exit_min.insert(h.id.clone(), min);
        }
        let mut ids: Vec<String> = self.exit_record.keys().cloned().collect();
        ids.sort();
        let mut out = Vec::with_capacity(ids.len());
        for header_id in ids {
            let Some(pwm_path) = self.headers.get(&header_id).map(|h| h.pwm_path.clone()) else {
                continue;
            };
            let was_pct = self.exit_record.get(&header_id).copied().flatten();
            let target_pct = crate::pwm::exit_duty(was_pct, floor_pct);
            let result = if was_pct == Some(target_pct) {
                None
            } else {
                let written = self
                    .writer
                    .write_file(&pwm_path, &percent_to_raw(target_pct).to_string());
                self.exit_record
                    .insert(header_id.clone(), written.is_ok().then_some(target_pct));
                // Keep the coalesce truthful: once the floor has landed, a late
                // command the latch raises to the same duty writes nothing.
                if let Some(ws) = self.write_state.get_mut(&header_id) {
                    if written.is_ok() {
                        ws.last_commanded_pct = Some(target_pct);
                        // DEC-406: this write was not read back, so the drift
                        // check falls back to comparing with the floor's duty.
                        ws.held_pct = None;
                    } else {
                        // [SAFETY] `TS-au` (DEC-420): a failed floor write may
                        // still have landed, or left the header at a failed
                        // earlier write's duty, so the next command must be
                        // written, not coalesced against `last_commanded_pct` —
                        // which stays, as in `set_pwm`.
                        ws.manual_mode_set = false;
                    }
                }
                Some(written)
            };
            out.push(HwmonExitFloorWrite {
                header_id,
                was_pct,
                target_pct,
                result,
            });
        }
        out
    }

    /// The hand-back ledger (DEC-382). Clone the `Arc` before wrapping the
    /// controller in its mutex for anything that must reach it without the lock.
    pub fn handback(&self) -> &Arc<HandBackLedger> {
        &self.handback
    }

    /// Give `header_id` back to what it was doing before the daemon first took
    /// it (DEC-382). Requires a valid lease, like every other write.
    ///
    /// `Ok(None)` when the daemon does not hold the header: there is nothing to
    /// give back, and nothing is written.
    pub fn hand_back(
        &mut self,
        header_id: &str,
        lease_id: &str,
    ) -> Result<Option<HandBackOutcome>, HwmonControlError> {
        self.lease_manager
            .validate_lease(lease_id)
            .map_err(HwmonControlError::Lease)?;
        let Some(taken) = self.handback.taken_header(header_id) else {
            return Ok(None);
        };
        let outcome = crate::hwmon::handback::hand_back(
            &mut *self.writer,
            &taken.enable_path,
            &taken.pwm_path,
            taken.action,
        );
        if outcome.released() {
            self.handback.note_handed_back(header_id);
            // [SAFETY] Forget this header's write state. The watchdog in
            // `set_pwm` reads any mode other than 1 on a header it believes it set
            // as a firmware reclaim, so keeping `manual_mode_set` would count the
            // daemon's own hand-back as one the next time something takes the
            // header — the obstacle that withdrew `D1-m`. It also makes that next
            // take re-assert `pwm_enable=1` from a clean slate.
            self.write_state.remove(header_id);
            self.cache.clear_hwmon_commanded(header_id);
            // DEC-406: a header nothing commands is not "not holding" anything.
            self.cache.clear_hwmon_duty_not_holding([header_id]);
        } else {
            // `PTA-k`: a FAILED hand-back leaves the header taken, but nothing
            // commands it any more either, so the same rule applies to its drift:
            // the episode closes and the flag goes. Before this only the released
            // arm obeyed it, so a header whose hand-back kept failing went on
            // publishing `duty_not_holding` — blaming corrections for what was a
            // failing hand-back — and a profile naming it again at the same duty
            // met a stale give-up and held instead of correcting.
            //
            // `last_commanded_pct` and `manual_mode_set` stay: the next tick's
            // retry of the hand-back and `TS-p`'s hold both read them.
            if let Some(ws) = self.write_state.get_mut(header_id) {
                ws.drift = DriftState::default();
            }
            self.cache.clear_hwmon_duty_not_holding([header_id]);
        }
        Ok(Some(outcome))
    }

    /// Cumulative PWM verify-after-write mismatch events per header.
    /// A non-zero value indicates that a write of N was followed by a read
    /// of !N — likely BIOS clamping, EC interference, or a concurrent
    /// in-process writer. Persists across leases.
    pub fn verify_mismatch_counts(&self) -> &HashMap<String, u64> {
        &self.verify_mismatch_counts
    }

    /// The header ids the thermal force actually drives — every **writable**
    /// header, in `headers()` order.
    ///
    /// [SAFETY] `OFN-ad`, DEC-372; `OFN-ah`/`OFN-ak`, DEC-376. This is the ONE
    /// definition of "a hwmon output this daemon can drive", and it now has
    /// four readers, all of which must agree or the daemon lies about itself:
    /// `HwmonBackend::force_all_with_floor` (the set it actually writes),
    /// `HwmonBackend::new` (which refuses to build a backend when this is empty,
    /// so the engine's `hwmon_be.is_some()` means what its readers assume),
    /// `HwmonBackend::delivery_targets` (the per-MEMBER deliverability that
    /// lists a control bound only to headers outside this set, `OFN-al` — the
    /// same set `new` measured, so it cannot disagree with the force), and
    /// `capabilities_handler` (`devices.hwmon.write_support` and
    /// `features.hwmon_write_supported`). Derived, never restated —
    /// DEC-334's "one flag, one gating shape".
    ///
    /// Safe to call once at construction and cache the emptiness of:
    /// `is_writable` is read from the sysfs permission bit at discovery
    /// (`pwm_discovery.rs`) and never recomputed — `hwmon_rescan_handler` does
    /// not replace a running controller.
    pub fn forced_target_ids(&self) -> Vec<String> {
        self.headers()
            .iter()
            .filter(|h| h.is_writable)
            .map(|h| h.id.clone())
            .collect()
    }

    /// Get the list of discovered PWM headers.
    pub fn headers(&self) -> Vec<&PwmHeaderDescriptor> {
        let mut headers: Vec<_> = self.headers.values().collect();
        headers.sort_by_key(|h| (&h.chip_name, h.pwm_index));
        headers
    }

    /// Look up a single header by id — O(1) against the internal map.
    ///
    /// Prefer this over scanning `headers()` (which allocates and sorts a
    /// Vec on every call) on per-write hot paths: the profile-engine tick
    /// writes to a header once per fan per second (DEC-146 P3-13).
    pub fn header(&self, id: &str) -> Option<&PwmHeaderDescriptor> {
        self.headers.get(id)
    }

    /// Get the lease manager (for take/release operations).
    pub fn lease_manager_mut(&mut self) -> &mut LeaseManager {
        &mut self.lease_manager
    }

    /// Get the lease manager (read-only).
    pub fn lease_manager(&self) -> &LeaseManager {
        &self.lease_manager
    }

    /// Cumulative BIOS pwm_enable reclaim events per header (persists across leases).
    pub fn enable_revert_counts(&self) -> &HashMap<String, u64> {
        &self.enable_revert_counts
    }

    /// Age in milliseconds of the most recent counted reclaim, per header.
    ///
    /// Computed at read time because `Instant` is not serialisable and an
    /// absolute daemon-local timestamp would be meaningless to a client anyway.
    /// A header present in `enable_revert_counts` but absent here can only mean
    /// a count recorded before this field existed, which cannot happen within
    /// one process — so the client may treat a missing entry as "unknown age",
    /// never as "just now".
    pub fn enable_revert_ages_ms(&self, now: Instant) -> HashMap<String, u64> {
        self.enable_revert_last_at
            .iter()
            .map(|(id, at)| {
                (
                    id.clone(),
                    now.saturating_duration_since(*at).as_millis() as u64,
                )
            })
            .collect()
    }

    /// The duty this controller last COMMANDED for a header, as a percent
    /// (AIO-MB Phase 5). `None` if it has never written to it.
    ///
    /// This is the authoritative command value, and the reason a validation
    /// sample reads it here rather than from the state cache: the cache's
    /// `last_commanded_pwm` is overwritten at 1 Hz by the poll's sysfs readback
    /// (AIO5-a), so for an uncontrolled header it reports the readback and for a
    /// controlled one it reports whichever producer wrote last. This field has
    /// exactly one producer.
    pub fn last_commanded_pct(&self, header_id: &str) -> Option<u8> {
        self.write_state
            .get(header_id)
            .and_then(|ws| ws.last_commanded_pct)
    }

    /// Set PWM on a header. Requires a valid lease.
    ///
    /// Includes a pwm_enable watchdog: on every call where manual_mode_set is
    /// true, reads back pwm_enable to detect BIOS/EC reclaim (SmartFan, etc.).
    /// If reclaimed, re-writes pwm_enable=1 and forces a full PWM re-write.
    pub fn set_pwm(
        &mut self,
        header_id: &str,
        pwm_percent: u8,
        lease_id: &str,
    ) -> Result<HwmonSetPwmResult, HwmonControlError> {
        // Validate lease
        self.lease_manager
            .validate_lease(lease_id)
            .map_err(HwmonControlError::Lease)?;
        // DEC-406: only the ENGINE's writes are reconciled — profile curves and
        // overrides, including under a thermal-safety lease the engine adopted.
        // A diagnostic writes under a `Verify` lease and must get exactly the
        // duty it asked for, with no correction counted against the header.
        // Derived from the lease this call was validated against, not from a
        // parameter a caller could forget to pass.
        let reconciles = self
            .lease_manager
            .active_lease()
            .is_some_and(|l| l.lease_id == lease_id && l.owner != HwmonWriter::Verify);

        // Check for system resume — reset all manual mode flags
        if self.cache.take_resume_flag() {
            log::info!("Clearing manual mode flags after system resume");
            for ws in self.write_state.values_mut() {
                ws.manual_mode_set = false;
            }
        }

        // Look up header — extract needed fields to avoid cloning the full descriptor.
        let (pwm_path, enable_path, supports_enable, rpm_path, chip_name) = {
            let h = self.headers.get(header_id).ok_or_else(|| {
                HwmonControlError::Validation(format!("unknown header: {header_id}"))
            })?;
            (
                h.pwm_path.clone(),
                h.enable_path.clone(),
                h.supports_enable,
                h.rpm_path.clone(),
                h.chip_name.clone(),
            )
        };

        // [SAFETY] `PTR-s` (DEC-420): the headers the shutdown hand-back gives
        // back are exactly the ledger's — those with a mode switch.
        let owned_by_hand_back = supports_enable && enable_path.is_some();
        self.refuse_after_hand_back(header_id, owned_by_hand_back)?;

        // Validate PWM range
        if pwm_percent > 100 {
            return Err(HwmonControlError::Validation(format!(
                "pwm_percent {pwm_percent} out of range (0–100)"
            )));
        }

        // DEC-392 (`TS-ar`): once the stop's exit floor has run, a command may
        // raise a header with no mode switch but never take it below the duty
        // the floor left it at — raised BEFORE the coalesce check, so a lower
        // command against a header already at its exit duty writes nothing.
        let effective_pct = match self.exit_min.get(header_id) {
            Some(&min) if pwm_percent < min => {
                log::info!(
                    "hwmon {header_id}: {pwm_percent} % raised to {min} % — the exit floor \
                     has already run"
                );
                min
            }
            _ => pwm_percent,
        };

        // ── pwm_enable watchdog ─────────────────────────────────────
        // When we believe manual mode is already set, read back pwm_enable
        // to detect BIOS/EC reclaim (Gigabyte SmartFan, MSI Smart Fan, etc.).
        //
        // [HOST-a / DEC-326] `enable != 1` is NOT sufficient on its own. Some
        // drivers synthesise `enable == 0` ("fan at full speed") from the duty
        // register rather than reporting the mode we set, so a wholly
        // successful write of 100% reads back as a reclaim. Confirm it against
        // the duty before believing it — see `pwm::is_full_speed_alias` for the
        // kernel condition and why this carries no chip table.
        //
        // The duty read is deliberately inside the `!= 1` arm: the common path
        // (mode still 1) costs exactly what it did before, and the extra read
        // happens only where we were about to declare a reclaim anyway.
        let enable_reclaimed = if supports_enable {
            // The watchdog runs BEFORE this call's write, so the duty register
            // still holds the PREVIOUS command — compare against that, never
            // against `effective_pct`. Reading `effective_pct` here would
            // declare a false reclaim on the first descending tick after a
            // 100% one, which is the very defect this guard exists to remove.
            let (mode_set, last_pct) =
                self.write_state.get(header_id).map_or((false, None), |ws| {
                    (ws.manual_mode_set, ws.last_commanded_pct)
                });
            if mode_set {
                let enable_read = enable_path.as_ref().and_then(|ep| {
                    self.writer
                        .read_file(ep)
                        .ok()
                        .and_then(|s| s.trim().parse::<u8>().ok())
                });
                match (enable_read, last_pct) {
                    (Some(1) | None, _) => false,
                    (Some(mode), Some(last)) => {
                        let duty_pct = self
                            .writer
                            .read_file(&pwm_path)
                            .ok()
                            .and_then(|s| s.trim().parse::<u8>().ok())
                            .map(raw_to_percent);
                        !crate::pwm::is_full_speed_alias(last, duty_pct, Some(mode))
                    }
                    // Manual mode set but nothing commanded yet: no duty of ours
                    // to confirm against, so the mode reading stands on its own.
                    (Some(_), None) => true,
                }
            } else {
                false
            }
        } else {
            false
        };

        if enable_reclaimed {
            *self
                .enable_revert_counts
                .entry(header_id.to_string())
                .or_insert(0) += 1;
            // Stamped on every counted reclaim, beside the count it dates and
            // before any throttling decision — the two must not diverge.
            self.enable_revert_last_at
                .insert(header_id.to_string(), Instant::now());
            let count = self
                .enable_revert_counts
                .get(header_id)
                .copied()
                .unwrap_or(0);

            // Throttle log emission: first reclaim WARN, subsequent DEBUG,
            // single INFO summary every WATCHDOG_SUMMARY_INTERVAL. The
            // cumulative count above is unaffected by throttling — it is the
            // canonical figure surfaced via /diagnostics/hardware.
            let log_state = self
                .watchdog_log_state
                .entry(header_id.to_string())
                .or_default();
            let action = decide_watchdog_log_action(
                log_state,
                Instant::now(),
                WATCHDOG_SUMMARY_INTERVAL,
                count,
            );
            match action {
                WatchdogLogAction::Warn => {
                    log::warn!(
                        "pwm_enable for '{header_id}' reclaimed by BIOS (count: {count}); \
                         daemon watchdog is restoring manual mode. \
                         Subsequent reverts logged at DEBUG; INFO summary every {}s.",
                        WATCHDOG_SUMMARY_INTERVAL.as_secs(),
                    );
                }
                WatchdogLogAction::Summary { delta, cumulative } => {
                    log::info!(
                        "pwm_enable for '{header_id}' reclaimed {delta} time(s) in last {}s \
                         (cumulative: {cumulative}); watchdog still restoring manual mode.",
                        WATCHDOG_SUMMARY_INTERVAL.as_secs(),
                    );
                }
                WatchdogLogAction::Debug => {
                    log::debug!("pwm_enable for '{header_id}' reclaimed by BIOS (count: {count})");
                }
            }
        }

        let ws = self.write_state.entry(header_id.to_string()).or_default();
        if enable_reclaimed {
            ws.manual_mode_set = false;
        }

        // Coalesce: skip if same as last commanded value and mode still set —
        // unless the readback says the duty did not hold (DEC-406), in which
        // case this falls through and writes it again as a correction.
        //
        // `PTA-j`: only the ENGINE's writes coalesce. A diagnostic's write is a
        // measurement, and one that repeats the engine's last duty is exactly the
        // one that finds a second writer holding the header elsewhere — skipping
        // it recorded that writer's RPM under the duty the diagnostic asked for.
        // It is written unconditionally, and (being non-reconciling) counts and
        // publishes nothing below.
        let mode_set = ws.manual_mode_set;
        let coalescible = reconciles && mode_set && ws.last_commanded_pct == Some(effective_pct);
        let correcting = coalescible
            && reconciles
            && self.duty_drift_rewrite(header_id, effective_pct, &pwm_path);
        if coalescible && !correcting {
            let now = Instant::now();
            let rpm = rpm_path.as_ref().and_then(|p| {
                self.writer
                    .read_file(p)
                    .ok()
                    .and_then(|s| s.trim().parse::<u16>().ok())
            });
            self.cache.update_hwmon_fans(vec![HwmonFanState {
                id: header_id.to_string(),
                rpm,
                last_commanded_pwm: Some(effective_pct),
                // See the write path below — `None` so the poll's readback
                // survives this refresh.
                pwm_readback_pct: None,
                pwm_commanded_pct: Some(effective_pct),
                updated_at: now,
                alarm: None,
                pwm_enable_mode: None,
            }]);
            return Ok(HwmonSetPwmResult {
                header_id: header_id.to_string(),
                pwm_percent: effective_pct,
                raw_value: percent_to_raw(effective_pct),
            });
        }

        // Write pwm_enable if not yet set (or if BIOS reclaimed it).
        if !mode_set && supports_enable {
            if let Some(ref ep) = enable_path {
                // [SAFETY] DEC-382: this write is the TAKE. Read what the header
                // was doing BEFORE it — on the first take only; every later one
                // starts from state the daemon or the firmware left — and record
                // the take before switching, so a process that dies between the
                // two leaves a record that replays as a no-op instead of a
                // manual-mode header nothing will give back.
                let original = if self.handback.needs_original(header_id) {
                    Some(HandBack::from_reading(
                        &chip_name,
                        crate::hwmon::handback::read_u8(&*self.writer, ep),
                        crate::hwmon::handback::read_u8(&*self.writer, &pwm_path),
                    ))
                } else {
                    None
                };
                // Again here: the reads above can wedge across the hand-back.
                // BEFORE `note_take`, so a refused take adds nothing to the record
                // ExecStopPost replays (DEC-420 review, SR-2).
                self.refuse_after_hand_back(header_id, owned_by_hand_back)?;
                self.handback.note_take(header_id, original);
                self.writer
                    .write_file(ep, PWM_ENABLE_MANUAL)
                    .map_err(HwmonControlError::Hardware)?;
            }
        }

        // Write PWM value
        let raw = percent_to_raw(effective_pct);
        if !supports_enable {
            // DEC-388 (`TS-y`): recorded before the write, as unknown — from here
            // the header may hold our duty whether or not the write reports
            // success — and confirmed below once it has.
            self.exit_record.insert(header_id.to_string(), None);
        }
        // And immediately before the duty write: the watchdog's and the drift
        // check's reads, and the enable write, can all wedge across the hand-back.
        if let Err(refused) = self.refuse_after_hand_back(header_id, owned_by_hand_back) {
            // A refused correction never landed, so — as for a failed write
            // below — it must not count toward DEC-406's give-up (SR-1).
            if correcting {
                if let Some(ws) = self.write_state.get_mut(header_id) {
                    ws.drift.corrections = ws.drift.corrections.saturating_sub(1);
                }
            }
            return Err(refused);
        }
        if let Err(e) = self.writer.write_file(&pwm_path, &raw.to_string()) {
            if let Some(ws) = self.write_state.get_mut(header_id) {
                // [SAFETY] `TS-au` (DEC-420): a failed write may still have
                // landed, so the header may hold this duty rather than the last
                // one that succeeded. Clearing the mode flag makes the next
                // command write instead of coalescing against
                // `last_commanded_pct` — at the cost of one `pwm_enable=1`
                // re-assert on a header with a mode switch. `last_commanded_pct`
                // itself stays: the thermal force floors a held header against
                // it, and wiping it is `TS-p`'s defect (DEC-386).
                ws.manual_mode_set = false;
                // DEC-406: a correction that never landed is not one that "did
                // not hold", so it must not count toward the give-up — or three
                // EIOs would publish `duty_not_holding` beside
                // `duty_corrections: 0`. The next tick re-takes the header as a
                // plain write, which restarts the count (DEC-420 accepts that a
                // header whose writes fail intermittently reaches the give-up
                // later); the failure reaches the engine's own throttled
                // write-failure log.
                if correcting {
                    ws.drift.corrections = ws.drift.corrections.saturating_sub(1);
                }
            }
            return Err(HwmonControlError::Hardware(e));
        }
        if !supports_enable {
            self.exit_record
                .insert(header_id.to_string(), Some(effective_pct));
        }

        // Verify write: read back and compare (best-effort). On mismatch,
        // increment a per-header counter so the discrepancy is observable
        // beyond the log line (DEC log signal can be lost under throttling).
        let mut held_pct = None;
        match self.writer.read_file(&pwm_path) {
            Ok(raw_str) => {
                if let Ok(actual_raw) = raw_str.trim().parse::<u8>() {
                    held_pct = Some(raw_to_percent(actual_raw));
                    if actual_raw != raw {
                        *self
                            .verify_mismatch_counts
                            .entry(header_id.to_string())
                            .or_insert(0) += 1;
                        log::warn!(
                            "PWM write verification mismatch for '{}': wrote {} ({}%), read back {} ({}%)",
                            header_id, raw, effective_pct, actual_raw, raw_to_percent(actual_raw)
                        );
                    }
                }
            }
            Err(e) => {
                log::debug!(
                    "PWM write verification readback failed for '{}': {}",
                    header_id,
                    e
                );
            }
        }

        // Update coalescing state
        let ws = self.write_state.entry(header_id.to_string()).or_default();
        ws.last_commanded_pct = Some(effective_pct);
        ws.manual_mode_set = true;
        ws.held_pct = held_pct;
        // DEC-406: a correction is counted once it has been written; any other
        // engine write restarts the corrections (`DriftState::after_plain_write`).
        // A diagnostic's write touches neither.
        if correcting {
            *self
                .duty_corrections
                .entry(header_id.to_string())
                .or_insert(0) += 1;
        } else if reconciles {
            ws.drift = ws.drift.after_plain_write();
        }
        if reconciles {
            self.publish_duty_reconciliation(header_id);
        }

        // Update cache with commanded value
        let now = Instant::now();
        let rpm = rpm_path.as_ref().and_then(|p| {
            self.writer
                .read_file(p)
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok())
        });
        self.cache.update_hwmon_fans(vec![HwmonFanState {
            id: header_id.to_string(),
            rpm,
            last_commanded_pwm: Some(effective_pct),
            // `None`, not `Some(effective_pct)`: this is the COMMAND, and the
            // readback is whatever sysfs says it became. The cache merge carries
            // the poll's answer forward across this refresh (AIO-MB Phase 5).
            pwm_readback_pct: None,
            pwm_commanded_pct: Some(effective_pct),
            updated_at: now,
            alarm: None,
            pwm_enable_mode: None,
        }]);

        Ok(HwmonSetPwmResult {
            header_id: header_id.to_string(),
            pwm_percent: effective_pct,
            raw_value: raw,
        })
    }

    /// [SAFETY] `PTR-s` (DEC-420). Once the shutdown hand-back has begun, refuse
    /// any write to a header it owns: the hand-back is the last writer, and a
    /// write landing after it re-takes the header — through the reclaim
    /// watchdog on one given back to a firmware mode, or by moving the duty of
    /// one given back in manual — with nothing left to drive it. Before
    /// DEC-406 such a late write moved the duty only when its command changed;
    /// DEC-406's readback made a same-command one rewrite it too, which is the
    /// widening `PTR-s` recorded.
    ///
    /// [`HwmonControlError::ShuttingDown`], deliberately never `Lease`: the
    /// thermal force answers a lease error by force-taking the lease and
    /// retrying, which would only meet this again; and a verify still in flight
    /// when the daemon stops gets the retryable `503` the handlers' own shutdown
    /// refusals give, not a `400` (DEC-420 review). A header with no mode switch
    /// is never refused — the hand-back does not reach it, and DEC-392's
    /// exit-floor latch relies on a late write to it still landing, raised.
    fn refuse_after_hand_back(
        &self,
        header_id: &str,
        owned_by_hand_back: bool,
    ) -> Result<(), HwmonControlError> {
        if owned_by_hand_back && self.handback.shutdown_hand_back_begun() {
            return Err(HwmonControlError::ShuttingDown(format!(
                "hwmon {header_id}: the shutdown hand-back has begun, and it owns this header"
            )));
        }
        Ok(())
    }

    /// Called when a lease is released. Resets coalescing state so the next
    /// lease holder gets a fresh pwm_enable write on their first set_pwm().
    pub fn on_lease_released(&mut self) {
        // DEC-406: the drift episodes go with the write state; their flags must
        // not outlive it on the wire. The correction counts are since boot.
        self.cache
            .clear_hwmon_duty_not_holding(self.write_state.keys().map(String::as_str));
        self.write_state.clear();
    }

    /// Duty corrections the engine has written per header since the daemon
    /// started (DEC-406).
    pub fn duty_corrections(&self) -> &HashMap<String, u32> {
        &self.duty_corrections
    }

    /// Whether the engine has given up correcting `header_id`'s duty (DEC-406).
    pub fn duty_not_holding(&self, header_id: &str) -> bool {
        self.write_state
            .get(header_id)
            .is_some_and(|ws| ws.drift.not_holding)
    }

    /// Read `pwm_path` back on a coalesced engine tick and decide whether to
    /// write the duty again (DEC-406, `PTR-g`). `true` = write it: the caller
    /// falls through to the normal write path, which counts it.
    ///
    /// Before DEC-406 a coalesced tick skipped the write without looking, so a
    /// second writer's duty stood for as long as the curve output was steady —
    /// 55 s against a commanded 46 %/40 % on 2026-09-08, and that header could
    /// be a pump. The thermal force is unaffected either way: it clears
    /// `manual_mode_set` first (`forget_manual_mode`), so it never coalesces.
    ///
    /// The read is direct, under the lock `set_pwm` already holds, beside the
    /// `fanN_input` read the coalesce refresh already makes — not the poll's
    /// cached readback, which can be up to a poll interval older than the write
    /// it would be judging.
    fn duty_drift_rewrite(&mut self, header_id: &str, commanded_pct: u8, pwm_path: &str) -> bool {
        let readback_pct = self
            .writer
            .read_file(pwm_path)
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())
            .map(raw_to_percent);
        let Some(ws) = self.write_state.get_mut(header_id) else {
            return false;
        };
        let expected_pct = ws.held_pct.unwrap_or(commanded_pct);
        let (action, next) = reconcile_decision(expected_pct, readback_pct, ws.drift);
        let changed = next != ws.drift;
        ws.drift = next;
        let read = readback_pct.map_or_else(|| "?".to_string(), |p| p.to_string());
        let attempts = crate::constants::DUTY_CORRECTION_ATTEMPTS;
        match action {
            DriftAction::Coalesce | DriftAction::Hold => {}
            DriftAction::Correct { first: true } => log::warn!(
                "hwmon {header_id}: duty reads {read} % but held {expected_pct} % after the \
                 engine's last write (commanded {commanded_pct} %) — rewriting it. Something \
                 else may be writing this header \
                 (a vendor tool, a script, firmware); further corrections are logged at DEBUG"
            ),
            DriftAction::Correct { first: false } => log::debug!(
                "hwmon {header_id}: duty reads {read} %, held {expected_pct} % — rewriting"
            ),
            DriftAction::GiveUp { log: true } => log::warn!(
                "hwmon {header_id}: {attempts} duty corrections in a row did not hold (reads \
                 {read} %, held {expected_pct} % after a write; commanded {commanded_pct} %) — \
                 no longer rewriting it until the \
                 command changes or the duty holds again (duty_not_holding)"
            ),
            DriftAction::GiveUp { log: false } => log::debug!(
                "hwmon {header_id}: duty still not holding at {commanded_pct} % (reads {read} %)"
            ),
            DriftAction::Recovered {
                was_not_holding: true,
            } => log::info!(
                "hwmon {header_id}: duty holds at {read} % again — the episode that set \
                 duty_not_holding is over"
            ),
            DriftAction::Recovered {
                was_not_holding: false,
            } => log::debug!("hwmon {header_id}: corrected duty holds at {read} %"),
        }
        if changed {
            self.publish_duty_reconciliation(header_id);
        }
        matches!(action, DriftAction::Correct { .. })
    }

    /// Publish `header_id`'s reconciliation record to the cache (DEC-406).
    fn publish_duty_reconciliation(&self, header_id: &str) {
        self.cache.set_hwmon_duty_reconciliation(
            header_id,
            DutyReconciliation {
                corrections: self.duty_corrections.get(header_id).copied().unwrap_or(0),
                not_holding: self.duty_not_holding(header_id),
            },
        );
    }

    /// Reset every header's `manual_mode_set` — and ONLY that — so the next write
    /// re-asserts `pwm_enable=1` (Audit P1-E), while each `last_commanded_pct`
    /// survives.
    ///
    /// [SAFETY] `TS-p` / DEC-386. The thermal force used `on_lease_released`,
    /// which also wiped `last_commanded_pct` — the one record of what a skipped
    /// control's header is running at, which the force floors against every tick.
    /// It came back only when that tick's own write succeeded, so a single failed
    /// write, or a verify that force-took the lease mid-scan, left no record and
    /// the next forced tick wrote the bare floor. Keeping the duty is safe for
    /// the two readers of the pair: coalescing and the reclaim watchdog both
    /// require `manual_mode_set`, which this clears.
    pub fn forget_manual_mode(&mut self) {
        for ws in self.write_state.values_mut() {
            ws.manual_mode_set = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::lease::{HwmonWriter, LeaseManager};
    use parking_lot::Mutex;
    use std::collections::HashMap as StdHashMap;
    use std::time::Duration;

    type WriteLog = Arc<Mutex<Vec<(String, String)>>>;

    /// Mock sysfs writer that records writes and provides canned reads.
    struct MockSysfsWriter {
        writes: WriteLog,
        files: StdHashMap<String, String>,
    }

    impl MockSysfsWriter {
        fn new() -> (Self, WriteLog) {
            let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    writes: writes.clone(),
                    files: StdHashMap::new(),
                },
                writes,
            )
        }

        fn with_file(mut self, path: &str, value: &str) -> Self {
            self.files.insert(path.to_string(), value.to_string());
            self
        }
    }

    impl SysfsWriter for MockSysfsWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes
                .lock()
                .push((path.to_string(), value.to_string()));
            Ok(())
        }

        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            self.files.get(path).cloned().ok_or(HwmonError::ReadError {
                path: path.to_string(),
                message: "not found".to_string(),
            })
        }
    }

    fn make_header(id: &str, label: &str, min_pwm: u8) -> PwmHeaderDescriptor {
        PwmHeaderDescriptor {
            id: id.to_string(),
            label: label.to_string(),
            chip_name: "it8696".to_string(),
            device_id: "it87.2624".to_string(),
            pwm_index: 1,
            supports_enable: true,
            pwm_path: "/sys/class/hwmon/hwmon0/pwm1".to_string(),
            enable_path: Some("/sys/class/hwmon/hwmon0/pwm1_enable".to_string()),
            rpm_available: true,
            rpm_path: Some("/sys/class/hwmon/hwmon0/fan1_input".to_string()),
            min_pwm_percent: min_pwm,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: None,
            is_aio: false,
            role: crate::hwmon::roles::HeaderRole::Unknown,
            role_source: crate::hwmon::roles::RoleSource::None,
            ..Default::default()
        }
    }

    fn setup_controller(
        headers: Vec<PwmHeaderDescriptor>,
    ) -> (HwmonPwmController, WriteLog, Arc<StateCache>) {
        let cache = Arc::new(StateCache::new());
        let (writer, writes) = MockSysfsWriter::new();
        let writer = writer.with_file("/sys/class/hwmon/hwmon0/fan1_input", "1200\n");
        let lease_mgr = LeaseManager::new();
        let ctrl = HwmonPwmController::new(headers, lease_mgr, Box::new(writer), cache.clone());
        (ctrl, writes, cache)
    }

    #[test]
    fn header_lookup_matches_headers_scan() {
        // DEC-146 P3-13: the O(1) accessor must agree with the sorted scan
        // and return None for unknown ids.
        let (ctrl, _writes, _cache) = setup_controller(vec![
            make_header("h1", "CHA_FAN1", 20),
            make_header("h2", "CHA_FAN2", 20),
        ]);
        assert_eq!(
            ctrl.header("h1").map(|h| h.label.as_str()),
            Some("CHA_FAN1")
        );
        assert_eq!(
            ctrl.header("h2").map(|h| h.label.as_str()),
            Some("CHA_FAN2")
        );
        assert!(ctrl.header("missing").is_none());
    }

    // ── DEC-388 (`TS-y`): the exit floor for headers with no mode switch ──

    /// A header with `pwmN` but no `pwmN_enable` — nothing for firmware to take
    /// back, so the exit floor is what reaches it.
    fn no_mode_header(id: &str, n: u8) -> PwmHeaderDescriptor {
        PwmHeaderDescriptor {
            supports_enable: false,
            enable_path: None,
            pwm_path: format!("/sys/class/hwmon/hwmon0/pwm{n}"),
            pwm_index: n,
            ..make_header(id, id, 0)
        }
    }

    fn engine_lease(ctrl: &mut HwmonPwmController) -> String {
        ctrl.lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap()
            .lease_id
    }

    /// [SAFETY] A no-mode header the daemon left below the floor is raised to it
    /// on stop; one above the floor is not rewritten; one it never wrote is left
    /// alone.
    #[test]
    fn the_exit_floor_raises_a_header_with_no_mode_switch() {
        let (mut ctrl, writes, _cache) = setup_controller(vec![
            no_mode_header("low", 2),
            no_mode_header("high", 3),
            no_mode_header("never", 4),
        ]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("low", 30, &lease).unwrap();
        ctrl.set_pwm("high", 80, &lease).unwrap();
        let before = writes.lock().len();

        let out = ctrl.apply_exit_floor(50);

        let summary: Vec<_> = out
            .iter()
            .map(|w| {
                (
                    w.header_id.as_str(),
                    w.was_pct,
                    w.target_pct,
                    w.result.is_some(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [("high", Some(80), 80, false), ("low", Some(30), 50, true)]
        );
        let new_writes = writes.lock()[before..].to_vec();
        assert_eq!(
            new_writes,
            [(
                "/sys/class/hwmon/hwmon0/pwm2".to_string(),
                percent_to_raw(50).to_string()
            )]
        );
    }

    /// A header WITH `pwmN_enable` belongs to DEC-382's hand-back, which gives it
    /// the mode it had; the exit floor must not write a duty over it.
    #[test]
    fn a_header_with_a_mode_switch_is_left_to_the_hand_back() {
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h1", 30, &lease).unwrap();
        let before = writes.lock().len();

        assert!(ctrl.apply_exit_floor(50).is_empty());
        assert_eq!(writes.lock().len(), before);
    }

    /// [SAFETY] The record survives the lease release that a profile
    /// deactivation performs — `write_state` does not — so a header the
    /// deactivated profile left at 20 % is still raised at the stop.
    #[test]
    fn the_exit_record_survives_a_profile_deactivation() {
        let (mut ctrl, writes, _cache) = setup_controller(vec![no_mode_header("h", 2)]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h", 20, &lease).unwrap();
        ctrl.on_lease_released();
        let before = writes.lock().len();

        let out = ctrl.apply_exit_floor(50);

        assert_eq!(
            out.len(),
            1,
            "precondition: the header is still on the record"
        );
        assert_eq!((out[0].was_pct, out[0].target_pct), (Some(20), 50));
        assert_eq!(writes.lock().len(), before + 1);
    }

    /// [SAFETY] A write that failed leaves the duty unknown, and an unknown duty
    /// leaves at full speed.
    #[test]
    fn a_failed_write_leaves_the_exit_duty_at_full_speed() {
        let (mut ctrl, writes, _cache) = setup_scripted_controller(
            vec![no_mode_header("h", 2)],
            vec![Err(HwmonError::WriteError {
                path: "/sys/class/hwmon/hwmon0/pwm2".to_string(),
                message: "EIO".to_string(),
            })],
        );
        let lease = engine_lease(&mut ctrl);
        assert!(
            ctrl.set_pwm("h", 30, &lease).is_err(),
            "precondition: the write failed"
        );

        let out = ctrl.apply_exit_floor(50);

        assert_eq!(out.len(), 1);
        assert_eq!((out[0].was_pct, out[0].target_pct), (None, 100));
        assert_eq!(
            writes.lock().last().map(|(_, v)| v.clone()),
            Some(percent_to_raw(100).to_string())
        );
    }

    #[test]
    fn a_zero_exit_floor_leaves_every_header_alone() {
        let (mut ctrl, writes, _cache) = setup_controller(vec![no_mode_header("h", 2)]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h", 20, &lease).unwrap();
        let before = writes.lock().len();
        assert!(ctrl.apply_exit_floor(0).is_empty());
        assert_eq!(writes.lock().len(), before);
    }

    // ── DEC-392 (`TS-ar`): the exit floor latches ──

    /// [SAFETY] Nothing that runs after the exit floor can lower a header with
    /// no mode switch: an engine batch that outlived the drains, or a verify's
    /// restore, writes through `set_pwm`. A header the floor raised is held at
    /// its exit duty, one first written after the floor at the floor itself, and
    /// a command above either still lands.
    #[test]
    fn nothing_after_the_exit_floor_can_lower_a_header_with_no_mode_switch() {
        let (mut ctrl, writes, _cache) =
            setup_controller(vec![no_mode_header("low", 2), no_mode_header("never", 3)]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("low", 30, &lease).unwrap();
        let out = ctrl.apply_exit_floor(50);
        assert_eq!(
            (out.len(), out[0].target_pct),
            (1, 50),
            "precondition: the floor raised the header it had written"
        );
        let before = writes.lock().len();

        let lower = ctrl.set_pwm("low", 20, &lease).unwrap();
        assert_eq!(lower.pwm_percent, 50);
        assert_eq!(
            writes.lock().len(),
            before,
            "a lower command against a header at its exit duty writes nothing"
        );
        assert_eq!(ctrl.last_commanded_pct("low"), Some(50));

        let unwritten = ctrl.set_pwm("never", 10, &lease).unwrap();
        assert_eq!(
            unwritten.pwm_percent, 50,
            "a header first written after the floor is raised to the floor"
        );
        assert_eq!(
            writes.lock().last().cloned(),
            Some((
                "/sys/class/hwmon/hwmon0/pwm3".to_string(),
                percent_to_raw(50).to_string()
            ))
        );

        let higher = ctrl.set_pwm("low", 80, &lease).unwrap();
        assert_eq!(higher.pwm_percent, 80);
        assert_eq!(
            writes.lock().last().cloned(),
            Some((
                "/sys/class/hwmon/hwmon0/pwm2".to_string(),
                percent_to_raw(80).to_string()
            ))
        );
    }

    /// [SAFETY] A header whose duty the floor could not vouch for was left at
    /// full speed, and the latch holds it there.
    #[test]
    fn an_unknown_duty_latches_at_full_speed() {
        let (mut ctrl, writes, _cache) = setup_scripted_controller(
            vec![no_mode_header("h", 2)],
            vec![Err(HwmonError::WriteError {
                path: "/sys/class/hwmon/hwmon0/pwm2".to_string(),
                message: "EIO".to_string(),
            })],
        );
        let lease = engine_lease(&mut ctrl);
        assert!(ctrl.set_pwm("h", 30, &lease).is_err(), "precondition");
        assert_eq!(ctrl.apply_exit_floor(50)[0].target_pct, 100, "precondition");

        let before = writes.lock().len();
        assert_eq!(ctrl.set_pwm("h", 30, &lease).unwrap().pwm_percent, 100);
        // The COUNT, not `writes.last()`, which the floor's own 100 satisfies
        // whether or not the late write happens (`F2`, retired by DEC-420).
        let writes = writes.lock();
        assert_eq!(writes.len(), before + 1, "the late write landed");
        assert_eq!(writes[before].1, percent_to_raw(100).to_string());
    }

    /// [SAFETY] `TS-au` (DEC-420), the row's own scenario. A no-mode header last
    /// written at 100, a failed write of 30 and a failed floor write: the
    /// header may be at 30, below the floor, so the late write the latch raises
    /// to 100 must be WRITTEN, not coalesced against the 100 that was last
    /// written successfully. Asserted by the write count — the last write is
    /// 100 whether or not the late one happens (the `F2` weakness of
    /// `an_unknown_duty_latches_at_full_speed`). The mock has no `pwmN` to
    /// read back, which is the unreadable-register case DEC-406's correction
    /// cannot see.
    #[test]
    fn a_failed_write_then_a_failed_floor_leaves_the_late_write_to_land() {
        let eio = || {
            Err(HwmonError::WriteError {
                path: "/sys/class/hwmon/hwmon0/pwm2".to_string(),
                message: "EIO".to_string(),
            })
        };
        let (mut ctrl, writes, _cache) =
            setup_scripted_controller(vec![no_mode_header("h", 2)], vec![Ok(()), eio(), eio()]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h", 100, &lease).unwrap();
        assert!(ctrl.set_pwm("h", 30, &lease).is_err(), "precondition");
        let floor = ctrl.apply_exit_floor(50);
        assert_eq!(floor[0].target_pct, 100, "precondition: an unknown duty");
        assert!(
            matches!(floor[0].result, Some(Err(_))),
            "precondition: the floor's own write failed"
        );
        let before = writes.lock().len();

        assert_eq!(ctrl.set_pwm("h", 20, &lease).unwrap().pwm_percent, 100);
        let writes = writes.lock();
        assert_eq!(
            writes.len(),
            before + 1,
            "the latch's late write must land: the header may still hold the failed 30"
        );
        assert_eq!(writes[before].1, percent_to_raw(100).to_string());
    }

    /// [SAFETY] `TS-au` (DEC-420) in normal control: `60` ok, `30`
    /// failed-but-possibly-landed, then `60` again must be written, on a header
    /// with a mode switch (which re-asserts `pwm_enable=1`) and on one without.
    #[test]
    fn after_a_failed_duty_write_the_same_command_is_written_again() {
        let eio = || {
            Err(HwmonError::WriteError {
                path: "pwm".to_string(),
                message: "EIO".to_string(),
            })
        };
        // enable + pwm, then the failed pwm.
        let (mut ctrl, writes, _cache) = setup_scripted_controller(
            vec![make_header("h1", "CHA_FAN1", 0)],
            vec![Ok(()), Ok(()), eio()],
        );
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h1", 60, &lease).unwrap();
        assert!(ctrl.set_pwm("h1", 30, &lease).is_err(), "precondition");
        let before = writes.lock().len();
        ctrl.set_pwm("h1", 60, &lease).unwrap();
        assert_eq!(
            writes.lock()[before..].to_vec(),
            vec![
                (
                    "/sys/class/hwmon/hwmon0/pwm1_enable".to_string(),
                    PWM_ENABLE_MANUAL.to_string()
                ),
                (
                    "/sys/class/hwmon/hwmon0/pwm1".to_string(),
                    percent_to_raw(60).to_string()
                ),
            ],
            "re-taken and rewritten, not coalesced"
        );
        assert_eq!(
            ctrl.last_commanded_pct("h1"),
            Some(60),
            "the failed write never replaced the last successful duty"
        );

        let (mut ctrl, writes, _cache) =
            setup_scripted_controller(vec![no_mode_header("h", 2)], vec![Ok(()), eio()]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h", 60, &lease).unwrap();
        assert!(ctrl.set_pwm("h", 30, &lease).is_err(), "precondition");
        let before = writes.lock().len();
        ctrl.set_pwm("h", 60, &lease).unwrap();
        assert_eq!(writes.lock().len(), before + 1, "no-mode header rewritten");
    }

    /// A writer whose read of one path starts the shutdown hand-back — a read
    /// that wedged in the kernel and returned after `hand_back_hwmon` began.
    /// Writes go through to `files` (and are logged), so a readback sees them;
    /// a test changes a file to model another writer.
    struct HandBackRaceWriter {
        writes: WriteLog,
        files: RaceFiles,
        ledger: Arc<Mutex<Option<Arc<crate::hwmon::handback::HandBackLedger>>>>,
        trip_on: Arc<Mutex<Option<String>>>,
    }

    impl SysfsWriter for HandBackRaceWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes
                .lock()
                .push((path.to_string(), value.to_string()));
            self.files
                .lock()
                .insert(path.to_string(), format!("{value}\n"));
            Ok(())
        }

        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            let mut trip = self.trip_on.lock();
            if trip.as_deref() == Some(path) {
                *trip = None;
                if let Some(l) = self.ledger.lock().as_ref() {
                    l.begin_shutdown_hand_back();
                }
            }
            self.files
                .lock()
                .get(path)
                .cloned()
                .ok_or(HwmonError::ReadError {
                    path: path.to_string(),
                    message: "not found".to_string(),
                })
        }
    }

    type TripOn = Arc<Mutex<Option<String>>>;
    type RaceFiles = Arc<Mutex<StdHashMap<String, String>>>;

    fn setup_race_controller(
        headers: Vec<PwmHeaderDescriptor>,
        files: &[(&str, &str)],
    ) -> (HwmonPwmController, WriteLog, TripOn, RaceFiles) {
        let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
        let ledger = Arc::new(Mutex::new(None));
        let trip_on: TripOn = Arc::new(Mutex::new(None));
        let files: RaceFiles = Arc::new(Mutex::new(
            files
                .iter()
                .map(|(p, v)| (p.to_string(), v.to_string()))
                .collect(),
        ));
        let writer = HandBackRaceWriter {
            writes: writes.clone(),
            files: files.clone(),
            ledger: ledger.clone(),
            trip_on: trip_on.clone(),
        };
        let ctrl = HwmonPwmController::new(
            headers,
            LeaseManager::new(),
            Box::new(writer),
            Arc::new(StateCache::new()),
        );
        *ledger.lock() = Some(ctrl.handback().clone());
        (ctrl, writes, trip_on, files)
    }

    const EN1: &str = "/sys/class/hwmon/hwmon0/pwm1_enable";
    const PWM1: &str = "/sys/class/hwmon/hwmon0/pwm1";

    /// [SAFETY] `PTR-s` (DEC-420), the entry check. After the hand-back gave
    /// `h1` back to mode 5, a late engine write — here the same command, the
    /// case DEC-406 made rewrite — is refused before it reads anything, so the
    /// hand-back's own mode is not counted as a firmware reclaim and nothing is
    /// written. The refusal is `ShuttingDown` (a retryable 503 at the API),
    /// never `Lease`. A header with no mode switch is not the hand-back's, and a
    /// late write to it still lands (DEC-392's latch depends on that).
    #[test]
    fn once_the_hand_back_has_begun_a_late_write_is_refused() {
        let (mut ctrl, writes, _trip, files) = setup_race_controller(
            vec![make_header("h1", "CHA_FAN1", 0), no_mode_header("h", 2)],
            &[(EN1, "5\n")],
        );
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h1", 60, &lease).unwrap();
        ctrl.set_pwm("h", 60, &lease).unwrap();
        // The hand-back gives h1 its recorded mode 5, then marks itself begun.
        files.lock().insert(EN1.to_string(), "5\n".to_string());
        ctrl.handback().begin_shutdown_hand_back();
        let before = writes.lock().len();

        let err = ctrl.set_pwm("h1", 60, &lease).unwrap_err();
        assert!(
            matches!(err, HwmonControlError::ShuttingDown(ref m) if m.contains("hand-back")),
            "refused as shutting down, never a lease error: {err}"
        );
        assert_eq!(writes.lock().len(), before, "nothing written to h1");
        assert!(
            ctrl.enable_revert_counts().is_empty(),
            "the hand-back's own mode must not be counted as a reclaim"
        );

        assert_eq!(ctrl.set_pwm("h", 70, &lease).unwrap().pwm_percent, 70);
        assert_eq!(
            writes.lock().len(),
            before + 1,
            "the no-mode header is written"
        );
    }

    /// [SAFETY] `PTR-s`, the check before the enable write: the take's reads of
    /// the original mode wedge across the hand-back's start, so the take must
    /// not switch the header to manual after it — nor add it to the record
    /// ExecStopPost replays (SR-2: the refusal precedes `note_take`).
    #[test]
    fn a_hand_back_begun_during_the_take_refuses_the_enable_write() {
        let (mut ctrl, writes, trip, _files) =
            setup_race_controller(vec![make_header("h1", "CHA_FAN1", 0)], &[(EN1, "5\n")]);
        let lease = engine_lease(&mut ctrl);
        *trip.lock() = Some(EN1.to_string());

        assert!(ctrl.set_pwm("h1", 60, &lease).is_err());
        assert!(
            trip.lock().is_none(),
            "precondition: the take read the mode"
        );
        assert!(writes.lock().is_empty(), "no enable write, no duty write");
        assert!(
            !ctrl.handback().is_taken("h1"),
            "a refused take must not be recorded as taken"
        );
    }

    /// [SAFETY] `PTR-s`, the check before the duty write: the watchdog's read
    /// of the mode wedges across the hand-back's start, so the duty write that
    /// would follow it must not land.
    #[test]
    fn a_hand_back_begun_during_the_watchdog_read_refuses_the_duty_write() {
        let (mut ctrl, writes, trip, _files) =
            setup_race_controller(vec![make_header("h1", "CHA_FAN1", 0)], &[(EN1, "1\n")]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h1", 60, &lease).unwrap();
        let before = writes.lock().len();
        *trip.lock() = Some(EN1.to_string());

        assert!(ctrl.set_pwm("h1", 40, &lease).is_err());
        assert!(
            trip.lock().is_none(),
            "precondition: the watchdog read the mode"
        );
        assert_eq!(writes.lock().len(), before, "the duty write must not land");
    }

    /// [SAFETY] SR-1 (DEC-420 review): a DEC-406 correction refused because the
    /// hand-back began during its readback never landed, so — like one whose
    /// write fails — it is not counted toward the give-up.
    #[test]
    fn a_correction_refused_by_the_hand_back_is_not_counted() {
        let (mut ctrl, writes, trip, files) =
            setup_race_controller(vec![make_header("h1", "CHA_FAN1", 0)], &[(EN1, "1\n")]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        // Another writer moves the duty; the next tick's readback of it is
        // where the hand-back begins.
        files
            .lock()
            .insert(PWM1.to_string(), format!("{}\n", percent_to_raw(60)));
        *trip.lock() = Some(PWM1.to_string());
        let before = writes.lock().len();

        assert!(ctrl.set_pwm("h1", 40, &lease).is_err());
        assert!(
            trip.lock().is_none(),
            "precondition: the drift readback ran"
        );
        assert!(
            ctrl.write_state["h1"].drift.in_episode,
            "precondition: the readback judged it a correction"
        );
        assert_eq!(writes.lock().len(), before, "precondition: refused");
        assert_eq!(
            ctrl.write_state["h1"].drift.corrections, 0,
            "a refused correction must not count toward the give-up"
        );
    }

    /// The latch is the no-mode headers' only: a header WITH `pwmN_enable` is
    /// DEC-382's hand-back, and a zero floor latches nothing at all.
    #[test]
    fn the_latch_leaves_mode_switch_headers_and_a_zero_floor_alone() {
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);
        let lease = engine_lease(&mut ctrl);
        ctrl.apply_exit_floor(50);
        assert_eq!(ctrl.set_pwm("h1", 20, &lease).unwrap().pwm_percent, 20);

        let (mut ctrl, _writes, _cache) = setup_controller(vec![no_mode_header("h", 2)]);
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h", 30, &lease).unwrap();
        ctrl.apply_exit_floor(0);
        assert_eq!(ctrl.set_pwm("h", 20, &lease).unwrap().pwm_percent, 20);
    }

    #[test]
    fn set_pwm_requires_lease() {
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let err = ctrl.set_pwm("h1", 50, "no-lease").unwrap_err();
        match err {
            HwmonControlError::Lease(_) => {}
            _ => panic!("expected lease error"),
        }
    }

    #[test]
    fn set_pwm_with_valid_lease() {
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let result = ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();

        assert_eq!(result.header_id, "h1");
        assert_eq!(result.pwm_percent, 50);
        assert_eq!(result.raw_value, percent_to_raw(50));

        let writes = writes.lock();
        // First write: pwm_enable → manual, then pwm value
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].1, "1"); // manual mode
        assert_eq!(writes[1].1, percent_to_raw(50).to_string());
    }

    #[test]
    fn set_pwm_writes_enable_once_per_lease() {
        // Manual mode is set on first write per lease, then skipped (coalescing)
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();
        ctrl.set_pwm("h1", 75, &lease.lease_id).unwrap();

        let writes = writes.lock();
        // enable(1) + pwm(50) + pwm(75) = 3 writes total (enable only on first call)
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0].1, "1"); // enable on first call
        assert_eq!(writes[1].1, percent_to_raw(50).to_string()); // pwm 50
        assert_eq!(writes[2].1, percent_to_raw(75).to_string()); // pwm 75 (no enable)
    }

    #[test]
    fn set_pwm_accepts_low_values_no_floor() {
        // No floor clamping — thermal safety handled by ThermalSafetyRule
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let result = ctrl.set_pwm("h1", 10, &lease.lease_id).unwrap();

        assert_eq!(result.pwm_percent, 10); // no clamping
    }

    #[test]
    fn set_pwm_cpu_header_allows_zero() {
        // CPU headers no longer have special floor — safety is centralized
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CPU_FAN", 0)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let result = ctrl.set_pwm("h1", 0, &lease.lease_id).unwrap();
        assert_eq!(result.pwm_percent, 0);
    }

    #[test]
    fn set_pwm_chassis_allows_zero() {
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let result = ctrl.set_pwm("h1", 0, &lease.lease_id).unwrap();
        assert_eq!(result.pwm_percent, 0);
    }

    #[test]
    fn set_pwm_unknown_header() {
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let err = ctrl
            .set_pwm("nonexistent", 50, &lease.lease_id)
            .unwrap_err();

        match err {
            HwmonControlError::Validation(msg) => {
                assert!(msg.contains("unknown header"));
            }
            _ => panic!("expected validation error"),
        }
    }

    #[test]
    fn set_pwm_invalid_percent() {
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let err = ctrl.set_pwm("h1", 200, &lease.lease_id).unwrap_err();

        match err {
            HwmonControlError::Validation(msg) => {
                assert!(msg.contains("out of range"));
            }
            _ => panic!("expected validation error"),
        }
    }

    /// T2 (test-tests audit): boundary pair for the `pwm_percent > 100` guard.
    /// Catches `>` ↔ `>=` mutations. 100 must be accepted (hardware allows
    /// full speed); 101 must be rejected. Far-outside values (200, etc.) are
    /// already covered by `set_pwm_invalid_percent` but don't distinguish
    /// strict vs non-strict comparison.
    #[test]
    fn set_pwm_boundary_100_accepted_101_rejected() {
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);
        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let lid = lease.lease_id.clone();

        // 100 — exact boundary, must succeed (full speed is a legal value).
        let ok_result = ctrl
            .set_pwm("h1", 100, &lid)
            .expect("pwm_percent=100 must be accepted");
        assert_eq!(ok_result.pwm_percent, 100);

        // 101 — one past the boundary, must be rejected with validation error.
        let err = ctrl
            .set_pwm("h1", 101, &lid)
            .expect_err("pwm_percent=101 must be rejected");
        match err {
            HwmonControlError::Validation(msg) => {
                assert!(
                    msg.contains("out of range"),
                    "expected 'out of range' in error message, got: {msg}",
                );
                assert!(
                    msg.contains("101"),
                    "error message should mention the offending value, got: {msg}",
                );
            }
            other => panic!("expected Validation error for 101, got: {other:?}"),
        }
    }

    #[test]
    fn set_pwm_updates_cache() {
        let (mut ctrl, _writes, cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 75, &lease.lease_id).unwrap();

        let snap = cache.snapshot();
        let fan = snap.hwmon_fans.get("h1").unwrap();
        assert_eq!(fan.last_commanded_pwm, Some(75));
        assert_eq!(fan.rpm, Some(1200));
    }

    #[test]
    fn set_pwm_with_expired_lease() {
        let cache = Arc::new(StateCache::new());
        let (writer, _writes) = MockSysfsWriter::new();
        let lease_mgr = LeaseManager::with_ttl(Duration::from_millis(1));
        let headers = vec![make_header("h1", "CHA_FAN1", 20)];
        let mut ctrl = HwmonPwmController::new(headers, lease_mgr, Box::new(writer), cache);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let id = lease.lease_id.clone();

        std::thread::sleep(Duration::from_millis(5));

        let err = ctrl.set_pwm("h1", 50, &id).unwrap_err();
        match err {
            HwmonControlError::Lease(_) => {}
            _ => panic!("expected lease error"),
        }
    }

    #[test]
    fn expired_lease_mid_batch_rejects_remaining_writes() {
        // If a lease expires between two writes in a batch, the second write
        // should fail with a lease error (not silently succeed).
        let cache = Arc::new(StateCache::new());
        let (writer, _writes) = MockSysfsWriter::new();
        let lease_mgr = LeaseManager::with_ttl(Duration::from_millis(1));
        let headers = vec![
            make_header("h1", "CHA_FAN1", 20),
            make_header("h2", "CHA_FAN2", 20),
        ];
        let mut ctrl = HwmonPwmController::new(headers, lease_mgr, Box::new(writer), cache);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let lid = lease.lease_id.clone();

        // First write succeeds (lease still valid)
        ctrl.set_pwm("h1", 50, &lid).unwrap();

        // Wait for lease to expire
        std::thread::sleep(Duration::from_millis(5));

        // Second write should fail — lease expired
        let err = ctrl.set_pwm("h2", 60, &lid).unwrap_err();
        match err {
            HwmonControlError::Lease(_) => {}
            _ => panic!("expected lease error on expired lease, got: {err:?}"),
        }
    }

    #[test]
    fn on_lease_released_resets_manual_mode() {
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let lease_id = lease.lease_id.clone();
        ctrl.set_pwm("h1", 50, &lease_id).unwrap();

        // Release the lease properly, then reset coalescing state
        ctrl.lease_manager_mut().release_lease(&lease_id).unwrap();
        ctrl.on_lease_released();

        // Take new lease and write again — should set enable mode again
        let lease2 = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Verify)
            .unwrap();
        ctrl.set_pwm("h1", 60, &lease2.lease_id).unwrap();

        let writes = writes.lock();
        // First lease: enable + pwm50. Second lease: enable + pwm60.
        assert_eq!(writes.len(), 4);
        assert_eq!(writes[0].1, "1"); // first enable
        assert_eq!(writes[2].1, "1"); // second enable after lease reset
    }

    #[test]
    fn headers_returns_sorted_list() {
        let h1 = make_header("h2", "CHA_FAN2", 20);
        let mut h2 = make_header("h1", "CHA_FAN1", 20);
        h2.pwm_index = 2;

        let (ctrl, _writes, _cache) = setup_controller(vec![h1, h2]);
        let headers = ctrl.headers();
        assert_eq!(headers.len(), 2);
        // Sorted by (chip_name, pwm_index)
        assert_eq!(headers[0].pwm_index, 1);
        assert_eq!(headers[1].pwm_index, 2);
    }

    #[test]
    fn set_pwm_updates_cache_with_commanded_value() {
        let cache = Arc::new(StateCache::new());
        let (mut ctrl, _writes, _) =
            setup_controller_with_cache(vec![make_header("h1", "CHA_FAN1", 0)], cache.clone());

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .expect("take lease");
        ctrl.set_pwm("h1", 75, &lease.lease_id).unwrap();

        let snap = cache.snapshot();
        let fan = snap.hwmon_fans.get("h1").expect("fan in cache");
        assert_eq!(fan.last_commanded_pwm, Some(75));
    }

    fn setup_controller_with_cache(
        headers: Vec<PwmHeaderDescriptor>,
        cache: Arc<StateCache>,
    ) -> (HwmonPwmController, WriteLog, Arc<StateCache>) {
        let (mock, writes) = MockSysfsWriter::new();
        let ctrl =
            HwmonPwmController::new(headers, LeaseManager::new(), Box::new(mock), cache.clone());
        (ctrl, writes, cache)
    }

    #[test]
    fn set_pwm_coalesces_identical_value() {
        // Two identical set_pwm calls → second produces zero sysfs writes
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap(); // identical

        let writes = writes.lock();
        // enable(1) + pwm(50) = 2 writes total; second call coalesced
        assert_eq!(writes.len(), 2);
    }

    #[test]
    fn set_pwm_coalescing_allows_different_value() {
        // Different value after coalesced call → only PWM written (enable skipped)
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap(); // coalesced
        ctrl.set_pwm("h1", 75, &lease.lease_id).unwrap(); // different

        let writes = writes.lock();
        // enable(1) + pwm(50) + pwm(75) = 3 writes total
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[2].1, percent_to_raw(75).to_string());
    }

    #[test]
    fn set_pwm_coalescing_is_exact_equality_not_delta() {
        // B2: a one-unit delta (50 → 51) must NOT coalesce — the guard is
        // `== Some(effective_pct)`, exact equality, NOT a delta tolerance. If the
        // GPU 5% rule (or any `abs_diff <= N`) leaked onto the hwmon path, 51 would
        // coalesce and drop this to 2 writes.
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap(); // enable(1) + pwm(50)
        ctrl.set_pwm("h1", 51, &lease.lease_id).unwrap(); // delta 1 → pwm(51), enable skipped

        let writes = writes.lock();
        // enable(1) + pwm(50) + pwm(51) = 3 writes; the second set_pwm went through.
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[2].1, percent_to_raw(51).to_string());
    }

    #[test]
    fn on_lease_released_resets_coalescing() {
        // After lease release + new lease → enable written on first call again
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let lid = lease.lease_id.clone();
        ctrl.set_pwm("h1", 50, &lid).unwrap(); // enable + pwm

        ctrl.lease_manager_mut().release_lease(&lid).unwrap();
        ctrl.on_lease_released();

        let lease2 = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Verify)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease2.lease_id).unwrap(); // same value, new lease

        let writes = writes.lock();
        // First lease: enable + pwm50
        // Second lease: enable + pwm50 (coalescing reset by lease release)
        assert_eq!(writes.len(), 4);
        assert_eq!(writes[0].1, "1"); // first enable
        assert_eq!(writes[2].1, "1"); // second enable after lease reset
    }

    #[test]
    fn set_pwm_coalesced_still_updates_cache() {
        // Even when coalesced, cache should be refreshed (staleness tracking)
        let cache = Arc::new(StateCache::new());
        let (mut ctrl, _writes, _) =
            setup_controller_with_cache(vec![make_header("h1", "CHA_FAN1", 0)], cache.clone());

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .expect("take lease");
        ctrl.set_pwm("h1", 60, &lease.lease_id).unwrap();
        let snap1 = cache.snapshot();
        let t1 = snap1.hwmon_fans.get("h1").unwrap().updated_at;

        std::thread::sleep(Duration::from_millis(2));

        ctrl.set_pwm("h1", 60, &lease.lease_id).unwrap(); // coalesced
        let snap2 = cache.snapshot();
        let t2 = snap2.hwmon_fans.get("h1").unwrap().updated_at;
        assert_eq!(
            snap2.hwmon_fans.get("h1").unwrap().last_commanded_pwm,
            Some(60)
        );
        assert!(t2 > t1, "cache timestamp should advance on coalesced write");
    }

    // ── Sysfs write failure tests (T1 audit finding) ───────────────

    /// Mock sysfs writer with scripted outcomes per write_file() call.
    /// Each call consumes the next result from the script; reads use a map.
    struct ScriptedSysfsWriter {
        write_results: std::cell::RefCell<Vec<Result<(), HwmonError>>>,
        writes: WriteLog,
        files: StdHashMap<String, String>,
    }

    impl ScriptedSysfsWriter {
        fn new(write_results: Vec<Result<(), HwmonError>>) -> (Self, WriteLog) {
            let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
            // Reverse so we can pop from the end (FIFO via pop from reversed vec)
            let mut results = write_results;
            results.reverse();
            (
                Self {
                    write_results: std::cell::RefCell::new(results),
                    writes: writes.clone(),
                    files: StdHashMap::new(),
                },
                writes,
            )
        }

        fn with_file(mut self, path: &str, value: &str) -> Self {
            self.files.insert(path.to_string(), value.to_string());
            self
        }
    }

    impl SysfsWriter for ScriptedSysfsWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes
                .lock()
                .push((path.to_string(), value.to_string()));
            let result = self.write_results.borrow_mut().pop().unwrap_or(Ok(()));
            result
        }

        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            self.files.get(path).cloned().ok_or(HwmonError::ReadError {
                path: path.to_string(),
                message: "not found".to_string(),
            })
        }
    }

    fn setup_scripted_controller(
        headers: Vec<PwmHeaderDescriptor>,
        write_results: Vec<Result<(), HwmonError>>,
    ) -> (HwmonPwmController, WriteLog, Arc<StateCache>) {
        let cache = Arc::new(StateCache::new());
        let (writer, writes) = ScriptedSysfsWriter::new(write_results);
        let writer = writer.with_file("/sys/class/hwmon/hwmon0/fan1_input", "1200\n");
        let lease_mgr = LeaseManager::new();
        let ctrl = HwmonPwmController::new(headers, lease_mgr, Box::new(writer), cache.clone());
        (ctrl, writes, cache)
    }

    #[test]
    fn set_pwm_enable_write_failure_returns_hardware_error() {
        // If the enable (pwm_enable → manual) write fails, set_pwm must return
        // an error and must NOT update the cache or coalescing state.
        let (mut ctrl, writes, cache) = setup_scripted_controller(
            vec![make_header("h1", "CHA_FAN1", 0)],
            vec![Err(HwmonError::WriteError {
                path: "/sys/class/hwmon/hwmon0/pwm1_enable".into(),
                message: "Permission denied".into(),
            })],
        );

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let result = ctrl.set_pwm("h1", 50, &lease.lease_id);

        assert!(result.is_err());
        match result.unwrap_err() {
            HwmonControlError::Hardware(_) => {}
            other => panic!("expected Hardware error, got: {other:?}"),
        }

        // Enable was attempted (1 write logged) but failed
        let w = writes.lock();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].1, "1"); // attempted to write pwm_enable=1

        // Cache must NOT be updated — the write failed
        let snap = cache.snapshot();
        assert!(
            !snap.hwmon_fans.contains_key("h1"),
            "cache should not be updated on write failure"
        );
    }

    #[test]
    fn set_pwm_value_write_failure_after_enable_succeeds() {
        // If enable succeeds but the PWM value write fails, set_pwm returns
        // an error. The enable was already written to hardware (irreversible),
        // but manual_mode_set stays false because it is only set after BOTH
        // writes succeed (line 246). This means a retry will re-issue the
        // enable write — safe and idempotent.
        let (mut ctrl, writes, cache) = setup_scripted_controller(
            vec![make_header("h1", "CHA_FAN1", 0)],
            vec![
                Ok(()), // enable write succeeds
                Err(HwmonError::WriteError {
                    path: "/sys/class/hwmon/hwmon0/pwm1".into(),
                    message: "I/O error".into(),
                }),
            ],
        );

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let result = ctrl.set_pwm("h1", 50, &lease.lease_id);

        assert!(result.is_err());
        match result.unwrap_err() {
            HwmonControlError::Hardware(_) => {}
            other => panic!("expected Hardware error, got: {other:?}"),
        }

        // Two writes attempted: enable (OK) + PWM value (failed)
        let w = writes.lock();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].1, "1"); // enable succeeded
        assert_eq!(w[1].1, percent_to_raw(50).to_string()); // PWM attempted

        // Cache must NOT be updated — the PWM write failed
        let snap = cache.snapshot();
        assert!(
            !snap.hwmon_fans.contains_key("h1"),
            "cache should not be updated on partial write failure"
        );
    }

    #[test]
    fn set_pwm_first_write_failure_does_not_attempt_pwm() {
        // When the enable write fails (first write), the PWM value write
        // must never be attempted — the ? operator returns early.
        let (mut ctrl, writes, _cache) = setup_scripted_controller(
            vec![make_header("h1", "CHA_FAN1", 0)],
            vec![Err(HwmonError::WriteError {
                path: "/sys/class/hwmon/hwmon0/pwm1_enable".into(),
                message: "Device removed".into(),
            })],
        );

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let _ = ctrl.set_pwm("h1", 50, &lease.lease_id);

        // Only the enable write was attempted — PWM write never reached
        let w = writes.lock();
        assert_eq!(w.len(), 1, "only enable write should be attempted");
    }

    // ── pwm_enable watchdog tests ────────────────────────────────────

    fn setup_controller_with_enable(
        headers: Vec<PwmHeaderDescriptor>,
        enable_value: &str,
    ) -> (HwmonPwmController, WriteLog, Arc<StateCache>) {
        let cache = Arc::new(StateCache::new());
        let (writer, writes) = MockSysfsWriter::new();
        let writer = writer
            .with_file("/sys/class/hwmon/hwmon0/fan1_input", "1200\n")
            .with_file("/sys/class/hwmon/hwmon0/pwm1_enable", enable_value);
        let lease_mgr = LeaseManager::new();
        let ctrl = HwmonPwmController::new(headers, lease_mgr, Box::new(writer), cache.clone());
        (ctrl, writes, cache)
    }

    #[test]
    fn watchdog_detects_bios_reclaim() {
        // Simulate BIOS reclaiming pwm_enable after first write.
        // Mock always returns "2" for pwm_enable reads (BIOS auto mode).
        let (mut ctrl, writes, _cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "2");

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();

        // First write: manual_mode_set=false, watchdog skipped, sets enable+PWM
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();
        assert_eq!(ctrl.enable_revert_counts().get("h1"), None);

        // Second write with different value: watchdog reads pwm_enable="2", detects revert
        ctrl.set_pwm("h1", 60, &lease.lease_id).unwrap();
        assert_eq!(ctrl.enable_revert_counts().get("h1"), Some(&1));

        let w = writes.lock();
        // First: enable(1) + pwm(50). Second: enable(1) + pwm(60) (re-wrote enable).
        assert_eq!(w.len(), 4);
        assert_eq!(w[0].1, "1"); // first enable
        assert_eq!(w[2].1, "1"); // watchdog re-wrote enable
    }

    /// A controller whose mock always reports pwm_enable="2" (BIOS auto), so
    /// every write after the first is seen as a reclaim. Mirrors
    /// `watchdog_detects_bios_reclaim`'s setup, which is the only way to make
    /// the watchdog fire.
    fn reclaiming_controller() -> (HwmonPwmController, crate::hwmon::lease::HwmonLease) {
        let (mut ctrl, _writes, _cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "2");
        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap(); // primes manual_mode_set
        (ctrl, lease)
    }

    #[test]
    fn reclaim_stamps_an_age_beside_the_count() {
        // DEC-360: the count alone cannot distinguish an active BIOS fight from
        // one reclaim the watchdog already remediated, because the map is
        // monotonic with no reset path.
        let (mut ctrl, lease) = reclaiming_controller();
        assert!(
            ctrl.enable_revert_ages_ms(Instant::now()).is_empty(),
            "precondition: nothing dated before a reclaim is counted"
        );

        ctrl.set_pwm("h1", 60, &lease.lease_id).unwrap();
        assert_eq!(ctrl.enable_revert_counts().get("h1"), Some(&1));
        assert!(
            ctrl.enable_revert_ages_ms(Instant::now())
                .contains_key("h1"),
            "a counted reclaim must be dated"
        );
    }

    #[test]
    fn the_age_advances_with_the_clock_not_with_logging() {
        // The GUI register row proposed reusing `WatchdogLogState::last_emit_at`.
        // That field records the last LOG EMISSION and emission is throttled —
        // first reclaim WARN, the rest DEBUG — so on a busy header it stops
        // advancing, and an age derived from it would read as stale on exactly
        // the machines where contention is worst. Taking `now` as a parameter is
        // what makes the age independent of whether anything was logged.
        let (mut ctrl, lease) = reclaiming_controller();
        ctrl.set_pwm("h1", 60, &lease.lease_id).unwrap();

        let t0 = Instant::now();
        let near = ctrl.enable_revert_ages_ms(t0)["h1"];
        let later = ctrl.enable_revert_ages_ms(t0 + Duration::from_secs(60))["h1"];
        assert!(
            later >= near + 59_000,
            "the age must advance with the clock: near={near} later={later}"
        );
    }

    #[test]
    fn a_later_reclaim_freshens_the_age_but_the_count_still_accumulates() {
        // The two answer different questions and must not be conflated: the
        // count says how much has happened, the age says how recently.
        let (mut ctrl, lease) = reclaiming_controller();
        ctrl.set_pwm("h1", 60, &lease.lease_id).unwrap();

        let future = Instant::now() + Duration::from_secs(3600);
        let aged = ctrl.enable_revert_ages_ms(future)["h1"];
        assert!(
            aged >= 3_600_000,
            "precondition: the first reclaim must look old"
        );

        ctrl.set_pwm("h1", 70, &lease.lease_id).unwrap();
        assert_eq!(
            ctrl.enable_revert_counts().get("h1"),
            Some(&2),
            "the count must keep accumulating"
        );
        let fresh = ctrl.enable_revert_ages_ms(future)["h1"];
        assert!(
            fresh < aged,
            "a newer reclaim must make the age younger: aged={aged} fresh={fresh}"
        );
    }

    // ── [HOST-a / DEC-326] the driver's full-speed alias ─────────────
    //
    // `it87.c:3612` synthesises `pwm_enable == 0` ("full speed") from the duty
    // register whenever it holds 0xff, on any header of a chip without
    // `FEAT_FANCTL_ONOFF` and on `pwm4+` of every ITE chip. A wholly successful
    // 100% write therefore reads back looking exactly like a BIOS reclaim.

    /// Seed both the enable AND the duty file, which the alias check reads.
    fn setup_controller_with_enable_and_duty(
        headers: Vec<PwmHeaderDescriptor>,
        enable_value: &str,
        duty_raw: &str,
    ) -> (HwmonPwmController, WriteLog, Arc<StateCache>) {
        let cache = Arc::new(StateCache::new());
        let (writer, writes) = MockSysfsWriter::new();
        let writer = writer
            .with_file("/sys/class/hwmon/hwmon0/fan1_input", "1200\n")
            .with_file("/sys/class/hwmon/hwmon0/pwm1_enable", enable_value)
            .with_file("/sys/class/hwmon/hwmon0/pwm1", duty_raw);
        let lease_mgr = LeaseManager::new();
        let ctrl = HwmonPwmController::new(headers, lease_mgr, Box::new(writer), cache.clone());
        (ctrl, writes, cache)
    }

    #[test]
    fn watchdog_ignores_the_full_speed_alias_at_100_percent() {
        // enable reads 0 and the duty register holds 255 — the exact live
        // signature measured on this host's it8696 at 100%.
        let (mut ctrl, _writes, _cache) = setup_controller_with_enable_and_duty(
            vec![make_header("h1", "CHA_FAN1", 0)],
            "0",
            "255\n",
        );
        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();

        ctrl.set_pwm("h1", 100, &lease.lease_id).unwrap();
        ctrl.set_pwm("h1", 100, &lease.lease_id).unwrap();

        assert_eq!(
            ctrl.enable_revert_counts().get("h1"),
            None,
            "our own 100% duty read back through the driver is not a BIOS reclaim"
        );
    }

    #[test]
    fn watchdog_still_detects_a_reclaim_when_the_duty_is_not_ours() {
        // The opposite branch. Same enable=0, but the duty register does NOT
        // hold what we commanded — so the mode reading means what it says.
        // Without this case a predicate stuck at "always alias" would pass the
        // test above.
        let (mut ctrl, _writes, _cache) = setup_controller_with_enable_and_duty(
            vec![make_header("h1", "CHA_FAN1", 0)],
            "0",
            "128\n",
        );
        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();

        ctrl.set_pwm("h1", 100, &lease.lease_id).unwrap();
        ctrl.set_pwm("h1", 100, &lease.lease_id).unwrap();

        assert_eq!(
            ctrl.enable_revert_counts().get("h1"),
            Some(&1),
            "enable=0 at a duty we did not command is a real loss of control"
        );
    }

    #[test]
    fn watchdog_compares_against_the_last_command_not_the_pending_one() {
        // The watchdog runs BEFORE this call's write, so the duty register still
        // holds the PREVIOUS command. Reading the pending duty instead would
        // declare a false reclaim on the first descending tick after a 100% one —
        // the same defect one tick over.
        let (mut ctrl, _writes, _cache) = setup_controller_with_enable_and_duty(
            vec![make_header("h1", "CHA_FAN1", 0)],
            "0",
            "255\n",
        );
        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();

        ctrl.set_pwm("h1", 100, &lease.lease_id).unwrap();
        // Descending: we ask for 40 while the register still shows our own 255.
        ctrl.set_pwm("h1", 40, &lease.lease_id).unwrap();

        assert_eq!(
            ctrl.enable_revert_counts().get("h1"),
            None,
            "the duty on the wire is still the 100% WE wrote, not a reclaim"
        );
    }

    #[test]
    fn watchdog_no_revert_when_enable_stays_manual() {
        // pwm_enable reads "1" — no revert detected, normal coalescing.
        let (mut ctrl, writes, _cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "1");

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap(); // coalesced

        assert!(ctrl.enable_revert_counts().is_empty());
        let w = writes.lock();
        // enable(1) + pwm(50) = 2 writes; second call coalesced
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn watchdog_revert_breaks_coalescing() {
        // Same PWM value, but BIOS reclaimed → must re-write both enable and PWM.
        let (mut ctrl, writes, _cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "2");

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap(); // would coalesce but BIOS reclaimed

        assert_eq!(ctrl.enable_revert_counts().get("h1"), Some(&1));
        let w = writes.lock();
        // First: enable + pwm50. Second: enable + pwm50 (forced by reclaim).
        assert_eq!(w.len(), 4);
    }

    #[test]
    fn watchdog_revert_count_persists_across_leases() {
        let (mut ctrl, _writes, _cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "2");

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let lid = lease.lease_id.clone();
        ctrl.set_pwm("h1", 50, &lid).unwrap();
        ctrl.set_pwm("h1", 60, &lid).unwrap(); // triggers revert

        ctrl.lease_manager_mut().release_lease(&lid).unwrap();
        ctrl.on_lease_released();

        let lease2 = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Verify)
            .unwrap();
        ctrl.set_pwm("h1", 70, &lease2.lease_id).unwrap();
        ctrl.set_pwm("h1", 80, &lease2.lease_id).unwrap(); // triggers revert again

        assert_eq!(ctrl.enable_revert_counts().get("h1"), Some(&2));
    }

    #[test]
    fn resume_flag_resets_manual_mode() {
        let (mut ctrl, writes, cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "1");

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap(); // enable + pwm

        // Simulate system resume
        cache.set_resume_detected();

        // Same PWM — would normally coalesce, but resume cleared manual_mode_set
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();

        let w = writes.lock();
        // First: enable(1) + pwm(50). After resume: enable(1) + pwm(50).
        assert_eq!(w.len(), 4);
        assert_eq!(w[2].1, "1"); // re-wrote enable after resume
    }

    // ── Watchdog log throttle tests ─────────────────────────────────
    //
    // The watchdog still runs once per reclaim — these tests cover the
    // *log-emission* throttle that turns 3,600 WARN/hr into roughly 60
    // INFO/hr while preserving the per-event cumulative counter.

    #[test]
    fn watchdog_log_throttle_first_event_emits_warn() {
        // Cold state: the very first reclaim per header must produce a WARN
        // so the operator sees BIOS interference at least once.
        let mut state = WatchdogLogState::default();
        let now = Instant::now();
        let action = decide_watchdog_log_action(&mut state, now, WATCHDOG_SUMMARY_INTERVAL, 1);

        assert_eq!(action, WatchdogLogAction::Warn);
        assert!(state.first_warn_emitted);
        assert_eq!(state.last_emit_at, Some(now));
        assert_eq!(state.count_at_last_summary, 1);
    }

    #[test]
    fn watchdog_log_throttle_subsequent_within_interval_are_debug() {
        // Within the summary interval, every reclaim after the first should
        // collapse to DEBUG so journalctl is not spammed once per second.
        let mut state = WatchdogLogState::default();
        let t0 = Instant::now();

        // First reclaim → WARN.
        let _ = decide_watchdog_log_action(&mut state, t0, WATCHDOG_SUMMARY_INTERVAL, 1);

        // 59 subsequent reclaims spaced one second apart, still inside the
        // 60-second window: every action must be DEBUG.
        for i in 1..60u64 {
            let action = decide_watchdog_log_action(
                &mut state,
                t0 + Duration::from_secs(i),
                WATCHDOG_SUMMARY_INTERVAL,
                i + 1,
            );
            assert_eq!(
                action,
                WatchdogLogAction::Debug,
                "expected Debug at offset {i}s, got {action:?}",
            );
        }
    }

    #[test]
    fn watchdog_log_throttle_summary_emits_once_per_interval() {
        // After the summary interval elapses, the throttle must emit a single
        // INFO summary with the delta and cumulative figure, then return to
        // DEBUG for the next interval.
        let mut state = WatchdogLogState::default();
        let t0 = Instant::now();

        // Minute 1: first reclaim WARN at count=1; then 59 DEBUG events.
        let _ = decide_watchdog_log_action(&mut state, t0, WATCHDOG_SUMMARY_INTERVAL, 1);
        for i in 1..60u64 {
            let _ = decide_watchdog_log_action(
                &mut state,
                t0 + Duration::from_secs(i),
                WATCHDOG_SUMMARY_INTERVAL,
                i + 1,
            );
        }

        // Minute 2 starts at exactly t+60s: a single INFO summary should
        // fire reporting delta=60 (60 events since the last emit) and
        // cumulative=61 (count at this moment).
        let action = decide_watchdog_log_action(
            &mut state,
            t0 + Duration::from_secs(60),
            WATCHDOG_SUMMARY_INTERVAL,
            61,
        );
        assert_eq!(
            action,
            WatchdogLogAction::Summary {
                delta: 60,
                cumulative: 61,
            }
        );

        // The next reclaim 1s later should drop back to DEBUG.
        let action = decide_watchdog_log_action(
            &mut state,
            t0 + Duration::from_secs(61),
            WATCHDOG_SUMMARY_INTERVAL,
            62,
        );
        assert_eq!(action, WatchdogLogAction::Debug);
    }

    #[test]
    fn watchdog_log_throttle_schedule_in_one_hour() {
        // Steady-state spam scenario: 3600 reclaims, one per second, over one
        // hour. The throttle must produce exactly:
        //   - 1 Warn (the first event)
        //   - 59 Summary (one at every minute boundary t=60, 120, ..., 3540)
        //   - 3540 Debug (everything else)
        // Total user-visible (Warn+Summary) emissions: 60 — one per minute,
        // matching the documented "1 + N/60 per minute" budget.
        let mut state = WatchdogLogState::default();
        let t0 = Instant::now();

        let mut warn_count = 0;
        let mut info_count = 0;
        let mut debug_count = 0;

        for sec in 0..3600u64 {
            let count = sec + 1;
            let action = decide_watchdog_log_action(
                &mut state,
                t0 + Duration::from_secs(sec),
                WATCHDOG_SUMMARY_INTERVAL,
                count,
            );
            match action {
                WatchdogLogAction::Warn => warn_count += 1,
                WatchdogLogAction::Summary { .. } => info_count += 1,
                WatchdogLogAction::Debug => debug_count += 1,
            }
        }

        assert_eq!(
            warn_count, 1,
            "exactly one WARN per header per controller lifetime"
        );
        assert_eq!(
            info_count, 59,
            "one INFO summary at each 60s boundary after the initial WARN",
        );
        assert_eq!(debug_count, 3540, "everything else collapses to DEBUG",);
        // Sanity: the three buckets sum to the total event count.
        assert_eq!(warn_count + info_count + debug_count, 3600);
    }

    #[test]
    fn watchdog_log_throttle_does_not_gate_revert_count() {
        // Critical invariant: throttling the *log* must never throttle the
        // *counter*. The cumulative enable_revert_counts must increment on
        // every event so /diagnostics/hardware stays truthful regardless of
        // log volume.
        let (mut ctrl, _writes, _cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "2");

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let lid = lease.lease_id.clone();

        // First write seeds manual_mode_set; subsequent writes each hit the
        // watchdog because the mock keeps returning pwm_enable="2".
        ctrl.set_pwm("h1", 50, &lid).unwrap();
        for pwm in 0..50u8 {
            ctrl.set_pwm("h1", pwm, &lid).unwrap();
        }

        // 50 reclaim events expected (one per call after the first).
        assert_eq!(ctrl.enable_revert_counts().get("h1"), Some(&50));
    }

    // ── PWM verify-after-write mismatch tests ───────────────────────
    //
    // T2 (test-tests audit): exercise the path at the bottom of set_pwm()
    // where the daemon reads back the PWM sysfs file and compares against
    // the value it just wrote. Without these, mutating `actual_raw != raw`
    // to `==` survives every existing test.

    /// Mock writer where read_file returns a *different* value than what was
    /// written for the pwm sysfs path. Simulates BIOS clamping or EC
    /// interference where our write was overridden.
    struct ClampingSysfsWriter {
        writes: WriteLog,
        readbacks: StdHashMap<String, String>,
    }

    impl ClampingSysfsWriter {
        fn new() -> (Self, WriteLog) {
            let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    writes: writes.clone(),
                    readbacks: StdHashMap::new(),
                },
                writes,
            )
        }

        fn returns_readback(mut self, path: &str, value: &str) -> Self {
            self.readbacks.insert(path.to_string(), value.to_string());
            self
        }
    }

    impl SysfsWriter for ClampingSysfsWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes
                .lock()
                .push((path.to_string(), value.to_string()));
            Ok(())
        }

        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            self.readbacks
                .get(path)
                .cloned()
                .ok_or(HwmonError::ReadError {
                    path: path.to_string(),
                    message: "not stubbed".to_string(),
                })
        }
    }

    #[test]
    fn verify_mismatch_increments_counter() {
        // Caller writes 50% (raw 128) but the BIOS/EC clamps it to 60%
        // (raw 153). The read-back fires the mismatch path, which must
        // increment verify_mismatch_counts so the divergence is observable.
        let cache = Arc::new(StateCache::new());
        let (writer, _writes) = ClampingSysfsWriter::new();
        // pwm sysfs file reports raw=153 (60%) regardless of what we wrote.
        let writer = writer
            .returns_readback("/sys/class/hwmon/hwmon0/pwm1", "153")
            .returns_readback("/sys/class/hwmon/hwmon0/fan1_input", "1200");
        let lease_mgr = LeaseManager::new();
        let mut ctrl = HwmonPwmController::new(
            vec![make_header("h1", "CHA_FAN1", 0)],
            lease_mgr,
            Box::new(writer),
            cache.clone(),
        );

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        // 50% → raw 128. The mock reports back 153, triggering the mismatch.
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();

        assert_eq!(
            ctrl.verify_mismatch_counts().get("h1"),
            Some(&1),
            "verify mismatch must be recorded when readback differs from write",
        );
    }

    #[test]
    fn verify_no_mismatch_when_readback_matches_write() {
        // Sanity test: when the readback matches the value we wrote, the
        // counter must NOT increment. Catches a mutation flipping `!=` to
        // `==` which would otherwise increment on every successful write.
        let cache = Arc::new(StateCache::new());
        let (writer, _writes) = ClampingSysfsWriter::new();
        // Writing 50 → raw 128. Readback returns the same value.
        let writer = writer
            .returns_readback("/sys/class/hwmon/hwmon0/pwm1", "128")
            .returns_readback("/sys/class/hwmon/hwmon0/fan1_input", "1200");
        let lease_mgr = LeaseManager::new();
        let mut ctrl = HwmonPwmController::new(
            vec![make_header("h1", "CHA_FAN1", 0)],
            lease_mgr,
            Box::new(writer),
            cache.clone(),
        );

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();

        assert!(
            ctrl.verify_mismatch_counts().is_empty(),
            "no mismatch must be recorded when readback equals write, got: {:?}",
            ctrl.verify_mismatch_counts(),
        );
    }

    #[test]
    fn verify_mismatch_accumulates_across_writes() {
        // Persistent BIOS clamping: the counter should keep climbing on
        // each subsequent write (not be reset, not cap at 1).
        let cache = Arc::new(StateCache::new());
        let (writer, _writes) = ClampingSysfsWriter::new();
        let writer = writer
            .returns_readback("/sys/class/hwmon/hwmon0/pwm1", "200")
            .returns_readback("/sys/class/hwmon/hwmon0/fan1_input", "1200");
        let lease_mgr = LeaseManager::new();
        let mut ctrl = HwmonPwmController::new(
            vec![make_header("h1", "CHA_FAN1", 0)],
            lease_mgr,
            Box::new(writer),
            cache.clone(),
        );

        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap();
        let lid = lease.lease_id.clone();
        ctrl.set_pwm("h1", 30, &lid).unwrap();
        ctrl.set_pwm("h1", 40, &lid).unwrap();
        ctrl.set_pwm("h1", 50, &lid).unwrap();

        assert_eq!(
            ctrl.verify_mismatch_counts().get("h1"),
            Some(&3),
            "verify mismatch counter must accumulate across writes",
        );
    }

    #[test]
    fn watchdog_log_throttle_state_is_per_header() {
        // Each header must have an independent first-WARN state — adding a
        // second header late should still produce a WARN for that header,
        // even after the first header has long since dropped to DEBUG.
        let mut state_a = WatchdogLogState::default();
        let mut state_b = WatchdogLogState::default();
        let t0 = Instant::now();

        // Header A: first reclaim at t=0 → WARN, then DEBUG at t=10s.
        assert_eq!(
            decide_watchdog_log_action(&mut state_a, t0, WATCHDOG_SUMMARY_INTERVAL, 1),
            WatchdogLogAction::Warn,
        );
        assert_eq!(
            decide_watchdog_log_action(
                &mut state_a,
                t0 + Duration::from_secs(10),
                WATCHDOG_SUMMARY_INTERVAL,
                2,
            ),
            WatchdogLogAction::Debug,
        );

        // Header B: first reclaim at t=10s → still its own WARN, regardless
        // of header A's throttle clock.
        assert_eq!(
            decide_watchdog_log_action(
                &mut state_b,
                t0 + Duration::from_secs(10),
                WATCHDOG_SUMMARY_INTERVAL,
                1,
            ),
            WatchdogLogAction::Warn,
        );
    }

    // ── DEC-382: take and give back ──────────────────────────────────────

    /// A sysfs whose files change when written, shared with the test so it can
    /// play the firmware. `MockSysfsWriter` never updates what it reads back,
    /// which is exactly the property these tests need: a capture made AFTER the
    /// `pwm_enable=1` write reads `1` here, so they can tell "read before the
    /// take" from "read after it". Paths marked write-only refuse every read, as
    /// sysfs does for a `0200` attribute, while still recording what is written.
    #[derive(Clone, Default)]
    struct LiveSysfs(
        Arc<Mutex<StdHashMap<String, String>>>,
        Arc<Mutex<Vec<String>>>,
    );

    impl LiveSysfs {
        fn set(&self, path: &str, value: &str) {
            self.0.lock().insert(path.into(), value.into());
        }
        fn get(&self, path: &str) -> Option<String> {
            self.0.lock().get(path).cloned()
        }
        fn write_only(&self, path: &str) {
            self.1.lock().push(path.into());
        }
    }

    impl SysfsWriter for LiveSysfs {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.set(path, value.trim());
            Ok(())
        }
        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            if self.1.lock().iter().any(|p| p == path) {
                return Err(HwmonError::ReadError {
                    path: path.into(),
                    message: "Permission denied (os error 13)".into(),
                });
            }
            self.get(path)
                .map(|v| format!("{v}\n"))
                .ok_or(HwmonError::ReadError {
                    path: path.into(),
                    message: "not found".into(),
                })
        }
    }

    const ENABLE: &str = "/sys/class/hwmon/hwmon0/pwm1_enable";
    const PWM: &str = "/sys/class/hwmon/hwmon0/pwm1";

    fn live_controller(
        enable: &str,
        pwm: &str,
    ) -> (HwmonPwmController, LiveSysfs, Arc<StateCache>, String) {
        let sysfs = LiveSysfs::default();
        sysfs.set(ENABLE, enable);
        sysfs.set(PWM, pwm);
        let cache = Arc::new(StateCache::new());
        let mut ctrl = HwmonPwmController::new(
            vec![make_header("h1", "CPU_FAN", 0)],
            LeaseManager::new(),
            Box::new(sysfs.clone()),
            cache.clone(),
        );
        let lease = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap()
            .lease_id;
        (ctrl, sysfs, cache, lease)
    }

    /// [SAFETY] DEC-382 (`TS-a`), at the call site: the mode recorded is the one
    /// the header had BEFORE `set_pwm` switched it to manual. Recorded after the
    /// switch, it would be `1` — and a hand-back would then leave the header in
    /// manual mode, which is the defect with the value-of-`2` part removed.
    #[test]
    fn the_first_take_records_the_mode_the_header_had_before_it() {
        let (mut ctrl, sysfs, _cache, lease) = live_controller("5", "90");
        ctrl.set_pwm("h1", 60, &lease).unwrap();
        assert_eq!(
            sysfs.get(ENABLE).as_deref(),
            Some("1"),
            "precondition: the take switched the header to manual"
        );
        assert_eq!(
            ctrl.handback().taken_header("h1").map(|t| t.action),
            Some(HandBack::Mode(5))
        );
    }

    /// A firmware reclaim makes the next write take the header again. The first
    /// original must survive that: the reclaimed mode is firmware re-asserting
    /// itself, not what the header was doing before the daemon arrived.
    #[test]
    fn a_retake_after_a_firmware_reclaim_keeps_the_first_original() {
        let (mut ctrl, sysfs, _cache, lease) = live_controller("5", "90");
        ctrl.set_pwm("h1", 60, &lease).unwrap();
        sysfs.set(ENABLE, "2");
        ctrl.set_pwm("h1", 61, &lease).unwrap();
        assert_eq!(
            ctrl.enable_revert_counts().get("h1"),
            Some(&1),
            "precondition: the watchdog saw the reclaim and took the header again"
        );
        assert_eq!(
            ctrl.handback().taken_header("h1").map(|t| t.action),
            Some(HandBack::Mode(5))
        );
    }

    /// [SAFETY] The obstacle that withdrew `D1-m`: after a hand-back the header
    /// reads its firmware mode, and the next take must NOT count that as a
    /// firmware reclaim. `hand_back` forgets the write state for exactly this.
    #[test]
    fn a_hand_back_is_not_counted_as_a_firmware_reclaim() {
        let (mut ctrl, sysfs, _cache, lease) = live_controller("5", "90");
        ctrl.set_pwm("h1", 60, &lease).unwrap();
        assert_eq!(
            ctrl.hand_back("h1", &lease).unwrap(),
            Some(HandBackOutcome::Restored)
        );
        assert_eq!(
            sysfs.get(ENABLE).as_deref(),
            Some("5"),
            "the recorded mode is back"
        );
        assert!(!ctrl.handback().is_taken("h1"));

        ctrl.set_pwm("h1", 60, &lease).unwrap();
        assert!(
            ctrl.enable_revert_counts().get("h1").is_none(),
            "the daemon's own hand-back must not read as a reclaim; got {:?}",
            ctrl.enable_revert_counts()
        );
        assert_eq!(
            sysfs.get(ENABLE).as_deref(),
            Some("1"),
            "the next take re-asserts manual mode"
        );
        assert!(ctrl.handback().is_taken("h1"));
    }

    /// [SAFETY] `TS-ab` at the call site: `set_pwm` hands the reading the
    /// header's own chip. A `dell_smm` switch that cannot be read is recorded as
    /// BIOS control and given back as it; the same unreadable switch on another
    /// chip keeps `fancontrol`'s full-speed fallback. The second arm is what
    /// fails if the call site passed a fixed chip name instead of the header's.
    #[test]
    fn an_unreadable_dell_smm_switch_goes_back_to_the_bios() {
        // (chip, recorded action, hand-back outcome, the switch afterwards: `2`
        // is the BIOS; `1` is the fallback's manual mode at 255)
        for (chip, action, restored, enable_after) in [
            (
                "dell_smm",
                HandBack::WriteOnlyMode(2),
                HandBackOutcome::Restored,
                "2",
            ),
            (
                "it8696",
                HandBack::FullSpeed,
                HandBackOutcome::FullSpeed,
                "1",
            ),
        ] {
            let sysfs = LiveSysfs::default();
            sysfs.set(ENABLE, "2");
            sysfs.set(PWM, "90");
            sysfs.write_only(ENABLE);
            let mut header = make_header("h1", "Processor Fan", 0);
            header.chip_name = chip.into();
            let mut ctrl = HwmonPwmController::new(
                vec![header],
                LeaseManager::new(),
                Box::new(sysfs.clone()),
                Arc::new(StateCache::new()),
            );
            let lease = ctrl
                .lease_manager_mut()
                .take_lease(HwmonWriter::Engine)
                .unwrap()
                .lease_id;

            ctrl.set_pwm("h1", 60, &lease).unwrap();
            assert_eq!(
                sysfs.get(ENABLE).as_deref(),
                Some("1"),
                "{chip}: precondition: the take switched the header to manual"
            );
            assert_eq!(
                ctrl.handback().taken_header("h1").map(|t| t.action),
                Some(action),
                "{chip}"
            );
            assert_eq!(
                ctrl.hand_back("h1", &lease).unwrap(),
                Some(restored),
                "{chip}"
            );
            assert_eq!(sysfs.get(ENABLE).as_deref(), Some(enable_after), "{chip}");
        }
    }

    #[test]
    fn a_hand_back_needs_a_lease_and_writes_nothing_for_an_untaken_header() {
        let (mut ctrl, sysfs, _cache, lease) = live_controller("5", "90");
        assert!(matches!(
            ctrl.hand_back("h1", "no-lease"),
            Err(HwmonControlError::Lease(_))
        ));
        assert_eq!(ctrl.hand_back("h1", &lease).unwrap(), None);
        assert_eq!(sysfs.get(ENABLE).as_deref(), Some("5"));
        assert_eq!(sysfs.get(PWM).as_deref(), Some("90"));
    }

    /// After a hand-back the daemon no longer commands the header, and the cache
    /// must stop saying it does.
    #[test]
    fn a_hand_back_clears_the_commanded_duty_from_the_cache() {
        let (mut ctrl, _sysfs, cache, lease) = live_controller("5", "90");
        ctrl.set_pwm("h1", 60, &lease).unwrap();
        let commanded = |c: &StateCache| {
            c.snapshot()
                .hwmon_fans
                .get("h1")
                .and_then(|f| f.pwm_commanded_pct)
        };
        assert_eq!(commanded(&cache), Some(60), "precondition");
        ctrl.hand_back("h1", &lease).unwrap();
        assert_eq!(commanded(&cache), None);
    }

    // ── DEC-406 (`PTR-g`): the engine reconciles a coalesced duty ─────────

    /// `LiveSysfs` plus a log of every duty write and an optional clamp, so a
    /// test can play a second writer (set `PWM` between ticks), a chip that will
    /// not go below a duty, or an unreadable duty register.
    #[derive(Clone, Default)]
    struct DriftSysfs {
        live: LiveSysfs,
        pwm_writes: Arc<Mutex<Vec<String>>>,
        clamp_raw_min: Arc<Mutex<Option<u8>>>,
        /// A coarse driver: every write lands on the nearest of these raw levels
        /// (`dell_smm` is 0/128/255).
        levels: Arc<Mutex<Option<Vec<u8>>>>,
        /// Fail every duty write (EIO), leaving the register as it was.
        fail_pwm_writes: Arc<Mutex<bool>>,
        /// Fail every `pwm_enable` write, so a hand-back cannot write anything.
        fail_enable_writes: Arc<Mutex<bool>>,
    }

    impl DriftSysfs {
        fn duty_writes(&self) -> usize {
            self.pwm_writes.lock().len()
        }
        fn external_write(&self, pct: u8) {
            self.live.set(PWM, &percent_to_raw(pct).to_string());
        }
    }

    impl SysfsWriter for DriftSysfs {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            let mut value = value.trim().to_string();
            if (path == PWM && *self.fail_pwm_writes.lock())
                || (path == ENABLE && *self.fail_enable_writes.lock())
            {
                return Err(HwmonError::WriteError {
                    path: path.into(),
                    message: "Input/output error (os error 5)".into(),
                });
            }
            if path == PWM {
                self.pwm_writes.lock().push(value.clone());
                if let (Some(min), Ok(raw)) = (*self.clamp_raw_min.lock(), value.parse::<u8>()) {
                    value = raw.max(min).to_string();
                }
                if let (Some(levels), Ok(raw)) = (self.levels.lock().as_ref(), value.parse::<u8>())
                {
                    let nearest = levels.iter().min_by_key(|l| l.abs_diff(raw)).copied();
                    value = nearest.unwrap_or(raw).to_string();
                }
            }
            self.live.set(path, &value);
            Ok(())
        }
        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            self.live.read_file(path)
        }
    }

    fn drift_controller(
        owner: HwmonWriter,
    ) -> (HwmonPwmController, DriftSysfs, Arc<StateCache>, String) {
        let sysfs = DriftSysfs::default();
        sysfs.live.set(ENABLE, "2");
        sysfs.live.set(PWM, "0");
        let cache = Arc::new(StateCache::new());
        let mut ctrl = HwmonPwmController::new(
            vec![make_header("h1", "CHA_FAN1", 0)],
            LeaseManager::new(),
            Box::new(sysfs.clone()),
            cache.clone(),
        );
        let lease = ctrl.lease_manager_mut().take_lease(owner).unwrap().lease_id;
        (ctrl, sysfs, cache, lease)
    }

    fn published(cache: &StateCache) -> DutyReconciliation {
        cache
            .snapshot()
            .hwmon_duty_reconciliation
            .get("h1")
            .copied()
            .unwrap_or_default()
    }

    /// The state machine over one whole episode: three corrections, one give-up,
    /// holds, then recovery — with exactly one WARN-level action for the first
    /// correction and one for the give-up. The log lines in `duty_drift_rewrite`
    /// are a 1:1 match on these actions, so this is where "logs once" is pinned.
    #[test]
    fn a_drift_episode_corrects_three_times_then_gives_up_once() {
        let mut st = DriftState::default();
        let mut actions = Vec::new();
        for _ in 0..6 {
            let (a, next) = reconcile_decision(40, Some(60), st);
            actions.push(a);
            st = next;
        }
        assert_eq!(
            actions,
            vec![
                DriftAction::Correct { first: true },
                DriftAction::Correct { first: false },
                DriftAction::Correct { first: false },
                DriftAction::GiveUp { log: true },
                DriftAction::Hold,
                DriftAction::Hold,
            ]
        );
        assert!(st.not_holding);

        // A changed command restarts the corrections but keeps the episode, so
        // neither WARN repeats however often the curve moves under a second writer.
        st = st.after_plain_write();
        assert!(!st.not_holding && st.corrections == 0 && st.in_episode);
        let mut again = Vec::new();
        for _ in 0..4 {
            let (a, next) = reconcile_decision(41, Some(60), st);
            again.push(a);
            st = next;
        }
        assert_eq!(
            again,
            vec![
                DriftAction::Correct { first: false },
                DriftAction::Correct { first: false },
                DriftAction::Correct { first: false },
                DriftAction::GiveUp { log: false },
            ]
        );

        let (a, st) = reconcile_decision(41, Some(41), st);
        assert_eq!(
            a,
            DriftAction::Recovered {
                was_not_holding: true
            }
        );
        assert_eq!(st, DriftState::default(), "the episode is over");
    }

    /// Within the tolerance is agreement (quantised duty), `None` is unknown, and
    /// neither opens an episode or changes anything.
    #[test]
    fn within_tolerance_or_unreadable_is_not_a_mismatch() {
        let st = DriftState::default();
        let tol = crate::constants::READBACK_TOLERANCE_PCT;
        assert_eq!(
            reconcile_decision(40, Some(40 + tol), st),
            (DriftAction::Coalesce, st)
        );
        assert_eq!(
            reconcile_decision(40, Some(40 - tol), st),
            (DriftAction::Coalesce, st)
        );
        assert_eq!(
            reconcile_decision(40, None, st),
            (DriftAction::Coalesce, st)
        );
        let open = DriftState {
            corrections: 2,
            in_episode: true,
            ..st
        };
        assert_eq!(
            reconcile_decision(40, None, open),
            (DriftAction::Coalesce, open),
            "an unreadable duty neither ends an episode nor counts toward the give-up"
        );
        assert!(matches!(
            reconcile_decision(40, Some(40 + tol + 1), st).0,
            DriftAction::Correct { first: true }
        ));
    }

    /// [SAFETY] `PTR-g`, at the call site: a second writer's duty between two
    /// engine ticks is written back on the next tick, counted, and published.
    #[test]
    fn an_external_duty_write_is_corrected_on_the_next_tick() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        let before = sysfs.duty_writes();

        sysfs.external_write(60);
        ctrl.set_pwm("h1", 40, &lease).unwrap();

        assert_eq!(
            sysfs.duty_writes(),
            before + 1,
            "the coalesced tick must write"
        );
        assert_eq!(
            sysfs.live.get(PWM).as_deref(),
            Some(percent_to_raw(40).to_string().as_str())
        );
        assert_eq!(ctrl.duty_corrections().get("h1"), Some(&1));
        assert_eq!(
            published(&cache),
            DutyReconciliation {
                corrections: 1,
                not_holding: false
            }
        );

        // The correction held: the next tick writes nothing and counts nothing.
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(sysfs.duty_writes(), before + 1);
        assert_eq!(ctrl.duty_corrections().get("h1"), Some(&1));
    }

    /// Readback within the tolerance is left alone: no write, no count.
    #[test]
    fn a_readback_within_tolerance_writes_nothing() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        let before = sysfs.duty_writes();
        sysfs.external_write(40 + crate::constants::READBACK_TOLERANCE_PCT);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(sysfs.duty_writes(), before);
        assert_eq!(published(&cache).corrections, 0);
    }

    /// [SAFETY] Three corrections a persistent second writer undoes flag the
    /// header and stop the writes: the daemon stops fighting it.
    #[test]
    fn three_corrections_that_do_not_hold_flag_the_header_and_stop_writing() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 20, &lease).unwrap();
        let before = sysfs.duty_writes();

        for tick in 1..=3 {
            sysfs.external_write(60);
            ctrl.set_pwm("h1", 20, &lease).unwrap();
            assert_eq!(sysfs.duty_writes(), before + tick, "correction {tick}");
            assert!(!ctrl.duty_not_holding("h1"));
        }
        sysfs.external_write(60);
        ctrl.set_pwm("h1", 20, &lease).unwrap();
        assert!(
            ctrl.duty_not_holding("h1"),
            "the fourth disagreement gives up"
        );
        for _ in 0..5 {
            sysfs.external_write(60);
            ctrl.set_pwm("h1", 20, &lease).unwrap();
        }
        assert_eq!(
            sysfs.duty_writes(),
            before + 3,
            "no writes after the give-up"
        );
        assert_eq!(
            published(&cache),
            DutyReconciliation {
                corrections: 3,
                not_holding: true
            }
        );
    }

    /// A changed command leaves `duty_not_holding` and corrections resume; the
    /// readback agreeing again also leaves it.
    #[test]
    fn a_changed_command_or_agreement_leaves_not_holding() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 20, &lease).unwrap();
        for _ in 0..4 {
            sysfs.external_write(60);
            ctrl.set_pwm("h1", 20, &lease).unwrap();
        }
        assert!(ctrl.duty_not_holding("h1"), "precondition");

        ctrl.set_pwm("h1", 22, &lease).unwrap();
        assert!(!ctrl.duty_not_holding("h1"));
        assert!(!published(&cache).not_holding, "the wire follows");
        sysfs.external_write(60);
        let before = sysfs.duty_writes();
        ctrl.set_pwm("h1", 22, &lease).unwrap();
        assert_eq!(sysfs.duty_writes(), before + 1, "correction resumes");

        for _ in 0..3 {
            sysfs.external_write(60);
            ctrl.set_pwm("h1", 22, &lease).unwrap();
        }
        assert!(ctrl.duty_not_holding("h1"), "precondition: flagged again");
        sysfs.external_write(22);
        ctrl.set_pwm("h1", 22, &lease).unwrap();
        assert!(!ctrl.duty_not_holding("h1"), "agreement leaves the flag");
        assert!(!published(&cache).not_holding);
    }

    /// S2-5: a coarse driver holds a different duty from the one written
    /// (`dell_smm`: 40 % lands on 128 = 50 %). That is what the header TOOK, so
    /// it is not drift: no correction, however many ticks — and a second writer
    /// on the same header is still corrected, back to the level it took.
    #[test]
    fn a_coarse_driver_is_not_drift_but_a_second_writer_on_it_is() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        *sysfs.levels.lock() = Some(vec![0, 128, 255]);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(sysfs.live.get(PWM).as_deref(), Some("128"), "precondition");
        let before = sysfs.duty_writes();
        for _ in 0..6 {
            ctrl.set_pwm("h1", 40, &lease).unwrap();
        }
        assert_eq!(sysfs.duty_writes(), before, "quantisation is not drift");
        assert_eq!(published(&cache), DutyReconciliation::default());

        sysfs.live.set(PWM, "255");
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(sysfs.duty_writes(), before + 1, "a second writer is");
        assert_eq!(sysfs.live.get(PWM).as_deref(), Some("128"));
        assert_eq!(published(&cache).corrections, 1);
    }

    /// S2-5: a chip that clamps a low duty is never corrected — the clamp is
    /// what it took — and stays visible through the write-verify counter.
    #[test]
    fn a_clamping_chip_is_not_corrected_but_is_counted_by_write_verify() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        *sysfs.clamp_raw_min.lock() = Some(percent_to_raw(35));
        ctrl.set_pwm("h1", 20, &lease).unwrap();
        let before = sysfs.duty_writes();
        for _ in 0..6 {
            ctrl.set_pwm("h1", 20, &lease).unwrap();
        }
        assert_eq!(sysfs.duty_writes(), before);
        assert_eq!(published(&cache), DutyReconciliation::default());
        assert_eq!(ctrl.verify_mismatch_counts().get("h1"), Some(&1));
    }

    /// An unreadable duty register produces no correction and no count.
    #[test]
    fn an_unreadable_duty_is_never_corrected() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        sysfs.external_write(60);
        sysfs.live.write_only(PWM);
        let before = sysfs.duty_writes();
        for _ in 0..5 {
            ctrl.set_pwm("h1", 40, &lease).unwrap();
        }
        assert_eq!(sysfs.duty_writes(), before);
        assert_eq!(published(&cache), DutyReconciliation::default());
    }

    /// A diagnostic's `Verify` lease is never reconciled: it gets exactly the duty
    /// it asked for, and nothing is counted against the header (S2-2).
    ///
    /// `PTA-j`: "exactly the duty it asked for" includes a repeat. Until DEC-406's
    /// residual was fixed this asserted the opposite — that the repeated 40 was
    /// NOT written — so a second writer's 60 stood under a diagnostic's "40 %".
    #[test]
    fn a_verify_lease_write_is_never_reconciled() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Verify);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        sysfs.external_write(60);
        let before = sysfs.duty_writes();
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(sysfs.duty_writes(), before + 1, "the repeat is written");
        assert_eq!(
            sysfs.live.get(PWM).as_deref(),
            Some(percent_to_raw(40).to_string().as_str()),
            "the header holds the duty the diagnostic asked for"
        );
        assert_eq!(published(&cache), DutyReconciliation::default());

        // Positive control: the same sequence under the engine's lease corrects.
        let (mut ctrl, sysfs, _cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        sysfs.external_write(60);
        let before = sysfs.duty_writes();
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(sysfs.duty_writes(), before + 1);
    }

    /// A header a diagnostic restored to the engine's last command agrees with it,
    /// so the engine's next tick writes and counts nothing.
    #[test]
    fn a_header_restored_by_a_diagnostic_is_not_corrected() {
        let (mut ctrl, sysfs, cache, engine) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 40, &engine).unwrap();
        let verify = ctrl
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::Verify)
            .lease_id;
        ctrl.set_pwm("h1", 70, &verify).unwrap();
        ctrl.set_pwm("h1", 40, &verify).unwrap();
        ctrl.lease_manager_mut().release_lease(&verify).unwrap();
        let engine = ctrl
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap()
            .lease_id;
        let before = sysfs.duty_writes();
        ctrl.set_pwm("h1", 40, &engine).unwrap();
        assert_eq!(sysfs.duty_writes(), before);
        assert_eq!(published(&cache).corrections, 0);
    }

    /// [SAFETY] A forced tick still writes (the force clears `manual_mode_set`
    /// first), and a forced write is not a correction.
    #[test]
    fn a_forced_write_still_writes_and_is_not_counted() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::ThermalSafety);
        ctrl.set_pwm("h1", 100, &lease).unwrap();
        let before = sysfs.duty_writes();
        ctrl.forget_manual_mode();
        ctrl.set_pwm("h1", 100, &lease).unwrap();
        assert_eq!(sysfs.duty_writes(), before + 1, "the forced tick writes");
        assert_eq!(published(&cache).corrections, 0);
    }

    /// it87's full-speed alias: at 100 % the chip reports mode 0 and a duty of
    /// 255. That is agreement, not drift and not a reclaim — no write, no flag.
    #[test]
    fn the_full_speed_alias_is_not_flagged() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 100, &lease).unwrap();
        sysfs.live.set(ENABLE, "0");
        sysfs.live.set(PWM, "255");
        let before = sysfs.duty_writes();
        for _ in 0..5 {
            ctrl.set_pwm("h1", 100, &lease).unwrap();
        }
        assert_eq!(sysfs.duty_writes(), before);
        assert!(!ctrl.duty_not_holding("h1"));
        assert_eq!(published(&cache), DutyReconciliation::default());
    }

    /// A BIOS reclaim still takes the full re-take path — the watchdog, not the
    /// drift check — and a re-take is not a correction.
    #[test]
    fn a_bios_reclaim_is_a_retake_not_a_correction() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        sysfs.live.set(ENABLE, "2");
        sysfs.external_write(80);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(sysfs.live.get(ENABLE).as_deref(), Some("1"), "re-taken");
        assert_eq!(
            sysfs.live.get(PWM).as_deref(),
            Some(percent_to_raw(40).to_string().as_str())
        );
        assert_eq!(ctrl.enable_revert_counts().get("h1"), Some(&1));
        assert_eq!(published(&cache).corrections, 0);
    }

    /// The flag describes a header the engine commands, so a hand-back or a
    /// profile deactivation clears it on the wire; the count is since boot.
    #[test]
    fn deactivation_and_hand_back_clear_the_flag_but_keep_the_count() {
        for hand_back in [false, true] {
            let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
            ctrl.set_pwm("h1", 20, &lease).unwrap();
            for _ in 0..4 {
                sysfs.external_write(60);
                ctrl.set_pwm("h1", 20, &lease).unwrap();
            }
            assert!(published(&cache).not_holding, "precondition");
            if hand_back {
                ctrl.hand_back("h1", &lease).unwrap();
            } else {
                ctrl.on_lease_released();
            }
            assert_eq!(
                published(&cache),
                DutyReconciliation {
                    corrections: 3,
                    not_holding: false
                },
                "hand_back = {hand_back}"
            );
        }
    }

    /// `PTA-k`: a hand-back that FAILS leaves the header taken but uncommanded,
    /// so the flag and the drift episode go exactly as on a released one — while
    /// the retry state stays. The second half is what discriminates: with a
    /// stale give-up, a profile naming the header again at the same duty HOLDS
    /// instead of correcting a second writer.
    #[test]
    fn a_failed_hand_back_clears_the_flag_but_keeps_the_retry_state() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 20, &lease).unwrap();
        for _ in 0..4 {
            sysfs.external_write(60);
            ctrl.set_pwm("h1", 20, &lease).unwrap();
        }
        assert!(published(&cache).not_holding, "precondition");
        assert!(ctrl.duty_not_holding("h1"), "precondition");

        *sysfs.fail_enable_writes.lock() = true;
        assert_eq!(
            ctrl.hand_back("h1", &lease).unwrap(),
            Some(HandBackOutcome::Failed),
            "precondition: nothing could be written"
        );

        assert_eq!(
            published(&cache),
            DutyReconciliation {
                corrections: 3,
                not_holding: false
            }
        );
        assert!(!ctrl.duty_not_holding("h1"));
        assert_eq!(ctrl.last_commanded_pct("h1"), Some(20), "retry state kept");

        *sysfs.fail_enable_writes.lock() = false;
        sysfs.external_write(60);
        let before = sysfs.duty_writes();
        ctrl.set_pwm("h1", 20, &lease).unwrap();
        assert_eq!(
            sysfs.duty_writes(),
            before + 1,
            "a fresh episode corrects the second writer"
        );
    }

    /// [SAFETY] The exit floor is unchanged: once it has latched, a correction
    /// rewrites the RAISED duty, so drift can never take a header with no mode
    /// switch below the duty the stop left it at (DEC-392).
    #[test]
    fn a_correction_after_the_exit_floor_rewrites_the_raised_duty() {
        let sysfs = DriftSysfs::default();
        sysfs.live.set(PWM, "0");
        let cache = Arc::new(StateCache::new());
        let mut ctrl = HwmonPwmController::new(
            vec![no_mode_header("h1", 1)],
            LeaseManager::new(),
            Box::new(sysfs.clone()),
            cache.clone(),
        );
        let lease = engine_lease(&mut ctrl);
        ctrl.set_pwm("h1", 30, &lease).unwrap();
        ctrl.apply_exit_floor(50);
        assert_eq!(ctrl.set_pwm("h1", 30, &lease).unwrap().pwm_percent, 50);

        sysfs.external_write(20);
        ctrl.set_pwm("h1", 30, &lease).unwrap();
        assert_eq!(
            sysfs.live.get(PWM).as_deref(),
            Some(percent_to_raw(50).to_string().as_str()),
            "the correction restores the exit duty, never the lower command"
        );
        assert_eq!(published(&cache).corrections, 1);
    }

    /// A correction whose write fails did not land, so it is not one that "did
    /// not hold": however many fail, the header is never flagged and the
    /// episode's first WARN is not repeated.
    ///
    /// Since DEC-420 (`TS-au`) a failed write also clears the header's mode
    /// flag, so the next tick RE-TAKES the header as a plain write rather than
    /// correcting it: that write restores the duty but is not counted, and it
    /// restarts the count (DEC-420 accepts the later give-up). A drift after it
    /// is corrected and counted as before.
    #[test]
    fn a_failed_correction_write_does_not_count_toward_the_give_up() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::Engine);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        *sysfs.fail_pwm_writes.lock() = true;
        for tick in 1..=5 {
            sysfs.external_write(60);
            assert!(
                ctrl.set_pwm("h1", 40, &lease).is_err(),
                "precondition: the correction write fails (tick {tick})"
            );
            assert!(!ctrl.duty_not_holding("h1"), "tick {tick}");
            let drift = ctrl.write_state["h1"].drift;
            assert_eq!(drift.corrections, 0, "tick {tick}");
            assert!(
                drift.in_episode,
                "the episode stays open, so no repeat WARN"
            );
        }
        assert_eq!(published(&cache), DutyReconciliation::default());

        *sysfs.fail_pwm_writes.lock() = false;
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(
            sysfs.live.get(PWM).as_deref(),
            Some(percent_to_raw(40).to_string().as_str()),
            "the re-take restores the duty"
        );
        assert_eq!(
            published(&cache).corrections,
            0,
            "a re-take after a failed write is a plain write, not a correction"
        );

        sysfs.external_write(60);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(
            sysfs.live.get(PWM).as_deref(),
            Some(percent_to_raw(40).to_string().as_str())
        );
        assert_eq!(published(&cache).corrections, 1, "a later drift is counted");
    }

    /// S2-2: after an emergency the engine adopts the thermal-safety lease and
    /// keeps renewing it (`backends.rs`), so its writes under THAT lease must be
    /// reconciled too — or drift correction would silently stop for the rest of
    /// the session after the first emergency.
    #[test]
    fn a_coalesced_write_under_an_adopted_thermal_safety_lease_is_reconciled() {
        let (mut ctrl, sysfs, cache, lease) = drift_controller(HwmonWriter::ThermalSafety);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        let before = sysfs.duty_writes();
        sysfs.external_write(60);
        ctrl.set_pwm("h1", 40, &lease).unwrap();
        assert_eq!(sysfs.duty_writes(), before + 1);
        assert_eq!(published(&cache).corrections, 1);
    }
}
