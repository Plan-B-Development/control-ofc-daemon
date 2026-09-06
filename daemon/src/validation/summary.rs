//! Derives the compatibility/evidence summary from a recorded session (§8).
//!
//! **Pure and total.** [`summarise`] takes a finished session and returns
//! findings; it reads no cache, takes no lock, and touches no hardware. That is
//! what makes §7's semantics exhaustively testable, and it is why the recorder
//! calls it at finalisation rather than deriving verdicts as it samples.
//!
//! Two rules run through everything here:
//!
//! * **`unavailable` never becomes `fail`** (§7). Hardware that does not expose a
//!   capability has not failed a test — it was never testable. Likewise the
//!   absence of a diagnostic is `not_tested`, never `pass`.
//! * **Referenced diagnostics are preserved, not recomputed** (§6, §10). Where a
//!   Phase 3 run is attached, its own `possible_device_override` and verdicts are
//!   carried through. This module must never grow a second copy of that
//!   algorithm.

use super::session::*;
use crate::constants;

/// Build the summary for a session.
///
/// The `state` of the session matters: a session that was interrupted reports
/// `interrupted` for anything a completed run would have decided, rather than
/// silently reporting it as untested (§7, §15).
pub fn summarise(session: &ValidationSession) -> Vec<ValidationFinding> {
    let interrupted = session.state == STATE_INTERRUPTED;
    let mut out = Vec::new();

    out.push(pwm_header_control(session, interrupted));
    out.push(pwm_readback(session, interrupted));
    out.extend(rpm_telemetry(session));
    out.push(pwm_response(session, interrupted));
    out.push(response_latency(session, interrupted));
    // DEC-334 (AIO Phase 8 Batch 2). All four are OBSERVATIONAL by construction:
    // §2 forbids treating hysteresis as a fault, §3 forbids reinterpreting a
    // plateau as pump failure, §4 forbids inferring cavitation or an electrical
    // fault from tach variability, and §6 forbids labelling unexpected RPM as
    // hardware failure. None of them can produce RESULT_FAIL.
    out.push(hysteresis_finding(session, interrupted));
    out.push(stability_finding(session, interrupted));
    out.push(effective_range_finding(session, interrupted));
    out.push(learned_range_finding(session, interrupted));
    out.push(startup_behaviour(session));
    // DEC-335 (Batch 3a §3). Observational like the four above: `not_established`
    // is a statement about the observation, never about the cooler.
    out.push(steady_state_finding(session));
    out.extend(divergence(session));
    out.push(device_override(session, interrupted));
    out.push(bios_reclaim(session));
    out.push(thermal_safety(session));
    out.push(control_restoration(session));
    out.push(control_path(session, interrupted));
    out.push(coolant_telemetry(session));
    out.push(daemon_restart_recovery(session));

    out
}

fn finding(id: &str, state: &str) -> ValidationFinding {
    ValidationFinding {
        id: id.to_string(),
        state: state.to_string(),
        detail: None,
        member_id: None,
        evidence_kind: None,
    }
}

fn with_detail(mut f: ValidationFinding, detail: impl Into<String>) -> ValidationFinding {
    f.detail = Some(detail.into());
    f
}

/// The state a not-run diagnostic reports: `interrupted` if the session was cut
/// short, otherwise `not_tested`. Never `pass` — that is §7's central rule.
fn absent_state(interrupted: bool) -> &'static str {
    if interrupted {
        RESULT_INTERRUPTED
    } else {
        RESULT_NOT_TESTED
    }
}

/// Every characterisation run in this session, **of either kind**.
///
/// [DEC-334, Q15] The behaviour sweep is a strict superset of the basic one: same
/// route, same run type, same `evidence[].characterization` payload, and
/// `ordered_diagnostics` runs it *instead of* the basic sweep when both are
/// requested. So every finding derived from a characterisation must see it — and
/// this one-line filter is the whole of that promise.
///
/// Filtering on `DIAG_CHARACTERIZATION` alone would have silently emptied
/// `pwm_response_characterization`, `response_latency` and
/// `possible_device_override` for any session that asked for the richer sweep,
/// with every one of them reporting `not_tested` about a diagnostic that had just
/// run. Pinned by `a_behaviour_run_feeds_every_basic_characterisation_finding`.
fn characterizations(session: &ValidationSession) -> impl Iterator<Item = &EvidenceRef> {
    session
        .evidence
        .iter()
        .filter(|e| e.kind == DIAG_CHARACTERIZATION || e.kind == DIAG_BEHAVIOUR)
}

fn verifies(session: &ValidationSession) -> impl Iterator<Item = &EvidenceRef> {
    session.evidence.iter().filter(|e| e.kind == DIAG_VERIFY)
}

// ── Individual findings ─────────────────────────────────────────────────────

