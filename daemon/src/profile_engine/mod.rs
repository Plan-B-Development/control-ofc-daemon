//! Profile engine — headless curve evaluation loop.
//!
//! Reads sensor values from StateCache, evaluates curves from the active
//! profile, and returns PWM write commands. Runs at 1Hz alongside the
//! existing polling loops.

mod backends;

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use backends::{GpuBackend, HwmonBackend, OpenFanBackend, SafetyWriteBackend, WriteBackend};

use crate::constants;
use crate::control_override::OverrideSnapshot;
use crate::health::cache::StateCache;
use crate::health::state::CachedSensorReading;
use crate::hwmon::types::SensorKind;
use crate::profile::{
    evaluate_curve, member_is_gpu, member_is_pump_or_cpu, ControlMember, DaemonProfile,
    LogicalControl, HARD_PUMP_CPU_FLOOR_PCT,
};

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
    /// DEC-149 two-state trigger latch per control id (`true` = load state).
    /// Trigger curves own their idle..load hysteresis and bypass the 2°C
    /// deadband; this holds the latch across cycles. Mirrors the GUI's
    /// ``TargetState.trigger_latch``.
    trigger_latch: HashMap<String, bool>,
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
        self.trigger_latch.clear();
        self.active_profile_id = None;
    }

    /// Drop all cross-tick state for a single control so its next evaluation
    /// re-anchors fresh. Called when a manual override clears (release or
    /// expiry) — the override paused this control's eval, so without a reset the
    /// resumed curve would step-rate-clamp from the pinned value. Mirrors the
    /// GUI's `clear_control_manual`, which drops the control's own state plus
    /// its per-member step-rate keys (`{control_id}::m::{member_id}`).
    pub fn reset_control(&mut self, control_id: &str) {
        self.last_output.remove(control_id);
        self.last_curve_output.remove(control_id);
        self.last_transition_temp.remove(control_id);
        self.trigger_latch.remove(control_id);
        // Per-member step-rate trackers live only in `last_output`.
        let prefix = format!("{control_id}::m::");
        self.last_output.retain(|k, _| !k.starts_with(&prefix));
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
            self.trigger_latch.clear();
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

/// Two-state latch (DEC-149): below the idle temp run idle speed; at/above the
/// load temp run load speed; within the idle..load band hold the current state.
/// Owns its own hysteresis, so it bypasses the 2°C deadband. Latch state lives
/// in `ProfileEngineState::trigger_latch` (`true` = load) keyed by control id.
/// Must match the GUI's `_evaluate_trigger` byte-for-byte (parity tuning_sequence).
fn evaluate_trigger(
    control: &LogicalControl,
    curve: &crate::profile::CurveConfig,
    current_temp: f64,
    state: &mut ProfileEngineState,
) -> f64 {
    let idle_temp = curve.trigger_idle_temp_c.unwrap_or(40.0);
    let load_temp = curve.trigger_load_temp_c.unwrap_or(60.0);
    let idle_pct = curve.trigger_idle_pct.unwrap_or(30.0);
    let load_pct = curve.trigger_load_pct.unwrap_or(80.0);
    let is_load = match state.trigger_latch.get(&control.id).copied() {
        // In the load state: fall back to idle only once temp reaches the idle temp.
        Some(true) => current_temp > idle_temp,
        // Idle or cold-start: enter the load state at/above the load temp.
        _ => current_temp >= load_temp,
    };
    state.trigger_latch.insert(control.id.clone(), is_load);
    if is_load {
        load_pct
    } else {
        idle_pct
    }
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

/// Combine child-curve outputs for a Mix curve (DEC-150), clamped 0–100.
///
/// `values` is non-empty (the caller drops unresolved children and skips the
/// Mix entirely when nothing resolves). Must stay byte-for-byte identical to
/// the GUI's `_combine_mix` (parity `tuning_sequence`). `subtract` is the first
/// input minus the sum of the rest, matching the ordered `mix_curve_ids`.
fn combine_mix(function: &str, values: &[f64]) -> f64 {
    let result = match function {
        "min" => values.iter().copied().fold(f64::INFINITY, f64::min),
        "average" => values.iter().sum::<f64>() / values.len() as f64,
        "sum" => values.iter().sum::<f64>(),
        "subtract" => values[0] - values[1..].iter().sum::<f64>(),
        // "max" — also the default for an unrecognised function
        _ => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    };
    result.clamp(0.0, 100.0)
}

/// Resolve a curve's raw output in the Mix evaluation context (DEC-150).
///
/// Mix recurses over its children (combining raw outputs); `visited` carries
/// the current resolution path so a curve reappearing on its own path drops out
/// (cycle → safe fallback). Sync is not a valid Mix child (Mix does not nest
/// Sync; the editor prevents it). Every single-temperature type uses the pure
/// `evaluate_curve` at its own sensor, clamped 0–100 — the "raw child-curve
/// output" the Mix combines. Returns None when the value cannot be resolved
/// (missing sensor, unresolvable/cyclic Mix). Must stay byte-for-byte identical
/// to the GUI's `_resolve_curve_output`.
fn resolve_curve_output(
    curve: &crate::profile::CurveConfig,
    profile: &DaemonProfile,
    sensors: &HashMap<String, CachedSensorReading>,
    visited: &mut HashSet<String>,
) -> Option<f64> {
    match curve.curve_type.as_str() {
        "mix" => resolve_mix(curve, profile, sensors, visited),
        "sync" => None, // Mix does not nest Sync (editor-prevented)
        _ => {
            let sensor = sensors.get(&curve.sensor_id)?;
            Some(evaluate_curve(curve, sensor.value_c).clamp(0.0, 100.0))
        }
    }
}

/// Combine a Mix curve's children (DEC-150). Each child is evaluated at its own
/// sensor; unresolved children are dropped; surviving values are combined by
/// `mix_function` and clamped 0–100. Returns None when the curve is part of a
/// cycle or no child resolves (control skipped — fan holds). Path-based
/// `visited` (insert on entry, remove on exit) matches the GUI's per-branch set
/// union so diamonds re-evaluate and only true cycles drop out.
fn resolve_mix(
    curve: &crate::profile::CurveConfig,
    profile: &DaemonProfile,
    sensors: &HashMap<String, CachedSensorReading>,
    visited: &mut HashSet<String>,
) -> Option<f64> {
    if visited.contains(&curve.id) {
        log::warn!(
            "Mix curve '{}' has a dependency cycle — skipping",
            curve.name
        );
        return None;
    }
    visited.insert(curve.id.clone());
    let mut values: Vec<f64> = Vec::new();
    for child_id in &curve.mix_curve_ids {
        if let Some(child) = profile.curves.iter().find(|c| &c.id == child_id) {
            if let Some(v) = resolve_curve_output(child, profile, sensors, visited) {
                values.push(v);
            }
        }
    }
    visited.remove(&curve.id);
    if values.is_empty() {
        return None;
    }
    let function = curve.mix_function.as_deref().unwrap_or("max");
    Some(combine_mix(function, &values))
}

/// Mirror another control's current-tick tuned output (DEC-151).
///
/// Reads `tick_outputs[target]` — populated for every control already evaluated
/// this tick, which the topological ordering guarantees is the Sync's target on
/// an acyclic graph. Adds `sync_offset_pct` and clamps 0–100; the Sync control's
/// own tuning is applied afterwards by the caller. Returns None (control skipped)
/// for an unset/self target or one not yet computed (cycle / skipped / missing).
/// Must match the GUI's `_resolve_sync_output` byte-for-byte.
fn resolve_sync_output(
    control: &LogicalControl,
    curve: &crate::profile::CurveConfig,
    tick_outputs: &HashMap<String, f64>,
) -> Option<f64> {
    let target_id = curve.sync_control_id.as_str();
    if target_id.is_empty() || target_id == control.id {
        return None;
    }
    let target_output = tick_outputs.get(target_id)?;
    let offset = curve.sync_offset_pct.unwrap_or(0.0);
    Some((target_output + offset).clamp(0.0, 100.0))
}

/// Resolve the raw curve output for one control, before the tuning pipeline.
///
/// Routes trigger to the latch, mix/sync to the context resolvers, and every
/// single-temperature type to the 2°C deadband path — mirroring the GUI's
/// `_curve_output_for_control`. Returns None when the control must be skipped
/// this tick (missing sensor, unresolvable composite).
fn curve_output_for_control(
    control: &LogicalControl,
    curve: &crate::profile::CurveConfig,
    profile: &DaemonProfile,
    sensors: &HashMap<String, CachedSensorReading>,
    tick_outputs: &HashMap<String, f64>,
    state: &mut ProfileEngineState,
) -> Option<f64> {
    match curve.curve_type.as_str() {
        "mix" => resolve_curve_output(curve, profile, sensors, &mut HashSet::new()),
        "sync" => resolve_sync_output(control, curve, tick_outputs),
        "trigger" => {
            let sensor = sensors.get(&curve.sensor_id)?;
            Some(evaluate_trigger(control, curve, sensor.value_c, state))
        }
        _ => {
            let sensor = sensors.get(&curve.sensor_id)?;
            Some(evaluate_curve_with_deadband(
                control,
                curve,
                sensor.value_c,
                state,
            ))
        }
    }
}

/// The control id a Sync-driven control depends on, else None. Mirrors the GUI's
/// `_sync_dependency`.
fn sync_dependency<'a>(control: &'a LogicalControl, profile: &'a DaemonProfile) -> Option<&'a str> {
    if control.mode != "curve" {
        return None;
    }
    let curve = profile.curves.iter().find(|c| c.id == control.curve_id)?;
    if curve.curve_type != "sync" {
        return None;
    }
    let target = curve.sync_control_id.as_str();
    if target.is_empty() {
        None
    } else {
        Some(target)
    }
}

/// Stable topological order of control indices for Sync dependency resolution
/// (DEC-151). A control whose curve is a `sync` depends on the control it
/// targets, so the target is emitted first; independent controls keep their
/// profile order (stable). A cycle is broken deterministically (the closing
/// Sync reads a not-yet-computed target and falls back at eval time). Mirrors
/// the GUI's `_ordered_controls` DFS so both evaluators order controls
/// identically (parity-critical). Sync-free profiles emit `[0, 1, …, n-1]`.
fn topological_control_order(profile: &DaemonProfile) -> Vec<usize> {
    let id_to_idx: HashMap<&str, usize> = profile
        .controls
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();
    let n = profile.controls.len();
    let mut ordered = Vec::with_capacity(n);
    let mut emitted = vec![false; n];
    let mut on_path = vec![false; n];
    for start in 0..n {
        topo_visit(
            start,
            profile,
            &id_to_idx,
            &mut ordered,
            &mut emitted,
            &mut on_path,
        );
    }
    ordered
}

fn topo_visit(
    idx: usize,
    profile: &DaemonProfile,
    id_to_idx: &HashMap<&str, usize>,
    ordered: &mut Vec<usize>,
    emitted: &mut [bool],
    on_path: &mut [bool],
) {
    if emitted[idx] || on_path[idx] {
        return;
    }
    on_path[idx] = true;
    if let Some(dep_id) = sync_dependency(&profile.controls[idx], profile) {
        if let Some(&dep_idx) = id_to_idx.get(dep_id) {
            if dep_idx != idx {
                topo_visit(dep_idx, profile, id_to_idx, ordered, emitted, on_path);
            }
        }
    }
    on_path[idx] = false;
    if !emitted[idx] {
        emitted[idx] = true;
        ordered.push(idx);
    }
}

/// Evaluate the active profile against current sensor readings.
///
/// Returns a list of PWM commands for each fan member in the profile.
/// The caller is responsible for executing the writes. `engine_state` holds
/// per-control cross-cycle state required by the tuning pipeline.
/// Per-member minimum-PWM floor (DEC-119 + DEC-162). GPU members carry no
/// floor (PMFW enforces its own OD_RANGE minimum); a pump/CPU header is hard-
/// floored to at least [`HARD_PUMP_CPU_FLOOR_PCT`] even when the control
/// declares a lower `minimum_pct`; every other member uses the control-wide
/// floor. Shared by the curve path and the override path so the safety floor
/// is computed in exactly one place.
fn member_effective_floor(control: &LogicalControl, member: &ControlMember) -> f64 {
    if member_is_gpu(member) {
        0.0
    } else if member_is_pump_or_cpu(member) {
        control.minimum_pct.max(HARD_PUMP_CPU_FLOOR_PCT)
    } else {
        control.minimum_pct
    }
}

pub fn evaluate_profile(
    profile: &DaemonProfile,
    sensors: &HashMap<String, CachedSensorReading>,
    engine_state: &mut ProfileEngineState,
) -> Vec<PwmCommand> {
    evaluate_profile_with_overrides(profile, sensors, engine_state, &OverrideSnapshot::default())
}

/// Like [`evaluate_profile`] but with a transient override/identify overlay
/// (DEC-163 / DEC-166). A control named in `overrides.controls` is pinned to its
/// fixed PWM — curve + tuning skipped, only the per-member hard safety floor
/// applied — and its cross-tick state is left untouched (the engine resets it
/// when the override clears, via [`ProfileEngineState::reset_control`]). Any fan
/// in `overrides.identify_stop` is forced to 0 after all other resolution,
/// floor-exempt. With an empty snapshot this is byte-identical to the
/// pre-override evaluator, so the 3-arg `evaluate_profile` (used by the parity
/// oracle) is unperturbed.
pub fn evaluate_profile_with_overrides(
    profile: &DaemonProfile,
    sensors: &HashMap<String, CachedSensorReading>,
    engine_state: &mut ProfileEngineState,
    overrides: &OverrideSnapshot,
) -> Vec<PwmCommand> {
    engine_state.sync_profile_id(&profile.id);

    let mut commands = Vec::new();
    // Per-tick control outputs (post-tuning), consumed by Sync curves mirroring
    // an already-evaluated control. Fresh each tick — distinct from
    // `engine_state.last_output`, which is the PREVIOUS tick and entangled with
    // step-rate limiting, so it must not be reused for Sync (DEC-151). Mirrors
    // the GUI's `status.control_outputs`.
    let mut tick_outputs: HashMap<String, f64> = HashMap::new();

    // Evaluate in stable topological order so a Sync control's target is already
    // in `tick_outputs` when the Sync mirrors it (DEC-151). Sync-free profiles
    // keep their natural profile order.
    for idx in topological_control_order(profile) {
        let control = &profile.controls[idx];

        // Transient manual override (DEC-163): pin this control's members to a
        // fixed PWM, skipping curve evaluation AND the tuning pipeline — only
        // the per-member hard safety floor still applies, so a stuck override
        // can never strand a pump/CPU below its minimum. Mirrors the GUI's
        // `_manual_controls` check at the top of `_evaluate_control`. Curve eval
        // is paused, so this control's cross-tick state does not advance; the
        // engine resets it when the override clears so the resumed curve
        // re-anchors instead of step-rate-clamping from the pin.
        if let Some(&override_pwm) = overrides.controls.get(&control.id) {
            // Publish the raw override intent as this control's tick output so a
            // Sync mirroring it sees the pinned value — matches the GUI, which
            // sets control_outputs = manual_pct (member-less controls still
            // publish for a downstream Sync).
            tick_outputs.insert(control.id.clone(), f64::from(override_pwm));
            for member in &control.members {
                let gpu_fan_zero_rpm = member.source == "amd_gpu" && member.fan_zero_rpm;
                let floor = member_effective_floor(control, member);
                let member_pwm = f64::from(override_pwm).max(floor).round().clamp(0.0, 100.0) as u8;
                commands.push(PwmCommand {
                    member_id: member.member_id.clone(),
                    source: member.source.clone(),
                    pwm_percent: member_pwm,
                    gpu_fan_zero_rpm,
                });
            }
            continue;
        }

        // Determine target output percentage. None → skip this control this tick
        // (manual mode always resolves; curve mode skips on missing curve /
        // sensor / unresolvable composite). The fan then holds its last value.
        let raw_output = if control.mode == "manual" {
            Some(control.manual_output_pct)
        } else {
            // Find the assigned curve, then resolve via the shared dispatcher
            // (deadband / trigger latch / mix / sync) — mirrors the GUI's
            // `_curve_output_for_control` so headless behaviour matches
            // GUI-driven behaviour.
            match profile.curves.iter().find(|c| c.id == control.curve_id) {
                None => {
                    log::debug!(
                        "Control '{}': curve '{}' not found, skipping",
                        control.name,
                        control.curve_id
                    );
                    None
                }
                Some(curve) => curve_output_for_control(
                    control,
                    curve,
                    profile,
                    sensors,
                    &tick_outputs,
                    engine_state,
                ),
            }
        };
        let Some(raw_output) = raw_output else {
            continue;
        };

        // Full tuning pipeline — tracks pre-rounding f64 across cycles so
        // step_up_pct / step_down_pct don't drift from integer quantisation.
        let prev = engine_state.last_output(&control.id);
        let tuned = apply_tuning(control, raw_output, prev);
        engine_state.last_output.insert(control.id.clone(), tuned);
        // Record the control's current-tick output BEFORE the members check so a
        // Sync can mirror even a member-less control — matches the GUI, which
        // sets `status.control_outputs` for every evaluated control regardless
        // of members (DEC-151).
        tick_outputs.insert(control.id.clone(), tuned);

        // Skip the write phase for member-less controls (they still publish a
        // tick output above for any Sync that mirrors them).
        if control.members.is_empty() {
            continue;
        }

        // Round-to-nearest when converting to the wire PWM value so 49.6
        // becomes 50 instead of being truncated to 49 — matches the GUI's
        // `round(pwm_percent)` in `_write_target`.
        let pwm_percent = tuned.round().clamp(0.0, 100.0) as u8;

        // Generate write commands for all members
        for member in &control.members {
            let gpu_fan_zero_rpm = member.source == "amd_gpu" && member.fan_zero_rpm;
            // DEC-119 + DEC-162: each member's effective minimum-PWM floor.
            // GPU members carry no floor (0% — PMFW enforces its own OD_RANGE
            // minimum). A pump/CPU header is hard-floored to at least
            // HARD_PUMP_CPU_FLOOR_PCT even when the control declares a lower
            // `minimum_pct`: validate() rejects such a profile at the API
            // boundary, but a persisted or hand-edited profile reaches the engine
            // un-validated via `resolve_initial_profile`, so this clamp is the
            // load-bearing safety net. Every other member uses the control-wide
            // floor. Recompute with a namespaced per-member step-rate tracker
            // only when the effective floor differs from the control-wide one
            // (covers both the GPU lower-to-0 and the pump raise-to-floor cases),
            // so headless mode matches the GUI's per-member flooring (the DEC-096
            // consistency guarantee); otherwise reuse the control-wide value so
            // the common path stays byte-identical and the parity oracle is
            // unperturbed. `!=` compares the same source value — no float hazard.
            let effective_floor = member_effective_floor(control, member);
            let member_pwm = if effective_floor != control.minimum_pct {
                let key = format!("{}::m::{}", control.id, member.member_id);
                let prev_member = engine_state.last_output(&key);
                let tuned_member =
                    apply_tuning_with_floor(control, raw_output, prev_member, effective_floor);
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

    // Fan-identify (DEC-166): force identified fans to 0 AFTER curve/override
    // resolution and FLOOR-EXEMPT — you must be able to stop a pump to find it.
    // Subordinate only to the 105°C thermal force, which short-circuits the
    // whole tick before this function is reached. Restore is the entry's
    // removal: the member resumes its curve command next tick (no prior PWM
    // remembered), and its per-member state stayed current (the control kept
    // evaluating; only the final command was zeroed), so no reset is needed.
    if !overrides.identify_stop.is_empty() {
        for cmd in &mut commands {
            if overrides.identify_stop.contains(&cmd.member_id) {
                cmd.pwm_percent = 0;
            }
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
/// the tick) → override/identify sweep → profile evaluation (+ override
/// overlay) → one `apply` per write backend. All per-backend gating lives in
/// [`backends`] (DEC-135).
// Wiring entrypoint: every argument is a distinct shared handle the loop owns
// for its lifetime; bundling them into a struct would only add indirection.
#[allow(clippy::too_many_arguments)]
pub async fn profile_engine_loop(
    cache: Arc<StateCache>,
    profile: Arc<Mutex<Option<DaemonProfile>>>,
    fan_controller: Option<Arc<Mutex<crate::serial::controller::FanController>>>,
    hwmon_controller: Option<Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>>,
    gpu_infos: Vec<crate::hwmon::gpu_detect::AmdGpuInfo>,
    safety: Arc<Mutex<crate::safety::ThermalSafetyRule>>,
    override_table: Arc<Mutex<crate::control_override::OverrideTable>>,
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
        // DEC-146 P3-6: one sensors snapshot per tick, shared by the safety
        // leg and curve evaluation — halves the per-second map clone and
        // makes the tick internally consistent (both legs see one snapshot).
        let sensors = cache.sensors_snapshot();
        let (decision, hottest_cpu_c) = {
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
                be.force_all(forced_pct).await;
            }
            if let Some(be) = hwmon_be.as_mut() {
                be.force_all(forced_pct).await;
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

        // Sweep expired override/identify entries on the daemon's own monotonic
        // clock (never a client timestamp) and reset the cross-tick state of any
        // control whose override just lapsed, so it re-anchors to its curve
        // instead of step-rate-clamping from the pin. Then snapshot the live
        // overlay to apply this tick. (While the GUI is the active writer this
        // overlay is computed but not written — the backends defer on
        // `gui_active`; it takes effect once the engine is primary.)
        let override_snapshot = {
            let mut table = override_table.lock();
            for control_id in table.sweep().controls {
                engine_state.reset_control(&control_id);
            }
            table.snapshot()
        };

        // Get active profile — scope guard strictly to avoid !Send across .await
        let commands = {
            let profile_guard = profile.lock();
            let Some(ref active_profile) = *profile_guard else {
                // No profile loaded — drop any leftover tuning state so a
                // later activation doesn't pick up stale cross-cycle outputs.
                engine_state.deactivate();
                continue;
            };
            evaluate_profile_with_overrides(
                active_profile,
                &sensors,
                &mut engine_state,
                &override_snapshot,
            )
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

    #[test]
    fn trigger_latch_holds_load_across_band_and_bypasses_deadband() {
        // DEC-149: a trigger curve latches — once in the load state it holds
        // load while temperature falls through the idle..load band, dropping to
        // idle only at the idle temp. The 55°C hold also proves the 2°C deadband
        // is bypassed (the deadband path would re-evaluate to idle at 55°C).
        let mut profile = make_profile("curve", "trigger", 0.0);
        profile.curves[0].trigger_idle_temp_c = Some(40.0);
        profile.curves[0].trigger_load_temp_c = Some(60.0);
        profile.curves[0].trigger_idle_pct = Some(30.0);
        profile.curves[0].trigger_load_pct = Some(80.0);
        let mut state = ProfileEngineState::new();
        let pwm = |t: f64, st: &mut ProfileEngineState| -> u8 {
            let cache = make_cache_with_sensor("cpu", t);
            evaluate_profile(&profile, &cache.sensors_snapshot(), st)[0].pwm_percent
        };
        assert_eq!(pwm(50.0, &mut state), 30); // cold-start in band -> idle
        assert_eq!(pwm(65.0, &mut state), 80); // enter load at/above load temp
        assert_eq!(pwm(55.0, &mut state), 80); // HOLD load (deadband would give 30)
        assert_eq!(pwm(45.0, &mut state), 80); // still holding load in the band
        assert_eq!(pwm(40.0, &mut state), 30); // drop to idle at the idle temp
        assert_eq!(pwm(50.0, &mut state), 30); // HOLD idle climbing through band
        assert_eq!(pwm(60.0, &mut state), 80); // re-enter load
    }

    // ── DEC-150 Mix / DEC-151 Sync (composite curves) ───────────────────

    fn openfan_control(id: &str, curve_id: &str, member: &str) -> LogicalControl {
        LogicalControl {
            id: id.into(),
            name: id.into(),
            mode: "curve".into(),
            curve_id: curve_id.into(),
            manual_output_pct: 0.0,
            members: vec![ControlMember {
                source: "openfan".into(),
                member_id: member.into(),
                member_label: "".into(),
                fan_zero_rpm: false,
            }],
            step_up_pct: 100.0,
            step_down_pct: 100.0,
            offset_pct: 0.0,
            minimum_pct: 0.0,
            start_pct: 0.0,
            stop_pct: 0.0,
        }
    }

    fn linear_curve(id: &str, sensor: &str) -> CurveConfig {
        CurveConfig {
            id: id.into(),
            name: id.into(),
            curve_type: "linear".into(),
            sensor_id: sensor.into(),
            start_temp_c: Some(30.0),
            start_output_pct: Some(20.0),
            end_temp_c: Some(80.0),
            end_output_pct: Some(100.0),
            ..Default::default()
        }
    }

    fn mix_curve(id: &str, function: &str, children: &[&str]) -> CurveConfig {
        CurveConfig {
            id: id.into(),
            name: id.into(),
            curve_type: "mix".into(),
            mix_function: Some(function.into()),
            mix_curve_ids: children.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn sync_curve(id: &str, target: &str, offset: f64) -> CurveConfig {
        CurveConfig {
            id: id.into(),
            name: id.into(),
            curve_type: "sync".into(),
            sync_control_id: target.into(),
            sync_offset_pct: Some(offset),
            ..Default::default()
        }
    }

    #[test]
    fn combine_mix_functions_and_clamp() {
        // Must match the GUI's `_combine_mix` exactly (parity tuning_sequence).
        assert_eq!(combine_mix("max", &[60.0, 40.0, 20.0]), 60.0);
        assert_eq!(combine_mix("min", &[60.0, 40.0, 20.0]), 20.0);
        assert_eq!(combine_mix("average", &[60.0, 40.0, 20.0]), 40.0);
        assert_eq!(combine_mix("sum", &[60.0, 40.0, 20.0]), 100.0); // 120 clamps to 100
        assert_eq!(combine_mix("subtract", &[60.0, 40.0, 20.0]), 0.0); // 60-40-20
        assert_eq!(combine_mix("bogus", &[10.0, 90.0]), 90.0); // unknown → max
    }

    #[test]
    fn mix_combines_children_at_own_sensors() {
        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("c", "mx", "openfan:ch00")],
            curves: vec![
                linear_curve("cpu", "cpu"), // at 50 → 52
                linear_curve("gpu", "gpu"), // at 70 → 84
                mix_curve("mx", "max", &["cpu", "gpu"]),
            ],
        };
        let cache = make_cache_with_sensors(&[("cpu".into(), 50.0), ("gpu".into(), 70.0)]);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert_eq!(cmds[0].pwm_percent, 84); // max(52, 84)
    }

    #[test]
    fn mix_self_cycle_skips_control() {
        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("c", "mx", "openfan:ch00")],
            curves: vec![mix_curve("mx", "max", &["mx"])], // references itself
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert!(cmds.is_empty()); // cycle → skipped, no command
    }

    #[test]
    fn sync_mirrors_target_with_offset_and_ordering() {
        // Mirror is listed BEFORE its target — the topological order must still
        // evaluate the target first so the mirror reads its current-tick output.
        let profile = DaemonProfile {
            id: "sync".into(),
            name: "Sync".into(),
            version: 7,
            description: "".into(),
            controls: vec![
                openfan_control("cmir", "sy", "openfan:ch01"),
                openfan_control("ctgt", "cv", "openfan:ch00"),
            ],
            curves: vec![linear_curve("cv", "cpu"), sync_curve("sy", "ctgt", 10.0)],
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        let tgt = cmds.iter().find(|c| c.member_id == "openfan:ch00").unwrap();
        let mir = cmds.iter().find(|c| c.member_id == "openfan:ch01").unwrap();
        assert_eq!(tgt.pwm_percent, 52);
        assert_eq!(mir.pwm_percent, 62); // 52 + 10
    }

    #[test]
    fn sync_two_cycle_both_skip() {
        // A mirrors B, B mirrors A. The sort breaks the cycle; both read a
        // not-yet-computed target and skip — no panic, no command.
        let profile = DaemonProfile {
            id: "cyc".into(),
            name: "Cyc".into(),
            version: 7,
            description: "".into(),
            controls: vec![
                openfan_control("ca", "sa", "openfan:ch00"),
                openfan_control("cb", "sb", "openfan:ch01"),
            ],
            curves: vec![sync_curve("sa", "cb", 0.0), sync_curve("sb", "ca", 0.0)],
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn topological_order_sync_free_is_profile_order() {
        let profile = DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: "".into(),
            controls: vec![
                openfan_control("c0", "x", "openfan:ch00"),
                openfan_control("c1", "x", "openfan:ch01"),
                openfan_control("c2", "x", "openfan:ch02"),
            ],
            curves: vec![],
        };
        assert_eq!(topological_control_order(&profile), vec![0, 1, 2]);
    }

    #[test]
    fn topological_order_puts_sync_target_first() {
        // controls [mirror(0)→target, target(1)] must order as [target, mirror].
        let profile = DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: "".into(),
            controls: vec![
                openfan_control("cmir", "sy", "openfan:ch01"),
                openfan_control("ctgt", "cv", "openfan:ch00"),
            ],
            curves: vec![linear_curve("cv", "cpu"), sync_curve("sy", "ctgt", 0.0)],
        };
        assert_eq!(topological_control_order(&profile), vec![1, 0]);
    }

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
                ..Default::default()
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

    /// Build a cache holding several sensors at once (parity multi-sensor Mix
    /// cases). All are tagged ``CpuTemp`` — curve evaluation looks sensors up by
    /// id, so the kind is irrelevant here; only the id→value mapping matters.
    fn make_cache_with_sensors(sensors: &[(String, f64)]) -> Arc<StateCache> {
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(
            sensors
                .iter()
                .map(|(id, temp)| CachedSensorReading {
                    id: id.clone(),
                    kind: SensorKind::CpuTemp,
                    label: id.clone(),
                    value_c: *temp,
                    source: DeviceLabel::Hwmon,
                    updated_at: Instant::now(),
                    rate_c_per_s: None,
                    session_min_c: None,
                    session_max_c: None,
                    chip_name: "k10temp".into(),
                    temp_type: None,
                    thresholds: None,
                })
                .collect(),
        );
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
            let profile: DaemonProfile =
                serde_json::from_value(vector["profile"].clone()).expect("profile");
            let mut state = ProfileEngineState::new();

            // Per-step sensor snapshots from either fixture shape: a single
            // `sensor_id` + `temps`, or a multi-sensor `sensor_temps` map
            // ({id:[temp_per_step]}, equal length — multi-sensor Mix cases).
            let steps: Vec<Vec<(String, f64)>> = if let Some(map) = vector.get("sensor_temps") {
                let map = map.as_object().unwrap();
                let n = map.values().next().unwrap().as_array().unwrap().len();
                (0..n)
                    .map(|i| {
                        map.iter()
                            .map(|(id, arr)| {
                                (id.clone(), arr.as_array().unwrap()[i].as_f64().unwrap())
                            })
                            .collect()
                    })
                    .collect()
            } else {
                let sensor_id = vector["sensor_id"].as_str().unwrap();
                vector["temps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| vec![(sensor_id.to_string(), t.as_f64().unwrap())])
                    .collect()
            };

            let mut produced: HashMap<String, Vec<u8>> = HashMap::new();
            for step in &steps {
                let cache = make_cache_with_sensors(step);
                for cmd in evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state) {
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
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
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
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].pwm_percent, 42); // manual_output_pct
    }

    #[test]
    fn evaluate_curve_mode_uses_sensor_temp() {
        let profile = make_profile("curve", "graph", 50.0);
        let cache = make_cache_with_sensor("cpu", 55.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert_eq!(cmds.len(), 1);
        // At 55°C with 30→20%, 80→100%: (55-30)/(80-30) = 0.5, 20+0.5*80 = 60%
        assert_eq!(cmds[0].pwm_percent, 60);
        assert_eq!(cmds[0].member_id, "openfan:ch00");
    }

    #[test]
    fn evaluate_missing_sensor_skips_control() {
        let profile = make_profile("curve", "graph", 50.0);
        let cache = make_cache_with_sensor("gpu", 50.0); // wrong sensor
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert!(cmds.is_empty()); // sensor "cpu" not found
    }

    #[test]
    fn evaluate_empty_members_skips_control() {
        let mut profile = make_profile("curve", "graph", 50.0);
        profile.controls[0].members.clear();
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn evaluate_offset_and_minimum_applied() {
        let mut profile = make_profile("curve", "flat", 20.0);
        profile.controls[0].offset_pct = 10.0;
        profile.controls[0].minimum_pct = 35.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert_eq!(cmds.len(), 1);
        // flat=20, +offset=10 → 30, but minimum=35 → clamped to 35
        assert_eq!(cmds[0].pwm_percent, 35);
    }

    #[test]
    fn evaluate_output_clamped_to_100() {
        let mut profile = make_profile("curve", "flat", 95.0);
        profile.controls[0].offset_pct = 20.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
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
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );

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
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );

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
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
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
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert_eq!(cmds.len(), 2);
        assert!(cmds.iter().all(|c| c.pwm_percent == 20));
    }

    // ── DEC-162: pump/CPU hard-floor clamp (the un-validated-path safety net) ──

    fn push_pump_member(profile: &mut DaemonProfile) {
        profile.controls[0].members.push(ControlMember {
            source: "hwmon".into(),
            member_id: "hwmon:z53:0000:pwm1:pwm1".into(),
            member_label: "AIO_PUMP".into(),
            fan_zero_rpm: false,
        });
    }

    #[test]
    fn evaluate_pump_member_clamped_to_floor_when_declared_too_low() {
        // The engine independently raises a pump member to the hard floor even
        // when the profile declares a lower minimum_pct (the boot-load / hand-edit
        // path that bypasses validate()). stop_pct=0 so the stop threshold cannot
        // mask the floor. Held across ticks incl. the first (no prior output → no
        // step-rate limiting → jumps straight to the floor, as a pump must).
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].members.clear();
        push_pump_member(&mut profile);
        profile.controls[0].manual_output_pct = 5.0;
        profile.controls[0].minimum_pct = 0.0;
        profile.controls[0].stop_pct = 0.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();
        for _ in 0..3 {
            let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
            let pump = cmds.iter().find(|c| c.source == "hwmon").unwrap();
            assert_eq!(
                pump.pwm_percent, 30,
                "pump must clamp to the hard floor every tick"
            );
        }
    }

    #[test]
    fn evaluate_pump_member_uses_higher_declared_floor() {
        // max(declared, hard floor): a stricter declared floor wins over 30.
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].members.clear();
        push_pump_member(&mut profile);
        profile.controls[0].manual_output_pct = 5.0;
        profile.controls[0].minimum_pct = 45.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        let pump = cmds.iter().find(|c| c.source == "hwmon").unwrap();
        assert_eq!(pump.pwm_percent, 45);
    }

    #[test]
    fn evaluate_mixed_pump_gpu_chassis_each_member_own_floor() {
        // One control, three members at manual 10% / control floor 20%: the pump
        // is hard-raised to 30, the openfan honours the control floor 20, and the
        // GPU runs at its natural 10 (no floor — DEC-119). Three distinct floors.
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].manual_output_pct = 10.0;
        profile.controls[0].minimum_pct = 20.0;
        push_pump_member(&mut profile);
        push_gpu_member(&mut profile);
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        let pump = cmds.iter().find(|c| c.source == "hwmon").unwrap();
        let gpu = cmds.iter().find(|c| c.source == "amd_gpu").unwrap();
        let openfan = cmds.iter().find(|c| c.source == "openfan").unwrap();
        assert_eq!(pump.pwm_percent, 30, "pump hard-raised");
        assert_eq!(openfan.pwm_percent, 20, "openfan honours control floor");
        assert_eq!(gpu.pwm_percent, 10, "GPU runs at natural output (no floor)");
    }

    #[test]
    fn evaluate_chassis_member_not_hard_raised() {
        // DEC-162 scope is pump/CPU only — a chassis/radiator fan is never
        // hard-raised by the engine; it honours the control-wide floor verbatim.
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].members.clear();
        profile.controls[0].members.push(ControlMember {
            source: "hwmon".into(),
            member_id: "hwmon:it8696:0000:pwm2:CHA_FAN".into(),
            member_label: "Radiator Top".into(),
            fan_zero_rpm: false,
        });
        profile.controls[0].manual_output_pct = 5.0;
        profile.controls[0].minimum_pct = 5.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        let cha = cmds.iter().find(|c| c.source == "hwmon").unwrap();
        assert_eq!(cha.pwm_percent, 5);
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
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 30);

        // Curve jumps to 80 (simulate by rebuilding profile)
        profile.curves[0].flat_output_pct = Some(80.0);

        // Cycle 2: temp rose, deadband releases, step_up caps the increase at +10 → 40
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 51.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 40);

        // Cycle 3: another +10 → 50
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 52.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 50);
    }

    #[test]
    fn tuning_step_down_rate_limits_large_drop() {
        let mut profile = make_profile("curve", "flat", 80.0);
        profile.controls[0].step_up_pct = 100.0;
        profile.controls[0].step_down_pct = 15.0;
        let mut state = ProfileEngineState::new();

        // Cycle 1: 80
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 80);

        // Drop curve to 20
        profile.curves[0].flat_output_pct = Some(20.0);

        // Cycle 2: temp rose so the deadband releases — step_down caps at -15 → 65
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 53.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 65);
    }

    #[test]
    fn tuning_stop_threshold_snaps_to_zero() {
        // Flat curve at 15%, stop_pct=20 → snapped to 0
        let mut profile = make_profile("curve", "flat", 15.0);
        profile.controls[0].stop_pct = 20.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
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
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 0);

        // Curve now says 25% (above stop_pct so not snapped; start hysteresis kicks in).
        // Bump temperature so the deadband releases and the new curve output is seen.
        profile.curves[0].flat_output_pct = Some(25.0);
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 51.0).sensors_snapshot(),
            &mut state,
        );
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
            let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
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

        let cmds = evaluate_profile(&profile_a, &cache.sensors_snapshot(), &mut state);
        assert_eq!(cmds[0].pwm_percent, 80);
        assert_eq!(state.last_output("ctrl1"), Some(80.0));

        // Profile id changes → state cleared → step_down_pct no longer bites
        let cmds = evaluate_profile(&profile_b, &cache.sensors_snapshot(), &mut state);
        assert_eq!(cmds[0].pwm_percent, 30);
    }

    #[test]
    fn tuning_state_cleared_on_deactivate() {
        let profile = make_profile("curve", "flat", 60.0);
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();

        evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
        assert!(state.last_output("ctrl1").is_some());

        state.deactivate();
        assert!(state.last_output("ctrl1").is_none());
    }

    #[test]
    fn deactivate_clears_trigger_latch() {
        // DEC-149: the trigger latch must not leak across profiles — deactivate
        // clears it alongside the deadband/step-rate state.
        let mut profile = make_profile("curve", "trigger", 0.0);
        profile.curves[0].trigger_idle_temp_c = Some(40.0);
        profile.curves[0].trigger_load_temp_c = Some(60.0);
        let cache = make_cache_with_sensor("cpu", 65.0);
        let mut state = ProfileEngineState::new();

        evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
        assert_eq!(state.trigger_latch.get("ctrl1"), Some(&true));

        state.deactivate();
        assert!(!state.trigger_latch.contains_key("ctrl1"));
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

        let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
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

        let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
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

        evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
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
                ..Default::default()
            }],
        }
    }

    #[test]
    fn deadband_holds_within_2c_below_anchor() {
        // Cycle 1 at 70°C → curve output 50%, anchor=70.
        // Cycle 2 at 69°C is within the 2°C deadband below 70 → HOLD 50%.
        let profile = make_graph_profile_for_deadband();
        let mut state = ProfileEngineState::new();

        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 70.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 50);
        assert_eq!(state.last_transition_temp("ctrl1"), Some(70.0));

        // Falling to 69°C — inside the deadband below 70.
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 69.0).sensors_snapshot(),
            &mut state,
        );
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

        evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 70.0).sensors_snapshot(),
            &mut state,
        );

        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 67.5).sensors_snapshot(),
            &mut state,
        );
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

        evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 65.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(state.last_transition_temp("ctrl1"), Some(65.0));

        evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 68.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(state.last_transition_temp("ctrl1"), Some(68.0));

        evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 70.0).sensors_snapshot(),
            &mut state,
        );
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
                ..Default::default()
            }],
        };
        let mut state = ProfileEngineState::new();

        evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 60.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(state.last_transition_temp("ctrl1"), Some(60.0));

        // Rise to 61°C: curve delta 0.02% < 0.5%, anchor must stay at 60.
        evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 61.0).sensors_snapshot(),
            &mut state,
        );
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
        evaluate_profile(
            &profile_a,
            &make_cache_with_sensor("cpu", 70.0).sensors_snapshot(),
            &mut state,
        );
        assert!(state.last_transition_temp("ctrl1").is_some());

        evaluate_profile(
            &profile_b,
            &make_cache_with_sensor("cpu", 60.0).sensors_snapshot(),
            &mut state,
        );
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
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 70.0).sensors_snapshot(),
            &mut state,
        );
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
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
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
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
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
                ..Default::default()
            }],
        }
    }

    #[test]
    fn evaluate_gpu_member_produces_amd_gpu_command() {
        // A profile with an amd_gpu member should produce PwmCommands with
        // source="amd_gpu" and the correct member_id.
        let profile = make_gpu_profile("curve", "graph", 50.0);
        let cache = make_cache_with_sensor("cpu", 55.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
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
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
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
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
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
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
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
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
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
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
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
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
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
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
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

    // ── DEC-163 manual override + DEC-166 fan identify ──────────────────

    fn override_snapshot(control_id: &str, pwm: u8) -> OverrideSnapshot {
        let mut controls = HashMap::new();
        controls.insert(control_id.to_string(), pwm);
        OverrideSnapshot {
            controls,
            identify_stop: HashSet::new(),
        }
    }

    fn identify_snapshot(fan_id: &str) -> OverrideSnapshot {
        let mut identify_stop = HashSet::new();
        identify_stop.insert(fan_id.to_string());
        OverrideSnapshot {
            controls: HashMap::new(),
            identify_stop,
        }
    }

    #[test]
    fn empty_overlay_matches_plain_evaluate_profile() {
        // Parity guard: the override-aware path with an empty snapshot must be
        // byte-identical to the 3-arg evaluator the parity oracle uses.
        let profile = make_profile("curve", "graph", 50.0);
        let sensors = make_cache_with_sensor("cpu", 55.0).sensors_snapshot();
        let mut s1 = ProfileEngineState::new();
        let mut s2 = ProfileEngineState::new();
        let plain = evaluate_profile(&profile, &sensors, &mut s1);
        let overlaid = evaluate_profile_with_overrides(
            &profile,
            &sensors,
            &mut s2,
            &OverrideSnapshot::default(),
        );
        assert_eq!(plain.len(), overlaid.len());
        for (a, b) in plain.iter().zip(&overlaid) {
            assert_eq!(a.member_id, b.member_id);
            assert_eq!(a.pwm_percent, b.pwm_percent);
        }
    }

    #[test]
    fn override_pins_pwm_skipping_curve() {
        let profile = make_profile("curve", "graph", 50.0);
        let sensors = make_cache_with_sensor("cpu", 55.0).sensors_snapshot();

        // Curve at 55°C on (30→20, 80→100) = 60%.
        let mut state = ProfileEngineState::new();
        assert_eq!(
            evaluate_profile(&profile, &sensors, &mut state)[0].pwm_percent,
            60
        );

        // Override pins 85%, ignoring the curve.
        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile_with_overrides(
            &profile,
            &sensors,
            &mut state,
            &override_snapshot("ctrl1", 85),
        );
        assert_eq!(cmds[0].pwm_percent, 85);
        // Curve eval was skipped → no cross-tick state advanced for the control.
        assert_eq!(state.last_output("ctrl1"), None);
    }

    #[test]
    fn override_respects_hard_pump_floor() {
        // A pump member overridden to 0% is clamped up to the hard 30% floor — a
        // stuck/fat-fingered override can never strand a pump (DEC-162).
        let mut profile = make_profile("curve", "graph", 50.0);
        profile.controls[0].members = vec![ControlMember {
            source: "hwmon".into(),
            member_id: "hwmon:nct6775:pwm1".into(),
            member_label: "Pump".into(),
            fan_zero_rpm: false,
        }];
        profile.controls[0].minimum_pct = 0.0;
        let sensors = make_cache_with_sensor("cpu", 55.0).sensors_snapshot();

        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile_with_overrides(
            &profile,
            &sensors,
            &mut state,
            &override_snapshot("ctrl1", 0),
        );
        assert_eq!(cmds[0].pwm_percent, 30, "pump override floored to 30%");

        // Above the floor the override value passes through unchanged.
        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile_with_overrides(
            &profile,
            &sensors,
            &mut state,
            &override_snapshot("ctrl1", 70),
        );
        assert_eq!(cmds[0].pwm_percent, 70);
    }

    #[test]
    fn override_gpu_member_floors_at_zero() {
        // A GPU member carries no daemon floor even when the control floor is
        // 30%, so an override may idle it to 0% (PMFW owns the minimum, DEC-119).
        let mut profile = make_profile("curve", "graph", 50.0);
        profile.controls[0].members = vec![ControlMember {
            source: "amd_gpu".into(),
            member_id: "amd_gpu:card0".into(),
            member_label: "".into(),
            fan_zero_rpm: false,
        }];
        profile.controls[0].minimum_pct = 30.0;
        let sensors = make_cache_with_sensor("cpu", 55.0).sensors_snapshot();

        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile_with_overrides(
            &profile,
            &sensors,
            &mut state,
            &override_snapshot("ctrl1", 0),
        );
        assert_eq!(cmds[0].pwm_percent, 0);
    }

    #[test]
    fn reset_control_reverts_to_fresh_curve() {
        // Warm the step-rate anchor to 100%, then prove reset_control drops it so
        // the resumed curve re-anchors fresh instead of step-rate-clamping from
        // the pin (mirrors the GUI's clear_control_manual).
        let mut profile = make_profile("curve", "graph", 50.0);
        profile.controls[0].step_down_pct = 10.0; // slow ramp-down
        let hot = make_cache_with_sensor("cpu", 80.0).sensors_snapshot(); // curve = 100
        let cold = make_cache_with_sensor("cpu", 30.0).sensors_snapshot(); // curve = 20

        let mut state = ProfileEngineState::new();
        assert_eq!(
            evaluate_profile(&profile, &hot, &mut state)[0].pwm_percent,
            100
        );

        // Contrast: without a reset, dropping to 30°C step-limits 100 → 90.
        let mut stepped = ProfileEngineState::new();
        evaluate_profile(&profile, &hot, &mut stepped);
        assert_eq!(
            evaluate_profile(&profile, &cold, &mut stepped)[0].pwm_percent,
            90
        );

        // With reset, the cold curve value (20%) is produced directly.
        state.reset_control("ctrl1");
        assert_eq!(
            evaluate_profile(&profile, &cold, &mut state)[0].pwm_percent,
            20
        );
    }

    #[test]
    fn identify_stop_forces_fan_to_zero_floor_exempt() {
        // identify-stop zeroes a pump fan despite its 30% floor — you must be
        // able to stop a pump to physically find it (DEC-166).
        let mut profile = make_profile("curve", "graph", 50.0);
        profile.controls[0].members = vec![ControlMember {
            source: "hwmon".into(),
            member_id: "hwmon:nct6775:pwm1".into(),
            member_label: "Pump".into(),
            fan_zero_rpm: false,
        }];
        let sensors = make_cache_with_sensor("cpu", 55.0).sensors_snapshot();
        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile_with_overrides(
            &profile,
            &sensors,
            &mut state,
            &identify_snapshot("hwmon:nct6775:pwm1"),
        );
        assert_eq!(cmds[0].pwm_percent, 0);
    }

    #[test]
    fn identify_stop_only_affects_the_named_fan() {
        let mut profile = make_profile("curve", "graph", 50.0);
        profile.controls[0].members = vec![
            ControlMember {
                source: "openfan".into(),
                member_id: "openfan:ch00".into(),
                member_label: "".into(),
                fan_zero_rpm: false,
            },
            ControlMember {
                source: "openfan".into(),
                member_id: "openfan:ch01".into(),
                member_label: "".into(),
                fan_zero_rpm: false,
            },
        ];
        let sensors = make_cache_with_sensor("cpu", 55.0).sensors_snapshot();
        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile_with_overrides(
            &profile,
            &sensors,
            &mut state,
            &identify_snapshot("openfan:ch00"),
        );
        let by_id = |id: &str| cmds.iter().find(|c| c.member_id == id).unwrap().pwm_percent;
        assert_eq!(by_id("openfan:ch00"), 0, "identified fan stopped");
        assert_eq!(by_id("openfan:ch01"), 60, "other fan keeps its curve value");
    }

    #[tokio::test(start_paused = true)]
    async fn safety_force_supersedes_active_override() {
        // 106°C forces every channel to 100% even with an override pinning 30% —
        // the safety tick short-circuits before the override overlay is applied.
        let cache = make_cache_with_sensor("cpu", 106.0);
        let profile_arc = Arc::new(Mutex::new(Some(make_profile("curve", "graph", 50.0))));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        let overrides = Arc::new(Mutex::new(crate::control_override::OverrideTable::new()));
        overrides
            .lock()
            .take_override("ctrl1", 30, std::time::Duration::from_secs(15));

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
            overrides,
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert_eq!(
            set_pwm_cmds.len(),
            10,
            "forced all channels, got {set_pwm_cmds:?}"
        );
        for cmd in &set_pwm_cmds {
            let hex_value = &cmd[cmd.len() - 3..cmd.len() - 1];
            assert_eq!(
                hex_value, "FF",
                "expected forced 100% (FF), not the 30% override: {cmd:?}"
            );
        }
    }
}
