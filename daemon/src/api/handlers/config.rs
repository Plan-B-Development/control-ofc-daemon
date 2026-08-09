//! Configuration endpoints.
//!
//! Read: `GET /config` — the effective merged configuration (DEC-243).
//! Write: profile search dirs, startup delay, preferred sensors, and the
//! DEC-243 admin keys (poll interval, serial port/timeout, the two `[detection]`
//! opt-ins). Every write lands in `runtime.toml`, never in the admin-owned
//! `daemon.toml` (ADR-002), and is persist-first: a failed write returns
//! `503 persistence_failed` and changes nothing the daemon acts on.

use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, AppState};
use crate::api::responses::*;
use crate::api::server::UdsConnectInfo;
use crate::runtime_config::RuntimeConfig;

/// POST /config/profile-search-dirs — add directories to the profile search path.
///
/// Accepts `{"add": ["/path/to/profiles"]}` — each directory must be an absolute path.
/// The system directory `/etc/control-ofc/profiles` is always preserved.
///
/// Flow: persist runtime.toml first, then update in-memory state. If the
/// persist fails, the in-memory state is left untouched and the handler
/// returns 503 `persistence_failed` so the GUI can retry or surface the
/// error to the user. See ADR-002 for the rationale behind the two-file
/// config split.
pub async fn update_profile_search_dirs_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<UdsConnectInfo>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let add = body.get("add").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect::<Vec<_>>()
    });

    let Some(new_dirs) = add else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("missing 'add' array of absolute directory paths"),
        );
    };

    for d in &new_dirs {
        if !d.starts_with('/') {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!("search dir must be absolute: {d}")),
            );
        }
        if d.contains("..") {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!(
                    "search dir must not contain path traversal (..): {d}"
                )),
            );
        }
    }

    // Multi-user confinement (DEC-205): a non-root client may only add search
    // directories that exist within its own home directory. Root/CLI callers
    // are exempt. The peer uid comes from SO_PEERCRED via the connect-info
    // layer; an unresolvable uid/home fails closed.
    if let Err(msg) = super::path_confine::confine_added_dirs(
        &new_dirs,
        peer.uid,
        super::path_confine::home_dir_for_uid,
    ) {
        return error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(msg));
    }

    // NOTE (DEC-205 residual, security review F1): confinement above validates the
    // *canonical* dir at add-time, but we store the raw path (below) and
    // `activate_profile_handler` re-canonicalizes stored search dirs at use-time.
    // A caller who later swaps an approved in-home dir for a symlink out of their
    // home can thus redirect it. Accepted as a bounded residual: activation
    // validates the profile schema and never returns file contents, and any fan
    // output is clamped by the safety floors — the caller gains nothing they
    // cannot already do inside their own home. Fully closing it needs use-time
    // peer-uid confinement of activation (a change to the pre-existing activation
    // path), deferred as out of Wave-2 scope.

    // Merge with existing dirs (dedup, always keep /etc/control-ofc/profiles)
    let mut merged: Vec<String> = {
        let current = state.profile_search_dirs.read();
        current.iter().map(|p| p.display().to_string()).collect()
    };
    for d in &new_dirs {
        if !merged.contains(d) {
            merged.push(d.clone());
        }
    }

    // Persist first. On failure, leave in-memory state alone and return 503
    // so the caller sees a durable, actionable error rather than a silent
    // drift between in-memory and on-disk state.
    let mut runtime = match runtime_for_update(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    runtime.set_profile_search_dirs(merged.clone());
    // DEC-252/255: fsync off the async worker threads the engine shares.
    let runtime_owned = runtime.clone();
    let rc_path = state.runtime_config_path.clone();
    if let Err(e) = super::persist_off_runtime(move || runtime_owned.save_to(&rc_path)).await {
        log::error!(
            "Failed to persist profile search dirs to {}: {e}",
            state.runtime_config_path.display()
        );
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::persistence_failed("failed to persist runtime configuration"),
        );
    }

    // Persist succeeded — commit the in-memory update.
    let path_bufs: Vec<std::path::PathBuf> = merged.iter().map(std::path::PathBuf::from).collect();
    *state.profile_search_dirs.write() = path_bufs;

    log::info!("Profile search dirs updated: {:?}", merged);

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "api_version": API_VERSION,
            "updated": true,
            "search_dirs": merged,
        })),
    )
}

