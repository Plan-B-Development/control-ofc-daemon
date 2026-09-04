//! Per-channel PWM header **roles** — what a fan header actually drives
//! (AIO-MB Phase 1).
//!
//! Distinct from [`crate::hwmon::aio`]'s `is_aio`, and deliberately so. `is_aio`
//! is **chip-level**: "this hwmon device is itself a liquid cooler" (an NZXT
//! Kraken, an Aquacomputer D5 Next). That is the right question for a cooler
//! plugged in over USB, and it is preserved unchanged.
//!
//! It is the wrong question for the far commoner case this module exists for:
//!
//! ```text
//! AIO pump → motherboard AIO_PUMP header → Nuvoton/ITE Super-I/O → hwmon
//! ```
//!
//! Here the hwmon device is a Super-I/O controller, not an AIO. `is_aio` is
//! (correctly) false, yet the *channel* is unambiguously a pump. A role is
//! per-channel, so it can say that.
//!
//! # Roles never lower a floor
//!
//! Every role this module can *infer* is already inside the 30% pump/CPU floor
//! set that [`crate::profile::member_needs_hard_floor`] computes from labels and
//! chip names:
//!
//! - `Pump` by label requires a `"pump"` substring — already a
//!   `CPU_PUMP_LABEL_HINTS` hit.
//! - `Pump` by chip mapping requires `is_liquid_cooler_chip` — already the
//!   `member_is_pump_or_cpu` cooler branch.
//!
//! So inference changes **no** floor anywhere, which is why the DEC-162
//! `role_classification.json` oracle passes unchanged. Only a *user assignment*
//! (`RoleSource::UserAssigned`) can raise a floor, and it is a union term — it
//! can add a floor, never remove one. Pinned by
//! `inferred_pump_roles_are_a_subset_of_the_existing_floor_set`.
//!
//! # Ambiguity is not resolved by guessing
//!
//! `CPU_OPT` is the header an AIO pump most often lands on, and it is *also*
//! where a second CPU-radiator fan lands. There is no evidence in the label to
//! tell those apart, so it classifies [`HeaderRole::Unknown`] and waits for the
//! user. Guessing `Pump` would floor a radiator fan at 30%; guessing
//! `RadiatorFan` would let identify stop a pump. Neither is acceptable, and the
//! AIO-MB brief is explicit that weak evidence must not be resolved by
//! inference.

use serde::{Deserialize, Serialize};

/// What a PWM header drives.
///
/// Serialises snake_case (`"cpu_fan"`, `"radiator_fan"`). Clients **must**
/// tolerate a token they do not recognise rather than dropping the header —
/// the same rule 273-i established for `skipped_controls[].reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HeaderRole {
    /// No evidence, or evidence too weak to act on (`CPU_OPT`, a synthesised
    /// `pwmN`). Never inferred into something stronger.
    #[default]
    Unknown,
    /// A CPU air-cooler fan. Carries the same 30% hard floor as a pump (it is
    /// in the existing pump/CPU bucket) but is **not** pump-safe for identify —
    /// stopping a CPU fan briefly is safe and is how you find it.
    CpuFan,
    /// A liquid-cooler pump. The only role that changes identify and verify
    /// behaviour: it must never be commanded to 0, or below the pump floor.
    Pump,
    /// A radiator fan on a liquid-cooler device. Inferred only for the
    /// non-pump channels of a known cooler chip; on a motherboard header there
    /// is no evidence that distinguishes it from a chassis fan, so it is
    /// user-assignable there and never guessed.
    RadiatorFan,
    /// An ordinary case fan.
    ChassisFan,
}

impl HeaderRole {
    /// The wire token, and the value accepted by `POST /config/header-role`.
    pub fn as_str(self) -> &'static str {
        match self {
            HeaderRole::Unknown => "unknown",
            HeaderRole::CpuFan => "cpu_fan",
            HeaderRole::Pump => "pump",
            HeaderRole::RadiatorFan => "radiator_fan",
            HeaderRole::ChassisFan => "chassis_fan",
        }
    }

    /// Parse a wire token. `None` for anything unrecognised — the caller turns
    /// that into a `400 validation_error` rather than silently defaulting,
    /// because silently defaulting a misspelled `"pmup"` to `Unknown` would
    /// drop a pump's protection while reporting success.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "unknown" => Some(HeaderRole::Unknown),
            "cpu_fan" => Some(HeaderRole::CpuFan),
            "pump" => Some(HeaderRole::Pump),
            "radiator_fan" => Some(HeaderRole::RadiatorFan),
            "chassis_fan" => Some(HeaderRole::ChassisFan),
            _ => None,
        }
    }

    /// Whether this role must never be stopped or driven below the pump floor.
    ///
    /// [SAFETY] The single predicate behind pump-safe identify (DEC-311) and the
    /// role-aware verify duty (`AIO1-a`). Deliberately `Pump` only: a `CpuFan`
    /// is safe to stop, and a `RadiatorFan` is a fan.
    pub fn is_pump(self) -> bool {
        matches!(self, HeaderRole::Pump)
    }
}

