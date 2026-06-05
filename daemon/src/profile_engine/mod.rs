//! Profile engine — headless curve evaluation loop.
//!
//! Reads sensor values from StateCache, evaluates curves from the active
//! profile, and returns PWM write commands. Runs at 1Hz alongside the
//! existing polling loops.

mod backends;

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

use backends::{GpuBackend, HwmonBackend, OpenFanBackend, SafetyWriteBackend, WriteBackend};

use crate::constants;
use crate::health::cache::StateCache;
use crate::hwmon::types::SensorKind;
use crate::profile::{evaluate_curve, DaemonProfile, LogicalControl};

/// A single PWM write command produced by the profile engine.
#[derive(Debug, Clone)]
pub struct PwmCommand {
    pub member_id: String,
    pub source: String, // "openfan", "hwmon", or "amd_gpu"
    pub pwm_percent: u8,
    /// For ``amd_gpu`` members only: when true, the GPU's PMFW
    /// ``fan_zero_rpm_enable`` flag is preserved while writing the curve.
    /// Comes from ``ControlMember.fan_zero_rpm`` (DEC-095). Always false
    /// for non-GPU members.
    pub gpu_fan_zero_rpm: bool,
}

/// Cross-cycle state owned by the profile engine loop.
///
/// Required by the tuning pipeline (`step_up_pct`, `step_down_pct`,
/// `start_pct`, `stop_pct`) so each cycle can rate-limit and hysteresis-gate
/// against the previous cycle's tuned output. Matches the GUI's per-target
/// `TargetState.last_output` in `control_loop.py`.
///
/// Also holds the per-control 2°C temperature deadband state so headless
/// profile mode behaves like GUI-driven mode at curve transitions
/// (DEC-096). The deadband fields mirror the GUI's
/// ``TargetState.last_commanded_pwm`` / ``last_transition_temp``.
///
/// Cleared whenever the active profile id changes or no profile is loaded,
/// mirroring the GUI's `_on_profile_changed` → `_reset_hysteresis()`.
#[derive(Debug, Default)]
pub struct ProfileEngineState {
    /// Last tuned output (pre-rounding f64) per control id.
    last_output: HashMap<String, f64>,
    /// Last raw curve output returned for the control (post-deadband).
    /// Used by the deadband to hold a stable value while temperature is
    /// drifting within ±deadband below the last transition.
    last_curve_output: HashMap<String, f64>,
    /// Temperature at which the last meaningful curve transition occurred.
    /// The deadband keeps the cached output as long as the current
    /// temperature falls within ``[t - HYSTERESIS_DEADBAND_C, t]``.
    last_transition_temp: HashMap<String, f64>,
    /// Id of the profile the current state belongs to.
    active_profile_id: Option<String>,
}

impl ProfileEngineState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current last-output for a control id (pre-rounding, pre-u8 conversion).
    pub fn last_output(&self, control_id: &str) -> Option<f64> {
        self.last_output.get(control_id).copied()
    }

    /// Last curve output (post-deadband, pre-tuning) for a control id.
    /// Exposed for tests so the deadband behaviour can be inspected.
    pub fn last_curve_output(&self, control_id: &str) -> Option<f64> {
        self.last_curve_output.get(control_id).copied()
    }

    /// Last temperature at which a curve transition was recorded for the
    /// control. Useful to verify the deadband anchor moves correctly.
    pub fn last_transition_temp(&self, control_id: &str) -> Option<f64> {
        self.last_transition_temp.get(control_id).copied()
    }

    /// Reset state to a profile-less state (call when active profile is
    /// cleared). The next `evaluate_profile` call starts fresh.
    pub fn deactivate(&mut self) {
        self.last_output.clear();
        self.last_curve_output.clear();
        self.last_transition_temp.clear();
        self.active_profile_id = None;
    }

    /// Clear last_output if the profile id changed since the previous call.
    ///
    /// Returns `true` when state was cleared. Used by `evaluate_profile` so
    /// swapping between profiles doesn't carry control-specific tuning state
    /// across unrelated curve definitions.
    fn sync_profile_id(&mut self, new_id: &str) -> bool {
        let changed = self.active_profile_id.as_deref() != Some(new_id);
        if changed {
            self.last_output.clear();
            self.last_curve_output.clear();
            self.last_transition_temp.clear();
            self.active_profile_id = Some(new_id.to_string());
        }
        changed
    }
}

/// Threshold (percent) below which a curve-output change does not move the
/// deadband transition anchor. Matches the GUI's `0.5` constant in
/// ``_evaluate_curve_with_hysteresis``.
const DEADBAND_ANCHOR_DELTA_PCT: f64 = 0.5;

/// Evaluate a curve with the 2°C falling-temperature deadband applied.
///
/// Returns the cached previous curve output when current temperature has
/// fallen within the deadband below the last transition anchor; otherwise
/// re-interpolates the curve and updates the anchor. Side-effects on
/// ``ProfileEngineState`` are scoped to per-control state so unrelated
/// controls are unaffected.
fn evaluate_curve_with_deadband(
    control: &LogicalControl,
    curve: &crate::profile::CurveConfig,
    current_temp: f64,
    state: &mut ProfileEngineState,
) -> f64 {
    let prev_pwm = state.last_curve_output.get(&control.id).copied();
    let prev_transition = state.last_transition_temp.get(&control.id).copied();

    if let (Some(prev_out), Some(anchor)) = (prev_pwm, prev_transition) {
        if current_temp <= anchor && current_temp >= anchor - constants::HYSTERESIS_DEADBAND_C {
            // Inside the deadband — hold the previously commanded output.
            // Do not move the anchor; do not update last_curve_output.
            return prev_out;
        }
    }

    let curve_output = evaluate_curve(curve, current_temp).clamp(0.0, 100.0);

    // Move the transition anchor only when the new curve output meaningfully
    // differs from the last one — keeps the deadband stationary as the curve
    // glides through small interpolation deltas (matches GUI parity).
    let move_anchor = prev_pwm
        .map(|p| (curve_output - p).abs() >= DEADBAND_ANCHOR_DELTA_PCT)
        .unwrap_or(true);
    if move_anchor {
        state
            .last_transition_temp
            .insert(control.id.clone(), current_temp);
    }
    state
        .last_curve_output
        .insert(control.id.clone(), curve_output);

    curve_output
}

