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

/// Stateless (cold-start) trigger value: the load speed at/above the load
/// temperature, else the idle speed. The latching hysteresis — holding the load
/// state down through the idle..load band — is applied per-control by the
/// profile engine, NOT here, so `evaluate_curve` stays a pure function for the
/// `curve_eval` parity tier. Must match the GUI's `_interpolate_trigger`
/// (DEC-126 / DEC-149).
fn evaluate_trigger_stateless(curve: &CurveConfig, temp_c: f64) -> f64 {
    let load_temp = curve.trigger_load_temp_c.unwrap_or(60.0);
    let load_pct = curve.trigger_load_pct.unwrap_or(80.0);
    let idle_pct = curve.trigger_idle_pct.unwrap_or(30.0);
    if temp_c >= load_temp {
        load_pct
    } else {
        idle_pct
    }
}

/// Load a profile from a JSON file.
pub fn load_profile(path: &Path) -> Result<DaemonProfile, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read profile '{}': {e}", path.display()))?;
    let profile: DaemonProfile = serde_json::from_str(&content)
        .map_err(|e| format!("failed to parse profile '{}': {e}", path.display()))?;
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

/// Whether a profile id is safe to use as a filename stem (`{id}.json`).
///
/// Rejects empty ids and any containing `/`, `\`, `..`, or a null byte, to
/// prevent CWE-22 path traversal. The single source of the id-safety rule for
/// both `find_profile` (activation) and `profile_store` (CRUD writes).
pub fn is_safe_profile_id(id: &str) -> bool {
    !(id.is_empty()
        || id.contains('/')
        || id.contains('\\')
        || id.contains("..")
        || id.contains('\0'))
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

const KNOWN_CURVE_TYPES: [&str; 7] = [
    "graph", "stepped", "linear", "flat", "trigger", "mix", "sync",
];
const KNOWN_MIX_FUNCTIONS: [&str; 5] = ["max", "min", "average", "sum", "subtract"];
const KNOWN_MEMBER_SOURCES: [&str; 4] = ["openfan", "hwmon", "amd_gpu", "intel_gpu"];

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

/// True when a control member is a GPU fan (`amd_gpu` or `intel_gpu`). GPU fans
/// carry no daemon floor — PMFW enforces its own OD_RANGE minimum (DEC-119).
/// Mirrors the GUI's `infer_member_role` GPU branch.
pub(crate) fn member_is_gpu(member: &ControlMember) -> bool {
    member.source == "amd_gpu" || member.source == "intel_gpu"
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

/// Severity of a [`FieldViolation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Hard error — the profile is rejected (not stored / not activated).
    Error,
    /// Soft warning — the profile is accepted, but the condition is surfaced
    /// (e.g. it references a sensor not present on this machine, so a control
    /// will sit at its safe fallback until the sensor appears).
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
///   on *this* host. The engine already tolerates a missing sensor at eval time
///   (the control holds a safe fallback, and the 105 °C thermal force
///   backstops), so a profile authored on another machine must still store,
///   validate, and import.
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
                    "sensor '{}' is not present on this machine; the control will hold a \
                     safe fallback until it appears",
                    curve.sensor_id
                ),
            );
        }

        match curve.curve_type.as_str() {
            "trigger" => {
                let idle = curve.trigger_idle_temp_c.unwrap_or(40.0);
                let load = curve.trigger_load_temp_c.unwrap_or(60.0);
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
                if curve.sync_control_id.is_empty()
                    || !control_ids.contains(curve.sync_control_id.as_str())
                {
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
        for src in ["amd_gpu", "intel_gpu"] {
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

    #[test]
    fn role_classification_matches_oracle() {
        // DEC-162 cross-stack agreement: the daemon classifiers must produce the
        // same role as the GUI's `infer_member_role` for every shared vector, or
        // a GUI-baked profile could be wrongly rejected by the FLOOR_TOO_LOW
        // backstop. The GUI half lives in tests/test_role_classification_parity.py
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
            } else if member_is_pump_or_cpu(&m) {
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
        // absent here. Must NOT be rejected — the engine holds a safe fallback.
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
        assert!(report.warnings.iter().any(|w| w.reason == "UNKNOWN_SENSOR"));
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
