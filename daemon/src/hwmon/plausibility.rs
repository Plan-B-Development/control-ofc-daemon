//! Cross-sensor plausibility filter for CPU temperature readings (`294-c`).
//!
//! [`reader::read_temp`] already rejects a reading outside `[-50, 250]°C`
//! (DEC-288), but that bound is per-sensor and absolute: it cannot see a value
//! that is individually plausible and only becomes absurd next to its
//! neighbours. The canonical case is a CPU channel pinned at or near 0°C.
//!
//! **Why a bogus-LOW reading is worse than a bogus-high one.** A high one is at
//! least loud — every fan goes to 100%. A low one is silent, and it defeats the
//! fallback built for a *missing* sensor: `hottest_cpu_reading` max-reduces, so
//! with a single `CpuTemp` reading 0°C there is nothing to out-rank it;
//! `safety_tick` sees a sensor that *is* present and therefore resets
//! `no_cpu_sensor_cycles`, so DEC-190's 40% floor never engages;
//! `ThermalSafetyRule::evaluate(0.0)` returns `None`; `thermal_state` stays
//! `"normal"`; and every curve runs at 0°C with nothing logged. The machine has
//! no thermal protection and no indication that anything is wrong.
//!
//! The failure is documented and real, not hypothetical — a Zen 3 `k10temp`
//! reporting all-zero (launchpad#1918065), and Super-I/O `peci`/`tsi` proxy
//! channels reported to read 0°C or several degrees low.
//!
//! **Two conditions, both required.** Either alone misfires:
//!
//! * *below the hottest motherboard reading by [`CPU_BELOW_MB_MARGIN_C`]* alone
//!   would reject an idle low-TDP CPU sitting under a hot VRM or chipset, which
//!   is ordinary rather than faulty; and
//! * *below [`CPU_ABSOLUTE_MIN_C`] absolute* alone would reject a genuinely cold
//!   machine — one booted in an unheated room, where the board is equally cold.
//!
//! Requiring both means a rejection needs a CPU that is simultaneously near
//! freezing **and** far colder than the board it is bolted to. That is not a
//! temperature; it is a broken channel.
//!
//! **Deliberately NOT a difference threshold.** US 9,606,160 B2 records the
//! general reason a bare absolute-difference rationality check is a poor
//! diagnostic — the threshold has to be wide enough to tolerate legitimate
//! spread, so it "has a detection range which is similar to and limited in
//! sensitivity to that of an out of range diagnostic", i.e. it degenerates into
//! the range check it was meant to improve on. This project has its own instance
//! of exactly that: `k10temp.c`'s `tctl_offset_table` gives Threadripper 19xx and
//! 29xx parts a legitimate +27°C Tctl offset, so a CPU-vs-CPU spread rule would
//! need `N > 27` and would miss most real faults. The absolute floor is what
//! stops this rule degenerating the same way — the margin narrows *which*
//! readings are considered, and the floor is what makes the verdict safe.
//!
//! **Fails open by construction.** With no `MbTemp` reading in the pass there is
//! no comparison to make and nothing is rejected. Rejecting a CPU sensor on no
//! evidence would be the same class of fault this module exists to prevent.

use super::types::{SensorKind, SensorReading};
use super::SensorReadFailure;

/// How far below the hottest `MbTemp` a `CpuTemp` reading must sit before it is
/// even considered implausible.
///
/// A CPU is normally hotter than the board around it, but not always: an idle
/// low-TDP part can sit below a loaded VRM or chipset sensor. 15°C is chosen to
/// clear that ordinary case comfortably rather than to sit close to it — the
/// documented faults are 30-40°C below the board, not 16.
pub const CPU_BELOW_MB_MARGIN_C: f64 = 15.0;

/// Absolute ceiling on an implausibly-low CPU reading.
///
/// A powered CPU does not run below this, whatever the ambient. Paired with
/// [`CPU_BELOW_MB_MARGIN_C`] so a cold-room boot — where the board is cold too —
/// cannot trip the rule.
pub const CPU_ABSOLUTE_MIN_C: f64 = 10.0;

/// The hottest `MbTemp` value in this pass, if any.
fn hottest_mb_c(readings: &[SensorReading]) -> Option<f64> {
    readings
        .iter()
        .filter(|r| r.kind == SensorKind::MbTemp)
        .map(|r| r.value_c)
        .fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a| a.max(v)))
        })
}

/// Whether one CPU reading is implausible given the hottest board reading.
///
/// Split out so the predicate can be tested directly at its boundaries, rather
/// than only through a whole-pass fixture.
pub fn is_implausibly_low_cpu(cpu_c: f64, hottest_mb_c: f64) -> bool {
    cpu_c < hottest_mb_c - CPU_BELOW_MB_MARGIN_C && cpu_c < CPU_ABSOLUTE_MIN_C
}

