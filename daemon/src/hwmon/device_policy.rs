//! Trusted device capability policy (AIO-MB Phase 4, DEC-316).
//!
//! A small typed policy model, compiled into the daemon binary, describing what
//! a *class* of cooling device may safely be asked to do. It exists to answer
//! one question truthfully — "how low may this pump be driven?" — from a source
//! no client can write to.
//!
//! # Why this is not data
//!
//! The socket is 0666 with no authentication (`docs/08 § Trust model`), so any
//! local user can already call any endpoint. The threat this module defends
//! against is therefore **not** an unauthorised caller; it is an imported or
//! shared *profile file* carrying `minimum_safe_pwm = 1`. The rule that follows
//! is narrow and absolute:
//!
//! > Safety-critical numbers never travel in a profile or in any inbound
//! > payload. They live in this file and are selected by id.
//!
//! [`DevicePolicy`] therefore deliberately derives **no** `Deserialize`. That
//! makes "no inbound payload can set a safety number" a property the compiler
//! enforces rather than one a reviewer has to notice — a cooling device stores
//! a `device_policy_id` *string*, and the daemon resolves it here. An
//! unrecognised id resolves to [`GENERIC_PUMP`] with a warning, which is the
//! fail-safe direction and mirrors DEC-311's treatment of an unrecognised role
//! token.
//!
//! # What ships in this phase
//!
//! Generic entries only. [`GENERIC_PUMP`]'s floor **is**
//! [`crate::profile::HARD_PUMP_CPU_FLOOR_PCT`], so with the shipped table every
//! header resolves to exactly the floor the engine already enforces and **no
//! behaviour moves**. The mechanism is here so that a later phase — with the
//! `AIO3-b` vendor sign-off and Phase 5 validation evidence — can add a named
//! entry as *data* rather than as a code change.
//!
//! # Where the floor is enforced (read this before wiring a relaxing entry)
//!
//! [`resolve_policy_floor`] is the reporting path: it computes the number the
//! API publishes as `effective_min_pwm_pct`. It is **not** yet the enforcement
//! path. The floor is independently enforced at five production sites:
//!
//! | Site | What it governs |
//! |---|---|
//! | `profile::validate` (the `FLOOR_TOO_LOW` check) | profile authoring |
//! | `profile_engine::tuning::member_effective_floor` | the eval clamp |
//! | `control_override::identify_target_for_role` | identify / pump perturbation |
//! | `api::handlers::hwmon_ctl::verify_test_duty` | PWM verify |
//! | `api::handlers::hwmon_ctl::hwmon_characterize_handler` | the characterisation sweep |
//!
//! Three of those reach the floor through `AppState::header_is_pump_protected`
//! rather than through `member_effective_floor`, so a future phase that adds a
//! relaxing entry must wire **all five** or a policy floor would be honoured by
//! the profile engine and silently ignored by identify, verify and
//! characterize — **plus, since DEC-322, the two diagnostic RESTORE writes
//! (`hwmon_ctl::restore_duty` and `characterization::RestoreOnDrop::restore_floor`),
//! so a future relaxing policy must wire SIX sites and not five** (`AUD3-l`).
//! `reported_floor_matches_enforced_floor_for_every_shipped_policy`
//! below is what keeps the reported number honest until that happens: it fails
//! the moment a shipped policy would report a floor the engine does not enforce.
//! It asserts over **both** values of `pump_protected`. It once tested only
//! `true` — which is precisely why it stayed green while a non-pump member of a
//! cooling device published an unenforced 30% (`WIRE-b`): the over-claim lived
//! in the branch the test never called.
//!
//! Be precise about what that loop now proves, because it is easy to over-credit:
//! the `false` branch is **policy-independent by construction** (it returns
//! before reading `policy`), so iterating the table there adds no per-policy
//! coverage — it is a regression guard against reintroducing a policy-derived
//! floor, and nothing more. What pins the value actually *published* is the
//! call-site test `a_cooling_device_member_carries_a_floor_only_when_it_is_pump_protected`
//! in `api::responses`. Do not delete that on the belief this invariant covers it.

/// The lowest duty any pump may be driven to, whatever a policy claims.
///
/// A backstop under [`resolve_policy_floor`], applied regardless of table
/// contents, so that a mistaken or malicious future table entry cannot reach a
/// stall. 20% is the figure the AIO brief cites as the lowest duty at which a
/// validated pump remains controllable; it is deliberately **not** 0.
pub const ABSOLUTE_PUMP_FLOOR_PCT: f64 = 20.0;

