//! Per-backend write paths for the profile engine (DEC-135).
//!
//! Each fan-control backend (OpenFan serial, AMD GPU PMFW, motherboard
//! hwmon) implements [`WriteBackend`]. ALL per-backend gating — write
//! coalescing/thresholds, failure caching, lease handling — lives behind
//! `apply`, so each rule exists in exactly one place per backend. The
//! engine loop is reduced to: safety tick → profile evaluation → `apply`
//! per backend. (The GUI-deferral gate was removed at 2.0.0 — DEC-165.)
//!
//! Backends that participate in forced safety writes (thermal emergency,
//! no-CPU-sensor fallback) additionally implement [`SafetyWriteBackend`].
//! [`GpuBackend`] deliberately does NOT (DEC-130): there is no GPU
//! emergency threshold. AMD PMFW firmware owns GPU thermal protection
//! (junction-temp throttling, firmware fan ramp) independently of OS fan
//! control, and forcing PMFW curve commits from a CPU emergency would add
//! SMU churn without improving GPU safety.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use super::PwmCommand;
use crate::constants;
use crate::health::cache::StateCache;
use crate::serial::protocol::NUM_CHANNELS;

/// One fan-control backend the profile engine writes through.
///
/// To add a backend: implement this trait, give the implementation sole
/// ownership of its gating rules (coalescing, failure caching, lease),
/// and call it from the loop's apply sequence in `profile_engine_loop`.
pub(crate) trait WriteBackend {
    /// Backend name for logs.
    #[allow(dead_code)] // part of the backend contract; used in tests/logs
    fn name(&self) -> &'static str;

    /// Apply this backend's share of the profile commands.
    ///
    /// The engine is the sole authoritative writer (DEC-165): there is no GUI
    /// deferral. Each backend still owns its coalescing, failure caching, and
    /// lease handling behind this call.
    async fn apply(&mut self, commands: &[PwmCommand]);
}

/// Backends that participate in forced safety writes (thermal emergency /
/// no-sensor fallback). `force_all` drives every output to `pct`
/// unconditionally — no GUI deferral, no coalescing shortcuts beyond the
/// controller's own exact-match skip.
pub(crate) trait SafetyWriteBackend: WriteBackend {
    /// Async since DEC-146 P3-8: implementations run their blocking
    /// serial/sysfs writes on the blocking pool instead of pinning a tokio
    /// worker for up to `channels × serial-timeout` during an emergency.
    async fn force_all(&mut self, pct: u8);
}

// ─── OpenFan (serial) ────────────────────────────────────────────────────

pub(crate) struct OpenFanBackend {
    ctrl: Arc<Mutex<crate::serial::controller::FanController>>,
    /// Consecutive write failures for P0-R2 safety alerting.
    consecutive_failures: u32,
}

impl OpenFanBackend {
    pub(crate) fn new(ctrl: Arc<Mutex<crate::serial::controller::FanController>>) -> Self {
        Self {
            ctrl,
            consecutive_failures: 0,
        }
    }
}