/// Apply the full per-control tuning pipeline.
///
/// Mirrors `ControlLoopService._apply_tuning` in the GUI so headless profile
/// mode produces the same output as GUI-driven mode for identical inputs.
/// Order matters: step-rate limiting runs AFTER offset/minimum so the
/// delta tracked cycle-to-cycle is the final clamped output; stop-threshold
/// comes after step-rate so a slow-falling curve can still snap to zero.
fn apply_tuning(control: &LogicalControl, raw_output: f64, last_output: Option<f64>) -> f64 {
    apply_tuning_with_floor(control, raw_output, last_output, control.minimum_pct)
}

/// `apply_tuning` with an explicit minimum-floor override.
///
/// DEC-119: GPU members carry no soft floor (`floor == 0.0`) even inside a
/// mixed control whose `minimum_pct` is non-zero, mirroring the GUI's
/// `member_minimum_pct`. Every other member passes `control.minimum_pct`, so
/// the public `apply_tuning` (and its tests) is unchanged. Keeping this a
/// floor parameter — rather than special-casing GPU inside the pipeline —
/// preserves the exact offset → floor → step → stop/start order for both.
fn apply_tuning_with_floor(
    control: &LogicalControl,
    raw_output: f64,
    last_output: Option<f64>,
    floor: f64,
) -> f64 {
    // 1. Offset
    let mut output = raw_output + control.offset_pct;

    // 2. Minimum floor (per-profile soft floor, distinct from daemon safety)
    if output < floor {
        output = floor;
    }

    // 3. Step-rate limiting — only bites when we have a previous cycle's output.
    //    step_up_pct / step_down_pct are per-cycle caps (1Hz here).
    if let Some(last) = last_output {
        let max_up = last + control.step_up_pct;
        let max_down = last - control.step_down_pct;
        output = output.clamp(max_down, max_up);
    }

    // 4. Stop threshold — snap to zero below stop_pct so the fan actually
    //    stops instead of spinning at a near-stall speed. `stop_pct == 0`
    //    disables the feature (matches GUI semantics).
    if control.stop_pct > 0.0 && output < control.stop_pct {
        output = 0.0;
    }

    // 5. Start threshold — when transitioning from stopped (previous cycle = 0)
    //    back to non-zero, jump up to at least `start_pct` so the fan actually
    //    spins up instead of stalling at a too-low PWM. Matches the GUI's
    //    guard: only triggers on the 0 → non-zero transition.
    if output > 0.0 && matches!(last_output, Some(prev) if prev == 0.0) && control.start_pct > 0.0 {
        output = output.max(control.start_pct);
    }

    // 6. Final clamp to the hardware range.
    output.clamp(0.0, 100.0)
}

/// Evaluate the active profile against current sensor readings.
///
/// Returns a list of PWM commands for each fan member in the profile.
/// The caller is responsible for executing the writes. `engine_state` holds
/// per-control cross-cycle state required by the tuning pipeline.
pub fn evaluate_profile(
    profile: &DaemonProfile,
    cache: &StateCache,
    engine_state: &mut ProfileEngineState,
) -> Vec<PwmCommand> {
    engine_state.sync_profile_id(&profile.id);

    let sensors = cache.sensors_snapshot();
    let mut commands = Vec::new();

    for control in &profile.controls {
        if control.members.is_empty() {
            continue;
        }

        // Determine target output percentage
        let raw_output = if control.mode == "manual" {
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
            let sensor = sensors.values().find(|s| s.id == curve.sensor_id);
            let Some(sensor) = sensor else {
                log::debug!(
                    "Control '{}': sensor '{}' not available, skipping",
                    control.name,
                    curve.sensor_id
                );
                continue;
            };

            // 2°C hysteresis deadband — DEC-096. While current temperature
            // has fallen ≤ HYSTERESIS_DEADBAND_C below the last transition
            // anchor, hold the previous curve output. Mirrors the GUI's
            // ``_evaluate_curve_with_hysteresis`` (control_loop.py) so
            // headless behaviour matches GUI-driven behaviour.
            evaluate_curve_with_deadband(control, curve, sensor.value_c, engine_state)
        };

        // Full tuning pipeline — tracks pre-rounding f64 across cycles so
        // step_up_pct / step_down_pct don't drift from integer quantisation.
        let prev = engine_state.last_output(&control.id);
        let tuned = apply_tuning(control, raw_output, prev);
        engine_state.last_output.insert(control.id.clone(), tuned);

        // Round-to-nearest when converting to the wire PWM value so 49.6
        // becomes 50 instead of being truncated to 49 — matches the GUI's
        // `round(pwm_percent)` in `_write_target`.
        let pwm_percent = tuned.round().clamp(0.0, 100.0) as u8;

        // Generate write commands for all members
        for member in &control.members {
            let gpu_fan_zero_rpm = member.source == "amd_gpu" && member.fan_zero_rpm;
            // DEC-119: GPU members are never soft-floored. In a mixed control
            // (non-zero `minimum_pct`) recompute the GPU member's output with a
            // 0% floor and its own namespaced step-rate tracker, so headless
            // mode matches the GUI's per-member flooring (the DEC-096
            // GUI/headless consistency guarantee). Every non-GPU member, and
            // any member of a 0-floor control, uses the control-wide value.
            let member_pwm = if member.source == "amd_gpu" && control.minimum_pct > 0.0 {
                let key = format!("{}::m::{}", control.id, member.member_id);
                let prev_member = engine_state.last_output(&key);
                let tuned_member = apply_tuning_with_floor(control, raw_output, prev_member, 0.0);
                engine_state.last_output.insert(key, tuned_member);
                tuned_member.round().clamp(0.0, 100.0) as u8
            } else {
                pwm_percent
            };
            commands.push(PwmCommand {
                member_id: member.member_id.clone(),
                source: member.source.clone(),
                pwm_percent: member_pwm,
                gpu_fan_zero_rpm,
            });
        }
    }

    commands
}

/// Outcome of one safety-tick evaluation (pure decision, unit-testable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SafetyDecision {
    /// `"normal"` | `"recovery"` | `"emergency"` | `"no_sensor_fallback"` —
    /// reported to the cache and surfaced via `GET /status` (DEC-132) and
    /// `/diagnostics/hardware`.
    pub(crate) thermal_state: &'static str,
    /// When `Some`, every OpenFan channel + hwmon header is forced to this
    /// PWM this tick. GPU fans are excluded by design (DEC-130).
    pub(crate) forced_pct: Option<u8>,
}