/// A trusted, compiled-in description of what a class of device may safely do.
///
/// **No `Deserialize`, deliberately** — see the module docs. Adding one would
/// silently convert every field here into something an inbound payload could
/// set, which is the exact failure this type exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct DevicePolicy {
    /// Stable id, referenced by `CoolingDevice::device_policy_id`.
    pub id: &'static str,
    /// Human-readable name for display.
    pub display_name: &'static str,
    /// Lowest duty this class of device may be commanded to, in percent.
    pub minimum_safe_pwm: f64,
    /// Whether this device may be driven to 0 at all. False for every pump.
    pub supports_stop: bool,
    /// How long after power-on the device ignores PWM, where that is a known
    /// property of the class. `None` means "unknown", never "zero" — see
    /// `AIO3-a`, which is why no shipped entry sets it.
    pub startup_override_seconds: Option<u32>,
    /// Expected tach range, for plausibility reporting only. Never a control input.
    pub expected_rpm_min: Option<u16>,
    /// Upper end of the expected tach range.
    pub expected_rpm_max: Option<u16>,
    /// Whether the device runs its own internal control loop that may override
    /// host PWM (the `possible_device_override` signature from Phase 3).
    pub internal_control_possible: bool,
}

/// The conservative default for any pump whose exact hardware is unknown.
///
/// Its floor **is** `HARD_PUMP_CPU_FLOOR_PCT` by construction, not a copy of it:
/// that is what makes the shipped table behaviour-neutral, and
/// `generic_pump_floor_is_the_engine_floor` fails if the two ever drift.
pub static GENERIC_PUMP: DevicePolicy = DevicePolicy {
    id: "generic_pump",
    display_name: "Generic pump (unknown hardware)",
    minimum_safe_pwm: crate::profile::HARD_PUMP_CPU_FLOOR_PCT,
    supports_stop: false,
    startup_override_seconds: None,
    expected_rpm_min: None,
    expected_rpm_max: None,
    internal_control_possible: false,
};

/// The default for an ordinary fan: no policy floor of its own.
///
/// A fan's minimum comes from its control's `minimum_pct` (and the GUI's
/// role-aware 20% chassis policy, DEC-095), not from here. This entry exists so
/// that a non-pump member resolves to something rather than to nothing.
pub static GENERIC_FAN: DevicePolicy = DevicePolicy {
    id: "generic_fan",
    display_name: "Generic fan",
    minimum_safe_pwm: 0.0,
    supports_stop: true,
    startup_override_seconds: None,
    expected_rpm_min: None,
    expected_rpm_max: None,
    internal_control_possible: false,
};

/// Every policy the daemon ships. Generic only in this phase, by decision.
pub static POLICIES: &[&DevicePolicy] = &[&GENERIC_PUMP, &GENERIC_FAN];

/// The id assumed for a cooling device that names none, and the fail-safe
/// landing spot for an id this daemon does not recognise.
pub const DEFAULT_POLICY_ID: &str = "generic_pump";

/// Resolve a policy id to its compiled-in policy.
///
/// An unrecognised id resolves to [`GENERIC_PUMP`] **with a warning** rather
/// than to an error or to a permissive default: a `runtime.toml` written by a
/// newer daemon, or edited by hand, must degrade toward more protection and not
/// less. This is DEC-311's treatment of an unknown role token, one layer up.
pub fn resolve(id: &str) -> &'static DevicePolicy {
    if let Some(p) = POLICIES.iter().find(|p| p.id == id) {
        return p;
    }
    log::warn!(
        "unknown device policy id {id:?}; falling back to {} (the conservative default)",
        GENERIC_PUMP.id
    );
    &GENERIC_PUMP
}

