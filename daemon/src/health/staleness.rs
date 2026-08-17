//! Staleness evaluation and health summary computation.
//!
//! Health computation is pure and deterministic: given a `DaemonState`,
//! config thresholds, and a reference `now` instant, it produces a
//! `HealthSummary` with no side effects.

use std::time::Instant;

use crate::health::state::DaemonState;

/// Status level for a subsystem or overall health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HealthStatus {
    /// Everything is within expected intervals.
    Ok,
    /// Data is stale but not critically so.
    Warn,
    /// Data is critically stale or subsystem has failed.
    Crit,
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::Warn => write!(f, "warn"),
            Self::Crit => write!(f, "crit"),
        }
    }
}

/// Health status for a single subsystem.
#[derive(Debug, Clone)]
pub struct SubsystemHealth {
    /// Subsystem name.
    pub name: String,
    /// Current status.
    pub status: HealthStatus,
    /// Age of the last update in milliseconds (None if never updated).
    pub age_ms: Option<u64>,
    /// Human-readable reason for the status.
    pub reason: String,
}

/// Complete health summary for the daemon.
#[derive(Debug, Clone)]
pub struct HealthSummary {
    /// Overall status (worst of all subsystems).
    pub overall: HealthStatus,
    /// Per-subsystem health reports.
    pub subsystems: Vec<SubsystemHealth>,
}

/// Configuration for staleness thresholds.
#[derive(Debug, Clone)]
pub struct StalenessConfig {
    /// Expected update interval for OpenFanController (ms).
    pub openfan_interval_ms: u64,
    /// Expected update interval for hwmon sensors (ms).
    pub hwmon_interval_ms: u64,
    /// Expected tick interval for the profile engine (ms).
    ///
    /// Unlike the two poll intervals this is **not** operator-configurable: the
    /// engine's period is a fixed 1 Hz (`profile_engine_loop`). It lives here so
    /// the threshold is visible next to its siblings and tests can shrink it.
    pub engine_interval_ms: u64,
}

impl Default for StalenessConfig {
    fn default() -> Self {
        Self {
            openfan_interval_ms: 1000,
            hwmon_interval_ms: 1000,
            engine_interval_ms: 1000,
        }
    }
}

/// The four reason strings a subsystem reports, one per status outcome.
///
/// Parameterised rather than hardcoded so each subsystem speaks in its own
/// terms: the poll loops report on *readings*, the engine on *ticks*.
struct SubsystemReasons {
    fresh: &'static str,
    stale: &'static str,
    critical: &'static str,
    never: &'static str,
}

/// Wording for the two data-polling subsystems (OpenFanController, hwmon).
const POLL_REASONS: SubsystemReasons = SubsystemReasons {
    fresh: "readings fresh",
    stale: "readings stale",
    critical: "readings critically stale",
    never: "never received data",
};

/// Wording for the profile engine (DEC-249). A stalled engine is not stale
/// *data* — it means nothing is driving the fans and nothing is evaluating the
/// 105°C rule, so the reasons say so in the operator's terms.
const ENGINE_REASONS: SubsystemReasons = SubsystemReasons {
    fresh: "evaluating on schedule",
    stale: "tick overdue",
    critical: "not ticking — fan control and thermal safety are stalled",
    never: "never ticked",
};

/// How long a single tick may legitimately take before it counts as wedged
/// rather than slow (DEC-259), as a multiple of the nominal 1 Hz period.
///
/// Derived, not guessed. The worst legitimate tick is a thermal `force_all` over
/// a degraded-but-open serial link: `NUM_CHANNELS` (10) writes, each bounded by
/// `serial.timeout_ms`, which the API caps at 1000 ms — so ~10 s for OpenFan
/// alone, plus the hwmon leg. 30× leaves room for that and for a slow sysfs
/// without ever reporting a genuinely dead engine as merely busy: past this
/// bound the tick is not slow, it is stuck, and the distinction stops being
/// useful to a user.
const WEDGED_TICK_MULTIPLE: u32 = 30;

