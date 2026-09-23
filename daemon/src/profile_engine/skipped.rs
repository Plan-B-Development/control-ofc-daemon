//! Tracking of controls the engine cannot resolve (273-i).
//!
//! A control whose curve will not resolve is *skipped*: no PWM command is
//! produced for it and its fans hold whatever they were last told. That is the
//! correct safety behaviour (DEC-269 — going blind must never reduce cooling),
//! and for a transient cause it is invisible by design, because the next tick
//! fixes it.
//!
//! What was missing is the case that never fixes itself. A Mix naming a curve id
//! that no longer exists, or a Sync whose target is skipped, is unresolvable for
//! as long as the profile says so — and the daemon said nothing at all about it:
//! no log (the one skip that *was* logged went to `log::debug!`, below the
//! shipped `RUST_LOG=info`) and no field on `/status`. The fan simply stopped
//! responding, and nothing anywhere could be pointed at to explain why.
//!
//! This tracker turns that silence into a bounded story per control, exactly as
//! [`SensorFailureTracker`](crate::health::sensor_failure::SensorFailureTracker)
//! does for unreadable sensors:
//!
//! - **Skipped for [`SKIP_DEBOUNCE_TICKS`] consecutive ticks:** logged once and
//!   listed on `/status`.
//! - **Resolves again:** logged once and removed from the list.
//! - **Neither:** nothing is emitted, so a one-tick blip costs no journal lines.
//!
//! The debounce is what makes it safe to run at 1 Hz. `curve_eligible`'s
//! freshness budget floors at 5 s, so a sensor sitting on that boundary can flap
//! between eligible and not; edge-triggering on the first skipped tick would
//! emit two lines per flap, forever — the precise failure DEC-193 was written to
//! stop. The cost is that a genuinely stuck control takes ~3 s to appear, on a
//! surface that is display-only.
//!
//! The tracker holds no I/O and no clock of its own (the caller passes `now`),
//! so it is a pure, deterministic state machine that is unit-tested directly.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

// The two types that reach `/status` live with the rest of the state model
// (`health::state`), exactly as `UnavailableSensor` does for DEC-193. Keeping
// them there is what stops the state model having to depend on the engine.
pub use crate::health::state::{SkipReason, SkippedControl};

/// Consecutive skipped ticks before a control is logged and listed.
///
/// Three ticks at the engine's 1 Hz cadence. See the module docs for why this is
/// debounced rather than edge-triggered.
pub const SKIP_DEBOUNCE_TICKS: u32 = 3;

/// What the evaluator records about one skipped control.
///
/// Deliberately carries no timestamp: `evaluate_profile_with_overrides` stays
/// clock-free and therefore trivially testable, and the tracker — which already
/// takes `now` from its caller — is the single place a clock is read. The
/// timestamped form is [`SkippedControl`], which the tracker produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipRecord {
    pub control_id: String,
    pub control_name: String,
    pub reason: SkipReason,
}

/// How much of a control this daemon can actually write (`OFN-j`).
///
/// Distinct from every other skip reason: the curve resolved and an output was
/// computed: it is the *delivery* that has nowhere to go, because no backend
/// can write a member — its source's backend is absent, or (`OFN-al`) its
/// header is one the daemon cannot write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deliverability {
    /// No members at all. Not a fault — a member-less control still publishes a
    /// tick output for any Sync that mirrors it (DEC-151).
    NoMembers,
    /// Every member can be written.
    All,
    /// Some members can be written and some cannot. The control IS commanding
    /// fans, so it must not be reported as uncommanded — see `BackendUnavailable`.
    Partial,
    /// No member can be written: the control commands nothing at all.
    None,
}

/// What this daemon can deliver a hwmon write to (`OFN-al`).
#[derive(Debug, Clone, Copy)]
pub enum HwmonTargets<'a> {
    /// No hwmon backend at all — no writable header was discovered (DEC-376).
    Absent,
    /// A backend whose writable set could not be read when it was built (a
    /// contended controller lock at engine start). Every `hwmon:` member is
    /// assumed deliverable — the same over-claim `HwmonBackend::new` already
    /// makes in that case, in the direction that never hides a live control.
    Unmeasured,
    /// The ids of the headers the daemon can write — `forced_target_ids()`,
    /// measured once when the backend was built.
    Writable(&'a HashSet<String>),
}

/// The backends a control's members are resolved against (`OFN-j`, `OFN-al`).
#[derive(Debug, Clone, Copy)]
pub struct DeliveryTargets<'a> {
    /// An OpenFanController has been adopted.
    pub openfan: bool,
    pub hwmon: HwmonTargets<'a>,
}

