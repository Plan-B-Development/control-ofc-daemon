//! Profile model — loads and evaluates GUI-created fan curve profiles.
//!
//! Compatible with the GUI's Profile v7 JSON format. v4 adds the per-GPU
//! ``fan_zero_rpm`` flag on ``ControlMember`` (DEC-095); v5 adds the
//! ``stepped`` (staircase) curve type (DEC-148); v6 adds the ``trigger``
//! (two-state latch) curve type (DEC-149); v7 adds the composite ``mix``
//! (combine other curves) and ``sync`` (mirror a control's output) curve types
//! (DEC-150/151, retiring DEC-014). Older profiles deserialise unchanged
//! because new fields use ``#[serde(default)]`` and unknown curve types fall
//! back to 50%.
//! Supports graph (piecewise linear), stepped (staircase), linear (2-point),
//! flat, trigger, mix, and sync curve types. The pure `evaluate_curve` serves
//! the single-temperature types; the composite mix/sync types are resolved with
//! an evaluation context inside the profile engine.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A fan control profile containing logical controls and curve definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonProfile {
    pub id: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub controls: Vec<LogicalControl>,
    #[serde(default)]
    pub curves: Vec<CurveConfig>,
}

fn default_version() -> u32 {
    7
}

/// A logical fan control group with curve assignment and member fans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogicalControl {
    pub id: String,
    pub name: String,
    #[serde(default = "default_mode")]
    pub mode: String, // "curve" or "manual"
    #[serde(default)]
    pub curve_id: String,
    #[serde(default = "default_manual")]
    pub manual_output_pct: f64,
    #[serde(default)]
    pub members: Vec<ControlMember>,
    #[serde(default = "default_step")]
    pub step_up_pct: f64,
    #[serde(default = "default_step")]
    pub step_down_pct: f64,
    #[serde(default)]
    pub offset_pct: f64,
    #[serde(default)]
    pub minimum_pct: f64,
    #[serde(default)]
    pub start_pct: f64,
    #[serde(default)]
    pub stop_pct: f64,
}

fn default_mode() -> String {
    "curve".into()
}
fn default_manual() -> f64 {
    50.0
}
fn default_step() -> f64 {
    100.0
}

/// A fan member within a logical control group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlMember {
    // "openfan", "hwmon", or "amd_gpu" — matches the write phases in
    // profile_engine.rs (apply_commands), which dispatches per-source.
    pub source: String,
    pub member_id: String, // e.g. "openfan:ch00", "hwmon:it8696:...", or "amd_gpu:<PCI_BDF>"
    #[serde(default)]
    pub member_label: String,
    /// Per-GPU-member zero-RPM toggle (v4). When true, the daemon preserves
    /// the PMFW ``fan_zero_rpm_enable`` setting while writing the curve so
    /// the GPU's idle fan-stop behaviour is honoured. Defaults to false to
    /// match the pre-v4 behaviour (zero-RPM disabled before every PMFW
    /// write). Ignored for non-GPU sources. See DEC-095.
    #[serde(default)]
    pub fan_zero_rpm: bool,
}

/// A fan curve configuration (graph, stepped, linear, flat, or trigger).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CurveConfig {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub curve_type: String,
    #[serde(default)]
    pub sensor_id: String,
    #[serde(default)]
    pub points: Vec<CurvePoint>,
    // Linear fields
    #[serde(default)]
    pub start_temp_c: Option<f64>,
    #[serde(default)]
    pub start_output_pct: Option<f64>,
    #[serde(default)]
    pub end_temp_c: Option<f64>,
    #[serde(default)]
    pub end_output_pct: Option<f64>,
    // Flat field
    #[serde(default)]
    pub flat_output_pct: Option<f64>,
    // Trigger fields (two-state latch)
    #[serde(default)]
    pub trigger_idle_temp_c: Option<f64>,
    #[serde(default)]
    pub trigger_load_temp_c: Option<f64>,
    #[serde(default)]
    pub trigger_idle_pct: Option<f64>,
    #[serde(default)]
    pub trigger_load_pct: Option<f64>,
    // Mix fields (combine other curves — DEC-150). `mix_function` is one of
    // max/min/average/sum/subtract (default max); `mix_curve_ids` references
    // other CurveConfig ids in the same profile, each evaluated at its own
    // sensor and combined.
    #[serde(default)]
    pub mix_function: Option<String>,
    #[serde(default)]
    pub mix_curve_ids: Vec<String>,
    // Sync fields (mirror a control's tuned output — DEC-151). `sync_control_id`
    // references a LogicalControl id; `sync_offset_pct` is added to that
    // control's current-tick output.
    #[serde(default)]
    pub sync_control_id: String,
    #[serde(default)]
    pub sync_offset_pct: Option<f64>,
}

/// A single point on a graph curve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurvePoint {
    pub temp_c: f64,
    pub output_pct: f64,
}

/// Evaluate a curve at a given temperature, returning an output percentage (0–100).
pub fn evaluate_curve(curve: &CurveConfig, temp_c: f64) -> f64 {
    match curve.curve_type.as_str() {
        "graph" => evaluate_graph(curve, temp_c),
        "stepped" => evaluate_stepped(curve, temp_c),
        "linear" => evaluate_linear(curve, temp_c),
        "flat" => curve.flat_output_pct.unwrap_or(50.0),
        "trigger" => evaluate_trigger_stateless(curve, temp_c),
        _ => {
            log::warn!(
                "Unknown curve type '{}' for curve '{}', defaulting to 50%",
                curve.curve_type,
                curve.name
            );
            50.0
        }
    }
}

fn evaluate_graph(curve: &CurveConfig, temp_c: f64) -> f64 {
    let points = &curve.points;
    if points.is_empty() {
        return 50.0;
    }
    if points.len() == 1 {
        return points[0].output_pct;
    }
    // Below first point
    if temp_c <= points[0].temp_c {
        return points[0].output_pct;
    }
    // Above last point
    if temp_c >= points[points.len() - 1].temp_c {
        return points[points.len() - 1].output_pct;
    }
    // Piecewise linear interpolation
    for i in 0..points.len() - 1 {
        let p0 = &points[i];
        let p1 = &points[i + 1];
        if temp_c >= p0.temp_c && temp_c <= p1.temp_c {
            let range = p1.temp_c - p0.temp_c;
            if range <= 0.0 {
                return p0.output_pct;
            }
            let t = (temp_c - p0.temp_c) / range;
            return p0.output_pct + t * (p1.output_pct - p0.output_pct);
        }
    }
    // Fallthrough (only reachable for non-monotonic temps in a hand-edited
    // profile): mirror the GUI's `_interpolate_graph`, which returns the last
    // point's output, so headless evaluation matches the GUI preview. Pinned by
    // the cross-stack parity harness (DEC-126).
    points[points.len() - 1].output_pct
}

/// Evaluate a stepped (staircase) curve: hold each point's output until the
/// next point's temperature is reached (lower-point-wins). Shares the graph
/// point model; only the fill rule differs. Below the first point clamps to
/// the first output; at/above the last point clamps to the last output. Must
/// stay byte-for-byte identical to the GUI's ``_interpolate_stepped``
/// (DEC-126 / DEC-148).
fn evaluate_stepped(curve: &CurveConfig, temp_c: f64) -> f64 {
    let points = &curve.points;
    if points.is_empty() {
        return 50.0;
    }
    let last = points.len() - 1;
    if temp_c <= points[0].temp_c {
        return points[0].output_pct;
    }
    if temp_c >= points[last].temp_c {
        return points[last].output_pct;
    }
    for i in 0..last {
        if temp_c >= points[i].temp_c && temp_c < points[i + 1].temp_c {
            return points[i].output_pct;
        }
    }
    points[last].output_pct
}

fn evaluate_linear(curve: &CurveConfig, temp_c: f64) -> f64 {
    let start_t = curve.start_temp_c.unwrap_or(30.0);
    let start_o = curve.start_output_pct.unwrap_or(20.0);
    let end_t = curve.end_temp_c.unwrap_or(80.0);
    let end_o = curve.end_output_pct.unwrap_or(100.0);

    if temp_c <= start_t {
        return start_o;
    }
    if temp_c >= end_t {
        return end_o;
    }
    let range = end_t - start_t;
    if range <= 0.0 {
        return start_o;
    }
    let t = (temp_c - start_t) / range;
    start_o + t * (end_o - start_o)
}

/// Trigger-curve default thresholds (DEC-149). Cross-stack parity: the GUI's
/// retained stateless `_interpolate_trigger` tier defaults to the same values
/// (the latched evaluator is daemon-only since the 2.0.0 cutover, DEC-165),
/// and the `parity_vectors.json` oracle (DEC-126) is authored against them —
/// do not change without updating the oracle in lockstep. `pub(crate)` so
/// `profile_engine::curve_eval` shares them.
pub(crate) const TRIGGER_IDLE_TEMP_C: f64 = 40.0;
pub(crate) const TRIGGER_LOAD_TEMP_C: f64 = 60.0;
pub(crate) const TRIGGER_IDLE_PCT: f64 = 30.0;
pub(crate) const TRIGGER_LOAD_PCT: f64 = 80.0;

/// Stateless (cold-start) trigger value: the load speed at/above the load
/// temperature, else the idle speed. The latching hysteresis — holding the load
/// state down through the idle..load band — is applied per-control by the
/// profile engine, NOT here, so `evaluate_curve` stays a pure function for the
/// `curve_eval` parity tier. Must match the GUI's `_interpolate_trigger`
/// (DEC-126 / DEC-149).
fn evaluate_trigger_stateless(curve: &CurveConfig, temp_c: f64) -> f64 {
    let load_temp = curve.trigger_load_temp_c.unwrap_or(TRIGGER_LOAD_TEMP_C);
    let load_pct = curve.trigger_load_pct.unwrap_or(TRIGGER_LOAD_PCT);
    let idle_pct = curve.trigger_idle_pct.unwrap_or(TRIGGER_IDLE_PCT);
    if temp_c >= load_temp {
        load_pct
    } else {
        idle_pct
    }
}

/// Load a profile from a JSON file.
pub fn load_profile(path: &Path) -> Result<DaemonProfile, String> {
    let content = crate::atomic_io::read_to_string_capped(path)
        .map_err(|e| format!("failed to read profile '{}': {e}", path.display()))?;
    let profile: DaemonProfile = serde_json::from_str(&content)
        .map_err(|e| format!("failed to parse profile '{}': {e}", path.display()))?;
    // Recursion bound (see MAX_PROFILE_CURVES). The boot paths (CLI --profile and
    // persisted-state restore) deliberately skip validate(), so this load-time
    // check is the net that keeps an oversized on-disk profile — hand-placed in
    // /etc/control-ofc/profiles, or predating this cap — from aborting the daemon
    // at startup. Callers already fail safe to imperative mode on Err.
    if profile.curves.len() > MAX_PROFILE_CURVES {
        return Err(format!(
            "profile '{}' has {} curves, exceeding the maximum of {MAX_PROFILE_CURVES}",
            path.display(),
            profile.curves.len()
        ));
    }
    if profile.controls.len() > MAX_PROFILE_CONTROLS {
        return Err(format!(
            "profile '{}' has {} controls, exceeding the maximum of {MAX_PROFILE_CONTROLS}",
            path.display(),
            profile.controls.len()
        ));
    }
    // DEC-249: numeric net. The count caps above stop an oversized profile from
    // aborting the daemon; this stops an out-of-range one from reaching the
    // engine, which the same boot paths would otherwise feed unvalidated.
    if let Err(e) = check_numeric_ranges(&profile) {
        return Err(format!(
            "profile '{}' has an out-of-range value: {e}",
            path.display()
        ));
    }
    if profile.version < 3 {
        log::warn!(
            "Profile '{}' has version {}, expected 3+ (v4 introduces fan_zero_rpm)",
            profile.name,
            profile.version
        );
    }
    log::info!(
        "Loaded profile '{}' ({} controls, {} curves)",
        profile.name,
        profile.controls.len(),
        profile.curves.len()
    );
    Ok(profile)
}

