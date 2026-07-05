//! Fine-grained temperature-sensor classification + deterministic default-CPU
//! selection (Phase 2).
//!
//! This is an ADVISORY REFINEMENT of the coarse [`SensorKind`] that discovery
//! assigns: it never changes `kind`, and the daemon's thermal safety continues
//! to key off `SensorKind::CpuTemp`. [`classify_temp_sensor`] reuses
//! [`crate::hwmon::discovery::classify_chip`] for the coarse decision and only
//! refines *within* it (a `CpuTemp` becomes cpu_package / cpu_core / cpu_tctl /
//! cpu_tdie; an `MbTemp` becomes motherboard / vrm / chipset / unknown), so the
//! fine class can never contradict `kind`. Pure functions — no sysfs access.

use std::fmt;

use crate::hwmon::types::SensorKind;

/// Fine-grained temperature-sensor class. A refinement of [`SensorKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempClass {
    CpuPackage,
    CpuCore,
    CpuTctl,
    CpuTdie,
    MotherboardTemp,
    VrmTemp,
    ChipsetTemp,
    GpuTemp,
    DiskTemp,
    CoolantTemp,
    UnknownTemp,
}

impl fmt::Display for TempClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::CpuPackage => "cpu_package",
            Self::CpuCore => "cpu_core",
            Self::CpuTctl => "cpu_tctl",
            Self::CpuTdie => "cpu_tdie",
            Self::MotherboardTemp => "motherboard_temp",
            Self::VrmTemp => "vrm_temp",
            Self::ChipsetTemp => "chipset_temp",
            Self::GpuTemp => "gpu_temp",
            Self::DiskTemp => "disk_temp",
            Self::CoolantTemp => "coolant_temp",
            Self::UnknownTemp => "unknown_temp",
        };
        f.write_str(s)
    }
}

/// Classifier confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
    Unknown,
}

