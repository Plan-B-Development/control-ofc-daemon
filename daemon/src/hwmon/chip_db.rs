//! Shared chip↔driver knowledge base and read-only hardware-detection
//! primitives.
//!
//! This is the single source of truth for "which kernel module drives this
//! Super-I/O chip, is it in mainline, does its I/O range collide with ACPI,
//! and what board is this". It is composed by two consumers:
//!   - the `GET /diagnostics/hardware` endpoint (via the thin `api::diagnostics`
//!     re-export shim), and
//!   - the `hwmon::superio` passive Super-I/O detector.
//!
//! Everything here is a passive read of already-populated kernel surfaces
//! (`/proc/modules`, `/proc/ioports`, `/sys/class/dmi`, `/dev/kmsg`,
//! `/proc/cpuinfo`) plus static lookup tables — it never probes I/O ports,
//! loads modules, or writes any sysfs attribute. Extracted from
//! `api::diagnostics` so the Super-I/O detector can depend on it without an
//! `hwmon → api` layering inversion (DEC-202 groundwork).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::api::responses::{AcpiConflictInfo, KernelModuleInfo, ModuleCollisionInfo};

/// Known hwmon driver modules and whether they're in the mainline kernel.
///
/// Note on `it87`: the *module name* exists in the mainline tree, but most
/// AM5/Z790-class chips people run (IT8625E/IT8686E/IT8688E/IT8696E, and —
/// pragmatically — IT8689E) still want the out-of-tree frankcrawford/it87
/// fork. IT8689E fan *control* only landed in mainline 7.1 (commit 66b8eaf)
/// and IT87952E enumerates since 6.4, but on the 6.12/6.18 LTS kernels most
/// users run, and for dual-chip control, the DKMS build is what they need.
/// Marking the module `false` keeps the modules table honest for DKMS users;
/// the chip-level column (`chip_driver_in_mainline`) reports per-chip.
const KNOWN_MODULES: &[(&str, bool)] = &[
    ("nct6775", true),
    ("nct6775_core", true),
    ("nct6775_platform", true),
    ("nct6683", true),
    ("nct6687", false),
    ("it87", false),
    ("f71882fg", true),
    // Additional Super-I/O hardware-monitor drivers (DEC-202). All long-mainline
    // (15+ years in-kernel; verified present in 7.2-rc2 Kconfig/Makefile and the
    // per-driver docs.kernel.org/hwmon/* "Supported chips" pages, 2026-07-07).
    // Unlike ITE, none of these families has an out-of-tree fork situation, so
    // `mainline = true` is unconditional for the whole family.
    ("f71805f", true),    // Fintek F71805F/806F/872F — distinct from f71882fg
    ("w83627ehf", true),  // Winbond W83627EHF/EHG/DHG/UHG, W83667HG
    ("w83627hf", true),   // Winbond W83627HF/THF, W83637HF, W83687THF, W83697HF
    ("smsc47m1", true),   // SMSC LPC47M10x/M11x/M13x/M14x/M15x/M19x, LPC47M292
    ("smsc47b397", true), // SMSC LPC47B397-NC, SCH5307-NS, SCH5317
    ("dme1737", true),    // SMSC DME1737, SCH311x, SCH5027, SCH5127
    ("pc87360", true),    // National PC87360/363/364/365/366
    ("pc87427", true),    // National PC87427
    ("asus_ec_sensors", true),
    ("asus_wmi_sensors", true),
    ("asus_wmi_ec_sensors", true),
    // ASUS ATK0110 ACPI hwmon — read-only sensors only. Mainline since
    // ~2.6.something. Tracked here so diagnostics can advise "this is a
    // sensor-read driver, not a PWM control path" when present.
    ("asus_atk0110", true),
    ("sch5627", true),
    ("sch5636", true),
    ("k10temp", true),
    ("coretemp", true),
    ("amdgpu", true),
    // DEC-110: intel_pch_thermal registers a hwmon device exposing
    // `temp1_input` (PCH temperature) on Intel platforms. It is sensor
    // enrichment only, NOT a PWM control path. Tracked so diagnostics can
    // honestly say "intel_pch_thermal: loaded (mainline)" on Intel boxes
    // rather than silently dropping it from the modules table.
    ("intel_pch_thermal", true),
    // x86_pkg_temp is intentionally NOT listed. Per the kernel
    // `x86_pkg_temp_thermal` driver, it registers with `.no_hwmon = true`,
    // so it appears only as a thermal_zone — never under
    // `/sys/class/hwmon`. Listing it would falsely advertise a hwmon
    // source we cannot read. Use `coretemp` for per-core / package CPU
    // temperatures on Intel.
];

/// Map an hwmon chip `name` (the string in `/sys/class/hwmon/hwmonN/name`) to
/// the kernel driver module that binds it.
///
/// Prefixes are matched **specific-before-general** — e.g. the Fintek
/// `f71805`/`f71806`/`f71872` chips must resolve to the separate `f71805f`
/// driver *before* the general `f718*` → `f71882fg` fallthrough, and the SMSC
/// `sch5307`/`sch5317`/`sch311x`/`sch5027`/`sch5127` codes must not be confused
/// with the distinct `sch5627`/`sch5636` drivers.
///
/// Coverage (DEC-202): ITE (it87), Nuvoton (nct6775 / nct6683 monitoring-only /
/// out-of-tree nct6687d), Fintek (f71805f, f71882fg), Winbond (w83627ehf,
/// w83627hf), SMSC (smsc47m1, smsc47b397, dme1737, sch5627, sch5636) and
/// National (pc87360, pc87427). Each mapping is verified against that driver's
/// docs.kernel.org "Supported chips" / "Prefix" listing (2026-07-07).
fn expected_driver_for_chip(chip_name: &str) -> &'static str {
    let lower = chip_name.to_lowercase();

    // ── Nuvoton ──
    // NCT6683/6686/6687 are a distinct family from the nct6775 line. The
    // mainline `nct6683` driver (monitoring-only — firmware disables writes)
    // reports hwmon name "nct6683"; the out-of-tree `nct6687d` driver reports
    // the actual chip ("nct6686"/"nct6687") and adds fan control. So the hwmon
    // name we observe tells us which driver bound it. Everything else in the
    // NCT6xxx/NCT5xxx range is the in-kernel nct6775 driver.
    if lower.starts_with("nct6683") {
        return "nct6683"; // mainline, monitoring-only
    }
    if lower.starts_with("nct6686") || lower.starts_with("nct6687") {
        return "nct6687"; // out-of-tree nct6687d; DEC-106 collision risk vs nct6775
    }
    if lower.starts_with("nct6") || lower.starts_with("nct5") {
        return "nct6775";
    }

    // ── ITE (per-chip mainline vs DKMS handled by chip_driver_in_mainline) ──
    if lower.starts_with("it8") {
        return "it87";
    }

    // ── Fintek: specific f71805/806/872 → f71805f BEFORE general → f71882fg ──
    if lower.starts_with("f71805") || lower.starts_with("f71806") || lower.starts_with("f71872") {
        return "f71805f";
    }
    if lower.starts_with("f718") || lower.starts_with("f8000") || lower.starts_with("f818") {
        return "f71882fg";
    }

    // ── Winbond: EHF/DHG/UHG family vs the older HF family (both "w836…") ──
    if lower.starts_with("w83627ehf")
        || lower.starts_with("w83627dhg")
        || lower.starts_with("w83627uhg")
        || lower.starts_with("w83667hg")
    {
        return "w83627ehf";
    }
    if lower.starts_with("w83627hf")
        || lower.starts_with("w83627thf")
        || lower.starts_with("w83637hf")
        || lower.starts_with("w83687thf")
        || lower.starts_with("w83697hf")
    {
        return "w83627hf";
    }

    // ── SMSC family ── (order the SCH codes so none masks another)
    if lower.starts_with("smsc47b397")
        || lower.starts_with("sch5307")
        || lower.starts_with("sch5317")
    {
        return "smsc47b397";
    }
    if lower.starts_with("smsc47m") {
        return "smsc47m1"; // covers 'smsc47m1' and 'smsc47m2'
    }
    if lower.starts_with("dme1737")
        || lower.starts_with("sch311")
        || lower.starts_with("sch5027")
        || lower.starts_with("sch5127")
    {
        return "dme1737";
    }
    if lower.starts_with("sch5627") {
        return "sch5627";
    }
    if lower.starts_with("sch5636") {
        return "sch5636";
    }

    // ── National Semiconductor ──
    if lower.starts_with("pc8736") {
        return "pc87360"; // PC87360/363/364/365/366
    }
    if lower.starts_with("pc87427") {
        return "pc87427";
    }

    "unknown"
}

