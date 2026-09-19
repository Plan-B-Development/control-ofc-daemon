//! Fan controller for OpenFanController write operations.
//!
//! Owns the serial transport behind a Mutex, validates inputs,
//! sends commands, and updates the cache.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::SerialError;
use crate::health::cache::StateCache;
use crate::serial::protocol::{Channel, Command, NUM_CHANNELS};
use crate::serial::transport::{send_command, SerialTransport};

use crate::constants;

// Legacy MIN_PWM_PERCENT removed — thermal safety is now handled by
// ThermalSafetyRule in safety.rs, not per-command clamping.

use crate::pwm::percent_to_raw;

/// Per-channel state for coalescing and stop tracking.
#[derive(Debug, Clone, Default)]
struct ChannelControl {
    last_commanded_pct: Option<u8>,
    stop_started_at: Option<Instant>,
    /// Whether this controller has ever sent the channel a duty (DEC-388).
    ///
    /// Never cleared. A reconnect or a failed reply makes the channel's duty
    /// UNKNOWN (`last_commanded_pct = None`), which is not the same as never
    /// having touched it — and the exit floor tells them apart: a channel the
    /// daemon never wrote is left alone, one whose duty it lost goes to full
    /// speed.
    written: bool,
    /// Whether this channel's duty is unknown because a reconnect or resume
    /// lost it (DEC-401, `TS-av`) — as opposed to never written, or a reply
    /// that failed. Set on every written channel when
    /// [`FanController::observe_write_generation`] sees a bump, cleared by the
    /// next write that lands. A failed reply leaves it as it was: the device
    /// may still be at its power-on default.
    lost_to_reconnect: bool,
    /// The lowest duty this channel may be written at from now on (DEC-388):
    /// latched by [`FanController::apply_exit_floor`] and never cleared.
    ///
    /// Once the stop has left a channel at its exit duty, nothing that runs
    /// after it may lower it — an OpenFan calibration sweep's next step or its
    /// drop-restore (the sweep runs inside an HTTP request, which outlives the
    /// server drain), or an engine batch that outlived the task drain.
    /// [`FanController::set_pwm`] raises any such command to this, under the
    /// same mutex the floor was applied under, so there is no window between a
    /// check and a write. Raising is never refused: a forced 100 % still lands.
    exit_min: Option<u8>,
}

/// One channel's exit-floor write (DEC-388), for the caller's report.
#[derive(Debug)]
pub struct ExitFloorWrite {
    pub channel: u8,
    /// What the controller last knew the channel held; `None` = unknown.
    pub was_pct: Option<u8>,
    pub target_pct: u8,
    pub result: Result<SetPwmResult, FanControlError>,
}

/// Fan controller that serialises access to the OpenFanController.
pub struct FanController {
    transport: Arc<Mutex<Box<dyn SerialTransport + Send>>>,
    cache: Arc<StateCache>,
    timeout: Duration,
    channels: Vec<ChannelControl>,
    /// Last OpenFan write generation this controller observed (DEC-256). A bump
    /// means the device may no longer hold what we last commanded — see
    /// `StateCache::invalidate_openfan_writes`.
    last_write_generation: u64,
}