impl Confidence {
    /// Numeric score for deterministic ranking (higher = more confident).
    fn score(self) -> u8 {
        match self {
            Self::High => 3,
            Self::Medium => 2,
            Self::Low => 1,
            Self::Unknown => 0,
        }
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// The classification of one temperature sensor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempClassification {
    pub class: TempClass,
    pub confidence: Confidence,
    pub rationale: String,
}

impl TempClassification {
    fn new(class: TempClass, confidence: Confidence, rationale: impl Into<String>) -> Self {
        Self {
            class,
            confidence,
            rationale: rationale.into(),
        }
    }
}

/// Classify a temperature sensor into a fine-grained [`TempClass`] with a
/// confidence and a plain-English rationale.
///
/// Reuses the coarse [`classify_chip`](crate::hwmon::discovery::classify_chip)
/// decision and only refines *within* it, so the result can never contradict
/// the sensor's `kind`.
pub fn classify_temp_sensor(chip_name: &str, label: &str) -> TempClassification {
    let l = label.to_lowercase();
    match crate::hwmon::discovery::classify_chip(chip_name, label) {
        SensorKind::CpuTemp => refine_cpu(chip_name, &l),
        SensorKind::MbTemp => refine_mb(chip_name, &l),
        SensorKind::GpuTemp => TempClassification::new(
            TempClass::GpuTemp,
            Confidence::High,
            "discrete-GPU temperature",
        ),
        SensorKind::DiskTemp => TempClassification::new(
            TempClass::DiskTemp,
            Confidence::High,
            "storage-device temperature",
        ),
        SensorKind::CoolantTemp => TempClassification::new(
            TempClass::CoolantTemp,
            Confidence::High,
            "liquid-cooler coolant temperature",
        ),
    }
}

/// True for a motherboard Super-I/O monitoring chip: Nuvoton `nct6*` or the ITE
/// `it8*` family (it85xx/it86xx/it87xx/it88xx — the whole range the `it87`
/// kernel driver binds, not just literally-`it87`-prefixed names).
fn is_superio_chip(chip: &str) -> bool {
    chip.starts_with("nct6") || chip.starts_with("it8")
}

/// Refine a coarse `CpuTemp` into a specific CPU sub-class. Sub-class from the
/// label; confidence from the source authority (k10temp / coretemp / sbtsi are
/// authoritative CPU sensors; a Super-I/O chip reads the CPU via PECI/TSI at
/// medium confidence; anything else is a low-confidence label inference).
fn refine_cpu(chip: &str, l: &str) -> TempClassification {
    let (class, what) = if l.contains("tctl") {
        (TempClass::CpuTctl, "Tctl control temperature")
    } else if l.contains("tdie") {
        (TempClass::CpuTdie, "Tdie die temperature")
    } else if l.contains("tccd") {
        (TempClass::CpuCore, "Tccd core-complex-die temperature")
    } else if l.contains("core") {
        (TempClass::CpuCore, "per-core temperature")
    } else if l.contains("package") {
        (TempClass::CpuPackage, "package temperature")
    } else {
        (TempClass::CpuPackage, "CPU temperature")
    };
    let (confidence, src) = match chip {
        "k10temp" | "coretemp" | "sbtsi_temp" => (Confidence::High, "authoritative CPU sensor"),
        c if is_superio_chip(c) => (Confidence::Medium, "motherboard Super-I/O CPU reading"),
        _ => (
            Confidence::Low,
            "inferred from the label on an unrecognised chip",
        ),
    };
    TempClassification::new(class, confidence, format!("{chip} {what} — {src}"))
}

/// Refine a coarse `MbTemp` into VRM / chipset / generic-motherboard, or honest
/// `unknown` when the coarse kind was only the unrecognised-chip default (an
/// unknown chip with no classifying label) rather than a real motherboard chip.
fn refine_mb(chip: &str, l: &str) -> TempClassification {
    let known_mobo = is_superio_chip(chip)
        || chip == "asus_ec_sensors"
        || chip == "asus_wmi_sensors"
        || chip == "gigabyte_wmi";
    if l.contains("vrm") {
        TempClassification::new(
            TempClass::VrmTemp,
            if known_mobo {
                Confidence::Medium
            } else {
                Confidence::Low
            },
            "VRM temperature (by label)",
        )
    } else if l.contains("pch") || l.contains("chipset") {
        TempClassification::new(
            TempClass::ChipsetTemp,
            if known_mobo {
                Confidence::Medium
            } else {
                Confidence::Low
            },
            "chipset / PCH temperature (by label)",
        )
    } else if known_mobo {
        TempClassification::new(
            TempClass::MotherboardTemp,
            Confidence::Medium,
            "motherboard temperature",
        )
    } else {
        // Coarse defaulted to MbTemp only because the chip is unrecognised and
        // the label gave no hint — surface that honestly rather than a false
        // motherboard_temp / medium.
        TempClassification::new(
            TempClass::UnknownTemp,
            Confidence::Unknown,
            "unrecognised chip with no classifying label",
        )
    }
}

/// A deterministic default-CPU-sensor recommendation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultCpuRecommendation {
    pub sensor_id: String,
    pub confidence: Confidence,
    pub rationale: String,
}

/// Rank a CPU sub-class as a default-CPU candidate (lower = more representative
/// of "the CPU temperature"). Non-CPU classes are not candidates.
fn cpu_class_rank(class: TempClass) -> Option<u8> {
    match class {
        TempClass::CpuTctl => Some(0), // AMD control temp — the canonical CPU temp
        TempClass::CpuPackage => Some(1), // Intel package / generic CPU
        TempClass::CpuTdie => Some(2), // AMD die
        TempClass::CpuCore => Some(3), // a single core — least representative
        _ => None,
    }
}

/// True when a fine class is a CPU sub-class (i.e. a default-CPU candidate).
pub fn is_cpu_class(class: TempClass) -> bool {
    cpu_class_rank(class).is_some()
}

