//! Curve evaluation: deadband + trigger latch, Mix/Sync composites, topo
//! ordering, and the per-control dispatcher. Extracted from mod.rs (C3).
//! State-machine fns take `&mut ProfileEngineState` unchanged.

use super::*;

/// Threshold (percent) below which a curve-output change does not move the
/// deadband transition anchor. Deadband evaluation is daemon-only since the
/// 2.0.0 sole-writer cutover (DEC-165) — the GUI's demo/preview tier is
/// stateless `interpolate()` and has no deadband.
pub(crate) const DEADBAND_ANCHOR_DELTA_PCT: f64 = 0.5;

/// Evaluate a curve with the 2°C falling-temperature deadband applied.
///
/// Returns the cached previous curve output when current temperature has
/// fallen within the deadband below the last transition anchor; otherwise
/// re-interpolates the curve and updates the anchor. Side-effects on
/// ``ProfileEngineState`` are scoped to per-control state so unrelated
/// controls are unaffected.
pub(crate) fn evaluate_curve_with_deadband(
    control: &LogicalControl,
    curve: &crate::profile::CurveConfig,
    current_temp: f64,
    state: &mut ProfileEngineState,
) -> f64 {
    let prev_pwm = state.last_curve_output.get(&control.id).copied();
    let prev_transition = state.last_transition_temp.get(&control.id).copied();

    if let (Some(prev_out), Some(anchor)) = (prev_pwm, prev_transition) {
        if current_temp <= anchor && current_temp >= anchor - constants::HYSTERESIS_DEADBAND_C {
            // Inside the deadband. Normally hold the previously commanded output
            // (do not move the anchor; do not update last_curve_output).
            //
            // DEC-188 steady-state valve: count consecutive holds and, once the
            // output has been pinned here for DEADBAND_MAX_HOLD_CYCLES, fall
            // through to re-anchor for a single tick — so a temperature that
            // settled just inside the band can't hold the pre-settle output
            // indefinitely (the "nothing changes for tens of seconds" stall).
            // Any in-band rise (band condition false) or a fall past the band
            // re-evaluates below and clears the streak, so the valve fires only
            // on a genuinely settled reading and cannot reintroduce oscillation.
            let held = state
                .deadband_hold_cycles
                .entry(control.id.clone())
                .or_insert(0);
            *held += 1;
            if *held < constants::DEADBAND_MAX_HOLD_CYCLES {
                return prev_out;
            }
            // Valve open — fall through to re-anchor at the settled temperature.
        }
    }

    // Re-evaluating (curve glide, band exit, or an open valve) ends any hold
    // streak so the next settle starts its own DEADBAND_MAX_HOLD_CYCLES window.
    state.deadband_hold_cycles.remove(&control.id);

    let curve_output = evaluate_curve(curve, current_temp).clamp(0.0, 100.0);

    // Move the transition anchor only when the new curve output meaningfully
    // differs from the last one — keeps the deadband stationary as the curve
    // glides through small interpolation deltas (DEC-096).
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
/// Daemon-owned outright since the 2.0.0 sole-writer cutover (DEC-165) — the
/// GUI kept only the stateless `interpolate` tier; latched behaviour is pinned
/// by the daemon-only `tuning_sequence` golden vectors in `parity_vectors.json`
/// (DEC-126).
pub(crate) fn evaluate_trigger(
    control: &LogicalControl,
    curve: &crate::profile::CurveConfig,
    current_temp: f64,
    state: &mut ProfileEngineState,
) -> f64 {
    let idle_temp = curve
        .trigger_idle_temp_c
        .unwrap_or(crate::profile::TRIGGER_IDLE_TEMP_C);
    let load_temp = curve
        .trigger_load_temp_c
        .unwrap_or(crate::profile::TRIGGER_LOAD_TEMP_C);
    let idle_pct = curve
        .trigger_idle_pct
        .unwrap_or(crate::profile::TRIGGER_IDLE_PCT);
    let load_pct = curve
        .trigger_load_pct
        .unwrap_or(crate::profile::TRIGGER_LOAD_PCT);
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

/// Combine child-curve outputs for a Mix curve (DEC-150), clamped 0–100.
///
/// `values` is non-empty (the caller drops unresolved children and skips the
/// Mix entirely when nothing resolves). Daemon-owned outright since the 2.0.0
/// cutover (DEC-165); pinned by the daemon-only `tuning_sequence` golden
/// vectors in `parity_vectors.json` (DEC-126). `subtract` is the first input
/// minus the sum of the rest, matching the ordered `mix_curve_ids`.
pub(crate) fn combine_mix(function: &str, values: &[f64]) -> f64 {
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
/// (missing sensor, unresolvable/cyclic Mix). Daemon-owned outright since the
/// 2.0.0 cutover (DEC-165) — the GUI no longer resolves composites; behaviour
/// is pinned by the `tuning_sequence` golden vectors (DEC-126).
pub(crate) fn resolve_curve_output(
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
/// `visited` (insert on entry, remove on exit) is a per-branch path set, so
/// diamonds re-evaluate and only true cycles drop out.
pub(crate) fn resolve_mix(
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
    // Depth backstop (see profile::MAX_PROFILE_CURVES). `visited` is the current
    // resolution path, so its length IS the recursion depth; because a cycle is
    // already rejected above, no curve repeats on a path and this can only trip
    // if a profile carrying more than MAX_PROFILE_CURVES curves reached the
    // engine — i.e. both the validate() and load_profile() caps were bypassed.
    // Falling out as None matches the cycle case: the control is skipped and its
    // fan holds, which is preferable to overflowing the stack and aborting.
    if visited.len() >= crate::profile::MAX_PROFILE_CURVES {
        log::warn!(
            "Mix curve '{}' exceeds the maximum dependency depth of {} — skipping",
            curve.name,
            crate::profile::MAX_PROFILE_CURVES
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
/// Daemon-owned outright since the 2.0.0 cutover (DEC-165); pinned by the
/// multi-control `tuning_sequence` golden vectors (DEC-126).
pub(crate) fn resolve_sync_output(
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
/// single-temperature type to the 2°C deadband path (daemon-owned since the
/// 2.0.0 cutover, DEC-165). Returns None when the control must be skipped
/// this tick (missing sensor, unresolvable composite).
pub(crate) fn curve_output_for_control(
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

/// The control id a Sync-driven control depends on, else None.
pub(crate) fn sync_dependency<'a>(
    control: &'a LogicalControl,
    profile: &'a DaemonProfile,
) -> Option<&'a str> {
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
/// Sync reads a not-yet-computed target and falls back at eval time). The
/// daemon has been the sole evaluator since the 2.0.0 cutover (DEC-165); the
/// multi-control `tuning_sequence` vectors deliberately list controls out of
/// dependency order to pin this sort (DEC-126). Sync-free profiles emit
/// `[0, 1, …, n-1]`.
pub(crate) fn topological_control_order(profile: &DaemonProfile) -> Vec<usize> {
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

pub(crate) fn topo_visit(
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