/// Whether a chip's driver is in the mainline kernel.
///
/// The ITE list mirrors mainline `it87` chip support
/// (`drivers/hwmon/it87.c` enum + docs.kernel.org/hwmon/it87.html,
/// verified against v7.1 / 7.2-rc4, master 2026-07-21). `it8622` added in DEC-144. `it8689`
/// is deliberately NOT listed (DEC-144, re-evaluated 2026-07): mainline 7.1
/// added IT8689E fan *control* (commit 66b8eaf — six PWM, FEAT_FANCTL_ONOFF,
/// not just sensors; released 2026-06-14), but 7.1 is not yet the common
/// kernel (the 6.12/6.18 LTS lines are) and some Gigabyte Rev 1 boards
/// still have EC quirks — reporting "mainline: yes" would steer users off
/// the DKMS build they still need. Revisit once 7.1+ is the common floor.
pub fn chip_driver_in_mainline(chip_name: &str) -> bool {
    let driver = expected_driver_for_chip(chip_name);
    // ITE chips IT8625E+ require out-of-tree frankcrawford/it87
    if driver == "it87" {
        let lower = chip_name.to_lowercase();
        let mainline_chips = [
            "it8603", "it8620", "it8622", "it8623", "it8628", "it8705", "it8712", "it8716",
            "it8718", "it8720", "it8721", "it8726", "it8728", "it8732", "it8758", "it8771",
            "it8772", "it8781", "it8782", "it8783", "it8786", "it8790", "it8792", "it8795",
            "it87952",
        ];
        return mainline_chips.iter().any(|c| lower.starts_with(c));
    }
    KNOWN_MODULES
        .iter()
        .find(|(name, _)| *name == driver)
        .map(|(_, mainline)| *mainline)
        .unwrap_or(false)
}

/// Return the expected driver name for a chip.
pub fn expected_driver(chip_name: &str) -> &'static str {
    expected_driver_for_chip(chip_name)
}

/// Whether `chip_name` is a Super-I/O hardware-monitor chip this project
/// recognizes — i.e. a known Super-I/O driver ([`expected_driver`]) maps to it.
///
/// This is the authoritative "is this a Super-I/O chip name?" gate for the
/// passive detector's **bound-hwmon** evidence: it lets ordinary sensor chips
/// (k10temp, coretemp, amdgpu, nvme, spd5118, zenpower, …) — which are legitimate
/// hwmon devices but not Super-I/O monitoring chips — be dropped so they are never
/// mis-reported as "Unrecognized Super-I/O" (DEC-207). It is deliberately distinct
/// from [`crate::hwmon::classify::is_superio_chip`], which answers a *different*
/// question (a CPU/MB temperature-confidence heuristic over `nct6*`/`it8*` only).
pub fn is_known_superio_chip(chip_name: &str) -> bool {
    expected_driver_for_chip(chip_name) != "unknown"
}

/// Detect which known hwmon kernel modules are currently loaded.
pub fn detect_loaded_modules() -> Vec<KernelModuleInfo> {
    detect_loaded_modules_from(Path::new("/proc/modules"))
}

/// Testable variant with injectable path.
pub fn detect_loaded_modules_from(proc_modules: &Path) -> Vec<KernelModuleInfo> {
    let content = match std::fs::read_to_string(proc_modules) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Cannot read {}: {e}", proc_modules.display());
            return KNOWN_MODULES
                .iter()
                .map(|(name, mainline)| KernelModuleInfo {
                    name: name.to_string(),
                    loaded: false,
                    in_mainline: *mainline,
                })
                .collect();
        }
    };

    let loaded: HashMap<&str, bool> = content
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(|name| (name, true))
        .collect();

    KNOWN_MODULES
        .iter()
        .map(|(name, mainline)| KernelModuleInfo {
            name: name.to_string(),
            loaded: loaded.contains_key(name),
            in_mainline: *mainline,
        })
        .collect()
}

/// One entry in the known-bad simultaneous-load lookup. See
/// `ModuleCollisionInfo` for why these matter (chip-ID overlap → wrong
/// driver can scribble into another chip's non-volatile state).
///
/// Kept as a small static table because the failure modes are rare,
/// well-documented, and the remediation in each case is identical
/// regardless of which module ended up binding first (blacklist one of
/// them).
struct ModuleCollisionEntry {
    module_a: &'static str,
    module_b: &'static str,
    severity: &'static str,
    summary: &'static str,
    remediation: &'static str,
}

const MODULE_COLLISIONS: &[ModuleCollisionEntry] = &[ModuleCollisionEntry {
    module_a: "nct6687",
    module_b: "nct6775",
    severity: "critical",
    summary: "nct6687 (out-of-tree) and nct6775 (in-kernel) are both loaded. \
             They race for the same Super I/O chip on MSI AM4/AM5 boards. \
             Older nct6687 builds declare chip ID 0xd450 — the legitimate \
             NCT6797D ID — so the wrong driver can write into the chip's \
             non-volatile fan control state and brick the affected header \
             (CPU_FAN is the most common casualty). The 0xd450 claim was \
             removed upstream in Fred78290/nct6687d PR #164 (2026); updating \
             the driver removes the brick mechanism, but already-loaded \
             modules and not-yet-updated packages remain at risk.",
    remediation: "(1) Identify the chip FIRST: run `cat /sys/class/hwmon/hwmon*/name` \
             to see which driver actually bound on this boot. \
             (2) If the chip is NCT6687-R (genuine MSI 500/600-series chip), \
             blacklist nct6775 instead: `echo 'blacklist nct6775' | sudo tee \
             /etc/modprobe.d/blacklist-nct6775.conf`. \
             (3) If the chip is NCT6797D or NCT6798D (common on AM4 400/500 MSI \
             boards e.g. B450M MORTAR, X470 GAMING PRO CARBON, MAG B450 TOMAHAWK \
             MAX), blacklist nct6687: `echo 'blacklist nct6687' | sudo tee \
             /etc/modprobe.d/blacklist-nct6687.conf`. \
             (4) Reboot. Do NOT write PWM until you have verified the chip and \
             blacklisted the OTHER driver — blacklisting the wrong one will \
             leave you with no fan control. \
             (Prevention: a current nct6687d build, post-PR #164, no longer \
             claims 0xd450 — updating the package is the durable fix.)",
}];

/// Minimal chip-binding record passed into the collision detector so it
/// can distinguish legitimate dual-Nuvoton boards from the brick scenario.
///
/// `chip_name` is the hwmon-reported name (e.g. `"nct6686"`, `"nct6798"`,
/// `"nct6799"`) — i.e. what the bound driver actually claimed. `device_id`
/// is the chip's stable platform identifier from the headers list
/// (typically the SuperIO I/O address segment such as `"nodev"`,
/// `"isa-0290"`, or a fully-qualified platform string). What matters for
/// the refinement (DEC-106) is whether multiple distinct nct6 chips exist
/// at distinct identifiers — the actual format does not need to be parsed.
pub struct ChipBinding<'a> {
    pub chip_name: &'a str,
    pub device_id: &'a str,
}

/// Detect pairs of loaded driver modules that are known to race for the
/// same chip. Returns one entry per detected collision; empty Vec when
/// none are present (the common case).
///
/// `chips` carries the currently-bound hwmon chips so the detector can
/// distinguish legitimate dual-Nuvoton boards (DEC-106). Pass `&[]` to
/// disable that refinement (every (nct6687, nct6775) load is then flagged).
pub fn detect_module_collisions(chips: &[ChipBinding<'_>]) -> Vec<ModuleCollisionInfo> {
    detect_module_collisions_from(Path::new("/proc/modules"), chips)
}

/// Testable variant. Reads loaded-module names from the supplied path,
/// compares against the static `MODULE_COLLISIONS` table, and returns
/// every pair that is concurrently present. Order of `module_a` and
/// `module_b` in the response mirrors the static table so the GUI
/// renders a deterministic banner.
///
/// DEC-106 refinement: when both modules of a pair are loaded but `chips`
/// shows multiple distinct nct6 chips at distinct `device_id`s (i.e. the
/// board has separate physical Super-I/O chips, one bound by nct6687d and
/// the other by nct6775), the collision is suppressed. This avoids a
/// false CRITICAL banner on legitimate dual-Nuvoton boards such as the
/// ASRock X870E Taichi Lite (NCT6686 at 0x0a20 + NCT6799 at 0x0290) while
/// keeping the original brick-risk detection intact for single-chip
/// boards where chip ID 0xd450 (NCT6797D) is the contested address.
pub fn detect_module_collisions_from(
    proc_modules: &Path,
    chips: &[ChipBinding<'_>],
) -> Vec<ModuleCollisionInfo> {
    let content = match std::fs::read_to_string(proc_modules) {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "detect_module_collisions: cannot read {}: {e}",
                proc_modules.display()
            );
            return Vec::new();
        }
    };
    let loaded: std::collections::HashSet<&str> = content
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    MODULE_COLLISIONS
        .iter()
        .filter(|entry| loaded.contains(entry.module_a) && loaded.contains(entry.module_b))
        .filter(|entry| !is_dual_nuvoton_safe_pair(entry, chips))
        .map(|entry| ModuleCollisionInfo {
            module_a: entry.module_a.to_string(),
            module_b: entry.module_b.to_string(),
            severity: entry.severity.to_string(),
            summary: entry.summary.to_string(),
            remediation: entry.remediation.to_string(),
        })
        .collect()
}