/// Did the header accept PWM commands at all?
fn pwm_header_control(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    // Prefer the sweep's own verdict — it tested many duties, verify tested one.
    for ev in characterizations(session) {
        if let Some(run) = &ev.characterization {
            if let Some(sum) = &run.summary {
                let state = match sum.command_acceptance.as_str() {
                    "pass" => RESULT_PASS,
                    "fail" => RESULT_FAIL,
                    // `partial`, or a token a newer daemon added: real evidence,
                    // but not a clean verdict. Rendered, never dropped.
                    _ => RESULT_UNKNOWN,
                };
                let mut f = with_detail(
                    finding(F_PWM_HEADER_CONTROL, state),
                    format!("command acceptance: {}", sum.command_acceptance),
                );
                f.member_id = Some(ev.member_id.clone());
                f.evidence_kind = Some(ev.kind.clone());
                return f;
            }
        }
    }
    for ev in verifies(session) {
        if let Some(v) = &ev.verify {
            let state = if v.write_ok { RESULT_PASS } else { RESULT_FAIL };
            let mut f = finding(F_PWM_HEADER_CONTROL, state);
            f.member_id = Some(ev.member_id.clone());
            f.evidence_kind = Some(DIAG_VERIFY.to_string());
            return f;
        }
    }
    finding(F_PWM_HEADER_CONTROL, absent_state(interrupted))
}

/// Did the written duty read back?
fn pwm_readback(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    for ev in characterizations(session) {
        if let Some(run) = &ev.characterization {
            if let Some(sum) = &run.summary {
                let state = match sum.pwm_readback.as_str() {
                    "pass" => RESULT_PASS,
                    // A clamp or a BIOS reclaim is evidence about the device, not
                    // a failed test of the daemon's write path.
                    "clamped" | "reverted" => RESULT_OBSERVED,
                    "unavailable" => RESULT_UNAVAILABLE,
                    _ => RESULT_UNKNOWN,
                };
                let mut f = with_detail(
                    finding(F_PWM_READBACK, state),
                    format!("readback: {}", sum.pwm_readback),
                );
                f.member_id = Some(ev.member_id.clone());
                f.evidence_kind = Some(ev.kind.clone());
                return f;
            }
        }
    }
    // Fall back to what the samples saw — a readback column that was never
    // populated is `unavailable`, not a failure.
    let any_readback = session
        .samples
        .iter()
        .any(|s| s.members.iter().any(|m| m.readback_pct.is_some()));
    if session.samples.is_empty() {
        finding(F_PWM_READBACK, absent_state(interrupted))
    } else if any_readback {
        with_detail(
            finding(F_PWM_READBACK, RESULT_OBSERVED),
            "readback sampled, not swept",
        )
    } else {
        with_detail(
            finding(F_PWM_READBACK, RESULT_UNAVAILABLE),
            "no PWM readback exposed",
        )
    }
}

/// One finding per member, so radiator identity survives (§3) rather than being
/// flattened into an invented single value.
fn rpm_telemetry(session: &ValidationSession) -> Vec<ValidationFinding> {
    let mut out = Vec::new();
    for member in &session.metadata.members {
        let id = &member.member_id;
        let saw_any = session.samples.iter().any(|s| {
            s.members
                .iter()
                .any(|m| &m.member_id == id && m.rpm.is_some())
        });
        let sampled = session
            .samples
            .iter()
            .any(|s| s.members.iter().any(|m| &m.member_id == id));

        let finding_id = if member.member_kind == MEMBER_PUMP {
            F_PUMP_RPM
        } else {
            F_RADIATOR_RPM
        };
        // Auxiliaries are recorded but are not part of §8's summary lines.
        if member.member_kind == MEMBER_AUXILIARY {
            continue;
        }
        let state = if !sampled {
            RESULT_NOT_TESTED
        } else if saw_any {
            RESULT_PASS
        } else {
            // A header with no tach reports no RPM. That is missing telemetry,
            // not a stopped fan and not a failure (§3, §7).
            RESULT_UNAVAILABLE
        };
        let mut f = with_detail(finding(finding_id, state), member.label.clone());
        f.member_id = Some(id.clone());
        out.push(f);
    }
    if out.is_empty() {
        out.push(finding(F_PUMP_RPM, RESULT_NOT_TESTED));
    }
    out
}

/// Did RPM respond across the swept duty range?
fn pwm_response(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    for ev in characterizations(session) {
        if let Some(run) = &ev.characterization {
            if let Some(sum) = &run.summary {
                let state = match sum.rpm_response.as_str() {
                    "pass" => RESULT_PASS,
                    "fail" => RESULT_FAIL,
                    "unavailable" => RESULT_UNAVAILABLE,
                    _ => RESULT_UNKNOWN,
                };
                let mut f = with_detail(
                    finding(F_PWM_RESPONSE, state),
                    format!("rpm response: {}", sum.rpm_response),
                );
                f.member_id = Some(ev.member_id.clone());
                f.evidence_kind = Some(ev.kind.clone());
                return f;
            }
        }
    }
    finding(F_PWM_RESPONSE, absent_state(interrupted))
}

