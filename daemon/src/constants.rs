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

/// How far a pump-role header is shifted from its baseline duty during a
/// pump-safe identify (DEC-311, AIO-MB Phase 1), in percentage points.
///
/// Large enough that the RPM change is audible and clearly visible in the
/// reading, small enough to stay well inside the pump's operating range. The
/// perturbation prefers to move **upward** so it never walks a pump toward its
/// stall floor; the downward direction is used only when there is no headroom,
/// and is clamped at `HARD_PUMP_CPU_FLOOR_PCT`.
pub const IDENTIFY_PUMP_DELTA_PCT: u8 = 25;

/// Baseline assumed for a pump-safe identify when nothing has been commanded
/// yet (`last_commanded_pwm` absent). A typical pump idle duty; the computed
/// target is clamped to the pump floor regardless, so this only affects which
/// *direction* the perturbation takes.
///
/// Reached less often than it looks. For hwmon, `polling.rs` fills
/// `last_commanded_pwm` from the sysfs read-back, so it is usually present even
/// for a header no profile drives. This is the genuine gap: a header whose PWM
/// read failed, or the first poll cycle after boot.
///
/// It deliberately does **not** claim to cover "a fan the active profile does
/// not drive". For that fan the identify overlay rewrites nothing at all —
/// there is no command to rewrite — so no baseline of any value would help. That
/// is register row `AIO1-b`, out of scope for DEC-311.
pub const IDENTIFY_PUMP_BASELINE_FALLBACK_PCT: u8 = 60;

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

/// Poll intervals a cached hwmon FAN entry may go unrefreshed before it is
/// evicted (OFS-m).
///
/// `update_hwmon_fans` only ever inserts, and `read_hwmon_fan_states` drops any
/// header with neither a readable RPM nor a readable PWM — so a header whose chip
/// unbinds mid-session left a cached entry whose `updated_at` stopped advancing
/// while `/fans` kept publishing it with an `age_ms` climbing without bound.
///
/// Deliberately judged on the entry's AGE rather than on a poll-failure streak.
/// The poll loop is not the only writer of this map: `HwmonPwmController::set_pwm`
/// reads the header's own RPM and inserts a fresh entry on every engine write. A
/// streak counted on poll failures alone would therefore evict genuinely-current
/// entries out from under an active profile and re-insert them on the next write,
/// flapping the fan in and out of `/fans` at 1 Hz. Age answers the question the
/// defect actually asks — "has anything refreshed this?" — and is unanimous
/// across both writers.
pub const HWMON_FAN_STALE_INTERVALS: u32 = 5;

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

// ── PWM/RPM characterisation (AIO-MB Phase 3) ────────────────────────

/// Default sweep points for `POST /hwmon/{id}/characterize`, from
/// `AIO-Phase3.md`. Clamped per-header before use — a pump-protected header
/// never sees a point below [`crate::profile::HARD_PUMP_CPU_FLOOR_PCT`].
pub const CHARACTERIZATION_DEFAULT_POINTS: [u8; 8] = [30, 40, 50, 60, 70, 80, 90, 100];

/// [SAFETY] The floor under every characterisation point on a NON-pump header.
///
/// **0% is unreachable through the characterisation endpoint for any header**,
/// and that is the invariant the whole diagnostic is built on: one flat clamp
/// `[max(CHARACTERIZATION_MIN_PCT, header floor) .. 100]`, never a
/// role-conditional branch. `AIO-Phase3.md` forbids only *automatically*
/// including 0%, so a lower non-pump floor was available and was declined —
/// a flat rule is testable in one assertion, where a role-conditional one is
/// only as correct as `header_is_pump_protected` is on every board.
///
/// **Scope, precisely (`AUD3-l`).** This governs *commanded sweep points*: none
/// can be 0, for any header. The RESTORE is governed separately, by
/// `RestoreOnDrop::restore_floor` and `hwmon_ctl::restore_duty` — a pump is
/// never restored below its floor, while an ordinary fan is put back exactly
/// where it was found, 0 included, because that is a restore rather than a
/// command. Until DEC-322 the restore had no floor at all, and this sentence
/// was read as covering it.
///
/// The cost (no chassis-fan stall/start-point discovery) is accepted and
/// recorded as `AIO3-c` in `DECISIONS_OPEN_ITEMS.md`. Do not "fix" it by
/// branching here; the reserved route is a per-device policy table.
pub const CHARACTERIZATION_MIN_PCT: u8 = 20;

/// Cap on caller-supplied sweep points. Mirrors the calibrate sweep's
/// `steps 2..=20`, so the worst-case run (20 x 15 s) stays the same order as
/// calibration's ~325 s and inside its accepted engine-pause precedent.
pub const CHARACTERIZATION_MAX_POINTS: usize = 20;

