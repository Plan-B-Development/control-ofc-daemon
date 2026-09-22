//! [SAFETY] The per-step gates a hardware diagnostic applies before it writes a
//! duty, defined once (DEC-407, Stage 3 of DEC-404).
//!
//! `characterization::run_sweep` spelled these out inline at two sites — the
//! top of every point and inside every hold — and the stall/restart probe
//! (`api::stall_probe`) needs the same ones at every write and on every sample.
//! Three copies of a safety rule is how one site ends up checking a subset
//! (DEC-336's finding), so the rule lives here and every loop calls it.
//!
//! **Behaviour-preserving by construction for characterisation.** The order is
//! the one `run_sweep` already used — shutdown, cancel, the three thermal
//! gates, keepalive — and the detail strings are produced from the same format
//! strings, parameterised only by the subject and the action the caller names.
//! Characterisation's own tests are the oracle for that claim.
//!
//! Control-path discovery keeps its own `thermal_gate` closure deliberately: its
//! stale-temperature detail carries no prefix, so folding it in here would change
//! a published string from a change that did not scope discovery.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::api::calibration::{
    check_thermal_safety, stale_temperature_refusal, thermal_force_state,
};
use crate::api::preflight::Diagnostic;
use crate::health::cache::StateCache;

/// Why [`step_gate`] stopped a run. The caller owns the wording of every arm
/// except [`GateStop::Thermal`], whose detail is already composed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GateStop {
    /// The daemon is going down; the caller must stop touching hardware.
    ShuttingDown,
    /// The caller's cancel flag is set.
    Cancelled,
    /// One of the three thermal gates refused; the detail says which.
    Thermal(ThermalRefusal, String),
    /// The keepalive failed: the deadman elapsed and a later diagnostic took
    /// this run's lease, so any further write would fail.
    Superseded,
}

/// Which of the three thermal gates refused — so a caller that publishes a
/// reason never re-parses the detail text or re-evaluates a gate whose answer
/// may have moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThermalRefusal {
    /// A sensor is over `CALIBRATION_MAX_TEMP_C`.
    TooHot,
    /// The thermal ladder is forcing fan output.
    Forcing,
    /// No usable temperature reading: the guards cannot be evaluated.
    Stale,
}


/// [SAFETY] The three thermal gates, in their established order: the two cheap
/// `value_c` comparisons (a sensor over `CALIBRATION_MAX_TEMP_C`, the ladder
/// forcing), then the freshness refusal — the only one of the three that can
/// see a poll loop that has stopped (DEC-336 / DEC-385).
///
/// `subject` and `action` complete the caller's detail, e.g. `"characterisation"`
/// and `"cannot write"`. Returns the first gate that refused and its detail.
pub(crate) fn thermal_refusal(
    cache: &StateCache,
    diagnostic: Diagnostic,
    subject: &str,
    action: &str,
) -> Option<(ThermalRefusal, String)> {
    if let Err(e) = check_thermal_safety(cache) {
        return Some((ThermalRefusal::TooHot, e.to_string()));
    }
    if let Some(state) = thermal_force_state(cache) {
        return Some((
            ThermalRefusal::Forcing,
            format!("thermal safety is forcing fan output ({state}); {subject} {action}"),
        ));
    }
    stale_temperature_refusal(cache, diagnostic).map(|reason| {
        (
            ThermalRefusal::Stale,
            format!("{subject} {action}: {reason}"),
        )
    })
}

/// [`thermal_refusal`]'s detail alone, for a caller that publishes no token.
pub(crate) fn thermal_gate(
    cache: &StateCache,
    diagnostic: Diagnostic,
    subject: &str,
    action: &str,
) -> Option<String> {
    thermal_refusal(cache, diagnostic, subject, action).map(|(_, detail)| detail)
}

/// [SAFETY] Every gate a diagnostic applies before a write, in order: shutdown,
/// cancel, [`thermal_gate`], keepalive.
///
/// The shutdown check comes first because the diagnostic task is detached: it
/// keeps running through `shutdown_sequence`, and a write landing after
/// `hand_back_hwmon` would re-assert `pwm_enable=1` through `set_pwm`'s reclaim
/// watchdog on a header nothing will drive again (DEC-290 / 277-c). Keepalive
/// comes last so the deadman is renewed only once every other gate has passed —
/// the DEC-296 rule that it measures liveness, not duration.
pub(crate) fn step_gate<S, K>(
    cache: &StateCache,
    diagnostic: Diagnostic,
    subject: &str,
    action: &str,
    shutting_down: &S,
    cancel: &AtomicBool,
    keepalive: &K,
) -> Result<(), GateStop>
where
    S: Fn() -> bool,
    K: Fn() -> bool,
{
    if shutting_down() {
        return Err(GateStop::ShuttingDown);
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(GateStop::Cancelled);
    }
    if let Some((which, detail)) = thermal_refusal(cache, diagnostic, subject, action) {
        return Err(GateStop::Thermal(which, detail));
    }
    if !keepalive() {
        return Err(GateStop::Superseded);
    }
    Ok(())
}
