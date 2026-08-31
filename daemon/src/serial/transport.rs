//! Minimal transport wrapper for serial I/O.
//!
//! Provides a trait for reading/writing lines with timeouts.
//! Concrete implementation will be added when a serial crate is approved.

use std::time::Duration;

use crate::error::SerialError;

/// Trait for serial port I/O (line-oriented).
///
/// Implementations must handle framing (lines terminated by `\n`).
pub trait SerialTransport {
    /// Write a line to the serial port. The line should already include `\n`.
    fn write_line(&mut self, data: &str) -> Result<(), SerialError>;

    /// Read a line from the serial port, with a timeout.
    /// Returns the line including any trailing `\r\n`.
    fn read_line(&mut self, timeout: Duration) -> Result<String, SerialError>;
}

/// Confirm that an already-open transport really is an OpenFanController.
///
/// [SAFETY] Openability is not identity (DEC-250). `RealSerialTransport::open`
/// succeeds on any readable tty — a modem, an Arduino, a 3D printer — so
/// accepting a port because it opened lets the wrong device be adopted as the
/// fan controller. Every subsequent write then "succeeds" against a device that
/// simply ignores it, **including the 105°C emergency `force_all`**: no error is
/// ever returned, so no `THERMAL SAFETY ... FAILED` is logged, `/status` reports
/// OpenFan healthy, and not one OpenFan-attached fan is actually being driven.
///
/// `ReadAllRpm` is the same handshake `auto_detect_port` uses to recognise the
/// controller in the first place. Sharing it keeps "what counts as an
/// OpenFanController" in exactly one place, so the configured-port path and the
/// detection path can never disagree about what they accept.
pub fn verify_openfan_identity(
    transport: &mut dyn SerialTransport,
    timeout: Duration,
) -> Result<(), SerialError> {
    send_command(
        transport,
        &crate::serial::protocol::Command::ReadAllRpm,
        timeout,
    )
    .map(|_| ())
}

/// Maximum debug lines to skip before giving up.
/// Normal firmware emits 0–3 debug lines; 50 is generous but finite.
const MAX_DEBUG_LINES: usize = 50;

/// Maximum out-of-band response frames to drain while resynchronising.
///
/// A desynchronised link is usually one frame behind, but the worst *legitimate*
/// backlog is larger than that: a board slower than `serial.timeout_ms` can make
/// all ten writes of a `force_all` tick time out while their acks still arrive, so
/// two such ticks queue ~20 frames. Draining shrinks the backlog by up to this many
/// per exchange, so even that case resynchronises within two exchanges — it costs
/// one spurious `Protocol` error, not a stuck link.
///
/// Effective tolerance is **15**, because the cap is checked *before* the read —
/// the same idiom as `MAX_DEBUG_LINES`, kept deliberately consistent with it.
///
/// Finite so a firmware that answers with the wrong code forever cannot wedge the
/// loop. The wall-clock deadline is the other bound, and the two are independent:
/// a fast flood hits this cap (~16 frames is milliseconds at 115200 baud), a slow
/// one hits the deadline.
const MAX_STALE_FRAMES: usize = 16;