/// Settle window per point, and its clamp. Default matches
/// [`VERIFY_WAIT_SECONDS`] — raised to 6 s for exactly this reason (DEC-101):
/// slow-spinning pumps need >3 s or they report a false `no_response`.
///
/// The maximum is load-bearing for DEC-296: the pause deadman is renewed once
/// per point, so the renewal interval is `settle + I/O`. At 15 s that leaves
/// ample margin inside [`VERIFY_PAUSE_DEADMAN`] (30 s); raising it past ~28 s
/// would let a healthy sweep time its own pause out.
pub const CHARACTERIZATION_DEFAULT_SETTLE_S: u64 = VERIFY_WAIT_SECONDS as u64;
pub const CHARACTERIZATION_SETTLE_MIN_S: u64 = 2;
pub const CHARACTERIZATION_SETTLE_MAX_S: u64 = 15;

/// Sub-sampling interval used to measure `first_change_ms` within a settle
/// window. Deliberately no early exit: the sweep always holds the full settle,
/// so a run can only ever be shorter than its budgeted pause, never longer.
pub const CHARACTERIZATION_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Readback tolerance in percentage points. A duty is stored as a 0-255 raw
/// value, so a round-trip through `percent -> raw -> percent` can legitimately
/// land one point away; 2 absorbs that without hiding a real clamp.
pub const CHARACTERIZATION_READBACK_TOLERANCE_PCT: u8 = 2;

/// Absolute RPM noise floor for "did this reading move?". Below this, tach
/// jitter on a healthy fan would read as a response.
pub const CHARACTERIZATION_RPM_NOISE_FLOOR: u16 = 50;

/// Minimum absolute RPM spread across the whole sweep before it is called
/// `responsive`. Paired with a >20% relative test (the same rule
/// `classify_verify_result` uses) so a slow pump is not a false negative:
/// 20% of a 900 RPM idle is 180, but 20% of a 300 RPM idle is only 60, which
/// tach noise alone can cover.
pub const CHARACTERIZATION_RESPONSIVE_MIN_DELTA_RPM: u16 = 150;

// ── PWM behaviour characterisation (AIO Phase 8 Batch 2, DEC-334) ────

/// Unique duties a **bidirectional** plan may hold.
///
/// [`CHARACTERIZATION_MAX_POINTS`] is a cap on **walked steps**, not on unique
/// duties, and DEC-334 deliberately left it where it was: the worst-case run,
/// the engine write-pause it budgets, and the session poll deadline derived from
/// it all stay exactly as tested. A bidirectional walk is `2n - 1` steps (the
/// turn-around duty is walked once, not twice — see `resolve_sweep_plan`), so
/// the unique-duty budget is the largest `n` with `2n - 1 <= MAX_POINTS`.
pub const CHARACTERIZATION_MAX_UNIQUE_BIDIRECTIONAL: usize =
    CHARACTERIZATION_MAX_POINTS.div_ceil(2);

/// [SAFETY] The renewal cadence **inside** the stability dwell loop.
///
/// **This constant exists because the settle-window assert could not be reused,
/// and copying it would have been silently wrong (DEC-333).** The existing
/// invariant is `CHARACTERIZATION_SETTLE_MAX_S * 2 <= VERIFY_PAUSE_DEADMAN`,
/// which reads "one renewal per settle window, 2x margin" — and it holds at
/// `15 * 2 == 30`, i.e. with **zero** headroom. A dwell renewing only once per
/// point would therefore overrun the pause deadman at *any* dwell length, and at
/// [`STABILITY_MAX_S`] it would also outlive the 60 s hwmon lease TTL, which is
/// the failure recorded a few lines below: a sweep that blew its lease could not
/// even restore the header.
///
/// So the dwell renews on its own cadence and the assert below is derived from
/// **this** value, not from the settle window. Changing the dwell length cannot
/// break the deadman; changing this can, and the build says so.
pub const STABILITY_RENEW_INTERVAL_S: u64 = 5;

/// Dwell length when stability sampling is requested, and its clamp. Off by
/// default: `stability_seconds` absent or `0` means no dwell at all, and the
/// statistics are then derived from the samples the settle window already takes.
pub const STABILITY_DEFAULT_S: u64 = 20;
pub const STABILITY_MIN_S: u64 = 5;
pub const STABILITY_MAX_S: u64 = 60;

/// How many walked steps may carry a dwell. The daemon picks them (lowest, mid,
/// highest of the plan) rather than the client, so the run's cost is bounded by
/// the daemon regardless of what is asked for.
pub const STABILITY_MAX_POINTS: usize = 3;

/// Retained samples below which stability statistics are `insufficient_data`.
///
/// A bare settle window yields `settle_s * 2` samples (500 ms cadence), so the
/// 2 s minimum settle gives 4 and the 6 s default gives 12. Set at 8 so the
/// default settle qualifies and the shortest one honestly does not — `AIO-Phase8
/// Batch 2 §4` requires `INSUFFICIENT_DATA` to be a real outcome rather than a
/// value invented from three readings.
pub const STABILITY_MIN_SAMPLES: usize = 8;

