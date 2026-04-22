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
