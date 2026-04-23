//! AMD GPU fan endpoints: set fan speed, reset to automatic.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::constants;

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
            let hwmon_path = gpu.hwmon_path.clone();
            let speed_pct = body.speed_pct;
            let result = tokio::task::spawn_blocking(move || {
                crate::hwmon::gpu_fan::set_legacy_pwm(&hwmon_path, speed_pct)
            })
            .await;

            return match result {
                Ok(Ok(())) => {
                    let fan_id = format!("amd_gpu:{gpu_id}");
                    state
                        .cache
                        .set_gpu_fan_commanded_pct(&fan_id, body.speed_pct);
                    state.cache.record_gui_write();
                    json_ok(
                        StatusCode::OK,
                        GpuSetFanResponse {
                            api_version: API_VERSION,
                            gpu_id,
                            speed_pct: body.speed_pct,
                        },
                    )
                }
                // M13: hardware_unavailable is a 503, not a 500. Sibling
                // hwmon handlers already use 503 for this case.
                Ok(Err(e)) => error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &ErrorEnvelope::hardware_unavailable(format!(
                        "GPU legacy PWM write failed: {e}"
                    )),
                ),
                // spawn_blocking task failure — that IS an internal error.
                Err(e) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorEnvelope::internal(format!("GPU fan write task failed: {e}")),
                ),
            };
        }
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorEnvelope::hardware_unavailable(format!(
                    "GPU {gpu_id} does not support fan control"
                )),
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
            json_ok(
                StatusCode::OK,
                GpuSetFanResponse {
                    api_version: API_VERSION,
                    gpu_id,
                    speed_pct: body.speed_pct,
                },
            )
        }
        // M13: hardware_unavailable is a 503.
        Ok(Err(e)) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(format!("GPU fan write failed: {e}")),
        ),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("GPU fan write task failed: {e}")),
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
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorEnvelope::validation(format!("GPU not found: {gpu_id}")),
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
                json_ok(
                    StatusCode::OK,
                    serde_json::json!({
                        "api_version": API_VERSION,
                        "gpu_id": gpu_id,
                        "reset": true,
                    }),
                )
            }
            // M13: hardware_unavailable is a 503.
            Ok(Err(e)) => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorEnvelope::hardware_unavailable(format!("GPU fan reset failed: {e}")),
            ),
            Err(e) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal(format!("GPU fan reset task failed: {e}")),
            ),
        }
    } else if gpu.has_pwm {
        let hwmon_path = gpu.hwmon_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::hwmon::gpu_fan::reset_legacy_to_auto(&hwmon_path)
        })
        .await;

        match result {
            Ok(Ok(())) => {
                let fan_id = format!("amd_gpu:{gpu_id}");
                state.cache.set_gpu_fan_commanded_pct(&fan_id, 0);
                log::info!("GPU {gpu_id} legacy fan reset to auto");
                json_ok(
                    StatusCode::OK,
                    serde_json::json!({
                        "api_version": API_VERSION,
                        "gpu_id": gpu_id,
                        "reset": true,
                    }),
                )
            }
            // M13: hardware_unavailable is a 503.
            Ok(Err(e)) => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorEnvelope::hardware_unavailable(format!("GPU legacy fan reset failed: {e}")),
            ),
            Err(e) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal(format!("GPU fan reset task failed: {e}")),
            ),
        }
    } else {
        error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::hardware_unavailable(format!(
                "GPU {gpu_id} does not support fan control"
            )),
        )
    }
}
