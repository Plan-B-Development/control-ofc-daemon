//! Centralized operational constants for the daemon.
//!
//! Hardware-protocol constants (channel count, max PWM) remain in
//! `serial::protocol` since they are device-specific.  This module holds
//! **operational tuning values** shared across multiple subsystems.
//!
//! # Adding constants here
//! Move a constant here when it:
//! - appears in more than one module, **or**
//! - is a tuning parameter an operator might reasonably want to review.
//!
//! Keep device-specific values (baud rate, probe ranges) in the module
//! that owns the hardware interaction.

use std::time::Duration;

// ── Fan stall detection ──────────────────────────────────────────────

/// PWM percent threshold below which a zero-RPM reading is *not*
/// considered a stall (fan may legitimately be stopped).
pub const STALL_PWM_THRESHOLD: u8 = 20;

// ── SSE streaming ────────────────────────────────────────────────────

/// Maximum lifetime for a single SSE connection before the client
/// must reconnect. Prevents resource leaks from idle connections.
pub const SSE_MAX_LIFETIME: Duration = Duration::from_secs(3600);

/// Interval between SSE heartbeat frames (server-sent `comment` lines).
pub const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Interval between SSE data pushes (sensor + fan snapshot).
pub const SSE_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum number of concurrent SSE client connections.
pub const SSE_MAX_CLIENTS: usize = 5;

// ── OpenFan serial controller ────────────────────────────────────────

/// Duration after which a 0% PWM command is rejected to prevent
/// accidental prolonged motor stop.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(8);

/// Maximum bytes to read per serial line. Prevents unbounded memory growth
/// if a malfunctioning device sends data without a newline terminator.
/// The OpenFanController protocol uses short ASCII frames (< 200 bytes).
pub const MAX_SERIAL_LINE_BYTES: u64 = 4096;

/// Baud rate for the OpenFanController serial connection.
pub const SERIAL_BAUD_RATE: u32 = 115_200;

/// Range of device indices to probe for each serial prefix
/// (e.g. `/dev/ttyACM0` through `/dev/ttyACM9`).
pub const SERIAL_PROBE_RANGE: std::ops::Range<u8> = 0..10;

// ── GPU fan control ──────────────────────────────────────────────────

/// Coalescing threshold for GPU fan writes. Writes within this
/// delta (%) of the last commanded value are suppressed to avoid
/// SMU firmware churn (DEC-070).
pub const GPU_COALESCE_DELTA_PCT: u16 = 5;

/// Number of curve points written when emulating a static fan speed via the
/// PMFW `fan_curve` interface. The fan_curve sysfs file accepts up to 5 indexed
/// `INDEX TEMP SPEED` entries; we write the same speed at every index so the
/// effective curve is flat. **This is not a retry count** — there is no retry
/// logic in the GPU PMFW write path. Reducing this value shortens the curve
/// rather than disabling retries.
pub const GPU_PMFW_NUM_CURVE_POINTS: u8 = 5;

/// Cooldown duration after a GPU fan write failure before retrying
/// the same fan at the same speed.
pub const GPU_FAIL_COOLDOWN: Duration = Duration::from_secs(60);

/// How long `POST /gpu/{gpu_id}/fan/verify` drives a test speed before
/// reading back the applied curve + RPM. Matches the hwmon verify wait
/// (`VERIFY_WAIT_SECONDS` in `api/handlers/hwmon_ctl.rs`): GPU fans spin up
/// quickly, but the tachometer (`fan1_input`) needs several seconds to settle
/// and the firmware needs time to commit the curve, so a shorter wait produced
/// false `no_rpm_effect` verdicts (DEC-101 rationale, applied to GPUs in
/// DEC-120). The GUI must keep its per-call HTTP timeout
/// (`client.py::verify_gpu_fan`) and its `VERIFY_PAUSE_SAFETY_MS` strictly
/// above this value (≥12 s and ≥9 s respectively).
pub const GPU_VERIFY_WAIT_SECONDS: u8 = 6;

// ── Profile engine ───────────────────────────────────────────────────

/// Duration of recent GUI activity that causes the profile engine to
/// defer writes (dual-writer guard — DEC-071/DEC-074).
pub const GUI_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);

/// Temperature deadband (°C) that the profile engine holds the previous
/// curve output across when temperature is falling. Closes the audible
/// parity gap between GUI-driven and headless evaluation. Mirrors the
/// GUI's ``HYSTERESIS_DEADBAND_C`` in ``services/control_loop.py``.
/// See DEC-096.
pub const HYSTERESIS_DEADBAND_C: f64 = 2.0;

// ── Profile engine — no-sensor safety ────────────────────────────────

/// If no CPU temperature sensor is found for this many consecutive
/// cycles, force all fans to `NO_SENSOR_SAFE_PCT`.
pub const NO_SENSOR_CYCLE_THRESHOLD: u32 = 5;

/// PWM percent forced on all fans when no CPU temperature sensor is
/// found for `NO_SENSOR_CYCLE_THRESHOLD` consecutive cycles.
pub const NO_SENSOR_SAFE_PCT: u8 = 40;

// ── Calibration ──────────────────────────────────────────────────────

/// Maximum temperature (°C) during calibration before aborting the
/// sweep. Separate from (and lower than) the safety.rs trigger
/// temperature (105°C) because calibration is a voluntary operation
/// and should abort with more headroom.
pub const CALIBRATION_MAX_TEMP_C: f64 = 85.0;

// Compile-time invariant checks — these fail the build if someone changes a
// constant to an unsafe value.
const _: () = assert!(CALIBRATION_MAX_TEMP_C < 105.0);
const _: () = assert!(NO_SENSOR_SAFE_PCT > 0);
const _: () = assert!(SSE_MAX_CLIENTS > 0);
const _: () = assert!(GPU_COALESCE_DELTA_PCT > 0);
// Slow-spinning GPU tachometers need a multi-second settle window; a too-short
// wait re-introduces the false `no_rpm_effect` verdicts DEC-101/DEC-120 fixed.
const _: () = assert!(GPU_VERIFY_WAIT_SECONDS >= 4);
