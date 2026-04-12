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

/// Convert a PWM percent (0–100) to a raw PWM value (0–255).
fn percent_to_raw(percent: u8) -> u8 {
    ((percent as u16 * 255 + 50) / 100) as u8
}

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

        let effective_pct = self.apply_safety(channel, pwm_percent)?;
        let ch_ctrl = &self.channels[channel as usize];

        // Coalesce: skip if same as last commanded
        if ch_ctrl.last_commanded_pct == Some(effective_pct) {
            return Ok(SetPwmResult {
                channel,
                pwm_percent: effective_pct,
                coalesced: true,
            });
        }

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

    /// Set PWM on all channels. `pwm_percent` is 0–100.
    pub fn set_pwm_all(&mut self, pwm_percent: u8) -> Result<SetPwmAllResult, FanControlError> {
        if pwm_percent > 100 {
            return Err(FanControlError::Validation(format!(
                "pwm_percent {pwm_percent} out of range (0–100)"
            )));
        }

        // Apply stop-timeout safety to all channels (checks 0% duration limit)
        if pwm_percent == 0 {
            for ch in 0..NUM_CHANNELS {
                self.apply_safety(ch, 0)?;
            }
        }
        let effective_pct = pwm_percent;

        let raw = percent_to_raw(effective_pct);
        let cmd = Command::SetAllPwm(raw);

        let mut transport = self.transport.lock();

        send_command(&mut **transport, &cmd, self.timeout).map_err(FanControlError::Serial)?;

        drop(transport);

        // Update all channel tracking
        let now = Instant::now();
        for ch in 0..NUM_CHANNELS {
            self.channels[ch as usize].last_commanded_pct = Some(effective_pct);
            if effective_pct == 0 {
                if self.channels[ch as usize].stop_started_at.is_none() {
                    self.channels[ch as usize].stop_started_at = Some(now);
                }
            } else {
                self.channels[ch as usize].stop_started_at = None;
            }
        }

        // Update cache (store percent, not raw — GUI displays this as "%")
        self.cache.set_openfan_commanded_pwm_all(effective_pct);

        Ok(SetPwmAllResult {
            pwm_percent: effective_pct,
            channels_affected: NUM_CHANNELS,
        })
    }

    /// Set target RPM on a single channel (closed-loop mode).
    pub fn set_target_rpm(
        &mut self,
        channel: u8,
        target_rpm: u16,
    ) -> Result<SetRpmResult, FanControlError> {
        if channel >= NUM_CHANNELS {
            return Err(FanControlError::Validation(format!(
                "channel {channel} out of range (0–{})",
                NUM_CHANNELS - 1
            )));
        }
        // Conservative max RPM cap (Noctua NF-A14 IPPC max is 3000)
        if target_rpm > 5000 {
            return Err(FanControlError::Validation(format!(
                "target_rpm {target_rpm} exceeds maximum (5000)"
            )));
        }

        let ch = Channel::new(channel).map_err(FanControlError::Serial)?;
        let cmd = Command::SetTargetRpm(ch, target_rpm);

        let mut transport = self.transport.lock();

        send_command(&mut **transport, &cmd, self.timeout).map_err(FanControlError::Serial)?;

        drop(transport);

        // Clear PWM tracking — switching to closed-loop mode
        self.channels[channel as usize].last_commanded_pct = None;
        self.channels[channel as usize].stop_started_at = None;

        Ok(SetRpmResult {
            channel,
            target_rpm,
        })
    }

    /// Apply safety rules: stop timeout and minimum PWM floor.
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

/// Result of an all-channel PWM set operation.
#[derive(Debug, Clone)]
pub struct SetPwmAllResult {
    pub pwm_percent: u8,
    pub channels_affected: u8,
}

