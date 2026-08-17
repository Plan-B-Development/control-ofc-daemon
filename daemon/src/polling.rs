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
    AioPumpState, AmdGpuFanState, CachedSensorReading, DeviceLabel, HwmonFanState, OpenFanState,
};
use crate::hwmon::gpu_detect::AmdGpuInfo;
use crate::hwmon::intel_gpu_detect::IntelGpuInfo;
use crate::hwmon::nouveau_detect::NouveauGpuInfo;
use crate::hwmon::nvml::NvmlBackend;
use crate::hwmon::types::{SensorKind, SensorReading};
use crate::serial::protocol::Command;
use crate::serial::transport::{send_command, SerialTransport};

/// Convert hwmon `SensorReading` (with `SystemTime`) into cache `CachedSensorReading` (with `Instant`).
fn to_cached(reading: &SensorReading) -> CachedSensorReading {
    use crate::hwmon::types::SensorSource;
    let source = match reading.source {
        SensorSource::AmdGpu => DeviceLabel::AmdGpu,
        SensorSource::IntelGpu => DeviceLabel::IntelGpu,
        SensorSource::NvidiaGpu => DeviceLabel::NvidiaGpu,
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
        thresholds: reading.thresholds.clone(),
    }
}

/// Derive a lightweight AIO summary from this tick's cached sensors (DEC-156).
///
/// Returns `None` when no coolant sensor is present — there is no AIO subsystem
/// to report, so the poll loop leaves `subsystem_timestamps.aio` unset. When a
/// coolant sensor exists, the summary carries the hottest coolant reading; pump
/// duty/RPM are surfaced through the normal fan table, so they stay `None` here.
/// No safety semantics (there is no coolant threshold — see `safety.rs`).
fn derive_aio_state(sensors: &[CachedSensorReading], now: Instant) -> Option<AioPumpState> {
    let coolant_temp_c = sensors
        .iter()
        .filter(|s| s.kind == SensorKind::CoolantTemp)
        .map(|s| s.value_c)
        .reduce(f64::max)?;
    Some(AioPumpState {
        detected: true,
        pump_rpm: None,
        coolant_temp_c: Some(coolant_temp_c),
        pump_duty_pct: None,
        last_commanded_pct: None,
        updated_at: Some(now),
    })
}

