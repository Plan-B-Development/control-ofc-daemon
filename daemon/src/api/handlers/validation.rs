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
            stop_permitted: Some(crate::hwmon::device_policy::stop_permitted(pump_protected)),
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

/// Which finaliser the request asked for.
enum Finalise {
    Stop,
    Cancel,
}

/// Finalise a session off the async worker threads (`AUD3-n`).
///
/// `stop`/`cancel` summarise the session and then persist it, and that write is
/// the expensive half of this request: `atomic_io::write_atomic` does `write` +
/// `fsync` + `rename` + a directory `fsync` over a document `AUD3-i` measures at
/// up to ~5.7 MiB. Running it inline blocked a tokio worker — the same runtime
/// the 1 Hz profile engine, and therefore the thermal-safety decision, is
/// scheduled on — while `prune_sessions_off_runtime()` on the very next line was
/// already careful to go off-runtime for a strictly cheaper read.
///
/// The whole engine call is wrapped rather than just the write. That keeps the
/// engine synchronous, which is its stated design and what lets it own its own
/// `save_lock` and stale-write guard, and it takes the slot-lock acquisition off
/// the runtime too — which matters because that same lock is what a wedged sysfs
/// write can hold up (`AUD3-k`).
///
/// **`Ok(None)` and `Err` are different facts and must not collapse.** `Ok(None)`
/// means no session has ever been started — a 404. `Err` means the finaliser
/// panicked, or the runtime is shutting down, and the session is *still
/// installed and still recording*: answering 404 there would tell a client the
/// session does not exist while the recorder keeps sampling it and the next
/// `POST` refuses with `AlreadyRecording`. It is a 500.
async fn finalise_off_runtime(
    state: &Arc<AppState>,
    which: Finalise,
) -> Result<Option<crate::validation::session::ValidationSession>, String> {
    let engine = state.validation.clone();
    match tokio::task::spawn_blocking(move || match which {
        Finalise::Stop => engine.stop(),
        Finalise::Cancel => engine.cancel(),
    })
    .await
    {
        Ok(session) => Ok(session),
        // A panicking finaliser must not take an API worker down. The session
        // stays installed and `recording`, which the next boot sweep represents
        // honestly as `interrupted` (§15) — never a fabricated "completed".
        Err(e) => Err(format!("validation finalise task failed: {e}")),
    }
}

/// Render a finalise result. Shared so `stop` and `cancel` cannot drift on the
/// three-way distinction above.
fn finalise_response(
    result: Result<Option<crate::validation::session::ValidationSession>, String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match result {
        Ok(Some(s)) => json_ok(StatusCode::OK, s),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no validation session has been started"),
        ),
        Err(e) => {
            log::warn!("{e}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal(e),
            )
        }
    }
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
    // `AUD3-n`: off the async runtime. `start` writes the session document —
    // `write` + `fsync` + `rename` + a directory `fsync`, over a document
    // `AUD3-i` measures at up to ~5.7 MiB — and blocks on the slot lock to do
    // it, all on the worker thread the 1 Hz profile engine shares. Wrapping the
    // whole call rather than only the write keeps `start`'s admit-only-if-
    // persisted rollback where it belongs, inside the engine.
    let started = {
        let engine = state.validation.clone();
        match tokio::task::spawn_blocking(move || engine.start(session, &ctx)).await {
            Ok(result) => result,
            // The blocking task panicked or the runtime is shutting down.
            // Reported as a persistence failure rather than unwrapped: a
            // panicking start must not take an API worker down with it.
            Err(e) => Err(StartError::Persistence(format!(
                "validation start task failed: {e}"
            ))),
        }
    };
    match started {
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
    let result = finalise_off_runtime(&state, Finalise::Stop).await;
    if matches!(result, Ok(Some(_))) {
        prune_sessions_off_runtime().await;
    }
    finalise_response(result)
}

