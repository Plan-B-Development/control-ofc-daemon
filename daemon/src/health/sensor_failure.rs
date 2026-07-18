//! Per-sensor read-failure tracking and quarantine (DEC-193).
//!
//! The hwmon poll loop reads a cached descriptor set every tick (DEC-133). A
//! descriptor that is still *present* in sysfs but whose `temp*_input` read
//! fails persistently — the canonical case is an `ath12k` WiFi-radio
//! temperature returning `ENETDOWN` while the radio is soft-blocked — used to
//! produce two unbounded journal streams:
//!
//! 1. a per-tick `WARN Failed to read sensor …` (1 Hz), and
//! 2. a `WARN Re-discovering sensors after persistent read failures …` every
//!    `threshold` ticks, because the read-failure→re-discovery recovery
//!    (designed for a device *unbound* mid-session) re-finds the still-present
//!    descriptor and rebuilds the streak forever.
//!
//! This tracker collapses both into a bounded, two-line story per sensor:
//!
//! - **Heal → fail:** the streak climbs to `threshold`. The loop earns exactly
//!   one re-discovery (the legitimate "did the device actually unbind?" probe).
//! - **Still failing after that probe:** the sensor is *quarantined* — logged
//!   once, excluded from further re-discovery triggers and per-tick logging,
//!   and surfaced as an [`UnavailableSensor`] for display.
//! - **Reads succeed again:** the sensor is un-quarantined (logged once) and
//!   re-enters normal service.
//! - **Descriptor genuinely vanishes** (unbound, not merely unreadable): it is
//!   reconciled out of the tracker entirely, so it leaves the unavailable list
//!   — it is *absent*, not *present-but-unreadable*.
//!
//! The tracker holds no I/O and no clock of its own (the caller passes `now`),
//! so it is a pure, deterministic state machine that is unit-tested directly.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::health::state::UnavailableSensor;
use crate::hwmon::types::SensorDescriptor;
use crate::hwmon::SensorReadFailure;

/// A logged transition produced by [`SensorFailureTracker::record_tick`]. Each
/// is emitted at most once per transition so the journal sees a bounded story.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerEvent {
    /// The sensor crossed the failure threshold and still failed after its one
    /// re-discovery probe → suppress further per-tick logging until recovery.
    Quarantined { id: String, reason: String },
    /// A previously-quarantined sensor read successfully again.
    Recovered { id: String },
}

/// Tracks per-descriptor read-failure streaks and the quarantine set.
#[derive(Debug)]
pub struct SensorFailureTracker {
    /// Consecutive failed reads before a sensor earns its one re-discovery and
    /// (if it then keeps failing) is quarantined.
    threshold: u32,
    /// Consecutive failure count per non-quarantined descriptor id.
    streak: HashMap<String, u32>,
    /// Currently-quarantined sensors, keyed by id. `since`/`reason` are stable
    /// for the lifetime of a quarantine (set once, on entry).
    quarantined: HashMap<String, UnavailableSensor>,
}

