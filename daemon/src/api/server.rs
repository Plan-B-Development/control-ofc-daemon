//! Unix socket HTTP server lifecycle.

use std::path::Path;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::net::UnixListener;

use super::handlers::{self, AppState};
use super::sse;

/// Error returned by [`serve`] when axum finishes unexpectedly.
pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

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
        // OpenFanController calibration sweep (diagnostic; daemon-performed).
        // The bare PWM/RPM write endpoints were retired at 2.0.0 (DEC-165) —
        // the profile engine is the sole writer.
        .route(
            "/fans/openfan/{channel}/calibrate",
            post(handlers::calibrate_openfan_handler),
        )
        // Capabilities
        .route("/capabilities", get(handlers::capabilities_handler))
        // GPU fan endpoints — the bare PWM write was retired at 2.0.0
        // (DEC-165); reset (daemon-mediated) + verify remain.
        .route(
            "/gpu/{gpu_id}/fan/reset",
            post(handlers::gpu_reset_fan_handler),
        )
        .route(
            "/gpu/{gpu_id}/fan/verify",
            post(handlers::gpu_verify_handler),
        )
        // Hwmon endpoints — the lease quartet and the bare PWM write were
        // retired at 2.0.0 (DEC-165): the engine self-leases and is the sole
        // writer. Header listing + verify (daemon-performed) remain.
        .route("/hwmon/headers", get(handlers::hwmon_headers_handler))
        .route(
            "/hwmon/{header_id}/verify",
            post(handlers::hwmon_verify_handler),
        )
        // Hwmon rescan
        .route("/hwmon/rescan", post(handlers::hwmon_rescan_handler))
        // Hardware diagnostics
        .route(
            "/diagnostics/hardware",
            get(handlers::hardware_diagnostics_handler),
        )
        // Manual override + fan identify (DEC-163 / DEC-166). axum 0.8 needs
        // method-chaining on a single route per path (duplicate paths panic).
        .route(
            "/control/{control_id}/override",
            post(handlers::override_take_handler).delete(handlers::override_release_handler),
        )
        .route(
            "/control/{control_id}/override/renew",
            post(handlers::override_renew_handler),
        )
        .route(
            "/fans/{fan_id}/identify",
            post(handlers::fan_identify_handler),
        )
        // Profile management
        .route("/profile/active", get(handlers::active_profile_handler))
        .route(
            "/profile/activate",
            post(handlers::activate_profile_handler),
        )
        .route(
            "/profile/deactivate",
            post(handlers::deactivate_profile_handler),
        )
        // Profile CRUD (DEC-160) — daemon-owned profile store. axum 0.8 needs
        // method-chaining on a single route per path (duplicate paths panic).
        .route(
            "/profiles",
            get(handlers::list_profiles_handler).post(handlers::create_profile_handler),
        )
        .route(
            "/profiles/{id}",
            get(handlers::get_profile_handler)
                .put(handlers::update_profile_handler)
                .delete(handlers::delete_profile_handler),
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

/// Serve axum over an already-bound Unix listener.
///
/// Binding, stale-socket removal, parent-dir creation, and the 0o666 chmod
/// all happen in `main::preflight_check` *before* any subsystem is spawned,
/// so that a bind failure is surfaced immediately as a fatal startup error
/// (see ADR-002 for the rationale — we don't want a half-started daemon
/// running polling loops with no one to talk to).
///
/// `socket_path` is kept around only for logging and for unlinking the
/// socket file on clean shutdown.
pub async fn serve(
    listener: UnixListener,
    socket_path: String,
    state: Arc<AppState>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), ServeError> {
    log::info!("IPC server listening on {socket_path}");

    let app = build_router(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
            log::info!("IPC server shutting down");
        })
        .await?;

    // Clean up socket file on clean shutdown.
    let path = Path::new(&socket_path);
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }

    Ok(())
}
