//! AMD GPU PMFW fan curve control via sysfs.
//!
//! RDNA3+ GPUs expose a firmware-managed fan curve at:
//! `/sys/class/drm/cardN/device/gpu_od/fan_ctrl/fan_curve`
//!
//! The file format is:
//! ```text
//! OD_FAN_CURVE:
//! 0: 40C 30%
//! 1: 50C 35%
//! ...
//! OD_RANGE:
//! FAN_CURVE(hotspot temp): 25C 100C
//! FAN_CURVE(fan speed): 15% 100%
//! ```
//!
//! Write format: `echo "N TEMP SPEED" > fan_curve` (e.g. `echo "0 45 40"`)
//! Commit: `echo "c" > fan_curve`
//! Reset to auto: `echo "r" > fan_curve`

use std::path::Path;

use crate::error::HwmonError;
use crate::pwm::percent_to_raw;

/// Trait for writing to sysfs fan curve files — allows test mocks to
/// capture every intermediate write (each of which the kernel processes
/// independently, but a regular file only retains the last).
pub trait FanCurveWriter {
    fn write(&self, path: &Path, content: &str) -> Result<(), HwmonError>;
}

/// Default writer using `std::fs::write`.
pub struct RealFanCurveWriter;

impl FanCurveWriter for RealFanCurveWriter {
    fn write(&self, path: &Path, content: &str) -> Result<(), HwmonError> {
        std::fs::write(path, content).map_err(|e| HwmonError::WriteError {
            path: path.display().to_string(),
            message: e.to_string(),
        })
    }
}

/// A single point on the PMFW fan curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FanCurvePoint {
    /// Point index (0-4 typically).
    pub index: u8,
    /// Temperature threshold in degrees Celsius.
    pub temp_c: i32,
    /// Fan speed as a percentage (0-100).
    pub speed_pct: u8,
}

/// Parsed PMFW fan curve with optional range limits.
#[derive(Debug, Clone)]
pub struct FanCurve {
    /// Curve points, ordered by index.
    pub points: Vec<FanCurvePoint>,
    /// Allowed temperature range (min, max) in Celsius, if reported.
    pub temp_range: Option<(i32, i32)>,
    /// Allowed fan speed range (min, max) as percentage, if reported.
    pub speed_range: Option<(u8, u8)>,
}

/// Parse the PMFW fan_curve sysfs file contents.
///
/// Expected format:
/// ```text
/// OD_FAN_CURVE:
/// 0: 40C 30%
/// 1: 50C 35%
/// ...
/// OD_RANGE:
/// FAN_CURVE(hotspot temp): 25C 100C
/// FAN_CURVE(fan speed): 15% 100%
/// ```
pub fn parse_fan_curve(content: &str) -> Result<FanCurve, HwmonError> {
    let mut points = Vec::new();
    let mut temp_range = None;
    let mut speed_range = None;

    for line in content.lines() {
        let trimmed = line.trim();

        // Parse curve points: "N: TTT°C SS%" or "N: TTTC SS%"
        if let Some(rest) = trimmed
            .strip_prefix(|c: char| c.is_ascii_digit())
            .and_then(|r| r.strip_prefix(':'))
        {
            let index: u8 = trimmed
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);

            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                let temp = parse_value_with_suffix(parts[0], 'C');
                let speed = parse_value_with_suffix(parts[1], '%');
                if let (Some(t), Some(s)) = (temp, speed) {
                    points.push(FanCurvePoint {
                        index,
                        temp_c: t,
                        speed_pct: s.clamp(0, 100) as u8,
                    });
                }
            }
        }

        // Parse temp range: "FAN_CURVE(hotspot temp): 25C 100C"
        if trimmed.contains("hotspot temp") {
            if let Some(rest) = trimmed.split(':').nth(1) {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 2 {
                    let lo = parse_value_with_suffix(parts[0], 'C');
                    let hi = parse_value_with_suffix(parts[1], 'C');
                    if let (Some(l), Some(h)) = (lo, hi) {
                        temp_range = Some((l, h));
                    }
                }
            }
        }

        // Parse speed range: "FAN_CURVE(fan speed): 15% 100%"
        if trimmed.contains("fan speed") {
            if let Some(rest) = trimmed.split(':').nth(1) {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 2 {
                    let lo = parse_value_with_suffix(parts[0], '%');
                    let hi = parse_value_with_suffix(parts[1], '%');
                    if let (Some(l), Some(h)) = (lo, hi) {
                        speed_range = Some((l as u8, h as u8));
                    }
                }
            }
        }
    }

    if points.is_empty() {
        return Err(HwmonError::ReadError {
            path: "<fan_curve>".into(),
            message: "no curve points found in fan_curve output".into(),
        });
    }

    points.sort_by_key(|p| p.index);

    Ok(FanCurve {
        points,
        temp_range,
        speed_range,
    })
}