/// Coefficient-of-variation thresholds for the `§4` stability classification:
/// `stable` at or below the first, `variable` at or below the second,
/// `unstable` above it.
pub const STABILITY_STABLE_MAX_CV_PCT: f64 = 3.0;
pub const STABILITY_VARIABLE_MAX_CV_PCT: f64 = 10.0;

/// Outlier threshold, as an Iglewicz-Hoaglin **modified z-score** (median and
/// median-absolute-deviation based). Outliers are **counted and reported**,
/// never silently dropped from the raw record (`§9`: do not overwrite raw
/// evidence with derived values).
///
/// **This is deliberately NOT a multiple of the standard deviation, and the
/// first draft that was got it wrong in a way no test would have shown.** With
/// a population σ the largest possible z-score in a window of `n` samples is
/// `(n-1)/√n`: at the 12 readings a default 6 s settle produces that ceiling is
/// **3.18**, and at 10 readings it is **2.85** — so a `3σ` rule is arithmetically
/// incapable of flagging anything at the sample counts this feature actually
/// collects, and the field would have always read 0 and looked like evidence of
/// a clean tach. A single extreme reading also inflates σ enough to hide itself
/// (masking), which is the same defect from the other direction. The median and
/// MAD move by almost nothing when one reading is wild, so this bound is
/// reachable — `an_outlier_is_detectable_at_the_sample_count_a_default_settle_produces`
/// pins exactly that, because a bound you cannot reach is not a bound (DEC-320).
pub const STABILITY_OUTLIER_MODIFIED_Z: f64 = 3.5;

/// `§5` settling criterion: reported RPM must stay within this band of the
/// rolling median for [`SETTLING_HOLD_SAMPLES`] consecutive samples.
pub const SETTLING_BAND_PCT: f64 = 5.0;
pub const SETTLING_HOLD_SAMPLES: usize = 4;

/// `§2` hysteresis: the rising/falling gap at a shared duty, as a percentage of
/// the sweep's observed RPM span, below which the difference is reported as
/// noise rather than hysteresis.
pub const HYSTERESIS_MIN_PCT: f64 = 5.0;

/// `§3` plateau detection. A plateau needs at least this many consecutive
/// duties whose RPM all sit within [`PLATEAU_BAND_PCT`] of the run's span —
/// "use tolerance bands and multiple observations rather than declaring a
/// plateau from a single equal reading".
pub const PLATEAU_MIN_POINTS: usize = 3;
pub const PLATEAU_BAND_PCT: f64 = 3.0;

/// `§6` learned-range tolerance: how far an observation may sit outside a
/// learned min/max band before it is reported as `outside_learned_range`.
/// Deliberately wide — this feeds cautious wording, never a fault verdict.
pub const LEARNED_RANGE_TOLERANCE_PCT: f64 = 25.0;

/// Learned-response store (`{state_dir}/pwm_baselines.json`), mirroring the
/// control-path store's bounds exactly. Ingest-bounded per string so that "too
/// large to read" can only mean "written by a newer version" (DEC-320).
pub const PWM_BASELINES_MAX_ENTRIES: usize = 64;
pub const PWM_BASELINE_MAX_TEXT_BYTES: usize = VALIDATION_MAX_TEXT_FIELD_BYTES;
pub const PWM_BASELINE_MAX_POINTS: usize = CHARACTERIZATION_MAX_POINTS;
/// Derived, not guessed: 4 text fields plus one `{duty, min, max}` triple per
/// retained point, each generously padded for JSON punctuation and key names.
pub const PWM_BASELINE_RECORD_MAX_BYTES: usize = {
    let text_fields = 4;
    let per_point = 64;
    text_fields * (PWM_BASELINE_MAX_TEXT_BYTES + 128) + PWM_BASELINE_MAX_POINTS * per_point + 512
};
pub const PWM_BASELINES_MAX_BYTES: u64 =
    (PWM_BASELINES_MAX_ENTRIES * PWM_BASELINE_RECORD_MAX_BYTES + 8192) as u64;

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
// AIO-MB Phase 3: 0% must be unreachable through characterisation, the settle
// clamp must be a non-empty range, and the whole per-point window (settle plus
// slack) must fit inside the pause deadman that is renewed once per point.
const _: () = assert!(CHARACTERIZATION_MIN_PCT > 0);
const _: () = assert!(CHARACTERIZATION_SETTLE_MIN_S <= CHARACTERIZATION_SETTLE_MAX_S);
const _: () = assert!(CHARACTERIZATION_DEFAULT_SETTLE_S >= CHARACTERIZATION_SETTLE_MIN_S);
const _: () = assert!(CHARACTERIZATION_DEFAULT_SETTLE_S <= CHARACTERIZATION_SETTLE_MAX_S);
const _: () = assert!(CHARACTERIZATION_SETTLE_MAX_S * 2 <= VERIFY_PAUSE_DEADMAN.as_secs());
const _: () = assert!(CHARACTERIZATION_MAX_POINTS > 0);

