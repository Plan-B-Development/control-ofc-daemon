//! Request handlers for the IPC API.
//!
//! Read handlers read from the `StateCache` — no direct hardware access.
//! Write handlers dispatch through the `FanController`.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use crate::constants;
use crate::health::cache::StateCache;
use crate::health::staleness::{compute_health, StalenessConfig};
use crate::hwmon::lease::LeaseError;
use crate::hwmon::pwm_control::{HwmonControlError, HwmonPwmController};
use crate::serial::controller::{FanControlError, FanController};

use super::responses::*;
use crate::health::state::DaemonState;

/// Build the sorted list of sensor entries from a cache snapshot.
fn build_sensor_entries(snap: &DaemonState, now: Instant) -> Vec<SensorEntry> {
    snap.sensors
        .values()
        .map(|s| {
            let age_ms = now.duration_since(s.updated_at).as_millis() as u64;
            SensorEntry {
                id: s.id.clone(),
                kind: s.kind.to_string(),
                label: s.label.clone(),
                value_c: s.value_c,
                source: s.source.to_string(),
                age_ms,
                rate_c_per_s: s.rate_c_per_s,
                session_min_c: s.session_min_c,
                session_max_c: s.session_max_c,
            }
        })
        .collect()
}

/// Build the sorted list of fan entries from a cache snapshot.
fn build_fan_entries(snap: &DaemonState, now: Instant) -> Vec<FanEntry> {
    let mut fans: Vec<FanEntry> = Vec::new();

    // OpenFanController fans
    for (ch, fan) in &snap.openfan_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let stall = if fan.rpm_polled {
            fan.last_commanded_pwm
                .map(|pwm| fan.rpm == 0 && pwm > constants::STALL_PWM_THRESHOLD)
        } else {
            None
        };
        fans.push(FanEntry {
            id: format!("openfan:ch{ch:02}"),
            source: "openfan".into(),
            rpm: Some(fan.rpm),
            last_commanded_pwm: fan.last_commanded_pwm,
            age_ms,
            stall_detected: stall,
        });
    }

    // Hwmon fans
    for (id, fan) in &snap.hwmon_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let stall = match (fan.rpm, fan.last_commanded_pwm) {
            (Some(rpm), Some(pwm)) => Some(rpm == 0 && pwm > constants::STALL_PWM_THRESHOLD),
            _ => None,
        };
        fans.push(FanEntry {
            id: id.clone(),
            source: "hwmon".into(),
            rpm: fan.rpm,
            last_commanded_pwm: fan.last_commanded_pwm,
            age_ms,
            stall_detected: stall,
        });
    }

    // AMD GPU fans
    for (id, fan) in &snap.gpu_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        fans.push(FanEntry {
            id: id.clone(),
            source: "amd_gpu".into(),
            rpm: fan.rpm,
            last_commanded_pwm: fan.last_commanded_pct,
            age_ms,
            stall_detected: None,
        });
    }

    fans.sort_by(|a, b| a.id.cmp(&b.id));
    fans
}

/// Shared application state passed to all handlers.
pub struct AppState {
    pub cache: Arc<StateCache>,
    pub staleness_config: StalenessConfig,
    pub daemon_version: String,
    /// Fan controller for OpenFanController write operations. `None` if not connected.
    /// Arc-wrapped to share between API handlers and the profile engine task.
    pub fan_controller: Option<Arc<Mutex<FanController>>>,
    /// Hwmon PWM controller for motherboard fan header writes. `None` if no headers found.
    /// Arc-wrapped to share between API handlers and the profile engine task.
    pub hwmon_controller: Option<Arc<Mutex<HwmonPwmController>>>,
    /// Daemon process start time for uptime calculation.
    pub start_time: Instant,
    /// Per-entity time-series history ring buffer.
    pub history: Arc<crate::health::history::HistoryRing>,
    /// Active profile for headless curve evaluation.
    pub active_profile: Arc<Mutex<Option<crate::profile::DaemonProfile>>>,
    /// Prevents concurrent calibration sweeps from corrupting each other.
    pub calibrating: AtomicBool,
    /// Detected AMD GPU info (populated at startup). Empty if no AMD GPU found.
    pub amd_gpus: Vec<crate::hwmon::gpu_detect::AmdGpuInfo>,
    /// Configured profile search directories (from daemon.toml [profiles] section).
    /// Wrapped in RwLock to allow runtime updates via SIGHUP reload or API endpoint.
    pub profile_search_dirs: parking_lot::RwLock<Vec<std::path::PathBuf>>,
    /// Path to daemon.toml — stored so handlers can persist config changes.
    pub config_path: String,
    /// Number of active SSE client connections (for connection limiting).
    pub sse_clients: Arc<AtomicUsize>,
}