/// POST /config/startup-delay — set the daemon startup delay (takes effect on restart).
///
/// Persists to runtime.toml. Returns 503 `persistence_failed` if the write
/// fails, so the caller knows the setting did not stick. The daemon's live
/// startup delay is only consulted at process start, so there is no
/// in-memory state to roll back.
pub async fn update_startup_delay_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let delay = match body.get("delay_secs").and_then(|v| v.as_u64()) {
        Some(d) if d <= 30 => d,
        Some(d) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!("delay_secs must be 0-30, got {d}")),
            );
        }
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("missing 'delay_secs' (integer 0-30)"),
            );
        }
    };

    let mut runtime = match runtime_for_update(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    runtime.set_startup_delay_secs(delay);
    // DEC-252/255: fsync off the async worker threads the engine shares.
    let runtime_owned = runtime.clone();
    let rc_path = state.runtime_config_path.clone();
    if let Err(e) = super::persist_off_runtime(move || runtime_owned.save_to(&rc_path)).await {
        log::error!(
            "Failed to persist startup delay to {}: {e}",
            state.runtime_config_path.display()
        );
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::persistence_failed("failed to persist runtime configuration"),
        );
    }

    log::info!("Startup delay set to {delay}s (takes effect on restart)");

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "api_version": API_VERSION,
            "updated": true,
            "delay_secs": delay,
            "note": "Takes effect on next daemon restart",
        })),
    )
}

/// Which preferred-sensor slot a request targets (Phase 5).
enum PreferredSensorRole {
    Cpu,
    Mb,
}

/// Shared logic for `POST /config/preferred-{cpu,mb}-sensor`. Body:
/// `{"sensor_id": "<stable id>"}` to set, or `{"sensor_id": null}` to clear.
///
/// A set is validated against the live sensor set (unknown id → 400); staleness
/// after a later hardware change is surfaced by the readiness model, not here.
/// Persist-first like the sibling handlers: a write failure returns 503 and does
/// not change anything the daemon acts on (the preference is advisory — thermal
/// safety still uses the hottest CpuTemp).
async fn set_preferred_sensor(
    state: &AppState,
    body: &serde_json::Value,
    role: PreferredSensorRole,
) -> (StatusCode, Json<serde_json::Value>) {
    // Parse sensor_id: the key is required; a string sets, null clears.
    let new_id: Option<String> = match body.get("sensor_id") {
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(
                    "missing 'sensor_id' (a stable sensor id, or null to clear)",
                ),
            );
        }
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("'sensor_id' must be a string or null"),
            );
        }
    };

    // Validate a set id against the live sensor set.
    if let Some(ref id) = new_id {
        if !state.cache.snapshot().sensors.contains_key(id) {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!("unknown sensor id: {id}")),
            );
        }
    }

    // Persist-first, matching the sibling config handlers.
    let mut runtime = match runtime_for_update(state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let role_str = match role {
        PreferredSensorRole::Cpu => {
            runtime.set_preferred_cpu_sensor(new_id.clone());
            "cpu"
        }
        PreferredSensorRole::Mb => {
            runtime.set_preferred_mb_sensor(new_id.clone());
            "mb"
        }
    };
    // DEC-252/255: fsync off the async worker threads the engine shares. This
    // setter was a plain `fn`, which is why it was missed the first time.
    let runtime_owned = runtime.clone();
    let rc_path = state.runtime_config_path.clone();
    if let Err(e) = super::persist_off_runtime(move || runtime_owned.save_to(&rc_path)).await {
        log::error!(
            "Failed to persist preferred {role_str} sensor to {}: {e}",
            state.runtime_config_path.display()
        );
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::persistence_failed("failed to persist runtime configuration"),
        );
    }

    log::info!("Preferred {role_str} sensor set to {new_id:?}");

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "api_version": API_VERSION,
            "updated": true,
            "role": role_str,
            "preferred_sensor": new_id,
        })),
    )
}

/// POST /config/preferred-cpu-sensor — set/clear the preferred CPU temp sensor (Phase 5).
pub async fn update_preferred_cpu_sensor_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let resp = set_preferred_sensor(&state, &body, PreferredSensorRole::Cpu).await;
    refresh_rollup_if_ok(&state, resp.0);
    resp
}

