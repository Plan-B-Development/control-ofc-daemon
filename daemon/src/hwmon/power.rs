//! CPU package power, read only inside a validation session (AIO Phase 8 Batch 3a, DEC-335).
//!
//! Read-only. Never writes hardware, never runs on the 1 Hz poll, and nothing in
//! the control path consults it. Like [`super::voltages`] it takes injected sysfs
//! roots so tests point at a `tempfile::tempdir()` rather than the real `/sys`.
//!
//! # Why this is not a `SensorReading`
//!
//! [`crate::hwmon::types::SensorKind`] is temperature-only and `SensorReading`
//! carries `value_c`, both of which feed curve binding and the thermal path. A
//! watt in either would be a lie in a field those consumers trust — the same
//! reasoning `WIRE-ag` recorded for voltage rails, which is why they got their
//! own type too. Package power therefore appears **only** on a validation
//! sample and its summary.
//!
//! # The two sources, in the order they are tried (Batch 3a Q1-C)
//!
//! 1. **hwmon `powerN_input` / `powerN_average` on a CPU chip.** Microwatts,
//!    read directly — no differentiation, no state. Restricted to the chips in
//!    [`CPU_POWER_CHIPS`] deliberately: a bare "any non-GPU `powerN_input`"
//!    filter would happily report a PSU or VRM rail as *CPU package power*, and
//!    a number that is confidently mislabelled is worse than no number.
//! 2. **powercap RAPL**, `/sys/class/powercap/intel-rapl:N/energy_uj` for a zone
//!    whose `name` starts `package-`. Subzones (`core`, `uncore`, `dram`) are
//!    **excluded** — `intel-rapl:0:0` is a *part* of the package, and reporting
//!    it as the package would under-count silently.
//!
//! Measured on the reference host (X870E AORUS MASTER, Zen 5): `k10temp`
//! publishes **no** power attribute at all, so branch 1 finds nothing and RAPL
//! is the only path. The fallback is the common case, not the exotic one.
//!
//! # Three properties of RAPL that shape this module
//!
//! - **It is a cumulative energy counter, not a power reading.** One sample is
//!   not a wattage. The first sample of a session therefore yields `None`, and
//!   `None` here means "not yet known", never `0.0`.
//! - **It wraps, and it wraps often.** Measured on the reference host,
//!   `max_energy_range_uj` is 65 532 610 987 µJ ≈ 65.5 kJ, which at 200 W wraps
//!   every **~5.5 minutes** — roughly 22 times in a two-hour observation. Wrap
//!   handling is a routine path here, not an edge case, and a test that runs for
//!   a minute will never reach it. See [`watts_from_energy_delta`].
//! - **It is root-only** (`energy_uj` is mode `0400`, the CVE-2020-8694 /
//!   PLATYPUS mitigation). The daemon runs as root so it can read it; an
//!   `EACCES` is reported as unavailable rather than as zero.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::constants;

use super::util::read_sysfs_string;

/// Wire token: the reading came from an hwmon `powerN_*` attribute.
pub const POWER_SOURCE_HWMON: &str = "hwmon";
/// Wire token: the reading was derived from a powercap RAPL energy counter.
pub const POWER_SOURCE_RAPL: &str = "powercap_rapl";

/// The default powercap root. Injected in tests.
pub const POWERCAP_ROOT: &str = "/sys/class/powercap";

/// hwmon chips whose `powerN_*` attributes are genuinely CPU package power.
///
/// Deliberately a small allow-list rather than "not a GPU". See the module docs:
/// the failure this prevents is confidently reporting some other rail as the
/// CPU package.
pub const CPU_POWER_CHIPS: &[&str] = &["k10temp", "coretemp", "zenpower", "zenpower3"];

/// Where a package-power reading comes from, resolved once at session start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerSource {
    /// [`POWER_SOURCE_HWMON`] or [`POWER_SOURCE_RAPL`].
    pub kind: &'static str,
    /// Human-facing origin, e.g. `k10temp power1` or `powercap package-0`.
    pub label: String,
    /// The file read each tick.
    path: PathBuf,
    /// RAPL only: the counter's wrap point, from `max_energy_range_uj`.
    max_energy_range_uj: Option<u64>,
}

