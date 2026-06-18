//! Request handlers for the IPC API.
//!
//! Read handlers read from the `StateCache` — no direct hardware access.
//! Write handlers dispatch through the `FanController`.

mod config;
mod control;
mod gpu;
mod hw_diagnostics;
mod hwmon_ctl;
mod openfan;
mod profile;
mod status;

pub use config::*;
pub use control::*;
pub use gpu::*;
pub use hw_diagnostics::*;
pub use hwmon_ctl::*;
pub use openfan::*;
pub use profile::*;
pub use status::*;

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::Json;

use crate::constants;
use crate::health::cache::StateCache;
use crate::health::staleness::StalenessConfig;
use crate::hwmon::pwm_control::HwmonPwmController;
use crate::serial::controller::FanController;

use super::responses::*;
use crate::health::state::DaemonState;

/// Build the sorted list of sensor entries from a cache snapshot.
pub(crate) fn build_sensor_entries(snap: &DaemonState, now: Instant) -> Vec<SensorEntry> {
    let mut entries: Vec<SensorEntry> = snap
        .sensors
        .values()
        .map(|s| {
            let age_ms = now.duration_since(s.updated_at).as_millis() as u64;
            SensorEntry {
                id: s.id.clone(),
                kind: s.kind.to_string(),
                label: s.label.clone(),
                value_c: s.value_c,
                source: s.source.to_string(),
                age_ms,
                rate_c_per_s: s.rate_c_per_s,
                session_min_c: s.session_min_c,
                session_max_c: s.session_max_c,
                chip_name: s.chip_name.clone(),
                temp_type: s.temp_type,
                thresholds: s.thresholds.as_ref().map(SensorThresholdsResponse::from),
            }
        })
        .collect();
    // DEC-146 P3-11: deterministic wire order, matching build_fan_entries —
    // and this function's doc comment, which promised "sorted" all along.
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// Build the sorted list of fan entries from a cache snapshot.
pub(crate) fn build_fan_entries(snap: &DaemonState, now: Instant) -> Vec<FanEntry> {
    let mut fans: Vec<FanEntry> = Vec::new();

    // OpenFanController fans
    for (ch, fan) in &snap.openfan_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let stall = if fan.rpm_polled {
            fan.last_commanded_pwm
                .map(|pwm| fan.rpm == 0 && pwm > constants::STALL_PWM_THRESHOLD)
        } else {
            None
        };
        fans.push(FanEntry {
            id: format!("openfan:ch{ch:02}"),
            source: "openfan".into(),
            rpm: Some(fan.rpm),
            last_commanded_pwm: fan.last_commanded_pwm,
            age_ms,
            stall_detected: stall,
        });
    }

    // Hwmon fans
    for (id, fan) in &snap.hwmon_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let stall = match (fan.rpm, fan.last_commanded_pwm) {
            (Some(rpm), Some(pwm)) => Some(rpm == 0 && pwm > constants::STALL_PWM_THRESHOLD),
            _ => None,
        };
        fans.push(FanEntry {
            id: id.clone(),
            source: "hwmon".into(),
            rpm: fan.rpm,
            last_commanded_pwm: fan.last_commanded_pwm,
            age_ms,
            stall_detected: stall,
        });
    }

    // Discrete GPU fans (AMD + Intel share the gpu_fans map; the vendor is
    // encoded in the ID prefix — `amd_gpu:` / `intel_gpu:` — DEC-121).
    for (id, fan) in &snap.gpu_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let source = if id.starts_with("intel_gpu:") {
            "intel_gpu"
        } else {
            "amd_gpu"
        };
        fans.push(FanEntry {
            id: id.clone(),
            source: source.into(),
            rpm: fan.rpm,
            last_commanded_pwm: fan.last_commanded_pct,
            age_ms,
            stall_detected: None,
        });
    }

    fans.sort_by(|a, b| a.id.cmp(&b.id));
    fans
}

