//! Integration tests for the IPC server over Unix domain socket.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Instant;

use http_body_util::BodyExt;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use control_ofc_daemon::api::handlers::AppState;
use control_ofc_daemon::api::server;
use control_ofc_daemon::health::cache::StateCache;
use control_ofc_daemon::health::history::HistoryRing;
use control_ofc_daemon::health::staleness::StalenessConfig;
use control_ofc_daemon::health::state::{
    AmdGpuFanState, CachedSensorReading, DeviceLabel, OpenFanState,
};
use control_ofc_daemon::hwmon::lease::{HwmonWriter, LeaseManager};
use control_ofc_daemon::hwmon::pwm_control::{HwmonPwmController, SysfsWriter};
use control_ofc_daemon::hwmon::pwm_discovery::PwmHeaderDescriptor;
use control_ofc_daemon::hwmon::types::SensorKind;
use control_ofc_daemon::profile::DaemonProfile;

/// Helper: create AppState with a pre-populated cache.
fn test_app_state() -> Arc<AppState> {
    let cache = Arc::new(StateCache::new());

    // Populate with test data
    cache.update_openfan_fans(vec![
        OpenFanState {
            channel: 0,
            rpm: 1200,
            last_commanded_pwm: Some(128),
            updated_at: Instant::now(),
            rpm_polled: true,
        },
        OpenFanState {
            channel: 1,
            rpm: 1100,
            last_commanded_pwm: None,
            updated_at: Instant::now(),
            rpm_polled: true,
        },
    ]);

    cache.update_sensors(vec![CachedSensorReading {
        id: "hwmon:k10temp:0000:00:18.3:Tctl".into(),
        kind: SensorKind::CpuTemp,
        label: "Tctl".into(),
        value_c: 55.0,
        source: DeviceLabel::Hwmon,
        updated_at: Instant::now(),
        rate_c_per_s: None,
        session_min_c: None,
        session_max_c: None,
        chip_name: "k10temp".into(),
        temp_type: None,
        thresholds: None,
    }]);

    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    })
}

/// Helper: make an HTTP request over a Unix socket and return the JSON body.
async fn uds_get(socket_path: &str, path: &str) -> (u16, serde_json::Value) {
    let stream = UnixStream::connect(socket_path).await.unwrap();
    let io = TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = Request::builder()
        .uri(path)
        .header("host", "localhost")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();

    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    (status, json)
}