// ── AIO Phase 8 Batch 2 (DEC-334) ────────────────────────────────────
// [SAFETY] The dwell's deadman/lease invariant, RE-DERIVED from the dwell's own
// renewal cadence rather than copied from the settle window's. Copying it would
// have kept the arithmetic and changed its meaning — the settle assert holds at
// exactly 30 == 30, so a dwell renewing once per point breaks the pause deadman
// at any dwell length, and at STABILITY_MAX_S it outlives the lease TTL too.
const _: () = assert!(STABILITY_RENEW_INTERVAL_S > 0);
const _: () = assert!(STABILITY_RENEW_INTERVAL_S * 2 <= VERIFY_PAUSE_DEADMAN.as_secs());
const _: () =
    assert!(STABILITY_RENEW_INTERVAL_S * 2 <= crate::hwmon::lease::DEFAULT_LEASE_TTL.as_secs());
// The renewal must be able to fire at least twice inside the longest dwell, or
// the cadence is decorative and the dwell is really renewing once per point.
const _: () = assert!(STABILITY_MAX_S >= STABILITY_RENEW_INTERVAL_S * 2);
const _: () = assert!(STABILITY_MIN_S <= STABILITY_DEFAULT_S);
const _: () = assert!(STABILITY_DEFAULT_S <= STABILITY_MAX_S);
const _: () = assert!(STABILITY_MAX_POINTS > 0);
const _: () = assert!(STABILITY_MAX_POINTS <= CHARACTERIZATION_MAX_POINTS);
// A bare settle window must be able to clear the sample threshold at the DEFAULT
// settle, or every point reports insufficient_data and the feature never fires.
const _: () = assert!(CHARACTERIZATION_DEFAULT_SETTLE_S * 2 >= STABILITY_MIN_SAMPLES as u64);
const _: () = assert!(STABILITY_MIN_SAMPLES > 2);
// A bidirectional walk is 2n-1 steps and must fit the TOTAL step cap (Q4).
const _: () = assert!(CHARACTERIZATION_MAX_UNIQUE_BIDIRECTIONAL > 1);
const _: () =
    assert!(2 * CHARACTERIZATION_MAX_UNIQUE_BIDIRECTIONAL - 1 <= CHARACTERIZATION_MAX_POINTS);
const _: () = assert!(PWM_BASELINES_MAX_ENTRIES > 0);
const _: () = assert!(PWM_BASELINE_MAX_TEXT_BYTES > 0);
const _: () = assert!(PWM_BASELINE_MAX_POINTS > 0);
const _: () = assert!(
    PWM_BASELINES_MAX_BYTES > (PWM_BASELINES_MAX_ENTRIES * PWM_BASELINE_RECORD_MAX_BYTES) as u64
);
// [SAFETY] The hwmon lease is renewed once per point, so the renewal interval is
// one settle window. It must sit well inside BOTH deadlines the sweep depends on:
// the engine-pause deadman above, and the hwmon lease TTL — which is 60 s and is
// NOT refreshed by a write. Before the renewal was added, a legal 20 x 15 s sweep
// blew the lease at t~60 s and then could not even restore the header.
const _: () =
    assert!(CHARACTERIZATION_SETTLE_MAX_S * 2 <= crate::hwmon::lease::DEFAULT_LEASE_TTL.as_secs());
// The renew interval must leave room for ~3 attempts inside the TTL, and the
// TTL must be non-trivial — a too-tight window would drop legitimate overrides.
const _: () = assert!(OVERRIDE_RENEW_SECS > 0);
const _: () = assert!(OVERRIDE_RENEW_SECS * 3 <= OVERRIDE_TTL_SECS);
const _: () = assert!(GPU_COALESCE_DELTA_PCT > 0);
// Slow-spinning fans/pumps and GPU tachometers need a multi-second settle
// window; a too-short wait re-introduces the false `no_rpm_effect` verdicts
// DEC-101/DEC-120 fixed.
const _: () = assert!(VERIFY_WAIT_SECONDS >= 4);

// ── AIO-MB Phase 5: validation sessions ─────────────────────────────────────

/// Sampling cadence for a recording validation session.
///
/// Matched to the poll loop deliberately: the recorder reads the state cache the
/// poll fills, so a faster tick would resample identical bytes and a slower one
/// would alias the 1 Hz telemetry it is copying. §4 is explicit — do not add an
/// aggressive second polling loop for data already being sampled elsewhere.
pub const VALIDATION_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Hard sample cap per session — 7200 at 1 Hz, so two hours.
///
/// **Cap-and-stop, never a ring buffer.** A ring evicts the OLDEST samples, and
/// those are the startup/self-bleeding evidence §9 exists to capture. On reaching
/// this the session finalises itself with `sample_limit_reached`, which is
/// bounded and never silently deletes the interesting end of the recording.
pub const VALIDATION_MAX_SAMPLES: usize = 7200;

