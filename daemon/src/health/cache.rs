//! In-memory cache with batch updates and consistent snapshot reads.
//!
//! Uses `RwLock` for concurrent access: multiple readers, exclusive writer.
//! Updates are atomic at the batch boundary.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::health::state::*;

/// Fallback hwmon poll interval when nothing has published the real one
/// (DEC-267) — matches `StalenessConfig::default()` and the shipped
/// `polling.poll_interval_ms` default.
const DEFAULT_HWMON_POLL_INTERVAL_MS: u64 = 1000;

/// Multiple of the poll interval past which a CPU reading is treated as no
/// longer current (DEC-267).
///
/// Five intervals is the same multiplier `health::staleness` uses for its `Crit`
/// boundary — a reading the health rollup would already call critically stale is
/// not one to run the 105 °C ladder on. Deliberately not tighter: at 2x (the
/// `Warn` boundary) an ordinary scheduling hiccup would drop the sensor.
///
/// DEC-269: the two are **not** identical, and the earlier claim that they
/// "match" was wrong. This budget is floored at [`DEFAULT_HWMON_POLL_INTERVAL_MS`]
/// and the rollup's is not, so below a 1 s poll interval the rollup calls hwmon
/// `crit` while the safety rule still trusts the reading. That asymmetry is the
/// safe direction — more headroom means fewer false fallbacks — and is kept
/// deliberately.
const CPU_TEMP_STALE_INTERVALS: u32 = 5;

/// Hard ceiling on the staleness budget, however the poll interval is configured
/// (DEC-269).
///
/// [SAFETY] `polling.poll_interval_ms` is validated only as `>= 100`; the
/// 250–2000 ms clamp lives on the API route, not on the config file. So an admin
/// typo of `poll_interval_ms = 3600000` would otherwise hand the 105 °C rule a
/// five-hour staleness budget — silently disabling the protection with no
/// signal anywhere. Defence in depth under the DEC-253 trusted-local posture.
///
/// DEC-270: this used to say the daemon stops trusting a temperature older than
/// the ceiling *regardless* of the interval. That is no longer true, and taken
/// literally it was not safe either. Once the cadence passes this ceiling the
/// budget is *shorter than one poll period*, so every reading is stale on
/// arrival, `hottest_cpu_reading` never returns `Fresh`, and the 105 °C ladder —
/// which runs only on a fresh reading — is disabled entirely. The floor in
/// [`StateCache::cpu_temp_stale_after`] now makes that impossible at any
/// cadence, and `apply_runtime_overlay` keeps the cadence low enough that this
/// ceiling never even binds. So this constant is no longer self-standing: do not
/// remove either guard on the strength of it.
pub(crate) const CPU_TEMP_STALE_CEILING_MS: u64 = 30_000;

/// The slowest poll cadence this daemon can actually supervise, derived from the
/// two constants above rather than written down twice (DEC-270).
///
/// [SAFETY] Above this, `interval * CPU_TEMP_STALE_INTERVALS` exceeds
/// [`CPU_TEMP_STALE_CEILING_MS`], so the budget stops tracking the cadence and
/// the 5x headroom this design promises erodes towards 1x — by a ~15 s cadence a
/// single missed poll already reads as stale, and at the 30 s ceiling there is no
/// margin left at all. Past 30 s it inverts outright: the budget is shorter than
/// one poll period, every reading is stale on arrival, the 105 °C ladder is
/// silently disabled and fans pin at NO_SENSOR_SAFE_PCT. Rather than pick a
/// failure direction, refuse the cadence:
/// `apply_runtime_overlay` clamps to this and logs a warning, so the daemon still
/// starts (a fan controller that will not boot over a config typo is worse than
/// one that polls faster than it was told) and still supervises temperature.
pub const MAX_SUPERVISABLE_POLL_INTERVAL_MS: u64 =
    CPU_TEMP_STALE_CEILING_MS / CPU_TEMP_STALE_INTERVALS as u64;

