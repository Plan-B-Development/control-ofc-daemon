//! Fan RPM-to-PWM calibration sweep.
//!
//! Sweeps a fan from low to high PWM, recording RPM at each step.
//! Safety: aborts if any sensor exceeds the thermal limit, and restores
//! the pre-calibration PWM on every exit path — completion, thermal
//! abort, or a failed PWM write mid-sweep (DEC-134).

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::preflight::TemperatureFreshness;
use crate::constants;
use crate::health::cache::StateCache;

/// A single calibration data point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalPoint {
    pub pwm_percent: u8,
    pub rpm: u16,
}

/// Result of a calibration sweep.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationResult {
    pub fan_id: String,
    pub points: Vec<CalPoint>,
    pub start_pwm: Option<u8>,
    pub stop_pwm: Option<u8>,
    pub min_rpm: u16,
    pub max_rpm: u16,
}

/// Request parameters for calibration.
#[derive(Debug, Deserialize)]
pub struct CalibrationRequest {
    #[serde(default = "default_steps")]
    pub steps: u8,
    #[serde(default = "default_hold_seconds")]
    pub hold_seconds: u64,
}

fn default_steps() -> u8 {
    10
}
fn default_hold_seconds() -> u64 {
    5
}

/// Error during calibration.
#[derive(Debug, thiserror::Error)]
pub enum CalibrationError {
    #[error("thermal abort: sensor {sensor_id} at {temp_c:.1}°C exceeds {limit_c}°C")]
    ThermalAbort {
        sensor_id: String,
        temp_c: f64,
        limit_c: f64,
    },
    /// The thermal ladder is forcing a duty, so calibration must not write
    /// (DEC-295). Deliberately NOT `ThermalAbort`: that means "too hot to
    /// calibrate", and this fires on a machine that may be perfectly cool —
    /// the emergency latches at its trigger and releases only well below it, and
    /// `no_sensor_fallback` forces indefinitely on a machine with no CPU
    /// sensor at all. Carries the state so the message can name it.
    #[error("thermal safety is forcing fan output ({state}); calibration cannot run")]
    ThermalForceActive { state: String },
    /// The temperatures every other guard reads are too old to trust (DEC-385,
    /// `TS-q`). Deliberately NOT `ThermalAbort` either: the machine may be cool,
    /// and the honest statement is that the daemon cannot tell. With the poll
    /// wedged on a hot reading the ladder cannot fire — stale-and-hot reports
    /// `normal` — so a sweep that started, or kept going, would drive this
    /// channel from 0 % on numbers nothing is measuring.
    #[error("calibration cannot run: {reason}")]
    StaleTemperature { reason: String },
    #[error("validation: {0}")]
    Validation(String),
    #[error("hardware: {0}")]
    Hardware(String),
}

/// Check whether any sensor in the cache exceeds the thermal limit.
/// Returns `Ok(())` or `Err(CalibrationError::ThermalAbort)`.
pub fn check_thermal_safety(cache: &StateCache) -> Result<(), CalibrationError> {
    let snap = cache.snapshot();
    for sensor in snap.sensors.values() {
        if sensor.value_c > constants::CALIBRATION_MAX_TEMP_C {
            return Err(CalibrationError::ThermalAbort {
                sensor_id: sensor.id.clone(),
                temp_c: sensor.value_c,
                limit_c: constants::CALIBRATION_MAX_TEMP_C,
            });
        }
    }
    Ok(())
}

/// The thermal ladder's forcing state, or `None` when it is not forcing
/// (DEC-295).
///
/// Deliberately a SEPARATE predicate rather than a new arm inside
/// [`check_thermal_safety`]: that function is also the verify gate, and DEC-295
/// was scoped to calibration, so widening its meaning would have changed
/// `/hwmon/{id}/verify` and `/gpu/{id}/fan/verify` behaviour from a change that
/// had not reviewed them. **DEC-297 then used this predicate to close exactly
/// that gap** in `verify_thermal_guard` — which is why it was separated rather
/// than folded in: the two callers wanted the rule at different times.
///
/// All three non-normal states force a duty — `emergency` 100%, `recovery` 60%,
/// `no_sensor_fallback` 40% — so any of them means the engine is writing a
/// value this sweep must not fight. `None` is a cache that has never published
/// a state, which is normal.
pub fn thermal_force_state(cache: &StateCache) -> Option<String> {
    match cache.snapshot().thermal_override_state {
        None => None,
        Some(s) if s == "normal" => None,
        Some(s) => Some(s),
    }
}