/// Engine liveness, which is a different question from data freshness.
///
/// [SAFETY] DEC-259. A single timestamp could not tell a *slow* tick from a
/// *stopped* engine, and reported the worse of the two. `force_all` walks ten
/// OpenFan channels at up to 1 s each, so a degraded link makes a legitimate
/// tick take 5-10 s — and the surface then read "not ticking — fan control and
/// thermal safety are stalled" **while the engine was driving the 105 °C
/// emergency**. Exactly inverted, in the one state where a user most needs to
/// trust it. Widening the threshold would have fixed the false alarm by blinding
/// the surface to a real death for just as long; the pair of stamps distinguishes
/// the cases instead.
///
/// A tick is *in progress* when it has started and not yet completed. There it is
/// judged on how long it has been running: normal, slow-but-working, or wedged.
/// Between ticks it is judged on how long ago the last one finished.
fn engine_health(
    started: Option<Instant>,
    completed: Option<Instant>,
    now: Instant,
    interval_ms: u64,
) -> SubsystemHealth {
    let entry = |status, reason: &str, age: Option<u64>| SubsystemHealth {
        name: "engine".into(),
        status,
        age_ms: age,
        reason: reason.into(),
    };

    let Some(started_at) = started else {
        return entry(HealthStatus::Crit, ENGINE_REASONS.never, None);
    };

    // Age is always "since the last COMPLETED pass" when there has been one —
    // the honest answer to "how long since the engine last finished its work".
    let age = age_ms(completed.or(Some(started_at)), now);
    let in_progress = completed.is_none_or(|done| done < started_at);

    if in_progress {
        let running_ms = now.duration_since(started_at).as_millis() as u64;
        if running_ms <= interval_ms * 2 {
            // A tick that started moments ago is simply a tick.
            entry(HealthStatus::Ok, ENGINE_REASONS.fresh, age)
        } else if running_ms <= interval_ms * u64::from(WEDGED_TICK_MULTIPLE) {
            entry(
                HealthStatus::Warn,
                "tick still running — a slow write is holding it up",
                age,
            )
        } else {
            entry(
                HealthStatus::Crit,
                "tick stuck — the engine has not finished a pass",
                age,
            )
        }
    } else {
        match evaluate_staleness(completed, now, interval_ms) {
            HealthStatus::Ok => entry(HealthStatus::Ok, ENGINE_REASONS.fresh, age),
            HealthStatus::Warn => entry(HealthStatus::Warn, ENGINE_REASONS.stale, age),
            HealthStatus::Crit => entry(HealthStatus::Crit, ENGINE_REASONS.critical, age),
        }
    }
}

/// Evaluate the staleness of a subsystem given its last update time.
///
/// - OK: age <= 2 × interval
/// - WARN: age > 2 × interval and <= 5 × interval
/// - CRIT: age > 5 × interval
fn evaluate_staleness(
    last_update: Option<Instant>,
    now: Instant,
    interval_ms: u64,
) -> HealthStatus {
    let last = match last_update {
        Some(t) => t,
        None => return HealthStatus::Crit, // never updated
    };

    let age = now.duration_since(last);
    let age_ms = age.as_millis() as u64;

    if age_ms <= interval_ms * 2 {
        HealthStatus::Ok
    } else if age_ms <= interval_ms * 5 {
        HealthStatus::Warn
    } else {
        HealthStatus::Crit
    }
}

/// Compute age in milliseconds from an optional instant.
fn age_ms(last_update: Option<Instant>, now: Instant) -> Option<u64> {
    last_update.map(|t| now.duration_since(t).as_millis() as u64)
}

/// Build one subsystem's health entry from its last-update instant.
///
/// Extracted when the engine became a third subsystem (DEC-249) — the two
/// existing blocks were already identical apart from their name, threshold and
/// wording, and a third copy of the same twenty lines would have made the
/// duplication the dominant shape of this function.
fn subsystem_health(
    name: &str,
    last_update: Option<Instant>,
    now: Instant,
    interval_ms: u64,
    reasons: &SubsystemReasons,
) -> SubsystemHealth {
    let status = evaluate_staleness(last_update, now, interval_ms);
    SubsystemHealth {
        name: name.into(),
        status,
        age_ms: age_ms(last_update, now),
        reason: match status {
            HealthStatus::Ok => reasons.fresh.into(),
            HealthStatus::Warn => reasons.stale.into(),
            HealthStatus::Crit => match last_update {
                None => reasons.never.into(),
                Some(_) => reasons.critical.into(),
            },
        },
    }
}

