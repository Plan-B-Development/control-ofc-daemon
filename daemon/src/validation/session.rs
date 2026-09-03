//! Validation-session data model (AIO-MB Phase 5).
//!
//! Pure data plus a handful of total constructors. Nothing here reads hardware,
//! takes a lock, or performs I/O — the recorder samples, the store persists, the
//! summariser derives, and this module only says what those three exchange.
//!
//! Every enum-ish value on the wire is a **stable lowercase token in a `String`**,
//! not a Rust enum. That is the house convention (`CharacterizationRun::state`,
//! `SkippedControl::reason`, `HeaderRole`): the client owns the wording, and an
//! unrecognised token must be rendered rather than dropped, so a newer daemon can
//! add one without a client update turning it into a blank cell.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Session lifecycle states (§2) ───────────────────────────────────────────

/// No session has ever run in this process.
pub const STATE_IDLE: &str = "idle";
/// Sampling now.
pub const STATE_RECORDING: &str = "recording";
/// Finalised normally, summary computed.
pub const STATE_COMPLETED: &str = "completed";
/// Ended by the user before finalisation.
pub const STATE_CANCELLED: &str = "cancelled";
/// Recording stopped without finalising and cannot be resumed — canonically a
/// daemon restart. **Never fabricated telemetry for the gap** (§15).
pub const STATE_INTERRUPTED: &str = "interrupted";
/// The engine itself failed.
pub const STATE_ERROR: &str = "error";

// ── Result semantics (§7) ───────────────────────────────────────────────────
//
// `UNAVAILABLE` must never become `FAIL`: hardware that does not expose a
// capability has not failed a test, it was never testable. Likewise absence of a
// diagnostic is `NOT_TESTED`, never `PASS`.

/// A meaningful criterion existed and was met.
pub const RESULT_PASS: &str = "pass";
/// A meaningful criterion existed and was not met.
pub const RESULT_FAIL: &str = "fail";
/// Happened, and is evidence rather than a verdict (§9's startup override).
pub const RESULT_OBSERVED: &str = "observed";
/// Watched for throughout the session and did not happen.
pub const RESULT_NOT_OBSERVED: &str = "not_observed";
/// The diagnostic that would decide this was not run.
pub const RESULT_NOT_TESTED: &str = "not_tested";
/// Ran, but the evidence does not settle the question.
pub const RESULT_UNKNOWN: &str = "unknown";
/// The hardware does not expose what this would need. **Not a failure.**
pub const RESULT_UNAVAILABLE: &str = "unavailable";
/// The session ended before this could be decided.
pub const RESULT_INTERRUPTED: &str = "interrupted";

// ── Session kinds (§8 / Phase 6 §11) ────────────────────────────────────────

/// A full AIO validation run.
pub const KIND_VALIDATION: &str = "validation";
/// A startup/lifecycle recording — same engine, same wire type, different
/// findings and a different Phase 6 preset. Two engines would be the duplication
/// §17 forbids.
pub const KIND_LIFECYCLE: &str = "lifecycle";

// ── Event markers (§5) ──────────────────────────────────────────────────────
//
// Only events Control-OFC can genuinely observe. A cold boot and a physical
// switch position are NOT here, deliberately — §5 forbids claiming them.

pub const EV_SESSION_STARTED: &str = "session_started";
pub const EV_SESSION_STOPPED: &str = "session_stopped";
pub const EV_PROFILE_ACTIVATED: &str = "profile_activated";
pub const EV_OVERRIDE_STARTED: &str = "manual_override_started";
pub const EV_OVERRIDE_ENDED: &str = "manual_override_ended";
pub const EV_THERMAL_ENTERED: &str = "thermal_failsafe_entered";
pub const EV_THERMAL_CLEARED: &str = "thermal_failsafe_cleared";
pub const EV_CONTROL_RECLAIMED: &str = "control_reclaimed";
pub const EV_CONTROL_RESTORED: &str = "control_restored";
pub const EV_SUSPEND: &str = "suspend";
pub const EV_RESUME: &str = "resume";
pub const EV_DAEMON_RESTART: &str = "daemon_restart_observed";
pub const EV_CHAR_STARTED: &str = "characterization_started";
pub const EV_CHAR_COMPLETED: &str = "characterization_completed";
pub const EV_VERIFY_STARTED: &str = "verify_started";
pub const EV_VERIFY_COMPLETED: &str = "verify_completed";
pub const EV_USER_MARKER: &str = "user_marker";
pub const EV_SAMPLE_LIMIT: &str = "sample_limit_reached";