/// Parse an integer value with a trailing suffix character (e.g. "45C" → 45, "30%" → 30).
fn parse_value_with_suffix(s: &str, _suffix: char) -> Option<i32> {
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

/// Read and parse the current PMFW fan curve from a sysfs file.
pub fn read_fan_curve(fan_curve_path: &Path) -> Result<FanCurve, HwmonError> {
    let content = std::fs::read_to_string(fan_curve_path).map_err(|e| HwmonError::ReadError {
        path: fan_curve_path.display().to_string(),
        message: e.to_string(),
    })?;
    parse_fan_curve(&content)
}

/// The flat speed of a curve where every point shares the same fan speed %,
/// i.e. a "static" curve as written by [`set_static_speed`]. Returns `None` for
/// an empty curve or a varying (firmware-default / auto) curve.
///
/// Used by the GPU fan verify to (a) confirm a static-speed write took effect
/// (read-back flat speed == requested) and (b) decide whether the GPU was being
/// driven at a static speed before the test (so it can be restored).
pub fn flat_speed_pct(curve: &FanCurve) -> Option<u8> {
    let first = curve.points.first()?.speed_pct;
    curve
        .points
        .iter()
        .all(|p| p.speed_pct == first)
        .then_some(first)
}

/// Best-effort parse of the PMFW `fan_minimum_pwm` sysfs attribute into a
/// percentage (0..=100).
///
/// The exact format is not documented upstream ("Reading back the file shows
/// you the current setting and the permitted ranges if changable"), but it
/// follows the same `fan_ctrl` convention as `fan_curve`: a current-value
/// section followed by an `OD_RANGE:` block, with fan speeds written as
/// `NN%`. We read the **current setting** only (everything before `OD_RANGE`)
/// and return the first plausible 0..=100 value, ignoring header labels.
///
/// Returns `None` when nothing parseable is found — diagnostics treat the
/// attribute as optional, so a fail-soft `None` is preferable to a guess.
pub fn parse_fan_minimum_pwm(content: &str) -> Option<u8> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Stop at the range section — we want the active setting, not the
        // permitted `OD_RANGE` bounds.
        if trimmed.to_ascii_uppercase().starts_with("OD_RANGE") {
            break;
        }
        // Tokens may include a "FAN_MINIMUM_PWM:" label prefix; skip any token
        // that isn't a bare/`%`-suffixed integer in range.
        for tok in trimmed.split_whitespace() {
            let stripped = tok.trim_end_matches('%').trim_end_matches(':');
            if let Ok(v) = stripped.parse::<u32>() {
                if v <= 100 {
                    return Some(v as u8);
                }
            }
        }
    }
    None
}

/// Read and parse the current PMFW `fan_minimum_pwm` setting (percent).
///
/// Diagnostics-only; never written by the daemon. Returns `None` on any read
/// or parse failure (the attribute is absent on many GPUs).
pub fn read_fan_minimum_pwm(path: &Path) -> Option<u8> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_fan_minimum_pwm(&content)
}

/// Write a fan curve to the PMFW sysfs file and commit it.
///
/// Each point is written as `"INDEX TEMP SPEED\n"`, followed by `"c\n"` to commit.
///
/// **Non-atomic:** if the process is killed between individual point writes
/// and the final commit, the GPU retains a partial curve. The daemon panic
/// hook resets to auto (`"r\n"` + `"c\n"`) to mitigate this.
pub fn write_fan_curve(fan_curve_path: &Path, points: &[FanCurvePoint]) -> Result<(), HwmonError> {
    write_fan_curve_with(&RealFanCurveWriter, fan_curve_path, points)
}

