//! Board voltage-rail catalogue: what each unlabelled ADC input is (`VOLT-b`, DEC-464).
//!
//! The kernel driver names only the inputs it measures internally (`3VSB`,
//! `Vbat`, `+3.3V`); every other `inN` is a pin whose board wiring the driver
//! does not know. lm-sensors names those per board in `/etc/sensors.d`, but
//! only libsensors reads that file — the kernel never does, so no label the
//! daemon reads ever changes because of it. This module carries the same facts
//! for the boards a public catalogue covers: frankcrawford/it87's Gigabyte
//! configs, keyed on what those configs key on.
//!
//! # The key is (CPU platform, chip, SIV)
//!
//! - **SIV** — the board's firmware System Information Vector, the word the
//!   `it87` driver exports at `/sys/class/gigabyte/id/gigabyte_siv`
//!   ([`super::gigabyte_siv::read_siv_word`]). Several boards share one SIV,
//!   and so does their wiring; the configs say so by listing them together.
//! - **chip** — the *canonical* name (DEC-442), so an it87 v2.0 rebuild that
//!   appends the SIV to the chip name changes nothing here.
//! - **platform** — measured, not assumed: 38 chip+SIV keys appear in both the
//!   AMD and the Intel file with different rail names (`CPU VCORE SOC` vs
//!   `CPU VAXG` on one input), because the rails are the CPU's. A lookup with
//!   no known CPU vendor finds nothing rather than guessing a platform.
//!
//! # What an entry means, and what it never overrides
//!
//! [`BoardRail::Named`] gives the rail's name and the divider multiplier that
//! turns the pin voltage into the rail voltage (`compute inN @ * k`).
//! [`BoardRail::Unmapped`] is an input the board's config does not map (`ignore
//! inN` — upstream: "channels not mapped by this SIV configuration"). That is
//! **not** "unconnected": the X299 configs ignore an input for one CPU family
//! that they label `DRAM CH(A/B)` for the other. It may be unused, unconnected,
//! or a rail nobody named; all that is known is that it is not a named rail.
//!
//! The X299 boards' maps depend on the CPU family, in two files upstream's
//! installer refuses to choose between; the table carries only the inputs both
//! files map identically, so the rest stay unnamed rather than guessed.
//!
//! **A channel the driver labelled is never touched** (DEC-464 Q2). Those are
//! the chip's internal inputs, which the driver has already scaled, so a
//! catalogue multiplier would scale them twice; and the configs `ignore` them on
//! a secondary chip because they duplicate the primary chip's.
//! [`super::voltages::apply_board_catalogue`] enforces this.
//!
//! Display-only, like every rail: nothing in the daemon reads one.

use super::voltage_catalogue_data::{AMD, INTEL};

/// What the catalogue says about one ADC input on one board.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BoardRail {
    /// The input is wired to the named rail through a divider; the rail
    /// voltage is the pin voltage times `multiplier` (1.0 where the config
    /// carries no `compute` line).
    Named {
        label: &'static str,
        multiplier: f64,
    },
    /// The board's config does not map this input — not a named rail, and not
    /// known to be unconnected either.
    Unmapped,
}

/// Constructor the generated table uses, so each entry stays one short line.
pub(super) const fn named(label: &'static str, multiplier: f64) -> BoardRail {
    BoardRail::Named { label, multiplier }
}

/// One config `chip` block: the (chip, SIV) keys it covers and its inputs.
#[derive(Debug)]
pub(super) struct Stanza {
    pub(super) chips: &'static [(&'static str, u32)],
    pub(super) rails: &'static [(u8, BoardRail)],
}

impl Stanza {
    fn rail(&self, channel: u8) -> Option<BoardRail> {
        self.rails
            .iter()
            .find(|(ch, _)| *ch == channel)
            .map(|(_, rail)| *rail)
    }
}

/// The table for a CPU vendor as `chip_db::read_cpu_vendor` spells it
/// (`"AMD"` / `"Intel"`); `None` for anything else, including the empty string
/// it returns when `/proc/cpuinfo` is unreadable or the vendor is unknown.
fn table_for(cpu_vendor: &str) -> Option<&'static [Stanza]> {
    match cpu_vendor {
        "AMD" => Some(AMD),
        "Intel" => Some(INTEL),
        _ => None,
    }
}

