//! Pure statistics over retained tach samples (AIO Phase 8 Batch 2, DEC-334).
//!
//! Everything here is a total function over slices: no I/O, no locks, no clock,
//! no hardware. That is deliberate on two counts.
//!
//! 1. It is the only way the `§4` / `§5` derivations can be exhaustively tested
//!    against dropouts, single samples, all-equal samples and empty input —
//!    the cases a live sweep produces rarely and a reviewer cannot provoke.
//! 2. **Batch 3 `§3` (steady-state detection) is this module's temperature-domain
//!    twin** — "a conservative rolling-window method based on temperature
//!    trend/slope and variance". [`settling_ms`] is that method in the RPM
//!    domain. Keep it reusable; do not inline any of this into the sweep.
//!
//! # What is deliberately *not* here
//!
//! No verdict about hardware health. `AIO-Phase8 Batch 2 §3` and `§4` are explicit
//! that a plateau is not a pump failure and that tach variability alone does not
//! evidence cavitation or an electrical fault. This module reports magnitudes and
//! a cautious classification; the interpretation lives in `validation::summary`,
//! and even there it stays `observed` / `not_observed`, never `fail`.

use crate::constants;

// ── Stability classification tokens ──────────────────────────────────
// Stable wire tokens. The client owns the wording and must render an
// unrecognised token rather than dropping it (273-i).

pub const STABILITY_STABLE: &str = "stable";
pub const STABILITY_VARIABLE: &str = "variable";
pub const STABILITY_UNSTABLE: &str = "unstable";
pub const STABILITY_INSUFFICIENT: &str = "insufficient_data";
/// Distinct from `insufficient_data`: nothing was readable at all, rather than
/// too little being readable. `§4` asks for the second; the Overview's status
/// vocabulary asks that lack of evidence never become a PASS, and this keeps the
/// two lacks distinguishable.
pub const STABILITY_UNAVAILABLE: &str = "unavailable";

// ── Hysteresis tokens ────────────────────────────────────────────────

pub const HYSTERESIS_NONE: &str = "none";
pub const HYSTERESIS_PRESENT: &str = "present";
pub const HYSTERESIS_INSUFFICIENT: &str = "insufficient_data";
pub const HYSTERESIS_NOT_TESTED: &str = "not_tested";

/// One retained tach reading from a settle or dwell window.
///
/// `rpm: None` is an **unreadable** tach, which is not the same thing as a fan
/// reported as stopped — see [`RpmStats::dropouts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpmSample {
    /// Milliseconds since the write that opened this window.
    pub at_ms: u64,
    pub rpm: Option<u16>,
}

/// `§4`'s statistics over one window. Every field is `Option` where the input
/// may not support it, so a caller cannot mistake "no data" for zero.
#[derive(Debug, Clone, PartialEq)]
pub struct RpmStats {
    /// Every retained reading, dropouts included.
    pub samples: u32,
    /// Readings that contributed to the statistics.
    pub usable: u32,
    /// Readings the tach did not deliver: an unreadable sample, **or** a `0`
    /// recorded while some other sample in the same window was non-zero.
    ///
    /// The second half is what makes this mean "the tach dropped out" rather
    /// than "the fan is stopped". A genuinely stopped fan reports `0` for the
    /// whole window and is not a dropout; a spinning one that reports `0` once
    /// is. Neither reading is removed from the raw record (`§9`).
    pub dropouts: u32,
    /// Readings whose Iglewicz-Hoaglin modified z-score exceeds
    /// [`constants::STABILITY_OUTLIER_MODIFIED_Z`]. **Counted, never removed** —
    /// the mean and deviation below still include them.
    ///
    /// Median/MAD based rather than mean/σ based, because a σ rule cannot fire
    /// at these sample counts; see the constant's own note.
    pub outliers: u32,
    pub mean: Option<f64>,
    pub median: Option<u16>,
    pub min: Option<u16>,
    pub max: Option<u16>,
    pub stddev: Option<f64>,
    /// Coefficient of variation, as a percentage of the mean. `None` when the
    /// mean is zero — a stopped fan has no meaningful relative spread, and
    /// dividing by it would manufacture an infinity.
    pub cv_pct: Option<f64>,
    pub verdict: &'static str,
}

impl RpmStats {
    /// The empty result, for a window that retained nothing at all.
    fn unavailable(samples: u32, dropouts: u32) -> Self {
        Self {
            samples,
            usable: 0,
            dropouts,
            outliers: 0,
            mean: None,
            median: None,
            min: None,
            max: None,
            stddev: None,
            cv_pct: None,
            verdict: STABILITY_UNAVAILABLE,
        }
    }
}

