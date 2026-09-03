//! Validation-session endpoints (AIO-MB Phase 5, §2, §13, §14, §16).
//!
//! # The orchestration rule, stated once
//!
//! Where a session runs a diagnostic, it **calls the existing handler** —
//! [`super::hwmon_ctl::hwmon_verify_handler`] and
//! [`super::hwmon_ctl::hwmon_characterize_handler`] — as a function. That is not
//! a shortcut; it is the whole point of §6. Those paths already take the hwmon
//! lease, clamp to the pump floor, refuse while the thermal ladder is forcing,
//! renew the engine-pause deadman, and restore the header on drop. Reimplementing
//! any of it here would create the "second copy of each diagnostic algorithm" §6
//! forbids, and a second PWM ownership path §2 forbids.
//!
//! So this module sequences and collects. It contains no PWM write, no lease
//! acquisition, and no floor arithmetic — deliberately, and that absence is
//! load-bearing.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::responses::ErrorEnvelope;
use crate::constants;
use crate::validation::recorder::{RecorderContext, StartError};
use crate::validation::session::*;
use crate::validation::store;

// ── Request bodies ──────────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Deserialize)]
pub struct StartSessionRequest {
    pub cooling_device_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    /// Diagnostics to run. **Empty (or absent) is legitimate** — a passive
    /// recording session — and yields `not_tested`, never `pass` (§7).
    #[serde(default)]
    pub diagnostics: Vec<String>,
    /// Members those diagnostics sweep. Absent defaults to the pump member.
    #[serde(default)]
    pub sweep_members: Vec<String>,
    /// Free-form user/test metadata (§11). Metadata only — it never reaches a
    /// safety decision.
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct EventRequest {
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub member_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct MeasurementRequest {
    pub kind: String,
    pub value: f64,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub member_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Reject a client-supplied free-text field longer than
/// [`constants::VALIDATION_MAX_TEXT_FIELD_BYTES`].
///
/// These fields are stored verbatim and are read by nothing — but the events and
/// measurements arrays are bounded only by COUNT, so unbounded text made the
/// session document unbounded too. Since DEC-320 an over-cap session is pruned,
/// which turned that from wasted disk into destroyed evidence. Bounding here is
/// what lets `SessionReadError::TooLarge` mean "written by an older daemon" and
/// therefore be safe to reclaim.
fn too_long(field: &str, value: Option<&String>) -> Option<(StatusCode, Json<serde_json::Value>)> {
    let over = value.is_some_and(|v| v.len() > constants::VALIDATION_MAX_TEXT_FIELD_BYTES);
    over.then(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!(
                "{field} exceeds {} bytes",
                constants::VALIDATION_MAX_TEXT_FIELD_BYTES
            )),
        )
    })
}

fn recorder_context(state: &Arc<AppState>) -> RecorderContext {
    RecorderContext {
        cache: state.cache.clone(),
        hwmon_controller: state.hwmon_controller.clone(),
        override_table: state.override_table.clone(),
        characterization: state.characterization.clone(),
    }
}