/// How a [`HeaderRole`] was established.
///
/// Reported on the wire so a client can distinguish a confident classification
/// from a guess worth asking the user about. Nothing in the daemon branches on
/// it — the daemon acts on the resolved [`HeaderRole`] alone — so it is
/// deliberately NOT ordered: an ordering nothing compares would be a claim the
/// code does not back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// No evidence — the role is `Unknown`.
    #[default]
    None,
    /// Inferred from the header's own `pwmN_label` / `fanN_label`.
    Label,
    /// Inferred from the hwmon chip being a known liquid cooler, plus the
    /// channel index.
    ChipMapping,
    /// Set explicitly by the user via `POST /config/header-role`. Outranks
    /// every inference.
    UserAssigned,
}

impl RoleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleSource::None => "none",
            RoleSource::Label => "label",
            RoleSource::ChipMapping => "chip_mapping",
            RoleSource::UserAssigned => "user_assigned",
        }
    }
}

/// Label substrings that mark a **pump**, checked before every other label rule.
///
/// Substring rather than token match, matching the existing
/// `profile::CPU_PUMP_LABEL_HINTS` convention — and in the safe direction: a
/// false positive floors a fan at 30% and refuses to stop it during identify,
/// while a false negative lets identify stop a pump. Covers `AIO_PUMP`, `PUMP`,
/// `W_PUMP`, `PUMP_FAN`, `PUMP_TACH` and vendor spellings like `SYS_FAN5_PUMP`.
const PUMP_LABEL_HINTS: &[&str] = &["pump"];

/// Labels that mark a CPU fan header. Matched on the normalised label so
/// `CPU_FAN`, `CPU_FAN1` and `CPUFAN` all hit — and, critically, `CPU_OPT`
/// does **not**. `CPU_OPT` is the ambiguous header the brief forbids guessing.
const CPU_FAN_LABEL_PREFIXES: &[&str] = &["cpu_fan", "cpufan"];

/// Labels that mark an ordinary case fan.
const CHASSIS_LABEL_HINTS: &[&str] = &["cha_fan", "chafan", "sys_fan", "sysfan", "chassis"];

/// Classify a PWM header from what discovery can see: chip name, channel index
/// and label. Pure — no sysfs, no hardware, no user state.
///
/// User assignment is **not** applied here: it is not known at discovery time
/// and lives in `runtime.toml`. Callers overlay it with [`resolve_role`], which
/// is the function every consumer should actually use.
///
/// Precedence is **label before chip mapping**, and that ordering is
/// load-bearing rather than arbitrary. A label is direct per-channel evidence; a
/// chip mapping is a positional assumption ("channel 1 of a cooler is the
/// pump"). Where they disagree the label is both more specific and safer: a
/// cooler whose `pwm2` is labelled `PUMP` (a dual-pump loop) classifies `Pump`
/// under label-first and `RadiatorFan` under mapping-first — and only one of
/// those refuses to stop it.
pub fn classify_header_role(
    chip_name: &str,
    pwm_index: u8,
    label: &str,
) -> (HeaderRole, RoleSource) {
    // A synthesised `pwmN` restates the channel index and carries no evidence
    // (DEC-229's rule, applied to roles). Skipping it here is what lets the chip
    // mapping below run on a cooler that publishes no label files.
    let has_real_label = !label.is_empty() && label != format!("pwm{pwm_index}");

    if has_real_label {
        let lower = label.to_lowercase();
        // Pump first: `SYS_FAN5_PUMP` contains a chassis hint too, and the pump
        // reading is the one that must win.
        if PUMP_LABEL_HINTS.iter().any(|h| lower.contains(h)) {
            return (HeaderRole::Pump, RoleSource::Label);
        }
        let normalised = lower.replace(['-', ' ', '.'], "_");
        if CPU_FAN_LABEL_PREFIXES
            .iter()
            .any(|p| normalised.starts_with(p))
        {
            return (HeaderRole::CpuFan, RoleSource::Label);
        }
        if CHASSIS_LABEL_HINTS.iter().any(|h| normalised.contains(h)) {
            return (HeaderRole::ChassisFan, RoleSource::Label);
        }
        // A real label we do not recognise — including `CPU_OPT`. Fall through
        // to the chip mapping; on a motherboard that yields `Unknown`.
    }

    // Chip mapping: on a known liquid cooler, channel 1 is the pump and the
    // rest are radiator fans. Verified against the mainline drivers in
    // `aio.rs`'s header comment (`nzxt-kraken3` pwm1=pump/pwm2=fan).
    if crate::hwmon::aio::is_liquid_cooler_chip(chip_name) {
        return if pwm_index == 1 {
            (HeaderRole::Pump, RoleSource::ChipMapping)
        } else {
            (HeaderRole::RadiatorFan, RoleSource::ChipMapping)
        };
    }

    (HeaderRole::Unknown, RoleSource::None)
}