/// Write a fan curve using a custom writer (allows test mocks to capture
/// every intermediate write that the kernel processes independently).
pub fn write_fan_curve_with(
    writer: &dyn FanCurveWriter,
    fan_curve_path: &Path,
    points: &[FanCurvePoint],
) -> Result<(), HwmonError> {
    for p in points {
        let cmd = format!("{} {} {}\n", p.index, p.temp_c, p.speed_pct);
        writer
            .write(fan_curve_path, &cmd)
            .map_err(|_| HwmonError::WriteError {
                path: fan_curve_path.display().to_string(),
                message: format!(
                    "failed to write curve point {} ({}°C {}%)",
                    p.index, p.temp_c, p.speed_pct
                ),
            })?;
    }

    // Commit
    writer
        .write(fan_curve_path, "c\n")
        .map_err(|_| HwmonError::WriteError {
            path: fan_curve_path.display().to_string(),
            message: "failed to commit fan curve".into(),
        })?;

    log::info!(
        "Wrote {} PMFW fan curve points to {}",
        points.len(),
        fan_curve_path.display()
    );
    Ok(())
}

/// Disable fan zero-RPM mode so the fan actually spins when a curve is applied.
///
/// RDNA3+ firmware keeps fans stopped below a temperature threshold when zero-RPM
/// is enabled. This must be disabled before writing a static speed, otherwise the
/// fan won't spin at idle temperatures. Each write to fan_zero_rpm_enable requires
/// its own commit ("c").
pub fn disable_zero_rpm(zero_rpm_path: &Path) -> Result<(), HwmonError> {
    // Read current state — skip if already disabled to avoid redundant
    // sysfs writes that cause GPU firmware churn (stuttering under load).
    // The file returns multi-line formatted output like:
    //   FAN_ZERO_RPM_ENABLE:
    //   0
    //   OD_RANGE:
    //   ZERO_RPM_ENABLE: 0 1
    // We check the line after the header for the current value.
    if let Ok(content) = std::fs::read_to_string(zero_rpm_path) {
        let mut found_header = false;
        for line in content.lines() {
            if line.contains("FAN_ZERO_RPM_ENABLE:") && !line.contains("OD_RANGE") {
                found_header = true;
                continue;
            }
            if found_header {
                if line.trim() == "0" {
                    return Ok(());
                }
                break;
            }
        }
    }

    std::fs::write(zero_rpm_path, "0\n").map_err(|e| HwmonError::WriteError {
        path: zero_rpm_path.display().to_string(),
        message: format!("failed to disable fan zero-RPM: {e}"),
    })?;
    std::fs::write(zero_rpm_path, "c\n").map_err(|e| HwmonError::WriteError {
        path: zero_rpm_path.display().to_string(),
        message: format!("failed to commit zero-RPM disable: {e}"),
    })?;
    log::info!("Disabled GPU fan zero-RPM: {}", zero_rpm_path.display());
    Ok(())
}

/// Parse the multi-line `fan_zero_rpm_enable` sysfs output to its current bool.
///
/// The file is formatted like `fan_curve` — a labelled current-value section
/// followed by an `OD_RANGE:` block:
/// ```text
/// FAN_ZERO_RPM_ENABLE:
/// 0
/// OD_RANGE:
/// ZERO_RPM_ENABLE: 0 1
/// ```
/// We read the value line immediately after the `FAN_ZERO_RPM_ENABLE:` header.
/// Returns `None` when nothing parseable is found.
pub fn parse_zero_rpm_enabled(content: &str) -> Option<bool> {
    let mut found_header = false;
    for line in content.lines() {
        if line.contains("FAN_ZERO_RPM_ENABLE:") && !line.contains("OD_RANGE") {
            found_header = true;
            continue;
        }
        if found_header {
            return match line.trim() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            };
        }
    }
    None
}

/// Read the current `fan_zero_rpm_enable` state. Diagnostics-only (used by the
/// GPU fan verify to distinguish a legitimately idle/stopped fan from a broken
/// one). Returns `None` on any read or parse failure.
pub fn read_zero_rpm_enabled(zero_rpm_path: &Path) -> Option<bool> {
    parse_zero_rpm_enabled(&std::fs::read_to_string(zero_rpm_path).ok()?)
}

