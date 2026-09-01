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

/// Settle window (seconds) shared by **both** verify endpoints —
/// `POST /hwmon/{header_id}/verify` and `POST /gpu/{gpu_id}/fan/verify` —
/// between driving a test value and reading the hardware back. This is the
/// single source of truth for that wait; `api/handlers/hwmon_ctl.rs` aliases
/// it locally so the two paths can never drift (DEC-101). Slow-spinning
/// fans/pumps and GPU tachometers (`fan1_input`) need several seconds to
/// settle, and PMFW needs time to commit the curve, so a shorter wait produced
/// false `no_rpm_effect` / `no_rpm_change` verdicts (DEC-101, extended to GPUs
/// in DEC-120). The GUI must keep its per-call HTTP timeouts
/// (`client.py::verify_hwmon_pwm` / `verify_gpu_fan`) and its
/// `VERIFY_PAUSE_SAFETY_MS` strictly above this value (≥12 s and ≥9 s
/// respectively).
pub const VERIFY_WAIT_SECONDS: u8 = 6;

/// Generous deadman backstop for the profile-engine verify pause (DEC-165).
/// The verify handler holds the pause for its whole lifetime via an RAII guard
/// that clears it on drop/panic/cancel — this only fires if that guard somehow
/// never runs. Set well above the worst-case verify duration (the
/// `VERIFY_WAIT_SECONDS` settle plus sysfs I/O and scheduling slack) so it never
/// trips a legitimate verify; it merely bounds a leaked pause.
pub const VERIFY_PAUSE_DEADMAN: Duration = Duration::from_secs(30);

/// How long `POST /gpu/{id}/fan/reset` waits for the GPU write lock (DEC-255)
/// before reporting a conflict.
///
/// Sized to sit well above an engine tick's hold (milliseconds) and well below
/// a `fan/verify` window (multiple seconds), so the two callers it can collide
/// with are distinguishable: wait out a tick, report a conflict for a verify.
/// Also below the GUI's own 5 s client timeout, so the user gets the explanatory
/// 409 rather than an opaque request timeout.
pub const GPU_RESET_LOCK_WAIT: Duration = Duration::from_millis(750);

// ── Profile engine ───────────────────────────────────────────────────

/// Temperature deadband (°C) that the profile engine holds the previous
/// curve output across when temperature is falling — it prevents audible fan
/// oscillation as the temperature hovers around a curve knee. Since the 2.0.0
/// sole-writer cutover (DEC-165) the daemon is the only evaluator and owns this
/// behaviour outright; it was historically kept in parity with the (now-deleted)
/// GUI control loop. See DEC-096.
pub const HYSTERESIS_DEADBAND_C: f64 = 2.0;

/// Maximum consecutive 1 Hz engine ticks the falling-temperature deadband
/// ([`HYSTERESIS_DEADBAND_C`], DEC-096) may hold a control's output before it
/// is force-released for a single tick so the curve re-anchors to the current
/// temperature (DEC-188). Without this valve a temperature that settles just
/// inside the 2°C band pins the pre-settle fan speed indefinitely — the
/// "nothing changes for tens of seconds" steady-state stall. The streak counts
/// only consecutive HELD ticks — any re-evaluation (the reading leaving the
/// band) resets it — so the valve fires solely to release an output that has
/// sat unchanged for the full window and cannot reintroduce oscillation.
/// 30 ticks ≈ 30 s, matching CoolerControl's "fan speed unchanged for 30 s →
/// bypass hysteresis" safety valve.
pub const DEADBAND_MAX_HOLD_CYCLES: u32 = 30;

// ── Manual override + fan-identify deadman (DEC-163 / DEC-166) ────────

/// Time-to-live for a daemon-owned manual override before it reverts to
/// autonomous curve control. Judged on the daemon's monotonic clock — never a
/// client timestamp — so a frozen/crashed/slept GUI cannot strand fans. The
/// GUI renews well inside this window (see `OVERRIDE_RENEW_SECS`); the thermal
///  force backstops regardless. K8s-leader-election-aligned (15 s
/// lease, renewed well within it). No absolute max-duration cap: a live
/// renewing GUI proves the user is present, so deliberate long sessions are
/// not force-reverted.
pub const OVERRIDE_TTL_SECS: u64 = 15;

/// Advisory renewal interval surfaced to the GUI: renew at ~⅓ TTL so ~3
/// attempts land before expiry, robust against transient Qt event-loop stalls.
pub const OVERRIDE_RENEW_SECS: u64 = 5;