/// Completed sessions retained on disk.
///
/// **Size, corrected 2026-09-04 (`AUD3-i`).** This said "~1 MB each at the sample
/// cap" and was wrong by up to two orders of magnitude: a session document is
/// 3.6 MiB at one member, 5.7 MiB at two and 7.8 MiB at three, because a sample
/// carries one entry per cooling-device member. Retention is bounded in *files*;
/// [`VALIDATION_MAX_SAMPLE_BYTES`] is what bounds each file.
pub const VALIDATION_MAX_RETAINED_SESSIONS: usize = 5;

/// Byte budget for the `samples` array of one persisted session.
///
/// **The sample cap alone does not bound the file, and assuming it did was
/// `AUD3-i`.** A sample serialises one `MemberSample` per cooling-device member,
/// so [`VALIDATION_MAX_SAMPLES`] bounds the row count while the byte count scales
/// with the topology. Measured against the real serialised shapes at 7200
/// samples: **3.6 MiB at one member, 5.7 MiB at two, 7.8 MiB at three, 137 MiB at
/// the 65-member maximum** a device can claim (`1 + 2 * MAX_MEMBERS_PER_LIST`).
/// Everything from two members up exceeded the 4 MiB read cap the store used, so
/// the daemon wrote sessions it could then never read, list, serve or prune.
///
/// `session::max_samples_for` divides this budget by the measured worst-case cost
/// of one sample, so the document stays inside [`VALIDATION_MAX_SESSION_BYTES`]
/// whatever the topology. **Sized so that every realistic AIO — a pump plus up to
/// four radiator fans, which covers a 360 mm cooler with margin — still records
/// the full two hours**, which is what
/// `the_derived_cap_is_the_full_two_hours_for_every_realistic_aio` pins.
///
/// The budget is a *reservation*, not the realised size: the probe is a genuine
/// upper bound (widest integers, longest tokens, a 128-byte sensor id), so it
/// runs ~40% above a typical sample and the file that results is correspondingly
/// smaller. That pessimism is deliberate — an estimate that was not a bound is
/// exactly what `AUD3-i` was — and it is why the budget must be sized above the
/// realised figures rather than at them. At 12 MiB a four-member cooler lost
/// samples to the pessimism alone. Only exotic many-member devices shorten now,
/// and those previously produced an unreadable file instead.
pub const VALIDATION_MAX_SAMPLE_BYTES: usize = 16 * 1024 * 1024;

/// Read cap for one session document, replacing `atomic_io::MAX_CONFIG_BYTES` at
/// the validation store's read sites.
///
/// A session is evidence the daemon itself produced, not operator-edited config,
/// and the daemon already holds it whole in memory while recording — so the 4 MiB
/// config cap was protecting against a cost it had already paid. The cap still
/// exists (a corrupt or hostile file in the directory must not be buffered whole)
/// but is sized from the write-side budget rather than chosen independently, and
/// the assertion below is what keeps the two from drifting apart again. The slack
/// over [`VALIDATION_MAX_SAMPLE_BYTES`] covers the non-sample content: events,
/// external measurements, metadata and the summary.
pub const VALIDATION_MAX_SESSION_BYTES: u64 = 24 * 1024 * 1024;

/// Reservation for everything in a session document that is NOT a sample:
/// events, external measurements, user metadata, the member snapshot and the
/// summary.
///
/// **This exists because the sample budget alone is not a bound on the file, and
/// believing it was is how `AUD3-i` nearly recurred inside its own fix.** The
/// `const` assertion below is only meaningful if the ancillary content is itself
/// bounded, which is what `VALIDATION_MAX_TEXT_FIELD_BYTES` and
/// `VALIDATION_MAX_METADATA_KEY_BYTES` are for. Worst case with those in force:
/// 4096 events x ~760 B = ~3.0 MiB, 512 measurements x ~810 B = ~0.4 MiB,
/// 16 metadata pairs = ~10 KB, 65 members = ~16 KB — ~3.4 MiB against this 4 MiB.
pub const VALIDATION_MAX_ANCILLARY_BYTES: usize = 4 * 1024 * 1024;

/// Bound on each free-text field a client may attach to a session (an event
/// `detail`, a measurement `kind`/`unit`/`note`/`member_id`, a metadata value).
///
/// Unbounded, these turned the delete path added for `AUD3-i` into a way to
/// destroy an operator's evidence: a session grown past the read cap by ~10 KB
/// measurement notes became `TooLarge` and was then pruned. Bounding at ingest
/// is what makes "too large to read" mean "written by a daemon older than this
/// one" and therefore safe to reclaim.
pub const VALIDATION_MAX_TEXT_FIELD_BYTES: usize = 512;

/// Bound on a user-metadata KEY. The value was already bounded by
/// [`VALIDATION_MAX_METADATA_VALUE_BYTES`]; the key was not.
pub const VALIDATION_MAX_METADATA_KEY_BYTES: usize = 128;

/// Cap on timeline events, so a pathological reclaim loop cannot grow the file
/// without bound between samples.
pub const VALIDATION_MAX_EVENTS: usize = 4096;

