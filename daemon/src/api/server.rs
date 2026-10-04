//! Unix socket HTTP server lifecycle.

use std::path::Path;
use std::sync::Arc;

use axum::extract::connect_info::Connected;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use axum::serve::IncomingStream;
use axum::Router;
use tokio::net::UnixListener;

use super::handlers::{self, AppState};

/// Error returned by [`serve`] when axum finishes unexpectedly.
pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

/// Peer credentials of a Unix-socket client, captured at connection-accept time
/// and exposed to handlers as `ConnectInfo<UdsConnectInfo>` (DEC-205).
///
/// `uid` is the connecting process's effective user id read from `SO_PEERCRED`,
/// or `None` when it could not be read. Handlers treat `None` as untrusted and
/// fail closed. Consumed by `POST /config/profile-search-dirs` to confine a
/// non-root caller's added search directories to its own home directory on
/// multi-user hosts (the file-picker UX is preserved for single-user desktops
/// and for root/CLI callers, which are exempt).
#[derive(Clone, Debug)]
pub struct UdsConnectInfo {
    /// Effective uid of the peer, or `None` if `SO_PEERCRED` was unavailable.
    pub uid: Option<u32>,
}

impl<'a> Connected<IncomingStream<'a, UnixListener>> for UdsConnectInfo {
    fn connect_info(stream: IncomingStream<'a, UnixListener>) -> Self {
        // `io()` yields the accepted `UnixStream`; `peer_cred()` reads
        // SO_PEERCRED. A read failure degrades to `None` (fail-closed for the
        // caller) rather than dropping the connection.
        Self {
            uid: stream.io().peer_cred().ok().map(|cred| cred.uid()),
        }
    }
}