// `MAX_SUPERVISABLE_POLL_INTERVAL_MS` is what `apply_runtime_overlay` clamps the
// cadence *down to*, so the danger is it becoming absurdly small: raising
// `CPU_TEMP_STALE_INTERVALS` far enough drives it below the API's own 250 ms
// floor, and past `CEILING` it reaches 0 — which would clamp the interval to
// zero and panic `tokio::time::interval` in the hwmon poll loop, killing the only
// writer of the sensor map the 105 °C rule reads.
//
// Asserting `MAX * INTERVALS <= CEILING` instead would be vacuous: `MAX` is
// *derived* by that division, so it holds for every input.
const _: () = assert!(MAX_SUPERVISABLE_POLL_INTERVAL_MS >= 250);

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
    /// The hwmon poll loop's configured interval, in ms (DEC-267).
    ///
    /// [SAFETY] Published here so the profile engine can tell a *stale* CPU
    /// reading from a current one. The engine's 105 °C rule reads
    /// `sensors_snapshot()`, which has no freshness filter of its own — so if
    /// the poll loop dies the last temperature is returned forever, the rule
    /// never crosses its threshold, and the no-sensor fallback never engages
    /// because the sensor is not *missing*, merely frozen. See
    /// `profile_engine::hottest_fresh_cpu_c`.
    ///
    /// Set from the same `polling.poll_interval_ms` that builds
    /// `StalenessConfig`, and deliberately set next to it in `main.rs` so the
    /// two derivations cannot drift. `poll_interval_ms` has a lower bound of
    /// 100 ms and **no upper bound**, which is why this is configured rather
    /// than a constant: a fixed budget would permanently mark a legitimately
    /// slow-polling system stale and pin its fans at `NO_SENSOR_SAFE_PCT`.
    hwmon_poll_interval_ms: AtomicU64,
    /// Serialises GPU fan writes between the profile engine and
    /// `POST /gpu/{id}/fan/reset` (DEC-255).
    ///
    /// [SAFETY] GPU writes hold no per-device lock by design (DEC-045), which was
    /// fine while every write was a single value. A PMFW curve write is not: it
    /// is N point writes followed by a `"c"` commit, and `reset_to_auto` is
    /// `"r"`+`"c"` then `"1"`+`"c"`. Two of those interleaving can commit a curve
    /// that is neither the profile's nor firmware-auto — a corrupt state no
    /// later tick reconciles, because the reset relinquishes the fan and the
    /// engine then skips it. The last-moment relinquish re-check narrows that
    /// race; only mutual exclusion removes it.
    ///
    /// Deliberately ONE lock rather than one per GPU: writes are 1 Hz and
    /// coalesced, machines carry one or two GPUs, and serialising them costs
    /// nothing measurable while a keyed map costs a lookup and more surface.
    ///
    /// `tokio::sync::Mutex`, not `parking_lot`: it is held across
    /// `spawn_blocking`. Lock order — strictly OUTSIDE `inner`; the write path
    /// takes `inner` briefly beneath it and no path holds `inner` across a GPU
    /// write, so no inversion is possible.
    gpu_write_lock: Arc<tokio::sync::Mutex<()>>,
    /// Monotonic counter bumped whenever the OpenFanController's *device-side*
    /// duty state may no longer match what we last commanded (DEC-256).
    ///
    /// `FanController` coalesces a write away when it equals `last_commanded_pct`,
    /// which is only sound while that cache reflects the device. Two events break
    /// that and neither was signalled: a system resume, and a serial reconnect —
    /// the poll loop swaps the transport underneath the controller after a USB
    /// re-enumeration, leaving per-channel state describing a device that may have
    /// come back at its power-on default. Every subsequent identical command was
    /// then coalesced into silence, so the fan sat at the firmware default with
    /// the daemon reporting the commanded value.
    ///
    /// A counter rather than a flag because `take_resume_flag` is a *swap*: the
    /// first consumer to call it clears it for everyone, and hwmon already owns
    /// that one. Each consumer compares against its own last-seen value instead —
    /// the same shape as `profile_activation_epoch`.
    openfan_write_generation: AtomicU64,
}

