//! Cooling-device topology (AIO-MB Phase 4, DEC-316).
//!
//! A named assembly binding a pump header, radiator fan headers, auxiliary
//! members and a control temperature source — so the daemon and the GUI can say
//! "these three channels are one cooler" instead of inferring it from labels
//! every time.
//!
//! # This is metadata, and the engine never reads it
//!
//! **The profile engine does not consult a cooling device.** Topology does not
//! replace `LogicalControl` / `ControlMember`, does not participate in curve
//! evaluation, and does not gate any write. It exists for presentation,
//! configuration, diagnostics and future profile generation. Keeping it inert
//! is what makes this phase safe to land: nothing here can change what a fan
//! does.
//!
//! The one place a device touches a safety-adjacent number is
//! `device_policy_id`, and that is a *selector* — the number itself lives in
//! [`crate::hwmon::device_policy`], compiled in, unreachable from any payload.
//!
//! # Persistence and the downgrade hazard
//!
//! Devices persist as a **top-level** `[[cooling_devices]]` array in
//! `runtime.toml`, not under `[hardware]` beside `header_roles`. That is a
//! deliberate deviation from the obvious precedent, for a safety reason:
//! `RuntimeHardware` carries `deny_unknown_fields`, so a `[hardware]` section
//! containing a key an older daemon does not know fails to parse — and at boot
//! `RuntimeConfig::load_from` does not even quarantine, it warns and falls back
//! to `Default`. A downgrade would therefore drop the user's pump role
//! assignment, and with it a 30% floor, leaving no artifact behind. `RuntimeConfig`
//! itself has no `deny_unknown_fields` by DEC-243's explicit design ("a
//! downgrade costs you only the newer keys, not all of them"), so a top-level
//! array degrades to *losing the topology* while `header_roles` survives.
//!
//! The same reasoning is why [`CoolingDeviceConfig`] itself carries **no**
//! `deny_unknown_fields`, unlike every other section in `runtime_config.rs`. A
//! future daemon adding a field here must not be able to make a downgrade
//! quarantine the whole file — that would reintroduce exactly the hazard the
//! top-level placement was chosen to avoid. Losing metadata is acceptable;
//! losing a pump floor is not.

use serde::{Deserialize, Serialize};

/// Upper bound on devices, so a hand-edited or hostile config cannot make
/// discovery unbounded work.
pub const MAX_COOLING_DEVICES: usize = 16;

/// Upper bound on members in any one list of a device.
pub const MAX_MEMBERS_PER_LIST: usize = 32;

/// Maximum length of a device id, in bytes.
pub const MAX_DEVICE_ID_BYTES: usize = 64;

/// What kind of cooling assembly a device describes.
///
/// Presentation only — no branch in the daemon reads this to decide anything
/// about a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CoolingDeviceKind {
    #[default]
    Unknown,
    AioLiquid,
    AirCooler,
    CustomLoop,
}

impl CoolingDeviceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CoolingDeviceKind::Unknown => "unknown",
            CoolingDeviceKind::AioLiquid => "aio_liquid",
            CoolingDeviceKind::AirCooler => "air_cooler",
            CoolingDeviceKind::CustomLoop => "custom_loop",
        }
    }

    /// Exact-token parse. Unknown tokens yield `None` so the caller can decide
    /// whether that is a rejection (an API payload) or a downgrade to
    /// `Unknown` with a warning (a config file written by a newer daemon).
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "unknown" => Some(CoolingDeviceKind::Unknown),
            "aio_liquid" => Some(CoolingDeviceKind::AioLiquid),
            "air_cooler" => Some(CoolingDeviceKind::AirCooler),
            "custom_loop" => Some(CoolingDeviceKind::CustomLoop),
            _ => None,
        }
    }
}

/// A cooling device exactly as persisted in `runtime.toml`.
///
/// `kind` and `device_policy_id` are stored as `String`, not as the enum and
/// not as a resolved policy — the same reasoning as `header_roles` storing a
/// role token as a string. A hand-edited or future-version value must degrade
/// to a warning, never make the file unparseable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CoolingDeviceConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pump_member: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub radiator_members: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auxiliary_members: Vec<String>,
    /// Advisory only — a curve keeps its own `sensor_id`. Two sources of truth
    /// for "which sensor drives this" would be a bug, so nothing in the control
    /// path reads this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_sensor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_sensor: Option<String>,
    /// Absent means coolant telemetry is **unavailable**, which is the normal
    /// case for a motherboard-connected AIO and is not an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coolant_sensor: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_policy_id: String,
}

impl CoolingDeviceConfig {
    /// The resolved kind, downgrading an unrecognised token to `Unknown`.
    pub fn resolved_kind(&self) -> CoolingDeviceKind {
        if self.kind.is_empty() {
            return CoolingDeviceKind::Unknown;
        }
        CoolingDeviceKind::from_token(&self.kind).unwrap_or_else(|| {
            log::warn!(
                "cooling device {:?}: unrecognised kind {:?}; treating as unknown",
                self.id,
                self.kind
            );
            CoolingDeviceKind::Unknown
        })
    }