// ── Orchestratable diagnostics (§6, scope Decision 3) ───────────────────────

/// The Phase 3 PWM/RPM sweep. Reused verbatim — never reimplemented (§6).
pub const DIAG_CHARACTERIZATION: &str = "pwm_characterization";
/// The short write/readback verification.
pub const DIAG_VERIFY: &str = "pwm_verify";

/// Is this a diagnostic the session knows how to run?
pub fn is_known_diagnostic(token: &str) -> bool {
    matches!(token, DIAG_CHARACTERIZATION | DIAG_VERIFY)
}

// ── Member roles within a session ───────────────────────────────────────────

pub const MEMBER_PUMP: &str = "pump";
pub const MEMBER_RADIATOR: &str = "radiator";
pub const MEMBER_AUXILIARY: &str = "auxiliary";

// ── Static session-start metadata (§1, §4) ──────────────────────────────────

/// A member's role and safety posture, snapshotted once at session start.
///
/// `pump_protected` is the daemon's **union** predicate, not the display role
/// (DEC-312): a header the user labelled `chassis_fan` that the hardware calls
/// `PUMP` is still protected, and evidence that said otherwise would be a lie
/// about what the daemon will refuse to do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemberRoleSnapshot {
    pub member_id: String,
    pub label: String,
    pub role: String,
    pub member_kind: String,
    pub pump_protected: bool,
    #[serde(default)]
    pub effective_min_pwm_pct: Option<u8>,
    #[serde(default)]
    pub stop_permitted: Option<bool>,
    #[serde(default)]
    pub writable: bool,
}

/// The compiled-in device policy in force for this session.
///
/// A snapshot, not a reference: the evidence must still be readable after the
/// policy table changes in a later release. Carries `Deserialize` because it
/// round-trips through the store — unlike `hwmon::device_policy::DevicePolicy`,
/// which deliberately has none so that no payload can ever set a safety number.
/// Nothing reads this struct back into a control decision; it is evidence only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DevicePolicySnapshot {
    pub id: String,
    pub display_name: String,
    pub minimum_safe_pwm_pct: f64,
    pub supports_stop: bool,
    #[serde(default)]
    pub startup_override_seconds: Option<u32>,
    #[serde(default)]
    pub expected_rpm_min: Option<u16>,
    #[serde(default)]
    pub expected_rpm_max: Option<u16>,
    pub internal_control_possible: bool,
}

/// Everything about the session that is fixed at start (§4's static half).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMetadata {
    pub cooling_device_id: String,
    pub device_name: String,
    pub device_kind: String,
    #[serde(default)]
    pub pump_member: Option<String>,
    #[serde(default)]
    pub radiator_members: Vec<String>,
    #[serde(default)]
    pub auxiliary_members: Vec<String>,
    #[serde(default)]
    pub temperature_sensor: Option<String>,
    #[serde(default)]
    pub coolant_sensor: Option<String>,
    /// `"available"` / `"unavailable"` — §1 is explicit that coolant telemetry is
    /// NOT required, and a motherboard-PWM AIO on CPU temperature is a valid target.
    pub coolant_telemetry: String,
    pub device_policy: DevicePolicySnapshot,
    #[serde(default)]
    pub members: Vec<MemberRoleSnapshot>,
    #[serde(default)]
    pub active_profile_id: Option<String>,
    #[serde(default)]
    pub active_profile_name: Option<String>,
    pub daemon_version: String,
    /// Free-form user/test metadata (§11) — physical pump mode, workload name,
    /// ambient notes. **Metadata only**: it never reaches a safety decision, and
    /// the daemon does not claim to have detected any of it electronically.
    #[serde(default)]
    pub user_metadata: BTreeMap<String, String>,
}

// ── Sampled dynamic data (§3, §4's dynamic half) ────────────────────────────

