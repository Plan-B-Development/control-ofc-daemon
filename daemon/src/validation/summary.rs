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
    out.push(startup_behaviour(session));
    out.extend(divergence(session));
    out.push(device_override(session, interrupted));
    out.push(bios_reclaim(session));
    out.push(thermal_safety(session));
    out.push(control_restoration(session));
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

fn characterizations(session: &ValidationSession) -> impl Iterator<Item = &EvidenceRef> {
    session
        .evidence
        .iter()
        .filter(|e| e.kind == DIAG_CHARACTERIZATION)
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
                f.evidence_kind = Some(DIAG_CHARACTERIZATION.to_string());
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
                f.evidence_kind = Some(DIAG_CHARACTERIZATION.to_string());
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
                f.evidence_kind = Some(DIAG_CHARACTERIZATION.to_string());
                return f;
            }
        }
    }
    finding(F_PWM_RESPONSE, absent_state(interrupted))
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
            let mut f = with_detail(
                finding(F_RESPONSE_LATENCY, RESULT_OBSERVED),
                format!(
                    "first RPM change {min}–{max} ms across {} points",
                    latencies.len()
                ),
            );
            f.member_id = Some(ev.member_id.clone());
            f.evidence_kind = Some(DIAG_CHARACTERIZATION.to_string());
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
    if seen.is_empty() {
        return finding(F_STARTUP_BEHAVIOUR, RESULT_NOT_OBSERVED);
    }
    with_detail(
        finding(F_STARTUP_BEHAVIOUR, RESULT_OBSERVED),
        format!("{} lifecycle event(s)", seen.len()),
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
                f.evidence_kind = Some(DIAG_CHARACTERIZATION.to_string());
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
                    f.evidence_kind = Some(DIAG_CHARACTERIZATION.to_string());
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
