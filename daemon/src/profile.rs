//! Profile model — loads and evaluates GUI-created fan curve profiles.
//!
//! Compatible with the GUI's Profile v3 JSON format. Supports graph (piecewise
//! linear), linear (2-point), and flat curve types.

use serde::{Deserialize, Serialize};
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
    3
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
}

/// A fan curve configuration (graph, linear, or flat).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        "linear" => evaluate_linear(curve, temp_c),
        "flat" => curve.flat_output_pct.unwrap_or(50.0),
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
    50.0
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

/// Load a profile from a JSON file.
pub fn load_profile(path: &Path) -> Result<DaemonProfile, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read profile '{}': {e}", path.display()))?;
    let profile: DaemonProfile = serde_json::from_str(&content)
        .map_err(|e| format!("failed to parse profile '{}': {e}", path.display()))?;
    if profile.version < 3 {
        log::warn!(
            "Profile '{}' has version {}, expected 3+",
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

/// Search for a profile by name in the given search directories.
///
/// The name must be a simple filename stem (no path separators or traversal
/// components). Names containing `/`, `\`, `..`, or null bytes are rejected
/// to prevent CWE-22 path traversal.
pub fn find_profile(name: &str, search_dirs: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    if name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
        || name.is_empty()
    {
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
            start_temp_c: None,
            start_output_pct: None,
            end_temp_c: None,
            end_output_pct: None,
            flat_output_pct: None,
        };
        assert!((evaluate_curve(&curve, 30.0) - 20.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 45.0) - 35.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 80.0) - 100.0).abs() < 0.01);
        assert!((evaluate_curve(&curve, 20.0) - 20.0).abs() < 0.01); // below range
        assert!((evaluate_curve(&curve, 90.0) - 100.0).abs() < 0.01); // above range
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
            start_temp_c: None,
            start_output_pct: None,
            end_temp_c: None,
            end_output_pct: None,
            flat_output_pct: Some(42.0),
        };
        assert!((evaluate_curve(&curve, 50.0) - 42.0).abs() < 0.01);
    }

    #[test]
    fn unknown_curve_type_returns_50() {
        let curve = CurveConfig {
            id: "unk".into(),
            name: "Unknown".into(),
            curve_type: "mystery".into(),
            sensor_id: "".into(),
            points: vec![],
            start_temp_c: None,
            start_output_pct: None,
            end_temp_c: None,
            end_output_pct: None,
            flat_output_pct: None,
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
            start_temp_c: None,
            start_output_pct: None,
            end_temp_c: None,
            end_output_pct: None,
            flat_output_pct: None,
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
        assert_eq!(profile.version, 3); // default
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
        let dir = tempfile::tempdir().unwrap();
        // Create a file that would match if traversal were allowed
        let target = dir.path().join("evil.json");
        std::fs::write(&target, r#"{"id":"evil","name":"Evil"}"#).unwrap();

        // These should all be rejected before the filesystem is consulted
        assert!(find_profile("../evil", &[dir.path().to_path_buf()]).is_none());
        assert!(find_profile("../../evil", &[dir.path().to_path_buf()]).is_none());
        assert!(find_profile("foo/bar", &[dir.path().to_path_buf()]).is_none());
        assert!(find_profile("foo\\bar", &[dir.path().to_path_buf()]).is_none());
        assert!(find_profile("foo\0bar", &[dir.path().to_path_buf()]).is_none());
        assert!(find_profile("", &[dir.path().to_path_buf()]).is_none());
    }
}