/// Which tach channel(s) does this header actually drive (AIO Phase 8 Batch 1)?
///
/// [SAFETY-adjacent reporting rule] A run that found nothing is `not_observed`,
/// **never** `fail`. §5 and the Overview both say it outright: "Do not label an
/// unexpected RPM response as hardware failure unless evidence supports that
/// conclusion." A header that drives no tach-reporting device, or whose device is
/// running under its own internal control, is a legitimate configuration — and it
/// is also exactly what a `possible_device_override` looks like from here.
///
/// `ambiguous` maps to `unknown` rather than to a verdict, for the same reason:
/// the run's own answer was "not repeatable enough to rely on", and promoting
/// that to either pass or fail would be the report inventing evidence.
fn control_path(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    for ev in session
        .evidence
        .iter()
        .filter(|e| e.kind == DIAG_CONTROL_PATH)
    {
        if let Some(run) = &ev.control_path {
            if let Some(sum) = &run.summary {
                use crate::api::discovery as disc;
                let state = match sum.relationship.as_str() {
                    disc::REL_CONFIRMED | disc::REL_PROBABLE | disc::REL_MULTIPLE => {
                        RESULT_OBSERVED
                    }
                    disc::REL_NO_RESPONSE => RESULT_NOT_OBSERVED,
                    _ => RESULT_UNKNOWN,
                };
                let detail = match sum.candidates.first() {
                    Some(best) => format!(
                        "{} -> {} ({} confidence)",
                        run.header_id, best.label, sum.confidence
                    ),
                    None => format!("{}: no tach channel responded", run.header_id),
                };
                let mut f = with_detail(finding(F_CONTROL_PATH, state), detail);
                f.member_id = Some(ev.member_id.clone());
                f.evidence_kind = Some(DIAG_CONTROL_PATH.to_string());
                return f;
            }
        }
    }
    finding(F_CONTROL_PATH, absent_state(interrupted))
}

/// §2. How far apart were the rising and falling curves?
///
/// `not_observed` when the walk was unidirectional — that is "we did not look",
/// which is a different statement from "there is none", and the Overview is
/// explicit that lack of evidence must not become a PASS.
fn hysteresis_finding(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    use crate::api::stats;
    for ev in characterizations(session) {
        let Some(sum) = ev
            .characterization
            .as_ref()
            .and_then(|r| r.summary.as_ref())
        else {
            continue;
        };
        let (state, detail) = match sum.hysteresis_verdict.as_str() {
            stats::HYSTERESIS_NOT_TESTED => (
                RESULT_NOT_TESTED,
                "the sweep walked one direction only".to_string(),
            ),
            stats::HYSTERESIS_INSUFFICIENT => (
                RESULT_UNKNOWN,
                "no duty carried readings in both directions".to_string(),
            ),
            stats::HYSTERESIS_NONE => (
                RESULT_NOT_OBSERVED,
                match sum.hysteresis_pct {
                    Some(p) => format!("rising and falling agree to within {p:.1}% of span"),
                    None => "rising and falling agree".to_string(),
                },
            ),
            // Present, and reported as an observation about the DEVICE, never a
            // verdict on it: §2 names an internal controller, a firmware clamp,
            // temperature dependence, noise and tach scaling as explanations.
            _ => (
                RESULT_OBSERVED,
                match (sum.hysteresis_pct, sum.hysteresis_worst_duty_pct) {
                    (Some(p), Some(d)) => {
                        format!("rising and falling differ by up to {p:.1}% of span, worst at {d}%")
                    }
                    (Some(p), None) => {
                        format!("rising and falling differ by up to {p:.1}% of span")
                    }
                    _ => "rising and falling differ".to_string(),
                },
            ),
        };
        let mut f = with_detail(finding(F_HYSTERESIS, state), detail);
        f.member_id = Some(ev.member_id.clone());
        f.evidence_kind = ev.characterization.as_ref().map(|_| ev.kind.clone());
        return f;
    }
    finding(F_HYSTERESIS, absent_state(interrupted))
}

