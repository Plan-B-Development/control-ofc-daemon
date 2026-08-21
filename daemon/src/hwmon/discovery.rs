//! Hwmon device discovery and stable ID generation.
//!
//! Enumerates `/sys/class/hwmon/hwmon*`, reads chip names and labels,
//! and produces `SensorDescriptor`s with stable IDs that do not depend
//! on the hwmon index.

use std::path::{Path, PathBuf};

use crate::error::HwmonError;
use crate::hwmon::types::{SensorDescriptor, SensorKind, SensorSource, SensorThresholds};

/// Plausible kernel range for temperature-threshold values in °C.
///
/// Many drivers report INT_MIN / INT_MAX (≈ ±2.1e6 °C after the /1000 div)
/// or `0` as placeholders for "register not configured". We discard anything
/// outside this range as garbage rather than confuse the UI with bogus values.
const THRESHOLD_MIN_C: f64 = -50.0;
const THRESHOLD_MAX_C: f64 = 200.0;

/// Read a `tempN_<attr>` temperature attribute as Celsius, applying the
/// daemon's plausibility filter (DEC-117).
///
/// Returns `None` when the file does not exist, the parse fails, or the
/// value is outside the plausibility window. Also drops `tempN_max == 0.0`
/// for it87-family chips — see `read_temp_attr_c` below for the empirical
/// rationale.
fn read_temp_attr_c(hwmon_dir: &Path, index: &str, attr: &str, chip_name: &str) -> Option<f64> {
    let path = hwmon_dir.join(format!("temp{index}_{attr}"));
    if !path.exists() {
        return None;
    }
    let raw = read_sysfs_string(&path).ok()?;
    let millidegrees: i64 = raw.trim().parse().ok()?;
    let value_c = millidegrees as f64 / 1000.0;
    if !(THRESHOLD_MIN_C..=THRESHOLD_MAX_C).contains(&value_c) {
        return None;
    }
    // it87-family `temp_max=0` empirical observation: many ITE chips
    // expose `tempN_max` even on channels the BIOS never configured, and
    // those channels read 0 °C at the sysfs surface. The driver itself
    // (drivers/hwmon/it87.c — mainline — and the frankcrawford/it87
    // out-of-tree fork) does NOT synthesise 0; it returns whatever the
    // chip register holds. A 0 °C upper-warning threshold is implausible
    // by hardware standards (sensors don't sit at 0 °C in any realistic
    // environment), so we treat it as "register uninitialised" and drop
    // it. Scoped to `it8*` chip names so legitimate cold-side thresholds
    // on other chips are preserved.
    //
    // References:
    //   - https://docs.kernel.org/hwmon/it87.html (mainline driver doc,
    //     does not document the register-default behaviour)
    //   - https://github.com/frankcrawford/it87 (out-of-tree driver
    //     covering newer IT86xx/IT89xx/IT96xx chips; same surface)
    if attr == "max" && chip_name.starts_with("it8") && value_c == 0.0 {
        return None;
    }
    Some(value_c)
}