// ── Profile engine — no-sensor safety ────────────────────────────────

/// If no CPU temperature sensor is found for this many consecutive
/// cycles, force all OpenFan+hwmon fans to `NO_SENSOR_SAFE_PCT`
/// (GPU fans excluded — DEC-130).
pub const NO_SENSOR_CYCLE_THRESHOLD: u32 = 5;

/// PWM percent forced on all OpenFan+hwmon fans when no CPU temperature
/// sensor is found for `NO_SENSOR_CYCLE_THRESHOLD` consecutive cycles.
pub const NO_SENSOR_SAFE_PCT: u8 = 40;

// ── Profile engine — OpenFan write-failure alerting (audit P3-5) ──────

/// Consecutive write failures — per channel, or across the whole link — before
/// the profile engine's OpenFan backend escalates to a SAFETY-level alert.
/// Tracked per channel (so a persistent single-channel fault among healthy
/// channels still trips, rather than being masked by the others resetting a
/// shared counter) and separately for the whole-link "serial down" case; each
/// fires once at the exact threshold (edge-triggered) to avoid 1 Hz log spam.
pub const OPENFAN_FAIL_ALERT_THRESHOLD: u32 = 5;

// ── Profile engine — hwmon write-failure log throttle (DEC-199) ───────

/// A persistent motherboard-fan (hwmon) write failure — canonically EROFS
/// ("Read-only file system", os error 30) when the systemd sandbox's
/// `ReadWritePaths=` carve-out does not cover the real `/sys/devices` inode
/// (DEC-199) — is logged once on the first failing tick, then only every
/// `HWMON_FAIL_SUMMARY_INTERVAL` ticks (≈ that many seconds at the 1 Hz loop)
/// as a "still failing" summary, so a stuck header cannot spam journald at
/// 1 Hz. A subsequent successful write clears the streak and logs an INFO
/// recovery line. (The OpenFan and GPU backends already throttle their own
/// write-failure logs — edge-triggered alerting and a 60 s fail-cooldown
/// respectively — so this covers the one remaining un-throttled backend.)
pub const HWMON_FAIL_SUMMARY_INTERVAL: u32 = 300;

// ── Sensor polling — descriptor cache (DEC-133) ──────────────────────

/// Consecutive failed value-reads for a single cached sensor descriptor
/// before the polling loop re-runs discovery. Catches devices unbound
/// mid-session (driver unload, USB detach) without re-enumerating sysfs
/// every tick.
pub const SENSOR_READ_FAIL_REDISCOVER_STREAK: u32 = 5;

// ── Thermal emergency ────────────────────────────────────────────────

/// CPU temperature (°C) at which the thermal emergency latches, forcing every
/// OpenFan channel and writable hwmon header to 100%.
///
/// **This is the single source for the trip point (DEC-292).** It had been
/// written out four times — here in a compile-time assert, in
/// `ThermalSafetyRule::new`, in the `/diagnostics/hardware` response, and in that
/// rule's own doc comment. The dangerous copy was the API response: it was a bare
/// literal, so moving the trip point would have left the daemon *reporting* one
/// value while *acting* on another, with the assert still guarding the old one.
/// Everything that needs this number now reads it from here.
///
/// **This is the FLOOR and the fallback, not the only value the daemon uses**
/// (DEC-308). Where the kernel publishes the CPU's own design ceiling, the engine
/// derives a higher trigger per tick — see
/// [`crate::profile_engine::effective_trigger_c`]. This constant is what a
/// machine gets when it publishes nothing usable, and the derivation is
/// raise-only, so no machine ever trips below this value.
///
/// **A single global value cannot be correct for every CPU.** Design ceilings
/// differ by family: AMD Zen 4/5 desktop ~95 °C, Intel 12th-14th gen desktop
/// 100 °C, Intel Core Ultra 200S (Arrow Lake) desktop ~105 °C, Intel Core Ultra
/// mobile ~110 °C. A part is *designed* to sit at its ceiling under sustained
/// load, so 105 fires during normal operation on the families at or above it: an
/// Arrow Lake desktop can latch the emergency while healthy and never release
/// (release needs <=[`THERMAL_EMERGENCY_RELEASE_C`], which a part holding Tjmax
/// never reaches), and a Core Ultra laptop sits 5 °C above the trigger at its
/// own ceiling.
///
/// **⚠ Those family figures are third-party, and the attempt to source them
/// primarily FAILED — 2026-09-01.** An earlier revision of this comment called
/// them "confirmed from Intel datasheets" via `D1-h`; that was an overclaim and
/// is retracted. Intel's own EDC datasheet pages for the Core Ultra 200S
/// (document 832586) render their specification tables via JavaScript and return
/// only a table of contents to a fetch; the Edge overview PDF is an image-only
/// scan; and ARK returns 403 to automated requests. Every figure that could
/// actually be read traces to secondary reporting. They are recorded here as
/// *motivation*, and deliberately **not** encoded as a table — DEC-308 reads the
/// ceiling from the running silicon instead, precisely so that no unverified
/// number becomes a safety threshold. TjMax is also user-adjustable on unlocked
/// parts, which a static table could not track either.
pub const THERMAL_EMERGENCY_TRIGGER_C: f64 = 105.0;

