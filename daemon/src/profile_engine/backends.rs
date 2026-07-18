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
use crate::clock::Clock;
use crate::constants;
use crate::health::cache::StateCache;
use crate::hwmon::lease::HwmonWriter;
use crate::hwmon::pwm_control::HwmonControlError;
use crate::serial::protocol::NUM_CHANNELS;

/// One fan-control backend the profile engine writes through.
///
/// To add a backend: implement this trait, give the implementation sole
/// ownership of its gating rules (coalescing, failure caching, lease),
/// and call it from the loop's apply sequence in `profile_engine_loop`.
pub(crate) trait WriteBackend {
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
    /// Per-channel consecutive write-failure streaks (audit P3-5). Replaces a
    /// single shared counter that reset on ANY channel's success, so a
    /// persistent single-channel fault among healthy channels never tripped the
    /// SAFETY alert. Reset per channel by that channel's own success.
    channel_failures: HashMap<u8, u32>,
    /// Consecutive ticks where EVERY attempted channel failed (or the blocking
    /// write task panicked): the whole-link "serial link down" signal (audit
    /// P3-5), kept distinct from the per-channel streaks. Reset when any channel
    /// succeeds.
    link_down_streak: u32,
    /// Engine write-pause gate (DEC-165). Re-checked inside the blocking write
    /// (DEC-191) so an OpenFan calibration sweep that claims the pause mid-tick
    /// is not overwritten by an engine tick already in flight.
    cache: Arc<StateCache>,
}

impl OpenFanBackend {
    pub(crate) fn new(
        ctrl: Arc<Mutex<crate::serial::controller::FanController>>,
        cache: Arc<StateCache>,
    ) -> Self {
        Self {
            ctrl,
            channel_failures: HashMap::new(),
            link_down_streak: 0,
            cache,
        }
    }

    /// Record this tick's per-channel write outcomes and fire SAFETY alerts
    /// (audit P3-5). Two independent signals, each edge-triggered at
    /// [`constants::OPENFAN_FAIL_ALERT_THRESHOLD`] so a persistent fault does not
    /// re-log every 1 Hz tick:
    /// - **per-channel** — a channel's consecutive-failure streak hitting the
    ///   threshold ("ch{n} not responding"); reset only by that channel's own
    ///   success, so a single dead channel among healthy ones is no longer
    ///   masked by the others.
    /// - **whole-link** — every attempted channel failing for the threshold
    ///   consecutively ("serial link appears down"); reset the moment any
    ///   channel succeeds.
    ///
    /// `results` holds only channels actually attempted this tick — channels
    /// skipped by the in-flight verify/calibration pause are absent, so they
    /// neither count as a failure nor reset a streak.
    fn note_outcomes(&mut self, results: &[(u8, Result<(), String>)]) {
        if results.is_empty() {
            return;
        }
        let mut any_ok = false;
        for (ch, res) in results {
            match res {
                Err(e) => {
                    let streak = self.channel_failures.entry(*ch).or_insert(0);
                    *streak += 1;
                    let n = *streak;
                    log::warn!(
                        "Profile engine: OpenFan ch{ch} write failed ({n} consecutive): {e}"
                    );
                    if n == constants::OPENFAN_FAIL_ALERT_THRESHOLD {
                        log::error!(
                            "SAFETY: OpenFan ch{ch} not responding \
                             ({n} consecutive write failures)"
                        );
                    }
                }
                Ok(()) => {
                    any_ok = true;
                    self.channel_failures.remove(ch);
                }
            }
        }
        if any_ok {
            self.link_down_streak = 0;
        } else {
            self.link_down_streak += 1;
            if self.link_down_streak == constants::OPENFAN_FAIL_ALERT_THRESHOLD {
                log::error!(
                    "SAFETY: OpenFan serial link appears down \
                     (all {} channels failing for {} consecutive ticks)",
                    results.len(),
                    self.link_down_streak
                );
            }
        }
    }

    /// Account a panicked blocking write task as a whole-link failure (no
    /// per-channel results exist — the task died). Returns the new link-down
    /// streak for the caller's alert log.
    fn note_task_panic(&mut self) -> u32 {
        self.link_down_streak += 1;
        let n = self.link_down_streak;
        // A persistent panic mode must also trip the whole-link SAFETY alert,
        // not just the per-tick "task panicked" log (audit P3-5 follow-up).
        if n == constants::OPENFAN_FAIL_ALERT_THRESHOLD {
            log::error!(
                "SAFETY: OpenFan serial link appears down \
                 (write task panicking for {n} consecutive ticks)"
            );
        }
        n
    }

