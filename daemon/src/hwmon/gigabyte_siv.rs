//! Gigabyte SIV (System Information Vector) — firmware-reported header counts.
//!
//! `X87-d`. `expected_chips` answers "which chips should this board have?" from
//! a curated DMI table (`chip_db::GIGABYTE_DUAL_CHIP_BOARDS`), which is an
//! inference: it is right only for boards someone has added. On Gigabyte boards
//! the `it87` driver publishes the firmware's own descriptor at
//! `/sys/class/gigabyte/id/gigabyte_siv`, and its low 32 bits carry how many fan
//! headers, temperature channels and voltage rails the *board* declares. That is
//! a measurement, and it lets a client state the deficit as a fact instead of
//! inferring one from a hard-coded list. Note what the counterpart count means:
//! `hwmon.total_headers` on the same response is `pwmN`-capable headers only, so
//! the difference is "headers with no controllable PWM" — monitor-only
//! tachometers are a disjoint set on `GET /inventory/hwmon`.
//!
//! Read-only sysfs. **No port I/O**, no `CAP_SYS_RAWIO`, and nothing here is
//! affected by the Super-I/O port-probe gate — the driver has already done the
//! SMI read and cached the result; this module only reads the file it exports.
//!
//! The DMI table stays as the fallback, and stays authoritative for chip
//! *names*: the SIV counts headers, it does not say which chip carries them.

use std::path::Path;

/// Default sysfs path of the Gigabyte SIV descriptor, as exported by `it87`.
pub const GIGABYTE_SIV_PATH: &str = "/sys/class/gigabyte/id/gigabyte_siv";

/// Firmware-declared hardware-monitoring counts for a Gigabyte board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct GigabyteSiv {
    /// Platform id, bits 31:28. Non-zero for a valid descriptor — the driver
    /// treats zero as `-ENODEV`, and so does [`parse_siv`].
    pub platform: u8,
    /// Vendor "special" nibble, bits 27:24. Reported verbatim; the driver
    /// carries no public decode for it, so neither do we.
    pub special: u8,
    /// Fan/pump headers the board declares, bits 20:16.
    pub fan_count: u8,
    /// Temperature channels the board declares, bits 12:8.
    pub temp_count: u8,
    /// Voltage rails the board declares, bits 4:0.
    pub volt_count: u8,
}

/// Decode the low 32 bits of a SIV/MGID word.
///
/// Mirrors `gbw_parse_mgid()` in `it87.c` field for field, including both of its
/// rejections: a zero word, and a zero platform nibble. Those are not cosmetic —
/// the driver's own `gigabyte_platform_valid()` refuses to apply any Gigabyte
/// workaround without a parseable SIV, so a descriptor this rejects is one the
/// driver is not acting on either, and reporting its counts would be inventing
/// hardware.
pub fn parse_siv_word(mgid: u32) -> Option<GigabyteSiv> {
    if mgid == 0 {
        return None;
    }
    let platform = ((mgid >> 28) & 0xF) as u8;
    if platform == 0 {
        return None;
    }
    Some(GigabyteSiv {
        platform,
        special: ((mgid >> 24) & 0xF) as u8,
        fan_count: ((mgid >> 16) & 0x1F) as u8,
        temp_count: ((mgid >> 8) & 0x1F) as u8,
        volt_count: (mgid & 0x1F) as u8,
    })
}

/// Parse the sysfs attribute's text.
///
/// The driver formats it with `sprintf(buf, "%08X\n", ...)`, so the expected
/// shape is eight upper-case hex digits and a newline — but this accepts a
/// `0x` prefix and either case as well, because the format of a sysfs attribute
/// is not a stable ABI and a stricter parser would fail closed on a cosmetic
/// change. It stays strict about the one thing that matters: anything that is
/// not a hex word yields `None` rather than a partial number.
pub fn parse_siv(raw: &str) -> Option<GigabyteSiv> {
    let trimmed = raw.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    u32::from_str_radix(digits, 16)
        .ok()
        .and_then(parse_siv_word)
}