/// Result of a target RPM set operation.
#[derive(Debug, Clone)]
pub struct SetRpmResult {
    pub channel: u8,
    pub target_rpm: u16,
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
                    written: written.clone(),
                },
                written,
            )
        }

        fn with_ok_responses(count: usize) -> (Self, Arc<parking_lot::Mutex<Vec<String>>>) {
            let responses = (0..count)
                .map(|_| Ok("<02|00:0400;>\r\n".to_string()))
                .collect();
            Self::with_responses(responses)
        }
    }

    impl SerialTransport for MockTransport {
        fn write_line(&mut self, data: &str) -> Result<(), SerialError> {
            self.written.lock().push(data.to_string());
            Ok(())
        }

        fn read_line(&mut self, _timeout: Duration) -> Result<String, SerialError> {
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

    // ── Percent to raw conversion ───────────────────────────────────

    #[test]
    fn percent_to_raw_boundaries() {
        assert_eq!(percent_to_raw(0), 0);
        assert_eq!(percent_to_raw(100), 255);
        assert_eq!(percent_to_raw(50), 128); // (50*255+50)/100 = 12800/100 = 128
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
    fn set_pwm_zero_rejected_after_stop_timeout() {
        // After constants::STOP_TIMEOUT (8s) at 0%, further 0% commands are rejected.
        let (transport, _written) = MockTransport::with_ok_responses(2);
        let mut ctrl = make_controller(transport);

        // First 0% succeeds and starts the stop timer
        ctrl.set_pwm(0, 0).unwrap();
        assert!(ctrl.channels[0].stop_started_at.is_some());

        // Backdate stop_started_at to simulate 8 seconds passing
        ctrl.channels[0].stop_started_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(9));

        // Next 0% should be rejected (stop timeout exceeded)
        let err = ctrl.set_pwm(0, 0).unwrap_err();
        match err {
            FanControlError::Validation(msg) => assert!(msg.contains("stop timeout")),
            _ => panic!("expected stop timeout validation error"),
        }

        // Non-zero PWM should still work (clears the stop timer)
        let result = ctrl.set_pwm(0, 50).unwrap();
        assert_eq!(result.pwm_percent, 50);
        assert!(ctrl.channels[0].stop_started_at.is_none());
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

    // ── Set PWM all channels ────────────────────────────────────────

    #[test]
    fn set_pwm_all_valid() {
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        let result = ctrl.set_pwm_all(75).unwrap();
        assert_eq!(result.pwm_percent, 75);
        assert_eq!(result.channels_affected, 10);
    }

    #[test]
    fn set_pwm_all_golden_frame() {
        let (transport, written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        ctrl.set_pwm_all(100).unwrap();

        // 100% → raw 255 = 0xFF
        let written = written.lock();
        assert_eq!(*written, vec![">03FF\n"]);
    }

    #[test]
    fn set_pwm_all_invalid_percent() {
        let (transport, _written) = MockTransport::with_ok_responses(0);
        let mut ctrl = make_controller(transport);

        let err = ctrl.set_pwm_all(101).unwrap_err();
        match err {
            FanControlError::Validation(_) => {}
            _ => panic!("expected validation error"),
        }
    }

    #[test]
    fn set_pwm_all_accepts_low_values() {
        // No floor clamping — thermal safety handled by ThermalSafetyRule
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        let result = ctrl.set_pwm_all(5).unwrap();
        assert_eq!(result.pwm_percent, 5); // no clamping
    }

    #[test]
    fn set_pwm_all_updates_cache() {
        let cache = Arc::new(StateCache::new());
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = FanController::new(
            Box::new(transport),
            cache.clone(),
            Duration::from_millis(500),
        );

        ctrl.set_pwm_all(60).unwrap();

        let snap = cache.snapshot();
        for ch in 0..10u8 {
            let fan = snap.openfan_fans.get(&ch).unwrap();
            assert_eq!(fan.last_commanded_pwm, Some(60));
        }
    }

    // ── Set target RPM ──────────────────────────────────────────────

    #[test]
    fn set_target_rpm_valid() {
        let (transport, _written) =
            MockTransport::with_responses(vec![Ok("<04|05:03E8;>\r\n".into())]);
        let mut ctrl = make_controller(transport);

        let result = ctrl.set_target_rpm(5, 1000).unwrap();
        assert_eq!(result.channel, 5);
        assert_eq!(result.target_rpm, 1000);
    }

    #[test]
    fn set_target_rpm_golden_frame() {
        let (transport, written) =
            MockTransport::with_responses(vec![Ok("<04|05:03E8;>\r\n".into())]);
        let mut ctrl = make_controller(transport);

        ctrl.set_target_rpm(5, 1000).unwrap();

        // channel 5 = 0x05, RPM 1000 = 0x03E8
        let written = written.lock();
        assert_eq!(*written, vec![">040503E8\n"]);
    }

    #[test]
    fn set_target_rpm_invalid_channel() {
        let (transport, _written) = MockTransport::with_ok_responses(0);
        let mut ctrl = make_controller(transport);

        let err = ctrl.set_target_rpm(10, 1000).unwrap_err();
        match err {
            FanControlError::Validation(msg) => assert!(msg.contains("out of range")),
            _ => panic!("expected validation error"),
        }
    }

    #[test]
    fn set_target_rpm_exceeds_max() {
        let (transport, _written) = MockTransport::with_ok_responses(0);
        let mut ctrl = make_controller(transport);

        let err = ctrl.set_target_rpm(0, 6000).unwrap_err();
        match err {
            FanControlError::Validation(msg) => assert!(msg.contains("exceeds maximum")),
            _ => panic!("expected validation error"),
        }
    }

    #[test]
    fn set_target_rpm_zero_is_valid() {
        let (transport, _written) =
            MockTransport::with_responses(vec![Ok("<04|00:0000;>\r\n".into())]);
        let mut ctrl = make_controller(transport);

        let result = ctrl.set_target_rpm(0, 0).unwrap();
        assert_eq!(result.target_rpm, 0);
    }

    #[test]
    fn set_target_rpm_clears_pwm_tracking() {
        let (transport, _written) = MockTransport::with_ok_responses(1);
        let mut ctrl = make_controller(transport);

        // Set PWM first
        ctrl.set_pwm(0, 50).unwrap();
        assert!(ctrl.channels[0].last_commanded_pct.is_some());

        // Switch to RPM mode — replace transport with new responses
        let (new_transport, _new_written) =
            MockTransport::with_responses(vec![Ok("<04|00:03E8;>\r\n".into())]);
        *ctrl.transport.lock() = Box::new(new_transport);

        ctrl.set_target_rpm(0, 1000).unwrap();
        assert!(ctrl.channels[0].last_commanded_pct.is_none());
    }
}
