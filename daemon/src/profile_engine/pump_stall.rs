//! Pump stall response (DEC-443, `TS-e`).
//!
//! Before this, a pump reading 0 RPM while commanded to turn raised a GUI
//! alert (`stall_detected` on `/fans`) and nothing else: no daemon path did
//! anything about it. This module is the detector and the response, as a pure,
//! clock-injected state machine the engine loop feeds once per tick.
//!
//! # What is watched (the user's Q8 and Q9)
//!
//! The active profile's **hwmon pump members only** — a member the daemon's
//! pump-protection union names a pump (`tuning::member_pump_floor`) — and, of
//! those, only a header that has **read above 0 RPM during this daemon run**.
//! The second rule keeps an empty `PUMP` header, or a tach wired to another
//! fan index, from being "rescued" forever; the cost is that a pump already
//! dead at boot is left to the CPU ladder and the GUI's existing stall alert.
//!
//! # What counts as a stall sample
//!
//! A FRESH tach reading of 0 while the duty the write path last commanded for
//! that header (`HwmonFanState::pwm_commanded_pct`, one producer) is at or above
//! the 30 % pump floor. The window is wall time from the first fresh zero,
//! closed by a fresh zero at least [`constants::PUMP_STALL_DETECT`] later with
//! every tick in between also a fresh zero. A fresh non-zero, a stale tach and
//! a diagnostic's write pause each discard the window, so two zeros with
//! nothing observed between them never confirm a stall. While the write pause
//! is held the response phases do not advance either.
//!
//! # The response (the user's Q5, Q6, Q7)
//!
//! | phase | commands | leaves when |
//! | --- | --- | --- |
//! | monitoring | the curve | a stall is confirmed → `stall_response` (first) or `held` (second this run) |
//! | `stall_response` | 100 % | after [`constants::PUMP_STALL_KICK`]: turning → monitoring; still 0 → `not_turning` |
//! | `not_turning` | 100 % | a fresh non-zero reading → monitoring |
//! | `held` | 100 % | a profile change or a restart — nothing else |
//!
//! Raise-only: the engine applies [`PumpStallWatch::raised_headers`] as
//! `max(command, 100)`, after evaluation and under the thermal force, so the
//! response can never lower a pump. A `held` pump reading 0 is published as
//! `not_turning`, so a client's error alert tracks the tach, not the latch.

use std::collections::HashMap;
use std::time::Instant;

use crate::constants;
use crate::health::state::PumpStallRecord;

/// One pump member's tach this tick, as the engine read it from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tach {
    /// Updated within the freshness budget.
    Fresh(u16),
    /// Stale, or no reading at all: evidence of nothing.
    Stale,
}

/// One watched header's input for a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PumpInput {
    pub(crate) header_id: String,
    pub(crate) tach: Tach,
    /// `HwmonFanState::pwm_commanded_pct`: `None` when the daemon has never
    /// commanded this header, which is never a stall sample.
    pub(crate) commanded_pct: Option<u8>,
}

/// A transition worth one log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StallEvent {
    /// A stall was confirmed; `held` when it is the second this run.
    Stalled { header_id: String, held: bool },
    /// Still 0 RPM after the response window at 100 %.
    NotTurning { header_id: String },
    /// Turning again; the pump returns to its curve.
    Recovered { header_id: String },
    /// A `held` pump released by a profile change.
    Released { header_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Monitoring { zero_since: Option<Instant> },
    Kick { since: Instant, turning: bool },
    NotTurning { since: Instant },
    Held { since: Instant, turning: bool },
}

#[derive(Debug, Clone, Copy)]
struct HeaderWatch {
    seen_spinning: bool,
    stalls: u32,
    phase: Phase,
}

impl Default for HeaderWatch {
    fn default() -> Self {
        Self {
            seen_spinning: false,
            stalls: 0,
            phase: Phase::Monitoring { zero_since: None },
        }
    }
}

/// The per-header stall watch (DEC-443). Owned by the engine loop.
#[derive(Debug, Default)]
pub(crate) struct PumpStallWatch {
    headers: HashMap<String, HeaderWatch>,
}