/// Read and decode the SIV descriptor at *path*.
///
/// `None` on every failure — absent file (the overwhelmingly common case: any
/// non-Gigabyte board, or `it87` not loaded), unreadable, or undecodable. The
/// caller reports "the firmware did not say", never a defaulted zero: a zero
/// `fan_count` would read as "this board has no fan headers", which is a
/// stronger and completely wrong claim.
pub fn read_siv(path: &Path) -> Option<GigabyteSiv> {
    std::fs::read_to_string(path)
        .ok()
        .as_deref()
        .and_then(parse_siv)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The descriptor measured on the X870E AORUS MASTER this row was opened
    /// from — decoded three ways there (vendor spec 8 headers, this word, and
    /// the 5 fans the reachable IT8696E supplies).
    const X870E_AORUS_MASTER: &str = "A008090A\n";

    #[test]
    fn decodes_the_measured_x870e_descriptor() {
        let siv = parse_siv(X870E_AORUS_MASTER).expect("valid descriptor");
        assert_eq!(siv.fan_count, 8, "board declares 8 fan/pump headers");
        assert_eq!(siv.temp_count, 9);
        assert_eq!(siv.volt_count, 10);
        assert_eq!(siv.platform, 0xA);
        assert_eq!(siv.special, 0x0);
    }

    #[test]
    fn fields_come_from_their_own_bits() {
        // Distinct values per field, so a transposed shift cannot pass. Every
        // count differs and none is a substring of the packed word by accident.
        let siv = parse_siv_word(0x4001_0203).expect("valid");
        assert_eq!(siv.platform, 0x4);
        assert_eq!(siv.special, 0x0);
        assert_eq!(siv.fan_count, 1);
        assert_eq!(siv.temp_count, 2);
        assert_eq!(siv.volt_count, 3);
    }

    #[test]
    fn counts_are_masked_to_five_bits() {
        // The neighbouring nibble must not bleed in: 0x1F is the driver's mask
        // and the fields are 8 bits apart, so bit 21/13/5 belongs to nobody.
        let siv = parse_siv_word(0xFFFF_FFFF).expect("valid");
        assert_eq!(siv.fan_count, 0x1F);
        assert_eq!(siv.temp_count, 0x1F);
        assert_eq!(siv.volt_count, 0x1F);
    }

    #[test]
    fn rejects_what_the_driver_rejects() {
        assert_eq!(parse_siv_word(0), None, "zero word is -ENODEV");
        assert_eq!(
            parse_siv_word(0x0008_090A),
            None,
            "zero platform nibble is -ENODEV, even with plausible counts"
        );
    }

    #[test]
    fn tolerates_formatting_but_not_garbage() {
        assert_eq!(parse_siv("a008090a"), parse_siv(X870E_AORUS_MASTER));
        assert_eq!(parse_siv("0xA008090A\n"), parse_siv(X870E_AORUS_MASTER));
        assert_eq!(parse_siv("   A008090A   "), parse_siv(X870E_AORUS_MASTER));
        assert_eq!(parse_siv(""), None);
        assert_eq!(parse_siv("not-a-word"), None);
        // A partial parse would be worse than none: this must not read as 0xA00.
        assert_eq!(parse_siv("A00zzzzz"), None);
    }

    #[test]
    fn missing_file_reads_as_no_answer() {
        assert_eq!(
            read_siv(Path::new("/nonexistent/gigabyte_siv")),
            None,
            "absence is the common case on every non-Gigabyte board"
        );
    }

    #[test]
    fn reads_and_decodes_a_real_file() {
        let dir = std::env::temp_dir().join(format!("ofc-siv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let path = dir.join("gigabyte_siv");
        std::fs::write(&path, X870E_AORUS_MASTER).expect("write");
        let siv = read_siv(&path).expect("decoded from disk");
        assert_eq!(siv.fan_count, 8);
        std::fs::remove_dir_all(&dir).ok();
    }
}
