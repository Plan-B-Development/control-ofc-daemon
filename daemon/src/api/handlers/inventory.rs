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

    let default_cpu =
        select_default_cpu(classified.iter().map(|(s, c)| (s.id.as_str(), c))).map(|r| {
            DefaultCpuEntry {
                sensor_id: r.sensor_id,
                confidence: r.confidence.to_string(),
                rationale: r.rationale,
            }
        });

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
        },
    )
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

    let inputs = ReadinessInputs {
        cpu_sensor_count,
        default_cpu_confident,
        pwm_total,
        pwm_writable,
        monitor_only_fan_count,
        unavailable_sensor_count: snap.unavailable_sensors.len(),
        unknown_sensor_count,
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
