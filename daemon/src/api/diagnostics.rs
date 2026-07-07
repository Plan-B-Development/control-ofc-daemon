//! Thin compatibility shim.
//!
//! The hardware chip↔driver detection primitives (module allowlist, chip→driver
//! mapping, ACPI-conflict scan, DMI board table, `/dev/kmsg` chip detection,
//! CPU-vendor / ppfeaturemask reads) now live in [`crate::hwmon::chip_db`] —
//! the single source of truth, shared with the `hwmon::superio` passive
//! Super-I/O detector (DEC-202). This module re-exports them so the
//! `GET /diagnostics/hardware` handler keeps resolving unchanged.
//!
//! Re-exports are **explicit** (not a `*` glob) on purpose: this namespace
//! historically held security-sensitive chip-access helpers, and a glob would
//! silently pull every future `chip_db` addition (e.g. a later-phase active
//! port probe or module loader) into `api::diagnostics`. Add an item here only
//! when a handler genuinely needs it.

pub use crate::hwmon::chip_db::{
    chip_driver_in_mainline, detect_acpi_conflicts, detect_loaded_modules,
    detect_module_collisions, expected_chips_for_board, expected_driver, read_board_info,
    read_cpu_vendor, read_kernel_detected_chips, read_ppfeaturemask, ChipBinding,
};