/// DEC-106: a `(nct6687, nct6775)` simultaneous load is benign on boards
/// that actually have TWO distinct nct6 chips (different `device_id`s),
/// because each driver binds to its own physical chip. The brick scenario
/// requires a single chip whose ID overlaps both drivers' tables — that
/// only happens on boards with one nct6 chip and the canonical 0xd450
/// (NCT6797D) ID.
///
/// Rule: suppress only the canonical `(nct6687, nct6775)` pair, and only
/// when `chips` shows at least two distinct nct6-family `device_id`s
/// (different physical chips). Any other entry in `MODULE_COLLISIONS` is
/// emitted unchanged.
fn is_dual_nuvoton_safe_pair(entry: &ModuleCollisionEntry, chips: &[ChipBinding<'_>]) -> bool {
    if entry.module_a != "nct6687" || entry.module_b != "nct6775" {
        return false;
    }
    // Distinct device_ids among nct6-family chips. We deliberately match
    // any chip name starting with "nct6" — the bound driver may report
    // the chip by family name (e.g. "nct6686", "nct6798", "nct6799")
    // and we do not need to parse the I/O address out of `device_id`.
    //
    // Closed-family assumption: every hwmon chip name in the wild that
    // starts with `nct6` belongs to the Nuvoton NCT6xxx Super-I/O family
    // (NCT6683/6686/6687/6775/6776/6779/6791/6792/6795/6796/6797/6798/
    // 6799). The kernel `nct6775-platform.c` chip table and Fred78290/
    // nct6687d source both enumerate this family explicitly, and no
    // non-Nuvoton hwmon driver claims the `nct6` prefix. If a future
    // hwmon family ever uses this prefix, the assumption would need to
    // be revisited; the `expected_driver_for_chip` function in the same
    // module already relies on the identical assumption for the
    // `nct6775` driver mapping.
    let distinct: std::collections::HashSet<&str> = chips
        .iter()
        .filter(|c| c.chip_name.to_lowercase().starts_with("nct6"))
        .map(|c| c.device_id)
        .collect();
    distinct.len() >= 2
}

/// If loading `module` would collide with an already-loaded driver (per the
/// `MODULE_COLLISIONS` table), return the name of that loaded counterpart.
///
/// Used by the `hwmon::superio` recommender to annotate a brick-risky "load
/// this driver" suggestion (DEC-106) without duplicating the collision table —
/// e.g. recommending `nct6775` while the out-of-tree `nct6687` is already
/// loaded (or vice-versa) returns `Some("nct6687")`. Returns `None` when the
/// module is not part of any known collision pair, or its counterpart is not
/// loaded.
pub fn conflicting_loaded_module<'a>(module: &str, loaded: &'a [String]) -> Option<&'a str> {
    for entry in MODULE_COLLISIONS {
        let counterpart = if entry.module_a == module {
            entry.module_b
        } else if entry.module_b == module {
            entry.module_a
        } else {
            continue;
        };
        if let Some(hit) = loaded.iter().find(|m| m.as_str() == counterpart) {
            return Some(hit.as_str());
        }
    }
    None
}

/// I/O port ranges used by common Super I/O chips.
const SIO_IO_RANGES: &[(&str, u16, u16)] = &[
    ("nct6775", 0x0290, 0x0299),
    ("nct6775", 0x04E0, 0x04EF),
    ("it87", 0x0290, 0x029F),
    ("it87", 0x0A20, 0x0A2F),
    ("it87", 0x0A40, 0x0A4F),
    ("it87", 0x0A60, 0x0A6F),
];

/// Detect ACPI I/O port conflicts with hwmon drivers.
pub fn detect_acpi_conflicts() -> Vec<AcpiConflictInfo> {
    detect_acpi_conflicts_from(Path::new("/proc/ioports"))
}

/// Testable variant with injectable path.
pub fn detect_acpi_conflicts_from(proc_ioports: &Path) -> Vec<AcpiConflictInfo> {
    let content = match std::fs::read_to_string(proc_ioports) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Cannot read {}: {e}", proc_ioports.display());
            return vec![];
        }
    };

    let mut conflicts = Vec::new();

    // Parse /proc/ioports lines like:
    //   0290-0299 : ACPI OpRegion AMW0.SHWM
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("ACPI") && !trimmed.contains("acpi") {
            continue;
        }

        // Parse the range: "0290-0299 : description"
        let parts: Vec<&str> = trimmed.splitn(2, " : ").collect();
        if parts.len() != 2 {
            continue;
        }
        let range_str = parts[0].trim();
        let description = parts[1].trim();

        let range_parts: Vec<&str> = range_str.split('-').collect();
        if range_parts.len() != 2 {
            continue;
        }

        let start = match u16::from_str_radix(range_parts[0].trim(), 16) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let end = match u16::from_str_radix(range_parts[1].trim(), 16) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check overlap with known SIO ranges
        for (driver, sio_start, sio_end) in SIO_IO_RANGES {
            if start <= *sio_end && end >= *sio_start {
                conflicts.push(AcpiConflictInfo {
                    io_range: range_str.to_string(),
                    claimed_by: description.to_string(),
                    conflicts_with_driver: driver.to_string(),
                });
            }
        }
    }

    conflicts
}

// ── DMI board identification ──────────────────────────────────────

use crate::api::responses::BoardInfo;

/// Read motherboard identification from DMI sysfs (world-readable, no root required).
pub fn read_board_info() -> BoardInfo {
    read_board_info_from(Path::new("/sys/class/dmi/id"))
}

/// Testable variant with injectable path.
pub fn read_board_info_from(dmi_dir: &Path) -> BoardInfo {
    let read_field = |field: &str| -> String {
        std::fs::read_to_string(dmi_dir.join(field))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    BoardInfo {
        vendor: read_field("board_vendor"),
        name: read_field("board_name"),
        bios_version: read_field("bios_version"),
    }
}

// ── DMI → expected-chips lookup (DEC-101) ─────────────────────────
//
// Some Gigabyte boards expose two ITE Super-IO chips on a single PCB. The
// upstream frankcrawford/it87 driver scans both 0x2E and 0x4E SuperIO base
// addresses, but a stale SuperIO state (e.g. left in config-mode by a prior
// `sensors-detect` run) or a missing `mmio=on` modparam on PR-#77-era code
// can cause the secondary chip's DEVID read to return 0xFFFF and the driver
// silently gives up on the second chip. Result: only N of M expected PWM
// headers reach hwmon.
//
// We expose the expected chip-list to the GUI so it can render a dual-chip
// warning when `expected_chips - chips_detected` is non-empty. The list is
// sourced from the it87.c DMI table (see linux source) plus community
// reports — chip names are normalised to the same format hwmon reports
// (lowercased, no `E` suffix). When a board is not in the table we return
// an empty Vec, the GUI does nothing and the rest of diagnostics keep
// working unchanged.
//
// Updates to this table are board-by-board; do not encode "any X870E
// Aorus" globs because Gigabyte ships single-chip variants with similar
// names. Each entry is a deliberate match.

/// One entry in the dual-chip board lookup. `board_name` is matched
/// case-insensitively as a substring (or exact, if more specific
/// matching is needed) against DMI `board_name`. `chips` lists the
/// hwmon chip names expected — usually two, occasionally three.
struct DualChipEntry {
    /// DMI board_name (case-insensitive substring match).
    board_name: &'static str,
    /// Expected chip names in `chip_name` format (e.g. "it8696", "it87952").
    chips: &'static [&'static str],
}