/// GET /status — overall health and subsystem freshness.
pub async fn status_handler(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let snap = state.cache.snapshot();
    let now = Instant::now();
    let health = compute_health(&snap, &state.staleness_config, now);

    let subsystems = health
        .subsystems
        .into_iter()
        .map(|s| SubsystemStatus {
            name: s.name,
            status: s.status.to_string(),
            age_ms: s.age_ms,
            reason: s.reason,
        })
        .collect();

    let uptime = state.start_time.elapsed().as_secs();
    let gui_last_seen = snap
        .last_gui_write_at
        .map(|t| now.duration_since(t).as_secs());

    Json(StatusResponse {
        api_version: API_VERSION,
        daemon_version: state.daemon_version.clone(),
        overall_status: health.overall.to_string(),
        subsystems,
        counters: Counters {
            last_error_summary: None,
        },
        uptime_seconds: Some(uptime),
        gui_last_seen_seconds_ago: gui_last_seen,
    })
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

    let subsystems = health
        .subsystems
        .into_iter()
        .map(|s| SubsystemStatus {
            name: s.name,
            status: s.status.to_string(),
            age_ms: s.age_ms,
            reason: s.reason,
        })
        .collect();

    let uptime = state.start_time.elapsed().as_secs();
    let gui_last_seen = snap
        .last_gui_write_at
        .map(|t| now.duration_since(t).as_secs());

    let status = StatusResponse {
        api_version: API_VERSION,
        daemon_version: state.daemon_version.clone(),
        overall_status: health.overall.to_string(),
        subsystems,
        counters: Counters {
            last_error_summary: None,
        },
        uptime_seconds: Some(uptime),
        gui_last_seen_seconds_ago: gui_last_seen,
    };

    Json(PollResponse {
        api_version: API_VERSION,
        status,
        sensors: build_sensor_entries(&snap, now),
        fans: build_fan_entries(&snap, now),
    })
}