/// POST /config/preferred-mb-sensor — set/clear the preferred motherboard sensor (Phase 5).
pub async fn update_preferred_mb_sensor_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let resp = set_preferred_sensor(&state, &body, PreferredSensorRole::Mb).await;
    refresh_rollup_if_ok(&state, resp.0);
    resp
}

/// DEC-206: after a successful preferred-sensor change, recompute the cached
/// readiness rollup on the blocking pool so the Dashboard health chip's
/// `selected_*_sensor_missing` state updates within a poll. Fire-and-forget — the
/// response does not depend on it and the change is already persisted.
fn refresh_rollup_if_ok(state: &Arc<AppState>, status: StatusCode) {
    if status == StatusCode::OK {
        let s = state.clone();
        // DEC-207: a preferred-sensor change invalidates the shared assessment
        // (readiness items depend on runtime.toml). Fire-and-forget, force-refresh
        // — the response does not depend on it; ensure_assessment coalesces the
        // scan off the poll path and logs its own failure.
        tokio::spawn(async move {
            let _ = super::ensure_assessment(s, true).await;
        });
    }
}

// ── DEC-243: readable + extended-writable daemon configuration ───────────

/// Build the effective on-disk config: admin `daemon.toml` with the
/// `runtime.toml` overlay applied. This is what a restart would produce.
///
/// Deliberately re-reads both files rather than reusing the running config —
/// the whole point of the endpoint is to expose the difference between what is
/// persisted and what is in effect.
fn effective_on_disk(state: &AppState) -> (crate::config::DaemonConfig, RuntimeConfig) {
    effective_on_disk_paths(&state.config_path, &state.runtime_config_path)
}

/// Path-based core of [`effective_on_disk`].
///
/// Public so `main`'s overlay tests can assert this merge and
/// `apply_runtime_overlay` agree. They are two independent implementations of
/// the same precedence rule: this one answers `GET /config`, the other decides
/// what the process actually runs on. If they drift, a setting persists,
/// reports `restart_pending` forever, and never applies.
pub fn effective_on_disk_paths(
    admin_path: &str,
    runtime_path: &std::path::Path,
) -> (crate::config::DaemonConfig, RuntimeConfig) {
    let mut cfg = crate::config::DaemonConfig::load(admin_path).unwrap_or_default();
    let runtime = RuntimeConfig::load_from(runtime_path);
    if let Some(dirs) = runtime.profile_search_dirs() {
        cfg.profiles.search_dirs = dirs.to_vec();
    }
    if let Some(d) = runtime.startup_delay_secs() {
        cfg.startup.delay_secs = d;
    }
    if let Some(p) = runtime.serial_port() {
        cfg.serial.port = Some(p.to_string());
    }
    if let Some(t) = runtime.serial_timeout_ms() {
        cfg.serial.timeout_ms = t;
    }
    if let Some(i) = runtime.poll_interval_ms() {
        cfg.polling.poll_interval_ms = i;
    }
    if let Some(a) = runtime.allow_port_probe() {
        cfg.detection.allow_port_probe = a;
    }
    if let Some(e) = runtime.enable_nvidia_telemetry() {
        cfg.detection.enable_nvidia_telemetry = e;
    }
    (cfg, runtime)
}

/// Assemble one `ConfigKey`, deriving `restart_pending` from disk-vs-running.
#[allow(clippy::too_many_arguments)]
fn config_key(
    key: &str,
    value: serde_json::Value,
    running: serde_json::Value,
    overridden: bool,
    is_admin_set: bool,
    mutable: bool,
    requires_restart: bool,
    requires_privilege: Option<&str>,
) -> ConfigKey {
    let pending = requires_restart && value != running;
    let source = if overridden {
        "runtime"
    } else if is_admin_set {
        "admin"
    } else {
        "default"
    };
    ConfigKey {
        key: key.to_string(),
        running_value: running,
        value,
        source: source.to_string(),
        mutable,
        requires_restart,
        restart_pending: pending,
        requires_privilege: requires_privilege.map(String::from),
    }
}