impl DeliveryTargets<'static> {
    /// Backend presence alone, with every header of a present hwmon backend
    /// taken as writable — the per-backend answer this replaced, for a caller
    /// that has no writable set to consult.
    pub fn backends(openfan: bool, hwmon: bool) -> Self {
        Self {
            openfan,
            hwmon: if hwmon {
                HwmonTargets::Unmeasured
            } else {
                HwmonTargets::Absent
            },
        }
    }
}

/// Whether this daemon can write `member` right now.
///
/// Per MEMBER, not per backend (`OFN-al`): on a board with at least one
/// writable header the hwmon backend exists, so a per-backend answer called a
/// member bound to a read-only header deliverable — and `HwmonBackend::apply`'s
/// DEC-102 backstop then dropped every one of its writes in silence. An
/// `hwmon:` member is deliverable only if its header is in the writable set,
/// which also covers an id this board never discovered (a profile imported
/// from another machine): nothing can write that either.
///
/// GPU sources are always deliverable: the GPU backend is not optional in the
/// tick body (`gpu_be` is a value, not an `Option`), and a GPU with no writable
/// fan path is rejected far earlier, at profile validation (DEC-102).
pub fn member_is_deliverable(
    member: &crate::profile::ControlMember,
    targets: DeliveryTargets<'_>,
) -> bool {
    match member.source.as_str() {
        "openfan" => targets.openfan,
        "hwmon" => match targets.hwmon {
            HwmonTargets::Absent => false,
            HwmonTargets::Unmeasured => true,
            HwmonTargets::Writable(ids) => ids.contains(&member.member_id),
        },
        _ => true,
    }
}

/// Classify one control against what this daemon can actually write.
///
/// Pure, and deliberately clock-free and profile-shaped like its neighbours in
/// this module, so the rule is testable without an engine, a serial device or a
/// sysfs tree.
pub fn control_deliverability(
    control: &crate::profile::LogicalControl,
    targets: DeliveryTargets<'_>,
) -> Deliverability {
    if control.members.is_empty() {
        return Deliverability::NoMembers;
    }
    let total = control.members.len();
    let live = control
        .members
        .iter()
        .filter(|m| member_is_deliverable(m, targets))
        .count();
    match live {
        0 => Deliverability::None,
        n if n == total => Deliverability::All,
        _ => Deliverability::Partial,
    }
}

/// A transition worth one journal line. Emitted at most once per transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipEvent {
    /// Crossed the debounce — the control is not being commanded.
    Skipped {
        id: String,
        name: String,
        reason: SkipReason,
    },
    /// A previously-listed control resolved again (or left the profile).
    Resumed { id: String, name: String },
}

/// Tracks which controls are being skipped, and for how long.
#[derive(Debug)]
pub struct SkippedControlTracker {
    /// Consecutive skipped ticks before listing.
    debounce: u32,
    /// Consecutive skipped-tick count per control id, for controls not yet
    /// listed. Cleared the moment a control resolves.
    streak: HashMap<String, u32>,
    /// Currently-listed controls. `since` and `reason` are stable for the
    /// lifetime of one skip — set once, on entry, mirroring
    /// `SensorFailureTracker`'s quarantine entries. A control whose *cause*
    /// changes while it is already listed keeps the original reason rather than
    /// producing a second log line for the same continuous outage.
    active: HashMap<String, SkippedControl>,
}

impl SkippedControlTracker {
    /// Create a tracker that lists a control after `debounce` consecutive
    /// skipped ticks.
    pub fn new(debounce: u32) -> Self {
        Self {
            debounce,
            streak: HashMap::new(),
            active: HashMap::new(),
        }
    }

    /// Record one tick's skipped set. `skipped` is every control the engine
    /// could not resolve this tick; anything absent from it resolved normally.
    ///
    /// Returns the (at-most-once) transition events for the caller to log.
    pub fn record_tick(&mut self, skipped: &[SkipRecord], now: Instant) -> Vec<SkipEvent> {
        let mut events = Vec::new();

        // Anything listed or streaking that is NOT skipped this tick has
        // resolved. A control removed from the profile also lands here; the
        // effect is the same and correct — it leaves the list.
        let still: HashMap<&str, &SkipRecord> =
            skipped.iter().map(|s| (s.control_id.as_str(), s)).collect();

        let resumed: Vec<String> = self
            .active
            .keys()
            .filter(|id| !still.contains_key(id.as_str()))
            .cloned()
            .collect();
        for id in resumed {
            if let Some(entry) = self.active.remove(&id) {
                events.push(SkipEvent::Resumed {
                    id: entry.control_id,
                    name: entry.control_name,
                });
            }
        }
        self.streak.retain(|id, _| still.contains_key(id.as_str()));

        for s in skipped {
            if self.active.contains_key(&s.control_id) {
                // Already listed — no second line for one continuous outage.
                continue;
            }
            let streak = self.streak.entry(s.control_id.clone()).or_insert(0);
            *streak += 1;
            if *streak >= self.debounce {
                self.streak.remove(&s.control_id);
                self.active.insert(
                    s.control_id.clone(),
                    SkippedControl {
                        control_id: s.control_id.clone(),
                        control_name: s.control_name.clone(),
                        reason: s.reason,
                        since: now,
                    },
                );
                events.push(SkipEvent::Skipped {
                    id: s.control_id.clone(),
                    name: s.control_name.clone(),
                    reason: s.reason,
                });
            }
        }

        events
    }

