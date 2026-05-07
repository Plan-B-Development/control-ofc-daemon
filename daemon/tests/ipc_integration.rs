//! Integration tests for the IPC server over Unix domain socket.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use control_ofc_daemon::api::handlers::AppState;
use control_ofc_daemon::api::server;
use control_ofc_daemon::error::SerialError;
use control_ofc_daemon::health::cache::StateCache;
use control_ofc_daemon::health::history::HistoryRing;
use control_ofc_daemon::health::staleness::StalenessConfig;
use control_ofc_daemon::health::state::{CachedSensorReading, DeviceLabel, OpenFanState};
use control_ofc_daemon::hwmon::lease::LeaseManager;
use control_ofc_daemon::hwmon::pwm_control::{HwmonPwmController, SysfsWriter};
use control_ofc_daemon::hwmon::pwm_discovery::PwmHeaderDescriptor;
use control_ofc_daemon::hwmon::types::SensorKind;
use control_ofc_daemon::serial::controller::FanController;
use control_ofc_daemon::serial::transport::SerialTransport;

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
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
    assert!(json["overall_status"].is_string());
    assert!(json["subsystems"].is_array());
    assert!(json["counters"].is_object());

    let _ = shutdown.send(());
    // Clean up socket
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
    assert!(status_obj["overall_status"].is_string());
    assert!(status_obj["subsystems"].is_array());
    assert!(status_obj["counters"].is_object());

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

/// Mock transport that accepts writes and returns canned OK responses in FIFO order.
struct IntegrationMockTransport {
    responses: Mutex<std::collections::VecDeque<Result<String, SerialError>>>,
}

impl IntegrationMockTransport {
    fn with_ok_responses(count: usize) -> Self {
        let responses = (0..count)
            .map(|_| Ok("<02|00:0400;>\r\n".to_string()))
            .collect();
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl SerialTransport for IntegrationMockTransport {
    fn write_line(&mut self, _data: &str) -> Result<(), SerialError> {
        Ok(())
    }

    fn read_line(&mut self, _timeout: Duration) -> Result<String, SerialError> {
        self.responses
            .lock()
            .pop_front()
            .unwrap_or(Err(SerialError::Timeout { timeout_ms: 500 }))
    }
}

/// Helper: create AppState with a mock FanController.
fn test_app_state_with_controller(response_count: usize) -> Arc<AppState> {
    let cache = Arc::new(StateCache::new());
    let transport = IntegrationMockTransport::with_ok_responses(response_count);
    let controller = FanController::new(
        Box::new(transport),
        cache.clone(),
        Duration::from_millis(500),
    );

    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: Some(Arc::new(Mutex::new(controller))),
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        amd_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    })
}

