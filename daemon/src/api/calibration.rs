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
/// [`diagnostic_temp_max_age`] widens the flat
/// [`constants::DIAGNOSTIC_TEMP_MAX_AGE`] to the cache's own trust window when
/// the operator has configured a slow poll. **Never refuse on a reading the
/// thermal ladder itself is still acting on** — see that function.
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
/// as unusable (DEC-336).
///
/// `max(DIAGNOSTIC_TEMP_MAX_AGE, cache.cpu_temp_stale_after())`.
///
/// # Why this is not just the constant
///
/// The flat 10 s was chosen in DEC-333 as "ten poll intervals", on the stated
/// assumption that the sensor poll runs at 1 s — and while this predicate was
/// only ever *reported*, an over-strict budget cost a warning line. DEC-336
/// made it **refuse**, and an over-strict budget now aborts a diagnostic.
///
/// `polling.poll_interval_ms` in `daemon.toml` has no upper bound in
/// `DaemonConfig::validate`; the overlay clamps it down to
/// `MAX_SUPERVISABLE_POLL_INTERVAL_MS` (6 s), which is a supported cadence, not
/// a typo. At 6 s, ordinary reading ages reach 6 s and one missed poll passes
/// 10 s — so a fixed budget would refuse discovery on a perfectly healthy
/// machine, with nothing the operator could set to stop it. Worse, the daemon's
/// own trust window at that cadence is 30 s, so the thermal ladder would still
/// be acting on a reading this gate had declared unusable.
///
/// **That is the invariant, and it is a relationship rather than a number: a
/// diagnostic must never refuse on a reading the ladder is still acting on.**
/// `cpu_temp_stale_after` is the one place that window is defined, so this
/// consumes it rather than restating it — and the flat constant is kept as a
/// FLOOR so the default 1 s cadence (whose window is 5 s) behaves exactly as it
/// did before.
pub fn diagnostic_temp_max_age(cache: &StateCache) -> std::time::Duration {
    constants::DIAGNOSTIC_TEMP_MAX_AGE.max(cache.cpu_temp_stale_after())
}

/// [SAFETY] The refusal the preflight publishes, performed (DEC-336, `P8-p`).
///
/// `Some(message)` when `diagnostic` blocks on a stale temperature source AND
/// the cache holds no usable reading. `None` otherwise — including for the two
/// diagnostics that only *warn*, so this cannot silently widen what verify and
/// characterisation refuse.
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
            // The budget actually applied, not the bare constant: on a slow poll
            // cadence they differ, and a message naming a limit the daemon did
            // not use sends the operator looking for the wrong fault.
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
fn restore_pre_cal<F>(
    channel: u8,
    pre_cal_pwm: Option<u8>,
    write_fn: &F,
    cache: &StateCache,
    why: &str,
) where
    F: Fn(u8, u8) -> Result<(), CalibrationError>,
{
    let Some(restore) = pre_cal_pwm else {
        return;
    };
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
}

impl<F: Fn(u8, u8) -> Result<(), CalibrationError>> Drop for RestoreOnDrop<'_, F> {
    fn drop(&mut self) {
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

    let fan_id = format!("openfan:ch{channel:02}");
    let step_size = 100.0 / steps as f64;

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

            // Set PWM
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
    #[tokio::test(start_paused = true)]
    async fn calibration_restores_pre_cal_pwm_on_thermal_abort() {
        let cache = make_cache(90.0, 0, 800); // over limit — aborts before any sweep write
        let (write_fn, writes) = recording_write_fn(None);

        let result = calibrate_openfan_channel(cache, 0, 3, 0, write_fn).await;

        assert!(matches!(
            result.unwrap_err(),
            CalibrationError::ThermalAbort { .. }
        ));
        let w = writes.lock().unwrap();
        assert_eq!(
            w.as_slice(),
            &[(0u8, 50u8)],
            "abort must restore (and nothing else was written)"
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

    /// No pre-calibration PWM in the cache → nothing to restore (the sweep's
    /// own writes are the only ones issued).
    #[tokio::test(start_paused = true)]
    async fn calibration_skips_restore_without_pre_cal_pwm() {
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
        assert_eq!(w.len(), 4, "sweep writes only — no restore; writes: {w:?}");
        assert_eq!(w.last(), Some(&(0u8, 100u8)));
    }
}