/// Load-time numeric net for a profile read from disk (DEC-249).
///
/// [`validate`] bounds every numeric field, but the boot paths — CLI
/// `--profile` and persisted-state restore — deliberately skip it (see
/// [`load_profile`]), so until this net existed an out-of-range or non-finite
/// value on disk reached the engine unchecked. One was enough: a negative
/// `step_up_pct` / `step_down_pct` pair inverted the step-rate window and
/// panicked the engine task mid-tick, killing the sole PWM writer *and* the
/// thermal leg while `/status` kept answering 200. `apply_tuning` no
/// longer panics on that input either — this is the other half, so nothing the
/// API would reject **on numeric grounds** can reach the engine from disk.
///
/// Deliberately narrower than `validate()`, which also rejects `TOO_MANY_POINTS`,
/// `TRIGGER_IDLE_GE_LOAD`, `UNKNOWN_CURVE_REF`, `MIX_CYCLE`/`SYNC_CYCLE`,
/// `FLOOR_TOO_LOW` and `PUMP_STOP_FORBIDDEN`. Those still reach the engine on the
/// boot paths and are each handled at eval time (visited-set and topo guards,
/// the floor clamp, unknown-curve fallback) — do not read this as a full
/// `validate()`.
///
/// Deliberately **numeric-only**, not a back door to full `validate()`. The boot
/// paths skip `validate` because a profile may legitimately reference a sensor
/// or header this machine does not have right now, and those stay tolerated (the
/// engine falls back safely per member). Ranges mirror `check_pct` (0..=100),
/// `check_offset` (-100..=100) and `check_finite` exactly, so any profile the
/// API accepts still loads.
fn check_numeric_ranges(profile: &DaemonProfile) -> Result<(), String> {
    fn finite(field: String, v: f64) -> Result<(), String> {
        if v.is_finite() {
            Ok(())
        } else {
            Err(format!("{field} must be a finite number (got {v})"))
        }
    }
    fn in_range(field: String, v: f64, lo: f64, hi: f64) -> Result<(), String> {
        finite(field.clone(), v)?;
        if (lo..=hi).contains(&v) {
            Ok(())
        } else {
            Err(format!("{field} must be between {lo} and {hi} (got {v})"))
        }
    }

    for (i, ctrl) in profile.controls.iter().enumerate() {
        let p = format!("controls[{i}]");
        in_range(
            format!("{p}.manual_output_pct"),
            ctrl.manual_output_pct,
            0.0,
            100.0,
        )?;
        in_range(format!("{p}.minimum_pct"), ctrl.minimum_pct, 0.0, 100.0)?;
        in_range(format!("{p}.start_pct"), ctrl.start_pct, 0.0, 100.0)?;
        in_range(format!("{p}.stop_pct"), ctrl.stop_pct, 0.0, 100.0)?;
        in_range(format!("{p}.step_up_pct"), ctrl.step_up_pct, 0.0, 100.0)?;
        in_range(format!("{p}.step_down_pct"), ctrl.step_down_pct, 0.0, 100.0)?;
        in_range(format!("{p}.offset_pct"), ctrl.offset_pct, -100.0, 100.0)?;
    }

    for (i, curve) in profile.curves.iter().enumerate() {
        let p = format!("curves[{i}]");
        for (j, pt) in curve.points.iter().enumerate() {
            finite(format!("{p}.points[{j}].temp_c"), pt.temp_c)?;
            in_range(
                format!("{p}.points[{j}].output_pct"),
                pt.output_pct,
                0.0,
                100.0,
            )?;
        }
        for (name, v) in [
            ("start_temp_c", curve.start_temp_c),
            ("end_temp_c", curve.end_temp_c),
            ("trigger_idle_temp_c", curve.trigger_idle_temp_c),
            ("trigger_load_temp_c", curve.trigger_load_temp_c),
        ] {
            if let Some(v) = v {
                finite(format!("{p}.{name}"), v)?;
            }
        }
        for (name, v) in [
            ("start_output_pct", curve.start_output_pct),
            ("end_output_pct", curve.end_output_pct),
            ("flat_output_pct", curve.flat_output_pct),
            ("trigger_idle_pct", curve.trigger_idle_pct),
            ("trigger_load_pct", curve.trigger_load_pct),
        ] {
            if let Some(v) = v {
                in_range(format!("{p}.{name}"), v, 0.0, 100.0)?;
            }
        }
        if let Some(v) = curve.sync_offset_pct {
            in_range(format!("{p}.sync_offset_pct"), v, -100.0, 100.0)?;
        }
    }

    Ok(())
}

/// Maximum byte length of a profile id. The on-disk filename is `{id}.json`;
/// Linux `NAME_MAX` is 255 bytes, so 128 leaves ample headroom while staying a
/// human-reasonable id length. Compared in bytes, not chars, because the
/// filesystem limit is on encoded bytes (DEC-173).
pub const MAX_PROFILE_ID_BYTES: usize = 128;

/// Whether a profile id is safe to use as a filename stem (`{id}.json`).
///
/// Rejects empty ids and any containing `/`, `\`, or `..` (CWE-22 path
/// traversal), plus two filesystem-safety limits (DEC-173): ids longer than
/// [`MAX_PROFILE_ID_BYTES`] bytes — an over-long id would otherwise surface as
/// an opaque `500 ENAMETOOLONG` from the filesystem instead of a clean `400` —
/// and any Unicode control character (C0/C1 + `DEL`; subsumes the old null-byte
/// check and keeps log lines / error envelopes free of control bytes). The
/// single source of the id-safety rule for both `find_profile` (activation) and
/// `profile_store` (CRUD writes).
pub fn is_safe_profile_id(id: &str) -> bool {
    !(id.is_empty()
        || id.len() > MAX_PROFILE_ID_BYTES
        || id.contains('/')
        || id.contains('\\')
        || id.contains("..")
        || id.chars().any(char::is_control))
}

/// Search for a profile by name in the given search directories.
///
/// The name must be a simple filename stem (no path separators or traversal
/// components). Names containing `/`, `\`, `..`, or null bytes are rejected
/// to prevent CWE-22 path traversal.
pub fn find_profile(name: &str, search_dirs: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    if !is_safe_profile_id(name) {
        log::warn!("rejected profile name with path traversal characters: {name:?}");
        return None;
    }

    for dir in search_dirs {
        let file = dir.join(format!("{name}.json"));
        if file.exists() {
            return Some(file);
        }
    }
    None
}

// ─────────────────────────── Validation (DEC-160) ───────────────────────────

/// Maximum number of points a graph/stepped curve may contain. Mirrors the
/// GUI's `MAX_CURVE_POINTS` (profile_service.py) so a profile the GUI accepts
/// the daemon also accepts, and vice-versa.
pub const MAX_CURVE_POINTS: usize = 256;

/// Maximum number of curves a profile may contain.
///
/// This is a **recursion bound**, not a taste limit. `profile_engine::curve_eval`
/// resolves Mix dependencies by mutual recursion (`resolve_curve_output` ↔
/// `resolve_mix`), so a chain of N mix curves recurses N frames deep. The
/// `visited` set rejects *cycles*, not *depth*, so a deep but perfectly acyclic
/// chain used to overflow the stack and abort the process (SIGABRT) on the next
/// engine tick — a reachable, reboot-surviving DoS of the sole PWM writer, since
/// activation persists `active_profile_id` and boot re-activates it.
///
/// 256 is far above any real cooling setup (a busy machine uses ~10–20 curves)
/// and far below the empirical overflow threshold. Enforced in three places:
/// [`validate`] (API front door, structured `TOO_MANY_CURVES`), [`load_profile`]
/// (every on-disk path — the boot paths deliberately skip `validate`), and a
/// depth guard in `resolve_mix` (eval-time last resort). Mirrors the GUI's
/// `MAX_PROFILE_CURVES` (profile_service.py).
pub const MAX_PROFILE_CURVES: usize = 256;

/// Maximum number of controls a profile may contain. Companion to
/// [`MAX_PROFILE_CURVES`], and enforced at the same three layers:
/// `curve_eval::topo_visit` recurses once per Sync-chained control, so [`validate`]
/// and [`load_profile`] cap the collection and `topo_visit` carries its own depth
/// backstop. Keeping the two symmetric matters — an asymmetry here would mean a
/// future path that reached the engine unvalidated failed safe for Mix but
/// overflowed the stack for Sync.
pub const MAX_PROFILE_CONTROLS: usize = 256;

const KNOWN_CURVE_TYPES: [&str; 7] = [
    "graph", "stepped", "linear", "flat", "trigger", "mix", "sync",
];
const KNOWN_MIX_FUNCTIONS: [&str; 5] = ["max", "min", "average", "sum", "subtract"];
const KNOWN_MEMBER_SOURCES: [&str; 5] = ["openfan", "hwmon", "amd_gpu", "intel_gpu", "nvidia_gpu"];

// ──────────────────── Role classification + floor (DEC-162) ────────────────────

/// Hard minimum-PWM floor for pump / CPU headers. Mirror of the GUI's
/// `ROLE_MINIMUM_PCT[CPU_PUMP]` (profile_service.py). A pump driven below this
/// can stall — coolant-flow loss leads to rapid thermal runaway — so the daemon
/// enforces it independently of the GUI-stamped `minimum_pct`, both at validate
/// time (reject, see [`validate`]) and at eval time (clamp, see `profile_engine`).
pub(crate) const HARD_PUMP_CPU_FLOOR_PCT: f64 = 30.0;

/// Header-label keywords (case-insensitive substring) marking a hwmon member as a
/// CPU or pump header. Mirror of the GUI's `_CPU_PUMP_LABEL_HINTS`. Distinct from
/// `hwmon::aio` `COOLANT_LABEL_HINTS`, which classifies temperature *sensors*.
const CPU_PUMP_LABEL_HINTS: &[&str] = &["cpu", "pump", "aio"];

/// True when a control member is a GPU fan (`amd_gpu`, `intel_gpu`, or
/// `nvidia_gpu`). GPU fans carry no daemon floor — PMFW enforces its own
/// OD_RANGE minimum (DEC-119). Intel + NVIDIA are read-only (no backend writes
/// them). Mirrors the GUI's `infer_member_role` GPU branch.
pub(crate) fn member_is_gpu(member: &ControlMember) -> bool {
    member.source == "amd_gpu" || member.source == "intel_gpu" || member.source == "nvidia_gpu"
}

/// True when a control member is a pump/CPU header that needs the hard floor.
///
/// Document-only classification (no live hardware): a `hwmon` member whose label
/// hints "cpu"/"pump"/"aio", or whose stable id embeds a known liquid-cooler chip
/// (`hwmon:<chip>:<device>:pwmN:<label>`). Mirrors the GUI's `infer_member_role`
/// CPU/pump branch (`_label_indicates_cpu_or_pump` + `_member_is_aio_header`), so
/// a profile the GUI bakes always agrees with the daemon backstop. The shared
/// `role_classification.json` fixture pins that agreement. OpenFan and GPU members
/// are never pump/CPU.
pub(crate) fn member_is_pump_or_cpu(member: &ControlMember) -> bool {
    if member.source != "hwmon" {
        return false;
    }
    let label = member.member_label.to_lowercase();
    if CPU_PUMP_LABEL_HINTS.iter().any(|hint| label.contains(hint)) {
        return true;
    }
    // Chip name is the 2nd colon-delimited field of the stable hwmon id; a
    // malformed id yields "" → not a cooler (matches the GUI's split fallback).
    let chip = member.member_id.split(':').nth(1).unwrap_or("");
    crate::hwmon::aio::is_liquid_cooler_chip(chip)
}

/// The label the *daemon itself* discovered for a hwmon header, read back out of
/// the member's stable id.
///
/// `pwm_discovery` mints the id as `hwmon:{chip}:{device_id}:pwm{N}:{label}`, so
/// the daemon's own view of the header's name travels with every member for free
/// — no hardware access and no schema change.
///
/// [SAFETY] This field cannot be forged into something *weaker*. The hwmon write
/// path resolves a member by exact-string lookup (`headers.get(id)`), so an id
/// whose label has been edited does not match any discovered header and nothing
/// is ever written through it. A client can therefore only ever hand us the
/// daemon's real label, or an id that is inert.
///
/// Split on the `:pwm` marker rather than by field index: `device_id` is a PCI
/// BDF (`0000:00:18.3`) and carries its own colons, and a label may too — taking
/// everything after the index keeps a label like `CPU:FAN` intact.
fn daemon_label_from_member_id(member_id: &str) -> Option<&str> {
    let (_, after_marker) = member_id.split_once(":pwm")?;
    let (_index, label) = after_marker.split_once(':')?;
    Some(label)
}

