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

use std::collections::HashMap;
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
}