/// Build the static session-start metadata (§1, §4).
///
/// Everything here is a snapshot taken once: the topology, each member's role and
/// safety posture, the compiled-in device policy, and the active profile. §4
/// separates this from the sampled dynamic data, and the separation is what lets
/// evidence stay readable after the policy table or the profile changes.
fn build_metadata(
    state: &Arc<AppState>,
    device: &crate::hwmon::cooling_device::CoolingDeviceConfig,
    user_metadata: std::collections::BTreeMap<String, String>,
) -> SessionMetadata {
    let policy = device.resolved_policy();
    let headers: Vec<crate::hwmon::pwm_discovery::PwmHeaderDescriptor> = state
        .hwmon_controller
        .as_ref()
        .map(|c| c.lock().headers().into_iter().cloned().collect())
        .unwrap_or_default();

    let mut members = Vec::new();
    let add = |id: &str, kind: &str, members: &mut Vec<MemberRoleSnapshot>| {
        let header = headers.iter().find(|h| h.id == id);
        // [SAFETY] The union predicate, never the display role (DEC-312): a user
        // may assign `chassis_fan` to a header the hardware labels `PUMP`, and
        // recording that as unprotected would be evidence that contradicts what
        // the daemon will actually refuse to do.
        let pump_protected = state.header_is_pump_protected(id);
        let floor = crate::hwmon::device_policy::resolve_policy_floor(policy, pump_protected);
        members.push(MemberRoleSnapshot {
            member_id: id.to_string(),
            label: header
                .map(|h| h.label.clone())
                .unwrap_or_else(|| id.to_string()),
            role: state.resolved_header_role(id).as_str().to_string(),
            member_kind: kind.to_string(),
            pump_protected,
            effective_min_pwm_pct: Some(floor.round() as u8),
            stop_permitted: Some(crate::hwmon::device_policy::stop_permitted(
                policy,
                pump_protected,
            )),
            writable: header.map(|h| h.is_writable).unwrap_or(false),
        });
    };

    if let Some(pump) = &device.pump_member {
        add(pump, MEMBER_PUMP, &mut members);
    }
    for r in &device.radiator_members {
        add(r, MEMBER_RADIATOR, &mut members);
    }
    for a in &device.auxiliary_members {
        add(a, MEMBER_AUXILIARY, &mut members);
    }

    let (active_profile_id, active_profile_name) = {
        let guard = state.active_profile.lock();
        match guard.as_ref() {
            Some(p) => (Some(p.id.clone()), Some(p.name.clone())),
            None => (None, None),
        }
    };

    SessionMetadata {
        cooling_device_id: device.id.clone(),
        device_name: device.name.clone(),
        device_kind: device.resolved_kind().as_str().to_string(),
        pump_member: device.pump_member.clone(),
        radiator_members: device.radiator_members.clone(),
        auxiliary_members: device.auxiliary_members.clone(),
        // §1: coolant telemetry is NOT required — a motherboard-PWM AIO on CPU
        // temperature is a valid target, so the preferred sensor stands in.
        temperature_sensor: device
            .preferred_sensor
            .clone()
            .or_else(|| device.fallback_sensor.clone()),
        coolant_sensor: device.coolant_sensor.clone(),
        coolant_telemetry: device.coolant_telemetry().to_string(),
        device_policy: DevicePolicySnapshot {
            id: policy.id.to_string(),
            display_name: policy.display_name.to_string(),
            minimum_safe_pwm_pct: policy.minimum_safe_pwm,
            supports_stop: policy.supports_stop,
            startup_override_seconds: policy.startup_override_seconds,
            expected_rpm_min: policy.expected_rpm_min,
            expected_rpm_max: policy.expected_rpm_max,
            internal_control_possible: policy.internal_control_possible,
        },
        members,
        active_profile_id,
        active_profile_name,
        daemon_version: state.daemon_version.clone(),
        user_metadata,
    }
}

fn start_error_response(e: StartError) -> (StatusCode, Json<serde_json::Value>) {
    match e {
        StartError::AlreadyRecording => error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::already_exists("a validation session is already recording"),
        ),
        StartError::UnknownDevice(id) => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found(&format!("cooling device '{id}'")),
        ),
        StartError::NotAMember(id) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!(
                "'{id}' is not a member of the named cooling device"
            )),
        ),
        StartError::UnknownDiagnostic(d) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!("unknown diagnostic '{d}'")),
        ),
        StartError::TooMany(what) => {
            error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(&what))
        }
        StartError::Persistence(e) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::persistence_failed(&e),
        ),
    }
}

// ── Handlers ────────────────────────────────────────────────────────────────

/// Trim retained sessions, off the runtime.
///
/// Pruning fully parses every retained session to sort them, so it must not run
/// inline on a tokio worker. Called at the session-lifecycle boundaries rather
/// than from the engine, so the engine stays synchronous and runtime-agnostic —
/// and so a cap-finalise on the recorder's own task does not have to care.
async fn prune_sessions_off_runtime() {
    let _ = super::persist_off_runtime(|| {
        store::prune_default();
        Ok::<(), String>(())
    })
    .await;
}

