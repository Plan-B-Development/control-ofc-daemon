//! OpenFanController protocol encoding and decoding.
//!
//! Wire format:
//! - Commands: `>CCPPQQ...\n` — all parameters are ASCII hex-encoded byte pairs.
//! - Responses: `<CC|NN:HHHH;NN:HHHH;...;>\r\n` — cmd code, then channel:hex_rpm pairs.
//! - Lines not starting with `<` are debug output and must be discarded.

use crate::error::SerialError;

/// Number of channels on the OpenFanController.
pub const NUM_CHANNELS: u8 = 10;

/// A channel identifier (0–9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Channel(u8);

impl Channel {
    /// Create a channel from a raw index (0–9).
    pub fn new(index: u8) -> Result<Self, SerialError> {
        if index < NUM_CHANNELS {
            Ok(Self(index))
        } else {
            Err(SerialError::Protocol {
                message: format!("channel {index} out of range (0–{})", NUM_CHANNELS - 1),
            })
        }
    }

    /// Raw channel index.
    pub fn index(self) -> u8 {
        self.0
    }
}

/// Commands that can be sent to the OpenFanController.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Read RPM from all channels. Wire: `>00\n`
    ReadAllRpm,
    /// Read RPM from a single channel. Wire: `>01{ch:02X}\n`
    ReadRpm(Channel),
    /// Set PWM on a single channel (switches to open-loop). Wire: `>02{ch:02X}{pwm:02X}\n`
    SetPwm(Channel, u8),
}

impl Command {
    /// The opcode byte identifying this command on the wire.
    ///
    /// It appears **twice** on the link: as the first hex byte pair of what
    /// [`Self::encode`] sends, and echoed back as the `command_code` of the
    /// controller's reply. [`crate::serial::transport::send_command`] correlates
    /// the two, so one definition here is what stops the request encoding and the
    /// reply correlation from drifting apart (the DEC-292 shape — the value that
    /// acts and the value that is reported must be the same value).
    pub fn opcode(&self) -> u8 {
        match self {
            Command::ReadAllRpm => 0x00,
            Command::ReadRpm(_) => 0x01,
            Command::SetPwm(..) => 0x02,
        }
    }

    /// Does `response` answer *this* command?
    ///
    /// **The opcode alone is not enough, and assuming it was is the defect both
    /// DEC-301 reviewers found independently.** Every per-channel write carries
    /// opcode `0x02`, so a one-frame offset *within* a tick's `ch0..ch9` burst —
    /// which is exactly the shape of `force_all` during a thermal emergency — matches on opcode and
    /// slips straight through: each write confirmed by its predecessor's ack, and
    /// the last write never acknowledged at all.
    ///
    /// The firmware echoes the channel it applied the command to, so that is the
    /// discriminator that actually separates one write from the next. Membership
    /// (`mentions_channel`) rather than an exact single-reading match: the shipped
    /// firmware answers a `SetPwm` with exactly one pair, but correlation should
    /// not become brittle about the *count* when what it needs to know is whether
    /// this frame is about the channel we addressed.
    pub fn matches_reply(&self, response: &Response) -> bool {
        if response.command_code() != self.opcode() {
            return false;
        }
        match self {
            Command::ReadAllRpm => true,
            Command::ReadRpm(ch) | Command::SetPwm(ch, _) => response.mentions_channel(ch.index()),
        }
    }

    /// Encode this command into the on-wire ASCII string (including trailing `\n`).
    pub fn encode(&self) -> String {
        let op = self.opcode();
        match self {
            Command::ReadAllRpm => format!(">{op:02X}\n"),
            Command::ReadRpm(ch) => format!(">{:02X}{:02X}\n", op, ch.index()),
            Command::SetPwm(ch, pwm) => {
                format!(">{:02X}{:02X}{:02X}\n", op, ch.index(), pwm)
            }
        }
    }
}

/// A single channel RPM reading from a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRpm {
    pub channel: u8,
    pub rpm: u16,
}

/// Parsed response from the OpenFanController.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// RPM readings (from ReadAllRpm or ReadRpm commands).
    Rpm {
        command_code: u8,
        readings: Vec<ChannelRpm>,
    },
}

impl Response {
    /// The command code the controller echoed in this frame.
    ///
    /// This is the discriminator that says *which request* the frame answers.
    /// It was parsed but never read outside tests until DEC-301; the link had no
    /// request/response correlation and a caller took whatever frame arrived next.
    pub fn command_code(&self) -> u8 {
        match self {
            Response::Rpm { command_code, .. } => *command_code,
        }
    }