/// Run the hwmon sensor polling loop.
///
/// Reads all hwmon temperature sensors every `interval`, and RPM/PWM for
/// all discovered PWM headers, pushing results into the cache.
///
/// Sensor descriptors are discovered once and cached (DEC-133); per-tick
/// work touches only `temp*_input` value files. Re-discovery (which
/// re-reads labels, types, and the DEC-117 threshold/alarm snapshot) runs
/// only on explicit triggers:
/// 1. `sensor_rescan` flag — set by `POST /hwmon/rescan`;
/// 2. a cached descriptor failing value-reads for
///    `SENSOR_READ_FAIL_REDISCOVER_STREAK` consecutive ticks (device
///    unbound mid-session);
/// 3. no `CpuTemp` descriptor in the cache — re-discover every tick so a
///    late-loading `k10temp`/`coretemp` is picked up immediately and the
///    no-sensor 40% fallback can release (this also covers the empty set).
///
/// Rationale: per-tick full discovery re-read ~12 threshold attributes per
/// sensor plus chip names/labels every second. Kernel drivers cache chip
/// registers (it87/nct6775 re-sweep at most every 1.5s) so most of that
/// was wasted syscalls, but on `asus_wmi_sensors` boards the kernel docs
/// warn that polling the WMI interface more frequently increases the risk
/// of fan/sensor misbehaviour — so the daemon now polls the minimum set.
#[allow(clippy::too_many_arguments)]
pub async fn hwmon_poll_loop(
    cache: Arc<StateCache>,
    history: Arc<crate::health::history::HistoryRing>,
    headers: Vec<crate::hwmon::pwm_discovery::PwmHeaderDescriptor>,
    gpu_infos: Vec<AmdGpuInfo>,
    intel_gpu_infos: Vec<IntelGpuInfo>,
    nouveau_gpu_infos: Vec<NouveauGpuInfo>,
    nvml_backend: Arc<dyn NvmlBackend>,
    hwmon_root: &Path,
    interval: Duration,
    sensor_rescan: Arc<std::sync::atomic::AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) {
    use crate::health::sensor_failure::{SensorFailureTracker, TrackerEvent};
    use crate::hwmon::types::{SensorDescriptor, SensorKind};
    use crate::hwmon::SensorReadOutcome;
    use std::sync::atomic::Ordering;

    let hwmon_root = hwmon_root.to_path_buf();
    let headers = Arc::new(headers);
    let gpu_infos = Arc::new(gpu_infos);
    let intel_gpu_infos = Arc::new(intel_gpu_infos);
    let nouveau_gpu_infos = Arc::new(nouveau_gpu_infos);
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut consecutive_errors: u32 = 0;
    let mut prev_boot: Option<Duration> = None;
    let mut prev_mono: Option<Instant> = None;

    // DEC-133: cached sensor descriptor set. `Arc` so each tick's spawn_blocking
    // can borrow the set without cloning descriptor contents.
    let mut descriptors: Arc<Vec<SensorDescriptor>> = Arc::new(Vec::new());
    let mut discovered_once = false;
    // DEC-193: owns per-descriptor read-failure streaks, the re-discovery
    // throttle, and quarantine of present-but-unreadable sensors (e.g. an
    // `ath12k` WiFi temp while the radio is down) so they cannot spam the
    // journal and are surfaced as "unavailable" instead.
    let mut failure_tracker =
        SensorFailureTracker::new(crate::constants::SENSOR_READ_FAIL_REDISCOVER_STREAK);

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

        // DEC-133/DEC-193: decide whether this tick re-runs sensor discovery.
        // The failure tracker grants a still-failing descriptor exactly one
        // re-discovery (the "did it actually unbind?" probe); once quarantined it
        // no longer asks, which is what ends the per-`threshold` re-discovery spam.
        let rescan_requested = sensor_rescan.swap(false, Ordering::SeqCst);
        let wants_rediscovery = failure_tracker.wants_rediscovery();
        let cpu_temp_missing = descriptors.iter().all(|d| d.kind != SensorKind::CpuTemp);
        let needs_discovery =
            !discovered_once || rescan_requested || wants_rediscovery || cpu_temp_missing;
        if rescan_requested {
            log::info!("Sensor descriptor refresh requested via /hwmon/rescan");
        } else if wants_rediscovery && discovered_once {
            log::warn!(
                "Re-discovering sensors after persistent read failures on {:?}",
                failure_tracker.rediscovery_ids()
            );
        }

        // Run blocking sysfs I/O on the blocking thread pool
        let root = hwmon_root.clone();
        let hdrs = headers.clone();
        let gpus = gpu_infos.clone();
        let intel_gpus = intel_gpu_infos.clone();
        let nouveau_gpus = nouveau_gpu_infos.clone();
        let nvml = nvml_backend.clone();
        let descs = descriptors.clone();
        let result: Result<_, tokio::task::JoinError> = tokio::task::spawn_blocking(move || {
            // NVIDIA NVML telemetry (opt-in, read-only, DEC-204): GPU temps merge
            // into the sensor readings, fan states into the GPU fan set. Both are
            // empty when NVML is disabled/absent (the default). Read once per tick.
            // Like the sysfs reads below, these are blocking C calls with no
            // per-call timeout: an NVIDIA driver fault could stall this tick until
            // it returns. The existing backstops bound the blast radius (the
            // shutdown-drain timeout still fires; the no-CPU-sensor 40% fallback
            // covers stale readings). A timeout wrapper around the whole blocking
            // leg — which would benefit the sysfs reads too — is a possible future
            // hardening, deliberately out of scope for this read-only slice.
            let (nvml_temps, nvml_fans) = read_nvml_states(&*nvml);
            // Sensor leg: full discovery only when triggered; otherwise the
            // hot path reads each cached descriptor's temp*_input file only.
            // The read returns successes *and* failures (DEC-193) — the loop owns
            // logging/quarantine policy, so this blocking leg stays silent.
            let sensors: Result<
                (Option<Vec<SensorDescriptor>>, SensorReadOutcome),
                crate::error::HwmonError,
            > = if needs_discovery {
                crate::hwmon::discovery::discover_sensors(&root).map(|fresh| {
                    let mut outcome = crate::hwmon::read_sensor_values(&fresh);
                    outcome.readings.extend(nvml_temps.iter().cloned());
                    (Some(fresh), outcome)
                })
            } else {
                let mut outcome = crate::hwmon::read_sensor_values(&descs);
                outcome.readings.extend(nvml_temps.iter().cloned());
                Ok((None, outcome))
            };
            let fan_states: Vec<HwmonFanState> = read_hwmon_fan_states(&hdrs);
            // AMD + Intel + NVIDIA discrete GPU fans share the cache `gpu_fans`
            // map, distinguished by their ID prefix (`amd_gpu:` / `intel_gpu:` /
            // `nvidia_gpu:`). Intel + nouveau + NVML are read-only (last_commanded_pct None).
            let mut gpu_fan_states: Vec<AmdGpuFanState> = read_gpu_fan_states(&gpus);
            gpu_fan_states.extend(read_intel_fan_states(&intel_gpus));
            // nouveau and NVML both mint `nvidia_gpu:<BDF>` ids, but they can
            // never collide for the same BDF: nouveau (open) and the proprietary
            // driver that provides libnvidia-ml are mutually exclusive kernel
            // modules per GPU, so each BDF is produced by at most one of the two.
            gpu_fan_states.extend(read_nouveau_fan_states(&nouveau_gpus));
            gpu_fan_states.extend(nvml_fans);
            (sensors, fan_states, gpu_fan_states)
        })
        .await;

        match result {
            Ok((Ok((fresh_descriptors, outcome)), fan_states, gpu_fan_states)) => {
                consecutive_errors = 0;
                if let Some(fresh) = fresh_descriptors {
                    if !discovered_once || rescan_requested {
                        log::info!("Sensor discovery: {} sensor(s) cached", fresh.len());
                    }
                    descriptors = Arc::new(fresh);
                    discovered_once = true;
                }

                let SensorReadOutcome { readings, failures } = outcome;

                // DEC-193: advance the failure tracker against the active
                // descriptor set. It quarantines a still-present-but-unreadable
                // sensor after one re-discovery probe (logged once), recovers it
                // on the next good read (logged once), and forgets a genuinely
                // unbound descriptor — replacing the old per-tick streak loop and
                // its 1-Hz `Failed to read sensor` spam.
                for event in failure_tracker.record_tick(&descriptors, &failures, Instant::now()) {
                    match event {
                        TrackerEvent::Quarantined { id, reason } => log::warn!(
                            "Sensor {id} unreadable ({reason}); suppressing further \
                             read-failure logs until it recovers"
                        ),
                        TrackerEvent::Recovered { id } => {
                            log::info!("Sensor {id} is readable again")
                        }
                    }
                }

                let cached: Vec<CachedSensorReading> = readings.iter().map(to_cached).collect();
                let count = cached.len();
                // Record to history ring buffer before cache update
                for r in &readings {
                    history.record(&r.id, r.value_c);
                }
                // Derive the AIO summary before `cached` is moved into the
                // cache (DEC-156): keeps `subsystem_timestamps.aio` fresh and
                // surfaces coolant temp on /status + /poll when a cooler exists.
                let aio_state = derive_aio_state(&cached, Instant::now());
                cache.update_sensors(cached);
                // Sync the quarantine set into the cache: evicts any stale
                // reading for an unavailable sensor and surfaces it on
                // /status + /poll (display-only). Cheap no-op when none.
                cache.update_unavailable_sensors(failure_tracker.unavailable());
                if let Some(aio) = aio_state {
                    cache.update_aio(aio);
                }

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
                duty_pct: None,           // AMD reports RPM, not a duty readback
                updated_at: now,
            }
        })
        .collect()
}

/// Read fan RPM for all detected Intel discrete GPUs (DEC-121).
///
/// Read-only: `last_commanded_pct` is always `None` because Intel fan control
/// is firmware-managed with no userspace write path. Only GPUs that actually
/// expose `fan1_input` produce a fan entity (a fanless/blower SKU yields none).
fn read_intel_fan_states(gpus: &[IntelGpuInfo]) -> Vec<AmdGpuFanState> {
    let now = Instant::now();
    gpus.iter()
        .filter(|g| g.has_fan_rpm)
        .map(|g| {
            let fan_input = g.hwmon_path.join("fan1_input");
            let rpm = std::fs::read_to_string(&fan_input)
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok());

            AmdGpuFanState {
                id: format!("intel_gpu:{}", g.pci_bdf),
                rpm,
                last_commanded_pct: None, // Always None — read-only.
                duty_pct: None,           // Intel reports RPM only
                updated_at: now,
            }
        })
        .collect()
}

/// Read fan RPM for all detected nouveau-backed NVIDIA discrete GPUs (DEC-204).
///
/// Read-only in Phase 1: `last_commanded_pct` is always `None`. Only GPUs that
/// actually expose `fan1_input` produce a fan entity. The id uses the vendor-
/// level `nvidia_gpu:` prefix (shared with the proprietary NVML leg added later).
fn read_nouveau_fan_states(gpus: &[NouveauGpuInfo]) -> Vec<AmdGpuFanState> {
    let now = Instant::now();
    gpus.iter()
        .filter(|g| g.has_fan_rpm)
        .map(|g| {
            let fan_input = g.hwmon_path.join("fan1_input");
            let rpm = std::fs::read_to_string(&fan_input)
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok());

            AmdGpuFanState {
                id: format!("nvidia_gpu:{}", g.pci_bdf),
                rpm,
                last_commanded_pct: None, // Always None — read-only (Phase 1).
                duty_pct: None,           // nouveau fan1_input is RPM only
                updated_at: now,
            }
        })
        .collect()
}