    /// The policy this device selects. Never a number from the config — always
    /// a compiled-in policy resolved by id.
    pub fn resolved_policy(&self) -> &'static crate::hwmon::device_policy::DevicePolicy {
        let id = if self.device_policy_id.is_empty() {
            crate::hwmon::device_policy::DEFAULT_POLICY_ID
        } else {
            &self.device_policy_id
        };
        crate::hwmon::device_policy::resolve(id)
    }

    /// Every member id this device claims, in pump → radiator → auxiliary order.
    pub fn all_members(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        if let Some(p) = &self.pump_member {
            out.push(p.as_str());
        }
        out.extend(self.radiator_members.iter().map(String::as_str));
        out.extend(self.auxiliary_members.iter().map(String::as_str));
        out
    }

    /// True when this device claims `header_id` in any role.
    pub fn claims(&self, header_id: &str) -> bool {
        self.all_members().contains(&header_id)
    }

    /// Whether coolant telemetry is available for this device.
    ///
    /// A motherboard-connected AIO normally has none, and that is a supported
    /// configuration rather than a fault — the brief calls it out explicitly.
    pub fn coolant_telemetry(&self) -> &'static str {
        match self.coolant_sensor.as_deref() {
            Some(s) if !s.is_empty() => "available",
            _ => "unavailable",
        }
    }
}

/// Reject a device that cannot be stored safely or would make later resolution
/// ambiguous. Returns a stable reason token suitable for a `validation_error`.
///
/// Deliberately **does not** check that member ids exist: the handler does that
/// against live discovery, exactly as `update_header_role_handler` does, so a
/// header that disappears across a rescan does not make the stored config
/// invalid.
pub fn validate_device(dev: &CoolingDeviceConfig) -> Result<(), String> {
    if dev.id.is_empty() {
        return Err("device id must not be empty".into());
    }
    if dev.id.len() > MAX_DEVICE_ID_BYTES {
        return Err(format!("device id exceeds {MAX_DEVICE_ID_BYTES} bytes"));
    }
    if !dev
        .id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("device id may contain only ASCII letters, digits, '-', '_' and '.'".into());
    }
    // `.` and `..` would be ambiguous if an id is ever used as a path segment.
    if dev.id == "." || dev.id == ".." {
        return Err("device id must not be '.' or '..'".into());
    }
    if dev.radiator_members.len() > MAX_MEMBERS_PER_LIST
        || dev.auxiliary_members.len() > MAX_MEMBERS_PER_LIST
    {
        return Err(format!(
            "a member list exceeds {MAX_MEMBERS_PER_LIST} entries"
        ));
    }
    if dev.all_members().iter().any(|m| m.is_empty()) {
        return Err("member ids must not be empty".into());
    }
    // A header in two roles of one device would make "what is this channel?"
    // unanswerable, and the GUI renders per-role rows.
    let mut seen = std::collections::HashSet::new();
    for m in dev.all_members() {
        if !seen.insert(m) {
            return Err(format!(
                "member {m:?} appears more than once in this device"
            ));
        }
    }
    Ok(())
}

