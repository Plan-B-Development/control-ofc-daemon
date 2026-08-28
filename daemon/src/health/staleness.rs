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
    writes_stalled_since: Option<Instant>,
    now: Instant,
    interval_ms: u64,
) -> SubsystemHealth {
    let entry = |status, reason: &str, age: Option<u64>| SubsystemHealth {
        name: "engine".into(),
        status,
        age_ms: age,
        reason: reason.into(),
    };

    // DEC-289: checked FIRST, because it outranks the liveness question. Since
    // the backend joins were bounded, a wedged device no longer freezes the loop
    // — which is the fix, but it means a wedged writer now presents as a
    // perfectly live engine on both stamps below. It is not: nothing is reaching
    // the hardware. Judged on the same slow-vs-stuck thresholds as a long tick,
    // because it is the same underlying event (a write that has not returned)
    // seen from the other side of the bound.
    if let Some(stalled_since) = writes_stalled_since {
        let stalled_ms = now.saturating_duration_since(stalled_since).as_millis() as u64;
        let age = age_ms(completed.or(started), now);
        // Both strings are deliberately narrow about what they assert.
        //
        // "the engine is ticking" was removed from the Crit text: this branch
        // returns BEFORE the liveness ladder, and the GPU join is still unbounded
        // (AUD-a2), so a loop frozen in the GPU leg leaves a stale stall stamp
        // that still satisfies this branch — the text would then claim liveness
        // for a frozen engine.
        //
        // "fans are holding their last duty" was removed from the Warn text: the
        // 30x threshold above is derived (DEC-259) from a *legitimate* thermal
        // `force_all` over a degraded serial link taking ~10s, which crosses this
        // 2x line within three seconds. The old wording therefore said fans were
        // holding while `force_all` was actively driving them to 100% — the exact
        // surface-contradicts-reality failure DEC-259 exists to remove.
        if stalled_ms > interval_ms * u64::from(WEDGED_TICK_MULTIPLE) {
            return entry(
                HealthStatus::Crit,
                "writes wedged — a backend write has not returned and nothing is \
                 reaching those fans",
                age,
            );
        } else if stalled_ms > interval_ms * 2 {
            return entry(
                HealthStatus::Warn,
                "a backend write has not returned yet — it is still in flight",
                age,
            );
        }
    }

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

/// Health of the *controls* the engine is responsible for, as distinct from
/// whether the engine is ticking (277-j).
///
/// A live engine ticking on schedule over a control whose curve will not resolve
/// is a healthy `engine` entry and an unhealthy machine: nothing is commanding
/// those fans, they hold their last duty indefinitely (DEC-269), and until this
/// entry existed `/status.overall_status` stayed `"ok"` throughout. The GUI
/// ribbon, Dashboard and System State all read that rollup, so the only signals
/// were one journal WARN and a Controls-card chip — itself suppressed while a
/// Manual or External override is showing.
///
/// **`Warn`, never `Crit`.** The fans are not stopped and there is no thermal
/// hazard: the 105 °C rule is a separate path that bypasses controls entirely
/// (`force_all`), so it still reaches every OpenFan channel and writable hwmon
/// header regardless of what is listed here. `Crit` is reserved for a subsystem
/// that has actually failed, and escalating this one would drown that
/// distinction on a machine with one mis-authored profile.
///
/// **This is deliberately louder than DEC-193's `unavailable_sensors`**, which
/// does not move `overall_status` at all, and the asymmetry is the point rather
/// than an inconsistency: an unavailable sensor is a *cause*, is frequently
/// benign (a WiFi radio powered down), and very often drives nothing. A skipped
/// control is the *consequence*, is never benign, and by construction means a
/// real fan is uncommanded right now.
///
/// `age_ms` is the LONGEST `skipped_for_ms` in the list — the oldest unresolved
/// control, which is the one an operator most needs to know has been sitting
/// there. Reporting the newest would let a flapping control mask a permanent one.
fn controls_health(state: &DaemonState, now: Instant) -> SubsystemHealth {
    let entry = |status, reason: String, age: Option<u64>| SubsystemHealth {
        name: "controls".into(),
        status,
        age_ms: age,
        reason,
    };

    let oldest = state
        .skipped_controls
        .iter()
        .map(|c| now.saturating_duration_since(c.since).as_millis() as u64)
        .max();

    let Some(age) = oldest else {
        return entry(
            HealthStatus::Ok,
            "every control resolves to a curve".into(),
            None,
        );
    };

    let n = state.skipped_controls.len();
    let noun = if n == 1 { "control" } else { "controls" };
    entry(
        HealthStatus::Warn,
        format!("{n} {noun} not being commanded — their fans hold their last speed"),
        Some(age),
    )
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
            ts.engine_writes_stalled_since,
            now,
            config.engine_interval_ms,
        ),
        // 277-j: APPENDED at index 3, never inserted — see the wire-shape note
        // above. "The engine is ticking" and "the engine is commanding every
        // control" are different questions, and only the first had an entry.
        controls_health(state, now),
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
    use crate::health::state::{DaemonState, SkipReason, SkippedControl, SubsystemTimestamps};
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

    fn engine_of(h: &HealthSummary) -> &SubsystemHealth {
        h.subsystems
            .iter()
            .find(|s| s.name == "engine")
            .expect("engine is part of the wire shape")
    }

    /// DEC-289. Since the backend joins were bounded, a wedged device no longer
    /// freezes the loop — so both engine stamps keep advancing and the engine
    /// looks perfectly alive. It is not: nothing is reaching the fans.
    ///
    /// The control case is load-bearing. Without it this test would pass for the
    /// wrong reason (an old `completed` stamp is stale on its own), which is
    /// exactly how its first draft passed with the fix removed.
    #[test]
    fn a_wedged_writer_is_unhealthy_even_while_the_loop_ticks_normally() {
        let now = Instant::now();
        let cfg = default_config();

        // Control: identical state, no stall — demonstrably healthy.
        let control = compute_health(&state_with_live_engine(now), &cfg, now);
        assert_eq!(
            engine_of(&control).status,
            HealthStatus::Ok,
            "control must be healthy or the assertion below proves nothing"
        );

        let mut state = state_with_live_engine(now);
        // Derived from the production constant, not a literal: a future bump of
        // WEDGED_TICK_MULTIPLE past a hardcoded 40 would silently move this test
        // into the wrong band and fail with a confusing diff.
        state.subsystem_timestamps.engine_writes_stalled_since = Some(
            now - Duration::from_millis(
                cfg.engine_interval_ms * (u64::from(WEDGED_TICK_MULTIPLE) + 10),
            ),
        );
        let health = compute_health(&state, &cfg, now);
        assert_eq!(
            engine_of(&health).status,
            HealthStatus::Crit,
            "a wedged writer reported as healthy: {}",
            engine_of(&health).reason
        );
        assert!(engine_of(&health).reason.contains("wedged"));
    }

    /// A write that is merely slow must not read as wedged — the same
    /// slow-vs-stuck distinction DEC-259 drew for a long tick.
    #[test]
    fn a_briefly_stalled_write_warns_rather_than_alarming() {
        let now = Instant::now();
        let cfg = default_config();
        let mut state = state_with_live_engine(now);
        // Strictly inside the (2x, WEDGED_TICK_MULTIPLE) band, expressed from the
        // constant so the band cannot silently invert.
        let mid = 2 + (u64::from(WEDGED_TICK_MULTIPLE) - 2) / 2;
        assert!(
            mid > 2 && mid < u64::from(WEDGED_TICK_MULTIPLE),
            "band is empty"
        );
        state.subsystem_timestamps.engine_writes_stalled_since =
            Some(now - Duration::from_millis(cfg.engine_interval_ms * mid));
        assert_eq!(
            engine_of(&compute_health(&state, &cfg, now)).status,
            HealthStatus::Warn
        );
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
            engine_writes_stalled_since: None,
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
        // 277-j added `controls` at 3 on the same terms; this assertion is the
        // pin that makes an *insertion* fail rather than silently reshuffle a
        // wire position a client is indexing by number.
        assert_eq!(health.subsystems.len(), 4);
        assert_eq!(health.subsystems[0].name, "openfan");
        assert_eq!(health.subsystems[1].name, "hwmon");
        let engine = &health.subsystems[2];
        assert_eq!(engine.name, "engine");
        assert_eq!(engine.status, HealthStatus::Ok);
        assert_eq!(engine.reason, "evaluating on schedule");
        assert_eq!(health.subsystems[3].name, "controls");
    }

    // ── 277-j: a control nothing can drive is a health signal ───────

    /// The gap this entry closes: a live engine ticking on schedule over a
    /// control whose curve will not resolve was a fully green `/status`. Nothing
    /// commands those fans, they hold their last duty indefinitely (DEC-269), and
    /// the GUI ribbon, Dashboard and System State all read `overall_status`.
    #[test]
    fn a_skipped_control_degrades_overall_status() {
        let now = Instant::now();
        let mut state = state_with_live_engine(now);
        state.subsystem_timestamps.openfan = Some(now);
        state.subsystem_timestamps.hwmon = Some(now);

        // Assert the PRESENCE before the absence (DEC-272's vacuous-absence
        // trap): without this, a change that made every subsystem Warn would let
        // the assertion below pass while proving nothing about skipped controls.
        let healthy = compute_health(&state, &default_config(), now);
        assert_eq!(
            healthy.overall,
            HealthStatus::Ok,
            "precondition: with nothing skipped this state must be fully green"
        );

        state.skipped_controls = vec![SkippedControl {
            control_id: "ctl".into(),
            control_name: "Front intake".into(),
            reason: SkipReason::CurveNotFound,
            since: now - Duration::from_secs(30),
        }];

        let health = compute_health(&state, &default_config(), now);
        let controls = health
            .subsystems
            .iter()
            .find(|s| s.name == "controls")
            .expect("the controls subsystem must be present");

        assert_eq!(controls.status, HealthStatus::Warn);
        assert_eq!(
            health.overall,
            HealthStatus::Warn,
            "the rollup must carry it — an entry that never reaches `overall` \
             leaves every existing consumer just as blind as before"
        );
        assert!(
            controls.reason.contains("not being commanded"),
            "the reason must say what is wrong in the operator's terms: {}",
            controls.reason
        );
        assert_eq!(
            controls.age_ms,
            Some(30_000),
            "age is the LONGEST skipped_for_ms — a flapping control must not \
             mask one that has been sitting unresolved"
        );
    }

    /// Warn, not Crit. The fans are not stopped and there is no thermal hazard:
    /// the 105 °C rule bypasses controls entirely (`force_all`), so it still
    /// reaches every OpenFan channel and writable hwmon header. Escalating this
    /// to Crit would drown the distinction that a subsystem has actually failed.
    #[test]
    fn a_skipped_control_does_not_report_as_a_failed_subsystem() {
        let now = Instant::now();
        let mut state = state_with_live_engine(now);
        state.subsystem_timestamps.openfan = Some(now);
        state.subsystem_timestamps.hwmon = Some(now);
        state.skipped_controls = (0..5)
            .map(|i| SkippedControl {
                control_id: format!("ctl{i}"),
                control_name: format!("Control {i}"),
                reason: SkipReason::SensorUnavailable,
                since: now - Duration::from_secs(3600),
            })
            .collect();

        let health = compute_health(&state, &default_config(), now);
        assert_ne!(
            health.overall,
            HealthStatus::Crit,
            "five controls skipped for an hour is still not a FAILED daemon — \
             Crit is reserved for a subsystem that has stopped working"
        );
    }

    /// The oldest wins, so a control that keeps recovering and re-skipping cannot
    /// hide one that has been unresolved since boot.
    #[test]
    fn controls_age_reports_the_oldest_skip() {
        let now = Instant::now();
        let mut state = state_with_live_engine(now);
        state.skipped_controls = vec![
            SkippedControl {
                control_id: "recent".into(),
                control_name: "Recent".into(),
                reason: SkipReason::CurveNotFound,
                since: now - Duration::from_secs(2),
            },
            SkippedControl {
                control_id: "old".into(),
                control_name: "Old".into(),
                reason: SkipReason::CurveNotFound,
                since: now - Duration::from_secs(900),
            },
        ];

        let controls = controls_health(&state, now);
        assert_eq!(controls.age_ms, Some(900_000));
        assert!(
            controls.reason.starts_with("2 controls"),
            "the count must be plural and accurate: {}",
            controls.reason
        );
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
