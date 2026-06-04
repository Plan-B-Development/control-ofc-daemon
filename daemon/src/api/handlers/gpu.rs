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
        // Legacy hwmon write path requires BOTH `pwm1` and `pwm1_enable` —
        // a read-only RDNA3/RDNA4 GPU (no `amdgpu.ppfeaturemask`) exposes
        // `pwm1` alone and used to surface a misleading 503 hardware_unavailable
        // here when `set_legacy_pwm` failed with ENOENT. DEC-098 narrows this
        // arm to the canonical capability check.
        None if gpu.can_write_legacy_pwm() => {
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
                &ErrorEnvelope::feature_unavailable(unsupported_fan_control_message(gpu)),
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
            constants::GPU_PMFW_NUM_CURVE_POINTS,
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
                // Record this as a GUI write so the profile engine defers for
                // the GUI_ACTIVITY_TIMEOUT window. Without this, a profile-
                // engine tick within ~1 s of the reset re-asserts the curve's
                // commanded speed and silently undoes the user's reset.
                state.cache.record_gui_write();
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
    } else if gpu.can_write_legacy_pwm() {
        let hwmon_path = gpu.hwmon_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::hwmon::gpu_fan::reset_legacy_to_auto(&hwmon_path)
        })
        .await;

        match result {
            Ok(Ok(())) => {
                let fan_id = format!("amd_gpu:{gpu_id}");
                state.cache.set_gpu_fan_commanded_pct(&fan_id, 0);
                // Same dual-writer guard as the PMFW reset arm above —
                // see comment there.
                state.cache.record_gui_write();
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
            &ErrorEnvelope::feature_unavailable(unsupported_fan_control_message(gpu)),
        )
    }
}