/// Read a `tempN_<attr>` alarm/fault bit. Sysfs convention is a "0" or "1"
/// trimmed-decimal string. Anything else is treated as None.
fn read_temp_attr_bool(hwmon_dir: &Path, index: &str, attr: &str) -> Option<bool> {
    let path = hwmon_dir.join(format!("temp{index}_{attr}"));
    if !path.exists() {
        return None;
    }
    let raw = read_sysfs_string(&path).ok()?;
    match raw.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// Read the curated threshold attribute set for a single sensor (DEC-117).
///
/// Returns `None` when no attribute was readable, so the API layer can skip
/// emitting an empty `thresholds` object on the wire.
fn read_thresholds(hwmon_dir: &Path, index: &str, chip_name: &str) -> Option<SensorThresholds> {
    let t = SensorThresholds {
        max_c: read_temp_attr_c(hwmon_dir, index, "max", chip_name),
        min_c: read_temp_attr_c(hwmon_dir, index, "min", chip_name),
        crit_c: read_temp_attr_c(hwmon_dir, index, "crit", chip_name),
        crit_hyst_c: read_temp_attr_c(hwmon_dir, index, "crit_hyst", chip_name),
        emergency_c: read_temp_attr_c(hwmon_dir, index, "emergency", chip_name),
        emergency_hyst_c: read_temp_attr_c(hwmon_dir, index, "emergency_hyst", chip_name),
        lcrit_c: read_temp_attr_c(hwmon_dir, index, "lcrit", chip_name),
        offset_c: read_temp_attr_c(hwmon_dir, index, "offset", chip_name),
        alarm: read_temp_attr_bool(hwmon_dir, index, "alarm"),
        max_alarm: read_temp_attr_bool(hwmon_dir, index, "max_alarm"),
        crit_alarm: read_temp_attr_bool(hwmon_dir, index, "crit_alarm"),
        fault: read_temp_attr_bool(hwmon_dir, index, "fault"),
    };
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Known chip name → sensor kind classification.
///
/// `pub(crate)` so the Phase-2 fine classifier (`hwmon::classify`) can refine
/// *within* this coarse decision and never contradict the sensor's `kind`.
pub(crate) fn classify_chip(chip_name: &str, label: &str) -> SensorKind {
    // Liquid-cooler coolant temperature takes priority over the generic
    // chip/label heuristics below: an NZXT Kraken `temp1` is coolant, and any
    // sensor a vendor labels coolant/water/liquid is coolant regardless of chip
    // (covers Aquacomputer "Coolant temp" and ASUS-EC "Water In"/"Water Out").
    // No safety semantics — see `safety.rs` (CPU-only) and `aio.rs`.
    if crate::hwmon::aio::is_coolant_sensor(chip_name, label) {
        return SensorKind::CoolantTemp;
    }
    let lower = label.to_lowercase();
    match chip_name {
        "k10temp" => SensorKind::CpuTemp,
        "coretemp" => SensorKind::CpuTemp,
        "amdgpu" => SensorKind::GpuTemp,
        // Intel discrete GPU (Arc). Both drivers register hwmon only for
        // discrete cards, so any "xe"/"i915" chip is a dGPU temp (DEC-121).
        "xe" | "i915" => SensorKind::GpuTemp,
        // NVIDIA discrete GPU via the open `nouveau` driver (DEC-204).
        "nouveau" => SensorKind::GpuTemp,
        "nvme" => SensorKind::DiskTemp,
        "sbtsi_temp" => SensorKind::CpuTemp,
        _ if chip_name.starts_with("it87") => SensorKind::MbTemp,
        // Nuvoton Super I/O families: default MbTemp, but TSI/PECI labels indicate CPU
        "nct6775" | "nct6683" | "nct6686" | "nct6687" => {
            if lower.contains("amd tsi")
                || lower.contains("tsi")
                || lower.contains("peci")
                || lower.contains("cpu")
            {
                SensorKind::CpuTemp
            } else {
                SensorKind::MbTemp
            }
        }
        // ASUS EC/WMI sensors: classify by label
        "asus_ec_sensors" | "asus_wmi_sensors" => {
            if lower.contains("cpu") {
                SensorKind::CpuTemp
            } else if lower.contains("gpu") {
                SensorKind::GpuTemp
            } else {
                SensorKind::MbTemp
            }
        }
        // Gigabyte WMI sensors: labels are generic, default to MbTemp
        "gigabyte_wmi" => SensorKind::MbTemp,
        _ => {
            // Fallback: try to guess from the label
            if lower.contains("cpu") || lower.contains("tctl") || lower.contains("tccd") {
                SensorKind::CpuTemp
            } else if lower.contains("gpu") || lower.contains("edge") || lower.contains("junction")
            {
                SensorKind::GpuTemp
            } else {
                SensorKind::MbTemp
            }
        }
    }
}

/// Build a stable ID for an hwmon sensor.
///
/// Format: `hwmon:<chip_name>:<device_id>:<label_or_index>`
/// where `device_id` is derived from the device path to distinguish
/// multiple chips with the same name (e.g. two NVMe drives).
fn build_stable_id(chip_name: &str, device_id: &str, label: &str) -> String {
    format!("hwmon:{chip_name}:{device_id}:{label}")
}

/// Extract a short device identifier from the sysfs device path.
///
/// For PCI devices: extracts the PCI address (e.g. `0000:03:00.0`).
use super::util::device_id_from_path;

/// The outcome of one discovery pass, including whether it was COMPLETE.
///
/// [SAFETY] DEC-272 round 2. A per-chip failure is logged and skipped so one bad
/// chip cannot blind the daemon to every other one — but that makes a *partial*
/// result indistinguishable from a *complete* one at the call site, and
/// `polling.rs` uses the descriptor set as evidence of which sensors still exist
/// (`StateCache::retain_sensors`). Trusting a partial set evicts a live sensor
/// that merely could not be enumerated this pass. `skipped_chips` is what lets
/// the caller tell "gone" from "could not tell": a REMOVED chip has no directory
/// and is silently absent from `hwmon_dirs`, so it never increments this.
pub struct SensorDiscovery {
    pub descriptors: Vec<SensorDescriptor>,
    /// hwmon directories still PRESENT in sysfs whose own metadata could not be
    /// read this pass — i.e. the chips this pass cannot speak for.
    ///
    /// Identities rather than a count, deliberately. A count can only say "the
    /// whole list is untrustworthy", which forces eviction off wholesale and,
    /// because nothing re-triggers discovery for a chip that contributes no
    /// descriptors, could leave it off for the rest of the process. Naming the
    /// chips lets the caller protect exactly their cached readings and keep
    /// evicting everything else, so DEC-272 row 01-c goes on working for every
    /// other chip and there is no wedgeable global state.
    ///
    /// DIRECTORIES rather than `device_id`s, also deliberately: a chip with no
    /// `device` symlink yields the literal `"nodev"`, so matching cached ids on
    /// device id alone silently protects EVERY such sensor — the global
    /// suspension again, wearing a per-chip disguise. The caller resolves these
    /// against the previous descriptor set's `input_path`s, which are real paths
    /// under the real directory. (Caught by
    /// `an_unreadable_chip_does_not_stop_other_sensors_being_evicted`, which
    /// failed against the device-id version.)
    ///
    /// A chip whose directory has GONE is removed, not unreadable, and is
    /// deliberately absent here so it still evicts at once — the "cannot read" vs
    /// "gone" distinction the mechanism rests on, tested by re-checking the
    /// directory rather than by parsing errno.
    pub unreadable_dirs: Vec<PathBuf>,
}

/// Discover all temperature sensors under a given sysfs hwmon root.
///
/// The `hwmon_root` parameter allows injecting a test fixture directory
/// instead of the real `/sys/class/hwmon`.
///
/// Discards the completeness signal; see [`discover_sensors_reporting_skips`]
/// when the result is used as evidence that a sensor no longer exists.
pub fn discover_sensors(hwmon_root: &Path) -> Result<Vec<SensorDescriptor>, HwmonError> {
    discover_sensors_reporting_skips(hwmon_root).map(|d| d.descriptors)
}

/// As [`discover_sensors`], but reports how many present chips were skipped.
pub fn discover_sensors_reporting_skips(hwmon_root: &Path) -> Result<SensorDiscovery, HwmonError> {
    let mut descriptors = Vec::new();
    let mut unreadable_dirs: Vec<PathBuf> = Vec::new();

    let entries = std::fs::read_dir(hwmon_root).map_err(|e| HwmonError::ReadError {
        path: hwmon_root.display().to_string(),
        message: e.to_string(),
    })?;

    let mut hwmon_dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hwmon"))
        })
        .collect();

    hwmon_dirs.sort();

    for hwmon_dir in hwmon_dirs {
        match discover_device_sensors(&hwmon_dir) {
            Ok(sensors) => descriptors.extend(sensors),
            Err(e) => {
                // A chip whose directory vanished between `read_dir` and here was
                // REMOVED; it must still evict. Only one that is still present but
                // unreadable is "cannot tell". Tested by re-checking the directory
                // rather than by matching errno text, which is not a stable API.
                if hwmon_dir.exists() {
                    log::warn!(
                        "Skipping {} (present but unreadable): {e}",
                        hwmon_dir.display()
                    );
                    unreadable_dirs.push(hwmon_dir.clone());
                } else {
                    log::info!("Skipping {} (removed during scan)", hwmon_dir.display());
                }
            }
        }
    }

    Ok(SensorDiscovery {
        descriptors,
        unreadable_dirs,
    })
}