/// The duty floor a header actually gets under a policy.
///
/// `pump_protected` is the daemon's own union predicate
/// (`AppState::header_is_pump_protected`), never a client claim and never the
/// resolved *display* role — DEC-312 is explicit that reading `role == "pump"`
/// alone for a safety decision is a bug.
///
/// The clamp to [`ABSOLUTE_PUMP_FLOOR_PCT`] is applied **after** the policy
/// value, so it binds regardless of what the table says. A policy may therefore
/// tighten a pump's range downward toward 20%, and can never reach a stop.
///
/// **Only a policy that knows it is describing a pump may move a pump's floor.**
/// `supports_stop` is that discriminator: a policy permitting a stop cannot be
/// describing a pump, so meeting one on a pump-protected header is a
/// misconfiguration — a device naming `generic_fan` for its pump member, say —
/// and not a validated relaxation. Such a policy falls back to [`GENERIC_PUMP`]
/// rather than being honoured, because honouring it would publish a floor
/// *below* the 30% the engine actually enforces. That is the one direction this
/// function must never fail in: reporting a floor lower than reality would make
/// the GUI's displayed pump minimum a lie, which is worse than the client-side
/// reconstruction this value replaces.
pub fn resolve_policy_floor(policy: &DevicePolicy, pump_protected: bool) -> f64 {
    if !pump_protected {
        // Nothing applies a DEVICE-POLICY floor to a header that is not
        // pump-protected: every site that could keys on
        // `AppState::header_is_pump_protected`, a union in which cooling-device
        // membership is not a term. Publishing the policy's own number here
        // advertises a floor no such site honours.
        //
        // The profile-role floor reached through
        // `profile_engine::tuning::member_effective_floor` is a *different*
        // floor — GUI-owned `minimum_pct`, DEC-095 — and is deliberately not
        // what this field reports. Do not read the sentence above as "no floor
        // of any kind applies here".
        //
        // `PwmHeaderEntry::from_descriptor` already avoids exactly this for a
        // header with NO device, by resolving it under `GENERIC_FAN` rather than
        // the pump policy — its comment gives this same reason. A radiator or
        // auxiliary member of an AIO reached this branch carrying its *device's*
        // `generic_pump` policy and so published 30 beside `stop_permitted:
        // true` (`WIRE-b`). It now reports 0, exactly as an ordinary chassis
        // header already did.
        return 0.0;
    }
    let declared = if policy.supports_stop {
        GENERIC_PUMP.minimum_safe_pwm
    } else {
        policy.minimum_safe_pwm
    };
    declared.max(ABSOLUTE_PUMP_FLOOR_PCT)
}