/// §4. How steady was the tach at a held duty?
fn stability_finding(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    use crate::api::stats;
    for ev in characterizations(session) {
        let Some(sum) = ev
            .characterization
            .as_ref()
            .and_then(|r| r.summary.as_ref())
        else {
            continue;
        };
        if sum.stability_verdict.is_empty() {
            continue;
        }
        let state = match sum.stability_verdict.as_str() {
            stats::STABILITY_UNAVAILABLE => RESULT_UNAVAILABLE,
            stats::STABILITY_INSUFFICIENT => RESULT_UNKNOWN,
            // `variable` and `unstable` are OBSERVED, never FAIL. §4: "Do not
            // claim cavitation, electrical failure or bubbles purely from tach
            // variability."
            _ => RESULT_OBSERVED,
        };
        let mut detail = format!("worst per-point stability: {}", sum.stability_verdict);
        if let Some(cv) = sum.worst_cv_pct {
            detail.push_str(&format!(" (worst CV {cv:.1}%)"));
        }
        if sum.total_dropouts > 0 {
            detail.push_str(&format!(", {} tach dropout(s)", sum.total_dropouts));
        }
        let mut f = with_detail(finding(F_RPM_STABILITY, state), detail);
        f.member_id = Some(ev.member_id.clone());
        f.evidence_kind = Some(ev.kind.clone());
        return f;
    }
    finding(F_RPM_STABILITY, absent_state(interrupted))
}

/// §3. Over what band does PWM actually move reported RPM?
fn effective_range_finding(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    for ev in characterizations(session) {
        let Some(sum) = ev
            .characterization
            .as_ref()
            .and_then(|r| r.summary.as_ref())
        else {
            continue;
        };
        let detail = match (sum.min_responsive_pct, sum.max_responsive_pct) {
            (Some(lo), Some(hi)) => {
                let mut d = format!("effective control range {lo}-{hi}%");
                if let Some(p) = sum.low_plateau_to_pct {
                    d.push_str(&format!(", low plateau to {p}%"));
                }
                if let Some(p) = sum.saturation_from_pct {
                    d.push_str(&format!(", saturating from {p}%"));
                }
                d
            }
            // A sweep that plateaued end to end has no responsive band. That is
            // an observation about the device — §3: "Do not reinterpret a
            // plateau as pump failure."
            _ => "no duty change produced a meaningful RPM change".to_string(),
        };
        let state = if sum.min_responsive_pct.is_some() {
            RESULT_OBSERVED
        } else {
            RESULT_NOT_OBSERVED
        };
        let mut f = with_detail(finding(F_EFFECTIVE_RANGE, state), detail);
        f.member_id = Some(ev.member_id.clone());
        f.evidence_kind = Some(ev.kind.clone());
        return f;
    }
    finding(F_EFFECTIVE_RANGE, absent_state(interrupted))
}

/// §6. Did this run agree with what previous runs learned?
fn learned_range_finding(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    for ev in characterizations(session) {
        let Some(sum) = ev
            .characterization
            .as_ref()
            .and_then(|r| r.summary.as_ref())
        else {
            continue;
        };
        let (state, detail) = match sum.outside_learned_range {
            // Three states, and this is the one that matters: no model yet is
            // NOT a pass. §6 compares against a previously learned response, and
            // a first run has nothing to compare with.
            None => (
                RESULT_NOT_TESTED,
                "no learned response range for this header yet".to_string(),
            ),
            Some(false) => (
                RESULT_NOT_OBSERVED,
                "every reading fell inside the learned response range".to_string(),
            ),
            Some(true) => {
                let mut d = "reported RPM outside the learned response range".to_string();
                if let Some(note) = &sum.learned_range_note {
                    d.push_str(&format!(" — {note}"));
                }
                if !sum.interpretation_states.is_empty() {
                    d.push_str(&format!(
                        "; possible explanations: {}",
                        sum.interpretation_states.join(", ")
                    ));
                }
                // OBSERVED, never FAIL. §8.5 requires cautious wording here and
                // forbids a generic red "hardware failed" for this condition.
                (RESULT_OBSERVED, d)
            }
        };
        let mut f = with_detail(finding(F_LEARNED_RANGE, state), detail);
        f.member_id = Some(ev.member_id.clone());
        f.evidence_kind = Some(ev.kind.clone());
        return f;
    }
    finding(F_LEARNED_RANGE, absent_state(interrupted))
}