/// POST /gpu/{gpu_id}/fan/verify — behavioural test of GPU fan-control
/// effectiveness (the GPU analogue of `hwmon_verify_handler`). Drives a test
/// speed (biased *upward* so cooling is never reduced), waits
/// `GPU_VERIFY_WAIT_SECONDS`, reads back the applied curve + `fan1_input` RPM,
/// restores the prior state, and classifies the outcome. No lease — GPU writes
/// never require one (DEC-045). See DEC-120.
pub async fn gpu_verify_handler(
    State(state): State<Arc<AppState>>,
    Path(gpu_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let gpu = match state.amd_gpus.iter().find(|g| g.pci_bdf == gpu_id) {
        Some(g) => g,
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorEnvelope::validation(format!("GPU not found: {gpu_id}")),
            );
        }
    };

    let method = gpu.fan_control_method();
    if gpu.fan_curve_path.is_none() && !gpu.can_write_legacy_pwm() {
        // No write path — the same read-only case the set handler rejects.
        return error_response(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::feature_unavailable(unsupported_fan_control_message(gpu)),
        );
    }

    // Mark GUI activity so the profile engine defers GPU writes for the test
    // window (dual-writer guard, DEC-071/074). The GUI control loop is paused
    // GUI-side via the `amd_gpu:{bdf}` verify key.
    state.cache.record_gui_write();

    let fan_id = format!("amd_gpu:{gpu_id}");
    let prior_pct = state
        .cache
        .snapshot()
        .gpu_fans
        .get(&fan_id)
        .and_then(|f| f.last_commanded_pct);

    let read_rpm = |hwmon: &std::path::Path| -> Option<u16> {
        std::fs::read_to_string(hwmon.join("fan1_input"))
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
    };

    if let Some(fan_curve_path) = gpu.fan_curve_path.clone() {
        // ── PMFW fan_curve path (RDNA3+) ──────────────────────────────
        let zero_rpm_path = gpu.fan_zero_rpm_path.clone();
        let hwmon_path = gpu.hwmon_path.clone();

        let initial_curve = crate::hwmon::gpu_fan::read_fan_curve(&fan_curve_path).ok();
        let (od_min, od_max) = initial_curve
            .as_ref()
            .and_then(|c| c.speed_range)
            .unwrap_or((15, 100));
        let initial_state = GpuVerifyState {
            applied_speed_pct: initial_curve
                .as_ref()
                .and_then(crate::hwmon::gpu_fan::flat_speed_pct),
            rpm: read_rpm(&hwmon_path),
            pwm_enable: None,
            zero_rpm_enabled: zero_rpm_path
                .as_deref()
                .and_then(crate::hwmon::gpu_fan::read_zero_rpm_enabled),
        };

        let test_speed = select_gpu_test_speed(
            prior_pct.or(initial_state.applied_speed_pct),
            od_min,
            od_max,
        );

        // Drive the test speed (disables zero-RPM, clamps to OD_RANGE, commits).
        if let Err(e) = crate::hwmon::gpu_fan::set_static_speed(
            &fan_curve_path,
            zero_rpm_path.as_deref(),
            test_speed,
            constants::GPU_PMFW_NUM_CURVE_POINTS,
        ) {
            let restore_failed = restore_pmfw(prior_pct, &fan_curve_path, zero_rpm_path.as_deref());
            return json_ok(
                StatusCode::OK,
                GpuVerifyResponse {
                    gpu_id,
                    result: "write_failed".into(),
                    initial_state,
                    final_state: GpuVerifyState {
                        applied_speed_pct: None,
                        rpm: read_rpm(&hwmon_path),
                        pwm_enable: None,
                        zero_rpm_enabled: None,
                    },
                    test_speed_pct: test_speed,
                    wait_seconds: 0,
                    fan_control_method: method.into(),
                    details: format!(
                        "The PMFW fan_curve write was rejected by the driver/firmware: {e}. \
                         Manual fan control is not functional in this state."
                    ),
                    restore_failed,
                },
            );
        }

        tokio::time::sleep(std::time::Duration::from_secs(
            constants::GPU_VERIFY_WAIT_SECONDS as u64,
        ))
        .await;

        let final_curve = crate::hwmon::gpu_fan::read_fan_curve(&fan_curve_path).ok();
        let final_state = GpuVerifyState {
            applied_speed_pct: final_curve
                .as_ref()
                .and_then(crate::hwmon::gpu_fan::flat_speed_pct),
            rpm: read_rpm(&hwmon_path),
            pwm_enable: None,
            zero_rpm_enabled: zero_rpm_path
                .as_deref()
                .and_then(crate::hwmon::gpu_fan::read_zero_rpm_enabled),
        };

        let restore_failed = restore_pmfw(prior_pct, &fan_curve_path, zero_rpm_path.as_deref());
        let (result, details) =
            classify_gpu_verify_result(&initial_state, &final_state, test_speed);

        json_ok(
            StatusCode::OK,
            GpuVerifyResponse {
                gpu_id,
                result,
                initial_state,
                final_state,
                test_speed_pct: test_speed,
                wait_seconds: constants::GPU_VERIFY_WAIT_SECONDS,
                fan_control_method: method.into(),
                details,
                restore_failed,
            },
        )
    } else {
        // ── Legacy hwmon pwm1 path (pre-RDNA3) ────────────────────────
        let hwmon_path = gpu.hwmon_path.clone();
        let read_pwm_pct = |hwmon: &std::path::Path| -> Option<u8> {
            std::fs::read_to_string(hwmon.join("pwm1"))
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
                .map(crate::pwm::raw_to_percent)
        };
        let read_enable = |hwmon: &std::path::Path| -> Option<u8> {
            std::fs::read_to_string(hwmon.join("pwm1_enable"))
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
        };

        let initial_state = GpuVerifyState {
            applied_speed_pct: read_pwm_pct(&hwmon_path),
            rpm: read_rpm(&hwmon_path),
            pwm_enable: read_enable(&hwmon_path),
            zero_rpm_enabled: None,
        };

        let test_speed =
            select_gpu_test_speed(prior_pct.or(initial_state.applied_speed_pct), 0, 100);

        if let Err(e) = crate::hwmon::gpu_fan::set_legacy_pwm(&hwmon_path, test_speed) {
            let restore_failed = restore_legacy(prior_pct, &hwmon_path);
            return json_ok(
                StatusCode::OK,
                GpuVerifyResponse {
                    gpu_id,
                    result: "write_failed".into(),
                    initial_state,
                    final_state: GpuVerifyState {
                        applied_speed_pct: read_pwm_pct(&hwmon_path),
                        rpm: read_rpm(&hwmon_path),
                        pwm_enable: read_enable(&hwmon_path),
                        zero_rpm_enabled: None,
                    },
                    test_speed_pct: test_speed,
                    wait_seconds: 0,
                    fan_control_method: method.into(),
                    details: format!(
                        "The legacy pwm1 write was rejected: {e}. Manual fan control is not \
                         functional in this state."
                    ),
                    restore_failed,
                },
            );
        }

        tokio::time::sleep(std::time::Duration::from_secs(
            constants::GPU_VERIFY_WAIT_SECONDS as u64,
        ))
        .await;

        let final_state = GpuVerifyState {
            applied_speed_pct: read_pwm_pct(&hwmon_path),
            rpm: read_rpm(&hwmon_path),
            pwm_enable: read_enable(&hwmon_path),
            zero_rpm_enabled: None,
        };

        let restore_failed = restore_legacy(prior_pct, &hwmon_path);
        let (result, details) =
            classify_gpu_verify_result(&initial_state, &final_state, test_speed);

        json_ok(
            StatusCode::OK,
            GpuVerifyResponse {
                gpu_id,
                result,
                initial_state,
                final_state,
                test_speed_pct: test_speed,
                wait_seconds: constants::GPU_VERIFY_WAIT_SECONDS,
                fan_control_method: method.into(),
                details,
                restore_failed,
            },
        )
    }
}