/// The `device_id` half of a chip's stable ids, from its hwmon directory alone.
///
/// Extracted so the SKIP path can derive the same value the success path would
/// have: a chip whose `name` will not read never reaches `discover_device_sensors`'
/// body, but its already-cached sensor ids still embed this, which is what lets
/// `polling.rs` protect exactly that chip's readings from eviction and no others.
fn device_id_for_hwmon_dir(hwmon_dir: &Path) -> String {
    let device_link = hwmon_dir.join("device");
    if device_link.exists() {
        let resolved = std::fs::read_link(&device_link)
            .or_else(|_| std::fs::canonicalize(&device_link))
            .unwrap_or_else(|e| {
                log::warn!(
                    "Could not resolve device symlink for {}: {}",
                    hwmon_dir.display(),
                    e
                );
                std::path::PathBuf::from("unknown")
            });
        device_id_from_path(&resolved)
    } else {
        "nodev".to_string()
    }
}

/// Discover temperature sensors for a single hwmon device directory.
fn discover_device_sensors(hwmon_dir: &Path) -> Result<Vec<SensorDescriptor>, HwmonError> {
    let chip_name = read_sysfs_string(&hwmon_dir.join("name"))?
        .trim()
        .to_string();

    let device_id = device_id_for_hwmon_dir(hwmon_dir);

    // Find all temp*_input files
    let mut sensors = Vec::new();
    let entries = std::fs::read_dir(hwmon_dir).map_err(|e| HwmonError::ReadError {
        path: hwmon_dir.display().to_string(),
        message: e.to_string(),
    })?;

    let mut temp_inputs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("temp") && n.ends_with("_input"))
        })
        .collect();

    temp_inputs.sort();

    for input_path in temp_inputs {
        let filename = input_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        // Extract index: temp1_input → 1
        let index = filename
            .strip_prefix("temp")
            .and_then(|s| s.strip_suffix("_input"))
            .unwrap_or("0");

        // Try to read the label file (temp1_label, etc.)
        let label_path = input_path.with_file_name(format!("temp{index}_label"));
        let label = if label_path.exists() {
            // [SAFETY] DEC-272 round 2 — propagate, do NOT default to empty.
            //
            // The label is not decoration: `build_stable_id` embeds it, and
            // `classify_sensor` derives `SensorKind` from it. Defaulting a
            // present-but-unreadable label to "" therefore renames the sensor
            // (`hwmon:chip:dev:Tctl` -> `hwmon:chip:dev:`) AND can reclassify it
            // — on `nct6775`/`asus_ec_sensors` an empty label falls through to
            // `MbTemp`. The chip still enumerates Ok, so the pass looks complete,
            // eviction runs, and the old id is dropped on false evidence: exactly
            // the "unreadable is not vanished" hole this round closed one level
            // up, reached through an attribute instead of the chip name. On a
            // CpuTemp that yields `CpuReading::Absent` and the 100/40 flap.
            //
            // Failing the whole chip costs one pass of its other sensors, whose
            // cached readings are protected by `unreadable_dirs` in the meantime.
            // That is the cheaper error.
            read_sysfs_string(&label_path)?.trim().to_string()
        } else {
            format!("temp{index}")
        };

        // Try to read tempN_type (may not exist — that's fine)
        let type_path = input_path.with_file_name(format!("temp{index}_type"));
        let temp_type = if type_path.exists() {
            read_sysfs_string(&type_path)
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
        } else {
            None
        };

        // DEC-117: snapshot the curated hwmon threshold attribute set once
        // at discovery. Since DEC-133 the polling loop caches descriptors,
        // so this snapshot is genuinely once-per-discovery: it refreshes
        // only when discovery re-runs — on /hwmon/rescan, on a read-failure
        // streak, or while no CpuTemp sensor is cached.
        let thresholds = read_thresholds(hwmon_dir, index, &chip_name);

        let kind = classify_chip(&chip_name, &label);
        let id = build_stable_id(&chip_name, &device_id, &label);
        let source = match chip_name.as_str() {
            "amdgpu" => SensorSource::AmdGpu,
            // The kernel registers the "xe"/"i915" hwmon node only for
            // discrete Intel GPUs (DGFX-gated), so these are always dGPU
            // temps (DEC-121).
            "xe" | "i915" => SensorSource::IntelGpu,
            // NVIDIA discrete GPU via the open `nouveau` driver. Read-only
            // telemetry; the writable `pwm1` is excluded from PWM discovery
            // (`is_gpu_owned_hwmon_chip`) so the engine never drives it (DEC-204).
            "nouveau" => SensorSource::NvidiaGpu,
            _ => SensorSource::Hwmon,
        };

        sensors.push(SensorDescriptor {
            id,
            kind,
            label: label.clone(),
            source,
            input_path: input_path.display().to_string(),
            chip_name: chip_name.clone(),
            temp_type,
            thresholds,
        });
    }

    Ok(sensors)
}