#[tokio::test]
async fn set_pwm_single_channel() {
    let state = test_app_state_with_controller(5);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "pwm_percent": 50 });
    let (status, json) = uds_post(&path, "/fans/openfan/0/pwm", &body).await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["channel"], 0);
    assert_eq!(json["pwm_percent"], 50);
    assert_eq!(json["coalesced"], false);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn set_pwm_all_channels() {
    let state = test_app_state_with_controller(5);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "pwm_percent": 75 });
    let (status, json) = uds_post(&path, "/fans/openfan/pwm", &body).await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["pwm_percent"], 75);
    assert_eq!(json["channels_affected"], 10);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn set_pwm_invalid_channel() {
    let state = test_app_state_with_controller(5);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "pwm_percent": 50 });
    let (status, json) = uds_post(&path, "/fans/openfan/99/pwm", &body).await;

    assert_eq!(status, 400);
    assert_eq!(json["error"]["code"], "validation_error");
    assert_eq!(json["error"]["retryable"], false);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn set_pwm_invalid_percent() {
    let state = test_app_state_with_controller(5);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "pwm_percent": 200 });
    let (status, json) = uds_post(&path, "/fans/openfan/0/pwm", &body).await;

    assert_eq!(status, 400);
    assert_eq!(json["error"]["code"], "validation_error");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn set_pwm_no_controller_returns_unavailable() {
    let state = test_app_state(); // no fan_controller
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "pwm_percent": 50 });
    let (status, json) = uds_post(&path, "/fans/openfan/0/pwm", &body).await;

    assert_eq!(status, 503);
    assert_eq!(json["error"]["code"], "hardware_unavailable");
    assert_eq!(json["error"]["retryable"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn set_target_rpm_single_channel() {
    let state = test_app_state_with_controller(5);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "target_rpm": 1200 });
    let (status, json) = uds_post(&path, "/fans/openfan/0/target_rpm", &body).await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["channel"], 0);
    assert_eq!(json["target_rpm"], 1200);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
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
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
async fn hwmon_lease_take_and_release() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Take lease
    let body = serde_json::json!({ "owner_hint": "test-gui" });
    let (status, json) = uds_post(&path, "/hwmon/lease/take", &body).await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert!(json["lease_id"].is_string());
    assert_eq!(json["owner_hint"], "test-gui");
    assert!(json["ttl_seconds"].as_u64().unwrap() > 0);

    let lease_id = json["lease_id"].as_str().unwrap().to_string();

    // Release lease
    let body = serde_json::json!({ "lease_id": lease_id });
    let (status, json) = uds_post(&path, "/hwmon/lease/release", &body).await;

    assert_eq!(status, 200);
    assert_eq!(json["released"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hwmon_lease_take_conflict() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // First take succeeds
    let body = serde_json::json!({ "owner_hint": "gui-1" });
    let (status, _) = uds_post(&path, "/hwmon/lease/take", &body).await;
    assert_eq!(status, 200);

    // Second take succeeds (force_take preempts — GUI always wins)
    let body = serde_json::json!({ "owner_hint": "gui-2" });
    let (status, json) = uds_post(&path, "/hwmon/lease/take", &body).await;

    assert_eq!(status, 200);
    assert_eq!(json["owner_hint"], "gui-2");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hwmon_set_pwm_with_lease() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Take lease
    let body = serde_json::json!({ "owner_hint": "gui" });
    let (_, lease_json) = uds_post(&path, "/hwmon/lease/take", &body).await;
    let lease_id = lease_json["lease_id"].as_str().unwrap();

    // Set PWM
    let body = serde_json::json!({ "pwm_percent": 60, "lease_id": lease_id });
    let (status, json) = uds_post(&path, "/hwmon/h1/pwm", &body).await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["header_id"], "h1");
    assert_eq!(json["pwm_percent"], 60);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hwmon_set_pwm_without_lease() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "pwm_percent": 50, "lease_id": "invalid" });
    let (status, json) = uds_post(&path, "/hwmon/h1/pwm", &body).await;

    assert_eq!(status, 403);
    assert_eq!(json["error"]["code"], "lease_required");

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
    assert_eq!(json["features"]["lease_required_for_hwmon_writes"], true);
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
    assert_eq!(json["devices"]["hwmon"]["lease_required"], true);
    assert_eq!(json["features"]["hwmon_write_supported"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── Lease status/renew integration tests ─────────────────────────────

#[tokio::test]
async fn lease_status_no_lease() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/hwmon/lease/status").await;

    assert_eq!(status, 200);
    assert_eq!(json["lease_required"], true);
    assert_eq!(json["held"], false);
    assert!(json.get("lease_id").is_none());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn lease_status_with_active_lease() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Take lease
    let body = serde_json::json!({ "owner_hint": "gui" });
    let (_, lease_json) = uds_post(&path, "/hwmon/lease/take", &body).await;
    let lease_id = lease_json["lease_id"].as_str().unwrap();

    // Check status
    let (status, json) = uds_get(&path, "/hwmon/lease/status").await;

    assert_eq!(status, 200);
    assert_eq!(json["held"], true);
    assert_eq!(json["lease_id"], lease_id);
    assert_eq!(json["owner_hint"], "gui");
    assert!(json["ttl_seconds_remaining"].as_u64().unwrap() > 0);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn lease_renew_extends_ttl() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Take lease
    let body = serde_json::json!({ "owner_hint": "gui" });
    let (_, lease_json) = uds_post(&path, "/hwmon/lease/take", &body).await;
    let lease_id = lease_json["lease_id"].as_str().unwrap();

    // Renew
    let body = serde_json::json!({ "lease_id": lease_id });
    let (status, json) = uds_post(&path, "/hwmon/lease/renew", &body).await;

    assert_eq!(status, 200);
    assert_eq!(json["lease_id"], lease_id);
    assert!(json["ttl_seconds"].as_u64().unwrap() > 55);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn lease_renew_invalid_id_fails() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "lease_id": "bogus" });
    let (status, json) = uds_post(&path, "/hwmon/lease/renew", &body).await;

    assert_eq!(status, 400);
    assert_eq!(json["error"]["code"], "lease_required");

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
async fn gpu_set_fan_unknown_gpu_returns_404() {
    // Sanity: unknown GPU id remains 404 (that IS a validation error — the
    // endpoint exists, the caller's id doesn't).
    let state = test_app_state(); // amd_gpus empty
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "speed_pct": 50 });
    let (status, json) = uds_post(&path, "/gpu/0000:99:00.0/fan/pwm", &body).await;

    assert_eq!(status, 404);
    assert_eq!(json["error"]["code"], "validation_error");

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
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    })
}