/// How quickly did RPM begin to move after a duty change?
fn response_latency(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    for ev in characterizations(session) {
        if let Some(run) = &ev.characterization {
            let latencies: Vec<u64> = run
                .points
                .iter()
                .filter_map(|p| p.first_change_ms)
                .collect();
            if latencies.is_empty() {
                continue;
            }
            let min = latencies.iter().copied().min().unwrap_or(0);
            let max = latencies.iter().copied().max().unwrap_or(0);
            // [DEC-334, §5] "Do not publish unrealistic millisecond precision
            // when the driver updates tach once per second or slower."
            //
            // This line used to read "first RPM change 500–3000 ms", which is
            // millisecond wording over a figure that can only ever be a multiple
            // of the sub-sample interval — the sweep detects a change by polling,
            // so the timing's true resolution is that cadence and nothing finer.
            // Report in the resolution that exists, and name it, exactly as
            // discovery's `measurement_resolution_ms` already does.
            let resolution_ms = run
                .summary
                .as_ref()
                .and_then(|s| s.measurement_resolution_ms)
                .unwrap_or_else(|| {
                    crate::constants::CHARACTERIZATION_SAMPLE_INTERVAL.as_millis() as u64
                })
                .max(1);
            let quantise = |ms: u64| (ms / resolution_ms) * resolution_ms;
            let detail = if min == max {
                format!(
                    "first RPM change ~{} ms across {} points (resolution {resolution_ms} ms)",
                    quantise(min),
                    latencies.len()
                )
            } else {
                format!(
                    "first RPM change ~{}–{} ms across {} points (resolution {resolution_ms} ms)",
                    quantise(min),
                    quantise(max),
                    latencies.len()
                )
            };
            let mut f = with_detail(finding(F_RESPONSE_LATENCY, RESULT_OBSERVED), detail);
            f.member_id = Some(ev.member_id.clone());
            f.evidence_kind = Some(ev.kind.clone());
            return f;
        }
    }
    // A sweep that ran but saw no RPM movement has latency data that is
    // unavailable, not untested.
    if characterizations(session).any(|e| e.characterization.is_some()) {
        return with_detail(
            finding(F_RESPONSE_LATENCY, RESULT_UNAVAILABLE),
            "no RPM change timing captured",
        );
    }
    finding(F_RESPONSE_LATENCY, absent_state(interrupted))
}

/// Derive the Batch 3a §1 and §3 analyses onto the session.
///
/// Called once at finalisation, before [`summarise`], because both findings read
/// what this writes. Kept separate from `summarise` for the same reason
/// everything in `stats` is separate from the sweep: these are total functions
/// over the recorded samples, and a caller that wants the numbers should not
/// have to render a finding to get them.
pub fn derive_analysis(session: &mut ValidationSession) {
    session.startup_fingerprints = startup_fingerprints(session);
    session.steady_state = steady_state_for(session);
}

/// The control temperature as a series [`crate::api::stats`] can analyse.
///
/// Keyed on `elapsed_ms` rather than `unix_ms`: the criterion is about rates,
/// and a wall clock can step. Samples with no temperature are dropped rather
/// than zero-filled — a missing reading is not 0 °C.
pub fn steady_state_for(session: &ValidationSession) -> Option<crate::api::stats::SteadyState> {
    let series: Vec<crate::api::stats::TempSample> = session
        .samples
        .iter()
        .filter_map(|s| {
            s.temperature_c.map(|t| crate::api::stats::TempSample {
                at_ms: s.elapsed_ms,
                temp_c: t,
            })
        })
        .collect();
    if series.is_empty() {
        return None;
    }
    Some(crate::api::stats::steady_state(&series))
}

/// §3 as a finding. **Never `fail`.**
///
/// `not_established` maps to `not_observed`, not to a failure: a run stopped
/// early and a loop that genuinely never settles are indistinguishable from the
/// data, and the Overview's vocabulary forbids turning either into a verdict
/// about the hardware.
fn steady_state_finding(session: &ValidationSession) -> ValidationFinding {
    use crate::api::stats::{
        STEADY_STATE_DETECTED, STEADY_STATE_INSUFFICIENT, STEADY_STATE_NOT_ESTABLISHED,
    };
    let Some(ss) = &session.steady_state else {
        return with_detail(
            finding(F_STEADY_STATE, RESULT_NOT_TESTED),
            "no control temperature was recorded".to_string(),
        );
    };
    let state = match ss.verdict.as_str() {
        STEADY_STATE_DETECTED => RESULT_OBSERVED,
        STEADY_STATE_NOT_ESTABLISHED => RESULT_NOT_OBSERVED,
        STEADY_STATE_INSUFFICIENT => RESULT_NOT_TESTED,
        // An unknown token must not silently become a pass.
        _ => RESULT_UNKNOWN,
    };
    // The criterion travels with the verdict, which §3 requires: a reader must
    // be able to see what rule produced it without consulting the source.
    let detail = match (ss.mean_c, ss.slope_c_per_min) {
        (Some(mean), Some(slope)) if ss.verdict == STEADY_STATE_DETECTED => format!(
            "mean {mean:.1} °C, trend {slope:+.2} °C/min ({}); criterion: {}",
            ss.confidence, ss.criterion
        ),
        _ => format!("criterion: {}", ss.criterion),
    };
    with_detail(finding(F_STEADY_STATE, state), detail)
}

/// §1's per-member startup fingerprint.
///
/// Pure over the recorded samples. Returns an entry only for members whose tach
/// gave enough readings to say anything — an empty vector is the honest result
/// for a board with no usable tach, and is not a row of zeroes.
pub fn startup_fingerprints(session: &ValidationSession) -> Vec<StartupFingerprint> {
    session
        .metadata
        .members
        .iter()
        .filter_map(|m| fingerprint_for(session, &m.member_id, &m.member_kind))
        .collect()
}