/// [SAFETY] How fresh the temperature telemetry the thermal gates read actually
/// is (DEC-336, register row `P8-p`).
///
/// The CpuTemp-then-fallback selection lives HERE, once, and is the only
/// producer of a [`TemperatureFreshness`] from a cache in this daemon. It used
/// to be inlined in `handlers::discovery::gather_preflight`, which made the
/// preflight the sole reader of a rule the write path also needs; a second
/// inlined copy at the write path would have been two gating shapes for one
/// safety rule, which is the proximate cause `CLAUDE.md § Hard-won lessons`
/// records for the v2.63.1 defect.
///
/// A machine with no CPU sensor at all falls back to every temperature it has,
/// rather than reporting "no temperature sensors" while a dozen are readable.
/// Every age is taken from ONE `Instant`, so two readings cannot be compared
/// against two different "now"s.
///
/// # The budget is derived, not fixed
///
/// [`diagnostic_temp_max_age`] is the thermal ladder's own trust window, so a
/// diagnostic refuses on a reading exactly when the ladder has stopped acting on
/// it — never while the ladder still trusts it, and never after — see that
/// function.
pub fn cache_temperature_freshness(cache: &StateCache) -> TemperatureFreshness {
    // Snapshot FIRST, then stamp `now`. Reversed, any time spent waiting on the
    // read lock behind the 1 Hz `update_sensors` writer is silently subtracted
    // from every age, and it subtracts in the fail-OPEN direction: a reading
    // stamped after `now` saturates to age 0 and reads as fresh. Sub-millisecond
    // in practice, and this is a gate rather than a report, so it is taken in
    // the conservative order. (The reversed order was carried over verbatim from
    // the copy that used to be inlined in `gather_preflight`.)
    //
    // `read_with`, not `snapshot()`: a full snapshot clones all five maps of
    // `DaemonState` to read one, which `health::cache` documents against as
    // EFF-1. This runs once per POST, once per preflight and once per discovery
    // cycle, beside two existing full-snapshot callers.
    let readings: Vec<(String, std::time::Instant)> = cache.read_with(|state| {
        let cpu: Vec<_> = state
            .sensors
            .values()
            .filter(|s| matches!(s.kind, crate::hwmon::types::SensorKind::CpuTemp))
            .map(|s| (s.id.clone(), s.updated_at))
            .collect();
        if cpu.is_empty() {
            state
                .sensors
                .values()
                .map(|s| (s.id.clone(), s.updated_at))
                .collect()
        } else {
            cpu
        }
    });
    let now = std::time::Instant::now();
    crate::api::preflight::temperature_freshness(&readings, diagnostic_temp_max_age(cache), now)
}

/// [SAFETY] How old a temperature reading may be before a diagnostic treats it
/// as unusable (DEC-336, narrowed to equality by DEC-395).
///
/// **Exactly** [`StateCache::cpu_temp_stale_after`] — the thermal ladder's own
/// trust window, at every poll cadence.
///
/// # Why it is that window, in both directions
///
/// **Never wider (DEC-395, register row `TS-aj`).** DEC-336 floored this at a
/// flat 10 s so discovery "behaves exactly as before" at the default 1 s
/// cadence, whose ladder window is 5 s. That left a band — ages 5 s to 10 s —
/// in which the ladder had already stopped trusting the CPU reading, and so
/// could not force on it however hot it read, while every diagnostic still
/// passed its staleness gate and went on to drive a header. Readings refresh
/// every poll, so a reading that old means reads are failing, which is exactly
/// the case the gate exists for. The floor protected discovery's pre-DEC-336
/// behaviour, a reason that never applied to the verify, characterisation and
/// calibration gates DEC-385 added on the same budget.
///
/// **Never narrower (DEC-336).** `polling.poll_interval_ms` has no upper bound
/// in `DaemonConfig::validate`; the overlay clamps it to
/// `MAX_SUPERVISABLE_POLL_INTERVAL_MS` (6 s), a supported cadence whose ladder
/// window is 30 s. A fixed budget below that would refuse a diagnostic on a
/// reading the ladder is still acting on, on a perfectly healthy machine.
///
/// **So the invariant is a relationship rather than a number: a diagnostic
/// refuses on a reading exactly when the ladder has stopped acting on it.**
/// Both sides compare `age <= window` (`preflight::temperature_freshness` and
/// `profile_engine::hottest_cpu_reading`), so equality of the window is
/// equality of the verdict. `cpu_temp_stale_after` is the one place that window
/// is defined, and this consumes it rather than restating it.
pub fn diagnostic_temp_max_age(cache: &StateCache) -> std::time::Duration {
    cache.cpu_temp_stale_after()
}

/// [SAFETY] The refusal the preflight publishes, performed (DEC-336, `P8-p`).
///
/// `Some(message)` when `diagnostic` blocks on a stale temperature source AND
/// the cache holds no usable reading ([`temperature_refusal`]). `None` otherwise.
/// Since DEC-385 every diagnostic blocks; the predicate is still consulted so the
/// published verdict and this refusal cannot come apart.
///
/// # Why this exists
///
/// `GET /diagnostics/preflight` has published `verdict: "blocked"`,
/// `blocking: ["temperature_source"]` for `control_path_discovery` since
/// DEC-333, and **nothing performed it**: the POST gated on
/// [`check_thermal_safety`] and [`thermal_force_state`], both of which compare
/// `value_c` with no age term. A wedged poll loop therefore froze the cache at
/// its last-known-good temperatures, the preflight said `blocked`, and the POST
/// returned `202` and perturbed a header on a machine whose real temperature
/// was unknown — leaving the refusal to whichever client happened to honour the
/// verdict, which §6.1 forbids in exactly those words.
///
/// It is deliberately keyed on [`Diagnostic::blocks_on_stale_temperature`]
/// rather than on a `matches!` of its own: that predicate is what the published
/// report is derived from, so consuming it is what makes the wire verdict and
/// the daemon's behaviour incapable of disagreeing.
///
/// [`Diagnostic::blocks_on_stale_temperature`]: crate::api::preflight::Diagnostic::blocks_on_stale_temperature
pub fn stale_temperature_refusal(
    cache: &StateCache,
    diagnostic: crate::api::preflight::Diagnostic,
) -> Option<String> {
    if !diagnostic.blocks_on_stale_temperature() {
        return None;
    }
    temperature_refusal(cache)
}

