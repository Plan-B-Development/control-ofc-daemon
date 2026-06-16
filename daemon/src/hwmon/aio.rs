//! Liquid-cooling (AIO / custom-loop) device recognition — the single source
//! of truth shared by coolant-sensor classification (`discovery.rs`), the
//! `is_aio` PWM-header flag (`pwm_discovery.rs`), and the dynamic `aio_hwmon`
//! capability (`api`).
//!
//! Scope is **hwmon-only** — the daemon never shells out to `liquidctl` or
//! opens USB-HID, so USB-only coolers are detected-but-uncontrollable and stay
//! out of scope. There is **no coolant safety rule**: classifying a sensor as
//! coolant has no thermal-override semantics (the CPU-only `safety.rs` rule is
//! unchanged).
//!
//! Per-driver pump writability is **not** second-guessed here. The kernel hwmon
//! ABI exposes a writable (`0644`) `pwmN` only for genuinely controllable
//! channels, so the existing file-permission `is_writable` check is already
//! truthful per-channel — verified against the mainline drivers:
//!   - `nzxt-kraken3`: `pwm1` (pump) `0644`; `pwm2` (fan) `0644` on Z-series/2023.
//!   - `corsair-cpro`, `nzxt-smart2`, `aquacomputer_d5next`: fan `pwm` `0644`.
//!   - `nzxt-kraken2`: **no** `pwm` attribute → monitor-only by absence.
//!
//! Chip strings below are the kernel hwmon **device** `name`s (NOT module
//! names) — verified in `drivers/hwmon/{nzxt-kraken3,nzxt-kraken2,
//! aquacomputer_d5next}.c`:
//!   - NZXT Kraken3 driver → `x53`, `z53`, `kraken2023`, `kraken2023elite`
//!   - NZXT Kraken2 driver → `kraken2`
//!   - Aquacomputer family → `d5next`, `highflownext`, `leakshield`, …

/// hwmon `name` strings for liquid coolers / pumps. Drives the `is_aio` header
/// flag and `aio_hwmon.present`. Fan/RGB controllers (`corsaircpro`,
/// `nzxtsmart2`, `octo`, `quadro`, `aquaero`, `farbwerk*`) are deliberately
/// excluded — they are not liquid coolers, so flagging them would be untruthful.
const LIQUID_COOLER_CHIPS: &[&str] = &[
    // NZXT Kraken (nzxt-kraken3 + nzxt-kraken2 drivers).
    "x53",
    "z53",
    "kraken2023",
    "kraken2023elite",
    "kraken2",
    // Aquacomputer liquid devices (aquacomputer_d5next driver) that report a
    // coolant temperature: pump (D5 Next), inline flow meter, leak shield.
    "d5next",
    "highflownext",
    "leakshield",
];

/// Chips whose temperature channel is **unambiguously** the coolant temp, so a
/// coolant sensor can be classified from the chip name alone. NZXT Kraken
/// devices expose a single `temp1` = coolant. Aquacomputer devices expose
/// **multiple labelled** temp channels (coolant, external probes, …), so they
/// are intentionally absent here — their coolant channel classifies via the
/// label hint (the driver labels it "Coolant temp"), avoiding false positives
/// on external probes.
const COOLANT_TEMP_CHIPS: &[&str] = &["x53", "z53", "kraken2023", "kraken2023elite", "kraken2"];

/// Coolant-temperature label keywords (case-insensitive substring match). A
/// lower-confidence fallback so a coolant channel on any chip — or an unlisted
/// cooler — still classifies when the vendor labels it (covers Aquacomputer
/// "Coolant temp" and ASUS-EC "Water In"/"Water Out").
const COOLANT_LABEL_HINTS: &[&str] = &["coolant", "water", "liquid"];

/// True when `chip_name` is a known hwmon liquid cooler (case-insensitive
/// exact match against [`LIQUID_COOLER_CHIPS`]).
pub fn is_liquid_cooler_chip(chip_name: &str) -> bool {
    let lower = chip_name.to_lowercase();
    LIQUID_COOLER_CHIPS.contains(&lower.as_str())
}

/// True when a temperature sensor should classify as coolant — either its chip
/// is a single-coolant-temp cooler (Kraken) or its label names a coolant.
pub fn is_coolant_sensor(chip_name: &str, label: &str) -> bool {
    let lower_chip = chip_name.to_lowercase();
    if COOLANT_TEMP_CHIPS.contains(&lower_chip.as_str()) {
        return true;
    }
    let lower_label = label.to_lowercase();
    COOLANT_LABEL_HINTS
        .iter()
        .any(|hint| lower_label.contains(hint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kraken_chips_are_liquid_coolers() {
        for chip in ["x53", "z53", "kraken2023", "kraken2023elite", "kraken2"] {
            assert!(is_liquid_cooler_chip(chip), "{chip} should be a cooler");
        }
    }

    #[test]
    fn aquacomputer_liquid_devices_are_coolers() {
        for chip in ["d5next", "highflownext", "leakshield"] {
            assert!(is_liquid_cooler_chip(chip), "{chip} should be a cooler");
        }
    }

    #[test]
    fn fan_controllers_are_not_coolers() {
        // Truthfulness: a fan/RGB hub is not a liquid cooler.
        for chip in [
            "corsaircpro",
            "nzxtsmart2",
            "octo",
            "quadro",
            "aquaero",
            "farbwerk",
            "it8696",
            "nct6798",
            "k10temp",
        ] {
            assert!(!is_liquid_cooler_chip(chip), "{chip} is not a cooler");
        }
    }

    #[test]
    fn cooler_match_is_case_insensitive() {
        assert!(is_liquid_cooler_chip("Z53"));
        assert!(is_liquid_cooler_chip("Kraken2023"));
    }

    #[test]
    fn kraken_temp_classifies_coolant_from_chip() {
        // Kraken exposes a single temp1 = coolant; chip name alone is enough.
        assert!(is_coolant_sensor("z53", "temp1"));
        assert!(is_coolant_sensor("kraken2", ""));
    }

    #[test]
    fn coolant_label_classifies_on_any_chip() {
        assert!(is_coolant_sensor("d5next", "Coolant temp"));
        assert!(is_coolant_sensor("asus_ec_sensors", "Water In"));
        assert!(is_coolant_sensor("nct6798", "Liquid"));
    }

    #[test]
    fn aquacomputer_external_probe_is_not_coolant() {
        // d5next is a cooler for is_aio, but an unlabelled/external probe must
        // NOT be force-classified as coolant — only its labelled coolant channel.
        assert!(!is_coolant_sensor("d5next", "External sensor 1"));
        assert!(!is_coolant_sensor("quadro", "Sensor 3"));
    }

    #[test]
    fn ordinary_sensors_are_not_coolant() {
        assert!(!is_coolant_sensor("k10temp", "Tctl"));
        assert!(!is_coolant_sensor("it8696", "temp1"));
        assert!(!is_coolant_sensor("amdgpu", "edge"));
    }
}