/// Build the axum router with all endpoints.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Read endpoints
        .route("/status", get(handlers::status_handler))
        .route("/sensors", get(handlers::sensors_handler))
        .route("/fans", get(handlers::fans_handler))
        .route("/poll", get(handlers::poll_handler))
        .route("/sensors/history", get(handlers::history_handler))
        // OpenFanController calibration (DEC-452): 202 + detached, like the
        // stall probe, on the SAME single verify slot. Capability-gated on
        // `control.openfan_calibration`. The bare PWM/RPM write endpoints were
        // retired at 2.0.0 (DEC-165) — the profile engine is the sole writer.
        .route(
            "/fans/openfan/{channel}/calibration",
            post(handlers::openfan_calibration_handler),
        )
        .route(
            "/diagnostics/openfan-calibration",
            get(handlers::openfan_calibration_status_handler)
                .delete(handlers::openfan_calibration_cancel_handler),
        )
        // DEPRECATED (DEC-452): the same run, held open until it ends.
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
        // AIO-MB Phase 3: the deeper PWM/RPM sweep that sits ALONGSIDE the quick
        // verify above (never in place of it). Returns 202 and runs detached;
        // the client polls the /diagnostics/characterization pair below.
        .route(
            "/hwmon/{header_id}/characterize",
            post(handlers::hwmon_characterize_handler),
        )
        .route(
            "/diagnostics/characterization",
            get(handlers::characterization_status_handler)
                .delete(handlers::characterization_cancel_handler),
        )
        // AIO Phase 8 Batch 1: read-only safety preflight. Takes no lease and no
        // slot, so calling it reserves nothing — the POST below still runs its
        // own guards. Capability-gated on `control.diagnostic_preflight`.
        .route(
            "/diagnostics/preflight",
            get(handlers::discovery::preflight_handler),
        )
        // AIO Phase 8 Batch 1: PWM to tach control-path discovery. Establishes
        // which output actually drives which tach by measurement rather than by
        // sysfs numbering. Returns 202 and runs detached, like characterise; it
        // claims the SAME single verify slot, so at most one of the four
        // diagnostics ever drives hardware. Capability-gated on
        // `control.control_path_discovery`.
        .route(
            "/hwmon/{header_id}/discover-control-path",
            post(handlers::discovery::discover_control_path_handler),
        )
        .route(
            "/diagnostics/control-path",
            get(handlers::discovery::control_path_status_handler)
                .delete(handlers::discovery::control_path_cancel_handler),
        )
        // DEC-407 (DEC-404 Stage 3): the stall/restart probe, the one diagnostic
        // that writes below 20 % on purpose. 202 + detached, like characterise,
        // on the SAME single verify slot. Capability-gated on
        // `control.stall_probe`.
        .route(
            "/hwmon/{header_id}/stall-probe",
            post(handlers::stall_probe::stall_probe_handler),
        )
        .route(
            "/diagnostics/stall-probe",
            get(handlers::stall_probe::stall_probe_status_handler)
                .delete(handlers::stall_probe::stall_probe_cancel_handler),
        )
        // AIO-MB Phase 5: validation sessions. A session RECORDS what an
        // already-configured cooler did, and may ORCHESTRATE the two diagnostics
        // above — it never writes a duty itself, so it adds no second PWM
        // ownership path (§2). Capability-gated on `control.validation_sessions`.
        .route(
            "/validation/session",
            post(handlers::validation::start_session_handler)
                .get(handlers::validation::get_session_handler)
                .delete(handlers::validation::cancel_session_handler),
        )
        .route(
            "/validation/session/stop",
            post(handlers::validation::stop_session_handler),
        )
        .route(
            "/validation/session/event",
            post(handlers::validation::post_event_handler),
        )
        .route(
            "/validation/session/measurement",
            post(handlers::validation::post_measurement_handler),
        )
        .route(
            "/validation/sessions",
            get(handlers::validation::list_sessions_handler),
        )
        .route(
            "/validation/sessions/{session_id}",
            get(handlers::validation::get_session_by_id_handler),
        )
        // Hwmon rescan
        .route("/hwmon/rescan", post(handlers::hwmon_rescan_handler))
        // DEC-265: adopt an OpenFanController that appeared after boot,
        // or that failed its identity probe once at startup.
        .route(
            "/fans/openfan/rescan",
            post(handlers::openfan_rescan_handler),
        )
        // `ROLE-f`: each OpenFan channel's role and pump protection.
        .route("/fans/openfan/roles", get(handlers::openfan_roles_handler))
        // DEC-481: the OpenFan firmware update.
        .route(
            "/fans/openfan/device",
            get(handlers::openfan_device_handler),
        )
        .route(
            "/fans/openfan/maintenance",
            get(handlers::openfan_maintenance_status_handler)
                .post(handlers::openfan_maintenance_start_handler)
                .delete(handlers::openfan_maintenance_cancel_handler),
        )
        // Hardware diagnostics
        .route(
            "/diagnostics/hardware",
            get(handlers::hardware_diagnostics_handler),
        )
        // Read-only hwmon inventory (Phase 1): structured CPU/motherboard
        // sensors, controllable PWM headers, and monitor-only fan tachometers
        // (fanN_input with no matching pwmN). Additive; never writes hardware.
        .route("/inventory/hwmon", get(handlers::hwmon_inventory_handler))
        // AIO-MB Phase 4: read side of the topology. On /inventory/* rather
        // than /config because GET /config has never carried per-device state.
        .route(
            "/inventory/cooling-devices",
            get(handlers::cooling_devices_handler),
        )
        // Structured hardware-readiness list (Phase 3): actionable diagnose-and-
        // guide items (severity + recommended action + blocks-flags). Read-only.
        .route(
            "/inventory/readiness",
            get(handlers::hwmon_readiness_handler),
        )
        // Combined readiness + Super-I/O snapshot (DEC-207): one atomic fetch for
        // the merged "Cooling Hardware Readiness" GUI page, served from a single
        // shared passive scan. Read-only; `?refresh=true` forces a fresh
        // (coalesced) scan. 404-gated on older daemons.
        .route(
            "/inventory/hardware-readiness",
            get(handlers::hardware_readiness_handler),
        )
        // Passive Super-I/O chip detection (DEC-202): per-chip presence +
        // allowlisted "load this driver" recommendations. Read-only — never
        // probes I/O ports, loads modules, or writes hardware. 404-gated.
        .route("/inventory/superio", get(handlers::superio_handler))
        // Opt-in ACTIVE Super-I/O port probe (DEC-203): a deliberate, one-shot
        // /dev/port read to identify an UNBOUND chip. Gated by
        // [detection] allow_port_probe + CAP_SYS_RAWIO; refuses ports owned by a
        // bound driver or ACPI. Off by default; reports availability truthfully.
        .route(
            "/inventory/superio/probe",
            post(handlers::superio_probe_handler),
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
        // Config management. GET /config is the read side (DEC-243) — before it
        // the writable knobs were write-only, so a client could only guess what
        // the daemon was actually configured with.
        .route("/config", get(handlers::get_config_handler))
        .route(
            "/config/profile-search-dirs",
            post(handlers::update_profile_search_dirs_handler),
        )
        .route(
            "/config/startup-delay",
            post(handlers::update_startup_delay_handler),
        )
        // DEC-388: the clean-stop exit floor. Applies live, not at next start.
        .route(
            "/config/exit-floor",
            post(handlers::update_exit_floor_handler),
        )
        // DEC-443: the coolant limit — applies live, like the exit floor.
        .route(
            "/config/coolant-limit",
            post(handlers::update_coolant_limit_handler),
        )
        // Persisted preferred CPU / motherboard sensor (Phase 5, DEC-200). Set
        // via {"sensor_id": "<id>"} or clear via {"sensor_id": null}. Advisory —
        // thermal safety still uses the hottest CpuTemp.
        .route(
            "/config/preferred-cpu-sensor",
            post(handlers::update_preferred_cpu_sensor_handler),
        )
        .route(
            "/config/preferred-mb-sensor",
            post(handlers::update_preferred_mb_sensor_handler),
        )
        // DEC-311 (AIO-MB Phase 1): assign a PWM header's role via
        // {"header_id": "<id>", "role": "pump"} or clear via {"role": null}.
        // NOT advisory, unlike the two above — a `pump` assignment earns the
        // header the 30% hard floor and pump-safe identify, and takes effect
        // immediately rather than at next start.
        .route(
            "/config/header-role",
            post(handlers::update_header_role_handler),
        )
        // AIO-MB Phase 4 (DEC-316): cooling-device topology. Metadata — no
        // engine path reads a device — so unlike the role setter above this
        // grants no floor and needs no follow-on safety action.
        .route(
            "/config/cooling-device",
            post(handlers::set_cooling_device_handler),
        )
        .route(
            "/config/cooling-device/{id}",
            delete(handlers::delete_cooling_device_handler),
        )
        // DEC-243: admin keys made runtime-mutable via the runtime.toml overlay
        // (ADR-002) rather than a privileged helper. All are start-only, so each
        // response says so and GET /config reports restart_pending per key. The
        // two [detection] opt-ins additionally need a root systemd drop-in that
        // no API can install — the responses state that rather than implying the
        // flag alone enables the feature.
        .route(
            "/config/poll-interval",
            post(handlers::update_poll_interval_handler),
        )
        .route(
            "/config/serial-port",
            post(handlers::update_serial_port_handler),
        )
        .route(
            "/config/serial-timeout",
            post(handlers::update_serial_timeout_handler),
        )
        .route(
            "/config/allow-port-probe",
            post(handlers::update_allow_port_probe_handler),
        )
        .route(
            "/config/nvidia-telemetry",
            post(handlers::update_nvidia_telemetry_handler),
        )
        .fallback(handlers::fallback_handler)
        // Explicit 4 MiB request-body cap (S1): profile POSTs are the only
        // large ingress; matches the file-read cap in `atomic_io`.
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(state)
}

