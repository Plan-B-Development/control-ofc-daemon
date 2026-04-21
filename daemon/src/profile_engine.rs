//! Profile engine — headless curve evaluation loop.
//!
//! Reads sensor values from StateCache, evaluates curves from the active
//! profile, and returns PWM write commands. Runs at 1Hz alongside the
//! existing polling loops.

use parking_lot::Mutex;
use std::sync::Arc;

use crate::constants;
use crate::health::cache::StateCache;
use crate::hwmon::types::SensorKind;
use crate::profile::{evaluate_curve, DaemonProfile};
use crate::serial::protocol::NUM_CHANNELS;

/// A single PWM write command produced by the profile engine.
#[derive(Debug, Clone)]
pub struct PwmCommand {
    pub member_id: String,
    pub source: String, // "openfan" or "hwmon"
    pub pwm_percent: u8,
}

/// Evaluate the active profile against current sensor readings.
///
/// Returns a list of PWM commands for each fan member in the profile.
/// The caller is responsible for executing the writes.
pub fn evaluate_profile(profile: &DaemonProfile, cache: &StateCache) -> Vec<PwmCommand> {
    let snap = cache.snapshot();
    let mut commands = Vec::new();

    for control in &profile.controls {
        if control.members.is_empty() {
            continue;
        }

        // Determine target output percentage
        let target_pct = if control.mode == "manual" {
            control.manual_output_pct
        } else {
            // Find the assigned curve
            let curve = profile.curves.iter().find(|c| c.id == control.curve_id);
            let Some(curve) = curve else {
                log::debug!(
                    "Control '{}': curve '{}' not found, skipping",
                    control.name,
                    control.curve_id
                );
                continue;
            };

            // Find the sensor reading
            let sensor = snap.sensors.values().find(|s| s.id == curve.sensor_id);
            let Some(sensor) = sensor else {
                log::debug!(
                    "Control '{}': sensor '{}' not available, skipping",
                    control.name,
                    curve.sensor_id
                );
                continue;
            };

            // Evaluate the curve at the current temperature
            evaluate_curve(curve, sensor.value_c)
        };

        // Apply tuning: offset, minimum floor
        let mut output = target_pct + control.offset_pct;
        if output < control.minimum_pct {
            output = control.minimum_pct;
        }
        let output = output.clamp(0.0, 100.0) as u8;

        // Generate write commands for all members
        for member in &control.members {
            commands.push(PwmCommand {
                member_id: member.member_id.clone(),
                source: member.source.clone(),
                pwm_percent: output,
            });
        }
    }

    commands
}

