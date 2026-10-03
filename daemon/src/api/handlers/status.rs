//! Read-only status endpoints: status, sensors, fans, poll, capabilities, history, fallback.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::{
    build_control_output_entries, build_cooling_safety_entries, build_fan_entries,
    build_sensor_entries, build_skipped_entries, build_status_response, build_unavailable_entries,
    error_response, json_ok, AppState,
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
    let (health, thermal_state, unavailable, skipped, outputs, verify_active, cooling) =
        state.cache.read_with(|snap| {
            (
                compute_health(snap, &state.staleness_config, now, openfan),
                thermal_state_of(snap),
                build_unavailable_entries(snap, now),
                build_skipped_entries(snap, now),
                build_control_output_entries(snap),
                // `WIRE-n`: read from the snapshot, NOT via
                // `state.cache.verify_active()` — that method takes its own
                // `inner.read()`, and calling it here would re-enter the guard
                // this closure already holds.
                snap.verify_active_at(now),
                // DEC-443: under the same guard as `thermal_state`.
                build_cooling_safety_entries(snap, now),
            )
        });
    Json(build_status_response(
        &state,
        thermal_state,
        unavailable,
        skipped,
        outputs,
        health,
        verify_active,
        cooling,
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
    let (
        health,
        thermal_state,
        unavailable,
        skipped,
        outputs,
        sensors,
        fans,
        verify_active,
        cooling,
    ) = state.cache.read_with(|snap| {
        (
            compute_health(snap, &state.staleness_config, now, openfan),
            thermal_state_of(snap),
            build_unavailable_entries(snap, now),
            build_skipped_entries(snap, now),
            build_control_output_entries(snap),
            build_sensor_entries(snap, now),
            build_fan_entries(snap, now),
            // `WIRE-n` — see the note in `status_handler`.
            snap.verify_active_at(now),
            // DEC-443 — see the note in `status_handler`.
            build_cooling_safety_entries(snap, now),
        )
    });

    Json(PollResponse {
        api_version: API_VERSION,
        status: build_status_response(
            &state,
            thermal_state,
            unavailable,
            skipped,
            outputs,
            health,
            verify_active,
            cooling,
        ),
        sensors,
        fans,
    })
}

/// `devices.amd_gpu`: the primary AMD GPU (`select_primary_gpu`), with the
/// kernel advisories for every AMD GPU on the machine (DEC-449, `BRD-q`).
///
/// Takes the whole list, and selects the primary card itself, so a caller
/// cannot hand the advisories the primary card alone. `kernel_release` is
/// `/proc/sys/kernel/osrelease`, injected so a test can pick an affected one.
fn amd_gpu_capability(
    gpus: &[crate::hwmon::gpu_detect::AmdGpuInfo],
    kernel_release: Option<&str>,
) -> AmdGpuCapability {
    if let Some(gpu) = crate::hwmon::gpu_detect::select_primary_gpu(gpus) {
        // DEC-445 (`DC-ch`): "can a profile drive this fan" — PMFW `fan_curve`
        // only, because that is all the engine's GPU backend writes. A pre-RDNA3
        // card keeps `fan_control_method: "hwmon_pwm"`, since verify and reset do
        // write its legacy `pwm1` (`AmdGpuInfo::can_write_legacy_pwm`, DEC-098),
        // but no engine has ever driven it, so it is not reported writable. The
        // engine's `backend_unavailable` classification reads the same predicate
        // (`GpuBackend::delivery_targets`), so for the card described here the two
        // agree; `devices.amd_gpu` describes only the primary card (`GPU-b`).
        let fan_write = gpu.fan_curve_path.is_some();
        // DEC-449 (`BRD-q`): the advisories cover every AMD GPU, not only the
        // card this entry describes; each message names the card it is about.
        let kernel_warnings = kernel_release
            .map(|release| crate::hwmon::kernel_warnings::detect_kernel_warnings(release, gpus))
            .unwrap_or_default();
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
    }
}

/// GET /capabilities — describe what the daemon can do on this machine.
pub async fn capabilities_handler(
    State(state): State<Arc<AppState>>,
) -> Json<CapabilitiesResponse> {
    let openfan_present = state.openfan().is_some();
    let hwmon_present = state.hwmon_controller.is_some();
    // `OFN-ak`, DEC-376: presence and WRITE support are different questions, and
    // deriving both from `is_some()` made the second one untruthful on a board
    // whose every `pwmN` is read-only — the daemon advertised a write path it
    // does not have, and the GUI's "headers detected but all are read-only"
    // banner (`dashboard_view.py`, `hw.present and not hw.write_support`) was
    // unreachable because its two operands were two copies of one expression
    // (`AUD2-g`/DEC-325, inverted). Write support is the same "≥ 1 writable
    // header" predicate the profile engine gates its backend on
    // (`HwmonBackend::new`) and the thermal force filters to
    // (`forced_target_ids`, DEC-295/DEC-372) — one definition, four readers
    // since the engine's per-member deliverability joined them (`OFN-al`).
    //
    // These two values come from ONE lock acquisition rather than two. The
    // controller lock is held for the whole of an uncancellable blocking
    // `std::fs::write`, so an avoidable second acquisition is avoidable
    // exposure. Note this is NOT a claim about the handler as a whole — it takes
    // the lock again below for the AIO header fold, which is safe (the
    // descriptors are frozen at discovery, so the two acquisitions cannot tear)
    // but means the honest statement is "one acquisition for these two reads",
    // not "one for the handler". Raised by `ofc:concurrency-reviewer` against an
    // earlier wording of this comment that claimed the latter.
    let (hwmon_header_count, hwmon_writable) = match state.hwmon_controller.as_ref() {
        Some(c) => {
            let guard = c.lock();
            (guard.headers().len(), !guard.forced_target_ids().is_empty())
        }
        None => (0, false),
    };

    // AMD GPU detection
    let amd_gpu_cap = amd_gpu_capability(
        &state.amd_gpus,
        crate::hwmon::kernel_warnings::read_kernel_release().as_deref(),
    );

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
                // `OFN-k`: every field here is derived, none is a literal.
                // `channels`/`rpm_support` used to be hardcoded `10`/`true`, so a
                // machine with no controller was told it had ten channels of
                // hardware that does not exist. Both GUI consumers happened to
                // read `channels` only inside their `present` branch, which is
                // what kept it latent rather than live — and is exactly why it
                // was worth fixing before the next client forgot to.
                //
                // The present-branch count comes from the protocol layer's own
                // `NUM_CHANNELS`, the constant `Channel::new` validates against,
                // for the reason stated on `openfan_stop_timeout_s` below: a
                // second literal drifts silently the moment the first one moves.
                channels: if openfan_present {
                    crate::serial::protocol::NUM_CHANNELS
                } else {
                    0
                },
                rpm_support: openfan_present,
                write_support: openfan_present,
            },
            hwmon: HwmonCapability {
                present: hwmon_present,
                pwm_header_count: hwmon_header_count,
                // Not `hwmon_present` — see `hwmon_writable` above (`OFN-ak`).
                write_support: hwmon_writable,
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
            // Not `hwmon_present` — see `hwmon_writable` above (`OFN-ak`).
            hwmon_write_supported: hwmon_writable,
        },
        limits: Limits {
            pwm_percent_min: 0,
            pwm_percent_max: 100,
            // Legacy floor fields removed — thermal safety centralized.
            // Derived from the constant the stop path actually uses, not a
            // literal: a hardcoded 8 here silently drifts the moment
            // STOP_TIMEOUT changes. It does NOT bound a held stop: a repeated 0%
            // coalesces before the timeout is checked (CONC-2), so a stop lasts
            // as long as it is commanded, and the timer only refuses a wire-bound
            // 0% against a stop no normal sequence leaves running — defence in
            // depth (`DC-b`). No client needs to size anything from it.
            // Saturating rather than `as u8`: a raw cast would silently wrap a
            // future STOP_TIMEOUT above 255 s into a tiny advertised value.
            openfan_stop_timeout_s: u8::try_from(crate::constants::STOP_TIMEOUT.as_secs())
                .unwrap_or(u8::MAX),
            // The constant every diagnostic's thermal gate compares against,
            // never a literal (`PTA-i`).
            diagnostic_max_temp_c: crate::constants::CALIBRATION_MAX_TEMP_C,
        },
        // Control-execution capability (DEC-159/160). 1.19.0 delivered daemon-
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
            // `WIRE-ac`: the single source of the pairing floor. `2.0.0` was the
            // DEC-165 cutover and stood here unchanged while ~30 release notes
            // declared `>= 2.23.0`; three numbers claimed to be one contract.
            // 2.23.0 is the published floor, so the wire now states it and the
            // prose quotes the wire.
            min_supported_gui: crate::constants::MIN_SUPPORTED_GUI.into(),
            openfan_rescan: true,
            profile_search_dir_remove: true,
            // DEC-311 (AIO-MB Phase 1): role classification, pump-safe identify,
            // role-aware verify, and POST /config/header-role.
            header_roles: true,
            // AIO-MB Phase 3: PWM/RPM response characterisation.
            pwm_characterization: true,
            pwm_behaviour_characterization: true,
            // AIO-MB Phase 4 (DEC-316), daemon >= 2.31.0. Gates the three
            // topology endpoints only — the additive header fields that shipped
            // with them are optional on the wire and need no flag.
            cooling_devices: true,
            // AIO-MB Phase 5, daemon >= 2.32.0. Gates the validation-session
            // routes. The additive `pwm_readback_pct` field that shipped with
            // them is optional on the wire and needs no flag.
            validation_sessions: true,
            // AIO Phase 8 Batch 1, daemon >= 2.39.0. Discovery and preflight are
            // flagged separately because preflight is read-only and also covers
            // the two pre-existing diagnostics.
            control_path_discovery: true,
            diagnostic_preflight: true,
            // AIO Phase 8 Batch 3a, daemon >= 2.41.0. Required rather than
            // optional: the session-kind fallback makes an older daemon
            // indistinguishable from a supporting one without it.
            thermal_observation: true,
            // AIO Phase 8 Run 2, daemon >= 2.43.0. Required rather than
            // optional: an older daemon parses and drops the request field, so
            // a 200 alone does not tell a client the session will stop itself.
            validation_auto_stop: true,
            // DEC-388: the clean-stop exit floor, its setter and its config key.
            exit_floor: true,
            // `WIRE-k`: five features that shipped before this block had keys
            // for them, so clients gated on a version string or on a probe's
            // 404. All true here — the flag exists so a client can stop
            // guessing, not so it can be turned off.
            gpu_fan_verify: true,
            hardware_readiness: true,
            superio_port_probe: true,
            preferred_sensors: true,
            daemon_config_report: true,
            // DEC-406: coalesced hwmon writes are reconciled against readback.
            duty_reconciliation: true,
            // DEC-407: the stall/restart probe below 20 %.
            stall_probe: true,
            // DEC-452: OpenFan calibration as a 202 + poll run.
            openfan_calibration: true,
            // DEC-442: hwmon chip names and ids survive the it87 v2.0 rename.
            canonical_chip_names: true,
            // DEC-443: coolant emergency, pump stall response, DC-aware pump
            // floor, cooling advisory, and `POST /config/coolant-limit`.
            cooling_failure_detection: true,
            // DEC-456: per-header PWM-control verdicts on `/hwmon/headers`.
            pwm_verification_records: true,
            // A user-assigned `no_fan` header role.
            header_role_no_fan: true,
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
        Json(ErrorEnvelope::route_not_found(uri.path())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `BRD-q` machine: an RDNA2 discrete card, which `select_primary_gpu`
    /// puts first, and an RDNA3 iGPU behind it.
    fn rdna2_card_and_rdna3_igpu() -> Vec<crate::hwmon::gpu_detect::AmdGpuInfo> {
        let card = |bdf: &str, device_id: u16, name: &str, discrete: bool| {
            crate::hwmon::gpu_detect::AmdGpuInfo {
                pci_bdf: bdf.into(),
                pci_device_id: device_id,
                pci_revision: 0xC0,
                pci_class: 0x030000,
                marketing_name: Some(name.into()),
                hwmon_path: std::path::PathBuf::from("/nonexistent"),
                fan_curve_path: None,
                fan_zero_rpm_path: None,
                is_discrete: discrete,
                has_fan_rpm: true,
                has_pwm: true,
                has_pwm_enable: true,
                overdrive_enabled: false,
            }
        };
        vec![
            card("0000:03:00.0", 0x73BF, "AMD Radeon RX 6900 XT", true),
            card("0000:c5:00.0", 0x15BF, "AMD Radeon 780M", false),
        ]
    }

    /// DEC-449 (`BRD-q`): the entry describes the primary card, and its
    /// advisories cover the iGPU behind it. Before, only the primary card was
    /// evaluated, so this machine got no warning at all.
    #[test]
    fn the_capability_warns_about_an_affected_card_that_is_not_the_primary() {
        let gpus = rdna2_card_and_rdna3_igpu();
        let cap = amd_gpu_capability(&gpus, Some("6.18.2"));
        assert_eq!(
            cap.pci_bdf.as_deref(),
            Some("0000:03:00.0"),
            "the primary card"
        );
        assert_eq!(cap.kernel_warnings.len(), 1);
        assert_eq!(
            cap.kernel_warnings[0].id,
            crate::hwmon::kernel_warnings::MES_HANG_4765_ID
        );
        assert!(cap.kernel_warnings[0]
            .message
            .contains("the AMD Radeon 780M (0000:c5:00.0)"));
    }

    #[test]
    fn a_fixed_or_unread_kernel_raises_nothing() {
        let gpus = rdna2_card_and_rdna3_igpu();
        assert!(amd_gpu_capability(&gpus, Some("7.2.7-1-cachyos"))
            .kernel_warnings
            .is_empty());
        assert!(amd_gpu_capability(&gpus, None).kernel_warnings.is_empty());
        let none = amd_gpu_capability(&[], Some("6.18.2"));
        assert!(!none.present && none.kernel_warnings.is_empty());
    }
}
