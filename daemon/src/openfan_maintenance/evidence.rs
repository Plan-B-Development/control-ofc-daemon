//! What the returned board's own reports say about which firmware it runs
//! (DEC-481).
//!
//! Two signals, each compared three ways — before the update, after it, and
//! with what the selected file contains:
//! - the USB configuration descriptor sysfs cached at enumeration, which every
//!   build compiles in;
//! - the `KEY:VALUE` strings the firmware answers `>05`/`>06` with.
//!
//! Neither proves the exact build: two builds can share both (the 2026-09-13
//! and 2026-09-27 releases do). What they can show is "consistent with the
//! file", "the previous firmware is back", or that they cannot tell.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{BoardSnapshot, FirmwareClaim};

/// `verdict` tokens.
pub const CONSISTENT_WITH_FILE: &str = "consistent_with_file";
pub const PREVIOUS_FIRMWARE: &str = "previous_firmware";
pub const INCONCLUSIVE: &str = "inconclusive";

/// The comparison, as the run record reports it. Each `Option` is `None` when
/// a side of the comparison is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// The board's descriptor differs from before the update.
    pub descriptor_changed: Option<bool>,
    /// The board's descriptor equals the file's.
    pub descriptor_matches_file: Option<bool>,
    /// The board's information strings equal the file's, on every key the
    /// file carries.
    pub info_matches_file: Option<bool>,
    /// The board's information strings differ from before the update.
    pub info_changed: Option<bool>,
    /// One of [`CONSISTENT_WITH_FILE`], [`PREVIOUS_FIRMWARE`], [`INCONCLUSIVE`].
    pub verdict: String,
}

/// What one signal says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Says {
    /// Matches the file and not what ran before.
    New,
    /// Matches what ran before and not the file.
    Old,
    /// Matches neither.
    Neither,
    /// The file and the old firmware look the same, or a side is unknown.
    CannotTell,
}

fn says<T: PartialEq>(before: Option<&T>, after: Option<&T>, file: Option<&T>) -> Says {
    let (Some(after), Some(file)) = (after, file) else {
        return Says::CannotTell;
    };
    match (after == file, before) {
        (true, Some(before)) if before == file => Says::CannotTell,
        (true, _) => Says::New,
        (false, Some(before)) if before == after => Says::Old,
        (false, _) => Says::Neither,
    }
}

/// The file's keys looked up in a board's strings: `None` when the board did
/// not answer, or lacks a key the file has.
fn on_file_keys(
    board: Option<&BTreeMap<String, String>>,
    file: &BTreeMap<String, String>,
) -> Option<BTreeMap<String, String>> {
    let board = board?;
    file.keys()
        .map(|k| board.get(k).map(|v| (k.clone(), v.clone())))
        .collect()
}