fn fingerprint_for(
    session: &ValidationSession,
    member_id: &str,
    role: &str,
) -> Option<StartupFingerprint> {
    // (elapsed_ms, rpm, requested, readback) for every tick this member reported
    // a tach reading. An unreadable tach is skipped, never treated as 0 RPM.
    let series: Vec<(u64, u16, Option<u8>, Option<u8>)> = session
        .samples
        .iter()
        .filter_map(|s| {
            let m = s.members.iter().find(|m| m.member_id == member_id)?;
            Some((s.elapsed_ms, m.rpm?, m.requested_pct, m.readback_pct))
        })
        .collect();
    if series.len() < constants::STARTUP_MIN_SAMPLES {
        return None;
    }

    // Where it ended up: the median of the final third, which is robust to a
    // single late spike in a way a mean or a last-sample read is not.
    let tail_start = series.len() * 2 / 3;
    let mut tail: Vec<u16> = series[tail_start..].iter().map(|s| s.1).collect();
    tail.sort_unstable();
    let settled = tail[tail.len() / 2];

    let base = StartupFingerprint {
        member_id: member_id.to_string(),
        role: role.to_string(),
        override_observed: false,
        override_duration_ms: None,
        peak_rpm: None,
        requested_pct_during: None,
        readback_pct_during: None,
        post_override_rpm: Some(settled),
        transition_ms: None,
        interpretation: STARTUP_NONE_OBSERVED.to_string(),
    };

    // A settled reading of zero gives no band to compare against — a ratio
    // around zero admits nothing, so no override can be established either way.
    if settled == 0 {
        return Some(base);
    }
    let band_high = f64::from(settled) * constants::STARTUP_OVERRIDE_RATIO;
    if f64::from(series[0].1) <= band_high {
        return Some(base);
    }

    // The override runs until the first reading back inside the band.
    let resolved_at = series.iter().position(|s| f64::from(s.1) <= band_high);
    let end_idx = resolved_at.unwrap_or(series.len());
    let during = &series[..end_idx];
    let (peak_idx, peak) = during
        .iter()
        .enumerate()
        .max_by_key(|(_, s)| s.1)
        .map(|(i, s)| (i, s.1))?;

    let (requested, readback) = (during[peak_idx].2, during[peak_idx].3);
    // The §1 rule: a high startup RPM is a device behaviour, not a control
    // fault, *when the command was honoured*. A mismatch is only claimed on
    // positive evidence of disagreement — an unreadable readback leaves the
    // benign reading standing, because lack of evidence must not become a fault.
    let interpretation = match (requested, readback) {
        (Some(req), Some(rb)) if req.abs_diff(rb) > constants::STARTUP_DUTY_MATCH_TOLERANCE_PCT => {
            STARTUP_COMMAND_MISMATCH
        }
        _ if resolved_at.is_none() => STARTUP_UNRESOLVED,
        _ => STARTUP_DEVICE_OVERRIDE,
    };

    Some(StartupFingerprint {
        override_observed: true,
        override_duration_ms: resolved_at.map(|i| series[i].0),
        peak_rpm: Some(peak),
        requested_pct_during: requested,
        readback_pct_during: readback,
        transition_ms: resolved_at.map(|i| series[i].0.saturating_sub(during[peak_idx].0)),
        interpretation: interpretation.to_string(),
        ..base
    })
}

/// Startup/lifecycle behaviour — §9 is explicit that a temporary high-RPM period
/// must be representable **without** being treated as a failure, so this is
/// always observational.
fn startup_behaviour(session: &ValidationSession) -> ValidationFinding {
    const LIFECYCLE: &[&str] = &[
        EV_PROFILE_ACTIVATED,
        EV_CONTROL_RECLAIMED,
        EV_CONTROL_RESTORED,
        EV_RESUME,
        EV_SUSPEND,
        EV_DAEMON_RESTART,
    ];
    let seen: Vec<&str> = session
        .events
        .iter()
        .filter(|e| LIFECYCLE.contains(&e.kind.as_str()))
        .map(|e| e.kind.as_str())
        .collect();
    // DEC-335 (§1): the fingerprint is evidence in its own right. Before it,
    // this finding could only count events, so a device that overrode its fans
    // for fifty seconds at power-on produced `not_observed` unless some
    // unrelated lifecycle event happened to coincide.
    let overrides: Vec<&StartupFingerprint> = session
        .startup_fingerprints
        .iter()
        .filter(|f| f.override_observed)
        .collect();

    if seen.is_empty() && overrides.is_empty() {
        return finding(F_STARTUP_BEHAVIOUR, RESULT_NOT_OBSERVED);
    }

    let mut parts: Vec<String> = Vec::new();
    if !seen.is_empty() {
        parts.push(format!("{} lifecycle event(s)", seen.len()));
    }
    for f in overrides {
        // Phrased as a device behaviour, with the command evidence attached.
        // §1 is explicit that a temporary high-RPM period must be
        // representable without being treated as a control failure, so the
        // duty and its readback are stated alongside the RPM rather than the
        // RPM alone — the two agreeing is exactly what rules the fault out.
        let duty = match (f.requested_pct_during, f.readback_pct_during) {
            (Some(req), Some(rb)) => format!(", duty {req}% / readback {rb}%"),
            (Some(req), None) => format!(", duty {req}%"),
            _ => String::new(),
        };
        let duration = f
            .override_duration_ms
            .map(|ms| format!(" for ~{} s", ms / 1000))
            .unwrap_or_else(|| " (unresolved within the recording)".to_string());
        parts.push(format!(
            "{}: startup override{}, peak {} RPM{} → settled {} RPM [{}]",
            f.member_id,
            duration,
            f.peak_rpm.map_or_else(|| "?".into(), |r| r.to_string()),
            duty,
            f.post_override_rpm
                .map_or_else(|| "?".into(), |r| r.to_string()),
            f.interpretation,
        ));
    }

    with_detail(
        finding(F_STARTUP_BEHAVIOUR, RESULT_OBSERVED),
        parts.join("; "),
    )
}