/// Re-enable fan zero-RPM mode (restore firmware idle fan-stop behaviour).
pub fn enable_zero_rpm(zero_rpm_path: &Path) -> Result<(), HwmonError> {
    std::fs::write(zero_rpm_path, "1\n").map_err(|e| HwmonError::WriteError {
        path: zero_rpm_path.display().to_string(),
        message: format!("failed to enable fan zero-RPM: {e}"),
    })?;
    std::fs::write(zero_rpm_path, "c\n").map_err(|e| HwmonError::WriteError {
        path: zero_rpm_path.display().to_string(),
        message: format!("failed to commit zero-RPM enable: {e}"),
    })?;
    log::info!("Re-enabled GPU fan zero-RPM: {}", zero_rpm_path.display());
    Ok(())
}

/// Set a static (flat) fan speed by writing all curve points to the same percentage.
///
/// If `zero_rpm_path` is provided, disables zero-RPM first so the fan actually
/// spins at low temperatures. This effectively gives "manual" control via the PMFW curve.
///
/// This is a thin convenience wrapper around
/// [`set_static_speed_with_zero_rpm`] with ``preserve_zero_rpm = false``,
/// preserving pre-v4 behaviour for callers that haven't opted in.
pub fn set_static_speed(
    fan_curve_path: &Path,
    zero_rpm_path: Option<&Path>,
    speed_pct: u8,
    num_points: u8,
) -> Result<(), HwmonError> {
    set_static_speed_with_zero_rpm(fan_curve_path, zero_rpm_path, speed_pct, num_points, false)
}

/// Set a static fan speed, with explicit control over zero-RPM handling.
///
/// When ``preserve_zero_rpm`` is ``false`` (default for manual API writes),
/// the daemon disables ``fan_zero_rpm_enable`` before writing the curve so
/// the fan spins continuously at the commanded speed. When ``true``
/// (profile-driven write where the user opted into idle stop, see DEC-095
/// and ``ControlMember.fan_zero_rpm``), the existing zero-RPM state is
/// preserved — the GPU firmware decides when to stop the fan based on the
/// curve and its idle threshold.
pub fn set_static_speed_with_zero_rpm(
    fan_curve_path: &Path,
    zero_rpm_path: Option<&Path>,
    speed_pct: u8,
    num_points: u8,
    preserve_zero_rpm: bool,
) -> Result<(), HwmonError> {
    if !preserve_zero_rpm {
        // Disable zero-RPM so the fan spins at any temperature
        if let Some(zrp) = zero_rpm_path {
            if let Err(e) = disable_zero_rpm(zrp) {
                log::warn!("Could not disable zero-RPM (continuing): {e}");
            }
        }
    }

    // Read the device's OD_RANGE to clamp values within PMFW-accepted bounds.
    // The AMDGPU driver rejects curve points outside the PPTable limits with
    // EINVAL. Typical ranges: temp 25-100°C, speed 15-100%.
    let (temp_min, temp_max, speed_min, speed_max) = match read_fan_curve(fan_curve_path) {
        Ok(curve) => {
            let (tlo, thi) = curve.temp_range.unwrap_or((25, 100));
            let (slo, shi) = curve.speed_range.unwrap_or((15, 100));
            (tlo, thi, slo, shi)
        }
        Err(_) => (25, 100, 15, 100), // safe fallback
    };

    let clamped_speed = speed_pct.max(speed_min).min(speed_max);
    if clamped_speed != speed_pct {
        log::debug!(
            "GPU fan speed clamped {speed_pct}% → {clamped_speed}% (OD_RANGE {speed_min}–{speed_max}%)"
        );
    }

    let points: Vec<FanCurvePoint> = (0..num_points)
        .map(|i| {
            let divisor = (num_points.max(1) as i32 - 1).max(1);
            let raw_temp = temp_min + (i as i32 * (temp_max - temp_min) / divisor);
            FanCurvePoint {
                index: i,
                temp_c: raw_temp.clamp(temp_min, temp_max),
                speed_pct: clamped_speed,
            }
        })
        .collect();

    write_fan_curve(fan_curve_path, &points)
}

