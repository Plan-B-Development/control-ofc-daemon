//! The cooling advisory (DEC-443, `TS-m`).
//!
//! The CPU ladder is a backstop a self-throttling CPU practically never
//! reaches: firmware holds the CPU at its ceiling, so a dead pump or stopped
//! fans produce a CPU **pinned at Tjmax**, not an emergency (DEC-390 already
//! says so in the docs). This advisory names that shape while it is happening:
//!
//! > the hottest FRESH CPU reading has been at or above its ceiling for
//! > [`constants::ADVISORY_HOLD`], while the highest duty the engine commands
//! > to any non-GPU output is below [`constants::ADVISORY_LOW_DUTY_PCT`].
//!
//! **It forces nothing** — it is logged once when raised and once when it
//! clears, and published on `/status` as `advisories[]`. Where the daemon lacks
//! either fact — no fresh CPU reading, or nothing commanded because no profile
//! is active — it stays quiet rather than guess.
//!
//! The ceiling is the CPU's own `tempN_crit` where an authoritative CPU chip
//! publishes one (Intel `coretemp`), else [`constants::ADVISORY_FALLBACK_CEILING_C`]
//! — AMD's `k10temp` publishes none (the user's Q12).

use std::collections::HashMap;
use std::time::Instant;

use crate::constants;
use crate::health::state::{CachedSensorReading, CoolingAdvisoryRecord};
use crate::hwmon::types::SensorKind;

/// The advisory's stable wire code.
pub(crate) const CPU_AT_CEILING_LOW_COOLING: &str = "cpu_at_ceiling_low_cooling";

/// The CPU ceiling the advisory judges against: the highest finite `crit` an
/// authoritative CPU chip publishes, else the fallback. Unlike
/// `effective_trigger_c` it adds no margin and applies no cap — it is the
/// ceiling itself, the temperature a throttling CPU sits at.
pub(crate) fn cpu_ceiling_c(sensors: &HashMap<String, CachedSensorReading>) -> f64 {
    sensors
        .values()
        .filter(|s| s.kind == SensorKind::CpuTemp)
        .filter(|s| crate::hwmon::classify::is_authoritative_cpu_chip(&s.chip_name))
        .filter_map(|s| s.thresholds.as_ref().and_then(|t| t.crit_c))
        .filter(|c| c.is_finite())
        .fold(None, |acc: Option<f64>, c| {
            Some(acc.map_or(c, |a| a.max(c)))
        })
        .unwrap_or(constants::ADVISORY_FALLBACK_CEILING_C)
}

/// A transition worth one log line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AdvisoryEvent {
    Raised(CoolingAdvisoryRecord),
    Cleared,
}

/// The advisory's cross-tick state. Owned by the engine loop.
#[derive(Debug, Default)]
pub(crate) struct CoolingAdvisory {
    /// When the condition was first met without a break, if it is met now.
    met_since: Option<Instant>,
    /// The record while the advisory is raised.
    raised: Option<CoolingAdvisoryRecord>,
}

impl CoolingAdvisory {
    /// Advance one tick.
    ///
    /// `hottest_fresh_cpu_c` is `None` unless the CPU reading is fresh;
    /// `max_duty_pct` is the highest duty applied to any non-GPU output this
    /// tick, `None` when the engine commanded nothing.
    pub(crate) fn tick(
        &mut self,
        hottest_fresh_cpu_c: Option<f64>,
        ceiling_c: f64,
        max_duty_pct: Option<u8>,
        now: Instant,
    ) -> Option<AdvisoryEvent> {
        let met = match (hottest_fresh_cpu_c, max_duty_pct) {
            (Some(t), Some(d)) => t >= ceiling_c && d < constants::ADVISORY_LOW_DUTY_PCT,
            _ => false,
        };
        if !met {
            self.met_since = None;
            return self.raised.take().map(|_| AdvisoryEvent::Cleared);
        }
        let since = *self.met_since.get_or_insert(now);
        let (Some(cpu_temp_c), Some(max_duty_pct)) = (hottest_fresh_cpu_c, max_duty_pct) else {
            return None;
        };
        let record = CoolingAdvisoryRecord {
            code: CPU_AT_CEILING_LOW_COOLING,
            since,
            cpu_temp_c,
            ceiling_c,
            max_duty_pct,
        };
        if let Some(r) = self.raised.as_mut() {
            *r = record;
            return None;
        }
        if now.saturating_duration_since(since) >= constants::ADVISORY_HOLD {
            self.raised = Some(record.clone());
            return Some(AdvisoryEvent::Raised(record));
        }
        None
    }

    /// The `/status` records: the advisory while raised, else nothing.
    pub(crate) fn snapshot(&self) -> Vec<CoolingAdvisoryRecord> {
        self.raised.iter().cloned().collect()
    }
}