    #[cfg(test)]
    fn channel_failure_streak(&self, ch: u8) -> u32 {
        self.channel_failures.get(&ch).copied().unwrap_or(0)
    }

    #[cfg(test)]
    fn link_down_streak(&self) -> u32 {
        self.link_down_streak
    }
}

impl WriteBackend for OpenFanBackend {
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
        let cache = self.cache.clone();
        let join = tokio::task::spawn_blocking(move || {
            chans
                .into_iter()
                .filter_map(|(ch, pct)| {
                    // Lock per command (DEC-099) so GUI API requests can
                    // interleave between channel writes.
                    let mut guard = ctrl.lock();
                    // DEC-191: re-check the engine write-pause while HOLDING the
                    // controller lock, so the check-and-write is atomic against a
                    // concurrent OpenFan calibration sweep (whose test writes take
                    // this same lock). An engine tick already in flight when the
                    // sweep claims the pause must not overwrite the sweep's test
                    // PWM; checking before the lock left a narrow window where one
                    // channel's write could still land just after the sweep
                    // claimed the pause, corrupting its first RPM readback. A
                    // skipped channel records no outcome (it was not attempted),
                    // so it neither counts as a failure nor resets a streak.
                    if cache.verify_active() {
                        return None;
                    }
                    let res = guard
                        .set_pwm(ch, pct)
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    Some((ch, res))
                })
                .collect::<Vec<(u8, Result<(), String>)>>()
        })
        .await;
        let results = match join {
            Ok(results) => results,
            Err(e) => {
                // Concurrency review D3: a panic inside the blocking task must
                // not be silent. The whole write task died, so account it as a
                // whole-link failure (audit P3-5) and alert immediately.
                let n = self.note_task_panic();
                log::error!(
                    "SAFETY: Profile engine OpenFan write task panicked: {e} \
                     (link-down streak {n})"
                );
                return;
            }
        };
        // `&mut self` state can't cross into the 'static closure, so the
        // per-channel + whole-link failure bookkeeping runs here on the returned
        // results (audit P3-5).
        self.note_outcomes(&results);
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
    /// Monotonic clock for the [`constants::GPU_FAIL_COOLDOWN`] TTL (P3-7).
    /// Injectable so the 60 s cooldown can be exercised under deterministic
    /// fake time, mirroring `OverrideTable`/`LeaseManager`; production uses
    /// [`crate::clock::SystemClock`].
    clock: Arc<dyn Clock>,
}

impl GpuBackend {
    pub(crate) fn new(
        cache: Arc<StateCache>,
        gpu_infos: Arc<Vec<crate::hwmon::gpu_detect::AmdGpuInfo>>,
    ) -> Self {
        Self::with_clock(cache, gpu_infos, Arc::new(crate::clock::SystemClock))
    }

    /// Construct on an injected clock. Tests advance a fake clock to exercise
    /// the fail-cooldown deterministically instead of sleeping 60 s.
    pub(crate) fn with_clock(
        cache: Arc<StateCache>,
        gpu_infos: Arc<Vec<crate::hwmon::gpu_detect::AmdGpuInfo>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            cache,
            gpu_infos,
            fail_cache: HashMap::new(),
            clock,
        }
    }

    #[cfg(test)]
    fn fail_cache_len(&self) -> usize {
        self.fail_cache.len()
    }
}

impl WriteBackend for GpuBackend {
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
            // P2-1: re-check the engine write-pause per fan. A GPU fan verify
            // (POST /gpu/{id}/fan/verify) sets the pause and force-writes a test
            // value mid-tick; an engine tick already past the loop-level
            // `verify_active` gate (mod.rs) must not overwrite it. GPU fans have
            // no lease (DEC-045), so this per-fan recheck is the only guard —
            // re-read each iteration so a verify starting mid-loop stops the
            // remaining fans too.
            if self.cache.verify_active() {
                continue;
            }
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
                    && self.clock.now().saturating_duration_since(*failed_at)
                        < constants::GPU_FAIL_COOLDOWN
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
                        .insert(fan_id, (cmd.pwm_percent, self.clock.now()));
                }
            }
        }
    }
}

// ─── Motherboard hwmon ───────────────────────────────────────────────────

