//! Tuning pipeline: offset -> floor -> step-rate -> stop-snap -> start-kick
//! -> clamp, plus per-member floor policy. Pure; extracted from mod.rs (C3).

use super::*;

/// Apply the full per-control tuning pipeline.
///
/// Mirrors `ControlLoopService._apply_tuning` in the GUI so headless profile
/// mode produces the same output as GUI-driven mode for identical inputs.
/// Order matters: step-rate limiting runs AFTER offset/minimum so the
/// delta tracked cycle-to-cycle is the final clamped output; stop-threshold
/// comes after step-rate so a slow-falling curve can still snap to zero.
pub(crate) fn apply_tuning(
    control: &LogicalControl,
    raw_output: f64,
    last_output: Option<f64>,
) -> f64 {
    apply_tuning_with_floor(control, raw_output, last_output, control.minimum_pct, false)
}

/// `apply_tuning` with an explicit minimum-floor override.
///
/// DEC-119: GPU members carry no soft floor (`floor == 0.0`) even inside a
/// mixed control whose `minimum_pct` is non-zero, mirroring the GUI's
/// `member_minimum_pct`. Every other member passes `control.minimum_pct`, so
/// the public `apply_tuning` (and its tests) is unchanged. Keeping this a
/// floor parameter — rather than special-casing GPU inside the pipeline —
/// preserves the exact offset → floor → step → stop/start order for both.
///
/// DEC-167: `floor_is_hard` marks a pump/CPU member whose floor must never be
/// breached. When set, the stop-snap (step 4) is skipped so a non-zero
/// `stop_pct` can never zero a pump (coolant-flow loss → thermal runaway). The
/// public `apply_tuning` passes `false` (the generic control-wide path).
pub(crate) fn apply_tuning_with_floor(
    control: &LogicalControl,
    raw_output: f64,
    last_output: Option<f64>,
    floor: f64,
    floor_is_hard: bool,
) -> f64 {
    // 1. Offset
    let mut output = raw_output + control.offset_pct;

    // 2. Minimum floor (per-profile soft floor, distinct from daemon safety)
    if output < floor {
        output = floor;
    }

    // The post-offset/post-floor demand — what the curve + floor want THIS
    // cycle, captured BEFORE step-rate limiting. The start-threshold (step 5)
    // judges "is the fan genuinely meant to run?" from this rather than from the
    // step-rate-capped `output`, so a slow ramp cannot strand a starting fan at
    // 0 (see step 5 / P3-2).
    let demand = output;

    // 3. Step-rate limiting — only bites when we have a previous cycle's output.
    //    step_up_pct / step_down_pct are per-cycle caps (1Hz here).
    if let Some(last) = last_output {
        let max_up = last + control.step_up_pct;
        let max_down = last - control.step_down_pct;
        output = output.clamp(max_down, max_up);
    }

    // 4. Stop threshold — snap to zero below stop_pct so the fan actually
    //    stops instead of spinning at a near-stall speed. `stop_pct == 0`
    //    disables the feature (matches GUI semantics). A HARD floor (pump/CPU)
    //    is exempt (DEC-167): a pump must never be snapped to 0 — the step-2
    //    floor already holds it at >= HARD_PUMP_CPU_FLOOR_PCT, and zeroing it
    //    would risk coolant-flow loss and rapid thermal runaway.
    if !floor_is_hard && control.stop_pct > 0.0 && output < control.stop_pct {
        output = 0.0;
    }

    // 5. Start threshold — when transitioning from stopped (previous cycle = 0)
    //    back to non-zero, jump up to at least `start_pct` so the fan actually
    //    spins up instead of stalling at a too-low PWM. Only the 0 → on
    //    transition triggers it.
    //
    //    P3-2 / DEC-192: the trigger is judged on `demand` (the pre-step-rate
    //    request), NOT the stepped/stopped `output`. With `step_up_pct <
    //    stop_pct`, step 3 caps a from-stopped fan below `stop_pct`, step 4 then
    //    snaps it to 0, and the old `output > 0.0` guard could never fire — the
    //    fan stayed off forever despite the curve demanding it on (until the
    //    105°C thermal force). Judging on `demand` rescues that case: the
    //    kick raises the zeroed `output` back to `start_pct`. A demand the
    //    stop-snap would itself keep off (`demand < stop_pct`) correctly does
    //    NOT start. For a HARD floor step 4 is skipped, so `demand_wants_on`
    //    collapses to `demand > 0` and the result is unchanged on that path.
    //    Default profiles (`step_up_pct = 100`, `stop_pct = 0`) are byte-
    //    identical to the old guard, so the `tuning_sequence` parity oracle is
    //    unperturbed.
    let demand_wants_on =
        demand > 0.0 && (floor_is_hard || control.stop_pct == 0.0 || demand >= control.stop_pct);
    if demand_wants_on
        && matches!(last_output, Some(prev) if prev == 0.0)
        && control.start_pct > 0.0
    {
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
/// Per-member minimum-PWM floor (DEC-119 + DEC-162). GPU members carry no
/// floor (PMFW enforces its own OD_RANGE minimum); a pump/CPU header is hard-
/// floored to at least [`HARD_PUMP_CPU_FLOOR_PCT`] even when the control
/// declares a lower `minimum_pct`; every other member uses the control-wide
/// floor. Shared by the curve path and the override path so the safety floor
/// is computed in exactly one place.
pub(crate) fn member_effective_floor(control: &LogicalControl, member: &ControlMember) -> f64 {
    if member_is_gpu(member) {
        0.0
    } else if member_is_pump_or_cpu(member) {
        control.minimum_pct.max(HARD_PUMP_CPU_FLOOR_PCT)
    } else {
        control.minimum_pct
    }
}