/// RPM at/above which (with `fan1_input` present) the fan is treated as
/// demonstrably spinning. GPU fans at ≥60% are typically well over 1000 RPM;
/// this floor distinguishes "spinning" from "stopped/idle".
const GPU_SPINNING_RPM: u16 = 200;

/// Tolerance (percentage points) for matching the read-back applied speed to
/// the requested test speed. PMFW/`pwm1` speeds are written exactly; this
/// absorbs raw↔percent rounding only.
const GPU_SPEED_MATCH_TOLERANCE: u16 = 3;

/// Choose a test speed for the GPU fan verify. Biased *upward* so the test
/// never reduces cooling on a hot GPU: from a low/idle start a jump to 75%
/// gives an unambiguous RPM rise; an already-driven-high fan is pushed to 100%
/// (the curve read-back still confirms the write even when RPM barely moves).
/// Clamped to the device OD_RANGE.
fn select_gpu_test_speed(current_pct: Option<u8>, od_min: u8, od_max: u8) -> u8 {
    let target: u8 = if current_pct.unwrap_or(0) < 60 {
        75
    } else {
        100
    };
    target.min(od_max).max(od_min.min(od_max))
}

/// Restore the GPU to its pre-verify state on the PMFW path: re-apply the prior
/// static speed if it was being driven, else reset to firmware-auto (which
/// re-enables zero-RPM). Mirrors how `hwmon_verify_handler` restores its prior
/// PWM. Returns `true` if the restore write failed (`restore_failed`).
fn restore_pmfw(
    prior_pct: Option<u8>,
    fan_curve_path: &std::path::Path,
    zero_rpm_path: Option<&std::path::Path>,
) -> bool {
    let result = match prior_pct {
        Some(p) if p > 0 => crate::hwmon::gpu_fan::set_static_speed(
            fan_curve_path,
            zero_rpm_path,
            p,
            constants::GPU_PMFW_NUM_CURVE_POINTS,
        ),
        _ => crate::hwmon::gpu_fan::reset_to_auto(fan_curve_path, zero_rpm_path),
    };
    if let Err(e) = &result {
        log::warn!("verify: GPU PMFW restore failed (fan may be left at test speed): {e}");
    }
    result.is_err()
}

/// Restore the GPU to its pre-verify state on the legacy `pwm1` path.
/// Returns `true` if the restore write failed.
fn restore_legacy(prior_pct: Option<u8>, hwmon_path: &std::path::Path) -> bool {
    let result = match prior_pct {
        Some(p) if p > 0 => crate::hwmon::gpu_fan::set_legacy_pwm(hwmon_path, p),
        _ => crate::hwmon::gpu_fan::reset_legacy_to_auto(hwmon_path),
    };
    if let Err(e) = &result {
        log::warn!("verify: GPU legacy restore failed (fan may be left at test speed): {e}");
    }
    result.is_err()
}