/// Look up one input. `None` when the board, chip or channel is not in the
/// catalogue — the caller then reports the channel exactly as before.
pub fn lookup(cpu_vendor: &str, chip: &str, siv: u32, channel: u8) -> Option<BoardRail> {
    table_for(cpu_vendor)?
        .iter()
        .find(|s| s.chips.iter().any(|&(c, w)| c == chip && w == siv))?
        .rail(channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The reference host: X870E AORUS MASTER, SIV `A008090A`, AMD.
    const HOST_SIV: u32 = 0xA008_090A;

    fn named_parts(rail: Option<BoardRail>) -> (&'static str, f64) {
        match rail {
            Some(BoardRail::Named { label, multiplier }) => (label, multiplier),
            other => panic!("expected a named rail, got {other:?}"),
        }
    }

    /// Pinned against the catalogue as read at the generating commit — and
    /// against this host's measured pins: 1.992 V × 6 = 11.95 V on `+12V`.
    #[test]
    fn the_reference_board_primary_chip_is_named_with_its_dividers() {
        assert_eq!(
            named_parts(lookup("AMD", "it8696", HOST_SIV, 0)),
            ("CPU Vcore", 1.0)
        );
        assert_eq!(
            named_parts(lookup("AMD", "it8696", HOST_SIV, 1)),
            ("+3.3V", 1.649)
        );
        assert_eq!(
            named_parts(lookup("AMD", "it8696", HOST_SIV, 2)),
            ("+12V", 6.0)
        );
        assert_eq!(
            named_parts(lookup("AMD", "it8696", HOST_SIV, 3)),
            ("+5V", 2.5)
        );
        assert_eq!(
            named_parts(lookup("AMD", "it8696", HOST_SIV, 6)),
            ("CPU VDDIO MEM", 1.0)
        );
        // in7–in9 are the driver's own internal inputs; the config is silent.
        assert_eq!(lookup("AMD", "it8696", HOST_SIV, 7), None);
    }

    #[test]
    fn the_reference_board_secondary_chip_has_unmapped_inputs() {
        assert_eq!(
            lookup("AMD", "it87952", HOST_SIV, 0),
            Some(BoardRail::Unmapped)
        );
        assert_eq!(
            named_parts(lookup("AMD", "it87952", HOST_SIV, 2)),
            ("PM VCC18", 1.0)
        );
        // Not mentioned by the config at all: nothing to say.
        assert_eq!(lookup("AMD", "it87952", HOST_SIV, 3), None);
    }

    /// The platform is part of the key: the same chip + SIV names input 4
    /// differently on the two platforms (gigabyte-it87-amd.conf:131 vs
    /// gigabyte-it87-intel.conf:114).
    #[test]
    fn the_cpu_platform_selects_between_two_maps_for_one_siv() {
        let amd = named_parts(lookup("AMD", "it8686", 0x1004_0607, 4));
        let intel = named_parts(lookup("Intel", "it8686", 0x1004_0607, 4));
        assert_eq!(amd.0, "CPU VCORE SOC");
        assert_eq!(intel.0, "CPU VAXG");
    }

    /// X299 (DEC-464): only the inputs both CPU-family files map identically.
    /// `in0` is "CPU Vcore" for Kaby Lake-X and "CPU VRIN" for Skylake-X, so it
    /// is left unnamed; `+12V` and its divider agree, so it is carried.
    #[test]
    fn x299_carries_only_the_inputs_both_cpu_families_agree_on() {
        assert_eq!(
            named_parts(lookup("Intel", "it8688", 0x5008_090A, 2)),
            ("+12V", 6.0)
        );
        assert_eq!(lookup("Intel", "it8688", 0x5008_090A, 0), None);
        // in6 is ignored for one family and DRAM CH(A/B) for the other — the
        // case that shows `ignore` is not "unconnected".
        assert_eq!(lookup("Intel", "it8688", 0x5008_090A, 6), None);
    }

    #[test]
    fn nothing_matches_without_a_known_platform_chip_or_siv() {
        assert_eq!(lookup("", "it8696", HOST_SIV, 2), None, "unknown vendor");
        assert_eq!(lookup("AMD", "nct6799", HOST_SIV, 2), None, "other chip");
        assert_eq!(lookup("AMD", "it8696", 0xA008_0909, 2), None, "other SIV");
        // The suffixed sysfs spelling is not the key; the canonical name is.
        assert_eq!(lookup("AMD", "it8696_a008090a", HOST_SIV, 2), None);
    }

    /// Whole-table invariants the generator also enforces — kept here so a
    /// hand edit of the generated file cannot slip one past the build.
    #[test]
    fn every_entry_is_well_formed_and_every_key_is_unique_per_platform() {
        for (name, table) in [("AMD", AMD), ("INTEL", INTEL)] {
            assert!(!table.is_empty(), "{name} table is empty");
            let mut keys = HashSet::new();
            for stanza in table {
                assert!(!stanza.rails.is_empty());
                for &(chip, siv) in stanza.chips {
                    assert!(
                        keys.insert((chip, siv)),
                        "{name}: ({chip}, {siv:08X}) is keyed twice — the first \
                         match would silently shadow the second"
                    );
                }
                let mut channels = HashSet::new();
                for &(ch, rail) in stanza.rails {
                    assert!(channels.insert(ch), "{name}: in{ch} listed twice");
                    if let BoardRail::Named { label, multiplier } = rail {
                        assert!(!label.trim().is_empty());
                        assert!(multiplier.is_finite() && multiplier >= 1.0);
                    }
                }
            }
        }
    }
}
