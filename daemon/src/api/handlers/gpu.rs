//! AMD GPU fan endpoints: reset to automatic (firmware default) + verify.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::constants;

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
        let fan_id = format!("amd_gpu:{gpu_id}");

        // DEC-255: hold the GPU write lock across the whole reset so an engine
        // tick cannot interleave its own multi-write curve commit with ours.
        // The guard is OWNED and moved into the blocking task deliberately — if
        // the client disconnects (the GUI gives this 5 s), the handler future is
        // dropped, and a borrowed guard would be released while the write was
        // still in flight.
        let Some(write_guard) = state
            .cache
            .lock_gpu_writes_soon(constants::GPU_RESET_LOCK_WAIT)
            .await
        else {
            // Held for longer than an engine tick ever holds it, so the holder
            // is a `fan/verify` running its multi-second window. Say so rather
            // than blocking past the GUI's 5 s timeout.
            return error_response(
                StatusCode::CONFLICT,
                &ErrorEnvelope::validation(
                    "a GPU fan verify is in progress — retry once it completes",
                ),
            );
        };
        let cache = state.cache.clone();
        let task_fan_id = fan_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _write_guard = write_guard;
            // DEC-165/DEC-254: relinquish so the engine stops re-asserting the
            // profile's curve and the reset is durable under an active profile.
            // Claimed BEFORE the write so the flag covers it — and claimed HERE,
            // inside the blocking task, because this task is not cancelled when
            // the client goes away. Doing it in the handler left a disconnect
            // able to strand the fan: relinquished, never reset, and skipped by
            // the engine for the rest of the process's life.
            let newly_claimed = cache.relinquish_gpu_fan(&task_fan_id);
            match crate::hwmon::gpu_fan::reset_to_auto(&path, zero_rpm.as_deref()) {
                Ok(()) => {
                    cache.set_gpu_fan_commanded_pct(&task_fan_id, 0);
                    Ok(())
                }
                Err(e) => {
                    // DEC-255: roll back only what THIS call claimed. An
                    // unconditional rollback lets a second, failing reset clear
                    // the flag a first, successful one owns — handing the fan
                    // back to the engine after the API said it was reset.
                    if newly_claimed {
                        cache.unrelinquish_gpu_fan(&task_fan_id);
                    }
                    Err(e)
                }
            }
            // A panic inside this closure skips the rollback, which is the
            // correct outcome rather than a leak: the global panic hook resets
            // every GPU curve to firmware-auto before unwinding, so a
            // relinquished fan matches the hardware state it leaves behind.
        })
        .await;

        match result {
            Ok(Ok(())) => {
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
        let fan_id = format!("amd_gpu:{gpu_id}");

        // DEC-255: same shape as the PMFW arm above — see its comments.
        let Some(write_guard) = state
            .cache
            .lock_gpu_writes_soon(constants::GPU_RESET_LOCK_WAIT)
            .await
        else {
            // Held for longer than an engine tick ever holds it, so the holder
            // is a `fan/verify` running its multi-second window. Say so rather
            // than blocking past the GUI's 5 s timeout.
            return error_response(
                StatusCode::CONFLICT,
                &ErrorEnvelope::validation(
                    "a GPU fan verify is in progress — retry once it completes",
                ),
            );
        };
        let cache = state.cache.clone();
        let task_fan_id = fan_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _write_guard = write_guard;
            let newly_claimed = cache.relinquish_gpu_fan(&task_fan_id);
            match crate::hwmon::gpu_fan::reset_legacy_to_auto(&hwmon_path) {
                Ok(()) => {
                    cache.set_gpu_fan_commanded_pct(&task_fan_id, 0);
                    Ok(())
                }
                Err(e) => {
                    if newly_claimed {
                        cache.unrelinquish_gpu_fan(&task_fan_id);
                    }
                    Err(e)
                }
            }
        })
        .await;

        match result {
            Ok(Ok(())) => {
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

/// Record what a completed GPU restore left the hardware at (DEC-297).
///
/// **This must mirror `restore_pmfw`/`restore_legacy`'s own match arms.** Both
/// treat `Some(p) if p > 0` as "put the static speed back" and everything else
/// as "hand the fan to firmware-auto" — so stamping only the `Some(p)` case left
/// an auto-restored fan reporting the TEST SPEED forever, which is worse than not
/// stamping at all: `GpuBackend::apply` coalesces within `GPU_COALESCE_DELTA_PCT`,
/// so a later profile command near that value is skipped and the card silently
/// stays on the firmware curve. `0` for the auto branch matches
/// `gpu_reset_fan_handler`, which has always stamped it that way.
///
/// A FAILED restore is deliberately not stamped: the hardware is still at the
/// test speed, the cache already says so, and it must keep saying so or the
/// engine's coalescing will not correct it.
fn stamp_restored_pct(
    cache: &crate::health::cache::StateCache,
    fan_id: &str,
    prior_pct: Option<u8>,
    restore_failed: bool,
) {
    if restore_failed {
        return;
    }
    cache.set_gpu_fan_commanded_pct(fan_id, prior_pct.filter(|p| *p > 0).unwrap_or(0));
}

/// What a GPU verify's uncancellable sequence produced (DEC-297).
///
/// The whole test-write -> settle -> read-back -> restore sequence runs inside one
/// `spawn_blocking`, so it cannot `return` a response directly. It reports what
/// happened and the handler builds the response from it.
enum GpuVerifySequence {
    /// The test write itself was rejected; `final_state` is the post-restore read.
    WriteFailed {
        initial: GpuVerifyState,
        final_state: GpuVerifyState,
        test_speed: u8,
        restore_failed: bool,
        /// The complete, arm-specific message for the response — PMFW and legacy
        /// word their rejection differently.
        details: String,
    },
    Completed {
        initial: GpuVerifyState,
        final_state: GpuVerifyState,
        test_speed: u8,
        restore_failed: bool,
    },
}

/// POST /gpu/{gpu_id}/fan/verify — behavioural test of GPU fan-control
/// effectiveness (the GPU analogue of `hwmon_verify_handler`). Drives a test
/// speed (biased *upward* so cooling is never reduced), waits
/// `VERIFY_WAIT_SECONDS`, reads back the applied curve + `fan1_input` RPM,
/// restores the prior state, and classifies the outcome. No lease — GPU writes
/// never require one (DEC-045). See DEC-120.
pub async fn gpu_verify_handler(
    State(state): State<Arc<AppState>>,
    Path(gpu_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Phase 6 (DEC-201): refuse to start a verify while the system is hot — the
    // verify pauses the engine (incl. the thermal force_all_with_floor) for its window.
    if let Some(resp) = super::verify_thermal_guard(&state.cache) {
        return resp;
    }
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

    // Single-flight + pause the engine's write phase for the verify's lifetime
    // so the engine's GPU backend does not overwrite our controlled test speed.
    // GPU writes need no lease (DEC-045); the pause is the only coordination.
    // NOT a suppression of the thermal `force_all_with_floor` — that runs before the engine's
    // `verify_active()` gate and always outranks a verify (corrected in DEC-297).
    let Some(verify_guard) =
        super::begin_verify_pause(&state.cache, constants::VERIFY_PAUSE_DEADMAN)
    else {
        return error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::validation("a hardware verify or calibration is already in progress"),
        );
    };

    // Release review, 2026-08-10. The verify pause above coordinates against the
    // ENGINE, and DEC-255's write lock coordinates the engine against
    // `fan/reset` — but nothing coordinated verify against reset, and verify is
    // the third multi-write PMFW producer. Take the write lock too, so a reset
    // arriving mid-verify cannot interleave its curve commit with the test
    // speed or the restore.
    //
    // Held for the whole verify window, not per-commit: this handler sleeps
    // between writing the test speed and reading back, and a lock dropped
    // across that sleep would reopen the exact gap. `reset` waits only
    // briefly for it (see `lock_gpu_writes_soon`) and reports a clear conflict
    // rather than hanging past the GUI's 5 s timeout.
    let gpu_write_guard = state.cache.lock_gpu_writes().await;

    let fan_id = format!("amd_gpu:{gpu_id}");
    let prior_pct = state
        .cache
        .snapshot()
        .gpu_fans
        .get(&fan_id)
        .and_then(|f| f.last_commanded_pct);

    // DEC-297: the ENTIRE test-write -> settle -> read-back -> restore sequence
    // runs inside one `spawn_blocking`, and BOTH guards are moved into it. This
    // is the DEC-290 shape, applied to the GPU analogue a release later.
    //
    // It used to run inline with `tokio::time::sleep(...).await` between the test
    // write and the restore. That `.await` is a cancellation point: a client
    // disconnect (or the GUI's own timeout) dropped the handler future, both
    // guards ran their `Drop`, and the restore never happened — leaving the fan
    // pinned at the test speed. `spawn_blocking` is not cancelled when its
    // `JoinHandle` is dropped, so the sequence always completes.
    //
    // NOT a copy of the hwmon fix: this path also holds an OWNED
    // `lock_gpu_writes` guard (DEC-255) for the whole window, which must move
    // inside too, and it takes no hwmon lease (GPU writes need none, DEC-045).
    let task_cache = state.cache.clone();
    let task_fan_id = fan_id.clone();

    let join = if let Some(fan_curve_path) = gpu.fan_curve_path.clone() {
        // ── PMFW fan_curve path (RDNA3+) ──────────────────────────────
        let zero_rpm_path = gpu.fan_zero_rpm_path.clone();
        let hwmon_path = gpu.hwmon_path.clone();
        tokio::task::spawn_blocking(move || {
            let verify_guard = verify_guard;
            let _gpu_write_guard = gpu_write_guard;
            let read_rpm = |hwmon: &std::path::Path| -> Option<u16> {
                std::fs::read_to_string(hwmon.join("fan1_input"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u16>().ok())
            };

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
                let restore_failed =
                    restore_pmfw(prior_pct, &fan_curve_path, zero_rpm_path.as_deref());
                stamp_restored_pct(&task_cache, &task_fan_id, prior_pct, restore_failed);
                return GpuVerifySequence::WriteFailed {
                    initial: initial_state,
                    final_state: GpuVerifyState {
                        applied_speed_pct: None,
                        rpm: read_rpm(&hwmon_path),
                        pwm_enable: None,
                        zero_rpm_enabled: None,
                    },
                    test_speed,
                    restore_failed,
                    details: format!(
                        "The PMFW fan_curve write was rejected by the driver/firmware: {e}. \
                         Manual fan control is not functional in this state."
                    ),
                };
            }
            // DEC-297: keep the cache truthful about what was last COMMANDED.
            // The engine's `apply` coalesces against this value (5% band), so a
            // cache still reporting the pre-verify duty would suppress the
            // engine's correction if the restore below ever fails — the strand
            // would then survive even under an active profile, which is the
            // opposite of what the register row assumed.
            task_cache.set_gpu_fan_commanded_pct(&task_fan_id, test_speed);

            // Blocking sleep, deliberately: this task is the uncancellable unit,
            // and an async sleep would reintroduce the cancellation point the
            // restructure exists to remove.
            std::thread::sleep(std::time::Duration::from_secs(
                constants::VERIFY_WAIT_SECONDS as u64,
            ));

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

            // DEC-296 liveness: prove we are still alive and keep the pause for
            // the restore. Unlike the hwmon verify we hold no lease to lose, so
            // a supersession cannot break the restore itself — but if the window
            // lapses with no successor the engine resumes writing and the
            // restore below races its curve output.
            let _ = verify_guard.renew(constants::VERIFY_PAUSE_DEADMAN);

            let restore_failed = restore_pmfw(prior_pct, &fan_curve_path, zero_rpm_path.as_deref());
            stamp_restored_pct(&task_cache, &task_fan_id, prior_pct, restore_failed);
            GpuVerifySequence::Completed {
                initial: initial_state,
                final_state,
                test_speed,
                restore_failed,
            }
        })
    } else {
        // ── Legacy hwmon pwm1 path (pre-RDNA3) ────────────────────────
        let hwmon_path = gpu.hwmon_path.clone();
        tokio::task::spawn_blocking(move || {
            let verify_guard = verify_guard;
            let _gpu_write_guard = gpu_write_guard;
            let read_rpm = |hwmon: &std::path::Path| -> Option<u16> {
                std::fs::read_to_string(hwmon.join("fan1_input"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u16>().ok())
            };
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
                stamp_restored_pct(&task_cache, &task_fan_id, prior_pct, restore_failed);
                return GpuVerifySequence::WriteFailed {
                    initial: initial_state,
                    final_state: GpuVerifyState {
                        applied_speed_pct: read_pwm_pct(&hwmon_path),
                        rpm: read_rpm(&hwmon_path),
                        pwm_enable: read_enable(&hwmon_path),
                        zero_rpm_enabled: None,
                    },
                    test_speed,
                    restore_failed,
                    details: format!(
                        "The legacy pwm1 write was rejected: {e}. Manual fan control is not \
                         functional in this state."
                    ),
                };
            }
            task_cache.set_gpu_fan_commanded_pct(&task_fan_id, test_speed);

            std::thread::sleep(std::time::Duration::from_secs(
                constants::VERIFY_WAIT_SECONDS as u64,
            ));

            let final_state = GpuVerifyState {
                applied_speed_pct: read_pwm_pct(&hwmon_path),
                rpm: read_rpm(&hwmon_path),
                pwm_enable: read_enable(&hwmon_path),
                zero_rpm_enabled: None,
            };

            let _ = verify_guard.renew(constants::VERIFY_PAUSE_DEADMAN);

            let restore_failed = restore_legacy(prior_pct, &hwmon_path);
            stamp_restored_pct(&task_cache, &task_fan_id, prior_pct, restore_failed);
            GpuVerifySequence::Completed {
                initial: initial_state,
                final_state,
                test_speed,
                restore_failed,
            }
        })
    };

    let sequence = match join.await {
        Ok(seq) => seq,
        Err(e) => {
            // The sequence panicked. Both guards were owned by the task and have
            // been dropped by the unwind, so nothing is stranded held — but the
            // fan may still be at the test speed, which the caller must be told.
            log::error!("GPU verify task panicked for {fan_id}: {e}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorEnvelope::internal(format!(
                    "the GPU verify sequence panicked; the fan may be left at its test speed: {e}"
                )),
            );
        }
    };

    match sequence {
        GpuVerifySequence::WriteFailed {
            initial,
            final_state,
            test_speed,
            restore_failed,
            details,
        } => json_ok(
            StatusCode::OK,
            GpuVerifyResponse {
                gpu_id,
                result: "write_failed".into(),
                initial_state: initial,
                final_state,
                test_speed_pct: test_speed,
                wait_seconds: 0,
                fan_control_method: method.into(),
                details,
                restore_failed,
            },
        ),
        GpuVerifySequence::Completed {
            initial,
            final_state,
            test_speed,
            restore_failed,
        } => {
            let (result, details) = classify_gpu_verify_result(&initial, &final_state, test_speed);
            json_ok(
                StatusCode::OK,
                GpuVerifyResponse {
                    gpu_id,
                    result,
                    initial_state: initial,
                    final_state,
                    test_speed_pct: test_speed,
                    wait_seconds: constants::VERIFY_WAIT_SECONDS,
                    fan_control_method: method.into(),
                    details,
                    restore_failed,
                },
            )
        }
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

    /// AppState carrying one PMFW GPU whose `fan_curve` is a real temp file, so
    /// the verify's writes and its restore are observable.
    fn pmfw_verify_state(curve: std::path::PathBuf) -> Arc<AppState> {
        use crate::hwmon::gpu_detect::AmdGpuInfo;
        let cache = Arc::new(crate::health::cache::StateCache::new());
        let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
        let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let gpu = AmdGpuInfo {
            pci_bdf: "0000:03:00.0".into(),
            pci_device_id: 0x7550,
            pci_revision: 0xC0,
            pci_class: 0x030000,
            marketing_name: Some("RX 9070 XT".into()),
            hwmon_path: std::path::PathBuf::from("/nonexistent/hwmon"),
            fan_curve_path: Some(curve),
            fan_zero_rpm_path: None,
            is_discrete: true,
            has_fan_rpm: false,
            has_pwm: false,
            has_pwm_enable: false,
            overdrive_enabled: true,
        };
        Arc::new(AppState {
            cache,
            staleness_config: crate::health::staleness::StalenessConfig::default(),
            daemon_version: "0.0.0-test".into(),
            fan_controller: Arc::new(parking_lot::RwLock::new(None)),
            openfan_runtime: crate::api::handlers::OpenFanRuntime {
                timeout: std::time::Duration::from_millis(500),
                interval: std::time::Duration::from_millis(1000),
                shutdown: shutdown_rx,
            },
            hwmon_controller: None,
            start_time: std::time::Instant::now(),
            history: Arc::new(crate::health::history::HistoryRing::new(250)),
            active_profile: Arc::new(parking_lot::Mutex::new(None)),
            calibrating: std::sync::atomic::AtomicBool::new(false),
            characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            validation: std::sync::Arc::new(Default::default()),
            characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
            last_openfan_rescan: Arc::new(parking_lot::Mutex::new(None)),
            adopted_poll_handles: Arc::new(parking_lot::Mutex::new(Vec::new())),
            amd_gpus: vec![gpu],
            intel_gpus: Vec::new(),
            nvidia_gpus: Vec::new(),
            profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
            config_path: String::new(),
            runtime_config_path: std::path::PathBuf::new(),
            sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
                std::collections::HashMap::new(),
            ))),
            cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
            override_table: Arc::new(parking_lot::Mutex::new(
                crate::control_override::OverrideTable::new(),
            )),
            allow_port_probe: false,
            running_config: Default::default(),
            readiness_rollup: readiness_rollup.clone(),
            config_write: Default::default(),
            runtime_config_degraded: Default::default(),
            assessment: Arc::new(crate::api::handlers::AssessmentCache::new(readiness_rollup)),
        })
    }

    /// DEC-297 remediation. Both reviewers independently found that the restore's
    /// **auto** branch was never stamped into the cache, and the test above misses
    /// it because it seeds a prior duty (40) and so always takes the `Some(p)`
    /// branch.
    ///
    /// This is the common Hardware-page case: a GPU no profile has commanded yet.
    /// The restore correctly hands the card back to firmware-auto, but the cache
    /// was left reporting the TEST SPEED — so `/fans` and `/poll` claimed a manual
    /// duty for a card on the firmware curve, and `GpuBackend::apply`'s 5% coalescing
    /// would then skip a later profile command near that value, leaving the GPU on
    /// firmware-auto while the daemon believed it was commanding.
    #[tokio::test]
    async fn a_gpu_verify_with_no_prior_duty_records_the_auto_restore() {
        let dir = tempfile::tempdir().unwrap();
        let curve = dir.path().join("fan_curve");
        std::fs::write(&curve, "0: 0C 40%\n").unwrap();
        let state = pmfw_verify_state(curve);
        let cache = state.cache.clone();
        let fan_id = "amd_gpu:0000:03:00.0";

        // No `set_gpu_fan_commanded_pct` seed: `prior_pct` is None, so the restore
        // takes the reset-to-auto branch.
        assert!(
            cache
                .snapshot()
                .gpu_fans
                .get(fan_id)
                .and_then(|f| f.last_commanded_pct)
                .is_none(),
            "fixture check: this test is about the NO-prior-duty path"
        );

        let _ = gpu_verify_handler(
            axum::extract::State(state.clone()),
            axum::extract::Path("0000:03:00.0".to_string()),
        )
        .await;

        let commanded = cache
            .snapshot()
            .gpu_fans
            .get(fan_id)
            .and_then(|f| f.last_commanded_pct);
        assert_eq!(
            commanded,
            Some(0),
            "an auto-restore must be recorded as 0 (the convention gpu_reset_fan_handler \
             already uses), not left at the test speed — otherwise the GUI shows a manual \
             duty for a firmware-controlled card and the engine coalesces away its own \
             correction"
        );
    }

    /// DEC-297 (AUD-b2). The GPU verify used to run inline with a
    /// `tokio::time::sleep(...).await` between the test write and the restore, so
    /// dropping the handler future — a client disconnect, or the GUI's own
    /// timeout — skipped the restore and left the fan pinned at the test speed.
    /// This is the same defect DEC-290 fixed for the hwmon verify, one endpoint
    /// over, and the handler's own doc claimed it already "mirrors" that one.
    ///
    /// Less severe than the hwmon case on purpose — `select_gpu_test_speed`
    /// biases UPWARD, so a strand leaves the fan too fast rather than too slow —
    /// but a strand it is: with no active profile nothing rewrites it.
    #[tokio::test]
    async fn a_cancelled_gpu_verify_still_restores_the_curve() {
        let dir = tempfile::tempdir().unwrap();
        let curve = dir.path().join("fan_curve");
        std::fs::write(&curve, "0: 0C 40%\n").unwrap();
        let state = pmfw_verify_state(curve.clone());
        let cache = state.cache.clone();
        let fan_id = "amd_gpu:0000:03:00.0";

        // Seed the prior duty. This is what the restore must put back, and it is
        // ALSO the only observable: `fan_curve` is a PMFW command channel, not a
        // state file — each write replaces the last, so after a run the file
        // holds only the trailing commit token and reading it back proves
        // nothing. The cache is where "what was last commanded" actually lives.
        cache.set_gpu_fan_commanded_pct(fan_id, 40);
        let test_speed = select_gpu_test_speed(Some(40), 15, 100);
        assert_ne!(
            test_speed, 40,
            "fixture check: the test speed must differ from the prior duty, or the \
             assertion below cannot discriminate a restore from its absence"
        );

        {
            let fut = gpu_verify_handler(
                axum::extract::State(state.clone()),
                axum::extract::Path("0000:03:00.0".to_string()),
            );
            tokio::pin!(fut);
            tokio::select! {
                _ = &mut fut => panic!("the verify completed too fast to model a cancellation"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            }
        } // <- handler future dropped, exactly as axum does on a client disconnect

        // The abandoned `spawn_blocking` keeps running. Poll for it rather than
        // sleeping a fixed span; the bound is a backstop, not the expected path.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(constants::VERIFY_WAIT_SECONDS as u64 + 6);
        while cache.verify_active() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            !cache.verify_active(),
            "the abandoned verify never finished — it must run to completion"
        );

        let commanded = cache
            .snapshot()
            .gpu_fans
            .get(fan_id)
            .and_then(|f| f.last_commanded_pct);
        assert_eq!(
            commanded,
            Some(40),
            "a cancelled GPU verify must still restore the prior duty; \
             {test_speed:?} means it was left at the test speed"
        );
    }

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