/// `POST /validation/session`
pub async fn start_session_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<StartSessionRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let devices = state.cooling_devices();
    let Some(device) = devices.iter().find(|d| d.id == body.cooling_device_id) else {
        return start_error_response(StartError::UnknownDevice(body.cooling_device_id));
    };

    // Validate diagnostics.
    for d in &body.diagnostics {
        if !is_known_diagnostic(d) {
            return start_error_response(StartError::UnknownDiagnostic(d.clone()));
        }
    }

    // Resolve the sweep set. Absent means the pump member, which is what a
    // caller asking for a diagnostic without naming members almost always wants
    // — and never silently means "nothing", which would look like a diagnostic
    // that ran and found nothing.
    let mut sweep = body.sweep_members.clone();
    if sweep.is_empty() {
        if let Some(p) = &device.pump_member {
            sweep.push(p.clone());
        }
    }
    if sweep.len() > constants::VALIDATION_MAX_SWEEP_MEMBERS {
        return start_error_response(StartError::TooMany(format!(
            "at most {} sweep members",
            constants::VALIDATION_MAX_SWEEP_MEMBERS
        )));
    }
    let all_members = device.all_members();
    for m in &sweep {
        if !all_members.iter().any(|a| a == m) {
            return start_error_response(StartError::NotAMember(m.clone()));
        }
    }

    // Bound the user metadata (§11).
    if body.metadata.len() > constants::VALIDATION_MAX_METADATA_KEYS {
        return start_error_response(StartError::TooMany(format!(
            "at most {} metadata keys",
            constants::VALIDATION_MAX_METADATA_KEYS
        )));
    }
    if let Some((k, _)) = body
        .metadata
        .iter()
        .find(|(_, v)| v.len() > constants::VALIDATION_MAX_METADATA_VALUE_BYTES)
    {
        return start_error_response(StartError::TooMany(format!(
            "metadata value for '{k}' exceeds {} bytes",
            constants::VALIDATION_MAX_METADATA_VALUE_BYTES
        )));
    }
    // The KEY was unbounded while the value was not, so one 4 MiB key under the
    // body limit could still push the document past the store's read cap — and
    // since DEC-320 an over-cap session is *pruned*, so that would have been a
    // way to destroy an operator's evidence rather than merely to waste disk.
    if let Some((k, _)) = body
        .metadata
        .iter()
        .find(|(k, _)| k.len() > constants::VALIDATION_MAX_METADATA_KEY_BYTES)
    {
        return start_error_response(StartError::TooMany(format!(
            "metadata key '{}...' exceeds {} bytes",
            k.chars().take(16).collect::<String>(),
            constants::VALIDATION_MAX_METADATA_KEY_BYTES
        )));
    }

    let kind = match body.kind.as_deref() {
        Some(KIND_LIFECYCLE) => KIND_LIFECYCLE,
        _ => KIND_VALIDATION,
    };
    let metadata = build_metadata(&state, device, body.metadata);
    let session = ValidationSession {
        session_id: next_session_id(),
        kind: kind.to_string(),
        state: STATE_RECORDING.to_string(),
        started_unix_ms: unix_ms(),
        completed_unix_ms: None,
        metadata,
        requested_diagnostics: body.diagnostics.clone(),
        sweep_members: sweep.clone(),
        samples: Vec::new(),
        events: Vec::new(),
        evidence: Vec::new(),
        external_measurements: Vec::new(),
        findings: Vec::new(),
        sample_limit_reached: false,
        interrupted_reason: None,
        truncated_at_unix_ms: None,
    };

    let ctx = recorder_context(&state);
    match state.validation.start(session, &ctx) {
        Ok(started) => {
            log::info!(
                "Validation session {} started for device '{}' ({} diagnostic(s), {} sweep member(s))",
                started.session_id,
                started.metadata.cooling_device_id,
                started.requested_diagnostics.len(),
                started.sweep_members.len()
            );
            if !started.requested_diagnostics.is_empty() {
                // Carry each member's writability from the metadata snapshot, so
                // the orchestrator can record a non-writable header as
                // `unavailable` WITHOUT driving a diagnostic at it.
                let targets: Vec<(String, bool)> = sweep
                    .iter()
                    .map(|id| {
                        let writable = started
                            .metadata
                            .members
                            .iter()
                            .find(|m| &m.member_id == id)
                            .map(|m| m.writable)
                            .unwrap_or(false);
                        (id.clone(), writable)
                    })
                    .collect();
                spawn_orchestration(
                    state.clone(),
                    started.session_id.clone(),
                    started.requested_diagnostics.clone(),
                    targets,
                );
            }
            prune_sessions_off_runtime().await;
            json_ok(StatusCode::OK, started)
        }
        Err(e) => start_error_response(e),
    }
}

/// `GET /validation/session`
pub async fn get_session_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.validation.snapshot() {
        Some(s) => json_ok(StatusCode::OK, s),
        None => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no validation session has been started"),
        ),
    }
}

