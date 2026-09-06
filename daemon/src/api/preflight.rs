//! Diagnostic preflight — the shared safety predicates, and the typed report
//! `GET /diagnostics/preflight` publishes (AIO Phase 8 Batch 1 §1, §6.1).
//!
//! # What this module is, and what it deliberately is not
//!
//! `AIO-Phase7-Batch1.md` §1 asks for a "common diagnostic safety state machine"
//! so that "each diagnostic does not implement its own safety rules". The agreed
//! scope (`AIO-Phase8-Batch1-SCOPE-AGREED.md`, Q3) settles what "common" means
//! here: **one definition of the predicates, consumed by everybody — not a
//! rewrite of three working `[SAFETY]` handlers onto a new state machine.**
//!
//! That distinction is the whole design. `hwmon_verify_handler`,
//! `hwmon_characterize_handler` and `calibrate_handler` each open with the same
//! sequence — shutdown guard, thermal guard, controller present, header exists,
//! claim the single-flight slot, resolve the pump floor, force-take the Verify
//! lease — and every step of it is *already* a shared function
//! ([`super::handlers::verify_thermal_guard`],
//! [`super::handlers::begin_verify_pause`],
//! [`super::handlers::AppState::header_is_pump_protected`]). So this module
//! achieves "one definition" by **calling those same functions**, and the three
//! handlers needed no edit at all. Their control flow, their guard drop order
//! and the two skip rules living inside `RestoreOnDrop::drop` are untouched by
//! construction rather than by care — which is the only way to be sure a
//! reporting layer did not move a safety boundary.
//!
//! # Why the report is pure
//!
//! [`build_report`] takes a plain [`PreflightInputs`] and returns a
//! [`PreflightReport`]. The handler gathers the inputs from `AppState`; every
//! *decision* is here, and is therefore unit-testable with no sysfs, no cache
//! and no hardware. Same shape as `characterization::summarise`, and for the
//! same reason: a rule that can only be exercised through a handler is a rule
//! nothing tests properly.
//!
//! # The one genuinely new predicate
//!
//! [`temperature_freshness`] is new safety *input*, not a refactor.
//! `calibration::check_thermal_safety` iterates whatever the state cache holds
//! and has never had a view of how old those readings are, so a poll loop wedged
//! on an unresponsive chip presents its last-known-good temperatures
//! indefinitely and every thermal gate passes on them. §1 requires "required
//! temperature source becomes stale/unavailable" as both a preflight check and a
//! runtime abort trigger, so it is expressed here once and consumed by both.
//!
//! **It does not change what the existing diagnostics do.** A stale temperature
//! source *blocks* `control_path_discovery` (the new diagnostic, whose abort
//! triggers this batch defines) and *warns* for verify and characterisation,
//! because those two handlers do not refuse on it and a preflight that claimed
//! otherwise would be lying about the daemon's own behaviour — which §6.1
//! forbids in exactly those words: the GUI "reflects daemon decisions".

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use crate::constants;

// ── Vocabulary ───────────────────────────────────────────────────────

/// Per-check state. Stable tokens; the client owns the wording and must render
/// an unrecognised one rather than dropping it (the 273-i rule).
pub const CHECK_PASS: &str = "pass";
pub const CHECK_WARN: &str = "warn";
pub const CHECK_FAIL: &str = "fail";
pub const CHECK_UNKNOWN: &str = "unknown";
pub const CHECK_NOT_APPLICABLE: &str = "not_applicable";

/// Overall verdict.
pub const VERDICT_READY: &str = "ready";
pub const VERDICT_WARN: &str = "warn";
pub const VERDICT_BLOCKED: &str = "blocked";

/// Stable check ids, in the order §1 lists them.
pub const CHECK_TARGET: &str = "target_discoverable";
pub const CHECK_ROLE: &str = "header_role";
pub const CHECK_WRITABLE: &str = "pwm_writable";
pub const CHECK_READBACK: &str = "pwm_readback";
pub const CHECK_OWNERSHIP: &str = "control_ownership";
pub const CHECK_SAFE_MINIMUM: &str = "safe_minimum";
pub const CHECK_TEMPERATURE: &str = "temperature_source";
pub const CHECK_THERMAL: &str = "thermal_state";
pub const CHECK_RECLAIM: &str = "reclaim_state";
pub const CHECK_ORIGINAL_STATE: &str = "original_state";
pub const CHECK_SUPPORTING: &str = "supporting_cooling";

