//! Read-only status endpoints: status, sensors, fans, poll, capabilities, history, fallback.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{
    build_fan_entries, build_sensor_entries, build_status_response, error_response, json_ok,
    AppState,
};
use crate::api::responses::*;
use crate::health::staleness::compute_health;

/// GET /status — overall health and subsystem freshness.
pub async fn status_handler(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let snap = state.cache.snapshot();
    let now = Instant::now();
    let health = compute_health(&snap, &state.staleness_config, now);
    Json(build_status_response(&state, &snap, health, now))
}

/// GET /sensors — cached sensor readings.
pub async fn sensors_handler(State(state): State<Arc<AppState>>) -> Json<SensorsResponse> {
    let snap = state.cache.snapshot();
    let now = Instant::now();

    Json(SensorsResponse {
        api_version: API_VERSION,
        sensors: build_sensor_entries(&snap, now),
    })
}

/// GET /fans — cached fan state (OpenFanController + hwmon).
pub async fn fans_handler(State(state): State<Arc<AppState>>) -> Json<FansResponse> {
    let snap = state.cache.snapshot();
    let now = Instant::now();

    Json(FansResponse {
        api_version: API_VERSION,
        fans: build_fan_entries(&snap, now),
    })
}

/// GET /poll — combined sensors, fans, and status in one response.
pub async fn poll_handler(State(state): State<Arc<AppState>>) -> Json<PollResponse> {
    let snap = state.cache.snapshot();
    let now = Instant::now();
    let health = compute_health(&snap, &state.staleness_config, now);

    Json(PollResponse {
        api_version: API_VERSION,
        status: build_status_response(&state, &snap, health, now),
        sensors: build_sensor_entries(&snap, now),
        fans: build_fan_entries(&snap, now),
    })
}

/// GET /capabilities — describe what the daemon can do on this machine.
pub async fn capabilities_handler(
    State(state): State<Arc<AppState>>,
) -> Json<CapabilitiesResponse> {
    let openfan_present = state.fan_controller.is_some();
    let hwmon_present = state.hwmon_controller.is_some();
    let hwmon_header_count = state
        .hwmon_controller
        .as_ref()
        .map(|c| c.lock().headers().len())
        .unwrap_or(0);

    // AMD GPU detection
    let primary_gpu = crate::hwmon::gpu_detect::select_primary_gpu(&state.amd_gpus);
    let amd_gpu_cap = if let Some(gpu) = primary_gpu {
        // Fan write requires either PMFW fan_curve or legacy hwmon pwm1+enable
        let fan_write = gpu.fan_curve_path.is_some() || (gpu.has_pwm && gpu.has_pwm_enable);
        AmdGpuCapability {
            present: true,
            model_name: gpu.marketing_name.clone(),
            display_label: gpu.display_label(),
            pci_id: Some(gpu.pci_bdf.clone()),
            pci_device_id: Some(gpu.pci_device_id),
            pci_revision: Some(gpu.pci_revision),
            fan_control_method: gpu.fan_control_method().to_string(),
            pmfw_supported: gpu.fan_curve_path.is_some(),
            fan_rpm_available: gpu.has_fan_rpm,
            fan_write_supported: fan_write,
            is_discrete: gpu.is_discrete,
            overdrive_enabled: gpu.overdrive_enabled,
            gpu_zero_rpm_available: gpu.fan_zero_rpm_path.is_some(),
        }
    } else {
        AmdGpuCapability {
            present: false,
            model_name: None,
            display_label: "AMD D-GPU".to_string(),
            pci_id: None,
            pci_device_id: None,
            pci_revision: None,
            fan_control_method: "none".to_string(),
            pmfw_supported: false,
            fan_rpm_available: false,
            fan_write_supported: false,
            is_discrete: false,
            overdrive_enabled: false,
            gpu_zero_rpm_available: false,
        }
    };

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
                lease_required: true,
                write_support: hwmon_present,
            },
            amd_gpu: amd_gpu_cap,
            aio_hwmon: UnsupportedCapability {
                present: false,
                status: "unsupported",
            },
            aio_usb: UnsupportedCapability {
                present: false,
                status: "unsupported",
            },
        },
        features: FeatureFlags {
            openfan_write_supported: openfan_present,
            hwmon_write_supported: hwmon_present,
            lease_required_for_hwmon_writes: true,
        },
        limits: Limits {
            pwm_percent_min: 0,
            pwm_percent_max: 100,
            // Legacy floor fields removed — thermal safety centralized
            openfan_stop_timeout_s: 8,
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