/// `DELETE /validation/session`
pub async fn cancel_session_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = finalise_off_runtime(&state, Finalise::Cancel).await;
    if matches!(result, Ok(Some(_))) {
        prune_sessions_off_runtime().await;
    }
    finalise_response(result)
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
                            control_path: None,
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
                        run_characterization(&state, &session_id, member, false).await
                    }
                    DIAG_BEHAVIOUR => run_characterization(&state, &session_id, member, true).await,
                    DIAG_CONTROL_PATH => run_discovery(&state, &session_id, member).await,
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
    // DEC-334 (Q15): the behaviour sweep SUPERSEDES the basic one when both are
    // asked for. They drive the same route and the same process-global run slot
    // and differ only in request parameters, and the behaviour walk is a strict
    // superset — so running both would sweep every member twice for data the
    // second run already contains. `summarise` derives the basic findings from
    // the behaviour run too, so nothing is lost by skipping it.
    let behaviour = requested.iter().any(|d| d == DIAG_BEHAVIOUR);
    if behaviour {
        out.push(DIAG_BEHAVIOUR);
    } else if requested.iter().any(|d| d == DIAG_CHARACTERIZATION) {
        out.push(DIAG_CHARACTERIZATION);
    }
    // AIO Phase 8 Batch 1. Ordered LAST deliberately: discovery perturbs around
    // whatever duty it finds, so running it after the two diagnostics that
    // restore their own pre-test duty means it measures the header's settled
    // working point rather than another diagnostic's leftovers.
    if requested.iter().any(|d| d == DIAG_CONTROL_PATH) {
        out.push(DIAG_CONTROL_PATH);
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
            control_path: None,
        },
    );
    state.validation.push_event_for(
        session_id,
        EV_VERIFY_COMPLETED,
        Some(outcome.to_string()),
        Some(member.to_string()),
    );
}

/// Ask the characterisation sweep to stop, **fenced on the run this session
/// started** (`AUD3-j`).
///
/// The fence is the whole safety property, and it is why this is not a bare
/// `characterization_cancel.store(true)`. The sweep runs detached and the slot is
/// process-global: a run whose deadman elapsed can legally be superseded, and a
/// session ending a fraction late would otherwise abort *someone else's* sweep
/// after its first point and strand that header mid-restore. Comparing the
/// `run_id` we were handed at 202 is what makes "cancel my diagnostic" mean only
/// that.
///
/// Check and set under ONE lock, for the reason `characterization_cancel_handler`
/// states: two acquisitions leave a window in which the run can finish and a new
/// one be installed between them, and the late store then aborts the successor.
///
/// Takes the slot and the flag rather than `AppState` so the rule is unit-testable
/// on its own: the four cases that matter — ours still running, ours already
/// finished, someone else's, and none at all — need no daemon around them.
/// A diagnostic run a session can cancel. Implemented by both detached sweeps so
/// [`cancel_run_fenced`] stays ONE definition of the fencing rule rather than
/// growing a near-identical copy per diagnostic (DEC-276).
pub(crate) trait CancellableRun {
    fn is_running(&self) -> bool;
    fn run_id(&self) -> &str;
}

