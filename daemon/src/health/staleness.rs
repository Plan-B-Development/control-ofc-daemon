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
}

impl Default for StalenessConfig {
    fn default() -> Self {
        Self {
            openfan_interval_ms: 1000,
            hwmon_interval_ms: 1000,
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

/// Compute the health summary for the daemon.
///
/// This function is pure: it takes the current state, config, and a reference
/// time, and returns a deterministic health summary.
pub fn compute_health(
    state: &DaemonState,
    config: &StalenessConfig,
    now: Instant,
) -> HealthSummary {
    let mut subsystems = Vec::new();

    // OpenFanController subsystem
    let openfan_status = evaluate_staleness(
        state.subsystem_timestamps.openfan,
        now,
        config.openfan_interval_ms,
    );
    subsystems.push(SubsystemHealth {
        name: "openfan".into(),
        status: openfan_status,
        age_ms: age_ms(state.subsystem_timestamps.openfan, now),
        reason: match openfan_status {
            HealthStatus::Ok => "readings fresh".into(),
            HealthStatus::Warn => "readings stale".into(),
            HealthStatus::Crit => match state.subsystem_timestamps.openfan {
                None => "never received data".into(),
                Some(_) => "readings critically stale".into(),
            },
        },
    });

    // hwmon sensor subsystem
    let hwmon_status = evaluate_staleness(
        state.subsystem_timestamps.hwmon,
        now,
        config.hwmon_interval_ms,
    );
    subsystems.push(SubsystemHealth {
        name: "hwmon".into(),
        status: hwmon_status,
        age_ms: age_ms(state.subsystem_timestamps.hwmon, now),
        reason: match hwmon_status {
            HealthStatus::Ok => "readings fresh".into(),
            HealthStatus::Warn => "readings stale".into(),
            HealthStatus::Crit => match state.subsystem_timestamps.hwmon {
                None => "never received data".into(),
                Some(_) => "readings critically stale".into(),
            },
        },
    });

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
        };

        let health = compute_health(&state, &default_config(), now);
        assert_eq!(health.overall, HealthStatus::Ok);
        assert_eq!(health.subsystems[0].status, HealthStatus::Ok);
        assert_eq!(health.subsystems[1].status, HealthStatus::Ok);
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
        let mut state = base_state();
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
        let mut state = base_state();
        state.subsystem_timestamps.openfan = Some(update_time);
        state.subsystem_timestamps.hwmon = Some(now);

        let health = compute_health(&state, &config, now);
        assert_eq!(health.subsystems[0].status, HealthStatus::Crit);
        assert_eq!(health.overall, HealthStatus::Crit);
    }

    // ── Overall escalation ──────────────────────────────────────────

    #[test]
    fn overall_is_worst_of_subsystems() {
        let now = Instant::now();
        let config = default_config();
        let mut state = base_state();
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
}
