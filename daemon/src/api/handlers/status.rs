//! Read-only status endpoints: status, sensors, fans, poll, capabilities, history, fallback.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{
    build_control_output_entries, build_fan_entries, build_sensor_entries, build_skipped_entries,
    build_status_response, build_unavailable_entries, error_response, json_ok, AppState,
};
use crate::api::responses::*;
use crate::health::staleness::{compute_health, OpenFanPresence};

/// Thermal-state field for a status response, defaulting to `"normal"` before
/// the engine's first tick. Extracted under the cache read guard so
/// `build_status_response` (which locks `override_table`) needs no snapshot
/// and no cache guard of its own (EFF-1).
fn thermal_state_of(snap: &crate::health::state::DaemonState) -> String {
    snap.thermal_override_state
        .clone()
        .unwrap_or_else(|| "normal".to_string())
}

/// Whether an OpenFanController is attached (OFS-j).
///
/// The same signal `GET /capabilities` reports as `devices.openfan.present`. It
/// lives on `AppState` because adoption owns it; `compute_health` is pure over
/// `DaemonState` and must be told.
fn openfan_presence(state: &AppState) -> OpenFanPresence {
    if state.openfan().is_some() {
        OpenFanPresence::Present
    } else {
        OpenFanPresence::Absent
    }
}

/// GET /status — overall health and subsystem freshness.
pub async fn status_handler(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let now = Instant::now();
    // OFS-j: resolved outside the cache guard — controller presence is AppState,
    // not `DaemonState`, and cannot be derived from the latter (an empty
    // `openfan_fans` also describes a controller adopted but not yet polled).
    let openfan = openfan_presence(&state);
    // EFF-1: read the state once under a shared guard instead of cloning the
    // whole `DaemonState`. Only pure reads happen inside; the override_table
    // lock in `build_status_response` stays outside the guard.
    let (health, thermal_state, unavailable, skipped, outputs) = state.cache.read_with(|snap| {
        (
            compute_health(snap, &state.staleness_config, now, openfan),
            thermal_state_of(snap),
            build_unavailable_entries(snap, now),
            build_skipped_entries(snap, now),
            build_control_output_entries(snap),
        )
    });
    Json(build_status_response(
        &state,
        thermal_state,
        unavailable,
        skipped,
        outputs,
        health,
    ))
}

/// GET /sensors — cached sensor readings.
pub async fn sensors_handler(State(state): State<Arc<AppState>>) -> Json<SensorsResponse> {
    let now = Instant::now();
    Json(SensorsResponse {
        api_version: API_VERSION,
        sensors: state
            .cache
            .read_with(|snap| build_sensor_entries(snap, now)),
    })
}

/// GET /fans — cached fan state (OpenFanController + hwmon).
pub async fn fans_handler(State(state): State<Arc<AppState>>) -> Json<FansResponse> {
    let now = Instant::now();
    Json(FansResponse {
        api_version: API_VERSION,
        fans: state.cache.read_with(|snap| build_fan_entries(snap, now)),
    })
}

/// GET /poll — combined sensors, fans, and status in one response.
pub async fn poll_handler(State(state): State<Arc<AppState>>) -> Json<PollResponse> {
    let now = Instant::now();
    // EFF-1: build everything that needs `DaemonState` under one read guard, so
    // the most frequent request (the GUI polls /poll at 1 Hz) no longer clones
    // the entire state. The `override_table` lock lives in
    // `build_status_response`, kept outside this guard to preserve lock order.
    let openfan = openfan_presence(&state);
    let (health, thermal_state, unavailable, skipped, outputs, sensors, fans) =
        state.cache.read_with(|snap| {
            (
                compute_health(snap, &state.staleness_config, now, openfan),
                thermal_state_of(snap),
                build_unavailable_entries(snap, now),
                build_skipped_entries(snap, now),
                build_control_output_entries(snap),
                build_sensor_entries(snap, now),
                build_fan_entries(snap, now),
            )
        });

    Json(PollResponse {
        api_version: API_VERSION,
        status: build_status_response(&state, thermal_state, unavailable, skipped, outputs, health),
        sensors,
        fans,
    })
}

