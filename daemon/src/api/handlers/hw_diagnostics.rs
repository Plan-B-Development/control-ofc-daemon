//! Hardware diagnostics endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::diagnostics;
use crate::api::responses::*;

/// GET /diagnostics/hardware — comprehensive hardware readiness report.
///
/// The report performs ~6 blocking sysfs/procfs reads (modules, ioports, DMI,
/// cpuinfo, kmsg, ppfeaturemask), so it runs on the blocking pool rather than
/// stalling a Tokio worker — mirroring the OpenFan write handlers (DEC-099).
pub async fn hardware_diagnostics_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match tokio::task::spawn_blocking(move || build_hardware_diagnostics(&state)).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("hardware diagnostics task failed: {e}")),
        ),
    }
}

/// Build the hardware-readiness report. Synchronous and blocking — invoked via
/// `spawn_blocking` from the handler above.
fn build_hardware_diagnostics(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    // Collect per-chip info from hwmon headers
    let mut chip_map: HashMap<(String, String), usize> = HashMap::new();
    if let Some(ref controller) = state.hwmon_controller {
        let ctrl = controller.lock();
        for h in ctrl.headers() {
            let key = (h.chip_name.clone(), h.device_id.clone());
            *chip_map.entry(key).or_insert(0) += 1;
        }
    }

    let total_headers = chip_map.values().sum::<usize>();
    let writable_headers = state
        .hwmon_controller
        .as_ref()
        .map(|c| c.lock().headers().iter().filter(|h| h.is_writable).count())
        .unwrap_or(0);

    let chips_detected: Vec<HwmonChipInfo> = chip_map
        .into_iter()
        .map(|((chip_name, device_id), count)| {
            let driver = diagnostics::expected_driver(&chip_name);
            let in_mainline = diagnostics::chip_driver_in_mainline(&chip_name);
            HwmonChipInfo {
                chip_name,
                device_id,
                expected_driver: driver.to_string(),
                in_mainline_kernel: in_mainline,
                header_count: count,
            }
        })
        .collect();

    // DEC-119: PCI-space scan for AMD VGA devices + driver binding. Done
    // independently of the hwmon scan so a GPU whose amdgpu driver did not
    // bind (blacklist, KMS failure, vfio-pci passthrough) is still reported —
    // such a device has no hwmon node and is absent from `gpu` below.
    let amd_pci_raw = crate::hwmon::gpu_detect::detect_amd_pci_devices();
    let amdgpu_module_loaded = crate::hwmon::gpu_detect::amdgpu_module_loaded();
    let amd_pci_devices: Vec<AmdPciDeviceInfo> = amd_pci_raw
        .iter()
        .map(|d| AmdPciDeviceInfo {
            pci_bdf: d.pci_bdf.clone(),
            pci_device_id: d.pci_device_id,
            driver: d.driver.clone(),
            amdgpu_bound: d.amdgpu_bound(),
            hwmon_present: state.amd_gpus.iter().any(|g| g.pci_bdf == d.pci_bdf),
        })
        .collect();

    // Kernel release read once and reused for the primary GPU's advisories.
    let kernel_release = crate::hwmon::kernel_warnings::read_kernel_release();

    // GPU diagnostics from detected GPUs
    let gpu_diag = crate::hwmon::gpu_detect::select_primary_gpu(&state.amd_gpus).map(|gpu| {
        let ppfeaturemask = diagnostics::read_ppfeaturemask();
        let bit14_set = ppfeaturemask
            .as_ref()
            .map(|s| {
                let trimmed = s.trim().strip_prefix("0x").unwrap_or(s.trim());
                u32::from_str_radix(trimmed, 16)
                    .map(|v| (v & 0x4000) != 0)
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        // DEC-119: firmware-enforced OD_RANGE fan-speed bounds (the ~15% min
        // on RDNA3+ that the user perceives as a "minimum"). Read on demand —
        // diagnostics already runs on the blocking pool.
        let (fan_speed_min_pct, fan_speed_max_pct) = gpu
            .fan_curve_path
            .as_ref()
            .and_then(|p| crate::hwmon::gpu_fan::read_fan_curve(p).ok())
            .and_then(|c| c.speed_range)
            .map_or((None, None), |(lo, hi)| (Some(lo), Some(hi)));

        // Best-effort PMFW fan_minimum_pwm (optional attribute).
        let fan_minimum_pwm = gpu
            .fan_minimum_pwm_path()
            .as_deref()
            .and_then(crate::hwmon::gpu_fan::read_fan_minimum_pwm);

        // Kernel-regression advisories for this GPU (same catalog as
        // /capabilities.amd_gpu.kernel_warnings, duplicated for the bundle).
        let kernel_warnings = kernel_release
            .as_deref()
            .map(|r| crate::hwmon::kernel_warnings::detect_kernel_warnings(r, gpu))
            .unwrap_or_default();

        // Driver-bound status cross-referenced from the PCI scan; an hwmon
        // node implies a bound driver, so default to true if the BDF is
        // somehow absent from the PCI listing.
        let amdgpu_driver_bound = amd_pci_raw
            .iter()
            .find(|d| d.pci_bdf == gpu.pci_bdf)
            .is_none_or(|d| d.amdgpu_bound());

        GpuDiagnostics {
            pci_bdf: gpu.pci_bdf.clone(),
            // M11: emit the same BDF under both names so callers aligned to
            // `/capabilities.amd_gpu.pci_id` can use the identical field here.
            pci_id: gpu.pci_bdf.clone(),
            pci_device_id: gpu.pci_device_id,
            pci_revision: gpu.pci_revision,
            model_name: gpu.marketing_name.clone(),
            fan_control_method: gpu.fan_control_method().to_string(),
            overdrive_enabled: gpu.overdrive_enabled,
            ppfeaturemask,
            ppfeaturemask_bit14_set: bit14_set,
            zero_rpm_available: gpu.fan_zero_rpm_path.is_some(),
            fan_speed_min_pct,
            fan_speed_max_pct,
            fan_minimum_pwm,
            amdgpu_driver_bound,
            kernel_warnings,
        }
    });

    // Intel discrete GPU diagnostics (DEC-121). Read-only — the note explains
    // why fan control is unavailable, grounded in the kernel ABI / firmware.
    let intel_gpu_diag =
        crate::hwmon::intel_gpu_detect::select_primary_intel_gpu(&state.intel_gpus).map(|gpu| {
            IntelGpuDiagnostics {
                pci_bdf: gpu.pci_bdf.clone(),
                pci_id: gpu.pci_bdf.clone(),
                pci_device_id: gpu.pci_device_id,
                pci_revision: gpu.pci_revision,
                model_name: gpu.marketing_name.clone(),
                driver: gpu.driver.clone(),
                fan_control_method: gpu.fan_control_method().to_string(),
                fan_rpm_available: gpu.has_fan_rpm,
                fan_control_note:
                    "Intel GPU fan control is managed autonomously by on-card firmware and is \
                     not exposed to Linux userspace (the xe/i915 drivers register no PWM \
                     interface). Temperature and fan RPM are read-only."
                        .to_string(),
            }
        });

    // NVIDIA discrete GPU diagnostics (DEC-204). Read-only — the note explains
    // why fan control is unavailable for both driver legs.
    let nvidia_gpu_diag =
        crate::hwmon::nvidia::select_primary_nvidia_gpu(&state.nvidia_gpus).map(|gpu| {
            NvidiaGpuDiagnostics {
                pci_bdf: gpu.pci_bdf.clone(),
                pci_id: gpu.pci_bdf.clone(),
                model_name: gpu.model_name.clone(),
                driver: gpu.driver.to_string(),
                driver_version: gpu.driver_version.clone(),
                fan_control_method: gpu.fan_control_method().to_string(),
                fan_rpm_available: gpu.fan_rpm_available,
                fan_control_note:
                    "NVIDIA GPU fan control is not exposed to this daemon: the open nouveau \
                     driver's writable pwm1 is deliberately excluded for safety, and the \
                     proprietary NVML backend is read-only telemetry. Temperature and fan \
                     telemetry are read-only."
                        .to_string(),
            }
        });

    // Thermal safety — report thresholds and whether a CPU sensor is USABLE.
    //
    // DEC-269: "present" is the wrong question now that the safety rule filters
    // by age (DEC-267). Answering it from the raw snapshot made
    // `{"state": "no_sensor_fallback", "cpu_sensor_found": true}` reachable —
    // a self-contradicting response, rendered by the GUI as one line reading
    // "State: no_sensor_fallback · CPU sensor: true". Apply the same freshness
    // budget the rule applies, so this field answers the question the state
    // beside it was decided on.
    let snap = state.cache.snapshot();
    let cpu_sensor_found = !matches!(
        crate::profile_engine::hottest_cpu_reading(
            &snap.sensors,
            std::time::Instant::now(),
            state.cache.cpu_temp_stale_after(),
        ),
        crate::profile_engine::CpuReading::Absent | crate::profile_engine::CpuReading::Stale(_)
    );

    let thermal_state = snap.thermal_override_state.as_deref().unwrap_or("normal");

    let thermal_safety = ThermalSafetyInfo {
        state: thermal_state.to_string(),
        cpu_sensor_found,
        // DEC-292: read the single source, never a literal. These were bare
        // `105.0` / `80.0` here, so moving the trip point in `safety.rs` would
        // have left the daemon REPORTING the old value while ACTING on the new
        // one — and the GUI renders this field verbatim as "Limit: N °C".
        emergency_threshold_c: crate::constants::THERMAL_EMERGENCY_TRIGGER_C,
        release_threshold_c: crate::constants::THERMAL_EMERGENCY_RELEASE_C,
    };

    // Kernel module detection
    let kernel_modules = diagnostics::detect_loaded_modules();

    // ACPI conflict detection
    let acpi_conflicts = diagnostics::detect_acpi_conflicts();

    // Revert counts from pwm_enable watchdog
    let enable_revert_counts = state
        .hwmon_controller
        .as_ref()
        .map(|c| c.lock().enable_revert_counts().clone())
        .unwrap_or_default();

    // DMI board identification
    let board = diagnostics::read_board_info();

    // DEC-110: CPU vendor — lets the GUI scope Intel-vs-AMD platform
    // quirks on boards from vendors that ship both (MSI, ASUS, ASRock,
    // Gigabyte). Empty string when /proc/cpuinfo is unreadable or the
    // vendor_id is unknown (hypervisors etc.).
    let cpu_vendor = diagnostics::read_cpu_vendor();

    // DEC-101: dual-chip detection support. `expected_chips` is the
    // deterministic DMI-board lookup; `kernel_detected_chips` is the
    // best-effort kmsg parse. Both fields default to empty Vec on
    // failure paths and are skipped from the wire when empty so older
    // clients ignore them.
    let expected_chips = diagnostics::expected_chips_for_board(&board.vendor, &board.name);
    let kernel_detected_chips = diagnostics::read_kernel_detected_chips();

    // DEC-105 / DEC-106: known-bad simultaneous-load detection. The
    // flagship case is (nct6687, nct6775) — both must never be loaded at
    // the same time on a SINGLE-chip board with NCT6797D because they
    // overlap on chip ID 0xd450 and either can corrupt the chip's
    // non-volatile fan registers. DEC-106 refinement: when chips_detected
    // contains two distinct nct6 chips (e.g. ASRock X870E Taichi Lite
    // has NCT6686 + NCT6799 at separate addresses), each driver legitimately
    // owns its chip and the collision is suppressed.
    let chip_bindings: Vec<diagnostics::ChipBinding<'_>> = chips_detected
        .iter()
        .map(|c| diagnostics::ChipBinding {
            chip_name: c.chip_name.as_str(),
            device_id: c.device_id.as_str(),
        })
        .collect();
    let module_collisions = diagnostics::detect_module_collisions(&chip_bindings);

    json_ok(
        StatusCode::OK,
        HardwareDiagnosticsResponse {
            api_version: API_VERSION,
            hwmon: HwmonDiagnostics {
                chips_detected,
                total_headers,
                writable_headers,
                enable_revert_counts,
            },
            gpu: gpu_diag,
            intel_gpu: intel_gpu_diag,
            nvidia_gpu: nvidia_gpu_diag,
            thermal_safety,
            kernel_modules,
            acpi_conflicts,
            board,
            expected_chips,
            kernel_detected_chips,
            module_collisions,
            cpu_vendor,
            amd_pci_devices,
            amdgpu_module_loaded,
        },
    )
}