/// Convert one energy-counter delta into watts, handling wrap.
///
/// Pure and clock-free so every branch is testable — the wrap in particular,
/// which a live test would have to run for minutes to provoke (module docs).
///
/// Returns `None` rather than a fabricated number when the result cannot be
/// trusted:
///
/// * `elapsed_us == 0` — no interval, so no rate.
/// * The derived power exceeds [`constants::POWER_MAX_PLAUSIBLE_W`]. **This is
///   the counter-reset guard, and it is the reason the ceiling exists.** A
///   driver reload or a suspend/resume resets `energy_uj` to a small value,
///   which is indistinguishable from a wrap by inspection — both simply read
///   lower than last time. Treating a reset as a wrap adds the whole counter
///   range to the delta and yields a spike of thousands of watts. Discarding
///   one implausible sample is right; publishing the spike is not.
/// * A wrap is claimed but no `max_energy_range_uj` is known, so its size
///   cannot be established.
pub fn watts_from_energy_delta(
    prev_uj: u64,
    now_uj: u64,
    max_energy_range_uj: Option<u64>,
    elapsed_us: u64,
) -> Option<f64> {
    if elapsed_us == 0 {
        return None;
    }
    let delta_uj = if now_uj >= prev_uj {
        now_uj - prev_uj
    } else {
        // Wrapped. `max_range - prev + now` is off by one microjoule against a
        // counter that wraps modulo `max_range + 1`; at these magnitudes that is
        // ~1e-11 of a 65 kJ range and is not worth the overflow risk of adding
        // one to a value that may already be `u64::MAX`.
        let max = max_energy_range_uj?;
        max.checked_sub(prev_uj)?.checked_add(now_uj)?
    };
    // µJ / µs == W exactly, so no unit constant is needed and none can drift.
    let watts = delta_uj as f64 / elapsed_us as f64;
    if !watts.is_finite() || watts > constants::POWER_MAX_PLAUSIBLE_W {
        return None;
    }
    Some(watts)
}

/// Samples CPU package power for the life of one validation session.
///
/// Holds the previous energy reading, which is why it is a struct rather than a
/// free function: a RAPL wattage is a difference between two ticks.
#[derive(Debug)]
pub struct PowerSampler {
    source: Option<PowerSource>,
    /// Previous `(energy_uj, when)` — RAPL only.
    last: Option<(u64, Instant)>,
}

impl PowerSampler {
    /// Resolve a source once, at session start.
    ///
    /// Finding nothing is a normal outcome and is **not** an error: the batch
    /// spec is explicit that fields the OS cannot expose must not be required.
    pub fn discover(hwmon_root: &Path, powercap_root: &Path) -> Self {
        let source = discover_hwmon_cpu_power(hwmon_root).or_else(|| discover_rapl(powercap_root));
        if let Some(src) = &source {
            log::info!("Validation power telemetry: {} ({})", src.label, src.kind);
        } else {
            log::info!(
                "Validation power telemetry unavailable: no CPU-chip hwmon power attribute and no powercap package zone"
            );
        }
        Self { source, last: None }
    }

    /// A sampler that will always report unavailable. For sessions that do not
    /// ask for power, and for tests.
    pub fn unavailable() -> Self {
        Self {
            source: None,
            last: None,
        }
    }

    /// The resolved source, for provenance in the session summary.
    pub fn source(&self) -> Option<&PowerSource> {
        self.source.as_ref()
    }

    /// Read one sample. `None` means "not known at this tick", never zero.
    ///
    /// For RAPL the **first** call of a session always returns `None`: one
    /// reading of a cumulative counter is not a power. Callers must render that
    /// as unknown rather than as an idle machine.
    pub fn sample(&mut self, now: Instant) -> Option<f64> {
        let source = self.source.as_ref()?;
        let raw = read_sysfs_string(&source.path)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()?;

        if source.kind == POWER_SOURCE_HWMON {
            // Microwatts, already a rate. No state, so no wrap and no priming.
            let watts = raw as f64 / 1_000_000.0;
            return (watts.is_finite() && watts <= constants::POWER_MAX_PLAUSIBLE_W)
                .then_some(watts);
        }

        let max = source.max_energy_range_uj;
        let previous = self.last.replace((raw, now));
        let (prev_uj, prev_at) = previous?;
        let elapsed_us = u64::try_from(now.duration_since(prev_at).as_micros()).ok()?;
        watts_from_energy_delta(prev_uj, raw, max, elapsed_us)
    }
}