/// Reset the GPU fan curve to automatic (firmware default).
///
/// Resets the curve via `"r\n"` + `"c\n"`, then re-enables zero-RPM if the path
/// is provided (restoring firmware idle fan-stop behaviour).
///
/// Zero-RPM is re-enabled even if the curve reset fails, because a prior
/// `set_static_speed` may have disabled it; leaving zero-RPM off means
/// the fan runs continuously when it otherwise would stop at idle.
pub fn reset_to_auto(
    fan_curve_path: &Path,
    zero_rpm_path: Option<&Path>,
) -> Result<(), HwmonError> {
    let reset_err = std::fs::write(fan_curve_path, "r\n")
        .and_then(|()| std::fs::write(fan_curve_path, "c\n"))
        .err();

    // Always attempt to re-enable zero-RPM, even if the curve reset failed.
    if let Some(zrp) = zero_rpm_path {
        if let Err(e) = enable_zero_rpm(zrp) {
            log::warn!("Could not re-enable zero-RPM (continuing): {e}");
        }
    }

    if let Some(io_err) = reset_err {
        return Err(HwmonError::WriteError {
            path: fan_curve_path.display().to_string(),
            message: format!("failed to reset fan curve to auto: {io_err}"),
        });
    }

    log::info!("Reset PMFW fan curve to auto: {}", fan_curve_path.display());
    Ok(())
}

// ── Pre-RDNA3 legacy PWM control ─────────────────────────────────────
//
// Pre-RDNA3 GPUs (RX 6000 and older) use the standard hwmon pwm1/pwm1_enable
// interface. These functions encapsulate those raw sysfs writes.

/// Set a legacy GPU fan speed via hwmon pwm1 (pre-RDNA3).
///
/// Writes `pwm1_enable=1` (manual mode) then the raw PWM value to `pwm1`.
/// amdgpu rejects `pwm1` writes with EINVAL unless manual mode is active.
pub fn set_legacy_pwm(hwmon_path: &Path, speed_pct: u8) -> Result<(), HwmonError> {
    let enable_path = hwmon_path.join("pwm1_enable");
    std::fs::write(&enable_path, "1\n").map_err(|e| HwmonError::WriteError {
        path: enable_path.display().to_string(),
        message: format!("failed to set manual mode: {e}"),
    })?;

    let pwm_path = hwmon_path.join("pwm1");
    let raw = percent_to_raw(speed_pct);
    std::fs::write(&pwm_path, format!("{raw}\n")).map_err(|e| HwmonError::WriteError {
        path: pwm_path.display().to_string(),
        message: format!("failed to write PWM value {raw}: {e}"),
    })?;

    log::info!(
        "Set legacy GPU fan to {speed_pct}% (raw {raw}) via {}",
        hwmon_path.display()
    );
    Ok(())
}