/// Shared application state passed to all handlers.
pub struct AppState {
    pub cache: Arc<StateCache>,
    pub staleness_config: StalenessConfig,
    pub daemon_version: String,
    /// Fan controller for OpenFanController write operations. `None` if not connected.
    /// Arc-wrapped to share between API handlers and the profile engine task.
    pub fan_controller: Option<Arc<Mutex<FanController>>>,
    /// Hwmon PWM controller for motherboard fan header writes. `None` if no headers found.
    /// Arc-wrapped to share between API handlers and the profile engine task.
    pub hwmon_controller: Option<Arc<Mutex<HwmonPwmController>>>,
    /// Daemon process start time for uptime calculation.
    pub start_time: Instant,
    /// Per-entity time-series history ring buffer.
    pub history: Arc<crate::health::history::HistoryRing>,
    /// Active profile for headless curve evaluation.
    pub active_profile: Arc<Mutex<Option<crate::profile::DaemonProfile>>>,
    /// Prevents concurrent calibration sweeps from corrupting each other.
    pub calibrating: AtomicBool,
    /// Detected AMD GPU info (populated at startup). Empty if no AMD GPU found.
    pub amd_gpus: Vec<crate::hwmon::gpu_detect::AmdGpuInfo>,
    /// Detected Intel discrete GPU info (populated at startup). Empty if none
    /// found. Read-only telemetry — no fan write path (DEC-121).
    pub intel_gpus: Vec<crate::hwmon::intel_gpu_detect::IntelGpuInfo>,
    /// Configured profile search directories (from daemon.toml [profiles] section).
    /// Wrapped in RwLock to allow runtime updates via SIGHUP reload or API endpoint.
    pub profile_search_dirs: parking_lot::RwLock<Vec<std::path::PathBuf>>,
    /// Path to the admin-owned daemon.toml (read-only to handlers).
    pub config_path: String,
    /// Path to the daemon-owned runtime.toml (read/write by handlers).
    /// Lives at `{state_dir}/runtime.toml`. See ADR-002.
    pub runtime_config_path: std::path::PathBuf,
    /// Number of active SSE client connections (for connection limiting).
    pub sse_clients: Arc<AtomicUsize>,
    /// Set by `POST /hwmon/rescan` to ask the sensor polling loop to refresh
    /// its cached descriptor set (labels, types, DEC-117 threshold snapshot)
    /// on its next tick. Swap-checked (and cleared) by the loop (DEC-133).
    pub sensor_rescan_requested: Arc<AtomicBool>,
    /// Daemon-owned manual-override + fan-identify state (DEC-163 / DEC-166).
    /// Mutated by the `/control/*/override` + `/fans/*/identify` handlers and
    /// swept + applied by the profile engine tick (both hold this same `Arc`).
    pub override_table: Arc<Mutex<crate::control_override::OverrideTable>>,
}

/// RAII guard that clears the profile engine's verify pause on drop (DEC-165),
/// so a dropped or panicked verify handler never leaves the engine paused.
/// Construct via [`begin_verify_pause`].
pub(crate) struct VerifyPauseGuard {
    cache: Arc<crate::health::cache::StateCache>,
}

impl Drop for VerifyPauseGuard {
    fn drop(&mut self) {
        self.cache.end_verify();
    }
}

/// Claim the single verify slot and pause the profile engine's write phase,
/// returning a guard that clears it on drop — or `None` if a verify is already
/// in progress (single-flight; the caller must reject with 409). While paused,
/// the engine skips its write phase so a verify's controlled test writes are not
/// overwritten. `window` is the deadman backstop; the guard is the normal clear
/// path.
pub(crate) fn begin_verify_pause(
    cache: &Arc<crate::health::cache::StateCache>,
    window: std::time::Duration,
) -> Option<VerifyPauseGuard> {
    if cache.try_begin_verify(window) {
        Some(VerifyPauseGuard {
            cache: cache.clone(),
        })
    } else {
        None
    }
}

pub(crate) fn build_status_response(
    state: &AppState,
    snap: &DaemonState,
    health: crate::health::staleness::HealthSummary,
) -> StatusResponse {
    let subsystems = health
        .subsystems
        .into_iter()
        .map(|s| SubsystemStatus {
            name: s.name,
            status: s.status.to_string(),
            age_ms: s.age_ms,
            reason: s.reason,
        })
        .collect();

    let uptime = state.start_time.elapsed().as_secs();

    // Daemon-owned override + identify state (DEC-163/DEC-166) — poll surface.
    let (override_rows, identify_rows) = state.override_table.lock().status_rows();
    let overrides = override_rows
        .into_iter()
        .map(|r| OverrideStatusEntry {
            control_id: r.control_id,
            pwm_percent: r.pwm_percent,
            expires_in_secs: r.expires_in_secs,
        })
        .collect();
    let fan_identify = identify_rows
        .into_iter()
        .map(|r| IdentifyStatusEntry {
            fan_id: r.fan_id,
            expires_in_secs: r.expires_in_secs,
        })
        .collect();

    StatusResponse {
        api_version: API_VERSION,
        daemon_version: state.daemon_version.clone(),
        overall_status: health.overall.to_string(),
        subsystems,
        counters: Counters {
            last_error_summary: None,
        },
        uptime_seconds: Some(uptime),
        // DEC-132: surface the profile engine's thermal override state.
        // `None` only before the engine's first tick — report "normal".
        thermal_state: snap
            .thermal_override_state
            .clone()
            .unwrap_or_else(|| "normal".to_string()),
        overrides,
        fan_identify,
    }
}

/// Serialize any `Serialize` value into a JSON response, returning HTTP 500
/// with a proper error envelope if serialization unexpectedly fails.
pub(crate) fn json_ok(
    status: StatusCode,
    val: impl serde::Serialize,
) -> (StatusCode, Json<serde_json::Value>) {
    match serde_json::to_value(val) {
        Ok(v) => (status, Json(v)),
        Err(e) => {
            log::error!("response serialization failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {
                        "code": "internal_error",
                        "message": "response serialization failed",
                        "retryable": true,
                        "source": "internal"
                    }
                })),
            )
        }
    }
}

