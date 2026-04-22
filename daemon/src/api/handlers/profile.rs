//! Profile management endpoints: active profile query, profile activation.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, AppState};
use crate::api::responses::*;

/// GET /profile/active — return the currently active profile, if any.
pub async fn active_profile_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let guard = state.active_profile.lock();
    match guard.as_ref() {
        Some(profile) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "api_version": API_VERSION,
                "active": true,
                "profile_id": profile.id,
                "profile_name": profile.name,
            })),
        ),
        None => (
            StatusCode::OK,
            Json(serde_json::json!({
                "api_version": API_VERSION,
                "active": false,
            })),
        ),
    }
}

/// POST /profile/activate — switch the active profile at runtime.
pub async fn activate_profile_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Accept either profile_id (search by name) or profile_path (direct file).
    // profile_path is restricted to known search directories to prevent
    // arbitrary filesystem reads (P1-R4 security hardening).
    let profile_path = if let Some(path) = body.get("profile_path").and_then(|v| v.as_str()) {
        let p = std::path::PathBuf::from(path);
        let canonical = match p.canonicalize() {
            Ok(c) => c,
            Err(_) => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!("profile path not found: {path}")),
                );
            }
        };
        // Canonicalize both sides to prevent symlink-based path traversal (CWE-22).
        // Skip search dirs that don't exist on disk (can't canonicalize).
        let search_dirs = state.profile_search_dirs.read();
        let allowed: Vec<std::path::PathBuf> = search_dirs
            .iter()
            .filter_map(|d| d.canonicalize().ok())
            .collect();
        if allowed.is_empty() {
            log::warn!(
                "No profile search directories exist on disk: {:?}",
                *search_dirs
            );
        }
        drop(search_dirs); // release lock before potentially long operations
        if !allowed.iter().any(|d| canonical.starts_with(d)) {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(
                    "profile_path must be within a profile search directory",
                ),
            );
        }
        canonical
    } else if let Some(id) = body.get("profile_id").and_then(|v| v.as_str()) {
        let search_dirs = state.profile_search_dirs.read();
        match crate::profile::find_profile(id, &search_dirs) {
            Some(p) => p,
            None => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &ErrorEnvelope::validation(format!("profile '{id}' not found in search paths")),
                );
            }
        }
    } else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("missing 'profile_id' or 'profile_path'"),
        );
    };

    // Load and validate
    let profile = match crate::profile::load_profile(&profile_path) {
        Ok(p) => p,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(e));
        }
    };

    let profile_name = profile.name.clone();
    let profile_id = profile.id.clone();

    // Apply
    {
        let mut guard = state.active_profile.lock();
        *guard = Some(profile);
    }

    // Treat activation as GUI activity so the profile engine defers writes
    // for the next GUI_ACTIVITY_TIMEOUT window. This gives the GUI exclusive
    // ownership of fan writes during a profile switch and prevents a dead
    // zone where neither writer is pushing values (see DEC on profile
    // activation deferral in CHANGELOG).
    state.cache.record_gui_write();

    // Persist
    let new_state = crate::daemon_state::DaemonState {
        version: 1,
        active_profile_id: Some(profile_id.clone()),
        active_profile_path: Some(profile_path.display().to_string()),
    };
    if let Err(e) = crate::daemon_state::save_state(&new_state) {
        log::warn!("Failed to persist profile state: {e}");
    }

    log::info!("Profile activated: '{profile_name}' (id={profile_id})");

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "api_version": API_VERSION,
            "activated": true,
            "profile_id": profile_id,
            "profile_name": profile_name,
        })),
    )
}