/// The diagnostics a preflight can be requested for.
///
/// The distinction matters for exactly one check — see
/// [`Diagnostic::blocks_on_stale_temperature`] — and is otherwise presentational.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnostic {
    Verify,
    Characterization,
    ControlPathDiscovery,
}

impl Diagnostic {
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "pwm_verify" => Some(Self::Verify),
            "pwm_characterization" => Some(Self::Characterization),
            "control_path_discovery" => Some(Self::ControlPathDiscovery),
            _ => None,
        }
    }

    pub fn token(self) -> &'static str {
        match self {
            Self::Verify => "pwm_verify",
            Self::Characterization => "pwm_characterization",
            Self::ControlPathDiscovery => "control_path_discovery",
        }
    }

    /// [SAFETY] Does a stale temperature source *block* this diagnostic, or only
    /// warn about it?
    ///
    /// Only the new diagnostic blocks. `pwm_verify` and `pwm_characterization`
    /// have shipped since 2.32.0 without a staleness gate, and adding one here
    /// would change what those endpoints do from a change scoped to *reporting*
    /// — the Q3 constraint. Reporting it as a warning is the honest middle: the
    /// operator sees the risk, and the preflight does not promise a refusal the
    /// daemon will not perform.
    pub fn blocks_on_stale_temperature(self) -> bool {
        matches!(self, Self::ControlPathDiscovery)
    }
}

// ── Wire types ───────────────────────────────────────────────────────

/// One preflight row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreflightCheck {
    /// Stable token — one of the `CHECK_*` ids above.
    pub check_id: String,
    /// `pass` | `warn` | `fail` | `unknown` | `not_applicable`.
    pub state: String,
    /// Human-readable specifics. Never the only carrier of meaning: every state
    /// the client acts on is in `state`.
    pub detail: String,
}

impl PreflightCheck {
    fn new(check_id: &str, state: &str, detail: impl Into<String>) -> Self {
        Self {
            check_id: check_id.into(),
            state: state.into(),
            detail: detail.into(),
        }
    }
}

/// Body of `GET /diagnostics/preflight`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreflightReport {
    pub header_id: String,
    /// Which diagnostic this preflight was evaluated for.
    pub diagnostic: String,
    /// `ready` | `warn` | `blocked`.
    pub verdict: String,
    pub checks: Vec<PreflightCheck>,
    /// The `check_id`s that produced `blocked`. Empty unless `verdict` is
    /// `blocked`; present so a client can name the blockers without re-deriving
    /// the verdict rule (and therefore without being able to disagree with it).
    pub blocking: Vec<String>,
}

/// Freshness of the temperature telemetry a diagnostic depends on.
///
/// Derived by [`temperature_freshness`]; a plain struct so the handler can hand
/// it to [`build_report`] and a test can construct one directly.
#[derive(Debug, Clone, PartialEq)]
pub struct TemperatureFreshness {
    /// How many temperature readings the cache holds at all.
    pub total: usize,
    /// How many are within the age bound.
    pub fresh: usize,
    /// The freshest reading's age, when there is one.
    pub newest_age_ms: Option<u64>,
    /// Id of the freshest reading, for the detail line.
    pub newest_id: Option<String>,
}

impl TemperatureFreshness {
    /// Is there at least one usable temperature source?
    pub fn is_usable(&self) -> bool {
        self.fresh > 0
    }
}

/// State of the cooling that is expected to keep running while this header is
/// tested (Overview § "Supporting-device rule").
///
/// **Reported, never driven** (agreed scope Q13). The engine's write phase is
/// already paused for the diagnostic's lifetime, so every other fan holds its
/// last commanded duty — which is itself "a known safe operating state". This
/// struct says whether that state is one an operator should be happy with; the
/// diagnostic remains a single-header writer and never touches a sibling.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SupportingCooling {
    /// False when this header belongs to no cooling device, in which case there
    /// is no supporting-cooling claim to make and the check reports
    /// `not_applicable` rather than inventing a pass.
    pub applicable: bool,
    pub device_id: Option<String>,
    /// Sibling members of the same cooling device, excluding this header.
    pub siblings: usize,
    /// Siblings observably moving air or coolant — a non-zero tach, or a
    /// non-zero PWM readback where no tach exists.
    pub siblings_running: usize,
    /// Siblings whose state could not be read at all.
    pub siblings_unknown: usize,
}

