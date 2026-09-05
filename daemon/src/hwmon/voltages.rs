//! Read-only inventory of board voltage rails (`WIRE-ag`).
//!
//! Super-I/O chips expose an ADC block as `inN_input` in sysfs, and until now
//! the daemon read none of it — `in0`–`in9` sit there on a typical board and
//! nothing discovered, polled or published them. This module discovers them so
//! a client can display them.
//!
//! Read-only: never writes hardware. Mirrors the injected-`&Path` root and the
//! per-directory error tolerance of `discovery` / `inventory`, so tests point it
//! at a `tempfile::tempdir()` instead of the real `/sys/class/hwmon`. **No port
//! I/O**, no `CAP_SYS_RAWIO`, and nothing here is affected by the Super-I/O
//! port-probe gate — the driver has already done the reads; this module only
//! reads the files it exports.
//!
//! # What a rail value actually means
//!
//! `inN_input` is millivolts **at the chip's input pin**, after whatever scaling
//! the *driver* applies. Boards routinely feed a rail through an external
//! resistor divider that the driver knows nothing about, so the pin voltage and
//! the rail voltage are different numbers. lm-sensors resolves this per board
//! with `/etc/sensors.d` `compute` lines; this daemon reads none, and neither
//! does the kernel.
//!
//! That is why [`VoltageDescriptor::identified`] exists. A channel the driver
//! labels (`3VSB`, `Vbat`, `+3.3V`) is one the driver claims to have identified,
//! and its value is meaningful as that rail. An **unlabelled** channel is a raw
//! ADC reading on a pin whose board wiring is unknown — it is a real measurement
//! of a real voltage, and it is *not* evidence of what any named rail is doing.
//! Publishing the two without distinction would present a divided 1.2 V reading
//! with the same authority as a direct 3.3 V one, so the flag travels with every
//! entry and clients are expected to render the distinction.
//!
//! Measured on the reference host (Gigabyte X870E AORUS MASTER, `it8696`): 10
//! rails, of which exactly 3 carry a label.
//!
//! # What is deliberately not here
//!
//! - **`inN_alarm` bits.** Measured on the same host, `in5_alarm` and `in6_alarm`
//!   read 1 while both channels sit *inside* their own `min`/`max` window. The
//!   bits are not trustworthy enough to raise anything on, and publishing them
//!   would put a fault on screen that the reading beside it disproves.
//! - **`inN_min` / `inN_max`.** Driver defaults on this chip (`0`/`3060` mV for
//!   seven of ten channels), not board limits, so they would imply a
//!   configured threshold that nobody configured.
//! - **GPU rails.** `amdgpu` publishes `vddgfx`/`vddnb` and Intel's `i915`/`xe`
//!   hwmon nodes publish `in0_input` (package voltage, per the kernel ABI doc
//!   `sysfs-driver-intel-i915-hwmon`). Those are GPU core voltages, not board
//!   rails, and reporting one as an unnamed board rail is exactly the confusion
//!   this module exists to avoid. See [`is_gpu_voltage_chip`].

use std::path::{Path, PathBuf};

use crate::error::HwmonError;

use super::util::{device_id_from_path, read_sysfs_string};

/// A discovered board voltage rail: one hwmon `inN_input` channel.
#[derive(Debug, Clone, PartialEq)]
pub struct VoltageDescriptor {
    /// Stable identifier `hwmon:<chip>:<device_id>:in<N>` — the label is
    /// deliberately **not** embedded, unlike the sensor id scheme. A rail's
    /// label can appear or change when the user installs an
    /// `/etc/sensors.d` file, and an id that moved with it would break any
    /// client that had stored one. The channel index cannot move.
    pub id: String,
    /// Hwmon chip name (e.g. `it8696`).
    pub chip_name: String,
    /// Resolved device id, as used by every other hwmon id in this daemon.
    pub device_id: String,
    /// Channel index `N` from `inN_input`.
    pub channel: u8,
    /// `inN_label` where the driver publishes one, else `in{N}`.
    pub label: String,
    /// Volts at the chip's input pin. See the module docs: this is the rail
    /// voltage only when [`Self::identified`] is true.
    pub value_v: f64,
    /// True when the driver published an `inN_label` for this channel — i.e.
    /// when the rail is identified rather than a raw ADC channel.
    pub identified: bool,
}