/// POST /fans/openfan/{channel}/pwm — set PWM on a single channel.
pub async fn set_pwm_handler(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<u8>,
    Json(body): Json<SetPwmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.fan_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.set_pwm(channel, body.pwm_percent) {
        Ok(result) => {
            state.cache.record_gui_write();
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(SetPwmResponse {
                        api_version: API_VERSION,
                        channel: result.channel,
                        pwm_percent: result.pwm_percent,
                        coalesced: result.coalesced,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(e) => fan_control_error_response(e),
    }
}

/// POST /fans/openfan/pwm — set PWM on all channels.
pub async fn set_pwm_all_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SetPwmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.fan_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.set_pwm_all(body.pwm_percent) {
        Ok(result) => {
            state.cache.record_gui_write();
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(SetPwmAllResponse {
                        api_version: API_VERSION,
                        pwm_percent: result.pwm_percent,
                        channels_affected: result.channels_affected,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(e) => fan_control_error_response(e),
    }
}

/// POST /fans/openfan/{channel}/target_rpm — set target RPM on a single channel.
pub async fn set_target_rpm_handler(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<u8>,
    Json(body): Json<SetRpmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.fan_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.set_target_rpm(channel, body.target_rpm) {
        Ok(result) => {
            state.cache.record_gui_write();
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(SetRpmResponse {
                        api_version: API_VERSION,
                        channel: result.channel,
                        target_rpm: result.target_rpm,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(e) => fan_control_error_response(e),
    }
}

/// Map a `FanControlError` to an HTTP error response.
fn fan_control_error_response(err: FanControlError) -> (StatusCode, Json<serde_json::Value>) {
    match err {
        FanControlError::Validation(msg) => {
            error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(msg))
        }
        FanControlError::Serial(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(err.to_string()),
        ),
    }
}

/// Serialize any `Serialize` value into a JSON response, returning HTTP 500
/// with a proper error envelope if serialization unexpectedly fails.
fn json_ok(
    status: StatusCode,
    val: impl serde::Serialize,
) -> (StatusCode, Json<serde_json::Value>) {
    match serde_json::to_value(val) {
        Ok(v) => (status, Json(v)),
        Err(e) => {
            log::error!("response serialization failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {
                        "code": "internal_error",
                        "message": "response serialization failed",
                        "retryable": true,
                        "source": "internal"
                    }
                })),
            )
        }
    }
}

/// Helper to serialize an ErrorEnvelope into a JSON value response.
fn error_response(
    status: StatusCode,
    envelope: &ErrorEnvelope,
) -> (StatusCode, Json<serde_json::Value>) {
    json_ok(status, envelope)
}

// ── Capabilities endpoint ────────────────────────────────────────────

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
            fan_control_method: gpu.fan_control_method().to_string(),
            pmfw_supported: gpu.fan_curve_path.is_some(),
            fan_rpm_available: gpu.has_fan_rpm,
            fan_write_supported: fan_write,
            is_discrete: gpu.is_discrete,
            overdrive_enabled: gpu.overdrive_enabled,
        }
    } else {
        AmdGpuCapability {
            present: false,
            model_name: None,
            display_label: "AMD D-GPU".to_string(),
            pci_id: None,
            fan_control_method: "none".to_string(),
            pmfw_supported: false,
            fan_rpm_available: false,
            fan_write_supported: false,
            is_discrete: false,
            overdrive_enabled: false,
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

// ── GPU fan endpoints ───────────────────────────────────────────────

/// Request body for `POST /gpu/{gpu_id}/fan/pwm`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GpuSetFanRequest {
    pub speed_pct: u8,
}

/// Response for successful GPU fan speed set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuSetFanResponse {
    pub api_version: u32,
    pub gpu_id: String,
    pub speed_pct: u8,
}

/// POST /gpu/{gpu_id}/fan/pwm — set GPU fan to a static speed percentage.
pub async fn gpu_set_fan_handler(
    State(state): State<Arc<AppState>>,
    Path(gpu_id): Path<String>,
    Json(body): Json<GpuSetFanRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.speed_pct > 100 {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation("speed_pct must be 0-100"),
        );
    }

    // Find the GPU by PCI BDF
    let gpu = state.amd_gpus.iter().find(|g| g.pci_bdf == gpu_id);
    let gpu = match gpu {
        Some(g) => g,
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorEnvelope::validation(format!("GPU not found: {gpu_id}")),
            );
        }
    };

    // Skip write if speed is within 5% of last commanded value. PMFW flat
    // curves don't benefit from 1% granularity — the firmware manages the
    // actual fan speed. A higher threshold avoids sysfs churn from minor
    // temperature fluctuations during gaming (each write triggers SMU
    // firmware processing that can stall the display pipeline).
    let fan_id = format!("amd_gpu:{gpu_id}");
    let snap = state.cache.snapshot();
    if let Some(cached_fan) = snap.gpu_fans.get(&fan_id) {
        if let Some(last_pct) = cached_fan.last_commanded_pct {
            let delta = (body.speed_pct as i16 - last_pct as i16).unsigned_abs();
            if delta < constants::GPU_COALESCE_DELTA_PCT {
                return json_ok(
                    StatusCode::OK,
                    GpuSetFanResponse {
                        api_version: API_VERSION,
                        gpu_id,
                        speed_pct: body.speed_pct,
                    },
                );
            }
        }
    }

    let fan_curve_path = match &gpu.fan_curve_path {
        Some(p) => p.clone(),
        None if gpu.has_pwm => {
            // Fallback to hwmon pwm1 for pre-RDNA3
            let pwm_path = gpu.hwmon_path.join("pwm1");
            let enable_path = gpu.hwmon_path.join("pwm1_enable");
            let speed_pct = body.speed_pct;
            let result = tokio::task::spawn_blocking(move || {
                // Set manual mode — must succeed before writing PWM value.
                // amdgpu rejects pwm1 writes with EINVAL when not in manual mode.
                std::fs::write(&enable_path, "1\n")?;
                let raw =
                    ((speed_pct as u16 * crate::serial::protocol::MAX_PWM as u16 + 50) / 100) as u8;
                std::fs::write(&pwm_path, format!("{raw}\n"))
            })
            .await;

            return match result {
                Ok(Ok(())) => {
                    let fan_id = format!("amd_gpu:{gpu_id}");
                    state
                        .cache
                        .set_gpu_fan_commanded_pct(&fan_id, body.speed_pct);
                    state.cache.record_gui_write();
                    (
                        StatusCode::OK,
                        Json(
                            serde_json::to_value(GpuSetFanResponse {
                                api_version: API_VERSION,
                                gpu_id,
                                speed_pct: body.speed_pct,
                            })
                            .unwrap(),
                        ),
                    )
                }
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        serde_json::to_value(ErrorEnvelope::hardware_unavailable(
                            "Failed to write GPU fan PWM",
                        ))
                        .unwrap(),
                    ),
                ),
            };
        }
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::to_value(ErrorEnvelope::hardware_unavailable(format!(
                        "GPU {gpu_id} does not support fan control"
                    )))
                    .unwrap(),
                ),
            );
        }
    };

    // PMFW fan_curve path (RDNA3+)
    let speed_pct = body.speed_pct;
    let zero_rpm_path = gpu.fan_zero_rpm_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::hwmon::gpu_fan::set_static_speed(
            &fan_curve_path,
            zero_rpm_path.as_deref(),
            speed_pct,
            constants::GPU_PMFW_WRITE_RETRIES,
        )
    })
    .await;

    match result {
        Ok(Ok(())) => {
            let fan_id = format!("amd_gpu:{gpu_id}");
            state
                .cache
                .set_gpu_fan_commanded_pct(&fan_id, body.speed_pct);
            state.cache.record_gui_write();
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(GpuSetFanResponse {
                        api_version: API_VERSION,
                        gpu_id,
                        speed_pct: body.speed_pct,
                    })
                    .unwrap(),
                ),
            )
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                serde_json::to_value(ErrorEnvelope::hardware_unavailable(format!(
                    "GPU fan write failed: {e}"
                )))
                .unwrap(),
            ),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                serde_json::to_value(ErrorEnvelope::internal(format!(
                    "GPU fan write task failed: {e}"
                )))
                .unwrap(),
            ),
        ),
    }
}