impl WriteBackend for OpenFanBackend {
    fn name(&self) -> &'static str {
        "openfan"
    }

    /// OpenFan writes (serial I/O on the blocking pool — lock per command).
    ///
    /// Exact-match coalescing lives below this in `serial::controller`.
    ///
    /// DEC-146 P3-8: serial writes block up to the configured timeout
    /// (500 ms default) per channel, so the batch runs on `spawn_blocking`
    /// (matching `GpuBackend::apply` and both poll loops) instead of pinning
    /// a tokio worker. The mutex is still taken per command (DEC-099) so
    /// concurrent API requests interleave exactly as before.
    async fn apply(&mut self, commands: &[PwmCommand]) {
        let chans: Vec<(u8, u8)> = commands
            .iter()
            .filter(|c| c.source == "openfan")
            .filter_map(|cmd| {
                let Some(ch_str) = cmd.member_id.strip_prefix("openfan:ch") else {
                    log::warn!(
                        "Profile engine: dropping openfan command with malformed member_id: {:?}",
                        cmd.member_id
                    );
                    return None;
                };
                let Ok(ch) = ch_str.parse::<u8>() else {
                    log::warn!(
                        "Profile engine: dropping openfan command with unparseable channel: {:?}",
                        cmd.member_id
                    );
                    return None;
                };
                Some((ch, cmd.pwm_percent))
            })
            .collect();
        if chans.is_empty() {
            return;
        }
        let ctrl = self.ctrl.clone();
        let join = tokio::task::spawn_blocking(move || {
            chans
                .into_iter()
                .map(|(ch, pct)| {
                    // Lock per command (DEC-099) so GUI API requests can
                    // interleave between channel writes.
                    let mut guard = ctrl.lock();
                    let res = guard
                        .set_pwm(ch, pct)
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    (ch, res)
                })
                .collect::<Vec<(u8, Result<(), String>)>>()
        })
        .await;
        let results = match join {
            Ok(results) => results,
            Err(e) => {
                // Concurrency review D3: a panic inside the blocking task
                // must not be silent — count it and alert immediately.
                self.consecutive_failures += 1;
                log::error!(
                    "SAFETY: Profile engine OpenFan write task panicked: {e} \
                     ({} consecutive failures)",
                    self.consecutive_failures
                );
                return;
            }
        };
        // `&mut self` state can't cross into the 'static closure — apply the
        // failure bookkeeping to the returned results instead.
        for (ch, res) in results {
            if let Err(e) = res {
                self.consecutive_failures += 1;
                let n = self.consecutive_failures;
                log::warn!("Profile engine: OpenFan ch{ch} write failed ({n} consecutive): {e}");
                if n == 5 {
                    log::error!(
                        "SAFETY: OpenFan serial link appears down ({n} consecutive write failures)"
                    );
                }
            } else {
                self.consecutive_failures = 0;
            }
        }
    }
}

impl SafetyWriteBackend for OpenFanBackend {
    /// Force every OpenFan channel to `pct`.
    ///
    /// DEC-099: drop the lock between channels so GUI requests can
    /// interleave during a long emergency scan; if the GUI overrides a
    /// safety value briefly, the next 1Hz tick re-asserts the forced value.
    ///
    /// DEC-146 P3-8: runs on the blocking pool — worst case is
    /// `NUM_CHANNELS × serial-timeout` (10 × 500 ms default), far too long
    /// to pin a tokio worker during a thermal emergency.
    async fn force_all(&mut self, pct: u8) {
        let ctrl = self.ctrl.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            for ch in 0..NUM_CHANNELS {
                let mut guard = ctrl.lock();
                if let Err(e) = guard.set_pwm(ch, pct) {
                    log::error!("THERMAL SAFETY: OpenFan ch{ch} write FAILED: {e}");
                }
            }
        })
        .await
        {
            // Concurrency review D3: never swallow a panicked safety write —
            // the next 1 Hz tick retries, but the operator must see this.
            log::error!("THERMAL SAFETY: OpenFan force_all task panicked: {e}");
        }
    }
}

// ─── AMD GPU (PMFW fan_curve / legacy pwm1) ──────────────────────────────

/// GPU fan writes via the PMFW `fan_curve` interface.
///
/// Deliberately NOT a [`SafetyWriteBackend`] (DEC-130) — see the module
/// docs. GPU thermal protection is the firmware's job.
pub(crate) struct GpuBackend {
    cache: Arc<StateCache>,
    gpu_infos: Arc<Vec<crate::hwmon::gpu_detect::AmdGpuInfo>>,
    /// GPU writes that failed — skip retry until the speed changes or a
    /// cooldown elapses. Prevents 1/sec journal spam when PMFW rejects the
    /// value. Key: fan_id, Value: (failed_speed_pct, failure_instant).
    fail_cache: HashMap<String, (u8, std::time::Instant)>,
}

impl GpuBackend {
    pub(crate) fn new(
        cache: Arc<StateCache>,
        gpu_infos: Arc<Vec<crate::hwmon::gpu_detect::AmdGpuInfo>>,
    ) -> Self {
        Self {
            cache,
            gpu_infos,
            fail_cache: HashMap::new(),
        }
    }