/// Classify the GPU fan verify outcome from the before/after state and the
/// (OD_RANGE-clamped) requested test speed. Avoids the documented false-failure
/// traps: OD_RANGE clamping is absorbed by the caller, zero-RPM idle is reported
/// as normal (not a fault), and a silent-ignore is distinguished from a real
/// no-effect by the read-back speed match.
fn classify_gpu_verify_result(
    initial: &GpuVerifyState,
    final_state: &GpuVerifyState,
    test_speed: u8,
) -> (String, String) {
    // Legacy path: a reverted pwm1_enable means the BIOS/EC reclaimed control.
    if let Some(en) = final_state.pwm_enable {
        if en != 1 {
            return (
                "pwm_enable_reverted".into(),
                format!(
                    "pwm1_enable changed from 1 to {en} during the test — the BIOS/EC firmware \
                     reclaimed automatic fan control. Disable any vendor 'Smart Fan' / EC \
                     fan-control option in firmware setup, then re-test."
                ),
            );
        }
    }

    // Did the requested speed actually get applied (read-back match)?
    let curve_applied = final_state
        .applied_speed_pct
        .map(|s| (s as i16 - test_speed as i16).unsigned_abs() <= GPU_SPEED_MATCH_TOLERANCE)
        .unwrap_or(false);
    if !curve_applied {
        let seen = final_state
            .applied_speed_pct
            .map(|s| format!("{s}%"))
            .unwrap_or_else(|| "unreadable".into());
        return (
            "curve_not_applied".into(),
            format!(
                "Requested {test_speed}% but the read-back fan speed was {seen}. The write was \
                 accepted at sysfs but silently not applied — typically amdgpu.ppfeaturemask bit \
                 14 (0x4000) is unset, an SMU firmware/driver mismatch, or a BIOS overdrive lock. \
                 Add 'amdgpu.ppfeaturemask=0xffffffff' to the kernel parameters and reboot."
            ),
        );
    }

    // Speed applied — did the fan physically respond?
    match final_state.rpm {
        Some(final_rpm) => {
            let init_rpm_s = initial
                .rpm
                .map(|r| r.to_string())
                .unwrap_or_else(|| "?".into());
            let rose = match initial.rpm {
                Some(ir) => (final_rpm as u32) > (ir as u32) + (ir as u32 / 5) + 50,
                None => final_rpm > GPU_SPINNING_RPM,
            };
            let already_fast =
                initial.applied_speed_pct.unwrap_or(0) >= 50 && final_rpm > GPU_SPINNING_RPM;
            if rose || already_fast {
                (
                    "effective".into(),
                    format!(
                        "GPU fan control verified: speed applied at {test_speed}%, \
                         RPM {init_rpm_s} \u{2192} {final_rpm}."
                    ),
                )
            } else if final_state.zero_rpm_enabled == Some(true) {
                (
                    "zero_rpm_suppressed".into(),
                    "The curve was applied but zero-RPM is enabled, so the fan stays stopped while \
                     the GPU is below its zero-RPM stop temperature. This is normal firmware \
                     behaviour — the fan will spin up under load."
                        .into(),
                )
            } else {
                (
                    "no_rpm_effect".into(),
                    format!(
                        "The curve was applied at {test_speed}% with zero-RPM disabled, but RPM did \
                         not respond ({init_rpm_s} \u{2192} {final_rpm}). The write is accepted but \
                         has no hardware effect — a possible SMU firmware issue or a known kernel \
                         regression for this GPU."
                    ),
                )
            }
        }
        None => (
            "rpm_unavailable".into(),
            format!(
                "Fan-speed write confirmed via curve read-back ({test_speed}%), but no fan1_input \
                 RPM sensor is available to corroborate the physical effect."
            ),
        ),
    }
}