/// One member's telemetry at one instant.
///
/// `requested_pct` and `readback_pct` are deliberately separate — §3 lists them
/// as separate columns and §10 classifies a device-side override from
/// `command low + readback low + RPM high`. `None` on any field means "not
/// known", never zero (§3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemberSample {
    pub member_id: String,
    /// `pump` / `radiator` / `auxiliary` — identity is preserved per member and
    /// never flattened into an invented aggregate (§3).
    pub role: String,
    #[serde(default)]
    pub requested_pct: Option<u8>,
    #[serde(default)]
    pub readback_pct: Option<u8>,
    #[serde(default)]
    pub rpm: Option<u16>,
    #[serde(default)]
    pub pwm_enable_mode: Option<u8>,
    #[serde(default)]
    pub alarm: Option<bool>,
    /// Cumulative BIOS/EC reclaim count for this header. Diffed between samples
    /// to place a `control_reclaimed` marker; absolute value is evidence too.
    #[serde(default)]
    pub enable_revert_count: u64,
    /// `daemon` when the daemon holds it in manual mode, `external` when
    /// something else does, `unknown` when the driver does not say.
    pub ownership: String,
}

pub const OWNERSHIP_DAEMON: &str = "daemon";
pub const OWNERSHIP_EXTERNAL: &str = "external";
pub const OWNERSHIP_UNKNOWN: &str = "unknown";

/// One sampling tick.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationSample {
    pub elapsed_ms: u64,
    pub unix_ms: u64,
    #[serde(default)]
    pub temperature_c: Option<f64>,
    #[serde(default)]
    pub temperature_sensor: Option<String>,
    #[serde(default)]
    pub coolant_c: Option<f64>,
    /// `normal` / `recovery` / `emergency` / `no_sensor_fallback`.
    pub thermal_state: String,
    pub members: Vec<MemberSample>,
}

/// Worst-case serialised bytes for one sample of `session`.
///
/// Built to be a genuine upper bound rather than a typical case: every optional
/// field is present, every integer sits at its widest decimal width, and the
/// longest role and ownership tokens are used.
///
/// **Every variable-length field is taken from the session itself, never
/// assumed.** An earlier version used a 128-byte placeholder for
/// `temperature_sensor` on the reasoning that no real sensor id is longer — but
/// `recorder.rs` copies `metadata.temperature_sensor` into *every* sample, and
/// that string is `preferred_sensor`/`fallback_sensor` from
/// `POST /config/cooling-device`, which is client-supplied. A long one made the
/// probe under-count without bound and reproduced `AUD3-i` exactly, inside its
/// own fix. Caught by `ofc:security-reviewer`. A guess is not a bound.
fn probe_sample_bytes(session: &ValidationSession) -> usize {
    let member_ids: Vec<String> = session
        .metadata
        .members
        .iter()
        .map(|m| m.member_id.clone())
        .collect();
    let probe = ValidationSample {
        elapsed_ms: u64::MAX,
        unix_ms: u64::MAX,
        temperature_c: Some(-100.5),
        // The session's own value, which is what every sample will carry.
        temperature_sensor: session.metadata.temperature_sensor.clone(),
        coolant_c: Some(-100.5),
        // The longest of the four thermal states, so the probe over-estimates.
        thermal_state: "no_sensor_fallback".to_string(),
        members: member_ids
            .iter()
            .map(|id| MemberSample {
                member_id: id.clone(),
                role: MEMBER_AUXILIARY.to_string(),
                requested_pct: Some(u8::MAX),
                readback_pct: Some(u8::MAX),
                rpm: Some(u16::MAX),
                pwm_enable_mode: Some(u8::MAX),
                alarm: Some(false),
                enable_revert_count: u64::MAX,
                ownership: OWNERSHIP_EXTERNAL.to_string(),
            })
            .collect(),
    };
    // Measured as the MARGINAL cost of one sample inside a `samples` field, not
    // as a standalone value. Two reasons, both of which would otherwise
    // under-count and so reintroduce the very defect this bounds:
    //   * `store::save_to` uses `to_vec_pretty`, and pretty-printing is ~45% of
    //     the cost, so a compact estimate is not the thing being bounded;
    //   * a sample nested under `samples` sits two levels deep, so every one of
    //     its ~40 lines carries four more spaces than it would standalone.
    // Taking the difference between a one-element and an empty `samples` field
    // captures the indentation, the separator and the brackets exactly, with no
    // fudge factor to drift.
    #[derive(serde::Serialize)]
    struct SamplesField<'a> {
        samples: &'a [ValidationSample],
    }
    let empty = serde_json::to_vec_pretty(&SamplesField { samples: &[] })
        .map(|v| v.len())
        .unwrap_or(0);
    let one = serde_json::to_vec_pretty(&SamplesField {
        samples: std::slice::from_ref(&probe),
    })
    .map(|v| v.len())
    // A serialisation failure must shrink the cap, never remove it.
    .unwrap_or(usize::MAX / 2);
    one.saturating_sub(empty)
}