/// GET /config — the daemon's effective configuration (DEC-243).
///
/// Read-only. Contains no secrets: paths, intervals and booleans only.
pub async fn get_config_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (disk, runtime) = effective_on_disk(&state);
    let run = &state.running_config;
    // Parse the admin file to decide `source`: "admin" must mean *this exact
    // key* is set in daemon.toml, not merely that its section header appears
    // (a commented-out header or a section holding only sibling keys would
    // both fool a raw text match).
    let admin_doc: toml::Value = std::fs::read_to_string(&state.config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    let admin_has = |dotted: &str| -> bool {
        let mut node = &admin_doc;
        for part in dotted.split('.') {
            match node.get(part) {
                Some(next) => node = next,
                None => return false,
            }
        }
        true
    };

    let keys = vec![
        // The ONLY key that applies live: `update_profile_search_dirs_handler`
        // swaps `state.profile_search_dirs` in-process, and SIGHUP re-applies it
        // too. So `requires_restart` is false, and the running value is read
        // from that live lock rather than from `running_config` (which is frozen
        // at startup and would therefore report a restart as owed forever — the
        // GUI re-registers its profiles dir on every connect, so that false
        // banner would fire for essentially every user).
        config_key(
            "profiles.search_dirs",
            serde_json::json!(disk.profiles.search_dirs),
            serde_json::json!(state
                .profile_search_dirs
                .read()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()),
            runtime.profile_search_dirs().is_some(),
            admin_has("profiles.search_dirs"),
            true,
            false,
            None,
        ),
        config_key(
            "startup.delay_secs",
            serde_json::json!(disk.startup.delay_secs),
            serde_json::json!(run.startup.delay_secs),
            runtime.startup_delay_secs().is_some(),
            admin_has("startup.delay_secs"),
            true,
            true,
            None,
        ),
        config_key(
            "polling.poll_interval_ms",
            serde_json::json!(disk.polling.poll_interval_ms),
            serde_json::json!(run.polling.poll_interval_ms),
            runtime.poll_interval_ms().is_some(),
            admin_has("polling.poll_interval_ms"),
            true,
            true,
            None,
        ),
        config_key(
            "serial.port",
            serde_json::json!(disk.serial.port),
            serde_json::json!(run.serial.port),
            runtime.serial_port().is_some(),
            admin_has("serial.port"),
            true,
            true,
            None,
        ),
        config_key(
            "serial.timeout_ms",
            serde_json::json!(disk.serial.timeout_ms),
            serde_json::json!(run.serial.timeout_ms),
            runtime.serial_timeout_ms().is_some(),
            admin_has("serial.timeout_ms"),
            true,
            true,
            None,
        ),
        config_key(
            "detection.allow_port_probe",
            serde_json::json!(disk.detection.allow_port_probe),
            serde_json::json!(run.detection.allow_port_probe),
            runtime.allow_port_probe().is_some(),
            admin_has("detection.allow_port_probe"),
            true,
            true,
            Some(
                "also requires the CAP_SYS_RAWIO systemd drop-in \
                 (superio-port-probe.conf.example)",
            ),
        ),
        config_key(
            "detection.enable_nvidia_telemetry",
            serde_json::json!(disk.detection.enable_nvidia_telemetry),
            serde_json::json!(run.detection.enable_nvidia_telemetry),
            runtime.enable_nvidia_telemetry().is_some(),
            admin_has("detection.enable_nvidia_telemetry"),
            true,
            true,
            Some(
                "also requires the /dev/nvidia* systemd drop-in \
                 (nvidia-telemetry.conf.example)",
            ),
        ),
        // Read-only by design (DEC-243). Editing either from an unprivileged
        // client is self-destructive: a bad socket path locks every client out
        // of the daemon permanently, and moving state_dir orphans runtime.toml
        // and the daemon-owned profile store. Shown so they are diagnosable.
        config_key(
            "ipc.socket_path",
            serde_json::json!(disk.ipc.socket_path),
            serde_json::json!(run.ipc.socket_path),
            false,
            admin_has("ipc.socket_path"),
            false,
            true,
            None,
        ),
        config_key(
            "state.state_dir",
            serde_json::json!(disk.state.state_dir),
            serde_json::json!(run.state.state_dir),
            false,
            admin_has("state.state_dir"),
            false,
            true,
            None,
        ),
    ];

    let restart_pending = keys.iter().any(|k| k.restart_pending);
    let body = ConfigResponse {
        api_version: API_VERSION,
        admin_config_path: state.config_path.clone(),
        runtime_config_path: state.runtime_config_path.display().to_string(),
        restart_pending,
        keys,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(body).unwrap_or_else(|_| serde_json::json!({}))),
    )
}