/// Run the profile engine loop as an async task.
///
/// Evaluates the active profile at 1Hz and writes PWM commands via the
/// serial and hwmon controllers. The safety rule has already been evaluated
/// by the time this runs — if safety override is active, this loop's
/// commands will be overridden.
pub async fn profile_engine_loop(
    cache: Arc<StateCache>,
    profile: Arc<Mutex<Option<DaemonProfile>>>,
    fan_controller: Option<Arc<Mutex<crate::serial::controller::FanController>>>,
    hwmon_controller: Option<Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>>,
    gpu_infos: Vec<crate::hwmon::gpu_detect::AmdGpuInfo>,
    safety: Arc<Mutex<crate::safety::ThermalSafetyRule>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let gpu_infos = Arc::new(gpu_infos);
    let interval = std::time::Duration::from_secs(1);
    log::info!("Profile engine started (1Hz)");

    // Track GPU writes that failed — skip retry until speed changes or cooldown elapses.
    // Key: fan_id, Value: (failed_speed_pct, failure_instant)
    let mut gpu_fail_cache: std::collections::HashMap<String, (u8, std::time::Instant)> =
        std::collections::HashMap::new();
    // Track consecutive OpenFan serial write failures for P0-R2 safety alerting.
    let mut openfan_consecutive_failures: u32 = 0;

    // Track consecutive cycles with no CPU temperature sensor (P0-R1).
    // If no CpuTemp sensor is found for N cycles, force fans to a safe minimum.
    let mut no_cpu_sensor_cycles: u32 = 0;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {},
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    log::info!("Profile engine shutting down");
                    break;
                }
            }
        }

        // Evaluate thermal safety rule against the hottest CpuTemp sensor.
        // Uses the max across ALL CpuTemp sensors (AMD Tctl, Intel Package id,
        // etc.) so the safety rule works on any platform — not just AMD.
        {
            let snap = cache.snapshot();
            let hottest_cpu_c: Option<f64> = snap
                .sensors
                .values()
                .filter(|s| s.kind == SensorKind::CpuTemp)
                .map(|s| s.value_c)
                .reduce(f64::max);

            // Track cycles with no CPU temp sensor. If missing for too long,
            // force fans to a safe minimum as a defensive fallback.
            let forced_by_no_sensor = if hottest_cpu_c.is_none() {
                no_cpu_sensor_cycles += 1;
                if no_cpu_sensor_cycles == constants::NO_SENSOR_CYCLE_THRESHOLD {
                    let safe_pct = constants::NO_SENSOR_SAFE_PCT;
                    log::error!(
                        "SAFETY: No CPU temperature sensor found for {no_cpu_sensor_cycles} \
                         consecutive cycles — forcing all fans to {safe_pct}%"
                    );
                }
                no_cpu_sensor_cycles >= constants::NO_SENSOR_CYCLE_THRESHOLD
            } else {
                if no_cpu_sensor_cycles >= constants::NO_SENSOR_CYCLE_THRESHOLD {
                    log::info!("CPU temperature sensor recovered after {no_cpu_sensor_cycles} missing cycles");
                }
                no_cpu_sensor_cycles = 0;
                false
            };

            let mut safety_guard = safety.lock();
            let safety_pct = hottest_cpu_c.and_then(|temp| safety_guard.evaluate(temp));

            // Report thermal safety state to cache for diagnostics
            let thermal_state = if safety_guard.is_active() {
                "emergency"
            } else if safety_pct.is_some() {
                "recovery"
            } else {
                "normal"
            };
            cache.set_thermal_override_state(thermal_state);

            // Determine if we need a forced override (thermal emergency OR missing sensor)
            let forced_pct = safety_pct.or(if forced_by_no_sensor {
                Some(constants::NO_SENSOR_SAFE_PCT)
            } else {
                None
            });

            if let Some(forced_pct) = forced_pct {
                let reason = if let Some(temp) = hottest_cpu_c {
                    format!("CPU temp {temp:.1}°C")
                } else {
                    "no CPU temp sensor".to_string()
                };

                // Emergency override — force ALL fan backends to safety PWM

                // OpenFan channels
                if let Some(ref ctrl) = fan_controller {
                    let mut guard = ctrl.lock();
                    for ch in 0..NUM_CHANNELS {
                        if let Err(e) = guard.set_pwm(ch, forced_pct) {
                            log::error!("THERMAL SAFETY: OpenFan ch{ch} write FAILED: {e}");
                        }
                    }
                }

                // hwmon fans (auto-lease for safety writes)
                if let Some(ref ctrl) = hwmon_controller {
                    let mut guard = ctrl.lock();
                    let hdr_ids: Vec<String> =
                        guard.headers().iter().map(|h| h.id.clone()).collect();
                    let lease_id = guard
                        .lease_manager_mut()
                        .force_take_lease("thermal-safety")
                        .lease_id;
                    for hdr_id in &hdr_ids {
                        if let Err(e) = guard.set_pwm(hdr_id, forced_pct, &lease_id) {
                            log::error!("THERMAL SAFETY: hwmon {hdr_id} write FAILED: {e}");
                        }
                    }
                }

                log::warn!("Thermal safety override: forcing all fans to {forced_pct}% ({reason})");
                continue;
            }
        }

        // Get active profile — scope guard strictly to avoid !Send across .await
        let commands = {
            let profile_guard = profile.lock();
            let Some(ref active_profile) = *profile_guard else {
                continue; // No profile loaded
            };
            evaluate_profile(active_profile, &cache)
        };

        // Execute write commands — split into sync (OpenFan) and async (GPU) phases
        // to avoid holding MutexGuards across .await points.

        // Phase 1: OpenFan writes (sync, uses parking_lot mutex)
        // Skip when GUI is actively connected — the GUI's control loop drives
        // fan speed via the API, and both writing simultaneously causes
        // unnecessary serial traffic and potential PWM oscillation.
        let snap = cache.snapshot();
        let gui_active = snap
            .last_gui_write_at
            .is_some_and(|t| t.elapsed() < constants::GUI_ACTIVITY_TIMEOUT);

        if !gui_active {
            if let Some(ref ctrl) = fan_controller {
                for cmd in commands.iter().filter(|c| c.source == "openfan") {
                    let Some(ch_str) = cmd.member_id.strip_prefix("openfan:ch") else {
                        log::warn!(
                            "Profile engine: dropping openfan command with malformed member_id: {:?}",
                            cmd.member_id
                        );
                        continue;
                    };
                    let Ok(ch) = ch_str.parse::<u8>() else {
                        log::warn!(
                            "Profile engine: dropping openfan command with unparseable channel: {:?}",
                            cmd.member_id
                        );
                        continue;
                    };
                    let mut guard = ctrl.lock();
                    if let Err(e) = guard.set_pwm(ch, cmd.pwm_percent) {
                        openfan_consecutive_failures += 1;
                        log::warn!(
                            "Profile engine: OpenFan ch{ch} write failed \
                             ({openfan_consecutive_failures} consecutive): {e}"
                        );
                        if openfan_consecutive_failures == 5 {
                            log::error!(
                                "SAFETY: OpenFan serial link appears down \
                                 ({openfan_consecutive_failures} consecutive write failures)"
                            );
                        }
                    } else {
                        openfan_consecutive_failures = 0;
                    }
                }
            }
        }

        // Phase 2: GPU fan writes (async via spawn_blocking, no lease required)
        // gui_active check computed above — GUI's control loop drives fan speed
        // via the API, and both writing simultaneously causes SMU firmware churn.
        for cmd in commands.iter().filter(|c| c.source == "amd_gpu") {
            // GUI takes priority — skip profile engine GPU writes
            if gui_active {
                continue;
            }

            // Write suppression: skip if speed matches last commanded value
            if let Some(cached) = snap.gpu_fans.get(&cmd.member_id) {
                if cached.last_commanded_pct == Some(cmd.pwm_percent) {
                    continue;
                }
            }

            // Failure suppression: skip if the same speed already failed recently.
            // Prevents 1/sec journal spam when PMFW rejects the value.
            if let Some((failed_pct, failed_at)) = gpu_fail_cache.get(&cmd.member_id) {
                if *failed_pct == cmd.pwm_percent
                    && failed_at.elapsed() < constants::GPU_FAIL_COOLDOWN
                {
                    continue;
                }
            }

            if let Some(bdf) = cmd.member_id.strip_prefix("amd_gpu:") {
                if let Some(gpu) = gpu_infos.iter().find(|g| g.pci_bdf == bdf) {
                    if let Some(ref curve_path) = gpu.fan_curve_path {
                        let path = curve_path.clone();
                        let zero_rpm = gpu.fan_zero_rpm_path.clone();
                        let pct = cmd.pwm_percent;
                        let cache_ref = cache.clone();
                        let fan_id = cmd.member_id.clone();
                        let fan_id_inner = fan_id.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            match crate::hwmon::gpu_fan::set_static_speed(
                                &path,
                                zero_rpm.as_deref(),
                                pct,
                                constants::GPU_PMFW_WRITE_RETRIES,
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
                                gpu_fail_cache.remove(&fan_id);
                            }
                            _ => {
                                gpu_fail_cache
                                    .insert(fan_id, (cmd.pwm_percent, std::time::Instant::now()));
                            }
                        }
                    }
                }
            }
        }

        // Phase 3: hwmon writes (auto-lease for headless profile mode)
        // The profile engine auto-acquires the lease when writing hwmon members.
        // If the GUI holds the lease, hwmon writes are skipped (GUI has priority).
        let hwmon_cmds: Vec<_> = commands.iter().filter(|c| c.source == "hwmon").collect();
        if !hwmon_cmds.is_empty() {
            if let Some(ref ctrl) = hwmon_controller {
                let mut guard = ctrl.lock();
                // Try to get or auto-acquire a lease for the profile engine
                let lease_id = {
                    let mgr = guard.lease_manager();
                    match mgr.active_lease() {
                        Some(lease) if lease.owner_hint == "gui" => {
                            // GUI has priority — skip hwmon writes
                            None
                        }
                        Some(lease) => Some(lease.lease_id.clone()),
                        None => None, // Need to acquire
                    }
                };
                let lease_id = lease_id.or_else(|| {
                    guard
                        .lease_manager_mut()
                        .take_lease("profile-engine")
                        .ok()
                        .map(|l| l.lease_id)
                });
                if let Some(ref lid) = lease_id {
                    for cmd in hwmon_cmds {
                        match guard.set_pwm(&cmd.member_id, cmd.pwm_percent, lid) {
                            Ok(_) => {}
                            Err(e) => {
                                log::warn!("hwmon write failed for {}: {e}", cmd.member_id);
                            }
                        }
                    }
                    // Renew to keep it alive for the next cycle
                    if let Err(e) = guard.lease_manager_mut().renew_lease(lid) {
                        log::debug!("lease renewal failed (will re-acquire next cycle): {e}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::{CachedSensorReading, DeviceLabel};
    use crate::hwmon::types::SensorKind;
    use crate::profile::{ControlMember, CurveConfig, CurvePoint, LogicalControl};
    use std::time::Instant;

    fn make_profile(mode: &str, curve_type: &str, flat_pct: f64) -> DaemonProfile {
        DaemonProfile {
            id: "test".into(),
            name: "Test".into(),
            version: 3,
            description: "".into(),
            controls: vec![LogicalControl {
                id: "ctrl1".into(),
                name: "All Fans".into(),
                mode: mode.into(),
                curve_id: "c1".into(),
                manual_output_pct: 42.0,
                members: vec![ControlMember {
                    source: "openfan".into(),
                    member_id: "openfan:ch00".into(),
                    member_label: "".into(),
                }],
                step_up_pct: 100.0,
                step_down_pct: 100.0,
                offset_pct: 0.0,
                minimum_pct: 0.0,
                start_pct: 0.0,
                stop_pct: 0.0,
            }],
            curves: vec![CurveConfig {
                id: "c1".into(),
                name: "Curve".into(),
                curve_type: curve_type.into(),
                sensor_id: "cpu".into(),
                points: vec![
                    CurvePoint {
                        temp_c: 30.0,
                        output_pct: 20.0,
                    },
                    CurvePoint {
                        temp_c: 80.0,
                        output_pct: 100.0,
                    },
                ],
                start_temp_c: None,
                start_output_pct: None,
                end_temp_c: None,
                end_output_pct: None,
                flat_output_pct: Some(flat_pct),
            }],
        }
    }

    fn make_cache_with_sensor(sensor_id: &str, temp_c: f64) -> Arc<StateCache> {
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![CachedSensorReading {
            id: sensor_id.into(),
            kind: SensorKind::CpuTemp,
            label: "Tctl".into(),
            value_c: temp_c,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
        }]);
        cache
    }

    fn make_cache_with_cpu_sensor(sensor_id: &str, label: &str, temp_c: f64) -> Arc<StateCache> {
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![CachedSensorReading {
            id: sensor_id.into(),
            kind: SensorKind::CpuTemp,
            label: label.into(),
            value_c: temp_c,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
        }]);
        cache
    }

    #[test]
    fn evaluate_uses_intel_cpu_sensor() {
        // The safety sensor lookup must work with Intel "Package id 0" labels
        let cache = make_cache_with_cpu_sensor("cpu", "Package id 0", 55.0);
        let profile = make_profile("curve", "graph", 50.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert_eq!(cmds.len(), 1);
        // At 55C with graph curve, should produce 60% (same as AMD Tctl test)
        assert_eq!(cmds[0].pwm_percent, 60);
    }

    #[test]
    fn evaluate_uses_hottest_cpu_sensor() {
        // When multiple CpuTemp sensors exist, curves should see all of them.
        // The safety rule in profile_engine_loop uses the hottest — verify
        // that the cache can hold multiple CpuTemp sensors.
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![
            CachedSensorReading {
                id: "cpu_tctl".into(),
                kind: SensorKind::CpuTemp,
                label: "Tctl".into(),
                value_c: 65.0,
                source: DeviceLabel::Hwmon,
                updated_at: Instant::now(),
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
            },
            CachedSensorReading {
                id: "cpu_tccd1".into(),
                kind: SensorKind::CpuTemp,
                label: "Tccd1".into(),
                value_c: 70.0,
                source: DeviceLabel::Hwmon,
                updated_at: Instant::now(),
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
            },
        ]);
        let snap = cache.snapshot();
        let hottest: Option<f64> = snap
            .sensors
            .values()
            .filter(|s| s.kind == SensorKind::CpuTemp)
            .map(|s| s.value_c)
            .reduce(f64::max);
        assert_eq!(hottest, Some(70.0));
    }

    #[test]
    fn safety_rule_triggers_on_hottest_cpu_sensor() {
        // Verify the safety rule evaluates against the max of all CpuTemp sensors.
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![
            CachedSensorReading {
                id: "cpu_tctl".into(),
                kind: SensorKind::CpuTemp,
                label: "Tctl".into(),
                value_c: 80.0,
                source: DeviceLabel::Hwmon,
                updated_at: Instant::now(),
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
            },
            CachedSensorReading {
                id: "cpu_tccd1".into(),
                kind: SensorKind::CpuTemp,
                label: "Tccd1".into(),
                value_c: 106.0, // This one triggers safety
                source: DeviceLabel::Hwmon,
                updated_at: Instant::now(),
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
            },
        ]);
        let snap = cache.snapshot();
        let hottest = snap
            .sensors
            .values()
            .filter(|s| s.kind == SensorKind::CpuTemp)
            .map(|s| s.value_c)
            .reduce(f64::max);

        assert_eq!(hottest, Some(106.0));
        // The hottest sensor (106C) should trigger the safety rule
        let override_pct = rule.evaluate(106.0);
        assert_eq!(override_pct, Some(100));
    }

    #[test]
    fn safety_no_cpu_sensor_returns_none() {
        // When no CpuTemp sensor exists, the hottest-sensor lookup returns None.
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![CachedSensorReading {
            id: "gpu_edge".into(),
            kind: SensorKind::GpuTemp,
            label: "edge".into(),
            value_c: 85.0,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
        }]);
        let snap = cache.snapshot();
        let hottest: Option<f64> = snap
            .sensors
            .values()
            .filter(|s| s.kind == SensorKind::CpuTemp)
            .map(|s| s.value_c)
            .reduce(f64::max);
        assert!(hottest.is_none());
    }

    #[test]
    fn evaluate_manual_mode_returns_manual_pct() {
        let profile = make_profile("manual", "flat", 50.0);
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].pwm_percent, 42); // manual_output_pct
    }

    #[test]
    fn evaluate_curve_mode_uses_sensor_temp() {
        let profile = make_profile("curve", "graph", 50.0);
        let cache = make_cache_with_sensor("cpu", 55.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert_eq!(cmds.len(), 1);
        // At 55°C with 30→20%, 80→100%: (55-30)/(80-30) = 0.5, 20+0.5*80 = 60%
        assert_eq!(cmds[0].pwm_percent, 60);
        assert_eq!(cmds[0].member_id, "openfan:ch00");
    }

    #[test]
    fn evaluate_missing_sensor_skips_control() {
        let profile = make_profile("curve", "graph", 50.0);
        let cache = make_cache_with_sensor("gpu", 50.0); // wrong sensor
        let cmds = evaluate_profile(&profile, &cache);
        assert!(cmds.is_empty()); // sensor "cpu" not found
    }

    #[test]
    fn evaluate_empty_members_skips_control() {
        let mut profile = make_profile("curve", "graph", 50.0);
        profile.controls[0].members.clear();
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert!(cmds.is_empty());
    }

    #[test]
    fn evaluate_offset_and_minimum_applied() {
        let mut profile = make_profile("curve", "flat", 20.0);
        profile.controls[0].offset_pct = 10.0;
        profile.controls[0].minimum_pct = 35.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert_eq!(cmds.len(), 1);
        // flat=20, +offset=10 → 30, but minimum=35 → clamped to 35
        assert_eq!(cmds[0].pwm_percent, 35);
    }

    #[test]
    fn evaluate_output_clamped_to_100() {
        let mut profile = make_profile("curve", "flat", 95.0);
        profile.controls[0].offset_pct = 20.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert_eq!(cmds[0].pwm_percent, 100); // 95+20=115, clamped to 100
    }

    // ── Profile engine loop integration tests (T2 audit finding) ───

    // Local mock transport for integration tests — records all writes.
    // Cannot use the MockTransport from serial::transport because it is
    // private to that module's #[cfg(test)] block.
    struct LoopTestTransport {
        written: Arc<parking_lot::Mutex<Vec<String>>>,
        responses: parking_lot::Mutex<std::collections::VecDeque<String>>,
    }

    impl LoopTestTransport {
        fn new(response_count: usize) -> (Self, Arc<parking_lot::Mutex<Vec<String>>>) {
            let written = Arc::new(parking_lot::Mutex::new(Vec::new()));
            // Pre-populate with generic SetPwm ACKs (command code 02)
            let responses: std::collections::VecDeque<String> = (0..response_count)
                .map(|_| "<02|00:0400;>\r\n".to_string())
                .collect();
            (
                Self {
                    written: written.clone(),
                    responses: parking_lot::Mutex::new(responses),
                },
                written,
            )
        }
    }

    impl crate::serial::transport::SerialTransport for LoopTestTransport {
        fn write_line(&mut self, data: &str) -> Result<(), crate::error::SerialError> {
            self.written.lock().push(data.to_string());
            Ok(())
        }

        fn read_line(
            &mut self,
            _timeout: std::time::Duration,
        ) -> Result<String, crate::error::SerialError> {
            self.responses
                .lock()
                .pop_front()
                .ok_or(crate::error::SerialError::Timeout { timeout_ms: 100 })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn loop_evaluates_profile_and_writes_openfan() {
        // Set up a profile with one openfan:ch00 member and a graph curve.
        // At 55°C on (30→20%, 80→100%): output = 20 + (55-30)/(80-30)*80 = 60%
        // The loop should write SetPwm(ch0, 60%) via the mock transport.
        let cache = make_cache_with_sensor("cpu", 55.0);
        let profile = make_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        // Mock transport with enough responses for one SetPwm command
        let (transport, written) = LoopTestTransport::new(1);
        let fan_ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(500),
        );
        let fan_ctrl = Some(Arc::new(Mutex::new(fan_ctrl)));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(profile_engine_loop(
            cache,
            profile_arc,
            fan_ctrl,
            None,   // no hwmon
            vec![], // no GPU
            safety,
            shutdown_rx,
        ));

        // Sleep to let the loop's internal 1s timer fire and run one iteration.
        // With start_paused=true, this auto-advances virtual time.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        // Signal shutdown and let it process
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        // Verify a SetPwm command was written (commands start with ">02")
        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            !set_pwm_cmds.is_empty(),
            "expected at least one SetPwm command, got: {cmds:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn safety_override_forces_all_channels_to_100() {
        // CPU temp at 106°C triggers thermal safety → all 10 channels forced to 100%
        let cache = make_cache_with_sensor("cpu", 106.0);
        // Profile doesn't matter — safety override takes precedence
        let profile_arc = Arc::new(Mutex::new(None::<DaemonProfile>));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        // Need 10 responses (one per channel)
        let (transport, written) = LoopTestTransport::new(10);
        let fan_ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(500),
        );
        let fan_ctrl = Some(Arc::new(Mutex::new(fan_ctrl)));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(profile_engine_loop(
            cache,
            profile_arc,
            fan_ctrl,
            None,
            vec![],
            safety,
            shutdown_rx,
        ));

        // Let one cycle run
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        // All 10 channels should have received SetPwm at 100% (raw 255 = 0xFF)
        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert_eq!(
            set_pwm_cmds.len(),
            10,
            "expected 10 SetPwm commands (one per channel), got {}: {set_pwm_cmds:?}",
            set_pwm_cmds.len()
        );
        // Each command encodes 100% → raw 255 → hex "FF" as the last two chars
        for cmd in &set_pwm_cmds {
            let hex_value = &cmd[cmd.len() - 3..cmd.len() - 1]; // before trailing \n
            assert_eq!(
                hex_value, "FF",
                "expected raw 0xFF (100%), got {hex_value} in command {cmd:?}"
            );
        }
    }

    /// Helper to build a profile with an `amd_gpu` member instead of `openfan`.
    fn make_gpu_profile(mode: &str, curve_type: &str, flat_pct: f64) -> DaemonProfile {
        DaemonProfile {
            id: "gpu-test".into(),
            name: "GPU Test".into(),
            version: 3,
            description: "".into(),
            controls: vec![LogicalControl {
                id: "gpu_ctrl".into(),
                name: "GPU Fan".into(),
                mode: mode.into(),
                curve_id: "c1".into(),
                manual_output_pct: 50.0,
                members: vec![ControlMember {
                    source: "amd_gpu".into(),
                    member_id: "amd_gpu:0000:03:00.0".into(),
                    member_label: "RX 9070 XT".into(),
                }],
                step_up_pct: 100.0,
                step_down_pct: 100.0,
                offset_pct: 0.0,
                minimum_pct: 0.0,
                start_pct: 0.0,
                stop_pct: 0.0,
            }],
            curves: vec![CurveConfig {
                id: "c1".into(),
                name: "Curve".into(),
                curve_type: curve_type.into(),
                sensor_id: "cpu".into(),
                points: vec![
                    CurvePoint {
                        temp_c: 30.0,
                        output_pct: 20.0,
                    },
                    CurvePoint {
                        temp_c: 80.0,
                        output_pct: 100.0,
                    },
                ],
                start_temp_c: None,
                start_output_pct: None,
                end_temp_c: None,
                end_output_pct: None,
                flat_output_pct: Some(flat_pct),
            }],
        }
    }

    #[test]
    fn evaluate_gpu_member_produces_amd_gpu_command() {
        // A profile with an amd_gpu member should produce PwmCommands with
        // source="amd_gpu" and the correct member_id.
        let profile = make_gpu_profile("curve", "graph", 50.0);
        let cache = make_cache_with_sensor("cpu", 55.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].source, "amd_gpu");
        assert_eq!(cmds[0].member_id, "amd_gpu:0000:03:00.0");
        // At 55C on (30->20%, 80->100%): (55-30)/(80-30)=0.5, 20+0.5*80=60%
        assert_eq!(cmds[0].pwm_percent, 60);
    }

    #[test]
    fn evaluate_gpu_manual_mode() {
        let profile = make_gpu_profile("manual", "flat", 50.0);
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].source, "amd_gpu");
        assert_eq!(cmds[0].pwm_percent, 50); // manual_output_pct
    }

    #[test]
    fn evaluate_mixed_openfan_and_gpu_members() {
        // A profile with both openfan and amd_gpu members should produce
        // commands for each source.
        let mut profile = make_gpu_profile("curve", "graph", 50.0);
        // Add an openfan member to the same control
        profile.controls[0].members.push(ControlMember {
            source: "openfan".into(),
            member_id: "openfan:ch00".into(),
            member_label: "".into(),
        });
        let cache = make_cache_with_sensor("cpu", 55.0);
        let cmds = evaluate_profile(&profile, &cache);
        assert_eq!(cmds.len(), 2);

        let gpu_cmd = cmds.iter().find(|c| c.source == "amd_gpu").unwrap();
        let ofc_cmd = cmds.iter().find(|c| c.source == "openfan").unwrap();
        assert_eq!(gpu_cmd.member_id, "amd_gpu:0000:03:00.0");
        assert_eq!(ofc_cmd.member_id, "openfan:ch00");
        // Both should get the same output percentage
        assert_eq!(gpu_cmd.pwm_percent, ofc_cmd.pwm_percent);
        assert_eq!(gpu_cmd.pwm_percent, 60);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_exits_cleanly() {
        let cache = make_cache_with_sensor("cpu", 50.0);
        let profile_arc = Arc::new(Mutex::new(None::<DaemonProfile>));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(profile_engine_loop(
            cache,
            profile_arc,
            None, // no fan controller
            None,
            vec![],
            safety,
            shutdown_rx,
        ));

        // Immediately signal shutdown
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        // The loop must complete — not hang
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "profile engine loop did not exit on shutdown"
        );
    }
}
