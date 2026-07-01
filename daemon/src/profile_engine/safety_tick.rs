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

    let thermal_state = if emergency_latched && !sensor_missing {
        "emergency"
    } else if force_no_sensor {
        // Covers BOTH the latched-emergency dropout (DEC-190) and the plain
        // 5-cycle no-sensor fallback (DEC-132): in each the daemon forces
        // NO_SENSOR_SAFE_PCT and force-takes the hwmon lease, so the GUI must
        // stand down — surface a distinct state rather than a stale "emergency"
        // with no force, or a bare "normal". (Mutually exclusive with the
        // "recovery" arm below: any no-sensor force implies `safety_pct` is None,
        // since `evaluate()` did not run this tick.)
        "no_sensor_fallback"
    } else if safety_pct.is_some() {
        "recovery"
    } else {
        "normal"
    };

    let forced_pct = safety_pct.or(if force_no_sensor {
        Some(constants::NO_SENSOR_SAFE_PCT)
    } else {
        None
    });

    SafetyDecision {
        thermal_state,
        forced_pct,
    }
}
