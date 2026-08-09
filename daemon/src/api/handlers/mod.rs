//! Request handlers for the IPC API.
//!
//! Read handlers read from the `StateCache` — no direct hardware access.
//! Write handlers dispatch through the `FanController`.

mod assessment;
pub mod config;
mod control;
mod gpu;
mod hw_diagnostics;
mod hwmon_ctl;
mod inventory;
mod openfan;
mod path_confine;
mod profile;
mod status;

pub use assessment::*;
pub use config::*;
pub use control::*;
pub use gpu::*;
pub use hw_diagnostics::*;
pub use hwmon_ctl::*;
pub use inventory::*;
pub use openfan::*;
pub use profile::*;
pub use status::*;

use parking_lot::Mutex;
use std::sync::atomic::AtomicBool;
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
                // DEC-193: wireless-radio PHY temps (e.g. ath12k WiFi) must not
                // drive a fan curve — derived from the chip name (the daemon
                // engine never consults this; it is an advisory hint the GUI uses
                // to filter its curve-source picker).
                control_eligible: !crate::hwmon::is_wireless_phy_chip(&s.chip_name),
            }
        })
        .collect();
    // DEC-146 P3-11: deterministic wire order, matching build_fan_entries —
    // and this function's doc comment, which promised "sorted" all along.
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// Build the sorted list of currently-unavailable sensor entries (DEC-193) from
/// a cache snapshot — sensors that exist but fail every read (e.g. an `ath12k`
/// WiFi temp while the radio is down). Surfaced on `/status` + `/poll` for
/// display only; they are absent from `build_sensor_entries` (evicted on
/// quarantine).
pub(crate) fn build_unavailable_entries(
    snap: &DaemonState,
    now: Instant,
) -> Vec<UnavailableSensorEntry> {
    let mut entries: Vec<UnavailableSensorEntry> = snap
        .unavailable_sensors
        .iter()
        .map(|u| UnavailableSensorEntry {
            id: u.id.clone(),
            label: u.label.clone(),
            reason: u.reason.clone(),
            unavailable_for_ms: now.duration_since(u.since).as_millis() as u64,
        })
        .collect();
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
            duty_pct: None,
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
            duty_pct: None,
            age_ms,
            stall_detected: stall,
        });
    }

    // Discrete GPU fans (AMD + Intel + NVIDIA share the gpu_fans map; the vendor
    // is encoded in the ID prefix — `amd_gpu:` / `intel_gpu:` / `nvidia_gpu:` —
    // DEC-121/DEC-204).
    for (id, fan) in &snap.gpu_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let source = if id.starts_with("intel_gpu:") {
            "intel_gpu"
        } else if id.starts_with("nvidia_gpu:") {
            "nvidia_gpu"
        } else {
            "amd_gpu"
        };
        fans.push(FanEntry {
            id: id.clone(),
            source: source.into(),
            rpm: fan.rpm,
            last_commanded_pwm: fan.last_commanded_pct,
            duty_pct: fan.duty_pct,
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
    /// Unified NVIDIA discrete GPU identity (nouveau + NVML legs), gathered at
    /// startup. Empty if none found. Read-only telemetry — no fan write path
    /// (DEC-204).
    pub nvidia_gpus: Vec<crate::hwmon::nvidia::NvidiaGpuIdentity>,
    /// Configured profile search directories (from daemon.toml [profiles] section).
    /// Wrapped in RwLock to allow runtime updates via SIGHUP reload or API endpoint.
    pub profile_search_dirs: parking_lot::RwLock<Vec<std::path::PathBuf>>,
    /// Path to the admin-owned daemon.toml (read-only to handlers).
    pub config_path: String,
    /// Path to the daemon-owned runtime.toml (read/write by handlers).
    /// Lives at `{state_dir}/runtime.toml`. See ADR-002.
    pub runtime_config_path: std::path::PathBuf,
    /// Set by `POST /hwmon/rescan` to ask the sensor polling loop to refresh
    /// its cached descriptor set (labels, types, DEC-117 threshold snapshot)
    /// on its next tick. Swap-checked (and cleared) by the loop (DEC-133).
    pub sensor_rescan_requested: Arc<AtomicBool>,
    /// Daemon-owned manual-override + fan-identify state (DEC-163 / DEC-166).
    /// Mutated by the `/control/*/override` + `/fans/*/identify` handlers and
    /// swept + applied by the profile engine tick (both hold this same `Arc`).
    pub override_table: Arc<Mutex<crate::control_override::OverrideTable>>,
    /// DEC-203: whether the opt-in active Super-I/O `/dev/port` probe is enabled
    /// (`[detection] allow_port_probe`). Off by default; the probe also needs the
    /// `CAP_SYS_RAWIO` drop-in to actually function.
    pub allow_port_probe: bool,
    /// The fully-resolved config this process is *running* on — `daemon.toml`
    /// with the `runtime.toml` overlay applied, captured at startup (DEC-243).
    ///
    /// `GET /config` compares this against a fresh read of the same two files to
    /// decide `restart_pending` per key. Nearly every runtime-mutable key is
    /// consumed once at process start, so "persisted" and "in effect" are
    /// genuinely different states and the API must not conflate them.
    pub running_config: crate::config::DaemonConfig,
    /// Cached compact readiness rollup (DEC-206) mirrored onto `/status` + `/poll`
    /// for the GUI Dashboard health chip. `None` until the first scan completes
    /// (startup seed). Written by [`AssessmentCache::store`] as the poll mirror of
    /// the full hardware-assessment snapshot (DEC-207) — refreshed only on
    /// discovery-changing events (startup / rescan / preferred-sensor /
    /// `/inventory/*` GET), never recomputed on the poll path. `build_status_response`
    /// only clones this small struct on the 1 Hz poll — it never re-runs the
    /// expensive scan (cache snapshot + sysfs walk + disk read + Super-I/O detect).
    pub readiness_rollup: Arc<Mutex<Option<crate::hwmon::readiness::ReadinessRollup>>>,
    /// Daemon-owned hardware-assessment cache + single-flight coordinator
    /// (DEC-207): ONE coalesced passive scan feeds the readiness rollup above,
    /// the `/inventory/readiness` + `/inventory/superio` compat readers, and the
    /// combined `/inventory/hardware-readiness` endpoint — so the expensive
    /// Super-I/O scan runs once instead of three times. Holds the SAME
    /// `readiness_rollup` `Arc` as its poll mirror. Never on the 1 Hz poll path.
    pub assessment: Arc<AssessmentCache>,
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

/// Phase 6 (DEC-201): refuse to START a hardware fan verify while the system is
/// hot. A verify pauses the engine's write phase for its window, which also
/// suppresses the 105 °C thermal `force_all` — so a fan diagnostic must never run
/// during a thermal event. Returns `Some(409 thermal_abort)` when any sensor
/// exceeds the calibrate/verify limit (reuses `check_thermal_safety`, matching
/// the calibrate sweep — DEC-134); `None` when it is safe to proceed.
pub(crate) fn verify_thermal_guard(
    cache: &crate::health::cache::StateCache,
) -> Option<(StatusCode, Json<serde_json::Value>)> {
    if let Err(crate::api::calibration::CalibrationError::ThermalAbort {
        sensor_id,
        temp_c,
        limit_c,
    }) = crate::api::calibration::check_thermal_safety(cache)
    {
        return Some(error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::thermal_abort(format!(
                "Cannot run a fan verify while hot: {sensor_id} at {temp_c:.1}°C \
                 (limit {limit_c:.0}°C). Let the system cool, then retry."
            )),
        ));
    }
    None
}

/// Run a blocking, fsync-ing persistence call off the async worker threads
/// (DEC-252).
///
/// `atomic_io::write_atomic` does `write` + `fsync` + `rename` + a directory
/// `fsync`. That is unbounded wall-clock time on whichever tokio worker thread
/// polls the handler — the same runtime the 1 Hz profile engine, and therefore
/// the 105 °C decision, is scheduled on.
///
/// Severity, stated honestly: the runtime is multi-threaded with one worker per
/// core (`#[tokio::main]` with no arguments), so a single write cannot starve
/// the engine on its own — every other worker keeps polling. This removes the
/// coupling rather than leaving the engine's timing dependent on how many cores
/// the machine happens to have and how many writes arrive at once. It also
/// matches what `gpu.rs` and `hw_diagnostics.rs` already do for their blocking
/// sysfs work.
pub(crate) async fn persist_off_runtime<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        // The closure panicked or the runtime is shutting down. Report it as a
        // persistence failure rather than unwrapping — a panicking write must
        // not take an API worker down with it.
        Err(e) => Err(format!("persistence task failed: {e}")),
    }
}

