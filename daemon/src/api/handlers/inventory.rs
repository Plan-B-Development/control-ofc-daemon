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
use crate::hwmon::inventory::discover_monitor_only_fans;
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
    // Temperature sensors — the live cache projection, identical to `/sensors`
    // (structure + current values, sorted, control_eligible, thresholds).
    let now = std::time::Instant::now();
    let snap = state.cache.snapshot();
    let temp_sensors = build_sensor_entries(&snap, now);

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
        },
    )
}
