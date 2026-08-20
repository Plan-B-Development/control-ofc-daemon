//! Thermal-safety tick: the 105/80/60C ladder + no-sensor fallback +
//! DEC-190 dropout handling. Pure decision returning SafetyDecision (C3).

use super::*;

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
pub(crate) fn evaluate_safety_tick(
    reading: super::CpuReading,
    no_cpu_sensor_cycles: &mut u32,
    safety: &mut crate::safety::ThermalSafetyRule,
) -> SafetyDecision {
    use super::CpuReading;

    // DEC-269: only a FRESH reading may drive the rule's state machine. A stale
    // one must not clear the latch, advance the recovery counter, or trigger a
    // new emergency — it is evidence of what *was* true, not what is.
    let (fresh_cpu_c, stale_cpu_c) = match reading {
        CpuReading::Fresh(t) => (Some(t), None),
        CpuReading::Stale(t) => (None, Some(t)),
        CpuReading::Absent => (None, None),
    };
    let hottest_cpu_c = fresh_cpu_c;
    // Track cycles with no CPU temp sensor. If missing for too long, force
    // fans to a safe minimum as a defensive fallback (P0-R1).
    let forced_by_no_sensor = if hottest_cpu_c.is_none() {
        *no_cpu_sensor_cycles += 1;
        let n = *no_cpu_sensor_cycles;
        // Deliberately NOT logged here. What this branch forces depends on
        // decisions made further down (a latched emergency or a recovery floor
        // outranks it, and a stale-but-hot reading suppresses it), so a message
        // written at this point can only guess. It used to hardcode "forcing to
        // 40%" and was simply false whenever `held_while_stale` won — at exactly
        // the moment, a live emergency going blind, when an operator most needs
        // the log to be true. The honest message is emitted after the decision.
        let _ = n;
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
    let sensor_missing = hottest_cpu_c.is_none();
    // `is_active()` reflects the LATCH: with the sensor missing, `evaluate()` is
    // not called, so a latched emergency stays latched (it can only clear on a
    // real reading at or below the release temp).
    let emergency_latched = safety.is_active();

    // DEC-190: a CPU-sensor dropout while a thermal emergency is latched must
    // not let fans fall to profile control. Before this fix, cycles 1–4 of such
    // a dropout forced nothing — `evaluate()` cannot run without a reading and
    // the 5-cycle no-sensor fallback had not yet tripped — so fans dropped from
    // forced-100% to the curve mid-emergency while `thermal_state` still
    // (incoherently) reported "emergency". Force the no-sensor safe floor
    // immediately in that case and report it honestly. The normal-operation
    // no-sensor debounce (`forced_by_no_sensor`, 5 cycles) is left unchanged: a
    // transient 1–2 cycle blip with no latched emergency must not spin fans.
    let force_no_sensor = forced_by_no_sensor || (sensor_missing && emergency_latched);

    // DEC-269: THE INVARIANT — losing sight of a sensor must never *lower* an
    // already-forced safety output.
    //
    // With a stale reading, hold whatever the rule is currently forcing (100%
    // latched, 60% mid-recovery) instead of falling to NO_SENSOR_SAFE_PCT.
    // Before this, a single poll leg exceeding the freshness budget — a wedged
    // USB-HID or NVML read, with the task still alive so supervision never
    // fires — dropped a latched emergency from 100% to 40% on a CPU last
    // measured at 95 C. Worse, it flapped: at ~5 s legs against a 1 Hz engine
    // the age crosses the budget on alternate ticks, oscillating 100/40/100.
    //
    // `Absent` deliberately does NOT take this path: DEC-190 chose
    // NO_SENSOR_SAFE_PCT for a vanished sensor on the reasoning that it cannot
    // confirm a live emergency, and that decision stands untouched.
    let held_while_stale = if stale_cpu_c.is_some() {
        safety.held_output_pct()
    } else {
        None
    };

    // DEC-269 round 2: the invariant above was implemented too narrowly. It held
    // only for output the rule *itself* was already forcing — so with nothing
    // latched, a stale-but-hot reading fell through to the no-sensor floor and
    // fans dropped from a curve output of ~85% to 40%, on a CPU last measured at
    // 104 C. That is a reduction in cooling caused by going blind, which is the
    // exact thing this ADR exists to forbid, one rung out. Worse, it is a
    // plausible route TO 105 C — at which point the emergency cannot fire,
    // because we are blind.
    //
    // So: while the last known temperature was at or above the release
    // threshold, do not force the no-sensor floor at all. Let the tick fall
    // through to profile evaluation, which runs on that same stale value and
    // therefore keeps commanding a high duty — exactly what the daemon did
    // before the freshness filter existed. The floor still applies once the last
    // known temperature was genuinely cool, which is the case DEC-132 was
    // written for.
    //
    // Note this cannot manufacture an emergency: it suppresses a force, never
    // adds one, and never touches the rule's state.
    let stale_but_hot = stale_cpu_c.is_some_and(|t| t >= safety.release_temp_c());

    // Suppressed rather than folded into `force_no_sensor` above, because the
    // 5-cycle counter must keep advancing: if the reading later goes Absent, or
    // cools, the debounce should already be satisfied rather than restarting.
    let force_no_sensor = force_no_sensor && !stale_but_hot;

    let thermal_state = if emergency_latched && (!sensor_missing || held_while_stale.is_some()) {
        "emergency"
    } else if held_while_stale.is_some() {
        // DEC-269 round 2: a held output outranks the no-sensor floor here for
        // the same reason it does in `forced_pct` below. Reporting
        // "no_sensor_fallback" while actually holding the 60% recovery floor
        // made `thermal_state` mean two different duties, which is precisely the
        // "reports one thing, does another" failure this change set exists to
        // remove. Four reviewers found it independently.
        "recovery"
    } else if force_no_sensor {
        // Covers BOTH the latched-emergency dropout (DEC-190) and the plain
        // 5-cycle no-sensor fallback (DEC-132): in each the daemon forces
        // NO_SENSOR_SAFE_PCT, so the GUI must surface a distinct safety state
        // (it has no control loop to stand down since the DEC-165 cutover — it
        // is display-only) rather than a stale "emergency" with no force, or a
        // bare "normal".
        //
        // Reaching this arm now genuinely implies NO_SENSOR_SAFE_PCT: the two
        // cases that could hold a different duty (a latched emergency, a
        // recovery floor) are both caught by earlier arms. The previous claim of
        // mutual exclusivity with "recovery" was false once `held_while_stale`
        // existed.
        "no_sensor_fallback"
    } else if safety_pct.is_some() {
        "recovery"
    } else {
        "normal"
    };

    // Order is the invariant: a fresh verdict wins, then a held output, and only
    // then the no-sensor floor. Putting the floor anywhere earlier is what let a
    // stale reading lower a latched emergency.
    let forced_pct = safety_pct.or(held_while_stale).or(if force_no_sensor {
        Some(constants::NO_SENSOR_SAFE_PCT)
    } else {
        None
    });

    // Edge-triggered, and written from the DECISION rather than from the branch
    // that proposed it, so the percentage and the cause are both true (DEC-269).
    if *no_cpu_sensor_cycles == constants::NO_SENSOR_CYCLE_THRESHOLD {
        let cause = if stale_cpu_c.is_some() {
            "the CPU temperature sensor has stopped updating"
        } else {
            "no CPU temperature sensor was found"
        };
        match forced_pct {
            Some(pct) => log::error!(
                "SAFETY: {cause} for {} consecutive cycles — forcing all \
                 OpenFan+hwmon fans to {pct}%",
                constants::NO_SENSOR_CYCLE_THRESHOLD
            ),
            None => log::warn!(
                "SAFETY: {cause} for {} consecutive cycles — the last known \
                 temperature was at or above the release threshold, so fan \
                 curves continue on it rather than dropping to the no-sensor \
                 floor",
                constants::NO_SENSOR_CYCLE_THRESHOLD
            ),
        }
    }

    SafetyDecision {
        thermal_state,
        forced_pct,
    }
}