/// Branch 1: an hwmon `powerN_input` / `powerN_average` on a CPU chip.
fn discover_hwmon_cpu_power(hwmon_root: &Path) -> Option<PowerSource> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(hwmon_root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hwmon"))
        })
        .collect();
    dirs.sort();

    for dir in dirs {
        let Ok(chip) = read_sysfs_string(&dir.join("name")) else {
            continue;
        };
        let chip = chip.trim().to_string();
        if !CPU_POWER_CHIPS.contains(&chip.as_str()) {
            continue;
        }
        // `_input` before `_average`: an instantaneous reading is what a thermal
        // observation wants, and an averaged one lags the workload it is meant
        // to explain.
        for suffix in ["_input", "_average"] {
            let mut channels: Vec<u8> = std::fs::read_dir(&dir)
                .ok()?
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_prefix("power")
                        .and_then(|s| s.strip_suffix(suffix))
                        .and_then(|n| n.parse::<u8>().ok())
                })
                .collect();
            channels.sort_unstable();
            if let Some(channel) = channels.first() {
                let path = dir.join(format!("power{channel}{suffix}"));
                let label = read_sysfs_string(&dir.join(format!("power{channel}_label")))
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("{chip} power{channel}"));
                return Some(PowerSource {
                    kind: POWER_SOURCE_HWMON,
                    label,
                    path,
                    max_energy_range_uj: None,
                });
            }
        }
    }
    None
}

