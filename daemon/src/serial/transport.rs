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

/// Maximum debug lines to skip before giving up.
/// Normal firmware emits 0–3 debug lines; 50 is generous but finite.
const MAX_DEBUG_LINES: usize = 50;

/// Send a command and read the response, skipping debug output lines.
///
/// Two safety guards prevent infinite loops:
/// 1. Wall-clock deadline: total operation bounded by `timeout`
/// 2. Iteration cap: at most `MAX_DEBUG_LINES` non-response lines skipped
pub fn send_command(
    transport: &mut dyn SerialTransport,
    command: &crate::serial::protocol::Command,
    timeout: Duration,
) -> Result<crate::serial::protocol::Response, SerialError> {
    use crate::serial::protocol::{decode_line, DecodedLine};
    use std::time::Instant;

    transport.write_line(&command.encode())?;

    let deadline = Instant::now() + timeout;
    let mut debug_lines_skipped: usize = 0;

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

        let line = transport.read_line(timeout)?;
        match decode_line(&line)? {
            DecodedLine::Response(response) => return Ok(response),
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
}
