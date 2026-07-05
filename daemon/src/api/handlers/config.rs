//! Runtime config endpoints: profile search dirs, startup delay.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, AppState};
use crate::api::responses::*;

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
    let mut runtime = crate::runtime_config::RuntimeConfig::load_from(&state.runtime_config_path);
    runtime.set_profile_search_dirs(merged.clone());
    if let Err(e) = runtime.save_to(&state.runtime_config_path) {
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

    let mut runtime = crate::runtime_config::RuntimeConfig::load_from(&state.runtime_config_path);
    runtime.set_startup_delay_secs(delay);
    if let Err(e) = runtime.save_to(&state.runtime_config_path) {
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
fn set_preferred_sensor(
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
    let mut runtime = crate::runtime_config::RuntimeConfig::load_from(&state.runtime_config_path);
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
    if let Err(e) = runtime.save_to(&state.runtime_config_path) {
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
    set_preferred_sensor(&state, &body, PreferredSensorRole::Cpu)
}

/// POST /config/preferred-mb-sensor — set/clear the preferred motherboard sensor (Phase 5).
pub async fn update_preferred_mb_sensor_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    set_preferred_sensor(&state, &body, PreferredSensorRole::Mb)
}
