//! In-memory cache with batch updates and consistent snapshot reads.
//!
//! Uses `RwLock` for concurrent access: multiple readers, exclusive writer.
//! Updates are atomic at the batch boundary.

use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
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
/// not one to run the thermal ladder on. Deliberately not tighter: at 2x (the
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
/// typo of `poll_interval_ms = 3600000` would otherwise hand the thermal-emergency rule a
/// five-hour staleness budget — silently disabling the protection with no
/// signal anywhere. Defence in depth under the DEC-253 trusted-local posture.
///
/// DEC-270: this used to say the daemon stops trusting a temperature older than
/// the ceiling *regardless* of the interval. That is no longer true, and taken
/// literally it was not safe either. Once the cadence passes this ceiling the
/// budget is *shorter than one poll period*, so every reading is stale on
/// arrival, `hottest_cpu_reading` never returns `Fresh`, and the thermal ladder —
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
/// one poll period, every reading is stale on arrival, the thermal ladder is
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
// writer of the sensor map the thermal-emergency rule reads.
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
    /// reading from a current one. The engine's thermal-emergency rule reads
    /// `sensors_snapshot()`, which has no freshness filter of its own — so if
    /// the poll loop dies the last temperature is returned forever, the rule
    /// never crosses its threshold, and the no-sensor fallback never engages
    /// because the sensor is not *missing*, merely frozen. See
    /// `profile_engine::hottest_fresh_cpu_c`.
    ///
    /// Set from the same `polling.poll_interval_ms` that builds
    /// `StalenessConfig`, and deliberately set next to it in `main.rs` so the
    /// two derivations cannot drift. `poll_interval_ms` runs from 100 ms to
    /// [`MAX_SUPERVISABLE_POLL_INTERVAL_MS`] (DEC-270 clamps the admin file; the
    /// API route is tighter still at 250-2000), which is why this is configured
    /// rather than a constant: a fixed budget would permanently mark a
    /// legitimately slow-polling system stale and pin its fans at
    /// `NO_SENSOR_SAFE_PCT`.
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
    /// Monotonic count of observed system resumes (AIO-MB Phase 5).
    ///
    /// A third consumer of the resume signal, and it needs its own counter for
    /// the reason spelled out above: `take_resume_flag` is a swap that hwmon
    /// already owns, so a second caller would steal the event from it. Reusing
    /// `openfan_write_generation` would be wrong for a different reason — that
    /// one is *also* bumped on serial reconnect, so it answers "may the device
    /// have been reset?", not "did the system resume?".
    ///
    /// The validation recorder compares this against its own last-seen value to
    /// emit a `resume` event marker, and never mutates it.
    resume_generation: AtomicU64,
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
            resume_generation: AtomicU64::new(0),
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
    /// it a dead poll loop freezes the last reading, the thermal ladder is
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
            // moment it lands, so the thermal ladder — which only runs on a
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
    /// Returns how many already-known channels this batch did **not** cover
    /// (OFS-b) — for the poll loop's short-frame log, not for the stamp.
    ///
    /// The stamp below stays unconditional on purpose. It answers *liveness*
    /// ("a `ReadAllRpm` returned Ok"), which a short frame does not falsify, and
    /// gating it on coverage would leave it meaning neither liveness nor
    /// freshness. Freshness is answered separately by `poll_subsystem_health`,
    /// which reduces over each channel's own `updated_at` — a channel this batch
    /// skipped keeps its old timestamp and ages on its own, so partial coverage
    /// surfaces there with nothing needing to detect it.
    ///
    /// The count is still worth returning, because a *log line* is exactly the
    /// thing that needs the detection: before this, a short frame produced no
    /// journal evidence at all.
    pub fn update_openfan_fans(&self, fans: Vec<OpenFanState>) -> usize {
        let now = Instant::now();
        let mut state = self.inner.write();
        let covered: std::collections::HashSet<u8> = fans.iter().map(|f| f.channel).collect();
        // F6: count only channels a poll has actually MEASURED. `force_all_with_floor` writes
        // `0..NUM_CHANNELS` unconditionally, so one thermal emergency mints an entry
        // for every channel the firmware does not report; those can never be
        // covered by a later frame, so counting them would latch the short-frame
        // warning on for the process lifetime and never emit the recovery line.
        let uncovered = state
            .openfan_fans
            .values()
            .filter(|f| f.rpm_polled && !covered.contains(&f.channel))
            .count();
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
        uncovered
    }

    /// Update all hwmon fan readings as a batch.
    pub fn update_hwmon_fans(&self, fans: Vec<HwmonFanState>) {
        let now = Instant::now();
        let mut state = self.inner.write();
        for mut fan in fans {
            // Merge, do not replace, the fields only the POLL samples (DEC-316).
            //
            // `HwmonPwmController::set_pwm` constructs a whole `HwmonFanState`
            // on every engine tick — including its coalesce fast path, which
            // skips sysfs but still refreshes the cache — and has no cheap way
            // to re-read these. A bare `insert` therefore erased the poll's
            // answer at ~1 Hz, making both fields permanently absent for any
            // header under an active profile: exactly the header whose fan
            // alarm matters most. Mirrors `update_openfan_fans`, which carries
            // `last_commanded_pwm` forward for the same reason.
            //
            // `pwm_readback_pct` joined this list in AIO-MB Phase 5 for exactly
            // the same reason: the poll is its only producer, so without the
            // merge a controlled header — the one a validation session cares
            // about — would report no readback at all.
            if fan.alarm.is_none()
                || fan.pwm_enable_mode.is_none()
                || fan.pwm_readback_pct.is_none()
            {
                if let Some(existing) = state.hwmon_fans.get(&fan.id) {
                    if fan.alarm.is_none() {
                        fan.alarm = existing.alarm;
                    }
                    if fan.pwm_enable_mode.is_none() {
                        fan.pwm_enable_mode = existing.pwm_enable_mode;
                    }
                    if fan.pwm_readback_pct.is_none() {
                        fan.pwm_readback_pct = existing.pwm_readback_pct;
                    }
                }
            }
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
    /// `trigger_c` is the trip point the rule ACTED on this tick, published in
    /// the same write as the state it produced (DEC-308). Same write on purpose:
    /// DEC-292's invariant is that what `/diagnostics/hardware` reports equals
    /// what the rule acts on, and since DEC-308 that value is per-machine rather
    /// than a constant the handler could read for itself.
    pub fn record_engine_tick(&self, thermal_state: &str, trigger_c: f64) {
        let now = Instant::now();
        let mut state = self.inner.write();
        state.thermal_override_state = Some(thermal_state.to_string());
        state.thermal_emergency_trigger_c = Some(trigger_c);
        state.subsystem_timestamps.engine_started = Some(now);
    }

    /// Try to claim the single hardware-verify slot, pausing the profile
    /// engine's write phase for the verify's lifetime. Returns the claimed
    /// **epoch**, or `None` if a verify is genuinely in progress (the caller
    /// must reject with 409) — this single-flight guard stops two concurrent
    /// verifies from clobbering each other's pause or lease (DEC-165).
    ///
    /// `window` is a deadman backstop, and DEC-296 made it two-sided. It always
    /// bounded the ENGINE half: [`Self::verify_active`] expires against
    /// `verify_active_until`, so the engine resumes writing after `window` even
    /// if the guard leaks. It did **not** bound the SLOT half — nothing cleared
    /// `verify_in_progress` on expiry, so a single leaked guard made every later
    /// verify *and* calibration return 409 for the rest of the process lifetime.
    /// An expired window now counts as free.
    ///
    /// That alone would be unsafe, which is why the epoch exists: a claimant
    /// whose window expired may still be alive (DEC-290 moved the guard inside a
    /// `spawn_blocking` task, so a wedged sysfs write holds it), and when it
    /// finally returns its `Drop` must not release the *successor's* pause
    /// mid-test-write. See [`Self::end_verify`].
    pub fn try_begin_verify(&self, window: Duration) -> Option<u64> {
        let mut state = self.inner.write();
        let now = Instant::now();
        let genuinely_held = state.verify_in_progress
            && state
                .verify_active_until
                .is_some_and(|deadline| now < deadline);
        if genuinely_held {
            return None;
        }
        if state.verify_in_progress {
            // DEC-296: we are STEALING an expired claim, not taking a free one.
            // Log it: the previous holder may still be alive and about to fail
            // its restore, and `force_take_lease` logs the eviction that causes
            // without logging the supersession that explains it.
            log::warn!(
                "verify slot: claiming a slot whose deadman elapsed (epoch {} superseded). \
                 The previous holder did not release it; if it is still running, its \
                 restore may fail.",
                state.verify_epoch
            );
        }
        // `wrapping_add` to be non-panicking rather than because wrapping is
        // reachable: at one increment per verify, u64 does not wrap.
        state.verify_epoch = state.verify_epoch.wrapping_add(1);
        state.verify_in_progress = true;
        state.verify_active_until = Some(now + window);
        Some(state.verify_epoch)
    }

    /// Release the hardware-verify slot (the engine resumes writing next tick),
    /// but **only if `epoch` is still the current claim** (DEC-296).
    ///
    /// A stranded claimant that returns after its deadman expired no longer owns
    /// the slot; releasing unconditionally would clear a *successor's* pause
    /// while that successor is mid-test-write, letting the engine overwrite its
    /// test duty and making its verdict false. Ignoring the stale release is the
    /// whole reason the claim returns a token.
    pub fn end_verify(&self, epoch: u64) -> bool {
        let mut state = self.inner.write();
        if state.verify_epoch != epoch {
            log::warn!(
                "verify slot: ignoring a release from superseded epoch {epoch} \
                 (current {}) — its deadman elapsed and another diagnostic owns the slot",
                state.verify_epoch
            );
            return false;
        }
        state.verify_in_progress = false;
        state.verify_active_until = None;
        true
    }

    /// Extend this claim's deadman, but only while `epoch` still owns the slot
    /// (DEC-296). Returns `false` if it has been superseded.
    ///
    /// The deadman measures **liveness**, not total duration. Without this a
    /// claimant that is merely slow — the hwmon verify's post-settle `read_state`
    /// is plain `fs::read_to_string` on sysfs and can block — is superseded at
    /// the window, its lease is force-taken by the successor, and its restore
    /// then fails with an opaque `InvalidLease`, stranding the header at the test
    /// duty. A claimant that reaches a checkpoint proves it is alive and keeps
    /// its slot; one that never reaches one is genuinely wedged and is correctly
    /// superseded.
    pub fn renew_verify(&self, epoch: u64, window: Duration) -> bool {
        let mut state = self.inner.write();
        if state.verify_epoch != epoch {
            return false;
        }
        state.verify_active_until = Some(Instant::now() + window);
        true
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

    /// Record whether a backend write is still outstanding (DEC-289).
    ///
    /// Edge-triggered: the stamp is set on the first stalled tick and left alone
    /// while the stall persists, so it answers "since when", not "as of when" —
    /// which is what `engine_health` needs to tell a slow write from a wedged
    /// one. Cleared the moment a write lands again.
    pub fn record_engine_write_stall(&self, outstanding: bool) {
        let mut state = self.inner.write();
        let stamp = &mut state.subsystem_timestamps.engine_writes_stalled_since;
        match (outstanding, *stamp) {
            (true, None) => *stamp = Some(Instant::now()),
            (true, Some(_)) => {}
            (false, _) => *stamp = None,
        }
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
            // OFS-i: deliberately does NOT touch `updated_at`. That field is when
            // this channel's RPM was last READ, and `build_fan_entries` publishes
            // it as the reading's `age_ms`. Refreshing it here made a *command*
            // present as fresh *telemetry*: the fan reported `age_ms` near zero
            // beside an `rpm` frozen at whatever the last real poll saw, and
            // `stall_detected` was then computed from that frozen value. Widest
            // exactly where it matters — a thermal `force_all_with_floor` writes every
            // channel, and a ~10-byte `SetPwm` completes on a degraded link far
            // more readily than an ~80-byte `ReadAllRpm`, so "poll dead, writes
            // still acking" showed every fan FRESH while nothing was measuring.
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
        // OFS-k: deliberately does NOT touch `updated_at` — byte-for-byte the same
        // rule DEC-302 (OFS-i) established for `set_openfan_commanded_pwm` one
        // function above, and this was the surviving instance of it. That field is
        // when this GPU's fan telemetry was last READ, and `build_fan_entries`
        // publishes it as the reading's `age_ms`. Refreshing it here made a
        // *command* present as fresh *telemetry*: the fan reported an `age_ms`
        // near zero beside an `rpm`/`duty_pct` frozen at whatever the last real
        // poll saw. The insert arm below still stamps it, which is correct — a
        // GPU seen for the first time by a write has no prior reading, and its
        // `rpm: None` says so honestly.
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

    /// Replace the set of controls the engine cannot resolve (273-i).
    ///
    /// Called by the engine tick on EVERY tick, including the early-return paths
    /// — see the call sites. That unconditional discipline is the point: a
    /// control listed here is "not being commanded right now", and a list that
    /// froze because an early `continue` skipped the publish would keep asserting
    /// that about a control the engine has since resumed, or has stopped
    /// evaluating entirely. DEC-249 is the same lesson one surface over.
    ///
    /// Unlike [`Self::update_unavailable_sensors`] this evicts nothing: a skipped
    /// control's *fans* are still real and still reporting RPM. What is unknown
    /// is only whether anything is commanding them, which is exactly what this
    /// list says.
    ///
    /// The common case (nothing skipped, nothing previously skipped) takes only a
    /// shared read lock and returns — the engine calls this at 1 Hz.
    pub fn update_skipped_controls(&self, skipped: Vec<SkippedControl>) {
        // Same deliberate double-checked shape as `update_unavailable_sensors`:
        // the fast-path read guard is dropped before the write lock is taken, and
        // the interleaving is harmless because the write is idempotent. The fast
        // path exists so the every-tick common case never contends for the write
        // lock; do not collapse it into a single lock.
        if skipped.is_empty() && self.inner.read().skipped_controls.is_empty() {
            return;
        }
        self.inner.write().skipped_controls = skipped;
    }

    /// Publish the engine's per-tick control state — which controls are skipped
    /// (273-i) and what output each evaluated control applied (277-k) — in **one
    /// write**.
    ///
    /// Called on EVERY tick from the single publish point in
    /// `TickCompletion::drop`, including the early-`continue` paths. An output is
    /// a *level*, and a level that froze because a `continue` skipped the publish
    /// would keep reporting a duty the engine has since stopped applying.
    /// Publishing empty is meaningful, not a gap — it says no control is being
    /// evaluated right now (no profile, or a thermal force driving the fans
    /// directly), which is exactly when a card must stop showing a number.
    ///
    /// **The two fields must move together, which is why this is one method and
    /// not two.** They were separate `write()` takes for a while, and each was
    /// individually atomic, but the *pair* was not: on a resolvability transition
    /// a control leaves `tick_outputs` as it enters the skip list, so a `/poll`
    /// landing in the gap saw the new skip entry beside the stale output and
    /// listed one control on both surfaces when it belongs on exactly one. Both
    /// readers take them together — `status_handler` and `poll_handler` build
    /// both inside a single `read_with` — so one guard closes the window
    /// completely. `docs/08` states that absence from `control_outputs` is
    /// *meaningful*, so a torn read there contradicts the contract published in
    /// the same release that introduced the field.
    ///
    /// It is also cheaper: with a profile active `control_outputs` is non-empty
    /// every tick, so the two-call shape took the write lock twice per tick where
    /// this takes it once.
    ///
    /// Same deliberate double-checked fast path as [`Self::update_skipped_controls`],
    /// now spanning both fields: the read guard is dropped before the write lock
    /// is taken, and the interleaving is harmless because the write is idempotent.
    /// It exists so the common no-profile case never contends for the write lock
    /// at 1 Hz; do not collapse it into a single lock.
    pub fn update_control_state(&self, skipped: Vec<SkippedControl>, outputs: Vec<ControlOutput>) {
        if skipped.is_empty() && outputs.is_empty() {
            let snap = self.inner.read();
            if snap.skipped_controls.is_empty() && snap.control_outputs.is_empty() {
                return;
            }
        }
        let mut state = self.inner.write();
        state.skipped_controls = skipped;
        state.control_outputs = outputs;
    }

    /// Drop cached readings for sensors that no longer exist.
    ///
    /// [SAFETY] DEC-272 (register row 01-c). [`update_sensors`] only ever
    /// *inserts*, so a descriptor that disappears — a module unload, a rescan
    /// that no longer finds it — left its last reading in this map forever. The
    /// DEC-193 quarantine does not cover it: the failure tracker deliberately
    /// *forgets* a descriptor that has genuinely unbound, so such a sensor is
    /// never listed unavailable and so never evicted by
    /// [`update_unavailable_sensors`]. Its reading therefore aged into
    /// `CpuReading::Stale` and stayed there for the life of the process — which
    /// is why DEC-190's `Absent` branch was largely dead for the very scenario it
    /// was written for, and why a vanished CPU sensor could hold a fan output
    /// rather than fall to the no-sensor floor.
    ///
    /// `live` MUST be the union of the poll loop's current descriptor set and the
    /// ids it actually read this tick. Descriptors alone is wrong: NVML
    /// temperatures (DEC-204) are merged into the readings without a descriptor
    /// of their own, so retaining on the descriptor set would evict every NVIDIA
    /// sensor on every tick.
    ///
    /// A present-but-*failing* descriptor stays in the descriptor set, so it is
    /// retained here and continues through the quarantine path unchanged. This
    /// evicts the vanished, not the unreadable — for descriptor-BEARING sensors.
    /// NVML temps (see above) have no descriptor at all, so one that a tick could
    /// not read is simply absent from `live` and is evicted; it returns on the
    /// next good read with its session min/max reset. That is a monitoring wart on
    /// an off-by-default experimental path, not a control-path defect: GPU temps
    /// are excluded from the thermal ladder by design (DEC-130).
    ///
    /// The caller is responsible for only passing a COMPLETE `live` set. A
    /// discovery pass that skipped an unreadable chip returns a partial descriptor
    /// list, and evicting on that evidence took a live CPU sensor to
    /// `CpuReading::Absent` — see the guard at the `polling.rs` call site
    /// ([SAFETY] DEC-272 round 2).
    /// Evict hwmon fan entries that nothing has refreshed within `max_age`
    /// (OFS-m).
    ///
    /// The sibling of [`Self::retain_sensors`] for the fan map, and it keys on age
    /// rather than on a live-id set for a reason specific to this map: the poll
    /// loop's PWM header set is built once at startup (`polling.rs`, `Arc::new`)
    /// and never re-enumerated — `/hwmon/rescan` refreshes sensor descriptors, not
    /// this — so "the currently-discovered header set" never shrinks and retaining
    /// against it would evict nothing, ever. Age is the only signal that actually
    /// distinguishes a header still being read from one that has gone silent.
    ///
    /// Both writers refresh `updated_at`: the poll loop on a successful read, and
    /// `HwmonPwmController::set_pwm` on every engine write (it reads the header's
    /// own RPM). So an entry is evicted only when *neither* has touched it —
    /// which is exactly the frozen-forever state this exists to remove, and is why
    /// an actively-commanded fan cannot be evicted from under a running profile.
    ///
    /// Nothing on the control or safety path consumes this map — the engine reads
    /// headers from `HwmonPwmController`, not from here — so eviction cannot
    /// affect fan control. It changes `/fans` and `/poll` only.
    pub fn retain_fresh_hwmon_fans(&self, max_age: std::time::Duration) {
        let now = Instant::now();
        // Fast path, mirroring `retain_sensors`: the steady-state tick evicts
        // nothing and must not take the write lock to discover that.
        //
        // `retain_sensors` documents that its read-then-write gap is safe only
        // because it has a single writer, and warns that a second one would
        // require a re-check under the write lock. **This map genuinely has two
        // writers** — the poll loop and `HwmonPwmController::set_pwm` — so that
        // warning applies here, and is satisfied rather than inherited: the
        // predicate below is re-evaluated against each entry's own `updated_at`
        // while holding the write lock, not against an id set captured during the
        // read. An entry inserted in the gap is therefore judged on its real age
        // and survives. `now` is sampled before the read, so a slow acquisition
        // makes ages look *smaller* — the conservative direction, since the worst
        // outcome is deferring an eviction by one tick.
        if self
            .inner
            .read()
            .hwmon_fans
            .values()
            .all(|f| now.saturating_duration_since(f.updated_at) <= max_age)
        {
            return;
        }
        let mut state = self.inner.write();
        state
            .hwmon_fans
            .retain(|_, f| now.saturating_duration_since(f.updated_at) <= max_age);
    }

    pub fn retain_sensors(&self, live: &HashSet<String>) {
        // Fast path mirrors `update_unavailable_sensors`: the steady-state tick
        // evicts nothing and must not take the write lock to discover that.
        //
        // The read guard is dropped before the write is taken, so the two are NOT
        // atomic together. What makes that harmless is the single-writer
        // invariant, not a re-check: the poll loop is the only production writer
        // of `state.sensors` (the other `update_sensors` callers are #[cfg(test)]),
        // and it is also the only caller of this method, so nothing can insert a
        // sensor into the gap. If a second writer is ever added, this needs a
        // re-check under the write lock — it does not have one today.
        if self.inner.read().sensors.keys().all(|id| live.contains(id)) {
            return;
        }
        let mut state = self.inner.write();
        state.sensors.retain(|id, _| live.contains(id));
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

    /// Read the resume counter (AIO-MB Phase 5). Non-consuming, unlike
    /// `take_resume_flag` — any number of observers may watch it.
    pub fn resume_generation(&self) -> u64 {
        self.resume_generation.load(Ordering::SeqCst)
    }

    pub fn set_resume_detected(&self) {
        // A resume invalidates OpenFan's coalescing cache for the same reason it
        // clears hwmon's manual-mode flags: the device may have been reset
        // underneath us (DEC-256).
        self.invalidate_openfan_writes();
        self.resume_generation.fetch_add(1, Ordering::SeqCst);
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
        cache.record_engine_tick("emergency", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
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

        cache.record_engine_tick("normal", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("normal")
        );

        // Redundant write — value stays correct.
        cache.record_engine_tick("normal", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("normal")
        );

        // Genuine change must be applied, not skipped.
        cache.record_engine_tick("emergency", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("emergency")
        );
        cache.record_engine_tick("recovery", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
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

        cache.record_engine_tick("normal", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
        let first = cache
            .snapshot()
            .subsystem_timestamps
            .engine_started
            .expect("tick must stamp the heartbeat");

        cache.record_engine_tick("emergency", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
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
            pwm_readback_pct: None,
            updated_at: Instant::now(),
            alarm: None,
            pwm_enable_mode: None,
        }]);

        let snap = cache.snapshot();
        assert_eq!(snap.hwmon_fans.len(), 1);
        assert_eq!(snap.hwmon_fans["it8696:fan1"].rpm, Some(800));
    }

    /// OFS-k: a COMMAND must not make stale telemetry look fresh.
    ///
    /// The GPU twin of DEC-302's `set_openfan_commanded_pwm` fix, and the instance
    /// that survived it. `build_fan_entries` publishes `updated_at` as the
    /// reading's `age_ms`, so refreshing it on a write reported an age near zero
    /// beside an `rpm` frozen at whatever the last real poll saw.
    #[test]
    fn a_gpu_command_does_not_refresh_the_telemetry_timestamp() {
        let cache = StateCache::new();
        cache.update_gpu_fans(vec![AmdGpuFanState {
            id: "amd_gpu:0000:2d:00.0".into(),
            rpm: Some(1200),
            last_commanded_pct: None,
            duty_pct: Some(40),
            updated_at: Instant::now() - std::time::Duration::from_secs(30),
        }]);
        let before = cache.gpu_fans_snapshot()["amd_gpu:0000:2d:00.0"].updated_at;

        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:2d:00.0", 75);

        let snap = cache.gpu_fans_snapshot();
        let after = &snap["amd_gpu:0000:2d:00.0"];
        assert_eq!(
            after.updated_at, before,
            "a command must leave the READING's timestamp alone — the rpm beside \
             it is still 30s old, and age_ms is computed from this field"
        );
        assert_eq!(
            after.last_commanded_pct,
            Some(75),
            "precondition: the command itself must still have been recorded, or \
             this test would pass against a no-op"
        );
    }

    /// OFS-k, the other half: a GPU seen for the first time BY a write has no
    /// prior reading, so stamping `updated_at` on insert is correct. `rpm: None`
    /// is what says "nothing measured this".
    #[test]
    fn a_first_ever_gpu_command_still_stamps_the_new_entry() {
        let cache = StateCache::new();
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:2d:00.0", 60);
        let snap = cache.gpu_fans_snapshot();
        let fan = &snap["amd_gpu:0000:2d:00.0"];
        assert!(fan.rpm.is_none(), "nothing has measured this fan");
        assert!(
            Instant::now().saturating_duration_since(fan.updated_at)
                < std::time::Duration::from_secs(5),
            "a newly-created entry is not stale — there is no older reading to \
             misrepresent"
        );
    }

    /// OFS-m: an entry nothing refreshes is evicted, and one something DOES
    /// refresh survives.
    ///
    /// The second half is the load-bearing one. `HwmonPwmController::set_pwm`
    /// refreshes this map on every engine write, so an eviction rule that keyed on
    /// poll failures alone would drop a fan that is being actively commanded and
    /// re-insert it on the next write — flapping it in and out of `/fans` at 1 Hz
    /// under a running profile.
    #[test]
    fn a_frozen_hwmon_fan_is_evicted_and_a_refreshed_one_is_not() {
        let cache = StateCache::new();
        let stale = std::time::Duration::from_secs(60);
        cache.update_hwmon_fans(vec![
            HwmonFanState {
                id: "it8696:pwm1".into(),
                rpm: Some(800),
                last_commanded_pwm: Some(40),
                pwm_readback_pct: None,
                updated_at: Instant::now() - stale,
                alarm: None,
                pwm_enable_mode: None,
            },
            HwmonFanState {
                id: "it8696:pwm2".into(),
                rpm: Some(900),
                last_commanded_pwm: Some(50),
                pwm_readback_pct: None,
                updated_at: Instant::now(),
                alarm: None,
                pwm_enable_mode: None,
            },
        ]);

        cache.retain_fresh_hwmon_fans(std::time::Duration::from_secs(5));

        let fans = cache.snapshot().hwmon_fans;
        assert!(
            !fans.contains_key("it8696:pwm1"),
            "a header nothing has refreshed for 60s must not keep being published \
             with an age_ms that climbs forever"
        );
        assert!(
            fans.contains_key("it8696:pwm2"),
            "a header something IS refreshing must survive — this is the write-path \
             case a poll-failure streak would have flapped"
        );
    }

    /// OFS-m: the steady-state tick evicts nothing and must not take the write
    /// lock to discover that — the same fast-path contract as `retain_sensors`.
    /// DEC-316 regression. The poll is the only sampler of `alarm` and
    /// `pwm_enable_mode`; `set_pwm` rebuilds the whole `HwmonFanState` on every
    /// engine tick with both `None`, including on its coalesce fast path. With
    /// a wholesale `insert` that erased the poll's answer at ~1 Hz, so
    /// `fan_alarm` was absent for exactly the headers under active control —
    /// the ones whose failing fan matters most. It made the field decoration.
    ///
    /// Asserts the MERGE, not merely that a value survives one write: the
    /// second update also carries a genuinely newer `rpm`, so a fix that simply
    /// dropped the later entry would fail here too.
    #[test]
    fn an_engine_write_does_not_erase_the_polls_alarm_or_enable_mode() {
        let cache = StateCache::new();
        let now = Instant::now();
        cache.update_hwmon_fans(vec![HwmonFanState {
            id: "hwmon:it8696:isa-0a40:pwm5:PUMP".into(),
            rpm: Some(1200),
            last_commanded_pwm: Some(40),
            pwm_readback_pct: None,
            alarm: Some(true),
            pwm_enable_mode: Some(1),
            updated_at: now,
        }]);

        // What `HwmonPwmController::set_pwm` publishes on every tick.
        cache.update_hwmon_fans(vec![HwmonFanState {
            id: "hwmon:it8696:isa-0a40:pwm5:PUMP".into(),
            rpm: Some(1350),
            last_commanded_pwm: Some(45),
            pwm_readback_pct: None,
            alarm: None,
            pwm_enable_mode: None,
            updated_at: now,
        }]);

        let fans = cache.snapshot().hwmon_fans;
        let fan = &fans["hwmon:it8696:isa-0a40:pwm5:PUMP"];
        assert_eq!(
            fan.alarm,
            Some(true),
            "the poll's alarm must survive an engine write"
        );
        assert_eq!(fan.pwm_enable_mode, Some(1), "so must the live enable mode");
        // The engine's own fields still win — this is a merge, not a skip.
        assert_eq!(fan.rpm, Some(1350));
        assert_eq!(fan.last_commanded_pwm, Some(45));
    }

    /// AIO-MB Phase 5: the same merge, for the readback.
    ///
    /// Its own test rather than a third assertion on the one above, because the
    /// two fields fail differently: alarm and enable-mode are absent from the
    /// write path because it cannot cheaply re-read them, while
    /// `pwm_readback_pct` is absent because the write path has no readback to
    /// report — it knows what it COMMANDED. Losing the merge would make the
    /// readback permanently absent for exactly the headers under active control,
    /// which are the only ones a validation session records.
    #[test]
    fn an_engine_write_does_not_erase_the_polls_pwm_readback() {
        let cache = StateCache::new();
        let now = Instant::now();
        // The poll: readback and command happen to agree here, as they do
        // whenever writes are landing.
        cache.update_hwmon_fans(vec![HwmonFanState {
            id: "hwmon:it8696:isa-0a40:pwm5:PUMP".into(),
            rpm: Some(1200),
            last_commanded_pwm: Some(40),
            pwm_readback_pct: Some(40),
            alarm: None,
            pwm_enable_mode: None,
            updated_at: now,
        }]);

        // The engine commands 45%. It has no readback to publish, so it sends
        // `None` — which must NOT erase the poll's answer.
        cache.update_hwmon_fans(vec![HwmonFanState {
            id: "hwmon:it8696:isa-0a40:pwm5:PUMP".into(),
            rpm: Some(1350),
            last_commanded_pwm: Some(45),
            pwm_readback_pct: None,
            alarm: None,
            pwm_enable_mode: None,
            updated_at: now,
        }]);

        let fans = cache.snapshot().hwmon_fans;
        let fan = &fans["hwmon:it8696:isa-0a40:pwm5:PUMP"];
        assert_eq!(
            fan.pwm_readback_pct,
            Some(40),
            "the poll's readback must survive an engine write"
        );
        // The engine's own fields still win — a merge, not a skip.
        assert_eq!(fan.last_commanded_pwm, Some(45));
        assert_eq!(fan.rpm, Some(1350));
    }

    /// And the other direction: a later poll reporting a genuinely CHANGED
    /// readback must be able to move it. A merge that carried the old value
    /// forward unconditionally would freeze the readback at its first sample.
    #[test]
    fn a_later_poll_updates_the_pwm_readback() {
        let cache = StateCache::new();
        let now = Instant::now();
        for pct in [40u8, 55] {
            cache.update_hwmon_fans(vec![HwmonFanState {
                id: "h1".into(),
                rpm: Some(1000),
                last_commanded_pwm: Some(pct),
                pwm_readback_pct: Some(pct),
                alarm: None,
                pwm_enable_mode: None,
                updated_at: now,
            }]);
        }
        assert_eq!(cache.snapshot().hwmon_fans["h1"].pwm_readback_pct, Some(55));
    }

    /// The other direction: a later poll that genuinely reads a CLEARED alarm
    /// must be able to clear it. A merge that carried `Some(true)` forward
    /// unconditionally would latch a fan fault on for the process lifetime.
    #[test]
    fn a_later_poll_can_still_clear_an_alarm() {
        let cache = StateCache::new();
        let now = Instant::now();
        cache.update_hwmon_fans(vec![HwmonFanState {
            id: "h".into(),
            rpm: Some(0),
            last_commanded_pwm: Some(40),
            pwm_readback_pct: None,
            alarm: Some(true),
            pwm_enable_mode: Some(1),
            updated_at: now,
        }]);
        cache.update_hwmon_fans(vec![HwmonFanState {
            id: "h".into(),
            rpm: Some(900),
            last_commanded_pwm: Some(40),
            pwm_readback_pct: None,
            alarm: Some(false),
            pwm_enable_mode: Some(2),
            updated_at: now,
        }]);
        let fans = cache.snapshot().hwmon_fans;
        assert_eq!(fans["h"].alarm, Some(false));
        assert_eq!(fans["h"].pwm_enable_mode, Some(2));
    }

    #[test]
    fn retain_fresh_hwmon_fans_is_a_no_op_when_everything_is_fresh() {
        let cache = StateCache::new();
        cache.update_hwmon_fans(vec![HwmonFanState {
            id: "it8696:pwm1".into(),
            rpm: Some(800),
            last_commanded_pwm: Some(40),
            pwm_readback_pct: None,
            updated_at: Instant::now(),
            alarm: None,
            pwm_enable_mode: None,
        }]);
        cache.retain_fresh_hwmon_fans(std::time::Duration::from_secs(5));
        assert_eq!(cache.snapshot().hwmon_fans.len(), 1);
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
        let e1 = cache
            .try_begin_verify(Duration::from_secs(60))
            .expect("free slot must be claimable");
        assert!(cache.verify_active());
        // Single-flight: a second concurrent claim is rejected.
        assert!(
            cache.try_begin_verify(Duration::from_secs(60)).is_none(),
            "a second concurrent verify must be rejected (single-flight)"
        );
        // end_verify releases the slot; it can be claimed again.
        cache.end_verify(e1);
        assert!(!cache.verify_active());
        let e2 = cache
            .try_begin_verify(Duration::from_secs(60))
            .expect("released slot must be re-claimable");
        cache.end_verify(e2);
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
        // DEC-296: the half this test did NOT cover, which is why the defect
        // survived it. The assertion above is the ENGINE half — the pause reads
        // inactive. The SLOT half was never checked, and nothing cleared
        // `verify_in_progress`, so with the state left exactly as above every
        // later verify and calibration was rejected for the process lifetime.
        assert!(
            cache.try_begin_verify(Duration::from_secs(60)).is_some(),
            "an expired deadman must free the SLOT too, not only the pause"
        );
    }

    /// DEC-296: the deadman frees the slot, using an already-elapsed window so
    /// the test is deterministic rather than sleeping.
    #[test]
    fn an_expired_verify_deadman_frees_the_slot_for_the_next_claimant() {
        use std::time::Duration;
        let cache = StateCache::new();
        let _stranded = cache
            .try_begin_verify(Duration::ZERO)
            .expect("first claim must succeed");
        // A zero window is already elapsed, so the claim is no longer genuinely
        // held even though `verify_in_progress` is still set.
        assert!(!cache.verify_active());
        assert!(
            cache.try_begin_verify(Duration::from_secs(60)).is_some(),
            "the next verify must be able to claim the slot once the deadman expired"
        );
        assert!(cache.verify_active(), "the new claim must pause the engine");
    }

    /// DEC-296 remediation: the deadman measures LIVENESS, not total duration.
    ///
    /// Found by review: "an expired window is free" alone supersedes a claimant
    /// that is merely SLOW, not dead. The hwmon verify's post-settle `read_state`
    /// is plain `fs::read_to_string` on sysfs with no lock held, so it can outlast
    /// the 30 s window; the successor then force-takes the hwmon lease and the
    /// original's restore fails, parking the header at the test duty. Renewing at
    /// a checkpoint keeps a live claimant's slot; a wedged one never reaches the
    /// checkpoint and is still correctly superseded.
    #[test]
    fn renewing_keeps_a_slow_but_live_claim_and_a_stale_renew_is_refused() {
        use std::time::Duration;
        let cache = StateCache::new();
        let slow = cache
            .try_begin_verify(Duration::ZERO)
            .expect("first claim must succeed");
        // Elapsed by construction — without a renew, the slot is stealable.
        assert!(!cache.verify_active());
        assert!(
            cache.renew_verify(slow, Duration::from_secs(60)),
            "the owner must be able to prove it is alive and keep its slot"
        );
        assert!(cache.verify_active(), "renewing must re-arm the pause");
        assert!(
            cache.try_begin_verify(Duration::from_secs(60)).is_none(),
            "a renewed claim must NOT be stealable — this is the whole point"
        );

        // A superseded claimant's renew is refused, so it can report the
        // supersession instead of attempting a write whose lease is already gone.
        let cache2 = StateCache::new();
        let stranded = cache2
            .try_begin_verify(Duration::ZERO)
            .expect("first claim");
        let _live = cache2
            .try_begin_verify(Duration::from_secs(60))
            .expect("expired slot is claimable");
        assert!(
            !cache2.renew_verify(stranded, Duration::from_secs(60)),
            "a superseded claimant must not be able to re-arm the slot it lost"
        );
    }

    /// DEC-296, and the reason the claim returns a token rather than a bool.
    ///
    /// The fix recorded in the register — "treat an expired deadline as free" —
    /// is not sufficient on its own, and this test is what distinguishes the two.
    /// A stranded claimant can still be ALIVE: DEC-290 moved the guard inside a
    /// `spawn_blocking` task, so a wedged sysfs write holds it indefinitely. When
    /// it finally returns, its `Drop` must not release the successor's pause —
    /// otherwise the engine resumes writing over the successor's test duty and
    /// that verify's verdict is silently false. That is the DEC-278
    /// "fix to the fix" shape.
    #[test]
    fn a_stranded_verify_returning_late_cannot_release_its_successors_pause() {
        use std::time::Duration;
        let cache = StateCache::new();
        // A claims and its deadman expires immediately.
        let stranded = cache
            .try_begin_verify(Duration::ZERO)
            .expect("first claim must succeed");
        // B claims the freed slot and holds a real window.
        let live = cache
            .try_begin_verify(Duration::from_secs(60))
            .expect("expired slot must be claimable");
        assert_ne!(stranded, live, "each claim must get a distinct epoch");
        assert!(cache.verify_active());

        // A finally returns and drops. It no longer owns the slot.
        cache.end_verify(stranded);

        assert!(
            cache.verify_active(),
            "a stale release must NOT clear the live claimant's pause — the engine \
             would resume writing over its test duty and its verdict would be false"
        );
        // B's own release still works.
        cache.end_verify(live);
        assert!(!cache.verify_active());
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

    /// [SAFETY] DEC-272 (register row 01-c). `update_sensors` only inserts, so
    /// without this a vanished sensor's reading lived in the map for the life of
    /// the process, ageing into `Stale` and never reaching `Absent`.
    #[test]
    fn retain_sensors_evicts_only_what_is_no_longer_live() {
        let cache = StateCache::new();
        cache.update_sensors(vec![
            make_sensor("hwmon:k10temp:nodev:Tctl", 55.0),
            make_sensor("hwmon:nvme:nodev:Composite", 38.0),
        ]);

        let live: HashSet<String> = ["hwmon:k10temp:nodev:Tctl".to_string()]
            .into_iter()
            .collect();
        cache.retain_sensors(&live);

        let sensors = cache.sensors_snapshot();
        assert_eq!(sensors.len(), 1, "the vanished sensor must be evicted");
        assert!(
            sensors.contains_key("hwmon:k10temp:nodev:Tctl"),
            "the live sensor must survive"
        );
    }

    /// The steady-state tick evicts nothing; that path must not mutate the map,
    /// and in particular must not clear it when everything is live.
    #[test]
    fn retain_sensors_is_a_no_op_when_every_sensor_is_live() {
        let cache = StateCache::new();
        cache.update_sensors(vec![
            make_sensor("hwmon:k10temp:nodev:Tctl", 55.0),
            make_sensor("nvidia_gpu:0000:03:00.0:temp", 61.0),
        ]);

        let live: HashSet<String> = cache.sensors_snapshot().keys().cloned().collect();
        cache.retain_sensors(&live);

        assert_eq!(cache.sensors_snapshot().len(), 2);
    }

    /// An empty live set means "nothing is live", not "skip the sweep". A poll
    /// tick that discovered nothing must not leave a full map of readings that
    /// can no longer change.
    #[test]
    fn retain_sensors_with_an_empty_live_set_clears_the_map() {
        let cache = StateCache::new();
        cache.update_sensors(vec![make_sensor("hwmon:k10temp:nodev:Tctl", 55.0)]);
        cache.retain_sensors(&HashSet::new());
        assert!(cache.sensors_snapshot().is_empty());
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

    /// OFS-i. `build_fan_entries` publishes `OpenFanState.updated_at` as the
    /// fan's telemetry `age_ms`, and `set_openfan_commanded_pwm` used to refresh
    /// it — so a *command* made a *reading* look fresh. The rpm stayed frozen at
    /// whatever the last real poll saw, and `stall_detected` was then computed
    /// from that frozen value.
    ///
    /// **The discriminator is exact `Instant` equality, deliberately.** The first
    /// draft asserted a published `age_ms >= 4000` against a synthetic `now`, and
    /// the defect reproduced it at 3999 — a one-millisecond margin that is really
    /// just the gap between two `Instant::now()` calls, so on a slower machine it
    /// would have passed WITH the bug present. `before == after` cannot drift:
    /// either the command moved the timestamp or it did not.
    #[test]
    fn a_pwm_command_does_not_make_a_stale_rpm_reading_look_fresh() {
        let cache = StateCache::new();
        cache.update_openfan_fans(vec![make_openfan(3, 1200)]);
        let before = cache.snapshot().openfan_fans[&3].updated_at;

        // The link stops answering RPM polls, but writes still ack — a ~10-byte
        // SetPwm completes on a degraded link where an ~80-byte ReadAllRpm does
        // not, which is why this is the reachable shape rather than a contrived one.
        cache.set_openfan_commanded_pwm(3, 200);

        let snap = cache.snapshot();
        let after = snap.openfan_fans[&3].updated_at;
        assert_eq!(
            before, after,
            "a command must not move the timestamp of a reading it did not take"
        );

        // The command must still have landed — otherwise this test would also
        // pass against a `set_openfan_commanded_pwm` that did nothing at all.
        let fans = crate::api::handlers::build_fan_entries(&snap, after);
        let fan = fans
            .iter()
            .find(|f| f.id == "openfan:ch03")
            .expect("channel 3 is cached");
        assert_eq!(fan.last_commanded_pwm, Some(200));
        assert_eq!(
            fan.rpm,
            Some(1200),
            "the reading is unchanged — nothing new was measured"
        );

        // And the published age is sourced from that reading time, so once the
        // poll stops the fan ages honestly instead of being held at zero.
        let later = crate::api::handlers::build_fan_entries(
            &snap,
            before + std::time::Duration::from_millis(4000),
        );
        assert_eq!(
            later
                .iter()
                .find(|f| f.id == "openfan:ch03")
                .expect("channel 3 is cached")
                .age_ms,
            4000
        );
    }

    /// OFS-b's detection half. The stamp stays unconditional on purpose (it
    /// answers liveness, which a short frame does not falsify) — what the poll
    /// loop needs is the *count*, so an incomplete frame can be logged at all.
    #[test]
    fn update_openfan_fans_reports_how_many_known_channels_a_frame_missed() {
        let cache = StateCache::new();
        let all: Vec<OpenFanState> = (0..10u8).map(|ch| make_openfan(ch, 900)).collect();
        assert_eq!(
            cache.update_openfan_fans(all),
            0,
            "the first full frame covers everything it knows about"
        );

        let short: Vec<OpenFanState> = (0..3u8).map(|ch| make_openfan(ch, 900)).collect();
        assert_eq!(
            cache.update_openfan_fans(short),
            7,
            "three of ten leaves seven known channels unrefreshed"
        );

        // A frame introducing a channel the cache has never seen is not a miss.
        let cache2 = StateCache::new();
        assert_eq!(cache2.update_openfan_fans(vec![make_openfan(0, 900)]), 0);
    }

    /// F6. A channel that only ever got WRITTEN — `force_all_with_floor` walks
    /// `0..NUM_CHANNELS` unconditionally, so a thermal emergency mints one for every
    /// channel the firmware does not report — must not count as uncovered.
    ///
    /// It can never be covered by a later frame, so counting it would latch the
    /// short-frame warning on for the process lifetime and suppress the recovery
    /// line forever, on exactly the hardware this change is for.
    #[test]
    fn a_write_created_channel_is_not_counted_as_a_missed_reading() {
        let cache = StateCache::new();
        cache.update_openfan_fans(vec![make_openfan(0, 900), make_openfan(1, 900)]);
        // The thermal force writes a channel the controller never reports.
        cache.set_openfan_commanded_pwm(9, 255);
        assert!(!cache.snapshot().openfan_fans[&9].rpm_polled);

        assert_eq!(
            cache.update_openfan_fans(vec![make_openfan(0, 900), make_openfan(1, 900)]),
            0,
            "a full frame must report zero missed even with a write-only channel cached"
        );

        // Control: a genuinely polled channel that the frame drops still counts.
        assert_eq!(
            cache.update_openfan_fans(vec![make_openfan(0, 900)]),
            1,
            "channel 1 was measured before and is missing now"
        );
    }
}