/// Headroom added to a CPU's own reported design ceiling to get its emergency
/// trigger (DEC-308).
///
/// [SAFETY] The trigger cannot be the ceiling itself: a part is *designed* to sit
/// at Tjmax under sustained load and throttles itself there, so a trigger at the
/// ceiling would fire on every healthy loaded machine — the exact false positive
/// this derivation exists to remove. Five degrees past the point the CPU is
/// already throttling means cooling has failed, not that the machine is busy.
///
/// Chosen at 5 because it leaves mainstream Intel 12th-14th gen desktop
/// (ceiling ~100) at exactly the historical 105, so the overwhelmingly common
/// case sees no change at all, while the two families that are genuinely broken
/// at 105 are lifted clear of their ceilings.
pub const THERMAL_TRIGGER_MARGIN_C: f64 = 5.0;

/// Hard ceiling on the derived emergency trigger (DEC-308).
///
/// [SAFETY] A `tempN_crit` is hardware-reported, and hardware lies. Without a cap
/// a chip reporting an absurd crit would push the trigger past the point the CPU
/// self-protects at (Intel documents THERMTRIP at approximately 130 °C), which
/// would silently disable the emergency altogether — strictly worse than the
/// single global value this replaces. 115 is the hottest design ceiling known to
/// this project (Core Ultra mobile, ~110) plus [`THERMAL_TRIGGER_MARGIN_C`], so
/// the derived trigger is always within `[105, 115]`.
pub const THERMAL_TRIGGER_MAX_C: f64 = 115.0;

/// CPU temperature (°C) at which a latched thermal emergency releases into its
/// recovery floor. Deliberately far below the trigger: the gap is the hysteresis
/// that stops the emergency flapping. See [`THERMAL_EMERGENCY_TRIGGER_C`].
pub const THERMAL_EMERGENCY_RELEASE_C: f64 = 80.0;

// ── Calibration ──────────────────────────────────────────────────────

/// Maximum temperature (°C) during calibration before aborting the
/// sweep. Separate from (and lower than) [`THERMAL_EMERGENCY_TRIGGER_C`]
/// because calibration is a voluntary operation and should abort with
/// more headroom.
pub const CALIBRATION_MAX_TEMP_C: f64 = 85.0;

// Compile-time invariant checks — these fail the build if someone changes a
// constant to an unsafe value.
const _: () = assert!(CALIBRATION_MAX_TEMP_C < THERMAL_EMERGENCY_TRIGGER_C);
const _: () = assert!(THERMAL_EMERGENCY_RELEASE_C < THERMAL_EMERGENCY_TRIGGER_C);
const _: () = assert!(NO_SENSOR_SAFE_PCT > 0);
// DEC-308: the derived trigger is clamped into [TRIGGER, TRIGGER_MAX], so the
// window must be non-empty and must never dip under the calibration abort.
const _: () = assert!(THERMAL_TRIGGER_MAX_C >= THERMAL_EMERGENCY_TRIGGER_C);
const _: () = assert!(THERMAL_TRIGGER_MARGIN_C > 0.0);
const _: () = assert!(DEADBAND_MAX_HOLD_CYCLES > 0);
// The renew interval must leave room for ~3 attempts inside the TTL, and the
// TTL must be non-trivial — a too-tight window would drop legitimate overrides.
const _: () = assert!(OVERRIDE_RENEW_SECS > 0);
const _: () = assert!(OVERRIDE_RENEW_SECS * 3 <= OVERRIDE_TTL_SECS);
const _: () = assert!(GPU_COALESCE_DELTA_PCT > 0);
// Slow-spinning fans/pumps and GPU tachometers need a multi-second settle
// window; a too-short wait re-introduces the false `no_rpm_effect` verdicts
// DEC-101/DEC-120 fixed.
const _: () = assert!(VERIFY_WAIT_SECONDS >= 4);