impl FanController {
    pub fn new(
        transport: Box<dyn SerialTransport + Send>,
        cache: Arc<StateCache>,
        timeout: Duration,
    ) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
            cache,
            timeout,
            channels: vec![ChannelControl::default(); NUM_CHANNELS as usize],
            last_write_generation: 0,
        }
    }

    /// Create a controller that shares a transport with another consumer (e.g. polling loop).
    pub fn new_shared(
        transport: Arc<Mutex<Box<dyn SerialTransport + Send>>>,
        cache: Arc<StateCache>,
        timeout: Duration,
    ) -> Self {
        Self {
            transport,
            cache,
            timeout,
            channels: vec![ChannelControl::default(); NUM_CHANNELS as usize],
            last_write_generation: 0,
        }
    }

    /// The duty this controller last put on `channel`, or `None` when it does not
    /// know it: nothing written since it started, the device may have lost it (a
    /// reconnect or resume clears every channel — DEC-256), or the last command's
    /// reply failed, so it may or may not have landed (DEC-383).
    ///
    /// Read by the thermal force to remember what a channel no profile controls
    /// was doing before an emergency, so it can be given back afterwards
    /// (DEC-382). `None` there means "unknown", and an unknown channel stays at
    /// the forced duty rather than being guessed down. Also read by the
    /// no-sensor floor to hold a skipped control's channel at its last duty
    /// (`TS-p`, DEC-386).
    ///
    /// [SAFETY] `TS-ak`: a reconnect or resume that no [`Self::set_pwm`] has
    /// observed yet already makes every duty unknown. The stored value is only
    /// cleared when the next write observes the bump, so the accessor checks the
    /// generation itself — otherwise a read before that write returns the
    /// pre-reconnect duty, and the force floors or snapshots a channel at a duty
    /// the device may no longer hold. Read-only: the clearing stays in
    /// [`Self::observe_write_generation`], so this takes `&self`.
    pub fn last_commanded_pct(&self, channel: u8) -> Option<u8> {
        if self.cache.openfan_write_generation() != self.last_write_generation {
            return None;
        }
        self.channels
            .get(channel as usize)
            .and_then(|c| c.last_commanded_pct)
    }

    /// The exit floor (DEC-388, `TS-j`): leave every channel this controller has
    /// ever written at `max(its last duty, floor_pct)`, or at 100 % where it no
    /// longer knows that duty. A channel it never wrote is left alone, and a
    /// `floor_pct` of 0 turns the whole step off — every channel keeps what it
    /// holds, as before DEC-388.
    ///
    /// OpenFan channels have no firmware curve to fall back to, so whatever a
    /// stop leaves them at they hold until a daemon owns them again. Goes through
    /// [`Self::set_pwm`], so a channel already at or above the floor coalesces
    /// and nothing is written to it.
    ///
    /// It also LATCHES the floor ([`ChannelControl::exit_min`]): each written
    /// channel at its exit duty, every other one at `floor_pct`. From then on no
    /// command can lower a channel below that, whoever sends it — the promise the
    /// stop logs has to survive the writers that outlive the drains.
    pub fn apply_exit_floor(&mut self, floor_pct: u8) -> Vec<ExitFloorWrite> {
        if floor_pct == 0 {
            return Vec::new();
        }
        // A reconnect or resume the next `set_pwm` has not yet seen would leave
        // `last_commanded_pct` describing a device that may have come back at its
        // power-on default — observe it first, so such a channel reads UNKNOWN
        // and goes to full speed rather than to `max(stale, floor)`.
        self.observe_write_generation();
        let targets: Vec<(u8, Option<u8>)> = self
            .channels
            .iter()
            .enumerate()
            .filter(|(_, c)| c.written)
            .map(|(ch, c)| (ch as u8, c.last_commanded_pct))
            .collect();
        for c in &mut self.channels {
            c.exit_min = Some(if c.written {
                crate::pwm::exit_duty(c.last_commanded_pct, floor_pct)
            } else {
                floor_pct
            });
        }
        targets
            .into_iter()
            .map(|(channel, was_pct)| {
                let target_pct = crate::pwm::exit_duty(was_pct, floor_pct);
                ExitFloorWrite {
                    channel,
                    was_pct,
                    target_pct,
                    result: self.set_pwm(channel, target_pct),
                }
            })
            .collect()
    }

    /// Whether `channel`'s duty is unknown because a reconnect or resume lost it
    /// (DEC-401, `TS-av`): the daemon had written the channel, and the device
    /// has since re-enumerated or the host resumed, with no write landing since.
    /// `false` for a channel never written and for one whose only unknown is a
    /// failed reply (DEC-383).
    ///
    /// [SAFETY] Read by the no-sensor floor's held arm, which gives such a
    /// channel full speed rather than the bare floor. Like
    /// [`Self::last_commanded_pct`] it honours a bump no write has observed yet,
    /// because the force reads it before its first write of a tick.
    pub fn duty_lost_to_reconnect(&self, channel: u8) -> bool {
        let Some(c) = self.channels.get(channel as usize) else {
            return false;
        };
        if self.cache.openfan_write_generation() != self.last_write_generation {
            return c.written;
        }
        c.lost_to_reconnect
    }

    /// Forget every channel's duty if the device may have lost it (DEC-256).
    ///
    /// Called at the top of [`Self::set_pwm`] and of [`Self::apply_exit_floor`],
    /// which both act on `last_commanded_pct` and must not act on a stale one.
    fn observe_write_generation(&mut self) {
        // DEC-256: a resume or a serial reconnect means the device may no longer
        // hold what we last commanded — the poll loop swaps the transport
        // underneath us after a USB re-enumeration, and the controller may come
        // back at its power-on default. Coalescing against a stale cache then
        // silences every subsequent identical command, leaving the fan at the
        // firmware default while the daemon reports the commanded value.
        //
        // Whether this firmware actually resets duty on re-enumeration is NOT
        // determinable from the protocol, so this takes the safe branch: assume
        // it might have. The cost when it did not is one redundant write per
        // channel, once, on a path that already just reconnected.
        let generation = self.cache.openfan_write_generation();
        if generation != self.last_write_generation {
            self.last_write_generation = generation;
            for ch in &mut self.channels {
                ch.last_commanded_pct = None;
                ch.lost_to_reconnect = ch.written;
                // The stop clock MUST be reset with it. `apply_safety`'s own
                // doc note says the expired-timer branch is unreachable because
                // "any non-zero write clears the timer; a repeat 0% coalesces"
                // — and this loop is exactly the tracking-state write outside
                // `set_pwm` that note guards against. Clearing only
                // `last_commanded_pct` disables the coalesce while leaving a
                // stale `stop_started_at` behind, so a channel legitimately
                // parked at 0% fails the 8 s stop timeout on its next tick,
                // every tick, forever: the write never lands, so neither field
                // is ever updated to break the cycle. The fan is then stranded
                // at whatever duty the re-enumerated device powered on with —
                // the precise failure DEC-256 exists to prevent — while the
                // daemon reports 0% and raises a link alert on healthy hardware.
                //
                // Resetting it is also the honest semantics: the device just
                // re-enumerated, so "how long has THIS device been stopped" is
                // unknown, and the safe answer is to start the clock again.
                ch.stop_started_at = None;
            }
        }
    }

    /// Set PWM on a single channel. `pwm_percent` is 0–100.
    ///
    /// - 0% is allowed for up to `constants::STOP_TIMEOUT` (8s), after which it's rejected.
    /// - Values are passed through as-is (0–100).
    /// - If the value equals the last commanded value, the write is coalesced (skipped).
    pub fn set_pwm(
        &mut self,
        channel: u8,
        pwm_percent: u8,
    ) -> Result<SetPwmResult, FanControlError> {
        if channel >= NUM_CHANNELS {
            return Err(FanControlError::Validation(format!(
                "channel {channel} out of range (0–{})",
                NUM_CHANNELS - 1
            )));
        }
        if pwm_percent > 100 {
            return Err(FanControlError::Validation(format!(
                "pwm_percent {pwm_percent} out of range (0–100)"
            )));
        }

        self.observe_write_generation();

        // DEC-388: once the stop's exit floor has run, a command may raise a
        // channel but never take it below the duty the floor left it at —
        // raised BEFORE the coalesce check, so a lower command against a channel
        // already at its exit duty writes nothing.
        let requested = pwm_percent;
        let pwm_percent = match self.channels[channel as usize].exit_min {
            Some(min) if pwm_percent < min => {
                log::info!(
                    "OpenFan channel {channel}: {requested} % raised to {min} % — the exit \
                     floor has already run"
                );
                min
            }
            _ => pwm_percent,
        };

        let ch_ctrl = &self.channels[channel as usize];

        // Coalesce BEFORE the stop-timeout check (CONC-2, 2026-07-21 audit):
        // a curve or identify-stop legitimately holding 0% re-sends the same
        // value every engine tick. A coalesced repeat writes nothing to the
        // wire, so it must not trip the stop timeout — with the old order,
        // every 0% tick past 8 s returned Validation, inflating per-channel
        // failure streaks (and the whole-link alert) on a healthy link. The
        // timeout below now guards only writes that would actually land.
        if ch_ctrl.last_commanded_pct == Some(pwm_percent) {
            return Ok(SetPwmResult {
                channel,
                pwm_percent,
                coalesced: true,
            });
        }

        let effective_pct = self.apply_safety(channel, pwm_percent)?;

        let raw = percent_to_raw(effective_pct);
        let ch = Channel::new(channel).map_err(FanControlError::Serial)?;
        let cmd = Command::SetPwm(ch, raw);

        // DEC-388: marked before the frame goes out — a frame whose reply fails
        // may still have landed, so from here the device may hold our duty.
        self.channels[channel as usize].written = true;

        let mut transport = self.transport.lock();
        let sent = send_command(&mut **transport, &cmd, self.timeout);
        drop(transport);

        if let Err(e) = sent {
            // [SAFETY] TS-o / DEC-383. `send_command` writes the frame BEFORE it
            // waits for the reply, so a reply that times out, arrives garbled or
            // answers something else says nothing about whether the device applied
            // this duty. Keeping the old value as "last commanded" then coalesced
            // every later identical command into silence: a channel tracked at 100
            // that took a 60 whose reply failed skipped every forced 100 % for the
            // rest of an emergency while the cache reported 100. The device's duty
            // is unknown, so the tracking says so.
            //
            // The stop clock goes with it, for the reason the reconnect path above
            // gives: clearing only `last_commanded_pct` would stop a 0 % hold from
            // coalescing while a stale `stop_started_at` rejects every wire-bound
            // 0 % past the timeout — forever, because the write never lands to
            // update either field.
            let ch = &mut self.channels[channel as usize];
            ch.last_commanded_pct = None;
            ch.stop_started_at = None;
            return Err(FanControlError::Serial(e));
        }

        // Update tracking state
        self.channels[channel as usize].last_commanded_pct = Some(effective_pct);
        self.channels[channel as usize].lost_to_reconnect = false;
        if effective_pct == 0 {
            if self.channels[channel as usize].stop_started_at.is_none() {
                self.channels[channel as usize].stop_started_at = Some(Instant::now());
            }
        } else {
            self.channels[channel as usize].stop_started_at = None;
        }

        // Update cache (store percent, not raw — GUI displays this as "%")
        self.cache.set_openfan_commanded_pwm(channel, effective_pct);

        Ok(SetPwmResult {
            channel,
            pwm_percent: effective_pct,
            coalesced: false,
        })
    }

    /// Apply safety rules: stop timeout only (the minimum-PWM floor is applied
    /// upstream in the profile-engine tuning pipeline).
    ///
    /// The coalesce check in [`Self::set_pwm`] runs first (CONC-2), so a
    /// steady 0% hold never reaches this — repeats coalesce. The timeout
    /// still rejects a *wire-bound* 0% against an expired stop timer. No
    /// normal `set_pwm` sequence produces that state (any non-zero write
    /// clears the timer; a repeat 0% coalesces) — kept as defence-in-depth
    /// against tracking state ever being written outside `set_pwm`.
    fn apply_safety(&self, channel: u8, pwm_percent: u8) -> Result<u8, FanControlError> {
        if pwm_percent == 0 {
            // Check stop timeout (hardware safety for serial protocol)
            if let Some(started) = self.channels[channel as usize].stop_started_at {
                if started.elapsed() >= constants::STOP_TIMEOUT {
                    return Err(FanControlError::Validation(format!(
                        "channel {channel}: 0% PWM exceeded {}s stop timeout",
                        constants::STOP_TIMEOUT.as_secs()
                    )));
                }
            }
        }
        Ok(pwm_percent)
    }
}