    /// Currently-listed controls, sorted by id so the `/status` array is stable
    /// across polls (a jittering array reads as churn to a diffing client).
    pub fn snapshot(&self) -> Vec<SkippedControl> {
        let mut out: Vec<SkippedControl> = self.active.values().cloned().collect();
        out.sort_by(|a, b| a.control_id.cmp(&b.control_id));
        out
    }

    /// Forget everything. Called on profile deactivation so the next activation
    /// tells its own story from scratch — a control that is still unresolvable
    /// under the new profile logs once more, which is what an operator who just
    /// changed profiles needs to see.
    pub fn clear(&mut self) {
        self.streak.clear();
        self.active.clear();
    }
}

impl Default for SkippedControlTracker {
    fn default() -> Self {
        Self::new(SKIP_DEBOUNCE_TICKS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip(id: &str, reason: SkipReason) -> SkipRecord {
        SkipRecord {
            control_id: id.to_string(),
            control_name: format!("{id} fans"),
            reason,
        }
    }

    /// The debounce is the whole reason this is not edge-triggered: a sensor on
    /// the freshness boundary flaps, and a per-tick log at 1 Hz is what DEC-193
    /// exists to prevent.
    #[test]
    fn a_control_is_not_listed_before_the_debounce_is_satisfied() {
        let mut t = SkippedControlTracker::new(3);
        let now = Instant::now();
        let one = skip("ctl", SkipReason::MixUnresolvable);

        assert!(t.record_tick(std::slice::from_ref(&one), now).is_empty());
        assert!(t.snapshot().is_empty(), "not listed after 1 tick");
        assert!(t.record_tick(std::slice::from_ref(&one), now).is_empty());
        assert!(t.snapshot().is_empty(), "not listed after 2 ticks");

        let events = t.record_tick(std::slice::from_ref(&one), now);
        assert_eq!(
            events,
            vec![SkipEvent::Skipped {
                id: "ctl".into(),
                name: "ctl fans".into(),
                reason: SkipReason::MixUnresolvable,
            }]
        );
        assert_eq!(t.snapshot().len(), 1, "listed on the 3rd consecutive tick");
    }

    #[test]
    fn a_flapping_control_never_reaches_the_journal() {
        let mut t = SkippedControlTracker::new(3);
        let now = Instant::now();
        let one = skip("ctl", SkipReason::SensorUnavailable);

        // Skip, resolve, skip, resolve … the streak resets each time.
        for _ in 0..10 {
            assert!(t.record_tick(std::slice::from_ref(&one), now).is_empty());
            assert!(t.record_tick(&[], now).is_empty());
        }
        assert!(
            t.snapshot().is_empty(),
            "a control that resolves every other tick must never be listed"
        );
    }

    #[test]
    fn a_listed_control_logs_once_not_every_tick() {
        let mut t = SkippedControlTracker::new(2);
        let now = Instant::now();
        let one = skip("ctl", SkipReason::CurveNotFound);

        t.record_tick(std::slice::from_ref(&one), now);
        let entry_events = t.record_tick(std::slice::from_ref(&one), now);
        assert_eq!(entry_events.len(), 1, "one line on entry");

        for _ in 0..100 {
            assert!(
                t.record_tick(std::slice::from_ref(&one), now).is_empty(),
                "a continuing skip must not re-log"
            );
        }
        assert_eq!(t.snapshot().len(), 1, "and must stay listed throughout");
    }

    #[test]
    fn resolving_emits_one_resumed_event_and_delists() {
        let mut t = SkippedControlTracker::new(1);
        let now = Instant::now();
        let one = skip("ctl", SkipReason::SyncUnresolvable);

        t.record_tick(std::slice::from_ref(&one), now);
        assert_eq!(t.snapshot().len(), 1);

        let events = t.record_tick(&[], now);
        assert_eq!(
            events,
            vec![SkipEvent::Resumed {
                id: "ctl".into(),
                name: "ctl fans".into(),
            }]
        );
        assert!(t.snapshot().is_empty());
        assert!(
            t.record_tick(&[], now).is_empty(),
            "resolving twice must not emit twice"
        );
    }

    /// `since` must be stamped when the control is LISTED, not re-stamped every
    /// tick — otherwise `skipped_for_ms` on `/status` would sit at ~0 forever
    /// and the field would be useless for telling a new problem from an old one.
    #[test]
    fn since_is_stamped_on_entry_and_does_not_advance() {
        let mut t = SkippedControlTracker::new(1);
        let entry = Instant::now();
        let later = entry + std::time::Duration::from_secs(30);
        let one = skip("ctl", SkipReason::MixUnresolvable);

        t.record_tick(std::slice::from_ref(&one), entry);
        let stamped = t.snapshot()[0].since;
        assert_eq!(stamped, entry);

        t.record_tick(std::slice::from_ref(&one), later);
        assert_eq!(
            t.snapshot()[0].since,
            entry,
            "since must stay at the moment the skip was first listed"
        );
    }

    /// The reason is set once, so one continuous outage tells one story even if
    /// the underlying cause shifts (e.g. a sensor vanishing turns a Mix from
    /// partially-resolvable into unresolvable).
    #[test]
    fn the_reason_is_stable_for_the_lifetime_of_one_skip() {
        let mut t = SkippedControlTracker::new(1);
        let now = Instant::now();

        t.record_tick(&[skip("ctl", SkipReason::SensorUnavailable)], now);
        let events = t.record_tick(&[skip("ctl", SkipReason::MixUnresolvable)], now);

        assert!(events.is_empty(), "a changed cause must not re-log");
        assert_eq!(t.snapshot()[0].reason, SkipReason::SensorUnavailable);
    }

    #[test]
    fn snapshot_is_sorted_by_control_id() {
        let mut t = SkippedControlTracker::new(1);
        let now = Instant::now();
        t.record_tick(
            &[
                skip("zulu", SkipReason::CurveNotFound),
                skip("alpha", SkipReason::CurveNotFound),
                skip("mike", SkipReason::CurveNotFound),
            ],
            now,
        );
        let ids: Vec<String> = t.snapshot().into_iter().map(|s| s.control_id).collect();
        assert_eq!(ids, vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    fn clear_forgets_everything_so_a_reactivation_re_logs() {
        let mut t = SkippedControlTracker::new(1);
        let now = Instant::now();
        let one = skip("ctl", SkipReason::CurveNotFound);

        t.record_tick(std::slice::from_ref(&one), now);
        assert_eq!(t.snapshot().len(), 1);

        t.clear();
        assert!(t.snapshot().is_empty());
        // No Resumed event for the cleared entry — a deactivation is not a
        // resolution, and claiming one would be a lie in the journal.
        let events = t.record_tick(std::slice::from_ref(&one), now);
        assert_eq!(
            events,
            vec![SkipEvent::Skipped {
                id: "ctl".into(),
                name: "ctl fans".into(),
                reason: SkipReason::CurveNotFound,
            }],
            "after a clear the control must be reported afresh"
        );
    }

    #[test]
    fn every_reason_has_a_distinct_token_and_description() {
        let all = [
            SkipReason::CurveNotFound,
            SkipReason::SensorUnavailable,
            SkipReason::MixUnresolvable,
            SkipReason::SyncUnresolvable,
            SkipReason::BackendUnavailable,
        ];
        let tokens: std::collections::HashSet<&str> = all.iter().map(|r| r.as_token()).collect();
        assert_eq!(tokens.len(), all.len(), "wire tokens must be distinct");
        let descriptions: std::collections::HashSet<&str> =
            all.iter().map(|r| r.describe()).collect();
        assert_eq!(descriptions.len(), all.len(), "log text must be distinct");
        for r in all {
            assert!(
                r.as_token()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "wire tokens are snake_case: {}",
                r.as_token()
            );
        }
    }

    // ── `OFN-j`: deliverability ───────────────────────────────────────────────

    fn control_with(sources: &[&str]) -> crate::profile::LogicalControl {
        crate::profile::LogicalControl {
            id: "ctl".into(),
            name: "Ctl".into(),
            mode: "curve".into(),
            curve_id: "c".into(),
            manual_output_pct: 0.0,
            members: sources
                .iter()
                .enumerate()
                .map(|(i, src)| crate::profile::ControlMember {
                    source: (*src).to_string(),
                    member_id: format!("{src}:{i}"),
                    member_label: String::new(),
                    fan_zero_rpm: false,
                })
                .collect(),
            step_up_pct: 100.0,
            step_down_pct: 100.0,
            offset_pct: 0.0,
            minimum_pct: 0.0,
            start_pct: 0.0,
            stop_pct: 0.0,
        }
    }

    #[test]
    fn an_openfan_only_control_is_undeliverable_without_a_controller() {
        // The DISCRIMINATING arm (DEC-340): this is the case only the new lookup
        // can report. Asserted against BOTH backend states in one loop, because
        // the `false` arm alone would pass against a classifier that always says
        // `None`, and the `true` arm alone against one that always says `All`.
        for openfan in [true, false] {
            let got = control_deliverability(
                &control_with(&["openfan"]),
                DeliveryTargets::backends(openfan, true),
            );
            let want = if openfan {
                Deliverability::All
            } else {
                Deliverability::None
            };
            assert_eq!(
                got, want,
                "openfan-only control with openfan_available={openfan}"
            );
        }
    }

    #[test]
    fn the_rule_is_not_openfan_specific() {
        // `backend_unavailable` means what its name says. An hwmon member on a
        // board with no writable header is the same defect, and gating this on
        // OpenFan alone would be the two-shapes-for-one-flag trap (DEC-334).
        assert_eq!(
            control_deliverability(
                &control_with(&["hwmon"]),
                DeliveryTargets::backends(true, false)
            ),
            Deliverability::None
        );
        // GPU is never a backend that can be absent here — `gpu_be` is a value,
        // not an `Option` — so a GPU-only control is always deliverable.
        assert_eq!(
            control_deliverability(
                &control_with(&["amd_gpu"]),
                DeliveryTargets::backends(false, false)
            ),
            Deliverability::All
        );
    }

    #[test]
    fn a_control_with_one_live_member_is_partial_not_none() {
        // The rule that keeps `/status` honest: this control IS commanding its
        // hwmon fan, so it must never be reported as uncommanded. Without this
        // arm, an `any`-shaped predicate would pass every other test here.
        assert_eq!(
            control_deliverability(
                &control_with(&["openfan", "hwmon"]),
                DeliveryTargets::backends(false, true)
            ),
            Deliverability::Partial
        );
        // ...and with neither backend the same control IS fully undeliverable,
        // which is what proves the `Partial` answer came from the member split
        // rather than from the member count.
        assert_eq!(
            control_deliverability(
                &control_with(&["openfan", "hwmon"]),
                DeliveryTargets::backends(false, false)
            ),
            Deliverability::None
        );
    }

    /// `OFN-al`: on a MIXED board the hwmon backend exists, so a per-backend
    /// answer called every `hwmon:` member deliverable. Per member, one bound
    /// only to headers outside the writable set is `None`. The discriminating
    /// arm (DEC-340) is the first: only the new lookup can produce it, since
    /// the backend is present. The others pin the rest of the split, so a
    /// classifier stuck at any one answer fails.
    #[test]
    fn hwmon_members_are_resolved_against_the_writable_set() {
        let writable: HashSet<String> = ["hwmon:1".to_string()].into();
        let mixed = DeliveryTargets {
            openfan: false,
            hwmon: HwmonTargets::Writable(&writable),
        };
        // `control_with` names members `{source}:{index}`.
        assert_eq!(
            control_deliverability(&control_with(&["hwmon"]), mixed),
            Deliverability::None,
            "a control bound only to a header the daemon cannot write"
        );
        assert_eq!(
            control_deliverability(&control_with(&["amd_gpu", "hwmon"]), mixed),
            Deliverability::All,
            "the writable header is deliverable"
        );
        assert_eq!(
            control_deliverability(&control_with(&["hwmon", "hwmon"]), mixed),
            Deliverability::Partial,
            "one writable member and one not"
        );
        // An unmeasured set keeps the per-backend answer: never hide a control
        // the daemon may well be driving.
        let unmeasured = DeliveryTargets {
            openfan: false,
            hwmon: HwmonTargets::Unmeasured,
        };
        assert_eq!(
            control_deliverability(&control_with(&["hwmon"]), unmeasured),
            Deliverability::All
        );
    }

    #[test]
    fn a_member_less_control_is_not_a_fault() {
        // It still publishes a tick output for any Sync that mirrors it
        // (DEC-151), so it must not be reported as uncommanded.
        assert_eq!(
            control_deliverability(&control_with(&[]), DeliveryTargets::backends(false, false)),
            Deliverability::NoMembers
        );
    }
}