/// [SAFETY] Why the thermal guards cannot be evaluated right now, or `None` when
/// at least one usable temperature reading exists ([`cache_temperature_freshness`]).
///
/// The unconditional half of [`stale_temperature_refusal`], for an operation that
/// publishes no preflight verdict to stay consistent with — the OpenFan
/// calibration sweep (DEC-385). A diagnostic that has a preflight entry must go
/// through [`stale_temperature_refusal`] instead.
pub fn temperature_refusal(cache: &StateCache) -> Option<String> {
    let freshness = cache_temperature_freshness(cache);
    if freshness.is_usable() {
        return None;
    }
    Some(match (freshness.total, freshness.newest_age_ms) {
        (0, _) => "no temperature readings are available, so the thermal guards \
                   cannot be evaluated"
            .to_string(),
        (_, Some(age)) => format!(
            "every temperature reading is stale — freshest is {age} ms old, limit \
             {} ms; the thermal guards cannot be evaluated",
            // The budget actually applied: it follows the poll cadence, and a
            // message naming a limit the daemon did not use sends the operator
            // looking for the wrong fault.
            diagnostic_temp_max_age(cache).as_millis()
        ),
        _ => "every temperature reading is stale, so the thermal guards cannot be \
              evaluated"
            .to_string(),
    })
}

/// Restore the pre-calibration duty, unless thermal safety is forcing (DEC-295).
///
/// [`RestoreOnDrop`] is now its ONLY caller — the normal-path call was deleted in
/// the same change, because scope exit covers completion and cancellation alike.
/// Kept as a named function rather than inlined into `Drop` so the thermal rule
/// is readable and testable on its own.
///
/// An UNKNOWN pre-calibration duty is restored to full speed (DEC-412) — the
/// exit floor's rule for a duty the daemon has lost track of (DEC-388), taken
/// from the same `pwm::exit_duty` so the two cannot drift. It is unknown when
/// the channel was never commanded, and since `TS-ad` also after a failed reply,
/// a reconnect or a resume. Skipping the restore, as this did before, left a
/// cancelled sweep's channel at its step — 0 % for the early ones — with only
/// the 100 % emergency left to write it again.
fn restore_pre_cal<F>(
    channel: u8,
    pre_cal_pwm: Option<u8>,
    write_fn: &F,
    cache: &StateCache,
    why: &str,
) where
    F: Fn(u8, u8) -> Result<(), CalibrationError>,
{
    let restore = crate::pwm::exit_duty(pre_cal_pwm, 0);
    if pre_cal_pwm.is_none() {
        log::info!(
            "ch{channel}: the pre-calibration duty is unknown (never commanded, or lost \
             to a failed reply, a reconnect or a resume) — restoring {restore}% ({why})"
        );
    }
    if let Some(state) = thermal_force_state(cache) {
        log::warn!(
            "ch{channel} left at the thermal-safety forced duty instead of restoring \
             {restore}% ({why}) — thermal safety is active ({state}) and outranks \
             calibration. It will not be restored automatically once the force clears."
        );
        return;
    }
    if let Err(e) = write_fn(channel, restore) {
        log::warn!("failed to restore pre-calibration PWM on ch{channel} ({why}): {e}");
    }
}

/// Restores the pre-calibration duty on drop (DEC-297, 295-e).
///
/// The sweep holds each step with `tokio::time::sleep(...).await`, which is a
/// cancellation point: a client disconnect drops the handler future mid-hold and
/// the channel is left at that step — **0 % for the early steps**, i.e. stranded
/// SLOW, unlike the GPU verify which biases its test speed upward on purpose.
/// `CalibrationGuard` only clears the single-flight flag; it does not restore.
///
/// Deliberately NOT the DEC-290 `spawn_blocking` shape used for the verifies. A
/// verify is ~6 s and makes a fine uncancellable unit; a sweep is
/// `steps x hold_seconds` — up to 300 s — and making that uncancellable would pin
/// a blocking thread and hold both single-flight flags for five minutes after the
/// client has gone. A drop guard restores the hardware without extending the
/// work's lifetime, which is the property that actually matters here.
struct RestoreOnDrop<'a, F: Fn(u8, u8) -> Result<(), CalibrationError>> {
    channel: u8,
    pre_cal_pwm: Option<u8>,
    write_fn: &'a F,
    cache: &'a StateCache,
    /// Set just before the sweep's first write is issued (DEC-412). A sweep
    /// refused before any step — a hot, stale or forcing thermal state — never
    /// touched the channel, so there is nothing to restore, and a refusal must
    /// write nothing. Set BEFORE the write rather than after it succeeds: a
    /// write whose reply fails may still have landed (`TS-o`).
    touched: &'a std::sync::atomic::AtomicBool,
}

impl<F: Fn(u8, u8) -> Result<(), CalibrationError>> Drop for RestoreOnDrop<'_, F> {
    fn drop(&mut self) {
        // Before DEC-412 this rule was implicit: a refused sweep on a channel
        // with a known duty restored that duty, which the controller coalesced
        // into no frame, and an unknown duty skipped the restore. Once an
        // unknown duty restores full speed, a refusal would write 100 % — so the
        // guard says outright what it restores: only what the sweep changed.
        if !self.touched.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        restore_pre_cal(
            self.channel,
            self.pre_cal_pwm,
            self.write_fn,
            self.cache,
            "sweep ended",
        );
    }
}