/// Compute the health summary for the daemon.
///
/// This function is pure: it takes the current state, config, and a reference
/// time, and returns a deterministic health summary.
pub fn compute_health(
    state: &DaemonState,
    config: &StalenessConfig,
    now: Instant,
) -> HealthSummary {
    let ts = &state.subsystem_timestamps;

    // Order is part of the wire shape: existing clients read `subsystems[0]` as
    // openfan and `[1]` as hwmon. Engine is appended, never inserted.
    let subsystems = vec![
        subsystem_health(
            "openfan",
            ts.openfan,
            now,
            config.openfan_interval_ms,
            &POLL_REASONS,
        ),
        subsystem_health(
            "hwmon",
            ts.hwmon,
            now,
            config.hwmon_interval_ms,
            &POLL_REASONS,
        ),
        // DEC-249: the profile engine is the sole PWM writer and runs the 105°C
        // rule, so its liveness belongs in the same rollup as the poll loops. It
        // feeds `overall`, which is the point: a dead engine must not present as
        // a healthy daemon.
        //
        // DEC-266: the task is now supervised — a death restores hardware and
        // ends the process — so a client sees a dropped socket rather than a
        // green /status. This rollup remains the signal for a *degraded* engine
        // (slow ticks), which supervision does not cover.
        engine_health(
            ts.engine_started,
            ts.engine_completed,
            now,
            config.engine_interval_ms,
        ),
    ];

    // Overall: worst of all subsystems
    let overall = subsystems
        .iter()
        .map(|s| s.status)
        .max()
        .unwrap_or(HealthStatus::Ok);

    HealthSummary {
        overall,
        subsystems,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::{DaemonState, SubsystemTimestamps};
    use std::time::Duration;

    fn base_state() -> DaemonState {
        DaemonState::default()
    }

    /// Baseline for tests about the *poll* subsystems, with the engine
    /// heartbeat fresh.
    ///
    /// `overall` is the worst of every subsystem, and the engine is one of them
    /// (DEC-249) — an unstamped engine is Crit, which would otherwise drive
    /// `overall` and make an assertion about openfan/hwmon pass or fail for the
    /// wrong reason. Any test asserting `overall` should start here.
    fn state_with_live_engine(now: Instant) -> DaemonState {
        DaemonState {
            subsystem_timestamps: SubsystemTimestamps {
                engine_started: Some(now),
                engine_completed: Some(now),
                ..Default::default()
            },
            ..DaemonState::default()
        }
    }

    fn default_config() -> StalenessConfig {
        StalenessConfig::default()
    }

    // ── Basic staleness transitions ─────────────────────────────────

    #[test]
    fn never_updated_is_crit() {
        let state = base_state();
        let now = Instant::now();
        let health = compute_health(&state, &default_config(), now);

        assert_eq!(health.overall, HealthStatus::Crit);

        let openfan = &health.subsystems[0];
        assert_eq!(openfan.name, "openfan");
        assert_eq!(openfan.status, HealthStatus::Crit);
        assert!(openfan.reason.contains("never"));
    }

    #[test]
    fn fresh_data_is_ok() {
        let now = Instant::now();
        let mut state = base_state();
        state.subsystem_timestamps = SubsystemTimestamps {
            openfan: Some(now),
            hwmon: Some(now),
            aio: None,
            engine_started: Some(now),
            engine_completed: Some(now),
        };

        let health = compute_health(&state, &default_config(), now);
        assert_eq!(health.overall, HealthStatus::Ok);
        assert_eq!(health.subsystems[0].status, HealthStatus::Ok);
        assert_eq!(health.subsystems[1].status, HealthStatus::Ok);
        assert_eq!(health.subsystems[2].status, HealthStatus::Ok);
    }

    #[test]
    fn stale_at_2x_boundary_is_ok() {
        let now = Instant::now();
        let config = default_config(); // 1000ms interval
                                       // Exactly at 2× boundary (2000ms)
        let update_time = now - Duration::from_millis(2000);
        let mut state = base_state();
        state.subsystem_timestamps.openfan = Some(update_time);
        state.subsystem_timestamps.hwmon = Some(now);

        let health = compute_health(&state, &config, now);
        assert_eq!(health.subsystems[0].status, HealthStatus::Ok);
    }

    #[test]
    fn stale_just_past_2x_is_warn() {
        let now = Instant::now();
        let config = default_config();
        let update_time = now - Duration::from_millis(2001);
        let mut state = state_with_live_engine(now);
        state.subsystem_timestamps.openfan = Some(update_time);
        state.subsystem_timestamps.hwmon = Some(now);

        let health = compute_health(&state, &config, now);
        assert_eq!(health.subsystems[0].status, HealthStatus::Warn);
        assert_eq!(health.overall, HealthStatus::Warn);
    }

    #[test]
    fn stale_at_5x_boundary_is_warn() {
        let now = Instant::now();
        let config = default_config();
        let update_time = now - Duration::from_millis(5000);
        let mut state = base_state();
        state.subsystem_timestamps.openfan = Some(update_time);
        state.subsystem_timestamps.hwmon = Some(now);

        let health = compute_health(&state, &config, now);
        assert_eq!(health.subsystems[0].status, HealthStatus::Warn);
    }

    #[test]
    fn stale_past_5x_is_crit() {
        let now = Instant::now();
        let config = default_config();
        let update_time = now - Duration::from_millis(5001);
        let mut state = state_with_live_engine(now);
        state.subsystem_timestamps.openfan = Some(update_time);
        state.subsystem_timestamps.hwmon = Some(now);

        let health = compute_health(&state, &config, now);
        assert_eq!(health.subsystems[0].status, HealthStatus::Crit);
        assert_eq!(health.overall, HealthStatus::Crit);
    }

    // ── DEC-249: profile-engine liveness ────────────────────────────

    #[test]
    fn engine_is_reported_as_a_subsystem() {
        let now = Instant::now();
        let mut state = state_with_live_engine(now);
        state.subsystem_timestamps.openfan = Some(now);
        state.subsystem_timestamps.hwmon = Some(now);

        let health = compute_health(&state, &default_config(), now);

        // Appended, never inserted — clients index openfan at 0 and hwmon at 1.
        assert_eq!(health.subsystems.len(), 3);
        assert_eq!(health.subsystems[0].name, "openfan");
        assert_eq!(health.subsystems[1].name, "hwmon");
        let engine = &health.subsystems[2];
        assert_eq!(engine.name, "engine");
        assert_eq!(engine.status, HealthStatus::Ok);
        assert_eq!(engine.reason, "evaluating on schedule");
    }

    #[test]
    fn stalled_engine_escalates_overall_despite_fresh_poll_data() {
        // The failure this surface exists to catch: the poll loops keep running
        // and reporting fresh data after the engine task dies, so every other
        // signal stays green while nothing drives the fans or evaluates the
        // 105°C rule.
        let now = Instant::now();
        let mut state = base_state();
        state.subsystem_timestamps.openfan = Some(now);
        state.subsystem_timestamps.hwmon = Some(now);
        // A STOPPED engine: the last tick finished, and no tick has begun since.
        // (A tick that started and has not finished is a different state — slow,
        // not dead — see `a_slow_tick_is_reported_as_busy_not_stalled`.)
        state.subsystem_timestamps.engine_started = Some(now - Duration::from_millis(6100));
        state.subsystem_timestamps.engine_completed = Some(now - Duration::from_millis(6000));

        let health = compute_health(&state, &default_config(), now);

        assert_eq!(health.subsystems[0].status, HealthStatus::Ok);
        assert_eq!(health.subsystems[1].status, HealthStatus::Ok);
        assert_eq!(health.subsystems[2].status, HealthStatus::Crit);
        assert_eq!(
            health.subsystems[2].reason,
            "not ticking — fan control and thermal safety are stalled"
        );
        assert_eq!(health.overall, HealthStatus::Crit);
    }

    #[test]
    fn a_slow_tick_is_reported_as_busy_not_stalled() {
        // [SAFETY] DEC-259, and the reason the stamps were split. `force_all`
        // walks ten OpenFan channels at up to 1 s each, so a degraded-but-open
        // link makes a legitimate tick take 5-10 s. With one timestamp the
        // surface reported "not ticking — fan control and thermal safety are
        // stalled" **while the engine was driving the 105 °C emergency**: the
        // exact inverse of the truth, in the state where it matters most.
        let now = Instant::now();
        let mut state = base_state();
        state.subsystem_timestamps.openfan = Some(now);
        state.subsystem_timestamps.hwmon = Some(now);
        // A tick began 8 s ago and has not finished — it is still writing.
        state.subsystem_timestamps.engine_started = Some(now - Duration::from_millis(8000));
        state.subsystem_timestamps.engine_completed = Some(now - Duration::from_millis(9000));

        let engine = &compute_health(&state, &default_config(), now).subsystems[2];

        assert_eq!(
            engine.status,
            HealthStatus::Warn,
            "a slow tick is busy, not dead — reporting crit here is a false alarm \
             during a thermal emergency"
        );
        assert!(
            engine.reason.contains("still running"),
            "and it must say so: {}",
            engine.reason
        );
    }

    #[test]
    fn a_tick_that_never_finishes_still_escalates_eventually() {
        // Busy must not become a permanent excuse: past the wedged bound the tick
        // is stuck, not slow, and the distinction stops helping anyone.
        let now = Instant::now();
        let mut state = base_state();
        state.subsystem_timestamps.openfan = Some(now);
        state.subsystem_timestamps.hwmon = Some(now);
        let ms = u64::from(WEDGED_TICK_MULTIPLE) * 1000 + 1;
        state.subsystem_timestamps.engine_started = Some(now - Duration::from_millis(ms));
        state.subsystem_timestamps.engine_completed = Some(now - Duration::from_millis(ms + 1000));

        let engine = &compute_health(&state, &default_config(), now).subsystems[2];
        assert_eq!(engine.status, HealthStatus::Crit);
        assert!(engine.reason.contains("stuck"), "{}", engine.reason);
    }

    #[test]
    fn a_tick_in_flight_right_now_is_not_an_alarm() {
        // The common case: /status lands in the microseconds between the start
        // stamp and the completion stamp of an ordinary tick.
        let now = Instant::now();
        let mut state = base_state();
        state.subsystem_timestamps.openfan = Some(now);
        state.subsystem_timestamps.hwmon = Some(now);
        state.subsystem_timestamps.engine_started = Some(now - Duration::from_millis(5));
        state.subsystem_timestamps.engine_completed = Some(now - Duration::from_millis(1005));

        let engine = &compute_health(&state, &default_config(), now).subsystems[2];
        assert_eq!(engine.status, HealthStatus::Ok, "{}", engine.reason);
    }

    #[test]
    fn engine_uses_its_own_threshold_not_the_poll_interval() {
        // The engine ticks at a fixed 1 Hz; the poll interval is operator-
        // configurable up to 2000 ms. Raising the poll interval must not widen
        // what counts as a live engine.
        let now = Instant::now();
        let config = StalenessConfig {
            openfan_interval_ms: 2000,
            hwmon_interval_ms: 2000,
            ..StalenessConfig::default()
        };
        let mut state = base_state();
        // 3 s: fresh for a 2 s poll interval (< 2×), overdue for a 1 s engine.
        let stamp = now - Duration::from_millis(3000);
        state.subsystem_timestamps.openfan = Some(stamp);
        state.subsystem_timestamps.hwmon = Some(stamp);
        state.subsystem_timestamps.engine_started = Some(stamp);
        state.subsystem_timestamps.engine_completed = Some(stamp);

        let health = compute_health(&state, &config, now);

        assert_eq!(health.subsystems[0].status, HealthStatus::Ok);
        assert_eq!(health.subsystems[2].status, HealthStatus::Warn);
    }

    // ── Overall escalation ──────────────────────────────────────────

    #[test]
    fn overall_is_worst_of_subsystems() {
        let now = Instant::now();
        let config = default_config();
        let mut state = state_with_live_engine(now);
        // openfan: fresh (OK)
        state.subsystem_timestamps.openfan = Some(now);
        // hwmon: critically stale (CRIT)
        state.subsystem_timestamps.hwmon = Some(now - Duration::from_millis(6000));

        let health = compute_health(&state, &config, now);
        assert_eq!(health.subsystems[0].status, HealthStatus::Ok); // openfan
        assert_eq!(health.subsystems[1].status, HealthStatus::Crit); // hwmon
        assert_eq!(health.overall, HealthStatus::Crit);
    }

    // ── Age tracking ────────────────────────────────────────────────

    #[test]
    fn age_ms_reported_correctly() {
        let now = Instant::now();
        let update_time = now - Duration::from_millis(1500);
        let mut state = base_state();
        state.subsystem_timestamps.openfan = Some(update_time);
        state.subsystem_timestamps.hwmon = Some(now);

        let health = compute_health(&state, &default_config(), now);
        let openfan_age = health.subsystems[0].age_ms.unwrap();
        // Allow small tolerance for test execution time
        assert!((1499..=1510).contains(&openfan_age));
    }

    #[test]
    fn never_updated_has_no_age() {
        let state = base_state();
        let now = Instant::now();
        let health = compute_health(&state, &default_config(), now);
        assert!(health.subsystems[0].age_ms.is_none());
    }

    // ── HealthStatus ordering ───────────────────────────────────────

    #[test]
    fn health_status_ordering() {
        assert!(HealthStatus::Ok < HealthStatus::Warn);
        assert!(HealthStatus::Warn < HealthStatus::Crit);
    }

    #[test]
    fn health_status_display_wire_strings() {
        // These exact strings are the API contract: handlers serialise
        // `to_string()` into `overall_status` / `subsystems[].status`, which the
        // GUI consumes (control-ofc-gui parse_status + diagnostics severity).
        // Existing tests only assert the enum variant, so a Warn->"warn" rename
        // would break the GUI undetected (/test-tests audit P2).
        assert_eq!(HealthStatus::Ok.to_string(), "ok");
        assert_eq!(HealthStatus::Warn.to_string(), "warn");
        assert_eq!(HealthStatus::Crit.to_string(), "crit");
    }
}