/// Build a `feature_unavailable` message tailored to *why* the GPU has no
/// write path. Distinguishes the "RDNA3+ without overdrive" case (we know
/// the kernel parameter that would unlock PMFW) from the generic "no fan
/// hardware" case so the error includes an actionable hint.
fn unsupported_fan_control_message(gpu: &crate::hwmon::gpu_detect::AmdGpuInfo) -> String {
    // RDNA3/RDNA4 shape: `pwm1` exists read-only, no `pwm1_enable`, no
    // PMFW `fan_curve`. The fix is `amdgpu.ppfeaturemask=0xffffffff`.
    if gpu.has_pwm
        && !gpu.has_pwm_enable
        && gpu.fan_curve_path.is_none()
        && crate::hwmon::gpu_detect::is_rdna3_or_rdna4(gpu.pci_device_id)
    {
        return format!(
            "GPU {} fan control is read-only on this kernel/firmware: \
             pwm1_enable is missing and PMFW fan_curve is not exposed. \
             Add 'amdgpu.ppfeaturemask=0xffffffff' to the kernel parameters \
             and reboot to enable PMFW fan control.",
            gpu.pci_bdf
        );
    }
    format!(
        "GPU {} fan control is read-only ({}); manual fan writes are not \
         supported in this hardware/firmware mode.",
        gpu.pci_bdf,
        gpu.fan_control_method()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(
        applied: Option<u8>,
        rpm: Option<u16>,
        pwm_enable: Option<u8>,
        zero_rpm: Option<bool>,
    ) -> GpuVerifyState {
        GpuVerifyState {
            applied_speed_pct: applied,
            rpm,
            pwm_enable,
            zero_rpm_enabled: zero_rpm,
        }
    }

    #[test]
    fn test_speed_biases_upward_from_idle() {
        assert_eq!(select_gpu_test_speed(None, 15, 100), 75);
        assert_eq!(select_gpu_test_speed(Some(0), 15, 100), 75);
        assert_eq!(select_gpu_test_speed(Some(40), 15, 100), 75);
    }

    #[test]
    fn test_speed_pushes_to_max_when_already_high() {
        assert_eq!(select_gpu_test_speed(Some(70), 15, 100), 100);
        assert_eq!(select_gpu_test_speed(Some(95), 15, 100), 100);
    }

    #[test]
    fn test_speed_clamped_to_od_range() {
        // Narrow OD_RANGE caps the upward target — never below od_min.
        assert_eq!(select_gpu_test_speed(None, 30, 50), 50);
        assert_eq!(select_gpu_test_speed(Some(80), 20, 60), 60);
    }

    #[test]
    fn classify_effective_via_rpm_rise_from_idle() {
        let init = st(None, Some(0), None, Some(false));
        let fin = st(Some(75), Some(1600), None, Some(false));
        let (r, _) = classify_gpu_verify_result(&init, &fin, 75);
        assert_eq!(r, "effective");
    }

    #[test]
    fn classify_effective_when_already_fast() {
        // Driven at 80% and spinning; test pushed to 100% — RPM barely moves but
        // the fan is demonstrably spinning and the curve applied.
        let init = st(Some(80), Some(1500), None, Some(false));
        let fin = st(Some(100), Some(1550), None, Some(false));
        let (r, _) = classify_gpu_verify_result(&init, &fin, 100);
        assert_eq!(r, "effective");
    }

    #[test]
    fn classify_curve_not_applied_when_readback_differs() {
        // Write silently ignored — old curve persists.
        let init = st(None, Some(0), None, Some(true));
        let fin = st(Some(30), Some(0), None, Some(true));
        let (r, d) = classify_gpu_verify_result(&init, &fin, 75);
        assert_eq!(r, "curve_not_applied");
        assert!(d.contains("ppfeaturemask"));
    }

    #[test]
    fn classify_curve_not_applied_when_unreadable() {
        let init = st(None, Some(0), None, Some(false));
        let fin = st(None, Some(0), None, Some(false));
        let (r, _) = classify_gpu_verify_result(&init, &fin, 75);
        assert_eq!(r, "curve_not_applied");
    }

    #[test]
    fn classify_no_rpm_effect_when_applied_but_dead() {
        // Curve applied, zero-RPM disabled, fan still stopped → genuine no-effect.
        let init = st(None, Some(0), None, Some(false));
        let fin = st(Some(75), Some(0), None, Some(false));
        let (r, d) = classify_gpu_verify_result(&init, &fin, 75);
        assert_eq!(r, "no_rpm_effect");
        assert!(d.contains("no hardware effect"));
    }

    #[test]
    fn classify_zero_rpm_suppressed_is_not_a_failure() {
        // Curve applied but zero-RPM still on (idle) → normal, not a fault.
        let init = st(None, Some(0), None, Some(true));
        let fin = st(Some(75), Some(0), None, Some(true));
        let (r, _) = classify_gpu_verify_result(&init, &fin, 75);
        assert_eq!(r, "zero_rpm_suppressed");
    }

    #[test]
    fn classify_rpm_unavailable_when_no_tach() {
        let init = st(None, None, None, Some(false));
        let fin = st(Some(75), None, None, Some(false));
        let (r, _) = classify_gpu_verify_result(&init, &fin, 75);
        assert_eq!(r, "rpm_unavailable");
    }

    #[test]
    fn classify_pwm_enable_reverted_on_legacy_path() {
        // Legacy path: pwm1_enable bounced back to auto (2) → BIOS reclaim.
        let init = st(Some(30), Some(800), Some(1), None);
        let fin = st(Some(75), Some(820), Some(2), None);
        let (r, d) = classify_gpu_verify_result(&init, &fin, 75);
        assert_eq!(r, "pwm_enable_reverted");
        assert!(d.contains("Smart Fan"));
    }

    #[test]
    fn classify_speed_match_absorbs_rounding() {
        // raw↔percent rounding within tolerance still counts as applied.
        let init = st(None, Some(0), None, Some(false));
        let fin = st(Some(73), Some(1400), None, Some(false));
        let (r, _) = classify_gpu_verify_result(&init, &fin, 75);
        assert_eq!(r, "effective");
    }
}
