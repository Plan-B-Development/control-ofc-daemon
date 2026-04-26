//! Hwmon PWM control with lease enforcement and safety floors.
//!
//! Writes `pwmN` sysfs files after validating lease, bounds, and mode.
//! All writes go through this module — no direct sysfs access elsewhere.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Periodic INFO summary cadence for the pwm_enable watchdog log throttle.
/// First reclaim per header is logged at WARN, subsequent reverts at DEBUG,
/// and a single INFO line is emitted every `WATCHDOG_SUMMARY_INTERVAL`
/// reporting the delta and cumulative count. See `decide_watchdog_log_action`.
const WATCHDOG_SUMMARY_INTERVAL: Duration = Duration::from_secs(60);

use crate::error::HwmonError;
use crate::health::cache::StateCache;
use crate::health::state::HwmonFanState;
use crate::hwmon::lease::{LeaseError, LeaseManager};
use crate::hwmon::pwm_discovery::PwmHeaderDescriptor;

/// PWM enable mode: manual (1) allows direct PWM writes.
///
/// Standard hwmon pwmX_enable values:
///   0 = no control (fan at full speed)
///   1 = manual PWM control via sysfs (universally supported)
///   2 = automatic/thermal cruise (driver-specific)
///   3+ = driver-specific (e.g., NCT6775 Speed Cruise, Smart Fan III/IV)
///
/// This daemon only writes value 1 (manual), which is safe across all drivers.
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
}

impl std::fmt::Display for HwmonControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lease(e) => write!(f, "lease error: {e}"),
            Self::Validation(msg) => write!(f, "validation error: {msg}"),
            Self::Hardware(e) => write!(f, "hardware error: {e}"),
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
    /// Per-header log throttle state for the pwm_enable watchdog. Persists
    /// across leases so the first WARN is emitted at most once per controller
    /// lifetime per header — subsequent reverts collapse into periodic INFO
    /// summaries instead of one WARN per second.
    watchdog_log_state: HashMap<String, WatchdogLogState>,
}

impl HwmonPwmController {
    pub fn new(
        headers: Vec<PwmHeaderDescriptor>,
        lease_manager: LeaseManager,
        writer: Box<dyn SysfsWriter>,
        cache: Arc<StateCache>,
    ) -> Self {
        let header_map: HashMap<String, PwmHeaderDescriptor> =
            headers.into_iter().map(|h| (h.id.clone(), h)).collect();

        Self {
            headers: header_map,
            lease_manager,
            writer,
            cache,
            write_state: HashMap::new(),
            enable_revert_counts: HashMap::new(),
            watchdog_log_state: HashMap::new(),
        }
    }

    /// Get the list of discovered PWM headers.
    pub fn headers(&self) -> Vec<&PwmHeaderDescriptor> {
        let mut headers: Vec<_> = self.headers.values().collect();
        headers.sort_by_key(|h| (&h.chip_name, h.pwm_index));
        headers
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

        // Check for system resume — reset all manual mode flags
        if self.cache.resume_detected.swap(false, Ordering::Relaxed) {
            log::info!("Clearing manual mode flags after system resume");
            for ws in self.write_state.values_mut() {
                ws.manual_mode_set = false;
            }
        }

        // Look up header — extract needed fields to avoid cloning the full descriptor.
        let (pwm_path, enable_path, supports_enable, rpm_path) = {
            let h = self.headers.get(header_id).ok_or_else(|| {
                HwmonControlError::Validation(format!("unknown header: {header_id}"))
            })?;
            (
                h.pwm_path.clone(),
                h.enable_path.clone(),
                h.supports_enable,
                h.rpm_path.clone(),
            )
        };

        // Validate PWM range
        if pwm_percent > 100 {
            return Err(HwmonControlError::Validation(format!(
                "pwm_percent {pwm_percent} out of range (0–100)"
            )));
        }

        let effective_pct = pwm_percent;

        // ── pwm_enable watchdog ─────────────────────────────────────
        // When we believe manual mode is already set, read back pwm_enable
        // to detect BIOS/EC reclaim (Gigabyte SmartFan, MSI Smart Fan, etc.).
        let enable_reclaimed = if supports_enable {
            let mode_set = self
                .write_state
                .get(header_id)
                .is_some_and(|ws| ws.manual_mode_set);
            if mode_set {
                enable_path
                    .as_ref()
                    .and_then(|ep| {
                        self.writer
                            .read_file(ep)
                            .ok()
                            .and_then(|s| s.trim().parse::<u8>().ok())
                    })
                    .is_some_and(|v| v != 1)
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

        // Coalesce: skip if same as last commanded value and mode still set.
        if ws.manual_mode_set && ws.last_commanded_pct == Some(effective_pct) {
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
                updated_at: now,
            }]);
            return Ok(HwmonSetPwmResult {
                header_id: header_id.to_string(),
                pwm_percent: effective_pct,
                raw_value: percent_to_raw(effective_pct),
            });
        }