    #[cfg(test)]
    fn fail_cache_len(&self) -> usize {
        self.fail_cache.len()
    }
}

impl WriteBackend for GpuBackend {
    fn name(&self) -> &'static str {
        "amd_gpu"
    }

    /// GPU fan writes (async via spawn_blocking, no lease required).
    ///
    /// Suppresses writes whose delta from the last commanded value is below
    /// `GPU_COALESCE_DELTA_PCT`, mirroring the API handler so headless and
    /// imperative paths share DEC-070's single 5% threshold (DEC-131).
    async fn apply(&mut self, commands: &[PwmCommand]) {
        // One snapshot per tick — advisory write-suppression state, not
        // correctness-critical (a torn read vs. the API path is harmless:
        // the next tick re-evaluates).
        let gpu_fans = self.cache.gpu_fans_snapshot();

        for cmd in commands.iter().filter(|c| c.source == "amd_gpu") {
            // DEC-165: skip a GPU fan the operator relinquished to firmware-auto
            // via POST /gpu/{id}/fan/reset, so the reset is durable under an
            // active profile (the set is cleared on the next profile activation).
            if self.cache.is_gpu_fan_relinquished(&cmd.member_id) {
                continue;
            }
            if let Some(cached) = gpu_fans.get(&cmd.member_id) {
                if let Some(last_pct) = cached.last_commanded_pct {
                    let delta = (cmd.pwm_percent as i16 - last_pct as i16).unsigned_abs();
                    if delta < constants::GPU_COALESCE_DELTA_PCT {
                        continue;
                    }
                }
            }

            // Failure suppression: skip if the same speed already failed
            // recently.
            if let Some((failed_pct, failed_at)) = self.fail_cache.get(&cmd.member_id) {
                if *failed_pct == cmd.pwm_percent
                    && failed_at.elapsed() < constants::GPU_FAIL_COOLDOWN
                {
                    continue;
                }
            }

            let Some(bdf) = cmd.member_id.strip_prefix("amd_gpu:") else {
                continue;
            };
            let Some(gpu) = self.gpu_infos.iter().find(|g| g.pci_bdf == bdf) else {
                continue;
            };
            let Some(ref curve_path) = gpu.fan_curve_path else {
                continue;
            };

            let path = curve_path.clone();
            let zero_rpm = gpu.fan_zero_rpm_path.clone();
            let pct = cmd.pwm_percent;
            let preserve_zero_rpm = cmd.gpu_fan_zero_rpm;
            let cache_ref = self.cache.clone();
            let fan_id = cmd.member_id.clone();
            let fan_id_inner = fan_id.clone();
            let result = tokio::task::spawn_blocking(move || {
                match crate::hwmon::gpu_fan::set_static_speed_with_zero_rpm(
                    &path,
                    zero_rpm.as_deref(),
                    pct,
                    constants::GPU_PMFW_NUM_CURVE_POINTS,
                    preserve_zero_rpm,
                ) {
                    Ok(()) => {
                        cache_ref.set_gpu_fan_commanded_pct(&fan_id_inner, pct);
                        Ok(())
                    }
                    Err(e) => {
                        log::warn!("GPU fan write failed: {e}");
                        Err(())
                    }
                }
            })
            .await;

            match result {
                Ok(Ok(())) => {
                    self.fail_cache.remove(&fan_id);
                }
                _ => {
                    self.fail_cache
                        .insert(fan_id, (cmd.pwm_percent, std::time::Instant::now()));
                }
            }
        }
    }
}

// ─── Motherboard hwmon ───────────────────────────────────────────────────

pub(crate) struct HwmonBackend {
    ctrl: Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>,
}

impl HwmonBackend {
    pub(crate) fn new(ctrl: Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>) -> Self {
        Self { ctrl }
    }
}