/// Serve axum over an already-bound Unix listener.
///
/// Binding, stale-socket removal, parent-dir creation, and the 0o666 chmod
/// all happen in `main::preflight_socket` *before* any subsystem is spawned,
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

    // `into_make_service_with_connect_info` threads per-connection peer
    // credentials (SO_PEERCRED) to handlers via `ConnectInfo<UdsConnectInfo>`
    // (DEC-205). The stock `UnixListener` already implements axum's `Listener`,
    // so no custom listener is needed — only the connect-info make-service.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<UdsConnectInfo>(),
    )
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

#[cfg(test)]
mod tests {
    /// What a route does while an OpenFan firmware update holds the controller
    /// (DEC-481).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DuringUpdate {
        /// Refused: 409 `validation_error`, `details.reason: "openfan_maintenance"`.
        Refused,
        /// Refused only when it targets an OpenFan channel.
        RefusedForOpenFan,
        /// The update's own route.
        Itself,
        /// Answers as it always does: it neither drives the OpenFan controller
        /// nor claims the diagnostic slot an update holds.
        Unaffected,
    }
    use DuringUpdate::*;

    /// Every route the router serves, classified. A route missing from here
    /// fails [`every_route_says_what_it_does_during_a_firmware_update`] until
    /// someone decides (DEC-367): a new route that drives the OpenFan
    /// controller, or claims the diagnostic slot, must be refused while an
    /// update holds the controller. The refusals are proven live in
    /// `daemon/tests/ipc_integration.rs` (identify, override, rescan) and at
    /// `verify_slot_refusal`'s call sites.
    const ROUTES: &[(&str, &str, DuringUpdate)] = &[
        ("GET", "/status", Unaffected),
        ("GET", "/sensors", Unaffected),
        ("GET", "/fans", Unaffected),
        ("GET", "/poll", Unaffected),
        ("GET", "/sensors/history", Unaffected),
        ("POST", "/fans/openfan/{channel}/calibration", Refused),
        ("GET", "/diagnostics/openfan-calibration", Unaffected),
        ("DELETE", "/diagnostics/openfan-calibration", Unaffected),
        ("POST", "/fans/openfan/{channel}/calibrate", Refused),
        ("GET", "/capabilities", Unaffected),
        ("POST", "/gpu/{gpu_id}/fan/reset", Unaffected),
        ("POST", "/gpu/{gpu_id}/fan/verify", Refused),
        ("GET", "/hwmon/headers", Unaffected),
        ("POST", "/hwmon/{header_id}/verify", Refused),
        ("POST", "/hwmon/{header_id}/characterize", Refused),
        ("GET", "/diagnostics/characterization", Unaffected),
        ("DELETE", "/diagnostics/characterization", Unaffected),
        ("GET", "/diagnostics/preflight", Unaffected),
        ("POST", "/hwmon/{header_id}/discover-control-path", Refused),
        ("GET", "/diagnostics/control-path", Unaffected),
        ("DELETE", "/diagnostics/control-path", Unaffected),
        ("POST", "/hwmon/{header_id}/stall-probe", Refused),
        ("GET", "/diagnostics/stall-probe", Unaffected),
        ("DELETE", "/diagnostics/stall-probe", Unaffected),
        ("POST", "/validation/session", Refused),
        ("GET", "/validation/session", Unaffected),
        ("DELETE", "/validation/session", Unaffected),
        ("POST", "/validation/session/stop", Unaffected),
        ("POST", "/validation/session/event", Unaffected),
        ("POST", "/validation/session/measurement", Unaffected),
        ("GET", "/validation/sessions", Unaffected),
        ("GET", "/validation/sessions/{session_id}", Unaffected),
        ("POST", "/hwmon/rescan", Unaffected),
        ("POST", "/fans/openfan/rescan", Refused),
        ("GET", "/fans/openfan/roles", Unaffected),
        ("GET", "/fans/openfan/device", Itself),
        ("GET", "/fans/openfan/maintenance", Itself),
        ("POST", "/fans/openfan/maintenance", Itself),
        ("DELETE", "/fans/openfan/maintenance", Itself),
        ("GET", "/diagnostics/hardware", Unaffected),
        ("GET", "/inventory/hwmon", Unaffected),
        ("GET", "/inventory/cooling-devices", Unaffected),
        ("GET", "/inventory/readiness", Unaffected),
        ("GET", "/inventory/hardware-readiness", Unaffected),
        ("GET", "/inventory/superio", Unaffected),
        ("POST", "/inventory/superio/probe", Unaffected),
        ("POST", "/control/{control_id}/override", RefusedForOpenFan),
        // Release and renew stay open: a client can always let go, and a renew
        // of an override the update's start released is refused as stale.
        ("DELETE", "/control/{control_id}/override", Unaffected),
        ("POST", "/control/{control_id}/override/renew", Unaffected),
        ("POST", "/fans/{fan_id}/identify", RefusedForOpenFan),
        ("GET", "/profile/active", Unaffected),
        ("POST", "/profile/activate", Unaffected),
        ("POST", "/profile/deactivate", Unaffected),
        ("GET", "/profiles", Unaffected),
        ("POST", "/profiles", Unaffected),
        ("GET", "/profiles/{id}", Unaffected),
        ("PUT", "/profiles/{id}", Unaffected),
        ("DELETE", "/profiles/{id}", Unaffected),
        ("GET", "/config", Unaffected),
        ("POST", "/config/profile-search-dirs", Unaffected),
        ("POST", "/config/startup-delay", Unaffected),
        ("POST", "/config/exit-floor", Unaffected),
        ("POST", "/config/coolant-limit", Unaffected),
        ("POST", "/config/preferred-cpu-sensor", Unaffected),
        ("POST", "/config/preferred-mb-sensor", Unaffected),
        ("POST", "/config/header-role", Unaffected),
        ("POST", "/config/cooling-device", Unaffected),
        ("DELETE", "/config/cooling-device/{id}", Unaffected),
        // The serial and polling setters persist for the next start.
        ("POST", "/config/poll-interval", Unaffected),
        ("POST", "/config/serial-port", Unaffected),
        ("POST", "/config/serial-timeout", Unaffected),
        ("POST", "/config/allow-port-probe", Unaffected),
        ("POST", "/config/nvidia-telemetry", Unaffected),
    ];

    /// `(method, path)` for every `.route(` in `build_router`'s body — the
    /// function's own text, brace-matched, with `//` comments dropped, so this
    /// test's table and any comment naming a route are not read as routes.
    fn served_routes() -> Vec<(String, String)> {
        let src = include_str!("server.rs");
        let start = src.find("pub fn build_router").expect("build_router");
        let open = start + src[start..].find('{').expect("body");
        let mut depth = 0usize;
        let mut end = open;
        for (i, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body: String = src[open..end]
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let mut routes = Vec::new();
        let mut at = 0;
        while let Some(found) = body[at..].find(".route(") {
            let args_start = at + found + ".route(".len();
            let mut depth = 1usize;
            let mut args_end = args_start;
            for (i, c) in body[args_start..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            args_end = args_start + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let args = &body[args_start..args_end];
            let path = args.split('"').nth(1).expect("a route's path literal");
            for method in ["get", "post", "put", "delete", "patch"] {
                let call = format!("{method}(handlers::");
                if args
                    .match_indices(&call)
                    .any(|(i, _)| i == 0 || !args.as_bytes()[i - 1].is_ascii_alphanumeric())
                {
                    routes.push((method.to_ascii_uppercase(), path.to_string()));
                }
            }
            at = args_end;
        }
        routes
    }

    #[test]
    fn every_route_says_what_it_does_during_a_firmware_update() {
        let served = served_routes();
        assert!(
            served.len() >= 70,
            "precondition: the parse found the router's routes ({})",
            served.len()
        );
        let unclassified: Vec<_> = served
            .iter()
            .filter(|(m, p)| !ROUTES.iter().any(|(rm, rp, _)| rm == m && rp == p))
            .collect();
        assert!(
            unclassified.is_empty(),
            "classify these in ROUTES — does each one drive the OpenFan controller or \
             claim the diagnostic slot? {unclassified:?}"
        );
        let stale: Vec<_> = ROUTES
            .iter()
            .filter(|(m, p, _)| !served.iter().any(|(sm, sp)| sm == m && sp == p))
            .collect();
        assert!(stale.is_empty(), "no longer served: {stale:?}");
    }
}
