//! Hardware polling loops for hwmon sensors and OpenFanController fans.
//!
//! Each subsystem gets its own async loop that runs on a configurable interval,
//! reads hardware, and pushes results into the shared `StateCache`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// Read CLOCK_BOOTTIME (monotonic clock that includes suspend time).
/// Returns Duration::ZERO on failure.
fn boottime_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is signal-safe per POSIX. `ts` is a valid
    // mutable reference to a stack-local timespec — the call writes only
    // to this struct and touches no other memory. CLOCK_BOOTTIME is
    // supported on all Linux kernels >= 2.6.39 (our minimum target).
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return Duration::ZERO;
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

use crate::health::cache::StateCache;
use crate::health::state::{
    AmdGpuFanState, CachedSensorReading, DeviceLabel, HwmonFanState, OpenFanState,
};
use crate::hwmon::gpu_detect::AmdGpuInfo;
use crate::hwmon::types::SensorReading;
use crate::serial::protocol::Command;
use crate::serial::transport::{send_command, SerialTransport};

/// Convert hwmon `SensorReading` (with `SystemTime`) into cache `CachedSensorReading` (with `Instant`).
fn to_cached(reading: &SensorReading) -> CachedSensorReading {
    use crate::hwmon::types::SensorSource;
    let source = match reading.source {
        SensorSource::AmdGpu => DeviceLabel::AmdGpu,
        SensorSource::Hwmon => DeviceLabel::Hwmon,
    };
    CachedSensorReading {
        id: reading.id.clone(),
        kind: reading.kind,
        label: reading.label.clone(),
        value_c: reading.value_c,
        source,
        updated_at: Instant::now(),
        // Rate and min/max are computed by the cache on update
        rate_c_per_s: None,
        session_min_c: None,
        session_max_c: None,
        chip_name: reading.chip_name.clone(),
        temp_type: reading.temp_type,
    }
}

/// Run the hwmon sensor polling loop.
///
/// Discovers and reads all hwmon temperature sensors every `interval`,
/// and reads RPM/PWM for all discovered PWM headers, pushing results into the cache.
pub async fn hwmon_poll_loop(
    cache: Arc<StateCache>,
    history: Arc<crate::health::history::HistoryRing>,
    headers: Vec<crate::hwmon::pwm_discovery::PwmHeaderDescriptor>,
    gpu_infos: Vec<AmdGpuInfo>,
    hwmon_root: &Path,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let hwmon_root = hwmon_root.to_path_buf();
    let headers = Arc::new(headers);
    let gpu_infos = Arc::new(gpu_infos);
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut consecutive_errors: u32 = 0;
    let mut prev_boot: Option<Duration> = None;
    let mut prev_mono: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => {
                log::info!("hwmon poll loop shutting down");
                return;
            }
        }

        // Detect system suspend/resume via CLOCK_BOOTTIME vs CLOCK_MONOTONIC gap.
        // CLOCK_MONOTONIC pauses during suspend; CLOCK_BOOTTIME does not.
        let now_boot = boottime_now();
        let now_mono = Instant::now();
        if let (Some(pb), Some(pm)) = (prev_boot, prev_mono) {
            let boot_delta = now_boot.saturating_sub(pb);
            let mono_delta = now_mono.duration_since(pm);
            let suspend_gap = boot_delta.saturating_sub(mono_delta);
            if suspend_gap > Duration::from_secs(3) {
                log::info!(
                    "System resume detected (suspended ~{:.0}s). \
                     Signalling hwmon manual mode reset.",
                    suspend_gap.as_secs_f64()
                );
                cache.set_resume_detected();
            }
        }
        prev_boot = Some(now_boot);
        prev_mono = Some(now_mono);

        // Run blocking sysfs I/O on the blocking thread pool
        let root = hwmon_root.clone();
        let hdrs = headers.clone();
        let gpus = gpu_infos.clone();
        let result: Result<_, tokio::task::JoinError> = tokio::task::spawn_blocking(move || {
            let sensors = crate::hwmon::collect_sensors(&root);
            let fan_states: Vec<HwmonFanState> = read_hwmon_fan_states(&hdrs);
            let gpu_fan_states: Vec<AmdGpuFanState> = read_gpu_fan_states(&gpus);
            (sensors, fan_states, gpu_fan_states)
        })
        .await;

        match result {
            Ok((Ok((_descriptors, readings)), fan_states, gpu_fan_states)) => {
                consecutive_errors = 0;
                let cached: Vec<CachedSensorReading> = readings.iter().map(to_cached).collect();
                let count = cached.len();
                // Record to history ring buffer before cache update
                for r in &readings {
                    history.record(&r.id, r.value_c);
                }
                cache.update_sensors(cached);

                // Update hwmon fan state in cache
                if !fan_states.is_empty() {
                    let fan_count = fan_states.len();
                    cache.update_hwmon_fans(fan_states);
                    log::debug!("hwmon poll: {count} sensors, {fan_count} fans updated");
                } else {
                    log::debug!("hwmon poll: {count} sensors updated");
                }

                // Update GPU fan state in cache
                if !gpu_fan_states.is_empty() {
                    let gpu_count = gpu_fan_states.len();
                    cache.update_gpu_fans(gpu_fan_states);
                    log::debug!("gpu poll: {gpu_count} GPU fans updated");
                }
            }
            Ok((Err(e), _, _)) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 {
                    log::warn!("hwmon poll error: {e}");
                } else if consecutive_errors == 4 {
                    log::warn!("hwmon poll error (suppressing until periodic reminder): {e}");
                } else if consecutive_errors.is_multiple_of(60) {
                    log::warn!(
                        "hwmon poll error (persistent — {consecutive_errors} consecutive failures): {e}"
                    );
                }
            }
            Err(e) => {
                log::error!("hwmon poll task panicked: {e}");
            }
        }
    }
}