const GIGABYTE_DUAL_CHIP_BOARDS: &[DualChipEntry] = &[
    // ── X870E AORUS family (IT8696E + IT87952E) ────────────────
    DualChipEntry {
        board_name: "X870E AORUS MASTER",
        chips: &["it8696", "it87952"],
    },
    DualChipEntry {
        board_name: "X870E AORUS PRO",
        chips: &["it8696", "it87952"],
    },
    // DEC-144: owner-confirmed dual-chip per frankcrawford/it87 issue
    // #89 (`it8696-isa-0a40` 5 fans + `it87952-isa-0a60` 3 fans,
    // control working). Substring match also covers the 2026 "X870E
    // AORUS ELITE X3D" refresh.
    DualChipEntry {
        board_name: "X870E AORUS ELITE",
        chips: &["it8696", "it87952"],
    },
    DualChipEntry {
        board_name: "X870 AORUS ELITE WIFI7",
        chips: &["it8696", "it87952"],
    },
    // DEC-144 caveat: the frankcrawford/it87 DMI table annotates the
    // ICE variant as a SINGLE-chip IT8696E board, conflicting with this
    // dual-chip expectation. Driver comments are contributor notes, not
    // authoritative, and removing this entry would not change behaviour
    // anyway (the plain "X870 AORUS ELITE WIFI7" entry above substring-
    // matches the ICE name). Resolving it properly needs exact-match
    // lookup support — deferred until an ICE owner report settles the
    // topology.
    DualChipEntry {
        board_name: "X870 AORUS ELITE WIFI7 ICE",
        chips: &["it8696", "it87952"],
    },
    // ── X670E AORUS family (IT8689E + IT87952E) ────────────────
    DualChipEntry {
        board_name: "X670E AORUS MASTER",
        chips: &["it8689", "it87952"],
    },
    DualChipEntry {
        board_name: "X670E AORUS PRO X",
        chips: &["it8689", "it87952"],
    },
    // DEC-144: plain X670 (non-E) ELITE AX — annotated "IT8689E +
    // IT87952E" in the frankcrawford/it87 DMI table (master, 2026-04).
    // The substring cannot false-match the X670E boards above ("X670 "
    // with a space is not a substring of "X670E ...").
    DualChipEntry {
        board_name: "X670 AORUS ELITE AX",
        chips: &["it8689", "it87952"],
    },
    // ── Z690 / Z790 AORUS family (IT8689E + IT87952E) ──────────
    DualChipEntry {
        board_name: "Z690 AORUS PRO",
        chips: &["it8689", "it87952"],
    },
    DualChipEntry {
        board_name: "Z790 AORUS ELITE AX",
        chips: &["it8689", "it87952"],
    },
    DualChipEntry {
        board_name: "Z790 AORUS MASTER",
        chips: &["it8689", "it87952"],
    },
    DualChipEntry {
        board_name: "Z790 AORUS XTREME",
        chips: &["it8689", "it87952"],
    },
    // ── X570 AORUS family (IT8688E + IT8792E/IT8795E) ──────────
    // The driver source comments group IT8792E and IT8795E together; on
    // the X570 generation the secondary chip is `it8792` in hwmon.
    DualChipEntry {
        board_name: "X570 AORUS MASTER",
        chips: &["it8688", "it8792"],
    },
    DualChipEntry {
        board_name: "X570 AORUS PRO",
        chips: &["it8688", "it8792"],
    },
    DualChipEntry {
        board_name: "X570 AORUS PRO WIFI",
        chips: &["it8688", "it8792"],
    },
    DualChipEntry {
        board_name: "X570 AORUS ULTRA",
        chips: &["it8688", "it8792"],
    },
    DualChipEntry {
        board_name: "TRX40 AORUS XTREME",
        chips: &["it8688", "it8792"],
    },
    // ── AM4 400-series AORUS boards (IT8686E + IT8792E) ────────
    // Same chip pairing as the X399 generation. Confirmed for X470 AORUS
    // ULTRA GAMING by the upstream lm-sensors config (`configs/Gigabyte/
    // X470-AORUS-ULTRA-GAMING.conf`). Other AM4 400-series AORUS boards
    // share the same SuperIO topology per vendor service manuals and the
    // frankcrawford/it87 driver's DMI table.
    DualChipEntry {
        board_name: "X470 AORUS ULTRA GAMING",
        chips: &["it8686", "it8792"],
    },
    DualChipEntry {
        board_name: "X470 AORUS GAMING 7 WIFI",
        chips: &["it8686", "it8792"],
    },
    DualChipEntry {
        board_name: "X470 AORUS GAMING 5 WIFI",
        chips: &["it8686", "it8792"],
    },
    // "B450 AORUS PRO" matches both the plain board and the WIFI variant via
    // substring match. The -CF variant is listed separately below for
    // legacy documentation continuity (same chips, identical behaviour).
    DualChipEntry {
        board_name: "B450 AORUS PRO",
        chips: &["it8686", "it8792"],
    },
    // ── Older dual-chip boards ──────────────────────────────────
    DualChipEntry {
        board_name: "X399 DESIGNARE EX-CF",
        chips: &["it8686", "it8792"],
    },
    DualChipEntry {
        board_name: "B450 AORUS PRO-CF",
        chips: &["it8686", "it8792"],
    },
    // ── DEC-106: AM4 500-series & AM5 800-series dual-chip AORUS ─
    // B550 VISION D — verified against upstream lm-sensors config
    // (`configs/Gigabyte/GA-B550-VISION-D.conf`): primary IT8688E at
    // 0x0a40, secondary IT8792E at 0x0a60.
    DualChipEntry {
        board_name: "B550 VISION D",
        chips: &["it8688", "it8792"],
    },
    // B850-AI-TOP — verified against frankcrawford/it87 issue #93:
    // primary IT8696E + secondary IT87952E. Same dual-chip topology as
    // the X870E AORUS MASTER above.
    DualChipEntry {
        board_name: "B850 AI TOP",
        chips: &["it8696", "it87952"],
    },
    // X870 AORUS STEALTH ICE: NOT enrolled above (yet). The earlier reason —
    // "secondary chip is an undriveable IT8883" — was wrong: per
    // frankcrawford/it87 #81/#70 the real pair is IT8696E + IT87952E, and
    // 0x8883 is only what the secondary reports when stuck in config mode
    // (clean read 0x8695), recovered with mmio=on. Enrolling it like the
    // other it8696+it87952 boards is a reasonable follow-up; kept out here to
    // avoid a behaviour change in the 2026-07 doc-correctness pass.
];

/// Look up the chip names a known Gigabyte dual-chip board is expected to
/// expose. Returns an empty Vec if the board is not in the table or
/// `board_name` is empty — i.e. callers can treat empty as "no info" and
/// the GUI will skip the warning UI.
pub fn expected_chips_for_board(board_vendor: &str, board_name: &str) -> Vec<String> {
    if board_name.is_empty() {
        return Vec::new();
    }
    // Cheap vendor sanity check — only Gigabyte boards are in the table at
    // present, so other vendors short-circuit. Empty vendor string still
    // matches (some firmwares omit the field).
    let vendor_lower = board_vendor.to_lowercase();
    if !vendor_lower.is_empty() && !vendor_lower.contains("gigabyte") {
        return Vec::new();
    }
    let board_upper = board_name.to_uppercase();
    for entry in GIGABYTE_DUAL_CHIP_BOARDS {
        if board_upper.contains(entry.board_name) {
            return entry.chips.iter().map(|s| (*s).to_string()).collect();
        }
    }
    Vec::new()
}

// ── Kernel-level chip detection (DEC-101) ──────────────────────────
//
// Best-effort signal of "what the kernel saw" before/independent of the
// hwmon binding step. When kernel logs are accessible (Arch default has
// `kernel.dmesg_restrict=0`), parsing dmesg for `it87:` lines surfaces
// the exact chip family the SuperIO scan returned. When logs are not
// readable (privileged-restricted distro, daemon running unprivileged
// without CAP_SYSLOG), we return an empty Vec and the GUI falls back
// to expected_chips alone.
//
// We do NOT shell out to `dmesg` or `journalctl` — both would add a
// runtime dependency and add another failure mode. Instead we read
// `/dev/kmsg` directly with O_NONBLOCK and parse a small ring of bytes.
// Each /dev/kmsg record is one line, so a partial read can only cut
// between records, not within one — making the parser robust without a
// full reader stack.

/// Parse chip names out of dmesg-style `it87:` lines.
///
/// Returns lowercased chip-name strings ("it8696", "it87952", …) for
/// any "Found IT8XXXX chip" line in the input. Lines that don't match
/// the pattern are skipped. Duplicates are de-duplicated (preserving
/// order of first appearance) so the GUI can compare against
/// `chips_detected` directly.
pub fn parse_kmsg_for_it87_chips(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        // Look for "it87:" or "it87 " (some kernels print with comma/space)
        // and a "Found IT" word boundary in the same line. Anchoring to
        // "it87" (driver name) keeps unrelated module messages out.
        let lower = line.to_lowercase();
        if !lower.contains("it87") {
            continue;
        }
        // Pull the IT8xxxx chip token.
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // Match "IT" or "it" followed by '8' and 3-5 digits.
            if i + 2 < bytes.len()
                && (bytes[i] == b'I' || bytes[i] == b'i')
                && (bytes[i + 1] == b'T' || bytes[i + 1] == b't')
                && bytes[i + 2] == b'8'
            {
                let mut j = i + 3;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let digits = j - (i + 3);
                if (3..=5).contains(&digits) {
                    let chip_lower = std::str::from_utf8(&bytes[i..j])
                        .unwrap_or_default()
                        .to_lowercase();
                    if !chip_lower.is_empty() && !out.contains(&chip_lower) {
                        out.push(chip_lower);
                    }
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Read kernel ring buffer and extract ITE chip names that the kernel
/// reported via `it87:` log lines.
///
/// Best-effort: returns an empty Vec when `/dev/kmsg` is not readable
/// (typical when `kernel.dmesg_restrict=1` and the daemon lacks
/// CAP_SYSLOG). Caller treats empty as "no info" and falls back to
/// `expected_chips`.
pub fn read_kernel_detected_chips() -> Vec<String> {
    read_kernel_detected_chips_from(Path::new("/dev/kmsg"))
}

/// Testable variant. Accepts a path so tests can supply a fixture with
/// canned kmsg records (each newline-terminated record exactly mirrors
/// the wire format, just without the leading priority/sequence prefix
/// — the parser is permissive about prefixes).
pub fn read_kernel_detected_chips_from(path: &Path) -> Vec<String> {
    // Open with O_NONBLOCK so we never block waiting for new records.
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            log::debug!("kernel chip detect: cannot open {}: {e}", path.display());
            return Vec::new();
        }
    };
    // Seek to the start of the ring buffer (SEEK_SET, offset 0) to read from
    // the oldest available record. On /dev/kmsg this is the canonical way to
    // start from the beginning without clearing the buffer; on a regular-file
    // test fixture it is an ordinary rewind. A seek failure is non-fatal —
    // we simply return no chips.
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        log::debug!("kernel chip detect: seek failed: {e}");
        return Vec::new();
    }

    // Cap at 1 MiB so a runaway log buffer cannot OOM us. The buffer is
    // read in non-blocking mode, so EAGAIN (no more records) is the loop
    // termination signal.
    const MAX_BYTES: usize = 1024 * 1024;
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 8192];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > MAX_BYTES {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {
                // EPIPE on /dev/kmsg means a record was overwritten while
                // we were reading — we've drained the available history.
                break;
            }
            Err(e) => {
                log::debug!("kernel chip detect: read failed: {e}");
                break;
            }
        }
    }

    let text = String::from_utf8_lossy(&buf);
    parse_kmsg_for_it87_chips(&text)
}

