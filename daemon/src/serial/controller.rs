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

        let mut transport = self.transport.lock();

        send_command(&mut **transport, &cmd, self.timeout).map_err(FanControlError::Serial)?;

        drop(transport);

        // Update tracking state
        self.channels[channel as usize].last_commanded_pct = Some(effective_pct);
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