/// Branch 2: a powercap RAPL **package** zone.
///
/// Only top-level `intel-rapl:N` zones are considered. A subzone is named
/// `intel-rapl:N:M` and measures a *part* of the package (`core`, `uncore`,
/// `dram`); the `package-` name check rejects those, and the depth check means
/// a future subzone that happened to be named `package-something` still cannot
/// be selected.
fn discover_rapl(powercap_root: &Path) -> Option<PowerSource> {
    let mut zones: Vec<PathBuf> = std::fs::read_dir(powercap_root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                // `intel-rapl:0` yes; `intel-rapl:0:0` (a subzone) no; the bare
                // `intel-rapl` control directory has no colon and no counter.
                .is_some_and(|n| n.matches(':').count() == 1)
        })
        .collect();
    zones.sort();

    for zone in zones {
        let Ok(name) = read_sysfs_string(&zone.join("name")) else {
            continue;
        };
        let name = name.trim().to_string();
        if !name.starts_with("package-") {
            continue;
        }
        let energy = zone.join("energy_uj");
        // Prove it is readable NOW rather than discovering at the first tick
        // that the daemon lacks the privilege — `energy_uj` is mode 0400.
        if read_sysfs_string(&energy).is_err() {
            log::warn!(
                "powercap zone {} is present but energy_uj is unreadable; package power will be unavailable",
                name
            );
            continue;
        }
        let max_energy_range_uj = read_sysfs_string(&zone.join("max_energy_range_uj"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        if max_energy_range_uj.is_none() {
            // Not fatal: readings still work until the first wrap, and a wrap
            // without a known range is reported as unknown rather than guessed.
            log::warn!(
                "powercap zone {name} publishes no max_energy_range_uj; a counter wrap will read as unknown"
            );
        }
        return Some(PowerSource {
            kind: POWER_SOURCE_RAPL,
            label: format!("powercap {name}"),
            path: energy,
            max_energy_range_uj,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    // ── watts_from_energy_delta: the pure core ───────────────────────

    #[test]
    fn a_plain_delta_is_microjoules_over_microseconds() {
        // 100 J over 1 s == 100 W.
        let w = watts_from_energy_delta(0, 100_000_000, Some(u64::MAX), 1_000_000);
        assert_eq!(w, Some(100.0));
    }

    #[test]
    fn a_wrap_is_completed_across_the_counter_range() {
        // Reference-host range. Previous reading 100 J below the top, now 50 J
        // past zero: 150 J in one second.
        let max = 65_532_610_987_u64;
        let w = watts_from_energy_delta(max - 100_000_000, 50_000_000, Some(max), 1_000_000);
        assert_eq!(w, Some(150.0));
    }

    #[test]
    fn a_wrap_with_no_known_range_is_unknown_rather_than_guessed() {
        assert_eq!(watts_from_energy_delta(500, 100, None, 1_000_000), None);
    }

    #[test]
    fn a_small_wrap_near_the_top_is_completed_normally() {
        // The counter sat 1 mJ below the top and is now 5 mJ past zero: a 6 mJ
        // delta, i.e. an idle machine. This is a WRAP, not a reset, and the
        // plausibility ceiling must not touch it. The first draft of the reset
        // test below used exactly these numbers and asserted `None`, which is
        // how this case was found.
        let max = 65_532_610_987_u64;
        let w = watts_from_energy_delta(max - 1_000, 5_000, Some(max), 1_000_000);
        assert_eq!(w, Some(0.006));
    }

    #[test]
    fn a_counter_reset_is_discarded_rather_than_published_as_a_spike() {
        // THE reason the plausibility ceiling exists, modelled the way a reset
        // actually happens: the counter had climbed to mid-range, a driver
        // reload put it back near zero, and "completing the wrap" from there
        // invents ~35 kJ of energy in one tick.
        let max = 65_532_610_987_u64;
        let w = watts_from_energy_delta(30_000_000_000, 1_000, Some(max), 1_000_000);
        assert_eq!(w, None, "an implausible derived power must be dropped");

        // A reset from a LOW counter value is caught too, and for the same
        // reason: completing the wrap from near zero spans almost the whole
        // range. Asserted so the guard's coverage is a measured property.
        assert_eq!(
            watts_from_energy_delta(1_000, 500, Some(max), 1_000_000),
            None
        );

        // Honest limit, asserted so it is a known property rather than a
        // surprise: a reset that happens while the counter sits near its TOP is
        // arithmetically identical to a small wrap and cannot be detected. It
        // does not matter — the energy it invents is the few millijoules
        // between `prev` and the wrap point.
        let sneaky = watts_from_energy_delta(max - 1_000, 500, Some(max), 1_000_000)
            .expect("an undetectable reset reads as a small wrap, by construction");
        assert!(
            sneaky < 0.01,
            "the undetectable case must be numerically negligible, got {sneaky} W"
        );
    }

    #[test]
    fn a_zero_interval_has_no_rate() {
        assert_eq!(watts_from_energy_delta(0, 1_000, Some(u64::MAX), 0), None);
    }

    #[test]
    fn an_idle_counter_reads_as_zero_watts_not_as_unknown() {
        // The opposite branch: zero is a legitimate reading and must not be
        // confused with "not known", which is what `None` means everywhere here.
        assert_eq!(
            watts_from_energy_delta(1_000, 1_000, Some(u64::MAX), 1_000_000),
            Some(0.0)
        );
    }

    // ── discovery ────────────────────────────────────────────────────

    #[test]
    fn rapl_selects_the_package_zone_and_never_a_subzone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A subzone sorts adjacent to its parent and is the trap this guards.
        write(&root.join("intel-rapl:0:0/name"), "core\n");
        write(&root.join("intel-rapl:0:0/energy_uj"), "123\n");
        write(&root.join("intel-rapl:0/name"), "package-0\n");
        write(&root.join("intel-rapl:0/energy_uj"), "456\n");
        write(
            &root.join("intel-rapl:0/max_energy_range_uj"),
            "65532610987\n",
        );

        let src = discover_rapl(root).expect("package zone must be found");
        assert_eq!(src.kind, POWER_SOURCE_RAPL);
        assert_eq!(src.label, "powercap package-0");
        assert_eq!(src.max_energy_range_uj, Some(65_532_610_987));
        assert!(
            src.path.to_string_lossy().contains("intel-rapl:0/"),
            "must read the package zone, got {}",
            src.path.display()
        );
    }

    #[test]
    fn an_unreadable_energy_counter_is_not_selected() {
        // Models the 0400 case for a non-root reader: the zone exists, the
        // counter does not read. Selecting it would defer the failure to the
        // first tick and report "unknown" every second for two hours.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("intel-rapl:0/name"), "package-0\n");
        // No energy_uj file at all.
        assert!(discover_rapl(root).is_none());
    }

    #[test]
    fn hwmon_power_is_taken_only_from_a_cpu_chip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // amdgpu publishes power1_input on the reference host and must NOT be
        // reported as CPU package power.
        write(&root.join("hwmon0/name"), "amdgpu\n");
        write(&root.join("hwmon0/power1_input"), "33000\n");
        assert!(
            discover_hwmon_cpu_power(root).is_none(),
            "a GPU power rail is not CPU package power"
        );

        write(&root.join("hwmon1/name"), "coretemp\n");
        write(&root.join("hwmon1/power1_input"), "42000000\n");
        let src = discover_hwmon_cpu_power(root).expect("a CPU chip's power must be found");
        assert_eq!(src.kind, POWER_SOURCE_HWMON);
    }

    #[test]
    fn the_reference_host_shape_falls_through_to_rapl() {
        // Measured: k10temp publishes NO power attribute, so branch 1 finds
        // nothing and RAPL is the only path. This is the common case.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path().join("hwmon");
        let powercap = tmp.path().join("powercap");
        write(&hwmon.join("hwmon6/name"), "k10temp\n");
        write(&hwmon.join("hwmon6/temp1_input"), "45000\n");
        write(&hwmon.join("hwmon3/name"), "amdgpu\n");
        write(&hwmon.join("hwmon3/power1_input"), "33000\n");
        write(&powercap.join("intel-rapl:0/name"), "package-0\n");
        write(&powercap.join("intel-rapl:0/energy_uj"), "1000\n");

        let sampler = PowerSampler::discover(&hwmon, &powercap);
        let src = sampler.source().expect("RAPL must be the resolved source");
        assert_eq!(src.kind, POWER_SOURCE_RAPL);
    }

    #[test]
    fn no_source_at_all_is_unavailable_not_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sampler =
            PowerSampler::discover(&tmp.path().join("nope"), &tmp.path().join("also"));
        assert!(sampler.source().is_none());
        assert_eq!(sampler.sample(Instant::now()), None);
    }

    #[test]
    fn the_first_rapl_sample_primes_the_counter_and_reports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let powercap = tmp.path().join("powercap");
        write(&powercap.join("intel-rapl:0/name"), "package-0\n");
        write(&powercap.join("intel-rapl:0/energy_uj"), "1000000\n");
        write(
            &powercap.join("intel-rapl:0/max_energy_range_uj"),
            "65532610987\n",
        );

        let mut sampler = PowerSampler::discover(&tmp.path().join("no-hwmon"), &powercap);
        let t0 = Instant::now();
        assert_eq!(
            sampler.sample(t0),
            None,
            "one reading of a cumulative counter is not a power"
        );

        // Second tick, one second later and 150 J on: 150 W.
        write(&powercap.join("intel-rapl:0/energy_uj"), "151000000\n");
        let watts = sampler
            .sample(t0 + std::time::Duration::from_secs(1))
            .expect("the second sample has an interval to divide by");
        assert!((watts - 150.0).abs() < 1.0, "expected ~150 W, got {watts}");
    }

    #[test]
    fn an_hwmon_sample_needs_no_priming() {
        // The opposite branch of the test above: `powerN_input` is already a
        // rate, so the FIRST sample is real. Without this, a sampler that
        // always primed would look correct.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path().join("hwmon");
        write(&hwmon.join("hwmon0/name"), "k10temp\n");
        write(&hwmon.join("hwmon0/power1_input"), "65000000\n");

        let mut sampler = PowerSampler::discover(&hwmon, &tmp.path().join("no-powercap"));
        assert_eq!(sampler.sample(Instant::now()), Some(65.0));
    }
}