/// Cap on externally measured observations attached to one session (§14).
pub const VALIDATION_MAX_EXTERNAL_MEASUREMENTS: usize = 512;

/// User/test metadata bounds (§11) — keys and value length.
pub const VALIDATION_MAX_METADATA_KEYS: usize = 16;
pub const VALIDATION_MAX_METADATA_VALUE_BYTES: usize = 512;

/// How many members one session may sweep. Each adds a full characterisation
/// (~3 min), so an unbounded list is a multi-hour run that drives every fan.
pub const VALIDATION_MAX_SWEEP_MEMBERS: usize = 8;

/// Minimum readback swing, in percent, before the summariser will judge whether
/// RPM followed PWM. Below this the duty did not meaningfully move and the
/// question is unanswerable — reported `not_tested`, never `pass` (§7).
pub const VALIDATION_DIVERGENCE_MIN_PWM_SWING_PCT: u8 = 15;

/// RPM swing, below which RPM is treated as having failed to follow a PWM change
/// that exceeded the swing threshold above (§10's "fails to follow the expected
/// direction"). Deliberately generous — this produces an `observed`, never a
/// `fail`, and a false positive here would misreport working hardware.
pub const VALIDATION_DIVERGENCE_MAX_RPM_SWING: u16 = 100;

// A cap of zero would make every session finalise before its first sample, and a
// retention of zero would delete each session as it was written.
const _: () = assert!(VALIDATION_MAX_SAMPLES > 0);
// The read cap must exceed the write budget, or the store would again produce
// documents it cannot read back. This is the invariant `AUD3-i` violated, made
// unbreakable at compile time rather than restated in prose.
// The read cap must exceed the write budget PLUS the ancillary reservation, or
// a session with a full complement of events and measurements would again be
// unreadable. The earlier form of this assertion compared against the sample
// budget alone, which bounded only part of the document — caught in review.
const _: () = assert!(
    VALIDATION_MAX_SESSION_BYTES as usize
        > VALIDATION_MAX_SAMPLE_BYTES + VALIDATION_MAX_ANCILLARY_BYTES
);
// A budget below one sample would make `max_samples_for` clamp every session to
// its floor of 1, silently reducing every recording to a single tick.
const _: () = assert!(VALIDATION_MAX_SAMPLE_BYTES > 64 * 1024);
const _: () = assert!(VALIDATION_MAX_RETAINED_SESSIONS > 0);
const _: () = assert!(VALIDATION_MAX_SWEEP_MEMBERS > 0);
// The divergence rule needs a real swing to test against; a zero threshold would
// classify a perfectly steady header as having "moved".
const _: () = assert!(VALIDATION_DIVERGENCE_MIN_PWM_SWING_PCT > 0);

/// Minimum `control-ofc-gui` version this daemon supports, published verbatim as
/// `control.min_supported_gui` on `GET /capabilities` (`WIRE-ac`).
///
/// A constant rather than a literal in the handler because it is *quoted* in two
/// other places that drift: each release's "Pairs with `control-ofc-gui` >= …"
/// line and `docs/08`. Those said `2.23.0` for roughly thirty releases while the
/// handler said `2.0.0`, which is the drift this constant exists to stop — the
/// wire is the single source and the prose cites it.
///
/// It is the floor the daemon places on the **GUI**, the opposite direction from
/// `control.autonomous_control`, which is the floor the GUI places on the daemon
/// (DEC-257). Raising it is a real compatibility claim: move the release-note
/// line in the same change, and only for a genuine GUI-side requirement.
pub const MIN_SUPPORTED_GUI: &str = "2.23.0";

// ── AIO Phase 8 Batch 1: control-path discovery + diagnostic preflight ──────

/// Default perturbation size, in duty points, for `POST
/// /hwmon/{id}/discover-control-path`.
///
/// 25 matches [`IDENTIFY_PUMP_DELTA_PCT`] deliberately: that value was chosen so
/// a pump moves audibly and measurably without approaching either rail, and the
/// discovery sweep wants exactly the same property. It is NOT the verify's 40,
/// which is sized to clear `classify_verify_result`'s >20 % relative test on a
/// single reading — discovery compares against a *measured* per-channel noise
/// floor across two cycles instead, so it can be gentler.
pub const DISCOVERY_DELTA_PCT: u8 = 25;

/// Clamp on a caller-supplied perturbation size. The floor keeps the swing above
/// tach noise on a slow pump; the ceiling keeps it well short of a rail so
/// [`perturbation_target`](crate::api::discovery::perturbation_target) always has
/// somewhere to go.
pub const DISCOVERY_DELTA_MIN_PCT: u8 = 10;
pub const DISCOVERY_DELTA_MAX_PCT: u8 = 40;

