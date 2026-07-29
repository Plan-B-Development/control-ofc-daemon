//! In-memory cache with batch updates and consistent snapshot reads.
//!
//! Uses `RwLock` for concurrent access: multiple readers, exclusive writer.
//! Updates are atomic at the batch boundary.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::health::state::*;

/// Thread-safe in-memory cache for daemon state.
///
/// All IPC responses should read from this cache rather than polling
/// hardware directly.
pub struct StateCache {
    inner: RwLock<DaemonState>,
    /// Set by the polling loop when a system suspend/resume is detected
    /// (CLOCK_BOOTTIME gap). Checked and cleared by HwmonPwmController
    /// on the next set_pwm() call to force re-establishing manual mode.
    pub resume_detected: AtomicBool,
    /// Monotonic counter bumped on every `POST /profile/activate` (DEC-188).
    /// The profile-engine loop tracks the last value it observed and re-anchors
    /// all cross-tick state when it changes, so re-activating the *same* profile
    /// id (the "tweak the active curve and re-apply" path) takes effect on the
    /// next tick instead of being suppressed by the 2°C deadband (DEC-096).
    /// Bumped and read under the `active_profile` mutex so the tick that first
    /// observes a swapped profile also observes the new epoch (no extra tick).
    profile_activation_epoch: AtomicU64,
}