/// GET /capabilities — describe what the daemon can do on this machine.
pub async fn capabilities_handler(
    State(state): State<Arc<AppState>>,
) -> Json<CapabilitiesResponse> {
    let openfan_present = state.openfan().is_some();
    let hwmon_present = state.hwmon_controller.is_some();
    let hwmon_header_count = state
        .hwmon_controller
        .as_ref()
        .map(|c| c.lock().headers().len())
        .unwrap_or(0);

    // AMD GPU detection
    let primary_gpu = crate::hwmon::gpu_detect::select_primary_gpu(&state.amd_gpus);
    let amd_gpu_cap = if let Some(gpu) = primary_gpu {
        // Fan write requires either PMFW fan_curve or legacy hwmon pwm1+enable.
        // The legacy half is canonicalised in `AmdGpuInfo::can_write_legacy_pwm`
        // so handlers and capability scoring agree on the same rule (DEC-098).
        let fan_write = gpu.fan_curve_path.is_some() || gpu.can_write_legacy_pwm();
        let kernel_warnings = match crate::hwmon::kernel_warnings::read_kernel_release() {
            Some(release) => crate::hwmon::kernel_warnings::detect_kernel_warnings(&release, gpu),
            None => Vec::new(),
        };
        AmdGpuCapability {
            present: true,
            model_name: gpu.marketing_name.clone(),
            display_label: gpu.display_label(),
            // M11: emit both names during the transition. Same BDF string.
            pci_id: Some(gpu.pci_bdf.clone()),
            pci_bdf: Some(gpu.pci_bdf.clone()),
            pci_device_id: Some(gpu.pci_device_id),
            pci_revision: Some(gpu.pci_revision),
            fan_control_method: gpu.fan_control_method().to_string(),
            pmfw_supported: gpu.fan_curve_path.is_some(),
            fan_rpm_available: gpu.has_fan_rpm,
            fan_write_supported: fan_write,
            is_discrete: gpu.is_discrete,
            overdrive_enabled: gpu.overdrive_enabled,
            gpu_zero_rpm_available: gpu.fan_zero_rpm_path.is_some(),
            kernel_warnings,
        }
    } else {
        AmdGpuCapability {
            present: false,
            model_name: None,
            display_label: "AMD D-GPU".to_string(),
            pci_id: None,
            pci_bdf: None,
            pci_device_id: None,
            pci_revision: None,
            fan_control_method: "none".to_string(),
            pmfw_supported: false,
            fan_rpm_available: false,
            fan_write_supported: false,
            is_discrete: false,
            overdrive_enabled: false,
            gpu_zero_rpm_available: false,
            kernel_warnings: Vec::new(),
        }
    };

    // Intel discrete GPU detection (DEC-121) — read-only monitoring only.
    let intel_gpu_cap =
        match crate::hwmon::intel_gpu_detect::select_primary_intel_gpu(&state.intel_gpus) {
            Some(gpu) => IntelGpuCapability {
                present: true,
                model_name: gpu.marketing_name.clone(),
                display_label: gpu.display_label(),
                pci_id: Some(gpu.pci_bdf.clone()),
                pci_bdf: Some(gpu.pci_bdf.clone()),
                pci_device_id: Some(gpu.pci_device_id),
                driver: Some(gpu.driver.clone()),
                fan_control_method: gpu.fan_control_method().to_string(),
                fan_rpm_available: gpu.has_fan_rpm,
                is_discrete: gpu.is_discrete,
            },
            None => IntelGpuCapability {
                present: false,
                model_name: None,
                display_label: "Intel D-GPU".to_string(),
                pci_id: None,
                pci_bdf: None,
                pci_device_id: None,
                driver: None,
                fan_control_method: "none".to_string(),
                fan_rpm_available: false,
                is_discrete: false,
            },
        };

    // NVIDIA discrete GPU detection (DEC-204) — read-only monitoring only
    // (nouveau hwmon leg + opt-in NVML leg, unified in `state.nvidia_gpus`).
    let nvidia_gpu_cap = match crate::hwmon::nvidia::select_primary_nvidia_gpu(&state.nvidia_gpus) {
        Some(gpu) => NvidiaGpuCapability {
            present: true,
            model_name: gpu.model_name.clone(),
            display_label: gpu.display_label(),
            pci_id: Some(gpu.pci_bdf.clone()),
            pci_bdf: Some(gpu.pci_bdf.clone()),
            driver: Some(gpu.driver.to_string()),
            driver_version: gpu.driver_version.clone(),
            fan_control_method: gpu.fan_control_method().to_string(),
            fan_rpm_available: gpu.fan_rpm_available,
            is_discrete: true,
        },
        None => NvidiaGpuCapability {
            present: false,
            model_name: None,
            display_label: "NVIDIA D-GPU".to_string(),
            pci_id: None,
            pci_bdf: None,
            driver: None,
            driver_version: None,
            fan_control_method: "none".to_string(),
            fan_rpm_available: false,
            is_discrete: false,
        },
    };

    // AIO (liquid cooler) hwmon capability — dynamic since 1.18.0 (DEC-156).
    // Pump writability is header-driven (available immediately at startup);
    // coolant sensing is read from the cache. USB-only coolers stay out of
    // scope and are reported via `aio_usb` (always unsupported).
    let (aio_total, aio_writable) = state
        .hwmon_controller
        .as_ref()
        .map(|c| {
            c.lock()
                .headers()
                .iter()
                .filter(|h| h.is_aio)
                .fold((0usize, 0usize), |(total, writable), h| {
                    (total + 1, writable + usize::from(h.is_writable))
                })
        })
        .unwrap_or((0, 0));
    let coolant_available = state
        .cache
        .sensors_snapshot()
        .values()
        .any(|s| s.kind == crate::hwmon::types::SensorKind::CoolantTemp);
    let aio_hwmon_cap =
        AioHwmonCapability::from_discovery(aio_total, aio_writable, coolant_available);

    Json(CapabilitiesResponse {
        api_version: API_VERSION,
        daemon_version: state.daemon_version.clone(),
        ipc_transport: "uds/http",
        devices: DeviceCapabilities {
            openfan: OpenfanCapability {
                present: openfan_present,
                channels: 10,
                rpm_support: true,
                write_support: openfan_present,
            },
            hwmon: HwmonCapability {
                present: hwmon_present,
                pwm_header_count: hwmon_header_count,
                write_support: hwmon_present,
            },
            amd_gpu: amd_gpu_cap,
            intel_gpu: intel_gpu_cap,
            nvidia_gpu: nvidia_gpu_cap,
            aio_hwmon: aio_hwmon_cap,
            aio_usb: UnsupportedCapability {
                present: false,
                status: "unsupported",
            },
        },
        features: FeatureFlags {
            openfan_write_supported: openfan_present,
            hwmon_write_supported: hwmon_present,
        },
        limits: Limits {
            pwm_percent_min: 0,
            pwm_percent_max: 100,
            // Legacy floor fields removed — thermal safety centralized.
            // Derived from the constant the stop path actually uses, not a
            // literal: a hardcoded 8 here silently drifts the moment
            // STOP_TIMEOUT changes, and clients size their identify/stop UI
            // timeouts from this advertised value.
            // Saturating rather than `as u8`: a raw cast would silently wrap a
            // future STOP_TIMEOUT above 255 s into a tiny advertised value.
            openfan_stop_timeout_s: u8::try_from(crate::constants::STOP_TIMEOUT.as_secs())
                .unwrap_or(u8::MAX),
        },
        // Control-execution capability (DEC-159/160). 1.20.0 delivered daemon-
        // owned profile storage; 1.21.0 added the manual-override (DEC-163) and
        // fan-identify (DEC-166) APIs. The 2.0.0 cutover (DEC-165) makes the
        // engine the sole writer: `autonomous_control` flips true and
        // `min_supported_gui` enforces the GUI floor for the legible hard-fail.
        control: ControlCapability {
            profile_storage: true,
            curve_evaluation: true,
            manual_override: true,
            fan_identify: true,
            autonomous_control: true,
            min_supported_gui: "2.0.0".into(),
            openfan_rescan: true,
            profile_search_dir_remove: true,
            // DEC-311 (AIO-MB Phase 1): role classification, pump-safe identify,
            // role-aware verify, and POST /config/header-role.
            header_roles: true,
            // AIO-MB Phase 3: PWM/RPM response characterisation.
            pwm_characterization: true,
            // AIO-MB Phase 4 (DEC-316), daemon >= 2.31.0. Gates the three
            // topology endpoints only — the additive header fields that shipped
            // with them are optional on the wire and need no flag.
            cooling_devices: true,
        },
    })
}

/// GET /sensors/history — time-series history for a sensor entity.
pub async fn history_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let entity_id = match params.get("id") {
        Some(id) => id.clone(),
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation("missing 'id' query parameter"),
            );
        }
    };
    let last: usize = params
        .get("last")
        .and_then(|s| s.parse().ok())
        .unwrap_or(250)
        .min(1000);

    let points = state.history.get_last(&entity_id, last);
    json_ok(
        StatusCode::OK,
        HistoryResponse {
            api_version: API_VERSION,
            entity_id,
            points,
        },
    )
}

/// Fallback handler for unknown routes.
pub async fn fallback_handler(uri: axum::http::Uri) -> (StatusCode, Json<ErrorEnvelope>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorEnvelope::not_found(uri.path())),
    )
}
