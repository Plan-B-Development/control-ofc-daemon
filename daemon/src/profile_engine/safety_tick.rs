//! Thermal-safety tick: the trigger/release ladder + no-sensor fallback, as ONE
//! decision table. Pure decision returning SafetyDecision (C3).
//!
//! # Why a table (DEC-386, `TS-l`)
//!
//! This used to be a sequence of suppressions — six interacting flags, and a
//! `thermal_state` if-chain computed separately from a `forced_pct` or-chain —
//! patched by DEC-190, 267, 269, 269 round 2 and 272. The two chains disagreed
//! once already (DEC-269 round 2 reported `no_sensor_fallback` while holding the
//! 60 % recovery floor). Each arm below returns a whole [`SafetyDecision`], so the
//! reported state and the forced duty can no longer come apart, and the match is
//! exhaustive over (reading, latched), so a new case is a compile error rather
//! than a silent fall-through.
//!
//! # The table
//!
//! | reading | latched | decision |
//! | --- | --- | --- |
//! | fresh | — | the rule decides (latch at the trigger, release at or below release) |
//! | stale or absent | yes | **emergency, 100 %** — held until a FRESH reading at or below release |
//! | stale, last value at or above release | no | normal — curves run on that value |
//! | stale or absent, blind for the debounce | no | no-sensor fallback |
//! | stale or absent, inside the debounce | no | normal |
//!
//! Two rows changed in DEC-386 and both were the user's decisions (2026-09-18):
//! a latched emergency that loses its sensor ENTIRELY now holds 100 % (DEC-190
//! dropped it to the 40 % no-sensor floor; a stale sensor already held), and
//! release hands control straight back — the two-tick 60 % recovery rung is gone.
//!
//! `TS-s` is a row of this table by decision, not an omission (DEC-400): a
//! latched emergency releases only on a fresh reading at or below release,
//! however long it is held, and a spurious reading above the trigger keeps it
//! latched. The user chose that over a bounded latch, following IEC 61511-1
//! 11.2.7: a safety function that has tripped stays tripped until its reset.

use super::*;

/// Outcome of one safety-tick evaluation (pure decision, unit-testable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SafetyDecision {
    /// `"normal"` | `"emergency"` | `"no_sensor_fallback"` — reported to the
    /// cache and surfaced via `GET /status` (DEC-132) and `/diagnostics/hardware`.
    /// `"recovery"` is no longer produced (DEC-386); clients still render it for
    /// older daemons.
    pub(crate) thermal_state: &'static str,
    /// When `Some`, the engine forces this duty as a FLOOR this tick (DEC-307):
    /// at 100 % every OpenFan channel and writable hwmon header the machine HAS,
    /// below it the active profile's members only (DEC-382). GPU fans are
    /// excluded by design (DEC-130).
    ///
    /// The enumeration is the design, not a message (DEC-371): this is a pure
    /// decision with no view of the backends, so the log line below names a duty
    /// and leaves the reach to the engine, which reports which backends had
    /// something to drive (DEC-372 — not which writes landed; see `ForcedScope`).
    pub(crate) forced_pct: Option<u8>,
}

impl SafetyDecision {
    /// Profile control proceeds; nothing is forced.
    const NORMAL: Self = Self {
        thermal_state: "normal",
        forced_pct: None,
    };

    /// The latched emergency, at the duty the rule forces.
    fn emergency(safety: &crate::safety::ThermalSafetyRule) -> Self {
        Self {
            thermal_state: "emergency",
            forced_pct: Some(safety.forced_output_pct()),
        }
    }

    /// No usable CPU reading for the debounce, and nothing latched.
    const NO_SENSOR_FALLBACK: Self = Self {
        thermal_state: "no_sensor_fallback",
        forced_pct: Some(constants::NO_SENSOR_SAFE_PCT),
    };
}

