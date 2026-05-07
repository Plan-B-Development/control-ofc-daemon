//! Hardware diagnostics: kernel module detection and ACPI conflict scanning.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::responses::{AcpiConflictInfo, KernelModuleInfo};

/// Known hwmon driver modules and whether they're in the mainline kernel.
///
/// Note on `it87`: the *module name* exists in the mainline tree, but every
/// AM5/Z790-class chip we actually care about (IT8625E/IT8686E/IT8688E/
/// IT8689E/IT8696E/IT87952E) requires the out-of-tree frankcrawford/it87
/// fork. Marking the module as `false` keeps the modules table honest for
/// users running the DKMS build — the chip-level mainline column
/// (`chip_driver_in_mainline`) still reports per-chip accuracy for the
/// few legacy IT87xx chips that genuinely are upstream.
const KNOWN_MODULES: &[(&str, bool)] = &[
    ("nct6775", true),
    ("nct6775_core", true),
    ("nct6775_platform", true),
    ("nct6683", true),
    ("nct6687", false),
    ("it87", false),
    ("f71882fg", true),
    ("asus_ec_sensors", true),
    ("asus_wmi_sensors", true),
    ("asus_wmi_ec_sensors", true),
    ("sch5627", true),
    ("sch5636", true),
    ("k10temp", true),
    ("coretemp", true),
    ("amdgpu", true),
];

/// Map chip_name prefix → expected kernel driver module name.
fn expected_driver_for_chip(chip_name: &str) -> &'static str {
    let lower = chip_name.to_lowercase();
    if lower.starts_with("nct6687") {
        "nct6687"
    } else if lower.starts_with("nct6") || lower.starts_with("nct5") {
        "nct6775"
    } else if lower.starts_with("it8") {
        "it87"
    } else if lower.starts_with("f718") || lower.starts_with("f8000") || lower.starts_with("f818") {
        "f71882fg"
    } else if lower.starts_with("sch5627") {
        "sch5627"
    } else if lower.starts_with("sch5636") {
        "sch5636"
    } else {
        "unknown"
    }
}

/// Whether a chip's driver is in the mainline kernel.
pub fn chip_driver_in_mainline(chip_name: &str) -> bool {
    let driver = expected_driver_for_chip(chip_name);
    // ITE chips IT8625E+ require out-of-tree frankcrawford/it87
    if driver == "it87" {
        let lower = chip_name.to_lowercase();
        let mainline_chips = [
            "it8603", "it8620", "it8623", "it8628", "it8705", "it8712", "it8716", "it8718",
            "it8720", "it8721", "it8726", "it8728", "it8732", "it8758", "it8771", "it8772",
            "it8781", "it8782", "it8783", "it8786", "it8790", "it8792", "it8795", "it87952",
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

use super::responses::BoardInfo;

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
    DualChipEntry {
        board_name: "X870 AORUS ELITE WIFI7",
        chips: &["it8696", "it87952"],
    },
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
    // ── Older dual-chip boards ──────────────────────────────────
    DualChipEntry {
        board_name: "X399 DESIGNARE EX-CF",
        chips: &["it8686", "it8792"],
    },
    DualChipEntry {
        board_name: "B450 AORUS PRO-CF",
        chips: &["it8686", "it8792"],
    },
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
    // SEEK_DATA (constant 3) on /dev/kmsg seeks to the first record after
    // the most recent dmesg --clear. On regular files SEEK_DATA also seeks
    // to the next data block, which is usually the start — both behaviours
    // are acceptable for our purposes. Fall back to SeekFrom::Start(0) on
    // EINVAL.
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
    fn mainline_detection() {
        assert!(chip_driver_in_mainline("nct6798"));
        assert!(!chip_driver_in_mainline("nct6687"));
        // IT8688E is NOT in mainline
        assert!(!chip_driver_in_mainline("it8688"));
        // IT8628E IS in mainline
        assert!(chip_driver_in_mainline("it8628"));
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
}