/// POST /gpu/{gpu_id}/fan/reset — reset GPU fan to automatic (firmware default).
pub async fn gpu_reset_fan_handler(
    State(state): State<Arc<AppState>>,
    Path(gpu_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let gpu = state.amd_gpus.iter().find(|g| g.pci_bdf == gpu_id);
    let gpu = match gpu {
        Some(g) => g,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::to_value(ErrorEnvelope::validation(format!(
                        "GPU not found: {gpu_id}"
                    )))
                    .unwrap(),
                ),
            );
        }
    };

    if let Some(fan_curve_path) = &gpu.fan_curve_path {
        let path = fan_curve_path.clone();
        let zero_rpm = gpu.fan_zero_rpm_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::hwmon::gpu_fan::reset_to_auto(&path, zero_rpm.as_deref())
        })
        .await;

        match result {
            Ok(Ok(())) => {
                let fan_id = format!("amd_gpu:{gpu_id}");
                state.cache.set_gpu_fan_commanded_pct(&fan_id, 0);
                log::info!("GPU {gpu_id} fan reset to auto");
                (
                    StatusCode::OK,
                    Json(
                        serde_json::to_value(serde_json::json!({
                            "api_version": API_VERSION,
                            "gpu_id": gpu_id,
                            "reset": true,
                        }))
                        .unwrap(),
                    ),
                )
            }
            Ok(Err(e)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::to_value(ErrorEnvelope::hardware_unavailable(format!(
                        "GPU fan reset failed: {e}"
                    )))
                    .unwrap(),
                ),
            ),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::to_value(ErrorEnvelope::internal(format!(
                        "GPU fan reset task failed: {e}"
                    )))
                    .unwrap(),
                ),
            ),
        }
    } else if gpu.has_pwm {
        // Pre-RDNA3: set pwm1_enable=2 (auto mode)
        let enable_path = gpu.hwmon_path.join("pwm1_enable");
        let result = tokio::task::spawn_blocking(move || std::fs::write(&enable_path, "2\n")).await;

        match result {
            Ok(Ok(())) => (
                StatusCode::OK,
                Json(
                    serde_json::to_value(serde_json::json!({
                        "api_version": API_VERSION,
                        "gpu_id": gpu_id,
                        "reset": true,
                    }))
                    .unwrap(),
                ),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::to_value(ErrorEnvelope::hardware_unavailable(
                        "Failed to reset GPU fan to auto",
                    ))
                    .unwrap(),
                ),
            ),
        }
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::to_value(ErrorEnvelope::hardware_unavailable(format!(
                    "GPU {gpu_id} does not support fan control"
                )))
                .unwrap(),
            ),
        )
    }
}