/// Everything [`build_report`] needs. Gathered by the handler; plain data so the
/// decision layer never touches hardware.
#[derive(Debug, Clone)]
pub struct PreflightInputs {
    pub header_id: String,
    pub diagnostic: Diagnostic,
    /// Is the header still present in discovery?
    pub header_known: bool,
    pub is_writable: bool,
    /// Did a `pwmN` read succeed just now?
    pub readback_pct: Option<u8>,
    pub pwm_enable: Option<u8>,
    /// Display role token, for the detail line only.
    pub role: String,
    /// [SAFETY] The UNION predicate, never the wire `role` (DEC-312).
    pub pump_protected: bool,
    /// The floor this header's commands will be clamped to.
    pub effective_floor_pct: u8,
    /// Is the single-flight verify slot already claimed?
    pub slot_busy: bool,
    /// Cumulative BIOS/EC reclaims seen on this header since boot.
    pub enable_revert_count: u64,
    pub temperature: TemperatureFreshness,
    /// `Some(state)` when the thermal ladder is forcing.
    pub thermal_forcing: Option<String>,
    /// `Some((sensor_id, temp_c, limit_c))` when a sensor is over the
    /// diagnostic temperature limit.
    pub too_hot: Option<(String, f64, f64)>,
    pub supporting: SupportingCooling,
}

// ── The one new predicate (pure) ─────────────────────────────────────

/// [SAFETY] How fresh is the temperature telemetry, as of `now`?
///
/// `readings` is `(id, updated_at)` for every cached temperature. Pure and
/// `now`-injected on purpose: `std::time::Instant` does **not** advance under
/// `#[tokio::test(start_paused)]` (CLAUDE.md, tokio-test trap 1), so a test that
/// tried to *wait* for staleness would age by ~0 ms and pass vacuously. Ages are
/// stamped by construction instead — the caller subtracts from `now`.
pub fn temperature_freshness(
    readings: &[(String, Instant)],
    max_age: Duration,
    now: Instant,
) -> TemperatureFreshness {
    let mut fresh = 0usize;
    let mut newest: Option<(String, Duration)> = None;
    for (id, updated_at) in readings {
        // `saturating_duration_since`, not `-`: a reading stamped fractionally
        // in the future (two clock reads racing) must read as age 0, not panic.
        let age = now.saturating_duration_since(*updated_at);
        if age <= max_age {
            fresh += 1;
        }
        if newest.as_ref().is_none_or(|(_, best)| age < *best) {
            newest = Some((id.clone(), age));
        }
    }
    TemperatureFreshness {
        total: readings.len(),
        fresh,
        newest_age_ms: newest.as_ref().map(|(_, age)| age.as_millis() as u64),
        newest_id: newest.map(|(id, _)| id),
    }
}

// ── Report derivation (pure) ─────────────────────────────────────────

/// Roll individual check states up into one verdict.
///
/// Any `fail` blocks. `unknown` is deliberately **not** a block: §5's reporting
/// rule is that lack of evidence must not become a PASS, and the mirror of that
/// is that it must not become a FAIL either — an unreadable `pwm_enable` on a
/// driver that does not expose one is not a safety event.
pub fn verdict_for(checks: &[PreflightCheck]) -> &'static str {
    if checks.iter().any(|c| c.state == CHECK_FAIL) {
        VERDICT_BLOCKED
    } else if checks.iter().any(|c| c.state == CHECK_WARN) {
        VERDICT_WARN
    } else {
        VERDICT_READY
    }
}