        // Write pwm_enable if not yet set (or if BIOS reclaimed it).
        if !ws.manual_mode_set && supports_enable {
            if let Some(ref ep) = enable_path {
                self.writer
                    .write_file(ep, PWM_ENABLE_MANUAL)
                    .map_err(HwmonControlError::Hardware)?;
            }
        }

        // Write PWM value
        let raw = percent_to_raw(effective_pct);
        self.writer
            .write_file(&pwm_path, &raw.to_string())
            .map_err(HwmonControlError::Hardware)?;

        // Verify write: read back and compare (best-effort)
        match self.writer.read_file(&pwm_path) {
            Ok(raw_str) => {
                if let Ok(actual_raw) = raw_str.trim().parse::<u8>() {
                    if actual_raw != raw {
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
            updated_at: now,
        }]);

        Ok(HwmonSetPwmResult {
            header_id: header_id.to_string(),
            pwm_percent: effective_pct,
            raw_value: raw,
        })
    }

    /// Called when a lease is released. Resets coalescing state so the next
    /// lease holder gets a fresh pwm_enable write on their first set_pwm().
    pub fn on_lease_released(&mut self) {
        self.write_state.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::lease::LeaseManager;
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
        let result = ctrl.set_pwm("h1", 10, &lease.lease_id).unwrap();

        assert_eq!(result.pwm_percent, 10); // no clamping
    }

    #[test]
    fn set_pwm_cpu_header_allows_zero() {
        // CPU headers no longer have special floor — safety is centralized
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CPU_FAN", 0)]);

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
        let result = ctrl.set_pwm("h1", 0, &lease.lease_id).unwrap();
        assert_eq!(result.pwm_percent, 0);
    }

    #[test]
    fn set_pwm_chassis_allows_zero() {
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
        let result = ctrl.set_pwm("h1", 0, &lease.lease_id).unwrap();
        assert_eq!(result.pwm_percent, 0);
    }

    #[test]
    fn set_pwm_unknown_header() {
        let (mut ctrl, _writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
        let err = ctrl.set_pwm("h1", 200, &lease.lease_id).unwrap_err();

        match err {
            HwmonControlError::Validation(msg) => {
                assert!(msg.contains("out of range"));
            }
            _ => panic!("expected validation error"),
        }
    }

    #[test]
    fn set_pwm_updates_cache() {
        let (mut ctrl, _writes, cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 20)]);

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
        let lease_id = lease.lease_id.clone();
        ctrl.set_pwm("h1", 50, &lease_id).unwrap();

        // Release the lease properly, then reset coalescing state
        ctrl.lease_manager_mut().release_lease(&lease_id).unwrap();
        ctrl.on_lease_released();

        // Take new lease and write again — should set enable mode again
        let lease2 = ctrl.lease_manager_mut().take_lease("gui2").unwrap();
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
            .take_lease("gui")
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap();
        ctrl.set_pwm("h1", 50, &lease.lease_id).unwrap(); // coalesced
        ctrl.set_pwm("h1", 75, &lease.lease_id).unwrap(); // different

        let writes = writes.lock();
        // enable(1) + pwm(50) + pwm(75) = 3 writes total
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[2].1, percent_to_raw(75).to_string());
    }

    #[test]
    fn on_lease_released_resets_coalescing() {
        // After lease release + new lease → enable written on first call again
        let (mut ctrl, writes, _cache) = setup_controller(vec![make_header("h1", "CHA_FAN1", 0)]);

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
        let lid = lease.lease_id.clone();
        ctrl.set_pwm("h1", 50, &lid).unwrap(); // enable + pwm

        ctrl.lease_manager_mut().release_lease(&lid).unwrap();
        ctrl.on_lease_released();

        let lease2 = ctrl.lease_manager_mut().take_lease("gui2").unwrap();
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
            .take_lease("gui")
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();

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

    #[test]
    fn watchdog_no_revert_when_enable_stays_manual() {
        // pwm_enable reads "1" — no revert detected, normal coalescing.
        let (mut ctrl, writes, _cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "1");

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
        let lid = lease.lease_id.clone();
        ctrl.set_pwm("h1", 50, &lid).unwrap();
        ctrl.set_pwm("h1", 60, &lid).unwrap(); // triggers revert

        ctrl.lease_manager_mut().release_lease(&lid).unwrap();
        ctrl.on_lease_released();

        let lease2 = ctrl.lease_manager_mut().take_lease("gui2").unwrap();
        ctrl.set_pwm("h1", 70, &lease2.lease_id).unwrap();
        ctrl.set_pwm("h1", 80, &lease2.lease_id).unwrap(); // triggers revert again

        assert_eq!(ctrl.enable_revert_counts().get("h1"), Some(&2));
    }

    #[test]
    fn resume_flag_resets_manual_mode() {
        let (mut ctrl, writes, cache) =
            setup_controller_with_enable(vec![make_header("h1", "CHA_FAN1", 0)], "1");

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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

        let lease = ctrl.lease_manager_mut().take_lease("gui").unwrap();
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
}