/// Compare `before` and `after` with the file the GUI described.
pub fn compare(before: &BoardSnapshot, after: &BoardSnapshot, file: &FirmwareClaim) -> Evidence {
    let desc = |s: &BoardSnapshot| s.usb.as_ref().and_then(|u| u.config_descriptor_hex.clone());
    let (desc_before, desc_after) = (desc(before), desc(after));
    let desc_file = file.usb_config_descriptor_hex.clone();

    let info_file = file.info.clone().filter(|m| !m.is_empty());
    let (info_before, info_after) = match &info_file {
        Some(f) => (
            on_file_keys(before.info().as_ref(), f),
            on_file_keys(after.info().as_ref(), f),
        ),
        None => (None, None),
    };

    let signals = [
        says(
            desc_before.as_ref(),
            desc_after.as_ref(),
            desc_file.as_ref(),
        ),
        says(
            info_before.as_ref(),
            info_after.as_ref(),
            info_file.as_ref(),
        ),
    ];
    let any = |w: Says| signals.contains(&w);
    let verdict = if any(Says::Old) && !any(Says::New) && !any(Says::Neither) {
        PREVIOUS_FIRMWARE
    } else if any(Says::New) && !any(Says::Old) && !any(Says::Neither) {
        CONSISTENT_WITH_FILE
    } else {
        INCONCLUSIVE
    };

    let both = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) => Some(a != b),
        _ => None,
    };
    Evidence {
        descriptor_changed: both(&desc_before, &desc_after),
        descriptor_matches_file: match (&desc_after, &desc_file) {
            (Some(a), Some(f)) => Some(a == f),
            _ => None,
        },
        info_matches_file: match (&info_after, &info_file) {
            (Some(a), Some(f)) => Some(a == f),
            _ => None,
        },
        info_changed: match (before.info(), after.info()) {
            (Some(b), Some(a)) => Some(a != b),
            _ => None,
        },
        verdict: verdict.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::usb_identity::UsbDevice;

    fn board(desc: &str, hw_rev: &str, protocol: &str) -> BoardSnapshot {
        BoardSnapshot {
            usb: Some(UsbDevice {
                port: "8-8".into(),
                vendor_id: "2e8a".into(),
                product_id: "000a".into(),
                manufacturer: Some("Karanovic Research".into()),
                product: Some("OpenFan".into()),
                serial: Some("DE615CB14721492C".into()),
                bcd_device: Some("0100".into()),
                config_descriptor_hex: Some(desc.into()),
            }),
            hw_info: Some(BTreeMap::from([("HW_REV".into(), hw_rev.into())])),
            fw_info: Some(BTreeMap::from([
                ("FW_REV".into(), "01".into()),
                ("PROTOCOL_VERSION".into(), protocol.into()),
            ])),
        }
    }

    fn file(desc: &str, hw_rev: &str, protocol: &str) -> FirmwareClaim {
        FirmwareClaim {
            sha256: "0".repeat(64),
            size: 512,
            usb_config_descriptor_hex: Some(desc.into()),
            info: Some(BTreeMap::from([
                ("HW_REV".into(), hw_rev.into()),
                ("FW_REV".into(), "01".into()),
                ("PROTOCOL_VERSION".into(), protocol.into()),
            ])),
        }
    }

    // The 2023 build's descriptor (ACM bmCapabilities 0x02) and a 2026 one (0x06).
    const OLD: &str = "0902620004010080fa0824020200";
    const NEW: &str = "0902620004010080fa0824020600";

    #[test]
    fn the_2023_to_2026_update_reads_as_consistent_with_the_file() {
        let e = compare(
            &board(OLD, "01", "1"),
            &board(NEW, "03", "01"),
            &file(NEW, "03", "01"),
        );
        assert_eq!(e.verdict, CONSISTENT_WITH_FILE);
        assert_eq!(e.descriptor_changed, Some(true));
        assert_eq!(e.descriptor_matches_file, Some(true));
        assert_eq!(e.info_matches_file, Some(true));
        assert_eq!(e.info_changed, Some(true));
    }

    #[test]
    fn the_old_firmware_coming_back_reads_as_previous() {
        let e = compare(
            &board(OLD, "01", "1"),
            &board(OLD, "01", "1"),
            &file(NEW, "03", "01"),
        );
        assert_eq!(e.verdict, PREVIOUS_FIRMWARE);
        assert_eq!(e.descriptor_changed, Some(false));
        assert_eq!(e.descriptor_matches_file, Some(false));
    }

    #[test]
    fn reflashing_the_same_build_cannot_tell() {
        let e = compare(
            &board(OLD, "01", "1"),
            &board(OLD, "01", "1"),
            &file(OLD, "01", "1"),
        );
        assert_eq!(e.verdict, INCONCLUSIVE);
        assert_eq!(e.descriptor_matches_file, Some(true));
    }

    #[test]
    fn a_board_matching_neither_side_is_inconclusive_not_consistent() {
        let other = "0902620004010080fa0824020700";
        let e = compare(
            &board(OLD, "01", "1"),
            &board(other, "03", "01"),
            &file(NEW, "03", "01"),
        );
        assert_eq!(e.descriptor_matches_file, Some(false));
        assert_eq!(e.verdict, INCONCLUSIVE, "one signal says new, one neither");
    }

    #[test]
    fn a_board_that_does_not_answer_its_information_is_judged_on_the_descriptor() {
        let mut after = board(NEW, "03", "01");
        after.hw_info = None;
        after.fw_info = None;
        let e = compare(&board(OLD, "01", "1"), &after, &file(NEW, "03", "01"));
        assert_eq!(e.info_matches_file, None);
        assert_eq!(e.verdict, CONSISTENT_WITH_FILE);
    }

    #[test]
    fn a_file_with_nothing_to_compare_is_inconclusive() {
        let e = compare(
            &board(OLD, "01", "1"),
            &board(NEW, "03", "01"),
            &FirmwareClaim {
                sha256: "0".repeat(64),
                size: 512,
                usb_config_descriptor_hex: None,
                info: None,
            },
        );
        assert_eq!(e.descriptor_matches_file, None);
        assert_eq!(e.info_matches_file, None);
        assert_eq!(e.verdict, INCONCLUSIVE);
    }

    #[test]
    fn a_board_missing_a_key_the_file_has_does_not_match_on_strings() {
        let mut after = board(NEW, "03", "01");
        after.fw_info = Some(BTreeMap::from([("FW_REV".into(), "01".into())]));
        let e = compare(&board(OLD, "01", "1"), &after, &file(NEW, "03", "01"));
        assert_eq!(e.info_matches_file, None, "unknown, not a mismatch");
        assert_eq!(e.verdict, CONSISTENT_WITH_FILE);
    }
}