impl SensorFailureTracker {
    /// Create a tracker that quarantines a still-failing sensor after
    /// `threshold` failures plus one re-discovery probe.
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            streak: HashMap::new(),
            quarantined: HashMap::new(),
        }
    }

    /// True when some present, not-yet-quarantined descriptor has reached the
    /// failure threshold — the poll loop should re-run discovery once. A
    /// quarantined sensor never re-triggers (that is what stops the 1/`threshold`
    /// re-discovery spam).
    pub fn wants_rediscovery(&self) -> bool {
        self.streak
            .iter()
            .any(|(id, &n)| n >= self.threshold && !self.quarantined.contains_key(id))
    }

    /// Sorted ids that have hit the threshold but are not yet quarantined — for
    /// the one-time "re-discovering after persistent read failures" log line.
    pub fn rediscovery_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .streak
            .iter()
            .filter(|(id, &n)| n >= self.threshold && !self.quarantined.contains_key(*id))
            .map(|(id, _)| id.as_str())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Record one tick's read outcome against the active descriptor set.
    ///
    /// `descriptors` is whatever set was read this tick (the cached set, or a
    /// freshly-discovered one). `failures` are the per-sensor read failures from
    /// that read. Returns the (at-most-once) transition events to log.
    ///
    /// Ordering within a tick: a descriptor absent from `descriptors` is treated
    /// as gone (reconciled out); a present descriptor either succeeded (cleared /
    /// recovered) or failed (streak advanced, possibly quarantined).
    pub fn record_tick(
        &mut self,
        descriptors: &[SensorDescriptor],
        failures: &[SensorReadFailure],
        now: Instant,
    ) -> Vec<TrackerEvent> {
        let present: HashSet<&str> = descriptors.iter().map(|d| d.id.as_str()).collect();

        // Reconcile: a descriptor no longer present is genuinely gone (unbound),
        // not merely unreadable — drop all tracking so it leaves the unavailable
        // list. This is what preserves the DEC-133 "device unbound mid-session"
        // behaviour: re-discovery drops it and the tracker forgets it.
        self.streak.retain(|id, _| present.contains(id.as_str()));
        self.quarantined
            .retain(|id, _| present.contains(id.as_str()));

        let failing: HashMap<&str, &SensorReadFailure> =
            failures.iter().map(|f| (f.id.as_str(), f)).collect();

        let mut events = Vec::new();
        for d in descriptors {
            let id = d.id.as_str();
            match failing.get(id) {
                Some(failure) => {
                    // Already quarantined → stay suppressed (no log, stable
                    // `since`/`reason`, no streak growth).
                    if self.quarantined.contains_key(id) {
                        continue;
                    }
                    let n = self.streak.entry(d.id.clone()).or_insert(0);
                    *n += 1;
                    // Crossed the threshold (so it already earned its one
                    // re-discovery) and failed again → quarantine, once.
                    if *n > self.threshold {
                        self.quarantined.insert(
                            d.id.clone(),
                            UnavailableSensor {
                                id: d.id.clone(),
                                label: failure.label.clone(),
                                reason: failure.reason.clone(),
                                since: now,
                            },
                        );
                        events.push(TrackerEvent::Quarantined {
                            id: d.id.clone(),
                            reason: failure.reason.clone(),
                        });
                    }
                }
                None => {
                    // Read succeeded this tick.
                    self.streak.remove(id);
                    if self.quarantined.remove(id).is_some() {
                        events.push(TrackerEvent::Recovered { id: d.id.clone() });
                    }
                }
            }
        }
        events
    }

    /// Snapshot of currently-unavailable sensors for the cache, sorted by id.
    pub fn unavailable(&self) -> Vec<UnavailableSensor> {
        let mut v: Vec<UnavailableSensor> = self.quarantined.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::types::{SensorKind, SensorSource};

    const THRESHOLD: u32 = 5;

    fn descriptor(id: &str) -> SensorDescriptor {
        SensorDescriptor {
            id: id.to_string(),
            kind: SensorKind::MbTemp,
            label: format!("{id}-label"),
            source: SensorSource::Hwmon,
            input_path: format!("/sys/class/hwmon/hwmon6/{id}_input"),
            chip_name: "ath12k_hwmon".to_string(),
            temp_type: None,
            thresholds: None,
        }
    }

    fn failure(id: &str) -> SensorReadFailure {
        SensorReadFailure {
            id: id.to_string(),
            label: format!("{id}-label"),
            reason: format!("read error: /sys/.../{id}_input: Network is down (os error 100)"),
        }
    }

    /// Drive `ticks` consecutive all-failing reads of one present descriptor and
    /// collect every event + every `wants_rediscovery()` observation, exactly as
    /// the poll loop would (decision is read *before* the post-read record).
    fn run_failing(tracker: &mut SensorFailureTracker, id: &str, ticks: usize) -> RunLog {
        let descs = [descriptor(id)];
        let fails = [failure(id)];
        let now = Instant::now();
        let mut log = RunLog::default();
        for _ in 0..ticks {
            if tracker.wants_rediscovery() {
                log.rediscovery_requests += 1;
            }
            for ev in tracker.record_tick(&descs, &fails, now) {
                match ev {
                    TrackerEvent::Quarantined { .. } => log.quarantine_events += 1,
                    TrackerEvent::Recovered { .. } => log.recovered_events += 1,
                }
            }
        }
        log
    }

    #[derive(Default)]
    struct RunLog {
        rediscovery_requests: usize,
        quarantine_events: usize,
        recovered_events: usize,
    }

    /// The core regression test: a sensor that is present every tick but always
    /// fails to read must request **exactly one** re-discovery and quarantine
    /// **exactly once**, no matter how many ticks elapse — never the 1-Hz /
    /// 1-per-`threshold` spam the bug produced.
    #[test]
    fn persistent_present_unreadable_sensor_logs_a_bounded_story() {
        let mut tracker = SensorFailureTracker::new(THRESHOLD);
        let log = run_failing(&mut tracker, "wifi", 1000);

        assert_eq!(
            log.rediscovery_requests, 1,
            "a still-present unreadable sensor must trigger re-discovery only once, \
             not once per {THRESHOLD} ticks"
        );
        assert_eq!(
            log.quarantine_events, 1,
            "quarantine must be logged exactly once, not every tick"
        );
        assert_eq!(log.recovered_events, 0);
        // It is surfaced as unavailable with its cause.
        let unavailable = tracker.unavailable();
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].id, "wifi");
        assert!(unavailable[0].reason.contains("Network is down"));
    }

    /// Re-discovery fires the tick *after* the streak reaches the threshold, and
    /// quarantine happens on the failing read that follows it.
    #[test]
    fn rediscovery_then_quarantine_timing() {
        let mut tracker = SensorFailureTracker::new(THRESHOLD);
        let descs = [descriptor("wifi")];
        let fails = [failure("wifi")];
        let now = Instant::now();

        // Ticks 1..=THRESHOLD: no re-discovery yet, streak climbing.
        for _ in 0..THRESHOLD {
            assert!(!tracker.wants_rediscovery());
            assert!(tracker.record_tick(&descs, &fails, now).is_empty());
        }
        // Streak == THRESHOLD → the next tick wants exactly one re-discovery.
        assert!(tracker.wants_rediscovery());
        assert_eq!(tracker.rediscovery_ids(), vec!["wifi"]);
        // That tick's read still fails → quarantine.
        let events = tracker.record_tick(&descs, &fails, now);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TrackerEvent::Quarantined { .. }));
        // Now quarantined → never wants re-discovery again.
        assert!(!tracker.wants_rediscovery());
    }

    /// A sensor that recovers (reads successfully) after quarantine emits a
    /// single Recovered event and leaves the unavailable list.
    #[test]
    fn recovery_unquarantines_once() {
        let mut tracker = SensorFailureTracker::new(THRESHOLD);
        run_failing(&mut tracker, "wifi", 20);
        assert_eq!(tracker.unavailable().len(), 1);

        // WiFi comes back: a read with no failures for the present descriptor.
        let descs = [descriptor("wifi")];
        let events = tracker.record_tick(&descs, &[], Instant::now());
        assert_eq!(events, vec![TrackerEvent::Recovered { id: "wifi".into() }]);
        assert!(tracker.unavailable().is_empty());
        assert!(!tracker.wants_rediscovery());

        // A subsequent success is a no-op (no duplicate Recovered).
        assert!(tracker.record_tick(&descs, &[], Instant::now()).is_empty());
    }

    /// A descriptor that genuinely disappears (unbound) before quarantine is
    /// reconciled away — it must NOT be quarantined (it is absent, not
    /// present-but-unreadable), preserving the DEC-133 unbind behaviour.
    #[test]
    fn vanished_descriptor_is_reconciled_not_quarantined() {
        let mut tracker = SensorFailureTracker::new(THRESHOLD);
        let descs = [descriptor("wifi")];
        let fails = [failure("wifi")];
        let now = Instant::now();
        // Fail right up to the threshold (about to earn re-discovery).
        for _ in 0..THRESHOLD {
            tracker.record_tick(&descs, &fails, now);
        }
        assert!(tracker.wants_rediscovery());

        // Re-discovery runs and the device is gone: the active set no longer
        // contains it. Reconciliation drops it; nothing is quarantined.
        let events = tracker.record_tick(&[], &[], now);
        assert!(events.is_empty());
        assert!(tracker.unavailable().is_empty());
        assert!(!tracker.wants_rediscovery());
    }

    /// B6: a sensor that is quarantined and THEN vanishes (device unbound) must be
    /// reconciled out of `unavailable()` — exercising `quarantined.retain(...)`,
    /// the branch the pre-quarantine vanish test never reaches. Deleting that
    /// retain would strand the sensor in `unavailable()` forever (re-opens DEC-193).
    #[test]
    fn quarantined_descriptor_that_vanishes_is_reconciled_out_silently() {
        let mut tracker = SensorFailureTracker::new(THRESHOLD);
        // Run well past quarantine: now quarantined and surfaced as unavailable.
        run_failing(&mut tracker, "wifi", 20);
        assert_eq!(tracker.unavailable().len(), 1);

        // The descriptor now vanishes entirely (empty present set) — the
        // `quarantined.retain(...)` reconcile branch, not a recovery.
        let events = tracker.record_tick(&[], &[], Instant::now());

        assert!(
            events.is_empty(),
            "a vanished quarantined sensor emits no Recovered event"
        );
        assert!(
            tracker.unavailable().is_empty(),
            "quarantined.retain must drop the absent id"
        );
        assert!(!tracker.wants_rediscovery());
    }

    /// A transient blip (a few failures, then success) never quarantines and
    /// never asks for re-discovery.
    #[test]
    fn transient_failures_below_threshold_are_silent() {
        let mut tracker = SensorFailureTracker::new(THRESHOLD);
        let descs = [descriptor("nvme")];
        let fails = [failure("nvme")];
        let now = Instant::now();

        for _ in 0..(THRESHOLD - 1) {
            assert!(tracker.record_tick(&descs, &fails, now).is_empty());
            assert!(!tracker.wants_rediscovery());
        }
        // Recovers before the threshold.
        assert!(tracker.record_tick(&descs, &[], now).is_empty());
        assert!(tracker.unavailable().is_empty());
        assert!(!tracker.wants_rediscovery());
    }

    /// Two independent failing sensors are tracked and quarantined separately;
    /// `rediscovery_ids` reports both, sorted.
    #[test]
    fn multiple_failing_sensors_tracked_independently() {
        let mut tracker = SensorFailureTracker::new(THRESHOLD);
        let descs = [descriptor("wifi"), descriptor("probe")];
        let fails = [failure("wifi"), failure("probe")];
        let now = Instant::now();

        for _ in 0..THRESHOLD {
            tracker.record_tick(&descs, &fails, now);
        }
        assert_eq!(tracker.rediscovery_ids(), vec!["probe", "wifi"]);

        let events = tracker.record_tick(&descs, &fails, now);
        assert_eq!(events.len(), 2, "both quarantine on the same tick");
        assert_eq!(tracker.unavailable().len(), 2);
    }
}