// ── Hwmon PWM endpoints ─────────────────────────────────────────────

/// GET /hwmon/headers — list discovered controllable PWM headers.
pub async fn hwmon_headers_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return (
            StatusCode::OK,
            Json(
                serde_json::to_value(PwmHeadersResponse {
                    api_version: API_VERSION,
                    headers: vec![],
                })
                .unwrap(),
            ),
        );
    };

    let ctrl = controller.lock();

    let headers = ctrl
        .headers()
        .iter()
        .map(|h| PwmHeaderEntry {
            id: h.id.clone(),
            label: h.label.clone(),
            chip_name: h.chip_name.clone(),
            pwm_index: h.pwm_index,
            supports_enable: h.supports_enable,
            rpm_available: h.rpm_available,
            min_pwm_percent: h.min_pwm_percent,
            max_pwm_percent: h.max_pwm_percent,
            is_writable: h.is_writable,
            pwm_mode: h.pwm_mode,
        })
        .collect();

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(PwmHeadersResponse {
                api_version: API_VERSION,
                headers,
            })
            .unwrap(),
        ),
    )
}

/// POST /hwmon/lease/take — acquire the exclusive hwmon write lease.
pub async fn hwmon_lease_take_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TakeLeaseRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let mut ctrl = controller.lock();

    // GUI always preempts internal holders (profile engine, thermal safety).
    // Use force_take to evict any current holder.
    match Ok::<_, crate::hwmon::lease::LeaseError>(
        ctrl.lease_manager_mut().force_take_lease(&body.owner_hint),
    ) {
        Ok(lease) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(LeaseResponse {
                    api_version: API_VERSION,
                    lease_id: lease.lease_id.clone(),
                    owner_hint: lease.owner_hint.clone(),
                    ttl_seconds: lease.ttl_seconds(),
                })
                .unwrap(),
            ),
        ),
        Err(LeaseError::AlreadyHeld {
            owner_hint,
            ttl_seconds,
        }) => error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::lease_already_held(format!(
                "lease held by '{owner_hint}' (expires in {ttl_seconds}s)"
            )),
        ),
        Err(e) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error(e.to_string()),
        ),
    }
}

/// POST /hwmon/lease/release — release the hwmon write lease.
pub async fn hwmon_lease_release_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ReleaseLeaseRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.lease_manager_mut().release_lease(&body.lease_id) {
        Ok(()) => {
            ctrl.on_lease_released();
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(LeaseReleasedResponse {
                        api_version: API_VERSION,
                        released: true,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(LeaseError::InvalidLease) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error("invalid or expired lease"),
        ),
        Err(LeaseError::NoLease) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error("no active lease to release"),
        ),
        Err(e) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error(e.to_string()),
        ),
    }
}

/// GET /hwmon/lease/status — lease status for UI display.
pub async fn hwmon_lease_status_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        let resp = LeaseStatusResponse {
            api_version: API_VERSION,
            lease_required: true,
            held: false,
            lease_id: None,
            ttl_seconds_remaining: None,
            owner_hint: None,
        };
        return json_ok(StatusCode::OK, resp);
    };

    let ctrl = controller.lock();

    let resp = match ctrl.lease_manager().active_lease() {
        Some(lease) => LeaseStatusResponse {
            api_version: API_VERSION,
            lease_required: true,
            held: true,
            lease_id: Some(lease.lease_id.clone()),
            ttl_seconds_remaining: Some(lease.ttl_seconds()),
            owner_hint: Some(lease.owner_hint.clone()),
        },
        None => LeaseStatusResponse {
            api_version: API_VERSION,
            lease_required: true,
            held: false,
            lease_id: None,
            ttl_seconds_remaining: None,
            owner_hint: None,
        },
    };

    json_ok(StatusCode::OK, resp)
}