/// [SAFETY] The floor under every duty control-path discovery commands, on ANY
/// header — the same flat rule as [`CHARACTERIZATION_MIN_PCT`] and for the same
/// reason: one clamp `[max(DISCOVERY_MIN_PCT, header floor) .. 100]`, never a
/// role-conditional branch, so "0 % is unreachable through this endpoint" is a
/// single assertion rather than a claim about how good `header_is_pump_protected`
/// is on every board.
///
/// `AIO-Phase7-Batch1` §2 is explicit for pump-role and ambiguous-role channels:
/// never command 0 %, never cross below the safe minimum, and choose the
/// perturbation entirely within the safe range. This constant is the "never 0 %"
/// half; the header floor supplied by the caller is the other.
pub const DISCOVERY_MIN_PCT: u8 = 20;

/// Perturbation cycles per run, and the clamp on a caller-supplied count.
///
/// Two by default because repeatability is a **confidence input** (§2), not a
/// nicety: one cycle cannot distinguish a tach that responded from a tach that
/// happened to drift during the window. Three is the ceiling so the worst-case
/// run stays inside the pause budget asserted below.
pub const DISCOVERY_DEFAULT_CYCLES: u8 = 2;
pub const DISCOVERY_MAX_CYCLES: u8 = 3;

/// Cap on the tach channels one run observes.
///
/// Load-bearing rather than cosmetic (DEC-320): every observation is copied into
/// the run, from there into a validation session's `evidence[]`, and from there
/// into every export. An unbounded channel set therefore scales the session
/// document with the machine's hwmon topology, which is exactly the shape of
/// defect `VALIDATION_MAX_SAMPLE_BYTES` exists to prevent. 32 is roughly four
/// times the largest tach count seen on a consumer board.
pub const DISCOVERY_MAX_TACH_CHANNELS: usize = 32;

/// Sub-sampling cadence inside an observation window. Shared with the
/// characterisation sweep so the two diagnostics read hardware at one rate, and
/// so [`crate::api::discovery::measurement_resolution_ms`] can derive a driver's
/// update cadence from samples the run already took rather than from a second,
/// faster polling loop.
pub const DISCOVERY_SAMPLE_INTERVAL: Duration = CHARACTERIZATION_SAMPLE_INTERVAL;

/// A tach must move at least this fraction of its own baseline before the run
/// will call it a response, expressed in percent.
pub const DISCOVERY_RESPONSE_MIN_PCT: u8 = 10;

/// …and it must move at least this multiple of the largest change seen on any
/// NON-target channel in the same window. This is what separates "this header
/// drives this tach" from "the whole machine sped up while we were looking".
pub const DISCOVERY_TARGET_OVER_NOISE: u16 = 3;

/// Absolute floor under a measured per-channel noise estimate, in RPM. A channel
/// that reads perfectly steady during the baseline window would otherwise get a
/// zero noise floor, making any single-RPM flicker a "response".
pub const DISCOVERY_MIN_NOISE_FLOOR_RPM: u16 = 50;

/// Maximum age of a temperature reading before a diagnostic treats its
/// temperature source as stale.
///
/// [SAFETY] `AIO-Phase7-Batch1` §1 requires "required temperature source becomes
/// stale/unavailable" as both a preflight check and a runtime abort trigger, and
/// nothing in this daemon previously expressed it: `check_thermal_safety`
/// iterates whatever the cache holds, with no view of how old it is, so a poll
/// loop wedged on an unresponsive chip presents its last-known-good temperatures
/// forever and every thermal gate passes on them.
///
/// 10 s is ten poll intervals ([`crate::constants::VALIDATION_SAMPLE_INTERVAL`]
/// is 1 s and the sensor poll matches it), so a healthy machine never trips it
/// and a genuinely wedged reader trips it inside one settle window.
pub const DIAGNOSTIC_TEMP_MAX_AGE: Duration = Duration::from_secs(10);

/// Retained control-path records. Keyed by header id, so this is a ceiling on
/// distinct headers ever discovered rather than on runs.
pub const CONTROL_PATHS_MAX_ENTRIES: usize = 64;

/// Ingest bound on every client-influenced string stored in a control-path
/// record. Matches [`VALIDATION_MAX_TEXT_FIELD_BYTES`].
pub const CONTROL_PATH_MAX_TEXT_BYTES: usize = VALIDATION_MAX_TEXT_FIELD_BYTES;

/// Tach references stored per record.
///
/// Deliberately far below [`DISCOVERY_MAX_TACH_CHANNELS`]: a run *observes* every
/// channel, but a record only keeps the ones that **responded**, and a single PWM
/// header driving more than eight tach-reporting devices is not a thing. This is
/// the difference between a per-record ceiling of ~10 KB and one of ~35 KB, and
/// it is a bound on stored data rather than on what the diagnostic may look at.
pub const CONTROL_PATH_MAX_TACH_REFS: usize = 8;

