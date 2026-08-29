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
    evaluate_curve, member_is_gpu, member_needs_hard_floor, ControlMember, DaemonProfile,
    LogicalControl, HARD_PUMP_CPU_FLOOR_PCT,
};

mod curve_eval;
mod safety_tick;
pub mod skipped;
mod tuning;

pub use crate::health::state::ControlOutput;
pub(crate) use curve_eval::*;
pub(crate) use safety_tick::*;
pub use skipped::{
    SkipEvent, SkipReason, SkipRecord, SkippedControl, SkippedControlTracker, SKIP_DEBOUNCE_TICKS,
};
pub(crate) use tuning::*;

/// A single PWM write command produced by the profile engine.
#[derive(Debug, Clone)]
pub struct PwmCommand {
    pub member_id: String,
    pub source: String, // "openfan", "hwmon", "amd_gpu" (writable); "intel_gpu"/"nvidia_gpu" accepted but read-only — no backend writes them (DEC-121/DEC-204)
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
///
/// EFF-3: per-activation static evaluation plan, cached so the engine does not
/// rebuild it every 1 Hz tick. The topological control order and the
/// `curve_id` → index map depend only on the active profile's structure, which
/// is immutable between activations. Invalidated (set to `None`) the moment the
/// active profile id changes (`sync_profile_id`) or the engine re-anchors on an
/// activation-epoch bump (`deactivate`, called by the tick loop on every
/// `POST /profile/activate`, including same-id re-apply — DEC-188). A `None`
/// cache therefore always means "recompute on next evaluate".
#[derive(Debug)]
struct StaticEvalCache {
    /// Topological control order (indices into `profile.controls`) so Sync
    /// dependency resolution is stable without re-running the DFS each tick.
    order: Vec<usize>,
    /// `curve_id` → index into `profile.curves`, replacing the per-control
    /// linear `curves.iter().find(...)` (O(controls × curves)) with an O(1)
    /// lookup on the hot path.
    curve_index: HashMap<String, usize>,
}

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
    /// DEC-188 steady-state safety valve: consecutive ticks the 2°C deadband has
    /// HELD this control's output. Reset whenever the curve actually
    /// re-evaluates (any in-band rise or a fall past the band). Once it reaches
    /// [`constants::DEADBAND_MAX_HOLD_CYCLES`] the deadband is bypassed for one
    /// tick so a temperature that settled just inside the band re-anchors to its
    /// true curve value instead of holding the pre-settle output forever.
    deadband_hold_cycles: HashMap<String, u32>,
    /// Id of the profile the current state belongs to.
    active_profile_id: Option<String>,
    /// EFF-3: cached topological order + curve index for the active profile.
    /// `None` until the first evaluate after a (re)activation; see
    /// [`StaticEvalCache`].
    static_cache: Option<StaticEvalCache>,
    /// 273-i: controls this tick's evaluation could not resolve. Raw facts,
    /// overwritten by every `evaluate_profile_with_overrides`; the debounce and
    /// the `since` stamps live in `skipped_tracker`, which the tick body feeds
    /// via [`ProfileEngineState::commit_skips`].
    ///
    /// Split this way so the evaluator stays clock-free and testable: it records
    /// WHAT was skipped, the caller decides WHEN that becomes a log line.
    skipped_this_tick: Vec<SkipRecord>,
    /// 273-i: debounced skip state, surfaced on `/status`.
    skipped_tracker: SkippedControlTracker,
    /// 277-k: this tick's applied output per control id, for the `/status` and
    /// `/poll` surfaces. Overwritten by every `evaluate_profile_with_overrides`,
    /// exactly like `skipped_this_tick` above and for the same reason — the
    /// evaluator records the facts, the tick body decides when to publish them.
    ///
    /// This is the *applied* control-wide value whatever drove it, curve or
    /// override, because the question the Controls card answers is "what are the
    /// fans doing?". Distinct from `last_output`, which is the PREVIOUS tick and
    /// is entangled with step-rate limiting (DEC-151), and which an override
    /// deliberately does not advance at all.
    tick_outputs: HashMap<String, f64>,
}

impl ProfileEngineState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed this tick's skipped set into the debounce tracker and return the
    /// transitions to log (273-i).
    ///
    /// Separate from `evaluate_profile_with_overrides` so the evaluator needs no
    /// clock: `now` is the caller's, exactly as `SensorFailureTracker` takes it.
    pub fn commit_skips(&mut self, now: std::time::Instant) -> Vec<SkipEvent> {
        self.skipped_tracker
            .record_tick(&self.skipped_this_tick, now)
    }

    /// Controls currently listed as skipped, sorted by id (273-i).
    pub fn skipped_snapshot(&self) -> Vec<SkippedControl> {
        self.skipped_tracker.snapshot()
    }

    /// This tick's applied output per control, sorted by id (277-k).
    ///
    /// Sorted here rather than at the wire boundary so the ordering is a property
    /// of the value, not of one serialiser — `/status` and `/poll` both render it
    /// and must not disagree.
    pub fn outputs_snapshot(&self) -> Vec<ControlOutput> {
        let mut out: Vec<ControlOutput> = self
            .tick_outputs
            .iter()
            .map(|(control_id, pct)| ControlOutput {
                control_id: control_id.clone(),
                output_pct: *pct,
            })
            .collect();
        out.sort_by(|a, b| a.control_id.cmp(&b.control_id));
        out
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

    /// Drop the cross-tick *tuning* state — deadband anchors, step-rate history,
    /// the trigger latch and the cached eval plan — so the next evaluation
    /// re-anchors fresh, **without forgetting which controls are unresolvable**
    /// (277-i).
    ///
    /// This is the half the thermal-force path actually wants, and separating it
    /// is the whole of 277-i. `force_all` puts the fans at a known duty, so P3-2's
    /// reasoning holds: a resumed curve must not step-rate-clamp from a
    /// pre-emergency anchor. But a thermal emergency says nothing about whether a
    /// control's *curve* resolves, and the full [`Self::deactivate`] also clears
    /// the skip tracker — correct for a profile switch, where the next profile's
    /// controls are genuinely different, and wrong here. The cost of conflating
    /// them was that a 105 °C event published an empty `skipped_controls[]` for
    /// its entire duration and for a further 3-tick debounce after recovery, so
    /// the one surface that says "nothing is commanding these fans" went silent
    /// exactly while an operator was most likely to be reading it.
    ///
    /// Note what this does NOT do: it does not touch `skipped_this_tick` or
    /// `skipped_tracker` at all, so the list simply *freezes* for the duration of
    /// a forced tick. That is the intended reading — a forced tick evaluates no
    /// curves, so it learns nothing new about resolvability, and the last thing
    /// it did learn stays true until it evaluates again.
    fn deactivate_tuning_only(&mut self) {
        self.last_output.clear();
        self.last_curve_output.clear();
        self.last_transition_temp.clear();
        self.trigger_latch.clear();
        self.deadband_hold_cycles.clear();
        self.active_profile_id = None;
        // 277-k: this tick's outputs are facts about an evaluation that is being
        // abandoned, so they go with the tuning state. A forced tick publishes no
        // per-control output — `force_all` drives the fans directly and bypasses
        // every control, so there is no control-wide output to report.
        self.tick_outputs.clear();
        // EFF-3: drop the cached eval plan so a re-anchor (epoch bump, thermal
        // force, or no-profile) rebuilds it against whatever activates next.
        // (Redundant with the sync_profile_id null below — deactivate also
        // clears active_profile_id, so the next evaluate rebuilds anyway — but
        // kept so a deactivated state is internally consistent, not half-cleared.)
        self.static_cache = None;
    }

    /// Reset state to a profile-less state (call when active profile is
    /// cleared). The next `evaluate_profile` call starts fresh.
    ///
    /// Everything [`Self::deactivate_tuning_only`] does, **plus** forgetting the
    /// skip state. Keep the two in this relationship rather than duplicating the
    /// field list: a field added to one and not the other is a half-cleared state,
    /// which is the bug class this split could otherwise introduce.
    pub fn deactivate(&mut self) {
        self.deactivate_tuning_only();
        // 273-i: a deactivation is not a resolution. Clearing rather than
        // draining means no bogus "resumed" line, and a control still
        // unresolvable under whatever activates next is reported afresh —
        // which is what an operator who just switched profiles needs to see.
        //
        // 277-i: this is the half a *thermal force* must not inherit — see
        // `deactivate_tuning_only`, which exists precisely to leave it alone.
        self.skipped_this_tick.clear();
        self.skipped_tracker.clear();
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
        self.deadband_hold_cycles.remove(control_id);
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
            self.deadband_hold_cycles.clear();
            self.active_profile_id = Some(new_id.to_string());
            // EFF-3: a different profile has a different structure → its cached
            // order/curve-index no longer applies. Rebuilt lazily next evaluate.
            self.static_cache = None;
        }
        changed
    }

    /// EFF-3: build the per-activation [`StaticEvalCache`] if absent or stale.
    /// Staleness is double-checked cheaply by control count — a freshly built
    /// cache always has `order.len() == controls.len()`, so an add/remove forces
    /// a rebuild even if an invalidation path was somehow missed. The common
    /// path (steady state on one activation) finds a valid cache and does
    /// nothing.
    fn ensure_static_cache(&mut self, profile: &DaemonProfile) {
        let valid = self
            .static_cache
            .as_ref()
            .is_some_and(|c| c.order.len() == profile.controls.len());
        if !valid {
            let order = topological_control_order(profile);
            let curve_index = profile
                .curves
                .iter()
                .enumerate()
                .map(|(i, c)| (c.id.clone(), i))
                .collect();
            self.static_cache = Some(StaticEvalCache { order, curve_index });
        }
    }

    /// Cached topological control order (EFF-3). Empty until
    /// [`ensure_static_cache`] has run for the active profile.
    fn eval_order(&self) -> &[usize] {
        match &self.static_cache {
            Some(c) => &c.order,
            None => &[],
        }
    }

    /// Cached `curve_id` → `profile.curves` index (EFF-3). `None` for an unknown
    /// curve id or before [`ensure_static_cache`] has run.
    fn curve_index_of(&self, curve_id: &str) -> Option<usize> {
        self.static_cache
            .as_ref()
            .and_then(|c| c.curve_index.get(curve_id).copied())
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
    // EFF-3: (re)build the cached topological order + curve index for this
    // activation (no-op on the steady-state path), then evaluate against the
    // cache instead of rebuilding both every tick.
    engine_state.ensure_static_cache(profile);

    let mut commands = Vec::new();
    // Per-tick control outputs (post-tuning), consumed by Sync curves mirroring
    // an already-evaluated control. Fresh each tick — distinct from
    // `engine_state.last_output`, which is the PREVIOUS tick and entangled with
    // step-rate limiting, so it must not be reused for Sync (DEC-151). Mirrors
    // the GUI's `status.control_outputs`.
    let mut tick_outputs: HashMap<String, f64> = HashMap::new();
    // 273-i: controls this tick could not resolve, with the reason. Collected
    // locally and moved onto `engine_state` at the end so the per-control loop
    // never needs a second mutable borrow of it.
    let mut skipped_this_tick: Vec<SkipRecord> = Vec::new();

    // Evaluate in stable topological order so a Sync control's target is already
    // in `tick_outputs` when the Sync mirrors it (DEC-151). Sync-free profiles
    // keep their natural profile order. The order is read from the cached plan
    // (EFF-3) by position, copying each `usize` out so no borrow of
    // `engine_state` is held across the per-control mutations below.
    let control_count = engine_state.eval_order().len();
    for order_pos in 0..control_count {
        let idx = engine_state.eval_order()[order_pos];
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

        // Determine target output percentage. `Err` → skip this control this
        // tick (manual mode always resolves; curve mode skips on missing curve /
        // sensor / unresolvable composite). The fan then holds its last value.
        //
        // 273-i: a skip now carries WHY, so the tick body can log it once and
        // publish it on `/status` instead of the control going silently
        // uncommanded. Note the classification is computed EAGERLY for every
        // curve-mode control, resolved or not: `Option::ok_or` takes its argument
        // by value, and the `let` below is unconditional. That is deliberate — see
        // the borrow note at the binding — and it costs one `curve_type` match per
        // curve-mode control per tick, not nothing.
        let raw_output: Result<f64, SkipReason> = if control.mode == "manual" {
            Ok(control.manual_output_pct)
        } else {
            // Find the assigned curve (O(1) via the cached curve index, EFF-3),
            // then resolve via the shared dispatcher (deadband / trigger latch /
            // mix / sync) — mirrors the GUI's `_curve_output_for_control` so
            // headless behaviour matches GUI-driven behaviour.
            match engine_state
                .curve_index_of(&control.curve_id)
                .map(|ci| &profile.curves[ci])
            {
                // Classified here rather than in `curve_eval::skip_reason`: this
                // is the only place that knows the curve LOOKUP is what failed.
                None => Err(SkipReason::CurveNotFound),
                Some(curve) => {
                    // Classify before the call, while `curve` is still borrowed
                    // immutably — `curve_output_for_control` takes
                    // `engine_state` mutably and ends this borrow.
                    let reason = curve_eval::skip_reason(curve);
                    curve_output_for_control(
                        control,
                        curve,
                        profile,
                        sensors,
                        &tick_outputs,
                        engine_state,
                    )
                    .ok_or(reason)
                }
            }
        };
        let raw_output = match raw_output {
            Ok(value) => value,
            Err(reason) => {
                skipped_this_tick.push(SkipRecord {
                    control_id: control.id.clone(),
                    control_name: control.name.clone(),
                    reason,
                });
                continue;
            }
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
            // DEC-119 + DEC-162 + DEC-167: each member's effective minimum-PWM
            // floor. GPU members carry no floor (0% — PMFW enforces its own
            // OD_RANGE minimum). A pump/CPU header is hard-floored to at least
            // HARD_PUMP_CPU_FLOOR_PCT even when the control declares a lower
            // `minimum_pct`, AND its stop-snap is skipped (`floor_is_hard`) so a
            // non-zero `stop_pct` can never zero a pump — coolant-flow loss leads
            // to rapid thermal runaway. validate() rejects both shapes at the API
            // boundary (FLOOR_TOO_LOW / PUMP_STOP_FORBIDDEN), but a persisted or
            // hand-edited profile reaches the engine un-validated via
            // `resolve_initial_profile`, so this per-member path is the
            // load-bearing safety net for every reachable input. Every other
            // member uses the control-wide floor. Recompute with a namespaced
            // per-member step-rate tracker when the effective floor differs from
            // the control-wide one (GPU lower-to-0, pump raise-to-floor) OR the
            // member is pump/CPU — so the hard-floor stop-snap exemption applies
            // even when effective_floor == minimum_pct, where the control-wide
            // `pwm_percent` above may already have been snapped to 0. This
            // matches the GUI's per-member flooring (the DEC-096 consistency
            // guarantee); otherwise reuse the control-wide value so the common
            // path stays byte-identical and the parity oracle is unperturbed.
            let effective_floor = member_effective_floor(control, member);
            // DEC-252: same eval-time superset the floor uses. A pump the author
            // renamed must not lose its stop-snap exemption either — that is the
            // half that keeps a non-zero `stop_pct` from zeroing it outright.
            let floor_is_hard = member_needs_hard_floor(member);
            let member_pwm = if effective_floor != control.minimum_pct || floor_is_hard {
                // EFF-4: this per-member step-rate key allocates each tick for
                // pump/CPU/GPU members. Left as-is deliberately — the key scheme
                // `{control_id}::m::{member_id}` is load-bearing for
                // `reset_control`'s prefix sweep, so collapsing it to a composite
                // key or a cached/scratch buffer would touch the whole
                // `last_output` map and the override-clear path for a sub-µs,
                // conditional-path saving. Not worth the hot-loop complexity.
                let key = format!("{}::m::{}", control.id, member.member_id);
                let prev_member = engine_state.last_output(&key);
                let tuned_member = apply_tuning_with_floor(
                    control,
                    raw_output,
                    prev_member,
                    effective_floor,
                    floor_is_hard,
                );
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

    // 273-i: publish this tick's skipped set for the caller to debounce and log.
    // Assigned unconditionally — an empty vec is the signal that every control
    // resolved, and is what retires a previously-listed skip.
    engine_state.skipped_this_tick = skipped_this_tick;

    // 277-k: hand this tick's applied outputs to the caller the same way, and for
    // the same reason — the return type stays `Vec<PwmCommand>`. Widening it to a
    // tuple would churn every caller, including the DEC-126 parity oracle and the
    // `evaluate_profile` wrapper above, for a value only the tick body wants.
    engine_state.tick_outputs = tick_outputs;

    commands
}

/// Stamps the engine's tick-completed timestamp on drop (DEC-259).
///
/// A guard rather than a call at the bottom of the loop body, because that body
/// has several `continue` paths and a shutdown `break`. With a guard, "started
/// but not completed" can only mean the tick is genuinely still running — never
/// that it took an exit somebody forgot to instrument. A panic mid-tick also
/// drops it, which is correct: the task is dead either way and both stamps then
/// freeze together.
struct TickCompletion<'a> {
    cache: &'a StateCache,
    /// 273-i: this tick's skipped-control list, published on drop.
    ///
    /// Carried here rather than published inline because the list is ONE logical
    /// value and must reach the cache in ONE write. Clearing it at the top of the
    /// tick and republishing at the bottom also satisfied "an early `continue`
    /// cannot leave it stale", but opened a window: a `/poll` landing between the
    /// two saw `skipped_controls: []` while a control was genuinely uncommanded.
    /// The engine and the GUI are two free-running 1 Hz clocks whose phases drift
    /// through each other, so that window is hit periodically — the warning would
    /// blink off for one poll at a time.
    ///
    /// Dropping publishes on EVERY exit path (both early `continue`s, the
    /// mid-tick shutdown `break`, and the normal end), so DEC-249's
    /// unreachable-by-construction property is kept and the window is gone. A
    /// tick that evaluated nothing publishes the empty default, which is correct:
    /// nothing was evaluated, so nothing is skipped.
    skipped: Vec<SkippedControl>,
    /// 277-k: this tick's per-control applied outputs, published on drop.
    ///
    /// Carried here for exactly the reasons documented on `skipped` above — it is
    /// ONE logical value, it must reach the cache in ONE write, and every exit
    /// path must publish it or a card keeps rendering a duty the engine has
    /// stopped applying. A tick that evaluated nothing publishes the empty
    /// default, which is correct: nothing was evaluated, so no control has an
    /// output to report.
    outputs: Vec<ControlOutput>,
    /// DEC-289: a backend write issued this tick (or an earlier one) has still
    /// not returned, so this tick has NOT finished its work.
    ///
    /// Bounding the backend joins means the loop no longer freezes behind a
    /// wedged device — but it also means the tick now *completes*, and a
    /// completed tick is exactly what `health::staleness::engine_health` reads to
    /// decide the engine is healthy. Stamping completion here would therefore
    /// have deleted the one signal `/status` already gives an operator for this
    /// condition ("tick still running — a slow write is holding it up", then
    /// "tick stuck"). Withholding the stamp keeps that surface behaving exactly
    /// as it did before the bound, with no new field and no wire change.
    writes_outstanding: bool,
}

impl<'a> TickCompletion<'a> {
    fn new(cache: &'a StateCache) -> Self {
        Self {
            cache,
            skipped: Vec::new(),
            outputs: Vec::new(),
            writes_outstanding: false,
        }
    }

    /// Record that a backend write is still in flight (DEC-289). Suppresses only
    /// the completion stamp — the skipped/outputs publication is unconditional,
    /// because those describe what this tick *decided*, which is true regardless
    /// of whether the write has landed yet.
    fn set_writes_outstanding(&mut self, outstanding: bool) {
        self.writes_outstanding = outstanding;
    }

    /// Record what this tick found. Not published until drop.
    fn set_skipped(&mut self, skipped: Vec<SkippedControl>) {
        self.skipped = skipped;
    }

    /// Record this tick's applied outputs (277-k). Not published until drop.
    fn set_outputs(&mut self, outputs: Vec<ControlOutput>) {
        self.outputs = outputs;
    }
}

impl Drop for TickCompletion<'_> {
    fn drop(&mut self) {
        // ONE call, publishing both fields under one write guard. Two calls made
        // each field individually atomic but left the PAIR torn: a poll landing
        // between them saw a control's new skip entry beside its stale output and
        // listed it on both surfaces at once.
        self.cache.update_control_state(
            std::mem::take(&mut self.skipped),
            std::mem::take(&mut self.outputs),
        );
        self.cache.record_engine_tick_complete();
        // DEC-289: the loop DID complete a pass — that stamp is truthful and
        // stays unconditional. Whether the writes it issued actually landed is a
        // separate question, and it gets its own stamp. Withholding the
        // completion stamp instead was tried and measured wrong: the loop keeps
        // ticking through a wedge, so `engine_started` refreshes every second and
        // the "tick still running" ladder never fires.
        self.cache
            .record_engine_write_stall(self.writes_outstanding);
    }
}

/// What the CPU sensors are telling us right now — and how much to trust it.
///
/// [SAFETY] DEC-267 introduced the freshness filter; DEC-269 made it three-way.
/// Collapsing "stale" into "absent" (a plain `Option`) lost a distinction the
/// safety ladder depends on: a six-second-old reading of 95 C is strong
/// evidence the machine is still hot, whereas a vanished sensor is evidence of
/// nothing. DEC-190 chose `NO_SENSOR_SAFE_PCT` for the *vanished* case
/// deliberately, and routing a stale reading into that same branch silently
/// extended a decision made about one state to a materially different one —
/// dropping a latched 105 C emergency from 100% to 40% on a CPU last seen at
/// 95 C, and flapping between the two as readings crossed the budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CpuReading {
    /// Updated within the freshness budget — safe to drive the rule with.
    Fresh(f64),
    /// Present but not updating. Usable as evidence of what *was* true, never
    /// as evidence of what is true now.
    Stale(f64),
    /// No CpuTemp sensor at all.
    Absent,
}

