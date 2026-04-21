//! Hwmon PWM control with lease enforcement and safety floors.
//!
//! Writes `pwmN` sysfs files after validating lease, bounds, and mode.
//! All writes go through this module — no direct sysfs access elsewhere.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

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

/// Convert a PWM percent (0–100) to raw sysfs value (0–255).
fn percent_to_raw(percent: u8) -> u8 {
    ((percent as u16 * 255 + 50) / 100) as u8
}

/// Convert a raw sysfs PWM value (0–255) back to percent (0–100).
fn raw_to_percent(raw: u8) -> u8 {
    ((raw as u16 * 100 + 127) / 255) as u8
}

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

/// Controller for hwmon PWM writes with lease enforcement and write verification.
pub struct HwmonPwmController {
    headers: HashMap<String, PwmHeaderDescriptor>,
    lease_manager: LeaseManager,
    writer: Box<dyn SysfsWriter>,
    cache: Arc<StateCache>,
    /// Per-header write state for coalescing (reset on lease release).
    write_state: HashMap<String, HeaderWriteState>,
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

    /// Set PWM on a header. Requires a valid lease.
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

        // Look up header — extract needed fields to avoid cloning the full descriptor
        // (which includes PathBuf fields). The borrow can't be held across mutable self
        // accesses below, so we copy the individual fields we need.
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

        // No per-header minimum floor — thermal safety is centralized in ThermalSafetyRule (P2-S1)
        // Validate PWM range
        if pwm_percent > 100 {
            return Err(HwmonControlError::Validation(format!(
                "pwm_percent {pwm_percent} out of range (0–100)"
            )));
        }

        let effective_pct = pwm_percent;

        // Coalesce: skip if same as last commanded value and mode already set.
        let ws = self.write_state.entry(header_id.to_string()).or_default();
        if ws.manual_mode_set && ws.last_commanded_pct == Some(effective_pct) {
            // Update cache timestamp even on coalesced writes so staleness stays fresh.
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

        // Write pwm_enable only if not yet set during this lease.
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
    fn percent_to_raw_conversion() {
        assert_eq!(percent_to_raw(0), 0);
        assert_eq!(percent_to_raw(100), 255);
        assert_eq!(percent_to_raw(50), 128);
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
    fn raw_to_percent_conversion() {
        assert_eq!(raw_to_percent(0), 0);
        assert_eq!(raw_to_percent(255), 100);
        assert_eq!(raw_to_percent(128), 50);
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
}