/// Run an OpenFan calibration sweep on a single channel.
///
/// The single sweep implementation — the `/fans/openfan/{ch}/calibrate`
/// handler delegates here (DEC-134; it previously kept a diverged inline
/// copy). This is a long-running async function (steps × hold_seconds). It:
/// 1. Reads the current PWM (for restore)
/// 2. Sweeps from 0% to 100% in `steps` increments
/// 3. Holds each step for `hold_seconds`, then reads RPM from cache
/// 4. Restores the pre-calibration PWM on every exit path — success, thermal
///    abort, or a failed PWM write mid-sweep (DEC-134; previously an early `?`
///    could park the fan at a sweep step) — **except while thermal safety is
///    forcing a duty**, where the channel is deliberately left at the forced
///    value rather than lowered back under it (DEC-295)
/// 5. Derives start_pwm (lowest PWM with RPM > 0) and stop_pwm
///
/// # Safety
/// - Checks thermal limit before each step
/// - Caller must hold appropriate locks (one calibration at a time)
pub async fn calibrate_openfan_channel(
    cache: Arc<StateCache>,
    channel: u8,
    steps: u8,
    hold_seconds: u64,
    write_fn: impl Fn(u8, u8) -> Result<(), CalibrationError>,
) -> Result<CalibrationResult, CalibrationError> {
    let clamped_steps = steps.clamp(2, 20);
    if clamped_steps != steps {
        log::info!(
            "Calibration: steps clamped from {steps} to {clamped_steps} (valid range: 2–20)"
        );
    }
    let clamped_hold = hold_seconds.clamp(2, 15);
    if clamped_hold != hold_seconds {
        log::info!(
            "Calibration: hold_seconds clamped from {hold_seconds} to {clamped_hold} (valid range: 2–15)"
        );
    }
    let steps = clamped_steps;
    let hold = Duration::from_secs(clamped_hold);

    // Read pre-calibration PWM from cache
    let snap = cache.snapshot();
    let pre_cal_pwm = snap
        .openfan_fans
        .get(&channel)
        .and_then(|f| f.last_commanded_pwm);

    let fan_id = crate::serial::openfan_member_id(channel);
    let step_size = 100.0 / steps as f64;
    let touched = std::sync::atomic::AtomicBool::new(false);

    // Sweep from 0% to 100%. Runs as an inner block so every exit —
    // success, thermal abort, write failure — flows through the restore
    // below instead of leaving the fan parked at a sweep step (DEC-134).
    let sweep = async {
        let mut points = Vec::with_capacity(steps as usize + 1);
        for i in 0..=steps {
            let pwm = (i as f64 * step_size).round().min(100.0) as u8;

            // Thermal check before each step
            check_thermal_safety(&cache)?;

            // DEC-295: the check above is a pure temperature test at
            // CALIBRATION_MAX_TEMP_C (85C), but the thermal emergency LATCHES at
            // its trigger and releases only at <=80C. The band 80 < T <= 85 therefore
            // passes it while the engine is still forcing 100% every tick — and
            // this sweep starts at 0%, so without this guard it would fight the
            // emergency at 1 Hz for the whole sweep. Abort rather than skip: a
            // sweep with holes in it produces a wrong curve, not a partial one.
            if let Some(state) = thermal_force_state(&cache) {
                return Err(CalibrationError::ThermalForceActive { state });
            }

            // [SAFETY] DEC-385 (`TS-q`): the two checks above read `value_c` with
            // no age term, so a wedged poll presents its last hot reading forever
            // and both pass while the ladder — which treats that reading as stale
            // and so cannot force — is blind. Before every step, not just the
            // first: a sweep runs up to steps x hold_seconds, and a poll that
            // wedges part way through must stop it.
            if let Some(reason) = temperature_refusal(&cache) {
                return Err(CalibrationError::StaleTemperature { reason });
            }

            // Set PWM. Marked first: from here the channel may hold a sweep duty
            // whether or not the write reports success, so the restore is owed.
            touched.store(true, std::sync::atomic::Ordering::SeqCst);
            write_fn(channel, pwm)?;

            // Wait for fan to settle
            tokio::time::sleep(hold).await;

            // Read RPM from cache
            let snap = cache.snapshot();
            let rpm = snap.openfan_fans.get(&channel).map(|f| f.rpm).unwrap_or(0);

            points.push(CalPoint {
                pwm_percent: pwm,
                rpm,
            });
        }
        Ok::<Vec<CalPoint>, CalibrationError>(points)
    };
    // DEC-297 (295-e): the restore now runs on EVERY exit including cancellation.
    // Constructed before the sweep is awaited, so a dropped handler future still
    // restores; DEC-295's rule that the restore is skipped under an active
    // thermal force lives in `restore_pre_cal`, shared by both paths.
    //
    // The residual DEC-295 recorded still stands and is unchanged: nothing
    // retries a restore that was skipped because the ladder was forcing.
    let _restore = RestoreOnDrop {
        channel,
        pre_cal_pwm,
        write_fn: &write_fn,
        cache: &cache,
        touched: &touched,
    };

    let sweep_result = sweep.await;
    let points = sweep_result?;

    // Derive start_pwm and stop_pwm
    let start_pwm = points.iter().find(|p| p.rpm > 0).map(|p| p.pwm_percent);

    let stop_pwm = points
        .iter()
        .rev()
        .find(|p| p.rpm == 0)
        .map(|p| p.pwm_percent);

    let min_rpm = points
        .iter()
        .map(|p| p.rpm)
        .filter(|&r| r > 0)
        .min()
        .unwrap_or(0);
    let max_rpm = points.iter().map(|p| p.rpm).max().unwrap_or(0);

    Ok(CalibrationResult {
        fan_id,
        points,
        start_pwm,
        stop_pwm,
        min_rpm,
        max_rpm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::{CachedSensorReading, DeviceLabel, OpenFanState};
    use crate::hwmon::types::SensorKind;
    use std::time::Instant;

    fn make_cache(sensor_temp: f64, channel: u8, rpm: u16) -> Arc<StateCache> {
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![CachedSensorReading {
            id: "cpu".into(),
            kind: SensorKind::CpuTemp,
            label: "Tctl".into(),
            value_c: sensor_temp,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }]);
        cache.update_openfan_fans(vec![OpenFanState {
            channel,
            rpm,
            last_commanded_pwm: Some(50),
            updated_at: Instant::now(),
            rpm_polled: true,
        }]);
        cache
    }

    #[test]
    fn thermal_check_passes_below_limit() {
        let cache = make_cache(60.0, 0, 1000);
        assert!(check_thermal_safety(&cache).is_ok());
    }

    #[test]
    fn thermal_check_fails_above_limit() {
        let cache = make_cache(90.0, 0, 1000);
        let err = check_thermal_safety(&cache).unwrap_err();
        assert!(matches!(err, CalibrationError::ThermalAbort { .. }));
    }

    /// Log of (channel, pwm) writes issued through the recording closure.
    type WriteLog = Arc<std::sync::Mutex<Vec<(u8, u8)>>>;

    /// Recording write closure: collects every (channel, pwm) write, with an
    /// optional PWM value that fails the write when commanded.
    fn recording_write_fn(
        fail_at_pwm: Option<u8>,
    ) -> (impl Fn(u8, u8) -> Result<(), CalibrationError>, WriteLog) {
        let writes: WriteLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writes2 = writes.clone();
        let f = move |ch: u8, pwm: u8| -> Result<(), CalibrationError> {
            if fail_at_pwm == Some(pwm) {
                return Err(CalibrationError::Hardware(format!(
                    "mock write failure at {pwm}%"
                )));
            }
            writes2.lock().unwrap().push((ch, pwm));
            Ok(())
        };
        (f, writes)
    }

    #[tokio::test(start_paused = true)]
    async fn calibration_sweep_basic() {
        let cache = make_cache(50.0, 0, 800);
        let result = calibrate_openfan_channel(
            cache,
            0,
            3, // 3 steps: 0%, 33%, 67%, 100%
            0, // 0s hold (clamped to 2s; paused-time test — sleeps are instant)
            |_ch, _pwm| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(result.fan_id, "openfan:ch00");
        assert_eq!(result.points.len(), 4); // 0..=3
        assert_eq!(result.max_rpm, 800);
    }

    #[tokio::test(start_paused = true)]
    async fn calibration_aborts_on_thermal() {
        let cache = make_cache(90.0, 0, 800); // over limit
        let result = calibrate_openfan_channel(cache, 0, 3, 0, |_ch, _pwm| Ok(())).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CalibrationError::ThermalAbort { .. }
        ));
    }

    /// DEC-134: a successful sweep ends with the pre-calibration PWM restored
    /// (the cache's `last_commanded_pwm` is 50 in `make_cache`).
    #[tokio::test(start_paused = true)]
    async fn calibration_restores_pre_cal_pwm_on_success() {
        let cache = make_cache(50.0, 0, 800);
        let (write_fn, writes) = recording_write_fn(None);

        calibrate_openfan_channel(cache, 0, 3, 0, write_fn)
            .await
            .unwrap();

        let w = writes.lock().unwrap();
        assert_eq!(
            w.last(),
            Some(&(0u8, 50u8)),
            "last write must restore the pre-calibration PWM; writes: {w:?}"
        );
    }

    /// DEC-134: a thermal abort must still restore the pre-calibration PWM.
    ///
    /// The machine heats past the limit after the sweep's first step, so the
    /// abort lands on a channel the sweep HAS touched. Until DEC-412 this test
    /// aborted before any write and asserted a lone restore; since DEC-412 a
    /// sweep that touched nothing restores nothing
    /// (`a_sweep_refused_before_its_first_step_writes_nothing`), so that shape
    /// would no longer exercise the restore at all.
    #[tokio::test(start_paused = true)]
    async fn calibration_restores_pre_cal_pwm_on_thermal_abort() {
        let cache = make_cache(50.0, 0, 800);
        let writes: WriteLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let write_fn = {
            let (cache, writes) = (cache.clone(), writes.clone());
            move |ch: u8, pwm: u8| -> Result<(), CalibrationError> {
                writes.lock().unwrap().push((ch, pwm));
                // Too hot from the next step's check on.
                cache.update_sensors(vec![aged_cpu(90.0, Duration::ZERO)]);
                Ok(())
            }
        };

        let result = calibrate_openfan_channel(cache, 0, 3, 0, write_fn).await;

        assert!(matches!(
            result.unwrap_err(),
            CalibrationError::ThermalAbort { .. }
        ));
        let w = writes.lock().unwrap();
        assert_eq!(
            w.as_slice(),
            &[(0u8, 0u8), (0u8, 50u8)],
            "the first step, then the abort's restore — and no second step"
        );
    }

    /// DEC-295: while thermal safety is forcing a duty, calibration must not
    /// write at all — not its sweep steps, and not its restore.
    ///
    /// `check_thermal_safety` alone does NOT cover this. It is a pure
    /// temperature test at 85C, while the emergency latches at 105C or higher
    /// (per-machine since DEC-308) and releases
    /// only at <=80C — so the whole band 80 < T <= 85 passes it with the engine
    /// still forcing 100% every tick. This fixture sits at 50C precisely to
    /// prove the new guard fires on the FORCED STATE and not on temperature;
    /// if it keyed on temperature this test could not fail.
    #[tokio::test(start_paused = true)]
    async fn calibration_refuses_to_run_while_thermal_safety_is_forcing() {
        let cache = make_cache(50.0, 0, 800); // comfortably under the 85C limit
        cache.record_engine_tick("emergency", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
        let (write_fn, writes) = recording_write_fn(None);

        let result = calibrate_openfan_channel(cache, 0, 3, 0, write_fn).await;

        assert!(
            matches!(
                result.unwrap_err(),
                CalibrationError::ThermalForceActive { ref state } if state == "emergency"
            ),
            "a forced thermal state must abort the sweep, naming the state"
        );
        let w = writes.lock().unwrap();
        assert!(
            w.is_empty(),
            "no write may reach the channel while thermal safety is forcing — \
             not a sweep step, and not the restore; got {w:?}"
        );
    }

    /// DEC-295, finding 1 of the review: pins that the step guard is INSIDE the
    /// sweep loop, which is the whole point of it.
    ///
    /// The two tests either side of this one both stay green if the guard is
    /// hoisted out of the loop — one sets the state before the call, the other
    /// after the last step. Neither exercises the case the in-loop placement
    /// exists for: an emergency latching PART WAY through a sweep, which is the
    /// realistic one, since a sweep runs for steps x hold_seconds (up to 300 s).
    #[tokio::test(start_paused = true)]
    async fn calibration_aborts_mid_sweep_when_thermal_safety_latches() {
        let cache = make_cache(50.0, 0, 800);
        let log: WriteLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (log2, cache2) = (log.clone(), cache.clone());
        // 3 steps -> 0, 33, 67, 100. Latch on the 33% write.
        let write_fn = move |ch: u8, pwm: u8| -> Result<(), CalibrationError> {
            log2.lock().unwrap().push((ch, pwm));
            if pwm == 33 {
                cache2
                    .record_engine_tick("emergency", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
            }
            Ok(())
        };

        let result = calibrate_openfan_channel(cache, 0, 3, 0, write_fn).await;

        assert!(
            matches!(
                result.unwrap_err(),
                CalibrationError::ThermalForceActive { .. }
            ),
            "a mid-sweep latch must abort the remaining steps"
        );
        let w = log.lock().unwrap();
        assert_eq!(
            w.as_slice(),
            &[(0u8, 0u8), (0u8, 33u8)],
            "no step beyond the latch may be written, and no restore; got {w:?}"
        );
    }

    /// A CPU reading aged `age`. Aged BY CONSTRUCTION: paused time does not
    /// advance `std::time::Instant`, so a test that slept would age it by ~0 ms.
    fn aged_cpu(temp_c: f64, age: Duration) -> CachedSensorReading {
        CachedSensorReading {
            id: "cpu".into(),
            kind: SensorKind::CpuTemp,
            label: "Tctl".into(),
            value_c: temp_c,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now() - age,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }
    }

    /// Older than the budget this cache applies — derived, so a slow configured
    /// poll cannot quietly make the fixture fresh.
    fn stale_age(cache: &StateCache) -> Duration {
        diagnostic_temp_max_age(cache) + Duration::from_secs(5)
    }

    /// [SAFETY] TS-aj / DEC-395 — the gate refuses a reading exactly when the
    /// ladder has stopped acting on it, judged by the LADDER'S OWN classifier.
    ///
    /// The right-hand side is `profile_engine::hottest_cpu_reading` — the
    /// function the safety tick actually calls — never a re-derivation of the
    /// window, so the test cannot share the budget's arithmetic. The ages walk
    /// the band DEC-336's 10 s floor opened at a 1 s poll (5–10 s: stale to the
    /// ladder, fresh to the gate) and its 2 s-poll neighbour, on both sides of
    /// each boundary. Precondition: both arms were observed, and the band itself
    /// was — a sample set that never entered it would pass with the floor back.
    #[test]
    fn the_gate_refuses_exactly_where_the_ladder_stops_trusting_the_reading() {
        let mut saw_refused = false;
        let mut saw_allowed = false;
        let mut saw_old_band = false;
        for (interval_ms, ages_ms) in [
            (1000u64, [4_000u64, 4_900, 5_100, 7_000, 9_900]),
            (2000, [9_000, 9_900, 10_100, 12_000, 15_000]),
        ] {
            for age_ms in ages_ms {
                let cache = StateCache::new();
                cache.set_hwmon_poll_interval_ms(interval_ms);
                cache.update_sensors(vec![aged_cpu(84.0, Duration::from_millis(age_ms))]);

                let ladder = crate::profile_engine::hottest_cpu_reading(
                    &cache.sensors_snapshot(),
                    Instant::now(),
                    cache.cpu_temp_stale_after(),
                );
                let ladder_stale = !matches!(ladder, crate::profile_engine::CpuReading::Fresh(_));
                let refused = temperature_refusal(&cache).is_some();
                assert_eq!(
                    refused, ladder_stale,
                    "poll {interval_ms} ms, reading {age_ms} ms old: the gate \
                     refused = {refused} while the ladder reads it as {ladder:?}"
                );
                saw_refused |= refused;
                saw_allowed |= !refused;
                saw_old_band |= ladder_stale && age_ms <= 10_000;
            }
        }
        assert!(saw_refused && saw_allowed, "both arms must be exercised");
        assert!(
            saw_old_band,
            "no sample fell in the band the 10 s floor left open"
        );
    }

    /// [SAFETY] TS-q / DEC-385 — the audit's scenario. The poll has wedged on a
    /// reading just under the 85 °C calibration limit: the temperature check
    /// passes (84 < 85) and the ladder is not forcing, because stale-and-hot
    /// reports `normal`. Before DEC-385 nothing else looked, so the sweep drove
    /// this channel from 0 % on a number nothing was measuring.
    ///
    /// Both arms: the same 84 °C, fresh, calibrates — so the refusal is the age.
    #[tokio::test(start_paused = true)]
    async fn calibration_refuses_a_stale_reading_the_ladder_cannot_act_on() {
        let cache = make_cache(84.0, 0, 800);
        let stale = stale_age(&cache);
        cache.update_sensors(vec![aged_cpu(84.0, stale)]);
        assert!(
            check_thermal_safety(&cache).is_ok(),
            "precondition: 84 °C passes"
        );
        assert!(
            thermal_force_state(&cache).is_none(),
            "precondition: nothing forcing"
        );
        let (write_fn, writes) = recording_write_fn(None);

        let result = calibrate_openfan_channel(cache, 0, 3, 0, write_fn).await;

        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                CalibrationError::StaleTemperature { .. }
            ),
            "{result:?}"
        );
        assert_eq!(
            writes.lock().unwrap().as_slice(),
            &[] as &[(u8, u8)],
            "no sweep step may be written — and since DEC-412 no restore either, \
             because a sweep refused before its first step never touched the channel"
        );

        let cache = make_cache(84.0, 0, 800);
        let (write_fn, writes) = recording_write_fn(None);
        calibrate_openfan_channel(cache, 0, 3, 0, write_fn)
            .await
            .expect("a FRESH 84 °C reading must calibrate");
        assert!(writes.lock().unwrap().contains(&(0u8, 0u8)));
    }

    /// DEC-385: the staleness check runs before EVERY step, like the two thermal
    /// checks beside it. A sweep runs for up to steps x hold_seconds, and a poll
    /// that wedges part way through must stop it at the next step — the case a
    /// check hoisted out of the loop would miss.
    #[tokio::test(start_paused = true)]
    async fn calibration_aborts_mid_sweep_when_the_readings_go_stale() {
        let cache = make_cache(50.0, 0, 800);
        let stale = stale_age(&cache);
        let log: WriteLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (log2, cache2) = (log.clone(), cache.clone());
        // 3 steps -> 0, 33, 67, 100. The poll wedges during the 33 % hold.
        let write_fn = move |ch: u8, pwm: u8| -> Result<(), CalibrationError> {
            log2.lock().unwrap().push((ch, pwm));
            if pwm == 33 {
                cache2.update_sensors(vec![aged_cpu(50.0, stale)]);
            }
            Ok(())
        };

        let result = calibrate_openfan_channel(cache, 0, 3, 0, write_fn).await;

        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                CalibrationError::StaleTemperature { .. }
            ),
            "{result:?}"
        );
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[(0u8, 0u8), (0u8, 33u8), (0u8, 50u8)],
            "no step past the wedge; then the restore"
        );
    }

    /// DEC-297 (295-e). The sweep holds each step with an `.await`, which is a
    /// cancellation point: a client disconnect dropped the handler future
    /// mid-hold and the restore never ran, leaving the channel at that step —
    /// **0% for the early steps**, i.e. stranded SLOW. `CalibrationGuard` only
    /// clears the single-flight flag; it does not restore.
    ///
    /// Fixed with a drop guard rather than DEC-290's `spawn_blocking` shape: a
    /// sweep runs up to 300 s, and making that uncancellable would pin a blocking
    /// thread and hold both single-flight flags long after the client has gone.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_calibration_still_restores_the_channel() {
        let cache = make_cache(50.0, 0, 800);
        let (write_fn, writes) = recording_write_fn(None);
        {
            let fut = calibrate_openfan_channel(cache.clone(), 0, 3, 0, write_fn);
            tokio::pin!(fut);
            // Abandon it mid-hold, exactly as axum does on a client disconnect.
            tokio::select! {
                _ = &mut fut => panic!("the sweep completed too fast to model a cancellation"),
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        } // <- future dropped here; the drop guard must still restore

        let w = writes.lock().unwrap();
        assert!(
            w.contains(&(0u8, 0u8)),
            "fixture check: the sweep must have written its first step before we \
             cancelled, or this test proves nothing; got {w:?}"
        );
        assert_eq!(
            w.last(),
            Some(&(0u8, 50u8)),
            "a cancelled sweep must still restore the pre-calibration duty; got {w:?}"
        );
    }

    /// DEC-412: the case that stranded a fan. A duty the daemon stopped knowing
    /// (`TS-ad` — here withdrawn exactly as a failed reply withdraws it) left a
    /// cancelled sweep's channel at its first step, 0 %, because the restore was
    /// skipped when there was nothing to restore. It now goes to full speed.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_calibration_with_an_unknown_duty_restores_full_speed() {
        let cache = make_cache(50.0, 0, 800);
        cache.clear_openfan_commanded_pwm(0);
        assert_eq!(
            cache.snapshot().openfan_fans[&0].last_commanded_pwm,
            None,
            "precondition: the duty is unknown"
        );
        let (write_fn, writes) = recording_write_fn(None);
        {
            let fut = calibrate_openfan_channel(cache.clone(), 0, 3, 0, write_fn);
            tokio::pin!(fut);
            tokio::select! {
                _ = &mut fut => panic!("the sweep completed too fast to model a cancellation"),
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }

        let w = writes.lock().unwrap();
        assert!(
            w.contains(&(0u8, 0u8)),
            "fixture check: the sweep must have written 0 % before the cancel; got {w:?}"
        );
        assert_eq!(
            w.last(),
            Some(&(0u8, 100u8)),
            "an unknown duty is restored to full speed, never left at 0 %; got {w:?}"
        );
    }

    /// DEC-412: a sweep refused before its first step never touched the channel,
    /// so nothing is restored — above all not the full speed an UNKNOWN duty now
    /// restores to, which would make a refusal write 100 %. Both refusal shapes
    /// the sweep has before a write (too hot; a stale temperature source) and
    /// both duty states, because the known-duty arm used to pass only by the
    /// controller's coalescing, which this recording closure does not do.
    #[tokio::test(start_paused = true)]
    async fn a_sweep_refused_before_its_first_step_writes_nothing() {
        for (temp, stale) in [(90.0, false), (50.0, true)] {
            for unknown_duty in [true, false] {
                let cache = make_cache(temp, 0, 800);
                if unknown_duty {
                    cache.clear_openfan_commanded_pwm(0);
                }
                if stale {
                    let old = std::time::Instant::now()
                        - diagnostic_temp_max_age(&cache)
                        - Duration::from_secs(60);
                    let mut reading = cache.snapshot().sensors["cpu"].clone();
                    reading.updated_at = old;
                    cache.update_sensors(vec![reading]);
                }
                let (write_fn, writes) = recording_write_fn(None);

                let result = calibrate_openfan_channel(cache, 0, 3, 0, write_fn).await;

                assert!(
                    result.is_err(),
                    "precondition: temp {temp}, stale {stale} must refuse the sweep"
                );
                assert!(
                    writes.lock().unwrap().is_empty(),
                    "a refused sweep wrote (temp {temp}, stale {stale}, unknown duty \
                     {unknown_duty}): {:?}",
                    writes.lock().unwrap()
                );
            }
        }
    }

    /// DEC-295: the restore guard in isolation.
    ///
    /// The test above aborts at the first step, so it never reaches the restore.
    /// Here the sweep runs to completion and the emergency latches during the
    /// final step — the real sequence, since the engine ticks concurrently — so
    /// the restore is the only guarded site left. Asserts the PRESENCE first:
    /// the identical sweep with no forced state DOES restore, so the absence
    /// below is a real one (`CLAUDE.md § Hard-won lessons`).
    #[tokio::test(start_paused = true)]
    async fn calibration_leaves_the_channel_forced_rather_than_restoring_under_it() {
        // Presence: no forced state -> the restore happens.
        let cache = make_cache(50.0, 0, 800);
        let (write_fn, writes) = recording_write_fn(None);
        calibrate_openfan_channel(cache, 0, 3, 0, write_fn)
            .await
            .unwrap();
        assert_eq!(
            writes.lock().unwrap().last(),
            Some(&(0u8, 50u8)),
            "control case: an unforced sweep must end by restoring 50%"
        );

        // Absence: the emergency latches during the last step write.
        let cache = make_cache(50.0, 0, 800);
        let log: WriteLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (log2, cache2) = (log.clone(), cache.clone());
        let write_fn = move |ch: u8, pwm: u8| -> Result<(), CalibrationError> {
            log2.lock().unwrap().push((ch, pwm));
            if pwm == 100 {
                cache2
                    .record_engine_tick("emergency", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
            }
            Ok(())
        };

        calibrate_openfan_channel(cache, 0, 3, 0, write_fn)
            .await
            .unwrap();

        let w = log.lock().unwrap();
        assert_eq!(
            w.last(),
            Some(&(0u8, 100u8)),
            "the sweep must complete; the last write is its final step, not a restore"
        );
        assert!(
            !w.iter()
                .skip_while(|(_, p)| *p != 100)
                .any(|(_, p)| *p == 50),
            "the pre-cal 50% must NOT be written back under an active force; got {w:?}"
        );
    }

    /// DEC-134 regression: a failed PWM write mid-sweep previously returned
    /// early WITHOUT restoring — parking the fan at the last sweep step.
    /// The restore must run even when the sweep errors out.
    #[tokio::test(start_paused = true)]
    async fn calibration_restores_pre_cal_pwm_on_write_failure() {
        let cache = make_cache(50.0, 0, 800);
        // 3 steps → 0%, 33%, 67%, 100%; fail the 67% write.
        let (write_fn, writes) = recording_write_fn(Some(67));

        let result = calibrate_openfan_channel(cache, 0, 3, 0, write_fn).await;

        assert!(matches!(result.unwrap_err(), CalibrationError::Hardware(_)));
        let w = writes.lock().unwrap();
        assert_eq!(
            w.last(),
            Some(&(0u8, 50u8)),
            "write failure mid-sweep must still restore; writes: {w:?}"
        );
    }

    /// DEC-412: no pre-calibration PWM in the cache → the duty is unknown, and
    /// it is restored to FULL speed, the exit floor's rule for a duty the daemon
    /// lost track of. Until DEC-412 this skipped the restore (the sweep's own
    /// writes were the only ones issued). A completed sweep already ends at
    /// 100 %, so the extra write is what discriminates here; the cancelled case
    /// below is the one that mattered.
    #[tokio::test(start_paused = true)]
    async fn calibration_restores_full_speed_without_pre_cal_pwm() {
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![CachedSensorReading {
            id: "cpu".into(),
            kind: SensorKind::CpuTemp,
            label: "Tctl".into(),
            value_c: 50.0,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }]);
        cache.update_openfan_fans(vec![OpenFanState {
            channel: 0,
            rpm: 800,
            last_commanded_pwm: None, // never commanded
            updated_at: Instant::now(),
            rpm_polled: true,
        }]);
        let (write_fn, writes) = recording_write_fn(None);

        calibrate_openfan_channel(cache, 0, 3, 0, write_fn)
            .await
            .unwrap();

        let w = writes.lock().unwrap();
        assert_eq!(
            w.len(),
            5,
            "four sweep writes, then the restore; writes: {w:?}"
        );
        assert_eq!(w.last(), Some(&(0u8, 100u8)));
    }
}