/// Read a sysfs file as a trimmed string.
use super::util::read_sysfs_string;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_fixture_with_chip_name(
        base: &Path,
        dir_name: &str,
        chip_name: &str,
        temps: &[(&str, Option<&str>)],
    ) -> PathBuf {
        let hwmon_dir = base.join(dir_name);
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), chip_name).unwrap();

        for (index, label) in temps {
            fs::write(hwmon_dir.join(format!("temp{index}_input")), "45000\n").unwrap();
            if let Some(lbl) = label {
                fs::write(
                    hwmon_dir.join(format!("temp{index}_label")),
                    format!("{lbl}\n"),
                )
                .unwrap();
            }
        }

        hwmon_dir
    }

    #[test]
    fn discover_cpu_sensor_k10temp() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "k10temp",
            &[("1", Some("Tctl")), ("3", Some("Tccd1"))],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
        assert_eq!(sensors[0].label, "Tctl");
        assert!(sensors[0].id.contains("k10temp"));
        assert!(sensors[0].id.contains("Tctl"));
        assert_eq!(sensors[1].label, "Tccd1");
    }

    #[test]
    fn discover_gpu_sensor_amdgpu() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "amdgpu",
            &[
                ("1", Some("edge")),
                ("2", Some("junction")),
                ("3", Some("mem")),
            ],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 3);
        for s in &sensors {
            assert_eq!(s.kind, SensorKind::GpuTemp);
        }
        assert_eq!(sensors[0].label, "edge");
    }

    #[test]
    fn discover_intel_gpu_xe_source_and_kind() {
        // Intel Arc (Battlemage) via the xe driver: temps start at temp2
        // (no temp1), classified as GpuTemp with source IntelGpu (DEC-121).
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "xe", &[("2", None), ("3", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        for s in &sensors {
            assert_eq!(s.kind, SensorKind::GpuTemp);
            assert_eq!(s.source, SensorSource::IntelGpu);
            assert_eq!(s.chip_name, "xe");
        }
        assert_eq!(sensors[0].label, "temp2");
    }

    #[test]
    fn discover_intel_gpu_i915_source_and_kind() {
        // Intel Arc A-series (Alchemist) via the i915 driver: temp1 only.
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "i915", &[("1", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].kind, SensorKind::GpuTemp);
        assert_eq!(sensors[0].source, SensorSource::IntelGpu);
    }

    #[test]
    fn discover_amdgpu_source_is_amd_not_intel() {
        // Regression: ensure the new Intel arm did not change amdgpu tagging.
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "amdgpu", &[("1", Some("edge"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors[0].source, SensorSource::AmdGpu);
    }

    #[test]
    fn discover_nvidia_gpu_nouveau_source_and_kind() {
        // NVIDIA via the open nouveau driver: temp1 classified as GpuTemp with
        // source NvidiaGpu (DEC-204). Temps flow through the normal pipeline;
        // the writable pwm1 is excluded elsewhere (`is_gpu_owned_hwmon_chip`).
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "nouveau", &[("1", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].kind, SensorKind::GpuTemp);
        assert_eq!(sensors[0].source, SensorSource::NvidiaGpu);
        assert_eq!(sensors[0].chip_name, "nouveau");
    }

    #[test]
    fn discover_nvme_disk_sensor() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "nvme", &[("1", Some("Composite"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].kind, SensorKind::DiskTemp);
        assert!(sensors[0].id.contains("nvme"));
    }

    #[test]
    fn discover_motherboard_sensor_ite() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "it8696", &[("1", None), ("2", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        for s in &sensors {
            assert_eq!(s.kind, SensorKind::MbTemp);
        }
        // Without labels, fallback name used
        assert_eq!(sensors[0].label, "temp1");
    }

    #[test]
    fn discover_missing_label_uses_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "k10temp",
            &[("1", None)], // no label file
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].label, "temp1");
        // Fallback label doesn't match CPU heuristics, but chip name does
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
    }

    #[test]
    fn discover_multiple_devices() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "k10temp", &[("1", Some("Tctl"))]);
        create_fixture_with_chip_name(tmp.path(), "hwmon1", "amdgpu", &[("1", Some("edge"))]);
        create_fixture_with_chip_name(tmp.path(), "hwmon2", "it8696", &[("1", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 3);

        let kinds: Vec<_> = sensors.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&SensorKind::CpuTemp));
        assert!(kinds.contains(&SensorKind::GpuTemp));
        assert!(kinds.contains(&SensorKind::MbTemp));
    }

    #[test]
    fn discover_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let sensors = discover_sensors(tmp.path()).unwrap();
        assert!(sensors.is_empty());
    }

    #[test]
    fn discover_skips_non_hwmon_entries() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a non-hwmon directory
        fs::create_dir_all(tmp.path().join("notahwmon")).unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "k10temp", &[("1", Some("Tctl"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
    }

    #[test]
    fn stable_id_does_not_contain_hwmon_index() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon5", "k10temp", &[("1", Some("Tctl"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert!(!sensors[0].id.contains("hwmon5"));
        assert!(sensors[0].id.starts_with("hwmon:k10temp:"));
    }

    #[test]
    fn device_id_extracts_pci_address() {
        let path = Path::new(
            "/sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0/0000:02:00.0/0000:03:00.0",
        );
        let id = device_id_from_path(path);
        assert_eq!(id, "0000:03:00.0");
    }

    #[test]
    fn device_id_extracts_platform_id() {
        let path = Path::new("/sys/devices/platform/it87.2624");
        let id = device_id_from_path(path);
        assert_eq!(id, "it87.2624");
    }

    // ── New driver classification tests ────────────────────────────────

    #[test]
    fn discover_nct6775_default_mbtemp() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "nct6775",
            &[("1", Some("SYSTIN")), ("2", Some("AUXTIN0"))],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        for s in &sensors {
            assert_eq!(s.kind, SensorKind::MbTemp);
            assert_eq!(s.chip_name, "nct6775");
        }
    }

    #[test]
    fn discover_nct6775_tsi_label_is_cpu() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "nct6775",
            &[("1", Some("AMD TSI Addr 98h")), ("2", Some("SYSTIN"))],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
        assert_eq!(sensors[1].kind, SensorKind::MbTemp);
    }

    #[test]
    fn discover_nct6683_peci_label_is_cpu() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "nct6683",
            &[("1", Some("PECI Agent 0")), ("2", Some("PCH_CHIP"))],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
        assert_eq!(sensors[1].kind, SensorKind::MbTemp);
    }

    #[test]
    fn discover_nct6687_family_handled() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "nct6687", &[("1", Some("CPU"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
    }

    #[test]
    fn discover_asus_ec_sensors_classifies_by_label() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "asus_ec_sensors",
            &[
                ("1", Some("CPU")),
                ("2", Some("GPU")),
                ("3", Some("Chipset")),
                ("4", Some("Motherboard")),
            ],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 4);
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
        assert_eq!(sensors[1].kind, SensorKind::GpuTemp);
        assert_eq!(sensors[2].kind, SensorKind::MbTemp);
        assert_eq!(sensors[3].kind, SensorKind::MbTemp);
    }

    #[test]
    fn discover_asus_wmi_sensors_classifies_by_label() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "asus_wmi_sensors",
            &[("1", Some("CPU Package Temp")), ("2", Some("VRM"))],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
        assert_eq!(sensors[1].kind, SensorKind::MbTemp);
    }

    #[test]
    fn discover_gigabyte_wmi_is_mbtemp() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "gigabyte_wmi",
            &[("1", Some("temp1")), ("2", Some("temp2"))],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        for s in &sensors {
            assert_eq!(s.kind, SensorKind::MbTemp);
        }
    }

    #[test]
    fn discover_sbtsi_temp_is_cpu() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "sbtsi_temp", &[("1", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].kind, SensorKind::CpuTemp);
    }

    // ── Coolant / liquid-cooler classification (AIO Phase 1) ──────────────

    #[test]
    fn discover_kraken3_coolant_from_chip_name() {
        // NZXT Kraken3 (Z53) exposes a single temp1 = coolant; the chip name
        // alone classifies it (no coolant label needed).
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "z53", &[("1", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].kind, SensorKind::CoolantTemp);
        // Contract: the new kind serialises as "coolant_temp".
        assert_eq!(sensors[0].kind.to_string(), "coolant_temp");
    }

    #[test]
    fn discover_kraken2_coolant_from_chip_name() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "kraken2", &[("1", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors[0].kind, SensorKind::CoolantTemp);
    }

    #[test]
    fn discover_aquacomputer_coolant_by_label_not_external_probe() {
        // Aquacomputer devices expose multiple labelled temp channels. Only the
        // labelled coolant channel classifies as coolant; an external probe
        // must NOT be force-classified (avoids false positives).
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "d5next",
            &[
                ("1", Some("Coolant temp")),
                ("2", Some("External sensor 1")),
            ],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].kind, SensorKind::CoolantTemp);
        assert_eq!(sensors[1].kind, SensorKind::MbTemp);
    }

    #[test]
    fn discover_asus_ec_water_now_classifies_coolant() {
        // Previously ASUS-EC "Water In"/"Water Out" fell through to MbTemp; the
        // coolant label hint now classifies them correctly.
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(
            tmp.path(),
            "hwmon0",
            "asus_ec_sensors",
            &[
                ("1", Some("Water In")),
                ("2", Some("Water Out")),
                ("3", Some("CPU")),
            ],
        );

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors[0].kind, SensorKind::CoolantTemp);
        assert_eq!(sensors[1].kind, SensorKind::CoolantTemp);
        // Non-coolant labels on the same chip are unaffected.
        assert_eq!(sensors[2].kind, SensorKind::CpuTemp);
    }

    #[test]
    fn discover_reads_temp_type() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "nct6683\n").unwrap();
        fs::write(hwmon_dir.join("temp1_input"), "45000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_label"), "SYSTIN\n").unwrap();
        fs::write(hwmon_dir.join("temp1_type"), "3\n").unwrap(); // diode
        fs::write(hwmon_dir.join("temp2_input"), "50000\n").unwrap();
        fs::write(hwmon_dir.join("temp2_label"), "AMD TSI Addr 98h\n").unwrap();
        fs::write(hwmon_dir.join("temp2_type"), "5\n").unwrap(); // AMD TSI

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 2);

        assert_eq!(sensors[0].temp_type, Some(3));
        assert_eq!(sensors[0].chip_name, "nct6683");
        assert_eq!(sensors[0].kind, SensorKind::MbTemp);

        assert_eq!(sensors[1].temp_type, Some(5));
        assert_eq!(sensors[1].kind, SensorKind::CpuTemp);
    }

    #[test]
    fn discover_missing_temp_type_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "k10temp", &[("1", Some("Tctl"))]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].temp_type, None);
    }

    // ── DEC-117: threshold attribute discovery ────────────────────────────

    #[test]
    fn discover_reads_curated_threshold_attributes() {
        // Set up a fixture exposing the high-value attributes: max/crit and
        // their alarms. Mirrors what nct6798 and coretemp typically expose.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "nct6798\n").unwrap();
        fs::write(hwmon_dir.join("temp1_input"), "55000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_label"), "CPUTIN\n").unwrap();
        fs::write(hwmon_dir.join("temp1_max"), "95000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_crit"), "105000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_crit_hyst"), "100000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_max_alarm"), "0\n").unwrap();
        fs::write(hwmon_dir.join("temp1_crit_alarm"), "0\n").unwrap();

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        let t = sensors[0]
            .thresholds
            .as_ref()
            .expect("thresholds populated");
        assert_eq!(t.max_c, Some(95.0));
        assert_eq!(t.crit_c, Some(105.0));
        assert_eq!(t.crit_hyst_c, Some(100.0));
        assert_eq!(t.max_alarm, Some(false));
        assert_eq!(t.crit_alarm, Some(false));
        // Attributes with no sysfs file remain None.
        assert!(t.emergency_c.is_none());
        assert!(t.lcrit_c.is_none());
        assert!(t.alarm.is_none());
        assert!(t.fault.is_none());
    }

    #[test]
    fn discover_thresholds_none_when_chip_exposes_nothing() {
        // k10temp typically exposes no thresholds — verify we set the field
        // to None rather than an all-None struct (so the API layer skips
        // emitting an empty object on the wire).
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "k10temp", &[("1", Some("Tctl"))]);
        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert!(sensors[0].thresholds.is_none());
    }

    #[test]
    fn discover_filters_implausible_threshold_values() {
        // Some kernel drivers report INT_MIN/INT_MAX or other absurd
        // placeholders when the register is unset. Daemon must drop those.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "nct6798\n").unwrap();
        fs::write(hwmon_dir.join("temp1_input"), "45000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_label"), "CPUTIN\n").unwrap();
        // -2147 °C — kernel placeholder
        fs::write(hwmon_dir.join("temp1_max"), "-2147000000\n").unwrap();
        // 250 °C — above the plausibility ceiling
        fs::write(hwmon_dir.join("temp1_crit"), "250000\n").unwrap();
        // Plausible value should still come through.
        fs::write(hwmon_dir.join("temp1_emergency"), "115000\n").unwrap();

        let sensors = discover_sensors(tmp.path()).unwrap();
        let t = sensors[0]
            .thresholds
            .as_ref()
            .expect("emergency keeps thresholds non-empty");
        assert!(t.max_c.is_none(), "INT_MIN-scale value should be dropped");
        assert!(t.crit_c.is_none(), "out-of-range value should be dropped");
        assert_eq!(t.emergency_c, Some(115.0));
    }

    #[test]
    fn discover_drops_it87_max_zero_placeholder() {
        // it87-family chips use 0 as "register not configured" for tempN_max.
        // Other thresholds at 0 °C are still legal (cold-side); only `max`
        // gets the special-case drop, and only for it87-* chips.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "it8689\n").unwrap();
        fs::write(hwmon_dir.join("temp1_input"), "40000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_label"), "temp1\n").unwrap();
        fs::write(hwmon_dir.join("temp1_max"), "0\n").unwrap();
        fs::write(hwmon_dir.join("temp1_crit"), "100000\n").unwrap();

        let sensors = discover_sensors(tmp.path()).unwrap();
        let t = sensors[0]
            .thresholds
            .as_ref()
            .expect("crit keeps non-empty");
        assert!(
            t.max_c.is_none(),
            "it87 max=0 must be dropped as placeholder"
        );
        assert_eq!(t.crit_c, Some(100.0));
    }

    #[test]
    fn discover_keeps_zero_max_for_non_it87_chips() {
        // Quirk is scoped to it87 — a 0°C max on nct6798 should be kept
        // (unusual, but not the known-placeholder pattern).
        let tmp = tempfile::tempdir().unwrap();
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "nct6798\n").unwrap();
        fs::write(hwmon_dir.join("temp1_input"), "30000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_label"), "AUXTIN0\n").unwrap();
        fs::write(hwmon_dir.join("temp1_max"), "0\n").unwrap();

        let sensors = discover_sensors(tmp.path()).unwrap();
        let t = sensors[0].thresholds.as_ref().unwrap();
        assert_eq!(t.max_c, Some(0.0));
    }

    #[test]
    fn discover_reads_alarm_bits() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "amdgpu\n").unwrap();
        fs::write(hwmon_dir.join("temp1_input"), "85000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_label"), "edge\n").unwrap();
        fs::write(hwmon_dir.join("temp1_alarm"), "1\n").unwrap();
        fs::write(hwmon_dir.join("temp1_fault"), "0\n").unwrap();

        let sensors = discover_sensors(tmp.path()).unwrap();
        let t = sensors[0].thresholds.as_ref().unwrap();
        assert_eq!(t.alarm, Some(true));
        assert_eq!(t.fault, Some(false));
    }

    #[test]
    fn discover_ignores_malformed_alarm_bit() {
        // Some drivers return values outside {0,1} for these files — treat
        // unparseable content as None rather than panic / misclassify.
        let tmp = tempfile::tempdir().unwrap();
        let hwmon_dir = tmp.path().join("hwmon0");
        fs::create_dir_all(&hwmon_dir).unwrap();
        fs::write(hwmon_dir.join("name"), "nct6798\n").unwrap();
        fs::write(hwmon_dir.join("temp1_input"), "40000\n").unwrap();
        fs::write(hwmon_dir.join("temp1_label"), "CPUTIN\n").unwrap();
        fs::write(hwmon_dir.join("temp1_alarm"), "garbage\n").unwrap();
        fs::write(hwmon_dir.join("temp1_max"), "95000\n").unwrap();

        let sensors = discover_sensors(tmp.path()).unwrap();
        let t = sensors[0].thresholds.as_ref().unwrap();
        assert!(t.alarm.is_none());
        assert_eq!(t.max_c, Some(95.0));
    }

    #[test]
    fn chip_name_propagated_to_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        create_fixture_with_chip_name(tmp.path(), "hwmon0", "it8696", &[("1", None)]);

        let sensors = discover_sensors(tmp.path()).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].chip_name, "it8696");
    }

    /// T2 (test-tests audit): table-driven classification for every arm and
    /// label sub-branch in `classify_chip`. Each row represents one mutually
    /// independent decision; if any of these rows drift the test will fail,
    /// blocking accidental removal of a chip arm or label keyword. Pure
    /// function — no sysfs fixtures needed.
    #[test]
    fn classify_chip_table_driven_all_arms_and_subbranches() {
        let cases: &[(&str, &str, SensorKind)] = &[
            // ── Direct chip-name arms ──
            ("k10temp", "Tctl", SensorKind::CpuTemp),
            ("k10temp", "anything", SensorKind::CpuTemp), // label ignored for k10temp
            ("coretemp", "Package id 0", SensorKind::CpuTemp),
            ("coretemp", "Core 3", SensorKind::CpuTemp),
            ("amdgpu", "edge", SensorKind::GpuTemp),
            ("amdgpu", "junction", SensorKind::GpuTemp),
            ("amdgpu", "mem", SensorKind::GpuTemp),
            // Intel discrete GPU drivers (DEC-121) — always GPU regardless of
            // label; xe exposes unlabelled temp2+, i915 exposes temp1.
            ("xe", "", SensorKind::GpuTemp),
            ("xe", "temp2", SensorKind::GpuTemp),
            ("i915", "", SensorKind::GpuTemp),
            ("i915", "temp1", SensorKind::GpuTemp),
            // NVIDIA via nouveau (DEC-204) — always GPU regardless of label.
            ("nouveau", "", SensorKind::GpuTemp),
            ("nouveau", "temp1", SensorKind::GpuTemp),
            ("nvme", "Composite", SensorKind::DiskTemp),
            ("nvme", "Sensor 1", SensorKind::DiskTemp),
            ("sbtsi_temp", "", SensorKind::CpuTemp),
            // ── it87* prefix arm ──
            ("it8696", "AUXTIN0", SensorKind::MbTemp),
            ("it8772", "anything", SensorKind::MbTemp),
            ("it8728", "", SensorKind::MbTemp),
            // ── NCT family default + each label sub-branch ──
            ("nct6775", "SYSTIN", SensorKind::MbTemp), // no keyword → MB
            ("nct6683", "AUXTIN0", SensorKind::MbTemp), // no keyword → MB
            ("nct6686", "PCH_CHIP", SensorKind::MbTemp),
            ("nct6687", "VRM", SensorKind::MbTemp),
            ("nct6775", "AMD TSI Addr 98h", SensorKind::CpuTemp), // amd tsi
            ("nct6683", "TSI", SensorKind::CpuTemp),              // tsi keyword
            ("nct6686", "PECI Agent 0", SensorKind::CpuTemp),     // peci keyword
            ("nct6687", "CPU", SensorKind::CpuTemp),              // cpu keyword
            ("nct6775", "cpu temp", SensorKind::CpuTemp),         // case-insensitive
            // ── ASUS EC / WMI arms ──
            ("asus_ec_sensors", "CPU", SensorKind::CpuTemp),
            ("asus_ec_sensors", "GPU", SensorKind::GpuTemp),
            ("asus_ec_sensors", "Chipset", SensorKind::MbTemp),
            ("asus_wmi_sensors", "CPU Package Temp", SensorKind::CpuTemp),
            ("asus_wmi_sensors", "GPU Hotspot", SensorKind::GpuTemp),
            ("asus_wmi_sensors", "VRM", SensorKind::MbTemp),
            // ── Gigabyte WMI arm (always MB) ──
            ("gigabyte_wmi", "temp1", SensorKind::MbTemp),
            ("gigabyte_wmi", "anything", SensorKind::MbTemp),
            // ── Fallback heuristic: each label keyword ──
            ("unknown_chip", "CPU package", SensorKind::CpuTemp),
            ("unknown_chip", "Tctl", SensorKind::CpuTemp),
            ("unknown_chip", "Tccd1", SensorKind::CpuTemp),
            ("unknown_chip", "GPU edge", SensorKind::GpuTemp),
            ("unknown_chip", "edge", SensorKind::GpuTemp),
            ("unknown_chip", "junction", SensorKind::GpuTemp),
            ("unknown_chip", "VRM", SensorKind::MbTemp), // no keyword match
            ("unknown_chip", "", SensorKind::MbTemp),    // empty label
        ];

        for (chip, label, expected) in cases {
            let got = classify_chip(chip, label);
            assert_eq!(
                got, *expected,
                "classify_chip({chip:?}, {label:?}) expected {expected:?}, got {got:?}",
            );
        }
    }

    /// CPU keywords must take precedence over GPU keywords in the fallback
    /// heuristic when both are present in the label — the arm-order at
    /// `_ => { if cpu || tctl || tccd ... else if gpu || edge || junction }`
    /// makes this an observable ordering choice. Locks the precedence so a
    /// future refactor doesn't silently flip it.
    #[test]
    fn classify_chip_fallback_cpu_keyword_wins_over_gpu_keyword() {
        // "cpu" wins over "gpu" by appearing first in the if-chain.
        assert_eq!(
            classify_chip("unknown_chip", "CPU and GPU combined sensor"),
            SensorKind::CpuTemp,
        );
        // Same precedence test using Tctl + edge.
        assert_eq!(
            classify_chip("unknown_chip", "Tctl-edge"),
            SensorKind::CpuTemp,
        );
    }
}