impl CancellableRun for crate::api::characterization::CharacterizationRun {
    fn is_running(&self) -> bool {
        self.is_running()
    }
    fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl CancellableRun for crate::api::discovery::ControlPathRun {
    fn is_running(&self) -> bool {
        self.is_running()
    }
    fn run_id(&self) -> &str {
        &self.run_id
    }
}

fn cancel_run_fenced<R: CancellableRun>(
    slot: &parking_lot::Mutex<Option<R>>,
    cancel: &std::sync::atomic::AtomicBool,
    run_id: Option<&str>,
) {
    use std::sync::atomic::Ordering;
    let Some(run_id) = run_id else {
        return;
    };
    // The STORE stays under the guard — one lock across check-and-set is the
    // whole point — but the log does not: a `log::info!` can block on a full
    // stderr pipe, and this mutex is on the sweep's per-point publish path.
    let cancelled = {
        let guard = slot.lock();
        let mine = guard
            .as_ref()
            .is_some_and(|r| CancellableRun::is_running(r) && r.run_id() == run_id);
        if mine {
            cancel.store(true, Ordering::SeqCst);
        }
        mine
    };
    if cancelled {
        log::info!("Validation session ended; cancelling its diagnostic run {run_id}");
    }
}

async fn run_characterization(
    state: &Arc<AppState>,
    session_id: &str,
    member: &str,
    behaviour: bool,
) {
    // Evidence is attributed to the token that was actually requested, so a
    // reader can tell a behaviour run from a basic one without inspecting the
    // run's fields. `EvidenceRef.kind` already carries this distinction; both
    // still land in `evidence[].characterization`, because it is the same run
    // type from the same route.
    let kind = if behaviour {
        DIAG_BEHAVIOUR
    } else {
        DIAG_CHARACTERIZATION
    };
    let started = unix_ms();
    // The existing handler returns 202 and sweeps detached; the run lands in the
    // process-global `RunSlot`, which is what we then watch.
    let (status, Json(body)) = super::hwmon_ctl::hwmon_characterize_handler(
        State(state.clone()),
        axum::extract::Path(member.to_string()),
        Json(crate::api::characterization::CharacterizationRequest {
            // DEC-334 Q3: the bidirectional walk is OFF for the basic token
            // inside a session — selecting the behaviour token IS the opt-in.
            bidirectional: Some(behaviour),
            stability_seconds: behaviour.then_some(crate::constants::STABILITY_DEFAULT_S),
            ..Default::default()
        }),
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
                kind: kind.to_string(),
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
                control_path: None,
            },
        );
        return;
    }

    let run_id = body
        .get("run_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Watch the slot until this run leaves `running`. Bounded by the sweep's own
    // worst case: every step at the maximum settle, plus every dwellable step at
    // the maximum dwell, plus slack.
    //
    // [SAFETY-adjacent] DEC-334 RE-DERIVED this rather than leaving it standing.
    // The old form was `MAX_POINTS * SETTLE_MAX_S * 2 + 60` = 660 s against a
    // 300 s worst case — a 2.2x margin. A stability dwell adds up to
    // `STABILITY_MAX_POINTS * STABILITY_MAX_S` to that worst case, which the old
    // formula does not know about, so it would have silently eroded to ~1.4x and
    // then started reporting healthy long runs as interrupted. Copying a bound
    // between two call sites keeps its arithmetic and changes its meaning
    // (DEC-333); this one is derived from what the sweep can actually take.
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(
            (constants::CHARACTERIZATION_MAX_POINTS as u64
                * constants::CHARACTERIZATION_SETTLE_MAX_S
                + constants::STABILITY_MAX_POINTS as u64 * constants::STABILITY_MAX_S)
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
            // ...and take the sweep down with us (`AUD3-j`). Returning alone left
            // the detached sweep in `hwmon_characterize_handler` still driving the
            // header AND still renewing the engine's write-pause once per point,
            // so ending a session suspended curve control for up to
            // `CHARACTERIZATION_MAX_POINTS × CHARACTERIZATION_SETTLE_MAX_S` after
            // the user had ended it. Thermal safety still outranked that — the
            // forced-duty branch runs above the `verify_active` gate — so it was
            // lost control intent, never lost cooling; it was still a diagnostic
            // that outlived the thing that asked for it.
            cancel_run_fenced(
                &state.characterization,
                &state.characterization_cancel,
                run_id.as_deref(),
            );
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
            kind: kind.to_string(),
            member_id: member.to_string(),
            run_id,
            started_unix_ms: started,
            completed_unix_ms: Some(unix_ms()),
            outcome: outcome.to_string(),
            detail,
            // Verbatim (§6) — every verdict on it is Phase 3's, recomputed nowhere.
            characterization: final_run,
            verify: None,
            control_path: None,
        },
    );
}