/// The role actually in force for a header: the user's assignment if there is
/// one, otherwise the inferred role. `assigned` is the persisted `runtime.toml`
/// map, keyed by the header's stable id (which for hwmon is also its fan id).
///
/// **This is the DISPLAY role, and a full substitution — including a downgrade.**
/// Reporting the user's own choice back to them is the honest thing for the
/// `role` wire field to do.
///
/// It is deliberately **not** the safety question. "May this header be stopped
/// or under-driven?" is answered by `AppState::header_is_pump_protected`, which
/// unions this with the discovery-time inference so an assignment can add
/// protection but never remove it. Substituting here and unioning there is the
/// intended split; collapsing them either way reintroduces a defect.
pub fn resolve_role(
    assigned: Option<HeaderRole>,
    inferred: (HeaderRole, RoleSource),
) -> (HeaderRole, RoleSource) {
    match assigned {
        Some(role) => (role, RoleSource::UserAssigned),
        None => inferred,
    }
}

/// The pump-protection union, computed from parts the caller already holds.
///
/// `inferred.0.is_pump() || resolve_role(assigned, inferred).0.is_pump()` — an
/// inferred pump OR a resolved one. It is a **union**, so a user assignment can
/// only ever *add* protection: assigning `chassis_fan` to a header the hardware
/// labels `PUMP` changes the display role and changes nothing about whether the
/// daemon will stop it (DEC-312).
///
/// This is the single definition of the predicate.
/// [`crate::api::handlers::AppState::header_is_pump_protected`] is the lookup
/// wrapper around it, for callers holding only a header id. Callers already
/// inside a loop over descriptors must use **this** function rather than the
/// wrapper: the wrapper takes the header-roles lock and then the controller
/// lock, so calling it while holding the controller lock would deadlock on a
/// non-reentrant `parking_lot::Mutex`. Two copies of the rule would be worse
/// still — a floor that disagreed with itself between two endpoints.
pub fn is_pump_protected(assigned: Option<HeaderRole>, inferred: (HeaderRole, RoleSource)) -> bool {
    inferred.0.is_pump() || resolve_role(assigned, inferred).0.is_pump()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [SAFETY] `AUD3-c` / DEC-322: the cross-stack oracle for the classifier
    /// that decides whether a client may offer to STOP a header.
    ///
    /// The GUI hand-mirrors `classify_header_role`'s label branches in
    /// `services/pump_protection.py` — it has to, because a daemon older than
    /// 2.31.0 publishes no `stop_permitted` and the reconstruction is the only
    /// answer available. Until this fixture there was no gate holding the two
    /// copies together, unlike the two previous times this project faced exactly
    /// this problem (DEC-126's `parity_vectors.json`, DEC-162's
    /// `role_classification.json`). The direction of harm is the unsafe one: if
    /// this side learns a new pump-classifying label and the GUI's copy does not,
    /// the GUI concludes "not protected" and the wizard offers to stop a pump.
    ///
    /// The GUI half is `tests/test_header_role_parity.py`, against a
    /// byte-identical copy; `parity.yml` in both repos fails if they diverge.
    ///
    /// `is_aio` is asserted here too rather than merely consumed, so the field
    /// the GUI depends on cannot drift from `aio::is_liquid_cooler_chip`.
    #[test]
    fn header_role_classification_matches_the_cross_stack_oracle() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/header_role_classification.json"
        );
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read header role fixture: {e}"));
        let vectors: serde_json::Value =
            serde_json::from_str(&text).expect("parse header role fixture");
        let cases = vectors["cases"].as_array().expect("cases array");
        assert!(!cases.is_empty(), "an empty oracle asserts nothing");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let chip = case["chip_name"].as_str().unwrap();
            let idx = case["pwm_index"].as_u64().unwrap() as u8;
            let label = case["label"].as_str().unwrap();

            let inferred = classify_header_role(chip, idx, label);
            assert_eq!(
                inferred.0.as_str(),
                case["role"].as_str().unwrap(),
                "role[{name}]"
            );
            assert_eq!(
                is_pump_protected(None, inferred),
                case["pump_protected"].as_bool().unwrap(),
                "pump_protected[{name}] — this is the value that decides whether \
                 a client may offer to stop the header"
            );
            assert_eq!(
                crate::hwmon::aio::is_liquid_cooler_chip(chip),
                case["is_aio"].as_bool().unwrap(),
                "is_aio[{name}] — the GUI cannot see chip_name and consumes this \
                 field instead, so it must not drift from the chip list"
            );
        }
    }

    /// The `it8696` case measured on the AIO-MB validation target: five
    /// channels, no label files at all, so discovery synthesises `pwmN`.
    #[test]
    fn synthesised_labels_carry_no_evidence() {
        for idx in 1..=5u8 {
            let (role, source) = classify_header_role("it8696", idx, &format!("pwm{idx}"));
            assert_eq!(
                role,
                HeaderRole::Unknown,
                "synthesised pwm{idx} must not infer a role"
            );
            assert_eq!(source, RoleSource::None);
        }
    }

    #[test]
    fn empty_label_carries_no_evidence() {
        assert_eq!(
            classify_header_role("nct6798", 3, ""),
            (HeaderRole::Unknown, RoleSource::None)
        );
    }

    #[test]
    fn explicit_pump_labels_classify_as_pump() {
        for label in [
            "AIO_PUMP",
            "PUMP",
            "W_PUMP",
            "PUMP_FAN",
            "PUMP_TACH",
            "SYS_FAN5_PUMP",
            "pump",
        ] {
            assert_eq!(
                classify_header_role("nct6798", 3, label),
                (HeaderRole::Pump, RoleSource::Label),
                "{label} must classify as a pump"
            );
        }
    }

    #[test]
    fn cpu_fan_is_a_cpu_fan_not_a_pump() {
        for label in ["CPU_FAN", "CPU_FAN1", "CPUFAN", "cpu-fan"] {
            assert_eq!(
                classify_header_role("nct6798", 1, label),
                (HeaderRole::CpuFan, RoleSource::Label),
                "{label} must classify as a CPU fan"
            );
        }
    }

    /// The brief's central ambiguity rule: `CPU_OPT` is where an AIO pump most
    /// often lands AND where a second radiator fan lands. Guessing either way
    /// is wrong.
    #[test]
    fn cpu_opt_stays_unknown() {
        assert_eq!(
            classify_header_role("nct6798", 2, "CPU_OPT"),
            (HeaderRole::Unknown, RoleSource::None)
        );
        assert_eq!(
            classify_header_role("it8696", 5, "CPU_OPT"),
            (HeaderRole::Unknown, RoleSource::None)
        );
    }

    #[test]
    fn chassis_labels_are_not_pumps() {
        for label in ["CHA_FAN1", "SYS_FAN2", "Chassis Fan", "sys_fan"] {
            let (role, _) = classify_header_role("nct6798", 4, label);
            assert_eq!(
                role,
                HeaderRole::ChassisFan,
                "{label} must be a chassis fan"
            );
            assert!(!role.is_pump());
        }
    }

    #[test]
    fn cooler_chip_maps_channel_one_to_the_pump() {
        for chip in ["z53", "x53", "kraken2023", "d5next"] {
            assert_eq!(
                classify_header_role(chip, 1, "pwm1"),
                (HeaderRole::Pump, RoleSource::ChipMapping),
                "{chip} pwm1 must map to the pump"
            );
            assert_eq!(
                classify_header_role(chip, 2, "pwm2"),
                (HeaderRole::RadiatorFan, RoleSource::ChipMapping),
                "{chip} pwm2 must map to a radiator fan"
            );
        }
    }

    /// Label beats chip mapping, and this is the case that proves why: a
    /// cooler's second channel labelled as a pump must not be demoted to a
    /// radiator fan, because a radiator fan is stoppable and a pump is not.
    #[test]
    fn a_pump_label_outranks_the_chip_channel_mapping() {
        assert_eq!(
            classify_header_role("z53", 2, "PUMP"),
            (HeaderRole::Pump, RoleSource::Label)
        );
    }

    #[test]
    fn user_assignment_outranks_every_inference() {
        let inferred = classify_header_role("nct6798", 2, "CPU_OPT");
        assert_eq!(inferred, (HeaderRole::Unknown, RoleSource::None));
        assert_eq!(
            resolve_role(Some(HeaderRole::Pump), inferred),
            (HeaderRole::Pump, RoleSource::UserAssigned)
        );
        // …including a downgrade: the user may correct a mis-inferred role.
        let inferred_pump = classify_header_role("nct6798", 3, "PUMP");
        assert_eq!(
            resolve_role(Some(HeaderRole::ChassisFan), inferred_pump),
            (HeaderRole::ChassisFan, RoleSource::UserAssigned)
        );
    }

    #[test]
    fn resolve_passes_the_inference_through_when_unassigned() {
        let inferred = classify_header_role("nct6798", 3, "AIO_PUMP");
        assert_eq!(resolve_role(None, inferred), inferred);
    }

    /// [SAFETY] The invariant that lets this whole feature ship without moving
    /// a single floor: everything the classifier can *infer* as a pump is
    /// already inside the existing 30% pump/CPU floor set, so inference alone
    /// changes nothing. Only a user assignment can add a floor.
    ///
    /// If this fails, the role model has started making floor decisions on its
    /// own and `role_classification.json` is no longer a sufficient oracle.
    #[test]
    fn inferred_pump_roles_are_a_subset_of_the_existing_floor_set() {
        // Mirrors `profile::CPU_PUMP_LABEL_HINTS` + the cooler-chip branch.
        let already_floored = |chip: &str, label: &str| {
            let l = label.to_lowercase();
            ["cpu", "pump", "aio"].iter().any(|h| l.contains(h))
                || crate::hwmon::aio::is_liquid_cooler_chip(chip)
        };
        let cases = [
            ("nct6798", 3u8, "AIO_PUMP"),
            ("nct6798", 3, "PUMP"),
            ("nct6798", 3, "W_PUMP"),
            ("nct6798", 3, "PUMP_FAN"),
            ("nct6798", 5, "SYS_FAN5_PUMP"),
            ("it8696", 1, "CPU_FAN"),
            ("z53", 1, "pwm1"),
            ("d5next", 1, "pwm1"),
            ("z53", 2, "PUMP"),
        ];
        for (chip, idx, label) in cases {
            let (role, _) = classify_header_role(chip, idx, label);
            if matches!(role, HeaderRole::Pump | HeaderRole::CpuFan) {
                assert!(
                    already_floored(chip, label),
                    "inferred {role:?} for {chip}/{label} is NOT already floored — \
                     inference would change a floor, which it must never do"
                );
            }
        }
    }

    #[test]
    fn only_pump_is_pump_safe() {
        assert!(HeaderRole::Pump.is_pump());
        for role in [
            HeaderRole::Unknown,
            HeaderRole::CpuFan,
            HeaderRole::RadiatorFan,
            HeaderRole::ChassisFan,
        ] {
            assert!(!role.is_pump(), "{role:?} must not be treated as a pump");
        }
    }

    #[test]
    fn wire_tokens_round_trip() {
        for role in [
            HeaderRole::Unknown,
            HeaderRole::CpuFan,
            HeaderRole::Pump,
            HeaderRole::RadiatorFan,
            HeaderRole::ChassisFan,
        ] {
            assert_eq!(HeaderRole::from_token(role.as_str()), Some(role));
        }
        // An unrecognised token is rejected, never defaulted — a silently
        // defaulted "pmup" would drop a pump's protection and report success.
        assert_eq!(HeaderRole::from_token("pmup"), None);
        assert_eq!(HeaderRole::from_token(""), None);
        assert_eq!(HeaderRole::from_token("PUMP"), None);
    }

    #[test]
    fn wire_tokens_match_serde() {
        // The hand-written `as_str` and the derived Serialize must not drift.
        for role in [
            HeaderRole::Unknown,
            HeaderRole::CpuFan,
            HeaderRole::Pump,
            HeaderRole::RadiatorFan,
            HeaderRole::ChassisFan,
        ] {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{}\"", role.as_str()));
        }
        for source in [
            RoleSource::None,
            RoleSource::Label,
            RoleSource::ChipMapping,
            RoleSource::UserAssigned,
        ] {
            let json = serde_json::to_string(&source).unwrap();
            assert_eq!(json, format!("\"{}\"", source.as_str()));
        }
    }
}