/// Build the full preflight report. Pure.
pub fn build_report(inputs: &PreflightInputs) -> PreflightReport {
    let mut checks = Vec::with_capacity(11);

    // 1. Target still discoverable. Everything below is meaningless without it,
    //    but the report is still returned in full so the client can render the
    //    whole list rather than a single error.
    checks.push(if inputs.header_known {
        PreflightCheck::new(CHECK_TARGET, CHECK_PASS, "Header found in discovery")
    } else {
        PreflightCheck::new(
            CHECK_TARGET,
            CHECK_FAIL,
            format!("No header '{}' in the current scan", inputs.header_id),
        )
    });

    // 2. Role, and 3. the ambiguous-role rule. §1 requires an ambiguous role to
    //    receive pump-safe treatment; `header_is_pump_protected` is that rule —
    //    a UNION of the user's assignment and the daemon's own label evidence,
    //    so it can only ever ADD protection.
    checks.push(if inputs.pump_protected {
        PreflightCheck::new(
            CHECK_ROLE,
            CHECK_PASS,
            format!(
                "Role '{}' — pump-protected: never stopped, never driven below {}%",
                inputs.role, inputs.effective_floor_pct
            ),
        )
    } else {
        PreflightCheck::new(
            CHECK_ROLE,
            CHECK_PASS,
            format!("Role '{}' — not pump-protected", inputs.role),
        )
    });

    // 4. Writable PWM.
    checks.push(if inputs.is_writable {
        PreflightCheck::new(CHECK_WRITABLE, CHECK_PASS, "PWM is writable")
    } else {
        PreflightCheck::new(
            CHECK_WRITABLE,
            CHECK_FAIL,
            "This header is read-only; an active diagnostic cannot run on it",
        )
    });

    // 5. Readback availability. A missing readback does not block: the sweep
    //    still measures command acceptance and tach response, and saying
    //    otherwise would refuse a diagnostic on every driver that exposes no
    //    readback.
    checks.push(match inputs.readback_pct {
        Some(pct) => PreflightCheck::new(CHECK_READBACK, CHECK_PASS, format!("Reads back {pct}%")),
        None => PreflightCheck::new(
            CHECK_READBACK,
            CHECK_WARN,
            "PWM readback unavailable — command acceptance cannot be confirmed",
        ),
    });

    // 6. Ownership. The single-flight slot is the real gate: at most one
    //    diagnostic drives hardware at a time, and a second one is a 409.
    checks.push(if inputs.slot_busy {
        PreflightCheck::new(
            CHECK_OWNERSHIP,
            CHECK_FAIL,
            "Another hardware diagnostic is already running",
        )
    } else {
        PreflightCheck::new(
            CHECK_OWNERSHIP,
            CHECK_PASS,
            "Control can be taken — no diagnostic in progress",
        )
    });

    // 7. Safe minimum known.
    checks.push(PreflightCheck::new(
        CHECK_SAFE_MINIMUM,
        CHECK_PASS,
        format!(
            "Commands clamped to {}%–100%",
            inputs.effective_floor_pct.max(constants::DISCOVERY_MIN_PCT)
        ),
    ));

    // 8. [SAFETY] Temperature source freshness — the new predicate. Blocking for
    //    the new diagnostic only; see `Diagnostic::blocks_on_stale_temperature`.
    let stale_state = if inputs.diagnostic.blocks_on_stale_temperature() {
        CHECK_FAIL
    } else {
        CHECK_WARN
    };
    checks.push(if inputs.temperature.total == 0 {
        PreflightCheck::new(
            CHECK_TEMPERATURE,
            stale_state,
            "No temperature sensors are available",
        )
    } else if inputs.temperature.is_usable() {
        PreflightCheck::new(
            CHECK_TEMPERATURE,
            CHECK_PASS,
            match (
                &inputs.temperature.newest_id,
                inputs.temperature.newest_age_ms,
            ) {
                (Some(id), Some(age)) => format!("{id} · {age} ms old"),
                _ => format!("{} fresh reading(s)", inputs.temperature.fresh),
            },
        )
    } else {
        PreflightCheck::new(
            CHECK_TEMPERATURE,
            stale_state,
            match inputs.temperature.newest_age_ms {
                Some(age) => format!(
                    "Every temperature reading is stale — freshest is {age} ms old, \
                     limit {} ms",
                    constants::DIAGNOSTIC_TEMP_MAX_AGE.as_millis()
                ),
                None => "Every temperature reading is stale".to_string(),
            },
        )
    });

    // 9. Thermal failsafe. Both limbs, because they are not the same test
    //    (DEC-297): the ladder latches at its per-machine trigger and releases
    //    at 80 °C, so the band above the release and below the diagnostic limit
    //    passes the temperature test while every fan is still being forced.
    checks.push(if let Some(state) = &inputs.thermal_forcing {
        PreflightCheck::new(
            CHECK_THERMAL,
            CHECK_FAIL,
            format!("Thermal safety is forcing fan output ({state})"),
        )
    } else if let Some((sensor, temp, limit)) = &inputs.too_hot {
        PreflightCheck::new(
            CHECK_THERMAL,
            CHECK_FAIL,
            format!("{sensor} at {temp:.1}°C exceeds the {limit:.0}°C diagnostic limit"),
        )
    } else {
        PreflightCheck::new(CHECK_THERMAL, CHECK_PASS, "No thermal failsafe active")
    });

    // 10. Existing reclaim state. `pwm_enable != 1` right now means something
    //     else holds the header; a non-zero historical count is worth saying but
    //     is not a reason to refuse.
    checks.push(match inputs.pwm_enable {
        Some(1) | None if inputs.enable_revert_count == 0 => {
            PreflightCheck::new(CHECK_RECLAIM, CHECK_PASS, "No reclaim detected")
        }
        Some(1) | None => PreflightCheck::new(
            CHECK_RECLAIM,
            CHECK_WARN,
            format!(
                "This header has been reclaimed by BIOS/EC {} time(s) since boot",
                inputs.enable_revert_count
            ),
        ),
        Some(mode) => PreflightCheck::new(
            CHECK_RECLAIM,
            CHECK_WARN,
            format!("pwm_enable is {mode} — another controller currently holds this header"),
        ),
    });

    // 11. Original state captured. Without it there is nothing to restore to,
    //     which §1 lists under Restoration rather than under the abort triggers —
    //     so it warns rather than blocks, and the restore guard reports
    //     `no_original_duty` if the run does move the header.
    checks.push(match inputs.readback_pct {
        Some(pct) => PreflightCheck::new(
            CHECK_ORIGINAL_STATE,
            CHECK_PASS,
            format!("Captured {pct}% to restore on exit"),
        ),
        None => PreflightCheck::new(
            CHECK_ORIGINAL_STATE,
            CHECK_WARN,
            "The pre-test duty could not be read — this header cannot be put back",
        ),
    });

    // 12. Supporting cooling — reported, never driven (Q13).
    checks.push(if !inputs.supporting.applicable {
        PreflightCheck::new(
            CHECK_SUPPORTING,
            CHECK_NOT_APPLICABLE,
            "This header is not part of a configured cooling device",
        )
    } else if inputs.supporting.siblings == 0 {
        PreflightCheck::new(
            CHECK_SUPPORTING,
            CHECK_NOT_APPLICABLE,
            "This cooling device has no other members",
        )
    } else if inputs.supporting.siblings_running > 0 {
        PreflightCheck::new(
            CHECK_SUPPORTING,
            CHECK_PASS,
            format!(
                "{} of {} sibling member(s) running and held at their current duty \
                 for the test",
                inputs.supporting.siblings_running, inputs.supporting.siblings
            ),
        )
    } else if inputs.supporting.siblings_unknown == inputs.supporting.siblings {
        PreflightCheck::new(
            CHECK_SUPPORTING,
            CHECK_UNKNOWN,
            "No sibling member's state could be read",
        )
    } else {
        PreflightCheck::new(
            CHECK_SUPPORTING,
            CHECK_WARN,
            format!(
                "No sibling member of this cooling device is observably running \
                 ({} of {} unreadable)",
                inputs.supporting.siblings_unknown, inputs.supporting.siblings
            ),
        )
    });

    let verdict = verdict_for(&checks);
    let blocking = checks
        .iter()
        .filter(|c| c.state == CHECK_FAIL)
        .map(|c| c.check_id.clone())
        .collect();

    PreflightReport {
        header_id: inputs.header_id.clone(),
        diagnostic: inputs.diagnostic.token().to_string(),
        verdict: verdict.to_string(),
        checks,
        blocking,
    }
}