impl WriteBackend for HwmonBackend {
    fn name(&self) -> &'static str {
        "hwmon"
    }

    /// hwmon writes (auto-lease for headless profile mode).
    ///
    /// The profile engine auto-acquires the lease when writing hwmon members
    /// and is the steady-state holder (DEC-165 — the GUI no longer takes the
    /// lease). DEC-146 P3-8: the body runs on the blocking pool (matching the
    /// hwmon poll loop). DEC-154: the lease-acquire → per-header write → renew
    /// sequence locks the controller mutex PER COMMAND (like `force_all` and
    /// `OpenFanBackend`), not once for the whole batch, so concurrent API
    /// requests are not starved for the duration of a multi-header tick. A
    /// thermal force-take mid-scan fails the remaining writes with InvalidLease;
    /// the next 1 Hz tick re-acquires.
    async fn apply(&mut self, commands: &[PwmCommand]) {
        let hwmon_cmds: Vec<(String, u8)> = commands
            .iter()
            .filter(|c| c.source == "hwmon")
            .map(|c| (c.member_id.clone(), c.pwm_percent))
            .collect();
        if hwmon_cmds.is_empty() {
            return;
        }
        let ctrl = self.ctrl.clone();
        let join = tokio::task::spawn_blocking(move || {
            // Phase 1: acquire (or reuse) the profile-engine lease under a brief
            // lock, then release it so concurrent API requests can interleave
            // with the per-header writes below.
            let lease_id: Option<String> = {
                let mut guard = ctrl.lock();
                let existing = {
                    let mgr = guard.lease_manager();
                    // Reuse any active lease — the engine's own, or a transient
                    // verify/thermal-safety force-take. The engine is the sole
                    // writer (DEC-165); there is no GUI lease to yield to (and
                    // during a verify the engine is paused, so it never reaches
                    // here while the "verify" lease is held). `None` ⇒ acquire.
                    mgr.active_lease().map(|lease| lease.lease_id.clone())
                };
                existing.or_else(|| {
                    guard
                        .lease_manager_mut()
                        .take_lease("profile-engine")
                        .ok()
                        .map(|l| l.lease_id)
                })
            };
            let Some(lease_id) = lease_id else { return };

            // Phase 2: one lock per header (DEC-154) so a concurrent reader or
            // lease op is not starved for the whole batch. A GUI/thermal
            // force-take mid-scan fails the remaining writes with InvalidLease;
            // the next 1 Hz tick re-acquires.
            for (member_id, pwm_percent) in &hwmon_cmds {
                let mut guard = ctrl.lock();
                // DEC-102 backstop on the engine path: never attempt a write to
                // a header discovered read-only (`is_writable == false`). The GUI
                // member-picker and profile load drop these, but the un-validated
                // boot-load path does not, so the engine must skip them itself —
                // otherwise every tick would EACCES-spam the log and the member
                // would silently never take effect.
                if guard.header(member_id).is_some_and(|h| !h.is_writable) {
                    continue;
                }
                if let Err(e) = guard.set_pwm(member_id, *pwm_percent, &lease_id) {
                    log::warn!("hwmon write failed for {member_id}: {e}");
                }
            }

            // Phase 3: renew under a brief lock to keep it alive for next cycle.
            if let Err(e) = ctrl.lock().lease_manager_mut().renew_lease(&lease_id) {
                log::debug!("lease renewal failed (will re-acquire next cycle): {e}");
            }
        })
        .await;
        if let Err(e) = join {
            // Concurrency review D3: surface panicked write tasks.
            log::error!("Profile engine: hwmon write task panicked: {e}");
        }
    }
}