/// Drop devices that cannot be trusted, keeping the rest.
///
/// Used on the load path: one bad hand-edited device must not cost the user
/// every other device, and must never abort the daemon.
pub fn sanitize(devices: Vec<CoolingDeviceConfig>) -> Vec<CoolingDeviceConfig> {
    let mut out: Vec<CoolingDeviceConfig> = Vec::new();
    for dev in devices {
        if out.len() >= MAX_COOLING_DEVICES {
            log::warn!(
                "more than {MAX_COOLING_DEVICES} cooling devices configured; dropping the rest"
            );
            break;
        }
        match validate_device(&dev) {
            Ok(()) => {
                if out.iter().any(|d| d.id == dev.id) {
                    log::warn!(
                        "duplicate cooling device id {:?}; keeping the first",
                        dev.id
                    );
                    continue;
                }
                out.push(dev);
            }
            Err(reason) => {
                log::warn!("dropping invalid cooling device {:?}: {reason}", dev.id);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aio() -> CoolingDeviceConfig {
        CoolingDeviceConfig {
            id: "aio-1".into(),
            name: "AIO Cooling System".into(),
            kind: "aio_liquid".into(),
            pump_member: Some("hwmon:it8696:isa-0a40:pwm5:PUMP".into()),
            radiator_members: vec![
                "hwmon:it8696:isa-0a40:pwm1:CPU_FAN".into(),
                "hwmon:it8696:isa-0a40:pwm2:CPU_OPT".into(),
            ],
            ..Default::default()
        }
    }

    /// The brief's topology requirement: one pump plus *multiple* radiator fans.
    #[test]
    fn a_device_represents_a_pump_and_several_radiator_fans() {
        let d = aio();
        assert!(validate_device(&d).is_ok());
        assert_eq!(d.radiator_members.len(), 2);
        assert_eq!(d.all_members().len(), 3);
        assert!(d.claims("hwmon:it8696:isa-0a40:pwm2:CPU_OPT"));
        assert!(!d.claims("hwmon:it8696:isa-0a40:pwm4:SYS_FAN"));
        assert_eq!(d.resolved_kind(), CoolingDeviceKind::AioLiquid);
    }

    /// The brief's other topology requirement, and the normal case for a
    /// motherboard AIO: no coolant sensor is *not* an error.
    #[test]
    fn missing_coolant_telemetry_is_supported_not_an_error() {
        let d = aio();
        assert_eq!(d.coolant_sensor, None);
        assert_eq!(d.coolant_telemetry(), "unavailable");
        assert!(
            validate_device(&d).is_ok(),
            "a device with no coolant sensor must remain valid"
        );

        let with = CoolingDeviceConfig {
            coolant_sensor: Some("hwmon:z53:coolant".into()),
            ..aio()
        };
        assert_eq!(with.coolant_telemetry(), "available");
        // An empty string is absence, not a sensor named "".
        let blank = CoolingDeviceConfig {
            coolant_sensor: Some(String::new()),
            ..aio()
        };
        assert_eq!(blank.coolant_telemetry(), "unavailable");
    }

    /// A device selects a policy by id; it never carries the number.
    #[test]
    fn a_device_resolves_a_compiled_in_policy() {
        let d = aio();
        // Names none -> the conservative default.
        assert_eq!(d.resolved_policy().id, "generic_pump");

        let named = CoolingDeviceConfig {
            device_policy_id: "generic_fan".into(),
            ..aio()
        };
        assert_eq!(named.resolved_policy().id, "generic_fan");

        // An id this daemon does not know degrades toward MORE protection.
        let future = CoolingDeviceConfig {
            device_policy_id: "some-future-validated-pump".into(),
            ..aio()
        };
        assert_eq!(future.resolved_policy().id, "generic_pump");
    }

    #[test]
    fn an_unrecognised_kind_degrades_to_unknown() {
        let d = CoolingDeviceConfig {
            kind: "thermosiphon".into(),
            ..aio()
        };
        assert_eq!(d.resolved_kind(), CoolingDeviceKind::Unknown);
        let blank = CoolingDeviceConfig {
            kind: String::new(),
            ..aio()
        };
        assert_eq!(blank.resolved_kind(), CoolingDeviceKind::Unknown);
    }

    #[test]
    fn validation_rejects_unusable_ids_and_duplicate_members() {
        let bad_id = CoolingDeviceConfig {
            id: "aio/1".into(),
            ..aio()
        };
        assert!(validate_device(&bad_id).is_err());

        let empty = CoolingDeviceConfig {
            id: String::new(),
            ..aio()
        };
        assert!(validate_device(&empty).is_err());

        let dotdot = CoolingDeviceConfig {
            id: "..".into(),
            ..aio()
        };
        assert!(validate_device(&dotdot).is_err());

        let long = CoolingDeviceConfig {
            id: "a".repeat(MAX_DEVICE_ID_BYTES + 1),
            ..aio()
        };
        assert!(validate_device(&long).is_err());

        // The same header as pump AND radiator is unanswerable.
        let dup = CoolingDeviceConfig {
            radiator_members: vec!["hwmon:it8696:isa-0a40:pwm5:PUMP".into()],
            ..aio()
        };
        assert!(validate_device(&dup).is_err());

        let too_many = CoolingDeviceConfig {
            radiator_members: (0..MAX_MEMBERS_PER_LIST + 1)
                .map(|i| format!("hwmon:x:y:pwm{i}:L"))
                .collect(),
            ..aio()
        };
        assert!(validate_device(&too_many).is_err());
    }

    /// One bad hand-edited device must not cost the user the good ones.
    #[test]
    fn sanitize_drops_only_the_invalid() {
        let good = aio();
        let bad = CoolingDeviceConfig {
            id: "has/slash".into(),
            ..Default::default()
        };
        let second = CoolingDeviceConfig {
            id: "loop-1".into(),
            ..Default::default()
        };
        let out = sanitize(vec![good.clone(), bad, second]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "aio-1");
        assert_eq!(out[1].id, "loop-1");
    }

    #[test]
    fn sanitize_dedupes_ids_and_bounds_the_count() {
        let a = aio();
        let dupe = CoolingDeviceConfig {
            name: "Impostor".into(),
            ..aio()
        };
        let out = sanitize(vec![a, dupe]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "AIO Cooling System", "the first must win");

        let many: Vec<_> = (0..MAX_COOLING_DEVICES + 5)
            .map(|i| CoolingDeviceConfig {
                id: format!("dev-{i}"),
                ..Default::default()
            })
            .collect();
        assert_eq!(sanitize(many).len(), MAX_COOLING_DEVICES);
    }

    #[test]
    fn kind_tokens_match_serde() {
        for k in [
            CoolingDeviceKind::Unknown,
            CoolingDeviceKind::AioLiquid,
            CoolingDeviceKind::AirCooler,
            CoolingDeviceKind::CustomLoop,
        ] {
            let json = serde_json::to_string(&k).unwrap();
            let token = json.trim_matches('"');
            assert_eq!(token, k.as_str());
            assert_eq!(CoolingDeviceKind::from_token(token), Some(k));
        }
        assert_eq!(CoolingDeviceKind::from_token("AIO_LIQUID"), None);
    }
}