pub(crate) struct HwmonBackend {
    ctrl: Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>,
    /// Per-member consecutive write-failure streaks (DEC-199). Without this the
    /// per-header `warn!` re-logged every 1 Hz tick, so a persistent hwmon write
    /// failure — canonically EROFS when the systemd sandbox's `ReadWritePaths=`
    /// carve-out does not cover the real `/sys/devices` inode — spammed journald
    /// at 1 Hz. We log the FIRST failure per member, then only a periodic summary
    /// every [`constants::HWMON_FAIL_SUMMARY_INTERVAL`] ticks, and an INFO
    /// recovery line when the member writes successfully again. Reset per member
    /// by its own success, so a single stuck header among healthy ones is tracked
    /// in isolation (mirrors [`OpenFanBackend`]'s `channel_failures`, audit P3-5).
    member_failures: HashMap<String, u32>,
}

impl HwmonBackend {
    pub(crate) fn new(ctrl: Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>) -> Self {
        Self {
            ctrl,
            member_failures: HashMap::new(),
        }
    }

    /// Record this tick's per-member hwmon write outcomes and throttle the log
    /// (DEC-199). `results` holds only members actually attempted — a member
    /// skipped read-only (DEC-102) or skipped because the engine held no lease
    /// this tick is absent, so it neither counts as a failure nor resets a
    /// streak. `&mut self` state can't cross into the `'static` blocking closure,
    /// so this bookkeeping runs on the returned outcomes after the join (mirrors
    /// [`OpenFanBackend::note_outcomes`]).
    fn note_outcomes(&mut self, results: &[(String, Result<(), String>)]) {
        for (member_id, res) in results {
            match res {
                Err(e) => {
                    let streak = self.member_failures.entry(member_id.clone()).or_insert(0);
                    *streak += 1;
                    let n = *streak;
                    if n == 1 {
                        log::warn!("hwmon write failed for {member_id}: {e}");
                    } else if n.is_multiple_of(constants::HWMON_FAIL_SUMMARY_INTERVAL) {
                        log::warn!(
                            "hwmon write still failing for {member_id} \
                             ({n} consecutive ticks): {e}"
                        );
                    }
                }
                Ok(()) => {
                    // Recovery edge: a member that had been failing wrote again.
                    if let Some(prev) = self.member_failures.remove(member_id) {
                        log::info!(
                            "hwmon write recovered for {member_id} \
                             (after {prev} consecutive failure(s))"
                        );
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn member_failure_streak(&self, member_id: &str) -> u32 {
        self.member_failures.get(member_id).copied().unwrap_or(0)
    }
}

impl WriteBackend for HwmonBackend {
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
                    // P2-1: reuse the engine's own lease or a transient
                    // thermal-safety force-take, but NEVER a hardware verify's
                    // lease. A verify force-takes the lease as "verify"
                    // (hwmon_ctl.rs) and sets the engine write-pause; if it
                    // starts *after* this tick passed the loop-level
                    // `verify_active` gate (mod.rs), the engine still reaches
                    // here. Adopting the verify's lease would let the engine
                    // write through it and clobber the test value — the bug the
                    // single up-front check did not close. Excluding the
                    // `Verify` owner makes `take_lease` below return AlreadyHeld ⇒
                    // `lease_id = None` ⇒ the engine skips its hwmon writes this
                    // tick; the verify's RAII guard releases the lease when it
                    // ends and the next tick re-acquires. Thermal-safety is NOT
                    // excluded — after an emergency the engine adopts and renews
                    // that lease as before, so there is no post-thermal stall.
                    // `None` ⇒ acquire.
                    mgr.active_lease()
                        .filter(|lease| lease.owner != HwmonWriter::Verify)
                        .map(|lease| lease.lease_id.clone())
                };
                existing.or_else(|| {
                    guard
                        .lease_manager_mut()
                        .take_lease(HwmonWriter::Engine)
                        .ok()
                        .map(|l| l.lease_id)
                })
            };
            // No lease this tick (e.g. a hardware verify holds it) ⇒ nothing was
            // attempted; return an empty outcome set so no member's failure
            // streak is advanced or reset.
            let Some(lease_id) = lease_id else {
                return Vec::new();
            };

            // Phase 2: one lock per header (DEC-154) so a concurrent reader or
            // lease op is not starved for the whole batch. A GUI/thermal
            // force-take mid-scan fails the remaining writes with InvalidLease;
            // the next 1 Hz tick re-acquires. Outcomes are collected here and the
            // (throttled) failure logging runs on `&mut self` after the join
            // (DEC-199) — the `'static` blocking closure cannot borrow self.
            let mut outcomes: Vec<(String, Result<(), String>)> =
                Vec::with_capacity(hwmon_cmds.len());
            for (member_id, pwm_percent) in &hwmon_cmds {
                let mut guard = ctrl.lock();
                // DEC-102 backstop on the engine path: never attempt a write to
                // a header discovered read-only (`is_writable == false`). The GUI
                // member-picker and profile load drop these, but the un-validated
                // boot-load path does not, so the engine must skip them itself —
                // otherwise every tick would EACCES-spam the log and the member
                // would silently never take effect. A skipped header records no
                // outcome (it was not attempted).
                if guard.header(member_id).is_some_and(|h| !h.is_writable) {
                    continue;
                }
                let res = guard
                    .set_pwm(member_id, *pwm_percent, &lease_id)
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                outcomes.push((member_id.clone(), res));
            }

            // Phase 3: renew under a brief lock to keep it alive for next cycle.
            if let Err(e) = ctrl.lock().lease_manager_mut().renew_lease(&lease_id) {
                log::debug!("lease renewal failed (will re-acquire next cycle): {e}");
            }
            outcomes
        })
        .await;
        match join {
            Ok(outcomes) => self.note_outcomes(&outcomes),
            Err(e) => {
                // Concurrency review D3: surface panicked write tasks.
                log::error!("Profile engine: hwmon write task panicked: {e}");
            }
        }
    }
}