/// Whether a member must be held at the pump/CPU hard floor **at eval time**
/// (DEC-252).
///
/// A superset of [`member_is_pump_or_cpu`]: the floor applies if the *author's*
/// label says pump/CPU, **or** the daemon's own discovered label does. Union,
/// never replacement — the daemon's view can only ever *add* a floor, never
/// remove one the author asked for.
///
/// The gap this closes: `member_label` is written by the client, and the GUI
/// resolves it through a display-name tier list, so renaming a `PUMP` header to
/// "Radiator Top" used to drop it from a 30% floor to 20% with nothing to catch
/// it (a documented past regression — DEC-228 shipped three of these). The
/// daemon held the real label the whole time and never looked.
///
/// **Deliberately not used by [`validate`].** Its `FLOOR_TOO_LOW` /
/// `PUMP_STOP_FORBIDDEN` errors *reject* a profile, and a daemon that rejected
/// more than the paired GUI stamps would break profile saving for anyone who
/// upgraded the daemon first — the GUI bakes `minimum_pct` from its own
/// classifier. Both `validate` sites already document the engine as the
/// independent eval-time backstop for anything that reaches it unvalidated; this
/// strengthens that backstop without moving the rejection line. When the GUI
/// adopts the same union it will simply stamp the higher floor itself.
///
/// **Known limit, not a hidden one:** `read_label` synthesises `pwm{N}` when a
/// chip publishes no label file (`pwm_discovery`), and the daemon parses no
/// `/etc/sensors.d`. On such a board the daemon's label is `pwm7`, carries no
/// hint, and this adds nothing — the author's label remains the only signal.
/// It bites exactly where the chip does publish a real name.
pub(crate) fn member_needs_hard_floor(member: &ControlMember) -> bool {
    if member_is_pump_or_cpu(member) {
        return true;
    }
    if member.source != "hwmon" {
        return false;
    }
    daemon_label_from_member_id(&member.member_id).is_some_and(|label| {
        let lower = label.to_lowercase();
        CPU_PUMP_LABEL_HINTS.iter().any(|hint| lower.contains(hint))
    })
}

/// Severity of a [`FieldViolation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Hard error — the profile is rejected (not stored / not activated).
    Error,
    /// Soft warning — the profile is accepted, but the condition is surfaced
    /// (e.g. it references a sensor not present on this machine, so a control
    /// using that sensor alone holds its last commanded duty until the sensor
    /// appears, while a Mix runs on its surviving inputs — DEC-272).
    Warning,
}

/// A single validation finding tied to a field path.
#[derive(Debug, Clone, Serialize)]
pub struct FieldViolation {
    /// Dotted/indexed path, e.g. `curves[2].points[5].output_pct`.
    pub field: String,
    /// Stable UPPER_SNAKE_CASE machine code, e.g. `OUT_OF_RANGE`. Clients map
    /// this to a localized message; never string-match `description`.
    pub reason: String,
    /// Human-readable explanation.
    pub description: String,
    pub severity: Severity,
}

/// The result of [`validate`]: hard `errors` (reject) and soft `warnings`
/// (accept + surface). See DEC-160.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ValidationReport {
    pub errors: Vec<FieldViolation>,
    pub warnings: Vec<FieldViolation>,
}

impl ValidationReport {
    /// A profile is valid (storable / activatable) iff it has no hard errors.
    /// Warnings never block.
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// All findings (errors then warnings) under a `field_violations` key, for
    /// the error-envelope `details` and the `?validate_only` response body.
    pub fn field_violations_json(&self) -> serde_json::Value {
        let all: Vec<&FieldViolation> = self.errors.iter().chain(self.warnings.iter()).collect();
        serde_json::json!({ "field_violations": all })
    }

    fn error(&mut self, field: impl Into<String>, reason: &str, description: impl Into<String>) {
        self.errors.push(FieldViolation {
            field: field.into(),
            reason: reason.into(),
            description: description.into(),
            severity: Severity::Error,
        });
    }

    fn warn(&mut self, field: impl Into<String>, reason: &str, description: impl Into<String>) {
        self.warnings.push(FieldViolation {
            field: field.into(),
            reason: reason.into(),
            description: description.into(),
            severity: Severity::Warning,
        });
    }
}

