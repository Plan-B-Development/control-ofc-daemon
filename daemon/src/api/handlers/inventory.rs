//! Read-only hwmon inventory endpoint (Phase 1).
//!
//! `GET /inventory/hwmon` returns a structured, read-only inventory of
//! hwmon-visible hardware for the GUI: temperature sensors (live, mirroring
//! `/sensors`), controllable PWM headers (mirroring `/hwmon/headers`), and
//! monitor-only fan tachometers — `fanN_input` files with no matching `pwmN`,
//! which are otherwise invisible to the API. The daemon never writes hardware
//! to build this report.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{build_sensor_entries, error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::health::state::DaemonState;
use crate::hwmon::classify::{
    classify_temp_sensor, is_cpu_class, select_default_cpu, Confidence, TempClass,
    TempClassification,
};
use crate::hwmon::inventory::discover_monitor_only_fans;
use crate::hwmon::readiness::{build_readiness, overall_severity, ReadinessInputs};
use crate::hwmon::HWMON_SYSFS_ROOT;

/// GET /inventory/hwmon — structured, read-only hardware inventory.
///
/// Runs on the blocking pool because monitor-only-fan discovery walks
/// `/sys/class/hwmon` (mirrors `/diagnostics/hardware`, DEC-099). Read-only.
pub async fn hwmon_inventory_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match tokio::task::spawn_blocking(move || build_hwmon_inventory(&state)).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("hwmon inventory task failed: {e}")),
        ),
    }
}

/// Assemble the inventory response. Synchronous and (for the fan scan) blocking
/// — invoked via `spawn_blocking` from the handler above.
fn build_hwmon_inventory(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    // Temperature sensors — the live cache projection (identical fields to
    // `/sensors`), enriched with the Phase-2 classification refinement.
    let now = std::time::Instant::now();
    let snap = state.cache.snapshot();
    let classified = classify_cache_sensors(&snap, now);

    // Persisted user selections (Phase 5): the preferred CPU sensor wins over the
    // auto-pick when present; both selections are echoed under `preferences`.
    let runtime = crate::runtime_config::RuntimeConfig::load_from(&state.runtime_config_path);
    let preferred_cpu = runtime.preferred_cpu_sensor().map(str::to_string);
    let preferred_mb = runtime.preferred_mb_sensor().map(str::to_string);
    let default_cpu = build_default_cpu(&classified, preferred_cpu.as_deref());
    let preferences = if preferred_cpu.is_some() || preferred_mb.is_some() {
        Some(InventoryPreferences {
            cpu_sensor_id: preferred_cpu,
            mb_sensor_id: preferred_mb,
        })
    } else {
        None
    };

    let temp_sensors: Vec<InventoryTempSensor> = classified
        .into_iter()
        .map(|(sensor, c)| InventoryTempSensor {
            classification: c.class.to_string(),
            confidence: c.confidence.to_string(),
            rationale: c.rationale,
            sensor,
        })
        .collect();

    // Controllable PWM headers — the controller's discovered set, identical to
    // `/hwmon/headers`. Empty when no controller was constructed at startup.
    let pwm_controls: Vec<PwmHeaderEntry> = match &state.hwmon_controller {
        Some(controller) => {
            let ctrl = controller.lock();
            ctrl.headers()
                .into_iter()
                .map(PwmHeaderEntry::from)
                .collect()
        }
        None => Vec::new(),
    };

    // Monitor-only fan tachometers — the one genuinely-new Phase-1 scan:
    // `fanN_input` with no matching `pwmN`. A scan failure (e.g. no
    // `/sys/class/hwmon` under a sandbox) degrades to an empty list, not an
    // error, so the sensors/PWM inventory still returns.
    let monitor_only_fans: Vec<FanInputEntry> =
        match discover_monitor_only_fans(std::path::Path::new(HWMON_SYSFS_ROOT)) {
            Ok(fans) => fans.iter().map(FanInputEntry::from).collect(),
            Err(e) => {
                log::warn!("hwmon inventory: monitor-only fan scan failed: {e}");
                Vec::new()
            }
        };

    json_ok(
        StatusCode::OK,
        HwmonInventoryResponse {
            api_version: API_VERSION,
            temp_sensors,
            pwm_controls,
            monitor_only_fans,
            default_cpu,
            preferences,
        },
    )
}

/// Build the `default_cpu` recommendation: the persisted preferred CPU sensor
/// wins when it is present in the live set (`source: "user"`), otherwise the
/// deterministic auto-pick (`source: "auto"`). A set-but-absent preference falls
/// back to auto — never blindly applied — and the readiness model flags it stale.
fn build_default_cpu(
    classified: &[(SensorEntry, TempClassification)],
    preferred_cpu: Option<&str>,
) -> Option<DefaultCpuEntry> {
    if let Some(pref) = preferred_cpu {
        if let Some((s, c)) = classified.iter().find(|(s, _)| s.id == pref) {
            return Some(DefaultCpuEntry {
                sensor_id: s.id.clone(),
                confidence: c.confidence.to_string(),
                rationale: "user-selected preferred CPU sensor".into(),
                source: "user".into(),
            });
        }
    }
    select_default_cpu(classified.iter().map(|(s, c)| (s.id.as_str(), c))).map(|r| {
        DefaultCpuEntry {
            sensor_id: r.sensor_id,
            confidence: r.confidence.to_string(),
            rationale: r.rationale,
            source: "auto".into(),
        }
    })
}

