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
    ///
    /// Repeats are dropped at ingest by `normalise_diagnostics` (`P8-s`), so the
    /// session records at most one of each and the response echoes what was
    /// actually taken. Unknown tokens are still rejected outright.
    #[serde(default)]
    pub diagnostics: Vec<String>,
    /// Members those diagnostics sweep. Absent defaults to the pump member.
    #[serde(default)]
    pub sweep_members: Vec<String>,
    /// Free-form user/test metadata (§11). Metadata only — it never reaches a
    /// safety decision.
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, String>,
    /// Finalise the session as soon as the orchestrated diagnostics finish
    /// (`P8-az`).
    ///
    /// **Opt-in, and the default is deliberately `false`.** Before this field a
    /// session ended only when an operator stopped it or the sample cap was hit
    /// — [`constants::VALIDATION_MAX_SAMPLES`] x
    /// [`constants::VALIDATION_SAMPLE_INTERVAL`], a flat two hours — so with
    /// everything ticked the diagnostics finished in ~4 min and the recorder ran
    /// for the remaining ~1 h 56 m. Defaulting this to `true` would change what
    /// an existing client and every `curl` user already gets, so the daemon keeps
    /// today's behaviour and the *caller* asks.
    ///
    /// Requires at least one entry in `diagnostics`: see
    /// [`start_session_handler`], which rejects the combination rather than
    /// accepting a flag nothing can act on.
    #[serde(default)]
    pub stop_when_diagnostics_complete: bool,
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
    let (hwmon_root, powercap_root) = RecorderContext::sysfs_roots();
    RecorderContext {
        cache: state.cache.clone(),
        hwmon_controller: state.hwmon_controller.clone(),
        override_table: state.override_table.clone(),
        characterization: state.characterization.clone(),
        hwmon_root,
        powercap_root,
    }
}

/// Open the opt-in startup lifecycle recording (Batch 3a §1, DEC-335).
///
/// Called once at daemon start, and only when `[startup] record_startup = true`.
/// Returns the session id when a recording was opened, `None` when there was
/// nothing to record or the slot was already busy.
///
/// # What it deliberately does not do
///
/// It writes no hardware, claims no verify slot and takes no hwmon lease — it is
/// the same passive recorder an operator would start by hand, with the same
/// engine. It also never blocks one: `ValidationEngine::start` pre-empts this
/// session the moment an operator starts their own.
///
/// A machine with no configured cooling device records nothing and logs once.
/// There is no sensible fallback — a validation session is scoped to a device,
/// and inventing one would produce a recording whose members mean nothing.
/// `AUD3-n`: `start` persists the session document, so the engine call is
/// handed to `spawn_blocking` exactly as the request path does. Building the
/// session itself is cheap and stays here.
pub async fn start_auto_startup_record(state: &Arc<AppState>) -> Option<String> {
    let devices = state.cooling_devices.read().clone();
    let Some(device) = devices.first() else {
        log::info!(
            "[startup] record_startup is enabled but no cooling device is configured; \
             nothing to record"
        );
        return None;
    };

    let metadata = build_metadata(state, device, Default::default());
    let started_unix_ms = unix_ms();
    let session = ValidationSession {
        session_id: next_session_id(),
        kind: KIND_LIFECYCLE.to_string(),
        state: STATE_RECORDING.to_string(),
        started_unix_ms,
        completed_unix_ms: None,
        metadata,
        // No active diagnostic. §1 is a passive observation of what the hardware
        // does on its own at power-on; running a sweep would change the very
        // behaviour being recorded.
        requested_diagnostics: Vec::new(),
        sweep_members: Vec::new(),
        samples: Vec::new(),
        // **The first and only emitter of `daemon_restart_observed`.** The token
        // has existed since Phase 5 and nothing has ever pushed it, because a
        // session cannot survive a restart — so both findings that consume it
        // were structurally unreachable. A recording that begins *because* the
        // daemon started is the one place the event is true by construction.
        events: vec![ValidationEvent {
            elapsed_ms: 0,
            unix_ms: started_unix_ms,
            kind: EV_DAEMON_RESTART.to_string(),
            detail: Some("recording opened automatically at daemon start".to_string()),
            member_id: None,
        }],
        evidence: Vec::new(),
        external_measurements: Vec::new(),
        findings: Vec::new(),
        sample_limit_reached: false,
        interrupted_reason: None,
        truncated_at_unix_ms: None,
        auto_started: true,
        // The startup record orchestrates no diagnostics at all; its bound is
        // `STARTUP_RECORD_WINDOW_S` through `stop_auto_startup_record`.
        stop_when_diagnostics_complete: false,
        startup_fingerprints: Vec::new(),
        steady_state: None,
    };

    let ctx = recorder_context(state);
    let device_id = device.id.clone();
    let engine = state.validation.clone();
    let started = tokio::task::spawn_blocking(move || engine.start(session, &ctx))
        .await
        .ok()?;
    match started {
        Ok(s) => {
            log::info!(
                "Startup lifecycle recording {} opened for {} ({} s window)",
                s.session_id,
                device_id,
                constants::STARTUP_RECORD_WINDOW_S
            );
            Some(s.session_id)
        }
        Err(e) => {
            // Never fatal, and never retried. An operator who started a session
            // in the first seconds after boot has already won, which is the
            // intended outcome.
            log::info!("Startup lifecycle recording not opened: {e:?}");
            None
        }
    }
}