/// Discover all board voltage rails under a given sysfs hwmon root.
///
/// The `hwmon_root` parameter allows injecting a test fixture directory instead
/// of the real `/sys/class/hwmon`. Read-only; never writes hardware. Results are
/// sorted by id for a deterministic wire order (matching the sensor/PWM/fan
/// builders). A single unreadable device directory is skipped, not fatal.
pub fn discover_voltages(hwmon_root: &Path) -> Result<Vec<VoltageDescriptor>, HwmonError> {
    let mut descriptors = Vec::new();

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
        match discover_device_voltages(&hwmon_dir) {
            Ok(rails) => descriptors.extend(rails),
            Err(e) => {
                log::warn!(
                    "Skipping voltage discovery for {}: {e}",
                    hwmon_dir.display()
                );
            }
        }
    }

    // Sorted by (chip, device, channel) and NOT by id: ids sort
    // lexicographically, where `in10` precedes `in2`, so a chip with ten or
    // more channels would emit in0, in1, in10, in11, …, in2. Not visible on a
    // 10-channel it8696; an nct6799-class chip has fifteen.
    descriptors.sort_by(|a, b| {
        (&a.chip_name, &a.device_id, a.channel).cmp(&(&b.chip_name, &b.device_id, b.channel))
    });
    Ok(descriptors)
}

/// True for an hwmon chip whose voltage channels are GPU core rails rather than
/// board rails.
///
/// Deliberately **not** `hwmon::is_gpu_owned_hwmon_chip`, and this is the whole
/// point of the function. That predicate is the single source of truth for the
/// PWM-header and monitor-only-fan exclusions, and its own test records that it
/// omits `xe`/`i915` *because those drivers register no `pwm` attribute*. That
/// reasoning does not carry over to voltages: `i915` and `xe` both register
/// `in0_input` (package voltage — kernel ABI
/// `Documentation/ABI/testing/sysfs-driver-intel-i915-hwmon`), so an Arc or
/// Battlemage card would otherwise appear in a board-rail table as an unnamed
/// channel on chip `xe`.
///
/// Widening the shared predicate instead would change what PWM discovery
/// excludes, which is a fan-control behaviour change for a display feature.
fn is_gpu_voltage_chip(chip_name: &str) -> bool {
    crate::hwmon::is_gpu_owned_hwmon_chip(chip_name) || matches!(chip_name, "i915" | "xe")
}

/// Discover voltage rails for a single hwmon device directory.
fn discover_device_voltages(hwmon_dir: &Path) -> Result<Vec<VoltageDescriptor>, HwmonError> {
    let chip_name = read_sysfs_string(&hwmon_dir.join("name"))?
        .trim()
        .to_string();

    // GPU core voltages are not board rails — see the module docs.
    if is_gpu_voltage_chip(&chip_name) {
        return Ok(Vec::new());
    }

    let device_id = resolve_device_id(hwmon_dir);

    let entries = std::fs::read_dir(hwmon_dir).map_err(|e| HwmonError::ReadError {
        path: hwmon_dir.display().to_string(),
        message: e.to_string(),
    })?;

    // Enumerate inN_input files (in0_input, in1_input, ...). `inN_input` is the
    // only attribute that makes a channel real; a chip publishing a stray
    // `inN_label` with no input is not a rail we can report a value for.
    let mut channels: Vec<u8> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_prefix("in")
                .and_then(|s| s.strip_suffix("_input"))
                .and_then(|n| n.parse::<u8>().ok())
        })
        .collect();

    channels.sort_unstable();

    let mut rails = Vec::new();
    for channel in channels {
        let input_path = hwmon_dir.join(format!("in{channel}_input"));
        // A channel whose value will not read or parse is dropped rather than
        // failing the chip: unlike a sensor label (DEC-272), nothing downstream
        // derives identity or safety from a rail, so the cheaper error is to
        // report the rails that did read.
        // `parse::<f64>` accepts "NaN" and "inf", and serde_json then serialises
        // a non-finite f64 as `null` — which would break this endpoint's own
        // `value_v: f64` contract from inside the daemon. Dropped rather than
        // coerced to 0.0 the way `util::sanitize_f64` would: on a rail table
        // "0.000 V" is a claim that the rail is dead, which is worse than
        // saying nothing.
        let Some(millivolts) = read_sysfs_string(&input_path)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|mv| mv.is_finite())
        else {
            log::warn!("Skipping unreadable voltage rail {}", input_path.display());
            continue;
        };

        // An empty label file is treated as no label — the driver published the
        // attribute but named nothing, which identifies the rail no better than
        // its absence does.
        let label_path = hwmon_dir.join(format!("in{channel}_label"));
        let published = read_sysfs_string(&label_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let identified = published.is_some();
        let label = published.unwrap_or_else(|| format!("in{channel}"));

        rails.push(VoltageDescriptor {
            id: format!("hwmon:{chip_name}:{device_id}:in{channel}"),
            chip_name: chip_name.clone(),
            device_id: device_id.clone(),
            channel,
            label,
            value_v: millivolts / 1000.0,
            identified,
        });
    }

    Ok(rails)
}