impl StateCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(DaemonState::default()),
            resume_detected: AtomicBool::new(false),
            profile_activation_epoch: AtomicU64::new(0),
        }
    }

    /// Bump the profile-activation epoch (DEC-188). Called by
    /// `activate_profile_handler` immediately after swapping `active_profile`,
    /// while still holding that mutex, so the engine sees the swap and the bump
    /// atomically. `SeqCst` is belt-and-braces over the mutex's own ordering.
    pub fn bump_profile_activation_epoch(&self) {
        self.profile_activation_epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Read the current profile-activation epoch (DEC-188). The engine loop
    /// reads this under the `active_profile` mutex and re-anchors its cross-tick
    /// state whenever the value differs from the previous tick's.
    pub fn profile_activation_epoch(&self) -> u64 {
        self.profile_activation_epoch.load(Ordering::SeqCst)
    }

    /// Get a consistent snapshot of the current state.
    ///
    /// The returned `DaemonState` is a clone — no torn reads are possible.
    pub fn snapshot(&self) -> DaemonState {
        let state = self.inner.read();
        state.clone()
    }

    /// Run `f` against the live state under a shared read guard, returning its
    /// result without cloning the whole `DaemonState`.
    ///
    /// EFF-1: the read-only response builders (`build_*`, `compute_health`) only
    /// borrow `&DaemonState`. `/poll` and `/status` are the most frequent
    /// requests (the GUI polls at 1 Hz); calling `snapshot()` for them clones
    /// the entire state (five `HashMap`s + owned `String`s) just to read it.
    /// `read_with` lets those builders run under the guard with no intermediate
    /// clone. `f` must NOT call back into `self` (the parking_lot read guard is
    /// not reentrant) — keep it to pure reads of the borrowed `&DaemonState`.
    pub fn read_with<R>(&self, f: impl FnOnce(&DaemonState) -> R) -> R {
        let state = self.inner.read();
        f(&state)
    }

    /// Clone only the sensor map. The profile engine's curve evaluation and
    /// thermal-safety scan read sensors but none of the fan/AIO state, so this
    /// avoids cloning the rest of `DaemonState` on every tick.
    pub fn sensors_snapshot(&self) -> HashMap<String, CachedSensorReading> {
        self.inner.read().sensors.clone()
    }

    /// Clone only the GPU-fan map, used by the profile engine's GPU
    /// write-suppression check. Typically 0–1 entries — far cheaper than a
    /// full snapshot.
    pub fn gpu_fans_snapshot(&self) -> HashMap<String, AmdGpuFanState> {
        self.inner.read().gpu_fans.clone()
    }

    /// Update all OpenFanController fan readings as a batch.
    ///
    /// Preserves `last_commanded_pwm` from existing entries when the incoming
    /// state doesn't carry one (the RPM poll can't read the commanded value
    /// from the controller) — mirroring `update_gpu_fans`, so the poll loop
    /// no longer needs a full `snapshot()` clone every second just to copy
    /// this one field forward (DEC-146 P3-7).
    pub fn update_openfan_fans(&self, fans: Vec<OpenFanState>) {
        let now = Instant::now();
        let mut state = self.inner.write();
        for mut fan in fans {
            if fan.last_commanded_pwm.is_none() {
                if let Some(existing) = state.openfan_fans.get(&fan.channel) {
                    fan.last_commanded_pwm = existing.last_commanded_pwm;
                }
            }
            state.openfan_fans.insert(fan.channel, fan);
        }
        state.subsystem_timestamps.openfan = Some(now);
        state.snapshot_at = now;
    }

    /// Update all hwmon fan readings as a batch.
    pub fn update_hwmon_fans(&self, fans: Vec<HwmonFanState>) {
        let now = Instant::now();
        let mut state = self.inner.write();
        for fan in fans {
            state.hwmon_fans.insert(fan.id.clone(), fan);
        }
        state.snapshot_at = now;
        // hwmon fan timestamps roll into the hwmon subsystem timestamp
    }

    /// Update all sensor readings as a batch, computing rate and min/max.
    pub fn update_sensors(&self, readings: Vec<CachedSensorReading>) {
        let now = Instant::now();
        let mut state = self.inner.write();
        for mut reading in readings {
            // Compute rate of change and update min/max from previous reading
            if let Some(prev) = state.sensors.get(&reading.id) {
                let elapsed = now.duration_since(prev.updated_at).as_secs_f64();
                if elapsed > 0.1 {
                    let raw_rate = (reading.value_c - prev.value_c) / elapsed;
                    // Exponential moving average for smoothing
                    let alpha = 0.3;
                    let smoothed = match prev.rate_c_per_s {
                        Some(prev_rate) => alpha * raw_rate + (1.0 - alpha) * prev_rate,
                        None => raw_rate,
                    };
                    reading.rate_c_per_s = Some((smoothed * 100.0).round() / 100.0);
                }
                // Track session min/max
                let prev_min = prev.session_min_c.unwrap_or(reading.value_c);
                let prev_max = prev.session_max_c.unwrap_or(reading.value_c);
                reading.session_min_c = Some(prev_min.min(reading.value_c));
                reading.session_max_c = Some(prev_max.max(reading.value_c));
            } else {
                // First reading for this sensor
                reading.session_min_c = Some(reading.value_c);
                reading.session_max_c = Some(reading.value_c);
            }
            state.sensors.insert(reading.id.clone(), reading);
        }
        state.subsystem_timestamps.hwmon = Some(now);
        state.snapshot_at = now;
    }

    /// Update the thermal safety override state.
    ///
    /// Unconditional write under the write lock (CONC-3, 2026-07-21 audit).
    /// An earlier fast path (EFF-4) took a read lock to compare-and-skip
    /// first — lossless only while the engine tick stayed the sole writer,
    /// an invariant no type enforces and a read→write TOCTOU if it ever
    /// broke. The engine calls this once per 1 Hz tick with a short string;
    /// an uncontended `parking_lot` write at that rate is noise, so the
    /// invariant-free form wins.
    pub fn set_thermal_override_state(&self, state_str: &str) {
        self.inner.write().thermal_override_state = Some(state_str.to_string());
    }

    /// Try to claim the single hardware-verify slot, pausing the profile
    /// engine's write phase for the verify's lifetime. Returns `false` if a
    /// verify is already in progress (the caller must reject with 409) — this
    /// single-flight guard stops two concurrent verifies from clobbering each
    /// other's pause or lease (DEC-165). `window` is a generous deadman
    /// backstop: the caller's RAII guard clears the flag on drop/panic/cancel,
    /// but if it somehow does not, the pause self-clears after `window` so a
    /// verify can never strand fan control.
    pub fn try_begin_verify(&self, window: Duration) -> bool {
        let mut state = self.inner.write();
        if state.verify_in_progress {
            return false;
        }
        state.verify_in_progress = true;
        state.verify_active_until = Some(Instant::now() + window);
        true
    }

    /// Release the hardware-verify slot (the engine resumes writing next tick).
    pub fn end_verify(&self) {
        let mut state = self.inner.write();
        state.verify_in_progress = false;
        state.verify_active_until = None;
    }

    /// True while a hardware verify is in progress — held for the verify's
    /// entire lifetime by the handler's RAII guard, and bounded by the deadman
    /// backstop so a leaked guard cannot pause the engine indefinitely.
    pub fn verify_active(&self) -> bool {
        let state = self.inner.read();
        state.verify_in_progress
            && state
                .verify_active_until
                .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Relinquish a GPU fan to firmware-auto: the profile engine stops writing
    /// it, so a `POST /gpu/{id}/fan/reset` is durable under an active profile
    /// instead of being re-asserted on the next tick. Cleared on the next
    /// profile activation (DEC-165).
    pub fn relinquish_gpu_fan(&self, fan_id: &str) {
        self.inner
            .write()
            .relinquished_gpu_fans
            .insert(fan_id.to_string());
    }

    /// Clear all relinquished GPU fans so a freshly-activated profile resumes
    /// controlling them.
    pub fn clear_relinquished_gpu_fans(&self) {
        self.inner.write().relinquished_gpu_fans.clear();
    }

    /// True if the given GPU fan has been relinquished to firmware-auto.
    pub fn is_gpu_fan_relinquished(&self, fan_id: &str) -> bool {
        self.inner.read().relinquished_gpu_fans.contains(fan_id)
    }

    /// Update the last commanded PWM for a single OpenFanController channel.
    pub fn set_openfan_commanded_pwm(&self, channel: u8, pwm: u8) {
        let now = Instant::now();
        let mut state = self.inner.write();
        if let Some(fan) = state.openfan_fans.get_mut(&channel) {
            fan.last_commanded_pwm = Some(pwm);
            fan.updated_at = now;
        } else {
            state.openfan_fans.insert(
                channel,
                OpenFanState {
                    channel,
                    rpm: 0,
                    last_commanded_pwm: Some(pwm),
                    updated_at: now,
                    rpm_polled: false,
                },
            );
        }
        state.snapshot_at = now;
    }

    /// Update AMD GPU fan readings as a batch.
    ///
    /// Preserves `last_commanded_pct` from existing entries when the polling
    /// update doesn't include one (polling sets it to None since it can't
    /// read the commanded value from sysfs).
    pub fn update_gpu_fans(&self, fans: Vec<AmdGpuFanState>) {
        let now = Instant::now();
        let mut state = self.inner.write();
        for mut fan in fans {
            if fan.last_commanded_pct.is_none() {
                if let Some(existing) = state.gpu_fans.get(&fan.id) {
                    fan.last_commanded_pct = existing.last_commanded_pct;
                }
            }
            state.gpu_fans.insert(fan.id.clone(), fan);
        }
        state.snapshot_at = now;
    }

    /// Update the last commanded speed for an AMD GPU fan.
    ///
    /// Creates a default `AmdGpuFanState` entry if the GPU has not been
    /// seen yet (e.g. first write before polling has run).
    pub fn set_gpu_fan_commanded_pct(&self, gpu_id: &str, pct: u8) {
        let now = Instant::now();
        let mut state = self.inner.write();
        let fan = state
            .gpu_fans
            .entry(gpu_id.to_string())
            .or_insert_with(|| AmdGpuFanState {
                id: gpu_id.to_string(),
                rpm: None,
                last_commanded_pct: None,
                duty_pct: None,
                updated_at: now,
            });
        fan.last_commanded_pct = Some(pct);
        fan.updated_at = now;
        state.snapshot_at = now;
    }

    /// Update AIO pump state.
    pub fn update_aio(&self, aio: AioPumpState) {
        let now = Instant::now();
        let mut state = self.inner.write();
        state.aio = aio;
        state.subsystem_timestamps.aio = Some(now);
        state.snapshot_at = now;
    }

    /// Replace the set of present-but-unreadable sensors (DEC-193) and evict any
    /// stale cached reading for the listed ids.
    ///
    /// Without the eviction, a sensor that was readable and then went
    /// permanently unreadable (e.g. WiFi soft-blocked → `ENETDOWN`) would linger
    /// in `sensors` at its last value forever — served as a live temperature and
    /// even usable as a curve input. Listing it here removes that stale entry;
    /// when it recovers, the next successful `update_sensors` re-inserts it and
    /// the poll loop drops it from this set.
    ///
    /// The common case (nothing unavailable, nothing previously unavailable)
    /// takes only a shared read lock and returns — the poll loop calls this every
    /// tick.
    pub fn update_unavailable_sensors(&self, unavailable: Vec<UnavailableSensor>) {
        // Deliberate double-checked shape: the fast-path read guard is dropped
        // before the write lock is taken, so another caller can interleave
        // between check and write. That race is harmless — the write path is
        // idempotent (re-removing absent ids / re-assigning an equal list), so
        // the worst case is duplicated work. Do not "fix" it by holding a
        // single lock across the whole function; the fast path exists so the
        // every-tick common case never contends for the write lock.
        if unavailable.is_empty() && self.inner.read().unavailable_sensors.is_empty() {
            return;
        }
        let mut state = self.inner.write();
        for u in &unavailable {
            state.sensors.remove(&u.id);
        }
        state.unavailable_sensors = unavailable;
    }
}

impl Default for StateCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StateCache {
    /// Check if a system resume was detected and clear the flag atomically.
    pub fn take_resume_flag(&self) -> bool {
        self.resume_detected.swap(false, Ordering::Relaxed)
    }

    /// Signal that a system resume was detected.
    pub fn set_resume_detected(&self) {
        self.resume_detected.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::types::SensorKind;

    fn make_openfan(channel: u8, rpm: u16) -> OpenFanState {
        OpenFanState {
            channel,
            rpm,
            last_commanded_pwm: None,
            updated_at: Instant::now(),
            rpm_polled: true,
        }
    }

    fn make_sensor(id: &str, value_c: f64) -> CachedSensorReading {
        CachedSensorReading {
            id: id.to_string(),
            kind: SensorKind::CpuTemp,
            label: "test".into(),
            value_c,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }
    }

    #[test]
    fn empty_cache_snapshot() {
        let cache = StateCache::new();
        let snap = cache.snapshot();
        assert!(snap.openfan_fans.is_empty());
        assert!(snap.hwmon_fans.is_empty());
        assert!(snap.sensors.is_empty());
        assert!(!snap.aio.detected);
    }

    #[test]
    fn update_openfan_fans_preserves_commanded_pwm_on_none() {
        // DEC-146 P3-7: the RPM poll can't read the commanded value from the
        // controller, so a poll update carrying None must not erase what a
        // write recorded — mirroring update_gpu_fans.
        let cache = StateCache::new();
        let mut written = make_openfan(0, 800);
        written.last_commanded_pwm = Some(40);
        cache.update_openfan_fans(vec![written]);

        // Poll cycle: rpm refreshed, commanded value unknown (None).
        cache.update_openfan_fans(vec![make_openfan(0, 820)]);
        let snap = cache.snapshot();
        assert_eq!(snap.openfan_fans[&0].last_commanded_pwm, Some(40));
        assert_eq!(snap.openfan_fans[&0].rpm, 820);

        // A new write overrides the preserved value.
        let mut rewritten = make_openfan(0, 830);
        rewritten.last_commanded_pwm = Some(60);
        cache.update_openfan_fans(vec![rewritten]);
        let snap = cache.snapshot();
        assert_eq!(snap.openfan_fans[&0].last_commanded_pwm, Some(60));
    }

    #[test]
    fn read_with_observes_live_state_without_snapshot() {
        // EFF-1: read_with runs a closure against the live state under a shared
        // read guard and returns a derived value, with no full DaemonState
        // clone. It must observe exactly what snapshot() would.
        let cache = StateCache::new();
        cache.set_thermal_override_state("emergency");
        cache.update_sensors(vec![]);

        let via_read_with = cache.read_with(|s| s.thermal_override_state.clone());
        let via_snapshot = cache.snapshot().thermal_override_state;
        assert_eq!(via_read_with, via_snapshot);
        assert_eq!(via_read_with.as_deref(), Some("emergency"));
    }

    #[test]
    fn set_thermal_override_state_applies_changes_and_is_idempotent() {
        // The engine calls this every tick (unconditional write since CONC-3
        // dropped the EFF-4 compare-and-skip fast path). A redundant write
        // must stay value-correct and a genuine change MUST land — this
        // guards against any future fast-path dropping real transitions.
        let cache = StateCache::new();
        assert_eq!(cache.snapshot().thermal_override_state, None);

        cache.set_thermal_override_state("normal");
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("normal")
        );

        // Redundant write — value stays correct.
        cache.set_thermal_override_state("normal");
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("normal")
        );

        // Genuine change must be applied, not skipped.
        cache.set_thermal_override_state("emergency");
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("emergency")
        );
        cache.set_thermal_override_state("recovery");
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("recovery")
        );
    }

    #[test]
    fn profile_activation_epoch_starts_zero_and_increments() {
        // DEC-188: the profile engine re-anchors its cross-tick state whenever
        // this value changes, so a fresh cache must start at 0 and every bump
        // (one per `POST /profile/activate`) must advance it monotonically.
        let cache = StateCache::new();
        assert_eq!(cache.profile_activation_epoch(), 0);
        cache.bump_profile_activation_epoch();
        assert_eq!(cache.profile_activation_epoch(), 1);
        cache.bump_profile_activation_epoch();
        assert_eq!(cache.profile_activation_epoch(), 2);
    }

    #[test]
    fn update_openfan_fans_batch() {
        let cache = StateCache::new();
        cache.update_openfan_fans(vec![make_openfan(0, 1200), make_openfan(1, 1100)]);

        let snap = cache.snapshot();
        assert_eq!(snap.openfan_fans.len(), 2);
        assert_eq!(snap.openfan_fans[&0].rpm, 1200);
        assert_eq!(snap.openfan_fans[&1].rpm, 1100);
        assert!(snap.subsystem_timestamps.openfan.is_some());
    }

    #[test]
    fn update_openfan_overwrites_existing() {
        let cache = StateCache::new();
        cache.update_openfan_fans(vec![make_openfan(0, 1200)]);
        cache.update_openfan_fans(vec![make_openfan(0, 1500)]);

        let snap = cache.snapshot();
        assert_eq!(snap.openfan_fans.len(), 1);
        assert_eq!(snap.openfan_fans[&0].rpm, 1500);
    }

    #[test]
    fn update_sensors_batch() {
        let cache = StateCache::new();
        cache.update_sensors(vec![
            make_sensor("hwmon:k10temp:0000:00:18.3:Tctl", 55.0),
            make_sensor("hwmon:amdgpu:0000:03:00.0:edge", 42.0),
        ]);

        let snap = cache.snapshot();
        assert_eq!(snap.sensors.len(), 2);
        assert!(
            (snap.sensors["hwmon:k10temp:0000:00:18.3:Tctl"].value_c - 55.0).abs() < f64::EPSILON
        );
        assert!(snap.subsystem_timestamps.hwmon.is_some());
    }

    #[test]
    fn update_hwmon_fans() {
        let cache = StateCache::new();
        cache.update_hwmon_fans(vec![HwmonFanState {
            id: "it8696:fan1".into(),
            rpm: Some(800),
            last_commanded_pwm: None,
            updated_at: Instant::now(),
        }]);

        let snap = cache.snapshot();
        assert_eq!(snap.hwmon_fans.len(), 1);
        assert_eq!(snap.hwmon_fans["it8696:fan1"].rpm, Some(800));
    }

    #[test]
    fn update_aio() {
        let cache = StateCache::new();
        cache.update_aio(AioPumpState {
            detected: true,
            pump_rpm: Some(2400),
            coolant_temp_c: Some(32.5),
            ..Default::default()
        });

        let snap = cache.snapshot();
        assert!(snap.aio.detected);
        assert_eq!(snap.aio.pump_rpm, Some(2400));
        assert!(snap.subsystem_timestamps.aio.is_some());
    }

    #[test]
    fn set_gpu_fan_creates_entry_if_missing() {
        let cache = StateCache::new();

        // No GPU fans in cache initially
        let snap = cache.snapshot();
        assert!(snap.gpu_fans.is_empty());

        // set_gpu_fan_commanded_pct should create the entry
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:2d:00.0", 75);

        let snap = cache.snapshot();
        assert_eq!(snap.gpu_fans.len(), 1);
        let fan = &snap.gpu_fans["amd_gpu:0000:2d:00.0"];
        assert_eq!(fan.id, "amd_gpu:0000:2d:00.0");
        assert_eq!(fan.last_commanded_pct, Some(75));
        assert_eq!(fan.rpm, None);
    }

    #[test]
    fn set_gpu_fan_updates_existing_entry() {
        let cache = StateCache::new();

        // Pre-populate via update_gpu_fans
        cache.update_gpu_fans(vec![crate::health::state::AmdGpuFanState {
            id: "amd_gpu:0000:2d:00.0".into(),
            rpm: Some(1800),
            last_commanded_pct: Some(50),
            duty_pct: None,
            updated_at: Instant::now(),
        }]);

        // Update commanded pct
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:2d:00.0", 90);

        let snap = cache.snapshot();
        let fan = &snap.gpu_fans["amd_gpu:0000:2d:00.0"];
        assert_eq!(fan.last_commanded_pct, Some(90));
        // RPM should be preserved
        assert_eq!(fan.rpm, Some(1800));
    }

    #[test]
    fn snapshot_is_consistent_clone() {
        let cache = StateCache::new();
        cache.update_openfan_fans(vec![make_openfan(0, 1200)]);

        let snap1 = cache.snapshot();

        // Mutate cache after snapshot
        cache.update_openfan_fans(vec![make_openfan(0, 9999)]);

        // snap1 should still show old value
        assert_eq!(snap1.openfan_fans[&0].rpm, 1200);

        // New snapshot shows new value
        let snap2 = cache.snapshot();
        assert_eq!(snap2.openfan_fans[&0].rpm, 9999);
    }

    #[test]
    fn verify_active_lifecycle_deadman_and_single_flight() {
        use std::time::Duration;
        let cache = StateCache::new();
        // Fresh cache: no verify in progress.
        assert!(!cache.verify_active());
        // Claiming the slot → active.
        assert!(cache.try_begin_verify(Duration::from_secs(60)));
        assert!(cache.verify_active());
        // Single-flight: a second concurrent claim is rejected.
        assert!(
            !cache.try_begin_verify(Duration::from_secs(60)),
            "a second concurrent verify must be rejected (single-flight)"
        );
        // end_verify releases the slot; it can be claimed again.
        cache.end_verify();
        assert!(!cache.verify_active());
        assert!(cache.try_begin_verify(Duration::from_secs(60)));
        cache.end_verify();
        // Deadman: even with the flag still set, an elapsed deadline reads
        // inactive, so a leaked guard can never strand the engine paused.
        {
            let mut state = cache.inner.write();
            state.verify_in_progress = true;
            state.verify_active_until = Some(std::time::Instant::now() - Duration::from_secs(1));
        }
        assert!(
            !cache.verify_active(),
            "an expired verify deadman must read inactive even with the flag set"
        );
    }

    #[test]
    fn sensors_snapshot_returns_sensor_map() {
        let cache = StateCache::new();
        cache.update_sensors(vec![
            make_sensor("hwmon:k10temp:0000:00:18.3:Tctl", 55.0),
            make_sensor("hwmon:nct6799:isa:fan", 30.0),
        ]);
        let sensors = cache.sensors_snapshot();
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors.len(), cache.snapshot().sensors.len());
        assert!((sensors["hwmon:k10temp:0000:00:18.3:Tctl"].value_c - 55.0).abs() < f64::EPSILON);
    }

    #[test]
    fn gpu_fans_snapshot_returns_gpu_fan_map() {
        let cache = StateCache::new();
        assert!(cache.gpu_fans_snapshot().is_empty());
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:2d:00.0", 75);
        let gpu_fans = cache.gpu_fans_snapshot();
        assert_eq!(gpu_fans.len(), 1);
        assert_eq!(
            gpu_fans["amd_gpu:0000:2d:00.0"].last_commanded_pct,
            Some(75)
        );
    }

    #[test]
    fn update_unavailable_sensors_evicts_stale_reading_and_recovers() {
        // DEC-193: a sensor that was readable then goes unreadable must be
        // evicted from `sensors` (no stale value served) and listed as
        // unavailable; recovery clears the list and lets it re-enter `sensors`.
        let cache = StateCache::new();
        cache.update_sensors(vec![make_sensor("hwmon:ath12k_hwmon:phy0:temp1", 48.0)]);
        assert!(cache
            .snapshot()
            .sensors
            .contains_key("hwmon:ath12k_hwmon:phy0:temp1"));

        cache.update_unavailable_sensors(vec![UnavailableSensor {
            id: "hwmon:ath12k_hwmon:phy0:temp1".into(),
            label: "temp1".into(),
            reason: "read error: Network is down (os error 100)".into(),
            since: Instant::now(),
        }]);

        let snap = cache.snapshot();
        assert!(
            !snap.sensors.contains_key("hwmon:ath12k_hwmon:phy0:temp1"),
            "stale reading must be evicted while unavailable"
        );
        assert_eq!(snap.unavailable_sensors.len(), 1);
        assert_eq!(snap.unavailable_sensors[0].label, "temp1");

        // Recovery: an empty unavailable set clears the list; a fresh reading
        // re-enters `sensors`.
        cache.update_unavailable_sensors(vec![]);
        cache.update_sensors(vec![make_sensor("hwmon:ath12k_hwmon:phy0:temp1", 50.0)]);
        let snap = cache.snapshot();
        assert!(snap.unavailable_sensors.is_empty());
        assert!(snap.sensors.contains_key("hwmon:ath12k_hwmon:phy0:temp1"));
    }

    #[test]
    fn update_unavailable_sensors_empty_is_noop_fast_path() {
        // The poll loop calls this every tick; with nothing unavailable it must
        // not disturb existing sensor state.
        let cache = StateCache::new();
        cache.update_sensors(vec![make_sensor("hwmon:k10temp:nodev:Tctl", 55.0)]);
        cache.update_unavailable_sensors(vec![]);
        let snap = cache.snapshot();
        assert!(snap.unavailable_sensors.is_empty());
        assert!(snap.sensors.contains_key("hwmon:k10temp:nodev:Tctl"));
    }

    #[test]
    fn take_resume_flag_swaps_and_clears() {
        // pwm_control calls take_resume_flag() once per set_pwm; it must return
        // true exactly once after a resume is signalled, then false until the
        // next resume. Locks the swap-and-clear semantics.
        let cache = StateCache::new();
        assert!(!cache.take_resume_flag(), "fresh cache: no resume pending");
        cache.set_resume_detected();
        assert!(cache.take_resume_flag(), "first take after resume is true");
        assert!(!cache.take_resume_flag(), "flag cleared after take");
    }
}