// ── CPU vendor identification (DEC-110) ────────────────────────────
//
// Reads the `vendor_id` line from /proc/cpuinfo and normalises the canonical
// CPUID strings to "Intel" / "AMD". Exposed in HardwareDiagnosticsResponse so
// the GUI can scope platform-specific quirks (Intel-only or AMD-only BIOS
// quirks on boards from vendors that ship both, e.g. MSI Z890 vs MSI X870E).
//
// We deliberately do not parse model_name, family, model, or stepping here —
// those are downstream of vendor for the GUI's purposes and would inflate the
// API surface for no current consumer. Add a CpuInfo struct later if more
// fields become useful.

/// Read CPU vendor from `/proc/cpuinfo` and normalise to `"Intel"`,
/// `"AMD"`, or `""` (empty when neither matches or the file cannot be read).
pub fn read_cpu_vendor() -> String {
    read_cpu_vendor_from(Path::new("/proc/cpuinfo"))
}

/// Testable variant with injectable path.
pub fn read_cpu_vendor_from(proc_cpuinfo: &Path) -> String {
    let content = match std::fs::read_to_string(proc_cpuinfo) {
        Ok(c) => c,
        Err(e) => {
            log::debug!(
                "read_cpu_vendor: cannot read {}: {e}",
                proc_cpuinfo.display()
            );
            return String::new();
        }
    };
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("vendor_id") {
            continue;
        }
        let Some(value) = trimmed.split(':').nth(1) else {
            continue;
        };
        return match value.trim() {
            "GenuineIntel" => "Intel".to_string(),
            "AuthenticAMD" | "HygonGenuine" => "AMD".to_string(),
            _ => String::new(),
        };
    }
    String::new()
}

/// Read the raw ppfeaturemask value as a hex string.
pub fn read_ppfeaturemask() -> Option<String> {
    read_ppfeaturemask_from(Path::new("/sys/module/amdgpu/parameters/ppfeaturemask"))
}