/// Worst-case serialised size of one record.
///
/// **Derived, not assumed** — this is the DEC-320 rule applied to its own
/// successor. Five scalar text fields (`header_id`, `relationship`, `confidence`,
/// `direction`, `run_id`) plus an id and a label for each stored tach reference,
/// every one of them at the full ingest bound, plus slack for JSON syntax, keys
/// and the numeric fields.
///
/// The first version of this constant assumed "about four text fields" and
/// produced a 256 KiB file cap for a store whose real worst case was 2.2 MiB —
/// i.e. a daemon that could write a document it would then refuse to read on the
/// next boot. That was found by a test asserting the **realised** file length of
/// a deliberately full store, not by re-deriving this arithmetic.
pub const CONTROL_PATH_RECORD_MAX_BYTES: usize = {
    // `header_id` is stored TWICE — once as the `BTreeMap` key and once as a
    // field — so the field count is 6, not 5. Missing that was worth 512 bytes a
    // record, and the realised-length test is what found it.
    let text_fields = 6 + 2 * CONTROL_PATH_MAX_TACH_REFS;
    // Per-field JSON overhead: the key name, quotes, colon, comma and the
    // pretty-printer's indentation. Scales WITH the field count rather than
    // being a flat allowance, so adding a field cannot silently eat the margin
    // (DEC-303: provision above the slope, not above today's worst cell).
    text_fields * (CONTROL_PATH_MAX_TEXT_BYTES + 128) + 512
};

/// Byte ceiling on `{state_dir}/control_paths.json`.
///
/// Read with a stat BEFORE the file is opened, exactly as
/// `validation::store::read_session` does: an over-size document is discarded
/// and deleted rather than parsed. Bounding the ingest (see
/// [`CONTROL_PATH_MAX_TEXT_BYTES`]) is what makes that safe — it means "too
/// large" can only be a document this daemon did not write, never a legitimate
/// one it must now refuse to read.
///
/// **Derived from the per-record ceiling** so the two can never disagree. A
/// hand-picked number here is how a legitimately full store becomes permanently
/// unreadable.
pub const CONTROL_PATHS_MAX_BYTES: u64 =
    (CONTROL_PATHS_MAX_ENTRIES * CONTROL_PATH_RECORD_MAX_BYTES + 8192) as u64;

// Compile-time invariants for the above.
// [SAFETY] 0 % must be unreachable through discovery, for every header.
const _: () = assert!(DISCOVERY_MIN_PCT > 0);
// A perturbation must fit between the floor and the ceiling with room to spare,
// or `perturbation_target` could be asked for a swing it cannot take in either
// direction.
const _: () = assert!(DISCOVERY_DELTA_MAX_PCT < 100 - DISCOVERY_MIN_PCT);
const _: () = assert!(DISCOVERY_DELTA_MIN_PCT <= DISCOVERY_DELTA_PCT);
const _: () = assert!(DISCOVERY_DELTA_PCT <= DISCOVERY_DELTA_MAX_PCT);
const _: () = assert!(DISCOVERY_DEFAULT_CYCLES >= 2);
const _: () = assert!(DISCOVERY_DEFAULT_CYCLES <= DISCOVERY_MAX_CYCLES);
const _: () = assert!(DISCOVERY_MAX_TACH_CHANNELS > 0);
const _: () = assert!(DISCOVERY_TARGET_OVER_NOISE > 1);
// [SAFETY] The pause deadman is renewed before **every observation window**, so
// the renewal interval is ONE window — `api::discovery::run_discovery` is what
// makes that true, and this assertion is the tripwire on it.
//
// The distinction is not pedantic and it was wrong on the first attempt: a cycle
// holds TWO windows, so renewing once per cycle makes the interval `2 × window`,
// which at the maximum settle (15 s) equals the deadman (30 s) before any I/O
// overhead. The pause then expires mid-run, the engine's write phase resumes,
// and `try_begin_verify`'s steal branch lets a second diagnostic force-take this
// run's lease so even its restore fails — the DEC-296 defect. If the renewal is
// ever moved back to per-cycle, this assertion must become
// `CHARACTERIZATION_SETTLE_MAX_S * 4 <= …`, which does not hold, which is the
// point of writing it this way.
const _: () = assert!(CHARACTERIZATION_SETTLE_MAX_S * 2 <= VERIFY_PAUSE_DEADMAN.as_secs());
const _: () = assert!(CONTROL_PATHS_MAX_ENTRIES > 0);
const _: () = assert!(CONTROL_PATH_MAX_TEXT_BYTES > 0);
const _: () = assert!(CONTROL_PATH_MAX_TACH_REFS > 0);
const _: () = assert!(CONTROL_PATH_MAX_TACH_REFS <= DISCOVERY_MAX_TACH_CHANNELS);
// The store must be able to hold its own maximum: every entry at full text
// length, with slack for JSON, still inside the byte ceiling. This holds by
// construction now that the file cap is derived from the record cap — the
// assertion stays as a tripwire for a future field added to the record without
// updating `CONTROL_PATH_RECORD_MAX_BYTES`.
const _: () = assert!(
    CONTROL_PATHS_MAX_ENTRIES * CONTROL_PATH_RECORD_MAX_BYTES < CONTROL_PATHS_MAX_BYTES as usize
);