    /// Does this frame carry a reading for `channel`?
    ///
    /// The comparison is against the **parsed `u8`**, deliberately. The firmware
    /// emits the channel as `%02X` while [`parse_rpm_pairs`] reads it as decimal —
    /// identical for the whole hardware range (`NUM_CHANNELS == 10`), but only
    /// because the range is single-digit. Comparing strings would bake that
    /// coincidence in.
    pub fn mentions_channel(&self, channel: u8) -> bool {
        match self {
            Response::Rpm { readings, .. } => readings.iter().any(|r| r.channel == channel),
        }
    }
}

/// Build the reply the real controller would send for the encoded `command`.
///
/// **Test-only, and shared on purpose.** Three separate mocks (`serial::transport`,
/// `serial::controller`, `profile_engine`) each answered `<02|00:0400;>` to *every*
/// write, whatever channel it addressed. That was invisible while nothing correlated
/// replies and wrong the moment DEC-301 did — a mock that answers channel 0 to
/// everything cannot distinguish one channel's ack from the next one's, which is the
/// exact defect both DEC-301 reviewers found in the production code. One definition
/// so a fourth mock cannot reintroduce it.
///
/// Wire shape is the firmware's, not the idealised fixture's: the payload byte is
/// `%02X` with no trailing `;` and no closing `>`.
#[cfg(test)]
pub(crate) fn firmware_echo_for(command: &str) -> String {
    let body = command.trim_start_matches('>').trim_end();
    let opcode = &body[0..2];
    match opcode {
        // ReadAllRpm — every channel answers.
        "00" => {
            let pairs: Vec<String> = (0..NUM_CHANNELS).map(|c| format!("{c:02}:04B0")).collect();
            format!("<00|{};>\r\n", pairs.join(";"))
        }
        // ReadRpm / SetPwm — only the addressed channel answers.
        _ => {
            let ch = u8::from_str_radix(&body[2..4], 16).expect("channel hex");
            let payload = body.get(4..6).unwrap_or("04B0");
            format!("<{opcode}|{ch:02}:{payload}\r\n")
        }
    }
}

/// Result of attempting to decode a line from the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedLine {
    /// Successfully parsed response.
    Response(Response),
    /// Line was not a response (debug output, empty, etc.) — should be discarded.
    DebugOutput(String),
}

/// Decode a single line from the controller.
///
/// Lines starting with `<` are parsed as responses.
/// All other lines are returned as `DebugOutput`.
pub fn decode_line(line: &str) -> Result<DecodedLine, SerialError> {
    let line = line.trim_end_matches(['\r', '\n']);

    if !line.starts_with('<') {
        return Ok(DecodedLine::DebugOutput(line.to_string()));
    }

    // Strip leading '<' and optionally trailing '>'
    // Real firmware (Karanovic OpenFan) does NOT send closing '>';
    // test fixtures and docs include it, so accept both formats.
    let after_prefix = line
        .strip_prefix('<')
        .ok_or_else(|| SerialError::Protocol {
            message: format!("response missing opening '<': {line}"),
        })?;
    let inner = after_prefix.strip_suffix('>').unwrap_or(after_prefix);

    // Split on '|' → command_code | data
    let (code_str, data_str) = inner.split_once('|').ok_or_else(|| SerialError::Protocol {
        message: format!("response missing '|' separator: {line}"),
    })?;

    let command_code = u8::from_str_radix(code_str, 16).map_err(|_| SerialError::Protocol {
        message: format!("invalid command code '{code_str}' in response: {line}"),
    })?;

    let readings = parse_rpm_pairs(data_str, line)?;

    Ok(DecodedLine::Response(Response::Rpm {
        command_code,
        readings,
    }))
}