/// Evaluate the thermal safety rule + no-CPU-sensor fallback for one tick.
///
/// Owns the no-sensor counter transitions (threshold-edge logging included)
/// and the thermal-state classification, separated from controller I/O so
/// the 105/80/60 ladder and the 5-cycle fallback are unit-testable (DEC-135).
fn evaluate_safety_tick(
    hottest_cpu_c: Option<f64>,
    no_cpu_sensor_cycles: &mut u32,
    safety: &mut crate::safety::ThermalSafetyRule,
) -> SafetyDecision {
    // Track cycles with no CPU temp sensor. If missing for too long, force
    // fans to a safe minimum as a defensive fallback (P0-R1).
    let forced_by_no_sensor = if hottest_cpu_c.is_none() {
        *no_cpu_sensor_cycles += 1;
        let n = *no_cpu_sensor_cycles;
        if n == constants::NO_SENSOR_CYCLE_THRESHOLD {
            let safe_pct = constants::NO_SENSOR_SAFE_PCT;
            log::error!(
                "SAFETY: No CPU temperature sensor found for {n} \
                 consecutive cycles — forcing all OpenFan+hwmon fans to {safe_pct}%"
            );
        }
        n >= constants::NO_SENSOR_CYCLE_THRESHOLD
    } else {
        let n = *no_cpu_sensor_cycles;
        if n >= constants::NO_SENSOR_CYCLE_THRESHOLD {
            log::info!("CPU temperature sensor recovered after {n} missing cycles");
        }
        *no_cpu_sensor_cycles = 0;
        false
    };

    let safety_pct = hottest_cpu_c.and_then(|temp| safety.evaluate(temp));

    let thermal_state = if safety.is_active() {
        "emergency"
    } else if safety_pct.is_some() {
        "recovery"
    } else if forced_by_no_sensor {
        // DEC-132: the no-sensor fallback also forces PWM (and force-takes
        // the hwmon lease), so the GUI must stand down here too — surface a
        // distinct state rather than claiming "normal".
        "no_sensor_fallback"
    } else {
        "normal"
    };

    let forced_pct = safety_pct.or(if forced_by_no_sensor {
        Some(constants::NO_SENSOR_SAFE_PCT)
    } else {
        None
    });

    SafetyDecision {
        thermal_state,
        forced_pct,
    }
}