/// `§4`: mean / median / min / max / σ / CV / dropouts / outliers over one window.
pub fn rpm_stats(samples: &[RpmSample]) -> RpmStats {
    let total = samples.len() as u32;
    let values: Vec<u16> = samples.iter().filter_map(|s| s.rpm).collect();
    let unreadable = total - values.len() as u32;

    // A `0` is a dropout only when the fan demonstrably turned during the
    // window. Deriving it from the window's own readings rather than from a
    // sibling fact about the header keeps the flag honest about the value it
    // describes (DEC-325).
    let any_spinning = values.iter().any(|&v| v > 0);
    let zero_dropouts = if any_spinning {
        values.iter().filter(|&&v| v == 0).count() as u32
    } else {
        0
    };
    let dropouts = unreadable + zero_dropouts;

    if values.is_empty() {
        return RpmStats::unavailable(total, dropouts);
    }

    let usable = values.len() as u32;
    let mut sorted = values.clone();
    sorted.sort_unstable();
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let median = median_of_sorted(&sorted);

    let n = values.len() as f64;
    let mean = values.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    // Population deviation: this is the whole window, not a sample drawn from a
    // larger one, and n == 1 must not divide by zero.
    let variance = values
        .iter()
        .map(|&v| {
            let d = f64::from(v) - mean;
            d * d
        })
        .sum::<f64>()
        / n;
    let stddev = variance.sqrt();
    let cv_pct = if mean > 0.0 {
        Some(stddev / mean * 100.0)
    } else {
        None
    };

    let outliers = count_outliers(&values, median);

    let verdict = classify_stability(usable, cv_pct);

    RpmStats {
        samples: total,
        usable,
        dropouts,
        outliers,
        mean: Some(mean),
        median: Some(median),
        min: Some(min),
        max: Some(max),
        stddev: Some(stddev),
        cv_pct,
        verdict,
    }
}

/// `§4`'s cautious classification. Kept separate from [`rpm_stats`] so the
/// threshold rule can be asserted on its own.
pub fn classify_stability(usable: u32, cv_pct: Option<f64>) -> &'static str {
    if usable == 0 {
        return STABILITY_UNAVAILABLE;
    }
    if (usable as usize) < constants::STABILITY_MIN_SAMPLES {
        return STABILITY_INSUFFICIENT;
    }
    match cv_pct {
        // A zero mean with usable samples is a fan reported as stopped for the
        // whole window. That is a perfectly steady reading, and calling it
        // `unstable` because the ratio is undefined would be the wrong way round.
        None => STABILITY_STABLE,
        Some(cv) if cv <= constants::STABILITY_STABLE_MAX_CV_PCT => STABILITY_STABLE,
        Some(cv) if cv <= constants::STABILITY_VARIABLE_MAX_CV_PCT => STABILITY_VARIABLE,
        Some(_) => STABILITY_UNSTABLE,
    }
}

/// Iglewicz-Hoaglin scale factors. `0.6745` is the 0.75 quantile of the standard
/// normal, which makes `MAD` a consistent estimator of σ; `1.253314` is `√(π/2)`,
/// the equivalent factor for the *mean* absolute deviation, used as the documented
/// fallback when more than half the readings are identical and the MAD is zero —
/// which is the common case for a steady fan and would otherwise divide by zero.
const MAD_SCALE: f64 = 0.6745;
const MEAN_AD_SCALE: f64 = 1.253_314;

/// Count readings far enough from the median to be outliers, robustly.
///
/// Returns 0 when every reading is identical: there is no dispersion to be an
/// outlier from, and flagging on exact equality would make a perfectly steady
/// tach look pathological.
fn count_outliers(values: &[u16], median: u16) -> u32 {
    if values.len() < 3 {
        return 0;
    }
    let med = f64::from(median);
    let mut devs: Vec<f64> = values.iter().map(|&v| (f64::from(v) - med).abs()).collect();
    devs.sort_by(|a, b| a.partial_cmp(b).expect("no NaN from integer input"));
    let n = devs.len();
    let mad = if n % 2 == 1 {
        devs[n / 2]
    } else {
        (devs[n / 2 - 1] + devs[n / 2]) / 2.0
    };

    let denom = if mad > 0.0 {
        mad / MAD_SCALE
    } else {
        let mean_ad = devs.iter().sum::<f64>() / n as f64;
        if mean_ad <= 0.0 {
            return 0;
        }
        mean_ad * MEAN_AD_SCALE
    };

    values
        .iter()
        .filter(|&&v| (f64::from(v) - med).abs() / denom > constants::STABILITY_OUTLIER_MODIFIED_Z)
        .count() as u32
}