/// Read RPM and current PWM for all discovered hwmon PWM headers.
fn read_hwmon_fan_states(
    headers: &[crate::hwmon::pwm_discovery::PwmHeaderDescriptor],
) -> Vec<HwmonFanState> {
    let now = Instant::now();
    headers
        .iter()
        .filter_map(|h| {
            // Read RPM if tach is available
            let rpm = h
                .rpm_path
                .as_ref()
                .and_then(|p| match std::fs::read_to_string(p) {
                    Ok(s) => s.trim().parse::<u16>().ok(),
                    Err(e) => {
                        log::debug!(
                            "hwmon header '{}': failed to read RPM from {}: {e}",
                            h.id,
                            p
                        );
                        None
                    }
                });

            // Read current PWM raw value and convert to percent
            let pwm_pct = match std::fs::read_to_string(&h.pwm_path) {
                Ok(s) => s.trim().parse::<u8>().ok().map(crate::pwm::raw_to_percent),
                Err(e) => {
                    log::debug!(
                        "hwmon header '{}': failed to read PWM from {}: {e}",
                        h.id,
                        h.pwm_path
                    );
                    None
                }
            };

            // Only report if we got at least one meaningful reading
            if rpm.is_some() || pwm_pct.is_some() {
                Some(HwmonFanState {
                    id: h.id.clone(),
                    rpm,
                    last_commanded_pwm: pwm_pct,
                    updated_at: now,
                })
            } else {
                log::debug!("hwmon header '{}': no readable RPM or PWM — skipping", h.id);
                None
            }
        })
        .collect()
}

/// Read fan RPM for all detected AMD GPUs.
fn read_gpu_fan_states(gpus: &[AmdGpuInfo]) -> Vec<AmdGpuFanState> {
    let now = Instant::now();
    gpus.iter()
        .filter(|g| g.has_fan_rpm)
        .map(|g| {
            let fan_input = g.hwmon_path.join("fan1_input");
            let rpm = std::fs::read_to_string(&fan_input)
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok());

            AmdGpuFanState {
                id: format!("amd_gpu:{}", g.pci_bdf),
                rpm,
                last_commanded_pct: None, // Preserved from cache by the caller
                updated_at: now,
            }
        })
        .collect()
}

