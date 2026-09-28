//! Hardware diagnostics endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{error_response, json_ok, AppState};
use crate::api::diagnostics;
use crate::api::responses::*;

/// The primary AMD GPU's diagnostics (`select_primary_gpu`), with the kernel
/// advisories for every AMD GPU on the machine (DEC-449, `BRD-q`).
///
/// Takes the whole list and selects the primary card itself, so a caller cannot
/// hand the advisories the primary card alone. `kernel_release` is injected so a
/// test can pick an affected one.
fn amd_gpu_diagnostics(
    gpus: &[crate::hwmon::gpu_detect::AmdGpuInfo],
    amd_pci_raw: &[crate::hwmon::gpu_detect::AmdPciDevice],
    kernel_release: Option<&str>,
) -> Option<GpuDiagnostics> {
    crate::hwmon::gpu_detect::select_primary_gpu(gpus).map(|gpu| {
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

        // Kernel-regression advisories (same catalog as
        // /capabilities.amd_gpu.kernel_warnings, duplicated for the bundle) —
        // for every AMD GPU, each message naming its card (DEC-449, `BRD-q`).
        let kernel_warnings = kernel_release
            .map(|r| crate::hwmon::kernel_warnings::detect_kernel_warnings(r, gpus))
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
    })
}

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
    // Keyed by the canonical chip name (DEC-442); the sysfs spelling rides
    // beside it and is the same for every header of one chip.
    let mut chip_map: HashMap<(String, String), usize> = HashMap::new();
    let mut sysfs_names: HashMap<(String, String), String> = HashMap::new();
    if let Some(ref controller) = state.hwmon_controller {
        let ctrl = controller.lock();
        for h in ctrl.headers() {
            let key = (h.chip_name.clone(), h.device_id.clone());
            sysfs_names
                .entry(key.clone())
                .or_insert_with(|| h.sysfs_chip_name().to_string());
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
            let sysfs_chip_name = sysfs_names
                .remove(&(chip_name.clone(), device_id.clone()))
                .unwrap_or_else(|| chip_name.clone());
            HwmonChipInfo {
                chip_name,
                sysfs_chip_name,
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

    // Kernel release read once and reused for the GPU advisories and the report.
    let kernel_release = crate::hwmon::kernel_warnings::read_kernel_release();

    // GPU diagnostics from detected GPUs
    let gpu_diag = amd_gpu_diagnostics(&state.amd_gpus, &amd_pci_raw, kernel_release.as_deref());

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
        // the threshold values here, so moving the trip point in `safety.rs` would
        // have left the daemon REPORTING the old value while ACTING on the new
        // one — and the GUI renders this field verbatim as "Limit: N °C".
        //
        // DEC-308 makes that invariant load-bearing rather than merely tidy: the
        // trip point is now derived per machine from the CPU's own reported design
        // ceiling, so the constant is only the floor. Report what the engine
        // actually acted on, published in the same write as the state beside it,
        // and fall back to the constant only before the first tick.
        emergency_threshold_c: snap
            .thermal_emergency_trigger_c
            .unwrap_or(crate::constants::THERMAL_EMERGENCY_TRIGGER_C),
        release_threshold_c: crate::constants::THERMAL_EMERGENCY_RELEASE_C,
        // DEC-443: the coolant thresholds the engine acted on, published in the
        // same write as `state`; the limit in force before the first tick.
        coolant_limit_c: snap
            .coolant_limit_c
            .unwrap_or_else(|| f64::from(state.cache.coolant_limit_c())),
        coolant_release_c: snap.coolant_release_c.unwrap_or_else(|| {
            f64::from(state.cache.coolant_limit_c()) - crate::constants::COOLANT_RELEASE_MARGIN_C
        }),
    };

    // Kernel module detection
    let kernel_modules = diagnostics::detect_loaded_modules();

    // ACPI conflict detection
    let acpi_conflicts = diagnostics::detect_acpi_conflicts();

    // Revert counts from pwm_enable watchdog
    let (enable_revert_counts, enable_revert_last_seen_ms) = state
        .hwmon_controller
        .as_ref()
        .map(|c| {
            // One lock, both maps: taken separately they could straddle a
            // reclaim and publish a count without the age that dates it.
            let ctrl = c.lock();
            let now = std::time::Instant::now();
            (
                ctrl.enable_revert_counts().clone(),
                ctrl.enable_revert_ages_ms(now),
            )
        })
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

    // `X87-d`: the board's own firmware-declared counts, where `it87` exports
    // them. Read unconditionally rather than gated on the DMI vendor — the file
    // exists only on boards whose driver published it, so its presence IS the
    // detection, and gating on a vendor string would reintroduce the DMI
    // dependency this field exists to stop relying on. Read-only sysfs: one
    // small file, no port I/O, unaffected by the port-probe gate.
    let board_firmware_counts = crate::hwmon::gigabyte_siv::read_siv(std::path::Path::new(
        crate::hwmon::gigabyte_siv::GIGABYTE_SIV_PATH,
    ));

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

    // `WIRE-ag`: board voltage rails. Read-only sysfs on the same hwmon tree
    // this handler already walks — no port I/O, unaffected by the port-probe
    // gate, and on the `spawn_blocking` side like every other read here. A
    // failed scan degrades to an empty list: a rail display is the least
    // important thing on this response and must never fail the whole report.
    let voltages: Vec<VoltageEntry> = crate::hwmon::voltages::discover_voltages(
        std::path::Path::new(crate::hwmon::HWMON_SYSFS_ROOT),
    )
    .unwrap_or_else(|e| {
        log::warn!("Voltage rail discovery failed: {e}");
        Vec::new()
    })
    .into_iter()
    .map(|v| VoltageEntry {
        id: v.id,
        chip_name: v.chip_name,
        channel: v.channel,
        label: v.label,
        value_v: v.value_v,
        identified: v.identified,
    })
    .collect();

    json_ok(
        StatusCode::OK,
        HardwareDiagnosticsResponse {
            api_version: API_VERSION,
            hwmon: HwmonDiagnostics {
                chips_detected,
                total_headers,
                writable_headers,
                enable_revert_counts,
                enable_revert_last_seen_ms,
            },
            gpu: gpu_diag,
            intel_gpu: intel_gpu_diag,
            nvidia_gpu: nvidia_gpu_diag,
            thermal_safety,
            kernel_modules,
            acpi_conflicts,
            board,
            // DEC-405: the same read the GPU advisories above use, now also
            // published — capped like every other environment fact.
            kernel_release: kernel_release
                .as_deref()
                .and_then(crate::hwmon::chip_db::cap_env_fact),
            expected_chips,
            board_firmware_counts,
            kernel_detected_chips,
            module_collisions,
            cpu_vendor,
            amd_pci_devices,
            amdgpu_module_loaded,
            voltages,
        },
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

    /// DEC-449 (`BRD-q`): the report's GPU section describes the primary card
    /// and carries the iGPU's advisory, as `/capabilities` does.
    #[test]
    fn the_report_warns_about_an_affected_card_that_is_not_the_primary() {
        let gpus = rdna2_card_and_rdna3_igpu();
        let diag = amd_gpu_diagnostics(&gpus, &[], Some("6.18.2")).expect("a GPU");
        assert_eq!(diag.pci_bdf, "0000:03:00.0", "the primary card");
        assert_eq!(diag.kernel_warnings.len(), 1);
        assert!(diag.kernel_warnings[0]
            .message
            .contains("the AMD Radeon 780M (0000:c5:00.0)"));
        assert!(amd_gpu_diagnostics(&gpus, &[], Some("6.18.7"))
            .expect("a GPU")
            .kernel_warnings
            .is_empty());
    }
}