impl StateCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(DaemonState::default()),
            resume_detected: AtomicBool::new(false),
            profile_activation_epoch: AtomicU64::new(0),
            hwmon_poll_interval_ms: AtomicU64::new(DEFAULT_HWMON_POLL_INTERVAL_MS),
            gpu_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            openfan_write_generation: AtomicU64::new(0),
        }
    }

    /// Publish the hwmon poll loop's configured interval (DEC-267).
    ///
    /// Called once at startup from the same value that builds
    /// `StalenessConfig`. Idempotent and lock-free.
    pub fn set_hwmon_poll_interval_ms(&self, ms: u64) {
        self.hwmon_poll_interval_ms.store(ms, Ordering::Relaxed);
    }

    /// How old a CPU temperature reading may be before the safety rule must
    /// treat it as absent rather than current (DEC-267).
    ///
    /// [SAFETY] This is what converts "the poll loop died" into "no CPU sensor",
    /// which is a state the daemon already handles correctly and has tested
    /// (DEC-132's 5-cycle fallback, DEC-190's latched-emergency dropout). Without
    /// it a dead poll loop freezes the last reading, the 105 °C ladder is
    /// evaluated forever against a temperature that can no longer rise, and
    /// `/status` reports a healthy engine throughout — because the engine *is*
    /// ticking, on stale data.
    pub fn cpu_temp_stale_after(&self) -> Duration {
        let interval = self
            .hwmon_poll_interval_ms
            .load(Ordering::Relaxed)
            .max(DEFAULT_HWMON_POLL_INTERVAL_MS);
        // `saturating_mul`, not `*`: a wrapping multiply would produce a *tiny*
        // budget — permanent false-stale, every fan pinned at NO_SENSOR_SAFE_PCT
        // — which is the worst possible direction to fail in (DEC-269).
        let budget = interval
            .saturating_mul(u64::from(CPU_TEMP_STALE_INTERVALS))
            .min(CPU_TEMP_STALE_CEILING_MS)
            // [SAFETY] Never below one poll period. The ceiling above is what
            // stops a mistyped interval buying an unbounded trust window, but
            // applied alone it fails the *other* way: with the cadence slower
            // than the ceiling, every reading is older than its budget the
            // moment it lands, so the 105 °C ladder — which only runs on a
            // `Fresh` reading — is permanently disabled and fans sit at
            // NO_SENSOR_SAFE_PCT on healthy hardware, with `/status` reporting a
            // ticking engine throughout. `apply_runtime_overlay` clamps the
            // interval to `MAX_SUPERVISABLE_POLL_INTERVAL_MS` so this floor is
            // unreachable in practice; it is kept because this atomic is
            // publicly settable and the invariant belongs where it is relied on,
            // not only where it currently happens to hold.
            .max(interval);
        Duration::from_millis(budget)
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

    /// Record that the profile engine reached this tick's safety decision:
    /// publish the thermal safety override state and stamp the engine liveness
    /// heartbeat.
    ///
    /// Unconditional write under the write lock (CONC-3, 2026-07-21 audit).
    /// An earlier fast path (EFF-4) took a read lock to compare-and-skip
    /// first — lossless only while the engine tick stayed the sole writer,
    /// an invariant no type enforces and a read→write TOCTOU if it ever
    /// broke. The engine calls this once per 1 Hz tick with a short string;
    /// an uncontended `parking_lot` write at that rate is noise, so the
    /// invariant-free form wins.
    ///
    /// DEC-249: the two writes are deliberately one call under one lock rather
    /// than two independent setters. The heartbeat's whole purpose is to tell a
    /// client whether `thermal_state` is still being published, so it must be
    /// stamped at exactly the point that publishes it — bound together, an early
    /// `continue` added above this line freezes both, and the heartbeat reports
    /// the outage. Two separate call sites could drift, leaving the heartbeat
    /// claiming health while the safety state went stale: the exact failure this
    /// surface exists to catch.
    pub fn record_engine_tick(&self, thermal_state: &str) {
        let now = Instant::now();
        let mut state = self.inner.write();
        state.thermal_override_state = Some(thermal_state.to_string());
        state.subsystem_timestamps.engine_started = Some(now);
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
    /// Returns `true` if **this call** claimed the fan, `false` if it was
    /// already relinquished (DEC-255).
    ///
    /// The caller must roll back only when it claimed: an unconditional rollback
    /// lets a second, failing reset clear the flag a first, *successful* reset
    /// owns — handing the fan back to the engine after the API told the user it
    /// was reset. That needs no concurrency at all, just two clicks.
    #[must_use]
    pub fn relinquish_gpu_fan(&self, fan_id: &str) -> bool {
        self.inner
            .write()
            .relinquished_gpu_fans
            .insert(fan_id.to_string())
    }

    /// Stamp the engine's tick-*completed* timestamp (DEC-259).
    ///
    /// Called from a drop guard in the engine loop so it fires on every exit
    /// path. Together with the started stamp this lets `compute_health` tell a
    /// slow tick (busy — report it, do not alarm) from a stopped one (the sole
    /// PWM writer is gone — alarm).
    pub fn record_engine_tick_complete(&self) {
        self.inner.write().subsystem_timestamps.engine_completed = Some(Instant::now());
    }

    /// Acquire exclusive access to the GPU fan write path (DEC-255).
    ///
    /// Returns an **owned** guard so it can be moved into the `spawn_blocking`
    /// task that performs the writes. That matters for more than ergonomics: if
    /// the HTTP client disconnects, the handler's future is dropped, and a
    /// borrowed guard would be released while the blocking write was still in
    /// flight — re-opening the very window this closes.
    pub async fn lock_gpu_writes(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.gpu_write_lock.clone().lock_owned().await
    }

    /// Acquire the GPU write lock, or give up after `within`.
    ///
    /// `fan/reset` uses this rather than an unbounded wait. Both of the other
    /// producers hold the lock for very different spans: an engine tick holds
    /// it for a few milliseconds, so a reset should simply wait that out, but a
    /// `fan/verify` holds it for its whole multi-second window, and blocking
    /// there would strand the caller past the GUI's 5 s timeout with no
    /// explanation. A bounded wait distinguishes the two — wait out a tick,
    /// report a conflict for a verify.
    pub async fn lock_gpu_writes_soon(
        &self,
        within: std::time::Duration,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        tokio::time::timeout(within, self.gpu_write_lock.clone().lock_owned())
            .await
            .ok()
    }

    /// Un-relinquish a single GPU fan — the rollback for a reset that claimed
    /// the flag up-front and then failed (DEC-254).
    ///
    /// `POST /gpu/{id}/fan/reset` sets the flag *before* writing firmware-auto,
    /// so the engine is already standing off while the write is in flight. If
    /// that write then fails, leaving the flag set would strand the fan: not
    /// reset, and no longer driven by the engine either. Distinct from
    /// [`Self::clear_relinquished_gpu_fans`], which clears every fan on profile
    /// activation and would also undo an unrelated, successful reset.
    pub fn unrelinquish_gpu_fan(&self, fan_id: &str) {
        self.inner.write().relinquished_gpu_fans.remove(fan_id);
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
    /// Read the OpenFan write generation (DEC-256). `FanController` compares this
    /// against its own last-seen value and drops its coalescing cache on a change.
    pub fn openfan_write_generation(&self) -> u64 {
        self.openfan_write_generation.load(Ordering::SeqCst)
    }

    /// Declare that the OpenFanController's device-side duty may no longer match
    /// what we last commanded, so the next write for each channel must actually
    /// reach the wire (DEC-256). Called on serial reconnect and on resume.
    pub fn invalidate_openfan_writes(&self) {
        self.openfan_write_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn set_resume_detected(&self) {
        // A resume invalidates OpenFan's coalescing cache for the same reason it
        // clears hwmon's manual-mode flags: the device may have been reset
        // underneath us (DEC-256).
        self.invalidate_openfan_writes();
        self.resume_detected.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    // ── DEC-255 / release review 2026-08-10: bounded GPU-write acquisition ──

    #[tokio::test]
    async fn a_free_gpu_write_lock_is_acquired_immediately() {
        let cache = StateCache::new();
        let got = cache
            .lock_gpu_writes_soon(std::time::Duration::from_millis(200))
            .await;
        assert!(got.is_some(), "an uncontended lock must be granted");
    }

    #[tokio::test]
    async fn a_held_gpu_write_lock_times_out_rather_than_blocking() {
        // This is what lets `fan/reset` tell an engine tick (milliseconds) apart
        // from a `fan/verify` (multiple seconds) and report a conflict instead
        // of hanging past the GUI's 5 s client timeout.
        let cache = StateCache::new();
        let _held = cache.lock_gpu_writes().await;

        let got = cache
            .lock_gpu_writes_soon(std::time::Duration::from_millis(50))
            .await;
        assert!(
            got.is_none(),
            "a held lock must time out, not block forever"
        );
    }

    #[tokio::test]
    async fn the_lock_is_grantable_again_once_released() {
        let cache = StateCache::new();
        let held = cache.lock_gpu_writes().await;
        drop(held);
        assert!(
            cache
                .lock_gpu_writes_soon(std::time::Duration::from_millis(200))
                .await
                .is_some(),
            "releasing must actually free the lock"
        );
    }

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
        cache.record_engine_tick("emergency");
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

        cache.record_engine_tick("normal");
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("normal")
        );

        // Redundant write — value stays correct.
        cache.record_engine_tick("normal");
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("normal")
        );

        // Genuine change must be applied, not skipped.
        cache.record_engine_tick("emergency");
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("emergency")
        );
        cache.record_engine_tick("recovery");
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("recovery")
        );
    }

    #[test]
    fn record_engine_tick_stamps_the_heartbeat_with_the_thermal_state() {
        // DEC-249: the heartbeat exists to tell a client whether `thermal_state`
        // is still being published, so the two must move together. A fresh cache
        // has never ticked — that is what makes a dead-on-arrival engine visible.
        let cache = StateCache::new();
        assert!(
            cache
                .snapshot()
                .subsystem_timestamps
                .engine_started
                .is_none(),
            "a cache that has seen no tick must not look alive"
        );

        cache.record_engine_tick("normal");
        let first = cache
            .snapshot()
            .subsystem_timestamps
            .engine_started
            .expect("tick must stamp the heartbeat");

        cache.record_engine_tick("emergency");
        let snap = cache.snapshot();
        assert!(
            snap.subsystem_timestamps.engine_started.unwrap() >= first,
            "heartbeat must advance monotonically"
        );
        assert_eq!(
            snap.thermal_override_state.as_deref(),
            Some("emergency"),
            "the same call must publish the thermal state"
        );
    }

    #[tokio::test]
    async fn gpu_write_lock_actually_excludes() {
        // DEC-255: the property the whole GPU-race fix now rests on. A PMFW
        // curve write is N point writes plus a commit and a reset is "r"+"c";
        // if these are not mutually exclusive they can interleave into a curve
        // that is neither the profile's nor firmware-auto, which no later tick
        // reconciles.
        let cache = Arc::new(StateCache::new());
        let held = cache.lock_gpu_writes().await;

        let contender = cache.clone();
        let blocked = tokio::time::timeout(std::time::Duration::from_millis(50), async move {
            contender.lock_gpu_writes().await
        })
        .await;
        assert!(blocked.is_err(), "a second GPU writer must wait");

        drop(held);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                cache.lock_gpu_writes(),
            )
            .await
            .is_ok(),
            "and must proceed once the first releases"
        );
    }

    #[test]
    fn relinquish_reports_whether_this_call_claimed() {
        // DEC-255: the bool is what makes the rollback ownership-aware. Without
        // it a second, failing reset clears the flag a first, successful reset
        // owns — no concurrency required, just two clicks.
        let cache = StateCache::new();
        assert!(
            cache.relinquish_gpu_fan("amd_gpu:0000:03:00.0"),
            "first claim"
        );
        assert!(
            !cache.relinquish_gpu_fan("amd_gpu:0000:03:00.0"),
            "second call must report that it did NOT claim"
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