#[tokio::test]
async fn gpu_set_fan_unsupported_returns_400_feature_unavailable() {
    // P1-1: when a GPU exists but has no fan write path (no PMFW fan_curve,
    // no legacy pwm1), the handler previously returned 400 hardware_unavailable
    // + retryable:true — a contract violation (hardware_unavailable is a 503
    // code, and the condition is permanent so not retryable).
    //
    // The fix is a dedicated `feature_unavailable` code (400, retryable:false,
    // source "validation") to distinguish "this device can't do this" from
    // "hardware failed transiently".
    let bdf = "0000:99:00.0";
    let state = test_app_state_with_unsupported_gpu(bdf);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "speed_pct": 50 });
    let (status, json) = uds_post(&path, &format!("/gpu/{bdf}/fan/pwm"), &body).await;

    assert_eq!(status, 400);
    assert_eq!(json["error"]["code"], "feature_unavailable");
    assert_eq!(json["error"]["retryable"], false);
    assert_eq!(json["error"]["source"], "validation");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
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
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    })
}

#[tokio::test]
async fn gpu_set_fan_read_only_rdna_returns_400_feature_unavailable() {
    // DEC-098: the legacy-PWM dispatch arm previously gated on `gpu.has_pwm`
    // alone. RDNA3/RDNA4 GPUs without overdrive expose `pwm1` read-only and
    // lack `pwm1_enable`, so the handler would attempt to write `pwm1_enable`,
    // fail with ENOENT, and surface 503 hardware_unavailable + retryable:true.
    // The canonical answer is 400 feature_unavailable + retryable:false (DEC-094)
    // and the message must include the `amdgpu.ppfeaturemask=0xffffffff` hint
    // so users on bare RDNA3/4 know how to unlock PMFW.
    let bdf = "0000:03:00.0";
    let state = test_app_state_with_read_only_gpu(bdf, 0x7550); // RX 9070 XT device id
    let (path, shutdown, _dir) = start_test_server(state).await;

    let body = serde_json::json!({ "speed_pct": 50 });
    let (status, json) = uds_post(&path, &format!("/gpu/{bdf}/fan/pwm"), &body).await;

    assert_eq!(status, 400);
    assert_eq!(json["error"]["code"], "feature_unavailable");
    assert_eq!(json["error"]["retryable"], false);
    assert_eq!(json["error"]["source"], "validation");
    let msg = json["error"]["message"].as_str().expect("message string");
    assert!(
        msg.contains("amdgpu.ppfeaturemask=0xffffffff"),
        "expected ppfeaturemask hint in message, got: {msg}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
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
            .take_lease("profile-engine")
            .expect("take should succeed");
        assert_eq!(
            guard.lease_manager().active_lease().unwrap().owner_hint,
            "profile-engine"
        );
    }

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let (status, _json) = uds_post(&path, "/profile/deactivate", &serde_json::json!({})).await;
    assert_eq!(status, 200);

    // Profile-engine lease should now be released — leaving the controller
    // free for a fresh GUI lease without a force-take.
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
async fn deactivate_profile_preserves_gui_lease() {
    // A non-profile-engine lease (e.g. GUI's own lease for manual writes)
    // must NOT be touched by deactivation — the GUI is still in control of
    // hwmon and may want to keep writing PWM directly.
    let state = test_app_state_with_hwmon();
    let gui_lease_id = {
        let ctrl = state.hwmon_controller.as_ref().unwrap();
        let mut guard = ctrl.lock();
        guard
            .lease_manager_mut()
            .take_lease("gui")
            .expect("take should succeed")
            .lease_id
    };

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;
    let (status, _json) = uds_post(&path, "/profile/deactivate", &serde_json::json!({})).await;
    assert_eq!(status, 200);

    // GUI lease unchanged.
    let ctrl = state.hwmon_controller.as_ref().unwrap();
    let guard = ctrl.lock();
    let active = guard
        .lease_manager()
        .active_lease()
        .expect("GUI lease should still be active");
    assert_eq!(active.owner_hint, "gui");
    assert_eq!(active.lease_id, gui_lease_id);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── Audit P2.7: GPU reset records gui_active ────────────────────────────

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
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sse_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    (state, tmp)
}

#[tokio::test]
async fn gpu_reset_fan_records_gui_write() {
    // Audit P2.7 regression: a successful POST /gpu/{id}/fan/reset must
    // call record_gui_write() so the profile engine defers for the
    // GUI_ACTIVITY_TIMEOUT window. Without this, the next 1 Hz profile-engine
    // tick re-asserts the curve and silently undoes the user's reset.
    let bdf = "0000:03:00.0";
    let (state, _tmp) = test_app_state_with_writable_pmfw_gpu(bdf);

    // Pre-condition: gui_active() is false (no prior writes).
    assert!(
        !state.cache.snapshot().gui_active(),
        "precondition: gui_active should start false"
    );

    let (path, shutdown, _dir) = start_test_server(state.clone()).await;
    let (status, json) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;

    assert_eq!(status, 200, "body: {json}");
    assert_eq!(json["reset"], true);

    // Post-condition: gui_active() is now true. The profile engine will
    // skip GPU writes until GUI_ACTIVITY_TIMEOUT (30 s) elapses.
    assert!(
        state.cache.snapshot().gui_active(),
        "reset must record a GUI write so the profile engine defers"
    );

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