/// Helper to serialize an ErrorEnvelope into a JSON value response.
pub(crate) fn error_response(
    status: StatusCode,
    envelope: &ErrorEnvelope,
) -> (StatusCode, Json<serde_json::Value>) {
    json_ok(status, envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::DaemonState;
    use std::time::Instant;

    #[test]
    fn json_ok_serializes_valid_struct() {
        let val = serde_json::json!({"key": "value"});
        let (status, Json(body)) = json_ok(StatusCode::OK, &val);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["key"], "value");
    }

    #[test]
    fn build_sensor_entries_returns_empty_for_empty_state() {
        let state = DaemonState::default();
        let entries = build_sensor_entries(&state, Instant::now());
        assert!(entries.is_empty());
    }

    #[test]
    fn build_fan_entries_returns_empty_for_empty_state() {
        let state = DaemonState::default();
        let entries = build_fan_entries(&state, Instant::now());
        assert!(entries.is_empty());
    }

    #[test]
    fn build_sensor_entries_sorts_by_id() {
        // DEC-146 P3-11: deterministic wire order across restarts/rescans.
        let mut state = DaemonState::default();
        let now = Instant::now();
        for id in ["z_temp", "a_temp", "m_temp"] {
            state.sensors.insert(
                id.into(),
                crate::health::state::CachedSensorReading {
                    id: id.into(),
                    kind: crate::hwmon::types::SensorKind::CpuTemp,
                    label: "t".into(),
                    value_c: 40.0,
                    source: crate::health::state::DeviceLabel::Hwmon,
                    updated_at: now,
                    rate_c_per_s: None,
                    session_min_c: None,
                    session_max_c: None,
                    chip_name: "k10temp".into(),
                    temp_type: None,
                    thresholds: None,
                },
            );
        }
        let ids: Vec<String> = build_sensor_entries(&state, now)
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["a_temp", "m_temp", "z_temp"]);
    }

    #[test]
    fn build_fan_entries_sorts_by_id() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        // Insert fans in reverse order
        state.hwmon_fans.insert(
            "hwmon:z_fan".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:z_fan".into(),
                rpm: Some(1000),
                last_commanded_pwm: None,
                updated_at: now,
            },
        );
        state.hwmon_fans.insert(
            "hwmon:a_fan".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:a_fan".into(),
                rpm: Some(500),
                last_commanded_pwm: None,
                updated_at: now,
            },
        );

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "hwmon:a_fan");
        assert_eq!(entries[1].id, "hwmon:z_fan");
    }

    #[test]
    fn stall_detection_uses_constant_threshold() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        // Fan at PWM=20 with RPM=0 should NOT be stalled (threshold is >20)
        state.hwmon_fans.insert(
            "hwmon:fan1".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:fan1".into(),
                rpm: Some(0),
                last_commanded_pwm: Some(constants::STALL_PWM_THRESHOLD),
                updated_at: now,
            },
        );

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries[0].stall_detected, Some(false));

        // Fan at PWM=21 with RPM=0 SHOULD be stalled
        state
            .hwmon_fans
            .get_mut("hwmon:fan1")
            .unwrap()
            .last_commanded_pwm = Some(constants::STALL_PWM_THRESHOLD + 1);

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries[0].stall_detected, Some(true));
    }

    #[test]
    fn build_sensor_entries_includes_chip_name_and_temp_type() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        state.sensors.insert(
            "hwmon:nct6683:nodev:SYSTIN".into(),
            crate::health::state::CachedSensorReading {
                id: "hwmon:nct6683:nodev:SYSTIN".into(),
                kind: crate::hwmon::types::SensorKind::MbTemp,
                label: "SYSTIN".into(),
                value_c: 42.0,
                source: crate::health::state::DeviceLabel::Hwmon,
                updated_at: now,
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
                chip_name: "nct6683".into(),
                temp_type: Some(3),
                thresholds: None,
            },
        );

        let entries = build_sensor_entries(&state, now);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].chip_name, "nct6683");
        assert_eq!(entries[0].temp_type, Some(3));

        // Verify JSON serialization includes the fields
        let json = serde_json::to_value(&entries[0]).unwrap();
        assert_eq!(json["chip_name"], "nct6683");
        assert_eq!(json["temp_type"], 3);
    }

    #[test]
    fn build_sensor_entries_omits_temp_type_when_none() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        state.sensors.insert(
            "hwmon:k10temp:nodev:Tctl".into(),
            crate::health::state::CachedSensorReading {
                id: "hwmon:k10temp:nodev:Tctl".into(),
                kind: crate::hwmon::types::SensorKind::CpuTemp,
                label: "Tctl".into(),
                value_c: 55.0,
                source: crate::health::state::DeviceLabel::Hwmon,
                updated_at: now,
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
                chip_name: "k10temp".into(),
                temp_type: None,
                thresholds: None,
            },
        );

        let entries = build_sensor_entries(&state, now);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].chip_name, "k10temp");
        assert_eq!(entries[0].temp_type, None);

        // Verify JSON serialization omits temp_type when None
        let json = serde_json::to_value(&entries[0]).unwrap();
        assert_eq!(json["chip_name"], "k10temp");
        assert!(json.get("temp_type").is_none());
    }
}