impl SafetyWriteBackend for HwmonBackend {
    /// Force every hwmon header to `pct` (auto-lease for safety writes).
    ///
    /// Takes the lease once (force-take as "thermal-safety"), then re-locks
    /// per header so concurrent GUI activity can proceed between writes
    /// (DEC-099). If a GUI request force-takes the lease mid-scan, writes
    /// fail with InvalidLease until the next 1Hz tick re-acquires it — same
    /// safety net as the OpenFan path.
    /// DEC-146 P3-8: runs on the blocking pool; the take-once /
    /// re-lock-per-header structure (DEC-099) is preserved verbatim inside
    /// the closure.
    async fn force_all(&mut self, pct: u8) {
        let ctrl = self.ctrl.clone();
        let join = tokio::task::spawn_blocking(move || {
            let (hdr_ids, lease_id) = {
                let mut guard = ctrl.lock();
                let hdr_ids: Vec<String> = guard.headers().iter().map(|h| h.id.clone()).collect();
                let lease_id = guard
                    .lease_manager_mut()
                    .force_take_lease("thermal-safety")
                    .lease_id;
                // Audit P1-E: a force-take is an ownership change the previous
                // holder was never notified of, so its coalescing state
                // (manual_mode_set) is stale. Reset it so thermal safety
                // unconditionally re-asserts pwm_enable=1 on its first forced
                // write — defense in depth alongside the per-write readback
                // watchdog in HwmonPwmController::set_pwm.
                guard.on_lease_released();
                (hdr_ids, lease_id)
            };
            for hdr_id in &hdr_ids {
                let mut guard = ctrl.lock();
                if let Err(e) = guard.set_pwm(hdr_id, pct, &lease_id) {
                    log::error!("THERMAL SAFETY: hwmon {hdr_id} write FAILED: {e}");
                }
            }
        })
        .await;
        if let Err(e) = join {
            // Concurrency review D3: never swallow a panicked safety write.
            log::error!("THERMAL SAFETY: hwmon force_all task panicked: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::HwmonError;
    use crate::hwmon::lease::LeaseManager;
    use crate::hwmon::pwm_control::{HwmonPwmController, SysfsWriter};
    use crate::hwmon::pwm_discovery::PwmHeaderDescriptor;

    fn cmd(member_id: &str, source: &str, pct: u8) -> PwmCommand {
        PwmCommand {
            member_id: member_id.into(),
            source: source.into(),
            pwm_percent: pct,
            gpu_fan_zero_rpm: false,
        }
    }

    // ── GPU backend ──────────────────────────────────────────────────

    fn fake_gpu(
        dir: &tempfile::TempDir,
    ) -> (crate::hwmon::gpu_detect::AmdGpuInfo, std::path::PathBuf) {
        let curve_path = dir.path().join("fan_curve");
        std::fs::write(&curve_path, "").unwrap();
        let gpu = crate::hwmon::gpu_detect::AmdGpuInfo {
            pci_bdf: "0000:03:00.0".into(),
            pci_device_id: 0x7550,
            pci_revision: 0xC0,
            pci_class: 0x030000,
            marketing_name: Some("RX 9070 XT".into()),
            hwmon_path: dir.path().to_path_buf(),
            fan_curve_path: Some(curve_path.clone()),
            fan_zero_rpm_path: None,
            is_discrete: true,
            has_fan_rpm: false,
            has_pwm: false,
            has_pwm_enable: false,
            overdrive_enabled: true,
        };
        (gpu, curve_path)
    }

    #[tokio::test]
    async fn gpu_backend_suppresses_below_threshold_and_writes_at_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = Arc::new(StateCache::new());
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:03:00.0", 60);
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        // delta 4 < 5 → suppressed (DEC-131).
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 64)])
            .await;
        assert!(std::fs::read_to_string(&curve_path).unwrap().is_empty());