/// Ids of `CpuTemp` readings this pass that are implausible next to the board,
/// each with a human-readable reason for `unavailable_sensors[].reason`.
///
/// The caller moves these out of `readings` and into the DEC-193 failure list,
/// so they quarantine, log once, surface on `/status` + `/poll`, and recover on
/// their own when the channel reads sanely again. No new wire shape and no new
/// contract: an implausible reading becomes an unreadable sensor, which is what
/// it is.
pub fn implausibly_low_cpu_readings(readings: &[SensorReading]) -> Vec<SensorReadFailure> {
    let Some(mb_c) = hottest_mb_c(readings) else {
        // Fail open: no board reading, no evidence, no rejection.
        return Vec::new();
    };
    readings
        .iter()
        .filter(|r| r.kind == SensorKind::CpuTemp)
        .filter(|r| is_implausibly_low_cpu(r.value_c, mb_c))
        .map(|r| SensorReadFailure {
            id: r.id.clone(),
            label: r.label.clone(),
            reason: format!(
                "implausible reading: {:.1}°C is below {:.1}°C and more than {:.0}°C colder \
                 than the hottest motherboard sensor ({:.1}°C) — treating the channel as \
                 unreadable rather than trusting it for fan control",
                r.value_c, CPU_ABSOLUTE_MIN_C, CPU_BELOW_MB_MARGIN_C, mb_c
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::types::SensorSource;
    use std::time::SystemTime;

    fn reading(id: &str, kind: SensorKind, value_c: f64) -> SensorReading {
        SensorReading {
            id: id.to_string(),
            kind,
            label: id.to_string(),
            value_c,
            timestamp: SystemTime::now(),
            source: SensorSource::Hwmon,
            chip_name: "test".to_string(),
            temp_type: None,
            thresholds: None,
        }
    }

    /// Both conditions are required, and each is checked ALONE against a case
    /// that satisfies only it. Without this pair the rule could be reduced to
    /// either half and every other test here would stay green.
    #[test]
    fn neither_condition_fires_on_its_own() {
        // Far below the board, but not near freezing: an idle low-TDP CPU under
        // a hot VRM. Ordinary, must NOT be rejected.
        assert!(
            !is_implausibly_low_cpu(30.0, 50.0),
            "idle CPU under a hot board is not a fault"
        );
        // Near freezing, but the board is equally cold: a machine booted in an
        // unheated room. Ordinary, must NOT be rejected.
        assert!(
            !is_implausibly_low_cpu(8.0, 10.0),
            "a cold machine is not a fault"
        );
        // Both: near freezing AND far colder than the board it is bolted to.
        assert!(
            is_implausibly_low_cpu(0.0, 40.0),
            "0 C beside a 40 C board is a broken channel"
        );
    }

    /// The documented real-world faults, by their published values.
    #[test]
    fn the_documented_failures_are_caught() {
        // launchpad#1918065 — Zen 3 k10temp reporting all-zero.
        assert!(is_implausibly_low_cpu(0.0, 35.0));
        // A peci/tsi proxy pinned near zero beside a warm board.
        assert!(is_implausibly_low_cpu(2.5, 38.0));
    }

    /// Boundaries, stated as the rule states them: strictly-less-than on both.
    #[test]
    fn boundaries_are_exclusive_on_both_conditions() {
        // Exactly AT the margin — not below it — so no rejection.
        assert!(!is_implausibly_low_cpu(5.0, 5.0 + CPU_BELOW_MB_MARGIN_C));
        assert!(is_implausibly_low_cpu(
            5.0,
            5.0 + CPU_BELOW_MB_MARGIN_C + 0.1
        ));
        // Exactly AT the absolute floor — not below it — so no rejection.
        assert!(!is_implausibly_low_cpu(CPU_ABSOLUTE_MIN_C, 100.0));
        assert!(is_implausibly_low_cpu(CPU_ABSOLUTE_MIN_C - 0.1, 100.0));
    }

    /// With no board reading there is no evidence, so nothing may be rejected.
    /// Rejecting a CPU sensor on no evidence would be this module's own defect.
    #[test]
    fn fails_open_with_no_motherboard_sensor() {
        let readings = vec![reading("cpu", SensorKind::CpuTemp, 0.0)];
        assert!(implausibly_low_cpu_readings(&readings).is_empty());
    }

    /// The reduction is over the HOTTEST board sensor, so one cool board sensor
    /// cannot mask a hot one and let a broken CPU channel through.
    #[test]
    fn compares_against_the_hottest_board_sensor() {
        let readings = vec![
            reading("cpu", SensorKind::CpuTemp, 0.0),
            reading("mb_cool", SensorKind::MbTemp, 9.0),
            reading("mb_hot", SensorKind::MbTemp, 45.0),
        ];
        let out = implausibly_low_cpu_readings(&readings);
        assert_eq!(
            out.len(),
            1,
            "the hot board sensor must decide, not the cool one"
        );
        assert_eq!(out[0].id, "cpu");
    }

    /// Only CpuTemp is judged. A cold GPU or board sensor is not this rule's
    /// business and must never be quarantined by it.
    #[test]
    fn only_cpu_sensors_are_judged() {
        let readings = vec![
            reading("gpu", SensorKind::GpuTemp, 0.0),
            reading("mb", SensorKind::MbTemp, 45.0),
            reading("disk", SensorKind::DiskTemp, 0.0),
        ];
        assert!(implausibly_low_cpu_readings(&readings).is_empty());
    }

    /// The failure carries the id and label the quarantine needs to surface it,
    /// and a reason naming both numbers a reader would want.
    #[test]
    fn the_failure_carries_what_the_quarantine_surfaces() {
        let readings = vec![
            reading("hwmon:k10temp:Tctl", SensorKind::CpuTemp, 0.0),
            reading("mb", SensorKind::MbTemp, 45.0),
        ];
        let out = implausibly_low_cpu_readings(&readings);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "hwmon:k10temp:Tctl");
        assert_eq!(out[0].label, "hwmon:k10temp:Tctl");
        assert!(out[0].reason.contains("implausible reading"));
        assert!(
            out[0].reason.contains("45.0"),
            "names the board reading it was judged against"
        );
    }
}