pub(crate) fn build_status_response(
    state: &AppState,
    thermal_state: String,
    unavailable_sensors: Vec<UnavailableSensorEntry>,
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

    // Active profile (DEC-194) — mirror id+name onto the poll surface so an
    // external activation shows within one poll. Tight lock: clone out and drop
    // the guard within this statement; the override_table lock above is already
    // released, so lock order (EFF-1) is preserved.
    let (active_profile_id, active_profile_name) = state
        .active_profile
        .lock()
        .as_ref()
        .map(|p| (Some(p.id.clone()), Some(p.name.clone())))
        .unwrap_or((None, None));

    // DEC-206: mirror the cached readiness rollup for the GUI Dashboard chip.
    // Cheap — clones a small `Option<ReadinessRollup>` under a tight lock (no
    // sysfs/disk; the rollup is refreshed off the poll path). Independent lock,
    // taken and released within this statement, so lock order is preserved.
    let readiness = state.readiness_rollup.lock().clone();

    StatusResponse {
        api_version: API_VERSION,
        daemon_version: state.daemon_version.clone(),
        overall_status: health.overall.to_string(),
        subsystems,
        uptime_seconds: Some(uptime),
        // DEC-132: surface the profile engine's thermal override state. The
        // caller extracts it from the cache (defaulting "normal" before the
        // engine's first tick) so this builder no longer needs a `DaemonState`
        // snapshot — only the `override_table` lock, which must stay OUTSIDE any
        // cache read guard to preserve the lock order (EFF-1).
        thermal_state,
        overrides,
        fan_identify,
        unavailable_sensors,
        active_profile_id,
        active_profile_name,
        readiness,
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
    fn build_fan_entries_routes_gpu_source_by_id_prefix() {
        // The wire `source` the GUI keys on is derived from the gpu_fans id
        // prefix. Each vendor prefix must map to the right source string
        // (DEC-121/DEC-204); a transposed branch would mislabel GPU fans.
        let mut state = DaemonState::default();
        let now = Instant::now();
        let cases = [
            ("amd_gpu:0000:03:00.0", "amd_gpu"),
            ("intel_gpu:0000:04:00.0", "intel_gpu"),
            ("nvidia_gpu:0000:05:00.0", "nvidia_gpu"),
        ];
        for (id, _) in cases {
            state.gpu_fans.insert(
                id.into(),
                crate::health::state::AmdGpuFanState {
                    id: id.into(),
                    rpm: Some(1200),
                    last_commanded_pct: None,
                    duty_pct: Some(33),
                    updated_at: now,
                },
            );
        }

        let entries = build_fan_entries(&state, now);
        for (id, expected_source) in cases {
            let e = entries.iter().find(|e| e.id == id).unwrap();
            assert_eq!(e.source.as_str(), expected_source, "source for {id}");
            // GPU fan telemetry here is read-only — no commanded PWM.
            assert_eq!(e.last_commanded_pwm, None);
            // The measured duty % must route from the cache to the wire (DEC-204).
            assert_eq!(e.duty_pct, Some(33), "duty_pct for {id}");
        }
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
    fn build_sensor_entries_marks_wireless_phy_not_control_eligible() {
        // DEC-193: an ath12k WiFi temp is surfaced for display but flagged
        // control_eligible=false so the GUI won't offer it as a curve source;
        // a real motherboard/CPU sensor stays eligible.
        let mut state = DaemonState::default();
        let now = Instant::now();
        let mk = |id: &str, chip: &str| crate::health::state::CachedSensorReading {
            id: id.into(),
            kind: crate::hwmon::types::SensorKind::MbTemp,
            label: "temp1".into(),
            value_c: 44.0,
            source: crate::health::state::DeviceLabel::Hwmon,
            updated_at: now,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: chip.into(),
            temp_type: None,
            thresholds: None,
        };
        state.sensors.insert(
            "hwmon:ath12k_hwmon:phy0:temp1".into(),
            mk("hwmon:ath12k_hwmon:phy0:temp1", "ath12k_hwmon"),
        );
        state.sensors.insert(
            "hwmon:k10temp:nodev:Tctl".into(),
            mk("hwmon:k10temp:nodev:Tctl", "k10temp"),
        );

        let entries = build_sensor_entries(&state, now);
        let wifi = entries
            .iter()
            .find(|e| e.chip_name == "ath12k_hwmon")
            .unwrap();
        let cpu = entries.iter().find(|e| e.chip_name == "k10temp").unwrap();
        assert!(
            !wifi.control_eligible,
            "wireless PHY must not be a curve source"
        );
        assert!(cpu.control_eligible, "real sensors stay control-eligible");
    }

    #[test]
    fn build_unavailable_entries_sorts_and_computes_age() {
        // DEC-193: unavailable sensors are surfaced sorted by id, with a
        // millisecond age since quarantine.
        let mut state = DaemonState::default();
        let now = Instant::now();
        let since = now - std::time::Duration::from_millis(1500);
        state.unavailable_sensors = vec![
            crate::health::state::UnavailableSensor {
                id: "z_sensor".into(),
                label: "z".into(),
                reason: "Network is down".into(),
                since,
            },
            crate::health::state::UnavailableSensor {
                id: "a_sensor".into(),
                label: "a".into(),
                reason: "Network is down".into(),
                since,
            },
        ];
        let entries = build_unavailable_entries(&state, now);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "a_sensor");
        assert_eq!(entries[1].id, "z_sensor");
        assert!(entries[0].unavailable_for_ms >= 1500);
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