/// Testable variant.
pub fn read_ppfeaturemask_from(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    // Normalize to hex format
    if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        Some(trimmed.to_string())
    } else if let Ok(dec) = trimmed.parse::<u32>() {
        Some(format!("0x{dec:08x}"))
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn expected_driver_mapping() {
        assert_eq!(expected_driver("nct6798"), "nct6775");
        assert_eq!(expected_driver("nct6687"), "nct6687");
        assert_eq!(expected_driver("it8696"), "it87");
        assert_eq!(expected_driver("it8688"), "it87");
        assert_eq!(expected_driver("f71882fg"), "f71882fg");
        assert_eq!(expected_driver("unknown_chip"), "unknown");
    }

    #[test]
    fn is_known_superio_chip_accepts_families_and_rejects_sensor_chips() {
        // DEC-207: authoritative Super-I/O gate for the detector's bound path.
        for c in [
            "it8688",
            "nct6799",
            "nct6687",
            "nct6683",
            "f71882fg",
            "f71805f",
            "w83627ehf",
            "w83627hf",
            "smsc47m1",
            "smsc47b397",
            "dme1737",
            "sch5627",
            "sch5636",
            "pc87360",
            "pc87427",
        ] {
            assert!(
                is_known_superio_chip(c),
                "{c} should be a known Super-I/O chip"
            );
        }
        for c in [
            "amdgpu", "k10temp", "coretemp", "nvme", "spd5118", "zenpower", "",
        ] {
            assert!(
                !is_known_superio_chip(c),
                "{c} must NOT be a Super-I/O chip"
            );
        }
    }

    #[test]
    fn mainline_detection() {
        assert!(chip_driver_in_mainline("nct6798"));
        assert!(!chip_driver_in_mainline("nct6687"));
        // IT8688E is NOT in mainline
        assert!(!chip_driver_in_mainline("it8688"));
        // IT8628E IS in mainline
        assert!(chip_driver_in_mainline("it8628"));
    }

    #[test]
    fn it8622_is_mainline() {
        // DEC-144: it8622 is in the mainline it87 `enum chips` (verified
        // against torvalds/linux drivers/hwmon/it87.c v7.1 / 7.2-rc4) but was
        // missing from our list — the chips table falsely told IT8622E
        // owners they needed the DKMS build.
        assert!(chip_driver_in_mainline("it8622"));
    }

    #[test]
    fn it8689_stays_out_of_mainline_pending_common_7_1() {
        // DEC-144 intent lock (re-evaluated 2026-07): mainline 7.1 added
        // IT8689E fan *control* (commit 66b8eaf — six PWM, FEAT_FANCTL_ONOFF;
        // released 2026-06-14), not just sensors. We still report it as NOT
        // mainline because 7.1 is not yet the common kernel (the 6.12/6.18
        // LTS lines are) and some Gigabyte Rev 1 boards still have EC quirks
        // — flipping this to true would steer users off the DKMS build they
        // still need. Do not change without revisiting DEC-144.
        assert!(!chip_driver_in_mainline("it8689"));
    }

    #[test]
    fn it8665_not_in_mainline() {
        // DEC-144: IT8665E (X399 ROG Zenith Extreme era) is NOT in the
        // mainline it87 enum — it must keep reporting out-of-tree so the
        // GUI's chip guidance (DKMS + `mmio=off` regression note, issue
        // #106) lines up with the modules table.
        assert!(!chip_driver_in_mainline("it8665"));
    }

    #[test]
    fn detect_modules_from_proc() {
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6775 28672 0 - Live 0xffffffffc0a00000\n\
             k10temp 16384 0 - Live 0xffffffffc0980000\n\
             amdgpu 8388608 12 - Live 0xffffffffc1000000\n",
        )
        .unwrap();

        let modules = detect_loaded_modules_from(&modules_path);
        let nct = modules.iter().find(|m| m.name == "nct6775").unwrap();
        assert!(nct.loaded);
        assert!(nct.in_mainline);

        let it87 = modules.iter().find(|m| m.name == "it87").unwrap();
        assert!(!it87.loaded);

        let nct6687 = modules.iter().find(|m| m.name == "nct6687").unwrap();
        assert!(!nct6687.loaded);
        assert!(!nct6687.in_mainline);
    }

    #[test]
    fn detect_acpi_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let ioports_path = tmp.path().join("ioports");
        fs::write(
            &ioports_path,
            "0000-0cf7 : PCI Bus 0000:00\n\
             0290-0299 : ACPI OpRegion AMW0.SHWM\n\
             0cf8-0cff : PCI conf1\n",
        )
        .unwrap();

        let conflicts = detect_acpi_conflicts_from(&ioports_path);
        assert_eq!(conflicts.len(), 2); // Overlaps with both nct6775 and it87 ranges
        assert!(conflicts
            .iter()
            .any(|c| c.conflicts_with_driver == "nct6775"));
    }

    #[test]
    fn no_acpi_conflict_when_no_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let ioports_path = tmp.path().join("ioports");
        fs::write(
            &ioports_path,
            "0000-001f : ACPI something\n\
             0400-040f : ACPI PM_TMR\n",
        )
        .unwrap();

        let conflicts = detect_acpi_conflicts_from(&ioports_path);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn ppfeaturemask_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ppfeaturemask");
        fs::write(&path, "0xffffffff\n").unwrap();
        assert_eq!(read_ppfeaturemask_from(&path), Some("0xffffffff".into()));
    }

    #[test]
    fn ppfeaturemask_decimal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ppfeaturemask");
        fs::write(&path, "4294967295\n").unwrap();
        assert_eq!(read_ppfeaturemask_from(&path), Some("0xffffffff".into()));
    }

    #[test]
    fn ppfeaturemask_missing() {
        assert_eq!(read_ppfeaturemask_from(Path::new("/nonexistent")), None);
    }

    #[test]
    fn read_board_info_from_sysfs() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("board_vendor"),
            "Gigabyte Technology Co., Ltd.\n",
        )
        .unwrap();
        fs::write(tmp.path().join("board_name"), "X870E AORUS MASTER\n").unwrap();
        fs::write(tmp.path().join("bios_version"), "F13a\n").unwrap();

        let info = read_board_info_from(tmp.path());
        assert_eq!(info.vendor, "Gigabyte Technology Co., Ltd.");
        assert_eq!(info.name, "X870E AORUS MASTER");
        assert_eq!(info.bios_version, "F13a");
    }

    #[test]
    fn read_board_info_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let info = read_board_info_from(tmp.path());
        assert_eq!(info.vendor, "");
        assert_eq!(info.name, "");
        assert_eq!(info.bios_version, "");
    }

    // ── DEC-101: dual-chip board lookup ────────────────────────

    #[test]
    fn expected_chips_x870e_aorus_master() {
        // The reference case — the user's reported board, has IT8696E +
        // IT87952E. If this regresses the dual-chip warning never fires.
        let chips = expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X870E AORUS MASTER");
        assert!(chips.contains(&"it8696".to_string()));
        assert!(chips.contains(&"it87952".to_string()));
        assert_eq!(chips.len(), 2);
    }

    #[test]
    fn expected_chips_handles_empty_board_name() {
        // Older firmwares with empty DMI must not panic and must return
        // empty so the GUI hides the dual-chip warning.
        assert!(expected_chips_for_board("Gigabyte", "").is_empty());
        assert!(expected_chips_for_board("", "").is_empty());
    }

    #[test]
    fn expected_chips_skips_non_gigabyte_vendor() {
        // ASUS/MSI boards aren't in the dual-chip table; even if a board
        // name happened to match, vendor mismatch should suppress the
        // lookup so we don't false-positive other vendors.
        assert!(expected_chips_for_board("ASUSTeK COMPUTER INC.", "X870E AORUS MASTER").is_empty());
    }

    #[test]
    fn expected_chips_unknown_board_returns_empty() {
        // Single-chip boards or unknown boards return empty so the GUI
        // treats them as "no info" and hides the warning.
        assert!(
            expected_chips_for_board("Gigabyte Technology Co., Ltd.", "B650 AORUS ELITE AX")
                .is_empty()
        );
    }

    #[test]
    fn expected_chips_substring_match_tolerates_dmi_suffix() {
        // Some firmwares append "  Rev 1.0" or trailing whitespace —
        // substring match must still succeed.
        let chips = expected_chips_for_board(
            "Gigabyte Technology Co., Ltd.",
            "Z790 AORUS MASTER  Rev 1.0",
        );
        assert_eq!(chips, vec!["it8689".to_string(), "it87952".to_string()]);
    }

    #[test]
    fn expected_chips_x570_aorus_master_secondary_is_it8792() {
        // Older X570 generation pairs the primary IT8688E with the smaller
        // IT8792E (or 8795E, same hwmon name). Regression: do not confuse
        // X570 with X670/X870 chip pairings.
        let chips = expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X570 AORUS MASTER");
        assert_eq!(chips, vec!["it8688".to_string(), "it8792".to_string()]);
    }

    // ── DEC-105: AM4 400-series dual-chip coverage ───────────

    #[test]
    fn expected_chips_x470_aorus_ultra_gaming_pairs_it8686_with_it8792() {
        // Verified against upstream lm-sensors config (configs/Gigabyte/
        // X470-AORUS-ULTRA-GAMING.conf): primary it8686-isa-0a40 +
        // secondary it8792-isa-0a60. If the chip pairing regresses, the
        // dual-chip missing-PWM warning either misfires or never fires
        // on this board generation.
        let chips =
            expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X470 AORUS ULTRA GAMING");
        assert_eq!(chips, vec!["it8686".to_string(), "it8792".to_string()]);
    }

    #[test]
    fn expected_chips_b450_aorus_pro_uses_am4_400_chip_pair() {
        // The B450 generation uses IT8686E (not IT8688E — that's X570).
        // Matches the existing B450 AORUS PRO-CF entry's chip list.
        let chips = expected_chips_for_board("Gigabyte Technology Co., Ltd.", "B450 AORUS PRO");
        assert_eq!(chips, vec!["it8686".to_string(), "it8792".to_string()]);
    }

    #[test]
    fn expected_chips_b450_aorus_pro_wifi_resolves_via_substring() {
        // The WIFI variant DMI name "B450 AORUS PRO WIFI" must match the
        // generic "B450 AORUS PRO" substring entry — keeping that
        // consolidation deliberate so adding new WIFI/CF variants does
        // not require a new entry per SKU.
        let chips =
            expected_chips_for_board("Gigabyte Technology Co., Ltd.", "B450 AORUS PRO WIFI");
        assert_eq!(chips, vec!["it8686".to_string(), "it8792".to_string()]);
    }

    #[test]
    fn expected_chips_b550_vision_d_pairs_it8688_with_it8792() {
        // DEC-106: AM4 500-series Gigabyte AORUS topology — verified
        // against upstream lm-sensors GA-B550-VISION-D.conf
        // (primary it8688-isa-0a40 + secondary it8792-isa-0a60).
        let chips = expected_chips_for_board("Gigabyte Technology Co., Ltd.", "B550 VISION D");
        assert_eq!(chips, vec!["it8688".to_string(), "it8792".to_string()]);
    }

    #[test]
    fn expected_chips_b850_ai_top_pairs_it8696_with_it87952() {
        // DEC-106: AM5 800-series Gigabyte AI-TOP variant — confirmed by
        // frankcrawford/it87 issue #93. Same dual-chip topology as the
        // X870E AORUS MASTER family.
        let chips = expected_chips_for_board("Gigabyte Technology Co., Ltd.", "B850 AI TOP");
        assert_eq!(chips, vec!["it8696".to_string(), "it87952".to_string()]);
    }

    #[test]
    fn expected_chips_x870e_aorus_elite_pairs_it8696_with_it87952() {
        // DEC-144: owner-confirmed dual-chip per frankcrawford/it87
        // issue #89 (it8696-isa-0a40 + it87952-isa-0a60, control
        // working on current driver builds).
        let chips = expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X870E AORUS ELITE");
        assert_eq!(chips, vec!["it8696".to_string(), "it87952".to_string()]);
    }

    #[test]
    fn expected_chips_x870e_aorus_elite_x3d_resolves_via_substring() {
        // DEC-144: the 2026 X3D refresh must resolve through the plain
        // "X870E AORUS ELITE" substring entry — issue #89's report is
        // from this exact SKU.
        let chips =
            expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X870E AORUS ELITE X3D");
        assert_eq!(chips, vec!["it8696".to_string(), "it87952".to_string()]);
    }

    #[test]
    fn expected_chips_x670_aorus_elite_ax_pairs_it8689_with_it87952() {
        // DEC-144: plain X670 (non-E) ELITE AX — annotated IT8689E +
        // IT87952E in the frankcrawford/it87 DMI table. Must not be
        // shadowed by (or shadow) the X670E entries.
        let chips =
            expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X670 AORUS ELITE AX");
        assert_eq!(chips, vec!["it8689".to_string(), "it87952".to_string()]);
    }

    #[test]
    fn expected_chips_x670_elite_entry_does_not_claim_x670e_skus() {
        // DEC-144 regression guard: the "X670 AORUS ELITE AX" needle
        // (note: no E after X670) must not substring-match unverified
        // X670E SKUs — "X670E AORUS ELITE AX" is deliberately NOT in the
        // table and must return empty, not inherit the X670 pairing.
        assert!(
            expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X670E AORUS ELITE AX")
                .is_empty()
        );
    }

    #[test]
    fn expected_chips_x870_aorus_stealth_ice_not_in_table() {
        // X870 AORUS STEALTH ICE is an IT8696E + IT87952E board (NOT an
        // undriveable "IT8883" — corrected 2026-07 per frankcrawford/it87
        // #81/#70; 0x8883 is only a stuck-config-mode symptom, clean read
        // 0x8695). It is deliberately left out of the table for now, so
        // `expected_chips` stays empty; enrolling it is a tracked follow-up.
        let chips =
            expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X870 AORUS STEALTH ICE");
        assert!(chips.is_empty());
    }

    #[test]
    fn expected_chips_x470_aorus_gaming_7_wifi_pairs_match() {
        // X470 AORUS GAMING 7 WIFI uses the same it8686+it8792 topology
        // per the it87.c DMI table and vendor service manual.
        let chips =
            expected_chips_for_board("Gigabyte Technology Co., Ltd.", "X470 AORUS GAMING 7 WIFI");
        assert_eq!(chips, vec!["it8686".to_string(), "it8792".to_string()]);
    }

    // ── DEC-105: module-collision detector ───────────────────

    #[test]
    fn detect_module_collisions_flags_nct6687_with_nct6775() {
        // Canonical brick scenario (DEC-105): both modules loaded on a
        // single-chip MSI board with NCT6797D — chip ID 0xd450 overlap
        // can corrupt non-volatile fan state. CRITICAL banner expected.
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6775 28672 0 - Live 0xffffffffc0a00000\n\
             nct6687 32768 0 - Live 0xffffffffc0b00000\n\
             k10temp 16384 0 - Live 0xffffffffc0980000\n",
        )
        .unwrap();

        // Single nct6 chip detected → cannot prove legitimate dual-Nuvoton.
        let chips = [ChipBinding {
            chip_name: "nct6797",
            device_id: "isa-0290",
        }];
        let collisions = detect_module_collisions_from(&modules_path, &chips);
        assert_eq!(collisions.len(), 1);
        let entry = &collisions[0];
        assert_eq!(entry.module_a, "nct6687");
        assert_eq!(entry.module_b, "nct6775");
        assert_eq!(entry.severity, "critical");
        assert!(entry.summary.contains("0xd450"));
        assert!(entry.remediation.contains("blacklist"));
    }

    #[test]
    fn detect_module_collisions_silent_when_only_nct6687_loaded() {
        // Lone nct6687 is fine — many MSI users intentionally run only
        // the out-of-tree driver. No false-positive collision banner.
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6687 32768 0 - Live 0xffffffffc0b00000\n\
             k10temp 16384 0 - Live 0xffffffffc0980000\n",
        )
        .unwrap();

        assert!(detect_module_collisions_from(&modules_path, &[]).is_empty());
    }

    #[test]
    fn detect_module_collisions_silent_when_only_nct6775_loaded() {
        // Lone nct6775 — the kernel-only setup. No collision.
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6775 28672 0 - Live 0xffffffffc0a00000\n\
             nct6775_core 16384 1 nct6775, Live 0xffffffffc0a01000\n",
        )
        .unwrap();

        assert!(detect_module_collisions_from(&modules_path, &[]).is_empty());
    }

    #[test]
    fn detect_module_collisions_returns_empty_on_unreadable_path() {
        // Daemon must never panic if /proc/modules is missing.
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect_module_collisions_from(&tmp.path().join("nonexistent"), &[]).is_empty());
    }

    // ── DEC-106: dual-Nuvoton refinement ─────────────────────────

    #[test]
    fn detect_module_collisions_suppressed_on_legitimate_dual_nuvoton_board() {
        // ASRock X870E Taichi Lite: NCT6686 at one address handled by
        // nct6687d + NCT6799 at another address handled by nct6775. Both
        // modules legitimately coexist; suppress the CRITICAL banner.
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6775 28672 0 - Live 0xffffffffc0a00000\n\
             nct6687 32768 0 - Live 0xffffffffc0b00000\n",
        )
        .unwrap();

        let chips = [
            ChipBinding {
                chip_name: "nct6686",
                device_id: "isa-0a20",
            },
            ChipBinding {
                chip_name: "nct6799",
                device_id: "isa-0290",
            },
        ];
        assert!(
            detect_module_collisions_from(&modules_path, &chips).is_empty(),
            "Legitimate dual-Nuvoton board (two distinct nct6 chips at \
             distinct device_ids) must not surface the CRITICAL collision"
        );
    }

    #[test]
    fn detect_module_collisions_still_critical_for_single_chip_collision() {
        // Even when the bound chip is reported as nct6798 (i.e. nct6775
        // appears to have won the race), a single nct6 chip with both
        // modules loaded is still the brick-risk shape — emit CRITICAL.
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6775 28672 0 - Live 0xffffffffc0a00000\n\
             nct6687 32768 0 - Live 0xffffffffc0b00000\n",
        )
        .unwrap();
        let chips = [ChipBinding {
            chip_name: "nct6798",
            device_id: "isa-0290",
        }];
        let collisions = detect_module_collisions_from(&modules_path, &chips);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].severity, "critical");
    }

    #[test]
    fn detect_module_collisions_critical_when_chips_unknown() {
        // Defensive: when chips_detected is empty (early boot, or daemon
        // running with no hwmon controller), fall back to the original
        // behaviour and surface the CRITICAL banner. Suppressing on no
        // evidence would be the dangerous direction.
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6775 28672 0 - Live 0xffffffffc0a00000\n\
             nct6687 32768 0 - Live 0xffffffffc0b00000\n",
        )
        .unwrap();
        let collisions = detect_module_collisions_from(&modules_path, &[]);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].severity, "critical");
    }

    #[test]
    fn detect_module_collisions_non_nct6_chips_ignored_for_suppression() {
        // Same chip name but only one nct6 entry — a coincidental k10temp
        // or amdgpu hwmon node must not satisfy the "two distinct nct6
        // chips" suppression rule.
        let tmp = tempfile::tempdir().unwrap();
        let modules_path = tmp.path().join("modules");
        fs::write(
            &modules_path,
            "nct6775 28672 0 - Live 0xffffffffc0a00000\n\
             nct6687 32768 0 - Live 0xffffffffc0b00000\n",
        )
        .unwrap();
        let chips = [
            ChipBinding {
                chip_name: "nct6798",
                device_id: "isa-0290",
            },
            ChipBinding {
                chip_name: "k10temp",
                device_id: "pci-00c3",
            },
            ChipBinding {
                chip_name: "amdgpu",
                device_id: "pci-0300",
            },
        ];
        let collisions = detect_module_collisions_from(&modules_path, &chips);
        assert_eq!(
            collisions.len(),
            1,
            "Non-nct6 chips must not count toward the dual-Nuvoton \
             suppression — only one nct6 chip present, so still CRITICAL"
        );
    }

    #[test]
    fn asus_atk0110_recognised_in_known_modules() {
        // DEC-105: asus_atk0110 must appear in the modules table so
        // diagnostics can advise that this driver is sensor-read-only and
        // never the PWM-write path. Skipping it leaves ASUS users with a
        // mystery "I see sensors but no headers" diagnostic gap.
        let entry = KNOWN_MODULES.iter().find(|(n, _)| *n == "asus_atk0110");
        assert!(entry.is_some(), "asus_atk0110 must be in KNOWN_MODULES");
        assert!(
            entry.unwrap().1,
            "asus_atk0110 is in mainline — flag must be true"
        );
    }

    // ── DEC-101: kmsg parser ─────────────────────────────────

    #[test]
    fn parse_kmsg_extracts_it87_chip_names() {
        // Realistic kmsg-style line. The driver emits both "Found IT8696E"
        // and the chip name in title case; the parser must lowercase.
        let text = "\
            6,1234,5,-;it87: Found IT8696E chip at 0xa40 [MMIO at 0x00000000fe100000], revision 0\n\
            6,1235,5,-;it87: Found IT87952E chip at 0xa60, revision 0\n";
        let chips = parse_kmsg_for_it87_chips(text);
        assert!(chips.contains(&"it8696".to_string()));
        assert!(chips.contains(&"it87952".to_string()));
        assert_eq!(chips.len(), 2);
    }

    #[test]
    fn parse_kmsg_dedupes_repeated_lines() {
        // The same chip may be logged twice during reload — must not
        // appear twice in the output.
        let text = "\
            it87: Found IT8696E chip at 0xa40\n\
            it87: Found IT8696E chip at 0xa40 (re-init)\n";
        let chips = parse_kmsg_for_it87_chips(text);
        assert_eq!(chips, vec!["it8696".to_string()]);
    }

    #[test]
    fn parse_kmsg_skips_lines_without_it87_module_tag() {
        // A user-space `IT8696` mention in some other dmesg line (e.g. a
        // udev rule script logging) must NOT be picked up — the line must
        // mention "it87" too. This avoids false positives.
        let text = "udev: detected IT8696E reference in /etc/something\n";
        assert!(parse_kmsg_for_it87_chips(text).is_empty());
    }

    #[test]
    fn parse_kmsg_handles_empty_input() {
        assert!(parse_kmsg_for_it87_chips("").is_empty());
    }

    #[test]
    fn parse_kmsg_rejects_short_chip_codes() {
        // "IT8" alone or "IT82" is too short — only IT8 followed by
        // 3-5 digits is a real chip code.
        let text = "it87: nonsense IT8 partial match IT82\n";
        assert!(parse_kmsg_for_it87_chips(text).is_empty());
    }

    #[test]
    fn read_kernel_detected_chips_returns_empty_when_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Point at a path that definitely doesn't exist — open should
        // fail and we should get an empty Vec, not a panic.
        let result = read_kernel_detected_chips_from(&tmp.path().join("nonexistent_kmsg"));
        assert!(result.is_empty());
    }

    #[test]
    fn read_kernel_detected_chips_parses_fixture_file() {
        // Use a regular file as a kmsg fixture — the function reads
        // bytes generically, so this exercises the parser path even
        // though the seek behaviour differs from real /dev/kmsg.
        let tmp = tempfile::tempdir().unwrap();
        let kmsg = tmp.path().join("kmsg_fixture");
        std::fs::write(
            &kmsg,
            "it87 driver version foo\n\
             it87: Found IT8696E chip at 0xa40, revision 0\n\
             it87: Found IT87952E chip at 0xa60, revision 0\n",
        )
        .unwrap();
        let chips = read_kernel_detected_chips_from(&kmsg);
        assert!(chips.contains(&"it8696".to_string()));
        assert!(chips.contains(&"it87952".to_string()));
    }

    // ── DEC-101: it87 module mainline flag ───────────────────

    // ── DEC-110: Intel CPU vendor & intel_pch_thermal ────────

    #[test]
    fn read_cpu_vendor_genuineintel_maps_to_intel() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cpuinfo");
        fs::write(
            &path,
            "processor\t: 0\n\
             vendor_id\t: GenuineIntel\n\
             cpu family\t: 6\n\
             model\t\t: 183\n\
             model name\t: 13th Gen Intel(R) Core(TM) i7-13700K\n",
        )
        .unwrap();
        assert_eq!(read_cpu_vendor_from(&path), "Intel");
    }

    #[test]
    fn read_cpu_vendor_authenticamd_maps_to_amd() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cpuinfo");
        fs::write(
            &path,
            "processor\t: 0\n\
             vendor_id\t: AuthenticAMD\n\
             cpu family\t: 25\n\
             model\t\t: 116\n\
             model name\t: AMD Ryzen 9 7950X 16-Core Processor\n",
        )
        .unwrap();
        assert_eq!(read_cpu_vendor_from(&path), "AMD");
    }

    #[test]
    fn read_cpu_vendor_hygon_maps_to_amd() {
        // Hygon Dhyana is an AMD Zen 1 derivative — same /proc/cpuinfo
        // shape and same vendor-quirk surface for our purposes.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cpuinfo");
        fs::write(&path, "vendor_id\t: HygonGenuine\n").unwrap();
        assert_eq!(read_cpu_vendor_from(&path), "AMD");
    }

    #[test]
    fn read_cpu_vendor_unknown_returns_empty() {
        // KVM hypervisor strings or anything we don't explicitly map
        // must surface as empty so the GUI falls back to non-platform
        // matching. Suppressing unknown vendors avoids false-positive
        // quirk hits on bare-metal CPUs we haven't classified yet.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cpuinfo");
        fs::write(&path, "vendor_id\t: KVMKVMKVM\n").unwrap();
        assert_eq!(read_cpu_vendor_from(&path), "");
    }

    #[test]
    fn read_cpu_vendor_missing_vendor_id_returns_empty() {
        // Some virtualized environments omit vendor_id entirely — must
        // not panic, must return empty.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cpuinfo");
        fs::write(&path, "processor\t: 0\nmodel name\t: Generic CPU\n").unwrap();
        assert_eq!(read_cpu_vendor_from(&path), "");
    }

    #[test]
    fn read_cpu_vendor_unreadable_file_returns_empty() {
        // /proc/cpuinfo guaranteed present on Linux, but tests must
        // tolerate missing fixtures rather than panicking.
        assert_eq!(
            read_cpu_vendor_from(Path::new("/nonexistent_proc_cpuinfo")),
            ""
        );
    }

    #[test]
    fn read_cpu_vendor_picks_first_vendor_id_line() {
        // SMP systems repeat the block per logical CPU — picking the
        // first match is correct because all logical CPUs share the
        // same vendor_id on real hardware.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cpuinfo");
        fs::write(
            &path,
            "processor\t: 0\n\
             vendor_id\t: GenuineIntel\n\
             processor\t: 1\n\
             vendor_id\t: GenuineIntel\n",
        )
        .unwrap();
        assert_eq!(read_cpu_vendor_from(&path), "Intel");
    }

    #[test]
    fn intel_pch_thermal_in_known_modules() {
        // DEC-110: intel_pch_thermal must surface in the modules table
        // so Intel users see it honestly reported as a sensor-enrichment
        // driver (not a PWM control path). Mainline since the driver's
        // introduction.
        let entry = KNOWN_MODULES
            .iter()
            .find(|(n, _)| *n == "intel_pch_thermal");
        assert!(
            entry.is_some(),
            "intel_pch_thermal must be in KNOWN_MODULES (DEC-110)"
        );
        assert!(
            entry.unwrap().1,
            "intel_pch_thermal is in mainline — flag must be true"
        );
    }

    #[test]
    fn x86_pkg_temp_intentionally_excluded() {
        // The kernel x86_pkg_temp_thermal driver registers with
        // .no_hwmon = true and only appears as a thermal_zone. Listing
        // it in KNOWN_MODULES would falsely advertise a hwmon source we
        // cannot read. coretemp covers the same physical CPU package
        // temperature with a real hwmon device.
        let entry = KNOWN_MODULES.iter().find(|(n, _)| *n == "x86_pkg_temp");
        assert!(
            entry.is_none(),
            "x86_pkg_temp must NOT be in KNOWN_MODULES — it is thermal_zone-only \
             (no_hwmon=true). coretemp is the correct hwmon source for Intel CPU package."
        );
    }

    #[test]
    fn it87_module_marked_out_of_tree() {
        // The `it87` module name does exist in mainline, but every chip we
        // care about (IT8625E+) requires the out-of-tree DKMS build. The
        // KNOWN_MODULES table lies if it claims mainline=true here — the
        // GUI's modules-table column would falsely advertise upstream
        // support to users running the DKMS build.
        let entry = KNOWN_MODULES.iter().find(|(n, _)| *n == "it87");
        assert!(entry.is_some(), "it87 must remain in KNOWN_MODULES");
        assert!(
            !entry.unwrap().1,
            "it87 KNOWN_MODULES mainline flag must be false (DEC-101) — every \
             chip we care about needs frankcrawford/it87 DKMS"
        );
    }

    // ── DEC-202: full Super-I/O chip→module recognition set ──

    #[test]
    fn expected_driver_fintek_f71805_split_is_not_misrouted() {
        // Regression for the pre-DEC-202 bug: F71805F/F71806F/F71872F are the
        // SEPARATE `f71805f` driver, not `f71882fg`. The general `f718*` rule
        // used to swallow them, so the daemon recommended a module that would
        // not bind.
        assert_eq!(expected_driver("f71805f"), "f71805f");
        assert_eq!(expected_driver("f71872f"), "f71805f");
        assert_eq!(expected_driver("f71806f"), "f71805f");
        // …the genuine f71882fg-family chips still resolve there.
        assert_eq!(expected_driver("f71882fg"), "f71882fg");
        assert_eq!(expected_driver("f71869"), "f71882fg");
        assert_eq!(expected_driver("f8000"), "f71882fg");
    }

    #[test]
    fn expected_driver_winbond_ehf_vs_hf_families() {
        assert_eq!(expected_driver("w83627ehf"), "w83627ehf");
        assert_eq!(expected_driver("w83627dhg"), "w83627ehf");
        assert_eq!(expected_driver("w83627uhg"), "w83627ehf");
        assert_eq!(expected_driver("w83667hg"), "w83627ehf");
        assert_eq!(expected_driver("w83627hf"), "w83627hf");
        assert_eq!(expected_driver("w83697hf"), "w83627hf");
        assert_eq!(expected_driver("w83687thf"), "w83627hf");
    }

    #[test]
    fn expected_driver_smsc_family_and_sch_disambiguation() {
        assert_eq!(expected_driver("smsc47m1"), "smsc47m1");
        assert_eq!(expected_driver("smsc47m2"), "smsc47m1");
        assert_eq!(expected_driver("smsc47b397"), "smsc47b397");
        assert_eq!(expected_driver("dme1737"), "dme1737");
        assert_eq!(expected_driver("sch3114"), "dme1737");
        assert_eq!(expected_driver("sch5027"), "dme1737");
        assert_eq!(expected_driver("sch5127"), "dme1737");
        assert_eq!(expected_driver("sch5307"), "smsc47b397");
        assert_eq!(expected_driver("sch5317"), "smsc47b397");
        // The distinct SCH56xx drivers must NOT be captured by the SCH rules
        // above.
        assert_eq!(expected_driver("sch5627"), "sch5627");
        assert_eq!(expected_driver("sch5636"), "sch5636");
    }

    #[test]
    fn expected_driver_national_family() {
        assert_eq!(expected_driver("pc87360"), "pc87360");
        assert_eq!(expected_driver("pc87366"), "pc87360");
        assert_eq!(expected_driver("pc87427"), "pc87427");
    }

    #[test]
    fn expected_driver_nuvoton_668x_not_misrouted_to_nct6775() {
        // Regression: NCT6683/6686/6687 are a distinct family — they must NOT
        // fall into the nct6* → nct6775 catch-all. A chip reporting "nct6683"
        // was bound by the mainline (monitoring-only) nct6683 driver; a chip
        // reporting "nct6686"/"nct6687" was bound by the out-of-tree nct6687d.
        assert_eq!(expected_driver("nct6683"), "nct6683");
        assert_eq!(expected_driver("nct6686"), "nct6687");
        assert_eq!(expected_driver("nct6687"), "nct6687");
        // The genuine nct6775-family chips still route to nct6775.
        assert_eq!(expected_driver("nct6799"), "nct6775");
        assert_eq!(expected_driver("nct6798"), "nct6775");
        // nct6683 is mainline; the nct6687d-driven chips are out-of-tree.
        assert!(chip_driver_in_mainline("nct6683"));
        assert!(!chip_driver_in_mainline("nct6686"));
        assert!(!chip_driver_in_mainline("nct6687"));
    }

    #[test]
    fn new_superio_families_report_mainline() {
        // None of the Winbond/SMSC/National/Fintek-split families has an
        // out-of-tree fork situation, so chip_driver_in_mainline is true.
        for chip in [
            "w83627ehf",
            "w83627hf",
            "f71805f",
            "smsc47m1",
            "smsc47b397",
            "dme1737",
            "sch5627",
            "sch5636",
            "pc87360",
            "pc87427",
        ] {
            assert!(
                chip_driver_in_mainline(chip),
                "{chip} should report mainline=true"
            );
        }
    }

    #[test]
    fn new_superio_modules_present_in_known_modules() {
        for module in [
            "f71805f",
            "w83627ehf",
            "w83627hf",
            "smsc47m1",
            "smsc47b397",
            "dme1737",
            "sch5627",
            "sch5636",
            "pc87360",
            "pc87427",
        ] {
            let entry = KNOWN_MODULES.iter().find(|(n, _)| *n == module);
            assert!(
                entry.is_some(),
                "{module} must be in KNOWN_MODULES (DEC-202)"
            );
            assert!(entry.unwrap().1, "{module} is long-mainline → flag true");
        }
    }

    #[test]
    fn conflicting_loaded_module_flags_dec106_pair() {
        let loaded = vec!["nct6687".to_string(), "coretemp".to_string()];
        // Recommending nct6775 while nct6687 is loaded → DEC-106 collision.
        assert_eq!(
            conflicting_loaded_module("nct6775", &loaded),
            Some("nct6687")
        );
        // …and symmetrically.
        let loaded2 = vec!["nct6775".to_string()];
        assert_eq!(
            conflicting_loaded_module("nct6687", &loaded2),
            Some("nct6775")
        );
        // No counterpart loaded → no conflict.
        assert_eq!(
            conflicting_loaded_module("nct6775", &["k10temp".to_string()]),
            None
        );
        // Module not in any collision pair → no conflict.
        assert_eq!(conflicting_loaded_module("it87", &loaded), None);
    }
}