/// Classify the live cache sensors once — shared by the inventory and readiness
/// handlers so their two views never disagree.
fn classify_cache_sensors(
    snap: &DaemonState,
    now: std::time::Instant,
) -> Vec<(SensorEntry, TempClassification)> {
    build_sensor_entries(snap, now)
        .into_iter()
        .map(|s| {
            let c = classify_temp_sensor(&s.chip_name, &s.label);
            (s, c)
        })
        .collect()
}

/// GET /inventory/readiness — structured, read-only hardware-readiness list.
///
/// Diagnoses the CPU/hwmon/PWM inventory into actionable items (severity +
/// recommended action + blocks-flags) for the GUI's first-run guide. Read-only;
/// never mutates the system. Runs on the blocking pool (monitor-only-fan scan).
pub async fn hwmon_readiness_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match tokio::task::spawn_blocking(move || build_readiness_response(&state)).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("hwmon readiness task failed: {e}")),
        ),
    }
}

fn build_readiness_response(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    let now = std::time::Instant::now();
    let snap = state.cache.snapshot();
    let classified = classify_cache_sensors(&snap, now);

    let cpu_sensor_count = classified
        .iter()
        .filter(|(_, c)| is_cpu_class(c.class))
        .count();
    let unknown_sensor_count = classified
        .iter()
        .filter(|(_, c)| c.class == TempClass::UnknownTemp)
        .count();
    let default_cpu_confident =
        select_default_cpu(classified.iter().map(|(s, c)| (s.id.as_str(), c)))
            .map(|r| r.confidence == Confidence::High);

    // PWM header counts (structural) — read the controller's discovered set.
    let (pwm_total, pwm_writable) = match &state.hwmon_controller {
        Some(controller) => {
            let ctrl = controller.lock();
            let headers = ctrl.headers();
            (
                headers.len(),
                headers.iter().filter(|h| h.is_writable).count(),
            )
        }
        None => (0, 0),
    };

    let monitor_only_fan_count = discover_monitor_only_fans(std::path::Path::new(HWMON_SYSFS_ROOT))
        .map(|v| v.len())
        .unwrap_or(0);

    // Persisted selections (Phase 5): present iff the stored id is in the live
    // set — a set-but-absent selection drives the readiness "missing" items.
    let runtime = crate::runtime_config::RuntimeConfig::load_from(&state.runtime_config_path);
    let selected_cpu_present = runtime
        .preferred_cpu_sensor()
        .map(|id| classified.iter().any(|(s, _)| s.id == id));
    let selected_mb_present = runtime
        .preferred_mb_sensor()
        .map(|id| classified.iter().any(|(s, _)| s.id == id));

    let inputs = ReadinessInputs {
        cpu_sensor_count,
        default_cpu_confident,
        pwm_total,
        pwm_writable,
        monitor_only_fan_count,
        unavailable_sensor_count: snap.unavailable_sensors.len(),
        unknown_sensor_count,
        selected_cpu_present,
        selected_mb_present,
    };

    let items = build_readiness(&inputs);
    let overall = overall_severity(&items);

    json_ok(
        StatusCode::OK,
        ReadinessResponse {
            api_version: API_VERSION,
            overall,
            items,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensor(id: &str, chip: &str, label: &str) -> SensorEntry {
        SensorEntry {
            id: id.into(),
            kind: "cpu_temp".into(),
            label: label.into(),
            value_c: 50.0,
            source: "hwmon".into(),
            age_ms: 0,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: chip.into(),
            temp_type: None,
            thresholds: None,
            control_eligible: true,
        }
    }

    fn classified(pairs: &[(&str, &str, &str)]) -> Vec<(SensorEntry, TempClassification)> {
        pairs
            .iter()
            .map(|(id, chip, label)| (sensor(id, chip, label), classify_temp_sensor(chip, label)))
            .collect()
    }

    #[test]
    fn default_cpu_user_preference_wins_when_present() {
        let c = classified(&[
            ("hwmon:k10temp:x:Tctl", "k10temp", "Tctl"),
            ("hwmon:coretemp:x:Package", "coretemp", "Package id 0"),
        ]);
        let d = build_default_cpu(&c, Some("hwmon:coretemp:x:Package")).unwrap();
        assert_eq!(d.sensor_id, "hwmon:coretemp:x:Package");
        assert_eq!(d.source, "user");
    }

    #[test]
    fn default_cpu_falls_back_to_auto_when_preference_absent() {
        // A set-but-absent preference must NOT be blindly applied — fall back to
        // the auto pick (the readiness model flags the stale selection).
        let c = classified(&[("hwmon:k10temp:x:Tctl", "k10temp", "Tctl")]);
        let d = build_default_cpu(&c, Some("hwmon:gone:x:Tctl")).unwrap();
        assert_eq!(d.sensor_id, "hwmon:k10temp:x:Tctl");
        assert_eq!(d.source, "auto");
    }

    #[test]
    fn default_cpu_auto_when_no_preference() {
        let c = classified(&[("hwmon:k10temp:x:Tctl", "k10temp", "Tctl")]);
        let d = build_default_cpu(&c, None).unwrap();
        assert_eq!(d.source, "auto");
        assert_eq!(d.sensor_id, "hwmon:k10temp:x:Tctl");
    }

    #[test]
    fn default_cpu_none_when_no_cpu_sensors() {
        let c = classified(&[("hwmon:nct6798:x:SYSTIN", "nct6798", "SYSTIN")]);
        assert!(build_default_cpu(&c, None).is_none());
    }
}