/// Result of a per-channel PWM set operation.
#[derive(Debug, Clone)]
pub struct SetPwmResult {
    pub channel: u8,
    pub pwm_percent: u8,
    pub coalesced: bool,
}

/// Errors from fan control operations.
#[derive(Debug)]
pub enum FanControlError {
    /// Input validation failure.
    Validation(String),
    /// Serial/hardware failure.
    Serial(SerialError),
}

impl std::fmt::Display for FanControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(msg) => write!(f, "validation error: {msg}"),
            Self::Serial(e) => write!(f, "serial error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Mock transport that records writes via shared state and returns canned responses.
    struct MockTransport {
        responses: VecDeque<Result<String, SerialError>>,
        /// Budget of acks to synthesise from the last written command instead of
        /// replaying a canned line.
        ///
        /// The old fixture answered `<02|00:0400;>` to *every* write, whatever
        /// channel it addressed. That was harmless while nothing correlated replies
        /// and actively misleading once DEC-301 did: real firmware echoes the opcode
        /// **and the channel it acted on** (`host_comm_process_request`), which is
        /// exactly the discriminator that stops one channel's ack confirming the
        /// next channel's write. A mock that answers channel 0 to everything could
        /// never have caught that.
        echo_acks_remaining: usize,
        written: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl MockTransport {
        fn with_responses(
            responses: Vec<Result<String, SerialError>>,
        ) -> (Self, Arc<parking_lot::Mutex<Vec<String>>>) {
            let written = Arc::new(parking_lot::Mutex::new(Vec::new()));
            (
                Self {
                    responses: responses.into(),
                    echo_acks_remaining: 0,
                    written: written.clone(),
                },
                written,
            )
        }

        /// `count` successful exchanges, each answered the way the firmware answers:
        /// same opcode, same channel. After the budget is spent, reads time out —
        /// preserving the count semantics the callers rely on.
        fn with_ok_responses(count: usize) -> (Self, Arc<parking_lot::Mutex<Vec<String>>>) {
            let (mut t, written) = Self::with_responses(vec![]);
            t.echo_acks_remaining = count;
            (t, written)
        }
    }

    impl SerialTransport for MockTransport {
        fn write_line(&mut self, data: &str) -> Result<(), SerialError> {
            self.written.lock().push(data.to_string());
            Ok(())
        }

        fn read_line(&mut self, _timeout: Duration) -> Result<String, SerialError> {
            if self.echo_acks_remaining > 0 {
                self.echo_acks_remaining -= 1;
                let last = self.written.lock().last().cloned();
                return match last {
                    Some(cmd) => Ok(crate::serial::protocol::firmware_echo_for(&cmd)),
                    None => Err(SerialError::Timeout { timeout_ms: 500 }),
                };
            }
            self.responses
                .pop_front()
                .unwrap_or(Err(SerialError::Timeout { timeout_ms: 500 }))
        }
    }

    fn make_controller(transport: MockTransport) -> FanController {
        FanController::new(
            Box::new(transport),
            Arc::new(StateCache::new()),
            Duration::from_millis(500),
        )
    }

    // ── DEC-388: the exit floor ─────────────────────────────────────

    /// [SAFETY] A channel below the floor is raised to it; one above it is not
    /// rewritten; one the controller never wrote is left alone entirely.
    #[test]
    fn the_exit_floor_raises_written_channels_and_leaves_the_rest() {
        let (transport, written) = MockTransport::with_ok_responses(3);
        let mut ctrl = make_controller(transport);
        ctrl.set_pwm(0, 30).unwrap();
        ctrl.set_pwm(1, 80).unwrap();
        let before = written.lock().len();

        let out = ctrl.apply_exit_floor(50);

        let summary: Vec<_> = out
            .iter()
            .map(|w| (w.channel, w.was_pct, w.target_pct))
            .collect();
        assert_eq!(
            summary,
            [(0, Some(30), 50), (1, Some(80), 80)],
            "only the two written channels, each at max(last, floor)"
        );
        assert!(!out[0].result.as_ref().unwrap().coalesced);
        assert!(
            out[1].result.as_ref().unwrap().coalesced,
            "a channel already above the floor is not rewritten"
        );
        let frames = written.lock()[before..].to_vec();
        assert_eq!(frames.len(), 1, "one frame, for channel 0: {frames:?}");
        assert_eq!(ctrl.last_commanded_pct(0), Some(50));
    }

    /// [SAFETY] A reconnect or resume makes every duty unknown (DEC-256), and an
    /// unknown duty leaves at FULL speed — even when no `set_pwm` has run since
    /// to notice the invalidation, which is the case at a stop.
    #[test]
    fn a_channel_whose_duty_was_lost_goes_to_full_speed_on_stop() {
        let (transport, written) = MockTransport::with_ok_responses(2);
        let cache = Arc::new(StateCache::new());
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );
        ctrl.set_pwm(3, 40).unwrap();
        cache.invalidate_openfan_writes();

        let out = ctrl.apply_exit_floor(50);

        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].channel, out[0].was_pct, out[0].target_pct),
            (3, None, 100)
        );
        assert!(!out[0].result.as_ref().unwrap().coalesced);
        let last = written.lock().last().cloned().unwrap();
        assert!(
            last.contains("FF"),
            "the exit frame carries raw 255 (100 %): {last}"
        );
    }

    /// [SAFETY] Nothing that runs after the exit floor can lower a channel
    /// below it: a calibration sweep's step or its drop-restore, or an engine
    /// batch that outlived the drain, all write through `set_pwm`. A channel the
    /// floor had to write is held at its exit duty, one it never wrote at the
    /// floor itself, and a command above either still lands.
    #[test]
    fn nothing_after_the_exit_floor_can_lower_a_channel() {
        let (transport, written) = MockTransport::with_ok_responses(4);
        let mut ctrl = make_controller(transport);
        ctrl.set_pwm(0, 30).unwrap();
        ctrl.apply_exit_floor(50);
        assert_eq!(ctrl.last_commanded_pct(0), Some(50), "precondition");
        let before = written.lock().len();

        let lower = ctrl.set_pwm(0, 20).unwrap();
        assert_eq!((lower.pwm_percent, lower.coalesced), (50, true));
        assert_eq!(
            written.lock().len(),
            before,
            "a lower command against a channel at its exit duty writes nothing"
        );
        assert_eq!(ctrl.last_commanded_pct(0), Some(50));

        let unwritten = ctrl.set_pwm(1, 10).unwrap();
        assert_eq!(
            (unwritten.pwm_percent, unwritten.coalesced),
            (50, false),
            "a channel first written after the floor is raised to the floor"
        );
        assert_eq!(ctrl.last_commanded_pct(1), Some(50));

        let higher = ctrl.set_pwm(0, 80).unwrap();
        assert_eq!((higher.pwm_percent, higher.coalesced), (80, false));
        assert_eq!(ctrl.last_commanded_pct(0), Some(80));
    }

    /// A floor of 0 turns the exit floor off: nothing is written at all, as
    /// before DEC-388 — even to a channel whose duty is unknown.
    #[test]
    fn a_zero_exit_floor_writes_nothing() {
        let (transport, written) = MockTransport::with_ok_responses(1);
        let cache = Arc::new(StateCache::new());
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );
        ctrl.set_pwm(2, 20).unwrap();
        cache.invalidate_openfan_writes();
        let before = written.lock().len();

        assert!(ctrl.apply_exit_floor(0).is_empty());
        assert_eq!(written.lock().len(), before);
    }

    // ── Set PWM per channel ─────────────────────────────────────────

    #[test]
    fn set_pwm_valid_channel() {
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        let result = ctrl.set_pwm(0, 50).unwrap();
        assert_eq!(result.channel, 0);
        assert_eq!(result.pwm_percent, 50);
        assert!(!result.coalesced);
    }

    #[test]
    fn set_pwm_golden_frame() {
        let (transport, written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        ctrl.set_pwm(5, 50).unwrap();

        // 50% → raw 128 = 0x80, channel 5 = 0x05
        let written = written.lock();
        assert_eq!(*written, vec![">020580\n"]);
    }

    #[test]
    fn set_pwm_invalid_channel() {
        let (transport, _written) = MockTransport::with_ok_responses(0);
        let mut ctrl = make_controller(transport);

        let err = ctrl.set_pwm(10, 50).unwrap_err();
        match err {
            FanControlError::Validation(msg) => assert!(msg.contains("out of range")),
            _ => panic!("expected validation error"),
        }
    }

    #[test]
    fn set_pwm_invalid_percent() {
        let (transport, _written) = MockTransport::with_ok_responses(0);
        let mut ctrl = make_controller(transport);

        let err = ctrl.set_pwm(0, 101).unwrap_err();
        match err {
            FanControlError::Validation(msg) => assert!(msg.contains("out of range")),
            _ => panic!("expected validation error"),
        }
    }

    #[test]
    fn set_pwm_accepts_low_values() {
        // No floor clamping — thermal safety handled by ThermalSafetyRule
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        let result = ctrl.set_pwm(0, 10).unwrap();
        assert_eq!(result.pwm_percent, 10); // no clamping, passed through
    }

    #[test]
    fn set_pwm_allows_zero() {
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        let result = ctrl.set_pwm(0, 0).unwrap();
        assert_eq!(result.pwm_percent, 0);
    }

    #[test]
    fn repeated_zero_beyond_stop_timeout_coalesces_not_errors() {
        // CONC-2 (2026-07-21 audit): a curve/identify hold at 0% re-sends 0
        // every engine tick. Past the 8 s stop timeout those repeats must
        // coalesce (Ok, nothing on the wire) rather than return Validation —
        // the pre-fix order errored every tick, inflating per-channel failure
        // streaks and risking a false link-down alert on a healthy link.
        let (transport, written) = MockTransport::with_ok_responses(2);
        let mut ctrl = make_controller(transport);

        // First 0% writes and starts the stop timer.
        ctrl.set_pwm(0, 0).unwrap();
        assert!(ctrl.channels[0].stop_started_at.is_some());
        let wire_writes_after_first = written.lock().len();

        // Backdate the timer beyond STOP_TIMEOUT (8 s).
        ctrl.channels[0].stop_started_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(9));

        let result = ctrl.set_pwm(0, 0).unwrap();
        assert!(result.coalesced, "repeat 0% must coalesce, not error");
        assert_eq!(
            written.lock().len(),
            wire_writes_after_first,
            "a coalesced repeat must not touch the wire"
        );

        // Non-zero PWM still works and clears the stop timer.
        let result = ctrl.set_pwm(0, 50).unwrap();
        assert_eq!(result.pwm_percent, 50);
        assert!(ctrl.channels[0].stop_started_at.is_none());
    }

    #[test]
    fn invalidation_resets_the_stop_clock_so_a_parked_channel_still_writes() {
        // Release review, 2026-08-10. DEC-256's write-generation invalidation
        // cleared `last_commanded_pct` but not `stop_started_at`, which is the
        // one combination `apply_safety`'s defence-in-depth branch rejects.
        //
        // Sequence: a channel is legitimately parked at 0% (a curve's stop-snap
        // or a DEC-166 identify-stop). Repeats coalesce, so the stop timer ages
        // past 8 s harmlessly. Then the device re-enumerates or the machine
        // resumes and the generation bumps. Pre-fix, the next 0% write lost its
        // coalesce, hit the expired timer, and returned Validation — and since
        // the write never landed, neither field changed, so it failed again
        // every tick forever while the fan sat at the device's power-on duty.
        let (transport, written) = MockTransport::with_ok_responses(4);
        let mut ctrl = make_controller(transport);

        ctrl.set_pwm(0, 0).unwrap();
        ctrl.channels[0].stop_started_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(9));
        let before = written.lock().len();

        // The device re-enumerated: invalidate, as polling.rs does on reconnect.
        ctrl.cache.invalidate_openfan_writes();

        let result = ctrl
            .set_pwm(0, 0)
            .expect("a parked channel must still be writable after invalidation");
        assert!(
            !result.coalesced,
            "invalidation must force this write onto the wire — that is its whole purpose"
        );
        assert_eq!(
            written.lock().len(),
            before + 1,
            "the post-reconnect write DEC-256 exists to force must actually reach the device"
        );

        // And the cycle must not re-arm: the next tick coalesces normally
        // instead of erroring, which is what proves the clock really restarted.
        let repeat = ctrl.set_pwm(0, 0).unwrap();
        assert!(repeat.coalesced);
    }

    #[test]
    fn stop_timeout_still_rejects_wire_bound_zero() {
        // The stop timeout is defence-in-depth for a *wire-bound* 0% against
        // an expired stop timer. No normal set_pwm sequence produces that
        // state (any non-zero write clears the timer; a repeat 0% coalesces),
        // so it guards tracking state written outside set_pwm — construct
        // that state directly.
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        ctrl.channels[0].last_commanded_pct = Some(50);
        ctrl.channels[0].stop_started_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(9));

        let err = ctrl.set_pwm(0, 0).unwrap_err();
        match err {
            FanControlError::Validation(msg) => assert!(msg.contains("stop timeout")),
            _ => panic!("expected stop timeout validation error"),
        }
    }

    #[test]
    fn set_pwm_coalesces_duplicate() {
        let (transport, written) = MockTransport::with_ok_responses(2);
        let mut ctrl = make_controller(transport);

        ctrl.set_pwm(0, 50).unwrap();
        let result = ctrl.set_pwm(0, 50).unwrap();
        assert!(result.coalesced);

        // Only one command should have been written
        assert_eq!(written.lock().len(), 1);
    }

    #[test]
    fn a_reconnect_or_resume_breaks_coalescing_so_the_next_write_lands() {
        // DEC-256. Coalescing is only sound while `last_commanded_pct` reflects
        // the DEVICE. A serial reconnect swaps the transport underneath this
        // controller after a USB re-enumeration, and a resume can reset the
        // hardware too — so the cache may describe a device that came back at its
        // power-on default. Every subsequent identical command was then coalesced
        // into silence, leaving the fan at the firmware default while the daemon
        // reported the commanded value.
        let (transport, written) = MockTransport::with_ok_responses(3);
        let cache = Arc::new(StateCache::new());
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );

        ctrl.set_pwm(0, 50).unwrap();
        assert!(
            ctrl.set_pwm(0, 50).unwrap().coalesced,
            "precondition: an identical repeat coalesces on a healthy link"
        );
        assert_eq!(written.lock().len(), 1);

        cache.invalidate_openfan_writes();

        let result = ctrl.set_pwm(0, 50).unwrap();
        assert!(
            !result.coalesced,
            "after a reconnect the same value must reach the wire again"
        );
        assert_eq!(written.lock().len(), 2);
    }

    /// [SAFETY] `TS-ak`: after a reconnect or resume, the accessor reports every
    /// channel's duty as unknown straight away — not only once the next
    /// `set_pwm` has observed the bump. The thermal force reads it BEFORE its
    /// first write, so a stale answer there floors or snapshots a channel at a
    /// duty the device may no longer hold.
    #[test]
    fn a_pending_invalidation_makes_the_last_duty_unknown_before_any_write() {
        let (transport, _written) = MockTransport::with_ok_responses(3);
        let cache = Arc::new(StateCache::new());
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );
        ctrl.set_pwm(0, 85).unwrap();
        ctrl.set_pwm(1, 40).unwrap();
        assert_eq!(ctrl.last_commanded_pct(0), Some(85), "precondition");

        cache.invalidate_openfan_writes();

        assert_eq!(ctrl.last_commanded_pct(0), None);
        assert_eq!(ctrl.last_commanded_pct(1), None);

        // Observing the bump does not bring the stale value back, and the next
        // landed write is known again.
        ctrl.set_pwm(0, 60).unwrap();
        assert_eq!(ctrl.last_commanded_pct(0), Some(60));
        assert_eq!(ctrl.last_commanded_pct(1), None);
    }

    /// [SAFETY] DEC-401 (`TS-av`): a reconnect or resume marks every channel the
    /// daemon had written as lost — before any write observes the bump, and after
    /// one has — and a never-written channel as not. A landed write clears it.
    #[test]
    fn a_reconnect_marks_every_written_channel_lost_until_a_write_lands() {
        let (transport, _written) = MockTransport::with_ok_responses(3);
        let cache = Arc::new(StateCache::new());
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );
        ctrl.set_pwm(0, 85).unwrap();
        ctrl.set_pwm(1, 40).unwrap();
        assert!(!ctrl.duty_lost_to_reconnect(0), "precondition: known duty");

        cache.invalidate_openfan_writes();

        // Pending: no write has observed the bump yet.
        assert!(ctrl.duty_lost_to_reconnect(0));
        assert!(ctrl.duty_lost_to_reconnect(1));
        assert!(!ctrl.duty_lost_to_reconnect(2), "never written is not lost");

        // Observed: channel 0's write lands and clears only channel 0.
        ctrl.set_pwm(0, 60).unwrap();
        assert!(!ctrl.duty_lost_to_reconnect(0));
        assert!(ctrl.duty_lost_to_reconnect(1));
        assert!(!ctrl.duty_lost_to_reconnect(2));
        assert!(!ctrl.duty_lost_to_reconnect(NUM_CHANNELS), "out of range");
    }

    /// [SAFETY] DEC-401: a failed reply on its own is an unknown duty but not a
    /// lost one (the user chose full speed for the reconnect/resume case only).
    /// A reconnect after it makes it lost, and a failed reply after the
    /// reconnect does not clear that — the device may still be at its power-on
    /// default.
    #[test]
    fn a_failed_reply_alone_is_not_a_duty_lost_to_a_reconnect() {
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let cache = Arc::new(StateCache::new());
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );
        ctrl.set_pwm(0, 60).unwrap();
        assert!(ctrl.set_pwm(0, 70).is_err(), "precondition: reply fails");
        assert_eq!(ctrl.last_commanded_pct(0), None, "precondition: unknown");
        assert!(!ctrl.duty_lost_to_reconnect(0));

        cache.invalidate_openfan_writes();
        assert!(ctrl.duty_lost_to_reconnect(0));

        assert!(ctrl.set_pwm(0, 80).is_err(), "the reply fails again");
        assert!(ctrl.duty_lost_to_reconnect(0));
    }

    #[test]
    fn invalidation_clears_every_channel_not_just_the_one_being_written() {
        // The device re-enumerates as a whole, so a per-channel invalidation
        // keyed off whichever channel happens to be written first would leave the
        // rest of the cache stale.
        let (transport, written) = MockTransport::with_ok_responses(6);
        let cache = Arc::new(StateCache::new());
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );

        ctrl.set_pwm(0, 40).unwrap();
        ctrl.set_pwm(1, 40).unwrap();
        assert_eq!(written.lock().len(), 2);

        cache.invalidate_openfan_writes();

        // Writing channel 0 first must not consume the invalidation for channel 1.
        assert!(!ctrl.set_pwm(0, 40).unwrap().coalesced);
        assert!(!ctrl.set_pwm(1, 40).unwrap().coalesced);
        assert_eq!(written.lock().len(), 4);
    }

    #[test]
    fn a_resume_also_invalidates_openfan_coalescing() {
        // hwmon has always cleared its manual-mode flags on resume; OpenFan had
        // no equivalent. `set_resume_detected` now bumps the generation too, and
        // must do so WITHOUT disturbing the hwmon flag, which is a separate
        // swap-once consumer.
        let cache = StateCache::new();
        let before = cache.openfan_write_generation();

        cache.set_resume_detected();

        assert_ne!(cache.openfan_write_generation(), before);
        assert!(
            cache.take_resume_flag(),
            "hwmon's own flag is still delivered"
        );
    }

    #[test]
    fn set_pwm_does_not_coalesce_different_values() {
        let (transport, written) = MockTransport::with_ok_responses(2);
        let mut ctrl = make_controller(transport);

        ctrl.set_pwm(0, 50).unwrap();
        let result = ctrl.set_pwm(0, 60).unwrap();
        assert!(!result.coalesced);

        assert_eq!(written.lock().len(), 2);
    }

    #[test]
    fn set_pwm_updates_cache() {
        let cache = Arc::new(StateCache::new());
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );

        ctrl.set_pwm(3, 75).unwrap();

        let snap = cache.snapshot();
        let fan = snap.openfan_fans.get(&3).unwrap();
        assert_eq!(fan.last_commanded_pwm, Some(75));
    }

    fn ack(pct: u8) -> String {
        crate::serial::protocol::firmware_echo_for(
            &Command::SetPwm(Channel::new(0).unwrap(), percent_to_raw(pct)).encode(),
        )
    }

    /// [SAFETY] TS-o / DEC-383, the audit's own scenario. A channel tracked at 100
    /// takes a 60 % whose frame is written and whose reply fails, so the device
    /// may well be at 60 %. The next 100 % must reach the wire; before the fix it
    /// coalesced against the stale 100, and so did every forced 100 % after it.
    #[test]
    fn a_failed_reply_forgets_the_duty_so_the_next_identical_write_lands() {
        let (transport, written) = MockTransport::with_responses(vec![
            Ok(ack(100)),
            Err(SerialError::Timeout { timeout_ms: 500 }),
            Ok(ack(100)),
        ]);
        let mut ctrl = make_controller(transport);

        ctrl.set_pwm(0, 100).unwrap();
        assert!(
            ctrl.set_pwm(0, 60).is_err(),
            "precondition: the 60 % reply failed"
        );
        assert_eq!(
            written.lock().len(),
            2,
            "precondition: the 60 % frame reached the wire before its reply failed"
        );

        let again = ctrl.set_pwm(0, 100).unwrap();
        assert!(
            !again.coalesced,
            "the device may be at 60 %, so 100 % must be written again"
        );
        assert_eq!(written.lock().len(), 3);
    }

    /// The other half of the same reset. A channel parked at 0 % past the stop
    /// timeout, whose next non-zero write fails, must not keep its stale stop
    /// clock once its tracking is cleared: the curve's next 0 % would no longer
    /// coalesce and the timeout would reject it every tick, forever.
    #[test]
    fn a_failed_reply_also_restarts_the_stop_clock() {
        let (transport, written) = MockTransport::with_responses(vec![
            Err(SerialError::Timeout { timeout_ms: 500 }),
            Ok(ack(0)),
        ]);
        let mut ctrl = make_controller(transport);
        ctrl.channels[0].last_commanded_pct = Some(0);
        ctrl.channels[0].stop_started_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(9));

        assert!(
            ctrl.set_pwm(0, 30).is_err(),
            "precondition: the 30 % reply failed"
        );
        let stop = ctrl.set_pwm(0, 0);
        assert!(
            stop.as_ref().is_ok_and(|r| !r.coalesced),
            "the next 0 % must reach the wire, not be rejected by a stale stop clock: {stop:?}"
        );
        assert_eq!(written.lock().len(), 2);
    }

    #[test]
    fn set_pwm_serial_timeout() {
        let (transport, _written) = MockTransport::with_responses(vec![]);
        let mut ctrl = make_controller(transport);

        let err = ctrl.set_pwm(0, 50).unwrap_err();
        match err {
            FanControlError::Serial(SerialError::Timeout { .. }) => {}
            _ => panic!("expected serial timeout"),
        }
    }
}