/// Run the profile engine loop as an async task.
///
/// One tick per second: safety evaluation (forced overrides short-circuit
/// the tick) → profile evaluation → one `apply` per write backend. All
/// per-backend gating lives in [`backends`] (DEC-135).
pub async fn profile_engine_loop(
    cache: Arc<StateCache>,
    profile: Arc<Mutex<Option<DaemonProfile>>>,
    fan_controller: Option<Arc<Mutex<crate::serial::controller::FanController>>>,
    hwmon_controller: Option<Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>>,
    gpu_infos: Vec<crate::hwmon::gpu_detect::AmdGpuInfo>,
    safety: Arc<Mutex<crate::safety::ThermalSafetyRule>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let interval = std::time::Duration::from_secs(1);
    log::info!("Profile engine started (1Hz)");

    // Per-backend write paths (DEC-135). Each backend owns its own gating —
    // GUI deferral, coalescing thresholds, failure caching, lease handling.
    // GpuBackend is deliberately NOT a SafetyWriteBackend (DEC-130): there
    // is no GPU emergency threshold.
    let mut openfan_be = fan_controller.map(OpenFanBackend::new);
    let mut gpu_be = GpuBackend::new(cache.clone(), Arc::new(gpu_infos));
    let mut hwmon_be = hwmon_controller.map(HwmonBackend::new);

    // Track consecutive cycles with no CPU temperature sensor (P0-R1).
    // If no CpuTemp sensor is found for N cycles, force fans to a safe minimum.
    let mut no_cpu_sensor_cycles: u32 = 0;

    // Cross-cycle tuning state for `evaluate_profile`. Cleared when the active
    // profile changes or is deactivated so step-rate limiting and start/stop
    // hysteresis don't leak between unrelated profiles.
    let mut engine_state = ProfileEngineState::new();

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

        // Evaluate thermal safety against the hottest CpuTemp sensor — the
        // max across ALL CpuTemp sensors (AMD Tctl, Intel Package id, etc.)
        // so the rule works on any platform, not just AMD — plus the
        // no-CPU-sensor fallback.
        let (decision, hottest_cpu_c) = {
            let sensors = cache.sensors_snapshot();
            let hottest_cpu_c: Option<f64> = sensors
                .values()
                .filter(|s| s.kind == SensorKind::CpuTemp)
                .map(|s| s.value_c)
                .reduce(f64::max);

            let mut safety_guard = safety.lock();
            let decision =
                evaluate_safety_tick(hottest_cpu_c, &mut no_cpu_sensor_cycles, &mut safety_guard);
            (decision, hottest_cpu_c)
        };

        // Report thermal safety state for /status (DEC-132) + /diagnostics.
        cache.set_thermal_override_state(decision.thermal_state);

        if let Some(forced_pct) = decision.forced_pct {
            // Forced safety override — all OpenFan channels and writable
            // hwmon headers. GPU fans are deliberately excluded (DEC-130):
            // AMD PMFW firmware owns GPU thermal protection (junction-temp
            // throttle, firmware fan ramp) independently of OS fan control,
            // and forcing PMFW curve commits from a CPU emergency would add
            // SMU churn without improving GPU safety. There is no GPU
            // emergency threshold; the exclusion is structural — GpuBackend
            // does not implement SafetyWriteBackend.
            if let Some(be) = openfan_be.as_mut() {
                be.force_all(forced_pct);
            }
            if let Some(be) = hwmon_be.as_mut() {
                be.force_all(forced_pct);
            }

            let reason = match hottest_cpu_c {
                Some(temp) => format!("CPU temp {temp:.1}°C"),
                None => "no CPU temp sensor".to_string(),
            };
            log::warn!(
                "Thermal safety override: forcing all OpenFan+hwmon fans to \
                 {forced_pct}% ({reason})"
            );
            // P3-2: drop cross-cycle tuning state so post-override
            // evaluation starts fresh instead of step-rate-clamping from a
            // pre-emergency anchor — the fans are physically at
            // `forced_pct`, not at the stale `last_output`.
            engine_state.deactivate();
            continue;
        }

        // Get active profile — scope guard strictly to avoid !Send across .await
        let commands = {
            let profile_guard = profile.lock();
            let Some(ref active_profile) = *profile_guard else {
                // No profile loaded — drop any leftover tuning state so a
                // later activation doesn't pick up stale cross-cycle outputs.
                engine_state.deactivate();
                continue;
            };
            evaluate_profile(active_profile, &cache, &mut engine_state)
        };

        // Apply per backend (DEC-135). Each backend owns its own gating and
        // none holds a controller guard across an .await. `gui_active` is
        // read once per tick — a cheap bool instead of the full snapshot the
        // old inline phases cloned every cycle.
        let gui_active = cache.gui_active();
        if let Some(be) = openfan_be.as_mut() {
            be.apply(&commands, gui_active).await;
        }
        gpu_be.apply(&commands, gui_active).await;
        if let Some(be) = hwmon_be.as_mut() {
            be.apply(&commands, gui_active).await;
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
                    fan_zero_rpm: false,
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
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
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
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }]);
        cache
    }

    // ─── Cross-stack evaluator parity (DEC-126) ──────────────────────────
    // Loads the canonical fixture shared byte-identically with the GUI
    // (control-ofc-gui/tests/fixtures/parity_vectors.json) and asserts the same
    // hand-authored oracle the GUI checks in tests/test_evaluator_parity.py.
    // Agreement on both sides pins headless and GUI-driven evaluation together.
    fn load_parity_vectors() -> serde_json::Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/parity_vectors.json"
        );
        let text =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read parity fixture: {e}"));
        serde_json::from_str(&text).expect("parse parity fixture")
    }

    #[test]
    fn parity_curve_eval_matches_oracle() {
        let vectors = load_parity_vectors();
        for case in vectors["curve_eval"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let curve: CurveConfig = serde_json::from_value(case["curve"].clone()).expect("curve");
            let temp = case["temp"].as_f64().unwrap();
            let expected = case["expected_pct"].as_f64().unwrap();
            let got = evaluate_curve(&curve, temp);
            assert!(
                (got - expected).abs() < 0.01,
                "curve_eval[{name}]: got {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn parity_tuning_sequence_matches_oracle() {
        use std::collections::HashMap;
        let vectors = load_parity_vectors();
        for vector in vectors["tuning_sequence"].as_array().unwrap() {
            let name = vector["name"].as_str().unwrap();
            let sensor_id = vector["sensor_id"].as_str().unwrap();
            let profile: DaemonProfile =
                serde_json::from_value(vector["profile"].clone()).expect("profile");
            let mut state = ProfileEngineState::new();

            let mut produced: HashMap<String, Vec<u8>> = HashMap::new();
            for temp in vector["temps"].as_array().unwrap() {
                let cache = make_cache_with_sensor(sensor_id, temp.as_f64().unwrap());
                for cmd in evaluate_profile(&profile, &cache, &mut state) {
                    produced
                        .entry(cmd.member_id)
                        .or_default()
                        .push(cmd.pwm_percent);
                }
            }

            for member in vector["expected"].as_array().unwrap() {
                let mid = member["member_id"].as_str().unwrap();
                let expected: Vec<u8> = member["pwm"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap() as u8)
                    .collect();
                assert_eq!(
                    produced.get(mid).map(|v| v.as_slice()),
                    Some(expected.as_slice()),
                    "tuning_sequence[{name}] / {mid}"
                );
            }
        }
    }

    #[test]
    fn evaluate_uses_intel_cpu_sensor() {
        // The safety sensor lookup must work with Intel "Package id 0" labels
        let cache = make_cache_with_cpu_sensor("cpu", "Package id 0", 55.0);
        let profile = make_profile("curve", "graph", 50.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
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
                chip_name: "k10temp".into(),
                temp_type: None,
                thresholds: None,
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
                chip_name: "k10temp".into(),
                temp_type: None,
                thresholds: None,
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
                chip_name: "k10temp".into(),
                temp_type: None,
                thresholds: None,
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
                chip_name: "k10temp".into(),
                temp_type: None,
                thresholds: None,
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
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
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
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].pwm_percent, 42); // manual_output_pct
    }

    #[test]
    fn evaluate_curve_mode_uses_sensor_temp() {
        let profile = make_profile("curve", "graph", 50.0);
        let cache = make_cache_with_sensor("cpu", 55.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert_eq!(cmds.len(), 1);
        // At 55°C with 30→20%, 80→100%: (55-30)/(80-30) = 0.5, 20+0.5*80 = 60%
        assert_eq!(cmds[0].pwm_percent, 60);
        assert_eq!(cmds[0].member_id, "openfan:ch00");
    }

    #[test]
    fn evaluate_missing_sensor_skips_control() {
        let profile = make_profile("curve", "graph", 50.0);
        let cache = make_cache_with_sensor("gpu", 50.0); // wrong sensor
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert!(cmds.is_empty()); // sensor "cpu" not found
    }

    #[test]
    fn evaluate_empty_members_skips_control() {
        let mut profile = make_profile("curve", "graph", 50.0);
        profile.controls[0].members.clear();
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert!(cmds.is_empty());
    }

    #[test]
    fn evaluate_offset_and_minimum_applied() {
        let mut profile = make_profile("curve", "flat", 20.0);
        profile.controls[0].offset_pct = 10.0;
        profile.controls[0].minimum_pct = 35.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert_eq!(cmds.len(), 1);
        // flat=20, +offset=10 → 30, but minimum=35 → clamped to 35
        assert_eq!(cmds[0].pwm_percent, 35);
    }

    #[test]
    fn evaluate_output_clamped_to_100() {
        let mut profile = make_profile("curve", "flat", 95.0);
        profile.controls[0].offset_pct = 20.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert_eq!(cmds[0].pwm_percent, 100); // 95+20=115, clamped to 100
    }

    // ── DEC-119: per-member GPU floor (headless ⇄ GUI consistency) ──────

    fn push_gpu_member(profile: &mut DaemonProfile) {
        profile.controls[0].members.push(ControlMember {
            source: "amd_gpu".into(),
            member_id: "amd_gpu:0000:03:00.0".into(),
            member_label: "9070XT Fan".into(),
            fan_zero_rpm: false,
        });
    }

    #[test]
    fn evaluate_gpu_member_not_floored_in_mixed_control() {
        // A GPU fan grouped with a chassis fan in one control. The chassis
        // member keeps the 20% control floor; the GPU member idles to its own
        // 0% floor in the same cycle. Mirrors the GUI control loop (DEC-096
        // consistency).
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].manual_output_pct = 10.0;
        profile.controls[0].minimum_pct = 20.0;
        push_gpu_member(&mut profile);
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());

        let openfan = cmds.iter().find(|c| c.source == "openfan").unwrap();
        let gpu = cmds.iter().find(|c| c.source == "amd_gpu").unwrap();
        assert_eq!(openfan.pwm_percent, 20); // floored at the control minimum
        assert_eq!(gpu.pwm_percent, 10); // GPU follows the value down past 20
    }

    #[test]
    fn evaluate_gpu_member_reaches_zero_in_mixed_control() {
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].manual_output_pct = 0.0;
        profile.controls[0].minimum_pct = 30.0;
        push_gpu_member(&mut profile);
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());

        let gpu = cmds.iter().find(|c| c.source == "amd_gpu").unwrap();
        let openfan = cmds.iter().find(|c| c.source == "openfan").unwrap();
        assert_eq!(gpu.pwm_percent, 0);
        assert_eq!(openfan.pwm_percent, 30);
    }

    #[test]
    fn evaluate_gpu_only_control_uses_control_output() {
        // A GPU-only control has minimum_pct 0, so the per-member branch is not
        // taken and the (already 0-floored) control-wide value is used.
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].manual_output_pct = 5.0;
        profile.controls[0].minimum_pct = 0.0;
        profile.controls[0].members = vec![ControlMember {
            source: "amd_gpu".into(),
            member_id: "amd_gpu:0000:03:00.0".into(),
            member_label: "".into(),
            fan_zero_rpm: false,
        }];
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].pwm_percent, 5);
    }

    #[test]
    fn evaluate_non_gpu_mixed_members_share_control_floor() {
        // Regression guard: a control with no GPU member is wholly unchanged —
        // every member gets the single control-wide floored output.
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].manual_output_pct = 10.0;
        profile.controls[0].minimum_pct = 20.0;
        profile.controls[0].members.push(ControlMember {
            source: "hwmon".into(),
            member_id: "hwmon:nct6799:0000:pwm2:Chassis".into(),
            member_label: "Chassis".into(),
            fan_zero_rpm: false,
        });
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert_eq!(cmds.len(), 2);
        assert!(cmds.iter().all(|c| c.pwm_percent == 20));
    }

    // ── M1: full tuning pipeline — step rate, start/stop, cross-cycle state ──

    #[test]
    fn tuning_step_up_rate_limits_large_jump() {
        // curve output jumps 30 → 80, step_up=10 → engine should only allow +10/cycle.
        // Bump temperature each cycle so the 2°C deadband releases — real
        // operation always has temperature drift.
        let mut profile = make_profile("curve", "flat", 30.0);
        profile.controls[0].step_up_pct = 10.0;
        profile.controls[0].step_down_pct = 100.0;
        let mut state = ProfileEngineState::new();

        // Cycle 1: no prior output → curve value passes through → 30
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 50.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 30);

        // Curve jumps to 80 (simulate by rebuilding profile)
        profile.curves[0].flat_output_pct = Some(80.0);

        // Cycle 2: temp rose, deadband releases, step_up caps the increase at +10 → 40
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 51.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 40);

        // Cycle 3: another +10 → 50
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 52.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 50);
    }

    #[test]
    fn tuning_step_down_rate_limits_large_drop() {
        let mut profile = make_profile("curve", "flat", 80.0);
        profile.controls[0].step_up_pct = 100.0;
        profile.controls[0].step_down_pct = 15.0;
        let mut state = ProfileEngineState::new();

        // Cycle 1: 80
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 50.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 80);

        // Drop curve to 20
        profile.curves[0].flat_output_pct = Some(20.0);

        // Cycle 2: temp rose so the deadband releases — step_down caps at -15 → 65
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 53.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 65);
    }

    #[test]
    fn tuning_stop_threshold_snaps_to_zero() {
        // Flat curve at 15%, stop_pct=20 → snapped to 0
        let mut profile = make_profile("curve", "flat", 15.0);
        profile.controls[0].stop_pct = 20.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        let cmds = evaluate_profile(&profile, &cache, &mut state);
        assert_eq!(cmds[0].pwm_percent, 0);
        assert_eq!(state.last_output("ctrl1"), Some(0.0));
    }

    #[test]
    fn tuning_start_threshold_jumps_from_zero() {
        // Previous cycle was stopped (below stop_pct). Next cycle curve says
        // a small non-zero value → start_pct should kick the fan to spin-up.
        let mut profile = make_profile("curve", "flat", 10.0);
        profile.controls[0].stop_pct = 20.0;
        profile.controls[0].start_pct = 35.0;
        // Step rate must NOT bite on the 0→start transition, else start_pct
        // gets clamped back down. GUI parity: start_pct applies after step-rate.
        profile.controls[0].step_up_pct = 100.0;
        let mut state = ProfileEngineState::new();

        // Cycle 1: 10% < stop_pct → snap to 0
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 50.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 0);

        // Curve now says 25% (above stop_pct so not snapped; start hysteresis kicks in).
        // Bump temperature so the deadband releases and the new curve output is seen.
        profile.curves[0].flat_output_pct = Some(25.0);
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 51.0), &mut state);
        // Without start_pct it would be 25; with start_pct=35 from 0 → clamped up to 35
        assert_eq!(cmds[0].pwm_percent, 35);
    }

    #[test]
    fn tuning_state_persists_across_cycles() {
        let mut profile = make_profile("curve", "flat", 50.0);
        profile.controls[0].step_up_pct = 5.0;
        profile.controls[0].step_down_pct = 5.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        // Three cycles on the same curve — output should be identical and
        // state.last_output should reflect the tuned value.
        for _ in 0..3 {
            let cmds = evaluate_profile(&profile, &cache, &mut state);
            assert_eq!(cmds[0].pwm_percent, 50);
        }
        assert_eq!(state.last_output("ctrl1"), Some(50.0));
    }

    #[test]
    fn tuning_state_cleared_on_profile_id_change() {
        // Profile A leaves last_output=80. Swapping to profile B with a
        // different id should discard A's state so B's first cycle evaluates
        // freely without A's rate-limit anchor.
        let profile_a = make_profile("curve", "flat", 80.0);
        let mut profile_b = make_profile("curve", "flat", 30.0);
        profile_b.id = "other".into();
        profile_b.controls[0].step_down_pct = 5.0; // would clamp if stale anchor persisted
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        let cmds = evaluate_profile(&profile_a, &cache, &mut state);
        assert_eq!(cmds[0].pwm_percent, 80);
        assert_eq!(state.last_output("ctrl1"), Some(80.0));

        // Profile id changes → state cleared → step_down_pct no longer bites
        let cmds = evaluate_profile(&profile_b, &cache, &mut state);
        assert_eq!(cmds[0].pwm_percent, 30);
    }

    #[test]
    fn tuning_state_cleared_on_deactivate() {
        let profile = make_profile("curve", "flat", 60.0);
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        evaluate_profile(&profile, &cache, &mut state);
        assert!(state.last_output("ctrl1").is_some());

        state.deactivate();
        assert!(state.last_output("ctrl1").is_none());
    }

    #[test]
    fn tuning_step_rate_ignored_on_first_cycle() {
        // With no prior last_output, step_up_pct must NOT cap the initial
        // value — otherwise the engine would start every fan at 0 and climb
        // 1%/s to the desired speed.
        let mut profile = make_profile("curve", "flat", 75.0);
        profile.controls[0].step_up_pct = 5.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        let cmds = evaluate_profile(&profile, &cache, &mut state);
        assert_eq!(cmds[0].pwm_percent, 75);
    }

    #[test]
    fn tuning_rounds_to_nearest_not_truncates() {
        // 49.6 should round to 50, not truncate to 49 (GUI parity, see
        // `_write_target`'s `round(pwm_percent)`).
        let mut profile = make_profile("curve", "flat", 49.6);
        profile.controls[0].step_up_pct = 100.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        let cmds = evaluate_profile(&profile, &cache, &mut state);
        assert_eq!(cmds[0].pwm_percent, 50);
    }

    #[test]
    fn tuning_tracks_float_not_rounded_value() {
        // Step-rate limit should operate on the f64 pre-rounded output so
        // 0.4 of a percent per cycle accumulates to a visible change instead
        // of being flattened to 0 at each integer rounding boundary.
        let mut profile = make_profile("curve", "flat", 10.2);
        profile.controls[0].step_up_pct = 100.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        evaluate_profile(&profile, &cache, &mut state);
        assert_eq!(state.last_output("ctrl1"), Some(10.2));
    }

    // ── 2°C temperature deadband (DEC-096) ───────────────────────────

    /// Helper: build a profile whose curve evaluates to a different output
    /// at each test temperature so deadband HOLDS are detectable.
    fn make_graph_profile_for_deadband() -> DaemonProfile {
        // Graph: (60, 30%), (70, 50%) → linear interpolation between.
        // Step-rate limits disabled so they don't mask deadband behaviour.
        DaemonProfile {
            id: "deadband-test".into(),
            name: "Deadband".into(),
            version: 4,
            description: "".into(),
            controls: vec![LogicalControl {
                id: "ctrl1".into(),
                name: "Test".into(),
                mode: "curve".into(),
                curve_id: "c1".into(),
                manual_output_pct: 0.0,
                members: vec![ControlMember {
                    source: "openfan".into(),
                    member_id: "openfan:ch00".into(),
                    member_label: "".into(),
                    fan_zero_rpm: false,
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
                curve_type: "graph".into(),
                sensor_id: "cpu".into(),
                points: vec![
                    CurvePoint {
                        temp_c: 60.0,
                        output_pct: 30.0,
                    },
                    CurvePoint {
                        temp_c: 70.0,
                        output_pct: 50.0,
                    },
                ],
                start_temp_c: None,
                start_output_pct: None,
                end_temp_c: None,
                end_output_pct: None,
                flat_output_pct: None,
            }],
        }
    }

    #[test]
    fn deadband_holds_within_2c_below_anchor() {
        // Cycle 1 at 70°C → curve output 50%, anchor=70.
        // Cycle 2 at 69°C is within the 2°C deadband below 70 → HOLD 50%.
        let profile = make_graph_profile_for_deadband();
        let mut state = ProfileEngineState::new();

        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 70.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 50);
        assert_eq!(state.last_transition_temp("ctrl1"), Some(70.0));

        // Falling to 69°C — inside the deadband below 70.
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 69.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 50, "deadband should hold at 50%");
        assert_eq!(
            state.last_transition_temp("ctrl1"),
            Some(70.0),
            "anchor must not move while held"
        );
    }

    #[test]
    fn deadband_releases_below_2c_threshold() {
        // Cycle 1 at 70°C → 50%, anchor=70.
        // Cycle 2 at 67.5°C is below 70-2=68 → re-evaluate curve.
        let profile = make_graph_profile_for_deadband();
        let mut state = ProfileEngineState::new();

        evaluate_profile(&profile, &make_cache_with_sensor("cpu", 70.0), &mut state);

        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 67.5), &mut state);
        // curve(67.5) = 30 + (67.5-60)/10 * 20 = 45.0 → rounded to 45
        assert_eq!(
            cmds[0].pwm_percent, 45,
            "below the deadband, curve must re-evaluate"
        );
        assert_eq!(state.last_transition_temp("ctrl1"), Some(67.5));
    }

    #[test]
    fn deadband_anchor_moves_on_rising_temperature() {
        // Rising temperature should move the anchor each cycle (output
        // changes meaningfully) so the deadband applies relative to the
        // current peak temperature rather than the original 70°C.
        let profile = make_graph_profile_for_deadband();
        let mut state = ProfileEngineState::new();

        evaluate_profile(&profile, &make_cache_with_sensor("cpu", 65.0), &mut state);
        assert_eq!(state.last_transition_temp("ctrl1"), Some(65.0));

        evaluate_profile(&profile, &make_cache_with_sensor("cpu", 68.0), &mut state);
        assert_eq!(state.last_transition_temp("ctrl1"), Some(68.0));

        evaluate_profile(&profile, &make_cache_with_sensor("cpu", 70.0), &mut state);
        assert_eq!(state.last_transition_temp("ctrl1"), Some(70.0));
    }

    #[test]
    fn deadband_anchor_does_not_move_for_tiny_curve_delta() {
        // When the curve output changes by < 0.5% between two evaluations,
        // the deadband anchor must NOT advance — preventing a slowly-rising
        // temperature from "dragging" the deadband forward and starving
        // hysteresis on a subsequent fall.
        // Use a near-flat curve segment: (60, 50%), (80, 50.4%) so
        // 60→61°C gives a delta of 0.02% (< 0.5%).
        let profile = DaemonProfile {
            id: "tiny-delta".into(),
            name: "Tiny".into(),
            version: 4,
            description: "".into(),
            controls: vec![LogicalControl {
                id: "ctrl1".into(),
                name: "Test".into(),
                mode: "curve".into(),
                curve_id: "c1".into(),
                manual_output_pct: 0.0,
                members: vec![ControlMember {
                    source: "openfan".into(),
                    member_id: "openfan:ch00".into(),
                    member_label: "".into(),
                    fan_zero_rpm: false,
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
                curve_type: "graph".into(),
                sensor_id: "cpu".into(),
                points: vec![
                    CurvePoint {
                        temp_c: 60.0,
                        output_pct: 50.0,
                    },
                    CurvePoint {
                        temp_c: 80.0,
                        output_pct: 50.4,
                    },
                ],
                start_temp_c: None,
                start_output_pct: None,
                end_temp_c: None,
                end_output_pct: None,
                flat_output_pct: None,
            }],
        };
        let mut state = ProfileEngineState::new();

        evaluate_profile(&profile, &make_cache_with_sensor("cpu", 60.0), &mut state);
        assert_eq!(state.last_transition_temp("ctrl1"), Some(60.0));

        // Rise to 61°C: curve delta 0.02% < 0.5%, anchor must stay at 60.
        evaluate_profile(&profile, &make_cache_with_sensor("cpu", 61.0), &mut state);
        assert_eq!(
            state.last_transition_temp("ctrl1"),
            Some(60.0),
            "tiny output delta must not move the anchor"
        );
    }

    #[test]
    fn deadband_state_cleared_on_profile_change() {
        // Switching profile id must clear the deadband state so it doesn't
        // bleed into the new profile's evaluations.
        let profile_a = make_graph_profile_for_deadband();
        let mut profile_b = make_graph_profile_for_deadband();
        profile_b.id = "other".into();

        let mut state = ProfileEngineState::new();
        evaluate_profile(&profile_a, &make_cache_with_sensor("cpu", 70.0), &mut state);
        assert!(state.last_transition_temp("ctrl1").is_some());

        evaluate_profile(&profile_b, &make_cache_with_sensor("cpu", 60.0), &mut state);
        // After profile swap + new evaluation, anchor should be from new
        // profile's first cycle, not the prior 70°C from profile_a.
        assert_eq!(state.last_transition_temp("ctrl1"), Some(60.0));
    }

    #[test]
    fn deadband_does_not_apply_to_manual_mode() {
        // Manual mode bypasses the curve, so manual_output_pct is the only
        // thing the engine should look at — the deadband is curve-only.
        let mut profile = make_graph_profile_for_deadband();
        profile.controls[0].mode = "manual".into();
        profile.controls[0].manual_output_pct = 42.0;

        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile(&profile, &make_cache_with_sensor("cpu", 70.0), &mut state);
        assert_eq!(cmds[0].pwm_percent, 42);
        // No curve evaluation happened → no deadband state recorded.
        assert!(state.last_transition_temp("ctrl1").is_none());
        assert!(state.last_curve_output("ctrl1").is_none());
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
                    fan_zero_rpm: false,
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
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
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
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
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
            fan_zero_rpm: false,
        });
        let cache = make_cache_with_sensor("cpu", 55.0);
        let cmds = evaluate_profile(&profile, &cache, &mut ProfileEngineState::new());
        assert_eq!(cmds.len(), 2);

        let gpu_cmd = cmds.iter().find(|c| c.source == "amd_gpu").unwrap();
        let ofc_cmd = cmds.iter().find(|c| c.source == "openfan").unwrap();
        assert_eq!(gpu_cmd.member_id, "amd_gpu:0000:03:00.0");
        assert_eq!(ofc_cmd.member_id, "openfan:ch00");
        // Both should get the same output percentage
        assert_eq!(gpu_cmd.pwm_percent, ofc_cmd.pwm_percent);
        assert_eq!(gpu_cmd.pwm_percent, 60);
    }

    /// T2 (test-tests audit): when `cache.gui_active()` is true (the GUI has
    /// written via the API within the last 30s) the profile-engine loop must
    /// skip its OpenFan write phase. Without this deferral, the GUI's control
    /// loop and the headless engine would race for the same fan, producing
    /// serial chatter and PWM oscillation. Mirrors DEC-074.
    #[tokio::test(start_paused = true)]
    async fn loop_defers_openfan_writes_when_gui_active() {
        let cache = make_cache_with_sensor("cpu", 55.0);
        // Mark the GUI as active — record_gui_write() flips gui_active() to true.
        cache.record_gui_write();
        assert!(cache.snapshot().gui_active(), "test precondition");

        let profile = make_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        // Allocate plenty of mock responses in case the engine does try to write.
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

        // Let the loop run one cycle.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        // No SetPwm (>02) commands should have been issued — the engine
        // deferred to the active GUI.
        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            set_pwm_cmds.is_empty(),
            "profile engine must NOT write OpenFan PWM while gui_active is true; got: {set_pwm_cmds:?}",
        );
    }

    /// Build a fake AMD GPU whose PMFW fan_curve path points into a tempdir.
    /// The engine's GPU phase writes curve points + a commit to that file via
    /// `std::fs::write` (truncating), so "file still empty" == "no write
    /// attempted" and "file non-empty" == "PMFW commit happened".
    fn make_fake_gpu(
        dir: &tempfile::TempDir,
    ) -> (crate::hwmon::gpu_detect::AmdGpuInfo, std::path::PathBuf) {
        let curve_path = dir.path().join("fan_curve");
        std::fs::write(&curve_path, "").unwrap();
        let gpu = crate::hwmon::gpu_detect::AmdGpuInfo {
            pci_bdf: "0000:03:00.0".into(),
            pci_device_id: 0x7550,
            pci_revision: 0xC0,
            pci_class: 0x030000,
            marketing_name: Some("RX 9070 XT".into()),
            hwmon_path: dir.path().to_path_buf(),
            fan_curve_path: Some(curve_path.clone()),
            fan_zero_rpm_path: None,
            is_discrete: true,
            has_fan_rpm: false,
            has_pwm: false,
            has_pwm_enable: false,
            overdrive_enabled: true,
        };
        (gpu, curve_path)
    }

    /// DEC-131: the engine's GPU write suppression must use the shared 5%
    /// PMFW coalesce threshold (GPU_COALESCE_DELTA_PCT), not exact-match.
    /// At 55°C the curve outputs 60%; with last_commanded 62% the delta is
    /// 2% < 5%, so the engine must skip the PMFW write entirely.
    #[tokio::test(start_paused = true)]
    async fn loop_suppresses_gpu_write_below_coalesce_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = make_fake_gpu(&dir);

        let cache = make_cache_with_sensor("cpu", 55.0);
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:03:00.0", 62);

        let profile = make_gpu_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(profile_engine_loop(
            cache,
            profile_arc,
            None, // no openfan
            None, // no hwmon
            vec![gpu],
            safety,
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        let content = std::fs::read_to_string(&curve_path).unwrap();
        assert!(
            content.is_empty(),
            "engine must suppress GPU writes below the 5% coalesce threshold \
             (DEC-131); fan_curve received: {content:?}"
        );
    }

    /// Companion to the suppression test: at a delta of exactly the
    /// threshold (curve 60% vs last_commanded 55% → 5%), the engine must
    /// write the PMFW curve.
    #[tokio::test(start_paused = true)]
    async fn loop_writes_gpu_when_delta_reaches_coalesce_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = make_fake_gpu(&dir);

        let cache = make_cache_with_sensor("cpu", 55.0);
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:03:00.0", 55);

        let profile = make_gpu_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(profile_engine_loop(
            cache.clone(),
            profile_arc,
            None,
            None,
            vec![gpu],
            safety,
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        let content = std::fs::read_to_string(&curve_path).unwrap();
        assert!(
            !content.is_empty(),
            "engine must write the PMFW curve once the delta reaches the 5% \
             coalesce threshold (DEC-131)"
        );
        // The cache must reflect the new commanded value (60%).
        let snap = cache.gpu_fans_snapshot();
        assert_eq!(
            snap.get("amd_gpu:0000:03:00.0")
                .and_then(|f| f.last_commanded_pct),
            Some(60)
        );
    }

    /// P3-2: a forced safety override must clear the engine's cross-cycle
    /// tuning state. The fans are physically at the forced PWM, so resuming
    /// normal evaluation from the pre-emergency `last_output` anchor would
    /// step-rate-clamp against a value the hardware no longer holds.
    ///
    /// Timeline (1 tick/s, paused time):
    ///   t1: 79°C → curve 98.4% → writes 98%   (anchor now 98.4)
    ///   t2: 106°C → EMERGENCY, all 10 ch → 100%, state cleared
    ///   t3: 60°C → release + recovery floor → all ch → 60%
    ///   t4: recovery floor (one extra cycle) → 60% (coalesced, no writes)
    ///   t5: normal eval at 60°C → curve 68%.
    ///       Fixed: fresh state → writes 68% (raw 0xAD).
    ///       Bug: stale anchor 98.4 with step_down 2%/cycle → 96% (raw 0xF5).
    #[tokio::test(start_paused = true)]
    async fn forced_override_resets_engine_tuning_state() {
        fn cpu_reading(temp_c: f64) -> CachedSensorReading {
            CachedSensorReading {
                id: "cpu".into(),
                kind: SensorKind::CpuTemp,
                label: "Tctl".into(),
                value_c: temp_c,
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

        let cache = make_cache_with_sensor("cpu", 79.0);
        let mut profile = make_profile("curve", "graph", 50.0);
        // Tight step-down so a stale anchor is observable: from 98.4% the
        // engine could only descend 2%/cycle.
        profile.controls[0].step_down_pct = 2.0;
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        // 1 (t1) + 10 (t2) + 10 (t3) + 0 (t4 coalesced) + 1 (t5) = 22 writes.
        let (transport, written) = LoopTestTransport::new(30);
        let fan_ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(500),
        );
        let fan_ctrl = Some(Arc::new(Mutex::new(fan_ctrl)));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(profile_engine_loop(
            cache.clone(),
            profile_arc,
            fan_ctrl,
            None,
            vec![],
            safety,
            shutdown_rx,
        ));

        // t1 @1.0s: normal evaluation at 79°C.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        // t2 @2.0s: thermal emergency.
        cache.update_sensors(vec![cpu_reading(106.0)]);
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        // t3 @3.0s: release + recovery floor.
        cache.update_sensors(vec![cpu_reading(60.0)]);
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        // t4 @4.0s: one-cycle recovery floor (writes coalesce at 60%).
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        // t5 @5.0s: normal evaluation resumes at 60°C.
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        let last = set_pwm_cmds.last().expect("expected SetPwm commands");
        let hex_value = &last[last.len() - 3..last.len() - 1];
        let expected = format!("{:02X}", crate::pwm::percent_to_raw(68));
        let stale = format!("{:02X}", crate::pwm::percent_to_raw(96));
        assert_eq!(
            hex_value, expected,
            "post-override evaluation must start from a fresh anchor \
             (expected 68% = 0x{expected}, stale-anchor bug yields 96% = 0x{stale}); \
             commands: {set_pwm_cmds:?}"
        );
    }

    /// DEC-135: the extracted safety step must walk the full 105/80/60
    /// ladder — trigger, hold, release-with-recovery, one extra recovery
    /// cycle, then normal.
    #[test]
    fn safety_tick_emergency_recovery_ladder() {
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        let d = evaluate_safety_tick(Some(106.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "emergency",
                forced_pct: Some(100)
            }
        );

        // Still above release threshold — hold at 100%.
        let d = evaluate_safety_tick(Some(90.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "emergency",
                forced_pct: Some(100)
            }
        );

        // Release at ≤80°C → recovery floor.
        let d = evaluate_safety_tick(Some(60.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "recovery",
                forced_pct: Some(60)
            }
        );

        // One extra recovery cycle.
        let d = evaluate_safety_tick(Some(60.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "recovery",
                forced_pct: Some(60)
            }
        );

        let d = evaluate_safety_tick(Some(60.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "normal",
                forced_pct: None
            }
        );
    }

    /// P0-R1 + DEC-132: the no-CPU-sensor fallback forces the safe minimum
    /// after the cycle threshold and surfaces a distinct thermal state
    /// (it forces PWM and force-takes the lease, so "normal" would lie to
    /// the GUI's stand-down logic).
    #[test]
    fn safety_tick_no_sensor_fallback_after_threshold() {
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        for i in 1..constants::NO_SENSOR_CYCLE_THRESHOLD {
            let d = evaluate_safety_tick(None, &mut cycles, &mut rule);
            assert_eq!(d.forced_pct, None, "cycle {i}: below threshold");
            assert_eq!(d.thermal_state, "normal", "cycle {i}: below threshold");
        }

        let d = evaluate_safety_tick(None, &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "no_sensor_fallback",
                forced_pct: Some(constants::NO_SENSOR_SAFE_PCT)
            }
        );

        // Sensor recovers → counter resets, normal control resumes.
        let d = evaluate_safety_tick(Some(50.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "normal",
                forced_pct: None
            }
        );
        assert_eq!(cycles, 0);
    }

    /// A sensor returning mid-streak (before the threshold) resets the
    /// counter — the fallback only fires on N *consecutive* missing cycles.
    #[test]
    fn safety_tick_counter_resets_on_sensor_return_mid_streak() {
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        for _ in 0..constants::NO_SENSOR_CYCLE_THRESHOLD - 1 {
            evaluate_safety_tick(None, &mut cycles, &mut rule);
        }
        evaluate_safety_tick(Some(50.0), &mut cycles, &mut rule);
        assert_eq!(cycles, 0);

        // A fresh streak must count from zero again.
        let d = evaluate_safety_tick(None, &mut cycles, &mut rule);
        assert_eq!(d.forced_pct, None);
        assert_eq!(cycles, 1);
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