/// Pick the deterministic default CPU temperature sensor from a set of
/// classified sensors. Prefers the most representative CPU sub-class, then the
/// highest confidence, then the lexicographically-smallest id (stable tiebreak).
/// Returns `None` when no CPU-class sensor is present. Advisory only — never a
/// silent replacement of a user's stored choice (that is Phase-5 persistence).
pub fn select_default_cpu<'a>(
    classified: impl IntoIterator<Item = (&'a str, &'a TempClassification)>,
) -> Option<DefaultCpuRecommendation> {
    let mut candidates: Vec<(u8, u8, &str, &TempClassification)> = classified
        .into_iter()
        .filter_map(|(id, c)| {
            cpu_class_rank(c.class).map(|rank| (rank, c.confidence.score(), id, c))
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0) // rank ascending (lower = more representative)
            .then(b.1.cmp(&a.1)) // confidence descending
            .then(a.2.cmp(b.2)) // id ascending (stable tiebreak)
    });
    let (_, _, id, chosen) = candidates[0];
    let total = candidates.len();
    let rationale = if total == 1 {
        format!("{} — the only CPU temperature candidate", chosen.rationale)
    } else {
        format!(
            "{} — selected as the most representative of {total} CPU temperature candidates",
            chosen.rationale
        )
    };
    Some(DefaultCpuRecommendation {
        sensor_id: id.to_string(),
        confidence: chosen.confidence,
        rationale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cls(chip: &str, label: &str) -> TempClassification {
        classify_temp_sensor(chip, label)
    }

    #[test]
    fn classifies_amd_cpu_subclasses() {
        assert_eq!(cls("k10temp", "Tctl").class, TempClass::CpuTctl);
        assert_eq!(cls("k10temp", "Tctl").confidence, Confidence::High);
        assert_eq!(cls("k10temp", "Tdie").class, TempClass::CpuTdie);
        assert_eq!(cls("k10temp", "Tccd1").class, TempClass::CpuCore);
        // Unlabelled k10temp is still authoritatively CPU.
        let u = cls("k10temp", "");
        assert_eq!(u.class, TempClass::CpuPackage);
        assert_eq!(u.confidence, Confidence::High);
    }

    #[test]
    fn classifies_intel_cpu_subclasses() {
        let pkg = cls("coretemp", "Package id 0");
        assert_eq!(pkg.class, TempClass::CpuPackage);
        assert_eq!(pkg.confidence, Confidence::High);
        assert_eq!(cls("coretemp", "Core 3").class, TempClass::CpuCore);
    }

    #[test]
    fn sbtsi_is_high_confidence_cpu() {
        let c = cls("sbtsi_temp", "");
        assert_eq!(c.class, TempClass::CpuPackage);
        assert_eq!(c.confidence, Confidence::High);
    }

    #[test]
    fn superio_cpu_reading_is_medium() {
        // A Nuvoton with a CPU label is coarsely CpuTemp; refined to a CPU
        // sub-class at MEDIUM (motherboard-sourced, not authoritative).
        let c = cls("nct6798", "CPU");
        assert_eq!(c.class, TempClass::CpuPackage);
        assert_eq!(c.confidence, Confidence::Medium);
    }

    #[test]
    fn motherboard_vrm_and_chipset() {
        assert_eq!(cls("it8696", "VRM").class, TempClass::VrmTemp);
        assert_eq!(cls("nct6798", "PCH_CHIP").class, TempClass::ChipsetTemp);
        assert_eq!(cls("nct6798", "SYSTIN").class, TempClass::MotherboardTemp);
        assert_eq!(cls("it8696", "System").class, TempClass::MotherboardTemp);
    }

    #[test]
    fn gpu_disk_coolant_echo_coarse_kind() {
        assert_eq!(cls("amdgpu", "edge").class, TempClass::GpuTemp);
        assert_eq!(cls("nvme", "Composite").class, TempClass::DiskTemp);
        assert_eq!(cls("z53", "").class, TempClass::CoolantTemp); // NZXT Kraken coolant
    }

    #[test]
    fn unknown_chip_cpu_label_is_low_confidence_cpu() {
        // Coarse classify_chip → CpuTemp (label "cpu"); refined to a CPU class
        // but at LOW confidence because the chip is unrecognised.
        let c = cls("mystery_chip", "CPU Core Temp");
        assert!(matches!(
            c.class,
            TempClass::CpuCore | TempClass::CpuPackage
        ));
        assert_eq!(c.confidence, Confidence::Low);
    }

    #[test]
    fn genuinely_unknown_sensor_is_unknown_temp() {
        // Unrecognised chip + a label with no CPU/GPU/VRM/chipset hint → coarse
        // MbTemp default, refined honestly to unknown_temp rather than a false
        // motherboard_temp / medium.
        let c = cls("mystery_chip", "TEMP1");
        assert_eq!(c.class, TempClass::UnknownTemp);
        assert_eq!(c.confidence, Confidence::Unknown);
    }

    #[test]
    fn class_and_confidence_display_snake_lower() {
        assert_eq!(TempClass::CpuTctl.to_string(), "cpu_tctl");
        assert_eq!(TempClass::MotherboardTemp.to_string(), "motherboard_temp");
        assert_eq!(TempClass::UnknownTemp.to_string(), "unknown_temp");
        assert_eq!(Confidence::High.to_string(), "high");
        assert_eq!(Confidence::Unknown.to_string(), "unknown");
    }

    // ── default-CPU selection ──────────────────────────────────────────────

    fn tc(class: TempClass, confidence: Confidence) -> TempClassification {
        TempClassification::new(class, confidence, "test")
    }

    fn pick(items: &[(&str, TempClassification)]) -> Option<DefaultCpuRecommendation> {
        select_default_cpu(items.iter().map(|(id, c)| (*id, c)))
    }

    #[test]
    fn default_cpu_none_when_no_cpu_sensor() {
        let items = [
            ("mb", tc(TempClass::MotherboardTemp, Confidence::Medium)),
            ("gpu", tc(TempClass::GpuTemp, Confidence::High)),
        ];
        assert!(pick(&items).is_none());
    }

    #[test]
    fn default_cpu_prefers_tctl_over_core_regardless_of_id() {
        // tctl (rank 0) beats core (rank 3) even though its id sorts later.
        let items = [
            ("a_tccd", tc(TempClass::CpuCore, Confidence::High)),
            ("z_tctl", tc(TempClass::CpuTctl, Confidence::High)),
        ];
        let r = pick(&items).unwrap();
        assert_eq!(r.sensor_id, "z_tctl");
        assert_eq!(r.confidence, Confidence::High);
    }

    #[test]
    fn default_cpu_prefers_package_over_cores_on_intel() {
        // package (rank 1) beats core (rank 3) even though its id sorts later.
        let items = [
            ("core0", tc(TempClass::CpuCore, Confidence::High)),
            ("core1", tc(TempClass::CpuCore, Confidence::High)),
            ("pkg", tc(TempClass::CpuPackage, Confidence::High)),
        ];
        assert_eq!(pick(&items).unwrap().sensor_id, "pkg");
    }

    #[test]
    fn default_cpu_confidence_tiebreak_prefers_authoritative() {
        // Same sub-class, different confidence: High (coretemp) wins over Medium
        // (Super-I/O) regardless of id order.
        let items = [
            ("a_superio", tc(TempClass::CpuPackage, Confidence::Medium)),
            ("z_coretemp", tc(TempClass::CpuPackage, Confidence::High)),
        ];
        let r = pick(&items).unwrap();
        assert_eq!(r.sensor_id, "z_coretemp");
        assert_eq!(r.confidence, Confidence::High);
    }

    #[test]
    fn default_cpu_is_stable_by_id_on_full_tie() {
        // Identical rank + confidence → deterministic lexicographic id tiebreak.
        let items = [
            ("m_tctl", tc(TempClass::CpuTctl, Confidence::High)),
            ("a_tctl", tc(TempClass::CpuTctl, Confidence::High)),
            ("z_tctl", tc(TempClass::CpuTctl, Confidence::High)),
        ];
        assert_eq!(pick(&items).unwrap().sensor_id, "a_tctl");
    }

    #[test]
    fn default_cpu_rationale_notes_candidate_count() {
        let one = [("t", tc(TempClass::CpuTctl, Confidence::High))];
        assert!(pick(&one).unwrap().rationale.contains("the only CPU"));
        let many = [
            ("a", tc(TempClass::CpuTctl, Confidence::High)),
            ("b", tc(TempClass::CpuCore, Confidence::High)),
        ];
        assert!(pick(&many)
            .unwrap()
            .rationale
            .contains("most representative"));
    }
}