/// Send a command and read **its** response, skipping anything that is not.
///
/// [SAFETY] The reply is correlated against the command's own opcode (DEC-301).
/// Before that, this function returned the first `<`-prefixed frame it read no
/// matter which request it answered — and the OpenFan link has two independent
/// 1 Hz users behind one mutex sharing one stateful reader (the poll loop's
/// `ReadAllRpm`, and the profile engine's per-channel `SetPwm`). One reply left
/// unread put the pipeline permanently one frame behind, so the poll loop cached
/// `SetPwm` acks as if they were tachometer readings: a *single*-channel frame
/// whose "RPM" is the echoed raw PWM byte. Measured at ~10% of readings, with
/// values that were exactly `pwm::percent_to_raw()` of the commanded percent.
///
/// Correlation also restores the meaning of a write acknowledgement. `set_pwm`
/// discards the response and treats `Ok` as "the controller took it", and the
/// 105 °C `force_all` writes through that same path — so an emergency write used
/// to be confirmed by whatever frame happened to be next in the buffer. It is now
/// confirmed only by an ack **for that channel**; a write the controller never
/// acknowledges times out and is reported instead of being silently swallowed.
///
/// The per-channel half of that is load-bearing, not decoration: matching on the
/// opcode alone would leave `force_all`'s ten back-to-back `SetPwm` writes — all
/// opcode `0x02` — able to absorb a one-frame offset undetected, each confirmed by
/// its predecessor's ack. See `Command::matches_reply`.
///
/// Three guards prevent infinite loops:
/// 1. Wall-clock deadline: total operation bounded by `timeout`
/// 2. At most `MAX_DEBUG_LINES` non-response lines skipped
/// 3. At most `MAX_STALE_FRAMES` responses to another command drained
pub fn send_command(
    transport: &mut dyn SerialTransport,
    command: &crate::serial::protocol::Command,
    timeout: Duration,
) -> Result<crate::serial::protocol::Response, SerialError> {
    use crate::serial::protocol::{decode_line, DecodedLine};
    use std::time::Instant;

    transport.write_line(&command.encode())?;

    let expected = command.opcode();
    let deadline = Instant::now() + timeout;
    let mut debug_lines_skipped: usize = 0;
    let mut stale_frames_drained: usize = 0;

    loop {
        if Instant::now() >= deadline {
            return Err(SerialError::Timeout {
                timeout_ms: timeout.as_millis() as u64,
            });
        }

        if debug_lines_skipped >= MAX_DEBUG_LINES {
            return Err(SerialError::Protocol {
                message: format!(
                    "no response after {debug_lines_skipped} debug lines — \
                     firmware may be in an error loop"
                ),
            });
        }

        if stale_frames_drained >= MAX_STALE_FRAMES {
            return Err(SerialError::Protocol {
                message: format!(
                    "no reply to {command:?} (opcode {expected:#04X}) after draining \
                     {stale_frames_drained} frames for other commands or channels — \
                     the link did not resynchronise"
                ),
            });
        }

        // Pass the REMAINING budget, not the full timeout. `RealSerialTransport`
        // currently ignores this argument and uses the port's configured timeout,
        // which is why draining is free today — but relying on that would make this
        // function's bound depend on a quirk marked with an underscore in another
        // file. Asking for what is actually left is the correct request either way,
        // and it keeps the total exchange bounded by `timeout` if the transport ever
        // starts honouring it.
        let line = transport.read_line(deadline.saturating_duration_since(Instant::now()))?;
        match decode_line(&line)? {
            // The reply to the command we just sent.
            DecodedLine::Response(response) if command.matches_reply(&response) => {
                if stale_frames_drained > 0 {
                    log::debug!(
                        "openfan: resynchronised — drained {stale_frames_drained} frame(s) \
                         for other commands before the reply to {expected:#04X}"
                    );
                }
                return Ok(response);
            }
            // A well-formed frame that is not the reply to this command — a
            // different opcode, or the same opcode for a different channel. Either
            // way it is a stale reply left in the pipeline by an earlier exchange.
            // Discarding it is what resynchronises the link: one exchange absorbs
            // the offset and every later read is aligned again. Returning it is the
            // DEC-301 defect; erroring on it would never heal, because the next
            // exchange would inherit the same offset and fail identically.
            DecodedLine::Response(_) => {
                stale_frames_drained += 1;
            }
            DecodedLine::DebugOutput(_) => {
                debug_lines_skipped += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::protocol::{Channel, ChannelRpm, Command, Response};
    use std::collections::VecDeque;

    /// Mock transport for testing.
    struct MockTransport {
        responses: VecDeque<Result<String, SerialError>>,
        written: Vec<String>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<String, SerialError>>) -> Self {
            Self {
                responses: responses.into(),
                written: Vec::new(),
            }
        }
    }

    impl SerialTransport for MockTransport {
        fn write_line(&mut self, data: &str) -> Result<(), SerialError> {
            self.written.push(data.to_string());
            Ok(())
        }

        fn read_line(&mut self, _timeout: Duration) -> Result<String, SerialError> {
            self.responses
                .pop_front()
                .unwrap_or(Err(SerialError::Timeout { timeout_ms: 500 }))
        }
    }

    #[test]
    fn send_command_read_all_rpm() {
        let mut transport = MockTransport::new(vec![Ok(
            "<00|00:04B0;01:044C;02:0000;03:0000;04:0000;05:0000;06:0000;07:0000;08:0000;09:0000;>\r\n".into(),
        )]);

        let result = send_command(
            &mut transport,
            &Command::ReadAllRpm,
            Duration::from_millis(500),
        )
        .unwrap();

        assert_eq!(transport.written, vec![">00\n"]);
        match result {
            Response::Rpm {
                command_code,
                readings,
            } => {
                assert_eq!(command_code, 0x00);
                assert_eq!(readings.len(), 10);
                assert_eq!(
                    readings[0],
                    ChannelRpm {
                        channel: 0,
                        rpm: 0x04B0
                    }
                );
            }
        }
    }

    #[test]
    fn send_command_skips_debug_lines() {
        let mut transport = MockTransport::new(vec![
            Ok("OpenFanController v1.2.3\r\n".into()),
            Ok("DEBUG: init complete\r\n".into()),
            Ok("<01|05:04B0;>\r\n".into()),
        ]);

        let ch = Channel::new(5).unwrap();
        let result = send_command(
            &mut transport,
            &Command::ReadRpm(ch),
            Duration::from_millis(500),
        )
        .unwrap();

        assert_eq!(transport.written, vec![">0105\n"]);
        match result {
            Response::Rpm {
                command_code,
                readings,
            } => {
                assert_eq!(command_code, 0x01);
                assert_eq!(readings.len(), 1);
                assert_eq!(readings[0].rpm, 0x04B0);
            }
        }
    }

    #[test]
    fn send_command_timeout() {
        let mut transport = MockTransport::new(vec![]);

        let result = send_command(
            &mut transport,
            &Command::ReadAllRpm,
            Duration::from_millis(500),
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timeout"));
    }

    #[test]
    fn send_command_set_pwm() {
        let ch = Channel::new(3).unwrap();
        let mut transport = MockTransport::new(vec![Ok("<02|03:0400;>\r\n".into())]);

        let result = send_command(
            &mut transport,
            &Command::SetPwm(ch, 128),
            Duration::from_millis(500),
        )
        .unwrap();

        assert_eq!(transport.written, vec![">020380\n"]);
        match result {
            Response::Rpm { command_code, .. } => {
                assert_eq!(command_code, 0x02);
            }
        }
    }

    #[test]
    fn send_command_real_firmware_no_closing_bracket() {
        // Real Karanovic OpenFan firmware does not include closing '>'
        let mut transport = MockTransport::new(vec![Ok(
            "<00|00:0546;01:0541;02:054A;03:051C;04:04F1;05:055E;06:0548;07:0521;08:0557;09:04DF;\r\n".into(),
        )]);

        let result = send_command(
            &mut transport,
            &Command::ReadAllRpm,
            Duration::from_millis(500),
        )
        .unwrap();

        match result {
            Response::Rpm {
                command_code,
                readings,
            } => {
                assert_eq!(command_code, 0x00);
                assert_eq!(readings.len(), 10);
                assert_eq!(readings[0].rpm, 0x0546);
                assert_eq!(readings[9].rpm, 0x04DF);
            }
        }
    }

    #[test]
    fn send_command_aborts_after_too_many_debug_lines() {
        // 60 debug lines exceeds MAX_DEBUG_LINES (50)
        let flood: Vec<Result<String, SerialError>> = (0..60)
            .map(|i| Ok(format!("DEBUG: flood line {i}\r\n")))
            .collect();
        let mut transport = MockTransport::new(flood);

        let result = send_command(
            &mut transport,
            &Command::ReadAllRpm,
            Duration::from_secs(10),
        );

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("debug lines"),
            "expected 'debug lines' in error, got: {msg}"
        );
    }

    #[test]
    fn send_command_deadline_exceeded() {
        // Duration::ZERO causes the deadline check to fire on second iteration
        let mut transport = MockTransport::new(vec![
            Ok("DEBUG: line 1\r\n".into()),
            Ok("DEBUG: line 2\r\n".into()),
        ]);

        let result = send_command(&mut transport, &Command::ReadAllRpm, Duration::ZERO);

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("timeout"),
            "expected 'timeout' in error, got: {msg}"
        );
    }

    #[test]
    fn send_command_many_debug_lines_then_response() {
        // 10 debug lines followed by a valid response — must succeed
        let mut lines: Vec<Result<String, SerialError>> = (0..10)
            .map(|i| Ok(format!("DEBUG: boot message {i}\r\n")))
            .collect();
        lines.push(Ok(
            "<00|00:04B0;01:044C;02:0000;03:0000;04:0000;05:0000;06:0000;07:0000;08:0000;09:0000;>\r\n".into(),
        ));
        let mut transport = MockTransport::new(lines);

        let result = send_command(&mut transport, &Command::ReadAllRpm, Duration::from_secs(5));

        assert!(result.is_ok());
        match result.unwrap() {
            Response::Rpm {
                command_code,
                readings,
            } => {
                assert_eq!(command_code, 0x00);
                assert_eq!(readings.len(), 10);
            }
        }
    }

    // ── Request/response correlation (DEC-301) ──────────────────────

    #[test]
    fn send_command_does_not_accept_a_frame_for_another_command() {
        // The DEC-301 defect, in the exact shape it was measured in the field:
        // a SetPwm ack is a SINGLE-channel frame whose "RPM" is the echoed raw
        // PWM byte. Channel 3 commanded 35% => percent_to_raw(35) == 89 == 0x0059.
        // Before correlation, this poll returned that frame and the daemon cached
        // 89 as channel 3's tachometer reading.
        let mut transport = MockTransport::new(vec![
            Ok("<02|03:0059;>\r\n".into()),
            Ok(
                "<00|00:04B0;01:044C;02:0000;03:0BB8;04:0000;05:0000;06:0000;07:0000;08:0000;09:0000;>\r\n"
                    .into(),
            ),
        ]);

        let result = send_command(
            &mut transport,
            &Command::ReadAllRpm,
            Duration::from_millis(500),
        )
        .unwrap();

        match result {
            Response::Rpm {
                command_code,
                readings,
            } => {
                assert_eq!(command_code, 0x00, "must return the ReadAllRpm reply");
                assert_eq!(readings.len(), 10, "a SetPwm ack carries only one channel");
                assert_eq!(
                    readings[3],
                    ChannelRpm {
                        channel: 3,
                        rpm: 0x0BB8
                    },
                    "channel 3 must carry its tachometer reading, not the PWM echo"
                );
                assert!(
                    !readings.iter().any(|r| r.rpm == 89),
                    "the echoed raw PWM byte must never surface as an RPM"
                );
            }
        }
    }

    #[test]
    fn send_command_correlates_a_set_pwm_ack() {
        // The mirror direction, and the safety-relevant one: `set_pwm` discards the
        // response and treats Ok as "the controller took it", and the 105 C
        // `force_all` writes through that path. A poll reply must not be able to
        // stand in as the acknowledgement for a write.
        let ch = Channel::new(3).unwrap();
        let mut transport = MockTransport::new(vec![
            Ok(
                "<00|00:04B0;01:044C;02:0000;03:0BB8;04:0000;05:0000;06:0000;07:0000;08:0000;09:0000;>\r\n"
                    .into(),
            ),
            Ok("<02|03:0059;>\r\n".into()),
        ]);

        let result = send_command(
            &mut transport,
            &Command::SetPwm(ch, 89),
            Duration::from_millis(500),
        )
        .unwrap();

        match result {
            Response::Rpm {
                command_code,
                readings,
            } => {
                assert_eq!(command_code, 0x02, "only a SetPwm ack acknowledges a write");
                assert_eq!(readings.len(), 1);
            }
        }
    }

    #[test]
    fn send_command_gives_up_after_draining_too_many_stale_frames() {
        // The drain is finite. 20 wrong-code frames exceeds MAX_STALE_FRAMES (16),
        // and the error must name the desynchronisation rather than blaming debug
        // output — the two budgets are counted separately on purpose.
        let flood: Vec<Result<String, SerialError>> = (0..20)
            .map(|i| Ok(format!("<02|0{}:0059;>\r\n", i % 10)))
            .collect();
        let mut transport = MockTransport::new(flood);

        let result = send_command(
            &mut transport,
            &Command::ReadAllRpm,
            Duration::from_secs(10),
        );

        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("resynchronise"),
            "expected a desynchronisation error, got: {msg}"
        );
        assert!(
            !msg.contains("debug lines"),
            "stale frames must not be charged to the debug-line budget: {msg}"
        );
    }

    #[test]
    fn send_command_rejects_another_channels_ack_for_the_same_opcode() {
        // Both DEC-301 reviewers found this independently. Every per-channel write
        // carries opcode 0x02, so opcode-only correlation lets a one-frame offset
        // ride an entire `force_all` burst: each write confirmed by its
        // predecessor's ack, and the tenth never acknowledged at all. That is the
        // 105 C emergency path, so the channel check is the load-bearing half.
        let ch = Channel::new(3).unwrap();
        let mut transport = MockTransport::new(vec![
            // Right opcode, WRONG channel — channel 2's ack.
            Ok("<02|02:0059;>\r\n".into()),
            // Channel 3's real ack.
            Ok("<02|03:0080;>\r\n".into()),
        ]);

        let result = send_command(
            &mut transport,
            &Command::SetPwm(ch, 128),
            Duration::from_millis(500),
        )
        .unwrap();

        match result {
            Response::Rpm { readings, .. } => {
                assert_eq!(
                    readings[0].channel, 3,
                    "a write must be acknowledged by an ack for ITS channel"
                );
            }
        }
    }

    #[test]
    fn send_command_accepts_the_real_firmware_set_pwm_ack_shape() {
        // The fixtures all used the idealised `<02|03:0059;>`. Upstream firmware
        // actually writes the PWM byte as %02X with no trailing ';' and no closing
        // '>' — `<02|03:59`. DEC-301 makes decoding that exact frame load-bearing
        // for the first time: before, any frame satisfied a write; now a SetPwm ack
        // that failed to decode would fail EVERY write. 0x59 == 89 == the raw PWM
        // for 35%, which is what the field measurement saw.
        let ch = Channel::new(3).unwrap();
        let mut transport = MockTransport::new(vec![Ok("<02|03:59\r\n".into())]);

        let result = send_command(
            &mut transport,
            &Command::SetPwm(ch, 89),
            Duration::from_millis(500),
        )
        .unwrap();

        match result {
            Response::Rpm {
                command_code,
                readings,
            } => {
                assert_eq!(command_code, 0x02);
                assert_eq!(
                    readings[0],
                    ChannelRpm {
                        channel: 3,
                        rpm: 0x59
                    }
                );
            }
        }
    }
}