/// POST /hwmon/lease/renew — extend the TTL of the current lease.
pub async fn hwmon_lease_renew_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RenewLeaseRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.lease_manager_mut().renew_lease(&body.lease_id) {
        Ok(lease) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(LeaseResponse {
                    api_version: API_VERSION,
                    lease_id: lease.lease_id.clone(),
                    owner_hint: lease.owner_hint.clone(),
                    ttl_seconds: lease.ttl_seconds(),
                })
                .unwrap(),
            ),
        ),
        Err(LeaseError::InvalidLease) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error("invalid or expired lease"),
        ),
        Err(e) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::lease_error(e.to_string()),
        ),
    }
}

/// POST /hwmon/{header_id}/pwm — set PWM on an hwmon header (requires lease).
pub async fn hwmon_set_pwm_handler(
    State(state): State<Arc<AppState>>,
    Path(header_id): Path<String>,
    Json(body): Json<HwmonSetPwmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref controller) = state.hwmon_controller else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("no hwmon PWM headers available"),
        );
    };

    let mut ctrl = controller.lock();

    match ctrl.set_pwm(&header_id, body.pwm_percent, &body.lease_id) {
        Ok(result) => {
            state.cache.record_gui_write();
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(HwmonSetPwmResponse {
                        api_version: API_VERSION,
                        header_id: result.header_id,
                        pwm_percent: result.pwm_percent,
                        raw_value: result.raw_value,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(e) => hwmon_control_error_response(e),
    }
}

/// Map a `HwmonControlError` to an HTTP error response.
fn hwmon_control_error_response(err: HwmonControlError) -> (StatusCode, Json<serde_json::Value>) {
    match err {
        HwmonControlError::Lease(LeaseError::AlreadyHeld {
            owner_hint,
            ttl_seconds,
        }) => error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::lease_already_held(format!(
                "lease held by '{owner_hint}' (expires in {ttl_seconds}s)"
            )),
        ),
        HwmonControlError::Lease(_) => error_response(
            StatusCode::FORBIDDEN,
            &ErrorEnvelope::lease_error("valid lease required for hwmon PWM writes"),
        ),
        HwmonControlError::Validation(msg) => {
            error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(msg))
        }
        HwmonControlError::Hardware(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(err.to_string()),
        ),
    }
}