/// Whether the daemon will let this header be driven to 0.
///
/// **The device policy is deliberately NOT a term (`AIO7-d`).** This value is a
/// prediction about one specific behaviour — what `POST /fans/{id}/identify`
/// will do — and that behaviour branches on `AppState::header_is_pump_protected`
/// alone: the union of the user's assigned role and discovery's inferred one,
/// with cooling-device membership not a term at all. So the published field is
/// exactly `!pump_protected`, and `stop_permitted_matches_identify` pins it.
///
/// It used to read `!pump_protected && policy.supports_stop`, which took the
/// policy from the *device* — and `PwmHeaderEntry::from_descriptor` resolves one
/// policy for **every member** of a device. A radiator fan in an AIO therefore
/// inherited `GENERIC_PUMP`'s `supports_stop: false` and was published as
/// unstoppable while identify stopped it. Measured on an X870E AORUS MASTER:
/// `pwm1`, a `radiator_members` entry with `role: unknown`, no pump label and
/// not a liquid-cooler channel 1, reported `stop_permitted: false` with its
/// identify predicate `false`.
///
/// Both directions were wrong and the second is the one that matters: a
/// `pump_member` named **without** a pump role — reachable by `curl`, and
/// something DEC-316's own "naming a `pump_member` confers no floor" note
/// already contemplates — was promised `stop_permitted: false` while identify
/// drove it to 0. A pump stopped while every client was told it would not be.
///
/// **Do not "fix" the divergence from the other end** by making device
/// membership a term in `header_is_pump_protected`: that would hand a 30% floor
/// and stop-refusal to every radiator and auxiliary fan in a device, which is a
/// real cooling and behaviour change rather than a reporting correction.
///
/// `DevicePolicy::supports_stop` is still published, as part of the policy
/// descriptor — what a *class of device* declares stays visible; it is simply no
/// longer conflated with a per-header prediction.
///
/// **The sibling field has since been brought into line (`WIRE-b`).** Until then
/// [`resolve_policy_floor`] resolved `effective_min_pwm_pct` through the device
/// policy for *every* member, so a non-pump member of an AIO published a 30%
/// floor nothing enforced for it, directly beside this field's `true`. Both are
/// now functions of `pump_protected` alone.
///
/// **That correlation is a property of the shipped table, not a contract, so do
/// not derive one from the other.** A future relaxing policy moves the floor
/// (down to [`ABSOLUTE_PUMP_FLOOR_PCT`]) without changing this value at all, and
/// the fix above deliberately did not make membership a term here — that would
/// hand a 30% floor and stop-refusal to every radiator and auxiliary fan in a
/// device, which is a real cooling change rather than a reporting correction.
pub fn stop_permitted(pump_protected: bool) -> bool {
    !pump_protected
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property that makes the shipped table behaviour-neutral: the generic
    /// pump's floor is not merely *equal to* the engine floor today, it is the
    /// same constant. If someone edits one, this fails rather than silently
    /// moving every unknown pump.
    #[test]
    fn generic_pump_floor_is_the_engine_floor() {
        assert_eq!(
            GENERIC_PUMP.minimum_safe_pwm,
            crate::profile::HARD_PUMP_CPU_FLOOR_PCT
        );
        assert!(
            !GENERIC_PUMP.supports_stop,
            "a pump must never be stoppable"
        );
    }

    /// The backstop holds for everything shipped, so no table edit alone can
    /// reach a stall.
    #[test]
    fn no_shipped_policy_is_below_the_absolute_pump_floor() {
        for p in POLICIES {
            // A fan legitimately has no floor; the bound is about pumps.
            if !p.supports_stop {
                assert!(
                    p.minimum_safe_pwm >= ABSOLUTE_PUMP_FLOOR_PCT,
                    "policy {} declares {} which is below the absolute pump floor {}",
                    p.id,
                    p.minimum_safe_pwm,
                    ABSOLUTE_PUMP_FLOOR_PCT
                );
            }
        }
    }

    /// **The honesty invariant.** `effective_min_pwm_pct` is published to the
    /// GUI, which stops re-deriving the floor itself and believes this number.
    /// For every shipped policy it must equal what the engine actually enforces
    /// — otherwise the API would advertise a floor no site honours, which is
    /// worse than the reconstruction it replaces.
    ///
    /// This is what makes it safe to ship the mechanism without wiring the five
    /// enforcement sites: it fails the moment a policy is added that would
    /// report something the engine does not do.
    #[test]
    fn reported_floor_matches_enforced_floor_for_every_shipped_policy() {
        for p in POLICIES {
            let reported = resolve_policy_floor(p, true);
            assert_eq!(
                reported,
                crate::profile::HARD_PUMP_CPU_FLOOR_PCT,
                "policy {} reports a pump floor of {reported} but the engine enforces {}. \
                 Adding a relaxing policy requires wiring all five enforcement sites \
                 listed in this module's docs first.",
                p.id,
                crate::profile::HARD_PUMP_CPU_FLOOR_PCT
            );

            // The other half of the predicate, and the half this test was
            // missing (`WIRE-b`). A header that is not pump-protected has no
            // enforced floor at all, so ANY non-zero report for it is a floor no
            // site honours — the exact failure the assertion above exists to
            // prevent, one branch over. Testing only `true` is what let
            // `generic_pump` publish 30 for a radiator fan.
            let unprotected = resolve_policy_floor(p, false);
            assert_eq!(
                unprotected, 0.0,
                "policy {} reports a floor of {unprotected} for a header that is not \
                 pump-protected, but no enforcement site applies one there",
                p.id
            );
        }
    }

    /// **`WIRE-b`.** The shape measured on an X870E AORUS MASTER: a radiator fan
    /// in an AIO cooling device resolves that *device's* policy — `generic_pump`
    /// by default ([`DEFAULT_POLICY_ID`]) — while `header_is_pump_protected`,
    /// which every enforcement site keys on, is false for it because membership
    /// is not a term in that union. It therefore published a 30% floor nothing
    /// applied, next to `stop_permitted: true`.
    ///
    /// **This is the regression-validity target for the fix:** bypass only the
    /// `!pump_protected` early return in [`resolve_policy_floor`] and this must
    /// go red. Both branches are asserted, or a predicate stuck at one value
    /// would pass.
    #[test]
    fn a_non_pump_member_of_a_pump_policy_device_reports_no_floor() {
        assert_eq!(
            resolve_policy_floor(&GENERIC_PUMP, false),
            0.0,
            "a radiator fan inheriting its device's pump policy must not be published \
             a floor the engine does not enforce for it"
        );
        assert_eq!(
            resolve_policy_floor(&GENERIC_PUMP, true),
            crate::profile::HARD_PUMP_CPU_FLOOR_PCT,
            "the pump member of that same device must keep the enforced floor"
        );
    }

    /// The mechanism genuinely works — a tighter policy lowers the floor toward
    /// the backstop. Uses a test-only policy so the shipped table stays generic.
    #[test]
    fn a_trusted_policy_may_tighten_the_range_down_to_the_backstop() {
        let validated = DevicePolicy {
            id: "test_validated_pump",
            display_name: "Validated pump",
            minimum_safe_pwm: 25.0,
            supports_stop: false,
            ..GENERIC_PUMP
        };
        assert_eq!(resolve_policy_floor(&validated, true), 25.0);

        let tighter = DevicePolicy {
            minimum_safe_pwm: 45.0,
            ..validated
        };
        assert_eq!(
            resolve_policy_floor(&tighter, true),
            45.0,
            "a policy above the generic floor must raise it"
        );
    }

    /// The clamp binds regardless of the table. This is the test the
    /// regression-validity check targets: bypass only the `.max(...)` in
    /// `resolve_policy_floor` and this must go red.
    #[test]
    fn a_policy_below_the_backstop_is_clamped_not_honoured() {
        let reckless = DevicePolicy {
            id: "test_reckless",
            minimum_safe_pwm: 5.0,
            supports_stop: false,
            ..GENERIC_PUMP
        };
        assert_eq!(
            resolve_policy_floor(&reckless, true),
            ABSOLUTE_PUMP_FLOOR_PCT,
            "a policy below the backstop must be clamped up to it"
        );
    }

    /// A fan policy pointed at a pump-protected header is a misconfiguration,
    /// not a relaxation. Honouring `generic_fan`'s 0% there would publish a
    /// floor below the 30% the engine enforces — the one direction this must
    /// never fail in.
    #[test]
    fn a_fan_policy_cannot_lower_a_pump_protected_header() {
        assert_eq!(
            resolve_policy_floor(&GENERIC_FAN, true),
            crate::profile::HARD_PUMP_CPU_FLOOR_PCT,
            "a stoppable policy must not move a pump's floor"
        );
        // The same policy on an ordinary header does what it says.
        assert_eq!(resolve_policy_floor(&GENERIC_FAN, false), 0.0);
    }

    #[test]
    fn an_unknown_policy_id_falls_back_to_the_conservative_default() {
        assert_eq!(resolve("generic_pump").id, "generic_pump");
        assert_eq!(resolve("generic_fan").id, "generic_fan");
        // The fail-safe direction: unknown means *more* protection.
        assert_eq!(resolve("nl-lc1-validated").id, "generic_pump");
        assert_eq!(resolve("").id, "generic_pump");
        assert_eq!(resolve(DEFAULT_POLICY_ID).id, "generic_pump");
    }

    /// A pump-protected header is never stoppable, whatever any policy claims.
    ///
    /// The third assertion is the one that changed (`AIO7-d`). It used to read
    /// `assert!(!stop_permitted(&GENERIC_PUMP, false))` — i.e. it *encoded the
    /// defect*: a header that is not pump-protected was expected to publish
    /// "unstoppable" purely because the device it belongs to has a pump policy,
    /// while identify stopped it. A test can pin a bug as firmly as it pins a
    /// fix, and this one did for a whole release.
    #[test]
    fn pump_protection_is_what_decides_stoppability() {
        assert!(
            !stop_permitted(true),
            "a protected header is never stoppable"
        );
        assert!(stop_permitted(false), "an unprotected header is stoppable");
    }

    /// [SAFETY] The published `stop_permitted` must equal what identify actually
    /// does, for **every shipped policy** — the `AIO7-d` honesty test, and the
    /// sibling of `reported_floor_matches_enforced_floor_for_every_shipped_policy`
    /// one field over.
    ///
    /// Identify branches on `AppState::header_is_pump_protected` alone, so any
    /// policy term reintroduced here would make the wire disagree with the
    /// behaviour it describes. Iterating `POLICIES` is what makes this catch a
    /// *future* policy rather than only today's two: adding one with
    /// `supports_stop: false` used to silently mark every member of every device
    /// using it as unstoppable.
    #[test]
    fn stop_permitted_matches_identify_for_every_shipped_policy() {
        for p in POLICIES {
            for pump_protected in [true, false] {
                assert_eq!(
                    stop_permitted(pump_protected),
                    !pump_protected,
                    "policy {} publishes a stoppability that identify does not \
                     obey: identify refuses a stop iff header_is_pump_protected \
                     ({pump_protected}), and cooling-device membership is not a \
                     term in that predicate. Do NOT reconcile this by adding \
                     membership to header_is_pump_protected — that floors every \
                     radiator fan in the device.",
                    p.id
                );
            }
        }
    }

    /// The compile-time property, asserted against the source.
    ///
    /// Matched in **attribute position** — a line that starts with `#[derive`.
    /// A substring search for "Deserialize" would match this module's own
    /// explanation of why it has none, which is the self-scanning trap recorded
    /// in `CLAUDE.md § Hard-won lessons` (the `polling.rs` precedent).
    #[test]
    fn device_policy_derives_no_deserialize() {
        let src = include_str!("device_policy.rs");
        for line in src.lines() {
            let t = line.trim_start();
            if t.starts_with("#[derive") {
                assert!(
                    !t.contains("Deserialize"),
                    "a derive in this module names Deserialize: {t}. Safety numbers must \
                     not be constructible from an inbound payload — see the module docs."
                );
            }
        }
    }
}