impl PumpStallWatch {
    /// Advance every watched header by one tick.
    ///
    /// `inputs` are this tick's watched headers — the active profile's hwmon
    /// pump members. A header absent from them is no longer a pump member (or
    /// no profile is active): it returns to monitoring, keeping what it has
    /// learnt (`seen_spinning`, its stall count), and is no longer raised.
    /// `paused` (a diagnostic holds the write pause) freezes every response
    /// phase and discards any detect window that was building: a stall is
    /// confirmed only from fresh zeros observed without a gap, so a window may
    /// not span a pause.
    pub(crate) fn tick(
        &mut self,
        inputs: &[PumpInput],
        now: Instant,
        paused: bool,
    ) -> Vec<StallEvent> {
        let mut events = Vec::new();
        for (id, w) in self.headers.iter_mut() {
            let watched = inputs.iter().any(|i| &i.header_id == id);
            if !watched || (paused && matches!(w.phase, Phase::Monitoring { .. })) {
                w.phase = Phase::Monitoring { zero_since: None };
            }
        }
        if paused {
            return events;
        }
        let pump_floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
        for input in inputs {
            let w = self.headers.entry(input.header_id.clone()).or_default();
            if let Tach::Fresh(rpm) = input.tach {
                if rpm > 0 {
                    w.seen_spinning = true;
                }
            }
            let id = || input.header_id.clone();
            w.phase = match (w.phase, input.tach) {
                (Phase::Monitoring { .. }, Tach::Fresh(rpm)) if rpm > 0 => {
                    Phase::Monitoring { zero_since: None }
                }
                (Phase::Monitoring { zero_since }, Tach::Fresh(_)) => {
                    let commanded = input.commanded_pct.is_some_and(|p| p >= pump_floor);
                    if !(w.seen_spinning && commanded) {
                        Phase::Monitoring { zero_since: None }
                    } else {
                        let since = zero_since.unwrap_or(now);
                        if now.saturating_duration_since(since) >= constants::PUMP_STALL_DETECT {
                            w.stalls += 1;
                            let held = w.stalls >= 2;
                            events.push(StallEvent::Stalled {
                                header_id: id(),
                                held,
                            });
                            if held {
                                Phase::Held {
                                    since: now,
                                    turning: false,
                                }
                            } else {
                                Phase::Kick {
                                    since: now,
                                    turning: false,
                                }
                            }
                        } else {
                            Phase::Monitoring {
                                zero_since: Some(since),
                            }
                        }
                    }
                }
                // A stale tach is evidence of nothing, so it cannot carry a
                // window across the gap: the next fresh zero starts a new one.
                (Phase::Monitoring { .. }, Tach::Stale) => Phase::Monitoring { zero_since: None },
                (Phase::Kick { since, turning }, tach) => {
                    let turning = match tach {
                        Tach::Fresh(rpm) => rpm > 0,
                        Tach::Stale => turning,
                    };
                    if now.saturating_duration_since(since) < constants::PUMP_STALL_KICK {
                        Phase::Kick { since, turning }
                    } else if turning {
                        events.push(StallEvent::Recovered { header_id: id() });
                        Phase::Monitoring { zero_since: None }
                    } else {
                        events.push(StallEvent::NotTurning { header_id: id() });
                        Phase::NotTurning { since: now }
                    }
                }
                (Phase::NotTurning { .. }, Tach::Fresh(rpm)) if rpm > 0 => {
                    events.push(StallEvent::Recovered { header_id: id() });
                    Phase::Monitoring { zero_since: None }
                }
                (n @ Phase::NotTurning { .. }, _) => n,
                (Phase::Held { since, turning }, tach) => Phase::Held {
                    since,
                    turning: match tach {
                        Tach::Fresh(rpm) => rpm > 0,
                        Tach::Stale => turning,
                    },
                },
            };
        }
        events
    }

    /// Release every `held` pump (a profile change — the user's Q6). A pump in
    /// its response window or not turning keeps going: a profile change does
    /// not fix a pump, and those phases end on their own evidence.
    pub(crate) fn release_holds(&mut self) -> Vec<StallEvent> {
        let mut events = Vec::new();
        for (id, w) in self.headers.iter_mut() {
            if matches!(w.phase, Phase::Held { .. }) {
                w.phase = Phase::Monitoring { zero_since: None };
                events.push(StallEvent::Released {
                    header_id: id.clone(),
                });
            }
        }
        events
    }