/// RAII guard that resets the calibrating flag on drop, ensuring cleanup
/// even on early return or panic.
struct CalibrationGuard<'a> {
    flag: &'a AtomicBool,
}
impl Drop for CalibrationGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// POST /fans/openfan/{channel}/calibrate — run a PWM-to-RPM calibration sweep.
pub async fn calibrate_openfan_handler(
    State(state): State<Arc<AppState>>,
    Path(channel): Path<u8>,
    Json(body): Json<crate::api::calibration::CalibrationRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if state.fan_controller.is_none() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable("OpenFanController not connected"),
        );
    }

    if channel > 9 {
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::validation(format!("invalid channel: {channel}")),
        );
    }

    // Prevent concurrent calibration sweeps
    if state
        .calibrating
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation("calibration already in progress"),
        );
    }

    // Drop guard resets `calibrating` to false on any exit path (early return, panic, success)
    let _guard = CalibrationGuard {
        flag: &state.calibrating,
    };

    let steps = body.steps.clamp(2, 20);
    if steps != body.steps {
        log::info!(
            "Calibration API: steps clamped from {} to {steps}",
            body.steps
        );
    }
    let clamped_hold = body.hold_seconds.clamp(2, 15);
    if clamped_hold != body.hold_seconds {
        log::info!(
            "Calibration API: hold_seconds clamped from {} to {clamped_hold}",
            body.hold_seconds
        );
    }
    let hold = std::time::Duration::from_secs(clamped_hold);
    let cache = state.cache.clone();
    let fan_id = format!("openfan:ch{channel:02}");

    // Read pre-calibration PWM
    let pre_cal_pwm = {
        let snap = cache.snapshot();
        snap.openfan_fans
            .get(&channel)
            .and_then(|f| f.last_commanded_pwm)
    };

    let step_size = 100.0 / steps as f64;
    let mut points = Vec::with_capacity(steps as usize + 1);

    for i in 0..=steps {
        let pwm = (i as f64 * step_size).round().min(100.0) as u8;

        // Thermal check
        if let Err(e) = crate::api::calibration::check_thermal_safety(&cache) {
            // Restore pre-cal PWM before returning
            if let (Some(restore), Some(ref ctrl)) = (pre_cal_pwm, &state.fan_controller) {
                {
                    let mut guard = ctrl.lock(); // parking_lot — always succeeds
                    let _ = guard.set_pwm(channel, restore);
                }
            }
            return match e {
                crate::api::calibration::CalibrationError::ThermalAbort {
                    sensor_id,
                    temp_c,
                    ..
                } => error_response(
                    StatusCode::CONFLICT,
                    &ErrorEnvelope {
                        error: ErrorBody {
                            code: "thermal_abort".into(),
                            message: format!("Thermal abort: {sensor_id} at {temp_c:.1}°C"),
                            retryable: true,
                            source: "hardware".into(),
                            details: None,
                        },
                    },
                ),
                _ => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorEnvelope::internal("calibration check failed"),
                ),
            };
        }

        // Set PWM via controller
        {
            let Some(ref ctrl) = state.fan_controller else {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &ErrorEnvelope::hardware_unavailable(
                        "OpenFanController disconnected during calibration",
                    ),
                );
            };
            let mut guard = ctrl.lock();
            if let Err(e) = guard.set_pwm(channel, pwm) {
                return fan_control_error_response(e);
            }
        }

        tokio::time::sleep(hold).await;

        // Read RPM from cache
        let snap = cache.snapshot();
        let rpm = snap.openfan_fans.get(&channel).map(|f| f.rpm).unwrap_or(0);

        points.push(crate::api::calibration::CalPoint {
            pwm_percent: pwm,
            rpm,
        });
    }

    // Restore pre-calibration PWM
    if let (Some(restore), Some(ref ctrl)) = (pre_cal_pwm, &state.fan_controller) {
        {
            let mut guard = ctrl.lock(); // parking_lot — always succeeds
            let _ = guard.set_pwm(channel, restore);
        }
    }

    // Derive start_pwm and stop_pwm
    let start_pwm = points.iter().find(|p| p.rpm > 0).map(|p| p.pwm_percent);
    let stop_pwm = points
        .iter()
        .rev()
        .find(|p| p.rpm == 0)
        .map(|p| p.pwm_percent);
    let min_rpm = points
        .iter()
        .map(|p| p.rpm)
        .filter(|&r| r > 0)
        .min()
        .unwrap_or(0);
    let max_rpm = points.iter().map(|p| p.rpm).max().unwrap_or(0);

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(CalibrationResponse {
                api_version: API_VERSION,
                fan_id,
                points,
                start_pwm,
                stop_pwm,
                min_rpm,
                max_rpm,
            })
            .unwrap(),
        ),
    )
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
    (
        StatusCode::OK,
        Json(
            serde_json::to_value(HistoryResponse {
                api_version: API_VERSION,
                entity_id,
                points,
            })
            .unwrap(),
        ),
    )
}

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

/// POST /hwmon/rescan — re-enumerate hwmon devices and return fresh header list.
///
/// Does not replace the running controller — returns discovery results for the
/// GUI to refresh its view. A daemon restart is needed to pick up truly new hardware.
pub async fn hwmon_rescan_handler(
    State(_state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::hwmon::pwm_discovery::discover_pwm_headers;
    use crate::hwmon::HWMON_SYSFS_ROOT;

    let hwmon_root = std::path::Path::new(HWMON_SYSFS_ROOT);
    match discover_pwm_headers(hwmon_root) {
        Ok(headers) => {
            let entries: Vec<PwmHeaderEntry> = headers
                .iter()
                .map(|h| PwmHeaderEntry {
                    id: h.id.clone(),
                    label: h.label.clone(),
                    chip_name: h.chip_name.clone(),
                    pwm_index: h.pwm_index,
                    supports_enable: h.supports_enable,
                    rpm_available: h.rpm_available,
                    min_pwm_percent: h.min_pwm_percent,
                    max_pwm_percent: h.max_pwm_percent,
                    is_writable: h.is_writable,
                    pwm_mode: h.pwm_mode,
                })
                .collect();
            log::info!("Hwmon rescan: found {} PWM header(s)", entries.len());
            let count = entries.len();
            json_ok(
                StatusCode::OK,
                serde_json::json!({
                    "api_version": API_VERSION,
                    "headers": entries,
                    "count": count,
                }),
            )
        }
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("hwmon rescan failed: {e}")),
        ),
    }
}