/// Close the startup auto-record, if it is still the session in the slot.
///
/// Fenced on the id: by the time the window elapses the recording may have been
/// pre-empted by an operator and a *different* session may be running. Stopping
/// unconditionally would then finalise the operator's session out from under
/// them — the precise failure this whole feature is supposed to be incapable of.
/// `AUD3-n` again: finalising persists, so this goes through the same
/// `finalise_off_runtime` hop the two request handlers use rather than calling
/// `stop()` inline. A source-scanning guard in `validation_phase5.rs` enforces
/// it, and it caught this exact call site while Batch 3a was being written.
pub async fn stop_auto_startup_record(state: &Arc<AppState>, session_id: &str) {
    // A cheap early-out, NOT the guarantee. `stop_if` below is what actually
    // fences; this only avoids a `spawn_blocking` hop in the common case where
    // the recording is plainly gone.
    if !state
        .validation
        .recording_session_id()
        .is_some_and(|id| id == session_id)
    {
        log::debug!("Startup lifecycle recording {session_id} already ended; nothing to stop");
        return;
    }
    // `stop_if`, not `stop`: the id comparison must happen under the SAME guard
    // as the finalisation. Checking `recording_session_id()` and then calling
    // the unfenced `stop()` leaves a `spawn_blocking` dispatch in the gap — long
    // enough for an operator to pre-empt and have this finalise *their* session
    // seconds after it started, which is the one outcome this feature must be
    // incapable of. Found by `ofc:concurrency-reviewer`.
    let engine = state.validation.clone();
    let owned = session_id.to_string();
    let finished = tokio::task::spawn_blocking(move || engine.stop_if(&owned)).await;
    match finished {
        Ok(Some(s)) => log::info!(
            "Startup lifecycle recording {} closed after {} s ({} samples)",
            s.session_id,
            constants::STARTUP_RECORD_WINDOW_S,
            s.samples.len()
        ),
        Ok(None) => {
            log::debug!("Startup lifecycle recording {session_id} was already superseded")
        }
        Err(e) => log::warn!("Startup lifecycle recording {session_id} could not be closed: {e}"),
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
        StartError::TooMany(what) | StartError::Unsatisfiable(what) => {
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
/// a document bounded by `VALIDATION_MAX_SESSION_BYTES` (28 MiB; ~5.7 MiB for a
/// realistic two-member session). Running it inline blocked a tokio worker —
/// the same runtime
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

    // Validate diagnostics, then bound the list.
    //
    // Validation is per token and says nothing about the COUNT, which is what
    // `P8-s` was: every entry of `[DIAG_VERIFY; 320_000]` is a known diagnostic.
    // `normalise_diagnostics` bounds it at the size of the closed token set, and
    // from here on the normalised list is the only one this handler reads — the
    // request field is not consulted again, so there is one shape for this value
    // rather than two that can disagree.
    for d in &body.diagnostics {
        if !is_known_diagnostic(d) {
            return start_error_response(StartError::UnknownDiagnostic(d.clone()));
        }
    }
    let diagnostics = normalise_diagnostics(&body.diagnostics);

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

    // `stop_when_diagnostics_complete` with nothing to complete (`P8-az`).
    //
    // **Placed here, after the sweep is resolved, and that position is the
    // rule.** The hop's real precondition is not "the caller named a
    // diagnostic" but "the orchestration walk has something to walk", and those
    // two differ: `spawn_orchestration` iterates `members x diagnostics`, so an
    // empty resolved sweep walks zero members and drops straight into the
    // terminal hop. Checking only `diagnostics` — which is where this guard was
    // first written — left that reachable, and it was reachable from an
    // ordinary configuration rather than a contrived one: `pump_member` is
    // `Option` and `validate_device` does not require it, so an air-cooler or
    // custom-loop device with only radiator members resolves `sweep` to `[]`
    // whenever the caller omits `sweep_members`. Measured before this moved: a
    // `200`, then `completed` with **0 samples and 0 evidence** within
    // milliseconds.
    //
    // Rejected rather than ignored, in both cases. An empty `diagnostics` array
    // is a legitimate passive recording session — but combined with this flag
    // there is no task to carry the stop, so the session would run to the
    // two-hour cap while `GET /validation/session` echoed a flag saying it stops
    // when the diagnostics finish. And accepting the empty-sweep case produces
    // the near-empty document that reads as a failed start, which is exactly the
    // alternative rejected when this feature was specified. Found by
    // `ofc:concurrency-reviewer`.
    if body.stop_when_diagnostics_complete {
        if diagnostics.is_empty() {
            return start_error_response(StartError::Unsatisfiable(
                "stop_when_diagnostics_complete requires at least one diagnostic".to_string(),
            ));
        }
        if sweep.is_empty() {
            return start_error_response(StartError::Unsatisfiable(
                "stop_when_diagnostics_complete requires at least one sweep member, and this \
                 cooling device has no pump member to default to — name one in sweep_members"
                    .to_string(),
            ));
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

    // An unrecognised kind still falls back to `validation` rather than being
    // rejected. That is pre-existing behaviour and it is why every kind added
    // after the first needs a capability flag: an older daemon accepts
    // `thermal_observation` and silently records something else, and a client
    // cannot tell from the response alone. `control.thermal_observation` is
    // what closes that on the client side. Registered as `P8-k`.
    let kind = match body.kind.as_deref() {
        Some(KIND_LIFECYCLE) => KIND_LIFECYCLE,
        Some(KIND_THERMAL) => KIND_THERMAL,
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
        requested_diagnostics: diagnostics.clone(),
        sweep_members: sweep.clone(),
        samples: Vec::new(),
        events: Vec::new(),
        evidence: Vec::new(),
        external_measurements: Vec::new(),
        findings: Vec::new(),
        sample_limit_reached: false,
        interrupted_reason: None,
        truncated_at_unix_ms: None,
        // An operator asked for this one. `ValidationEngine::start` reads the
        // flag to decide whether an in-flight session may be pre-empted, so a
        // hand-started session must never carry it.
        auto_started: false,
        // Rejected above unless at least one diagnostic was requested, so this
        // is never true on a session `spawn_orchestration` will not be given.
        stop_when_diagnostics_complete: body.stop_when_diagnostics_complete,
        // Both are derived at finalisation from the recorded samples, never
        // during recording: a fingerprint or a steady-state verdict computed
        // from a partial run would have to be recomputed anyway, and publishing
        // an interim one invites a client to render it as settled.
        startup_fingerprints: Vec::new(),
        steady_state: None,
    };

    let ctx = recorder_context(&state);
    // `AUD3-n`: off the async runtime. `start` writes the session document —
    // `write` + `fsync` + `rename` + a directory `fsync`, over a document
    // `AUD3-i` measures at ~5.7 MiB for a realistic two-member session and
    // `VALIDATION_MAX_SESSION_BYTES` bounds at 28 MiB — and blocks on the slot
    // lock to do it, all on the worker thread the 1 Hz profile engine shares.
    // Wrapping the whole call rather than only the write keeps
    // `start`'s admit-only-if-
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
                    started.stop_when_diagnostics_complete,
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
///
/// When `auto_stop` is set the task finalises the session on its way out —
/// **only** by falling off the end of the walk, never from either early return.
/// See the hop itself for why that distinction is the whole safety argument.
fn spawn_orchestration(
    state: Arc<AppState>,
    session_id: String,
    diagnostics: Vec<String>,
    members: Vec<(String, bool)>,
    auto_stop: bool,
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
        // ── The terminal hop (`P8-az`) ───────────────────────────────────
        //
        // Reached ONLY by falling off the end of the walk. Both early returns
        // above — the shutdown check and the superseded check — leave without
        // touching the session, and that asymmetry is the whole safety
        // argument, not a tidiness preference:
        //
        //   * shutdown: `shutdown_sequence` is already finalising the world.
        //     Racing it with a finalise of our own would write the session
        //     document a second time from a detached task the shutdown does not
        //     wait for, against the store the boot sweep is about to read.
        //     An interrupted session is represented honestly as `interrupted`
        //     (§15); a `completed` stamped on the way out of a shutdown would
        //     be a fabricated verdict.
        //   * superseded: the slot no longer holds our session, so there is
        //     nothing of ours left to stop. `stop_if` would refuse anyway — the
        //     early return simply means we do not pay a `spawn_blocking` hop to
        //     be told so.
        //
        // The shutdown flag is re-read HERE and not merely inherited from the
        // loop, because the loop's check is per-diagnostic: a shutdown signalled
        // after the last one would otherwise slip through the gap and take the
        // hop, which is the one case the paragraph above says cannot happen.
        // (`stop_if`'s own `is_recording()` guard makes either ordering benign
        // against the shutdown flush — but "benign by luck of the interleaving"
        // is not the invariant this comment claims, and a comment that overstates
        // its code is how the next reader is misled.)
        //
        // A walk with no members cannot reach this: the start handler refuses
        // the flag when the RESOLVED sweep is empty, not merely when
        // `diagnostics` is. That claim used to read the other way round — "it
        // is deliberate, the operator learns at once" — and it was wrong; the
        // artefact is a `completed` session with 0 samples and 0 evidence.
        //
        // `stop_if`, never `stop`: the id comparison must happen under the SAME
        // guard as the finalisation. Between the last diagnostic and this line
        // an operator can stop the session and start another, and the unfenced
        // `stop()` would then finalise THEIRS — seconds after it began, from a
        // task that belongs to a session that has already ended. This is
        // `stop_auto_startup_record`'s lesson, and it is the same call for the
        // same reason.
        //
        // `AUD3-n`: finalising summarises and persists (`write` + `fsync` +
        // `rename` + a directory `fsync`, over a document `AUD3-i` measures at
        // ~5.7 MiB for a realistic two-member session, bounded at 28 MiB by
        // `VALIDATION_MAX_SESSION_BYTES`) and blocks on the slot lock to do
        // it — so it goes off the runtime the 1 Hz profile engine shares,
        // exactly as the two
        // request handlers and the startup record do. A source-scanning guard
        // in `validation_phase5.rs` enforces this.
        if auto_stop && !*shutdown.borrow() {
            let engine = state.validation.clone();
            let owned = session_id.clone();
            match tokio::task::spawn_blocking(move || engine.stop_if(&owned)).await {
                Ok(Some(s)) => {
                    log::info!(
                        "Validation session {} finalised on diagnostic completion ({} samples, \
                         {} evidence record(s))",
                        s.session_id,
                        s.samples.len(),
                        s.evidence.len()
                    );
                    // The governing rule for this whole hop: an auto-stop must
                    // be indistinguishable from the operator pressing Stop.
                    // `stop_session_handler` prunes after a successful finalise
                    // — a persisted session can push retention over its bound —
                    // and skipping it here would make the retained set depend on
                    // *how* a session ended.
                    prune_sessions_off_runtime().await;
                }
                // Stopped, cancelled or pre-empted between the last diagnostic
                // and here. Not an error: the operator got there first.
                Ok(None) => log::debug!(
                    "Validation session {session_id} was already finalised; nothing to stop"
                ),
                // The session stays installed and recording, which the operator
                // can still stop by hand and the boot sweep represents as
                // `interrupted`. Never a fabricated `completed`.
                Err(e) => log::warn!(
                    "Validation session {session_id} could not be finalised automatically: {e}"
                ),
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

/// What watching a detached diagnostic run to its end came to.
enum Watched<R> {
    /// The watch ended on the run's own terms. `Some(run)` is the terminal
    /// snapshot; `None` means the run left no such snapshot to attach —
    /// superseded by a successor, never installed, or the deadline elapsed with
    /// it still sweeping.
    Finished(Option<R>),
    /// The session that asked for this run stopped being the recording one
    /// while it was still going. The caller must return WITHOUT attaching
    /// evidence — see the fence note on `attach_evidence_for`.
    SessionEnded,
}

/// Watch a detached diagnostic run until it leaves `running`, then cancel it.
///
/// ONE definition of the watch, for the same reason [`cancel_run_fenced`] is one
/// definition of the fence (DEC-276). Both session-orchestrated diagnostics walk
/// an identical loop over a process-global slot, differing only in which
/// slot/cancel pair they watch and how their deadline is derived. That
/// duplication was not theoretical: DEC-344 had to apply one cancel fix twice,
/// once per copy — `P8-ap` and `P8-bk` were the same defect in the same file, in
/// two functions. Extracted by DEC-374 (`P8-bu`), which also retires the TWO
/// source-scanning guards that stood in for a runtime test — the one here that
/// pinned the fall-through cancel at both copies, and the one in
/// `tests/validation_phase5.rs` that pinned the session fence.
///
/// Every wait is a bounded poll against `deadline`, never a bare sleep.
///
/// Two cancels, and both are load-bearing:
///
/// * **The session fence** (`AUD3-j`). Returning alone left the detached sweep
///   still driving the header AND still renewing the engine's write-pause once
///   per point, so ending a session suspended curve control for up to the
///   sweep's full worst case after the user had ended it. Thermal safety still
///   outranked that — the forced-duty branch runs above the `verify_active`
///   gate — so it was lost control intent, never lost cooling; it was still a
///   diagnostic that outlived the thing that asked for it.
/// * **The fall-through** (`P8-ap`/`P8-bk`, DEC-344). The loop has four exits
///   and only the session-ended one used to cancel — but the deadline break is
///   the exit reached with our sweep STILL RUNNING. Unconditional here rather
///   than inside that break: the fence acts only on a running run whose
///   `run_id` is ours, so it is a no-op for the other three by construction.
///
/// Cancellation is only ever a REQUEST; the sweep observes it at its next step
/// boundary.
async fn watch_run<R: CancellableRun + Clone>(
    slot: &parking_lot::Mutex<Option<R>>,
    cancel: &std::sync::atomic::AtomicBool,
    run_id: Option<&str>,
    deadline: tokio::time::Instant,
    session_live: impl Fn() -> bool,
) -> Watched<R> {
    let mut final_run = None;
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let snap = slot.lock().clone();
        match snap {
            Some(run) if Some(run.run_id()) == run_id => {
                if !CancellableRun::is_running(&run) {
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
        // Stop if THIS session is no longer the live one...
        if !session_live() {
            // ...and take the sweep down with us (`AUD3-j`).
            cancel_run_fenced(slot, cancel, run_id);
            return Watched::SessionEnded;
        }
    }
    cancel_run_fenced(slot, cancel, run_id);
    Watched::Finished(final_run)
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
    let final_run = match watch_run(
        &state.characterization,
        &state.characterization_cancel,
        run_id.as_deref(),
        deadline,
        || state.validation.recording_session_id().as_deref() == Some(session_id),
    )
    .await
    {
        // The session that asked for this run is gone. `watch_run` has already
        // cancelled the sweep (`AUD3-j`); attaching evidence now would file it
        // against a session that is no longer recording.
        Watched::SessionEnded => return,
        Watched::Finished(run) => run,
    };

    let outcome = match &final_run {
        Some(run) if run.state == crate::api::characterization::STATE_COMPLETE => RESULT_OBSERVED,
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
    let final_run = match watch_run(
        &state.control_path,
        &state.control_path_cancel,
        run_id.as_deref(),
        deadline,
        || state.validation.recording_session_id().as_deref() == Some(session_id),
    )
    .await
    {
        // The session that asked for this run is gone. `watch_run` has already
        // cancelled the sweep (`AUD3-j`); attaching evidence now would file it
        // against a session that is no longer recording.
        Watched::SessionEnded => return,
        Watched::Finished(run) => run,
    };

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
            auto_started: false,
            stop_when_diagnostics_complete: false,
            startup_fingerprints: vec![],
            steady_state: None,
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

    // ── `P8-bu` (DEC-374): the watch loop, tested at the loop ────────
    //
    // Both rules below used to be pinned by source-scanning guards — one here
    // and one in `tests/validation_phase5.rs` — because neither watch loop was
    // reachable without stubbing `hwmon_characterize_handler`, which drives
    // hardware. Extracting `watch_run` removes that obstacle: the loop takes
    // its slot, its cancel flag, its deadline and its liveness predicate as
    // arguments, so the two exits that matter can be driven directly. The
    // guards are retired in favour of these.
    //
    // Time is virtual (`start_paused`), and that is sound here in a way it is
    // not everywhere: the loop ages against `tokio::time::Instant`, which
    // paused time DOES advance — unlike `std::time::Instant`, the trap
    // `CLAUDE.md` records. No `spawn_blocking` is outstanding, so auto-advance
    // is not inhibited either.

    /// `P8-ap`/`P8-bk` (DEC-344): the deadline exit must cancel.
    ///
    /// It is the one exit reached with our sweep STILL RUNNING. Without the
    /// cancel the detached sweep keeps driving the header and keeps renewing
    /// the engine's write-pause long after the orchestrator has stopped waiting
    /// and filed `RESULT_UNKNOWN` against it.
    ///
    /// The precondition matters as much as the assertion: without it a loop
    /// that fell out for some other reason would satisfy the cancel check and
    /// prove nothing about the deadline.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_exit_cancels_a_sweep_that_is_still_running() {
        let slot = parking_lot::Mutex::new(Some(run("char-11", ch::STATE_RUNNING)));
        let cancel = AtomicBool::new(false);
        let started = tokio::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs(2);

        let watched = watch_run(&slot, &cancel, Some("char-11"), deadline, || true).await;

        assert!(
            tokio::time::Instant::now() >= deadline,
            "precondition: the loop must actually have run to its deadline, or \
             this test is asserting about some other exit"
        );
        assert!(
            matches!(watched, Watched::Finished(None)),
            "a sweep still running at the deadline leaves no terminal snapshot \
             to attach — the caller files RESULT_UNKNOWN"
        );
        assert!(
            cancel.load(Ordering::SeqCst),
            "the sweep is still driving the header; the orchestrator must ask \
             it to stop before it walks away"
        );
    }

    /// The opposite branch, and it is what stops an unconditional `store(true)`
    /// satisfying the test above: a run that reached a terminal state on its own
    /// is attached and must NOT be cancelled. Its successor clears the flag
    /// under the slot lock at install, so arming it for a finished run is a race
    /// to lose.
    #[tokio::test(start_paused = true)]
    async fn a_run_that_finishes_while_watched_is_attached_and_not_cancelled() {
        let slot = std::sync::Arc::new(parking_lot::Mutex::new(Some(run(
            "char-12",
            ch::STATE_RUNNING,
        ))));
        let cancel = AtomicBool::new(false);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);

        // Flip it mid-watch rather than seeding a terminal run: the loop must be
        // shown to WATCH a running run until it leaves `running`, not merely to
        // read a slot that was already finished when it arrived.
        let flipper = slot.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            *flipper.lock() = Some(run("char-12", ch::STATE_COMPLETE));
        });

        let watched = watch_run(&slot, &cancel, Some("char-12"), deadline, || true).await;

        assert!(
            tokio::time::Instant::now() < deadline,
            "precondition: this must be the run-finished exit, not the deadline"
        );
        match watched {
            Watched::Finished(Some(r)) => assert_eq!(
                r.state,
                ch::STATE_COMPLETE,
                "the terminal snapshot is attached verbatim"
            ),
            _ => panic!("a run that finished must be attached"),
        }
        assert!(
            !cancel.load(Ordering::SeqCst),
            "a run that finished on its own needs no cancelling"
        );
    }

    /// `AUD3-j`: ending a session must end the sweep that session started.
    ///
    /// Returning alone left the detached sweep still driving the header AND
    /// still renewing the engine's write-pause once per point, so ending a
    /// session suspended curve control for up to
    /// `CHARACTERIZATION_MAX_POINTS × CHARACTERIZATION_SETTLE_MAX_S` after the
    /// user had ended it. Thermal safety still outranked that — the forced-duty
    /// branch runs above the `verify_active` gate — so it was lost control
    /// intent, never lost cooling; it was still a diagnostic that outlived the
    /// thing that asked for it.
    ///
    /// `SessionEnded` is the half the caller reads: it is what makes the
    /// orchestrator return WITHOUT attaching evidence to a session that is no
    /// longer recording.
    #[tokio::test(start_paused = true)]
    async fn ending_the_session_cancels_the_sweep_and_refuses_to_attach() {
        let slot = parking_lot::Mutex::new(Some(run("char-13", ch::STATE_RUNNING)));
        let cancel = AtomicBool::new(false);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);

        let watched = watch_run(&slot, &cancel, Some("char-13"), deadline, || false).await;

        assert!(
            tokio::time::Instant::now() < deadline,
            "precondition: the session fence must be what ended this watch, not \
             the deadline"
        );
        assert!(
            matches!(watched, Watched::SessionEnded),
            "the caller must be told to abandon the run rather than file it"
        );
        assert!(
            cancel.load(Ordering::SeqCst),
            "the sweep the ended session started must be asked to stop"
        );
    }

    /// The `run_id` fence, at the loop rather than at [`cancel_run_fenced`].
    ///
    /// A successor legally takes the slot once the predecessor's deadman
    /// elapses (DEC-296). Watching on past that point would attribute a
    /// stranger's evidence to this session — and cancelling would abort a sweep
    /// this session never started.
    #[tokio::test(start_paused = true)]
    async fn a_superseded_or_absent_run_is_neither_attached_nor_cancelled() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);

        let taken = parking_lot::Mutex::new(Some(run("char-15", ch::STATE_RUNNING)));
        let cancel = AtomicBool::new(false);
        let watched = watch_run(&taken, &cancel, Some("char-14"), deadline, || true).await;
        // Per half, not once at the end: both halves share one `deadline`, and
        // without this the test cannot tell the fence from the deadline.
        // Measured, not feared — with `Some(_) => break` changed to a no-op the
        // loop spins to the 30 s deadline, `cancel_run_fenced` no-ops anyway on
        // the id mismatch, and BOTH assertions below still hold. This test
        // passed with the fence it exists to pin removed.
        assert!(
            tokio::time::Instant::now() < deadline,
            "precondition: the `run_id` fence must be what ended this watch. \
             Reaching the deadline means the superseded arm stopped breaking, \
             which would stall the orchestration walk for the sweep's whole \
             worst case before filing RESULT_UNKNOWN"
        );
        assert!(
            matches!(watched, Watched::Finished(None)),
            "a successor's run is not ours to attach"
        );
        assert!(
            !cancel.load(Ordering::SeqCst),
            "a run this session did not start must never be cancelled"
        );

        let empty: parking_lot::Mutex<Option<ch::CharacterizationRun>> =
            parking_lot::Mutex::new(None);
        let cancel = AtomicBool::new(false);
        let watched = watch_run(&empty, &cancel, Some("char-14"), deadline, || true).await;
        assert!(
            tokio::time::Instant::now() < deadline,
            "precondition: the empty-slot arm must be what ended this watch"
        );
        assert!(
            matches!(watched, Watched::Finished(None)),
            "an empty slot leaves nothing to attach"
        );
        assert!(!cancel.load(Ordering::SeqCst));
    }

    /// `P8-bu` (DEC-374): the watch loop must stay ONE loop.
    ///
    /// The two orchestrated diagnostics used to carry a copy each, and the cost
    /// was not hypothetical — DEC-344 had to apply one cancel fix twice, once
    /// per copy. The four tests above exercise `watch_run`'s exits directly,
    /// but they would stay green over a third copy they never see, which is
    /// exactly how `P8-ap` and `P8-bk` came to be two rows instead of one.
    ///
    /// **Its limit, stated rather than glossed:** this is a drift tripwire, not
    /// a proof. A third loop written with a differently-named local would evade
    /// it. What it does catch is the cheap, likely mistake — copying an existing
    /// orchestrator and editing the slot — and the assertion message says what
    /// to do instead.
    ///
    /// Matched at statement indentation, never as a substring, and over the
    /// production half only: this guard's own prose would otherwise match it.
    #[test]
    fn the_slot_watch_loop_has_exactly_one_definition() {
        let src = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/api/handlers/validation.rs"
        ));
        let production = src.split("#[cfg(test)]").next().expect("production half");
        let lines: Vec<&str> = production.lines().collect();

        let watchers = lines
            .iter()
            .filter(|l| l.starts_with("async fn watch_run<"))
            .count();
        let loop_locals = lines
            .iter()
            .filter(|l| **l == "    let mut final_run = None;")
            .count();

        assert_eq!(
            watchers, 1,
            "there must be exactly one slot-watching helper"
        );
        assert_eq!(
            loop_locals, 1,
            "the slot-watch loop count is not 1. TWO different edits land here. \
             0: `watch_run`'s own `let mut final_run = None;` was renamed or \
             re-indented — re-point this guard, nothing is wrong. 2 or more: a \
             second watch loop has appeared, and a diagnostic that watches its \
             own run must call `watch_run` rather than hand-roll it — a copy \
             carries its own two cancels, and a fix to one does not reach the \
             other (`P8-ap`/`P8-bk`, DEC-344)."
        );

        // The PAIR matters as much as the presence, and this half is inherited
        // from the retired guard rather than invented: passing one diagnostic's
        // slot with the other's cancel flag takes down the wrong run, and it
        // COMPILES — both flags are `Arc<AtomicBool>` and deref-coerce
        // identically, while `R` is inferred from the slot alone (measured: a
        // crossed pair builds clean). Nothing else pins this — the unit tests
        // above never read the call sites, which is `CLAUDE.md`'s "extracting a
        // rule does not test the call site" in another coat.
        let call_sites: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim_end().ends_with("watch_run(") && !l.contains("async fn"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            call_sites.len(),
            2,
            "expected the two orchestrators (characterization, discovery); a \
             third would need its pairing checked here too: {call_sites:?}"
        );
        for i in call_sites {
            let slot = lines[i + 1]
                .trim()
                .trim_end_matches(',')
                .strip_prefix("&state.")
                .unwrap_or_else(|| {
                    panic!(
                        "the `watch_run` call at line {} must pass a `&state.` \
                         slot as its first argument, found: {}",
                        i + 2,
                        lines[i + 1]
                    )
                });
            assert_eq!(
                lines[i + 2].trim().trim_end_matches(','),
                format!("&state.{slot}_cancel"),
                "the `watch_run` call at line {} crosses its slot and cancel \
                 flag. It would ask the WRONG diagnostic to stop: the ended \
                 session's own sweep keeps driving the header and renewing the \
                 engine's write-pause (`AUD3-j`), while an unrelated run is \
                 aborted mid-restore. This compiles, and every other test passes.",
                i + 1
            );
        }
    }
}
