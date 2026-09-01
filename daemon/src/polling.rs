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

/// One poll leg's sensor result.
///
/// The `Vec<PathBuf>` rides alongside a freshly discovered descriptor set and names
/// the hwmon directories that pass could not read — the chips whose absence from
/// the set is NOT evidence their sensors have vanished ([SAFETY] DEC-272 round 2).
type SensorLegResult = Result<
    (
        Option<(
            Vec<crate::hwmon::types::SensorDescriptor>,
            Vec<std::path::PathBuf>,
        )>,
        crate::hwmon::SensorReadOutcome,
    ),
    crate::error::HwmonError,
>;

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
    // DEC-294: injectable for the same reason `hwmon_root` is. The vendor gates
    // the bogus-sensor demotion, so a test that read the host's own DMI would
    // pass on this machine and fail on an ASUS one.
    dmi_root: &Path,
    interval: Duration,
    sensor_rescan: Arc<std::sync::atomic::AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) {
    use crate::health::sensor_failure::{SensorFailureTracker, TrackerEvent};
    use crate::hwmon::types::{SensorDescriptor, SensorKind};
    use crate::hwmon::SensorReadOutcome;
    use std::sync::atomic::Ordering;

    // DEC-267/269: this loop owns the interval, so it publishes it — the profile
    // engine derives its CPU-reading staleness budget from this value.
    //
    // Set HERE rather than in `main.rs` because nothing could pin the `main.rs`
    // wiring: deleting that line left the whole suite green while the atomic
    // silently kept its 1 s default, which on a slower-polling daemon under-
    // states the budget and judges a healthy-but-slow loop dead — forcing every
    // fan to NO_SENSOR_SAFE_PCT. Publishing from the loop makes the value
    // self-correcting and puts it somewhere a test can reach.
    cache.set_hwmon_poll_interval_ms(interval.as_millis() as u64);

    // DEC-294: read the board vendor ONCE here, not per discovery. It is a
    // property of the physical board and cannot change while the process runs,
    // and reading it in the loop (rather than at the `main.rs` call site) is
    // what makes the wiring reachable from a test — the identical reasoning as
    // `set_hwmon_poll_interval_ms` above, which was moved here after deleting
    // its `main.rs` line left the whole suite green.
    let board_vendor = crate::hwmon::chip_db::read_board_info_from(dmi_root).vendor;
    let hwmon_root = hwmon_root.to_path_buf();
    let headers = Arc::new(headers);
    let gpu_infos = Arc::new(gpu_infos);
    let intel_gpu_infos = Arc::new(intel_gpu_infos);
    let nouveau_gpu_infos = Arc::new(nouveau_gpu_infos);
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut consecutive_errors: u32 = 0;
    // DEC-272: the in-flight blocking read, held across ticks so a wedged one is
    // re-awaited rather than re-spawned. See the [SAFETY] note at the await site.
    type PollTickOutput = (SensorLegResult, Vec<HwmonFanState>, Vec<AmdGpuFanState>);
    let mut pending: Option<tokio::task::JoinHandle<PollTickOutput>> = None;
    // Set when the read now in flight was spawned with discovery enabled, and
    // read when that read's result is processed — which, since DEC-272, can be a
    // later tick than the one that spawned it.
    let mut rescan_requested = false;
    let mut prev_boot: Option<Duration> = None;
    let mut prev_mono: Option<Instant> = None;

    // DEC-133: cached sensor descriptor set. `Arc` so each tick's spawn_blocking
    // can borrow the set without cloning descriptor contents.
    let mut descriptors: Arc<Vec<SensorDescriptor>> = Arc::new(Vec::new());
    // [SAFETY] DEC-272 round 2: `device_id`s of chips the last discovery pass
    // could not speak for. Their cached readings are exempt from vanished-sensor
    // eviction; every other chip's are not. Sticky across non-discovery ticks,
    // which is safe BECAUSE it is per-chip — a wholesale "the list is untrusted"
    // flag could pin eviction off for the whole process, since a chip that
    // contributes no descriptors also never triggers a rediscovery.
    let mut protected_ids: Vec<String> = Vec::new();
    let mut discovered_once = false;
    // DEC-193: owns per-descriptor read-failure streaks, the re-discovery
    // throttle, and quarantine of present-but-unreadable sensors (e.g. an
    // `ath12k` WiFi temp while the radio is down) so they cannot spam the
    // journal and are surfaced as "unavailable" instead.
    let mut failure_tracker =
        SensorFailureTracker::new(crate::constants::SENSOR_READ_FAIL_REDISCOVER_STREAK);

    loop {
        tokio::select! {
            // [SAFETY] DEC-272 round 2. `biased` — shutdown is polled FIRST.
            // Unbiased, `select!` chooses randomly among ready branches, and after
            // a wedged read's freshness budget elapses BOTH are ready (the tick is
            // overdue, the shutdown flag is set). SIGTERM was therefore observed
            // after 5 s x a geometric number of rounds — measured 4.5 s, 9.5 s,
            // 4.5 s across three runs, with no bound on the tail. The daemon did
            // stop, just unpredictably slowly, and that is the window in which
            // systemd escalates to SIGKILL and the hardware is left where it lay.
            biased;
            _ = shutdown.changed() => {
                log::info!("hwmon poll loop shutting down");
                return;
            }
            _ = tick.tick() => {}
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

        // Wait for this tick's blocking read, starting one only if the previous
        // tick's is not still running.
        //
        // [SAFETY] DEC-272 (register row 01-b). `spawn_blocking` CANNOT be
        // cancelled: a timeout abandons the join, never the thread. Spawning a
        // fresh read every tick while a wedged one still holds its thread fills
        // tokio's blocking pool (512 by default) in roughly 8.5 minutes at 1 Hz,
        // and once it is full EVERY `spawn_blocking` in the process starves —
        // including the profile engine's PWM writes. That turns a stalled sensor
        // feed into a dead sole writer, so single-flight here is load-bearing
        // rather than tidiness. Re-awaiting `&mut handle` is cancel-safe, so an
        // abandoned read is picked back up where it left off, not restarted.
        let mut handle = match pending.take() {
            Some(h) => h,
            None => {
                // DEC-133/DEC-193: decide whether this tick re-runs sensor discovery.
                // The failure tracker grants a still-failing descriptor exactly one
                // re-discovery (the "did it actually unbind?" probe); once quarantined it
                // no longer asks, which is what ends the per-`threshold` re-discovery spam.
                rescan_requested = sensor_rescan.swap(false, Ordering::SeqCst);
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
                let vendor = board_vendor.clone();
                let hdrs = headers.clone();
                let gpus = gpu_infos.clone();
                let intel_gpus = intel_gpu_infos.clone();
                let nouveau_gpus = nouveau_gpu_infos.clone();
                let nvml = nvml_backend.clone();
                let descs = descriptors.clone();
                tokio::task::spawn_blocking(move || {
                    // NVIDIA NVML telemetry (opt-in, read-only, DEC-204): GPU temps merge
                    // into the sensor readings, fan states into the GPU fan set. Both are
                    // empty when NVML is disabled/absent (the default). Read once per tick.
                    // Like the sysfs reads below, these are blocking C calls with no
                    // per-call timeout: an NVIDIA driver fault could stall this tick until
                    // it returns. The existing backstops bound the blast radius (the
                    // shutdown-drain timeout still fires; and since DEC-267 the engine
                    // age-filters CPU readings, so a stall no longer leaves the thermal
                    // rule evaluating a frozen temperature forever).
                    //
                    // DEC-269 recorded that a stall here leaves the task ALIVE, so
                    // `spawn_supervised` never fires, and named a timeout wrapper
                    // around the whole blocking leg as the real fix while leaving it
                    // out of scope. DEC-272 took it: the join is now bounded by
                    // `cpu_temp_stale_after()` at the await site below, and the
                    // outstanding handle is re-awaited rather than re-spawned. So a
                    // stall here no longer stalls the LOOP — readings simply age out
                    // and the freshness filters act on that.
                    let (nvml_temps, nvml_fans) = read_nvml_states(&*nvml);
                    // Sensor leg: full discovery only when triggered; otherwise the
                    // hot path reads each cached descriptor's temp*_input file only.
                    // The read returns successes *and* failures (DEC-193) — the loop owns
                    // logging/quarantine policy, so this blocking leg stays silent.
                    let sensors: SensorLegResult = if needs_discovery {
                        crate::hwmon::discovery::discover_sensors_reporting_skips(&root, &vendor)
                            .map(|found| {
                                let mut outcome =
                                    crate::hwmon::read_sensor_values(&found.descriptors);
                                outcome.readings.extend(nvml_temps.iter().cloned());
                                (Some((found.descriptors, found.unreadable_dirs)), outcome)
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
            }
        };

        // Budget: `cpu_temp_stale_after()`, reused rather than duplicated as a new
        // constant. It is already derived from the poll interval, already clamped
        // and floored, and is by definition the instant the safety ladder stops
        // trusting a reading — so a read still running past it cannot produce a
        // value the thermal-emergency rule would act on. There is nothing left to wait for.
        let result: Result<_, tokio::task::JoinError> =
            match tokio::time::timeout(cache.cpu_temp_stale_after(), &mut handle).await {
                Ok(joined) => joined,
                Err(_) => {
                    // Hold the handle so the next tick re-awaits THIS read instead
                    // of stacking another one behind it.
                    pending = Some(handle);
                    consecutive_errors += 1;
                    if consecutive_errors <= 3 {
                        log::warn!(
                            "hwmon poll read exceeded its freshness budget — readings will \
                             age out until it returns"
                        );
                    } else if consecutive_errors == 4 {
                        log::warn!(
                            "hwmon poll read still overdue (suppressing until periodic \
                             reminder)"
                        );
                    } else if consecutive_errors.is_multiple_of(60) {
                        log::error!(
                            "hwmon poll read wedged — {consecutive_errors} consecutive \
                             ticks with no completed read; the sensor feed is frozen"
                        );
                    }
                    continue;
                }
            };

        match result {
            Ok((Ok((fresh_descriptors, outcome)), fan_states, gpu_fan_states)) => {
                consecutive_errors = 0;
                if let Some((fresh, unreadable_dirs)) = fresh_descriptors {
                    if !discovered_once || rescan_requested {
                        log::info!("Sensor discovery: {} sensor(s) cached", fresh.len());
                    }
                    // [SAFETY] DEC-272 round 2. A pass that could not read a chip
                    // yields a partial set; adopting it is fine (the readable
                    // sensors still work), but that chip's absence is not proof
                    // its sensors are GONE. Record which chips, not merely how
                    // many — see `SensorDiscovery::skipped_device_ids`.
                    // Resolve the unreadable directories against the PREVIOUS
                    // descriptor set — the one still in `descriptors` — because
                    // that is the only place the mapping from chip to sensor id
                    // survives once the chip stops enumerating.
                    let newly_protected: Vec<String> = if unreadable_dirs.is_empty() {
                        Vec::new()
                    } else {
                        descriptors
                            .iter()
                            .filter(|d| {
                                unreadable_dirs
                                    .iter()
                                    .any(|dir| std::path::Path::new(&d.input_path).starts_with(dir))
                            })
                            .map(|d| d.id.clone())
                            .collect()
                    };
                    if !newly_protected.is_empty() && protected_ids != newly_protected {
                        log::warn!(
                            "Sensor discovery could not read {} chip(s) — {} cached reading(s) \
                             are exempt from vanished-sensor eviction until a pass reads them \
                             again: {}",
                            unreadable_dirs.len(),
                            newly_protected.len(),
                            newly_protected.join(", ")
                        );
                    }
                    protected_ids = newly_protected;
                    descriptors = Arc::new(fresh);
                    discovered_once = true;
                }

                let SensorReadOutcome {
                    mut readings,
                    mut failures,
                } = outcome;

                // 294-c: a CPU reading that is individually plausible but absurd
                // beside the board is reclassified as a read FAILURE here, before
                // the tracker runs, so it takes the DEC-193 path — quarantined,
                // logged once, surfaced as `unavailable_sensors[]`, and recovered
                // by itself when the channel reads sanely again.
                //
                // Doing it HERE and not in `read_temp` is forced by the shape of
                // the test: `read_temp` sees one descriptor and cannot compare a
                // CPU channel against the board. Doing it as a *failure* and not
                // as a silent ladder-side filter is deliberate — a rejected
                // sensor the user cannot see is the bogus-low fault wearing a
                // different hat.
                let implausible =
                    crate::hwmon::plausibility::implausibly_low_cpu_readings(&readings);
                if !implausible.is_empty() {
                    let rejected: std::collections::HashSet<String> =
                        implausible.iter().map(|f| f.id.clone()).collect();
                    readings.retain(|r| !rejected.contains(&r.id));
                    failures.extend(implausible);
                }

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
                // DEC-272: the live sensor set for this tick, built BEFORE `cached`
                // moves into the cache. Union of the descriptor set and the ids
                // actually read — NVML temps (DEC-204) carry no descriptor, so
                // descriptors alone would evict every NVIDIA sensor every tick.
                let mut live: std::collections::HashSet<String> = descriptors
                    .iter()
                    .map(|d| d.id.clone())
                    .chain(cached.iter().map(|r| r.id.clone()))
                    .collect();
                cache.update_sensors(cached);
                // Sync the quarantine set into the cache: evicts any stale
                // reading for an unavailable sensor and surfaces it on
                // /status + /poll (display-only). Cheap no-op when none.
                cache.update_unavailable_sensors(failure_tracker.unavailable());
                // DEC-272 (01-c): evict readings whose sensor has VANISHED. The
                // quarantine above covers present-but-unreadable; this covers the
                // descriptor that is simply gone, which the tracker forgets and so
                // never reports — leaving its reading cached forever and keeping
                // `CpuReading::Absent` unreachable.
                //
                // [SAFETY] DEC-272 round 2 — evict on absence, EXCEPT for chips the
                // last pass could not read.
                //
                // `discover_sensors` skips a chip whose own metadata will not read,
                // so a transient sysfs failure returns Ok with that chip's sensors
                // missing. Evicting on that evidence took a live CPU sensor to
                // `CpuReading::Absent`, and `Absent` is deliberately excluded from
                // DEC-269's stale-hold (`safety_tick.rs`) — so a latched thermal
                // emergency fell from a forced 100% to NO_SENSOR_SAFE_PCT (40%) and
                // back on rediscovery: the 100/40/100 flap DEC-269 removed.
                //
                // Protection is per-chip rather than a global suspension. A global
                // one is worse than it looks: a skipped chip contributes no
                // descriptors, so it can never produce a read failure, never reaches
                // `failure_tracker.wants_rediscovery()`, and — unless it held the
                // only CpuTemp — never re-triggers a pass at all. Eviction would
                // then stay off until `/hwmon/rescan` or a restart, silently
                // re-opening row 01-c for every OTHER chip too.
                //
                // A genuinely REMOVED chip leaves no directory, is never listed as
                // unreadable, and still evicts on the very next pass.
                live.extend(protected_ids.iter().cloned());
                cache.retain_sensors(&live);
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
                // OFS-m: drop fan entries nothing has refreshed. Deliberately
                // OUTSIDE the `is_empty` guard above — a tick where *every*
                // header went unreadable produces an empty batch, and that is
                // precisely the case that used to leave the whole map frozen and
                // ageing forever.
                cache.retain_fresh_hwmon_fans(
                    interval.saturating_mul(crate::constants::HWMON_FAN_STALE_INTERVALS),
                );

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
/// adopted there is no OpenFan backend at all, which also costs the thermal
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
    // OFS-b: edge state for the short-frame log, so an incomplete frame is
    // reported once rather than at 1 Hz for as long as it persists.
    let mut short_frame_logged = false;

    loop {
        tokio::select! {
            // [SAFETY] DEC-272 round 2 — same reasoning as the hwmon loop above.
            // This leg also awaits a `spawn_blocking` reconnect/probe, so it too
            // can arrive at the select with an overdue tick AND a set stop flag
            // and pick between them at random. Bounded here by
            // `shutdown_sequence`'s per-task timeout rather than unbounded, so
            // the cost is one timeout of SIGTERM latency — still worth removing.
            biased;
            _ = shutdown.changed() => {
                log::info!("openfan poll loop shutting down");
                return;
            }
            _ = tick.tick() => {}
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
                        let uncovered = cache.update_openfan_fans(fans);
                        log::debug!("openfan poll: {count} channels updated");
                        // OFS-b: a frame that covers fewer channels than the cache
                        // already knows leaves the rest ageing, and it is NOT an
                        // error — `send_command` returned Ok, so `poll_attempt_failed`
                        // counts it a success and the reconnect ladder is not armed.
                        // That is deliberate (a short frame is not evidence the link
                        // is down, and a reconnect resets Arduino-class boards —
                        // DEC-291 rationed exactly that). Before this there was no
                        // journal evidence of it at all.
                        //
                        // Edge-triggered, like DEC-298's write-stall pair: one line
                        // when coverage breaks and one when it returns, never 1 Hz
                        // for the duration.
                        if uncovered > 0 && !short_frame_logged {
                            log::warn!(
                                "openfan poll returned {count} channels, leaving {uncovered} \
                                 known channel(s) unrefreshed — their readings will age; \
                                 the link is still answering, so no reconnect is attempted"
                            );
                            short_frame_logged = true;
                        } else if uncovered == 0 && short_frame_logged {
                            log::info!("openfan poll is covering every known channel again");
                            short_frame_logged = false;
                        }
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

    /// Write a fake Super-I/O board sensor (MbTemp) into the tempdir sysfs root.
    fn write_board_temp(root: &std::path::Path, temp_c: f64) {
        let dir = root.join("hwmon7");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), "nct6776\n").unwrap();
        fs::write(
            dir.join("temp1_input"),
            format!("{}\n", (temp_c * 1000.0) as i64),
        )
        .unwrap();
        fs::write(dir.join("temp1_label"), "SYSTIN\n").unwrap();
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
        spawn_poll_loop_with_nvml(root, Arc::new(crate::hwmon::nvml::DisabledNvml))
    }

    /// DEC-294: a path that does not exist, so `read_board_info_from` yields an
    /// empty vendor. Every pre-existing test wants exactly that — vendor-unknown
    /// means the bogus-sensor demotion cannot fire and their behaviour is
    /// unchanged. It is a fixture path, never the host's `/sys/class/dmi/id`.
    fn no_dmi() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent/dmi")
    }

    /// DEC-294: spawn with a fixture DMI directory, so the vendor the loop
    /// actually passes into discovery is observable from a test.
    fn spawn_poll_loop_with_dmi(
        root: std::path::PathBuf,
        dmi_root: std::path::PathBuf,
    ) -> PollHarness {
        spawn_poll_loop_full(root, dmi_root, Arc::new(crate::hwmon::nvml::DisabledNvml))
    }

    fn spawn_poll_loop_with_nvml(
        root: std::path::PathBuf,
        nvml: Arc<dyn crate::hwmon::nvml::NvmlBackend>,
    ) -> PollHarness {
        spawn_poll_loop_full(root, no_dmi(), nvml)
    }

    fn spawn_poll_loop_full(
        root: std::path::PathBuf,
        dmi_root: std::path::PathBuf,
        nvml: Arc<dyn crate::hwmon::nvml::NvmlBackend>,
    ) -> PollHarness {
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
                nvml,
                &root,
                &dmi_root,
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

    /// An NVML backend whose blocking read parks until released.
    ///
    /// This is the exact failure DEC-272 bounds: a driver fault stalls the
    /// blocking leg with the *task still alive*, so `spawn_supervised` never
    /// fires and nothing upstream notices.
    ///
    /// It polls a flag on a 20 ms sleep rather than parking on a channel purely
    /// so the test can release it promptly. That is not a spin loop.
    ///
    /// The self-release cap is load-bearing, not belt-and-braces. Dropping a
    /// tokio runtime BLOCKS until its outstanding blocking tasks finish, and a
    /// failing assertion skips the test's own release — so an unbounded park
    /// turns a red test into a hung one. Verified by mutation: without the cap,
    /// deleting the single-flight guard hangs the suite instead of failing it.
    const WEDGE_SELF_RELEASE: Duration = Duration::from_secs(15);

    /// An NVML backend that always reports one GPU temperature.
    struct StaticNvml;

    impl crate::hwmon::nvml::NvmlBackend for StaticNvml {
        fn read_all(&self) -> Vec<crate::hwmon::nvml::NvmlReading> {
            vec![crate::hwmon::nvml::NvmlReading {
                pci_bdf: "0000:03:00.0".into(),
                temp_c: Some(61.0),
                fan_duty_pct: None,
                fan_rpm: None,
            }]
        }

        fn devices(&self) -> Vec<crate::hwmon::nvml::NvmlDeviceIdentity> {
            Vec::new()
        }
    }

    struct WedgingNvml {
        entered: Arc<std::sync::atomic::AtomicUsize>,
        release: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::hwmon::nvml::NvmlBackend for WedgingNvml {
        fn read_all(&self) -> Vec<crate::hwmon::nvml::NvmlReading> {
            self.entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let deadline = std::time::Instant::now() + WEDGE_SELF_RELEASE;
            while !self.release.load(std::sync::atomic::Ordering::SeqCst)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Vec::new()
        }

        fn devices(&self) -> Vec<crate::hwmon::nvml::NvmlDeviceIdentity> {
            Vec::new()
        }
    }

    /// [SAFETY] DEC-272 (register row 01-b). A wedged blocking read must never be
    /// re-spawned while the previous one is still running — and bounding it must
    /// not strand the loop on the abandoned handle either.
    ///
    /// `spawn_blocking` cannot be cancelled: a timeout abandons the join, never
    /// the thread. A fresh read per tick therefore leaks one pool thread per
    /// tick, and at tokio's default 512 the pool is exhausted in roughly 8.5
    /// minutes at 1 Hz. After that EVERY `spawn_blocking` in the process starves,
    /// including the profile engine's PWM writes — so the fix for a frozen sensor
    /// feed would take out the sole writer. That is why single-flight is
    /// load-bearing rather than tidiness.
    ///
    /// Real time, NOT `start_paused`: tokio will not auto-advance virtual time
    /// while a blocking task is outstanding, which is precisely the state under
    /// test — a paused-time version of this test hangs instead of failing. Both
    /// halves share one test because the freshness budget floors at 5 s
    /// (`cpu_temp_stale_after`: interval floored at 1 s, x5), so each wedge costs
    /// real seconds and a second test would double that for no new coverage.
    #[tokio::test]
    async fn a_wedged_read_is_bounded_and_never_re_spawned() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);

        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let nvml = Arc::new(WedgingNvml {
            entered: entered.clone(),
            release: release.clone(),
        });
        let h = spawn_poll_loop_with_nvml(tmp.path().to_path_buf(), nvml);

        // Tick 1 fires at t=0 and wedges. Its budget elapses at t=5s, the next
        // tick lands at t=6s (1 s interval, `Skip`). Asserting at t=7.5s means an
        // unbounded loop has had two clear opportunities to spawn another read.
        tokio::time::sleep(Duration::from_millis(7500)).await;
        assert_eq!(
            entered.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a wedged read must be re-awaited, never re-spawned — every extra \
             spawn permanently leaks a blocking-pool thread"
        );
        assert!(
            sensor_by_label(&h.cache, "Tctl").is_none(),
            "precondition: nothing can land while the only read is wedged"
        );

        // The other half: releasing it must let the abandoned read complete and
        // polling resume, rather than leaving the loop stuck on a dead handle.
        release.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut resumed = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if sensor_by_label(&h.cache, "Tctl").is_some() {
                resumed = true;
                break;
            }
        }
        assert!(
            resumed,
            "the loop must consume the abandoned read and resume polling"
        );

        stop(h).await;
    }

    /// [SAFETY] DEC-272 round 2 — the BOUND itself, which nothing pinned.
    ///
    /// `a_wedged_read_is_bounded_and_never_re_spawned` above pins the
    /// single-flight half, but it passes with the `tokio::time::timeout` deleted
    /// entirely (measured: replace it with a plain `(&mut handle).await` and the
    /// test is still green). Its observations — `entered == 1`, no reading landing,
    /// resumption after release — are all equally true of a loop that is simply
    /// blocked on the join, so the freshness bound had no regression guard at all.
    /// That is the failure mode CLAUDE.md names: a [SAFETY] mechanism whose test
    /// measures its neighbour.
    ///
    /// The discriminator is LIVENESS. With the bound, the await returns after
    /// `cpu_temp_stale_after()` (5 s floor), the loop `continue`s, re-enters its
    /// `select!` and observes shutdown. Without it, the loop is parked inside the
    /// join and cannot see shutdown until the read itself returns — so a wedged
    /// sensor makes the daemon unstoppable, and systemd escalates to SIGKILL with
    /// the hardware left wherever it was.
    ///
    /// The wedge self-releases at `WEDGE_SELF_RELEASE` (15 s) so a failure here is
    /// a slow red, never a hung CI job.
    #[tokio::test]
    async fn a_wedged_read_does_not_make_the_loop_unstoppable() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);

        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let nvml = Arc::new(WedgingNvml {
            entered: entered.clone(),
            release: release.clone(),
        });
        let h = spawn_poll_loop_with_nvml(tmp.path().to_path_buf(), nvml);

        // Let tick 1 fire and wedge, then ask the loop to stop while it is stuck.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            entered.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "precondition: the read must actually be wedged before we signal stop"
        );
        let _ = h.shutdown_tx.send(true);

        // 9 s sits between the two outcomes: bounded exits at ~5 s (the budget),
        // unbounded cannot exit before the wedge self-releases at 15 s.
        let exited = tokio::time::timeout(Duration::from_secs(9), h.handle).await;
        // Release regardless so the blocking thread is not still running when the
        // runtime is dropped — a failed assertion must not become a hung binary.
        release.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            exited.is_ok(),
            "the poll loop must observe shutdown while a blocking read is wedged; \
             without the freshness bound it stays parked in the join and the daemon \
             can only be stopped by SIGKILL"
        );
    }

    /// [SAFETY] DEC-272 round 2 — the shutdown-first ORDERING, pinned deterministically.
    ///
    /// `a_wedged_read_does_not_make_the_loop_unstoppable` pins the freshness
    /// BOUND (measured: delete the timeout and it reds). It cannot pin `biased`,
    /// because an unbiased `select!` still exits inside the 9 s window half the
    /// time — deleting `biased` would red it on a coin flip, and a guard that
    /// fails 50% of the time is one CI re-run away from being deleted as flaky.
    ///
    /// Making it behavioural instead would cost ~5 s per repetition for a
    /// property that is one token of source. So this asserts the token, in the
    /// same spirit as the release-workflow guards in `daemon/tests/`: cheap,
    /// deterministic, and precise about what it protects. The behavioural test
    /// keeps covering the thing behaviour can actually see.
    #[test]
    fn both_poll_loops_take_shutdown_before_a_due_tick() {
        let whole = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/polling.rs"));
        // Production code only. Scanning the whole file makes this test match its
        // OWN string literals — which it did on the first run, and which would
        // have let it pass while the production selects were unbiased.
        let src = whole
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("polling.rs has a #[cfg(test)] module");

        // Every `tokio::select!` in this file is a poll loop's wait, and each must
        // resolve shutdown first: an overdue tick and a set stop flag are both
        // ready after a slow leg, and an unbiased choice between them made SIGTERM
        // latency a geometric random variable (measured 4.5 s / 9.5 s / 4.5 s).
        let selects: Vec<&str> = src
            .match_indices("tokio::select! {")
            .map(|(i, _)| &src[i..])
            .collect();
        assert!(
            selects.len() >= 2,
            "expected the hwmon and openfan poll loops to both use select!; found {}",
            selects.len()
        );
        for (n, block) in selects.iter().enumerate() {
            // Generous window: both selects carry a multi-line [SAFETY] comment
            // between the brace and the arms, and a window that clipped the last
            // arm made this test fail for the wrong reason on its first run.
            let head: String = block.lines().take(40).collect::<Vec<_>>().join("\n");
            assert!(
                head.contains("biased;"),
                "poll-loop select #{n} is not `biased`, so a due tick can win over a \
                 pending shutdown and SIGTERM latency becomes unbounded"
            );
            let biased_at = head.find("biased;").unwrap();
            let shutdown_at = head
                .find("shutdown.changed()")
                .expect("a poll-loop select must have a shutdown arm");
            let tick_at = head.find("tick.tick()").expect("...and a tick arm");
            assert!(
                biased_at < shutdown_at && shutdown_at < tick_at,
                "with `biased` the arms are polled in written order, so the shutdown \
                 arm must come FIRST — select #{n} has it after the tick, which makes \
                 `biased` actively worse than none"
            );
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

    #[tokio::test(start_paused = true)]
    async fn the_poll_loop_publishes_its_interval_for_the_staleness_budget() {
        // DEC-269. The engine derives its CPU-reading staleness budget from this
        // value, and it used to be published by a line in `main.rs` that no test
        // could reach — deleting it left the entire suite green while the budget
        // silently reverted to its 1 s default. On a daemon polling slower than
        // that, the understated budget judges a healthy loop dead and forces
        // every fan to NO_SENSOR_SAFE_PCT. Publishing from the loop puts the
        // wiring somewhere a test can stand.
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);

        let cache = Arc::new(StateCache::new());
        assert_eq!(
            cache.cpu_temp_stale_after(),
            Duration::from_secs(5),
            "precondition: the default budget, before the loop publishes anything"
        );

        let rescan = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let history = Arc::new(crate::health::history::HistoryRing::new(16));
        let (cache2, root) = (cache.clone(), tmp.path().to_path_buf());
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
                &no_dmi(),
                Duration::from_secs(4),
                rescan,
                shutdown_rx,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            cache.cpu_temp_stale_after(),
            Duration::from_secs(20),
            "the loop must publish its own interval (4 s x 5), not leave the default"
        );

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
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

    /// [SAFETY] DEC-272 (register row 01-c) — the CALL SITE, not the helper.
    ///
    /// `StateCache::retain_sensors` has its own unit test; this asserts the poll
    /// loop actually calls it. Deleting the call left that unit test green, which
    /// is the recurring failure mode CLAUDE.md names: a thoroughly tested pure
    /// helper that no production path invokes is an untested rule.
    #[tokio::test(start_paused = true)]
    async fn a_vanished_sensor_is_evicted_from_the_cache_on_rediscovery() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        write_nvme(tmp.path());
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
        assert!(
            sensor_by_label(&h.cache, "Composite").is_some(),
            "precondition: the nvme sensor was discovered"
        );

        // The device goes away entirely — a module unload, not a read failure.
        // The DEC-193 tracker FORGETS a genuinely unbound descriptor, so nothing
        // quarantines this and nothing used to evict it.
        fs::remove_dir_all(tmp.path().join("hwmon1")).unwrap();
        h.rescan.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1000)).await; // tick 2

        assert!(
            sensor_by_label(&h.cache, "Composite").is_none(),
            "a vanished sensor's reading must not linger in the cache — that is \
             what kept CpuReading::Absent unreachable"
        );
        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "eviction must be surgical: the surviving sensor stays"
        );

        stop(h).await;
    }

    /// [SAFETY] DEC-272 round 2 — a chip that cannot be ENUMERATED is not a chip
    /// that is GONE, and only the second may evict.
    ///
    /// `discover_sensors` logs and skips a chip whose own metadata read fails, so
    /// one bad chip cannot blind the daemon to the rest — but it then returns
    /// `Ok` with a *partial* set that is indistinguishable from a complete one.
    /// Row 01-c's eviction used that set as proof a sensor no longer exists, so a
    /// single transient sysfs failure on the CPU chip evicted a live Tctl. That
    /// matters far beyond a missing row on `/status`: the reading becomes
    /// `CpuReading::Absent` rather than `Stale`, and `safety_tick` deliberately
    /// excludes `Absent` from DEC-269's stale-hold — so a latched thermal emergency
    /// drops from a forced 100% to `NO_SENSOR_SAFE_PCT` (40%) and back on
    /// rediscovery. That is the 100/40/100 flap DEC-269 was written to remove,
    /// reachable from one failed read.
    ///
    /// Phase 2 pins that the suspension is TEMPORARY, not a disabling of 01-c: a
    /// later clean pass evicts a genuinely removed chip. The complementary half —
    /// that a removed directory evicts immediately — is
    /// `a_vanished_sensor_is_evicted_from_the_cache_on_rediscovery` above.
    #[tokio::test(start_paused = true)]
    async fn a_chip_that_failed_to_enumerate_is_not_treated_as_vanished() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        write_nvme(tmp.path());
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "precondition: the CPU sensor was discovered"
        );

        // Phase 1: the CPU chip's own `name` read fails. The directory is still
        // there — this is a transient sysfs error, not an unbind.
        fs::remove_file(tmp.path().join("hwmon0/name")).unwrap();
        h.rescan.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1000)).await; // tick 2

        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "a chip that merely failed to enumerate must NOT be evicted — evicting \
             it yields CpuReading::Absent, which DEC-269's stale-hold excludes, so a \
             latched thermal emergency falls from 100% to the 40% no-sensor floor"
        );

        // Phase 2: the chip read recovers AND the nvme device is genuinely removed.
        // The next pass is complete, so eviction resumes and takes the real casualty.
        fs::write(tmp.path().join("hwmon0/name"), "k10temp\n").unwrap();
        fs::remove_dir_all(tmp.path().join("hwmon1")).unwrap();
        h.rescan.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1000)).await; // tick 3

        assert!(
            sensor_by_label(&h.cache, "Composite").is_none(),
            "suspension must be temporary: a clean pass still evicts a vanished sensor"
        );
        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "the recovered CPU sensor stays"
        );

        stop(h).await;
    }

    /// [SAFETY] DEC-272 round 2 — an unreadable LABEL must not rename a sensor.
    ///
    /// The chip-name read is not the only one that can fail. `tempN_label` feeds
    /// both `build_stable_id` and the `SensorKind` classification, and it used to
    /// default to "" on a failed read — silently renaming `…:Tctl` to `…:` and,
    /// on chips that classify by label, demoting a `CpuTemp` to `MbTemp`. The chip
    /// enumerates Ok either way, so the pass looks complete and eviction drops the
    /// original id on false evidence. For a CPU sensor that is `CpuReading::Absent`
    /// and the 100/40 flap DEC-269 removed, reached one attribute below the guard
    /// that was supposed to stop it.
    ///
    /// The label is made a DIRECTORY rather than chmod 000 deliberately: a read of
    /// a directory fails with EISDIR for root too, so the fixture holds whatever
    /// user the suite runs as.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_label_does_not_rename_a_sensor() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
        let before = sensor_by_label(&h.cache, "Tctl").expect("precondition: Tctl discovered");

        // The label file becomes unreadable while still existing.
        let label = tmp.path().join("hwmon0/temp1_label");
        fs::remove_file(&label).unwrap();
        fs::create_dir(&label).unwrap();
        h.rescan.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1000)).await; // tick 2

        let after = sensor_by_label(&h.cache, "Tctl");
        assert!(
            after.is_some(),
            "an unreadable label must not evict or rename the sensor"
        );
        assert_eq!(
            after.unwrap().id,
            before.id,
            "the stable id must not change when only the label read failed"
        );
        assert_eq!(
            before.kind,
            SensorKind::CpuTemp,
            "precondition: it was classified from the label"
        );

        stop(h).await;
    }

    /// [SAFETY] DEC-272 round 2 — protection is PER-CHIP, not a global suspension.
    ///
    /// The test above skips the CPU chip, and that is the case that self-heals:
    /// a missing CpuTemp descriptor sets `cpu_temp_missing`, which re-runs
    /// discovery every tick until it comes back. A NON-CPU chip has no such
    /// trigger — it contributes no descriptors, so it can never produce a read
    /// failure, never reaches `failure_tracker.wants_rediscovery()`, and never
    /// re-triggers a pass. A global "the descriptor list is untrusted" flag would
    /// therefore latch off for the rest of the process and silently stop evicting
    /// EVERY other chip's vanished sensors — re-opening row 01-c by the back door,
    /// which is the bug this whole mechanism exists to close.
    ///
    /// So the assertion that matters is not "the unreadable chip survived" but
    /// "the unreadable chip survived AND a genuinely removed one still went",
    /// in the same pass.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_chip_does_not_stop_other_sensors_being_evicted() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        write_nvme(tmp.path());
        // A third chip, so one can be unreadable and another genuinely removed.
        let third = tmp.path().join("hwmon2");
        fs::create_dir_all(&third).unwrap();
        fs::write(third.join("name"), "acpitz\n").unwrap();
        fs::write(third.join("temp1_input"), "42000\n").unwrap();
        fs::write(third.join("temp1_label"), "Ambient\n").unwrap();

        let h = spawn_poll_loop(tmp.path().to_path_buf());
        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
        for label in ["Tctl", "Composite", "Ambient"] {
            assert!(
                sensor_by_label(&h.cache, label).is_some(),
                "precondition: {label} was discovered"
            );
        }

        // nvme: present but unreadable (transient). acpitz: genuinely removed.
        // The CPU chip stays healthy throughout, so `cpu_temp_missing` never fires
        // and this pass runs only because of the explicit rescan.
        fs::remove_file(tmp.path().join("hwmon1/name")).unwrap();
        fs::remove_dir_all(tmp.path().join("hwmon2")).unwrap();
        h.rescan.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1000)).await; // tick 2

        assert!(
            sensor_by_label(&h.cache, "Composite").is_some(),
            "a chip that merely failed to enumerate must be protected from eviction"
        );
        assert!(
            sensor_by_label(&h.cache, "Ambient").is_none(),
            "...but that protection must not extend to a chip that genuinely went \
             away in the same pass — a global suspension would keep this cached \
             forever, which is row 01-c re-opened"
        );
        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "the healthy CPU sensor is untouched"
        );

        stop(h).await;
    }

    /// [SAFETY] DEC-272 — the adjudication of register row 01-d.
    ///
    /// 01-d's concern is that `hottest_cpu_reading` lets fresh win outright, so a
    /// frozen Tctl at 106 C beside a fresh Tccd at 61 C yields `Fresh(61)` and
    /// releases the emergency. That reduction is pinned as deliberate by
    /// `a_stale_sensor_does_not_mask_a_fresh_hotter_one`; `max(fresh, stale)` is
    /// explicitly NOT the fix, because it reinstates the frozen-value-holds-the-
    /// latch bug DEC-269 removed.
    ///
    /// What makes that safe is that the frozen sibling cannot PERSIST, and this is
    /// the test for that claim. A CPU sensor that reads fine and then stops is
    /// quarantined and evicted within a bounded number of ticks, so it stops
    /// contributing to the reduce rather than sitting in it at 106 C forever.
    /// Together with `a_vanished_sensor_is_evicted_from_the_cache_on_rediscovery`
    /// (the descriptor-disappears case, which nothing used to evict at all) the
    /// window is bounded at both ends — which is why 01-d closes as NON-ISSUE
    /// rather than growing a new threshold on the safety ladder.
    ///
    /// It has to start READABLE. A sensor that never reads is never cached, so
    /// asserting it is absent afterwards would pass vacuously and prove nothing.
    #[tokio::test(start_paused = true)]
    async fn a_hot_cpu_sensor_that_stops_reading_cannot_linger_as_a_frozen_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);

        // A second CPU-chip sensor, initially readable and HOT — the frozen
        // sibling 01-d is about.
        let hot = tmp.path().join("hwmon2");
        fs::create_dir_all(&hot).unwrap();
        fs::write(hot.join("name"), "k10temp\n").unwrap();
        fs::write(hot.join("temp1_input"), "106000\n").unwrap();
        fs::write(hot.join("temp1_label"), "Tccd1\n").unwrap();

        let h = spawn_poll_loop(tmp.path().to_path_buf());
        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
        let seeded = sensor_by_label(&h.cache, "Tccd1").expect("precondition: it was discovered");
        assert!(
            (seeded.value_c - 106.0).abs() < f64::EPSILON,
            "precondition: it is cached at its hot value"
        );

        // Now it stops reading, without its descriptor going away.
        fs::write(hot.join("temp1_input"), "garbage\n").unwrap();
        let ticks = crate::constants::SENSOR_READ_FAIL_REDISCOVER_STREAK + 4;
        for _ in 0..ticks {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        assert!(
            sensor_by_label(&h.cache, "Tccd1").is_none(),
            "a frozen hot CPU sibling must be evicted within the quarantine bound, \
             not left in the reduce the thermal-emergency rule runs over"
        );
        assert!(
            sensor_by_label(&h.cache, "Tctl").is_some(),
            "the readable CPU sensor stays in service"
        );

        stop(h).await;
    }

    /// [SAFETY] DEC-272 (register row 01-c) — the trap in the eviction, pinned.
    ///
    /// NVML temperatures (DEC-204) are merged into each tick's readings but have
    /// NO entry in the descriptor set: they are minted from the driver, not
    /// discovered from sysfs. Retaining on the descriptor set alone therefore
    /// evicts every NVIDIA sensor on every single tick — the eviction would
    /// silently delete a live sensor feed. The retain set has to be the union.
    #[tokio::test(start_paused = true)]
    async fn an_nvml_sensor_survives_eviction_despite_having_no_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 55.0);
        let h = spawn_poll_loop_with_nvml(tmp.path().to_path_buf(), Arc::new(StaticNvml));

        tokio::time::sleep(Duration::from_millis(500)).await; // tick 1
        assert!(
            sensor_by_label(&h.cache, "GPU").is_some(),
            "precondition: the NVML sensor reached the cache"
        );

        // Several more ticks, each of which runs the eviction.
        tokio::time::sleep(Duration::from_millis(3000)).await;
        assert!(
            sensor_by_label(&h.cache, "GPU").is_some(),
            "an NVML sensor has no descriptor, so retaining on descriptors alone \
             would evict it every tick"
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
    /// 294-c, at the CALL SITE. The predicate has its own unit tests in
    /// `hwmon::plausibility`; this drives the real `hwmon_poll_loop` against a
    /// real sysfs fixture, so deleting the call in the loop reds it.
    ///
    /// That distinction is the point. `CLAUDE.md § Hard-won lessons` records
    /// "extracting a rule into a testable function does NOT test the call site"
    /// as having recurred five times in this project; a green predicate with a
    /// deleted call site is exactly that failure.
    ///
    /// The fixture is the documented Zen 3 fault (launchpad#1918065): a k10temp
    /// pinned at 0 C beside a board reading 45 C. Both READ fine — nothing
    /// upstream can tell them apart, which is why the rejection has to happen
    /// where both readings are visible at once.
    #[tokio::test(start_paused = true)]
    async fn poll_loop_quarantines_an_implausibly_low_cpu_sensor() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 0.0);
        write_board_temp(tmp.path(), 45.0);
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        let ticks = crate::constants::SENSOR_READ_FAIL_REDISCOVER_STREAK + 4;
        for _ in 0..ticks {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        let snap = h.cache.snapshot();
        assert!(
            snap.unavailable_sensors
                .iter()
                .any(|s| s.id.contains("k10temp")),
            "a CPU sensor pinned at 0 C beside a 45 C board must be quarantined; \
             unavailable={:?}",
            snap.unavailable_sensors
        );
        assert!(
            !snap.sensors.values().any(|s| s.chip_name == "k10temp"),
            "an implausible reading must never be served as a live CPU temperature — \
             serving it resets no_cpu_sensor_cycles and silently suppresses the \
             DEC-190 absent-sensor floor, which is the whole fault 294-c exists to fix"
        );
        assert!(
            snap.sensors.values().any(|s| s.chip_name == "nct6776"),
            "the board sensor it was judged against must stay in service — a filter \
             that quarantined the witness too would pass the assertion above for the \
             wrong reason"
        );
    }

    /// The other half of `294-c`, and the half that matters for safety: a
    /// quarantined CPU sensor must come BACK when it reads sanely again.
    ///
    /// A filter that could strand a CPU sensor out of service permanently would
    /// be a worse fault than the one it fixes — the machine would run with no CPU
    /// temperature and sit on DEC-190's 40% floor forever. Recovery works by
    /// construction (a quarantined sensor keeps its descriptor, so it is still
    /// read every tick, and a plausible reading simply never enters `failures`),
    /// but "by construction" is a claim, and this asserts it.
    ///
    /// Asserts the PRESENCE of the quarantine first, so the recovery assertion
    /// cannot pass vacuously against a sensor that was never quarantined —
    /// `CLAUDE.md § Hard-won lessons`.
    #[tokio::test(start_paused = true)]
    async fn a_quarantined_cpu_sensor_recovers_when_it_reads_sanely_again() {
        let tmp = tempfile::tempdir().unwrap();
        write_k10temp(tmp.path(), 0.0);
        write_board_temp(tmp.path(), 45.0);
        let h = spawn_poll_loop(tmp.path().to_path_buf());

        let ticks = crate::constants::SENSOR_READ_FAIL_REDISCOVER_STREAK + 4;
        for _ in 0..ticks {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        assert!(
            h.cache
                .snapshot()
                .unavailable_sensors
                .iter()
                .any(|s| s.id.contains("k10temp")),
            "precondition: the implausible sensor must actually be quarantined first"
        );

        // The channel starts reading a real temperature.
        write_k10temp(tmp.path(), 42.0);
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        let snap = h.cache.snapshot();
        assert!(
            !snap
                .unavailable_sensors
                .iter()
                .any(|s| s.id.contains("k10temp")),
            "a sensor that reads sanely again must leave the quarantine; \
             unavailable={:?}",
            snap.unavailable_sensors
        );
        assert!(
            snap.sensors.values().any(|s| s.chip_name == "k10temp"),
            "and must be served as a live CPU reading again, or the thermal ladder \
             stays blind for the life of the process"
        );
    }

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

    /// DEC-294: pins the PRODUCTION wiring, not the rule.
    ///
    /// `classify_chip` and `discover_sensors_with_vendor` are unit-tested
    /// elsewhere. Neither proves the poll loop actually *passes* a DMI-derived
    /// vendor into discovery — and if it stopped doing so (a refactor passing
    /// `""`, which `discover_sensors` still offers), the whole mitigation would
    /// be off on every real machine with the suite fully green. That is the
    /// "extracting a rule into a testable function does NOT test the call site"
    /// failure recorded in `CLAUDE.md § Hard-won lessons`, which has recurred
    /// five times; both reviewers raised it independently on this change.
    ///
    /// Asserts the PECI half too: demoting CPUTIN is only correct because the
    /// board keeps a usable CPU sensor.
    #[tokio::test]
    async fn poll_loop_passes_the_dmi_vendor_into_classification() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon0 = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon0).unwrap();
        fs::write(hwmon0.join("name"), "nct6776\n").unwrap();
        fs::write(hwmon0.join("temp1_input"), "115000\n").unwrap();
        fs::write(hwmon0.join("temp1_label"), "CPUTIN\n").unwrap();
        fs::write(hwmon0.join("temp2_input"), "45000\n").unwrap();
        fs::write(hwmon0.join("temp2_label"), "PECI Agent 0\n").unwrap();

        // A fixture DMI tree, never the host's — the rule is vendor-gated, so a
        // test reading real DMI would behave differently on an ASUS machine.
        let dmi = tempfile::tempdir().unwrap();
        fs::write(dmi.path().join("board_vendor"), "ASUSTeK COMPUTER INC.\n").unwrap();

        let h = spawn_poll_loop_with_dmi(tmp.path().to_path_buf(), dmi.path().to_path_buf());
        tokio::time::sleep(Duration::from_millis(500)).await;

        let cputin = sensor_by_label(&h.cache, "CPUTIN").expect("CPUTIN must be discovered");
        assert_eq!(
            cputin.kind,
            SensorKind::MbTemp,
            "the loop did not pass the DMI vendor into discovery — the DEC-294 \
             demotion is off on every real machine"
        );
        let peci = sensor_by_label(&h.cache, "PECI Agent 0").expect("PECI must be discovered");
        assert_eq!(
            peci.kind,
            SensorKind::CpuTemp,
            "demoting CPUTIN is only safe while the board keeps a usable CPU sensor"
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

    /// OFS-b, the decision half. A short frame is a *successful* read — the
    /// controller answered — so it must NOT arm the reconnect ladder. That was
    /// chosen deliberately over the more "correct-looking" alternative: a
    /// reconnect resets Arduino-class boards, and DEC-291 spent real effort
    /// rationing exactly that, so treating incomplete coverage as a dead link
    /// would reset the controller every time a frame came up short.
    ///
    /// The visibility this case does get is the edge-triggered log beside
    /// `update_openfan_fans`, driven by the uncovered count that method returns —
    /// never the failure counter this test pins.
    #[test]
    fn a_short_frame_is_a_successful_poll_and_never_arms_the_reconnect_ladder() {
        // A frame carrying three readings where ten were expected is still
        // `Ok(Ok(_))`: `send_command` returned, the link answered.
        let short: Result<Result<Vec<u8>, crate::error::SerialError>, tokio::task::JoinError> =
            Ok(Ok(vec![0, 1, 2]));
        assert!(
            !poll_attempt_failed(&short),
            "an incomplete frame is not a failed poll — it must not count toward \
             RECONNECT_THRESHOLD, because a reconnect resets the board (DEC-291)"
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