/// Orchestrate one control-path discovery run (AIO Phase 8 Batch 1, §5).
///
/// Structurally identical to [`run_characterization`], and deliberately so: the
/// existing handler is called as a function — lease, pump floor, thermal
/// refusal, restore guard and all — and this only watches the slot and attaches
/// the result. A session orchestrates diagnostics; it never reimplements one.
async fn run_discovery(state: &Arc<AppState>, session_id: &str, member: &str) {
    let started = unix_ms();
    state.validation.push_event_for(
        session_id,
        EV_DISCOVERY_STARTED,
        None,
        Some(member.to_string()),
    );
    let (status, Json(body)) = super::discovery::discover_control_path_handler(
        State(state.clone()),
        axum::extract::Path(member.to_string()),
        Json(crate::api::discovery::DiscoveryRequest::default()),
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
                kind: DIAG_CONTROL_PATH.to_string(),
                member_id: member.to_string(),
                run_id: None,
                started_unix_ms: started,
                completed_unix_ms: Some(unix_ms()),
                // Refused, not failed (§7): the slot was taken, the ladder was
                // forcing, the header is read-only, or the daemon is going down.
                outcome: RESULT_UNAVAILABLE.to_string(),
                detail,
                characterization: None,
                verify: None,
                control_path: None,
            },
        );
        state.validation.push_event_for(
            session_id,
            EV_DISCOVERY_COMPLETED,
            Some(RESULT_UNAVAILABLE.to_string()),
            Some(member.to_string()),
        );
        return;
    }

    let run_id = body
        .get("run_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Bounded by the sweep's own worst case: every cycle holding the maximum
    // settle at BOTH the baseline and the perturbed duty, plus slack. A bare
    // `sleep` would be the tokio trap this project has recorded — every wait
    // here is a bounded poll against a deadline.
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(
            constants::DISCOVERY_MAX_CYCLES as u64
                * 2
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
        let snap = state.control_path.lock().clone();
        match snap {
            Some(run) if Some(&run.run_id) == run_id.as_ref() => {
                if !run.is_running() {
                    final_run = Some(run);
                    break;
                }
            }
            // A different run took the slot — ours is gone, and attributing
            // someone else's evidence to this session is the exact defect the
            // `run_id` fence exists to prevent.
            Some(_) => break,
            None => break,
        }
        if state.validation.recording_session_id().as_deref() != Some(session_id) {
            // ...and take the sweep down with us (`AUD3-j`): a detached run left
            // alive would keep driving the header AND keep renewing the engine's
            // write-pause after the user ended the session.
            cancel_run_fenced(
                &state.control_path,
                &state.control_path_cancel,
                run_id.as_deref(),
            );
            return;
        }
    }

    let outcome = match &final_run {
        Some(run) if run.state == crate::api::discovery::STATE_COMPLETE => RESULT_OBSERVED,
        Some(_) => RESULT_INTERRUPTED,
        None => RESULT_UNKNOWN,
    };
    let detail = final_run.as_ref().and_then(|r| r.detail.clone());
    state.validation.attach_evidence_for(
        session_id,
        EvidenceRef {
            kind: DIAG_CONTROL_PATH.to_string(),
            member_id: member.to_string(),
            run_id,
            started_unix_ms: started,
            completed_unix_ms: Some(unix_ms()),
            outcome: outcome.to_string(),
            detail,
            characterization: None,
            verify: None,
            // Verbatim (§6) — the relationship, the confidence and the
            // measurement resolution are the ones `discovery::summarise`
            // computed, recomputed nowhere.
            control_path: final_run,
        },
    );
    state.validation.push_event_for(
        session_id,
        EV_DISCOVERY_COMPLETED,
        Some(outcome.to_string()),
        Some(member.to_string()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::characterization as ch;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The smallest `ValidationSession` the response mapper can be handed. Built
    /// literally because `SessionMetadata` has no `Default` — every field of it is
    /// load-bearing evidence, so making one defaultable would let a real session
    /// be constructed with an empty device.
    fn finalised_session() -> ValidationSession {
        ValidationSession {
            session_id: "val-fin".into(),
            kind: KIND_VALIDATION.into(),
            state: STATE_COMPLETED.into(),
            started_unix_ms: 1,
            completed_unix_ms: Some(2),
            metadata: SessionMetadata {
                cooling_device_id: "dev-1".into(),
                device_name: "Test AIO".into(),
                device_kind: "aio_liquid".into(),
                pump_member: None,
                radiator_members: vec![],
                auxiliary_members: vec![],
                temperature_sensor: None,
                coolant_sensor: None,
                coolant_telemetry: "unavailable".into(),
                device_policy: DevicePolicySnapshot {
                    id: "generic_pump".into(),
                    display_name: "Generic pump".into(),
                    minimum_safe_pwm_pct: 30.0,
                    supports_stop: false,
                    startup_override_seconds: None,
                    expected_rpm_min: None,
                    expected_rpm_max: None,
                    internal_control_possible: true,
                },
                members: vec![],
                active_profile_id: None,
                active_profile_name: None,
                daemon_version: "0.0.0-test".into(),
                user_metadata: Default::default(),
            },
            requested_diagnostics: vec![],
            sweep_members: vec![],
            samples: vec![],
            events: vec![],
            evidence: vec![],
            external_measurements: vec![],
            findings: vec![],
            sample_limit_reached: false,
            interrupted_reason: None,
            truncated_at_unix_ms: None,
        }
    }

    fn run(id: &str, state: &str) -> ch::CharacterizationRun {
        ch::CharacterizationRun {
            run_id: id.into(),
            header_id: "hwmon:it87:isa-0a30:pwm2:PUMP".into(),
            state: state.into(),
            requested_points_pct: vec![30, 60, 100],
            settle_seconds: 5,
            points: vec![],
            summary: None,
            original_pct: Some(40),
            restore_failed: false,
            restore_outcome: ch::RestoreOutcome::Pending.token().to_string(),
            detail: None,
            ..Default::default()
        }
    }

    /// A broken finaliser and an absent session are different facts, and the
    /// wire must not collapse them (review finding on DEC-323).
    ///
    /// Folding a `JoinError` into `None` answered `404 "no validation session has
    /// been started"` while the session was still installed and still recording —
    /// so a client would conclude the session did not exist, stop offering to stop
    /// it, and then be refused `AlreadyRecording` on its next `POST`.
    #[test]
    fn a_broken_finaliser_is_a_500_and_an_absent_session_is_a_404() {
        let (code, _) = finalise_response(Ok(None));
        assert_eq!(
            code,
            StatusCode::NOT_FOUND,
            "no session ever started is a 404"
        );

        let (code, body) = finalise_response(Err("validation finalise task failed: panic".into()));
        assert_eq!(
            code,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a finaliser that broke must not be reported as an absent session"
        );
        assert_eq!(
            body.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
            Some("internal_error")
        );

        // And the ordinary case still answers 200, so a mapping that returned
        // one status for everything cannot satisfy the two assertions above.
        let (code, _) = finalise_response(Ok(Some(finalised_session())));
        assert_eq!(code, StatusCode::OK);
    }

    /// `AUD3-j`: ending a session must end the sweep that session started.
    ///
    /// Without this the detached sweep kept driving the header AND kept renewing
    /// the engine's write-pause once per point, so curve control stayed suspended
    /// for up to `CHARACTERIZATION_MAX_POINTS × CHARACTERIZATION_SETTLE_MAX_S`
    /// after the user ended the session.
    #[test]
    fn ending_a_session_cancels_the_run_that_session_started() {
        let slot = parking_lot::Mutex::new(Some(run("char-7", ch::STATE_RUNNING)));
        let cancel = AtomicBool::new(false);
        cancel_run_fenced(&slot, &cancel, Some("char-7"));
        assert!(
            cancel.load(Ordering::SeqCst),
            "our own running sweep must be asked to stop"
        );
    }

    /// The fence, and the reason it is not a bare `store(true)`.
    ///
    /// The slot is process-global and a run whose deadman elapsed can legally be
    /// superseded. A session ending a fraction late would otherwise abort a
    /// stranger's sweep after its first point, strand that header mid-restore, and
    /// report the abort as though the user had asked for it.
    #[test]
    fn a_successor_run_is_never_cancelled_by_the_session_that_preceded_it() {
        let slot = parking_lot::Mutex::new(Some(run("char-8", ch::STATE_RUNNING)));
        let cancel = AtomicBool::new(false);
        cancel_run_fenced(&slot, &cancel, Some("char-7"));
        assert!(
            !cancel.load(Ordering::SeqCst),
            "a run this session did not start must never be cancelled"
        );
    }

    /// A finished run must not leave the flag armed: the next sweep clears it
    /// under the slot lock at install, but arming it for a terminal run is a
    /// pointless race to lose.
    #[test]
    fn a_run_that_has_already_finished_is_not_cancelled() {
        for state in [
            ch::STATE_COMPLETE,
            ch::STATE_CANCELLED,
            ch::STATE_ABORTED,
            ch::STATE_FAILED,
        ] {
            let slot = parking_lot::Mutex::new(Some(run("char-9", state)));
            let cancel = AtomicBool::new(false);
            cancel_run_fenced(&slot, &cancel, Some("char-9"));
            assert!(
                !cancel.load(Ordering::SeqCst),
                "a '{state}' run needs no cancelling"
            );
        }
    }

    /// No run id means the 202 never carried one — there is nothing this session
    /// can prove it owns, so it cancels nothing. An empty slot likewise.
    #[test]
    fn an_unidentified_run_cancels_nothing() {
        let slot = parking_lot::Mutex::new(Some(run("char-10", ch::STATE_RUNNING)));
        let cancel = AtomicBool::new(false);
        cancel_run_fenced(&slot, &cancel, None);
        assert!(!cancel.load(Ordering::SeqCst));

        let empty: parking_lot::Mutex<Option<crate::api::characterization::CharacterizationRun>> =
            parking_lot::Mutex::new(None);
        cancel_run_fenced(&empty, &cancel, Some("char-10"));
        assert!(!cancel.load(Ordering::SeqCst));
    }

    // ── AIO Phase 8 Batch 2 (DEC-334) ────────────────────────────────

    #[test]
    fn the_behaviour_token_is_recognised_and_ordered_with_the_other_diagnostics() {
        assert!(is_known_diagnostic(DIAG_BEHAVIOUR));
        let out = ordered_diagnostics(&[
            DIAG_CONTROL_PATH.to_string(),
            DIAG_BEHAVIOUR.to_string(),
            DIAG_VERIFY.to_string(),
        ]);
        assert_eq!(out, vec![DIAG_VERIFY, DIAG_BEHAVIOUR, DIAG_CONTROL_PATH]);
    }

    /// [DEC-334 Q15] Both tokens drive the same route and the same
    /// process-global run slot, and the behaviour walk is a strict superset — so
    /// asking for both must sweep each member ONCE, not twice.
    #[test]
    fn requesting_both_characterisations_runs_only_the_behaviour_sweep() {
        let out = ordered_diagnostics(&[
            DIAG_CHARACTERIZATION.to_string(),
            DIAG_BEHAVIOUR.to_string(),
        ]);
        assert_eq!(out, vec![DIAG_BEHAVIOUR]);
        assert!(
            !out.contains(&DIAG_CHARACTERIZATION),
            "the basic sweep is superseded, not run alongside"
        );
    }

    /// The other side of the same rule: the basic token still works alone, so an
    /// older client that never learned the new one is unaffected.
    #[test]
    fn the_basic_token_still_runs_on_its_own() {
        assert_eq!(
            ordered_diagnostics(&[DIAG_CHARACTERIZATION.to_string()]),
            vec![DIAG_CHARACTERIZATION]
        );
    }

    /// [DEC-334 Q15] The load-bearing half of "behaviour supersedes": if the
    /// findings only looked at `pwm_characterization` evidence, a session that
    /// asked for the richer sweep would report `not_tested` for every basic
    /// characterisation finding — about a diagnostic that had just run.
    ///
    /// Asserts a RELATIONSHIP: the same evidence under either kind must produce
    /// the same basic findings. A literal expectation would pass against a
    /// filter that happened to match only the token the test wrote.
    #[test]
    fn a_behaviour_run_feeds_every_basic_characterisation_finding() {
        use crate::validation::session::{F_PWM_RESPONSE, F_RESPONSE_LATENCY, RESULT_NOT_TESTED};
        let mut r = run("char-9", ch::STATE_COMPLETE);
        r.points = vec![ch::CharPoint {
            requested_pct: 60,
            command_accepted: true,
            readback_pct: Some(60),
            pwm_enable: Some(1),
            rpm_before: Some(900),
            rpm_after: Some(1800),
            first_change_ms: Some(1500),
            readback_verdict: "match".into(),
            rpm_verdict: "changed".into(),
            direction: "rising".into(),
            ..Default::default()
        }];
        r.summary = Some(ch::summarise(&r.points, &[]));

        let findings_for = |kind: &str| {
            let mut session = finalised_session();
            session.evidence = vec![EvidenceRef {
                kind: kind.to_string(),
                member_id: "hwmon:it87:isa-0a30:pwm2:PUMP".into(),
                run_id: Some("char-9".into()),
                started_unix_ms: 1,
                completed_unix_ms: Some(2),
                outcome: crate::validation::session::RESULT_OBSERVED.to_string(),
                detail: None,
                characterization: Some(r.clone()),
                verify: None,
                control_path: None,
            }];
            crate::validation::summary::summarise(&session)
        };

        let basic = findings_for(DIAG_CHARACTERIZATION);
        let behaviour = findings_for(DIAG_BEHAVIOUR);
        let state_of = |fs: &[ValidationFinding], id: &str| {
            fs.iter()
                .find(|f| f.id == id)
                .map(|f| f.state.clone())
                .unwrap_or_else(|| panic!("no {id} finding"))
        };

        // [contract review] `evidence_kind` must name the token that ACTUALLY
        // ran. Six findings hardcoded `pwm_characterization`, so a behaviour-only
        // session exported six findings attributed to a diagnostic that never
        // executed — in a batch whose entire subject is provenance honesty.
        let kind_of = |fs: &[ValidationFinding], id: &str| {
            fs.iter()
                .find(|f| f.id == id)
                .and_then(|f| f.evidence_kind.clone())
                .unwrap_or_else(|| panic!("no {id} finding"))
        };
        for id in [F_PWM_RESPONSE, F_RESPONSE_LATENCY] {
            assert_eq!(
                kind_of(&basic, id),
                DIAG_CHARACTERIZATION,
                "{id} must name the basic token when the basic run produced it"
            );
            assert_eq!(
                kind_of(&behaviour, id),
                DIAG_BEHAVIOUR,
                "{id} must name the BEHAVIOUR token when that is what ran"
            );
        }

        for id in [F_PWM_RESPONSE, F_RESPONSE_LATENCY] {
            // The precondition that makes the comparison mean something: the
            // basic run must genuinely have produced a finding, or "they match"
            // would be two `not_tested`s agreeing.
            assert_ne!(
                state_of(&basic, id),
                RESULT_NOT_TESTED,
                "precondition: the basic run must produce a real {id} finding"
            );
            assert_eq!(
                state_of(&behaviour, id),
                state_of(&basic, id),
                "a behaviour run must feed {id} exactly as a basic run does"
            );
        }
    }
}