/// Run the OpenFanController RPM polling loop.
///
/// Sends `ReadAllRpm` every `interval` and pushes fan state into the cache.
/// After 5 consecutive errors, enters reconnect mode: attempts `auto_detect_port`
/// with exponential backoff (1s..30s) until the device reappears.
pub async fn openfan_poll_loop(
    cache: Arc<StateCache>,
    transport: Arc<parking_lot::Mutex<Box<dyn SerialTransport + Send>>>,
    timeout: Duration,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut consecutive_errors: u32 = 0;
    let reconnect_threshold: u32 = 5;
    let mut reconnect_backoff: u32 = 1;

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => {
                log::info!("openfan poll loop shutting down");
                return;
            }
        }

        // If too many consecutive errors, attempt reconnect instead of polling
        if consecutive_errors >= reconnect_threshold {
            let cycle = consecutive_errors - reconnect_threshold;
            let skip_cycles = reconnect_backoff.min(30);
            if skip_cycles == 0 || !cycle.is_multiple_of(skip_cycles) {
                consecutive_errors += 1;
                continue;
            }

            let t = timeout;
            let reconnect_result = tokio::task::spawn_blocking(move || {
                crate::serial::real_transport::auto_detect_port(t).and_then(|path| {
                    crate::serial::real_transport::RealSerialTransport::open(&path, t)
                        .ok()
                        .map(|rt| -> Box<dyn SerialTransport + Send> { Box::new(rt) })
                })
            })
            .await;

            match reconnect_result {
                Ok(Some(new_transport)) => {
                    let mut guard = transport.lock();
                    *guard = new_transport;
                    consecutive_errors = 0;
                    reconnect_backoff = 1;
                    log::info!("OpenFan Controller reconnected");
                    continue;
                }
                _ => {
                    reconnect_backoff = (reconnect_backoff * 2).min(30);
                    consecutive_errors += 1;
                    if consecutive_errors == reconnect_threshold + 1 {
                        log::warn!("OpenFan Controller disconnected — entering reconnect mode");
                    }
                    continue;
                }
            }
        }

        // Serial I/O is blocking — run on blocking pool
        let transport = transport.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = transport.lock();
            send_command(&mut **guard, &Command::ReadAllRpm, timeout)
        })
        .await;

        match result {
            Ok(Ok(response)) => {
                consecutive_errors = 0;
                reconnect_backoff = 1;
                let now = Instant::now();
                match response {
                    crate::serial::protocol::Response::Rpm { readings, .. } => {
                        // Preserve last_commanded_pwm from existing cache entries
                        let snap = cache.snapshot();
                        let fans: Vec<OpenFanState> = readings
                            .iter()
                            .map(|r| {
                                let existing_pwm = snap
                                    .openfan_fans
                                    .get(&r.channel)
                                    .and_then(|f| f.last_commanded_pwm);
                                OpenFanState {
                                    channel: r.channel,
                                    rpm: r.rpm,
                                    last_commanded_pwm: existing_pwm,
                                    updated_at: now,
                                    rpm_polled: true,
                                }
                            })
                            .collect();
                        let count = fans.len();
                        cache.update_openfan_fans(fans);
                        log::debug!("openfan poll: {count} channels updated");
                    }
                }
            }
            Ok(Err(e)) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 {
                    log::warn!("openfan poll error: {e}");
                } else if consecutive_errors == 4 {
                    log::warn!("openfan poll error (suppressing until periodic reminder): {e}");
                } else if consecutive_errors.is_multiple_of(60) {
                    log::warn!(
                        "openfan poll error (persistent — {consecutive_errors} consecutive failures): {e}"
                    );
                }
            }
            Err(e) => {
                log::error!("openfan poll task panicked: {e}");
            }
        }
    }
}