fn median_of_sorted(sorted: &[u16]) -> u16 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        // Widen before adding: two u16s near the top of the range overflow.
        let a = u32::from(sorted[n / 2 - 1]);
        let b = u32::from(sorted[n / 2]);
        ((a + b) / 2) as u16
    }
}

/// `§5`'s settling criterion, as the spec words it: the first sample from which
/// reported RPM stays within [`constants::SETTLING_BAND_PCT`] of the rolling
/// median for [`constants::SETTLING_HOLD_SAMPLES`] consecutive readings.
///
/// Returns that sample's `at_ms`, or `None` when the window never settled or
/// held too few usable readings to judge. **`None` is not "settled instantly"**
/// — the same distinction `first_change_ms` already carries.
pub fn settling_ms(samples: &[RpmSample]) -> Option<u64> {
    let usable: Vec<&RpmSample> = samples.iter().filter(|s| s.rpm.is_some()).collect();
    let hold = constants::SETTLING_HOLD_SAMPLES;
    if usable.len() < hold {
        return None;
    }
    for start in 0..=(usable.len() - hold) {
        // A WINDOW of `hold` readings, not the whole remaining tail.
        //
        // The first draft took `&usable[start..]`, which made the constant a
        // minimum tail length rather than the observation period both its own
        // doc and `constants::SETTLING_HOLD_SAMPLES` describe — and tuning it
        // changed nothing. Worse, one late tach spike then disqualified every
        // start index, so a window that plainly settled reported `None`
        // ("never settled"); DEC-334 §7 expects occasional outliers at these
        // sample counts, so that case is normal rather than exotic.
        let window = &usable[start..start + hold];
        let mut vals: Vec<u16> = window.iter().filter_map(|s| s.rpm).collect();
        vals.sort_unstable();
        let med = f64::from(median_of_sorted(&vals));
        // A zero median means the fan read as stopped for the rest of the
        // window; a percentage band around zero admits nothing, so treat an
        // all-zero tail as settled rather than never-settling.
        let band = if med > 0.0 {
            med * constants::SETTLING_BAND_PCT / 100.0
        } else {
            0.0
        };
        let settled = window
            .iter()
            .filter_map(|s| s.rpm)
            .all(|v| (f64::from(v) - med).abs() <= band);
        if settled {
            return Some(window[0].at_ms);
        }
    }
    None
}

/// A measured duty and the RPM observed at it, for the shape analyses below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DutyRpm {
    pub duty_pct: u8,
    pub rpm: u16,
}

/// A contiguous span of duties over which RPM did not meaningfully change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plateau {
    pub from_pct: u8,
    pub to_pct: u8,
    pub rpm_min: u16,
    pub rpm_max: u16,
}

/// `§3`: flat regions, requiring "tolerance bands and multiple observations
/// rather than declaring a plateau from a single equal reading".
///
/// `points` must be sorted ascending by duty and hold at most one entry per
/// duty; [`fold_direction`] produces exactly that. Returns maximal runs of at
/// least [`constants::PLATEAU_MIN_POINTS`] consecutive duties whose RPM spread
/// stays within [`constants::PLATEAU_BAND_PCT`] of the run's overall span.
pub fn plateaus(points: &[DutyRpm]) -> Vec<Plateau> {
    if points.len() < constants::PLATEAU_MIN_POINTS {
        return Vec::new();
    }
    let lo = points.iter().map(|p| p.rpm).min().unwrap_or(0);
    let hi = points.iter().map(|p| p.rpm).max().unwrap_or(0);
    let span = f64::from(hi.saturating_sub(lo));
    if span <= 0.0 {
        // Every reading identical: the whole sweep is one plateau. Saying so is
        // more useful than reporting none, and it is what a device running its
        // own internal control actually looks like.
        return vec![Plateau {
            from_pct: points[0].duty_pct,
            to_pct: points[points.len() - 1].duty_pct,
            rpm_min: lo,
            rpm_max: hi,
        }];
    }
    let band = span * constants::PLATEAU_BAND_PCT / 100.0;

    let mut out = Vec::new();
    let mut start = 0usize;
    while start < points.len() {
        let mut end = start;
        let mut run_min = points[start].rpm;
        let mut run_max = points[start].rpm;
        while end + 1 < points.len() {
            let next = points[end + 1].rpm;
            let new_min = run_min.min(next);
            let new_max = run_max.max(next);
            if f64::from(new_max.saturating_sub(new_min)) > band {
                break;
            }
            run_min = new_min;
            run_max = new_max;
            end += 1;
        }
        if end - start + 1 >= constants::PLATEAU_MIN_POINTS {
            out.push(Plateau {
                from_pct: points[start].duty_pct,
                to_pct: points[end].duty_pct,
                rpm_min: run_min,
                rpm_max: run_max,
            });
            start = end + 1;
        } else {
            start += 1;
        }
    }
    out
}