/// Did RPM follow PWM? One finding per member whose duty actually moved.
///
/// **Requires a real duty swing.** §10's signature is RPM failing to follow a
/// change; with a steady duty the question is unanswerable, and answering it
/// anyway would classify a pump idling correctly at its floor as divergent.
fn divergence(session: &ValidationSession) -> Vec<ValidationFinding> {
    let mut out = Vec::new();
    for member in &session.metadata.members {
        if member.member_kind == MEMBER_AUXILIARY {
            continue;
        }
        let id = &member.member_id;
        let mut readbacks: Vec<u8> = Vec::new();
        let mut rpms: Vec<u16> = Vec::new();
        for s in &session.samples {
            for m in &s.members {
                if &m.member_id != id {
                    continue;
                }
                if let Some(r) = m.readback_pct {
                    readbacks.push(r);
                }
                if let Some(r) = m.rpm {
                    rpms.push(r);
                }
            }
        }
        let mut f = if readbacks.is_empty() || rpms.is_empty() {
            with_detail(
                finding(F_PWM_RPM_DIVERGENCE, RESULT_UNAVAILABLE),
                "PWM readback or RPM not exposed",
            )
        } else {
            let pwm_swing = max_of(&readbacks) - min_of(&readbacks);
            let rpm_swing = max_of(&rpms) - min_of(&rpms);
            if pwm_swing < constants::VALIDATION_DIVERGENCE_MIN_PWM_SWING_PCT {
                // The duty never moved enough to test whether RPM follows it.
                with_detail(
                    finding(F_PWM_RPM_DIVERGENCE, RESULT_NOT_TESTED),
                    format!("PWM varied by only {pwm_swing}%"),
                )
            } else if rpm_swing < constants::VALIDATION_DIVERGENCE_MAX_RPM_SWING {
                with_detail(
                    finding(F_PWM_RPM_DIVERGENCE, RESULT_OBSERVED),
                    format!("PWM varied {pwm_swing}% but RPM only {rpm_swing}"),
                )
            } else {
                with_detail(
                    finding(F_PWM_RPM_DIVERGENCE, RESULT_NOT_OBSERVED),
                    format!("RPM varied {rpm_swing} across {pwm_swing}% PWM"),
                )
            }
        };
        f.member_id = Some(id.clone());
        out.push(f);
    }
    if out.is_empty() {
        out.push(finding(F_PWM_RPM_DIVERGENCE, RESULT_NOT_TESTED));
    }
    out
}

/// §10's classification, **preserved from Phase 3 and never recomputed**.
fn device_override(session: &ValidationSession, interrupted: bool) -> ValidationFinding {
    for ev in characterizations(session) {
        if let Some(run) = &ev.characterization {
            if let Some(sum) = &run.summary {
                // Cautious semantics (§10): a possible device-side control is
                // `observed` evidence, NEVER a failure. Misclassifying working
                // motherboard PWM control as failed is the specific outcome §10
                // exists to prevent.
                let state = if sum.possible_device_override {
                    RESULT_OBSERVED
                } else {
                    RESULT_NOT_OBSERVED
                };
                let mut f = with_detail(
                    finding(F_DEVICE_OVERRIDE, state),
                    if sum.possible_device_override {
                        "PWM control/readback valid, physical response unexpected"
                    } else {
                        "physical response tracked PWM"
                    },
                );
                f.member_id = Some(ev.member_id.clone());
                f.evidence_kind = Some(ev.kind.clone());
                return f;
            }
        }
    }
    finding(F_DEVICE_OVERRIDE, absent_state(interrupted))
}