/// The highest duty this tick applies to a non-GPU output (DEC-443, the
/// user's Q13): the max over OpenFan and hwmon commands, raised to the forced
/// floor when a thermal force is in effect. `None` when nothing is commanded.
pub(crate) fn max_non_gpu_duty(
    commands: &[super::PwmCommand],
    forced_pct: Option<u8>,
) -> Option<u8> {
    let commanded = commands
        .iter()
        .filter(|c| !crate::profile::member_is_gpu_source(&c.source))
        .map(|c| c.pwm_percent)
        .max()?;
    Some(forced_pct.map_or(commanded, |f| commanded.max(f)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const HOLD: u64 = constants::ADVISORY_HOLD.as_secs();
    const LOW: u8 = constants::ADVISORY_LOW_DUTY_PCT;

    fn drive(
        a: &mut CoolingAdvisory,
        t0: Instant,
        from: u64,
        secs: u64,
        cpu: Option<f64>,
        duty: Option<u8>,
    ) -> Vec<AdvisoryEvent> {
        (from..from + secs)
            .filter_map(|s| a.tick(cpu, 95.0, duty, t0 + Duration::from_secs(s)))
            .collect()
    }

    #[test]
    fn it_is_raised_only_after_the_hold_and_clears_when_cooling_rises() {
        let t0 = Instant::now();
        let mut a = CoolingAdvisory::default();
        let early = drive(&mut a, t0, 0, HOLD, Some(95.0), Some(LOW - 1));
        assert!(early.is_empty());
        assert!(a.snapshot().is_empty());
        let raised = drive(&mut a, t0, HOLD, 1, Some(95.0), Some(LOW - 1));
        assert!(matches!(raised.as_slice(), [AdvisoryEvent::Raised(_)]));
        assert_eq!(a.snapshot()[0].code, CPU_AT_CEILING_LOW_COOLING);
        // Raised once, not every tick.
        assert!(drive(&mut a, t0, HOLD + 1, 10, Some(95.0), Some(LOW - 1)).is_empty());
        // The fans rise: cleared.
        let cleared = drive(&mut a, t0, HOLD + 11, 1, Some(95.0), Some(LOW));
        assert_eq!(cleared, vec![AdvisoryEvent::Cleared]);
        assert!(a.snapshot().is_empty());
    }

    /// Both halves of the condition are required, and an unknown fact keeps it
    /// quiet — a stale CPU reading, or nothing commanded.
    #[test]
    fn it_stays_quiet_without_both_facts() {
        let t0 = Instant::now();
        for (cpu, duty) in [
            (Some(94.9), Some(LOW - 1)), // below the ceiling
            (Some(95.0), Some(LOW)),     // cooling not low
            (None, Some(LOW - 1)),       // no fresh CPU reading
            (Some(95.0), None),          // nothing commanded
        ] {
            let mut a = CoolingAdvisory::default();
            let ev = drive(&mut a, t0, 0, HOLD * 3, cpu, duty);
            assert!(ev.is_empty(), "{cpu:?} {duty:?}");
        }
    }

    /// A break in the condition restarts the hold.
    #[test]
    fn a_break_restarts_the_hold() {
        let t0 = Instant::now();
        let mut a = CoolingAdvisory::default();
        drive(&mut a, t0, 0, HOLD - 1, Some(95.0), Some(10));
        drive(&mut a, t0, HOLD - 1, 1, Some(80.0), Some(10));
        let ev = drive(&mut a, t0, HOLD, HOLD - 1, Some(95.0), Some(10));
        assert!(ev.is_empty());
    }

    #[test]
    fn the_ceiling_is_the_published_crit_else_the_fallback() {
        use crate::health::state::DeviceLabel;
        let cpu = |chip: &str, crit: Option<f64>| CachedSensorReading {
            id: format!("{chip}:t"),
            kind: SensorKind::CpuTemp,
            label: "Package".into(),
            value_c: 50.0,
            source: DeviceLabel::Hwmon,
            updated_at: Instant::now(),
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: chip.into(),
            temp_type: None,
            thresholds: crit.map(|c| crate::hwmon::types::SensorThresholds {
                crit_c: Some(c),
                ..Default::default()
            }),
        };
        let map = |s: CachedSensorReading| HashMap::from([(s.id.clone(), s)]);
        assert_eq!(cpu_ceiling_c(&map(cpu("coretemp", Some(100.0)))), 100.0);
        assert_eq!(
            cpu_ceiling_c(&map(cpu("k10temp", None))),
            constants::ADVISORY_FALLBACK_CEILING_C
        );
        assert_eq!(
            cpu_ceiling_c(&map(cpu("coretemp", Some(f64::NAN)))),
            constants::ADVISORY_FALLBACK_CEILING_C,
            "a NaN crit is ignored"
        );
    }

    #[test]
    fn the_max_duty_excludes_gpu_and_counts_the_force() {
        let cmd = |source: &str, pct| super::super::PwmCommand {
            member_id: "m".into(),
            source: source.into(),
            pwm_percent: pct,
            gpu_fan_zero_rpm: false,
        };
        let cmds = vec![cmd("hwmon", 30), cmd("openfan", 40), cmd("amd_gpu", 90)];
        assert_eq!(max_non_gpu_duty(&cmds, None), Some(40));
        assert_eq!(max_non_gpu_duty(&cmds, Some(100)), Some(100));
        assert_eq!(max_non_gpu_duty(&[cmd("amd_gpu", 90)], None), None);
        assert_eq!(max_non_gpu_duty(&[], None), None);
    }
}
