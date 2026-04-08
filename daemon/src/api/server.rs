//! Unix socket HTTP server lifecycle.

use std::path::Path;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::net::UnixListener;

use super::handlers::{self, AppState};
use super::sse;

/// Build the axum router with all endpoints.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Read endpoints
        .route("/status", get(handlers::status_handler))
        .route("/sensors", get(handlers::sensors_handler))
        .route("/fans", get(handlers::fans_handler))
        .route("/poll", get(handlers::poll_handler))
        .route("/sensors/history", get(handlers::history_handler))
        // Server-Sent Events
        .route("/events", get(sse::events_handler))
        // Write endpoints (OpenFanController)
        .route(
            "/fans/openfan/{channel}/pwm",
            post(handlers::set_pwm_handler),
        )
        .route("/fans/openfan/pwm", post(handlers::set_pwm_all_handler))
        .route(
            "/fans/openfan/{channel}/target_rpm",
            post(handlers::set_target_rpm_handler),
        )
        .route(
            "/fans/openfan/{channel}/calibrate",
            post(handlers::calibrate_openfan_handler),
        )
        // Capabilities
        .route("/capabilities", get(handlers::capabilities_handler))
        // GPU fan endpoints
        .route("/gpu/{gpu_id}/fan/pwm", post(handlers::gpu_set_fan_handler))
        .route(
            "/gpu/{gpu_id}/fan/reset",
            post(handlers::gpu_reset_fan_handler),
        )
        // Hwmon PWM endpoints
        .route("/hwmon/headers", get(handlers::hwmon_headers_handler))
        .route(
            "/hwmon/lease/take",
            post(handlers::hwmon_lease_take_handler),
        )
        .route(
            "/hwmon/lease/release",
            post(handlers::hwmon_lease_release_handler),
        )
        .route(
            "/hwmon/lease/status",
            get(handlers::hwmon_lease_status_handler),
        )
        .route(
            "/hwmon/lease/renew",
            post(handlers::hwmon_lease_renew_handler),
        )
        .route(
            "/hwmon/{header_id}/pwm",
            post(handlers::hwmon_set_pwm_handler),
        )
        // Hwmon rescan
        .route("/hwmon/rescan", post(handlers::hwmon_rescan_handler))
        // Profile management
        .route("/profile/active", get(handlers::active_profile_handler))
        .route(
            "/profile/activate",
            post(handlers::activate_profile_handler),
        )
        // Config management
        .route(
            "/config/profile-search-dirs",
            post(handlers::update_profile_search_dirs_handler),
        )
        .route(
            "/config/startup-delay",
            post(handlers::update_startup_delay_handler),
        )
        .fallback(handlers::fallback_handler)
        .with_state(state)
}

/// Start the IPC server on a Unix socket.
///
/// This function:
/// 1. Creates the parent directory if needed
/// 2. Removes any stale socket file
/// 3. Binds and serves until the shutdown signal fires
pub async fn serve(
    socket_path: &str,
    state: Arc<AppState>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = Path::new(socket_path);

    // Create parent directory if needed
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            log::info!("Created socket directory: {}", parent.display());
        }
    }

    // Remove stale socket file
    if path.exists() {
        std::fs::remove_file(path)?;
        log::info!("Removed stale socket: {socket_path}");
    }

    let listener = UnixListener::bind(path)?;

    // DEC-049: World-writable (0o666) to allow non-root GUI connections.
    // The daemon is local-only (Unix socket, not TCP-exposed). Any local user
    // can connect and issue commands including fan writes. On multi-user systems,
    // restrict to a dedicated group (0o660) and add GUI users to that group.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    }

    log::info!("IPC server listening on {socket_path}");

    let app = build_router(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
            log::info!("IPC server shutting down");
        })
        .await?;

    // Clean up socket file
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }

    Ok(())
}