/// How many samples `session` may hold within `budget_bytes`.
///
/// **This is what bounds the persisted document (`AUD3-i`).**
/// [`crate::constants::VALIDATION_MAX_SAMPLES`] bounds the sample *count*, which
/// is not the same thing: a sample carries one entry per cooling-device member,
/// so the byte count scales with the topology and a three-member cooler reached
/// 7.8 MiB against a 4 MiB read cap. Dividing a byte budget by the measured
/// worst-case sample cost bounds the file directly, whatever the topology.
///
/// Never returns 0 — a pathological topology records one sample rather than
/// finalising before its first tick — and never exceeds the hard sample cap, so
/// the documented two-hour ceiling still holds for every realistic device.
pub fn max_samples_for(session: &ValidationSession, budget_bytes: usize) -> usize {
    let per = probe_sample_bytes(session).max(1);
    (budget_bytes / per).clamp(1, crate::constants::VALIDATION_MAX_SAMPLES)
}

/// A point on the session timeline (§5). Phase 6 places these as chart markers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationEvent {
    pub elapsed_ms: u64,
    pub unix_ms: u64,
    pub kind: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub member_id: Option<String>,
}

// ── Referenced diagnostics (§6) ─────────────────────────────────────────────

/// The outcome of one orchestrated diagnostic, carried by reference.
///
/// §6: a session is an orchestrator/evidence collector, **not** a second copy of
/// each diagnostic algorithm. The Phase 3 run is attached verbatim, so
/// `possible_device_override` and every verdict on it are the ones Phase 3
/// computed — this module recalculates none of them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidenceRef {
    pub kind: String,
    pub member_id: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub started_unix_ms: u64,
    #[serde(default)]
    pub completed_unix_ms: Option<u64>,
    /// A result token — how the *orchestration* went, not a verdict on the
    /// hardware. A diagnostic the slot refused is `unavailable`, and that never
    /// becomes a `fail` (§7).
    pub outcome: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// The Phase 3 run, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub characterization: Option<crate::api::characterization::CharacterizationRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyEvidence>,
}

/// The result of a PWM write/readback verification, flattened for evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyEvidence {
    pub header_id: String,
    pub write_ok: bool,
    #[serde(default)]
    pub readback_pct: Option<u8>,
    #[serde(default)]
    pub requested_pct: Option<u8>,
    #[serde(default)]
    pub rpm_before: Option<u16>,
    #[serde(default)]
    pub rpm_after: Option<u16>,
    #[serde(default)]
    pub detail: Option<String>,
}

// ── External measurements (§14) ─────────────────────────────────────────────

/// An externally measured observation — a meter reading, typed in by a person.
///
/// **Explicitly untrusted and read by nothing.** §14: these are not daemon
/// control/safety inputs. The daemon stores and returns them; no code path
/// consults one, and none may be added.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExternalMeasurement {
    pub unix_ms: u64,
    pub kind: String,
    pub value: f64,
    pub unit: String,
    #[serde(default)]
    pub member_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

// ── Summary (§8) ────────────────────────────────────────────────────────────

/// One line of the compatibility/evidence summary.
///
/// `id` is a stable token and the **client owns the wording** (§8, §16): Phase 6
/// renders "PWM header control" from `pwm_header_control` and recalculates no
/// backend meaning. An unrecognised id must be rendered, not dropped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationFinding {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub member_id: Option<String>,
    #[serde(default)]
    pub evidence_kind: Option<String>,
}

