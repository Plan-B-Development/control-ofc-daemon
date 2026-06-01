//! Hardware diagnostics endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{json_ok, AppState};
use crate::api::diagnostics;
use crate::api::responses::*;

/// GET /diagnostics/hardware — comprehensive hardware readiness report.
pub async fn hardware_diagnostics_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
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
        }
    });

    // Thermal safety — report thresholds and whether CPU sensor is present
    let snap = state.cache.snapshot();
    let cpu_sensor_found = snap
        .sensors
        .values()
        .any(|s| s.kind == crate::hwmon::types::SensorKind::CpuTemp);

    let thermal_state = snap.thermal_override_state.as_deref().unwrap_or("normal");

    let thermal_safety = ThermalSafetyInfo {
        state: thermal_state.to_string(),
        cpu_sensor_found,
        emergency_threshold_c: 105.0,
        release_threshold_c: 80.0,
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
            thermal_safety,
            kernel_modules,
            acpi_conflicts,
            board,
            expected_chips,
            kernel_detected_chips,
            module_collisions,
            cpu_vendor,
        },
    )
}