    /// Headers the engine must command at [`constants::PUMP_STALL_RESPONSE_PCT`]
    /// this tick. Sorted, so the command order is deterministic.
    pub(crate) fn raised_headers(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .headers
            .iter()
            .filter(|(_, w)| !matches!(w.phase, Phase::Monitoring { .. }))
            .map(|(id, _)| id.as_str())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The `/status` records, sorted by header id.
    pub(crate) fn snapshot(&self) -> Vec<PumpStallRecord> {
        let mut out: Vec<PumpStallRecord> = self
            .headers
            .iter()
            .filter_map(|(id, w)| {
                let (state, since) = match w.phase {
                    Phase::Monitoring { .. } => return None,
                    Phase::Kick { since, .. } => ("stall_response", since),
                    Phase::NotTurning { since } => ("not_turning", since),
                    Phase::Held {
                        since,
                        turning: false,
                    } => ("not_turning", since),
                    Phase::Held {
                        since,
                        turning: true,
                    } => ("held", since),
                };
                Some(PumpStallRecord {
                    header_id: id.clone(),
                    state,
                    since,
                    stall_count: w.stalls,
                })
            })
            .collect();
        out.sort_by(|a, b| a.header_id.cmp(&b.header_id));
        out
    }
}

/// Raise every stalled pump's command to the response duty (DEC-443).
///
/// [SAFETY] Raise-only: an existing command becomes `max(command, 100)`; a
/// raised header with no command this tick — its control is skipped, so it
/// holds its last duty (`TS-p`) — gets one, which is also a raise. Applied at
/// the engine's call site after evaluation and never inside
/// `evaluate_profile`, whose 3-arg form is the parity oracle's surface.
pub(crate) fn apply_stall_response(commands: &mut Vec<super::PwmCommand>, raised: &[&str]) {
    let duty = constants::PUMP_STALL_RESPONSE_PCT;
    for id in raised {
        let mut found = false;
        for cmd in commands
            .iter_mut()
            .filter(|c| c.source == "hwmon" && c.member_id == *id)
        {
            cmd.pwm_percent = cmd.pwm_percent.max(duty);
            found = true;
        }
        if !found {
            commands.push(super::PwmCommand {
                member_id: (*id).to_string(),
                source: "hwmon".into(),
                pwm_percent: duty,
                gpu_fan_zero_rpm: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const ID: &str = "hwmon:nct6798:nct6775.2592:pwm2:PUMP";
    const FLOOR: u8 = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;

    fn input(tach: Tach, commanded: Option<u8>) -> Vec<PumpInput> {
        vec![PumpInput {
            header_id: ID.into(),
            tach,
            commanded_pct: commanded,
        }]
    }

    /// Drive `watch` one tick per second from `t0 + start` for `secs` ticks.
    fn run(
        watch: &mut PumpStallWatch,
        t0: Instant,
        start: u64,
        secs: u64,
        tach: Tach,
    ) -> Vec<StallEvent> {
        let mut all = Vec::new();
        for s in start..start + secs {
            all.extend(watch.tick(&input(tach, Some(40)), t0 + Duration::from_secs(s), false));
        }
        all
    }

    fn state(watch: &PumpStallWatch) -> Option<&'static str> {
        watch.snapshot().first().map(|r| r.state)
    }

    fn detect_s() -> u64 {
        constants::PUMP_STALL_DETECT.as_secs()
    }

    fn kick_s() -> u64 {
        constants::PUMP_STALL_KICK.as_secs()
    }

    /// Spin up, then read 0: nothing until the detect window has elapsed, then
    /// the response — and the header is raised exactly while it runs.
    #[test]
    fn a_seen_pump_that_stops_is_answered_after_the_detect_window() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 3, Tach::Fresh(1500));
        // Zero from t=3. At t=3+N-1 the window has not elapsed.
        let early = run(&mut w, t0, 3, detect_s(), Tach::Fresh(0));
        assert!(early.is_empty(), "{early:?}");
        assert!(w.raised_headers().is_empty());
        let fired = run(&mut w, t0, 3 + detect_s(), 1, Tach::Fresh(0));
        assert_eq!(
            fired,
            vec![StallEvent::Stalled {
                header_id: ID.into(),
                held: false
            }]
        );
        assert_eq!(state(&w), Some("stall_response"));
        assert_eq!(w.raised_headers(), vec![ID]);
    }

    /// Q8: a header never seen turning is never answered — an empty PUMP
    /// header, or a tach on another fan index.
    #[test]
    fn a_pump_never_seen_turning_is_left_alone() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        let ev = run(&mut w, t0, 0, detect_s() * 3, Tach::Fresh(0));
        assert!(ev.is_empty());
        assert!(w.raised_headers().is_empty());
    }

    /// A header the daemon has not commanded at the pump floor is not a stall:
    /// a zero there may simply be firmware's choice.
    #[test]
    fn a_zero_below_the_pump_floor_or_uncommanded_is_not_a_stall_sample() {
        let t0 = Instant::now();
        for commanded in [None, Some(FLOOR - 1)] {
            let mut w = PumpStallWatch::default();
            w.tick(&input(Tach::Fresh(1500), commanded), t0, false);
            for s in 1..=detect_s() * 2 {
                w.tick(
                    &input(Tach::Fresh(0), commanded),
                    t0 + Duration::from_secs(s),
                    false,
                );
            }
            assert!(w.raised_headers().is_empty(), "{commanded:?}");
        }
    }

    /// A fresh non-zero reading inside the window resets it.
    #[test]
    fn a_turning_reading_inside_the_window_resets_it() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 1, Tach::Fresh(1500));
        run(&mut w, t0, 1, detect_s() - 1, Tach::Fresh(0));
        run(&mut w, t0, detect_s(), 1, Tach::Fresh(900));
        let ev = run(&mut w, t0, detect_s() + 1, detect_s() - 1, Tach::Fresh(0));
        assert!(
            ev.is_empty(),
            "the window must restart after a turning read"
        );
    }

    /// A stale tach cannot confirm a stall, and it discards a window that was
    /// building: two fresh zeros with a blind gap between them are not a stall.
    /// The window then restarts from the next fresh zero and closes normally.
    #[test]
    fn a_stale_tach_discards_the_window_it_interrupts() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 1, Tach::Fresh(1500));
        run(&mut w, t0, 1, 1, Tach::Fresh(0)); // window opens at t=1
        let stale = run(&mut w, t0, 2, detect_s() * 2, Tach::Stale);
        assert!(stale.is_empty(), "stale cannot confirm");
        let after = 2 + detect_s() * 2;
        let first = run(&mut w, t0, after, 1, Tach::Fresh(0));
        assert!(
            first.is_empty(),
            "a zero after a blind gap opens a new window"
        );
        assert!(w.raised_headers().is_empty());
        // Presence: the restarted window does close.
        let fired = run(&mut w, t0, after + 1, detect_s(), Tach::Fresh(0));
        assert_eq!(fired.len(), 1, "{fired:?}");
    }

    /// The concurrency review's P3: a window open when a diagnostic takes the
    /// write pause must not close on the first zero after the pause ends — the
    /// pause is a gap in observation, like a stale tach.
    #[test]
    fn a_detect_window_does_not_span_a_write_pause() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 1, Tach::Fresh(1500));
        run(&mut w, t0, 1, 1, Tach::Fresh(0)); // window opens at t=1
        let pause = detect_s() * 3;
        for s in 2..2 + pause {
            let ev = w.tick(
                &input(Tach::Fresh(0), Some(40)),
                t0 + Duration::from_secs(s),
                true,
            );
            assert!(ev.is_empty());
        }
        let after = 2 + pause;
        let first = run(&mut w, t0, after, 1, Tach::Fresh(0));
        assert!(first.is_empty(), "{first:?}");
        assert!(w.raised_headers().is_empty());
        // Presence: a stall observed after the pause is still answered.
        let fired = run(&mut w, t0, after + 1, detect_s(), Tach::Fresh(0));
        assert_eq!(
            fired,
            vec![StallEvent::Stalled {
                header_id: ID.into(),
                held: false
            }]
        );
    }

    /// Q5/Q6: after the response window a turning pump returns to its curve.
    #[test]
    fn a_pump_that_turns_again_returns_to_its_curve_after_the_window() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 1, Tach::Fresh(1500));
        run(&mut w, t0, 1, detect_s() + 1, Tach::Fresh(0));
        assert_eq!(state(&w), Some("stall_response"));
        let k0 = 2 + detect_s();
        // Turning again early: still held for the whole window.
        run(&mut w, t0, k0, kick_s() - 1, Tach::Fresh(2000));
        assert_eq!(state(&w), Some("stall_response"), "the window runs out");
        let ev = run(&mut w, t0, k0 + kick_s() - 1, 2, Tach::Fresh(2000));
        assert!(ev.contains(&StallEvent::Recovered {
            header_id: ID.into()
        }));
        assert!(w.raised_headers().is_empty());
    }

    /// Q7: still 0 after the window → `not_turning`, held at 100 % until a
    /// turning reading returns it to its curve.
    #[test]
    fn a_pump_still_stopped_after_the_window_is_held_as_not_turning() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 1, Tach::Fresh(1500));
        run(&mut w, t0, 1, detect_s() + 1, Tach::Fresh(0));
        let k0 = 2 + detect_s();
        let ev = run(&mut w, t0, k0, kick_s() + 1, Tach::Fresh(0));
        assert!(ev.contains(&StallEvent::NotTurning {
            header_id: ID.into()
        }));
        assert_eq!(state(&w), Some("not_turning"));
        assert_eq!(w.raised_headers(), vec![ID], "still held at 100 %");
        run(&mut w, t0, k0 + kick_s() + 1, 60, Tach::Fresh(0));
        assert_eq!(state(&w), Some("not_turning"), "no timeout on a dead pump");
        let ev = run(&mut w, t0, k0 + kick_s() + 61, 1, Tach::Fresh(700));
        assert!(ev.contains(&StallEvent::Recovered {
            header_id: ID.into()
        }));
        assert!(w.raised_headers().is_empty());
    }

    /// Q6: a SECOND stall in the run is held at 100 % until a profile change —
    /// turning again does not release it.
    #[test]
    fn a_second_stall_is_held_until_a_profile_change() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 1, Tach::Fresh(1500));
        run(&mut w, t0, 1, detect_s() + 1, Tach::Fresh(0));
        let k0 = 2 + detect_s();
        run(&mut w, t0, k0, kick_s() + 1, Tach::Fresh(1500)); // recovered
        assert!(
            w.raised_headers().is_empty(),
            "precondition: first recovered"
        );
        let s2 = k0 + kick_s() + 1;
        let ev = run(&mut w, t0, s2, detect_s() + 1, Tach::Fresh(0));
        assert!(ev.contains(&StallEvent::Stalled {
            header_id: ID.into(),
            held: true
        }));
        assert_eq!(state(&w), Some("not_turning"), "held and reading 0");
        run(&mut w, t0, s2 + detect_s() + 1, 300, Tach::Fresh(1500));
        assert_eq!(state(&w), Some("held"), "turning does not release a hold");
        assert_eq!(w.raised_headers(), vec![ID]);
        let rel = w.release_holds();
        assert_eq!(rel.len(), 1);
        assert!(
            w.raised_headers().is_empty(),
            "a profile change releases it"
        );
        assert_eq!(w.snapshot().len(), 0);
    }

    /// Q9: a header that stops being a pump member leaves the response.
    #[test]
    fn a_header_leaving_the_profile_is_no_longer_raised() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 1, Tach::Fresh(1500));
        run(&mut w, t0, 1, detect_s() + 1, Tach::Fresh(0));
        assert_eq!(w.raised_headers(), vec![ID]);
        w.tick(&[], t0 + Duration::from_secs(50), false);
        assert!(w.raised_headers().is_empty());
    }

    /// A diagnostic's write pause freezes the watch: no stall is confirmed
    /// under it, and no response window runs out.
    #[test]
    fn the_write_pause_freezes_the_watch() {
        let t0 = Instant::now();
        let mut w = PumpStallWatch::default();
        run(&mut w, t0, 0, 1, Tach::Fresh(1500));
        for s in 1..=detect_s() * 3 {
            let ev = w.tick(
                &input(Tach::Fresh(0), Some(40)),
                t0 + Duration::from_secs(s),
                true,
            );
            assert!(ev.is_empty());
        }
        assert!(w.raised_headers().is_empty());
    }

    /// [SAFETY] Raise-only: an existing command is raised, never lowered; a
    /// missing one (skipped control) is added; other members are untouched.
    #[test]
    fn the_response_only_raises_and_reaches_a_skipped_member() {
        let cmd = |id: &str, pct| super::super::PwmCommand {
            member_id: id.into(),
            source: "hwmon".into(),
            pwm_percent: pct,
            gpu_fan_zero_rpm: false,
        };
        let mut cmds = vec![cmd("a", 40), cmd("b", 55)];
        apply_stall_response(&mut cmds, &["a", "c"]);
        let pct = |id: &str| {
            cmds.iter()
                .find(|c| c.member_id == id)
                .map(|c| c.pwm_percent)
        };
        assert_eq!(pct("a"), Some(constants::PUMP_STALL_RESPONSE_PCT));
        assert_eq!(pct("b"), Some(55), "an unwatched member is untouched");
        assert_eq!(pct("c"), Some(constants::PUMP_STALL_RESPONSE_PCT));
    }
}