        // delta 5 ≥ 5 → written, cache updated.
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 65)])
            .await;
        assert!(!std::fs::read_to_string(&curve_path).unwrap().is_empty());
        assert_eq!(
            cache
                .gpu_fans_snapshot()
                .get("amd_gpu:0000:03:00.0")
                .and_then(|f| f.last_commanded_pct),
            Some(65)
        );
    }

    #[tokio::test]
    async fn gpu_backend_caches_failures_and_clears_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let (mut gpu, curve_path) = fake_gpu(&dir);
        // Point the curve at a non-existent directory so the write fails.
        let bad_path = dir.path().join("missing").join("fan_curve");
        gpu.fan_curve_path = Some(bad_path);
        let cache = Arc::new(StateCache::new());
        let mut be = GpuBackend::new(cache, Arc::new(vec![gpu.clone()]));

        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert_eq!(be.fail_cache_len(), 1, "failed write must be cached");

        // Same speed within the cooldown → suppressed (no second attempt
        // visible; the cache entry persists).
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert_eq!(be.fail_cache_len(), 1);

        // Repair the path via a fresh backend pointing at the good file —
        // a successful write clears the failure cache.
        let mut gpu_ok = gpu;
        gpu_ok.fan_curve_path = Some(curve_path);
        let cache = Arc::new(StateCache::new());
        let mut be = GpuBackend::new(cache, Arc::new(vec![gpu_ok]));
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert_eq!(be.fail_cache_len(), 0);
    }

    // ── hwmon backend ────────────────────────────────────────────────

    type WriteLog = Arc<Mutex<Vec<(String, String)>>>;

    struct TestWriter {
        writes: WriteLog,
    }

    impl SysfsWriter for TestWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes.lock().push((path.into(), value.into()));
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, HwmonError> {
            Ok("128\n".into())
        }
    }

    fn make_header(id: &str) -> PwmHeaderDescriptor {
        PwmHeaderDescriptor {
            id: id.to_string(),
            label: "CHA_FAN1".to_string(),
            chip_name: "it8696".to_string(),
            device_id: "it87.2624".to_string(),
            pwm_index: 1,
            supports_enable: true,
            pwm_path: "/sys/class/hwmon/hwmon0/pwm1".to_string(),
            enable_path: Some("/sys/class/hwmon/hwmon0/pwm1_enable".to_string()),
            rpm_available: false,
            rpm_path: None,
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: None,
            is_aio: false,
        }
    }

    fn hwmon_backend(headers: Vec<PwmHeaderDescriptor>) -> (HwmonBackend, WriteLog) {
        let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
        let writer = TestWriter {
            writes: writes.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(headers, LeaseManager::new(), Box::new(writer), cache);
        (HwmonBackend::new(Arc::new(Mutex::new(ctrl))), writes)
    }

    /// Like `TestWriter`, but reports `pwm_enable` as already manual (`1`) so the
    /// per-write readback watchdog in `set_pwm` does NOT fire. This isolates the
    /// thermal force-take coalescing reset (audit P1-E) from the watchdog, which
    /// would otherwise re-assert manual mode regardless.
    struct EnableManualWriter {
        writes: WriteLog,
    }

    impl SysfsWriter for EnableManualWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes.lock().push((path.into(), value.into()));
            Ok(())
        }
        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            if path.ends_with("_enable") {
                Ok("1\n".into())
            } else {
                Ok("128\n".into())
            }
        }
    }

    /// Like `make_header`, but at a distinct sysfs path per `index` so each
    /// header's write is independently observable in the write log.
    fn make_header_idx(id: &str, index: u8) -> PwmHeaderDescriptor {
        PwmHeaderDescriptor {
            id: id.to_string(),
            label: "CHA_FAN1".to_string(),
            chip_name: "it8696".to_string(),
            device_id: "it87.2624".to_string(),
            pwm_index: index,
            supports_enable: true,
            pwm_path: format!("/sys/class/hwmon/hwmon0/pwm{index}"),
            enable_path: Some(format!("/sys/class/hwmon/hwmon0/pwm{index}_enable")),
            rpm_available: false,
            rpm_path: None,
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: None,
            is_aio: false,
        }
    }

    #[tokio::test]
    async fn hwmon_apply_writes_every_header_in_batch() {
        // DEC-154 per-command locking must still write EVERY header in a batch
        // (none dropped by the per-header re-lock) and auto-acquire the lease.
        let (mut be, writes) = hwmon_backend(vec![
            make_header_idx("hwmon:it8696:pwm1", 1),
            make_header_idx("hwmon:it8696:pwm2", 2),
            make_header_idx("hwmon:it8696:pwm3", 3),
        ]);

        be.apply(&[
            cmd("hwmon:it8696:pwm1", "hwmon", 40),
            cmd("hwmon:it8696:pwm2", "hwmon", 55),
            cmd("hwmon:it8696:pwm3", "hwmon", 70),
        ])
        .await;

        let w = writes.lock();
        for pwm in ["pwm1", "pwm2", "pwm3"] {
            assert!(
                w.iter().any(|(p, _)| p.ends_with(pwm)),
                "per-command apply must write header {pwm}; got {w:?}"
            );
        }
        drop(w);

        // The profile engine auto-acquired the lease and still holds it.
        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert!(
            lease.is_some_and(|l| l.owner_hint == "profile-engine"),
            "profile-engine lease must be active after apply"
        );
    }

    #[tokio::test]
    async fn force_all_reasserts_manual_mode_after_engine_write() {
        // Engine controls a header first (manual_mode_set = true), then thermal
        // safety force-takes the lease. The readback watchdog is held off (enable
        // reads back as 1), so WITHOUT the P1-E reset the stale manual_mode_set
        // would make thermal safety SKIP the pwm_enable write. With the reset it
        // must re-assert pwm_enable=1 on the forced write.
        let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
        let writer = EnableManualWriter {
            writes: writes.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(
            vec![make_header("hwmon:it8696:pwm1")],
            LeaseManager::new(),
            Box::new(writer),
            cache,
        );
        let mut be = HwmonBackend::new(Arc::new(Mutex::new(ctrl)));

        // 1. Engine controls the header → first pwm_enable=1 write.
        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 40)]).await;
        // 2. Thermal safety force-takes the lease and forces 100%.
        be.force_all(100).await;

        let enable_writes = writes
            .lock()
            .iter()
            .filter(|(p, v)| p.ends_with("_enable") && v.trim() == "1")
            .count();
        assert_eq!(
            enable_writes, 2,
            "thermal force-take must reset coalescing so pwm_enable=1 is \
             re-asserted on the forced write (audit P1-E)"
        );
    }

    #[tokio::test]
    async fn hwmon_backend_auto_leases_and_writes() {
        let (mut be, writes) = hwmon_backend(vec![make_header("hwmon:it8696:pwm1")]);

        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;

        let w = writes.lock();
        assert!(
            w.iter().any(|(p, _)| p.ends_with("pwm1")),
            "expected a pwm write after auto-lease; got {w:?}"
        );
        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(lease.map(|l| l.owner_hint), Some("profile-engine".into()));
    }

    #[tokio::test]
    async fn hwmon_apply_skips_read_only_header() {
        // DEC-102 engine-path backstop: a read-only header (is_writable=false)
        // must be skipped, never EACCES-spammed; a writable sibling still writes.
        let mut ro = make_header_idx("hwmon:it8696:pwm1", 1);
        ro.is_writable = false;
        let rw = make_header_idx("hwmon:it8696:pwm2", 2);
        let (mut be, writes) = hwmon_backend(vec![ro, rw]);

        be.apply(&[
            cmd("hwmon:it8696:pwm1", "hwmon", 40),
            cmd("hwmon:it8696:pwm2", "hwmon", 55),
        ])
        .await;

        let w = writes.lock();
        assert!(
            !w.iter().any(|(p, _)| p.ends_with("pwm1")),
            "read-only header must be skipped (no write); got {w:?}"
        );
        assert!(
            w.iter().any(|(p, _)| p.ends_with("pwm2")),
            "writable sibling must still be written; got {w:?}"
        );
    }

    #[tokio::test]
    async fn gpu_backend_skips_relinquished_fan() {
        // DEC-165: a GPU fan relinquished to firmware-auto via reset must be
        // skipped by the engine so the reset is durable under an active profile.
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = Arc::new(StateCache::new());
        cache.relinquish_gpu_fan("amd_gpu:0000:03:00.0");
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "engine must not write a relinquished GPU fan (DEC-165)"
        );

        // Clearing the relinquish (e.g. on profile activation) resumes control.
        cache.clear_relinquished_gpu_fans();
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            !std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "engine must resume writing after the relinquish is cleared"
        );
    }

    #[tokio::test]
    async fn hwmon_force_all_takes_thermal_safety_lease_and_writes_every_header() {
        let (mut be, writes) = hwmon_backend(vec![
            make_header("hwmon:it8696:pwm1"),
            make_header("hwmon:it8696:pwm2"),
        ]);
        // Even a GUI-held lease is force-taken for safety writes.
        be.ctrl.lock().lease_manager_mut().force_take_lease("gui");

        be.force_all(100).await;

        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(lease.map(|l| l.owner_hint), Some("thermal-safety".into()));
        let w = writes.lock();
        let pwm_writes: Vec<_> = w.iter().filter(|(p, _)| p.ends_with("pwm1")).collect();
        assert!(
            !pwm_writes.is_empty(),
            "expected forced pwm writes; got {w:?}"
        );
    }

    // ── OpenFan backend ──────────────────────────────────────────────

    struct SerialMock {
        written: Arc<Mutex<Vec<String>>>,
    }

    impl crate::serial::transport::SerialTransport for SerialMock {
        fn write_line(&mut self, data: &str) -> Result<(), crate::error::SerialError> {
            self.written.lock().push(data.to_string());
            Ok(())
        }
        fn read_line(
            &mut self,
            _timeout: std::time::Duration,
        ) -> Result<String, crate::error::SerialError> {
            Ok("OK".into())
        }
    }

    fn openfan_backend() -> (OpenFanBackend, Arc<Mutex<Vec<String>>>) {
        let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let transport = SerialMock {
            written: written.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache,
            std::time::Duration::from_millis(100),
        );
        (OpenFanBackend::new(Arc::new(Mutex::new(ctrl))), written)
    }

    #[tokio::test]
    async fn openfan_backend_drops_malformed_member_ids() {
        let (mut be, written) = openfan_backend();

        be.apply(&[
            cmd("openfan:chXX", "openfan", 50),
            cmd("not-a-channel", "openfan", 50),
        ])
        .await;

        assert!(written.lock().is_empty());
    }

    #[tokio::test]
    async fn openfan_backend_writes_when_gui_inactive() {
        let (mut be, written) = openfan_backend();

        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;

        let w = written.lock();
        assert!(
            w.iter().any(|c| c.starts_with(">02")),
            "expected a SetPwm command; got {w:?}"
        );
    }

    #[tokio::test]
    async fn openfan_force_all_writes_every_channel() {
        let (mut be, written) = openfan_backend();

        be.force_all(100).await;

        let w = written.lock();
        let set_pwm: Vec<_> = w.iter().filter(|c| c.starts_with(">02")).collect();
        assert_eq!(
            set_pwm.len(),
            NUM_CHANNELS as usize,
            "expected one forced SetPwm per channel; got {w:?}"
        );
    }

    /// A toggleable serial transport: `write_line` fails (link "vanished")
    /// while `fail` is set, otherwise records the write. Models an OpenFan
    /// controller being unplugged and re-plugged at runtime.
    struct FlakySerial {
        written: Arc<Mutex<Vec<String>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::serial::transport::SerialTransport for FlakySerial {
        fn write_line(&mut self, data: &str) -> Result<(), crate::error::SerialError> {
            if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(crate::error::SerialError::Timeout { timeout_ms: 100 });
            }
            self.written.lock().push(data.to_string());
            Ok(())
        }
        fn read_line(
            &mut self,
            _timeout: std::time::Duration,
        ) -> Result<String, crate::error::SerialError> {
            Ok("OK".into())
        }
    }

    #[tokio::test]
    async fn openfan_backend_tolerates_vanish_then_resumes_on_reappear() {
        // Matrix row (OpenFan vanish/reappear): when the serial link drops, the
        // engine's OpenFan apply must NOT panic and must record no successful
        // write; when the link returns, writes resume. Post-flip the engine is
        // the sole writer (DEC-165), so this resilience is load-bearing.
        let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let transport = FlakySerial {
            written: written.clone(),
            fail: fail.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache,
            std::time::Duration::from_millis(100),
        );
        let mut be = OpenFanBackend::new(Arc::new(Mutex::new(ctrl)));

        // Vanished: writes fail; the engine no-ops without panicking.
        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;
        assert!(
            written.lock().is_empty(),
            "no successful OpenFan write while the link is down"
        );

        // Reappeared: writes resume on the next tick.
        fail.store(false, std::sync::atomic::Ordering::Relaxed);
        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;
        let w = written.lock();
        assert!(
            w.iter().any(|c| c.starts_with(">02")),
            "OpenFan writes must resume once the link reappears; got {w:?}"
        );
    }
}