/// Validate a profile's structure and intra-profile referential integrity.
///
/// Returns hard `errors` (the profile is rejected) and soft `warnings`
/// (accepted, surfaced). The split is deliberate (DEC-160):
///
/// * **Machine-independent invariants are hard errors** — non-finite numbers,
///   out-of-range percentages, point-count limits, trigger temp ordering,
///   intra-profile reference integrity (`curve_id` / `mix_curve_ids` /
///   `sync_control_id` resolve), and Mix/Sync acyclicity. These are wrong on
///   any machine.
/// * **Machine-dependent facts are warnings only** — a `sensor_id` not present
///   on *this* host. The engine tolerates a missing sensor at eval time without
///   dropping cooling, so a profile authored on another machine must still
///   store, validate, and import. What it actually does differs by curve shape
///   (DEC-272): a curve naming the missing sensor **alone** is skipped and its
///   fans hold their last commanded duty — never 0%, never lower — while a
///   **Mix** runs on the inputs it does have. The thermal force backstops
///   either way.
///
/// `known_sensor_ids` is the set of sensor entity ids currently discovered on
/// this machine (the keys of `cache.sensors_snapshot()`).
///
/// The role-aware minimum-PWM floor backstop (DEC-162) is folded in below: a
/// control with a pump/CPU member declaring `minimum_pct` below
/// [`HARD_PUMP_CPU_FLOOR_PCT`] is a hard `FLOOR_TOO_LOW` error. Classification is
/// document-only (`member_is_pump_or_cpu`), so this needed no signature change.
pub fn validate(profile: &DaemonProfile, known_sensor_ids: &HashSet<String>) -> ValidationReport {
    let mut report = ValidationReport::default();

    let curve_ids: HashSet<&str> = profile.curves.iter().map(|c| c.id.as_str()).collect();
    let control_ids: HashSet<&str> = profile.controls.iter().map(|c| c.id.as_str()).collect();

    // ── Collection sizes (recursion bounds — see MAX_PROFILE_CURVES) ──
    if profile.curves.len() > MAX_PROFILE_CURVES {
        report.error(
            "curves".to_string(),
            "TOO_MANY_CURVES",
            format!(
                "{} curves exceeds the maximum of {MAX_PROFILE_CURVES}",
                profile.curves.len()
            ),
        );
    }
    if profile.controls.len() > MAX_PROFILE_CONTROLS {
        report.error(
            "controls".to_string(),
            "TOO_MANY_CONTROLS",
            format!(
                "{} controls exceeds the maximum of {MAX_PROFILE_CONTROLS}",
                profile.controls.len()
            ),
        );
    }

    // ── Curves ──
    for (i, curve) in profile.curves.iter().enumerate() {
        let p = format!("curves[{i}]");

        if !KNOWN_CURVE_TYPES.contains(&curve.curve_type.as_str()) {
            report.warn(
                format!("{p}.type"),
                "UNKNOWN_CURVE_TYPE",
                format!(
                    "curve type '{}' is unrecognised; the engine will output 50%",
                    curve.curve_type
                ),
            );
        }

        if curve.points.len() > MAX_CURVE_POINTS {
            report.error(
                format!("{p}.points"),
                "TOO_MANY_POINTS",
                format!(
                    "{} points exceeds the maximum of {MAX_CURVE_POINTS}",
                    curve.points.len()
                ),
            );
        }
        for (j, pt) in curve.points.iter().enumerate() {
            check_finite(&mut report, format!("{p}.points[{j}].temp_c"), pt.temp_c);
            check_pct(
                &mut report,
                format!("{p}.points[{j}].output_pct"),
                pt.output_pct,
            );
        }

        check_opt_finite(&mut report, &p, "start_temp_c", curve.start_temp_c);
        check_opt_finite(&mut report, &p, "end_temp_c", curve.end_temp_c);
        check_opt_pct(&mut report, &p, "start_output_pct", curve.start_output_pct);
        check_opt_pct(&mut report, &p, "end_output_pct", curve.end_output_pct);
        check_opt_pct(&mut report, &p, "flat_output_pct", curve.flat_output_pct);
        check_opt_finite(
            &mut report,
            &p,
            "trigger_idle_temp_c",
            curve.trigger_idle_temp_c,
        );
        check_opt_finite(
            &mut report,
            &p,
            "trigger_load_temp_c",
            curve.trigger_load_temp_c,
        );
        check_opt_pct(&mut report, &p, "trigger_idle_pct", curve.trigger_idle_pct);
        check_opt_pct(&mut report, &p, "trigger_load_pct", curve.trigger_load_pct);
        check_opt_offset(&mut report, &p, "sync_offset_pct", curve.sync_offset_pct);

        // Sensor reference is machine-dependent → warning, never an error.
        if !curve.sensor_id.is_empty() && !known_sensor_ids.contains(&curve.sensor_id) {
            report.warn(
                format!("{p}.sensor_id"),
                "UNKNOWN_SENSOR",
                format!(
                    "sensor '{}' is not present on this machine. A curve using it on its \
                     own will not command its fans — they hold their last speed; a \
                     combined (Mix) curve will run on the inputs it does have",
                    curve.sensor_id
                ),
            );
        }

        match curve.curve_type.as_str() {
            "trigger" => {
                let idle = curve.trigger_idle_temp_c.unwrap_or(TRIGGER_IDLE_TEMP_C);
                let load = curve.trigger_load_temp_c.unwrap_or(TRIGGER_LOAD_TEMP_C);
                if idle.is_finite() && load.is_finite() && idle >= load {
                    report.error(
                        format!("{p}.trigger_idle_temp_c"),
                        "TRIGGER_IDLE_GE_LOAD",
                        format!(
                            "idle temp {idle} must be below load temp {load} or the trigger oscillates"
                        ),
                    );
                }
            }
            "mix" => {
                if let Some(f) = curve.mix_function.as_deref() {
                    if !KNOWN_MIX_FUNCTIONS.contains(&f) {
                        report.warn(
                            format!("{p}.mix_function"),
                            "UNKNOWN_MIX_FUNCTION",
                            format!(
                                "mix function '{f}' is unrecognised; the engine defaults to 'max'"
                            ),
                        );
                    }
                }
                for (k, mid) in curve.mix_curve_ids.iter().enumerate() {
                    if !curve_ids.contains(mid.as_str()) {
                        report.error(
                            format!("{p}.mix_curve_ids[{k}]"),
                            "UNKNOWN_CURVE_REF",
                            format!(
                                "mix references curve '{mid}', which does not exist in this profile"
                            ),
                        );
                    }
                }
            }
            "sync" => {
                let sid = curve.sync_control_id.as_str();
                if sid.is_empty() || !control_ids.contains(sid) {
                    report.error(
                        format!("{p}.sync_control_id"),
                        "UNKNOWN_CONTROL_REF",
                        format!(
                            "sync references control '{}', which does not exist in this profile",
                            curve.sync_control_id
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    // ── Controls ──
    for (i, ctrl) in profile.controls.iter().enumerate() {
        let p = format!("controls[{i}]");
        check_pct(
            &mut report,
            format!("{p}.manual_output_pct"),
            ctrl.manual_output_pct,
        );
        check_pct(&mut report, format!("{p}.minimum_pct"), ctrl.minimum_pct);
        check_pct(&mut report, format!("{p}.start_pct"), ctrl.start_pct);
        check_pct(&mut report, format!("{p}.stop_pct"), ctrl.stop_pct);
        check_pct(&mut report, format!("{p}.step_up_pct"), ctrl.step_up_pct);
        check_pct(
            &mut report,
            format!("{p}.step_down_pct"),
            ctrl.step_down_pct,
        );
        check_offset(&mut report, format!("{p}.offset_pct"), ctrl.offset_pct);

        if ctrl.mode == "curve"
            && (ctrl.curve_id.is_empty() || !curve_ids.contains(ctrl.curve_id.as_str()))
        {
            report.error(
                format!("{p}.curve_id"),
                "UNKNOWN_CURVE_REF",
                format!(
                    "control is in curve mode but curve '{}' does not exist in this profile",
                    ctrl.curve_id
                ),
            );
        }

        for (k, m) in ctrl.members.iter().enumerate() {
            if !KNOWN_MEMBER_SOURCES.contains(&m.source.as_str()) {
                report.warn(
                    format!("{p}.members[{k}].source"),
                    "UNKNOWN_SOURCE",
                    format!("member source '{}' is unrecognised", m.source),
                );
            }
        }

        // DEC-162: a control with a pump/CPU member must declare a floor at least
        // as high as the hard pump floor, or the pump can stall. Reject so a
        // too-low floor never persists; the engine also clamps at eval time for
        // any profile that reaches it un-validated (boot-load / hand-edit). GPU
        // and chassis-only controls never trigger this.
        if ctrl.minimum_pct < HARD_PUMP_CPU_FLOOR_PCT
            && ctrl.members.iter().any(member_is_pump_or_cpu)
        {
            report.error(
                format!("{p}.minimum_pct"),
                "FLOOR_TOO_LOW",
                format!(
                    "control has a pump/CPU member but minimum_pct {} is below the \
                     required {HARD_PUMP_CPU_FLOOR_PCT}% pump floor",
                    ctrl.minimum_pct
                ),
            );
        }

        // DEC-167: a pump must never be configured to stop. A non-zero stop_pct
        // can snap a pump/CPU control's output to 0 when it falls below the
        // threshold — even though the control is hard-floored — and stopping a
        // pump risks coolant-flow loss and rapid thermal runaway. Reject so the
        // shape never persists; the engine also skips the stop-snap for pump/CPU
        // members at eval time for any profile that reaches it un-validated
        // (boot-load / hand-edit). GPU and chassis-only controls are unaffected.
        if ctrl.stop_pct != 0.0 && ctrl.members.iter().any(member_is_pump_or_cpu) {
            report.error(
                format!("{p}.stop_pct"),
                "PUMP_STOP_FORBIDDEN",
                format!(
                    "control has a pump/CPU member but stop_pct {} is non-zero; a \
                     pump must never be configured to stop (coolant-flow loss leads \
                     to rapid thermal runaway)",
                    ctrl.stop_pct
                ),
            );
        }
    }

    // ── Cycle detection (Mix over curves, Sync over controls) ──
    if let Some(node) = mix_cycle(profile) {
        report.error(
            "curves",
            "MIX_CYCLE",
            format!("mix curves form a dependency cycle involving '{node}'"),
        );
    }
    if let Some(node) = sync_cycle(profile) {
        report.error(
            "controls",
            "SYNC_CYCLE",
            format!("sync controls form a dependency cycle involving control '{node}'"),
        );
    }

    report
}

fn check_finite(r: &mut ValidationReport, field: String, v: f64) {
    if !v.is_finite() {
        r.error(field, "NON_FINITE", "value must be a finite number");
    }
}

fn check_pct(r: &mut ValidationReport, field: String, v: f64) {
    if !v.is_finite() {
        r.error(field, "NON_FINITE", "value must be a finite number");
    } else if !(0.0..=100.0).contains(&v) {
        r.error(
            field,
            "OUT_OF_RANGE",
            format!("value {v} must be between 0 and 100"),
        );
    }
}

fn check_offset(r: &mut ValidationReport, field: String, v: f64) {
    if !v.is_finite() {
        r.error(field, "NON_FINITE", "value must be a finite number");
    } else if !(-100.0..=100.0).contains(&v) {
        r.error(
            field,
            "OUT_OF_RANGE",
            format!("offset {v} must be between -100 and 100"),
        );
    }
}

fn check_opt_finite(r: &mut ValidationReport, prefix: &str, name: &str, v: Option<f64>) {
    if let Some(v) = v {
        check_finite(r, format!("{prefix}.{name}"), v);
    }
}

fn check_opt_pct(r: &mut ValidationReport, prefix: &str, name: &str, v: Option<f64>) {
    if let Some(v) = v {
        check_pct(r, format!("{prefix}.{name}"), v);
    }
}

fn check_opt_offset(r: &mut ValidationReport, prefix: &str, name: &str, v: Option<f64>) {
    if let Some(v) = v {
        check_offset(r, format!("{prefix}.{name}"), v);
    }
}

/// Detect a cycle among `mix` curves (a curve referencing, transitively,
/// itself via `mix_curve_ids`). Returns an involved curve id if found.
fn mix_cycle(profile: &DaemonProfile) -> Option<String> {
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for c in &profile.curves {
        if c.curve_type == "mix" {
            adj.insert(c.id.clone(), c.mix_curve_ids.clone());
        }
    }
    graph_first_cycle(&adj)
}

/// Detect a cycle among `sync` controls (a control whose sync target,
/// transitively, mirrors itself). Returns an involved control id if found.
fn sync_cycle(profile: &DaemonProfile) -> Option<String> {
    // curve_id -> target control id, for sync curves only.
    let sync_target: HashMap<&str, &str> = profile
        .curves
        .iter()
        .filter(|c| c.curve_type == "sync")
        .map(|c| (c.id.as_str(), c.sync_control_id.as_str()))
        .collect();
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for ctrl in &profile.controls {
        if ctrl.mode == "curve" {
            if let Some(target) = sync_target.get(ctrl.curve_id.as_str()) {
                if !target.is_empty() {
                    adj.insert(ctrl.id.clone(), vec![(*target).to_string()]);
                }
            }
        }
    }
    graph_first_cycle(&adj)
}

/// Iterative DFS cycle detection over a directed adjacency map. Returns the
/// first node found on a back-edge (i.e. part of a cycle), or `None` if the
/// graph is acyclic.
fn graph_first_cycle(adj: &HashMap<String, Vec<String>>) -> Option<String> {
    let mut done: HashSet<String> = HashSet::new();
    for start in adj.keys() {
        if done.contains(start) {
            continue;
        }
        let mut on_path: HashSet<String> = HashSet::new();
        let mut stack: Vec<(String, usize)> = vec![(start.clone(), 0)];
        on_path.insert(start.clone());
        while let Some((node, idx)) = stack.last().cloned() {
            let neighbors = adj.get(&node).map(Vec::as_slice).unwrap_or(&[]);
            if idx < neighbors.len() {
                stack.last_mut().unwrap().1 = idx + 1;
                let next = &neighbors[idx];
                if on_path.contains(next) {
                    return Some(next.clone());
                }
                if !done.contains(next) {
                    on_path.insert(next.clone());
                    stack.push((next.clone(), 0));
                }
            } else {
                on_path.remove(&node);
                done.insert(node.clone());
                stack.pop();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_graph_interpolation() {
        let curve = CurveConfig {
            id: "test".into(),
            name: "Test".into(),
            curve_type: "graph".into(),
            sensor_id: "".into(),
            points: vec![
                CurvePoint {
                    temp_c: 30.0,
                    output_pct: 20.0,
                },
                CurvePoint {
                    temp_c: 60.0,
                    output_pct: 50.0,
                },
                CurvePoint {
                    temp_c: 80.0,
                    output_pct: 100.0,
                },
            ],
            ..Default::default()
        };
        assert!((evaluate_curve(&curve, 30.0) - 20.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 45.0) - 35.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 80.0) - 100.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 20.0) - 20.0).abs() < 0.01); // below range
        assert!((evaluate_curve(&curve, 90.0) - 100.0).abs() < 0.01); // above range
    }

    /// T2 (test-tests audit): mid-segment interpolation with an asymmetric
    /// temperature so the `-` and `*` operators in the interpolation formula
    /// (line ~159) actually matter. A clean halfway point like 45°C on the
    /// 30→60 segment masks `+`/`-` mutations because of x+y == y+x symmetry.
    /// Picks two non-halfway samples on different segments to lock down the
    /// linear-interpolation arithmetic.
    #[test]
    fn evaluate_graph_interpolation_asymmetric_mid_segment() {
        let curve = CurveConfig {
            id: "asym".into(),
            name: "Asymmetric".into(),
            curve_type: "graph".into(),
            sensor_id: "".into(),
            points: vec![
                CurvePoint {
                    temp_c: 30.0,
                    output_pct: 20.0,
                },
                CurvePoint {
                    temp_c: 80.0,
                    output_pct: 100.0,
                },
            ],
            ..Default::default()
        };
        // 47°C: t = (47-30)/(80-30) = 0.34; output = 20 + 0.34*80 = 47.2.
        // A `+` ↔ `-` mutation on either subtraction would shift the result
        // away from 47.2 to a value that fails the strict tolerance below.
        let v = evaluate_curve(&curve, 47.0);
        assert!(
            (v - 47.2).abs() < 0.01,
            "evaluate_curve(47°C) on 30→20, 80→100 expected 47.2, got {v}"
        );

        // 73°C: t = (73-30)/(80-30) = 0.86; output = 20 + 0.86*80 = 88.8.
        // A second asymmetric sample on the same segment catches mutations
        // that happen to be invariant at one specific temperature.
        let v = evaluate_curve(&curve, 73.0);
        assert!(
            (v - 88.8).abs() < 0.01,
            "evaluate_curve(73°C) on 30→20, 80→100 expected 88.8, got {v}"
        );
    }

    /// Multi-segment curve mid-segment interpolation. The 3-knot test above
    /// only checks the 45°C symmetric midpoint; this verifies that the loop
    /// over segments at line ~151 picks the correct segment and uses its
    /// endpoints (not the wrong segment's, not the first/last segment's).
    #[test]
    fn evaluate_graph_picks_correct_segment_for_mid_curve_temp() {
        let curve = CurveConfig {
            id: "multi".into(),
            name: "Multi".into(),
            curve_type: "graph".into(),
            sensor_id: "".into(),
            points: vec![
                CurvePoint {
                    temp_c: 30.0,
                    output_pct: 20.0,
                },
                CurvePoint {
                    temp_c: 60.0,
                    output_pct: 50.0,
                },
                CurvePoint {
                    temp_c: 80.0,
                    output_pct: 100.0,
                },
            ],
            ..Default::default()
        };
        // 67°C lives on the second segment (60→80, 50→100).
        // t = (67-60)/(80-60) = 0.35; output = 50 + 0.35*50 = 67.5.
        // If the loop accidentally evaluated the first segment, the answer
        // would be 50 + (67-30)/(60-30)*(50-20) = 50 + 37 = 87 — not 67.5.
        let v = evaluate_curve(&curve, 67.0);
        assert!(
            (v - 67.5).abs() < 0.01,
            "evaluate_curve(67°C) must pick segment (60,80) and yield 67.5, got {v}"
        );
    }

    #[test]
    fn evaluate_linear() {
        let curve = CurveConfig {
            id: "lin".into(),
            name: "Linear".into(),
            curve_type: "linear".into(),
            sensor_id: "".into(),
            points: vec![],
            start_temp_c: Some(30.0),
            start_output_pct: Some(20.0),
            end_temp_c: Some(80.0),
            end_output_pct: Some(100.0),
            flat_output_pct: None,
            ..Default::default()
        };
        assert!((evaluate_curve(&curve, 55.0) - 60.0).abs() < 0.01);
    }

    #[test]
    fn evaluate_flat() {
        let curve = CurveConfig {
            id: "flat".into(),
            name: "Flat".into(),
            curve_type: "flat".into(),
            sensor_id: "".into(),
            points: vec![],
            flat_output_pct: Some(42.0),
            ..Default::default()
        };
        assert!((evaluate_curve(&curve, 50.0) - 42.0).abs() < 0.01);
    }

    #[test]
    fn evaluate_stepped_staircase() {
        // DEC-148: stepped holds the lower point's output until the next
        // point's temperature is reached (no ramp). Mirrors the GUI's
        // `_interpolate_stepped`; pinned by the DEC-126 parity harness.
        let curve = CurveConfig {
            id: "step".into(),
            name: "Stepped".into(),
            curve_type: "stepped".into(),
            sensor_id: "".into(),
            points: vec![
                CurvePoint {
                    temp_c: 30.0,
                    output_pct: 20.0,
                },
                CurvePoint {
                    temp_c: 60.0,
                    output_pct: 50.0,
                },
                CurvePoint {
                    temp_c: 80.0,
                    output_pct: 100.0,
                },
            ],
            ..Default::default()
        };
        // Below the first point clamps to the first output.
        assert!((evaluate_curve(&curve, 20.0) - 20.0).abs() < 0.01);
        // Exactly on a node returns that node's output.
        assert!((evaluate_curve(&curve, 30.0) - 20.0).abs() < 0.01);
        // Anywhere in [30,60) holds the lower point's output — no interpolation.
        assert!((evaluate_curve(&curve, 45.0) - 20.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 59.9) - 20.0).abs() < 0.01);
        // At the next node the step transitions up (half-open boundary).
        assert!((evaluate_curve(&curve, 60.0) - 50.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 79.9) - 50.0).abs() < 0.01);
        // At/above the last point clamps to the last output.
        assert!((evaluate_curve(&curve, 80.0) - 100.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 95.0) - 100.0).abs() < 0.01);
    }

    #[test]
    fn evaluate_stepped_empty_points_defaults_to_50() {
        let curve = CurveConfig {
            id: "step_empty".into(),
            name: "Empty".into(),
            curve_type: "stepped".into(),
            sensor_id: "".into(),
            points: vec![],
            ..Default::default()
        };
        assert!((evaluate_curve(&curve, 50.0) - 50.0).abs() < 0.01);
    }

    #[test]
    fn evaluate_trigger_stateless_coldstart() {
        // DEC-149: the pure `evaluate_curve` returns the cold-start value — load
        // at/above the load temp, else idle. The latching behaviour is applied
        // statefully by the profile engine (see its latch test + the parity
        // tuning_sequence), not here. Mirrors the GUI's `_interpolate_trigger`.
        let curve = CurveConfig {
            id: "trg".into(),
            name: "Trigger".into(),
            curve_type: "trigger".into(),
            sensor_id: "".into(),
            trigger_idle_temp_c: Some(40.0),
            trigger_load_temp_c: Some(60.0),
            trigger_idle_pct: Some(30.0),
            trigger_load_pct: Some(80.0),
            ..Default::default()
        };
        assert!((evaluate_curve(&curve, 35.0) - 30.0).abs() < 0.01); // below idle
        assert!((evaluate_curve(&curve, 50.0) - 30.0).abs() < 0.01); // in band, cold = idle
        assert!((evaluate_curve(&curve, 60.0) - 80.0).abs() < 0.01); // at load temp (inclusive)
        assert!((evaluate_curve(&curve, 70.0) - 80.0).abs() < 0.01); // above load
    }

    #[test]
    fn evaluate_trigger_stateless_uses_defaults_when_fields_missing() {
        let curve = CurveConfig {
            id: "trg".into(),
            name: "Trigger".into(),
            curve_type: "trigger".into(),
            ..Default::default()
        };
        // Defaults: idle 40°/30%, load 60°/80% — must match the GUI dataclass.
        assert!((evaluate_curve(&curve, 30.0) - 30.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 65.0) - 80.0).abs() < 0.01);
    }

    #[test]
    fn deserialize_mix_and_sync_curves() {
        // v7 (DEC-150/151): mix/sync curve fields round-trip through serde. The
        // pure `evaluate_curve` never drives them (they need an evaluation
        // context), so this pins the wire shape the profile engine's resolvers
        // consume.
        let json = r#"{
            "id": "p", "name": "P", "version": 7,
            "controls": [],
            "curves": [
                {"id": "mx", "name": "Mix", "type": "mix", "mix_function": "average", "mix_curve_ids": ["a", "b"]},
                {"id": "sy", "name": "Sync", "type": "sync", "sync_control_id": "ctrl", "sync_offset_pct": 12.5}
            ]
        }"#;
        let profile: DaemonProfile = serde_json::from_str(json).unwrap();
        let mx = &profile.curves[0];
        assert_eq!(mx.curve_type, "mix");
        assert_eq!(mx.mix_function.as_deref(), Some("average"));
        assert_eq!(mx.mix_curve_ids, vec!["a".to_string(), "b".to_string()]);
        let sy = &profile.curves[1];
        assert_eq!(sy.curve_type, "sync");
        assert_eq!(sy.sync_control_id, "ctrl");
        assert_eq!(sy.sync_offset_pct, Some(12.5));
    }

    #[test]
    fn mix_sync_fields_default_when_absent() {
        // A non-composite curve leaves the new fields at their serde defaults,
        // so older profiles deserialise unchanged.
        let curve = CurveConfig {
            curve_type: "flat".into(),
            ..Default::default()
        };
        assert!(curve.mix_function.is_none());
        assert!(curve.mix_curve_ids.is_empty());
        assert_eq!(curve.sync_control_id, "");
        assert!(curve.sync_offset_pct.is_none());
    }

    #[test]
    fn unknown_curve_type_returns_50() {
        let curve = CurveConfig {
            id: "unk".into(),
            name: "Unknown".into(),
            curve_type: "mystery".into(),
            sensor_id: "".into(),
            points: vec![],
            ..Default::default()
        };
        assert!((evaluate_curve(&curve, 50.0) - 50.0).abs() < 0.01);
    }

    #[test]
    fn empty_graph_returns_50() {
        let curve = CurveConfig {
            id: "empty".into(),
            name: "Empty".into(),
            curve_type: "graph".into(),
            sensor_id: "".into(),
            points: vec![],
            ..Default::default()
        };
        assert!((evaluate_curve(&curve, 50.0) - 50.0).abs() < 0.01);
    }

    #[test]
    fn load_profile_from_json_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test_profile.json");
        std::fs::write(
            &path,
            r#"{
            "id": "test",
            "name": "Test Profile",
            "version": 3,
            "controls": [],
            "curves": [
                {
                    "id": "c1",
                    "name": "Curve 1",
                    "type": "flat",
                    "sensor_id": "",
                    "points": [],
                    "flat_output_pct": 50.0
                }
            ]
        }"#,
        )
        .unwrap();

        let profile = load_profile(&path).unwrap();
        assert_eq!(profile.name, "Test Profile");
        assert_eq!(profile.id, "test");
        assert_eq!(profile.version, 3);
        assert_eq!(profile.curves.len(), 1);
    }

    #[test]
    fn load_profile_invalid_json_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.json");
        std::fs::write(&path, "not valid json").unwrap();

        let result = load_profile(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to parse"));
    }

    #[test]
    fn load_profile_missing_file_fails() {
        let path = std::path::PathBuf::from("/nonexistent/path/profile.json");
        let result = load_profile(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to read"));
    }

    #[test]
    fn load_profile_missing_optional_fields_uses_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("minimal.json");
        std::fs::write(&path, r#"{"id": "min", "name": "Minimal"}"#).unwrap();

        let profile = load_profile(&path).unwrap();
        assert_eq!(profile.name, "Minimal");
        assert!(profile.controls.is_empty());
        assert!(profile.curves.is_empty());
        assert_eq!(profile.version, 7); // default — v7 (DEC-150/151)
    }

    #[test]
    fn load_v3_profile_uses_fan_zero_rpm_default_false() {
        // A v3 profile predates the fan_zero_rpm field. Loading it must
        // succeed (forward-compatible) and the missing field must default
        // to false so the existing zero-RPM-disabled behaviour is preserved.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("v3.json");
        std::fs::write(
            &path,
            r#"{
                "id": "legacy",
                "name": "Legacy V3",
                "version": 3,
                "controls": [{
                    "id": "g",
                    "name": "GPU",
                    "mode": "manual",
                    "manual_output_pct": 60,
                    "members": [{
                        "source": "amd_gpu",
                        "member_id": "amd_gpu:0000:03:00.0",
                        "member_label": "9070XT"
                    }]
                }],
                "curves": []
            }"#,
        )
        .unwrap();

        let profile = load_profile(&path).unwrap();
        assert_eq!(profile.controls.len(), 1);
        assert_eq!(profile.controls[0].members.len(), 1);
        assert!(!profile.controls[0].members[0].fan_zero_rpm);
    }

    #[test]
    fn load_v4_profile_honours_fan_zero_rpm_true() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("v4.json");
        std::fs::write(
            &path,
            r#"{
                "id": "modern",
                "name": "Modern V4",
                "version": 4,
                "controls": [{
                    "id": "g",
                    "name": "GPU",
                    "mode": "manual",
                    "manual_output_pct": 60,
                    "members": [{
                        "source": "amd_gpu",
                        "member_id": "amd_gpu:0000:03:00.0",
                        "member_label": "9070XT",
                        "fan_zero_rpm": true
                    }]
                }],
                "curves": []
            }"#,
        )
        .unwrap();

        let profile = load_profile(&path).unwrap();
        assert!(profile.controls[0].members[0].fan_zero_rpm);
    }

    #[test]
    fn load_control_uses_serde_defaults() {
        // A control with only `id`+`name` must materialise the custom serde
        // defaults: mode="curve", manual=50%, step=100%. Pins default_mode /
        // default_manual / default_step (profile.rs), whose mutants otherwise
        // survive because no test exercises a control with these fields absent.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.json");
        std::fs::write(
            &path,
            r#"{"id":"p","name":"P","controls":[{"id":"c","name":"C"}]}"#,
        )
        .unwrap();

        let profile = load_profile(&path).unwrap();
        let c = &profile.controls[0];
        assert_eq!(c.mode, "curve"); // default_mode
        assert_eq!(c.manual_output_pct, 50.0); // default_manual
        assert_eq!(c.step_up_pct, 100.0); // default_step
        assert_eq!(c.step_down_pct, 100.0); // default_step
    }

    #[test]
    fn find_profile_returns_first_match() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        // Profile exists only in dir2
        std::fs::write(
            dir2.path().join("quiet.json"),
            r#"{"id": "quiet", "name": "Quiet"}"#,
        )
        .unwrap();

        let dirs = vec![dir1.path().to_path_buf(), dir2.path().to_path_buf()];
        let result = find_profile("quiet", &dirs);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), dir2.path().join("quiet.json"));
    }

    #[test]
    fn find_profile_prefers_first_directory() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        // Profile exists in both — dir1 should win
        std::fs::write(
            dir1.path().join("balanced.json"),
            r#"{"id": "bal1", "name": "First"}"#,
        )
        .unwrap();
        std::fs::write(
            dir2.path().join("balanced.json"),
            r#"{"id": "bal2", "name": "Second"}"#,
        )
        .unwrap();

        let dirs = vec![dir1.path().to_path_buf(), dir2.path().to_path_buf()];
        let result = find_profile("balanced", &dirs);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), dir1.path().join("balanced.json"));
    }

    #[test]
    fn find_profile_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        assert!(find_profile("nonexistent", &dirs).is_none());
    }

    #[test]
    fn find_profile_empty_dirs_returns_none() {
        let dirs: Vec<std::path::PathBuf> = vec![];
        assert!(find_profile("any", &dirs).is_none());
    }

    #[test]
    fn find_profile_rejects_path_traversal() {
        // Security regression test (CWE-22). Each rejected input has a REAL file
        // planted at the exact path `find_profile` would compute if its guard
        // clause were removed, so a bypass returns `Some(..)` and this test
        // FAILS. (The earlier version planted the decoy inside the search dir,
        // where no traversal input ever resolved — so the whole guard could be
        // deleted and the test still passed. See /test-tests audit P1.)
        //
        // Layout: <root>/secret.json is the escape target; <root>/profiles is
        // the search dir. Linux filename rules let us make 4 of the 5 clauses
        // discriminating; NUL cannot be a real filename, so it stays defensive.
        let root = tempfile::tempdir().unwrap();
        let profiles = root.path().join("profiles");
        std::fs::create_dir(&profiles).unwrap();
        let dirs = [profiles.clone()];

        // `..` clause: profiles/../secret.json == root/secret.json (exists).
        std::fs::write(root.path().join("secret.json"), "{}").unwrap();
        assert!(find_profile("../secret", &dirs).is_none());
        // `..` substring without a separator — pins the `..` clause on its own,
        // independently of the `/` clause that also rejects "../secret".
        std::fs::write(profiles.join("a..b.json"), "{}").unwrap();
        assert!(find_profile("a..b", &dirs).is_none());
        // `/` clause: profiles/sub/nested.json (exists).
        std::fs::create_dir(profiles.join("sub")).unwrap();
        std::fs::write(profiles.join("sub/nested.json"), "{}").unwrap();
        assert!(find_profile("sub/nested", &dirs).is_none());
        // `\` clause: on Linux a backslash is a literal filename character.
        std::fs::write(profiles.join("a\\b.json"), "{}").unwrap();
        assert!(find_profile("a\\b", &dirs).is_none());
        // empty clause: profiles/.json (exists).
        std::fs::write(profiles.join(".json"), "{}").unwrap();
        assert!(find_profile("", &dirs).is_none());
        // NUL clause: cannot be a real filename; defense-in-depth assertion.
        assert!(find_profile("foo\0bar", &dirs).is_none());

        // Positive control: a legitimate name IS found, so a mutant that makes
        // the guard reject everything is caught here too.
        std::fs::write(profiles.join("quiet.json"), "{}").unwrap();
        assert!(find_profile("quiet", &dirs).is_some());
    }

    #[test]
    fn is_safe_profile_id_accepts_normal_ids() {
        // The GUI emits 8-char hex ids; hand-authored/imported ids may use word
        // characters. None of these should be rejected by the DEC-173 limits.
        assert!(is_safe_profile_id("quiet"));
        assert!(is_safe_profile_id("a1b2c3d4")); // GUI uuid4()[:8] shape
        assert!(is_safe_profile_id("my-profile_2"));
        // Boundary: exactly MAX_PROFILE_ID_BYTES bytes is allowed.
        assert!(is_safe_profile_id(&"a".repeat(MAX_PROFILE_ID_BYTES)));
    }

    #[test]
    fn is_safe_profile_id_rejects_overlong() {
        // One byte over the cap is rejected — an over-long id would otherwise
        // surface as an opaque 500 ENAMETOOLONG once `{id}.json` is written.
        assert!(!is_safe_profile_id(&"a".repeat(MAX_PROFILE_ID_BYTES + 1)));
        // Bytes, not chars: 'é' is 2 UTF-8 bytes, so this id is under the cap by
        // char count but over it by byte count, and must still be rejected.
        let multibyte = "é".repeat(MAX_PROFILE_ID_BYTES / 2 + 1);
        assert!(multibyte.chars().count() <= MAX_PROFILE_ID_BYTES);
        assert!(multibyte.len() > MAX_PROFILE_ID_BYTES);
        assert!(!is_safe_profile_id(&multibyte));
    }

    #[test]
    fn is_safe_profile_id_rejects_control_chars() {
        // C0 controls, DEL, C1 controls, and NUL (subsumed from the old explicit
        // null-byte check) are all rejected — they corrupt log lines / error
        // envelopes and have no business in a filename stem.
        assert!(!is_safe_profile_id("a\tb")); // tab (C0)
        assert!(!is_safe_profile_id("a\nb")); // newline (C0)
        assert!(!is_safe_profile_id("a\u{07}b")); // BEL (C0)
        assert!(!is_safe_profile_id("a\u{7f}b")); // DEL
        assert!(!is_safe_profile_id("a\u{9f}b")); // C1 control
        assert!(!is_safe_profile_id("foo\0bar")); // NUL
    }

    // ───────────────────────── validate() (DEC-160) ─────────────────────────

    fn mk_profile(curves: Vec<CurveConfig>, controls: Vec<LogicalControl>) -> DaemonProfile {
        DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: String::new(),
            controls,
            curves,
        }
    }

    fn graph_curve(id: &str, sensor: &str) -> CurveConfig {
        CurveConfig {
            id: id.into(),
            name: id.into(),
            curve_type: "graph".into(),
            sensor_id: sensor.into(),
            points: vec![
                CurvePoint {
                    temp_c: 30.0,
                    output_pct: 20.0,
                },
                CurvePoint {
                    temp_c: 80.0,
                    output_pct: 100.0,
                },
            ],
            ..Default::default()
        }
    }

    fn curve_control(id: &str, curve_id: &str) -> LogicalControl {
        LogicalControl {
            id: id.into(),
            name: id.into(),
            mode: "curve".into(),
            curve_id: curve_id.into(),
            manual_output_pct: 50.0,
            members: vec![],
            step_up_pct: 100.0,
            step_down_pct: 100.0,
            offset_pct: 0.0,
            minimum_pct: 0.0,
            start_pct: 0.0,
            stop_pct: 0.0,
        }
    }

    fn sset(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn member(source: &str, id: &str, label: &str) -> ControlMember {
        ControlMember {
            source: source.into(),
            member_id: id.into(),
            member_label: label.into(),
            fan_zero_rpm: false,
        }
    }

    /// A manual control carrying `members` with the given floor (DEC-162 tests).
    fn control_with_members(min_pct: f64, members: Vec<ControlMember>) -> LogicalControl {
        LogicalControl {
            id: "ctl".into(),
            name: "ctl".into(),
            mode: "manual".into(),
            curve_id: String::new(),
            manual_output_pct: 50.0,
            members,
            step_up_pct: 100.0,
            step_down_pct: 100.0,
            offset_pct: 0.0,
            minimum_pct: min_pct,
            start_pct: 0.0,
            stop_pct: 0.0,
        }
    }

    // ───────────────────── DEC-162 role classification ──────────────────────

    #[test]
    fn classify_pump_by_label_hint() {
        for label in ["AIO_PUMP", "CPU_FAN", "Pump", "cpu_opt"] {
            let m = member("hwmon", "hwmon:nct6798:dev:pwm1:x", label);
            assert!(
                member_is_pump_or_cpu(&m),
                "{label} should classify pump/CPU"
            );
            assert!(!member_is_gpu(&m));
        }
    }

    #[test]
    fn classify_pump_by_aio_chip_even_with_bare_label() {
        // Kraken / Aquacomputer pump headers are often labelled only "pwm1"; the
        // chip embedded in the stable id is the schema-free signal (DEC-156).
        for chip in ["z53", "kraken2023", "d5next"] {
            let id = format!("hwmon:{chip}:nodev:pwm1:pwm1");
            let m = member("hwmon", &id, "pwm1");
            assert!(member_is_pump_or_cpu(&m), "{chip} pump should classify");
        }
    }

    #[test]
    fn classify_radiator_and_openfan_are_not_pump() {
        let rad = member("hwmon", "hwmon:it8696:dev:pwm2:CHA_FAN", "Radiator Top");
        assert!(!member_is_pump_or_cpu(&rad));
        let of = member("openfan", "openfan:ch00", "");
        assert!(!member_is_pump_or_cpu(&of));
    }

    #[test]
    fn classify_gpu_is_gpu_not_pump() {
        for src in ["amd_gpu", "intel_gpu", "nvidia_gpu"] {
            let m = member(src, "amd_gpu:0000:03:00.0", "9070XT Fan");
            assert!(member_is_gpu(&m));
            assert!(!member_is_pump_or_cpu(&m));
        }
    }

    #[test]
    fn classify_empty_label_non_cooler_is_not_pump() {
        let m = member("hwmon", "hwmon:nct6798:dev:pwm4:", "");
        assert!(!member_is_pump_or_cpu(&m));
    }

    // ────── DEC-252: the daemon's own label as an eval-time floor backstop ────

    #[test]
    fn renamed_pump_keeps_its_hard_floor_at_eval_time() {
        // THE case. `member_label` is written by the client, and the GUI resolves
        // it through a display-name tier list — so renaming a PUMP header to
        // "Radiator Top" dropped it from the 30% floor to 20%, silently. The
        // daemon held the real label in the member's own id the whole time.
        let renamed = member("hwmon", "hwmon:nct6798:0000:pwm3:PUMP", "Radiator Top");
        assert!(
            !member_is_pump_or_cpu(&renamed),
            "precondition: the author-declared label carries no hint"
        );
        assert!(
            member_needs_hard_floor(&renamed),
            "the daemon's own discovered label must still assert pump-ness"
        );
    }

    #[test]
    fn daemon_label_only_ever_adds_a_floor() {
        // Union, never replacement: a member the author declared as a pump stays
        // one even when the daemon's own label is the synthetic placeholder.
        let declared = member("hwmon", "hwmon:nct6798:0000:pwm3:pwm3", "AIO_PUMP");
        assert!(member_is_pump_or_cpu(&declared));
        assert!(member_needs_hard_floor(&declared));

        // And a chassis fan stays chassis on both signals.
        let chassis = member("hwmon", "hwmon:it8696:0000:pwm2:CHA_FAN", "Radiator Top");
        assert!(!member_needs_hard_floor(&chassis));
        let openfan = member("openfan", "openfan:ch00", "Front");
        assert!(!member_needs_hard_floor(&openfan));
    }

    #[test]
    fn synthetic_pwm_label_adds_nothing() {
        // The honest limit, pinned so nobody mistakes this for a pump detector:
        // `read_label` synthesises "pwm{N}" when the chip publishes no label file
        // and the daemon parses no /etc/sensors.d, so on such a board the
        // author's label remains the only signal.
        let m = member("hwmon", "hwmon:nct6798:0000:pwm7:pwm7", "Radiator Top");
        assert!(!member_needs_hard_floor(&m));
    }

    #[test]
    fn daemon_label_survives_colons_in_the_device_id_and_label() {
        // device_id is a PCI BDF and carries its own colons; a label may too.
        // Parsing by field index would take the wrong slice.
        assert_eq!(
            daemon_label_from_member_id("hwmon:k10temp:0000:00:18.3:pwm2:CPU_FAN"),
            Some("CPU_FAN")
        );
        assert_eq!(
            daemon_label_from_member_id("hwmon:nct6798:dev:pwm1:CPU:FAN"),
            Some("CPU:FAN")
        );
        assert_eq!(
            daemon_label_from_member_id("hwmon:nct6798:dev:pwm4:"),
            Some("")
        );
        // Malformed / non-hwmon ids yield no label rather than a wrong one.
        assert_eq!(daemon_label_from_member_id("openfan:ch00"), None);
        assert_eq!(daemon_label_from_member_id("garbage"), None);
    }

    #[test]
    fn validate_still_rejects_only_on_the_author_declared_label() {
        // [SAFETY] Version-skew guard. validate()'s FLOOR_TOO_LOW *rejects* a
        // profile, and the GUI bakes `minimum_pct` from its own classifier. If
        // the daemon rejected more than the paired GUI stamps, upgrading the
        // daemon first would block profile saving outright. The eval-time clamp
        // is strengthened; the rejection line does not move.
        let renamed = member("hwmon", "hwmon:nct6798:0000:pwm3:PUMP", "Radiator Top");
        let mut ctrl = control_with_members(20.0, vec![renamed]);
        ctrl.curve_id = "c".into();
        ctrl.mode = "curve".into();
        let profile = mk_profile(vec![graph_curve("c", "cpu")], vec![ctrl]);

        let report = validate(&profile, &sset(&["cpu"]));
        assert!(
            !report.errors.iter().any(|e| e.reason == "FLOOR_TOO_LOW"),
            "a profile an older GUI baked must still validate: {:?}",
            report.errors
        );
    }

    // ────────────────── DEC-162 validate() FLOOR_TOO_LOW backstop ────────────

    fn floor_violation(report: &ValidationReport) -> bool {
        report.errors.iter().any(|e| e.reason == "FLOOR_TOO_LOW")
    }

    #[test]
    fn validate_rejects_pump_control_below_floor() {
        for min in [0.0, 25.0, 29.0] {
            let pump = member("hwmon", "hwmon:z53:n:pwm1:pwm1", "AIO_PUMP");
            let report = validate(
                &mk_profile(vec![], vec![control_with_members(min, vec![pump])]),
                &sset(&[]),
            );
            assert!(!report.is_valid(), "min {min} must reject");
            assert!(floor_violation(&report), "min {min} must be FLOOR_TOO_LOW");
            assert!(report
                .errors
                .iter()
                .any(|e| e.field == "controls[0].minimum_pct"));
        }
    }

    #[test]
    fn validate_accepts_pump_control_at_or_above_floor() {
        for min in [30.0, 45.0, 100.0] {
            let pump = member("hwmon", "hwmon:z53:n:pwm1:pwm1", "AIO_PUMP");
            let report = validate(
                &mk_profile(vec![], vec![control_with_members(min, vec![pump])]),
                &sset(&[]),
            );
            assert!(
                !floor_violation(&report),
                "min {min} must NOT be FLOOR_TOO_LOW"
            );
        }
    }

    #[test]
    fn validate_gpu_only_control_at_zero_passes() {
        let gpu = member("amd_gpu", "amd_gpu:0000:03:00.0", "GPU Fan");
        let report = validate(
            &mk_profile(vec![], vec![control_with_members(0.0, vec![gpu])]),
            &sset(&[]),
        );
        assert!(
            !floor_violation(&report),
            "GPU at 0% must pass: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_chassis_control_at_twenty_passes() {
        // Chassis is GUI-baked advisory; the daemon hard backstop is pump/CPU only.
        let cha = member("hwmon", "hwmon:it8696:d:pwm2:CHA_FAN", "Radiator Top");
        let report = validate(
            &mk_profile(vec![], vec![control_with_members(20.0, vec![cha])]),
            &sset(&[]),
        );
        assert!(!floor_violation(&report), "chassis at 20% must pass");
    }

    #[test]
    fn validate_mixed_pump_below_floor_rejects_once() {
        // A pump + chassis in one control still trips on the pump; one error.
        let members = vec![
            member("hwmon", "hwmon:it8696:d:pwm2:CHA_FAN", "Radiator Top"),
            member("hwmon", "hwmon:z53:n:pwm1:pwm1", "AIO_PUMP"),
        ];
        let report = validate(
            &mk_profile(vec![], vec![control_with_members(10.0, members)]),
            &sset(&[]),
        );
        let n = report
            .errors
            .iter()
            .filter(|e| e.reason == "FLOOR_TOO_LOW")
            .count();
        assert_eq!(n, 1, "exactly one FLOOR_TOO_LOW per control");
    }

    // ─────────────── DEC-167 validate() PUMP_STOP_FORBIDDEN backstop ──────────

    fn pump_stop_violation(report: &ValidationReport) -> bool {
        report
            .errors
            .iter()
            .any(|e| e.reason == "PUMP_STOP_FORBIDDEN")
    }

    #[test]
    fn validate_rejects_pump_control_with_nonzero_stop_pct() {
        // DEC-167: a pump must never be configured to stop. A pump control with a
        // valid floor (30) but stop_pct=35 would let the engine snap it to 0 below
        // the threshold — reject at author time.
        let pump = member("hwmon", "hwmon:z53:n:pwm1:pwm1", "AIO_PUMP");
        let mut ctl = control_with_members(30.0, vec![pump]);
        ctl.stop_pct = 35.0;
        let report = validate(&mk_profile(vec![], vec![ctl]), &sset(&[]));
        assert!(
            !report.is_valid(),
            "pump with non-zero stop_pct must reject"
        );
        assert!(pump_stop_violation(&report), "must be PUMP_STOP_FORBIDDEN");
        assert!(report
            .errors
            .iter()
            .any(|e| e.field == "controls[0].stop_pct"));
    }

    #[test]
    fn validate_accepts_pump_control_with_zero_stop_pct() {
        // The common, correct shape: a pump with stop_pct=0 (stop disabled).
        let pump = member("hwmon", "hwmon:z53:n:pwm1:pwm1", "AIO_PUMP");
        let ctl = control_with_members(30.0, vec![pump]); // stop_pct defaults to 0
        let report = validate(&mk_profile(vec![], vec![ctl]), &sset(&[]));
        assert!(
            !pump_stop_violation(&report),
            "pump with stop_pct=0 must NOT trip PUMP_STOP_FORBIDDEN: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_accepts_nonzero_stop_pct_on_nonpump_control() {
        // A chassis/openfan control keeps the legitimate stop-to-0 feature.
        let cha = member("hwmon", "hwmon:it8696:d:pwm2:CHA_FAN", "Radiator Top");
        let mut ctl = control_with_members(20.0, vec![cha]);
        ctl.stop_pct = 20.0;
        let report = validate(&mk_profile(vec![], vec![ctl]), &sset(&[]));
        assert!(
            !pump_stop_violation(&report),
            "non-pump stop_pct must be allowed: {:?}",
            report.errors
        );
    }

    #[test]
    fn role_classification_matches_oracle() {
        // DEC-162 cross-stack agreement: the daemon classifiers must produce the
        // same role as the GUI's `infer_member_role` for every shared vector.
        //
        // DEC-257: pinned against `member_needs_hard_floor` — the EVAL-TIME union
        // (DEC-252) — rather than the narrower `member_is_pump_or_cpu`, because
        // that is the classifier deciding the runtime floor and the stop-snap
        // exemption, and it is what the GUI now mirrors. `validate()`'s rejection
        // deliberately stays on the narrow one, so the GUI can only ever be
        // stricter than what the daemon accepts — never the reverse. The GUI half lives in tests/test_role_classification_parity.py
        // against a byte-identical copy of this fixture.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/role_classification.json"
        );
        let text =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read role fixture: {e}"));
        let vectors: serde_json::Value = serde_json::from_str(&text).expect("parse role fixture");
        for case in vectors["cases"].as_array().unwrap() {
            let m: ControlMember =
                serde_json::from_value(case.clone()).expect("deserialize member");
            let got = if member_is_gpu(&m) {
                "gpu"
            } else if member_needs_hard_floor(&m) {
                "cpu_or_pump"
            } else {
                "chassis"
            };
            let expected = case["role"].as_str().unwrap();
            assert_eq!(got, expected, "role[{}]", case["name"]);
        }
    }

    #[test]
    fn validate_valid_profile_passes_clean() {
        let profile = mk_profile(
            vec![graph_curve("c", "cpu")],
            vec![curve_control("ctl", "c")],
        );
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(report.is_valid(), "unexpected errors: {:?}", report.errors);
        assert!(
            report.warnings.is_empty(),
            "unexpected warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn validate_unknown_sensor_is_warning_not_error() {
        // Portability: a profile authored on another machine references a sensor
        // absent here. Must NOT be rejected — the engine keeps cooling either
        // way (a lone curve holds, a Mix runs on its other inputs, DEC-272).
        let profile = mk_profile(
            vec![graph_curve("c", "ghost_sensor")],
            vec![curve_control("ctl", "c")],
        );
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(
            report.is_valid(),
            "unknown sensor must not block: {:?}",
            report.errors
        );
        let warning = report
            .warnings
            .iter()
            .find(|w| w.reason == "UNKNOWN_SENSOR")
            .expect("an absent sensor must warn");

        // 273-h. The text is user-facing and was untrue after DEC-272: it
        // promised "the control will hold a safe fallback until it appears",
        // which describes neither case the engine actually implements. Pinned
        // because a warning nobody can act on correctly is worse than none —
        // and because the wording drifted from the behaviour once already.
        assert!(
            !warning.description.contains("safe fallback"),
            "the retired 'safe fallback' promise is back: {}",
            warning.description
        );
        assert!(
            warning.description.contains("hold their last speed"),
            "the warning must say what a lone curve does — hold, not fall back: {}",
            warning.description
        );
        assert!(
            warning.description.contains("Mix"),
            "the warning must say a Mix keeps running on its surviving inputs: {}",
            warning.description
        );
    }

    #[test]
    fn validate_out_of_range_pct_is_error() {
        let mut curve = graph_curve("c", "cpu");
        curve.points[1].output_pct = 140.0;
        let profile = mk_profile(vec![curve], vec![curve_control("ctl", "c")]);
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(!report.is_valid());
        assert!(report.errors.iter().any(|e| e.reason == "OUT_OF_RANGE"));
    }

    #[test]
    fn validate_nonfinite_is_error() {
        let mut curve = graph_curve("c", "cpu");
        curve.points[0].temp_c = f64::NAN;
        let profile = mk_profile(vec![curve], vec![curve_control("ctl", "c")]);
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(report.errors.iter().any(|e| e.reason == "NON_FINITE"));
    }

    #[test]
    fn validate_too_many_points_is_error() {
        let mut curve = graph_curve("c", "cpu");
        curve.points = (0..=MAX_CURVE_POINTS)
            .map(|i| CurvePoint {
                temp_c: i as f64,
                output_pct: 50.0,
            })
            .collect(); // MAX_CURVE_POINTS + 1 points
        let profile = mk_profile(vec![curve], vec![curve_control("ctl", "c")]);
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(report.errors.iter().any(|e| e.reason == "TOO_MANY_POINTS"));
    }

    // ────── Recursion bounds: MAX_PROFILE_CURVES / MAX_PROFILE_CONTROLS ──────
    // A deep but ACYCLIC Mix chain used to recurse once per curve and abort the
    // process with a stack overflow on the next engine tick. Cycle detection does
    // not help — these chains are legal DAGs. Guarded at three layers; these tests
    // pin all three.

    #[test]
    fn validate_too_many_curves_is_error() {
        let curves: Vec<CurveConfig> = (0..=MAX_PROFILE_CURVES)
            .map(|i| graph_curve(&format!("c{i}"), "cpu"))
            .collect(); // MAX_PROFILE_CURVES + 1
        let report = validate(&mk_profile(curves, vec![]), &sset(&["cpu"]));
        assert!(report.errors.iter().any(|e| e.reason == "TOO_MANY_CURVES"));
    }

    #[test]
    fn validate_exactly_max_curves_is_allowed() {
        // Boundary: the cap must not reject a profile sitting exactly on it.
        let curves: Vec<CurveConfig> = (0..MAX_PROFILE_CURVES)
            .map(|i| graph_curve(&format!("c{i}"), "cpu"))
            .collect();
        let report = validate(&mk_profile(curves, vec![]), &sset(&["cpu"]));
        assert!(!report.errors.iter().any(|e| e.reason == "TOO_MANY_CURVES"));
    }

    #[test]
    fn validate_too_many_controls_is_error() {
        let controls: Vec<LogicalControl> = (0..=MAX_PROFILE_CONTROLS)
            .map(|i| curve_control(&format!("ctl{i}"), "c"))
            .collect(); // MAX_PROFILE_CONTROLS + 1
        let profile = mk_profile(vec![graph_curve("c", "cpu")], controls);
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(report
            .errors
            .iter()
            .any(|e| e.reason == "TOO_MANY_CONTROLS"));
    }

    #[test]
    fn load_profile_rejects_oversized_profile() {
        // The load-time net. The boot paths (CLI --profile, persisted-state
        // restore) skip validate(), so an oversized profile already on disk must
        // be refused HERE or it aborts the daemon at startup — a crash loop that
        // survives reboot. Callers treat Err as "no profile" (imperative mode).
        let curves: Vec<CurveConfig> = (0..=MAX_PROFILE_CURVES)
            .map(|i| graph_curve(&format!("c{i}"), "cpu"))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.json");
        std::fs::write(
            &path,
            serde_json::to_string(&mk_profile(curves, vec![])).unwrap(),
        )
        .unwrap();

        let err = load_profile(&path).unwrap_err();
        assert!(
            err.contains("exceeding the maximum"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_profile_accepts_profile_at_the_cap() {
        let curves: Vec<CurveConfig> = (0..MAX_PROFILE_CURVES)
            .map(|i| graph_curve(&format!("c{i}"), "cpu"))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atcap.json");
        std::fs::write(
            &path,
            serde_json::to_string(&mk_profile(curves, vec![])).unwrap(),
        )
        .unwrap();

        let loaded = load_profile(&path).expect("a profile exactly at the cap must load");
        assert_eq!(loaded.curves.len(), MAX_PROFILE_CURVES);
    }

    // ── DEC-249: load-time numeric net ──────────────────────────────────────

    #[test]
    fn load_profile_rejects_negative_step_rates() {
        // The exact input that killed the engine. `validate()` bounds these
        // 0..=100, but the boot paths skip it, so this profile used to load
        // cleanly and then panic `apply_tuning` on the engine's second tick —
        // taking the sole PWM writer and the thermal leg with it.
        let mut ctrl = curve_control("c1", "cv");
        ctrl.step_up_pct = -50.0;
        ctrl.step_down_pct = -50.0;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("negsteps.json");
        std::fs::write(
            &path,
            serde_json::to_string(&mk_profile(vec![graph_curve("cv", "cpu")], vec![ctrl])).unwrap(),
        )
        .unwrap();

        let err = load_profile(&path).unwrap_err();
        assert!(
            err.contains("step_up_pct") && err.contains("between 0 and 100"),
            "error must name the offending field: {err}"
        );
    }

    #[test]
    fn load_profile_accepts_the_full_valid_range() {
        // The net must not reject anything the API accepts. Every field sits on
        // a boundary `check_pct` / `check_offset` allow.
        let mut ctrl = curve_control("c1", "cv");
        ctrl.step_up_pct = 0.0;
        ctrl.step_down_pct = 100.0;
        ctrl.minimum_pct = 100.0;
        ctrl.start_pct = 0.0;
        ctrl.stop_pct = 100.0;
        ctrl.manual_output_pct = 0.0;
        ctrl.offset_pct = -100.0;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundaries.json");
        std::fs::write(
            &path,
            serde_json::to_string(&mk_profile(vec![graph_curve("cv", "cpu")], vec![ctrl])).unwrap(),
        )
        .unwrap();

        assert!(load_profile(&path).is_ok());
    }

    #[test]
    fn numeric_net_rejects_non_finite_values() {
        // `f64::clamp` panics on a NaN bound as well as an inverted one, so the
        // net mirrors `check_finite` too. Exercised directly: JSON has no NaN
        // literal, so this guards the in-memory shape rather than a parse.
        let mut ctrl = curve_control("c1", "cv");
        ctrl.step_up_pct = f64::NAN;
        assert!(check_numeric_ranges(&mk_profile(vec![], vec![ctrl.clone()])).is_err());

        ctrl.step_up_pct = 100.0;
        ctrl.offset_pct = f64::INFINITY;
        assert!(check_numeric_ranges(&mk_profile(vec![], vec![ctrl])).is_err());

        let mut curve = graph_curve("cv", "cpu");
        curve.points[0].temp_c = f64::NAN;
        assert!(check_numeric_ranges(&mk_profile(vec![curve], vec![])).is_err());
    }

    #[test]
    fn numeric_net_covers_optional_curve_scalars() {
        // The optional Stepped/Trigger/Sync scalars are bounded by `validate()`
        // too, so the load-time net must reach them — not only `points`.
        let mut curve = graph_curve("cv", "cpu");
        curve.flat_output_pct = Some(150.0);
        assert!(check_numeric_ranges(&mk_profile(vec![curve], vec![])).is_err());

        let mut curve = graph_curve("cv2", "cpu");
        curve.sync_offset_pct = Some(-250.0);
        assert!(check_numeric_ranges(&mk_profile(vec![curve], vec![])).is_err());
    }

    #[test]
    fn load_profile_rejects_too_many_controls() {
        // The CONTROLS half of the load-time net. Distinct from the curves case
        // above and separately load-bearing: controls chain through Sync, which
        // recurses in `curve_eval::topo_visit`, and the boot paths skip
        // validate() — so without this guard an oversized-by-CONTROL-count
        // profile on disk still reaches the engine and aborts it at startup.
        let controls: Vec<LogicalControl> = (0..=MAX_PROFILE_CONTROLS)
            .map(|i| curve_control(&format!("ctl{i}"), "c"))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many_controls.json");
        std::fs::write(
            &path,
            serde_json::to_string(&mk_profile(vec![graph_curve("c", "cpu")], controls)).unwrap(),
        )
        .unwrap();

        let err = load_profile(&path).unwrap_err();
        assert!(
            err.contains("controls") && err.contains("exceeding the maximum"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_profile_accepts_controls_at_the_cap() {
        let controls: Vec<LogicalControl> = (0..MAX_PROFILE_CONTROLS)
            .map(|i| curve_control(&format!("ctl{i}"), "c"))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("controls_atcap.json");
        std::fs::write(
            &path,
            serde_json::to_string(&mk_profile(vec![graph_curve("c", "cpu")], controls)).unwrap(),
        )
        .unwrap();

        let loaded = load_profile(&path).expect("controls exactly at the cap must load");
        assert_eq!(loaded.controls.len(), MAX_PROFILE_CONTROLS);
    }

    #[test]
    fn validate_exactly_max_controls_is_allowed() {
        // Boundary twin of validate_exactly_max_curves_is_allowed: a `>` that
        // slipped to `>=` would reject a profile sitting exactly on the cap.
        let controls: Vec<LogicalControl> = (0..MAX_PROFILE_CONTROLS)
            .map(|i| curve_control(&format!("ctl{i}"), "c"))
            .collect();
        let profile = mk_profile(vec![graph_curve("c", "cpu")], controls);
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(!report
            .errors
            .iter()
            .any(|e| e.reason == "TOO_MANY_CONTROLS"));
    }

    #[test]
    fn validate_trigger_idle_ge_load_is_error() {
        let curve = CurveConfig {
            id: "t".into(),
            name: "T".into(),
            curve_type: "trigger".into(),
            sensor_id: "cpu".into(),
            trigger_idle_temp_c: Some(60.0),
            trigger_load_temp_c: Some(50.0),
            trigger_idle_pct: Some(30.0),
            trigger_load_pct: Some(80.0),
            ..Default::default()
        };
        let profile = mk_profile(vec![curve], vec![curve_control("ctl", "t")]);
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(report
            .errors
            .iter()
            .any(|e| e.reason == "TRIGGER_IDLE_GE_LOAD"));
    }

    #[test]
    fn validate_dangling_curve_ref_is_error() {
        let profile = mk_profile(vec![], vec![curve_control("ctl", "missing")]);
        let report = validate(&profile, &sset(&[]));
        assert!(report
            .errors
            .iter()
            .any(|e| e.reason == "UNKNOWN_CURVE_REF"));
    }

    #[test]
    fn validate_mix_cycle_is_error() {
        let a = CurveConfig {
            id: "a".into(),
            name: "A".into(),
            curve_type: "mix".into(),
            mix_curve_ids: vec!["b".into()],
            ..Default::default()
        };
        let b = CurveConfig {
            id: "b".into(),
            name: "B".into(),
            curve_type: "mix".into(),
            mix_curve_ids: vec!["a".into()],
            ..Default::default()
        };
        let report = validate(&mk_profile(vec![a, b], vec![]), &sset(&[]));
        assert!(report.errors.iter().any(|e| e.reason == "MIX_CYCLE"));
    }

    #[test]
    fn validate_sync_cycle_is_error() {
        // c1 mirrors c2 (via sync curve s1), c2 mirrors c1 (via s2) → cycle.
        let s1 = CurveConfig {
            id: "s1".into(),
            name: "S1".into(),
            curve_type: "sync".into(),
            sync_control_id: "c2".into(),
            ..Default::default()
        };
        let s2 = CurveConfig {
            id: "s2".into(),
            name: "S2".into(),
            curve_type: "sync".into(),
            sync_control_id: "c1".into(),
            ..Default::default()
        };
        let report = validate(
            &mk_profile(
                vec![s1, s2],
                vec![curve_control("c1", "s1"), curve_control("c2", "s2")],
            ),
            &sset(&[]),
        );
        assert!(report.errors.iter().any(|e| e.reason == "SYNC_CYCLE"));
    }

    // ── validate() branch coverage (Phase 7 mutation hardening) ──────────────
    // These pin the Mix/Sync/trigger/offset/source/max-points validation
    // branches that cargo-mutants flagged as untested in `validate`.

    #[test]
    fn validate_exactly_max_points_passes() {
        // Boundary: `points.len() > MAX` (not `>=`) — exactly MAX points is OK.
        let mut curve = graph_curve("c", "cpu");
        curve.points = (0..MAX_CURVE_POINTS)
            .map(|i| CurvePoint {
                temp_c: i as f64,
                output_pct: 50.0,
            })
            .collect(); // EXACTLY MAX_CURVE_POINTS
        let report = validate(&mk_profile(vec![curve], vec![]), &sset(&["cpu"]));
        assert!(
            !report.errors.iter().any(|e| e.reason == "TOO_MANY_POINTS"),
            "exactly MAX_CURVE_POINTS must be accepted: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_valid_trigger_idle_below_load_passes() {
        // A well-formed trigger (idle < load, both finite) must NOT error —
        // pins the `&&` guards in the idle>=load check.
        let curve = CurveConfig {
            id: "t".into(),
            name: "T".into(),
            curve_type: "trigger".into(),
            sensor_id: "cpu".into(),
            trigger_idle_temp_c: Some(40.0),
            trigger_load_temp_c: Some(60.0),
            trigger_idle_pct: Some(30.0),
            trigger_load_pct: Some(80.0),
            ..Default::default()
        };
        let report = validate(&mk_profile(vec![curve], vec![]), &sset(&["cpu"]));
        assert!(
            !report
                .errors
                .iter()
                .any(|e| e.reason == "TRIGGER_IDLE_GE_LOAD"),
            "idle below load must be valid: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_mix_unknown_function_warns() {
        let mix = CurveConfig {
            id: "m".into(),
            name: "M".into(),
            curve_type: "mix".into(),
            mix_function: Some("definitely-not-a-function".into()),
            mix_curve_ids: vec!["g".into()], // valid ref — only the function is bad
            ..Default::default()
        };
        let report = validate(
            &mk_profile(vec![graph_curve("g", "cpu"), mix], vec![]),
            &sset(&["cpu"]),
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.reason == "UNKNOWN_MIX_FUNCTION"),
            "unknown mix function must warn: {:?}",
            report.warnings
        );
    }

    #[test]
    fn validate_mix_dangling_ref_is_error() {
        let mix = CurveConfig {
            id: "m".into(),
            name: "M".into(),
            curve_type: "mix".into(),
            mix_curve_ids: vec!["nonexistent".into()],
            ..Default::default()
        };
        let report = validate(&mk_profile(vec![mix], vec![]), &sset(&[]));
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.reason == "UNKNOWN_CURVE_REF"),
            "a mix referencing a missing curve must error: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_sync_dangling_ref_is_error() {
        // Non-empty sync_control_id that does not exist → UNKNOWN_CONTROL_REF.
        // (Also pins the `||`/`!` in the sync reference check.)
        let sync = CurveConfig {
            id: "s".into(),
            name: "S".into(),
            curve_type: "sync".into(),
            sync_control_id: "missing".into(),
            ..Default::default()
        };
        let report = validate(&mk_profile(vec![sync], vec![]), &sset(&[]));
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.reason == "UNKNOWN_CONTROL_REF"),
            "a sync referencing a missing control must error: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_unknown_member_source_warns() {
        let bogus = member("martian", "martian:0", "");
        let report = validate(
            &mk_profile(vec![], vec![control_with_members(0.0, vec![bogus])]),
            &sset(&[]),
        );
        assert!(
            report.warnings.iter().any(|w| w.reason == "UNKNOWN_SOURCE"),
            "an unknown member source must warn: {:?}",
            report.warnings
        );
    }

    #[test]
    fn validate_nvidia_gpu_member_source_is_known() {
        // nvidia_gpu is a recognised (read-only) GPU source like intel_gpu — a
        // profile referencing it must NOT raise UNKNOWN_SOURCE (DEC-204).
        let m = member("nvidia_gpu", "nvidia_gpu:0000:03:00.0", "");
        let report = validate(
            &mk_profile(vec![], vec![control_with_members(0.0, vec![m])]),
            &sset(&[]),
        );
        assert!(
            !report.warnings.iter().any(|w| w.reason == "UNKNOWN_SOURCE"),
            "nvidia_gpu must be a known member source: {:?}",
            report.warnings
        );
    }

    #[test]
    fn validate_out_of_range_offset_is_error() {
        // check_offset must flag an offset outside ±100 — a no-op check_offset
        // (the mutant) would let a wild offset through unvalidated.
        let mut ctl = curve_control("ctl", "c");
        ctl.offset_pct = 150.0;
        let report = validate(
            &mk_profile(vec![graph_curve("c", "cpu")], vec![ctl]),
            &sset(&["cpu"]),
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.reason == "OUT_OF_RANGE" && e.field.ends_with("offset_pct")),
            "offset_pct 150 must be OUT_OF_RANGE: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_collects_all_errors_no_fail_fast() {
        let mut curve = graph_curve("c", "cpu");
        curve.points[0].output_pct = 200.0; // error 1: OUT_OF_RANGE
        let profile = mk_profile(vec![curve], vec![curve_control("ctl", "missing")]); // error 2
        let report = validate(&profile, &sset(&["cpu"]));
        assert!(
            report.errors.len() >= 2,
            "expected all errors collected, got {:?}",
            report.errors
        );
    }

    #[test]
    fn severity_serializes_lowercase() {
        let v = FieldViolation {
            field: "x".into(),
            reason: "R".into(),
            description: "d".into(),
            severity: Severity::Warning,
        };
        assert_eq!(serde_json::to_value(&v).unwrap()["severity"], "warning");
    }

    #[test]
    fn field_violations_json_has_array() {
        let mut curve = graph_curve("c", "cpu");
        curve.points[0].output_pct = 200.0;
        let report = validate(&mk_profile(vec![curve], vec![]), &sset(&["cpu"]));
        let details = report.field_violations_json();
        assert!(details["field_violations"].is_array());
        assert!(!details["field_violations"].as_array().unwrap().is_empty());
    }
}