/// `POST /validation/session/stop`
pub async fn stop_session_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.validation.stop() {
        Some(s) => {
            prune_sessions_off_runtime().await;
            json_ok(StatusCode::OK, s)
        }
        None => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no validation session has been started"),
        ),
    }
}

/// `DELETE /validation/session`
pub async fn cancel_session_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.validation.cancel() {
        Some(s) => {
            prune_sessions_off_runtime().await;
            json_ok(StatusCode::OK, s)
        }
        None => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no validation session has been started"),
        ),
    }
}

/// `POST /validation/session/event` — a user marker (§5).
pub async fn post_event_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<EventRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    for (field, value) in [
        ("detail", body.detail.as_ref()),
        ("member_id", body.member_id.as_ref()),
    ] {
        if let Some(resp) = too_long(field, value) {
            return resp;
        }
    }
    if state
        .validation
        .push_event(EV_USER_MARKER, body.detail, body.member_id)
    {
        json_ok(StatusCode::OK, serde_json::json!({"recorded": true}))
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no validation session is recording"),
        )
    }
}

/// `POST /validation/session/measurement` — an external measurement (§14).
///
/// **Untrusted and read by nothing.** The daemon stores and returns these; no
/// control or safety path consults one, and none may be added.
pub async fn post_measurement_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MeasurementRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !body.value.is_finite() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("measurement value must be finite"),
        );
    }
    for (field, value) in [
        ("kind", Some(&body.kind)),
        ("unit", body.unit.as_ref()),
        ("note", body.note.as_ref()),
        ("member_id", body.member_id.as_ref()),
    ] {
        if let Some(resp) = too_long(field, value) {
            return resp;
        }
    }
    let m = ExternalMeasurement {
        unix_ms: unix_ms(),
        kind: body.kind,
        value: body.value,
        unit: body.unit.unwrap_or_default(),
        member_id: body.member_id,
        note: body.note,
    };
    if state.validation.add_measurement(m) {
        json_ok(StatusCode::OK, serde_json::json!({"recorded": true}))
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no validation session is recording"),
        )
    }
}

/// `GET /validation/sessions` — the retained index, newest first.
pub async fn list_sessions_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Read the store off the runtime: `list_from` fully parses every retained
    // session to sort them, and doing that inline blocks a tokio worker for as
    // long as it takes. Up to five sessions, each bounded by
    // `VALIDATION_MAX_SAMPLE_BYTES` — 3.6 MiB at one member, 7.8 MiB at three.
    // This said "~1 MB each" until 2026-09-04 (`AUD3-i`), which understated the
    // cost of doing it inline rather than overstating it.
    let mut sessions = super::persist_off_runtime(|| Ok::<_, String>(store::list()))
        .await
        .unwrap_or_default();
    // Prefer the live copy for whichever session is still recording. The on-disk
    // copy is only flushed every 30 s, so without this the index would report a
    // sample count tens of seconds behind the one `/poll` and
    // `GET /validation/session` are simultaneously showing for the same session —
    // two different numbers for one thing, with nothing saying which is stale.
    if let Some(live) = state.validation.snapshot() {
        if let Some(slot) = sessions
            .iter_mut()
            .find(|s| s.session_id == live.session_id)
        {
            *slot = live;
        } else if live.is_recording() {
            sessions.insert(0, live);
        }
    }
    let index: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "session_id": s.session_id,
                "kind": s.kind,
                "state": s.state,
                "started_unix_ms": s.started_unix_ms,
                "completed_unix_ms": s.completed_unix_ms,
                "cooling_device_id": s.metadata.cooling_device_id,
                "device_name": s.metadata.device_name,
                "sample_count": s.samples.len(),
                "event_count": s.events.len(),
                "sample_limit_reached": s.sample_limit_reached,
                "interrupted_reason": s.interrupted_reason,
            })
        })
        .collect();
    json_ok(
        StatusCode::OK,
        serde_json::json!({
            "api_version": crate::api::responses::API_VERSION,
            "sessions": index,
        }),
    )
}

/// `GET /validation/sessions/{id}` — one completed session in full.
pub async fn get_session_by_id_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // The live copy wins for the session still recording — the on-disk one lags
    // by up to one flush interval, and serving it here would contradict
    // `GET /validation/session` for the same id.
    if let Some(live) = state.validation.snapshot() {
        if live.session_id == session_id {
            return json_ok(StatusCode::OK, live);
        }
    }
    let loaded = {
        let id = session_id.clone();
        super::persist_off_runtime(move || store::load(&id)).await
    };
    match loaded {
        Ok(Some(s)) => json_ok(StatusCode::OK, s),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found(&format!("validation session '{session_id}'")),
        ),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(&e),
        ),
    }
}