/// `§3`'s effective control range: the duties between the low plateau and the
/// high saturation plateau, where PWM changes actually move reported RPM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EffectiveRange {
    pub min_responsive_pct: Option<u8>,
    pub max_responsive_pct: Option<u8>,
    /// Top of a flat region at the bottom of the sweep.
    pub low_plateau_to_pct: Option<u8>,
    /// Duty above which RPM stops rising meaningfully.
    pub saturation_from_pct: Option<u8>,
}

/// Derive [`EffectiveRange`] from the plateaus of a duty-sorted series.
///
/// A plateau only counts as *low* if it starts at the lowest tested duty, and
/// only as *saturation* if it ends at the highest — an interior flat region is
/// neither, and calling it saturation would understate the usable range.
pub fn effective_range(points: &[DutyRpm]) -> EffectiveRange {
    let mut out = EffectiveRange::default();
    if points.is_empty() {
        return out;
    }
    let first = points[0].duty_pct;
    let last = points[points.len() - 1].duty_pct;
    let found = plateaus(points);

    // A single plateau spanning the whole sweep is not a control range at all;
    // reporting a "responsive" band inside it would invent one.
    if found
        .iter()
        .any(|p| p.from_pct == first && p.to_pct == last)
    {
        out.low_plateau_to_pct = Some(last);
        out.saturation_from_pct = Some(first);
        return out;
    }

    let low = found.iter().find(|p| p.from_pct == first);
    let high = found.iter().find(|p| p.to_pct == last);
    out.low_plateau_to_pct = low.map(|p| p.to_pct);
    out.saturation_from_pct = high.map(|p| p.from_pct);

    out.min_responsive_pct = match low {
        Some(p) => points
            .iter()
            .find(|d| d.duty_pct > p.to_pct)
            .map(|d| d.duty_pct),
        None => Some(first),
    };
    out.max_responsive_pct = match high {
        Some(p) => points
            .iter()
            .rev()
            .find(|d| d.duty_pct < p.from_pct)
            .map(|d| d.duty_pct),
        None => Some(last),
    };
    // A low plateau that swallowed everything below the saturation point can
    // invert these; an inverted range is not a range.
    if let (Some(lo), Some(hi)) = (out.min_responsive_pct, out.max_responsive_pct) {
        if lo > hi {
            out.min_responsive_pct = None;
            out.max_responsive_pct = None;
        }
    }
    out
}

/// `§2`'s hysteresis comparison at shared duties.
#[derive(Debug, Clone, PartialEq)]
pub struct Hysteresis {
    /// Largest rising/falling gap, as a percentage of the sweep's RPM span.
    pub magnitude_pct: Option<f64>,
    /// The duty at which that largest gap was measured.
    pub worst_duty_pct: Option<u8>,
    /// Largest gap in raw RPM.
    pub worst_delta_rpm: Option<u16>,
    /// How many duties carried both a rising and a falling reading.
    pub compared_points: u32,
    pub verdict: &'static str,
}

/// Compare rising against falling at every duty that carries **both**.
///
/// Duties with only one direction are excluded rather than compared against
/// nothing: the lowest duty has no rising sample and the highest no falling one
/// (the walk turns around at the bottom and the first step is a `Ramp` whose
/// approach direction is unknown), and pairing either with a neighbour would be
/// a fabricated comparison, not a measurement.
pub fn hysteresis(rising: &[DutyRpm], falling: &[DutyRpm], bidirectional: bool) -> Hysteresis {
    let mut out = Hysteresis {
        magnitude_pct: None,
        worst_duty_pct: None,
        worst_delta_rpm: None,
        compared_points: 0,
        verdict: if bidirectional {
            HYSTERESIS_INSUFFICIENT
        } else {
            HYSTERESIS_NOT_TESTED
        },
    };
    if !bidirectional {
        return out;
    }

    let mut all: Vec<u16> = rising.iter().map(|p| p.rpm).collect();
    all.extend(falling.iter().map(|p| p.rpm));
    if all.is_empty() {
        return out;
    }
    let span =
        f64::from(all.iter().copied().max().unwrap_or(0) - all.iter().copied().min().unwrap_or(0));

    let mut worst_delta: Option<(u8, u16)> = None;
    for r in rising {
        let Some(f) = falling.iter().find(|f| f.duty_pct == r.duty_pct) else {
            continue;
        };
        out.compared_points += 1;
        let delta = r.rpm.abs_diff(f.rpm);
        if worst_delta.is_none_or(|(_, d)| delta > d) {
            worst_delta = Some((r.duty_pct, delta));
        }
    }

    let Some((duty, delta)) = worst_delta else {
        return out;
    };
    out.worst_duty_pct = Some(duty);
    out.worst_delta_rpm = Some(delta);
    if span > 0.0 {
        let pct = f64::from(delta) / span * 100.0;
        out.magnitude_pct = Some(pct);
        out.verdict = if pct >= constants::HYSTERESIS_MIN_PCT {
            HYSTERESIS_PRESENT
        } else {
            HYSTERESIS_NONE
        };
    } else {
        // Every reading identical in both directions: no span to normalise
        // against, and no hysteresis either.
        out.magnitude_pct = Some(0.0);
        out.verdict = HYSTERESIS_NONE;
    }
    out
}