impl SafetyWriteBackend for HwmonBackend {
    /// Force every hwmon header to `pct` (auto-lease for safety writes).
    ///
    /// Force-takes the lease as thermal-safety, then re-locks the controller per
    /// header so concurrent GUI activity can proceed between writes (DEC-099).
    /// Because the lock is dropped between headers, a GUI hardware-verify can
    /// force-take the lease mid-scan and invalidate ours. Thermal safety
    /// outranks maintenance, so a write that fails with a lease error re-takes
    /// the lease and retries that header once — bounded, so a persistent
    /// preemptor cannot thrash — rather than leaving the remaining fans
    /// un-forced. (The lease system is hwmon-only; the OpenFan path has none.)
    /// DEC-146 P3-8: runs on the blocking pool; the re-lock-per-header structure
    /// (DEC-099) is preserved inside the closure.
    async fn force_all(&mut self, pct: u8) {
        let ctrl = self.ctrl.clone();
        let join = tokio::task::spawn_blocking(move || {
            let (hdr_ids, mut lease_id) = {
                let mut guard = ctrl.lock();
                let hdr_ids: Vec<String> = guard.headers().iter().map(|h| h.id.clone()).collect();
                let lease_id = guard
                    .lease_manager_mut()
                    .force_take_lease(HwmonWriter::ThermalSafety)
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
                match guard.set_pwm(hdr_id, pct, &lease_id) {
                    Ok(_) => {}
                    Err(HwmonControlError::Lease(_)) => {
                        // A concurrent GUI verify force-took the lease in the
                        // window DEC-099 leaves between headers, invalidating
                        // ours. Thermal safety outranks maintenance: re-take
                        // unconditionally and retry THIS header once. The re-take
                        // and retry share this one lock, so a preemptor cannot
                        // slip between them; the bound (one re-take per header)
                        // stops a persistent preemptor from thrashing the scan.
                        // force-take resets the new owner's coalescing state, so
                        // re-assert pwm_enable on the retry (Audit P1-E).
                        lease_id = guard
                            .lease_manager_mut()
                            .force_take_lease(HwmonWriter::ThermalSafety)
                            .lease_id;
                        guard.on_lease_released();
                        if let Err(e) = guard.set_pwm(hdr_id, pct, &lease_id) {
                            log::error!(
                                "THERMAL SAFETY: hwmon {hdr_id} write FAILED after lease re-take: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        log::error!("THERMAL SAFETY: hwmon {hdr_id} write FAILED: {e}");
                    }
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

    #[tokio::test]
    async fn gpu_backend_fail_cooldown_is_clock_gated() {
        // P3-7: with an injectable clock the 60 s GPU_FAIL_COOLDOWN is testable
        // deterministically. A failed write is retried only after the cooldown
        // elapses on the daemon's clock — advanced here instead of sleeping.
        use std::sync::atomic::{AtomicU64, Ordering};
        struct AdvanceClock {
            base: std::time::Instant,
            offset_ms: AtomicU64,
        }
        impl Clock for AdvanceClock {
            fn now(&self) -> std::time::Instant {
                self.base + std::time::Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
            }
        }
        let clock = Arc::new(AdvanceClock {
            base: std::time::Instant::now(),
            offset_ms: AtomicU64::new(0),
        });

        let dir = tempfile::tempdir().unwrap();
        let (mut gpu, _good) = fake_gpu(&dir);
        // Curve path under a not-yet-existing subdir → the first write fails.
        let sub = dir.path().join("sub");
        let curve_path = sub.join("fan_curve");
        gpu.fan_curve_path = Some(curve_path.clone());
        let cache = Arc::new(StateCache::new());
        let mut be = GpuBackend::with_clock(cache, Arc::new(vec![gpu]), clock.clone());

        // t=0: write fails (parent dir missing) → cached.
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert_eq!(be.fail_cache_len(), 1, "failed write must be cached");

        // Make the path writable so a *retry* could succeed.
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(&curve_path, "").unwrap();

        // Within the cooldown: same speed suppressed — no retry, file stays empty.
        clock.offset_ms.store(
            (constants::GPU_FAIL_COOLDOWN / 2).as_millis() as u64,
            Ordering::SeqCst,
        );
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "within cooldown the failed speed must not be retried"
        );
        assert_eq!(be.fail_cache_len(), 1);

        // Past the cooldown: the retry fires, succeeds, and clears the cache.
        clock.offset_ms.store(
            (constants::GPU_FAIL_COOLDOWN + std::time::Duration::from_secs(1)).as_millis() as u64,
            Ordering::SeqCst,
        );
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            !std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "after the cooldown the failed speed must be retried"
        );
        assert_eq!(be.fail_cache_len(), 0, "successful retry clears the cache");
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

    /// A [`SysfsWriter`] that fails writes whose path contains `fail_fragment`
    /// with EROFS ("Read-only file system", os error 30) — the DEC-199 sandbox
    /// carve-out symptom. `fail_fragment` is swappable at runtime so a test can
    /// "repair" the sandbox and observe recovery, and is path-selective so one
    /// stuck header can fail while a sibling still writes.
    struct CarveoutFailWriter {
        writes: WriteLog,
        fail_fragment: Arc<Mutex<Option<String>>>,
    }

    impl SysfsWriter for CarveoutFailWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            let fails = {
                let guard = self.fail_fragment.lock();
                guard.as_deref().is_some_and(|frag| path.contains(frag))
            };
            if fails {
                return Err(HwmonError::WriteError {
                    path: path.into(),
                    message: "Read-only file system (os error 30)".into(),
                });
            }
            self.writes.lock().push((path.into(), value.into()));
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, HwmonError> {
            Ok("128\n".into())
        }
    }

    fn hwmon_backend_carveout(
        headers: Vec<PwmHeaderDescriptor>,
        fail_fragment: Option<&str>,
    ) -> (HwmonBackend, Arc<Mutex<Option<String>>>) {
        let frag = Arc::new(Mutex::new(fail_fragment.map(String::from)));
        let writer = CarveoutFailWriter {
            writes: Arc::new(Mutex::new(Vec::new())),
            fail_fragment: frag.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(headers, LeaseManager::new(), Box::new(writer), cache);
        (HwmonBackend::new(Arc::new(Mutex::new(ctrl))), frag)
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
            lease.is_some_and(|l| l.owner == HwmonWriter::Engine),
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
        assert_eq!(lease.map(|l| l.owner), Some(HwmonWriter::Engine));
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
        drop(w);
        assert_eq!(
            be.member_failure_streak("hwmon:it8696:pwm1"),
            0,
            "a read-only header is skipped, never counted as a write failure (DEC-199)"
        );
    }

    #[tokio::test]
    async fn hwmon_failure_streak_increments_then_clears_on_recovery() {
        // DEC-199: a persistent hwmon write failure (canonically EROFS from a
        // misconfigured sandbox carve-out) must advance a per-member streak so
        // the log can be throttled, and clear the moment the member writes again.
        let (mut be, frag) =
            hwmon_backend_carveout(vec![make_header("hwmon:it8696:pwm1")], Some("pwm1"));

        for expected in 1..=3 {
            be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;
            assert_eq!(
                be.member_failure_streak("hwmon:it8696:pwm1"),
                expected,
                "each failing tick must advance the member's streak"
            );
        }

        // Sandbox carve-out repaired → the write succeeds → the streak clears.
        *frag.lock() = None;
        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;
        assert_eq!(
            be.member_failure_streak("hwmon:it8696:pwm1"),
            0,
            "a successful write must reset the member's failure streak"
        );
    }

    #[tokio::test]
    async fn hwmon_failure_streak_isolated_per_member() {
        // A single stuck header must not mask a healthy sibling: the failing
        // member accrues a streak while the writable one stays at zero (mirrors
        // the OpenFan per-channel isolation, audit P3-5).
        let (mut be, _frag) = hwmon_backend_carveout(
            vec![
                make_header_idx("hwmon:it8696:pwm1", 1),
                make_header_idx("hwmon:it8696:pwm2", 2),
            ],
            Some("pwm2"),
        );

        be.apply(&[
            cmd("hwmon:it8696:pwm1", "hwmon", 40),
            cmd("hwmon:it8696:pwm2", "hwmon", 55),
        ])
        .await;

        assert_eq!(
            be.member_failure_streak("hwmon:it8696:pwm2"),
            1,
            "the stuck header must accrue a failure streak"
        );
        assert_eq!(
            be.member_failure_streak("hwmon:it8696:pwm1"),
            0,
            "the healthy sibling must stay at zero"
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
        // Distinct pwm paths per header (make_header hardcodes pwm1) so the
        // value assertion below proves EVERY header — not just the first — was
        // driven to 100%.
        let (mut be, writes) = hwmon_backend(vec![header_with_paths(1), header_with_paths(2)]);
        // Even an engine-held lease is force-taken for safety writes.
        be.ctrl
            .lock()
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::Engine);

        be.force_all(100).await;

        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(lease.map(|l| l.owner), Some(HwmonWriter::ThermalSafety));
        let w = writes.lock();
        // Pin the forced VALUE (100% → raw "255"), not just write-presence: a
        // force_all that ignored its pct and wrote 40% would else pass. Assert
        // both headers' pwm data write (the pwm{i} path excludes pwm{i}_enable).
        for i in 1..=2 {
            let pwm_path = format!("/sys/class/hwmon/hwmon0/pwm{i}");
            let vals: Vec<_> = w
                .iter()
                .filter(|(p, _)| *p == pwm_path)
                .map(|(_, v)| v.trim())
                .collect();
            assert!(
                !vals.is_empty(),
                "header pwm{i} received no forced write; got {w:?}"
            );
            assert!(
                vals.iter().all(|v| *v == "255"),
                "header pwm{i} must be forced to 100% (raw 255); got {vals:?}"
            );
        }
    }

    /// Writer that signals once (on its first write) so a test can time a
    /// mid-scan lease preemption, then holds the lock long enough that the
    /// parked preemptor crosses parking_lot's eventual-fairness window and
    /// reliably acquires the lock in the gap after the first header.
    struct SignalOnFirstWriter {
        writes: WriteLog,
        tx: std::sync::mpsc::Sender<()>,
        signaled: bool,
    }

    impl SysfsWriter for SignalOnFirstWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes.lock().push((path.into(), value.into()));
            if !self.signaled {
                self.signaled = true;
                let _ = self.tx.send(());
                // Still holding the controller lock: park the preemptor long
                // enough that the next unlock hands off to it (fairness), not
                // to force_all's own re-lock for the second header.
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, HwmonError> {
            Ok("128\n".into())
        }
    }

    fn header_with_paths(i: usize) -> PwmHeaderDescriptor {
        let mut h = make_header(&format!("hwmon:it8696:pwm{i}"));
        h.pwm_path = format!("/sys/class/hwmon/hwmon0/pwm{i}");
        h.enable_path = Some(format!("/sys/class/hwmon/hwmon0/pwm{i}_enable"));
        h
    }

    #[tokio::test]
    async fn hwmon_force_all_completes_every_header_despite_midscan_verify_preempt() {
        // Regression for the force_all partial-write bug. DEC-099 drops the
        // controller lock between headers, so a GUI verify can force-take the
        // lease mid-scan and invalidate force_all's. The retry-on-lease-error
        // fix re-takes thermal-safety and still forces EVERY header; without it
        // the header after the preemption is silently left un-forced during a
        // thermal emergency.
        const N: usize = 8;
        let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let writer = SignalOnFirstWriter {
            writes: writes.clone(),
            tx,
            signaled: false,
        };
        let headers: Vec<_> = (1..=N).map(header_with_paths).collect();
        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(headers, LeaseManager::new(), Box::new(writer), cache);
        let mut be = HwmonBackend::new(Arc::new(Mutex::new(ctrl)));

        // Exactly one mid-scan preemption: a GUI verify force-takes the lease
        // once force_all is past the first header.
        let ctrl_for_preempt = be.ctrl.clone();
        let preemptor = std::thread::spawn(move || {
            rx.recv().expect("force_all must write at least one header");
            ctrl_for_preempt
                .lock()
                .lease_manager_mut()
                .force_take_lease(HwmonWriter::Verify);
        });

        be.force_all(100).await;
        preemptor.join().unwrap();

        // Every header must have been forced to 100% despite the mid-scan
        // preemption. Assert the VALUE (raw "255"), not just presence — the
        // failure message already claims "100%", so prove it.
        let w = writes.lock();
        for i in 1..=N {
            let pwm_path = format!("/sys/class/hwmon/hwmon0/pwm{i}");
            let vals: Vec<_> = w
                .iter()
                .filter(|(p, _)| *p == pwm_path)
                .map(|(_, v)| v.trim())
                .collect();
            assert!(
                !vals.is_empty(),
                "header pwm{i} was not forced (partial-write bug); writes={w:?}"
            );
            assert!(
                vals.iter().all(|v| *v == "255"),
                "header pwm{i} must be forced to 100% (raw 255); got {vals:?}"
            );
        }
        drop(w);

        // force_all reclaimed the lease from the verify preemptor — proof the
        // re-take (not just a lucky race) carried the scan to completion.
        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(
            lease.map(|l| l.owner),
            Some(HwmonWriter::ThermalSafety),
            "force_all must re-take thermal-safety after a mid-scan verify preempt"
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

    fn openfan_backend() -> (OpenFanBackend, Arc<Mutex<Vec<String>>>, Arc<StateCache>) {
        let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let transport = SerialMock {
            written: written.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(100),
        );
        (
            OpenFanBackend::new(Arc::new(Mutex::new(ctrl)), cache.clone()),
            written,
            cache,
        )
    }

    #[tokio::test]
    async fn openfan_backend_drops_malformed_member_ids() {
        let (mut be, written, _cache) = openfan_backend();

        be.apply(&[
            cmd("openfan:chXX", "openfan", 50),
            cmd("not-a-channel", "openfan", 50),
        ])
        .await;

        assert!(written.lock().is_empty());
    }

    #[tokio::test]
    async fn openfan_backend_writes_when_gui_inactive() {
        let (mut be, written, _cache) = openfan_backend();

        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;

        let w = written.lock();
        assert!(
            w.iter().any(|c| c.starts_with(">02")),
            "expected a SetPwm command; got {w:?}"
        );
    }

    #[tokio::test]
    async fn openfan_force_all_writes_every_channel() {
        let (mut be, written, _cache) = openfan_backend();

        be.force_all(100).await;

        let w = written.lock();
        let set_pwm: Vec<_> = w.iter().filter(|c| c.starts_with(">02")).collect();
        assert_eq!(
            set_pwm.len(),
            NUM_CHANNELS as usize,
            "expected one forced SetPwm per channel; got {w:?}"
        );
        // Count alone can't catch a force_all that ignores its pct argument and
        // sends e.g. 40% during a 105°C emergency. Pin the VALUE: 100% → raw 255
        // → frame ">02{ch:02X}FF\n" for every channel.
        for frame in &set_pwm {
            assert!(
                frame.trim_end().ends_with("FF"),
                "thermal force must drive every OpenFan channel to 100% (raw FF); got {frame:?}"
            );
        }
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
            cache.clone(),
            std::time::Duration::from_millis(100),
        );
        let mut be = OpenFanBackend::new(Arc::new(Mutex::new(ctrl)), cache);

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

    // ── Phase 2 / P-CAL additions (daemon /audit 2026-06-26) ──────────

    #[test]
    fn openfan_per_channel_failure_streak_not_masked_by_healthy_channel() {
        // P3-5: a persistent single-channel fault must climb its OWN streak to
        // the SAFETY threshold even while another channel succeeds every tick.
        // The pre-fix shared counter reset on ANY success, so it never tripped.
        let (mut be, _written, _cache) = openfan_backend();
        for _ in 0..constants::OPENFAN_FAIL_ALERT_THRESHOLD {
            be.note_outcomes(&[(0, Ok(())), (3, Err("link".into()))]);
        }
        assert_eq!(
            be.channel_failure_streak(3),
            constants::OPENFAN_FAIL_ALERT_THRESHOLD,
            "the dead channel's streak must reach the threshold despite ch0 succeeding"
        );
        assert_eq!(
            be.channel_failure_streak(0),
            0,
            "the healthy channel's streak stays at 0"
        );
        assert_eq!(
            be.link_down_streak(),
            0,
            "a partial fault is not a whole-link failure"
        );
        // The dead channel's own success — and only that — resets its streak.
        be.note_outcomes(&[(3, Ok(()))]);
        assert_eq!(be.channel_failure_streak(3), 0);
    }

    #[test]
    fn openfan_whole_link_down_trips_distinct_link_streak() {
        // P3-5: every attempted channel failing for the threshold consecutively
        // trips the distinct whole-link "serial down" streak; one success
        // anywhere resets it.
        let (mut be, _written, _cache) = openfan_backend();
        for _ in 0..constants::OPENFAN_FAIL_ALERT_THRESHOLD {
            be.note_outcomes(&[(0, Err("x".into())), (1, Err("x".into()))]);
        }
        assert_eq!(
            be.link_down_streak(),
            constants::OPENFAN_FAIL_ALERT_THRESHOLD
        );
        be.note_outcomes(&[(0, Ok(())), (1, Err("x".into()))]);
        assert_eq!(
            be.link_down_streak(),
            0,
            "any channel success resets the whole-link streak"
        );
    }

    #[tokio::test]
    async fn openfan_backend_skips_writes_while_engine_paused() {
        // DEC-191: while a verify/calibration holds the engine write-pause, the
        // OpenFan backend's in-flight recheck must skip writes (so a calibration
        // sweep's test PWM survives), then resume when the pause clears.
        let (mut be, written, cache) = openfan_backend();

        assert!(cache.try_begin_verify(std::time::Duration::from_secs(30)));
        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;
        assert!(
            written.lock().is_empty(),
            "no OpenFan write may land while the engine is paused (DEC-191)"
        );

        cache.end_verify();
        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;
        assert!(
            written.lock().iter().any(|c| c.starts_with(">02")),
            "writes resume once the pause clears"
        );
    }

    #[tokio::test]
    async fn hwmon_apply_skips_while_a_verify_lease_is_held() {
        // P2-1: a hardware verify force-takes the lease as "verify". If it starts
        // after the loop-level gate, the engine still reaches apply; it must NOT
        // adopt the verify lease and write through it (clobbering the test value).
        // Option B: the engine skips its hwmon writes and leaves the lease intact.
        let (mut be, writes) = hwmon_backend(vec![make_header("hwmon:it8696:pwm1")]);
        be.ctrl
            .lock()
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::Verify);

        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;

        let w = writes.lock();
        assert!(
            !w.iter().any(|(p, _)| p.ends_with("pwm1")),
            "engine must not write hwmon while a verify holds the lease (P2-1); got {w:?}"
        );
        drop(w);
        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(
            lease.map(|l| l.owner),
            Some(HwmonWriter::Verify),
            "the verify lease must remain intact — the engine did not take over"
        );
    }

    #[tokio::test]
    async fn hwmon_apply_reuses_thermal_safety_lease_no_post_emergency_stall() {
        // P2-1 regression guard: Option B excludes ONLY the verify lease. A
        // non-verify foreign lease (e.g. "thermal-safety" left by force_all after
        // an emergency) must still be reused so the engine resumes hwmon control
        // immediately instead of being locked out for the lease's 60 s TTL.
        let (mut be, writes) = hwmon_backend(vec![make_header("hwmon:it8696:pwm1")]);
        be.ctrl
            .lock()
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::ThermalSafety);

        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;

        let w = writes.lock();
        assert!(
            w.iter().any(|(p, _)| p.ends_with("pwm1")),
            "engine must reuse a thermal-safety lease and write (no 60 s stall); got {w:?}"
        );
    }

    #[tokio::test]
    async fn gpu_backend_skips_writes_while_engine_paused() {
        // P2-1: GPU fans have no lease (DEC-045), so the per-fan verify_active
        // recheck is the ONLY guard against the engine overwriting a GPU verify's
        // test value mid-tick. While the pause is held, no curve write may land.
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = Arc::new(StateCache::new());
        assert!(cache.try_begin_verify(std::time::Duration::from_secs(30)));
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "engine must not write a GPU fan while a verify holds the pause (P2-1)"
        );

        cache.end_verify();
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            !std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "writes resume once the pause clears"
        );
    }
}