// ── Orchestration ───────────────────────────────────────────────────────────

/// Run the requested diagnostics, in a fixed order, attaching each result.
///
/// Order is `pwm_verify` then `pwm_characterization`, per member: verify is
/// seconds and answers "does this header respond to a write at all?", while the
/// sweep is minutes. A **failed diagnostic does not abort the session** — a
/// header that fails verify is exactly §10's device-override signature, so the
/// sweep that follows is more valuable, not less, and `unavailable` never
/// becomes `fail` (§7).
fn spawn_orchestration(
    state: Arc<AppState>,
    session_id: String,
    diagnostics: Vec<String>,
    members: Vec<(String, bool)>,
) {
    // Shutdown-aware, because this task is detached and is NOT in `task_handles`.
    //
    // Until Phase 5 the verify and characterize handlers could only be reached
    // over HTTP, and `shutdown_sequence` stops the IPC server first — so nothing
    // could start a diagnostic after the drain, and neither handler needed an
    // entry guard. Calling them as functions from a detached task breaks that
    // invariant: without this subscription the orchestrator could enter
    // `run_verify` after `restore_hardware()`, whose write re-asserts
    // `pwm_enable=1` at the test duty while the DEC-290 check then deliberately
    // skips the restore — leaving the header latched in manual with no daemon.
    // Both handlers now also guard at entry, so this is defence in depth rather
    // than the only barrier.
    let shutdown = state.openfan_runtime.shutdown.clone();
    tokio::spawn(async move {
        for (member, writable) in &members {
            // A read-only header cannot be swept, and attempting it would drive
            // three minutes of diagnostic at something that will refuse every
            // write. Record it as `unavailable` — the hardware does not expose
            // what the diagnostic needs, which is never a failure (§7) — and
            // move on. Mirrors the DEC-102 posture of dropping non-writable
            // headers rather than discovering their read-only-ness the hard way.
            if !*writable {
                for diag in ordered_diagnostics(&diagnostics) {
                    state.validation.attach_evidence_for(
                        &session_id,
                        EvidenceRef {
                            kind: diag.to_string(),
                            member_id: member.clone(),
                            run_id: None,
                            started_unix_ms: unix_ms(),
                            completed_unix_ms: Some(unix_ms()),
                            outcome: RESULT_UNAVAILABLE.to_string(),
                            detail: Some("header is not writable".to_string()),
                            characterization: None,
                            verify: None,
                        },
                    );
                }
                continue;
            }
            for diag in ordered_diagnostics(&diagnostics) {
                if *shutdown.borrow() {
                    return;
                }
                // Stop as soon as THIS session is no longer the one recording.
                // Checking `is_recording()` alone is not enough: a cancel
                // followed by a new session leaves that true for a *different*
                // session, and every append below would then land on it.
                if state.validation.recording_session_id().as_deref() != Some(session_id.as_str()) {
                    return;
                }
                match diag {
                    DIAG_VERIFY => run_verify(&state, &session_id, member).await,
                    DIAG_CHARACTERIZATION => {
                        run_characterization(&state, &session_id, member).await
                    }
                    _ => {}
                }
            }
        }
    });
}

/// The fixed order, filtered to what was actually requested.
fn ordered_diagnostics(requested: &[String]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if requested.iter().any(|d| d == DIAG_VERIFY) {
        out.push(DIAG_VERIFY);
    }
    if requested.iter().any(|d| d == DIAG_CHARACTERIZATION) {
        out.push(DIAG_CHARACTERIZATION);
    }
    out
}