/// Collapse repeated readings at one duty into a single duty-sorted series.
///
/// Where a duty was measured more than once in the same direction (it cannot be,
/// in the plans this module is fed, but the function must be total) the **last**
/// reading wins — it is the one taken after the longest time at that duty.
pub fn fold_direction(points: impl IntoIterator<Item = DutyRpm>) -> Vec<DutyRpm> {
    let mut out: Vec<DutyRpm> = Vec::new();
    for p in points {
        match out.iter_mut().find(|e| e.duty_pct == p.duty_pct) {
            Some(existing) => *existing = p,
            None => out.push(p),
        }
    }
    out.sort_unstable_by_key(|p| p.duty_pct);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(at_ms: u64, rpm: u16) -> RpmSample {
        RpmSample {
            at_ms,
            rpm: Some(rpm),
        }
    }
    fn gap(at_ms: u64) -> RpmSample {
        RpmSample { at_ms, rpm: None }
    }
    /// `n` readings around `base`, alternating +/- `spread`, at 500 ms cadence.
    fn window(n: usize, base: u16, spread: u16) -> Vec<RpmSample> {
        (0..n)
            .map(|i| {
                let v = if i % 2 == 0 {
                    base + spread
                } else {
                    base - spread
                };
                s(i as u64 * 500, v)
            })
            .collect()
    }
    fn dr(duty_pct: u8, rpm: u16) -> DutyRpm {
        DutyRpm { duty_pct, rpm }
    }

    // ── rpm_stats ────────────────────────────────────────────────────

    #[test]
    fn an_empty_window_is_unavailable_not_zero() {
        let st = rpm_stats(&[]);
        assert_eq!(st.verdict, STABILITY_UNAVAILABLE);
        assert_eq!(st.usable, 0);
        assert_eq!(st.mean, None, "no readings must not report a mean of 0");
        assert_eq!(st.min, None);
        assert_eq!(st.cv_pct, None);
    }

    #[test]
    fn a_window_of_only_unreadable_samples_is_unavailable_and_counts_every_one() {
        let st = rpm_stats(&[gap(0), gap(500), gap(1000)]);
        assert_eq!(st.verdict, STABILITY_UNAVAILABLE);
        assert_eq!(st.samples, 3);
        assert_eq!(st.dropouts, 3);
        assert_eq!(st.usable, 0);
    }

    /// The honest half of the dropout definition, and the reason it is not just
    /// "count the zeros": a fan that is genuinely stopped reports 0 for the whole
    /// window, and calling every one of those a tach dropout would invent a fault.
    #[test]
    fn an_all_zero_window_is_a_stopped_fan_and_reports_no_dropouts() {
        let st = rpm_stats(&[s(0, 0), s(500, 0), s(1000, 0), s(1500, 0)]);
        assert_eq!(st.dropouts, 0, "a steadily stopped fan has not dropped out");
        assert_eq!(st.usable, 4);
        assert_eq!(st.max, Some(0));
        assert_eq!(
            st.cv_pct, None,
            "a zero mean has no meaningful relative spread"
        );
    }

    /// The other half: the same 0 reading IS a dropout once the window proves
    /// the fan was turning.
    #[test]
    fn a_zero_among_spinning_samples_is_a_dropout() {
        let st = rpm_stats(&[s(0, 900), s(500, 0), s(1000, 910), s(1500, 905)]);
        assert_eq!(st.dropouts, 1);
        assert_eq!(st.usable, 4, "a dropout is still a retained reading");
    }

    #[test]
    fn outliers_are_counted_but_never_removed_from_the_statistics() {
        // Nine tight readings and one far away.
        let mut v: Vec<RpmSample> = (0..9).map(|i| s(i * 500, 1000)).collect();
        v.push(s(4500, 5000));
        let st = rpm_stats(&v);
        assert_eq!(st.outliers, 1);
        assert_eq!(st.max, Some(5000), "the outlier stays in the raw range");
        let mean = st.mean.expect("mean");
        assert!(
            mean > 1000.0,
            "the outlier must still be inside the mean ({mean}), not silently dropped"
        );
        assert_eq!(st.usable, 10, "an outlier is still a usable reading");
    }

    /// [DEC-320] A bound derived from an assumed maximum is not a bound. The
    /// first draft of the outlier rule was `3 * stddev`, which **cannot fire** at
    /// the sample counts this feature collects: with a population σ the largest
    /// possible z-score in a window of `n` readings is `(n-1)/√n`. The field
    /// would have read 0 forever and looked like a clean tach.
    ///
    /// **Sample count chosen so the check actually discriminates.** The first
    /// draft of *this* test used a 12-reading window — the count a default 6 s
    /// settle produces — and it **passed with the sigma rule restored**, because
    /// `11/√12 = 3.18` clears `3.0`. It proved nothing. Pinned instead at
    /// [`constants::STABILITY_MIN_SAMPLES`], the floor at which statistics are
    /// published at all, where the σ ceiling is unreachable by construction.
    #[test]
    fn an_outlier_is_detectable_at_the_minimum_publishable_sample_count() {
        let n = constants::STABILITY_MIN_SAMPLES;
        // Precondition: this test only discriminates while a sigma rule is
        // arithmetically incapable of firing here. If MIN_SAMPLES is ever raised
        // past ~13 this fails loudly instead of passing for the wrong reason.
        let sigma_ceiling = (n as f64 - 1.0) / (n as f64).sqrt();
        assert!(
            sigma_ceiling < 3.0,
            "this test no longer discriminates: at n={n} the sigma ceiling is \
             {sigma_ceiling:.2}, so the rule it exists to rule out could fire. \
             Re-read why it exists before adjusting it."
        );

        let mut v: Vec<RpmSample> = (0..n - 1).map(|i| s(i as u64 * 500, 1000)).collect();
        v.push(s((n as u64 - 1) * 500, 4000));
        let st = rpm_stats(&v);
        assert_eq!(
            st.outliers,
            1,
            "a 4x reading among {} steady ones must be detectable at the minimum \
             publishable sample count",
            n - 1
        );
        assert_eq!(
            st.verdict, STABILITY_UNSTABLE,
            "and the window it sits in is not a stable one"
        );
    }

    #[test]
    fn the_median_of_an_even_window_does_not_overflow_u16() {
        let st = rpm_stats(&[s(0, u16::MAX), s(500, u16::MAX)]);
        assert_eq!(st.median, Some(u16::MAX));
    }

    #[test]
    fn median_picks_the_middle_of_an_odd_window() {
        let st = rpm_stats(&[s(0, 100), s(500, 900), s(1000, 500)]);
        assert_eq!(st.median, Some(500));
    }

    // ── classify_stability ───────────────────────────────────────────

    #[test]
    fn too_few_samples_is_insufficient_data_rather_than_a_guess() {
        let n = constants::STABILITY_MIN_SAMPLES - 1;
        let st = rpm_stats(&window(n, 1000, 1));
        assert_eq!(st.verdict, STABILITY_INSUFFICIENT);
        assert!(
            st.mean.is_some(),
            "insufficient for a verdict still reports what it measured"
        );
    }

    /// Assert the RELATIONSHIP to the thresholds, not the literals — a threshold
    /// change must not silently invert what these tests claim.
    #[test]
    fn the_three_stability_bands_are_ordered_by_cv() {
        let n = constants::STABILITY_MIN_SAMPLES + 4;
        let stable = constants::STABILITY_STABLE_MAX_CV_PCT;
        let variable = constants::STABILITY_VARIABLE_MAX_CV_PCT;
        assert_eq!(
            classify_stability(n as u32, Some(stable / 2.0)),
            STABILITY_STABLE
        );
        assert_eq!(classify_stability(n as u32, Some(stable)), STABILITY_STABLE);
        assert_eq!(
            classify_stability(n as u32, Some((stable + variable) / 2.0)),
            STABILITY_VARIABLE
        );
        assert_eq!(
            classify_stability(n as u32, Some(variable)),
            STABILITY_VARIABLE
        );
        assert_eq!(
            classify_stability(n as u32, Some(variable * 2.0)),
            STABILITY_UNSTABLE
        );
    }

    #[test]
    fn a_steadily_stopped_fan_classifies_stable_not_unstable() {
        let n = constants::STABILITY_MIN_SAMPLES + 2;
        let all_zero: Vec<RpmSample> = (0..n).map(|i| s(i as u64 * 500, 0)).collect();
        let st = rpm_stats(&all_zero);
        assert_eq!(st.verdict, STABILITY_STABLE);
    }

    #[test]
    fn a_real_spread_reads_as_unstable() {
        // +/-30% swing about 1000.
        let n = constants::STABILITY_MIN_SAMPLES + 2;
        let st = rpm_stats(&window(n, 1000, 300));
        assert_eq!(st.verdict, STABILITY_UNSTABLE);
        assert!(st.cv_pct.expect("cv") > constants::STABILITY_VARIABLE_MAX_CV_PCT);
    }

    // ── settling_ms ──────────────────────────────────────────────────

    #[test]
    fn a_window_that_never_settles_reports_none_not_zero() {
        // Monotonically climbing: no tail ever sits inside the band.
        let v: Vec<RpmSample> = (0..12)
            .map(|i| s(i * 500, 500 + (i as u16) * 300))
            .collect();
        assert_eq!(settling_ms(&v), None);
    }

    #[test]
    fn settling_reports_the_first_sample_of_the_settled_tail() {
        // Four noisy readings, then a flat tail starting at 2000 ms.
        let v = vec![
            s(0, 400),
            s(500, 1200),
            s(1000, 700),
            s(1500, 1100),
            s(2000, 1000),
            s(2500, 1005),
            s(3000, 998),
            s(3500, 1002),
        ];
        assert_eq!(settling_ms(&v), Some(2000));
    }

    /// A settled window followed by one late spike still reports a settling
    /// time.
    ///
    /// The first draft matched the whole remaining tail rather than a window of
    /// `SETTLING_HOLD_SAMPLES`, so a single late outlier disqualified every
    /// start index and the window read as "never settled". DEC-334 §7 expects
    /// occasional outliers at these sample counts, so that is the normal case,
    /// not an exotic one — and it also made the constant a minimum tail length
    /// rather than the observation period its own doc describes.
    #[test]
    fn a_late_spike_does_not_erase_an_earlier_settled_window() {
        let hold = constants::SETTLING_HOLD_SAMPLES;
        // `hold` steady readings from t=0, then one wild one.
        let mut v: Vec<RpmSample> = (0..hold).map(|i| s(i as u64 * 500, 1000)).collect();
        v.push(s(hold as u64 * 500, 4000));
        assert_eq!(
            settling_ms(&v),
            Some(0),
            "a window of {hold} steady readings settled at t=0 regardless of what \
             happened afterwards"
        );
    }

    /// The other direction: the criterion is still a real one, and noise inside
    /// the window disqualifies it.
    #[test]
    fn noise_inside_the_window_still_prevents_a_settling_claim() {
        let hold = constants::SETTLING_HOLD_SAMPLES;
        let v: Vec<RpmSample> = (0..hold * 2)
            .map(|i| s(i as u64 * 500, if i % 2 == 0 { 500 } else { 3000 }))
            .collect();
        assert_eq!(settling_ms(&v), None);
    }

    #[test]
    fn too_few_usable_samples_cannot_claim_a_settling_time() {
        let v = vec![s(0, 1000), gap(500), s(1000, 1000)];
        assert!(v.len() >= constants::SETTLING_HOLD_SAMPLES.saturating_sub(1));
        assert_eq!(settling_ms(&v), None);
    }

    #[test]
    fn settling_ignores_dropouts_rather_than_treating_them_as_a_disturbance() {
        let v = vec![
            s(0, 1000),
            gap(500),
            s(1000, 1002),
            gap(1500),
            s(2000, 999),
            s(2500, 1001),
        ];
        assert_eq!(
            settling_ms(&v),
            Some(0),
            "an unreadable sample is missing data, not a departure from the band"
        );
    }

    // ── plateaus / effective_range ───────────────────────────────────

    #[test]
    fn a_single_equal_pair_is_not_a_plateau() {
        let pts = vec![dr(30, 900), dr(40, 900), dr(50, 2000), dr(60, 3000)];
        // A compile-time precondition, not a runtime one: this test only means
        // something while a plateau needs more than two points.
        const _: () = assert!(constants::PLATEAU_MIN_POINTS > 2);
        assert!(plateaus(&pts).is_empty());
    }

    #[test]
    fn a_flat_low_region_is_reported_with_its_span() {
        let pts = vec![
            dr(30, 900),
            dr(40, 905),
            dr(50, 902),
            dr(60, 1800),
            dr(70, 2600),
            dr(80, 3400),
        ];
        let found = plateaus(&pts);
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].from_pct, 30);
        assert_eq!(found[0].to_pct, 50);
    }

    #[test]
    fn an_entirely_flat_sweep_is_one_plateau_over_the_whole_range() {
        let pts = vec![dr(30, 2000), dr(40, 2000), dr(50, 2000), dr(60, 2000)];
        let found = plateaus(&pts);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].from_pct, found[0].to_pct), (30, 60));
    }

    #[test]
    fn an_entirely_flat_sweep_reports_no_responsive_band() {
        let pts = vec![dr(30, 2000), dr(40, 2000), dr(50, 2000), dr(60, 2000)];
        let r = effective_range(&pts);
        assert_eq!(r.min_responsive_pct, None);
        assert_eq!(r.max_responsive_pct, None);
    }

    #[test]
    fn the_responsive_band_starts_above_the_low_plateau_and_ends_below_saturation() {
        let pts = vec![
            dr(30, 900),
            dr(40, 905),
            dr(50, 902),
            dr(60, 1800),
            dr(70, 2600),
            dr(80, 3380),
            dr(90, 3385),
            dr(100, 3382),
        ];
        let r = effective_range(&pts);
        assert_eq!(r.low_plateau_to_pct, Some(50));
        assert_eq!(r.min_responsive_pct, Some(60));
        assert_eq!(r.saturation_from_pct, Some(80));
        assert_eq!(r.max_responsive_pct, Some(70));
    }

    #[test]
    fn a_fully_responsive_sweep_spans_the_whole_tested_range() {
        let pts = vec![dr(30, 500), dr(40, 1200), dr(50, 2000), dr(60, 2900)];
        let r = effective_range(&pts);
        assert_eq!(r.min_responsive_pct, Some(30));
        assert_eq!(r.max_responsive_pct, Some(60));
        assert_eq!(r.low_plateau_to_pct, None);
        assert_eq!(r.saturation_from_pct, None);
    }

    // ── hysteresis ───────────────────────────────────────────────────

    #[test]
    fn a_unidirectional_sweep_reports_not_tested_rather_than_none() {
        let h = hysteresis(&[dr(30, 900), dr(40, 1200)], &[], false);
        assert_eq!(h.verdict, HYSTERESIS_NOT_TESTED);
        assert_eq!(h.compared_points, 0);
        assert_eq!(h.magnitude_pct, None);
    }

    #[test]
    fn a_bidirectional_sweep_with_no_shared_duty_is_insufficient_not_none() {
        let h = hysteresis(&[dr(40, 1200)], &[dr(30, 900)], true);
        assert_eq!(h.verdict, HYSTERESIS_INSUFFICIENT);
        assert_eq!(h.compared_points, 0);
    }

    #[test]
    fn only_duties_present_in_both_directions_are_compared() {
        let rising = vec![dr(40, 1200), dr(50, 2000), dr(60, 2800)];
        let falling = vec![dr(40, 1250), dr(50, 2050)];
        let h = hysteresis(&rising, &falling, true);
        assert_eq!(
            h.compared_points, 2,
            "60% has no falling reading and must not be paired with anything"
        );
    }

    #[test]
    fn a_small_gap_is_noise_and_a_large_one_is_hysteresis() {
        let span_lo = 900u16;
        let span_hi = 3000u16;
        let span = f64::from(span_hi - span_lo);
        // A gap deliberately under the threshold, and one deliberately over it.
        let small = (span * constants::HYSTERESIS_MIN_PCT / 100.0 / 2.0) as u16;
        let large = (span * constants::HYSTERESIS_MIN_PCT / 100.0 * 3.0) as u16;

        let rising = vec![dr(30, span_lo), dr(60, 1800), dr(100, span_hi)];
        let quiet = vec![dr(30, span_lo), dr(60, 1800 + small), dr(100, span_hi)];
        let loud = vec![dr(30, span_lo), dr(60, 1800 + large), dr(100, span_hi)];

        assert_eq!(hysteresis(&rising, &quiet, true).verdict, HYSTERESIS_NONE);
        let h = hysteresis(&rising, &loud, true);
        assert_eq!(h.verdict, HYSTERESIS_PRESENT);
        assert_eq!(h.worst_duty_pct, Some(60));
        assert_eq!(h.worst_delta_rpm, Some(large));
    }

    #[test]
    fn identical_readings_in_both_directions_are_no_hysteresis_not_a_division_by_zero() {
        let pts = vec![dr(30, 2000), dr(40, 2000)];
        let h = hysteresis(&pts, &pts, true);
        assert_eq!(h.verdict, HYSTERESIS_NONE);
        assert_eq!(h.magnitude_pct, Some(0.0));
    }

    // ── fold_direction ───────────────────────────────────────────────

    #[test]
    fn folding_sorts_by_duty_and_keeps_the_last_reading_at_a_repeated_duty() {
        let out = fold_direction(vec![dr(60, 100), dr(30, 900), dr(60, 200)]);
        assert_eq!(out, vec![dr(30, 900), dr(60, 200)]);
    }
}