/// Evaluate the thermal safety rule + no-CPU-sensor fallback for one tick.
///
/// Owns the no-sensor counter (with its threshold-edge logging) and the decision
/// table in the module doc, separated from controller I/O so every row is
/// unit-testable (DEC-135).
pub(crate) fn evaluate_safety_tick(
    reading: super::CpuReading,
    no_cpu_sensor_cycles: &mut u32,
    safety: &mut crate::safety::ThermalSafetyRule,
) -> SafetyDecision {
    use super::CpuReading;

    // Cycles without a FRESH CPU reading. Stale and absent both count, so a
    // reading that goes stale and is then evicted has already served its
    // debounce; a fresh reading resets it.
    let blind_cycles = match reading {
        CpuReading::Fresh(_) => {
            let n = *no_cpu_sensor_cycles;
            if n >= constants::NO_SENSOR_CYCLE_THRESHOLD {
                log::info!("CPU temperature sensor recovered after {n} missing cycles");
            }
            *no_cpu_sensor_cycles = 0;
            0
        }
        CpuReading::Stale(_) | CpuReading::Absent => {
            *no_cpu_sensor_cycles += 1;
            *no_cpu_sensor_cycles
        }
    };
    let blind_for_the_debounce = blind_cycles >= constants::NO_SENSOR_CYCLE_THRESHOLD;

    let decision = match (reading, safety.is_active()) {
        // DEC-269: only a FRESH reading may move the latch — latch at the
        // trigger, release at or below release. Nothing else in this table
        // touches the rule's state.
        (CpuReading::Fresh(t), _) => match safety.evaluate(t) {
            Some(_) => SafetyDecision::emergency(safety),
            None => SafetyDecision::NORMAL,
        },

        // [SAFETY] DEC-269 / DEC-386: going blind never LOWERS a latched
        // emergency. The latch means the trigger was seen and release has not
        // been since, and that stays true whether the sensor is stale or gone.
        // Held until a fresh reading at or below release; DEC-190's 40 % for a
        // vanished sensor is retired.
        (CpuReading::Stale(_) | CpuReading::Absent, true) => SafetyDecision::emergency(safety),

        // [SAFETY] DEC-269 round 2: nothing latched, and the last thing we knew
        // was at or above release. Forcing the no-sensor floor here would LOWER
        // cooling — a curve running on that same stale value is commanding more
        // — on a CPU that may be heading for the trigger we can no longer see.
        //
        // DEC-272: that curves still run on the stale value is a CROSS-MODULE
        // dependency. `curve_eligible` exempts `SensorKind::CpuTemp` from its age
        // filter for exactly this row; remove the exemption and this becomes a
        // HOLD at whatever the control last commanded. Pinned by
        // `a_stale_but_hot_cpu_curve_keeps_climbing_while_a_stale_gpu_curve_holds`.
        //
        // The debounce keeps counting through this row, so a reading that later
        // goes absent, or cools, has already served it.
        (CpuReading::Stale(t), false) if t >= safety.release_temp_c() => SafetyDecision::NORMAL,

        // DEC-132: no usable CPU reading for the debounce and nothing latched —
        // the floor, over the active profile's members only (DEC-382). A skipped
        // control's members are floored against their last applied duty, not
        // treated as uncommanded (`TS-p`, `ForceReach::ProfileMembers::held`).
        (CpuReading::Stale(_) | CpuReading::Absent, false) if blind_for_the_debounce => {
            SafetyDecision::NO_SENSOR_FALLBACK
        }

        // Inside the debounce: a transient 1–2 cycle blip must not spin fans.
        (CpuReading::Stale(_) | CpuReading::Absent, false) => SafetyDecision::NORMAL,
    };

    // Edge-triggered, and written from the DECISION rather than from the arm
    // that produced it, so the duty and the cause are both true (DEC-269).
    if blind_cycles == constants::NO_SENSOR_CYCLE_THRESHOLD {
        let cause = if matches!(reading, CpuReading::Stale(_)) {
            "the CPU temperature sensor has stopped updating"
        } else {
            "no CPU temperature sensor was found"
        };
        match decision.forced_pct {
            Some(pct) => log::error!(
                "SAFETY: {cause} for {} consecutive cycles — forcing fans to {pct}% \
                 ({})",
                constants::NO_SENSOR_CYCLE_THRESHOLD,
                decision.thermal_state
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

    decision
}