/// Reduce the CPU sensors to a single reading, preferring fresh over stale.
///
/// Fresh readings win outright: if any CpuTemp is current, stale siblings are
/// ignored entirely (they cannot lower the hottest-fresh value, and they carry
/// no information the fresh one lacks). Only when *nothing* is current does the
/// hottest stale value stand in, so the caller can hold rather than fall back.
///
/// `stale_after` comes from [`StateCache::cpu_temp_stale_after`], i.e. from the
/// configured poll interval — not a constant, because the interval has no upper
/// bound and a fixed budget would mark a legitimately slow system permanently
/// stale.
pub(crate) fn hottest_cpu_reading(
    sensors: &HashMap<String, CachedSensorReading>,
    now: std::time::Instant,
    stale_after: std::time::Duration,
) -> CpuReading {
    let mut hottest_fresh: Option<f64> = None;
    let mut hottest_stale: Option<f64> = None;

    for s in sensors.values().filter(|s| s.kind == SensorKind::CpuTemp) {
        // `saturating_duration_since` because `updated_at` can be marginally in
        // the future relative to `now` across a clock read boundary; that is a
        // fresh reading, not an absent one.
        let target = if now.saturating_duration_since(s.updated_at) <= stale_after {
            &mut hottest_fresh
        } else {
            &mut hottest_stale
        };
        *target = Some(target.map_or(s.value_c, |v: f64| v.max(s.value_c)));
    }

    match (hottest_fresh, hottest_stale) {
        (Some(t), _) => CpuReading::Fresh(t),
        (None, Some(t)) => CpuReading::Stale(t),
        (None, None) => CpuReading::Absent,
    }
}