// Finding ids — §8's list, one token each.
pub const F_PWM_HEADER_CONTROL: &str = "pwm_header_control";
pub const F_PWM_READBACK: &str = "pwm_readback";
pub const F_PUMP_RPM: &str = "pump_rpm_telemetry";
pub const F_RADIATOR_RPM: &str = "radiator_rpm_telemetry";
pub const F_PWM_RESPONSE: &str = "pwm_response_characterization";
pub const F_RESPONSE_LATENCY: &str = "response_latency";
pub const F_STARTUP_BEHAVIOUR: &str = "startup_lifecycle_behaviour";
pub const F_PWM_RPM_DIVERGENCE: &str = "pwm_rpm_divergence";
pub const F_DEVICE_OVERRIDE: &str = "possible_device_override";
pub const F_BIOS_RECLAIM: &str = "bios_ec_control_reclaim";
pub const F_THERMAL_SAFETY: &str = "thermal_safety";
pub const F_CONTROL_RESTORATION: &str = "control_restoration";
pub const F_COOLANT_TELEMETRY: &str = "coolant_telemetry";
pub const F_DAEMON_RESTART_RECOVERY: &str = "daemon_restart_recovery";

// ── The session itself ──────────────────────────────────────────────────────

/// A complete validation session — static metadata, sampled telemetry, the event
/// timeline, referenced diagnostics, and the derived summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationSession {
    pub session_id: String,
    pub kind: String,
    pub state: String,
    pub started_unix_ms: u64,
    #[serde(default)]
    pub completed_unix_ms: Option<u64>,
    pub metadata: SessionMetadata,
    /// Which diagnostics the caller asked for at start. Empty is legitimate —
    /// a passive recording session — and yields `not_tested`, never `pass` (§7).
    #[serde(default)]
    pub requested_diagnostics: Vec<String>,
    /// Which members those diagnostics sweep. Defaults to the pump member.
    #[serde(default)]
    pub sweep_members: Vec<String>,
    #[serde(default)]
    pub samples: Vec<ValidationSample>,
    #[serde(default)]
    pub events: Vec<ValidationEvent>,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default)]
    pub external_measurements: Vec<ExternalMeasurement>,
    #[serde(default)]
    pub findings: Vec<ValidationFinding>,
    /// Recording stopped because the sample cap was reached, not because the
    /// user stopped it. Cap-and-stop, never a ring: evicting the oldest samples
    /// would discard the startup evidence §9 exists to capture.
    #[serde(default)]
    pub sample_limit_reached: bool,
    #[serde(default)]
    pub interrupted_reason: Option<String>,
    /// The last sample actually recorded before an interruption. Telemetry is
    /// **never fabricated** for the gap after it (§15).
    #[serde(default)]
    pub truncated_at_unix_ms: Option<u64>,
}

impl ValidationSession {
    /// Is this session still sampling?
    pub fn is_recording(&self) -> bool {
        self.state == STATE_RECORDING
    }

    /// Has it reached a state from which it will not sample again?
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state.as_str(),
            STATE_COMPLETED | STATE_CANCELLED | STATE_INTERRUPTED | STATE_ERROR
        )
    }

    /// Wall-clock milliseconds since start, from the last sample or event.
    pub fn elapsed_ms(&self) -> u64 {
        let last_sample = self.samples.last().map(|s| s.elapsed_ms).unwrap_or(0);
        let last_event = self.events.last().map(|e| e.elapsed_ms).unwrap_or(0);
        last_sample.max(last_event)
    }
}

/// Milliseconds since the unix epoch. Saturates rather than panicking on a clock
/// before 1970 — the same posture as `HistoryRing::record`.
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Session ids are `val-<unix_ms>-<seq>`: sortable, unique within a process, and
/// safe as a filename component without escaping.
pub fn next_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    format!("val-{}-{}", unix_ms(), n)
}

/// Is this string safe to use as a session-file basename?
///
/// The store never interpolates a caller-supplied id into a path without this —
/// `..` or a `/` would escape the session directory. Mirrors
/// `profile_store::is_safe_profile_id`.
pub fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