/// Did anything take a header back off the daemon?
fn bios_reclaim(session: &ValidationSession) -> ValidationFinding {
    if session
        .events
        .iter()
        .any(|e| e.kind == EV_CONTROL_RECLAIMED)
    {
        return with_detail(
            finding(F_BIOS_RECLAIM, RESULT_OBSERVED),
            "pwm_enable reverted during the session",
        );
    }
    // A sweep aborts on reclaim and says so; that is evidence too.
    for ev in characterizations(session) {
        if let Some(run) = &ev.characterization {
            if let Some(sum) = &run.summary {
                if sum.interference_detected {
                    let mut f = with_detail(
                        finding(F_BIOS_RECLAIM, RESULT_OBSERVED),
                        "interference detected during characterisation",
                    );
                    f.evidence_kind = Some(ev.kind.clone());
                    return f;
                }
            }
        }
    }
    let any_enable = session
        .samples
        .iter()
        .any(|s| s.members.iter().any(|m| m.pwm_enable_mode.is_some()));
    if session.samples.is_empty() || !any_enable {
        return with_detail(
            finding(F_BIOS_RECLAIM, RESULT_UNAVAILABLE),
            "pwm_enable not exposed",
        );
    }
    finding(F_BIOS_RECLAIM, RESULT_NOT_OBSERVED)
}

/// Was thermal safety actually armed for the duration?
fn thermal_safety(session: &ValidationSession) -> ValidationFinding {
    if session.samples.is_empty() {
        return finding(F_THERMAL_SAFETY, RESULT_NOT_TESTED);
    }
    let degraded = session
        .samples
        .iter()
        .any(|s| s.thermal_state == "no_sensor_fallback");
    let fired = session.events.iter().any(|e| e.kind == EV_THERMAL_ENTERED);
    if degraded {
        // The ladder fell back to a fixed floor because no CPU sensor was
        // readable. Safety was still enforced, but not from a real measurement.
        return with_detail(
            finding(F_THERMAL_SAFETY, RESULT_FAIL),
            "no CPU sensor — thermal ladder ran on its fallback floor",
        );
    }
    if fired {
        return with_detail(
            finding(F_THERMAL_SAFETY, RESULT_OBSERVED),
            "thermal failsafe engaged during the session",
        );
    }
    with_detail(
        finding(F_THERMAL_SAFETY, RESULT_PASS),
        "armed on a live CPU sensor throughout",
    )
}

fn control_restoration(session: &ValidationSession) -> ValidationFinding {
    let reclaimed = session
        .events
        .iter()
        .filter(|e| e.kind == EV_CONTROL_RECLAIMED)
        .count();
    let restored = session
        .events
        .iter()
        .filter(|e| e.kind == EV_CONTROL_RESTORED)
        .count();
    if reclaimed == 0 {
        // Nothing took control away, so restoration was never exercised.
        return finding(F_CONTROL_RESTORATION, RESULT_NOT_TESTED);
    }
    if restored >= reclaimed {
        with_detail(
            finding(F_CONTROL_RESTORATION, RESULT_PASS),
            format!("{restored} restore(s) after {reclaimed} reclaim(s)"),
        )
    } else {
        with_detail(
            finding(F_CONTROL_RESTORATION, RESULT_FAIL),
            format!("only {restored} restore(s) after {reclaimed} reclaim(s)"),
        )
    }
}

fn coolant_telemetry(session: &ValidationSession) -> ValidationFinding {
    // §1: coolant telemetry is NOT required. Its absence is `unavailable` and
    // must never read as a failure of the cooler or the session.
    if session.metadata.coolant_sensor.is_none() {
        return with_detail(
            finding(F_COOLANT_TELEMETRY, RESULT_UNAVAILABLE),
            "no coolant sensor on this device",
        );
    }
    if session.samples.iter().any(|s| s.coolant_c.is_some()) {
        finding(F_COOLANT_TELEMETRY, RESULT_PASS)
    } else if session.samples.is_empty() {
        finding(F_COOLANT_TELEMETRY, RESULT_NOT_TESTED)
    } else {
        with_detail(
            finding(F_COOLANT_TELEMETRY, RESULT_UNAVAILABLE),
            "coolant sensor configured but never read",
        )
    }
}

fn daemon_restart_recovery(session: &ValidationSession) -> ValidationFinding {
    if session.events.iter().any(|e| e.kind == EV_DAEMON_RESTART) {
        return with_detail(
            finding(F_DAEMON_RESTART_RECOVERY, RESULT_OBSERVED),
            "session survived a daemon restart",
        );
    }
    // §8's own worked example ends with exactly this line.
    finding(F_DAEMON_RESTART_RECOVERY, RESULT_NOT_TESTED)
}

// ── Small numeric helpers (no external crate, no panics on empty) ───────────

fn min_of<T: Copy + Ord + Default>(v: &[T]) -> T {
    v.iter().copied().min().unwrap_or_default()
}

fn max_of<T: Copy + Ord + Default>(v: &[T]) -> T {
    v.iter().copied().max().unwrap_or_default()
}