/// Whether a sensor is current enough to drive a fan CURVE this tick.
///
/// [SAFETY] DEC-272 (register row 01-a). Curve evaluation used to read the raw
/// snapshot while only the safety leg was age-filtered, so a frozen GPU-temp or
/// coolant sensor drove its curve indefinitely under `thermal_state: "normal"` —
/// the same silent-failure shape DEC-267/269 removed from the CPU ladder, one
/// rung out and with no ladder of its own to catch it.
///
/// Dropping a stale sensor makes it *missing*, and `curve_eval` already resolves
/// a missing sensor to `None`, which skips the control for the tick and leaves
/// its fans at the last commanded duty. So "stale" degrades to "hold", never to
/// 0% — the outcome DEC-269 was right to worry about and the reason no new
/// fallback constant is needed here.
///
/// COMPOSITES need more than this predicate to get there, and the way they get
/// there is deliberately NOT "skip the control". As first written this sentence
/// was true of single-sensor curves alone: `resolve_mix` dropped the unresolvable
/// child and combined the survivors, so `max(cpu, gpu)` with a stale GPU sensor
/// became `max(cpu)` and the fan ramped DOWN while blind (measured 100% -> 36%).
/// Skipping the whole Mix instead is worse in the mirror case, because THIS
/// EXEMPTION is what a Mix's surviving CpuTemp child relies on: a fresh 95 C CPU
/// reading stayed in the snapshot while the control issued no command at all.
/// A Mix has one fan set, not one per child.
///
/// So a Mix combines whatever resolved and then refuses to fall while anything is
/// missing (`combine_mix_partial` + the clamp in `curve_output_for_control`).
/// Pinned from both sides by
/// `a_stale_mix_child_holds_the_control_instead_of_lowering_it` and
/// `a_mix_still_climbs_on_a_live_child_when_another_drops_out`.
///
/// `CpuTemp` is DELIBERATELY EXEMPT, and that is not an oversight to tidy up
/// later. The thermal ladder is the sole authority on a stale CPU reading and has
/// already adjudicated both halves: stale-and-cool forces `NO_SENSOR_SAFE_PCT`
/// (DEC-132/190), while stale-and-HOT deliberately suppresses that force and lets
/// the tick fall through to curve evaluation on the frozen value, "which
/// therefore keeps commanding a high duty" (DEC-269 round 2). Filtering CpuTemp
/// here would silently break that second case: a control still climbing under
/// `step_up_pct` would freeze at its current duty instead of continuing toward
/// the hot target — a reduction in cooling caused by going blind, which is
/// precisely what DEC-269 exists to forbid. Pinned by
/// `a_stale_but_hot_cpu_curve_keeps_climbing_while_a_stale_gpu_curve_holds`.
pub(crate) fn curve_eligible(
    sensor: &CachedSensorReading,
    now: std::time::Instant,
    stale_after: std::time::Duration,
) -> bool {
    sensor.kind == SensorKind::CpuTemp
        // `saturating_duration_since` for the same reason `hottest_cpu_reading`
        // uses it: `updated_at` can sit marginally in the future across a clock
        // read boundary, and that is a fresh reading, not an ancient one.
        || now.saturating_duration_since(sensor.updated_at) <= stale_after
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
    // DEC-265: a shared slot, not a value. It can be filled after boot by
    // `POST /fans/openfan/rescan`, and the engine must pick that up — otherwise
    // the route adopts a controller that the sole PWM writer never sees, and the
    // 105 C `force_all` still has no OpenFan leg.
    fan_controller: Arc<
        parking_lot::RwLock<Option<Arc<Mutex<crate::serial::controller::FanController>>>>,
    >,
    hwmon_controller: Option<Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>>,
    gpu_infos: Vec<crate::hwmon::gpu_detect::AmdGpuInfo>,
    safety: Arc<Mutex<crate::safety::ThermalSafetyRule>>,
    override_table: Arc<Mutex<crate::control_override::OverrideTable>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Interval (not `sleep`) so the period is measured tick-to-tick and the
    // per-tick work time is absorbed rather than added (a bare `sleep(1s)` +
    // work yields a `1s + work` period that drifts the 1Hz-calibrated step-rate
    // limiter and falling-temp deadband). `Skip` over-runs without bursting,
    // matching the hwmon/openfan poll loops (`polling.rs`).
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    log::info!("Profile engine started (1Hz)");

    // Per-backend write paths (DEC-135). Each backend owns its own gating —
    // coalescing thresholds, failure caching, lease handling (no GUI deferral
    // as of 2.0.0 — DEC-165).
    // GpuBackend is deliberately NOT a SafetyWriteBackend (DEC-130): there
    // is no GPU emergency threshold.
    let mut openfan_be = fan_controller
        .read()
        .clone()
        .map(|ctrl| OpenFanBackend::new(ctrl, cache.clone()));
    let mut gpu_be = GpuBackend::new(cache.clone(), Arc::new(gpu_infos));
    let mut hwmon_be = hwmon_controller.map(HwmonBackend::new);

    // Track consecutive cycles with no CPU temperature sensor (P0-R1).
    // If no CpuTemp sensor is found for N cycles, force fans to a safe minimum.
    let mut no_cpu_sensor_cycles: u32 = 0;

    // Cross-cycle tuning state for `evaluate_profile`. Cleared when the active
    // profile changes or is deactivated so step-rate limiting and start/stop
    // hysteresis don't leak between unrelated profiles.
    let mut engine_state = ProfileEngineState::new();

    // DEC-188: last profile-activation epoch this loop observed. A bump (any
    // `POST /profile/activate`, including re-activating the *same* id after
    // editing its curve) re-anchors ALL cross-tick state so the new curve takes
    // effect on the next tick instead of being suppressed by the 2°C deadband
    // (DEC-096). Seeded from the current value so a bump that landed before the
    // loop started doesn't trigger a spurious (though harmless) reset.
    let mut last_epoch = cache.profile_activation_epoch();

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            changed = shutdown.changed() => {
                // DEC-265: a dropped Sender must end the loop, not spin it. The
                // `Result` used to be discarded, and `changed()` returns `Err`
                // immediately and forever once every Sender is gone — so this arm
                // would fire continuously with `borrow()` still false, never
                // reaching the tick that paces the loop. A 1 Hz engine becomes a
                // busy loop pinning a core, and the heartbeat would report peak
                // health throughout, because it *is* ticking.
                if changed.is_err() {
                    log::warn!(
                        "Profile engine shutdown channel closed — no sender remains; \
                         stopping the engine"
                    );
                    break;
                }
                if *shutdown.borrow() {
                    log::info!("Profile engine shutting down");
                    break;
                }
            }
        }

        // DEC-265: adopt a controller that appeared after boot. Only checked
        // while there is no backend, so the steady state is one uncontended
        // read-lock acquisition per tick and an adopted backend is never
        // rebuilt underneath its own coalescing/failure caches.
        if openfan_be.is_none() {
            if let Some(ctrl) = fan_controller.read().clone() {
                log::info!(
                    "Profile engine picked up an OpenFan controller adopted after \
                     startup — fan control and the thermal emergency now reach it"
                );
                openfan_be = Some(OpenFanBackend::new(ctrl, cache.clone()));
            }
        }

        // Evaluate thermal safety against the hottest CpuTemp sensor — the
        // max across ALL CpuTemp sensors (AMD Tctl, Intel Package id, etc.)
        // so the rule works on any platform, not just AMD — plus the
        // no-CPU-sensor fallback.
        // DEC-146 P3-6: one sensors snapshot per tick, shared by the safety
        // leg and curve evaluation — halves the per-second map clone and
        // makes the tick internally consistent (both legs see one snapshot).
        let snapshot_taken_at = std::time::Instant::now();
        let mut sensors = cache.sensors_snapshot();
        let stale_after = cache.cpu_temp_stale_after();
        let (decision, hottest_cpu_c) = {
            // DEC-267/269: classify the reading before acting on it. `now` is
            // sampled BEFORE the snapshot above, so any reading written during
            // the gap saturates to age 0 (fresh) rather than being judged late —
            // the bias belongs on the fail-safe side.
            let cpu_reading = hottest_cpu_reading(&sensors, snapshot_taken_at, stale_after);

            let mut safety_guard = safety.lock();
            let decision =
                evaluate_safety_tick(cpu_reading, &mut no_cpu_sensor_cycles, &mut safety_guard);
            (decision, cpu_reading)
        };

        // DEC-272 (01-a): the safety leg above has had the raw snapshot; from here
        // on, curve evaluation sees only sensors current enough to drive a fan.
        // Filtered IN PLACE rather than into a second map so DEC-146 P3-6's
        // one-clone-per-tick property survives — this removes entries, it does not
        // copy them. Applied at the call site, never inside `evaluate_profile`,
        // whose 3-arg form is the parity oracle's surface and must stay
        // byte-identical (`parity_vectors.json`).
        sensors.retain(|_, s| curve_eligible(s, snapshot_taken_at, stale_after));

        // Report thermal safety state for /status (DEC-132) + /diagnostics, and
        // stamp the engine liveness heartbeat in the same write (DEC-249). This
        // line is reached unconditionally once per tick, so a frozen heartbeat
        // means the engine stopped ticking.
        //
        // DEC-266: this is no longer the ONLY signal that the sole PWM writer
        // died — the task is supervised now, and its death restores hardware and
        // exits the process. The heartbeat still matters, because it also
        // distinguishes a *slow* tick from a stopped one, which supervision
        // cannot. Do not conclude from the supervisor's existence that this is
        // redundant, or from this that the supervisor is.
        cache.record_engine_tick(decision.thermal_state);
        // DEC-259: pairs the start stamp above with a completion stamp on every
        // exit from this body. Without the pair a *slow* tick was indistinguishable
        // from a *stopped* engine, and the surface reported the worse of the two —
        // "fan control and thermal safety are stalled" while `force_all` was
        // actively driving the 105°C emergency below.
        let mut tick_done = TickCompletion::new(&cache);
        // DEC-289: publish the backends' write-stall state HERE, before any
        // branch. Every exit from this body — the emergency `continue`, the
        // mid-tick shutdown `break`, the verify skip, the normal end — passes
        // this point, so a wedge cannot be under-reported by a path that simply
        // did not think about it. It reflects what the PREVIOUS tick left
        // outstanding, which is one tick behind; that is immaterial against
        // `engine_health`'s 2x/30x thresholds and is the price of covering every
        // path with one call instead of auditing them all forever.
        tick_done.set_writes_outstanding(
            // DEC-298: report *stalled*, not merely *outstanding*. A tick now
            // re-issues immediately after harvesting, so a device slower than
            // the write budget always has something in flight here — and
            // `record_engine_write_stall` only clears its stamp on `false`, so
            // reporting outstanding-ness would pin `engine_writes_stalled_since`
            // and trip the 30x `crit` "writes wedged" on a device writing every
            // 1.5 s. Per backend, so a wedged hwmon is not masked by a healthy
            // OpenFan.
            openfan_be
                .as_ref()
                .is_some_and(OpenFanBackend::writes_stalled)
                || hwmon_be.as_ref().is_some_and(HwmonBackend::writes_stalled)
                // DEC-299: the third backend, bounded at last.
                || gpu_be.writes_stalled(),
        );

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

            // DEC-269: name the three cases distinctly. "stale" is the one an
            // operator most needs to tell apart — the sensor is still listed,
            // so a log saying "no CPU temp sensor" would contradict the UI.
            let reason = match hottest_cpu_c {
                CpuReading::Fresh(temp) => format!("CPU temp {temp:.1}°C"),
                CpuReading::Stale(temp) => {
                    format!("CPU temp {temp:.1}°C, STALE — the sensor has stopped updating")
                }
                CpuReading::Absent => "no CPU temp sensor".to_string(),
            };
            log::warn!(
                "Thermal safety override: forcing all OpenFan+hwmon fans to \
                 {forced_pct}% ({reason})"
            );
            // P3-2: drop cross-cycle tuning state so post-override
            // evaluation starts fresh instead of step-rate-clamping from a
            // pre-emergency anchor — the fans are physically at
            // `forced_pct`, not at the stale `last_output`.
            //
            // 277-i: the TUNING half only. The full `deactivate()` also clears
            // the skip tracker, which is right for a profile switch and wrong
            // here: an emergency says nothing about whether a control's curve
            // resolves, and inheriting that clear meant this surface published an
            // empty list for the entire 105 °C → 80 °C hold plus a 3-tick
            // debounce blackout after recovery.
            engine_state.deactivate_tuning_only();
            // Publish before the `continue`, so the list survives the event
            // rather than being dropped by `TickCompletion::drop`'s empty
            // default. It FREEZES for the duration — a forced tick evaluates no
            // curves, so it learns nothing new to report.
            //
            // Read a listed control correctly during an event: it means "this
            // control's curve is unresolvable", NOT "this fan is stopped".
            // `force_all` reaches OpenFan channels and writable hwmon headers but
            // excludes GPU fans by design (DEC-130), so a GPU-bound control with
            // an unresolvable curve genuinely is uncommanded throughout — which
            // is precisely the case that must not go silent. The thermal banner
            // already explains the override for everything else.
            tick_done.set_skipped(engine_state.skipped_snapshot());
            // 277-k: outputs are NOT published here, and the asymmetry is
            // deliberate. `force_all` drives the fans directly and bypasses every
            // control, so there is no control-wide output to report — an empty
            // list is the honest answer, and `deactivate_tuning_only` has already
            // cleared them. A card falls back to "—" for the event, which is true.
            continue;
        }

        // Sweep expired override/identify entries on the daemon's own monotonic
        // clock (never a client timestamp) and reset the cross-tick state of any
        // control whose override just lapsed, so it re-anchors to its curve
        // instead of step-rate-clamping from the pin. Then snapshot the live
        // overlay to apply this tick.
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

            // DEC-188: re-anchor on an activation epoch bump. Read under the
            // same `active_profile` lock the handler bumps it under, so the
            // first tick that observes a swapped profile also observes the new
            // epoch — the edited curve re-evaluates this very tick rather than
            // waiting for the temperature to leave the deadband. Re-activating
            // the same id (curve tweak + re-apply) is the path `sync_profile_id`
            // could not catch on its own.
            let epoch = cache.profile_activation_epoch();
            if epoch != last_epoch {
                last_epoch = epoch;
                engine_state.deactivate();
            }

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

        // 273-i: fold this tick's skipped set into the debounce tracker, log the
        // transitions, and publish the current list. Done before the shutdown
        // check below so a control that went unresolvable is still reported on
        // the tick the daemon is asked to stop — the operator stopping the
        // daemon to investigate is exactly who needs the line.
        for event in engine_state.commit_skips(std::time::Instant::now()) {
            match event {
                SkipEvent::Skipped { id, name, reason } => log::warn!(
                    "Control '{name}' ({id}) is NOT being commanded: {} — its fans hold \
                     their last speed until this resolves",
                    reason.describe()
                ),
                SkipEvent::Resumed { id, name } => {
                    log::info!("Control '{name}' ({id}) is being commanded again")
                }
            }
        }
        tick_done.set_skipped(engine_state.skipped_snapshot());
        // 277-k: published beside the skip list, through the same single
        // `TickCompletion::drop` point. Both answer the Controls page's two
        // questions — "is anything commanding this?" and "at what duty?" — and
        // they must not be able to disagree by a tick.
        tick_done.set_outputs(engine_state.outputs_snapshot());

        // If shutdown was signalled while this tick was computing (after the
        // `select!` arm, before the write phase), stop here so the engine does
        // not issue a routine control write that could race
        // `restore_hardware()` on the way out. Defense-in-depth: in normal
        // timing the write is fast, `.await`-joined, and drained before the
        // restore anyway (`shutdown_sequence` awaits this task first). A thermal
        // emergency is handled earlier this tick (force_all + `continue`) and is
        // never suppressed here. The only window this cannot close — a single
        // sysfs/serial write that hangs past `SHUTDOWN_TASK_TIMEOUT`, which
        // `spawn_blocking` cannot cancel — is backstopped by `ExecStopPost`.
        if *shutdown.borrow() {
            log::info!("Profile engine shutting down (mid-tick)");
            break;
        }

        // Apply per backend (DEC-135). Each backend owns its own gating and
        // none holds a controller guard across an .await. The engine is the
        // sole authoritative writer (DEC-165) — no GUI deferral.
        //
        // While a hardware verify (or an OpenFan calibration sweep) holds the
        // write-pause, skip the write phase so its controlled test writes are
        // not overwritten. This loop-level gate handles the steady-state pause;
        // each backend ALSO re-checks the pause in-flight — hwmon refuses to
        // adopt a "verify" lease, GPU re-checks per fan, OpenFan re-checks per
        // channel — to close the race where a verify/calibration begins *after*
        // this gate is read but before the awaited writes land (P2-1 / DEC-191).
        // Thermal safety force_all runs earlier this tick and `continue`s before
        // here, so a verify never suppresses an emergency. Deadman-bounded
        // (DEC-165).
        if !cache.verify_active() {
            if let Some(be) = openfan_be.as_mut() {
                be.apply(&commands).await;
            }
            gpu_be.apply(&commands).await;
            if let Some(be) = hwmon_be.as_mut() {
                be.apply(&commands).await;
            }
        }
        // DEC-289: refresh after the write phase so a wedge starting THIS tick is
        // reported now rather than next tick. Deliberately OUTSIDE the
        // `verify_active` gate above: a verify skips the write phase, and
        // clearing the stall there would reset a genuine wedge's age every time
        // maintenance ran. DEC-299: GPU is now consulted too — its join is bounded
        // as of that change, so a wedge there is reportable. Before it, a wedged
        // GPU write held the loop and there was nothing to report *because*
        // nothing was still running to do the reporting.
        tick_done.set_writes_outstanding(
            // DEC-298: report *stalled*, not merely *outstanding*. A tick now
            // re-issues immediately after harvesting, so a device slower than
            // the write budget always has something in flight here — and
            // `record_engine_write_stall` only clears its stamp on `false`, so
            // reporting outstanding-ness would pin `engine_writes_stalled_since`
            // and trip the 30x `crit` "writes wedged" on a device writing every
            // 1.5 s. Per backend, so a wedged hwmon is not masked by a healthy
            // OpenFan.
            openfan_be
                .as_ref()
                .is_some_and(OpenFanBackend::writes_stalled)
                || hwmon_be.as_ref().is_some_and(HwmonBackend::writes_stalled)
                // DEC-299: the third backend, bounded at last.
                || gpu_be.writes_stalled(),
        );
    }

    // DEC-289: drain any write still in flight before this task ends. Placed
    // after the loop so it covers every `break` above without each one having to
    // remember. `main.rs`'s shutdown drains this task's handle and used to get
    // the backend writes for free — the loop could not end a tick with a write
    // outstanding. Bounding the joins made that possible, so the guarantee has to
    // be restored explicitly here or the restore races a detached write still
    // holding the controller lock.
    let drain_budget = std::time::Duration::from_secs(2);
    if let Some(be) = openfan_be.as_mut() {
        be.drain_writes(drain_budget).await;
    }
    if let Some(be) = hwmon_be.as_mut() {
        be.drain_writes(drain_budget).await;
    }
    // DEC-299: the GPU backend now holds a bounded handle too, so it needs the
    // same drain.
    //
    // The justification is NOT the one this comment first gave. `restore_gpu_fans_to_auto`
    // takes **no** GPU write lock, so "a detached write still holding
    // `lock_gpu_writes`" describes mutual exclusion that does not exist on that
    // path. The real risk is simpler and worse: a detached write still driving
    // PMFW curve points races the restore with nothing arbitrating between them,
    // and this drain is the only mitigation — best-effort past `drain_budget`.
    gpu_be.drain_writes(drain_budget).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::cache::{CPU_TEMP_STALE_CEILING_MS, MAX_SUPERVISABLE_POLL_INTERVAL_MS};
    use crate::health::state::{CachedSensorReading, DeviceLabel};
    use crate::hwmon::types::SensorKind;
    use crate::profile::{ControlMember, CurveConfig, CurvePoint, LogicalControl};
    use std::time::{Duration, Instant};

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
        // Daemon-owned since the 2.0.0 cutover (DEC-165); the `tuning_sequence`
        // golden vectors (DEC-126) pin combine_mix end-to-end.
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

    /// [SAFETY] DEC-272 round 2 — the curve freshness filter must never LOWER a
    /// composite's commanded duty.
    ///
    /// `curve_eligible` (`mod.rs`) drops a stale sensor from the snapshot, and its
    /// own doc promises that makes the control *skip* and its fans *hold*. That
    /// was true only for single-sensor curves: `resolve_mix` used to drop the
    /// unresolvable child and combine the survivors, so `max(cpu, gpu)` with a
    /// frozen GPU sensor became `max(cpu)` and the fan ramped DOWN while the
    /// daemon was blind to the input that had been driving it — a reduction in
    /// cooling caused by going blind, which DEC-269 forbids.
    ///
    /// Measured before the fix: 100% -> 36% in ONE tick. Nothing damped it —
    /// `default_step()` is 100 in both directions, Mix bypasses the 2 C deadband
    /// by design (DEC-150), and the openfan floor is 20%. The safety ladder cannot
    /// catch it either: the CPU sensor is fresh, so no force is in play.
    ///
    /// The presence is asserted BEFORE the absence deliberately. "No command was
    /// issued" passes vacuously against a profile that never commanded anything,
    /// so the fresh arm has to prove the 100% first for the stale arm to mean
    /// what it claims.
    #[test]
    fn a_stale_mix_child_holds_the_control_instead_of_lowering_it() {
        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("c", "mx", "openfan:ch00")],
            curves: vec![
                linear_curve("cpu", "cpu"), // 30→20%, 80→100%: at 40 C → 36%
                linear_curve("gpu", "gpu"), // at 85 C → clamped 100%
                mix_curve("mx", "max", &["cpu", "gpu"]),
            ],
        };
        let now = Instant::now();
        let budget = std::time::Duration::from_secs(5);

        // PRESENCE: both children fresh → the mix rides the hot GPU child at 100%.
        let fresh: std::collections::HashMap<String, CachedSensorReading> = [
            (
                "cpu".to_string(),
                reading_aged("cpu", SensorKind::CpuTemp, 40.0, std::time::Duration::ZERO),
            ),
            (
                "gpu".to_string(),
                reading_aged("gpu", SensorKind::GpuTemp, 85.0, std::time::Duration::ZERO),
            ),
        ]
        .into_iter()
        .collect();
        // ONE state across both ticks — the engine keeps `last_output` per control,
        // and that carry-over is exactly what the never-lower rule stands on. A
        // fresh state per tick would test a machine that does not exist.
        let mut state = ProfileEngineState::new();
        let before = evaluate_profile(&profile, &fresh, &mut state);
        assert_eq!(before.len(), 1, "the fresh arm must command something");
        assert_eq!(
            before[0].pwm_percent, 100,
            "with both children fresh the mix must ride the hot GPU child"
        );

        // ABSENCE: freeze the GPU sensor, then apply the production filter verbatim
        // (`profile_engine/mod.rs` call site).
        let mut stale = fresh.clone();
        stale.insert(
            "gpu".into(),
            reading_aged(
                "gpu",
                SensorKind::GpuTemp,
                85.0,
                std::time::Duration::from_secs(6),
            ),
        );
        stale.retain(|_, s| curve_eligible(s, now, budget));
        assert!(
            !stale.contains_key("gpu") && stale.contains_key("cpu"),
            "the filter must drop the stale GPU sensor and keep the fresh CPU one"
        );

        let after = evaluate_profile(&profile, &stale, &mut state);
        assert_eq!(after.len(), 1, "the control must still be commanded");
        assert!(
            after[0].pwm_percent >= before[0].pwm_percent,
            "losing the hot child must never LOWER the duty: was {}%, now {}% — \
             combining the survivors alone yields 36%, which is the ramp-down \
             DEC-269 forbids",
            before[0].pwm_percent,
            after[0].pwm_percent
        );
    }

    /// [SAFETY] DEC-272 round 2 — the MIRROR of the test above, and the reason
    /// "skip the whole Mix" was the wrong fix for it.
    ///
    /// Here the surviving child is the hot one. `curve_eligible` deliberately
    /// exempts `CpuTemp`, so a single-sensor CPU control keeps climbing toward a
    /// hot target rather than holding — that exemption is load-bearing for
    /// DEC-269 round 2. A Mix has ONE fan set, not one per child, so skipping the
    /// control because a DIFFERENT child dropped out discards the surviving
    /// child's live demand: measured, a fresh 95 C CPU reading sat in the
    /// snapshot while the control issued no command at all and the fan stayed at
    /// its old duty, backstopped only by the 105 C emergency.
    ///
    /// The CPU here is FRESH. No staleness of the CPU is needed to reach it.
    #[test]
    fn a_mix_still_climbs_on_a_live_child_when_another_drops_out() {
        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("c", "mx", "openfan:ch00")],
            curves: vec![
                linear_curve("cpu", "cpu"),
                linear_curve("gpu", "gpu"),
                mix_curve("mx", "max", &["cpu", "gpu"]),
            ],
        };
        let now = Instant::now();
        let budget = std::time::Duration::from_secs(5);
        let mut state = ProfileEngineState::new();

        // Tick 1: everything cool and fresh → a low duty to climb away from.
        let cool: std::collections::HashMap<String, CachedSensorReading> = [
            (
                "cpu".to_string(),
                reading_aged("cpu", SensorKind::CpuTemp, 40.0, std::time::Duration::ZERO),
            ),
            (
                "gpu".to_string(),
                reading_aged("gpu", SensorKind::GpuTemp, 40.0, std::time::Duration::ZERO),
            ),
        ]
        .into_iter()
        .collect();
        let before = evaluate_profile(&profile, &cool, &mut state);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].pwm_percent, 36, "40 C on 30→20/80→100");

        // Tick 2: CPU is FRESH and hot; the GPU sensor has aged out.
        let mut hot = cool.clone();
        hot.insert(
            "cpu".into(),
            reading_aged("cpu", SensorKind::CpuTemp, 95.0, std::time::Duration::ZERO),
        );
        hot.insert(
            "gpu".into(),
            reading_aged(
                "gpu",
                SensorKind::GpuTemp,
                40.0,
                std::time::Duration::from_secs(6),
            ),
        );
        hot.retain(|_, s| curve_eligible(s, now, budget));
        assert!(
            hot.contains_key("cpu") && !hot.contains_key("gpu"),
            "precondition: a live CPU reading and no GPU one"
        );

        let after = evaluate_profile(&profile, &hot, &mut state);
        assert_eq!(
            after.len(),
            1,
            "a live 95 C CPU child must still command the fan — issuing nothing \
             leaves it at {}% with a hot reading in hand",
            before[0].pwm_percent
        );
        assert_eq!(
            after[0].pwm_percent, 100,
            "the surviving child must drive the fan to its target"
        );
    }

    /// [SAFETY] DEC-272 round 2, the sharpest edge of the same defect.
    ///
    /// `combine_mix` computes `subtract` as `values[0] - values[1..].sum()`. While
    /// unresolvable children were dropped, losing the FIRST child re-indexed the
    /// vector and silently promoted the second child to minuend — so `A - B`
    /// became a bare `B`, and the commanded duty could JUMP rather than fall.
    /// Measured before the fix: 0% -> 36%.
    #[test]
    fn a_stale_first_mix_child_does_not_reindex_subtract() {
        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("c", "mx", "openfan:ch00")],
            curves: vec![
                linear_curve("gpu", "gpu"), // first child, at 85 C → 100%
                linear_curve("cpu", "cpu"), // second child, at 40 C → 36%
                mix_curve("mx", "subtract", &["gpu", "cpu"]),
            ],
        };
        let now = Instant::now();
        let budget = std::time::Duration::from_secs(5);

        // PRESENCE: 100 - 36 = 64.
        let fresh: std::collections::HashMap<String, CachedSensorReading> = [
            (
                "cpu".to_string(),
                reading_aged("cpu", SensorKind::CpuTemp, 40.0, std::time::Duration::ZERO),
            ),
            (
                "gpu".to_string(),
                reading_aged("gpu", SensorKind::GpuTemp, 85.0, std::time::Duration::ZERO),
            ),
        ]
        .into_iter()
        .collect();
        let mut state = ProfileEngineState::new();
        let before = evaluate_profile(&profile, &fresh, &mut state);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].pwm_percent, 64, "subtract: 100 - 36");

        // ABSENCE: the MINUEND goes stale.
        let mut stale = fresh.clone();
        stale.insert(
            "gpu".into(),
            reading_aged(
                "gpu",
                SensorKind::GpuTemp,
                85.0,
                std::time::Duration::from_secs(6),
            ),
        );
        stale.retain(|_, s| curve_eligible(s, now, budget));

        let after = evaluate_profile(&profile, &stale, &mut state);
        assert!(
            after.is_empty() || after[0].pwm_percent >= before[0].pwm_percent,
            "losing the minuend must never promote the subtrahend into its place — \
             `A - B` becoming a bare `B` changed 0% into 36%. Holding or rising is \
             acceptable; falling is not. got {:?}",
            after.iter().map(|c| c.pwm_percent).collect::<Vec<_>>()
        );
    }

    // ── 273-i: a control the engine cannot resolve is no longer silent ──

    /// A Mix naming a curve id the profile does not have is unresolvable
    /// forever — the canonical "config error, fan silently stops responding"
    /// case this row exists for. It must be recorded with a reason.
    #[test]
    fn a_mix_naming_a_missing_child_curve_is_recorded_as_skipped() {
        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "mx", "openfan:ch00")],
            // "deleted" is not among the profile's curves.
            curves: vec![mix_curve("mx", "max", &["deleted"])],
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);

        assert!(cmds.is_empty(), "an unresolvable Mix commands nothing");
        assert_eq!(
            state.skipped_this_tick,
            vec![SkipRecord {
                control_id: "ctl".into(),
                control_name: "ctl".into(),
                reason: SkipReason::MixUnresolvable,
            }],
            "the skip must be recorded WITH a reason — an empty list here is the \
             defect this row describes: the fan stops and nothing says why"
        );
    }

    /// A control whose own `curve_id` is missing is a different fault from a Mix
    /// whose CHILD is missing, and an operator needs to tell them apart.
    #[test]
    fn a_control_naming_a_missing_curve_is_recorded_as_curve_not_found() {
        let profile = DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "gone", "openfan:ch00")],
            curves: vec![linear_curve("present", "cpu")],
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();
        evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);

        assert_eq!(
            state.skipped_this_tick.len(),
            1,
            "the control must be recorded as skipped"
        );
        assert_eq!(
            state.skipped_this_tick[0].reason,
            SkipReason::CurveNotFound,
            "a missing curve must not be reported as a missing sensor"
        );
    }

    #[test]
    fn a_control_whose_sensor_is_absent_is_recorded_as_sensor_unavailable() {
        let profile = DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "c", "openfan:ch00")],
            curves: vec![linear_curve("c", "ghost")],
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();
        evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);

        assert_eq!(state.skipped_this_tick.len(), 1);
        assert_eq!(
            state.skipped_this_tick[0].reason,
            SkipReason::SensorUnavailable
        );
    }

    /// The healthy case: nothing skipped means an EMPTY list, which is what
    /// retires a previously-listed skip. A test that only ever asserted the
    /// non-empty direction would pass with the list wired to always report.
    #[test]
    fn a_control_that_resolves_records_no_skip() {
        let profile = DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "c", "openfan:ch00")],
            curves: vec![linear_curve("c", "cpu")],
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);

        assert!(!cmds.is_empty(), "a resolvable control must command");
        assert!(
            state.skipped_this_tick.is_empty(),
            "a healthy profile must report nothing skipped"
        );
    }

    /// Reaching `/status` takes the debounce as well as the record, so pin the
    /// whole path: record → commit → snapshot.
    #[test]
    fn a_persistent_skip_reaches_the_status_snapshot_after_the_debounce() {
        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "mx", "openfan:ch00")],
            curves: vec![mix_curve("mx", "max", &["deleted"])],
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();
        let now = Instant::now();

        let mut events = Vec::new();
        for _ in 0..SKIP_DEBOUNCE_TICKS {
            evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
            events.extend(state.commit_skips(now));
        }

        assert_eq!(events.len(), 1, "exactly one journal line, on entry");
        let snapshot = state.skipped_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].control_id, "ctl");
        assert_eq!(snapshot[0].reason, SkipReason::MixUnresolvable);
    }

    /// [SAFETY] 273-i / DEC-249 — an early-exit tick publishes the ENGINE'S
    /// current view, never whatever the cache happened to be holding.
    ///
    /// **Reworded by 277-i, which narrowed the rule this test pins.** It used to
    /// be called "…clears the skipped list" and read as though a thermal
    /// emergency always empties it. That is no longer true and, read as a general
    /// rule, was always too strong: what an early-exit tick guarantees is that
    /// `TickCompletion::drop` publishes the engine's own tracker, so nothing stale
    /// survives *that the engine does not still believe*. Here the tracker is
    /// empty — a fresh `ProfileEngineState` that has evaluated nothing — so the
    /// published list is empty and the value seeded directly into the cache is
    /// correctly overwritten.
    ///
    /// When the engine HAS tracked a skip through real evaluation, that skip now
    /// survives the emergency instead: see
    /// `a_thermal_emergency_does_not_erase_the_skipped_list`. The two are not in
    /// tension — this one is about a cache value the engine never put there.
    ///
    /// The mechanism is still the single publish in `TickCompletion::drop`, which
    /// every exit path runs through — NOT a clear at the top of the tick, which
    /// was tried and withdrawn for opening a torn-read window. A maintainer
    /// debugging a failure here who goes looking for a top-of-tick clear will
    /// re-open that window and red
    /// `the_skipped_list_is_published_exactly_once_per_tick`.
    #[tokio::test]
    async fn a_thermal_emergency_tick_drops_a_stale_cached_skip() {
        let cache = make_cache_with_sensor("cpu", 110.0); // over the 105 °C line
        cache.update_skipped_controls(vec![SkippedControl {
            control_id: "stale".into(),
            control_name: "Stale".into(),
            reason: SkipReason::MixUnresolvable,
            since: Instant::now(),
        }]);
        assert!(
            !cache.read_with(|s| s.skipped_controls.is_empty()),
            "precondition: the list starts non-empty"
        );

        run_one_engine_tick(cache.clone(), None).await;

        // Assert the PRESENCE before the absence: without this the test would
        // still pass if the emergency never fired, silently degrading into a
        // duplicate of the no-profile case below (DEC-272's vacuous-absence
        // trap).
        assert_eq!(
            cache.read_with(|s| s.thermal_override_state.clone()),
            Some("emergency".to_string()),
            "precondition: this tick must actually take the thermal-force path"
        );
        assert!(
            cache.read_with(|s| s.skipped_controls.is_empty()),
            "an early-exit tick must publish the engine's own (here empty) view, \
             not leave a cached value the engine never recorded asserting a stale \
             claim"
        );
    }

    /// The other early exit: no profile loaded. Same reasoning.
    #[tokio::test]
    async fn a_no_profile_tick_clears_the_skipped_list() {
        let cache = make_cache_with_sensor("cpu", 40.0); // nowhere near emergency
        cache.update_skipped_controls(vec![SkippedControl {
            control_id: "stale".into(),
            control_name: "Stale".into(),
            reason: SkipReason::CurveNotFound,
            since: Instant::now(),
        }]);

        run_one_engine_tick(cache.clone(), None).await;

        assert!(
            cache.read_with(|s| s.skipped_controls.is_empty()),
            "with no profile active nothing is skipped, because nothing is evaluated"
        );
    }

    /// [SAFETY-adjacent] 273-i — the skipped list must reach the cache in ONE
    /// write per tick.
    ///
    /// The first version cleared it at the top of the tick and republished at the
    /// bottom. That satisfied "an early `continue` cannot leave it stale", but a
    /// `/poll` landing between the two saw `skipped_controls: []` while a control
    /// was genuinely uncommanded — and since the engine and the GUI are two
    /// free-running 1 Hz clocks whose phases drift through each other, that
    /// window is hit periodically rather than never. The warning would blink off
    /// for one poll at a time, which is worse than a warning that stays on.
    ///
    /// Behaviour cannot see a window that only exists between two statements, so
    /// this asserts the structural property instead: exactly one call site, in
    /// the guard's `drop`. Same tool and reasoning as `polling.rs`'s biased pin.
    #[test]
    fn the_skipped_list_is_published_exactly_once_per_tick() {
        let whole = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/profile_engine/mod.rs"
        ));
        // Production half only — otherwise this matches its own literals, which
        // is how the polling.rs version of this guard first passed.
        let src = whole
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("mod.rs has a #[cfg(test)] module");

        let calls = src.matches("update_control_state(").count();
        assert_eq!(
            calls, 1,
            "the control state must reach the cache through exactly ONE call — a \
             second one means it is published in two phases again, and a poll \
             landing between them reports nothing skipped while a fan is \
             uncommanded (found {calls})"
        );
        assert!(
            src.contains("impl Drop for TickCompletion")
                && src[src.find("impl Drop for TickCompletion").unwrap()..]
                    .contains("update_control_state"),
            "the one call must live in TickCompletion::drop, so every exit path \
             from the tick publishes exactly once"
        );
        // 277-k, DEC-279 round 2: the skipped list and the outputs must also
        // travel TOGETHER. Publishing them through two single-field methods would
        // satisfy every assertion above while re-opening a cross-field torn read
        // — a control appearing in both arrays at once, which `docs/08` says
        // cannot happen. Naming the single-field publishers here is what makes
        // that regression loud instead of silent.
        for banned in ["update_skipped_controls(", "update_control_outputs("] {
            assert!(
                !src.contains(banned),
                "the tick must not publish either field on its own ({banned}) — \
                 both go through update_control_state so a reader can never see \
                 one updated and the other stale"
            );
        }
    }

    /// The guard publishes an EMPTY list when the tick set nothing — which is
    /// what makes an early `continue` correct rather than merely tolerated.
    #[test]
    fn the_tick_guard_publishes_empty_when_nothing_was_evaluated() {
        let cache = Arc::new(StateCache::new());
        cache.update_skipped_controls(vec![SkippedControl {
            control_id: "stale".into(),
            control_name: "Stale".into(),
            reason: SkipReason::MixUnresolvable,
            since: Instant::now(),
        }]);

        drop(TickCompletion::new(&cache));

        assert!(
            cache.read_with(|s| s.skipped_controls.is_empty()),
            "a tick that recorded nothing must retire the previous list"
        );
    }

    #[test]
    fn the_tick_guard_publishes_what_the_tick_recorded() {
        let cache = Arc::new(StateCache::new());
        {
            let mut guard = TickCompletion::new(&cache);
            guard.set_skipped(vec![SkippedControl {
                control_id: "ctl".into(),
                control_name: "Ctl".into(),
                reason: SkipReason::SyncUnresolvable,
                since: Instant::now(),
            }]);
            assert!(
                cache.read_with(|s| s.skipped_controls.is_empty()),
                "nothing is published before the guard drops — that single write \
                 at the end is what removes the torn-read window"
            );
        }
        assert_eq!(cache.read_with(|s| s.skipped_controls.len()), 1);
    }

    /// [SAFETY-adjacent] 273-i — the END-TO-END path, which nothing else covers.
    ///
    /// Every other test here stops one step short: the tracker tests drive the
    /// tracker, the evaluator tests read `engine_state`, and the two early-exit
    /// loop tests only ever assert the list is EMPTY. Deleting
    /// `tick_done.set_skipped(...)` from the tick body would leave all of them
    /// green while the feature published nothing — the "extracting a rule does
    /// not test the call site" trap this project has hit five times.
    ///
    /// So this runs the real loop against a real profile with an unresolvable
    /// Mix and asserts the real cache ends up carrying it. It costs ~3.5 s of
    /// wall clock because the debounce is three 1 Hz ticks and the tracker's
    /// threshold is not injectable from here. That is the price of testing the
    /// thing rather than a proxy for it, and it is one test.
    #[tokio::test]
    async fn a_real_tick_publishes_a_skipped_control_to_the_cache() {
        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "mx", "openfan:ch00")],
            // "deleted" is not among the profile's curves → never resolvable.
            curves: vec![mix_curve("mx", "max", &["deleted"])],
        };
        let cache = make_cache_with_sensor("cpu", 40.0); // no thermal emergency

        let observed = cache.clone();
        run_engine_ticks_until(
            cache.clone(),
            Some(profile),
            SKIP_DEBOUNCE_TICKS,
            move || observed.read_with(|s| !s.skipped_controls.is_empty()),
        )
        .await;

        let skipped = cache.read_with(|s| s.skipped_controls.clone());
        assert_eq!(
            skipped.len(),
            1,
            "after the debounce the cache must carry the skipped control — this \
             is the only assertion that the tick body actually hands its findings \
             to the guard"
        );
        assert_eq!(skipped[0].control_id, "ctl");
        assert_eq!(skipped[0].reason, SkipReason::MixUnresolvable);
    }

    /// [SAFETY-adjacent] 277-i — the skipped list must survive a thermal event.
    ///
    /// The forced path called the full `deactivate()`, which also clears the skip
    /// tracker, and then `continue`d before the publish — so `TickCompletion::drop`
    /// published its empty default for the ENTIRE 105 °C → 80 °C hold, plus a
    /// fresh 3-tick debounce blackout on recovery. The one surface that says
    /// "nothing is commanding these fans" went silent exactly while an operator
    /// was most likely to be reading it, and the GUI chip blinked off.
    ///
    /// This must be driven as a TRANSITION. A cache seeded hot from the start
    /// never lists a skip in the first place (the debounce never completes), so
    /// it would assert nothing at all — which is why the temperature is raised
    /// from inside `ready`: that is the only hook this harness offers to change
    /// the world mid-run, and the change is the whole subject of the test.
    #[tokio::test]
    async fn a_thermal_emergency_does_not_erase_the_skipped_list() {
        use std::sync::atomic::{AtomicU8, Ordering};

        let profile = DaemonProfile {
            id: "mix".into(),
            name: "Mix".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "mx", "openfan:ch00")],
            curves: vec![mix_curve("mx", "max", &["deleted"])],
        };
        let cache = make_cache_with_sensor("cpu", 40.0);

        let observed = cache.clone();
        let phase = Arc::new(AtomicU8::new(0));
        let p = phase.clone();
        run_engine_ticks_until(
            cache.clone(),
            Some(profile),
            // +2: let the base sleep cover the debounce, so the whole shared poll
            // budget is left for the transition and the emergency. With the bare
            // debounce count all three had to fit one 5 s budget — fine on an idle
            // machine, a flaky red on a saturated runner, and a flaky red is a
            // test someone eventually deletes.
            SKIP_DEBOUNCE_TICKS + 2,
            move || {
                if p.load(Ordering::SeqCst) == 0 {
                    if observed.read_with(|s| !s.skipped_controls.is_empty()) {
                        // Listed. Now push the CPU past the 105 °C threshold.
                        observed.update_sensors(vec![CachedSensorReading {
                            id: "cpu".into(),
                            kind: SensorKind::CpuTemp,
                            label: "Tctl".into(),
                            value_c: 110.0,
                            source: DeviceLabel::Hwmon,
                            updated_at: Instant::now(),
                            rate_c_per_s: None,
                            session_min_c: None,
                            session_max_c: None,
                            chip_name: "k10temp".into(),
                            temp_type: None,
                            thresholds: None,
                        }]);
                        p.store(1, Ordering::SeqCst);
                    }
                    return false;
                }
                // Wait until the engine has actually taken the forced path, so the
                // assertion below is about an emergency and not about a tick that
                // simply had not noticed the heat yet.
                observed.read_with(|s| s.thermal_override_state.as_deref() == Some("emergency"))
            },
        )
        .await;

        assert_eq!(
            cache.read_with(|s| s.thermal_override_state.clone()),
            Some("emergency".to_string()),
            "precondition: the engine must actually be in the forced path, or \
             this test asserts nothing about emergencies"
        );
        let skipped = cache.read_with(|s| s.skipped_controls.clone());
        assert_eq!(
            skipped.len(),
            1,
            "the skipped list must survive the emergency — a forced tick \
             evaluates no curves, so it learns nothing new about resolvability \
             and must not discard what it already knew"
        );
        assert_eq!(skipped[0].control_id, "ctl");
    }

    /// 277-k — the call site, on the same terms as the skipped-list test above.
    ///
    /// `outputs_snapshot()` can be perfect and the feature still publish nothing:
    /// deleting `tick_done.set_outputs(...)` from the tick body leaves every unit
    /// test of the snapshot green. So this drives the real loop and asserts the
    /// real cache carries the value.
    #[tokio::test]
    async fn a_real_tick_publishes_control_outputs_to_the_cache() {
        let profile = DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "lin", "openfan:ch00")],
            curves: vec![linear_curve("lin", "cpu")],
        };
        let cache = make_cache_with_sensor("cpu", 40.0);

        let observed = cache.clone();
        run_engine_ticks_until(cache.clone(), Some(profile), 1, move || {
            observed.read_with(|s| !s.control_outputs.is_empty())
        })
        .await;

        let outputs = cache.read_with(|s| s.control_outputs.clone());
        assert_eq!(
            outputs.len(),
            1,
            "the tick body must hand its per-control outputs to the guard — \
             without this the live Controls card has no output feed at all"
        );
        assert_eq!(outputs[0].control_id, "ctl");
        assert!(
            outputs[0].output_pct > 0.0,
            "a 40 °C linear curve must produce a real duty, not a placeholder: {:?}",
            outputs[0]
        );
    }

    /// 277-k — a forced tick publishes NO output, and that is the honest answer.
    ///
    /// `force_all` drives the fans directly and bypasses every control, so there
    /// is no control-wide output to report. Publishing the pre-emergency value
    /// would have a card confidently display a duty nothing is applying.
    ///
    /// **Driven as a TRANSITION, and that is the whole point of the test.** The
    /// first version seeded the cache hot at 110 °C, so no normal evaluation ever
    /// ran and `control_outputs` was empty because nothing had ever populated it
    /// — the DEC-272 vacuous-absence trap. It could not have caught the
    /// regression it names: if the forced path republished the pre-emergency
    /// duty, there was no pre-emergency duty to republish, so it passed either
    /// way. Establishing the value FIRST is what gives the absence meaning.
    #[tokio::test]
    async fn a_thermal_emergency_publishes_no_control_output() {
        use std::sync::atomic::{AtomicU8, Ordering};

        let profile = DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("ctl", "lin", "openfan:ch00")],
            curves: vec![linear_curve("lin", "cpu")],
        };
        let cache = make_cache_with_sensor("cpu", 40.0); // nowhere near emergency

        let observed = cache.clone();
        let phase = Arc::new(AtomicU8::new(0));
        let p = phase.clone();
        let saw_output = Arc::new(AtomicU8::new(0));
        let saw = saw_output.clone();
        run_engine_ticks_until(cache.clone(), Some(profile), 1, move || {
            if p.load(Ordering::SeqCst) == 0 {
                if observed.read_with(|s| !s.control_outputs.is_empty()) {
                    // A real duty is on the wire. Record that we saw it — the
                    // assertion below cannot check it afterwards, because the
                    // whole point is that the emergency clears it.
                    saw.store(1, Ordering::SeqCst);
                    observed.update_sensors(vec![CachedSensorReading {
                        id: "cpu".into(),
                        kind: SensorKind::CpuTemp,
                        label: "Tctl".into(),
                        value_c: 110.0,
                        source: DeviceLabel::Hwmon,
                        updated_at: Instant::now(),
                        rate_c_per_s: None,
                        session_min_c: None,
                        session_max_c: None,
                        chip_name: "k10temp".into(),
                        temp_type: None,
                        thresholds: None,
                    }]);
                    p.store(1, Ordering::SeqCst);
                }
                return false;
            }
            observed.read_with(|s| s.thermal_override_state.as_deref() == Some("emergency"))
        })
        .await;

        // Assert the PRESENCE before the absence (DEC-272).
        assert_eq!(
            saw_output.load(Ordering::SeqCst),
            1,
            "precondition: a real per-control output must have been published \
             BEFORE the emergency, or the emptiness asserted below proves nothing"
        );
        assert_eq!(
            cache.read_with(|s| s.thermal_override_state.clone()),
            Some("emergency".to_string()),
            "precondition: the forced path must actually have run"
        );
        assert!(
            cache.read_with(|s| s.control_outputs.is_empty()),
            "a forced tick evaluates no control, so it has no control-wide output \
             to report — republishing the pre-emergency duty would be a card lying \
             about what the fans are doing"
        );
    }

    /// Drive `profile_engine_loop` for exactly one tick against no hardware.
    ///
    /// `tokio::time::interval` fires immediately, so the first tick runs at once;
    /// the shutdown signal is then set and the loop exits at the next select.
    /// Real time rather than `start_paused`, because DEC-272 records that paused
    /// time does not advance `std::time::Instant` — which is what every age in
    /// this engine is measured against.
    async fn run_one_engine_tick(cache: Arc<StateCache>, profile: Option<DaemonProfile>) {
        run_engine_ticks(cache, profile, 1).await;
    }

    /// As above, but let `ticks` of the 1 Hz loop complete before stopping.
    async fn run_engine_ticks(cache: Arc<StateCache>, profile: Option<DaemonProfile>, ticks: u32) {
        run_engine_ticks_until(cache, profile, ticks, || true).await;
    }

    /// As `run_engine_ticks`, but wait — bounded — until `ready` observes what the
    /// caller is about to assert, instead of trusting a fixed wall-clock margin.
    ///
    /// The old form slept `200 + 1000*(ticks-1)` ms and assumed the Nth tick's
    /// guard had dropped by then. On a loaded runner a final tick delayed past
    /// 200 ms turns `a_real_tick_publishes_a_skipped_control_to_the_cache` — the
    /// ONE test that pins the tick body actually handing its findings to the guard
    /// — red for a reason that has nothing to do with the code, and a flaky guard
    /// is a guard someone eventually deletes. Callers asserting an ABSENCE pass
    /// `|| true`: there is nothing to wait for, and polling for "still empty"
    /// would prove nothing.
    async fn run_engine_ticks_until(
        cache: Arc<StateCache>,
        profile: Option<DaemonProfile>,
        ticks: u32,
        ready: impl Fn() -> bool,
    ) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(profile_engine_loop(
            cache,
            Arc::new(Mutex::new(profile)),
            Arc::new(parking_lot::RwLock::new(None)),
            None,
            Vec::new(),
            Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new())),
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            rx,
        ));
        // The interval fires immediately, then once a second. `saturating_sub`
        // rather than `- 1`: `ticks == 0` would underflow and sleep ~584 million
        // years in debug... or panic. No caller passes 0 today.
        tokio::time::sleep(std::time::Duration::from_millis(
            200 + 1000 * u64::from(ticks.saturating_sub(1)),
        ))
        .await;
        // Then poll, bounded, for what the caller actually cares about.
        //
        // Bounded by ITERATION COUNT, not by a wall-clock deadline. The obvious
        // form — `std::time::Instant::now() < deadline` around a
        // `tokio::time::sleep` — mixes two clocks, and `tokio::time::pause` moves
        // only one of them: each sleep auto-advances virtual time instantly while
        // `std::time::Instant` barely moves, so the loop hot-spins for five
        // seconds of REAL time, advancing virtual time by hours and running
        // thousands of engine ticks before returning a meaningless answer. (Not a
        // hang — an earlier version of this comment said "spins forever and CI
        // HANGS", which was wrong about the mechanism. A bogus green is worse.)
        // DEC-272 trap 1, and it would be armed by any future caller adding
        // `start_paused`, not by anything visible here.
        //
        // The counter fixes THAT. It does not make the helper `start_paused`-safe
        // in general: trap 2 still applies — an outstanding `spawn_blocking`
        // suspends auto-advance, so a sleep never completes and the counter never
        // advances. Unreachable today only because this harness runs with no
        // backend attached. Do not read the counter as a general all-clear.
        for _ in 0..200 {
            if ready() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let _ = tx.send(true);
        // Bounded: a hung join would turn a red test into a hung CI job
        // (DEC-272 trap 3).
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    #[test]
    fn skip_reason_classifies_each_curve_shape_distinctly() {
        let mix = mix_curve("m", "max", &["a"]);
        let sync = sync_curve("s", "target", 0.0);
        let linear = linear_curve("l", "cpu");
        assert_eq!(curve_eval::skip_reason(&mix), SkipReason::MixUnresolvable);
        assert_eq!(curve_eval::skip_reason(&sync), SkipReason::SyncUnresolvable);
        assert_eq!(
            curve_eval::skip_reason(&linear),
            SkipReason::SensorUnavailable
        );
    }

    /// [SAFETY-adjacent] 273-i — a curve type added to the dispatcher but not to
    /// the classifier would be misreported, not caught.
    ///
    /// `curve_output_for_control` and `skip_reason` are two matches over the same
    /// vocabulary. Nothing links them, so a new type added to the first silently
    /// falls into the second's `_` arm and every skip of that type reports
    /// "sensor unavailable" regardless of the real cause — a wrong answer is
    /// worse here than no answer, because it sends the operator to the wrong
    /// place.
    ///
    /// The exemption list is the mechanism, in the same shape as the GUI's
    /// `test_the_runtime_text_exemption_is_not_quietly_widening`: a type may fold
    /// into the default, but only deliberately and in writing.
    #[test]
    fn skip_reason_covers_every_curve_type_the_dispatcher_handles() {
        /// Types whose ONLY `None` path is a missing sensor, so the classifier's
        /// default arm is the correct answer for them. Adding to this list is a
        /// claim that must be true of the dispatcher arm.
        const FOLDS_INTO_SENSOR_UNAVAILABLE: &[&str] = &["trigger"];

        let src = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/profile_engine/curve_eval.rs"
        ));

        /// Literal match arms (`"name" =>`) inside one top-level fn body.
        fn arms_of(src: &str, func: &str) -> std::collections::BTreeSet<String> {
            let start = src
                .find(func)
                .unwrap_or_else(|| panic!("{func} not found in curve_eval.rs"));
            // rustfmt puts a top-level fn's closing brace at column 0.
            let len = src[start..]
                .find("\n}\n")
                .expect("unterminated function body");
            let body = &src[start..start + len];
            body.match_indices("\" =>")
                .filter_map(|(i, _)| {
                    let head = &body[..i];
                    head.rfind('"').map(|q| head[q + 1..].to_string())
                })
                .collect()
        }

        let dispatcher = arms_of(src, "fn curve_output_for_control");
        let classifier = arms_of(src, "fn skip_reason");

        assert!(
            !dispatcher.is_empty() && !classifier.is_empty(),
            "the arm scanner found nothing — it has stopped testing anything \
             (dispatcher: {dispatcher:?}, classifier: {classifier:?})"
        );

        let unclassified: Vec<&String> = dispatcher
            .difference(&classifier)
            .filter(|t| !FOLDS_INTO_SENSOR_UNAVAILABLE.contains(&t.as_str()))
            .collect();
        assert!(
            unclassified.is_empty(),
            "curve type(s) {unclassified:?} are dispatched but not classified — every \
             skip of one would be reported as 'sensor_unavailable' whatever the real \
             cause. Give each an arm in `skip_reason`, or add it to \
             FOLDS_INTO_SENSOR_UNAVAILABLE if a missing sensor really is its only \
             unresolvable path"
        );

        let stale: Vec<&&str> = FOLDS_INTO_SENSOR_UNAVAILABLE
            .iter()
            .filter(|t| !dispatcher.contains(**t))
            .collect();
        assert!(
            stale.is_empty(),
            "exemption(s) {stale:?} name curve types the dispatcher no longer has — \
             an exemption list that outlives its subject quietly widens"
        );
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
    fn mix_deep_acyclic_chain_does_not_overflow_the_stack() {
        // Regression: a LONG but perfectly ACYCLIC mix chain (c0→c1→…→cN) is a
        // legal DAG, so cycle detection never fires — the engine simply recursed
        // once per link. At ~3k links that overflowed the stack and aborted the
        // process (SIGABRT) on the tick, a DoS of the sole PWM writer that
        // survived reboot because activation persists active_profile_id.
        //
        // The chain is built in memory, bypassing the validate()/load_profile()
        // caps on purpose: this pins the third layer, the depth backstop in
        // resolve_mix. If that guard is removed this test does not merely fail —
        // it aborts the test binary, which is precisely the regression.
        const CHAIN: usize = 3_000;
        let mut curves: Vec<CurveConfig> = (0..CHAIN)
            .map(|i| {
                let child = format!("c{}", i + 1);
                mix_curve(&format!("c{i}"), "max", &[child.as_str()])
            })
            .collect();
        curves.push(mix_curve(&format!("c{CHAIN}"), "max", &[])); // terminal

        let profile = DaemonProfile {
            id: "deep".into(),
            name: "Deep".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("c", "c0", "openfan:ch00")],
            curves,
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        // Depth exceeded → None, same safe fallback as the cycle case: the
        // control is skipped and its fan holds.
        assert!(cmds.is_empty());
    }

    #[test]
    fn sync_deep_chain_does_not_overflow_the_stack() {
        // Twin of mix_deep_acyclic_chain_...: a long Sync chain (ctl0 mirrors
        // ctl1 mirrors ctl2 ...) is acyclic, so `on_path` never fires and
        // topo_visit recurses once per link. Built in memory to bypass the
        // ingestion caps on purpose, exercising topo_visit's own depth backstop.
        //
        // Honest scope note: unlike the Mix twin, this chain does NOT abort
        // without the guard — a topo_visit frame is far smaller than a
        // resolve_mix one (no Vec, no HashSet), so 3k links fit comfortably.
        // What this pins is that the backstop FIRES without losing work: every
        // control must still be ordered exactly once, and evaluation must
        // terminate. The guard's value is bounding depth for chains far longer
        // than any we want to enumerate in a unit test.
        const CHAIN: usize = 3_000;
        let mut curves: Vec<CurveConfig> = (0..CHAIN)
            .map(|i| sync_curve(&format!("s{i}"), &format!("ctl{}", i + 1), 0.0))
            .collect();
        let mut term = mix_curve("term", "max", &[]);
        term.curve_type = "flat".into();
        term.sensor_id = "cpu".into();
        term.flat_output_pct = Some(50.0);
        curves.push(term);

        let mut controls: Vec<LogicalControl> = (0..CHAIN)
            .map(|i| openfan_control(&format!("ctl{i}"), &format!("s{i}"), "openfan:ch00"))
            .collect();
        controls.push(openfan_control(
            &format!("ctl{CHAIN}"),
            "term",
            "openfan:ch01",
        ));

        let profile = DaemonProfile {
            id: "syncdeep".into(),
            name: "SyncDeep".into(),
            version: 7,
            description: "".into(),
            controls,
            curves,
        };
        // The backstop bails out of deep recursion; it must not drop controls.
        let ordered = curve_eval::topological_control_order(&profile);
        assert_eq!(
            ordered.len(),
            CHAIN + 1,
            "every control must still be ordered exactly once"
        );
        let unique: std::collections::HashSet<usize> = ordered.iter().copied().collect();
        assert_eq!(unique.len(), CHAIN + 1, "no duplicates in the ordering");

        // And a full evaluation still terminates.
        let cache = make_cache_with_sensor("cpu", 50.0);
        let _ = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
    }

    #[test]
    fn mix_diamond_shares_a_child_across_both_branches() {
        // Pins the insert/remove discipline the depth guard depends on. `visited`
        // is a PATH set, not a seen-set: the shared child D must resolve on the
        // B branch, be REMOVED on the way out, and resolve again on the C branch.
        // If `visited` accumulated across siblings instead, D would look like a
        // cycle on the second branch and silently drop out — and `visited.len()`
        // would count distinct nodes rather than depth, which would make the
        // MAX_PROFILE_CURVES backstop fire on legal wide graphs.
        // The shared child D must itself be a MIX curve — only `resolve_mix`
        // inserts into `visited`, so a flat shared child would never exercise the
        // discipline at all. The root SUMs its two branches, so a dropped branch
        // changes the value rather than being masked by max().
        let mut term = mix_curve("term", "max", &[]);
        term.curve_type = "flat".into();
        term.sensor_id = "cpu".into(); // non-mix curves resolve at their own sensor
        term.flat_output_pct = Some(40.0);

        let profile = DaemonProfile {
            id: "diamond".into(),
            name: "Diamond".into(),
            version: 7,
            description: "".into(),
            controls: vec![openfan_control("c", "a", "openfan:ch00")],
            curves: vec![
                mix_curve("a", "sum", &["b", "c2"]),
                mix_curve("b", "max", &["d"]),
                mix_curve("c2", "max", &["d"]),
                mix_curve("d", "max", &["term"]),
                term,
            ],
        };
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        assert_eq!(cmds.len(), 1, "diamond must resolve, not be skipped");
        // Both branches reach D (40%) → sum = 80. With a seen-set, the second
        // branch would see D as already-visited, mistake it for a cycle, drop
        // out, and yield 40.
        assert_eq!(
            cmds[0].pwm_percent, 80,
            "both diamond branches must resolve"
        );
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

    // ── DEC-267/269: a frozen sensor map is not a live one ──────────────
    //
    // The failure DEC-267 guards is silent in every channel the daemon has: the
    // engine keeps ticking (heartbeat green), the sensor is present (no-sensor
    // fallback never engages), and the temperature never rises (105 C never
    // trips). Only the age distinguishes it.
    //
    // DEC-269 then split "stale" from "absent", because collapsing them let a
    // latched emergency fall from 100% to 40%.

    fn cpu_reading(id: &str, temp_c: f64, updated_at: Instant) -> CachedSensorReading {
        CachedSensorReading {
            id: id.into(),
            kind: SensorKind::CpuTemp,
            label: "Tctl".into(),
            value_c: temp_c,
            source: DeviceLabel::Hwmon,
            updated_at,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
        }
    }

    fn sensor_map(readings: Vec<CachedSensorReading>) -> HashMap<String, CachedSensorReading> {
        readings.into_iter().map(|r| (r.id.clone(), r)).collect()
    }

    const BUDGET: Duration = Duration::from_secs(5);

    fn gpu_reading(id: &str, temp_c: f64, updated_at: Instant) -> CachedSensorReading {
        CachedSensorReading {
            kind: SensorKind::GpuTemp,
            ..cpu_reading(id, temp_c, updated_at)
        }
    }

    #[test]
    fn a_fresh_non_cpu_sensor_may_drive_a_curve() {
        let now = Instant::now();
        assert!(curve_eligible(&gpu_reading("gpu", 70.0, now), now, BUDGET));
    }

    /// The 01-a fix itself: a frozen GPU/coolant sensor stops driving its curve.
    #[test]
    fn a_stale_non_cpu_sensor_may_not_drive_a_curve() {
        let now = Instant::now();
        let old = now - Duration::from_secs(6);
        assert!(!curve_eligible(&gpu_reading("gpu", 70.0, old), now, BUDGET));
    }

    /// The exemption, at the unit level. A stale CPU reading stays eligible so the
    /// DEC-269 round 2 fall-through keeps working; the thermal ladder — not this
    /// predicate — is the authority on what a stale CPU reading means.
    #[test]
    fn a_stale_cpu_sensor_stays_eligible_because_the_ladder_owns_that_case() {
        let now = Instant::now();
        let old = now - Duration::from_secs(600);
        assert!(curve_eligible(&cpu_reading("cpu", 95.0, old), now, BUDGET));
    }

    /// Mirrors `hottest_cpu_reading`: a timestamp marginally in the future across
    /// a clock-read boundary is a fresh reading, not an impossibly old one.
    #[test]
    fn a_future_dated_non_cpu_sensor_is_treated_as_fresh() {
        let now = Instant::now();
        let future = now + Duration::from_millis(5);
        assert!(curve_eligible(
            &gpu_reading("gpu", 70.0, future),
            now,
            BUDGET
        ));
    }

    #[test]
    fn a_fresh_cpu_reading_is_used() {
        let now = Instant::now();
        let sensors = sensor_map(vec![cpu_reading("cpu", 62.0, now)]);
        assert_eq!(
            hottest_cpu_reading(&sensors, now, BUDGET),
            CpuReading::Fresh(62.0)
        );
    }

    #[test]
    fn a_stale_cpu_reading_is_stale_not_absent_and_not_fresh() {
        // The DEC-269 distinction. Reporting Fresh would let a frozen number
        // drive the 105 C ladder forever (the DEC-267 bug); reporting Absent
        // would drop a latched emergency to 40% (the DEC-269 bug).
        let now = Instant::now();
        let sensors = sensor_map(vec![cpu_reading(
            "cpu",
            62.0,
            now - Duration::from_secs(30),
        )]);
        assert_eq!(
            hottest_cpu_reading(&sensors, now, BUDGET),
            CpuReading::Stale(62.0)
        );
    }

    #[test]
    fn no_cpu_sensor_at_all_is_absent() {
        let now = Instant::now();
        assert_eq!(
            hottest_cpu_reading(&HashMap::new(), now, BUDGET),
            CpuReading::Absent
        );
    }

    #[test]
    fn a_reading_exactly_at_the_budget_is_still_fresh() {
        // Boundary is inclusive: at exactly one budget the reading is on time,
        // not late. An exclusive bound would drop a sensor that arrived exactly
        // on schedule.
        let now = Instant::now();
        let sensors = sensor_map(vec![cpu_reading("cpu", 55.0, now - BUDGET)]);
        assert_eq!(
            hottest_cpu_reading(&sensors, now, BUDGET),
            CpuReading::Fresh(55.0)
        );
    }

    #[test]
    fn a_stale_sensor_does_not_mask_a_fresh_hotter_one() {
        // Fresh wins outright — a stale sibling cannot lower the verdict, and a
        // stale HOTTER one must not inflate it either.
        let now = Instant::now();
        let sensors = sensor_map(vec![
            cpu_reading("stale_hot", 99.0, now - Duration::from_secs(30)),
            cpu_reading("fresh", 61.0, now),
        ]);
        assert_eq!(
            hottest_cpu_reading(&sensors, now, BUDGET),
            CpuReading::Fresh(61.0)
        );
    }

    #[test]
    fn with_nothing_fresh_the_hottest_stale_reading_stands_in() {
        let now = Instant::now();
        let sensors = sensor_map(vec![
            cpu_reading("a", 71.0, now - Duration::from_secs(30)),
            cpu_reading("b", 88.0, now - Duration::from_secs(31)),
        ]);
        assert_eq!(
            hottest_cpu_reading(&sensors, now, BUDGET),
            CpuReading::Stale(88.0)
        );
    }

    #[test]
    fn non_cpu_sensors_are_still_ignored_regardless_of_age() {
        let now = Instant::now();
        let mut gpu = cpu_reading("gpu", 90.0, now);
        gpu.kind = SensorKind::GpuTemp;
        assert_eq!(
            hottest_cpu_reading(&sensor_map(vec![gpu]), now, BUDGET),
            CpuReading::Absent
        );
    }

    // ── DEC-269: losing sight must never LOWER an already-forced output ──

    /// Latch a real 105 C emergency the way the runtime does.
    fn latched_emergency() -> crate::safety::ThermalSafetyRule {
        let mut safety = crate::safety::ThermalSafetyRule::new();
        assert_eq!(safety.evaluate(106.0), Some(100), "precondition: latched");
        assert!(safety.is_active());
        safety
    }

    #[test]
    fn a_stale_reading_during_a_latched_emergency_holds_100_not_40() {
        // THE regression this ADR exists for. A single poll leg exceeding the
        // freshness budget — task still alive, so supervision never fires —
        // used to drop every fan from 100% to 40% on a CPU last measured at
        // 95 C, mid-emergency. DEC-190's 40% was chosen for a *vanished*
        // sensor; a stale one is different evidence.
        let mut safety = latched_emergency();
        let mut cycles = 0u32;

        let decision = evaluate_safety_tick(CpuReading::Stale(95.0), &mut cycles, &mut safety);

        assert_eq!(decision.forced_pct, Some(100));
        assert_eq!(decision.thermal_state, "emergency");
        assert!(
            safety.is_active(),
            "a stale reading must not clear the latch"
        );
    }

    #[test]
    fn a_stale_reading_cannot_release_a_latched_emergency_however_cool_it_reads() {
        // A stale reading is not evidence of anything current, so it must not
        // drive the state machine at all — including downward.
        let mut safety = latched_emergency();
        let mut cycles = 0u32;

        let decision = evaluate_safety_tick(CpuReading::Stale(20.0), &mut cycles, &mut safety);

        assert!(
            safety.is_active(),
            "a stale 20 C reading released the latch — evaluate() was called on stale data"
        );
        assert_eq!(decision.forced_pct, Some(100));
    }

    #[test]
    fn an_absent_sensor_during_a_latched_emergency_still_forces_40_per_dec190() {
        // DEC-190 unchanged, and deliberately so: a vanished sensor cannot
        // confirm a live emergency, and 40% over holding 100% was an explicit
        // user decision. DEC-269 narrows which situations reach this branch; it
        // does not revisit the branch.
        let mut safety = latched_emergency();
        let mut cycles = 0u32;

        let decision = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut safety);

        assert_eq!(decision.forced_pct, Some(constants::NO_SENSOR_SAFE_PCT));
        assert_eq!(decision.thermal_state, "no_sensor_fallback");
    }

    #[test]
    fn a_stale_reading_during_recovery_holds_the_recovery_floor() {
        // Same invariant one rung down: mid-recovery the rule is holding 60%,
        // and a stale tick used to let fans fall to the curve for that tick.
        let mut safety = crate::safety::ThermalSafetyRule::new();
        safety.evaluate(106.0);
        assert_eq!(safety.evaluate(70.0), Some(60), "precondition: in recovery");
        let mut cycles = 0u32;

        let decision = evaluate_safety_tick(CpuReading::Stale(70.0), &mut cycles, &mut safety);

        assert_eq!(decision.forced_pct, Some(60));
        assert_eq!(decision.thermal_state, "recovery");
    }

    #[test]
    fn a_stale_reading_with_nothing_latched_still_falls_back_after_the_debounce() {
        // With no emergency and no recovery there is nothing to hold, so a
        // frozen feed is conservatively driven to NO_SENSOR_SAFE_PCT — the
        // DEC-267 behaviour, preserved.
        let mut safety = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;
        let mut decision = evaluate_safety_tick(CpuReading::Stale(45.0), &mut cycles, &mut safety);
        assert_eq!(decision.forced_pct, None, "no force before the debounce");

        for _ in 1..constants::NO_SENSOR_CYCLE_THRESHOLD {
            decision = evaluate_safety_tick(CpuReading::Stale(45.0), &mut cycles, &mut safety);
        }

        assert_eq!(decision.thermal_state, "no_sensor_fallback");
        assert_eq!(decision.forced_pct, Some(constants::NO_SENSOR_SAFE_PCT));
    }

    #[test]
    fn losing_sight_never_lowers_an_already_forced_output() {
        // The invariant itself, stated as a property over the whole matrix
        // rather than as one example — so a new state cannot be added that
        // quietly violates it.
        for (name, mut safety, forced_before) in [
            ("latched", latched_emergency(), 100u8),
            (
                "recovery",
                {
                    let mut s = crate::safety::ThermalSafetyRule::new();
                    s.evaluate(106.0);
                    s.evaluate(70.0);
                    s
                },
                60u8,
            ),
        ] {
            let mut cycles = 0u32;
            let decision = evaluate_safety_tick(CpuReading::Stale(90.0), &mut cycles, &mut safety);
            let after = decision.forced_pct.unwrap_or(0);
            assert!(
                after >= forced_before,
                "{name}: going blind lowered the forced output from {forced_before}% to \
                 {after}% — losing sight of a sensor must never reduce cooling"
            );
        }
    }

    #[test]
    fn a_stale_recovery_hold_is_reported_as_recovery_past_the_debounce() {
        // DEC-269 round 2. Four reviewers found this independently, and it
        // slipped the round-1 tests because they drove exactly ONE stale tick —
        // the branch only flips once the no-sensor counter crosses its 5-cycle
        // threshold. Past that point the daemon reported "no_sensor_fallback"
        // (which means 40%) while actually holding the 60% recovery floor, so
        // `thermal_state` meant two different duties.
        let mut safety = crate::safety::ThermalSafetyRule::new();
        safety.evaluate(106.0);
        assert_eq!(safety.evaluate(70.0), Some(60), "precondition: in recovery");
        let mut cycles = 0u32;

        let mut decision = SafetyDecision {
            thermal_state: "unset",
            forced_pct: None,
        };
        for _ in 0..(constants::NO_SENSOR_CYCLE_THRESHOLD + 3) {
            decision = evaluate_safety_tick(CpuReading::Stale(70.0), &mut cycles, &mut safety);
        }

        assert!(
            cycles > constants::NO_SENSOR_CYCLE_THRESHOLD,
            "precondition: we are past the debounce"
        );
        assert_eq!(
            decision.forced_pct,
            Some(60),
            "the recovery floor still holds"
        );
        assert_eq!(
            decision.thermal_state, "recovery",
            "state must describe the duty actually being forced, not the branch \
             that proposed a different one"
        );
    }

    #[test]
    fn a_stale_but_hot_reading_does_not_drop_cooling_to_the_no_sensor_floor() {
        // DEC-269 round 2, the sharper half. With nothing latched, a wedged poll
        // leg at 104 C used to force every fan from a curve output of ~85% down
        // to 40% — a REDUCTION in cooling caused by going blind, and a plausible
        // route to the 105 C the emergency can no longer detect. The invariant
        // was implemented only for output the rule was already forcing; this is
        // it applied where it always should have been.
        let mut safety = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        let mut decision = SafetyDecision {
            thermal_state: "unset",
            forced_pct: None,
        };
        for _ in 0..(constants::NO_SENSOR_CYCLE_THRESHOLD + 3) {
            decision = evaluate_safety_tick(CpuReading::Stale(104.0), &mut cycles, &mut safety);
        }

        assert_eq!(
            decision.forced_pct,
            None,
            "a stale reading last seen at 104 C must not force fans DOWN to \
             {}% — curves keep running on it, as they did before the freshness \
             filter existed",
            constants::NO_SENSOR_SAFE_PCT
        );
    }

    #[test]
    fn a_stale_and_cool_reading_still_reaches_the_no_sensor_floor() {
        // The other side of the same boundary: once the last known temperature
        // is genuinely cool, the DEC-132 fallback is the right answer and must
        // still fire. Guards against "fixed" by suppressing the floor entirely.
        let mut safety = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        let mut decision = SafetyDecision {
            thermal_state: "unset",
            forced_pct: None,
        };
        for _ in 0..(constants::NO_SENSOR_CYCLE_THRESHOLD + 1) {
            decision = evaluate_safety_tick(CpuReading::Stale(45.0), &mut cycles, &mut safety);
        }

        assert_eq!(decision.forced_pct, Some(constants::NO_SENSOR_SAFE_PCT));
        assert_eq!(decision.thermal_state, "no_sensor_fallback");
    }

    #[test]
    fn a_latched_emergency_still_clears_once_fresh_readings_return() {
        // The exit path. Round 1 pinned entering and holding the stale state but
        // never leaving it — and "evaluate() is never called on stale" is exactly
        // the kind of change whose obvious future mutation strands the latch.
        let mut safety = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        let d = evaluate_safety_tick(CpuReading::Fresh(106.0), &mut cycles, &mut safety);
        assert_eq!((d.forced_pct, d.thermal_state), (Some(100), "emergency"));

        for _ in 0..8 {
            let d = evaluate_safety_tick(CpuReading::Stale(95.0), &mut cycles, &mut safety);
            assert_eq!((d.forced_pct, d.thermal_state), (Some(100), "emergency"));
        }

        // Fresh readings resume, below the release threshold.
        let d = evaluate_safety_tick(CpuReading::Fresh(70.0), &mut cycles, &mut safety);
        assert_eq!((d.forced_pct, d.thermal_state), (Some(60), "recovery"));
        assert_eq!(cycles, 0, "a fresh reading resets the no-sensor debounce");
        let d = evaluate_safety_tick(CpuReading::Fresh(70.0), &mut cycles, &mut safety);
        assert_eq!((d.forced_pct, d.thermal_state), (Some(60), "recovery"));
        let d = evaluate_safety_tick(CpuReading::Fresh(70.0), &mut cycles, &mut safety);
        assert_eq!(
            (d.forced_pct, d.thermal_state),
            (None, "normal"),
            "the emergency must end — a stale-hold that cannot clear is a stuck force"
        );
    }

    // ── DEC-289: a wedged write must stay visible on /status ────────
    //
    // Bounding the backend joins means the loop keeps running through a wedge —
    // which is the fix, and which is also why a wedged writer now presents as a
    // perfectly live engine on both tick stamps. The stall stamp is the only
    // thing that still distinguishes it, so these pin its edge behaviour.

    #[test]
    fn a_stalled_write_is_stamped_once_and_cleared_only_when_one_lands() {
        let cache = StateCache::new();

        // Several consecutive ticks, each with a write still outstanding — the
        // shape a wedged device now produces, because the loop no longer freezes.
        for _ in 0..3 {
            cache.record_engine_tick("normal");
            let mut t = TickCompletion::new(&cache);
            t.set_writes_outstanding(true);
        }
        let first = cache
            .snapshot()
            .subsystem_timestamps
            .engine_writes_stalled_since
            .expect("a stalled write must be stamped");

        cache.record_engine_tick("normal");
        {
            let mut t = TickCompletion::new(&cache);
            t.set_writes_outstanding(true);
        }
        assert_eq!(
            cache
                .snapshot()
                .subsystem_timestamps
                .engine_writes_stalled_since,
            Some(first),
            "the stamp must answer 'since when', not 'as of when' — re-stamping \
             each tick would reset the age and the wedge would never be reported"
        );

        // The loop's own liveness stamps keep advancing throughout. That is the
        // whole point of the fix and it is why the stall needs its own stamp.
        assert!(
            cache
                .snapshot()
                .subsystem_timestamps
                .engine_completed
                .is_some(),
            "the loop completed passes while the write was stalled"
        );

        // A write that lands clears it.
        cache.record_engine_tick("normal");
        drop(TickCompletion::new(&cache));
        assert!(
            cache
                .snapshot()
                .subsystem_timestamps
                .engine_writes_stalled_since
                .is_none(),
            "a landed write must clear the stall"
        );
    }

    #[test]
    fn the_staleness_budget_follows_the_configured_poll_interval() {
        // A fixed budget would mark a legitimately slow-polling system
        // permanently stale and pin its fans at NO_SENSOR_SAFE_PCT forever.
        let cache = StateCache::new();
        assert_eq!(cache.cpu_temp_stale_after(), Duration::from_secs(5));

        cache.set_hwmon_poll_interval_ms(4000);
        assert_eq!(cache.cpu_temp_stale_after(), Duration::from_secs(20));

        // Below the default the budget floors rather than shrinking, so a fast
        // poll cannot make an ordinary scheduling hiccup look like a dead loop.
        cache.set_hwmon_poll_interval_ms(200);
        assert_eq!(cache.cpu_temp_stale_after(), Duration::from_secs(5));

        // DEC-269: capped at the ceiling across the whole supervisable range, so
        // a slow-but-legal cadence cannot hand the 105 C rule a long trust window.
        cache.set_hwmon_poll_interval_ms(MAX_SUPERVISABLE_POLL_INTERVAL_MS);
        assert_eq!(
            cache.cpu_temp_stale_after(),
            Duration::from_millis(CPU_TEMP_STALE_CEILING_MS),
            "the slowest supervisable cadence should land exactly on the ceiling"
        );
    }

    #[test]
    fn the_staleness_budget_is_never_shorter_than_one_poll_period() {
        // DEC-270. The ceiling alone fails the opposite way from the trust window
        // it was written to prevent: clamp the budget below the cadence and every
        // reading is stale the moment it lands, so `hottest_cpu_reading` never
        // returns `Fresh`, the 105 C ladder never runs, and fans sit at
        // NO_SENSOR_SAFE_PCT on healthy hardware while `/status` shows a ticking
        // engine. Neither direction is acceptable, so the budget floors at the
        // cadence and `apply_runtime_overlay` keeps the cadence supervisable.
        let cache = StateCache::new();
        for interval_ms in [200_u64, 1_000, 4_000, 6_000, 30_000, 3_600_000, u64::MAX] {
            cache.set_hwmon_poll_interval_ms(interval_ms);
            let budget = cache.cpu_temp_stale_after();
            assert!(
                budget >= Duration::from_millis(interval_ms),
                "budget {budget:?} is shorter than the {interval_ms} ms poll period \
                 — every reading would be stale on arrival"
            );
            // Exact, not just the floor: `budget >= interval` alone would also
            // accept `interval * 100` or a constant enormous Duration, which
            // would reopen the long-trust-window the ceiling exists to close.
            let floored = interval_ms.max(1_000);
            let expected = floored
                .saturating_mul(5)
                .min(CPU_TEMP_STALE_CEILING_MS)
                .max(floored);
            assert_eq!(
                budget,
                Duration::from_millis(expected),
                "budget for a {interval_ms} ms cadence should be exactly {expected} ms"
            );
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
    fn evaluate_renamed_pump_is_still_floored_and_never_stop_snapped() {
        // DEC-252, at the eval path rather than at the classifier — the wiring is
        // the part that can silently rot. This member's author-declared label
        // carries no pump hint (the user renamed the header); only the label the
        // daemon itself discovered, carried in the member's stable id, says PUMP.
        //
        // Both halves are asserted: the 30% floor, and the stop-snap exemption. A
        // non-zero stop_pct with a 5% demand would otherwise zero the pump
        // outright, which is the coolant-flow-loss case DEC-167 exists to stop.
        let mut profile = make_profile("manual", "flat", 0.0);
        profile.controls[0].members.clear();
        profile.controls[0].members.push(ControlMember {
            source: "hwmon".into(),
            member_id: "hwmon:nct6798:0000:pwm3:PUMP".into(),
            member_label: "Radiator Top".into(),
            fan_zero_rpm: false,
        });
        profile.controls[0].manual_output_pct = 5.0;
        profile.controls[0].minimum_pct = 20.0;
        // Above the 30% hard floor on purpose: a stop_pct below it would never
        // threaten the floored output, and the stop-snap half of this test would
        // pass without the exemption being wired at all.
        profile.controls[0].stop_pct = 35.0;

        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();
        for tick in 0..3 {
            let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
            let pump = cmds.iter().find(|c| c.source == "hwmon").unwrap();
            assert_eq!(
                pump.pwm_percent, 30,
                "tick {tick}: a renamed pump must still hold the hard floor"
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
    fn evaluate_pump_member_floor_survives_nonzero_stop_pct() {
        // DEC-167 regression: a pump/CPU control with a non-zero stop_pct ABOVE
        // the hard floor must NOT be snapped to 0. minimum_pct=30 (== the hard
        // floor) so effective_floor == minimum_pct and the per-member branch was
        // historically skipped — the control-wide value was snapped to 0 by the
        // stop threshold (35 > floored 30). The fix runs the per-member branch
        // for the pump and skips its stop-snap. Held across ticks. FAILS pre-fix.
        let mut profile = make_profile("curve", "flat", 10.0);
        profile.controls[0].members.clear();
        push_pump_member(&mut profile);
        profile.controls[0].minimum_pct = 30.0;
        profile.controls[0].stop_pct = 35.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let mut state = ProfileEngineState::new();
        for tick in 0..3 {
            let cmds = evaluate_profile(&profile, &cache.sensors_snapshot(), &mut state);
            let pump = cmds.iter().find(|c| c.source == "hwmon").unwrap();
            assert_eq!(
                pump.pwm_percent, 30,
                "tick {tick}: pump must hold the hard floor, never snap to 0"
            );
        }
    }

    #[test]
    fn evaluate_pump_member_floor_survives_stop_pct_on_unvalidated_boot_path() {
        // DEC-167: same protection on the un-validated boot/hand-edit path, where
        // minimum_pct=20 is BELOW the hard floor (validate() would reject it with
        // FLOOR_TOO_LOW, but resolve_initial_profile never calls validate()).
        // effective_floor=30 != 20 so the per-member safety-net branch IS taken —
        // but pre-fix its own stop-snap still re-zeroed the floored value
        // (30 < 35). The skip-snap makes the safety net actually hold. FAILS pre-fix.
        let mut profile = make_profile("curve", "flat", 10.0);
        profile.controls[0].members.clear();
        push_pump_member(&mut profile);
        profile.controls[0].minimum_pct = 20.0;
        profile.controls[0].stop_pct = 35.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        let pump = cmds.iter().find(|c| c.source == "hwmon").unwrap();
        assert_eq!(
            pump.pwm_percent, 30,
            "pump must clamp to the hard floor on the un-validated boot path, never 0"
        );
    }

    #[test]
    fn evaluate_nonpump_member_still_snaps_to_zero_below_stop_pct() {
        // Guard against over-scoping DEC-167: a non-pump (openfan) member must
        // KEEP the legitimate stop-to-0 feature. Flat curve 10 with stop_pct=20
        // → below threshold → snaps to 0. Passes before and after the fix.
        let mut profile = make_profile("curve", "flat", 10.0);
        profile.controls[0].minimum_pct = 0.0;
        profile.controls[0].stop_pct = 20.0;
        let cache = make_cache_with_sensor("cpu", 50.0);
        let cmds = evaluate_profile(
            &profile,
            &cache.sensors_snapshot(),
            &mut ProfileEngineState::new(),
        );
        let fan = cmds.iter().find(|c| c.source == "openfan").unwrap();
        assert_eq!(
            fan.pwm_percent, 0,
            "non-pump fan must still stop below stop_pct"
        );
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

    // ── DEC-249: the engine must not die on an unvalidated on-disk profile ──

    #[tokio::test]
    async fn every_tick_records_completion_even_on_the_no_profile_path() {
        // DEC-259: the completion stamp comes from a drop guard precisely because
        // the loop body has several `continue` paths. If any of them skipped it,
        // "started but not completed" would be permanently true and the surface
        // would report a healthy idle daemon as a tick that never finishes.
        //
        // The no-profile path is the one an idle daemon takes every second.
        let cache = Arc::new(StateCache::new());
        let profile = Arc::new(Mutex::new(None));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));
        let overrides = Arc::new(Mutex::new(crate::control_override::OverrideTable::new()));
        let (tx, rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(profile_engine_loop(
            cache.clone(),
            profile,
            Arc::new(parking_lot::RwLock::new(None)),
            None,
            Vec::new(),
            safety,
            overrides,
            rx,
        ));

        // Let the immediate first tick run, then stop the loop.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let _ = tx.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;

        let ts = cache.snapshot().subsystem_timestamps;
        let started = ts.engine_started.expect("the engine must have ticked");
        let completed = ts.engine_completed.expect(
            "the completion guard must fire on the no-profile `continue` path — \
             without it an idle daemon looks permanently mid-tick",
        );
        assert!(
            completed >= started,
            "completion must not predate the start it belongs to"
        );
    }

    #[test]
    fn tuning_survives_negative_step_rates_from_an_unvalidated_profile() {
        // `validate()` bounds step_up_pct/step_down_pct to 0..=100, but the boot
        // paths (CLI `--profile`, persisted-state restore) skip it by design, so
        // a hand-edited or corrupt on-disk profile reaches the engine unchecked.
        // A negative pair inverted the step-rate window and `f64::clamp`
        // panicked on tick 2 — killing the engine task, and with it the sole PWM
        // writer and the 105°C thermal leg, while `/status` kept answering 200.
        let mut profile = make_profile("curve", "flat", 30.0);
        profile.controls[0].step_up_pct = -50.0;
        profile.controls[0].step_down_pct = -50.0;
        let mut state = ProfileEngineState::new();

        // Tick 1: no prior output, so step-rate limiting is skipped entirely —
        // this tick always worked, which is why the failure looked like a
        // healthy start followed by silence.
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 30);

        // Tick 2: `last_output` is now Some. This is the tick that aborted.
        profile.curves[0].flat_output_pct = Some(80.0);
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 51.0).sensors_snapshot(),
            &mut state,
        );

        // A negative cap reads as "no movement in that direction", so the
        // control holds its previous output instead of taking the machine's fan
        // control down with it.
        assert_eq!(cmds[0].pwm_percent, 30);
    }

    #[test]
    fn tuning_survives_non_finite_step_rates() {
        // Same reachability as above (unvalidated boot path), different input:
        // `f64::clamp` also panics when either bound is NaN, not only when they
        // are inverted.
        let mut profile = make_profile("curve", "flat", 40.0);
        profile.controls[0].step_up_pct = f64::NAN;
        profile.controls[0].step_down_pct = f64::INFINITY;
        let mut state = ProfileEngineState::new();

        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 40);

        profile.curves[0].flat_output_pct = Some(90.0);
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 51.0).sensors_snapshot(),
            &mut state,
        );
        // NaN up-cap collapses to 0 (no rise); the infinite down-cap imposes no
        // limit but the curve is rising, so the output holds.
        assert_eq!(cmds[0].pwm_percent, 40);
    }

    #[test]
    fn tuning_start_threshold_survives_small_step_up() {
        // P3-2 / DEC-192: a stopped fan must spin up when the curve genuinely
        // demands it on, even when step_up_pct < stop_pct. Pre-fix, step-rate
        // capped the from-zero output below stop_pct, the stop-snap zeroed it,
        // and the start-kick (gated on output > 0) could never fire — the fan
        // stayed off forever (until the 105°C thermal force).
        let mut profile = make_profile("curve", "flat", 10.0);
        profile.controls[0].stop_pct = 20.0;
        profile.controls[0].start_pct = 35.0;
        profile.controls[0].step_up_pct = 5.0; // < stop_pct — the bug trigger
        let mut state = ProfileEngineState::new();

        // Cycle 1: curve 10 < stop 20 → snap to 0 (fan stopped).
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 0);

        // Cycle 2: curve now demands 60 (well above stop_pct). The demand is
        // ≥ stop_pct, so the start-kick fires and spins up to start_pct despite
        // step_up=5 capping the stepped output to 5 → snapped to 0.
        profile.curves[0].flat_output_pct = Some(60.0);
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 51.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(
            cmds[0].pwm_percent, 35,
            "stopped fan must spin up to start_pct despite step_up_pct < stop_pct"
        );
    }

    #[test]
    fn tuning_start_threshold_not_triggered_when_demand_below_stop() {
        // P3-2 guard: a from-stopped fan whose demand is genuinely below
        // stop_pct must stay off — the start-kick must NOT over-fire on a
        // near-zero request the stop threshold should keep stopped.
        let mut profile = make_profile("curve", "flat", 10.0);
        profile.controls[0].stop_pct = 20.0;
        profile.controls[0].start_pct = 35.0;
        profile.controls[0].step_up_pct = 5.0;
        let mut state = ProfileEngineState::new();

        // Cycle 1: 10 < stop 20 → 0.
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(cmds[0].pwm_percent, 0);

        // Cycle 2: demand 15, still < stop 20 → must remain 0 (no spurious start).
        profile.curves[0].flat_output_pct = Some(15.0);
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 51.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(
            cmds[0].pwm_percent, 0,
            "demand below stop_pct must not trigger the start-kick"
        );
    }

    #[test]
    fn eval_static_cache_invalidated_on_profile_change_curve_reorder() {
        // EFF-3 regression: the cached curve index maps curve_id → slot in
        // profile.curves. When the active profile changes (the DEC-188 re-anchor
        // path also routes here — deactivate() clears active_profile_id, so the
        // next evaluate sees an id change), sync_profile_id MUST null the cache.
        // If the new profile reorders its curves (same control count, so the
        // count guard can't catch it), a surviving stale index would resolve the
        // control to the wrong curve. Remove the sync_profile_id null and this
        // emits 10 instead of 80.
        let flat = |id: &str, out: f64| CurveConfig {
            id: id.into(),
            name: id.into(),
            curve_type: "flat".into(),
            sensor_id: "cpu".into(),
            flat_output_pct: Some(out),
            ..Default::default()
        };
        let mut profile = make_profile("curve", "flat", 0.0);
        profile.id = "A".into();
        profile.controls[0].curve_id = "hot".into();
        profile.curves = vec![flat("hot", 80.0), flat("cold", 10.0)];

        let mut state = ProfileEngineState::new();
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(
            cmds[0].pwm_percent, 80,
            "control follows curve 'hot' (slot 0)"
        );

        // Activate a different profile whose curves are reordered so 'hot' is now
        // slot 1. A stale index (hot→0) would resolve curves[0]='cold' → 10; the
        // rebuilt index resolves hot→1 → 80.
        profile.id = "B".into();
        profile.curves = vec![flat("cold", 10.0), flat("hot", 80.0)];
        let cmds = evaluate_profile(
            &profile,
            &make_cache_with_sensor("cpu", 50.0).sensors_snapshot(),
            &mut state,
        );
        assert_eq!(
            cmds[0].pwm_percent, 80,
            "rebuilt curve index must resolve 'hot' after a profile change + reorder"
        );
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

    // ── DEC-188 steady-state deadband valve ──────────────────────────

    #[test]
    fn deadband_valve_releases_after_max_hold_cycles() {
        // A temperature that settles just inside the 2°C band must not pin the
        // pre-settle output forever. After DEADBAND_MAX_HOLD_CYCLES consecutive
        // holds the valve opens for one tick and the output re-anchors to the
        // settled temperature's true curve value.
        let profile = make_graph_profile_for_deadband();
        let mut state = ProfileEngineState::new();
        let eval = |t: f64, st: &mut ProfileEngineState| -> u8 {
            evaluate_profile(
                &profile,
                &make_cache_with_sensor("cpu", t).sensors_snapshot(),
                st,
            )[0]
            .pwm_percent
        };

        // Anchor at 70°C → curve = 50%.
        assert_eq!(eval(70.0, &mut state), 50);

        // Settle at 68.5°C (inside [68, 70]); the deadband holds 50% for the
        // whole window short of the valve threshold.
        let n = constants::DEADBAND_MAX_HOLD_CYCLES;
        for i in 1..n {
            assert_eq!(
                eval(68.5, &mut state),
                50,
                "tick {i}: deadband must still hold the pre-settle output"
            );
        }

        // The next in-band tick opens the valve → curve(68.5) = 30 + 8.5*2 = 47%.
        assert_eq!(
            eval(68.5, &mut state),
            47,
            "valve must re-anchor to the settled curve value after the hold window"
        );
        assert_eq!(
            state.last_transition_temp("ctrl1"),
            Some(68.5),
            "valve re-anchors the deadband to the settled temperature"
        );
    }

    #[test]
    fn deadband_valve_streak_resets_on_reevaluation() {
        // Only CONSECUTIVE in-band holds count toward the valve. A re-evaluation
        // (here a fall past the band) clears the streak, so the valve needs a
        // fresh full window afterwards and cannot fire mid-drift.
        let profile = make_graph_profile_for_deadband();
        let mut state = ProfileEngineState::new();
        let eval = |t: f64, st: &mut ProfileEngineState| -> u8 {
            evaluate_profile(
                &profile,
                &make_cache_with_sensor("cpu", t).sensors_snapshot(),
                st,
            )[0]
            .pwm_percent
        };
        let n = constants::DEADBAND_MAX_HOLD_CYCLES;

        assert_eq!(eval(70.0, &mut state), 50); // anchor 70 → 50%
                                                // A handful of in-band holds — not enough to open the valve.
        for _ in 0..5 {
            assert_eq!(eval(69.0, &mut state), 50);
        }
        // Fall past the band → re-evaluate, re-anchor at 67°C (curve = 44%), and
        // discard the 5-tick streak.
        assert_eq!(eval(67.0, &mut state), 44);
        assert_eq!(state.last_transition_temp("ctrl1"), Some(67.0));

        // Settle at 66°C (inside the new [65, 67] band): a FULL fresh window of
        // holds at 44% is required, proving the earlier streak was discarded.
        for i in 1..n {
            assert_eq!(
                eval(66.0, &mut state),
                44,
                "tick {i}: fresh window must hold; the pre-fall streak must not count"
            );
        }
        assert_eq!(
            eval(66.0, &mut state),
            42,
            "valve fires only after a full fresh window → curve(66) = 42%"
        );
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

    /// One control on one curve, ramping under a deliberately SLOW
    /// `step_up_pct` so the control is still mid-climb when its sensor freezes.
    /// A control that has already settled would hold the same duty either way and
    /// the test would prove nothing.
    fn ramping_profile(sensor_id: &str) -> DaemonProfile {
        DaemonProfile {
            id: "ramp".into(),
            name: "Ramp".into(),
            version: 7,
            description: String::new(),
            controls: vec![LogicalControl {
                id: "c".into(),
                name: "c".into(),
                mode: "curve".into(),
                curve_id: "cv".into(),
                manual_output_pct: 0.0,
                members: vec![ControlMember {
                    source: "openfan".into(),
                    member_id: "openfan:ch00".into(),
                    member_label: String::new(),
                    fan_zero_rpm: false,
                }],
                step_up_pct: 3.0,
                step_down_pct: 3.0,
                offset_pct: 0.0,
                minimum_pct: 0.0,
                start_pct: 0.0,
                stop_pct: 0.0,
            }],
            curves: vec![CurveConfig {
                id: "cv".into(),
                name: "cv".into(),
                curve_type: "linear".into(),
                sensor_id: sensor_id.into(),
                start_temp_c: Some(30.0),
                start_output_pct: Some(0.0),
                end_temp_c: Some(95.0),
                end_output_pct: Some(100.0),
                ..Default::default()
            }],
        }
    }

    fn reading_aged(
        id: &str,
        kind: SensorKind,
        temp_c: f64,
        age: std::time::Duration,
    ) -> CachedSensorReading {
        CachedSensorReading {
            id: id.into(),
            kind,
            label: id.into(),
            value_c: temp_c,
            source: DeviceLabel::Hwmon,
            // Aged by construction, NOT by waiting. `updated_at` is a
            // `std::time::Instant`, and tokio's `start_paused` clock does not
            // advance that — 40 virtual seconds age a reading by ~0 ms, so a
            // loop-level staleness test that waits for the budget can never see a
            // stale sensor. Stamping the age directly is the only way to reach
            // this state deterministically. (Cost of learning that the other way:
            // a test where both arms passed identically.)
            updated_at: Instant::now() - age,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "chip".into(),
            temp_type: None,
            thresholds: None,
        }
    }

    fn reading(id: &str, kind: SensorKind, temp_c: f64) -> CachedSensorReading {
        reading_aged(id, kind, temp_c, std::time::Duration::ZERO)
    }

    /// Run the ramping profile against a sensor of `kind` that ramps hot and then
    /// FREEZES mid-ramp, and return the PWM frames the loop commanded.
    ///
    /// A cool `CpuTemp` keepalive is refreshed for the whole run. Without it the
    /// GPU variant has no CPU sensor at all, so DEC-132's five-cycle fallback
    /// forces every one of the ten OpenFan channels to 40% and swamps the signal
    /// this test is trying to read. Holding the safety ladder at "normal" in both
    /// runs is what makes `kind` the only variable.
    async fn frames_while_sensor_freezes_mid_ramp(kind: SensorKind) -> Vec<String> {
        const STALE: std::time::Duration = std::time::Duration::from_secs(60);

        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![
            reading("cpu-keepalive", SensorKind::CpuTemp, 40.0),
            reading("s", kind, 40.0),
        ]);

        let keepalive = {
            let c = cache.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    c.update_sensors(vec![reading("cpu-keepalive", SensorKind::CpuTemp, 40.0)]);
                }
            })
        };
        let subject = {
            let c = cache.clone();
            tokio::spawn(async move {
                // Five seconds hot and fresh — enough for the 3%/tick control to
                // be climbing but nowhere near its 100% target.
                for _ in 0..5 {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    c.update_sensors(vec![reading("s", kind, 95.0)]);
                }
                // Then it freezes: same hot value, stamped well past the budget.
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                c.update_sensors(vec![reading_aged("s", kind, 95.0, STALE)]);
            })
        };

        let profile_arc = Arc::new(Mutex::new(Some(ramping_profile("s"))));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));
        let (transport, written) = LoopTestTransport::new(512);
        let fan_ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(500),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(profile_engine_loop(
            cache,
            profile_arc,
            Arc::new(parking_lot::RwLock::new(Some(Arc::new(Mutex::new(
                fan_ctrl,
            ))))),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        // Long enough for a control that keeps evaluating to reach 100%.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;
        keepalive.abort();
        subject.abort();

        let frames = written.lock();
        frames
            .iter()
            .filter(|f| f.starts_with(">0200"))
            .map(|f| f.trim_end().to_ascii_lowercase())
            .collect()
    }

    /// [SAFETY] DEC-272 (register row 01-a) — the call site, and the reason
    /// `curve_eligible` exempts `CpuTemp`.
    ///
    /// Both halves in one test because the point is the CONTRAST, and the two runs
    /// differ in exactly one variable: the sensor's `kind`. Everything else — the
    /// profile, the ramp, the freeze point, the safety state — is identical.
    ///
    /// - A stale **GPU** sensor is dropped from curve evaluation, so the control
    ///   is skipped and its duty stops advancing. That is the 01-a fix: a frozen
    ///   GPU or coolant sensor no longer drives its curve forever under
    ///   `thermal_state: "normal"`.
    /// - A stale **CPU** sensor keeps evaluating, so a control caught mid-ramp
    ///   goes on climbing toward the hot target. That is DEC-269 round 2, which
    ///   deliberately lets a stale-but-hot tick fall through to curve evaluation.
    ///   Filtering `CpuTemp` here would freeze that control instead — a reduction
    ///   in cooling caused by going blind, which is the exact failure DEC-269
    ///   exists to forbid. This test is why the exemption is there.
    #[tokio::test(start_paused = true)]
    async fn a_stale_but_hot_cpu_curve_keeps_climbing_while_a_stale_gpu_curve_holds() {
        let gpu = frames_while_sensor_freezes_mid_ramp(SensorKind::GpuTemp).await;
        let cpu = frames_while_sensor_freezes_mid_ramp(SensorKind::CpuTemp).await;

        assert!(
            !gpu.is_empty() && !cpu.is_empty(),
            "both runs must ramp before their sensor freezes"
        );
        assert!(
            cpu.len() > gpu.len(),
            "a stale CPU curve must keep climbing (DEC-269 r2) while a stale GPU \
             curve holds (DEC-272 01-a); cpu={cpu:?} gpu={gpu:?}"
        );
        // `>0200ff` is SetPwm ch0 at raw 255 = 100%, the curve's target at 95 C.
        assert!(
            cpu.iter().any(|f| f == ">0200ff"),
            "a stale-but-hot CPU curve must still reach the target: {cpu:?}"
        );
        assert!(
            !gpu.iter().any(|f| f == ">0200ff"),
            "a stale GPU curve must NOT keep climbing to the target: {gpu:?}"
        );
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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
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

        // Pin the commanded value, not just that *a* write happened: at 55°C the
        // graph curve (30→20%, 80→100%) yields 60% → percent_to_raw(60)=153=0x99
        // on ch0, so the only SetPwm frame must be ">020099". Asserting the exact
        // frame turns this from a change-detector into a curve-eval regression guard.
        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            !set_pwm_cmds.is_empty() && set_pwm_cmds.iter().all(|c| c.as_str() == ">020099\n"),
            "expected SetPwm 60% (raw 0x99) on ch0 = \">020099\"; got: {cmds:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn loop_adopts_an_openfan_controller_installed_after_it_started() {
        // [SAFETY] DEC-265. `POST /fans/openfan/rescan` installs a controller into
        // the shared slot; if the engine did not re-read that slot, the route would
        // report success while the SOLE PWM WRITER still had no OpenFan backend —
        // and the 105 C `force_all` is guarded by `if let Some(be) = openfan_be`,
        // so the thermal emergency would still have no path to those fans.
        //
        // Starts with an EMPTY slot, exactly as a boot with no controller found.
        let cache = make_cache_with_sensor("cpu", 55.0);
        let profile = make_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        let slot = Arc::new(parking_lot::RwLock::new(None));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(profile_engine_loop(
            cache.clone(),
            profile_arc,
            slot.clone(),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        // Several ticks with nothing adopted — the engine must survive this and
        // keep running rather than latch "no backend" permanently.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

        // Now a rescan adopts a controller.
        let (transport, written) = LoopTestTransport::new(1);
        let fan_ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(500),
        );
        *slot.write() = Some(Arc::new(Mutex::new(fan_ctrl)));

        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        // Same 55 C / graph-curve expectation as `loop_evaluates_profile_and_writes_openfan`,
        // so this asserts the adopted backend is fully wired, not merely constructed.
        let cmds = written.lock();
        let set_pwm: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            !set_pwm.is_empty() && set_pwm.iter().all(|c| c.as_str() == ">020099\n"),
            "engine must pick up a controller adopted after startup and drive it \
             (expected SetPwm 60% on ch0); got: {cmds:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn loop_first_tick_is_immediate_interval_semantics() {
        // Regression guard for the interval-vs-sleep fix: the loop must be
        // driven by `tokio::time::interval` (whose first `tick()` fires
        // immediately), NOT `sleep(1s)`-then-work. We shut down after only
        // 500 ms of virtual time — less than one 1 s period — and still expect
        // a write, proving the first evaluation happened at t≈0. The pre-fix
        // `sleep`-first loop would not have ticked yet at 500 ms (0 writes), so
        // this test fails if the interval scheduler is reverted to a bare sleep.
        let cache = make_cache_with_sensor("cpu", 55.0);
        let profile = make_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        // Less than one period: only an immediate first tick can write by now.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        shutdown_tx.send(true).unwrap();
        let _ = handle.await; // flush any dispatched blocking write before asserting

        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            !set_pwm_cmds.is_empty(),
            "interval's first tick must fire immediately (<1s); got: {cmds:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn loop_shutting_down_issues_no_control_write() {
        // Phase 2 guard: an engine that observes shutdown must not issue a
        // routine control write (defense-in-depth before `restore_hardware()`).
        // With shutdown preset before the loop runs, whichever `select!` arm
        // wins the loop breaks before the write phase → zero writes. The
        // positive control is `loop_evaluates_profile_and_writes_openfan`, which
        // writes ≥1 with the identical setup minus the shutdown, so this 0 is
        // non-vacuous.
        let cache = make_cache_with_sensor("cpu", 55.0);
        let profile = make_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        let (transport, written) = LoopTestTransport::new(1);
        let fan_ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(500),
        );
        let fan_ctrl = Some(Arc::new(Mutex::new(fan_ctrl)));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        shutdown_tx.send(true).unwrap(); // shutting down before the loop's first tick

        let handle = tokio::spawn(profile_engine_loop(
            cache,
            profile_arc,
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            set_pwm_cmds.is_empty(),
            "a shutting-down engine must not issue control writes; got: {cmds:?}"
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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
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

    /// P5 precondition (DEC-165): with a profile active and no GUI, the engine
    /// is the sole writer and must drive OpenFan PWM. This is the gate the
    /// GUI's (now deleted) control loop relied on — it must hold before the GUI
    /// is thinned. Replaces the old `loop_defers_openfan_writes_when_gui_active`
    /// deferral test, which is meaningless now the defer machinery is gone.
    #[tokio::test(start_paused = true)]
    async fn loop_writes_openfan_when_profile_active() {
        let cache = make_cache_with_sensor("cpu", 55.0);

        let profile = make_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        // Let the loop run a cycle.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        // The engine is now primary: it must have issued SetPwm (>02) commands —
        // and at the pinned value. Same profile/sensor as
        // loop_evaluates_profile_and_writes_openfan: 55°C → 60% → raw 0x99 on ch0,
        // so the only frame is ">020099". Pin the value, not mere occurrence.
        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            !set_pwm_cmds.is_empty() && set_pwm_cmds.iter().all(|c| c.as_str() == ">020099\n"),
            "profile engine must write OpenFan PWM 60% (raw 0x99) on ch0 = \">020099\" (DEC-165); got: {cmds:?}",
        );
    }

    /// P5.4 (DEC-165): while a hardware verify is in progress the engine pauses
    /// its write phase so the verify's controlled test writes are not
    /// overwritten. With a profile active AND a verify claimed, the loop must
    /// issue NO OpenFan PWM writes for the verify's lifetime.
    #[tokio::test(start_paused = true)]
    async fn loop_pauses_writes_during_verify() {
        let cache = make_cache_with_sensor("cpu", 55.0);
        // Claim the verify slot for a long window. The deadman uses the real
        // wall clock, which the test's simulated tokio time barely advances, so
        // the pause stays active for the whole run.
        assert!(cache
            .try_begin_verify(std::time::Duration::from_secs(60))
            .is_some());

        let profile = make_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            set_pwm_cmds.is_empty(),
            "engine must NOT write while a verify is in progress (DEC-165); got: {cmds:?}",
        );
    }

    /// Imperative mode (no active profile): the engine must issue NO writes, so
    /// deactivating a profile cleanly stops control instead of asserting stale
    /// curve outputs. The single-writer invariant has no dual-writer gap to
    /// cover post-flip (DEC-165) — this pins the "engine is silent with no
    /// profile" half; `loop_writes_openfan_when_profile_active` pins the other.
    #[tokio::test(start_paused = true)]
    async fn loop_does_not_write_without_active_profile() {
        let cache = make_cache_with_sensor("cpu", 55.0);
        let profile_arc = Arc::new(Mutex::new(None)); // no profile → imperative
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        assert!(
            set_pwm_cmds.is_empty(),
            "engine must not write in imperative mode (no active profile); got: {cmds:?}",
        );
    }

    /// DEC-188: editing the active profile's curve and re-applying it (same id)
    /// must take effect on the next tick. Without the activation-epoch reset the
    /// 2°C deadband holds the pre-edit output for tens of seconds, because at a
    /// stable temperature the reading never leaves the band. This drives the loop
    /// exactly as `activate_profile_handler` would — swap the profile and bump
    /// the epoch under the same lock — and asserts the new value is written.
    #[tokio::test(start_paused = true)]
    async fn loop_reactivation_reanchors_through_deadband() {
        let cache = make_cache_with_sensor("cpu", 50.0);

        // Initial active profile: flat 30%, one openfan member.
        let profile_arc = Arc::new(Mutex::new(Some(make_profile("curve", "flat", 30.0))));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        let (transport, written) = LoopTestTransport::new(10);
        let fan_ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(500),
        );
        let fan_ctrl = Some(Arc::new(Mutex::new(fan_ctrl)));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(profile_engine_loop(
            cache.clone(),
            profile_arc.clone(),
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        // Several ticks: the first anchors at 30%; the rest are deadband holds
        // (temperature unchanged, inside the band) — the engine is now "stuck"
        // at 30% and would stay there for DEADBAND_MAX_HOLD_CYCLES.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

        // Edit the active profile's curve to 80% (SAME id) and re-apply, exactly
        // as the activate handler does: swap the profile and bump the epoch under
        // the same `active_profile` lock the engine reads it under.
        {
            let mut guard = profile_arc.lock();
            *guard = Some(make_profile("curve", "flat", 80.0));
            cache.bump_profile_activation_epoch();
        }

        // One more tick: the epoch bump re-anchors, so the new 80% applies now
        // instead of waiting for the temperature to leave the deadband.
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        // The final SetPwm must be 80%, not the held 30%. (Writes coalesce, so in
        // the unfixed code the only frame would be the initial 30%.)
        let cmds = written.lock();
        let set_pwm_cmds: Vec<_> = cmds.iter().filter(|c| c.starts_with(">02")).collect();
        let last = set_pwm_cmds.last().expect("expected SetPwm commands");
        let hex_value = &last[last.len() - 3..last.len() - 1];
        let expected = format!("{:02X}", crate::pwm::percent_to_raw(80));
        let stale = format!("{:02X}", crate::pwm::percent_to_raw(30));
        assert_eq!(
            hex_value, expected,
            "re-activation must re-anchor through the deadband (expected 80% = 0x{expected}; \
             stale-hold bug yields 30% = 0x{stale}); commands: {set_pwm_cmds:?}"
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
            Arc::new(parking_lot::RwLock::new(None)), // no openfan
            None,                                     // no hwmon
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
            Arc::new(parking_lot::RwLock::new(None)),
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

    /// DEC-130: the 105 °C thermal force drives OpenFan + writable hwmon to
    /// 100 %, but GPU fans are EXCLUDED — AMD PMFW firmware owns GPU thermal
    /// protection and `GpuBackend` deliberately does not implement
    /// `SafetyWriteBackend`. This pins the exclusion behaviourally: with a GPU
    /// member in the active profile and a fake GPU registered, a sustained
    /// emergency must force OpenFan yet leave the GPU's `fan_curve` untouched.
    #[tokio::test(start_paused = true)]
    async fn loop_thermal_force_excludes_gpu() {
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = make_fake_gpu(&dir);

        // 110 °C ≥ the 105 °C force threshold → sustained thermal emergency.
        let cache = make_cache_with_sensor("cpu", 110.0);

        // A profile that controls the GPU, so absent the DEC-130 exclusion the
        // engine would have a GPU member it could force.
        let profile = make_gpu_profile("curve", "graph", 50.0);
        let profile_arc = Arc::new(Mutex::new(Some(profile)));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        // OpenFan present so we can confirm the force path actually ran.
        let (transport, written) = LoopTestTransport::new(30);
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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
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

        // The GPU was NOT forced — its PMFW curve file is still empty (DEC-130).
        let content = std::fs::read_to_string(&curve_path).unwrap();
        assert!(
            content.is_empty(),
            "GPU must be excluded from the thermal force (DEC-130); \
             fan_curve received: {content:?}"
        );
        // ...but the force path DID run: OpenFan channels were forced to 100 %.
        let cmds = written.lock();
        assert!(
            cmds.iter().any(|c| c.starts_with(">02")),
            "thermal force must drive OpenFan (proves the force ran); got: {cmds:?}"
        );
    }

    // ── P1-1: thermal-force hwmon leg ────────────────────────────────
    // A recording hwmon writer + a writable header, so the thermal force's
    // hwmon leg can be observed *through the loop*. Mirrors the private
    // TestWriter / make_header in backends.rs (not reachable from this module).
    struct RecordingSysfsWriter {
        writes: Arc<Mutex<Vec<(String, String)>>>,
    }
    impl crate::hwmon::pwm_control::SysfsWriter for RecordingSysfsWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), crate::error::HwmonError> {
            self.writes.lock().push((path.into(), value.into()));
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, crate::error::HwmonError> {
            Ok("128\n".into())
        }
    }

    fn writable_pwm_header(id: &str) -> crate::hwmon::pwm_discovery::PwmHeaderDescriptor {
        crate::hwmon::pwm_discovery::PwmHeaderDescriptor {
            id: id.to_string(),
            label: "CHA_FAN1".to_string(),
            chip_name: "it8696".to_string(),
            device_id: "it87.2624".to_string(),
            pwm_index: 1,
            supports_enable: true,
            pwm_path: "/sys/class/hwmon/hwmon0/pwm1".to_string(),
            enable_path: Some("/sys/class/hwmon/hwmon0/pwm1_enable".to_string()),
            rpm_available: false,
            rpm_path: None,
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: None,
            is_aio: false,
        }
    }

    /// P1-1: the thermal force must drive WRITABLE HWMON headers to 100 % through
    /// the loop (the `force_all` branch, hwmon leg). OpenFan-force + GPU-exclusion
    /// are already covered (`safety_override_forces_all_channels_to_100` /
    /// `loop_thermal_force_excludes_gpu`); the hwmon leg had no loop-level test —
    /// every other thermal loop test passes `hwmon_controller = None`.
    /// A sysfs writer whose PWM write blocks on opening a FIFO nobody reads —
    /// the DEC-278 model. A `thread::sleep` does not fail the same way as a
    /// blocked `open(2)`, which is the whole reason that lesson exists.
    struct LoopWedgingWriter {
        fifo: std::path::PathBuf,
    }

    impl crate::hwmon::pwm_control::SysfsWriter for LoopWedgingWriter {
        fn write_file(&mut self, path: &str, _value: &str) -> Result<(), crate::error::HwmonError> {
            if path.ends_with("pwm1") {
                let _ = std::fs::OpenOptions::new().write(true).open(&self.fifo);
            }
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, crate::error::HwmonError> {
            Ok("128\n".into())
        }
    }

    /// DEC-299 (closes AUD-a4 and AUD-a5): the headline safety property of the
    /// DEC-289/298/299 bound, pinned at the LOOP level for the first time.
    ///
    /// Everything else tests `BoundedWrite` directly. Nothing spawned the real
    /// `profile_engine_loop` with a wedged backend, so nothing proved the three
    /// things that matter when a device wedges in the field:
    ///   1. the loop keeps ticking instead of freezing (the DEC-289 premise),
    ///   2. the thermal ladder still evaluates and publishes `thermal_state`,
    ///   3. the OTHER backend still reaches its hardware (AUD-a5),
    ///   4. the stall is REPORTED, so `/status` can say so.
    ///
    /// Real time, not `start_paused`: tokio will not auto-advance virtual time
    /// while a `spawn_blocking` task is outstanding, so a paused-time version of
    /// this would HANG rather than fail (DEC-272 trap 2).
    #[tokio::test]
    async fn a_wedged_backend_does_not_stop_the_loop_the_ladder_or_the_stall_report() {
        let fifo =
            std::env::temp_dir().join(format!("ofc-dec299-loop-{}.fifo", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(
            unsafe { libc::mkfifo(c.as_ptr(), 0o600) },
            0,
            "mkfifo failed"
        );

        // Hard self-release backstop: without it a failed assertion skips the
        // cleanup, the blocking thread never returns, and dropping the runtime
        // blocks forever — a red test becomes a hung CI job (DEC-272 trap 3).
        // Release the wedge from a DETACHED thread, repeatedly, starting after the
        // assertions below have had their look at the stalled state.
        //
        // Three things here were learned the hard way and are not decoration.
        // (a) Never open the FIFO from the test's own thread: a reader open
        // blocks until a writer appears, so if the wedged writer has already
        // gone it blocks forever — on a current-thread runtime that stalls
        // everything. (b) Open REPEATEDLY: one reader open frees only the
        // writers blocked at that instant, which `backends.rs::release_backstop`
        // records finding the same way. (c) Never join it — joining made every
        // run wait out the full sleep.
        {
            let f = fifo.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(5));
                for _ in 0..60 {
                    let _ = std::fs::File::open(&f);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            });
        }

        let cache = make_cache_with_sensor("cpu", 106.0); // >=105 -> force
        let profile_arc = Arc::new(Mutex::new(None::<DaemonProfile>));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        // hwmon WEDGES; OpenFan is healthy and must keep writing.
        let ctrl = crate::hwmon::pwm_control::HwmonPwmController::new(
            vec![writable_pwm_header("hwmon:it8696:pwm1")],
            crate::hwmon::lease::LeaseManager::new(),
            Box::new(LoopWedgingWriter { fifo: fifo.clone() }),
            cache.clone(),
        );
        let (transport, openfan_written) = LoopTestTransport::new(512);
        let fan_ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(500),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(profile_engine_loop(
            cache.clone(),
            profile_arc,
            Arc::new(parking_lot::RwLock::new(Some(Arc::new(Mutex::new(
                fan_ctrl,
            ))))),
            Some(Arc::new(Mutex::new(ctrl))),
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        let snap = cache.snapshot();

        assert!(
            snap.subsystem_timestamps.engine_started.is_some(),
            "the loop never completed a tick — a wedged backend froze it, which is \
             exactly what the bound exists to prevent"
        );
        assert_eq!(
            snap.thermal_override_state.as_deref(),
            Some("emergency"),
            "the ladder must keep evaluating while a backend is wedged"
        );
        assert!(
            !openfan_written.lock().is_empty(),
            "the healthy OpenFan backend wrote nothing — a wedge in hwmon must not \
             stop the backend that is working"
        );
        assert!(
            snap.subsystem_timestamps
                .engine_writes_stalled_since
                .is_some(),
            "a wedged write must be reported, or the operator sees a healthy daemon \
             while the fans hold their last duty"
        );

        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;
        let _ = std::fs::remove_file(&fifo);
    }

    #[tokio::test(start_paused = true)]
    async fn loop_thermal_force_drives_hwmon() {
        let cache = make_cache_with_sensor("cpu", 106.0); // ≥105 °C → force
        let profile_arc = Arc::new(Mutex::new(None::<DaemonProfile>));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        let writes: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let ctrl = crate::hwmon::pwm_control::HwmonPwmController::new(
            vec![writable_pwm_header("hwmon:it8696:pwm1")],
            crate::hwmon::lease::LeaseManager::new(),
            Box::new(RecordingSysfsWriter {
                writes: writes.clone(),
            }),
            cache.clone(),
        );
        let hwmon_ctrl = Some(Arc::new(Mutex::new(ctrl)));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(profile_engine_loop(
            cache,
            profile_arc,
            Arc::new(parking_lot::RwLock::new(None)), // no OpenFan — isolates the hwmon leg
            hwmon_ctrl,
            vec![], // no GPU
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        let w = writes.lock();
        let pwm_write = w
            .iter()
            .find(|(p, _)| p.ends_with("pwm1") && !p.ends_with("_enable"));
        let (_, val) = pwm_write
            .unwrap_or_else(|| panic!("thermal force must write the hwmon pwm header; got {w:?}"));
        assert_eq!(
            val.trim(),
            "255",
            "thermal force must drive hwmon to 100% (raw 255), got {val:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn loop_writes_thermal_state_to_cache_during_emergency() {
        // A2: the engine loop must write `thermal_state` to the cache on the
        // forced/emergency path — which short-circuits via `continue`. `/status`
        // maps None→"normal", so it cannot distinguish "engine wrote normal" from
        // "engine never wrote"; assert the cache field directly. No controllers
        // are needed — the cache write precedes any backend use.
        let cache = make_cache_with_sensor("cpu", 106.0); // ≥105 °C → emergency + force 100
        let profile_arc = Arc::new(Mutex::new(None::<DaemonProfile>));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        // Precondition: nothing has written thermal state yet.
        assert_eq!(cache.snapshot().thermal_override_state, None);

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(profile_engine_loop(
            cache.clone(),
            profile_arc,
            Arc::new(parking_lot::RwLock::new(None)), // no OpenFan
            None,   // no hwmon — the cache write precedes any backend use
            vec![], // no GPU
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await; // ≥1 tick
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = handle.await;

        // The emergency path (which hits `continue`) MUST still have written the
        // cache. If the setter were moved below the `continue`, this stays None.
        assert_eq!(
            cache.snapshot().thermal_override_state.as_deref(),
            Some("emergency"),
            "engine loop must write thermal_state to the cache on the forced path"
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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
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

        let d = evaluate_safety_tick(CpuReading::Fresh(106.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "emergency",
                forced_pct: Some(100)
            }
        );

        // Still above release threshold — hold at 100%.
        let d = evaluate_safety_tick(CpuReading::Fresh(90.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "emergency",
                forced_pct: Some(100)
            }
        );

        // Release at ≤80°C → recovery floor.
        let d = evaluate_safety_tick(CpuReading::Fresh(60.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "recovery",
                forced_pct: Some(60)
            }
        );

        // One extra recovery cycle.
        let d = evaluate_safety_tick(CpuReading::Fresh(60.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "recovery",
                forced_pct: Some(60)
            }
        );

        let d = evaluate_safety_tick(CpuReading::Fresh(60.0), &mut cycles, &mut rule);
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
    /// (it forces PWM, so "normal" would misinform the GUI's thermal-safety
    /// banner — there is no GUI stand-down logic since DEC-165, the daemon is
    /// the sole writer).
    #[test]
    fn safety_tick_no_sensor_fallback_after_threshold() {
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        for i in 1..constants::NO_SENSOR_CYCLE_THRESHOLD {
            let d = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
            assert_eq!(d.forced_pct, None, "cycle {i}: below threshold");
            assert_eq!(d.thermal_state, "normal", "cycle {i}: below threshold");
        }

        let d = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "no_sensor_fallback",
                forced_pct: Some(constants::NO_SENSOR_SAFE_PCT)
            }
        );

        // Sensor recovers → counter resets, normal control resumes.
        let d = evaluate_safety_tick(CpuReading::Fresh(50.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "normal",
                forced_pct: None
            }
        );
        assert_eq!(cycles, 0);
    }

    #[test]
    fn safety_tick_no_sensor_fallback_persists_past_threshold_without_emergency() {
        // A3: with NO emergency latched, the plain no-sensor fallback must KEEP
        // forcing past the threshold on the counter alone. Kills the mutation
        // `n >= THRESHOLD` → `n == THRESHOLD` (which survives both existing tests:
        // `..._after_threshold` stops at cycle 5, and the DEC-190 dropout test
        // holds via the emergency latch, not the counter).
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        // Cycles 1..THRESHOLD: below the gate, nothing forced.
        for cycle in 1..constants::NO_SENSOR_CYCLE_THRESHOLD {
            let d = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
            assert_eq!(
                d,
                SafetyDecision {
                    thermal_state: "normal",
                    forced_pct: None
                },
                "cycle {cycle}: below threshold must not force"
            );
            assert!(
                !rule.is_active(),
                "cycle {cycle}: no emergency may be latched in this path"
            );
        }

        // Cycle THRESHOLD (forcing begins), THRESHOLD+1, THRESHOLD+2: force PERSISTS.
        for cycle in
            constants::NO_SENSOR_CYCLE_THRESHOLD..=(constants::NO_SENSOR_CYCLE_THRESHOLD + 2)
        {
            let d = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
            assert_eq!(
                d,
                SafetyDecision {
                    thermal_state: "no_sensor_fallback",
                    forced_pct: Some(constants::NO_SENSOR_SAFE_PCT),
                },
                "cycle {cycle}: non-emergency no-sensor force must persist past the \
                 threshold (n >= THRESHOLD, not n == THRESHOLD)"
            );
            // Persistence is the counter, not the latch.
            assert!(
                !rule.is_active(),
                "cycle {cycle}: emergency latch must stay clear"
            );
        }
    }

    #[test]
    fn safety_tick_forces_floor_on_sensor_dropout_during_latched_emergency() {
        // DEC-190: once a thermal emergency is latched, a CPU-sensor dropout must
        // force the no-sensor safe floor IMMEDIATELY — not fall to profile
        // control for cycles 1-4 (the pre-fix bug) — and report a coherent state
        // rather than a stale "emergency" with no force.
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        // Latch the emergency with a real over-limit reading.
        let d = evaluate_safety_tick(CpuReading::Fresh(106.0), &mut cycles, &mut rule);
        assert_eq!(d.thermal_state, "emergency");
        assert_eq!(d.forced_pct, Some(100));

        // Sensor vanishes the very next tick (cycle 1 of the dropout): force the
        // no-sensor floor NOW, do not drop to profile control.
        let d = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "no_sensor_fallback",
                forced_pct: Some(constants::NO_SENSOR_SAFE_PCT),
            },
            "a dropout mid-emergency must force the no-sensor floor immediately, \
             coherent with the reported state"
        );
    }

    #[test]
    fn safety_tick_transient_dropout_without_emergency_does_not_force() {
        // DEC-190 scope guard: with NO emergency latched, the normal-operation
        // no-sensor debounce is UNCHANGED — a transient 1-4 cycle dropout must
        // not force fans (only the 5-cycle fallback does). Guards against an
        // over-broad "force 40% whenever the sensor is missing" rewrite.
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        for cycle in 1..constants::NO_SENSOR_CYCLE_THRESHOLD {
            let d = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
            assert_eq!(
                d,
                SafetyDecision {
                    thermal_state: "normal",
                    forced_pct: None,
                },
                "cycle {cycle}: a transient dropout with no emergency must not force"
            );
        }
    }

    #[test]
    fn safety_tick_dropout_during_emergency_stays_forced_past_the_no_sensor_threshold() {
        // DEC-190: a dropout during a latched emergency forces 40% from cycle 1
        // AND keeps forcing 40% once the generic 5-cycle no-sensor fallback ALSO
        // trips (both branches of `force_no_sensor` true) — no flip-flop at the
        // threshold boundary.
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;
        evaluate_safety_tick(CpuReading::Fresh(106.0), &mut cycles, &mut rule); // latch emergency

        for cycle in 1..=(constants::NO_SENSOR_CYCLE_THRESHOLD + 2) {
            let d = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
            assert_eq!(
                d,
                SafetyDecision {
                    thermal_state: "no_sensor_fallback",
                    forced_pct: Some(constants::NO_SENSOR_SAFE_PCT),
                },
                "cycle {cycle} of a mid-emergency dropout must hold the 40% floor"
            );
        }
    }

    #[test]
    fn safety_tick_sensor_return_after_dropout_resumes_emergency_force() {
        // DEC-190 latch-persistence: the emergency latch survives a multi-cycle
        // sensor dropout (evaluate() never ran), so a still-hot reading on return
        // (>= the 80°C release temp) snaps back to forced-100% that tick.
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;
        evaluate_safety_tick(CpuReading::Fresh(106.0), &mut cycles, &mut rule); // latch 100%
        for _ in 0..3 {
            evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule); // dropout → 40%
        }
        // Sensor returns at 95°C (still above the 80°C release): emergency resumes.
        let d = evaluate_safety_tick(CpuReading::Fresh(95.0), &mut cycles, &mut rule);
        assert_eq!(
            d,
            SafetyDecision {
                thermal_state: "emergency",
                forced_pct: Some(100),
            },
            "a still-hot reading after a dropout must re-assert forced-100%"
        );
    }

    /// A sensor returning mid-streak (before the threshold) resets the
    /// counter — the fallback only fires on N *consecutive* missing cycles.
    #[test]
    fn safety_tick_counter_resets_on_sensor_return_mid_streak() {
        let mut rule = crate::safety::ThermalSafetyRule::new();
        let mut cycles = 0u32;

        for _ in 0..constants::NO_SENSOR_CYCLE_THRESHOLD - 1 {
            evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
        }
        evaluate_safety_tick(CpuReading::Fresh(50.0), &mut cycles, &mut rule);
        assert_eq!(cycles, 0);

        // A fresh streak must count from zero again.
        let d = evaluate_safety_tick(CpuReading::Absent, &mut cycles, &mut rule);
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
            Arc::new(parking_lot::RwLock::new(None)), // no fan controller
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

    #[tokio::test]
    async fn the_loop_applies_the_freshness_filter_to_its_own_safety_input() {
        // DEC-269. Three reviewers independently found that only
        // `hottest_cpu_reading` was pinned, never the loop's USE of it —
        // reverting the call site to the pre-DEC-267 inline reduce, or passing
        // `Duration::MAX`, left the whole suite green. That is the recurring
        // shape in this codebase: extracting a rule into a testable function is
        // not the same as testing the code that calls it.
        //
        // NOTE for anyone extending this: `#[tokio::test(start_paused = true)]`
        // and `tokio::time::advance` do NOT work here. The virtual clock does
        // not move `std::time::Instant`, which is what both `updated_at` and the
        // filter's `now` use — every reading would stay "fresh" and the test
        // would pass vacuously. Age the reading by CONSTRUCTING it in the past.
        let cache = Arc::new(StateCache::new());
        cache.update_sensors(vec![cpu_reading(
            "cpu",
            62.0,
            Instant::now() - Duration::from_secs(3600),
        )]);

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(profile_engine_loop(
            cache.clone(),
            Arc::new(Mutex::new(None::<DaemonProfile>)),
            Arc::new(parking_lot::RwLock::new(None)),
            None,
            vec![],
            Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new())),
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        // The loop ticks at 1 Hz and the plain no-sensor fallback needs
        // NO_SENSOR_CYCLE_THRESHOLD cycles, so allow a little over that.
        tokio::time::sleep(Duration::from_millis(
            1000 * u64::from(constants::NO_SENSOR_CYCLE_THRESHOLD) + 800,
        ))
        .await;

        let state = cache.snapshot().thermal_override_state.clone();
        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

        assert_eq!(
            state.as_deref(),
            Some("no_sensor_fallback"),
            "an hour-old CPU reading reached the safety rule as if it were current \
             — the loop is not applying its freshness filter"
        );
    }

    #[tokio::test]
    async fn a_dropped_shutdown_sender_stops_the_loop_instead_of_spinning_it() {
        // DEC-265 regression. `changed()` returns Err immediately and FOREVER
        // once every Sender is gone. Discarding that Result (the pre-fix code)
        // makes this arm fire continuously with `borrow()` still false, so the
        // loop never reaches the tick that paces it and never breaks: a 1 Hz
        // engine becomes a busy loop pinning a core, while the liveness
        // heartbeat reports peak health because it genuinely *is* ticking.
        //
        // So the failure mode is a hang, and a timeout is what detects it.
        // Deleting the `changed.is_err()` break makes this test time out.
        let cache = make_cache_with_sensor("cpu", 50.0);
        let profile_arc = Arc::new(Mutex::new(None::<DaemonProfile>));
        let safety = Arc::new(Mutex::new(crate::safety::ThermalSafetyRule::new()));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(profile_engine_loop(
            cache,
            profile_arc,
            Arc::new(parking_lot::RwLock::new(None)),
            None,
            vec![],
            safety,
            Arc::new(Mutex::new(crate::control_override::OverrideTable::new())),
            shutdown_rx,
        ));

        // Never signalled — just dropped, as it would be if `main`'s frame
        // unwound or the owner was restructured to drop it early.
        drop(shutdown_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "a dropped shutdown sender must stop the engine, not spin it forever"
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
            Arc::new(parking_lot::RwLock::new(fan_ctrl)),
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