/// Helper: start the IPC server on a temp socket and return (path, shutdown_sender).
async fn start_test_server(
    state: Arc<AppState>,
) -> (String, tokio::sync::oneshot::Sender<()>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("test.sock").to_str().unwrap().to_string();

    let (tx, rx) = tokio::sync::oneshot::channel();

    // Bind the listener here (mirrors what preflight_check does in main).
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let path_clone = socket_path.clone();
    tokio::spawn(async move {
        server::serve(listener, path_clone, state, rx)
            .await
            .unwrap();
    });

    // Wait for the socket to become available
    for _ in 0..50 {
        if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Return tmp so it stays alive for the test's duration (dropped at test end)
    (socket_path, tx, tmp)
}

#[tokio::test]
async fn status_endpoint_returns_health() {
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/status").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["daemon_version"], "0.1.0-test");
    // Pin the exact wire string, not just `is_string()` — the fixture's fresh
    // timestamps make every subsystem (and thus overall) "ok", and the GUI's
    // severity display depends on these literals (/test-tests audit P2).
    assert_eq!(json["overall_status"], "ok");
    assert!(json["subsystems"].is_array());
    assert_eq!(json["subsystems"][0]["status"], "ok");
    // DEC-170: the counters envelope (only ever carried a dead last_error_summary)
    // was removed — /status no longer emits it.
    assert!(json.get("counters").is_none());
    // DEC-132: thermal_state defaults to "normal" before the profile engine's
    // first tick reports anything.
    assert_eq!(json["thermal_state"], "normal");

    let _ = shutdown.send(());
    // Clean up socket
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn status_endpoint_reflects_thermal_override_state() {
    // DEC-132: /status must surface the profile engine's thermal override
    // state so the GUI can stand its control loop down during an emergency
    // (previously only /diagnostics/hardware exposed it).
    let state = test_app_state();
    let cache = state.cache.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    cache.set_thermal_override_state("emergency");
    let (status, json) = uds_get(&path, "/status").await;
    assert_eq!(status, 200);
    assert_eq!(json["thermal_state"], "emergency");

    cache.set_thermal_override_state("recovery");
    let (_, json) = uds_get(&path, "/status").await;
    assert_eq!(json["thermal_state"], "recovery");

    cache.set_thermal_override_state("normal");
    let (_, json) = uds_get(&path, "/status").await;
    assert_eq!(json["thermal_state"], "normal");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn status_and_poll_surface_active_profile() {
    // DEC-194: the active profile id+name are mirrored onto /status and /poll so
    // an external activation (CLI --profile, another client, systemd) shows within
    // one 1 Hz poll instead of the GUI's slow /profile/active refresh. Both keys
    // are OMITTED when no profile is active, keeping the additive wire shape
    // unchanged — a client treats an absent key as "unknown" and falls back to
    // /profile/active.
    let state = test_app_state();
    let active = state.active_profile.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Default: no active profile → both keys absent on /status and /poll's status.
    let (status, json) = uds_get(&path, "/status").await;
    assert_eq!(status, 200);
    assert!(
        json.get("active_profile_id").is_none(),
        "active_profile_id must be omitted when no profile is active"
    );
    assert!(json.get("active_profile_name").is_none());
    let (_, poll) = uds_get(&path, "/poll").await;
    assert!(poll["status"].get("active_profile_name").is_none());

    // Activate a profile → id+name appear on both surfaces (same StatusResponse).
    *active.lock() = Some(DaemonProfile {
        id: "silent".into(),
        name: "Silent".into(),
        version: 7,
        description: String::new(),
        controls: Vec::new(),
        curves: Vec::new(),
    });
    let (_, json) = uds_get(&path, "/status").await;
    assert_eq!(json["active_profile_id"], "silent");
    assert_eq!(json["active_profile_name"], "Silent");
    let (_, poll) = uds_get(&path, "/poll").await;
    assert_eq!(poll["status"]["active_profile_id"], "silent");
    assert_eq!(poll["status"]["active_profile_name"], "Silent");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hardware_diagnostics_endpoint_returns_report() {
    // Exercises the spawn_blocking offload path: the handler performs blocking
    // sysfs/procfs reads on the blocking pool and serializes the report. The
    // thermal thresholds are hardcoded constants, so they're machine-independent
    // and safe to assert regardless of the host's actual hardware.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/diagnostics/hardware").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert!(json["hwmon"].is_object());
    assert!(json["thermal_safety"].is_object());
    assert_eq!(json["thermal_safety"]["emergency_threshold_c"], 105.0);
    assert_eq!(json["thermal_safety"]["release_threshold_c"], 80.0);
    assert!(json["kernel_modules"].is_array());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn inventory_hwmon_endpoint_returns_structured_inventory() {
    // Phase 1: GET /inventory/hwmon composes the live sensor set (from the
    // cache) + controllable PWM headers (none here) + monitor-only fan
    // tachometers (scanned from real sysfs on the blocking pool). Only the
    // machine-independent shape is asserted — the monitor_only_fans list depends
    // on the host's /sys/class/hwmon and is omitted when empty, mirroring the
    // /diagnostics/hardware test discipline.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/inventory/hwmon").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    // temp_sensors mirrors /sensors — the fixture seeds one CPU sensor.
    let sensors = json["temp_sensors"].as_array().unwrap();
    assert_eq!(sensors.len(), 1);
    assert_eq!(sensors[0]["id"], "hwmon:k10temp:0000:00:18.3:Tctl");
    assert_eq!(sensors[0]["kind"], "cpu_temp");
    // Phase 2: the fixture's k10temp Tctl refines to cpu_tctl (high), flattened
    // onto the same temp_sensors entry, and is the deterministic default CPU.
    assert_eq!(sensors[0]["classification"], "cpu_tctl");
    assert_eq!(sensors[0]["confidence"], "high");
    assert_eq!(
        json["default_cpu"]["sensor_id"],
        "hwmon:k10temp:0000:00:18.3:Tctl"
    );
    assert_eq!(json["default_cpu"]["confidence"], "high");
    // Phase 5: no persisted preference in the test state → the auto pick.
    assert_eq!(json["default_cpu"]["source"], "auto");
    // No hwmon controller in the test state → no controllable headers.
    assert!(json["pwm_controls"].as_array().unwrap().is_empty());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn inventory_readiness_endpoint_reports_structured_items() {
    // Phase 3: GET /inventory/readiness diagnoses the inventory into actionable
    // items. The fixture has one CPU sensor (cpu_sensor_present / ok) and no
    // hwmon controller (no_pwm_controls / warning), so overall = warning. The
    // monitor_only item depends on the host's sysfs and is not asserted.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/inventory/readiness").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["overall"], "warning");
    let items = json["items"].as_array().unwrap();
    assert!(items
        .iter()
        .any(|i| i["code"] == "cpu_sensor_present" && i["severity"] == "ok"));
    let no_pwm = items
        .iter()
        .find(|i| i["code"] == "no_pwm_controls")
        .expect("no_pwm_controls item present");
    assert_eq!(no_pwm["severity"], "warning");
    assert_eq!(no_pwm["blocks_control"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn config_preferred_cpu_sensor_rejects_unknown_id() {
    // Phase 5: setting a preferred sensor is validated against the live sensor
    // set before persisting — an unknown id (or a missing key) is a 400.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/config/preferred-cpu-sensor",
        &serde_json::json!({ "sensor_id": "hwmon:does-not-exist" }),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(json["error"]["code"], "validation_error");

    let (status, _) = uds_post(
        &path,
        "/config/preferred-cpu-sensor",
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 400);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn sensors_endpoint_returns_readings() {
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/sensors").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);

    let sensors = json["sensors"].as_array().unwrap();
    assert_eq!(sensors.len(), 1);
    assert_eq!(sensors[0]["id"], "hwmon:k10temp:0000:00:18.3:Tctl");
    assert_eq!(sensors[0]["kind"], "cpu_temp");
    assert_eq!(sensors[0]["value_c"], 55.0);
    assert_eq!(sensors[0]["source"], "hwmon");
    assert!(sensors[0]["age_ms"].is_number());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn fans_endpoint_returns_fan_state() {
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/fans").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);

    let fans = json["fans"].as_array().unwrap();
    assert_eq!(fans.len(), 2);

    // Fans are sorted by ID
    assert_eq!(fans[0]["id"], "openfan:ch00");
    assert_eq!(fans[0]["source"], "openfan");
    assert_eq!(fans[0]["rpm"], 1200);
    assert_eq!(fans[0]["last_commanded_pwm"], 128);

    assert_eq!(fans[1]["id"], "openfan:ch01");
    assert_eq!(fans[1]["rpm"], 1100);
    // last_commanded_pwm should be absent (None)
    assert!(fans[1].get("last_commanded_pwm").is_none());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn fans_endpoint_tags_intel_gpu_source_by_id_prefix() {
    // DEC-121: AMD and Intel discrete GPU fans share the cache `gpu_fans` map.
    // `build_fan_entries` must derive the wire `source` from the ID prefix so an
    // Intel fan reports "intel_gpu", not "amd_gpu". A regression here would
    // scatter Intel telemetry in the GUI (it groups/dedups by source).
    let cache = Arc::new(StateCache::new());
    cache.update_gpu_fans(vec![
        AmdGpuFanState {
            id: "amd_gpu:0000:2d:00.0".into(),
            rpm: Some(900),
            last_commanded_pct: Some(40),
            updated_at: Instant::now(),
        },
        AmdGpuFanState {
            id: "intel_gpu:0000:03:00.0".into(),
            rpm: Some(1500),
            last_commanded_pct: None,
            updated_at: Instant::now(),
        },
    ]);

    let state = Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    });
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/fans").await;
    assert_eq!(status, 200);
    let fans = json["fans"].as_array().unwrap();
    // Sorted by ID: amd_gpu:* before intel_gpu:*
    let amd = fans
        .iter()
        .find(|f| f["id"] == "amd_gpu:0000:2d:00.0")
        .unwrap();
    let intel = fans
        .iter()
        .find(|f| f["id"] == "intel_gpu:0000:03:00.0")
        .unwrap();
    assert_eq!(amd["source"], "amd_gpu");
    assert_eq!(intel["source"], "intel_gpu");
    assert_eq!(intel["rpm"], 1500);
    // Read-only: Intel never has a commanded PWM.
    assert!(intel.get("last_commanded_pwm").is_none());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capabilities_includes_intel_gpu_absent_by_default() {
    // The additive `intel_gpu` capability object must always be present (the
    // GUI parser reads it); with no Intel GPU it reports present:false.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    let intel = &json["devices"]["intel_gpu"];
    assert_eq!(intel["present"], false);
    assert_eq!(intel["fan_control_method"], "none");
    assert_eq!(intel["display_label"], "Intel D-GPU");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn poll_endpoint_returns_batched_shape() {
    // Contract test for GET /poll — the GUI's primary 1 Hz read endpoint.
    //
    // Audit finding: /poll had no integration coverage, so a breaking schema
    // change (renaming "status" to "overall", dropping "sensors", etc.) would
    // not be caught here and the GUI's parser would silently fall back to
    // defaults. This test locks in the top-level keys the GUI consumes in
    // DaemonClient.poll() (see control-ofc-gui/src/control_ofc/api/client.py).
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/poll").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);

    // Status block — same shape as /status.
    let status_obj = json["status"]
        .as_object()
        .expect("/poll response must contain 'status' object (GUI consumes it)");
    // Exact wire string — fresh fixture timestamps yield "ok" (audit P2).
    assert_eq!(status_obj["overall_status"], "ok");
    assert!(status_obj["subsystems"].is_array());
    // DEC-170: counters envelope removed.
    assert!(status_obj.get("counters").is_none());

    // Sensors block — same shape as /sensors.
    let sensors = json["sensors"]
        .as_array()
        .expect("/poll response must contain 'sensors' array");
    assert_eq!(sensors.len(), 1);
    assert_eq!(sensors[0]["id"], "hwmon:k10temp:0000:00:18.3:Tctl");
    assert_eq!(sensors[0]["kind"], "cpu_temp");
    assert!(sensors[0]["age_ms"].is_number());

    // Fans block — same shape as /fans.
    let fans = json["fans"]
        .as_array()
        .expect("/poll response must contain 'fans' array");
    assert_eq!(fans.len(), 2);
    assert_eq!(fans[0]["id"], "openfan:ch00");
    assert!(fans[0]["age_ms"].is_number());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// Helper: make an HTTP POST request over a Unix socket and return the JSON body.
async fn uds_post(
    socket_path: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let stream = UnixStream::connect(socket_path).await.unwrap();
    let io = TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let body_bytes = serde_json::to_vec(body).unwrap();
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(body_bytes)))
        .unwrap();

    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let resp_body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();

    (status, json)
}

// ── Hwmon integration tests ──────────────────────────────────────────

/// Mock sysfs writer for hwmon integration tests.
struct HwmonMockWriter;

impl SysfsWriter for HwmonMockWriter {
    fn write_file(
        &mut self,
        _path: &str,
        _value: &str,
    ) -> Result<(), control_ofc_daemon::error::HwmonError> {
        Ok(())
    }

    fn read_file(&self, _path: &str) -> Result<String, control_ofc_daemon::error::HwmonError> {
        Ok("1200\n".to_string())
    }
}

fn make_test_header(id: &str, label: &str, min_pwm: u8) -> PwmHeaderDescriptor {
    PwmHeaderDescriptor {
        id: id.to_string(),
        label: label.to_string(),
        chip_name: "it8696".to_string(),
        device_id: "it87.2624".to_string(),
        pwm_index: 1,
        supports_enable: true,
        pwm_path: "/sys/class/hwmon/hwmon0/pwm1".to_string(),
        enable_path: Some("/sys/class/hwmon/hwmon0/pwm1_enable".to_string()),
        rpm_available: true,
        rpm_path: Some("/sys/class/hwmon/hwmon0/fan1_input".to_string()),
        min_pwm_percent: min_pwm,
        max_pwm_percent: 100,
        is_writable: true,
        pwm_mode: None,
        is_aio: false,
    }
}

fn test_app_state_with_hwmon() -> Arc<AppState> {
    let cache = Arc::new(StateCache::new());
    let headers = vec![
        make_test_header("h1", "CHA_FAN1", 20),
        make_test_header("h2", "CPU_FAN", 30),
    ];
    let lease_mgr = LeaseManager::new();
    let ctrl =
        HwmonPwmController::new(headers, lease_mgr, Box::new(HwmonMockWriter), cache.clone());

    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: Some(Arc::new(Mutex::new(ctrl))),
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    })
}

#[tokio::test]
async fn hwmon_headers_returns_discovered() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/hwmon/headers").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    let headers = json["headers"].as_array().unwrap();
    assert_eq!(headers.len(), 2);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hwmon_headers_empty_when_no_controller() {
    let state = test_app_state(); // no hwmon_controller
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/hwmon/headers").await;

    assert_eq!(status, 200);
    assert_eq!(json["headers"].as_array().unwrap().len(), 0);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── Capabilities integration tests ───────────────────────────────────

#[tokio::test]
async fn capabilities_endpoint_returns_schema() {
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["ipc_transport"], "uds/http");

    // Device capabilities
    assert_eq!(json["devices"]["openfan"]["present"], false);
    assert_eq!(json["devices"]["hwmon"]["present"], false);
    assert_eq!(json["devices"]["aio_hwmon"]["status"], "unsupported");
    assert_eq!(json["devices"]["aio_usb"]["status"], "unsupported");

    // Feature flags
    assert_eq!(json["features"]["openfan_write_supported"], false);
    assert_eq!(json["features"]["hwmon_write_supported"], false);
    // DEC-170: lease capability surface retired — the feature flag is gone.
    assert!(json["features"]
        .get("lease_required_for_hwmon_writes")
        .is_none());
    // Limits
    assert_eq!(json["limits"]["pwm_percent_min"], 0);
    assert_eq!(json["limits"]["pwm_percent_max"], 100);
    // Legacy floor fields removed — thermal safety is centralized

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capabilities_with_hwmon_shows_headers() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;

    assert_eq!(status, 200);
    assert_eq!(json["devices"]["hwmon"]["present"], true);
    assert_eq!(json["devices"]["hwmon"]["pwm_header_count"], 2);
    // DEC-170: per-header lease_required flag retired.
    assert!(json["devices"]["hwmon"].get("lease_required").is_none());
    assert_eq!(json["features"]["hwmon_write_supported"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn unknown_endpoint_returns_error_envelope() {
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/nonexistent").await;

    assert_eq!(status, 404);
    assert_eq!(json["error"]["code"], "not_found");
    assert_eq!(json["error"]["retryable"], false);
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("/nonexistent"));

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── M12 / M13: hardware-unavailable status code consistency ─────────────

#[tokio::test]
async fn hwmon_verify_no_controller_returns_503() {
    // M12: when no hwmon controller is present (OpenFan-only or GPU-only
    // systems), /hwmon/{id}/verify must return 503 hardware_unavailable to
    // match every sibling hwmon endpoint, not 404 validation_error.
    let state = test_app_state(); // hwmon_controller = None
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "lease_id": "any" });
    let (status, json) = uds_post(&path, "/hwmon/fake:header/verify", &body).await;

    assert_eq!(status, 503);
    assert_eq!(json["error"]["code"], "hardware_unavailable");
    assert_eq!(json["error"]["retryable"], true);
    assert_eq!(json["error"]["source"], "hardware");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn gpu_reset_fan_unknown_gpu_returns_404() {
    let state = test_app_state(); // amd_gpus empty
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) =
        uds_post(&path, "/gpu/0000:99:00.0/fan/reset", &serde_json::json!({})).await;

    assert_eq!(status, 404);
    assert_eq!(json["error"]["code"], "validation_error");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// Construct an `AppState` with one GPU that has no write path at all.
fn test_app_state_with_unsupported_gpu(pci_bdf: &str) -> Arc<AppState> {
    use control_ofc_daemon::hwmon::gpu_detect::AmdGpuInfo;
    use std::path::PathBuf;

    let cache = Arc::new(StateCache::new());
    let unsupported = AmdGpuInfo {
        pci_bdf: pci_bdf.into(),
        pci_device_id: 0x0000,
        pci_revision: 0x00,
        pci_class: 0x030000,
        marketing_name: Some("Fake unsupported GPU".into()),
        hwmon_path: PathBuf::from("/nonexistent/hwmon"),
        fan_curve_path: None,
        fan_zero_rpm_path: None,
        is_discrete: true,
        has_fan_rpm: false,
        has_pwm: false,
        has_pwm_enable: false,
        overdrive_enabled: false,
    };
    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: vec![unsupported],
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    })
}

#[tokio::test]
async fn gpu_reset_fan_unsupported_returns_400_feature_unavailable() {
    // P1-1 (reset path): mirror of the set-fan test for /gpu/{id}/fan/reset.
    let bdf = "0000:99:00.1";
    let state = test_app_state_with_unsupported_gpu(bdf);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;

    assert_eq!(status, 400);
    assert_eq!(json["error"]["code"], "feature_unavailable");
    assert_eq!(json["error"]["retryable"], false);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// Construct an `AppState` with one read-only RDNA3/4 GPU: `pwm1` exists but
/// `pwm1_enable` does NOT, and there is no PMFW `fan_curve` either. This is
/// the bare-RDNA4 shape on a kernel without `amdgpu.ppfeaturemask=0xffffffff`.
/// Before DEC-098 the handlers fell through to `set_legacy_pwm` here and
/// returned a misleading 503 hardware_unavailable when the `pwm1_enable`
/// write hit ENOENT; the canonical answer is 400 feature_unavailable.
fn test_app_state_with_read_only_gpu(pci_bdf: &str, pci_device_id: u16) -> Arc<AppState> {
    use control_ofc_daemon::hwmon::gpu_detect::AmdGpuInfo;
    use std::path::PathBuf;

    let cache = Arc::new(StateCache::new());
    let read_only = AmdGpuInfo {
        pci_bdf: pci_bdf.into(),
        pci_device_id,
        pci_revision: 0xC0,
        pci_class: 0x030000,
        marketing_name: Some("RX 9070 XT".into()),
        hwmon_path: PathBuf::from("/nonexistent/hwmon"),
        fan_curve_path: None,
        fan_zero_rpm_path: None,
        is_discrete: true,
        has_fan_rpm: true,
        has_pwm: true,         // pwm1 exists
        has_pwm_enable: false, // but pwm1_enable does NOT — this is the bug shape
        overdrive_enabled: false,
    };
    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: vec![read_only],
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    })
}

#[tokio::test]
async fn gpu_reset_fan_read_only_rdna_returns_400_feature_unavailable() {
    // DEC-098 mirror for the reset path.
    let bdf = "0000:03:00.0";
    let state = test_app_state_with_read_only_gpu(bdf, 0x7550);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;

    assert_eq!(status, 400);
    assert_eq!(json["error"]["code"], "feature_unavailable");
    assert_eq!(json["error"]["retryable"], false);
    assert_eq!(json["error"]["source"], "validation");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── /profile/deactivate (DEC-097) ───────────────────────────────────────

/// Helper: test_app_state with an active profile pre-populated.
fn test_app_state_with_active_profile() -> Arc<AppState> {
    let state = test_app_state();
    {
        let mut guard = state.active_profile.lock();
        *guard = Some(control_ofc_daemon::profile::DaemonProfile {
            id: "balanced".into(),
            name: "Balanced".into(),
            version: 4,
            description: String::new(),
            controls: Vec::new(),
            curves: Vec::new(),
        });
    }
    state
}

#[tokio::test]
async fn deactivate_profile_clears_active_profile() {
    let state = test_app_state_with_active_profile();
    assert!(state.active_profile.lock().is_some(), "precondition");

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let (status, json) = uds_post(&path, "/profile/deactivate", &serde_json::json!({})).await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["deactivated"], true);
    assert_eq!(json["previous_profile_id"], "balanced");
    assert_eq!(json["previous_profile_name"], "Balanced");

    // In-memory state must be cleared.
    assert!(
        state.active_profile.lock().is_none(),
        "active_profile must be None after deactivation"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn deactivate_profile_idempotent_when_no_active() {
    let state = test_app_state();
    assert!(state.active_profile.lock().is_none(), "precondition");

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let (status, json) = uds_post(&path, "/profile/deactivate", &serde_json::json!({})).await;

    assert_eq!(status, 200);
    assert_eq!(json["deactivated"], true);
    // No previous profile → fields are JSON null
    assert!(
        json["previous_profile_id"].is_null(),
        "previous_profile_id must be null when no profile was active"
    );
    assert!(json["previous_profile_name"].is_null());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn deactivate_profile_releases_profile_engine_lease() {
    // Build the same hwmon-equipped state the lease tests use, take a
    // "profile-engine" lease, and verify deactivation releases it.
    let state = test_app_state_with_hwmon();
    {
        let mut active = state.active_profile.lock();
        *active = Some(control_ofc_daemon::profile::DaemonProfile {
            id: "balanced".into(),
            name: "Balanced".into(),
            version: 4,
            description: String::new(),
            controls: Vec::new(),
            curves: Vec::new(),
        });
    }
    {
        let ctrl = state.hwmon_controller.as_ref().unwrap();
        let mut guard = ctrl.lock();
        guard
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .expect("take should succeed");
        assert_eq!(
            guard.lease_manager().active_lease().unwrap().owner,
            HwmonWriter::Engine
        );
    }

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let (status, _json) = uds_post(&path, "/profile/deactivate", &serde_json::json!({})).await;
    assert_eq!(status, 200);

    // Profile-engine lease should now be released — leaving the controller
    // free for the engine to re-acquire on its next tick.
    let ctrl = state.hwmon_controller.as_ref().unwrap();
    let guard = ctrl.lock();
    assert!(
        guard.lease_manager().active_lease().is_none(),
        "profile-engine lease must be released after deactivation"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn deactivate_profile_preserves_foreign_lease() {
    // A non-engine holder (a hardware verify or a thermal-safety force) must NOT
    // be touched by deactivation — the deactivate handler releases only the
    // engine's own lease (post-2.0.0 there is no GUI/client lease — DEC-165/197).
    let state = test_app_state_with_hwmon();
    let foreign_lease_id = {
        let ctrl = state.hwmon_controller.as_ref().unwrap();
        let mut guard = ctrl.lock();
        guard
            .lease_manager_mut()
            .take_lease(HwmonWriter::Verify)
            .expect("take should succeed")
            .lease_id
    };

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;
    let (status, _json) = uds_post(&path, "/profile/deactivate", &serde_json::json!({})).await;
    assert_eq!(status, 200);

    // Foreign (verify) lease unchanged.
    let ctrl = state.hwmon_controller.as_ref().unwrap();
    let guard = ctrl.lock();
    let active = guard
        .lease_manager()
        .active_lease()
        .expect("foreign lease should still be active");
    assert_eq!(active.owner, HwmonWriter::Verify);
    assert_eq!(active.lease_id, foreign_lease_id);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn deactivate_profile_preserves_thermal_lease() {
    // A live thermal-safety force-take must also survive a deactivation —
    // deactivate releases ONLY the engine's own lease. Guards profile.rs against
    // a `!= Verify` broadening that would wrongly release a post-emergency thermal
    // lease (DEC-197).
    let state = test_app_state_with_hwmon();
    let thermal_lease_id = {
        let ctrl = state.hwmon_controller.as_ref().unwrap();
        let mut guard = ctrl.lock();
        guard
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::ThermalSafety)
            .lease_id
    };

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;
    let (status, _json) = uds_post(&path, "/profile/deactivate", &serde_json::json!({})).await;
    assert_eq!(status, 200);

    let ctrl = state.hwmon_controller.as_ref().unwrap();
    let guard = ctrl.lock();
    let active = guard
        .lease_manager()
        .active_lease()
        .expect("thermal-safety lease should still be active");
    assert_eq!(active.owner, HwmonWriter::ThermalSafety);
    assert_eq!(active.lease_id, thermal_lease_id);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// Hwmon writer that records every write so a test can assert `pwm_enable`
/// re-assertion. `read_file` returns "1" so the `pwm_enable` watchdog sees
/// manual mode still set (it would re-write on any read ≠ 1) — leaving
/// *coalescing*, not the watchdog, to decide whether `pwm_enable` is re-written.
#[derive(Clone)]
struct LoggingHwmonWriter {
    writes: Arc<Mutex<Vec<(String, String)>>>,
}

impl SysfsWriter for LoggingHwmonWriter {
    fn write_file(
        &mut self,
        path: &str,
        value: &str,
    ) -> Result<(), control_ofc_daemon::error::HwmonError> {
        self.writes
            .lock()
            .push((path.to_string(), value.to_string()));
        Ok(())
    }

    fn read_file(&self, _path: &str) -> Result<String, control_ofc_daemon::error::HwmonError> {
        Ok("1\n".to_string())
    }
}

#[tokio::test]
async fn deactivate_profile_resets_hwmon_coalescing() {
    // Audit P3-3: deactivation releases the profile-engine lease AND must pair
    // it with `on_lease_released()`, so a later reactivation re-asserts
    // `pwm_enable=1` from a clean slate. Without the pairing, the controller's
    // stale write-state would coalesce the next same-value write and skip the
    // enable re-assert — this test fails if the pairing is removed.
    let writes: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let cache = Arc::new(StateCache::new());
    let ctrl = HwmonPwmController::new(
        vec![make_test_header("h1", "CHA_FAN1", 0)],
        LeaseManager::new(),
        Box::new(LoggingHwmonWriter {
            writes: writes.clone(),
        }),
        cache.clone(),
    );
    let state = Arc::new(AppState {
        cache: cache.clone(),
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: Some(Arc::new(Mutex::new(ctrl))),
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(Mutex::new(Some(DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: String::new(),
            controls: Vec::new(),
            curves: Vec::new(),
        }))),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    });
    let hwmon = state.hwmon_controller.clone().unwrap();

    // The engine holds a profile-engine lease and has written a value, seeding
    // coalescing state (manual_mode_set=true, last=50): enable(1) + pwm(50).
    {
        let mut g = hwmon.lock();
        let lease = g
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap()
            .lease_id;
        g.set_pwm("h1", 50, &lease).unwrap();
    }
    assert_eq!(writes.lock().len(), 2, "seed = enable(1) + pwm(50)");

    let (sock, shutdown, _d) = start_test_server(state.clone()).await;
    let (st, _j) = uds_post(&sock, "/profile/deactivate", &serde_json::json!({})).await;
    assert_eq!(st, 200);

    // Reacquire control with a fresh lease and the SAME value. Because
    // deactivate reset coalescing, this re-writes pwm_enable=1 rather than
    // coalescing to a no-op.
    {
        let mut g = hwmon.lock();
        let lease = g
            .lease_manager_mut()
            .take_lease(HwmonWriter::Engine)
            .unwrap()
            .lease_id;
        g.set_pwm("h1", 50, &lease).unwrap();
    }

    let w = writes.lock();
    let enable_asserts = w
        .iter()
        .filter(|(p, v)| p.ends_with("pwm1_enable") && v == "1")
        .count();
    assert_eq!(
        enable_asserts, 2,
        "pwm_enable must be re-asserted after deactivate (coalescing reset by \
         on_lease_released); writes were {:?}",
        *w
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&sock);
}

// ── GPU reset succeeds (daemon-mediated action) ─────────────────────────

/// Construct an `AppState` with a fully-writable PMFW GPU pointing at real
/// files in a tempdir. The caller must keep the returned ``TempDir`` alive
/// for the duration of the test (drop deletes the files).
fn test_app_state_with_writable_pmfw_gpu(pci_bdf: &str) -> (Arc<AppState>, tempfile::TempDir) {
    use control_ofc_daemon::hwmon::gpu_detect::AmdGpuInfo;

    let tmp = tempfile::tempdir().unwrap();
    let fan_curve_path = tmp.path().join("fan_curve");
    let zero_rpm_path = tmp.path().join("fan_zero_rpm_enable");

    // Pre-populate with the multi-line PMFW format the daemon parses.
    std::fs::write(
        &fan_curve_path,
        "OD_FAN_CURVE:\n0: 25C 30%\n1: 50C 50%\n2: 70C 70%\n3: 85C 85%\n4: 100C 100%\n\
         OD_RANGE:\nFAN_CURVE(hotspot temp): 25C 100C\nFAN_CURVE(fan speed): 15% 100%\n",
    )
    .unwrap();
    std::fs::write(
        &zero_rpm_path,
        "FAN_ZERO_RPM_ENABLE:\n1\nOD_RANGE:\nZERO_RPM_ENABLE: 0 1\n",
    )
    .unwrap();

    let cache = Arc::new(StateCache::new());
    let pmfw = AmdGpuInfo {
        pci_bdf: pci_bdf.into(),
        pci_device_id: 0x7550,
        pci_revision: 0xC0,
        pci_class: 0x030000,
        marketing_name: Some("RX 9070 XT (test)".into()),
        hwmon_path: tmp.path().to_path_buf(),
        fan_curve_path: Some(fan_curve_path),
        fan_zero_rpm_path: Some(zero_rpm_path),
        is_discrete: true,
        has_fan_rpm: true,
        has_pwm: true,
        has_pwm_enable: false,
        overdrive_enabled: true,
    };

    let state = Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: vec![pmfw],
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    });
    (state, tmp)
}

#[tokio::test]
async fn gpu_reset_fan_succeeds() {
    // POST /gpu/{id}/fan/reset on a writable PMFW GPU restores automatic fan
    // control and returns 200 {reset:true}. DEC-165: the daemon engine is the
    // sole writer now, so reset no longer records GUI activity — it is a
    // daemon-mediated action, not a GUI write.
    let bdf = "0000:03:00.0";
    let (state, _tmp) = test_app_state_with_writable_pmfw_gpu(bdf);

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;
    let (status, json) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;

    assert_eq!(status, 200, "body: {json}");
    assert_eq!(json["reset"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── Audit P2.5: HwmonVerifyResponse.restore_failed wire format ──────────

#[tokio::test]
async fn hwmon_verify_response_omits_restore_failed_when_false() {
    // skip_serializing_if = "is_false" means a successful restore yields a
    // response without the field — older clients that don't know about it
    // see exactly the same wire shape as before. The GUI dataclass defaults
    // to ``False`` when the field is missing.
    use control_ofc_daemon::api::responses::{HwmonVerifyResponse, HwmonVerifyState};

    let resp = HwmonVerifyResponse {
        header_id: "h1".into(),
        result: "effective".into(),
        initial_state: HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(128),
            pwm_percent: Some(50),
            rpm: Some(1200),
        },
        final_state: HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(178),
            pwm_percent: Some(70),
            rpm: Some(900),
        },
        test_pwm_percent: 70,
        wait_seconds: 6,
        details: "ok".into(),
        restore_failed: false,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert!(
        json.get("restore_failed").is_none(),
        "restore_failed must be omitted when false (skip_serializing_if): {json}"
    );
}

#[tokio::test]
async fn hwmon_verify_response_includes_restore_failed_when_true() {
    use control_ofc_daemon::api::responses::{HwmonVerifyResponse, HwmonVerifyState};

    let resp = HwmonVerifyResponse {
        header_id: "h1".into(),
        result: "effective".into(),
        initial_state: HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(128),
            pwm_percent: Some(50),
            rpm: Some(1200),
        },
        final_state: HwmonVerifyState {
            pwm_enable: Some(1),
            pwm_raw: Some(51),
            pwm_percent: Some(20),
            rpm: Some(700),
        },
        test_pwm_percent: 20,
        wait_seconds: 6,
        details: "ok".into(),
        restore_failed: true,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(
        json["restore_failed"], true,
        "restore_failed must be present and true so GUI can warn the operator"
    );
}

/// DEC-102 integration: discovery + IPC. Build a fake hwmon root with one
/// motherboard chip (`it8696` with `pwm1`/`pwm1_enable`) and one amdgpu
/// chip (RDNA3+ shape: `pwm1` + `fan1_input`, no `pwm1_enable`). Run real
/// `discover_pwm_headers` over it, hand the result to `HwmonPwmController`,
/// then call `GET /hwmon/headers` over the IPC socket. The amdgpu header
/// must not appear on the wire — this is the canonical pre-DEC-102
/// failure mode (the GUI used to surface `hwmon:amdgpu:.../pwm1:pwm1` and
/// the user could bind it to a profile, producing 1 Hz EACCES storms).
#[tokio::test]
async fn hwmon_discovery_excludes_amdgpu_end_to_end_via_ipc() {
    use control_ofc_daemon::hwmon::pwm_discovery::discover_pwm_headers;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Motherboard chip — fully writable.
    {
        let dir = root.join("hwmon0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("name"), "it8696").unwrap();
        std::fs::write(dir.join("pwm1"), "128\n").unwrap();
        std::fs::write(dir.join("pwm1_enable"), "2\n").unwrap();
        std::fs::write(dir.join("fan1_input"), "1200\n").unwrap();
        std::fs::write(dir.join("fan1_label"), "CPU_FAN\n").unwrap();
    }
    // AMD GPU — RDNA3+ shape. pwm1 present, pwm1_enable absent.
    {
        let dir = root.join("hwmon1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("name"), "amdgpu").unwrap();
        std::fs::write(dir.join("pwm1"), "0\n").unwrap();
        std::fs::write(dir.join("fan1_input"), "0\n").unwrap();
    }

    let descriptors = discover_pwm_headers(root).expect("discovery succeeds");
    assert_eq!(
        descriptors.len(),
        1,
        "discovery must drop amdgpu before the IPC layer ever sees it: {descriptors:#?}"
    );
    assert_eq!(descriptors[0].chip_name, "it8696");

    // Hand the discovery result to a real controller and serve over IPC.
    let cache = Arc::new(StateCache::new());
    let lease_mgr = LeaseManager::new();
    let ctrl = HwmonPwmController::new(
        descriptors,
        lease_mgr,
        Box::new(HwmonMockWriter),
        cache.clone(),
    );
    let state = Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: Some(Arc::new(Mutex::new(ctrl))),
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    });
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/hwmon/headers").await;
    assert_eq!(status, 200);
    let headers = json["headers"].as_array().unwrap();
    assert_eq!(headers.len(), 1, "amdgpu must not appear: {headers:#?}");
    assert_eq!(headers[0]["chip_name"], "it8696");
    // Sanity: no header id contains "amdgpu" anywhere.
    for h in headers {
        let id = h["id"].as_str().unwrap_or_default();
        assert!(
            !id.contains("amdgpu"),
            "header id must not reference amdgpu: {id}"
        );
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// POST /profile/activate — path-traversal protection (P2-A from /audit)
//
// `profile_path` is the canonical CWE-22 surface for the daemon: a caller
// supplies a filesystem path which the daemon then opens. The handler
// canonicalises both the candidate path and every configured search
// directory, then requires `candidate.starts_with(dir)` for at least one
// `dir`. Unit tests in `profile.rs::find_profile` cover the lookup-by-id
// side; the next three tests lock the `profile_path`-by-direct-path side
// at the IPC layer so a regression in the canonicalise/starts_with logic
// would fail CI rather than wait to be caught in production.
// ---------------------------------------------------------------------------

/// Build an AppState whose `profile_search_dirs` points at `dirs`. No fan or
/// hwmon controllers are wired in — these tests only need the profile
/// activation handler to run.
fn test_app_state_with_profile_dirs(dirs: Vec<std::path::PathBuf>) -> Arc<AppState> {
    Arc::new(AppState {
        cache: Arc::new(StateCache::new()),
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: None,
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(dirs),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
    })
}

#[tokio::test]
async fn profile_activate_rejects_path_outside_search_dirs() {
    // Set up: search dir at /tmp/.X/search, profile placed at /tmp/.X/outside.
    // The candidate path is fully outside the search dir, so the handler must
    // reject it with 400 validation_error — never load the file.
    let tmp = tempfile::tempdir().unwrap();
    let search_dir = tmp.path().join("search");
    let outside_dir = tmp.path().join("outside");
    std::fs::create_dir_all(&search_dir).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();

    let outside_profile = outside_dir.join("evil.json");
    std::fs::write(&outside_profile, r#"{"id": "evil", "name": "Outside"}"#).unwrap();

    let state = test_app_state_with_profile_dirs(vec![search_dir.clone()]);
    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let body = serde_json::json!({
        "profile_path": outside_profile.display().to_string(),
    });
    let (status, json) = uds_post(&path, "/profile/activate", &body).await;

    assert_eq!(
        status, 400,
        "profile_path outside any search dir must be 400, got {status}: {json}"
    );
    assert_eq!(json["error"]["code"], "validation_error");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("search directory"),
        "error must mention search directory restriction: {json}"
    );
    // Crucially: no profile was activated.
    assert!(state.active_profile.lock().is_none());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn profile_activate_rejects_symlink_chained_outside_search_dirs() {
    // Set up: search dir at /tmp/.X/search, real profile at /tmp/.X/outside.
    // Place a symlink INSIDE the search dir that points OUT to the real file.
    // `starts_with` on the raw path would let this past, but `canonicalize`
    // resolves the symlink to its real location, which is outside — the
    // handler must reject. This is the TOCTOU-resistant CWE-22 check.
    let tmp = tempfile::tempdir().unwrap();
    let search_dir = tmp.path().join("search");
    let outside_dir = tmp.path().join("outside");
    std::fs::create_dir_all(&search_dir).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();

    let real_profile = outside_dir.join("real.json");
    std::fs::write(&real_profile, r#"{"id": "real", "name": "Real"}"#).unwrap();

    let symlink_path = search_dir.join("link.json");
    std::os::unix::fs::symlink(&real_profile, &symlink_path).unwrap();

    let state = test_app_state_with_profile_dirs(vec![search_dir.clone()]);
    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let body = serde_json::json!({
        "profile_path": symlink_path.display().to_string(),
    });
    let (status, json) = uds_post(&path, "/profile/activate", &body).await;

    assert_eq!(
        status, 400,
        "symlink pointing outside search dir must be 400, got {status}: {json}"
    );
    assert_eq!(json["error"]["code"], "validation_error");
    assert!(state.active_profile.lock().is_none());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn profile_activate_accepts_path_inside_search_dir() {
    // Positive control: a profile_path that canonicalizes to a path inside
    // a configured search dir must load and activate. Without this, the
    // two rejection tests above would also pass if the handler accidentally
    // rejected every path.
    let tmp = tempfile::tempdir().unwrap();
    let search_dir = tmp.path().join("search");
    std::fs::create_dir_all(&search_dir).unwrap();

    let profile_path = search_dir.join("ok.json");
    std::fs::write(&profile_path, r#"{"id": "ok", "name": "OK", "version": 4}"#).unwrap();

    let state = test_app_state_with_profile_dirs(vec![search_dir.clone()]);
    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let body = serde_json::json!({
        "profile_path": profile_path.display().to_string(),
    });
    let (status, json) = uds_post(&path, "/profile/activate", &body).await;

    assert_eq!(
        status, 200,
        "profile inside search dir must activate, got {status}: {json}"
    );
    assert_eq!(json["activated"], true);
    assert_eq!(json["profile_id"], "ok");
    assert_eq!(json["profile_name"], "OK");
    let guard = state.active_profile.lock();
    assert_eq!(
        guard.as_ref().map(|p| p.id.as_str()),
        Some("ok"),
        "in-memory active profile must reflect the activation"
    );
    drop(guard);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ───────────────────── Profile CRUD (DEC-160) ─────────────────────

/// Send an arbitrary-method request (optionally with a JSON body) over the UDS.
async fn uds_send(
    socket_path: &str,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> (u16, serde_json::Value) {
    let stream = UnixStream::connect(socket_path).await.unwrap();
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let body_bytes = body
        .map(|b| serde_json::to_vec(b).unwrap())
        .unwrap_or_default();
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(body_bytes)))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let resp_body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&resp_body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// A minimal, valid v7 profile: one curve-mode control bound to a flat curve.
/// Flat curves carry no `sensor_id`, so this validates clean on any machine.
fn valid_profile(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": id,
        "description": "",
        "version": 7,
        "controls": [{"id": "c", "name": "C", "mode": "curve", "curve_id": "fc", "members": []}],
        "curves": [{"id": "fc", "name": "Flat", "type": "flat", "flat_output_pct": 40.0}],
    })
}

/// AppState whose profile store is an isolated temp dir (never `/var/lib`).
/// Returns the state plus the TempDir, which the caller must keep alive.
fn state_with_temp_store() -> (Arc<AppState>, tempfile::TempDir) {
    let store = tempfile::tempdir().unwrap();
    let state = test_app_state();
    state
        .profile_search_dirs
        .write()
        .push(store.path().to_path_buf());
    (state, store)
}

#[tokio::test]
async fn profiles_crud_roundtrip() {
    let (state, store) = state_with_temp_store();
    let (sock, _tx, _sock_tmp) = start_test_server(state).await;

    // Create.
    let (st, body) = uds_send(&sock, "POST", "/profiles", Some(&valid_profile("p1"))).await;
    assert_eq!(st, 201, "create: {body}");
    assert_eq!(body["created"], true);
    assert!(
        store.path().join("p1.json").exists(),
        "profile must be persisted to the store"
    );

    // Get (lossless).
    let (st, body) = uds_get(&sock, "/profiles/p1").await;
    assert_eq!(st, 200);
    assert_eq!(body["id"], "p1");
    assert_eq!(body["curves"][0]["type"], "flat");

    // List includes it.
    let (st, body) = uds_get(&sock, "/profiles").await;
    assert_eq!(st, 200);
    let ids: Vec<&str> = body["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"p1"), "list must contain p1: {ids:?}");

    // Duplicate create → 409 already_exists.
    let (st, body) = uds_send(&sock, "POST", "/profiles", Some(&valid_profile("p1"))).await;
    assert_eq!(st, 409);
    assert_eq!(body["error"]["code"], "already_exists");

    // Update (PUT replaces).
    let mut updated = valid_profile("p1");
    updated["name"] = "Renamed".into();
    let (st, body) = uds_send(&sock, "PUT", "/profiles/p1", Some(&updated)).await;
    assert_eq!(st, 200, "update: {body}");
    assert_eq!(body["profile_id"], "p1");

    // Delete, then it's gone.
    let (st, _) = uds_send(&sock, "DELETE", "/profiles/p1", None).await;
    assert_eq!(st, 200);
    let (st, _) = uds_get(&sock, "/profiles/p1").await;
    assert_eq!(st, 404);
}

#[tokio::test]
async fn profile_create_invalid_returns_field_violations() {
    let (state, store) = state_with_temp_store();
    let (sock, _tx, _sock_tmp) = start_test_server(state).await;

    let bad = serde_json::json!({
        "id": "bad", "name": "Bad", "version": 7, "controls": [],
        "curves": [{"id": "fc", "name": "F", "type": "flat", "flat_output_pct": 140.0}],
    });
    let (st, body) = uds_send(&sock, "POST", "/profiles", Some(&bad)).await;
    assert_eq!(st, 400);
    assert_eq!(body["error"]["code"], "validation_error");
    let violations = body["error"]["details"]["field_violations"]
        .as_array()
        .unwrap();
    assert!(
        violations.iter().any(|v| v["reason"] == "OUT_OF_RANGE"),
        "expected OUT_OF_RANGE, got {violations:?}"
    );
    assert!(
        !store.path().join("bad.json").exists(),
        "invalid profile must not be persisted"
    );
}

// Semantic 400 `validation_error` envelope: a VALID-JSON body that is *missing a
// required field* on a `Json<serde_json::Value>` handler returns the standard
// error envelope {code, message, retryable, source}. NOTE: a *syntactically*
// malformed JSON body, or a missing field on a TYPED extractor (e.g.
// `/control/{id}/override` without `pwm_percent`), deliberately returns axum's
// plain-text rejection (400 for a syntax error, 422 for a typed-shape reject) —
// NOT this envelope — because the daemon uses axum's default `Json<T>` extractor
// with no custom rejection mapping (see daemon/src/api/responses.rs:1182-1191).
// The absence of an envelope on those paths is intentional, not a coverage gap.
#[tokio::test]
async fn profile_create_missing_id_returns_validation_envelope() {
    let (sock, _tx, _d) = start_test_server(test_app_state()).await;
    // Valid JSON object with no `id` key → create_profile_handler's missing-`id`
    // branch emits ErrorEnvelope::validation("missing 'id' field") and returns
    // 400 before ever touching the profile store.
    let (st, body) = uds_post(
        &sock,
        "/profiles",
        &serde_json::json!({"name": "No Id", "version": 7, "controls": [], "curves": []}),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    let err = &body["error"];
    assert_eq!(err["code"], "validation_error");
    assert!(
        !err["message"].as_str().unwrap_or("").is_empty(),
        "error.message must be a non-empty string: {body}"
    );
    assert_eq!(err["retryable"], false);
    assert_eq!(err["source"], "validation");
}

/// Second `Json<serde_json::Value>` handler on the same semantic-400 path:
/// activation with neither `profile_id` nor `profile_path` hits
/// activate_profile_handler's missing-selector branch → same envelope.
#[tokio::test]
async fn profile_activate_missing_selector_returns_validation_envelope() {
    let (sock, _tx, _d) = start_test_server(test_app_state()).await;
    let (st, body) = uds_post(&sock, "/profile/activate", &serde_json::json!({})).await;
    assert_eq!(st, 400, "{body}");
    let err = &body["error"];
    assert_eq!(err["code"], "validation_error");
    assert!(
        !err["message"].as_str().unwrap_or("").is_empty(),
        "error.message must be a non-empty string: {body}"
    );
    assert_eq!(err["retryable"], false);
    assert_eq!(err["source"], "validation");
}

#[tokio::test]
async fn profile_validate_only_persists_nothing_but_still_rejects_invalid() {
    let (state, store) = state_with_temp_store();
    let (sock, _tx, _sock_tmp) = start_test_server(state).await;

    // Valid + validate_only → 200, nothing written.
    let (st, body) = uds_send(
        &sock,
        "POST",
        "/profiles?validate_only=true",
        Some(&valid_profile("dry")),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["valid"], true);
    assert!(
        !store.path().join("dry.json").exists(),
        "validate_only must not persist"
    );

    // Invalid + validate_only → 400 (AIP-163: fails exactly when a real one would).
    let bad = serde_json::json!({
        "id": "dry2", "name": "Bad", "version": 7, "controls": [],
        "curves": [{"id": "fc", "name": "F", "type": "flat", "flat_output_pct": 140.0}],
    });
    let (st, _) = uds_send(&sock, "POST", "/profiles?validate_only=true", Some(&bad)).await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn profile_put_id_mismatch_rejected() {
    let (state, _store) = state_with_temp_store();
    let (sock, _tx, _sock_tmp) = start_test_server(state).await;

    // Body id "realid" but path id "otherid" → 400.
    let (st, body) = uds_send(
        &sock,
        "PUT",
        "/profiles/otherid",
        Some(&valid_profile("realid")),
    )
    .await;
    assert_eq!(st, 400);
    assert_eq!(body["error"]["code"], "validation_error");
}

#[tokio::test]
async fn delete_active_profile_returns_409() {
    // Mark a profile active in-memory directly (no activate → no /var/lib write).
    let (state, _store) = state_with_temp_store();
    *state.active_profile.lock() = Some(DaemonProfile {
        id: "p1".into(),
        name: "P1".into(),
        version: 7,
        description: String::new(),
        controls: vec![],
        curves: vec![],
    });
    let (sock, _tx, _sock_tmp) = start_test_server(state).await;

    let (st, body) = uds_send(&sock, "DELETE", "/profiles/p1", None).await;
    assert_eq!(st, 409);
    assert_eq!(body["error"]["code"], "profile_in_use");
}

#[tokio::test]
async fn profiles_persist_across_restart() {
    // The store dir survives a daemon restart: a fresh AppState over the same
    // dir still lists a previously-created profile.
    let store = tempfile::tempdir().unwrap();

    let state_a = test_app_state();
    state_a
        .profile_search_dirs
        .write()
        .push(store.path().to_path_buf());
    let (sock_a, tx_a, _tmp_a) = start_test_server(state_a).await;
    let (st, _) = uds_send(
        &sock_a,
        "POST",
        "/profiles",
        Some(&valid_profile("persist")),
    )
    .await;
    assert_eq!(st, 201);
    let _ = tx_a.send(()); // stop server A

    let state_b = test_app_state();
    state_b
        .profile_search_dirs
        .write()
        .push(store.path().to_path_buf());
    let (sock_b, _tx_b, _tmp_b) = start_test_server(state_b).await;
    let (st, body) = uds_get(&sock_b, "/profiles").await;
    assert_eq!(st, 200);
    let ids: Vec<&str> = body["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"persist"),
        "store must survive restart: {ids:?}"
    );
}

#[tokio::test]
async fn capabilities_advertises_profile_storage() {
    let (state, _store) = state_with_temp_store();
    let (sock, _tx, _sock_tmp) = start_test_server(state).await;
    let (st, body) = uds_get(&sock, "/capabilities").await;
    assert_eq!(st, 200);
    assert_eq!(body["control"]["profile_storage"], true);
    assert_eq!(body["control"]["curve_evaluation"], true);
    // DEC-163 / DEC-166: the override + identify APIs land in 1.21.0.
    assert_eq!(body["control"]["manual_override"], true);
    assert_eq!(body["control"]["fan_identify"], true);
    // DEC-165 (2.0.0 flip): the daemon is the sole authoritative writer and
    // advertises the version floor that powers the GUI's safety gate.
    assert_eq!(body["control"]["autonomous_control"], true);
    assert_eq!(body["control"]["min_supported_gui"], "2.0.0");
}

#[tokio::test]
async fn hwmon_verify_rejects_concurrent_with_409() {
    // DEC-165 single-flight: while one hardware verify holds the slot, a second
    // verify must be rejected with 409 rather than clobbering the first's engine
    // pause / "verify" lease. Pre-occupy the slot as if a verify were in flight.
    let state = test_app_state_with_hwmon();
    assert!(state
        .cache
        .try_begin_verify(std::time::Duration::from_secs(30)));
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(&path, "/hwmon/h1/verify", &serde_json::json!({})).await;
    assert_eq!(status, 409, "concurrent verify must be rejected: {json}");
    assert_eq!(json["error"]["code"], "validation_error");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn retired_write_and_lease_endpoints_return_404() {
    // DEC-165: the bare PWM-write + lease endpoints were retired from the
    // contract at 2.0.0. They must now hit the fallback handler (404), proving
    // a stray old-GUI write cannot silently succeed against a new daemon.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    for ep in [
        "/fans/openfan/0/pwm",
        "/fans/openfan/pwm",
        "/fans/openfan/0/target_rpm",
        "/hwmon/h1/pwm",
        "/gpu/0000:03:00.0/fan/pwm",
        "/hwmon/lease/take",
        "/hwmon/lease/release",
        "/hwmon/lease/renew",
    ] {
        let (status, json) = uds_post(&path, ep, &serde_json::json!({})).await;
        assert_eq!(status, 404, "retired endpoint {ep} must 404: {json}");
        assert_eq!(json["error"]["code"], "not_found", "endpoint {ep}");
    }
    // GET /hwmon/lease/status is also retired.
    let (status, _) = uds_get(&path, "/hwmon/lease/status").await;
    assert_eq!(status, 404, "retired GET /hwmon/lease/status must 404");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── Manual override + fan identify API (DEC-163 / DEC-166) ──────────────

/// `test_app_state()` plus an active profile carrying one (member-less) control,
/// which is all the override take-path needs to validate the control id.
fn app_state_with_control(control_id: &str) -> Arc<AppState> {
    let state = test_app_state();
    {
        let mut guard = state.active_profile.lock();
        *guard = Some(control_ofc_daemon::profile::DaemonProfile {
            id: "p".into(),
            name: "P".into(),
            version: 7,
            description: String::new(),
            controls: vec![control_ofc_daemon::profile::LogicalControl {
                id: control_id.into(),
                name: control_id.into(),
                mode: "curve".into(),
                curve_id: "c".into(),
                manual_output_pct: 0.0,
                members: Vec::new(),
                step_up_pct: 100.0,
                step_down_pct: 100.0,
                offset_pct: 0.0,
                minimum_pct: 0.0,
                start_pct: 0.0,
                stop_pct: 0.0,
            }],
            curves: Vec::new(),
        });
    }
    state
}

#[tokio::test]
async fn override_take_renew_release_roundtrip() {
    let (sock, _tx, _d) = start_test_server(app_state_with_control("ctrl1")).await;

    // Take
    let (st, body) = uds_post(
        &sock,
        "/control/ctrl1/override",
        &serde_json::json!({"pwm_percent": 80}),
    )
    .await;
    assert_eq!(st, 200, "take: {body}");
    assert_eq!(body["pwm_percent"], 80);
    assert_eq!(body["ttl_secs"], 15);
    assert_eq!(body["renew_secs"], 5);
    let token = body["override_token"].as_u64().unwrap();

    // /status surfaces it (poll-authoritative).
    let (_st, status) = uds_get(&sock, "/status").await;
    assert_eq!(status["overrides"][0]["control_id"], "ctrl1");
    assert_eq!(status["overrides"][0]["pwm_percent"], 80);

    // Renew with the current token extends the deadman.
    let (st, body) = uds_post(
        &sock,
        "/control/ctrl1/override/renew",
        &serde_json::json!({"override_token": token}),
    )
    .await;
    assert_eq!(st, 200, "renew: {body}");
    assert_eq!(body["override_token"], token);

    // Release reverts to curve immediately.
    let (st, body) = uds_send(
        &sock,
        "DELETE",
        "/control/ctrl1/override",
        Some(&serde_json::json!({"override_token": token})),
    )
    .await;
    assert_eq!(st, 200, "release: {body}");
    assert_eq!(body["released"], true);

    // Gone from /status (omitted when empty).
    let (_st, status) = uds_get(&sock, "/status").await;
    assert!(status.get("overrides").is_none());
}

#[tokio::test]
async fn override_stale_token_rejected_with_409() {
    let (sock, _tx, _d) = start_test_server(app_state_with_control("ctrl1")).await;
    let (_s, b1) = uds_post(
        &sock,
        "/control/ctrl1/override",
        &serde_json::json!({"pwm_percent": 50}),
    )
    .await;
    let stale = b1["override_token"].as_u64().unwrap();
    // A second take supersedes the first token.
    let _ = uds_post(
        &sock,
        "/control/ctrl1/override",
        &serde_json::json!({"pwm_percent": 60}),
    )
    .await;

    let (st, body) = uds_post(
        &sock,
        "/control/ctrl1/override/renew",
        &serde_json::json!({"override_token": stale}),
    )
    .await;
    assert_eq!(st, 409, "stale renew must be rejected: {body}");
    assert_eq!(body["error"]["code"], "stale_fencing_token");

    let (st, body) = uds_send(
        &sock,
        "DELETE",
        "/control/ctrl1/override",
        Some(&serde_json::json!({"override_token": stale})),
    )
    .await;
    assert_eq!(st, 409, "stale release must be rejected: {body}");
    assert_eq!(body["error"]["code"], "stale_fencing_token");
}

#[tokio::test]
async fn override_unknown_control_is_404() {
    let (sock, _tx, _d) = start_test_server(app_state_with_control("ctrl1")).await;
    let (st, body) = uds_post(
        &sock,
        "/control/nope/override",
        &serde_json::json!({"pwm_percent": 50}),
    )
    .await;
    assert_eq!(st, 404, "{body}");
    assert_eq!(body["error"]["code"], "validation_error");
}

#[tokio::test]
async fn override_pwm_out_of_range_is_400() {
    let (sock, _tx, _d) = start_test_server(app_state_with_control("ctrl1")).await;
    let (st, body) = uds_post(
        &sock,
        "/control/ctrl1/override",
        &serde_json::json!({"pwm_percent": 150}),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    assert_eq!(body["error"]["code"], "validation_error");
}

#[tokio::test]
async fn override_error_envelope_shape_complete() {
    // Pin the FULL error envelope (not just `code`) on a representative override
    // error path, so a change to the {code, message, retryable, source} contract
    // documented in docs/08 can't slip through with green CI.
    let (sock, _tx, _d) = start_test_server(app_state_with_control("ctrl1")).await;
    let (st, body) = uds_post(
        &sock,
        "/control/ctrl1/override",
        &serde_json::json!({"pwm_percent": 150}),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    let err = &body["error"];
    assert_eq!(err["code"], "validation_error");
    assert!(
        !err["message"].as_str().unwrap_or("").is_empty(),
        "error.message must be a non-empty string: {body}"
    );
    assert_eq!(err["retryable"], false);
    assert_eq!(err["source"], "validation");
}

#[tokio::test]
async fn override_renew_unknown_control_is_404() {
    let (sock, _tx, _d) = start_test_server(app_state_with_control("ctrl1")).await;
    let (st, body) = uds_post(
        &sock,
        "/control/ctrl1/override/renew",
        &serde_json::json!({"override_token": 999}),
    )
    .await;
    assert_eq!(st, 404, "{body}");
    assert_eq!(body["error"]["code"], "override_expired");
}

#[tokio::test]
async fn activate_profile_clears_standing_overrides_and_gpu_relinquish() {
    // DEC-189 + audit P3-4: activating a profile must clear any standing manual
    // override (so an override taken against the previous profile cannot bleed
    // onto a same-id control in the newly-activated one) AND clear any GPU fans
    // previously relinquished to firmware-auto. Drives the real activate +
    // override handlers end-to-end on the HEADLESS path — there is no GUI
    // `_release_all_overrides()` here, so the daemon must self-scope.
    let tmp = tempfile::tempdir().unwrap();
    let search_dir = tmp.path().join("search");
    std::fs::create_dir_all(&search_dir).unwrap();
    let profile_path = search_dir.join("p.json");
    // Minimal profile that validates: a control "cpu" bound to a flat curve, so
    // the override take-path finds the control in the active profile.
    std::fs::write(
        &profile_path,
        r#"{"id":"p","name":"P","version":7,
            "controls":[{"id":"cpu","name":"CPU","curve_id":"c1"}],
            "curves":[{"id":"c1","name":"C1","type":"flat","flat_output_pct":50}]}"#,
    )
    .unwrap();

    let state = test_app_state_with_profile_dirs(vec![search_dir.clone()]);
    let (sock, shutdown, _d) = start_test_server(state.clone()).await;
    let activate = serde_json::json!({ "profile_path": profile_path.display().to_string() });

    // Activate, pin "cpu" with a manual override, and relinquish a GPU fan.
    let (st, j) = uds_post(&sock, "/profile/activate", &activate).await;
    assert_eq!(st, 200, "first activate: {j}");
    let (st, j) = uds_post(
        &sock,
        "/control/cpu/override",
        &serde_json::json!({"pwm_percent": 80}),
    )
    .await;
    assert_eq!(st, 200, "override take: {j}");
    state.cache.relinquish_gpu_fan("amd_gpu:0000:03:00.0");

    // Sanity: /status surfaces the live override and the fan is relinquished.
    let (_st, status) = uds_get(&sock, "/status").await;
    assert_eq!(status["overrides"][0]["control_id"], "cpu");
    assert!(state.cache.is_gpu_fan_relinquished("amd_gpu:0000:03:00.0"));

    // Re-activate the SAME profile id (the DEC-188 "edit the active curve and
    // re-apply" path — exactly the case the GUI cannot self-heal headlessly).
    let (st, j) = uds_post(&sock, "/profile/activate", &activate).await;
    assert_eq!(st, 200, "re-activate: {j}");

    // The override is gone from /status (cleared on activation) and the GPU fan
    // is no longer relinquished.
    let (_st, status) = uds_get(&sock, "/status").await;
    assert!(
        status.get("overrides").is_none(),
        "activation must clear standing overrides, got {status}"
    );
    assert!(
        !state.cache.is_gpu_fan_relinquished("amd_gpu:0000:03:00.0"),
        "activation must clear relinquished GPU fans"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn fan_identify_stop_then_restore() {
    let (sock, _tx, _d) = start_test_server(test_app_state()).await;

    // Stop a known fan (openfan:ch00 is populated by test_app_state).
    let (st, body) = uds_post(
        &sock,
        "/fans/openfan:ch00/identify",
        &serde_json::json!({"action": "stop"}),
    )
    .await;
    assert_eq!(st, 200, "stop: {body}");
    assert_eq!(body["action"], "stop");
    assert!(body["expires_in_secs"].as_u64().unwrap() <= 15);

    let (_st, status) = uds_get(&sock, "/status").await;
    assert_eq!(status["fan_identify"][0]["fan_id"], "openfan:ch00");

    // Restore.
    let (st, body) = uds_post(
        &sock,
        "/fans/openfan:ch00/identify",
        &serde_json::json!({"action": "restore"}),
    )
    .await;
    assert_eq!(st, 200, "restore: {body}");
    assert_eq!(body["action"], "restore");

    let (_st, status) = uds_get(&sock, "/status").await;
    assert!(status.get("fan_identify").is_none());
}

#[tokio::test]
async fn fan_identify_unknown_fan_is_404() {
    let (sock, _tx, _d) = start_test_server(test_app_state()).await;
    let (st, body) = uds_post(
        &sock,
        "/fans/nope:fan/identify",
        &serde_json::json!({"action": "stop"}),
    )
    .await;
    assert_eq!(st, 404, "{body}");
    // Pin the error code, not just the status — a client (or docs/08) relies on
    // `validation_error` for an unknown fan id.
    assert_eq!(body["error"]["code"], "validation_error");
}

#[tokio::test]
async fn fan_identify_bad_action_is_400() {
    let (sock, _tx, _d) = start_test_server(test_app_state()).await;
    let (st, body) = uds_post(
        &sock,
        "/fans/openfan:ch00/identify",
        &serde_json::json!({"action": "wiggle"}),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    assert_eq!(body["error"]["code"], "validation_error");
}

#[tokio::test]
async fn create_profile_rejects_overlong_id() {
    // DEC-173: an over-long id is rejected with a clean 400 at the safety gate,
    // BEFORE it reaches the filesystem — where it would otherwise surface as an
    // opaque 500 ENAMETOOLONG when `{id}.json` is written. The id check fires
    // ahead of any store-dir need, so no store has to be configured here.
    let (sock, _tx, _d) = start_test_server(test_app_state()).await;
    let overlong = "a".repeat(129); // one byte over MAX_PROFILE_ID_BYTES (128)
    let (st, body) = uds_post(&sock, "/profiles", &serde_json::json!({ "id": overlong })).await;
    assert_eq!(st, 400, "{body}");
    assert_eq!(body["error"]["code"], "validation_error");
}

#[tokio::test]
async fn save_profile_failure_message_omits_internal_path() {
    // DEC-173: a save failure must not leak the internal store path in the
    // client envelope — the path-bearing detail goes to the server log only.
    // Force the failure root-independently: the store dir's parent is a regular
    // FILE, so create_dir fails with ENOTDIR even when the suite runs as root (a
    // 0o500 dir would not stop root).
    let tmp = tempfile::tempdir().unwrap();
    let not_a_dir = tmp.path().join("not-a-dir");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let store = not_a_dir.join("store"); // parent is a file → create_dir_private errors
    let state = test_app_state_with_profile_dirs(vec![store]);
    let (sock, _tx, _d) = start_test_server(state).await;

    // Minimal profile with no sensor refs — validates clean against the fixture's
    // empty sensor set, so the request reaches the save path (not a 400).
    let body = serde_json::json!({
        "id": "p", "name": "P", "description": "", "version": 7,
        "controls": [], "curves": []
    });
    let (st, resp) = uds_post(&sock, "/profiles", &body).await;

    assert_eq!(st, 500, "{resp}");
    assert_eq!(resp["error"]["code"], "internal_error");
    let msg = resp["error"]["message"].as_str().unwrap();
    assert_eq!(msg, "failed to save profile");
    assert!(
        !msg.contains('/'),
        "client message must not leak a path: {msg}"
    );
}
