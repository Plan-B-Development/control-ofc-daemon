//! Fan RPM-to-PWM calibration sweep.
//!
//! Sweeps a fan from low to high PWM, recording RPM at each step.
//! Safety: aborts if any sensor exceeds the thermal limit, and restores
//! the pre-calibration PWM on every exit path — completion, thermal
//! abort, or a failed PWM write mid-sweep (DEC-134).

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

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

/// Run an OpenFan calibration sweep on a single channel.
///
/// The single sweep implementation — the `/fans/openfan/{ch}/calibrate`
/// handler delegates here (DEC-134; it previously kept a diverged inline
/// copy). This is a long-running async function (steps × hold_seconds). It:
/// 1. Reads the current PWM (for restore)
/// 2. Sweeps from 0% to 100% in `steps` increments
/// 3. Holds each step for `hold_seconds`, then reads RPM from cache
/// 4. Restores the pre-calibration PWM on EVERY exit path — success,
///    thermal abort, or a failed PWM write mid-sweep (DEC-134; previously
///    an early `?` could park the fan at a sweep step)
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
    let sweep_result = sweep.await;

    // Restore pre-calibration PWM — best-effort, on every exit path.
    if let Some(restore) = pre_cal_pwm {
        if let Err(e) = write_fn(channel, restore) {
            log::warn!("failed to restore pre-calibration PWM on ch{channel}: {e}");
        }
    }

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