/// POST /config/profile-search-dirs — add directories to the profile search path.
///
/// Accepts `{"add": ["/path/to/profiles"]}` — each directory must be an absolute path.
/// The system directory `/etc/onlyfans/profiles` is always preserved.
/// Updates take effect immediately (in-memory) and are persisted to daemon.toml.
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

    // Validate: each dir must be an absolute path
    for d in &new_dirs {
        if !d.starts_with('/') {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::validation(format!("search dir must be absolute: {d}")),
            );
        }
    }

    // Merge with existing dirs (deduplicate, always keep /etc/onlyfans/profiles)
    let mut merged: Vec<String> = {
        let current = state.profile_search_dirs.read();
        current.iter().map(|p| p.display().to_string()).collect()
    };
    for d in &new_dirs {
        if !merged.contains(d) {
            merged.push(d.clone());
        }
    }

    // Update in-memory state
    let path_bufs: Vec<std::path::PathBuf> = merged.iter().map(std::path::PathBuf::from).collect();
    *state.profile_search_dirs.write() = path_bufs;

    // Persist to daemon.toml (best-effort — in-memory update already succeeded)
    if let Err(e) = crate::config::persist_profile_search_dirs(&state.config_path, &merged) {
        log::warn!("Failed to persist search dirs to daemon.toml: {e}");
    }

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

    // Persist to daemon.toml (best-effort)
    if let Err(e) = crate::config::persist_startup_delay(&state.config_path, delay) {
        log::warn!("Failed to persist startup delay: {e}");
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

/// Fallback handler for unknown routes.
pub async fn fallback_handler(uri: axum::http::Uri) -> (StatusCode, Json<ErrorEnvelope>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorEnvelope::not_found(uri.path())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::DaemonState;
    use std::time::Instant;

    #[test]
    fn json_ok_serializes_valid_struct() {
        let val = serde_json::json!({"key": "value"});
        let (status, Json(body)) = json_ok(StatusCode::OK, &val);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["key"], "value");
    }

    #[test]
    fn build_sensor_entries_returns_empty_for_empty_state() {
        let state = DaemonState::default();
        let entries = build_sensor_entries(&state, Instant::now());
        assert!(entries.is_empty());
    }

    #[test]
    fn build_fan_entries_returns_empty_for_empty_state() {
        let state = DaemonState::default();
        let entries = build_fan_entries(&state, Instant::now());
        assert!(entries.is_empty());
    }

    #[test]
    fn build_fan_entries_sorts_by_id() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        // Insert fans in reverse order
        state.hwmon_fans.insert(
            "hwmon:z_fan".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:z_fan".into(),
                rpm: Some(1000),
                last_commanded_pwm: None,
                updated_at: now,
            },
        );
        state.hwmon_fans.insert(
            "hwmon:a_fan".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:a_fan".into(),
                rpm: Some(500),
                last_commanded_pwm: None,
                updated_at: now,
            },
        );

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "hwmon:a_fan");
        assert_eq!(entries[1].id, "hwmon:z_fan");
    }

    #[test]
    fn stall_detection_uses_constant_threshold() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        // Fan at PWM=20 with RPM=0 should NOT be stalled (threshold is >20)
        state.hwmon_fans.insert(
            "hwmon:fan1".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:fan1".into(),
                rpm: Some(0),
                last_commanded_pwm: Some(constants::STALL_PWM_THRESHOLD),
                updated_at: now,
            },
        );

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries[0].stall_detected, Some(false));

        // Fan at PWM=21 with RPM=0 SHOULD be stalled
        state
            .hwmon_fans
            .get_mut("hwmon:fan1")
            .unwrap()
            .last_commanded_pwm = Some(constants::STALL_PWM_THRESHOLD + 1);

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries[0].stall_detected, Some(true));
    }
}