/// Read NVIDIA telemetry from the (opt-in) NVML backend into the poll pipeline
/// (DEC-204). Produces GPU temperature `SensorReading`s (source `NvidiaGpu`,
/// kind `GpuTemp`) plus read-only fan states (`nvidia_gpu:<BDF>`) carrying the
/// firmware-reported `duty_pct` and — on driver R565+ — `rpm`. Returns empty
/// vecs when NVML is disabled/absent. Runs on the blocking poll thread.
fn read_nvml_states(backend: &dyn NvmlBackend) -> (Vec<SensorReading>, Vec<AmdGpuFanState>) {
    let now_sys = std::time::SystemTime::now();
    let now = Instant::now();
    let mut sensors = Vec::new();
    let mut fans = Vec::new();
    for r in backend.read_all() {
        if let Some(temp_c) = r.temp_c {
            sensors.push(SensorReading {
                id: format!("nvidia_gpu:{}:temp", r.pci_bdf),
                kind: SensorKind::GpuTemp,
                label: "GPU".to_string(),
                value_c: temp_c,
                timestamp: now_sys,
                source: crate::hwmon::types::SensorSource::NvidiaGpu,
                chip_name: "nvml".to_string(),
                temp_type: None,
                thresholds: None,
            });
        }
        // Emit a single aggregate fan entity when NVML reported any fan telemetry.
        if r.fan_duty_pct.is_some() || r.fan_rpm.is_some() {
            fans.push(AmdGpuFanState {
                id: format!("nvidia_gpu:{}", r.pci_bdf),
                rpm: r.fan_rpm,
                last_commanded_pct: None, // read-only
                duty_pct: r.fan_duty_pct,
                updated_at: now,
            });
        }
    }
    (sensors, fans)
}

/// Run the OpenFanController RPM polling loop.
///
/// Sends `ReadAllRpm` every `interval` and pushes fan state into the cache.
/// After 5 consecutive errors, enters reconnect mode: attempts `auto_detect_port`
/// with exponential backoff (1s..30s) until the device reappears.
/// Verify and adopt a re-opened OpenFan transport, or refuse it (DEC-260).
///
/// Extracted from `openfan_poll_loop` for the same reason `first_openfan_port`
/// was extracted from `main`: the reconnect arm's two safety steps were
/// reachable only behind five consecutive failures *and* a backoff cycle, so
/// the pre-release review could delete either one with the whole suite green.
/// They are:
///
/// 1. **Identity** (DEC-250/255) — detection probes on its own fd and then
///    closes it, so the transport actually adopted has never been verified.
///    "Openability is not identity" applies hardest here, because this is the
///    path that runs continuously at runtime, where a device swap between
///    probe and open is the entire risk.
/// 2. **Invalidation** (DEC-256) — the device just re-enumerated, so
///    `FanController`'s per-channel coalescing cache describes a state that may
///    no longer exist. Invalidate *before* the transport goes live, or the next
///    identical command is coalesced into silence.
///
/// They belong together: invalidating only after a *verified* adoption is what
/// keeps an impostor from resetting the real device's write cache.
fn adopt_reconnected_transport<T: SerialTransport + Send + 'static>(
    cache: &StateCache,
    timeout: Duration,
    open: impl FnOnce() -> Option<(String, T)>,
) -> Option<Box<dyn SerialTransport + Send>> {
    let (path, mut rt) = open()?;
    crate::serial::transport::verify_openfan_identity(&mut rt, timeout)
        .map_err(|e| {
            log::warn!(
                "Reconnect: {path} opened but did not identify as an \
                 OpenFanController ({e}) — not adopting it"
            );
        })
        .ok()?;
    cache.invalidate_openfan_writes();
    Some(Box::new(rt))
}

/// Consecutive failed polls before the loop stops polling and starts trying to
/// reconnect instead. Named so the value is pinnable: lowering it diverts the
/// loop into a seconds-long blocking probe on every transient hiccup.
const RECONNECT_THRESHOLD: u32 = 5;

/// Should this over-threshold cycle attempt a reconnect, or skip and wait?
///
/// The loop calls this; so do its tests. It used to be an expression inline in
/// the loop with a *copy* of it in the test module, and the copy had drifted
/// into asserting the opposite of production for `backoff == 0` (DEC-266). A
/// mirror that can disagree with the thing it mirrors is worse than no test.
///
/// A zero window means "no backoff" — attempt every cycle. Written as a guard
/// rather than falling into `is_multiple_of(0)`. Unreachable from the loop's own
/// arithmetic today (backoff starts at 1 and only doubles), but load-bearing if
/// that changes, and the safe direction is to retry rather than to strand a
/// reconnectable controller forever.
fn attempts_reconnect_this_cycle(consecutive_errors: u32, threshold: u32, backoff: u32) -> bool {
    let cycle = consecutive_errors.saturating_sub(threshold);
    let skip_cycles = backoff.min(30);
    skip_cycles == 0 || cycle.is_multiple_of(skip_cycles)
}

/// Does a completed poll attempt count toward the reconnect threshold?
///
/// DEC-265: a panicked blocking task counts, like any other failed poll. It did
/// not before, so the one failure mode that never self-heals — the task dying
/// every tick — was also the one that could never reach the threshold and
/// prompt the reconnect that might have fixed it.
///
/// All three outcomes route through this single rule so a newly added one
/// cannot quietly opt out of the failure count the way the panic arm did.
fn poll_attempt_failed<T>(
    result: &Result<Result<T, crate::error::SerialError>, tokio::task::JoinError>,
) -> bool {
    !matches!(result, Ok(Ok(_)))
}

/// Poll the OpenFanController for per-channel RPM at `interval`, reconnecting
/// when the device stops answering.
///
/// Runs until `shutdown` flips. Serial I/O is blocking, so each poll and each
/// reconnect attempt runs on the blocking pool.
///
/// Reconnect is not a separate task: after `reconnect_threshold` consecutive
/// failures the loop stops polling and starts attempting adoption instead, with
/// exponential backoff capped at 30 cycles. A candidate must pass the DEC-250
/// identity probe before it is adopted — an openable port that is not an
/// OpenFanController is worse than no port, because every subsequent write
/// silently goes somewhere else.
///
/// Spawned either when a controller is adopted at startup (`main.rs`) or when
/// one is adopted later by `POST /fans/openfan/rescan` (DEC-265,
/// `api::handlers::openfan`). Exactly one loop exists per adopted controller:
/// the slot is written once and never replaced — the rescan installs only under
/// the same write guard it checks (DEC-266), so two racing rescans cannot each
/// start a loop, and neither can replace a controller the engine is already
/// writing through. Until *some* controller is
/// adopted there is no OpenFan backend at all, which also costs the 105 C
/// thermal emergency its OpenFan leg.
///
/// (This block was lost once in `419025d`, which moved the reconnect helper out
/// and left the loop bare, and again in DEC-266, where it ended up attached to
/// the constant below it and silently documented a `u32`. Keep it on the fn.)
pub async fn openfan_poll_loop(
    cache: Arc<StateCache>,
    transport: Arc<parking_lot::Mutex<Box<dyn SerialTransport + Send>>>,
    timeout: Duration,
    interval: Duration,
    shutdown: watch::Receiver<bool>,
) {
    // DEC-266: the real reconnect probe, injected. Everything else lives in
    // `openfan_poll_loop_with`, which is the same code the daemon runs — so a
    // test can drive the actual loop instead of a copy of its arithmetic. Before
    // this split the loop had never been executed by any test ("driving the real
    // loop needs a serial device"), and that is exactly how the panic-counting
    // fix came to be pinned at its helper but not at its call site.
    openfan_poll_loop_with(cache, transport, timeout, interval, shutdown, |c, t| {
        adopt_reconnected_transport(c, t, || {
            let path = crate::serial::real_transport::auto_detect_port(t)?;
            let rt = crate::serial::real_transport::RealSerialTransport::open(&path, t).ok()?;
            Some((path, rt))
        })
    })
    .await;
}