/// Parse `NN:HHHH;NN:HHHH;...;` into a vec of channel RPM readings.
fn parse_rpm_pairs(data: &str, original_line: &str) -> Result<Vec<ChannelRpm>, SerialError> {
    let mut readings = Vec::new();

    for segment in data.split(';') {
        if segment.is_empty() {
            continue;
        }

        let (ch_str, rpm_str) = segment
            .split_once(':')
            .ok_or_else(|| SerialError::Protocol {
                message: format!(
                    "invalid channel:rpm pair '{segment}' in response: {original_line}"
                ),
            })?;

        let channel = ch_str.parse::<u8>().map_err(|_| SerialError::Protocol {
            message: format!("invalid channel number '{ch_str}' in response: {original_line}"),
        })?;

        let rpm = u16::from_str_radix(rpm_str, 16).map_err(|_| SerialError::Protocol {
            message: format!("invalid hex RPM '{rpm_str}' in response: {original_line}"),
        })?;

        readings.push(ChannelRpm { channel, rpm });
    }

    if readings.is_empty() {
        return Err(SerialError::Protocol {
            message: format!("response contains no readings: {original_line}"),
        });
    }

    Ok(readings)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Channel validation ──────────────────────────────────────────

    #[test]
    fn channel_valid_range() {
        for i in 0..10 {
            assert!(Channel::new(i).is_ok());
        }
    }

    #[test]
    fn channel_out_of_range() {
        assert!(Channel::new(10).is_err());
        assert!(Channel::new(255).is_err());
    }

    // ── Encoding golden tests ───────────────────────────────────────

    #[test]
    fn encode_read_all_rpm() {
        assert_eq!(Command::ReadAllRpm.encode(), ">00\n");
    }

    #[test]
    fn encode_read_single_rpm() {
        let ch = Channel::new(5).unwrap();
        assert_eq!(Command::ReadRpm(ch).encode(), ">0105\n");
    }

    #[test]
    fn encode_read_rpm_channel_0() {
        let ch = Channel::new(0).unwrap();
        assert_eq!(Command::ReadRpm(ch).encode(), ">0100\n");
    }

    #[test]
    fn encode_set_pwm() {
        let ch = Channel::new(5).unwrap();
        assert_eq!(Command::SetPwm(ch, 128).encode(), ">020580\n");
    }

    #[test]
    fn encode_set_pwm_max() {
        let ch = Channel::new(0).unwrap();
        assert_eq!(Command::SetPwm(ch, 255).encode(), ">0200FF\n");
    }

    #[test]
    fn encode_set_pwm_zero() {
        let ch = Channel::new(9).unwrap();
        assert_eq!(Command::SetPwm(ch, 0).encode(), ">020900\n");
    }

    // ── Decoding golden tests ───────────────────────────────────────

    #[test]
    fn decode_full_read_all_response() {
        let line = "<00|00:04B0;01:044C;02:0000;03:0BB8;04:0000;05:0000;06:0000;07:0000;08:0000;09:0000;>\r\n";
        let result = decode_line(line).unwrap();
        match result {
            DecodedLine::Response(Response::Rpm {
                command_code,
                readings,
            }) => {
                assert_eq!(command_code, 0x00);
                assert_eq!(readings.len(), 10);
                assert_eq!(
                    readings[0],
                    ChannelRpm {
                        channel: 0,
                        rpm: 0x04B0
                    }
                ); // 1200 RPM
                assert_eq!(
                    readings[1],
                    ChannelRpm {
                        channel: 1,
                        rpm: 0x044C
                    }
                ); // 1100 RPM
                assert_eq!(readings[2], ChannelRpm { channel: 2, rpm: 0 });
                assert_eq!(
                    readings[3],
                    ChannelRpm {
                        channel: 3,
                        rpm: 0x0BB8
                    }
                ); // 3000 RPM
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn decode_single_channel_response() {
        let line = "<01|05:04B0;>";
        let result = decode_line(line).unwrap();
        match result {
            DecodedLine::Response(Response::Rpm {
                command_code,
                readings,
            }) => {
                assert_eq!(command_code, 0x01);
                assert_eq!(readings.len(), 1);
                assert_eq!(
                    readings[0],
                    ChannelRpm {
                        channel: 5,
                        rpm: 0x04B0
                    }
                );
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn decode_set_pwm_response() {
        // After a set command the firmware echoes back a SINGLE-channel frame.
        // Its payload is NOT an RPM: on real hardware it is the raw PWM byte just
        // commanded (DEC-301 measured 35% -> 89, 34% -> 87, 28% -> 71, 29% -> 74,
        // exactly `pwm::percent_to_raw()`). This fixture's 0x04B0 is an idealised
        // value kept only to exercise the parser; the SHAPE (code 0x02, one
        // channel) is the part that matches the wire. Reading this frame as a
        // tachometer value is the defect DEC-301 fixed.
        let line = "<02|05:04B0;>";
        let result = decode_line(line).unwrap();
        match result {
            DecodedLine::Response(Response::Rpm {
                command_code,
                readings,
            }) => {
                assert_eq!(command_code, 0x02);
                assert_eq!(readings.len(), 1);
            }
            _ => panic!("expected Response"),
        }
    }

    // ── Debug output / non-response lines ───────────────────────────

    #[test]
    fn decode_debug_output() {
        let line = "OpenFanController v1.2.3 ready";
        let result = decode_line(line).unwrap();
        assert_eq!(
            result,
            DecodedLine::DebugOutput("OpenFanController v1.2.3 ready".to_string())
        );
    }

    #[test]
    fn decode_empty_line() {
        let result = decode_line("").unwrap();
        assert_eq!(result, DecodedLine::DebugOutput("".to_string()));
    }

    #[test]
    fn decode_whitespace_only() {
        let result = decode_line("\r\n").unwrap();
        assert_eq!(result, DecodedLine::DebugOutput("".to_string()));
    }

    // ── Error cases ─────────────────────────────────────────────────

    #[test]
    fn decode_without_closing_bracket() {
        // Real firmware (Karanovic OpenFan) does not send closing '>'
        let line = "<00|00:04B0;01:044C;";
        let result = decode_line(line).unwrap();
        match result {
            DecodedLine::Response(Response::Rpm {
                command_code,
                readings,
            }) => {
                assert_eq!(command_code, 0x00);
                assert_eq!(readings.len(), 2);
                assert_eq!(readings[0].rpm, 0x04B0);
                assert_eq!(readings[1].rpm, 0x044C);
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn decode_real_firmware_full_response() {
        // Actual response captured from Karanovic OpenFan hardware
        let line = "<00|00:0546;01:0541;02:054A;03:051C;04:04F1;05:055E;06:0548;07:0521;08:0557;09:04DF;\r\n";
        let result = decode_line(line).unwrap();
        match result {
            DecodedLine::Response(Response::Rpm {
                command_code,
                readings,
            }) => {
                assert_eq!(command_code, 0x00);
                assert_eq!(readings.len(), 10);
                assert_eq!(readings[0].rpm, 0x0546); // 1350 RPM
                assert_eq!(readings[9].rpm, 0x04DF); // 1247 RPM
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn decode_missing_separator() {
        let line = "<0000:04B0;>";
        let result = decode_line(line);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'|' separator"));
    }

    #[test]
    fn decode_invalid_command_code() {
        let line = "<ZZ|00:04B0;>";
        let result = decode_line(line);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid command code"));
    }

    #[test]
    fn decode_invalid_channel_number() {
        let line = "<00|XX:04B0;>";
        let result = decode_line(line);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid channel number"));
    }

    #[test]
    fn decode_invalid_hex_rpm() {
        let line = "<00|00:ZZZZ;>";
        let result = decode_line(line);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid hex RPM"));
    }

    #[test]
    fn decode_missing_colon_in_pair() {
        let line = "<00|0004B0;>";
        let result = decode_line(line);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid channel:rpm pair"));
    }

    #[test]
    fn decode_empty_data() {
        let line = "<00|>";
        let result = decode_line(line);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no readings"));
    }

    // ── Roundtrip sanity ────────────────────────────────────────────

    #[test]
    fn all_commands_end_with_newline() {
        let commands = vec![
            Command::ReadAllRpm,
            Command::ReadRpm(Channel::new(0).unwrap()),
            Command::SetPwm(Channel::new(0).unwrap(), 0),
        ];
        for cmd in commands {
            let encoded = cmd.encode();
            assert!(
                encoded.ends_with('\n'),
                "command must end with newline: {encoded}"
            );
            assert!(
                encoded.starts_with('>'),
                "command must start with '>': {encoded}"
            );
        }
    }

    // ── Opcode single-definition invariant (DEC-301) ────────────────

    #[test]
    fn opcode_matches_the_encoded_command_prefix() {
        // `opcode()` is the value `send_command` correlates the reply against, and
        // `encode()` is what goes on the wire. If they ever disagree, correlation
        // rejects every reply to that command and the link dies silently. Pin the
        // pair for every variant so a new command cannot be added with only one
        // half defined.
        let ch = Channel::new(7).unwrap();
        for cmd in [
            Command::ReadAllRpm,
            Command::ReadRpm(ch),
            Command::SetPwm(ch, 0x80),
        ] {
            let encoded = cmd.encode();
            let prefix = &encoded[1..3];
            let from_wire = u8::from_str_radix(prefix, 16).expect("opcode prefix is hex");
            assert_eq!(
                from_wire,
                cmd.opcode(),
                "encode() prefix and opcode() disagree for {cmd:?}"
            );
        }
    }

    #[test]
    fn response_command_code_accessor_reports_the_parsed_code() {
        let line = "<02|05:0059;>";
        match decode_line(line).unwrap() {
            DecodedLine::Response(resp) => assert_eq!(resp.command_code(), 0x02),
            _ => panic!("expected Response"),
        }
    }
}