/// Reset a legacy GPU fan to automatic mode via hwmon pwm1_enable (pre-RDNA3).
///
/// Writes `pwm1_enable=2` to restore firmware-managed fan control.
pub fn reset_legacy_to_auto(hwmon_path: &Path) -> Result<(), HwmonError> {
    let enable_path = hwmon_path.join("pwm1_enable");
    std::fs::write(&enable_path, "2\n").map_err(|e| HwmonError::WriteError {
        path: enable_path.display().to_string(),
        message: format!("failed to reset to auto mode: {e}"),
    })?;

    log::info!("Reset legacy GPU fan to auto via {}", hwmon_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    /// Mock writer that records every write for verification.
    struct RecordingWriter {
        writes: RefCell<Vec<(String, String)>>,
    }

    impl RecordingWriter {
        fn new() -> Self {
            Self {
                writes: RefCell::new(Vec::new()),
            }
        }

        fn writes(&self) -> Vec<(String, String)> {
            self.writes.borrow().clone()
        }
    }

    impl FanCurveWriter for RecordingWriter {
        fn write(&self, path: &Path, content: &str) -> Result<(), HwmonError> {
            self.writes
                .borrow_mut()
                .push((path.display().to_string(), content.to_string()));
            Ok(())
        }
    }

    const SAMPLE_CURVE: &str = "\
OD_FAN_CURVE:
0: 40C 30%
1: 50C 35%
2: 60C 50%
3: 70C 75%
4: 80C 100%
OD_RANGE:
FAN_CURVE(hotspot temp): 25C 100C
FAN_CURVE(fan speed): 15% 100%
";

    const FLAT_CURVE: &str = "\
OD_FAN_CURVE:
0: 40C 75%
1: 50C 75%
2: 60C 75%
3: 70C 75%
4: 80C 75%
OD_RANGE:
FAN_CURVE(hotspot temp): 25C 100C
FAN_CURVE(fan speed): 15% 100%
";

    const ZERO_RPM_ENABLED: &str = "\
FAN_ZERO_RPM_ENABLE:
1
OD_RANGE:
ZERO_RPM_ENABLE: 0 1
";

    const ZERO_RPM_DISABLED: &str = "\
FAN_ZERO_RPM_ENABLE:
0
OD_RANGE:
ZERO_RPM_ENABLE: 0 1
";

    #[test]
    fn flat_speed_detects_static_curve() {
        let flat = parse_fan_curve(FLAT_CURVE).unwrap();
        assert_eq!(flat_speed_pct(&flat), Some(75));
    }

    #[test]
    fn flat_speed_none_for_varying_curve() {
        let varying = parse_fan_curve(SAMPLE_CURVE).unwrap();
        assert_eq!(flat_speed_pct(&varying), None);
    }

    #[test]
    fn flat_speed_none_for_empty_curve() {
        let empty = FanCurve {
            points: vec![],
            temp_range: None,
            speed_range: None,
        };
        assert_eq!(flat_speed_pct(&empty), None);
    }

    #[test]
    fn parse_zero_rpm_enabled_reads_value_line() {
        assert_eq!(parse_zero_rpm_enabled(ZERO_RPM_ENABLED), Some(true));
        assert_eq!(parse_zero_rpm_enabled(ZERO_RPM_DISABLED), Some(false));
    }

    #[test]
    fn parse_zero_rpm_enabled_none_when_absent() {
        assert_eq!(parse_zero_rpm_enabled("garbage\nmore garbage\n"), None);
    }

    #[test]
    fn parse_standard_curve() {
        let curve = parse_fan_curve(SAMPLE_CURVE).unwrap();
        assert_eq!(curve.points.len(), 5);
        assert_eq!(curve.points[0].index, 0);
        assert_eq!(curve.points[0].temp_c, 40);
        assert_eq!(curve.points[0].speed_pct, 30);
        assert_eq!(curve.points[4].temp_c, 80);
        assert_eq!(curve.points[4].speed_pct, 100);
    }

    #[test]
    fn parse_ranges() {
        let curve = parse_fan_curve(SAMPLE_CURVE).unwrap();
        assert_eq!(curve.temp_range, Some((25, 100)));
        assert_eq!(curve.speed_range, Some((15, 100)));
    }

    #[test]
    fn parse_empty_fails() {
        let result = parse_fan_curve("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_no_points_fails() {
        let result = parse_fan_curve("OD_FAN_CURVE:\nOD_RANGE:\n");
        assert!(result.is_err());
    }

    #[test]
    fn parse_partial_curve() {
        let content = "OD_FAN_CURVE:\n0: 40C 30%\n2: 60C 50%\n";
        let curve = parse_fan_curve(content).unwrap();
        assert_eq!(curve.points.len(), 2);
        assert_eq!(curve.points[0].index, 0);
        assert_eq!(curve.points[1].index, 2);
    }

    // ── DEC-119: fan_minimum_pwm parsing ────────────────────────────

    #[test]
    fn parse_fmp_value_on_own_line() {
        let content = "FAN_MINIMUM_PWM:\n20%\nOD_RANGE:\nFAN_MINIMUM_PWM: 0% 100%\n";
        assert_eq!(parse_fan_minimum_pwm(content), Some(20));
    }

    #[test]
    fn parse_fmp_inline_label() {
        assert_eq!(parse_fan_minimum_pwm("FAN_MINIMUM_PWM: 35%\n"), Some(35));
    }

    #[test]
    fn parse_fmp_bare_integer() {
        assert_eq!(parse_fan_minimum_pwm("15\n"), Some(15));
    }

    #[test]
    fn parse_fmp_stops_before_range_bounds() {
        // The current setting (20%) must win over the OD_RANGE lower bound (0).
        let content = "FAN_MINIMUM_PWM:\n20%\nOD_RANGE:\nFAN_MINIMUM_PWM: 0 100\n";
        assert_eq!(parse_fan_minimum_pwm(content), Some(20));
    }

    #[test]
    fn parse_fmp_out_of_range_token_skipped() {
        // A bare value above 100 (e.g. a raw 0-255 PWM with no '%') is NOT
        // mistaken for a percentage — fail-soft to None rather than mislabel.
        assert_eq!(parse_fan_minimum_pwm("FAN_MINIMUM_PWM: 255\n"), None);
    }

    #[test]
    fn parse_fmp_garbage_is_none() {
        assert_eq!(parse_fan_minimum_pwm("no numbers here\n"), None);
        assert_eq!(parse_fan_minimum_pwm(""), None);
    }

    #[test]
    fn read_fan_minimum_pwm_missing_file_is_none() {
        assert_eq!(
            read_fan_minimum_pwm(Path::new("/nonexistent/fan_minimum_pwm")),
            None
        );
    }

    #[test]
    fn read_fan_curve_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fan_curve");
        fs::write(&path, SAMPLE_CURVE).unwrap();

        let curve = read_fan_curve(&path).unwrap();
        assert_eq!(curve.points.len(), 5);
    }

    #[test]
    fn write_fan_curve_captures_all_intermediate_writes() {
        let writer = RecordingWriter::new();
        let path = Path::new("/fake/fan_curve");

        let points = vec![
            FanCurvePoint {
                index: 0,
                temp_c: 40,
                speed_pct: 30,
            },
            FanCurvePoint {
                index: 1,
                temp_c: 60,
                speed_pct: 50,
            },
        ];
        write_fan_curve_with(&writer, path, &points).unwrap();

        let writes = writer.writes();
        assert_eq!(writes.len(), 3, "Expected 2 point writes + 1 commit");
        assert_eq!(writes[0].1, "0 40 30\n");
        assert_eq!(writes[1].1, "1 60 50\n");
        assert_eq!(writes[2].1, "c\n");
    }

    #[test]
    fn write_fan_curve_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fan_curve");
        // Create file so writes succeed
        fs::write(&path, "").unwrap();

        let points = vec![
            FanCurvePoint {
                index: 0,
                temp_c: 40,
                speed_pct: 30,
            },
            FanCurvePoint {
                index: 1,
                temp_c: 60,
                speed_pct: 50,
            },
        ];
        write_fan_curve(&path, &points).unwrap();

        // File should contain the last write (commit "c\n")
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "c\n");
    }

    #[test]
    fn set_static_speed_writes_flat_curve() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fan_curve");
        fs::write(&path, SAMPLE_CURVE).unwrap();

        set_static_speed(&path, None, 75, 5).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "c\n");
    }

    #[test]
    fn set_static_speed_clamps_below_od_range_minimum() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fan_curve");
        // OD_RANGE says fan speed minimum is 15%
        fs::write(&path, SAMPLE_CURVE).unwrap();

        // Request 5% — should be clamped to 15% and succeed (no EINVAL)
        set_static_speed(&path, None, 5, 5).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "c\n"); // committed successfully
    }

    #[test]
    fn set_static_speed_disables_zero_rpm() {
        let tmp = tempfile::tempdir().unwrap();
        let curve_path = tmp.path().join("fan_curve");
        let zrp_path = tmp.path().join("fan_zero_rpm_enable");
        fs::write(&curve_path, SAMPLE_CURVE).unwrap();
        fs::write(&zrp_path, "1\n").unwrap();

        set_static_speed(&curve_path, Some(&zrp_path), 50, 5).unwrap();
        // zero-RPM file should contain the commit "c\n" (last write)
        let zrp_content = fs::read_to_string(&zrp_path).unwrap();
        assert_eq!(zrp_content, "c\n");
    }

    #[test]
    fn set_static_speed_with_preserve_zero_rpm_skips_disable() {
        // Profile-driven path with ControlMember.fan_zero_rpm = true must
        // not touch fan_zero_rpm_enable so the GPU's idle behaviour is
        // honoured. The original sysfs contents must be left untouched.
        let tmp = tempfile::tempdir().unwrap();
        let curve_path = tmp.path().join("fan_curve");
        let zrp_path = tmp.path().join("fan_zero_rpm_enable");
        fs::write(&curve_path, SAMPLE_CURVE).unwrap();
        fs::write(&zrp_path, "1\n").unwrap();

        set_static_speed_with_zero_rpm(&curve_path, Some(&zrp_path), 50, 5, true).unwrap();

        // zero-RPM file untouched by the daemon — still contains the
        // pre-existing "1\n", not the "c\n" the disable path would write.
        let zrp_content = fs::read_to_string(&zrp_path).unwrap();
        assert_eq!(zrp_content, "1\n");
        // Curve was still written and committed.
        let curve_content = fs::read_to_string(&curve_path).unwrap();
        assert_eq!(curve_content, "c\n");
    }

    #[test]
    fn set_static_speed_with_preserve_zero_rpm_false_matches_legacy() {
        // preserve_zero_rpm = false must behave exactly like the legacy
        // set_static_speed wrapper (regression guard for DEC-053).
        let tmp = tempfile::tempdir().unwrap();
        let curve_path = tmp.path().join("fan_curve");
        let zrp_path = tmp.path().join("fan_zero_rpm_enable");
        fs::write(&curve_path, SAMPLE_CURVE).unwrap();
        fs::write(&zrp_path, "1\n").unwrap();

        set_static_speed_with_zero_rpm(&curve_path, Some(&zrp_path), 50, 5, false).unwrap();
        let zrp_content = fs::read_to_string(&zrp_path).unwrap();
        assert_eq!(zrp_content, "c\n");
    }

    #[test]
    fn reset_to_auto_writes_r_then_c() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fan_curve");
        fs::write(&path, "").unwrap();

        reset_to_auto(&path, None).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "c\n");
    }

    #[test]
    fn reset_to_auto_reenables_zero_rpm() {
        let tmp = tempfile::tempdir().unwrap();
        let curve_path = tmp.path().join("fan_curve");
        let zrp_path = tmp.path().join("fan_zero_rpm_enable");
        fs::write(&curve_path, "").unwrap();
        fs::write(&zrp_path, "0\n").unwrap();

        reset_to_auto(&curve_path, Some(&zrp_path)).unwrap();
        // zero-RPM should be re-enabled (last write is commit "c\n")
        let zrp_content = fs::read_to_string(&zrp_path).unwrap();
        assert_eq!(zrp_content, "c\n");
    }

    #[test]
    fn read_missing_file_returns_error() {
        let result = read_fan_curve(Path::new("/nonexistent/fan_curve"));
        assert!(result.is_err());
    }

    #[test]
    fn speed_clamped_to_100() {
        let content = "OD_FAN_CURVE:\n0: 40C 120%\n";
        let curve = parse_fan_curve(content).unwrap();
        assert_eq!(curve.points[0].speed_pct, 100);
    }

    // ── Legacy PWM tests ───────────────────────────────────────────

    #[test]
    fn set_legacy_pwm_writes_enable_and_value() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path();
        fs::write(hwmon.join("pwm1_enable"), "").unwrap();
        fs::write(hwmon.join("pwm1"), "").unwrap();

        set_legacy_pwm(hwmon, 50).unwrap();

        let enable = fs::read_to_string(hwmon.join("pwm1_enable")).unwrap();
        assert_eq!(enable, "1\n");
        let pwm = fs::read_to_string(hwmon.join("pwm1")).unwrap();
        assert_eq!(pwm, "128\n");
    }

    #[test]
    fn set_legacy_pwm_full_speed() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path();
        fs::write(hwmon.join("pwm1_enable"), "").unwrap();
        fs::write(hwmon.join("pwm1"), "").unwrap();

        set_legacy_pwm(hwmon, 100).unwrap();
        let pwm = fs::read_to_string(hwmon.join("pwm1")).unwrap();
        assert_eq!(pwm, "255\n");
    }

    #[test]
    fn reset_legacy_to_auto_writes_enable_2() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp.path();
        fs::write(hwmon.join("pwm1_enable"), "1\n").unwrap();

        reset_legacy_to_auto(hwmon).unwrap();

        let enable = fs::read_to_string(hwmon.join("pwm1_enable")).unwrap();
        assert_eq!(enable, "2\n");
    }

    #[test]
    fn set_legacy_pwm_missing_path_returns_error() {
        let result = set_legacy_pwm(Path::new("/nonexistent/hwmon99"), 50);
        assert!(result.is_err());
    }

    #[test]
    fn reset_legacy_missing_path_returns_error() {
        let result = reset_legacy_to_auto(Path::new("/nonexistent/hwmon99"));
        assert!(result.is_err());
    }
}