async fn run_verify(state: &Arc<AppState>, session_id: &str, member: &str) {
    let started = unix_ms();
    state.validation.push_event_for(
        session_id,
        EV_VERIFY_STARTED,
        None,
        Some(member.to_string()),
    );
    // The existing handler — lease, thermal refusal, role-aware duty and all.
    let (status, Json(body)) = super::hwmon_ctl::hwmon_verify_handler(
        State(state.clone()),
        axum::extract::Path(member.to_string()),
    )
    .await;

    let ok = status == StatusCode::OK;
    let outcome = if ok {
        RESULT_OBSERVED
    } else {
        // A refusal is not a hardware failure: the thermal ladder was forcing,
        // another diagnostic held the slot, or the daemon is going down.
        // `unavailable`, never `fail` (§7).
        RESULT_UNAVAILABLE
    };
    let evidence = VerifyEvidence {
        header_id: member.to_string(),
        write_ok: ok,
        readback_pct: body
            .get("readback_pct")
            .and_then(|v| v.as_u64())
            .map(|v| v as u8),
        requested_pct: body
            .get("test_pwm_percent")
            .and_then(|v| v.as_u64())
            .map(|v| v as u8),
        rpm_before: body
            .get("rpm_before")
            .and_then(|v| v.as_u64())
            .map(|v| v as u16),
        rpm_after: body
            .get("rpm_after")
            .and_then(|v| v.as_u64())
            .map(|v| v as u16),
        detail: body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .map(str::to_string),
    };
    state.validation.attach_evidence_for(
        session_id,
        EvidenceRef {
            kind: DIAG_VERIFY.to_string(),
            member_id: member.to_string(),
            run_id: None,
            started_unix_ms: started,
            completed_unix_ms: Some(unix_ms()),
            outcome: outcome.to_string(),
            detail: evidence.detail.clone(),
            characterization: None,
            verify: Some(evidence),
        },
    );
    state.validation.push_event_for(
        session_id,
        EV_VERIFY_COMPLETED,
        Some(outcome.to_string()),
        Some(member.to_string()),
    );
}

async fn run_characterization(state: &Arc<AppState>, session_id: &str, member: &str) {
    let started = unix_ms();
    // The existing handler returns 202 and sweeps detached; the run lands in the
    // process-global `RunSlot`, which is what we then watch.
    let (status, Json(body)) = super::hwmon_ctl::hwmon_characterize_handler(
        State(state.clone()),
        axum::extract::Path(member.to_string()),
        Json(crate::api::characterization::CharacterizationRequest::default()),
    )
    .await;

    if status != StatusCode::ACCEPTED {
        let detail = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .map(str::to_string);
        state.validation.attach_evidence_for(
            session_id,
            EvidenceRef {
                kind: DIAG_CHARACTERIZATION.to_string(),
                member_id: member.to_string(),
                run_id: None,
                started_unix_ms: started,
                completed_unix_ms: Some(unix_ms()),
                // Refused, not failed — the ladder was forcing, the slot was
                // taken, or the daemon is going down. §7: never a hardware
                // failure.
                outcome: RESULT_UNAVAILABLE.to_string(),
                detail,
                characterization: None,
                verify: None,
            },
        );
        return;
    }

    let run_id = body
        .get("run_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Watch the slot until this run leaves `running`. Bounded by the sweep's own
    // worst case: every point at the maximum settle, plus slack.
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(
            constants::CHARACTERIZATION_MAX_POINTS as u64
                * constants::CHARACTERIZATION_SETTLE_MAX_S
                * 2
                + 60,
        );
    let mut final_run = None;
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let snap = state.characterization.lock().clone();
        match snap {
            Some(run) if Some(&run.run_id) == run_id.as_ref() => {
                if !run.is_running() {
                    final_run = Some(run);
                    break;
                }
            }
            // A different run took the slot — ours is gone and we must not
            // attribute someone else's evidence to this session.
            Some(_) => break,
            None => break,
        }
        // Stop if THIS session is no longer the live one — see the fence note
        // on `attach_evidence_for`.
        if state.validation.recording_session_id().as_deref() != Some(session_id) {
            return;
        }
    }

    let outcome = match &final_run {
        Some(run) if run.state == "complete" => RESULT_OBSERVED,
        Some(_) => RESULT_INTERRUPTED,
        None => RESULT_UNKNOWN,
    };
    let detail = final_run.as_ref().and_then(|r| r.detail.clone());
    state.validation.attach_evidence_for(
        session_id,
        EvidenceRef {
            kind: DIAG_CHARACTERIZATION.to_string(),
            member_id: member.to_string(),
            run_id,
            started_unix_ms: started,
            completed_unix_ms: Some(unix_ms()),
            outcome: outcome.to_string(),
            detail,
            // Verbatim (§6) — every verdict on it is Phase 3's, recomputed nowhere.
            characterization: final_run,
            verify: None,
        },
    );
}