/// Load runtime.toml for a setter, converting an unreadable existing file into a
/// 503 rather than letting the write erase it (DEC-252).
///
/// Every `POST /config/*` is load → mutate one key → save. Without this the
/// fallback-to-defaults inside `load_from` turns a failed read into a permanent
/// overwrite of every other setting.
fn runtime_for_update(
    state: &AppState,
) -> Result<RuntimeConfig, (StatusCode, Json<serde_json::Value>)> {
    RuntimeConfig::load_for_update(&state.runtime_config_path).map_err(|e| {
        log::error!("{e}");
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::persistence_failed(
                "existing runtime configuration could not be read; refusing to overwrite it",
            ),
        )
    })
}

/// Shared persist-and-report tail for the DEC-243 setters.
async fn persist_runtime(
    state: &AppState,
    runtime: &RuntimeConfig,
    key: &str,
    applied: serde_json::Value,
) -> (StatusCode, Json<serde_json::Value>) {
    // DEC-252: write + fsync + rename + directory fsync, off the async worker
    // threads the 1 Hz profile engine shares. See `persist_off_runtime`.
    let runtime_owned = runtime.clone();
    let path = state.runtime_config_path.clone();
    if let Err(e) = super::persist_off_runtime(move || runtime_owned.save_to(&path)).await {
        log::error!(
            "Failed to persist {key} to {}: {e}",
            state.runtime_config_path.display()
        );
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::persistence_failed("failed to persist runtime configuration"),
        );
    }
    log::info!("{key} set to {applied} (takes effect on restart)");
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "api_version": API_VERSION,
            "updated": true,
            "key": key,
            "value": applied,
            "note": "Takes effect on next daemon restart",
        })),
    )
}

/// POST /config/poll-interval — `{"poll_interval_ms": 250..=2000}`.
pub async fn update_poll_interval_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let ms = match body.get("poll_interval_ms").and_then(|v| v.as_u64()) {
        // Floor of 250 ms: the control loop, serial I/O and sysfs writes all run
        // on this cadence, so a tiny value is a self-inflicted denial of service
        // on the very hardware the daemon is meant to protect.
        //
        // [SAFETY] Ceiling of 2000 ms, deliberately tighter than what
        // `daemon.toml` accepts. This interval drives the sensor poll loop, and
        // the thermal-safety leg reads the cache with no age filter — so it
        // bounds how stale a temperature the 105 C emergency rule can act on.
        // `StalenessConfig` is itself derived from this value, so raising it
        // also widens what counts as "fresh" and nothing flags the degradation.
        // The admin file has no ceiling, but this endpoint is reachable by any
        // local user (the socket is 0666, DEC-049), so the API caps what the
        // hand-edited file does not.
        Some(v) if (250..=2_000).contains(&v) => v,
        Some(v) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!("poll_interval_ms must be 250-2000, got {v}")),
            );
        }
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("missing 'poll_interval_ms' (integer 250-2000)"),
            );
        }
    };
    let mut runtime = match runtime_for_update(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    runtime.set_poll_interval_ms(Some(ms));
    persist_runtime(
        &state,
        &runtime,
        "polling.poll_interval_ms",
        serde_json::json!(ms),
    )
    .await
}

/// POST /config/serial-port — `{"port": "/dev/ttyACM0"}` or `{"port": null}`
/// to clear the override and return to auto-detection.
pub async fn update_serial_port_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let port: Option<String> = match body.get("port") {
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(
                    "missing 'port' (a device path, or null to auto-detect)",
                ),
            );
        }
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => {
            // [SAFETY] Validate against the transport's OWN allowlist, not a
            // private `/dev/` test. The daemon opens this path as root, and the
            // endpoint is reachable by any local user (0666 socket, DEC-049).
            // A second copy of a security check drifts: the previous `/dev/`
            // test accepted `/dev/shm/...` and `/dev/mqueue/...` — the only
            // world-writable, symlink-capable directories under /dev — which
            // `RealSerialTransport::open` then rejected anyway.
            if !crate::serial::real_transport::is_allowed_serial_path(s) {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorEnvelope::validation(format!(
                        "serial port must be a supported serial device path: {s}"
                    )),
                );
            }
            // Bound the length: an oversized value would push runtime.toml past
            // the 4 MiB read cap, after which `load_from` treats the file as
            // malformed and silently reverts EVERY runtime setting to defaults —
            // which the next successful write then makes permanent.
            if s.len() > 256 {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorEnvelope::validation("serial port path must be 256 characters or fewer"),
                );
            }
            Some(s.clone())
        }
        Some(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("'port' must be a string or null"),
            );
        }
    };
    let mut runtime = match runtime_for_update(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    runtime.set_serial_port(port.clone());
    persist_runtime(&state, &runtime, "serial.port", serde_json::json!(port)).await
}