/// Resolve the hwmon device id, matching `inventory::resolve_device_id`.
fn resolve_device_id(hwmon_dir: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build an hwmon device directory with a chip name and a set of
    /// `(channel, millivolts, label)` rails. A `None` label writes no label file.
    fn write_chip(root: &Path, dir: &str, chip: &str, rails: &[(u8, &str, Option<&str>)]) {
        let d = root.join(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("name"), format!("{chip}\n")).unwrap();
        for (ch, mv, label) in rails {
            fs::write(d.join(format!("in{ch}_input")), format!("{mv}\n")).unwrap();
            if let Some(l) = label {
                fs::write(d.join(format!("in{ch}_label")), format!("{l}\n")).unwrap();
            }
        }
    }

    #[test]
    fn labelled_rail_is_identified_and_scaled_to_volts() {
        let root = tempdir().unwrap();
        write_chip(
            root.path(),
            "hwmon4",
            "it8696",
            &[(7, "3288", Some("3VSB"))],
        );

        let rails = discover_voltages(root.path()).unwrap();

        assert_eq!(rails.len(), 1);
        assert_eq!(rails[0].label, "3VSB");
        assert!(rails[0].identified);
        assert_eq!(rails[0].channel, 7);
        // Millivolts on the wire from sysfs, volts in the descriptor.
        assert!((rails[0].value_v - 3.288).abs() < 1e-9);
    }

    /// The whole point of the flag: an unlabelled channel is a raw ADC reading,
    /// and must be distinguishable from an identified rail. Asserted as the
    /// relationship between the two entries rather than as two literals, so a
    /// call site that hardcoded either answer fails.
    #[test]
    fn unlabelled_rail_is_not_identified_and_falls_back_to_channel_name() {
        let root = tempdir().unwrap();
        write_chip(
            root.path(),
            "hwmon4",
            "it8696",
            &[(0, "1236", None), (7, "3288", Some("3VSB"))],
        );

        let rails = discover_voltages(root.path()).unwrap();

        assert_eq!(rails.len(), 2);
        let raw = rails.iter().find(|r| r.channel == 0).unwrap();
        let named = rails.iter().find(|r| r.channel == 7).unwrap();
        assert_eq!(raw.label, "in0");
        assert_eq!(named.label, "3VSB");
        assert_ne!(
            raw.identified, named.identified,
            "a labelled and an unlabelled rail must not report the same identification"
        );
        assert!(!raw.identified);
        assert!(named.identified);
    }

    /// A label file the driver published but left empty identifies the rail no
    /// better than its absence, so it must not set the flag.
    #[test]
    fn empty_label_file_does_not_identify_the_rail() {
        let root = tempdir().unwrap();
        let d = root.path().join("hwmon4");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("name"), "it8696\n").unwrap();
        fs::write(d.join("in0_input"), "1236\n").unwrap();
        fs::write(d.join("in0_label"), "  \n").unwrap();

        let rails = discover_voltages(root.path()).unwrap();

        assert_eq!(rails.len(), 1);
        assert!(!rails[0].identified);
        assert_eq!(rails[0].label, "in0");
    }

    /// `WIRE-ag` review: `i915`/`xe` publish `in0_input` (package voltage) and
    /// are NOT covered by `is_gpu_owned_hwmon_chip`, whose omission of them is
    /// justified by their lack of a `pwm` attribute. A board-rail table must not
    /// show a GPU core voltage as an unnamed channel.
    #[test]
    fn gpu_chips_are_excluded_including_the_intel_ones() {
        for gpu_chip in ["amdgpu", "nouveau", "i915", "xe"] {
            let root = tempdir().unwrap();
            write_chip(
                root.path(),
                "hwmon1",
                gpu_chip,
                &[(0, "1050", Some("vpkg"))],
            );
            write_chip(
                root.path(),
                "hwmon4",
                "it8696",
                &[(9, "3072", Some("+3.3V"))],
            );

            let rails = discover_voltages(root.path()).unwrap();

            assert_eq!(
                rails
                    .iter()
                    .map(|r| r.chip_name.as_str())
                    .collect::<Vec<_>>(),
                vec!["it8696"],
                "{gpu_chip} rails must not be reported as board rails"
            );
        }
    }

    /// `parse::<f64>` accepts "NaN"/"inf", and serde_json serialises a
    /// non-finite f64 as `null` — breaking this endpoint's own `value_v: f64`
    /// contract from inside the daemon.
    #[test]
    fn non_finite_readings_are_dropped_rather_than_published() {
        for bad in ["NaN", "inf", "-inf"] {
            let root = tempdir().unwrap();
            write_chip(
                root.path(),
                "hwmon4",
                "it8696",
                &[(0, bad, None), (7, "3288", Some("3VSB"))],
            );

            let rails = discover_voltages(root.path()).unwrap();

            assert_eq!(rails.len(), 1, "{bad} must not reach the wire");
            assert_eq!(rails[0].channel, 7);
            assert!(rails[0].value_v.is_finite());
        }
    }

    /// Channels must come out in NUMERIC order, and the sample has to be able to
    /// show it: with only single-digit channels this passes under a
    /// lexicographic id sort too, which is how the first version of this test
    /// proved nothing (`CLAUDE.md` — "pick the sample that can move"). Asserted
    /// as the realised sequence rather than by re-deriving the sort.
    #[test]
    fn rails_come_out_in_numeric_channel_order_past_ten_channels() {
        let root = tempdir().unwrap();
        write_chip(
            root.path(),
            "hwmon4",
            "nct6799",
            &[
                (10, "1000", None),
                (2, "2000", None),
                (0, "3000", None),
                (11, "4000", None),
                (9, "5000", None),
            ],
        );

        let rails = discover_voltages(root.path()).unwrap();

        assert_eq!(
            rails.iter().map(|r| r.channel).collect::<Vec<u8>>(),
            vec![0, 2, 9, 10, 11],
            "channels must sort numerically; sorting by id puts in10 before in2"
        );
    }

    /// Two chips must not interleave, and each stays in numeric order.
    #[test]
    fn rails_group_by_chip_before_channel() {
        let root = tempdir().unwrap();
        write_chip(
            root.path(),
            "hwmon0",
            "nct6799",
            &[(10, "1000", None), (2, "2000", None)],
        );
        write_chip(
            root.path(),
            "hwmon4",
            "it8696",
            &[(3, "3000", None), (1, "4000", None)],
        );

        let rails = discover_voltages(root.path()).unwrap();

        assert_eq!(
            rails
                .iter()
                .map(|r| (r.chip_name.as_str(), r.channel))
                .collect::<Vec<_>>(),
            vec![
                ("it8696", 1),
                ("it8696", 3),
                ("nct6799", 2),
                ("nct6799", 10)
            ]
        );
    }

    /// A stray `inN_label` with no `inN_input` is not a rail — there is no value
    /// to report for it.
    #[test]
    fn label_without_input_is_not_a_rail() {
        let root = tempdir().unwrap();
        let d = root.path().join("hwmon4");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("name"), "it8696\n").unwrap();
        fs::write(d.join("in3_label"), "+12V\n").unwrap();

        assert!(discover_voltages(root.path()).unwrap().is_empty());
    }

    /// An unreadable value drops that channel, not the whole chip.
    #[test]
    fn unparseable_rail_is_skipped_without_losing_its_siblings() {
        let root = tempdir().unwrap();
        write_chip(
            root.path(),
            "hwmon4",
            "it8696",
            &[(0, "not-a-number", None), (7, "3288", Some("3VSB"))],
        );

        let rails = discover_voltages(root.path()).unwrap();

        assert_eq!(rails.len(), 1);
        assert_eq!(rails[0].channel, 7);
    }

    /// A chip with no `name` fails its own directory and must not fail the scan.
    #[test]
    fn a_broken_device_directory_is_skipped_not_fatal() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("hwmon0")).unwrap();
        write_chip(
            root.path(),
            "hwmon4",
            "it8696",
            &[(7, "3288", Some("3VSB"))],
        );

        let rails = discover_voltages(root.path()).unwrap();

        assert_eq!(rails.len(), 1);
    }

    #[test]
    fn a_chip_with_no_voltage_channels_reports_none() {
        let root = tempdir().unwrap();
        write_chip(root.path(), "hwmon0", "k10temp", &[]);

        assert!(discover_voltages(root.path()).unwrap().is_empty());
    }

    /// The id must not move when a label appears — a user dropping an
    /// `/etc/sensors.d` file in must not invalidate a stored id.
    #[test]
    fn id_is_stable_across_a_label_appearing() {
        let root = tempdir().unwrap();
        write_chip(root.path(), "hwmon4", "it8696", &[(3, "1992", None)]);
        let before = discover_voltages(root.path()).unwrap();

        fs::write(root.path().join("hwmon4/in3_label"), "+12V\n").unwrap();
        let after = discover_voltages(root.path()).unwrap();

        assert_eq!(before[0].id, after[0].id);
        assert_ne!(before[0].label, after[0].label);
        assert!(!before[0].identified && after[0].identified);
    }
}