/// The poll loop proper, with the reconnect probe as a parameter.
///
/// `reconnect` runs on the blocking pool and returns a replacement transport, or
/// `None` if nothing suitable was found. It must perform the DEC-250 identity
/// handshake itself — the production closure does so via
/// [`adopt_reconnected_transport`].
async fn openfan_poll_loop_with<F>(
    cache: Arc<StateCache>,
    transport: Arc<parking_lot::Mutex<Box<dyn SerialTransport + Send>>>,
    timeout: Duration,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    reconnect: F,
) where
    F: Fn(&Arc<StateCache>, Duration) -> Option<Box<dyn SerialTransport + Send>>
        + Send
        + Sync
        + Clone
        + 'static,
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut consecutive_errors: u32 = 0;
    let reconnect_threshold: u32 = RECONNECT_THRESHOLD;
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
            if !attempts_reconnect_this_cycle(
                consecutive_errors,
                reconnect_threshold,
                reconnect_backoff,
            ) {
                consecutive_errors += 1;
                continue;
            }

            let t = timeout;
            let c = cache.clone();
            let probe = reconnect.clone();
            let reconnect_result = tokio::task::spawn_blocking(move || probe(&c, t)).await;

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

        // DEC-265/266: one accounting rule for every outcome, applied before the
        // match so no arm can forget it (the panic arm used to).
        if poll_attempt_failed(&result) {
            consecutive_errors += 1;
        } else {
            consecutive_errors = 0;
            reconnect_backoff = 1;
        }

        match result {
            Ok(Ok(response)) => {
                let now = Instant::now();
                match response {
                    crate::serial::protocol::Response::Rpm { readings, .. } => {
                        let fans: Vec<OpenFanState> = readings
                            .iter()
                            .map(|r| OpenFanState {
                                channel: r.channel,
                                rpm: r.rpm,
                                // None → update_openfan_fans preserves the
                                // cached value (DEC-146 P3-7). Previously this
                                // loop cloned the entire DaemonState every
                                // second just to copy this one field forward.
                                last_commanded_pwm: None,
                                updated_at: now,
                                rpm_polled: true,
                            })
                            .collect();
                        let count = fans.len();
                        cache.update_openfan_fans(fans);
                        log::debug!("openfan poll: {count} channels updated");
                    }
                }
            }
            Ok(Err(e)) => {
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
                // Counted above by `poll_attempt_failed` (DEC-265) — this arm
                // only distinguishes a panicked task in the log from a fan that
                // merely refused the read.
                log::error!("openfan poll task panicked: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── DEC-260: reconnect adoption (identity + cache invalidation) ──

    /// Replays canned lines, so a "device" can be made to answer the identity
    /// probe like an OpenFanController — or like anything else.
    struct ReplayTransport(std::collections::VecDeque<String>);

    impl SerialTransport for ReplayTransport {
        fn write_line(&mut self, _data: &str) -> Result<(), crate::error::SerialError> {
            Ok(())
        }

        fn read_line(&mut self, _timeout: Duration) -> Result<String, crate::error::SerialError> {
            self.0
                .pop_front()
                .ok_or(crate::error::SerialError::Timeout { timeout_ms: 50 })
        }
    }

    fn openfan_replies() -> ReplayTransport {
        ReplayTransport(
            vec![concat!(
                "<00|00:04B0;01:044C;02:0000;03:0000;04:0000;",
                "05:0000;06:0000;07:0000;08:0000;09:0000;>\r\n"
            )
            .to_string()]
            .into(),
        )
    }

    #[test]
    fn a_reconnected_openfan_is_adopted_and_invalidates_the_write_cache() {
        let cache = StateCache::new();
        let before = cache.openfan_write_generation();

        let adopted = adopt_reconnected_transport(&cache, Duration::from_millis(50), || {
            Some(("/dev/ttyACM0".to_string(), openfan_replies()))
        });

        assert!(
            adopted.is_some(),
            "a device that identifies must be adopted"
        );
        assert_ne!(
            cache.openfan_write_generation(),
            before,
            "DEC-256: the re-enumerated device's stale per-channel coalescing cache \
             must be invalidated, or the next identical command is coalesced into silence"
        );
    }

    #[test]
    fn a_reconnected_impostor_is_refused_and_leaves_the_write_cache_alone() {
        // DEC-250/255. The port opens but answers with something that is not an
        // OpenFanController frame — a device swap between probe and open. It must
        // not be adopted, and it must not get to reset the real device's cache.
        let cache = StateCache::new();
        let before = cache.openfan_write_generation();

        let adopted = adopt_reconnected_transport(&cache, Duration::from_millis(50), || {
            Some((
                "/dev/ttyACM0".to_string(),
                ReplayTransport(vec!["I am a 3D printer\r\n".to_string()].into()),
            ))
        });

        assert!(
            adopted.is_none(),
            "openability is not identity — an unverified device must not be adopted"
        );
        assert_eq!(
            cache.openfan_write_generation(),
            before,
            "a refused impostor must not invalidate the real device's write cache"
        );
    }

    #[test]
    fn a_port_that_will_not_open_is_not_an_adoption() {
        let cache = StateCache::new();
        let before = cache.openfan_write_generation();

        let adopted = adopt_reconnected_transport::<ReplayTransport>(
            &cache,
            Duration::from_millis(50),
            || None,
        );

        assert!(adopted.is_none());
        assert_eq!(cache.openfan_write_generation(), before);
    }

    // ── DEC-133: sensor descriptor cache ─────────────────────────────

    /// Write a fake k10temp device (CpuTemp) into the tempdir sysfs root.
    fn write_k10temp(root: &std::path::Path, temp_c: f64) {
        let dir = root.join("hwmon0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), "k10temp\n").unwrap();
        fs::write(
            dir.join("temp1_input"),
            format!("{}\n", (temp_c * 1000.0) as i64),
        )
        .unwrap();
        fs::write(dir.join("temp1_label"), "Tctl\n").unwrap();
    }

    /// Write a fake nvme device (non-CPU) into the tempdir sysfs root.
    fn write_nvme(root: &std::path::Path) {
        let dir = root.join("hwmon1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), "nvme\n").unwrap();
        fs::write(dir.join("temp1_input"), "38000\n").unwrap();
        fs::write(dir.join("temp1_label"), "Composite\n").unwrap();
    }

    /// Write a chip whose `temp1_input` exists (so discovery creates a
    /// descriptor) but holds unparseable data, so every value-read fails while
    /// the sysfs node persists — the present-but-unreadable shape of an `ath12k`
    /// WiFi temp with the radio off (DEC-193).
    fn write_unreadable_chip(root: &std::path::Path, dir: &str, chip: &str) {
        let d = root.join(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("name"), format!("{chip}\n")).unwrap();
        fs::write(d.join("temp1_input"), "garbage\n").unwrap();
    }

    /// Write a fake NZXT Kraken (z53) into the tempdir sysfs root: a single
    /// `temp1` = coolant (no label needed — chip name classifies it).
    fn write_kraken(root: &std::path::Path, coolant_c: f64) {
        let dir = root.join("hwmon2");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), "z53\n").unwrap();
        fs::write(
            dir.join("temp1_input"),
            format!("{}\n", (coolant_c * 1000.0) as i64),
        )
        .unwrap();
    }

    fn cached_temp(label: &str, kind: SensorKind, value_c: f64) -> CachedSensorReading {
        CachedSensorReading {
            id: format!("hwmon:test:{label}"),
            kind,
            label: label.to_string(),
            value_c,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "z53".to_string(),
            temp_type: None,
            thresholds: None,
        }
    }

    #[test]
    fn derive_aio_state_none_without_coolant() {
        let now = Instant::now();
        let sensors = [cached_temp("Tctl", SensorKind::CpuTemp, 60.0)];
        assert!(derive_aio_state(&sensors, now).is_none());
    }

    #[test]
    fn derive_aio_state_reports_hottest_coolant() {
        let now = Instant::now();
        let sensors = [
            cached_temp("Tctl", SensorKind::CpuTemp, 80.0),
            cached_temp("Coolant", SensorKind::CoolantTemp, 34.0),
            cached_temp("Coolant2", SensorKind::CoolantTemp, 37.5),
        ];
        let aio = derive_aio_state(&sensors, now).expect("coolant present");
        assert!(aio.detected);
        assert_eq!(aio.coolant_temp_c, Some(37.5));
        assert!(aio.updated_at.is_some());
    }

    struct PollHarness {
        cache: Arc<StateCache>,
        rescan: Arc<std::sync::atomic::AtomicBool>,
        shutdown_tx: watch::Sender<bool>,
        handle: tokio::task::JoinHandle<()>,
    }

    fn spawn_poll_loop(root: std::path::PathBuf) -> PollHarness {
        let cache = Arc::new(StateCache::new());
        let rescan = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let history = Arc::new(crate::health::history::HistoryRing::new(16));
        let (cache2, rescan2) = (cache.clone(), rescan.clone());
        let handle = tokio::spawn(async move {
            hwmon_poll_loop(
                cache2,
                history,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Arc::new(crate::hwmon::nvml::DisabledNvml),
                &root,
                Duration::from_secs(1),
                rescan2,
                shutdown_rx,
            )
            .await;
        });
        PollHarness {
            cache,
            rescan,
            shutdown_tx,
            handle,
        }
    }

    async fn stop(h: PollHarness) {
        let _ = h.shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), h.handle).await;
    }

    fn sensor_by_label(
        cache: &StateCache,
        label: &str,
    ) -> Option<crate::health::state::CachedSensorReading> {
        cache
            .sensors_snapshot()
            .values()
            .find(|s| s.label == label)
            .cloned()
    }

    /// Per-tick reads must use the cached descriptor set: changing a label
    /// file after discovery is invisible until a refresh trigger, while
    /// values stay fresh.
    #[tokio::test(start_paused = true)]
    async fn poll_loop_uses_cached_descriptors_between_ticks() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        // Tick 1 fires immediately (interval semantics) — wait past it.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "initial discovery"
        );

        // Mutate the label (descriptor metadata) AND the value.
        fs::write(tmp.path().join("hwmon0/temp1_label"), "Changed\n").unwrap();
        fs::write(tmp.path().join("hwmon0/temp1_input"), "65000\n").unwrap();
        tokio::time::sleep(Duration::from_millis(1000)).await; // tick 2

        let s = sensor_by_label(&h.cache, "Tctl")
            .expect("label must come from the cached descriptor, not a re-read");
        assert!(
            (s.value_c - 65.0).abs() < f64::EPSILON,
            "values must stay fresh"
        );
        assert!(
            sensor_by_label(&h.cache, "Changed").is_none(),
            "no re-discovery without a trigger (DEC-133)"
        );

        stop(h).await;
    }

    /// DEC-156: a discovered coolant sensor populates `AioPumpState` and sets
    /// the `aio` subsystem freshness timestamp via the live poll loop (wires the
    /// previously-dead `update_aio`).
    #[tokio::test(start_paused = true)]
    async fn poll_loop_populates_aio_state_for_kraken() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0); // CPU sensor present
        write_kraken(tmp.path(), 33.0); // NZXT Kraken coolant
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1

        let snap = h.cache.snapshot();
        assert!(snap.aio.detected, "kraken coolant must mark AIO detected");
        assert_eq!(snap.aio.coolant_temp_c, Some(33.0));
        assert!(
            snap.subsystem_timestamps.aio.is_some(),
            "aio subsystem freshness must be set"
        );

        stop(h).await;
    }

    /// Without a coolant sensor, the loop must NOT fabricate AIO freshness.
    #[tokio::test(start_paused = true)]
    async fn poll_loop_leaves_aio_unset_without_coolant() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1

        let snap = h.cache.snapshot();
        assert!(!snap.aio.detected);
        assert!(snap.subsystem_timestamps.aio.is_none());

        stop(h).await;
    }

    /// The /hwmon/rescan flag forces a descriptor refresh on the next tick
    /// and is consumed by the loop.
    #[tokio::test(start_paused = true)]
    async fn poll_loop_rediscovers_on_rescan_flag() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
        fs::write(tmp.path().join("hwmon0/temp1_label"), "Renamed\n").unwrap();
        h.rescan.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1000)).await; // tick 2

        assert!(
            sensor_by_label(&h.cache, "Renamed").is_some(),
            "rescan flag must trigger re-discovery"
        );
        assert!(
            !h.rescan.load(std::sync::atomic::Ordering::SeqCst),
            "flag must be consumed"
        );

        stop(h).await;
    }

    /// A descriptor failing value-reads for the configured streak triggers
    /// one re-discovery (device unbound mid-session).
    #[tokio::test(start_paused = true)]
    async fn poll_loop_rediscovers_after_read_failure_streak() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
                                                              // Device vanishes; a different chip appears (not yet discovered).
        fs::remove_file(tmp.path().join("hwmon0/temp1_input")).unwrap();
        write_nvme(tmp.path());

        // Streak builds one failed tick at a time, then the next tick
        // re-discovers: STREAK ticks + 1 + margin.
        let ticks = crate::constants::SENSOR_READ_FAIL_REDISCOVER_STREAK + 2;
        for _ in 0..ticks {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        assert!(
            sensor_by_label(&h.cache, "Composite").is_some(),
            "read-failure streak must trigger re-discovery and pick up the new chip"
        );

        stop(h).await;
    }

    /// DEC-193: a sensor present in sysfs but failing every read (an `ath12k`
    /// WiFi temp while the radio is down) is quarantined — surfaced as
    /// unavailable, kept out of `sensors`, and it stops driving re-discovery so
    /// it no longer spams the journal. We prove the loop is no longer
    /// re-discovering by adding a fresh chip *after* quarantine and asserting it
    /// is NOT picked up — a still-re-discovering loop would surface it.
    #[tokio::test(start_paused = true)]
    async fn poll_loop_quarantines_present_but_unreadable_sensor() {
        let tmp = tempfile::tempdir().unwrap();
        // Readable CPU sensor → no cpu_temp_missing trigger forcing per-tick
        // discovery, isolating the failure-driven re-discovery path.
        write_k10temp(tmp.path(), 55.0);
        write_unreadable_chip(tmp.path(), "hwmon3", "ath12k_hwmon");
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        // Advance past STREAK + the single re-discovery + quarantine.
        let ticks = crate::constants::SENSOR_READ_FAIL_REDISCOVER_STREAK + 4;
        for _ in 0..ticks {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        let snap = h.cache.snapshot();
        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "the readable CPU sensor stays in service"
        );
        assert_eq!(
            snap.unavailable_sensors.len(),
            1,
            "the unreadable WiFi temp must be quarantined as unavailable"
        );
        assert!(snap.unavailable_sensors[0].id.contains("ath12k_hwmon"));
        assert!(snap.unavailable_sensors[0]
            .reason
            .contains("invalid temperature"));
        assert!(
            !snap.sensors.values().any(|s| s.chip_name == "ath12k_hwmon"),
            "an unreadable sensor must never be served as a live reading"
        );

        // A new chip appears AFTER quarantine. Because the WiFi temp is
        // quarantined (no re-discovery) and the CPU sensor is present (no
        // cpu_temp_missing trigger), the loop performs NO discovery — so the new
        // chip stays invisible. This is the anti-spam property at the loop level.
        write_nvme(tmp.path());
        for _ in 0..(crate::constants::SENSOR_READ_FAIL_REDISCOVER_STREAK + 2) {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        assert!(
            sensor_by_label(&h.cache, "Composite").is_none(),
            "a quarantined sensor must stop driving re-discovery (no journal spam)"
        );
        assert_eq!(
            h.cache.snapshot().unavailable_sensors.len(),
            1,
            "the quarantine stays stable — surfaced once, not re-churned"
        );

        stop(h).await;
    }

    /// While no CpuTemp descriptor is cached, the loop re-discovers every
    /// tick — a late-loading k10temp must appear without any flag, so the
    /// no-sensor 40% fallback can release (P0-R1).
    #[tokio::test(start_paused = true)]
    async fn poll_loop_rediscovers_while_no_cpu_sensor_cached() {
        let tmp = tempfile::tempdir().unwrap();
        write_nvme(tmp.path()); // non-CPU only
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
        assert!(sensor_by_label(&h.cache, "Composite").is_some());

        // CPU sensor module loads late.
        write_k10temp(tmp.path(), 60.0);
        tokio::time::sleep(Duration::from_millis(1000)).await; // tick 2

        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "missing-CpuTemp trigger must re-discover every tick"
        );

        stop(h).await;
    }

    fn fake_intel_gpu(
        hwmon_path: std::path::PathBuf,
        bdf: &str,
        has_fan_rpm: bool,
    ) -> IntelGpuInfo {
        IntelGpuInfo {
            pci_bdf: bdf.to_string(),
            pci_device_id: 0xE20B,
            pci_revision: 0,
            pci_class: 0x030000,
            marketing_name: Some("Arc B580".into()),
            driver: "xe".into(),
            hwmon_path,
            is_discrete: true,
            has_fan_rpm,
        }
    }

    #[test]
    fn read_intel_fan_states_reads_rpm_with_intel_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path().join("hwmon3");
        fs::create_dir_all(&hwmon).unwrap();
        fs::write(hwmon.join("fan1_input"), "1234\n").unwrap();

        let gpus = [fake_intel_gpu(hwmon, "0000:03:00.0", true)];
        let states = read_intel_fan_states(&gpus);

        assert_eq!(states.len(), 1);
        assert_eq!(states[0].id, "intel_gpu:0000:03:00.0");
        assert_eq!(states[0].rpm, Some(1234));
        // Read-only: never a commanded percentage.
        assert_eq!(states[0].last_commanded_pct, None);
        // hwmon RPM source has no duty readback — duty_pct is NVML-only.
        assert_eq!(states[0].duty_pct, None);
    }

    #[test]
    fn read_intel_fan_states_skips_fanless_gpu() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path().join("hwmon3");
        fs::create_dir_all(&hwmon).unwrap();

        let gpus = [fake_intel_gpu(hwmon, "0000:03:00.0", false)];
        assert!(read_intel_fan_states(&gpus).is_empty());
    }

    #[test]
    fn read_intel_fan_states_missing_file_yields_none_rpm() {
        // has_fan_rpm true at detection, but the file vanished by poll time.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path().join("hwmon3");
        fs::create_dir_all(&hwmon).unwrap();

        let gpus = [fake_intel_gpu(hwmon, "0000:03:00.0", true)];
        let states = read_intel_fan_states(&gpus);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].rpm, None);
    }

    fn fake_nouveau_gpu(
        hwmon_path: std::path::PathBuf,
        bdf: &str,
        has_fan_rpm: bool,
    ) -> NouveauGpuInfo {
        NouveauGpuInfo {
            pci_bdf: bdf.to_string(),
            hwmon_path,
            has_fan_rpm,
        }
    }

    #[test]
    fn read_nouveau_fan_states_reads_rpm_with_nvidia_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path().join("hwmon4");
        fs::create_dir_all(&hwmon).unwrap();
        fs::write(hwmon.join("fan1_input"), "1600\n").unwrap();

        let gpus = [fake_nouveau_gpu(hwmon, "0000:03:00.0", true)];
        let states = read_nouveau_fan_states(&gpus);

        assert_eq!(states.len(), 1);
        assert_eq!(states[0].id, "nvidia_gpu:0000:03:00.0");
        assert_eq!(states[0].rpm, Some(1600));
        // Read-only: never a commanded percentage.
        assert_eq!(states[0].last_commanded_pct, None);
        // nouveau fan1_input has no duty readback — duty_pct is NVML-only.
        assert_eq!(states[0].duty_pct, None);
    }

    #[test]
    fn read_nouveau_fan_states_skips_fanless_gpu() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path().join("hwmon4");
        fs::create_dir_all(&hwmon).unwrap();

        let gpus = [fake_nouveau_gpu(hwmon, "0000:03:00.0", false)];
        assert!(read_nouveau_fan_states(&gpus).is_empty());
    }

    #[test]
    fn read_nouveau_fan_states_missing_file_yields_none_rpm() {
        // has_fan_rpm true at detection, but the file vanished by poll time.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path().join("hwmon4");
        fs::create_dir_all(&hwmon).unwrap();

        let gpus = [fake_nouveau_gpu(hwmon, "0000:03:00.0", true)];
        let states = read_nouveau_fan_states(&gpus);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].rpm, None);
    }

    #[test]
    fn to_cached_maps_nvidia_source_to_nvidia_label() {
        // A nouveau GPU temp's `source` must survive the SensorReading ->
        // CachedSensorReading conversion so `/sensors` reports "nvidia_gpu"
        // (DEC-204) — a wrong/omitted arm would silently retag it as hwmon.
        let reading = SensorReading {
            id: "nvidia_gpu:nouveau:temp1".into(),
            kind: SensorKind::GpuTemp,
            label: "temp1".into(),
            value_c: 42.0,
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            source: crate::hwmon::types::SensorSource::NvidiaGpu,
            chip_name: "nouveau".into(),
            temp_type: None,
            thresholds: None,
        };
        let cached = to_cached(&reading);
        assert_eq!(cached.source, DeviceLabel::NvidiaGpu);
        assert_eq!(cached.kind, SensorKind::GpuTemp);
    }

    #[test]
    fn read_nvml_states_produces_temp_and_fan() {
        use crate::hwmon::nvml::{FakeNvml, NvmlReading};
        let backend = FakeNvml::new(vec![NvmlReading {
            pci_bdf: "0000:03:00.0".into(),
            temp_c: Some(61.0),
            fan_duty_pct: Some(55),
            fan_rpm: Some(1800),
        }]);
        let (temps, fans) = read_nvml_states(&backend);

        assert_eq!(temps.len(), 1);
        assert_eq!(temps[0].id, "nvidia_gpu:0000:03:00.0:temp");
        assert_eq!(
            temps[0].source,
            crate::hwmon::types::SensorSource::NvidiaGpu
        );
        assert_eq!(temps[0].kind, SensorKind::GpuTemp);
        assert_eq!(temps[0].value_c, 61.0);

        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].id, "nvidia_gpu:0000:03:00.0");
        assert_eq!(fans[0].rpm, Some(1800));
        assert_eq!(fans[0].duty_pct, Some(55));
        // Read-only: never a commanded percentage.
        assert_eq!(fans[0].last_commanded_pct, None);
    }

    #[test]
    fn read_nvml_states_empty_backend_yields_nothing() {
        let (temps, fans) = read_nvml_states(&crate::hwmon::nvml::DisabledNvml);
        assert!(temps.is_empty());
        assert!(fans.is_empty());
    }

    #[test]
    fn read_nvml_states_temp_only_emits_no_fan() {
        use crate::hwmon::nvml::{FakeNvml, NvmlReading};
        // A GPU reporting temperature but no fan telemetry (fanless/unsupported):
        // a temp sensor appears, but no fan entity is fabricated.
        let backend = FakeNvml::new(vec![NvmlReading {
            pci_bdf: "0000:03:00.0".into(),
            temp_c: Some(50.0),
            fan_duty_pct: None,
            fan_rpm: None,
        }]);
        let (temps, fans) = read_nvml_states(&backend);
        assert_eq!(temps.len(), 1);
        assert!(fans.is_empty());
    }

    #[test]
    fn read_nvml_states_emits_fan_on_duty_only_or_rpm_only() {
        use crate::hwmon::nvml::{FakeNvml, NvmlReading};
        // Partial fan telemetry must still emit a fan entity — pins the `||`
        // emission gate (a `&&` regression would drop these). (a) duty %, no RPM
        // (common pre-R565); (b) RPM, no duty.
        let duty_only = FakeNvml::new(vec![NvmlReading {
            pci_bdf: "0000:03:00.0".into(),
            temp_c: None,
            fan_duty_pct: Some(60),
            fan_rpm: None,
        }]);
        let (_, fans) = read_nvml_states(&duty_only);
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].duty_pct, Some(60));
        assert_eq!(fans[0].rpm, None);

        let rpm_only = FakeNvml::new(vec![NvmlReading {
            pci_bdf: "0000:03:00.0".into(),
            temp_c: None,
            fan_duty_pct: None,
            fan_rpm: Some(1500),
        }]);
        let (_, fans) = read_nvml_states(&rpm_only);
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].rpm, Some(1500));
        assert_eq!(fans[0].duty_pct, None);
    }

    #[test]
    fn read_nvml_states_multi_gpu_distinct_ids() {
        use crate::hwmon::nvml::{FakeNvml, NvmlReading};
        let backend = FakeNvml::new(vec![
            NvmlReading {
                pci_bdf: "0000:03:00.0".into(),
                temp_c: Some(40.0),
                fan_duty_pct: Some(30),
                fan_rpm: None,
            },
            NvmlReading {
                pci_bdf: "0000:0a:00.0".into(),
                temp_c: Some(70.0),
                fan_duty_pct: Some(80),
                fan_rpm: None,
            },
        ]);
        let (temps, fans) = read_nvml_states(&backend);
        assert_eq!(temps.len(), 2);
        assert_eq!(fans.len(), 2);
        // Distinct BDFs → distinct fan ids (no collision/overwrite).
        assert_eq!(fans[0].id, "nvidia_gpu:0000:03:00.0");
        assert_eq!(fans[1].id, "nvidia_gpu:0000:0a:00.0");
        assert_eq!(temps[0].id, "nvidia_gpu:0000:03:00.0:temp");
        assert_eq!(temps[1].id, "nvidia_gpu:0000:0a:00.0:temp");
    }

    // ── DEC-265: the reconnect *trigger* arithmetic ──
    //
    // The block that decides WHEN to attempt a reconnect (threshold, backoff,
    // skip-cycles) had no test at all: the existing DEC-260 cases cover
    // `adopt_reconnected_transport`, i.e. what happens once an attempt is made.
    // The whole trigger could be deleted and the suite stayed green. Driving the
    // real loop needs a serial device, so the arithmetic is pinned directly —
    // but through the SAME function the loop calls, not a copy of it.
    //
    // DEC-266: the copy that used to live here had drifted into asserting the
    // opposite of production for `backoff == 0`. The loop's inline expression
    // was the SKIP decision (`skip_cycles == 0 || !cycle.is_multiple_of(..)`),
    // the mirror read as the ATTEMPT decision, and nothing could notice.
    use super::{
        attempts_reconnect_this_cycle as attempts_this_cycle, openfan_poll_loop_with,
        poll_attempt_failed, RECONNECT_THRESHOLD,
    };

    /// A transport whose every write panics, counting how many polls the loop
    /// issued before it gave up on polling.
    struct PanickingTransport(Arc<std::sync::atomic::AtomicU32>);

    impl SerialTransport for PanickingTransport {
        fn write_line(&mut self, _data: &str) -> Result<(), crate::error::SerialError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("blocking poll task died");
        }
        fn read_line(&mut self, _t: Duration) -> Result<String, crate::error::SerialError> {
            unreachable!("write_line panics first")
        }
    }

    #[tokio::test]
    async fn the_loop_stops_polling_once_panics_reach_the_reconnect_threshold() {
        // DEC-266. The helper test below proves `poll_attempt_failed` classifies a
        // panicked task as a failure. It does NOT prove the loop still calls it —
        // and that distinction is exactly the mirror trap this release already fell
        // into once. So drive the REAL loop.
        //
        // Reverting the loop to the pre-DEC-265 per-arm accounting (panic arm does
        // not increment) makes it poll forever: the threshold is never reached, the
        // reconnect that might fix the fault is never attempted, and this assertion
        // fails on the poll count.
        let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let reconnects = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let transport: Arc<parking_lot::Mutex<Box<dyn SerialTransport + Send>>> = Arc::new(
            parking_lot::Mutex::new(Box::new(PanickingTransport(polls.clone()))),
        );
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let r = reconnects.clone();
        let handle = tokio::spawn(openfan_poll_loop_with(
            Arc::new(StateCache::new()),
            transport,
            Duration::from_millis(1),
            Duration::from_millis(1),
            shutdown_rx,
            // Never finds anything, and never touches real hardware — the point is
            // to observe that the loop switched from polling to reconnecting.
            move |_cache, _t| {
                r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                None
            },
        ));

        // Long enough for far more than RECONNECT_THRESHOLD ticks at 1 ms.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

        let polled = polls.load(std::sync::atomic::Ordering::SeqCst);
        let tried_reconnect = reconnects.load(std::sync::atomic::Ordering::SeqCst);

        assert!(
            tried_reconnect > 0,
            "a task that panics every tick must eventually trigger a reconnect \
             attempt; it never did ({polled} polls, {tried_reconnect} reconnects) — \
             the panic outcome is not reaching the failure count"
        );
        assert!(
            polled <= RECONNECT_THRESHOLD + 2,
            "the loop kept polling ({polled} polls) past the reconnect threshold \
             of {RECONNECT_THRESHOLD} — panics are not being counted"
        );
    }

    #[test]
    fn a_panicked_poll_task_counts_toward_the_reconnect_threshold() {
        // DEC-265 regression. A blocking task that dies every tick is the one
        // failure that never self-heals, so it is the one that most needs to
        // reach the threshold — yet it used to be the only outcome that did not
        // count. The loop logged forever and never attempted the reconnect.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let join_err = rt.block_on(async {
            tokio::spawn(async { panic!("blocking poll task died") })
                .await
                .expect_err("task should have panicked")
        });
        let panicked: Result<Result<u8, crate::error::SerialError>, tokio::task::JoinError> =
            Err(join_err);
        assert!(
            poll_attempt_failed(&panicked),
            "a panicked poll task must count as a failed poll"
        );
    }

    #[test]
    fn a_good_read_clears_the_failure_count_and_a_refused_one_does_not() {
        let good: Result<Result<u8, crate::error::SerialError>, tokio::task::JoinError> = Ok(Ok(7));
        assert!(
            !poll_attempt_failed(&good),
            "a successful poll must clear the failure count, not add to it"
        );

        // DEC-266: the ordinary disconnect — an unplugged or wedged controller
        // refusing the read. Asserting only the `Ok(Ok)` half left the most
        // common failure unpinned, and `result.is_err()` is the obvious
        // "simplification" of `!matches!(result, Ok(Ok(_)))`: it keeps the panic
        // case counting, so every other test stays green, while silently
        // disarming the reconnect trigger for every real serial error.
        let refused: Result<Result<u8, crate::error::SerialError>, tokio::task::JoinError> =
            Ok(Err(crate::error::SerialError::Timeout { timeout_ms: 100 }));
        assert!(
            poll_attempt_failed(&refused),
            "a refused read must count toward the reconnect threshold"
        );
    }

    #[test]
    fn reconnect_does_not_engage_before_the_threshold() {
        // The gate is `consecutive_errors >= RECONNECT_THRESHOLD`, and it lives
        // in the loop rather than in the arithmetic — so what is pinnable here
        // is the threshold value itself. Lowering it diverts the loop into a
        // seconds-long blocking probe on every transient hiccup; raising it
        // strands a genuinely disconnected controller for longer.
        assert_eq!(
            RECONNECT_THRESHOLD, 5,
            "the reconnect threshold is a deliberate value, not an accident"
        );
        // And the first cycle that does engage attempts immediately: backoff
        // starts at 1, so a disconnect that self-heals recovers in one tick.
        assert!(attempts_this_cycle(
            RECONNECT_THRESHOLD,
            RECONNECT_THRESHOLD,
            1
        ));
    }

    #[test]
    fn the_first_cycle_past_the_threshold_attempts_immediately() {
        // backoff starts at 1, so the very first over-threshold cycle retries
        // rather than waiting — a disconnect that self-heals recovers in one tick.
        assert!(attempts_this_cycle(5, 5, 1));
    }

    #[test]
    fn backoff_skips_cycles_but_always_comes_back_around() {
        // At backoff=4 exactly every 4th cycle attempts. The property that matters
        // is that it never stops attempting: an unbounded backoff would strand a
        // reconnectable controller forever.
        let attempts: Vec<u32> = (0..12)
            .filter(|c| attempts_this_cycle(5 + c, 5, 4))
            .collect();
        assert_eq!(attempts, vec![0, 4, 8]);
    }

    #[test]
    fn backoff_is_capped_so_retries_never_stop() {
        // The loop caps both the doubling and the skip window at 30. Past the cap
        // the period stays 30 cycles rather than growing without bound.
        let mut backoff: u32 = 1;
        for _ in 0..20 {
            backoff = (backoff * 2).min(30);
        }
        assert_eq!(backoff, 30, "backoff must saturate, not overflow or grow");
        let attempts: Vec<u32> = (0..91)
            .filter(|c| attempts_this_cycle(5 + c, 5, backoff))
            .collect();
        assert_eq!(attempts, vec![0, 30, 60, 90]);
    }

    #[test]
    fn a_zero_backoff_attempts_every_cycle_instead_of_dividing_by_zero() {
        // `skip_cycles == 0 ||` is what stops `is_multiple_of(0)` being reached.
        // Not reachable from the loop's own arithmetic today (backoff starts at 1
        // and only doubles), but the guard is load-bearing if that ever changes.
        assert!(attempts_this_cycle(5, 5, 0));
    }
}