/// POST /config/serial-timeout — `{"timeout_ms": 50..=1000}`.
pub async fn update_serial_timeout_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let ms = match body.get("timeout_ms").and_then(|v| v.as_u64()) {
        // [SAFETY] Ceiling of 1000 ms, tighter than the admin file allows. An
        // emergency `force_all` awaits the OpenFan backend before the hwmon one,
        // costing up to `channels x timeout` on a wedged serial link — at 5000 ms
        // that is ~40 s for 8 channels, during which no further safety
        // evaluation runs. Same reasoning as the poll ceiling: the API is
        // unprivileged-reachable, the file is not.
        Some(v) if (50..=1_000).contains(&v) => v,
        Some(v) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!("timeout_ms must be 50-1000, got {v}")),
            );
        }
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("missing 'timeout_ms' (integer 50-1000)"),
            );
        }
    };
    let mut runtime = match runtime_for_update(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    runtime.set_serial_timeout_ms(Some(ms));
    persist_runtime(&state, &runtime, "serial.timeout_ms", serde_json::json!(ms)).await
}

/// Shared body parse for the two `[detection]` opt-ins.
fn parse_enabled(body: &serde_json::Value) -> Result<bool, (StatusCode, Json<serde_json::Value>)> {
    match body.get("enabled") {
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(_) => Err(error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("'enabled' must be a boolean"),
        )),
        None => Err(error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("missing 'enabled' (boolean)"),
        )),
    }
}

/// POST /config/allow-port-probe — `{"enabled": bool}` (DEC-203 opt-in).
///
/// Persisting `true` is only half of enabling the probe: it also needs the
/// `CAP_SYS_RAWIO` systemd drop-in, which this unprivileged-reachable endpoint
/// cannot install. The response says so explicitly so a client cannot honestly
/// report the feature as on.
pub async fn update_allow_port_probe_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let enabled = match parse_enabled(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let mut runtime = match runtime_for_update(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    runtime.set_allow_port_probe(Some(enabled));
    let (status, mut resp) = persist_runtime(
        &state,
        &runtime,
        "detection.allow_port_probe",
        serde_json::json!(enabled),
    )
    .await;
    if status == StatusCode::OK && enabled {
        if let Some(obj) = resp.0.as_object_mut() {
            obj.insert(
                "requires_privilege".into(),
                serde_json::json!(
                    "also requires the CAP_SYS_RAWIO systemd drop-in \
                     (superio-port-probe.conf.example)"
                ),
            );
        }
    }
    (status, resp)
}

/// POST /config/nvidia-telemetry — `{"enabled": bool}` (DEC-204 opt-in).
///
/// Same half-a-requirement caveat as the port probe: the NVML backend also needs
/// the `/dev/nvidia*` drop-in.
pub async fn update_nvidia_telemetry_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let enabled = match parse_enabled(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let mut runtime = match runtime_for_update(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    runtime.set_enable_nvidia_telemetry(Some(enabled));
    let (status, mut resp) = persist_runtime(
        &state,
        &runtime,
        "detection.enable_nvidia_telemetry",
        serde_json::json!(enabled),
    )
    .await;
    if status == StatusCode::OK && enabled {
        if let Some(obj) = resp.0.as_object_mut() {
            obj.insert(
                "requires_privilege".into(),
                serde_json::json!(
                    "also requires the /dev/nvidia* systemd drop-in \
                     (nvidia-telemetry.conf.example)"
                ),
            );
        }
    }
    (status, resp)
}
