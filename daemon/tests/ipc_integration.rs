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

/// Helper: create AppState with a pre-populated cache, representing a healthy
/// running daemon — fresh poll data *and* a live profile engine.
fn test_app_state() -> Arc<AppState> {
    test_app_state_with_engine(true)
}

/// `test_app_state` with a real `runtime.toml` path, so the `POST /config/*`
/// setters can be exercised end-to-end (DEC-255).
fn test_app_state_with_runtime_config(runtime_cfg: std::path::PathBuf) -> Arc<AppState> {
    test_app_state_inner(true, runtime_cfg)
}

/// `test_app_state`, with control over whether the profile engine has ticked.
///
/// DEC-249 made engine liveness a subsystem, so `overall_status` now depends on
/// it. Pass `false` for the unhealthy shape: a daemon whose engine task has
/// never completed a tick (spawned but dead on arrival), which must not present
/// as healthy however fresh the poll data is.
fn test_app_state_with_engine(engine_ticked: bool) -> Arc<AppState> {
    test_app_state_inner(engine_ticked, std::path::PathBuf::new())
}

fn test_app_state_inner(engine_ticked: bool, runtime_cfg: std::path::PathBuf) -> Arc<AppState> {
    let cache = Arc::new(StateCache::new());

    if engine_ticked {
        cache.record_engine_tick(
            "normal",
            control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
        );
    }

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

    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: runtime_cfg,
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
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
    // DEC-132: the thermal state the engine reported on its last tick. The
    // pre-first-tick default is covered by
    // `status_is_crit_when_engine_has_never_ticked`.
    assert_eq!(json["thermal_state"], "normal");

    let _ = shutdown.send(());
    // Clean up socket
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn status_is_crit_when_engine_has_never_ticked() {
    // DEC-249. The profile engine is the sole PWM writer and runs the thermal-emergency
    // rule, but nothing supervises its task — a panic inside a tick used to end
    // fan control silently while /status kept answering 200 with every
    // subsystem "ok". Engine liveness is now a subsystem of its own, so a
    // daemon whose engine is not ticking cannot report itself healthy no matter
    // how fresh the poll data is.
    let state = test_app_state_with_engine(false);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/status").await;

    assert_eq!(
        status, 200,
        "a dead engine is reported, not a failed request"
    );
    // The poll subsystems are fresh — only the engine is not.
    assert_eq!(json["subsystems"][0]["status"], "ok", "openfan");
    assert_eq!(json["subsystems"][1]["status"], "ok", "hwmon");

    let engine = &json["subsystems"][2];
    assert_eq!(engine["name"], "engine");
    assert_eq!(engine["status"], "crit");
    assert_eq!(engine["reason"], "never ticked");
    assert!(
        engine["age_ms"].is_null(),
        "no age for a subsystem that never reported"
    );

    // The whole point: it must escalate to overall, which is what a client acts on.
    assert_eq!(json["overall_status"], "crit");

    // Unchanged: thermal_state still defaults to "normal" before the first tick
    // (DEC-132) — the default lives in the response builder, not the heartbeat.
    assert_eq!(json["thermal_state"], "normal");

    let _ = shutdown.send(());
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

    cache.record_engine_tick(
        "emergency",
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
    );
    let (status, json) = uds_get(&path, "/status").await;
    assert_eq!(status, 200);
    assert_eq!(json["thermal_state"], "emergency");

    cache.record_engine_tick(
        "recovery",
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
    );
    let (_, json) = uds_get(&path, "/status").await;
    assert_eq!(json["thermal_state"], "recovery");

    cache.record_engine_tick(
        "normal",
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
    );
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
async fn status_and_poll_surface_readiness_rollup() {
    // DEC-206: the compact readiness rollup is mirrored onto /status and /poll for
    // the GUI Dashboard health chip. It is OMITTED until the rollup is cached
    // (older daemon, or before the startup seed task runs), keeping the additive
    // wire shape unchanged — a client treats an absent key as "no rollup" and
    // hides the chip. Once cached, it rides both surfaces (same StatusResponse).
    use control_ofc_daemon::hwmon::readiness::{ReadinessRollup, ReadinessSeverity};
    let state = test_app_state();
    let rollup_slot = state.readiness_rollup.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Default: no cached rollup → key absent on /status and /poll's status.
    let (status, json) = uds_get(&path, "/status").await;
    assert_eq!(status, 200);
    assert!(
        json.get("readiness").is_none(),
        "readiness rollup must be omitted when not yet cached"
    );
    let (_, poll) = uds_get(&path, "/poll").await;
    assert!(poll["status"].get("readiness").is_none());

    // Cache a rollup → it rides both surfaces with counts + the top item.
    *rollup_slot.lock() = Some(ReadinessRollup {
        overall: ReadinessSeverity::Warning,
        critical: 0,
        warning: 1,
        info: 0,
        top_summary: Some("No motherboard PWM fan controls detected".into()),
        top_code: Some("no_pwm_controls".into()),
    });
    let (_, json) = uds_get(&path, "/status").await;
    assert_eq!(json["readiness"]["overall"], "warning");
    assert_eq!(json["readiness"]["warning"], 1);
    assert_eq!(json["readiness"]["top_code"], "no_pwm_controls");
    let (_, poll) = uds_get(&path, "/poll").await;
    assert_eq!(poll["status"]["readiness"]["overall"], "warning");
    assert_eq!(poll["status"]["readiness"]["top_code"], "no_pwm_controls");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn readiness_get_writes_through_to_poll_rollup() {
    // DEC-206: GET /inventory/readiness caches the rollup as a side-effect, so the
    // next /poll carries a `readiness` whose `overall` matches the full list — the
    // write-through path (the daemon never recomputes readiness on the poll itself).
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Before any readiness GET the rollup is not cached → absent on /poll.
    let (_, poll) = uds_get(&path, "/poll").await;
    assert!(poll["status"].get("readiness").is_none());

    // GET the full readiness list — this side-effect-caches the rollup.
    let (status, readiness) = uds_get(&path, "/inventory/readiness").await;
    assert_eq!(status, 200);
    let overall = readiness["overall"]
        .as_str()
        .expect("overall present")
        .to_string();

    // The next /poll now carries the rollup, with the same overall severity.
    let (_, poll) = uds_get(&path, "/poll").await;
    assert_eq!(poll["status"]["readiness"]["overall"], overall);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hardware_readiness_endpoint_returns_combined_snapshot() {
    // DEC-207: the combined endpoint returns the readiness list + rollup + the
    // Super-I/O report from ONE shared scan, so the merged GUI page fetches
    // everything in one request. Only the machine-independent shape is asserted.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/inventory/hardware-readiness").await;
    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    // The readiness half: rollup + overall + items.
    let overall = json["overall"].as_str().expect("overall present");
    assert_eq!(json["rollup"]["overall"], overall);
    assert!(json["items"].is_array());
    // The Super-I/O half: the passive report (arch_supported is a bool on any host;
    // chips is always an array — host-dependent contents, stable shape).
    assert!(json["superio"]["arch_supported"].is_boolean());
    assert!(json["superio"]["chips"].is_array());
    // Freshness + generation (a fresh scan ran, so generation >= 1).
    assert!(json["scanned_age_ms"].is_u64());
    assert!(json["generation"].as_u64().expect("generation") >= 1);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hardware_readiness_writes_through_to_poll_rollup() {
    // DEC-207: the combined endpoint (like /inventory/readiness) caches the rollup,
    // so the next /poll carries a `readiness` whose `overall` matches the snapshot.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, combined) = uds_get(&path, "/inventory/hardware-readiness").await;
    assert_eq!(status, 200);
    let overall = combined["overall"].as_str().expect("overall").to_string();

    let (_, poll) = uds_get(&path, "/poll").await;
    assert_eq!(poll["status"]["readiness"]["overall"], overall);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn combined_endpoint_refresh_bumps_generation_and_serves_cached_within_ttl() {
    // DEC-207: `?refresh=true` forces a new scan (higher generation); a plain GET
    // within the coalescing TTL reuses that scan (same generation), so opening the
    // page then hitting Refresh scans exactly once more.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (_, a) = uds_get(&path, "/inventory/hardware-readiness").await;
    let g1 = a["generation"].as_u64().expect("g1");

    let (_, b) = uds_get(&path, "/inventory/hardware-readiness?refresh=true").await;
    let g2 = b["generation"].as_u64().expect("g2");
    assert!(
        g2 > g1,
        "forced refresh must bump the generation ({g1} -> {g2})"
    );

    // A plain GET within the 3 s TTL reuses the forced scan (no new generation).
    let (_, c) = uds_get(&path, "/inventory/hardware-readiness").await;
    assert_eq!(
        c["generation"].as_u64().expect("g3"),
        g2,
        "a plain GET within the TTL must reuse the cached scan"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn superio_endpoint_matches_combined_endpoint_superio() {
    // DEC-207: /inventory/superio and the combined endpoint both project the SAME
    // shared assessment's Super-I/O report — they must agree (no cross-endpoint
    // drift, and /inventory/superio reused the combined scan instead of re-scanning).
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (_, combined) = uds_get(&path, "/inventory/hardware-readiness").await;
    let (status, superio) = uds_get(&path, "/inventory/superio").await;
    assert_eq!(status, 200);
    assert_eq!(superio, combined["superio"]);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// [SAFETY] DEC-308 — `/diagnostics/hardware` must report the trip point the
/// ENGINE acted on, not the compile-time constant.
///
/// DEC-292's invariant used to be free: the handler and the rule both read one
/// constant, so they could not disagree. DEC-308 made the trip point per-machine,
/// and the sibling assertion in `hardware_diagnostics_endpoint_returns_report`
/// silently stopped proving anything — it compares the response against a FRESH
/// `ThermalSafetyRule` while the fixture seeds the cache with that same constant,
/// so both sides are 105 and a handler that read the constant directly would pass.
///
/// This is the test that fails for that. It seeds a trip point that is
/// deliberately NOT the constant, which is the only way to tell "reports what the
/// engine acted on" from "reports the constant". Raised by `ofc:security-reviewer`
/// during the DEC-308 review, as a P2 on the tripwire itself.
#[tokio::test]
async fn hardware_diagnostics_reports_the_derived_trip_point_not_the_constant() {
    let derived = control_ofc_daemon::constants::THERMAL_TRIGGER_MAX_C;
    assert_ne!(
        derived,
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
        "precondition: the seeded trip point must differ from the constant, or \
         this test cannot distinguish the two sources"
    );

    let state = test_app_state();
    state.cache.record_engine_tick("normal", derived);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/diagnostics/hardware").await;
    assert_eq!(status, 200);
    assert_eq!(
        json["thermal_safety"]["emergency_threshold_c"], derived,
        "the endpoint must report the trip point the engine acted on; reporting \
         the constant while the engine acts on a derived value is exactly the \
         drift DEC-292 exists to catch, and it is reachable again since DEC-308"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hardware_diagnostics_endpoint_returns_report() {
    // Exercises the spawn_blocking offload path: the handler performs blocking
    // sysfs/procfs reads on the blocking pool and serializes the report.
    //
    // The RELEASE threshold is still a hardcoded constant and so is
    // machine-independent. The EMERGENCY threshold is not, as of DEC-308 — it is
    // derived per machine and published by the engine — so this fixture is
    // machine-independent only because it seeds the cache with the constant
    // itself. The assertions below therefore pin the VALUE here; what pins the
    // handler-to-cache LINK is
    // `hardware_diagnostics_reports_the_derived_trip_point_not_the_constant`.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/diagnostics/hardware").await;

    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert!(json["hwmon"].is_object());
    assert!(json["thermal_safety"].is_object());

    // DEC-292: assert what the endpoint REPORTS equals what the rule ACTS on.
    // These were two bare literals, which pinned the value but not the link — the
    // response built its numbers independently of `ThermalSafetyRule`, so moving
    // the trip point would have left the daemon reporting the old one while
    // acting on the new one, and this test would have gone green against the
    // stale value it had been given.
    let acting = control_ofc_daemon::safety::ThermalSafetyRule::new();
    // NOTE what this does and does not prove since DEC-308. `acting` is a FRESH
    // rule, so `trigger_temp_c()` is the default-constructor value — and this
    // fixture seeds the cache with that same constant, so both sides are the
    // constant and a handler that ignored the cache entirely would still pass.
    // That is the very drift DEC-292 named, so the link is pinned separately by
    // the test below rather than pretended to here.
    assert_eq!(
        json["thermal_safety"]["emergency_threshold_c"],
        acting.trigger_temp_c(),
        "the reported emergency threshold has drifted from the one that acts"
    );
    assert_eq!(
        json["thermal_safety"]["release_threshold_c"],
        acting.release_temp_c(),
        "the reported release threshold has drifted from the one that acts"
    );

    // And a deliberate tripwire on the values themselves, so the trip point
    // cannot be moved silently — a safety threshold change should have to edit a
    // test that says so out loud.
    //
    // This tripwire did its job during the D1 batch: a trial raise to 110 could
    // not land silently, and having to edit this line out loud is what surfaced
    // that 110 is exactly Core Ultra mobile's Tjmax. The raise was withdrawn.
    //
    // DEC-308 then closed `D1-q` by a different route — the trip point is derived
    // per machine from the CPU's own reported ceiling — so what this line pins is
    // now the **floor and fallback**, which is what this fixture exercises (it
    // seeds the cache directly, with no CPU sensor and therefore no ceiling to
    // derive from). Still worth pinning literally, and now more so: a machine that
    // reports nothing usable is the common case, and the derivation is raise-only,
    // so this value is the guaranteed lower bound on every machine.
    //
    // Keep these literal — deriving them from the constants would make them agree
    // with any future change automatically, which is the one thing they exist not
    // to do.
    assert_eq!(json["thermal_safety"]["emergency_threshold_c"], 105.0);
    assert_eq!(json["thermal_safety"]["release_threshold_c"], 80.0);
    // DEC-308's two new safety thresholds get the same treatment, for the same
    // reason: the margin decides whether a healthy part at its ceiling trips, and
    // the cap is the only thing stopping a lying chip from pushing the trigger
    // past the CPU's own THERMTRIP and disabling the emergency outright.
    assert_eq!(control_ofc_daemon::constants::THERMAL_TRIGGER_MARGIN_C, 5.0);
    assert_eq!(control_ofc_daemon::constants::THERMAL_TRIGGER_MAX_C, 115.0);
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
async fn inventory_superio_endpoint_returns_report() {
    // GET /inventory/superio returns the passive detection report (DEC-202). With
    // allow_port_probe=false (the test default) the active probe advertises as
    // unavailable — the passive report itself is always present and structured.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/inventory/superio").await;
    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert!(json["chips"].is_array(), "report carries a chips array");
    assert!(json["arch_supported"].is_boolean());
    assert_eq!(
        json["port_probe_available"], false,
        "opt-in probe is off by default"
    );
    assert!(json["port_probe_reason"].is_string());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn inventory_superio_probe_disabled_by_default() {
    // POST /inventory/superio/probe with allow_port_probe=false must NOT touch a
    // port: it returns 200 with the passive report, port_probe_available=false,
    // and a note explaining the probe did not run (DEC-203).
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(&path, "/inventory/superio/probe", &serde_json::json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(json["port_probe_available"], false);
    let notes = json["notes"].as_array().unwrap();
    // Attribute the skip to the DISABLED CONFIG FLAG specifically (reason mentions
    // allow_port_probe), not a hardware-unavailable open failure: both paths push
    // an "Active port probe not run: {reason}" note, so the reason text is what
    // proves the config gate fired rather than an EACCES on /dev/port.
    assert!(
        notes.iter().any(|n| {
            let s = n.as_str().unwrap_or("");
            s.contains("Active port probe not run") && s.contains("allow_port_probe")
        }),
        "the note must attribute the skip to the disabled config flag: {notes:?}"
    );

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
            duty_pct: None,
            updated_at: Instant::now(),
        },
        AmdGpuFanState {
            id: "intel_gpu:0000:03:00.0".into(),
            rpm: Some(1500),
            last_commanded_pct: None,
            duty_pct: None,
            updated_at: Instant::now(),
        },
    ]);

    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    let state = Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
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
async fn sensors_endpoint_tags_nvidia_gpu_source() {
    // DEC-204: a nouveau-backed NVIDIA GPU temperature reaches /sensors via the
    // normal discovery pipeline (NOT read_nouveau_fan_states) and must serialize
    // source "nvidia_gpu", kind "gpu_temp", chip_name "nouveau".
    let state = test_app_state();
    state.cache.update_sensors(vec![CachedSensorReading {
        id: "nvidia_gpu:0000:01:00.0:temp".into(),
        kind: SensorKind::GpuTemp,
        label: "NVIDIA GPU".into(),
        value_c: 42.0,
        source: DeviceLabel::NvidiaGpu,
        updated_at: Instant::now(),
        rate_c_per_s: None,
        session_min_c: None,
        session_max_c: None,
        chip_name: "nouveau".into(),
        temp_type: None,
        thresholds: None,
    }]);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/sensors").await;
    assert_eq!(status, 200);
    let sensors = json["sensors"].as_array().unwrap();
    let nvidia = sensors
        .iter()
        .find(|s| s["id"] == "nvidia_gpu:0000:01:00.0:temp")
        .expect("nvidia gpu temp present on /sensors");
    assert_eq!(nvidia["source"], "nvidia_gpu");
    assert_eq!(nvidia["kind"], "gpu_temp");
    assert_eq!(nvidia["chip_name"], "nouveau");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn fans_endpoint_serializes_duty_pct_including_zero() {
    // DEC-204: NVML-measured `duty_pct` must serialize even when 0 (only None is
    // skipped by skip_serializing_if) and round-trip other values. Treating 0 as
    // "absent" would hide a genuinely-stopped NVIDIA fan in the GUI.
    let state = test_app_state();
    state.cache.update_gpu_fans(vec![
        AmdGpuFanState {
            id: "nvidia_gpu:0000:01:00.0".into(),
            rpm: Some(0),
            last_commanded_pct: None,
            duty_pct: Some(0),
            updated_at: Instant::now(),
        },
        AmdGpuFanState {
            id: "nvidia_gpu:0000:0a:00.0".into(),
            rpm: Some(1400),
            last_commanded_pct: None,
            duty_pct: Some(33),
            updated_at: Instant::now(),
        },
    ]);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/fans").await;
    assert_eq!(status, 200);
    let fans = json["fans"].as_array().unwrap();
    let zero = fans
        .iter()
        .find(|f| f["id"] == "nvidia_gpu:0000:01:00.0")
        .unwrap();
    assert_eq!(
        zero["duty_pct"], 0,
        "duty_pct=0 must serialize, not be omitted"
    );
    assert_eq!(zero["source"], "nvidia_gpu");
    let some = fans
        .iter()
        .find(|f| f["id"] == "nvidia_gpu:0000:0a:00.0")
        .unwrap();
    assert_eq!(some["duty_pct"], 33);

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

/// Build an AppState carrying one NVIDIA GPU identity for the DEC-204
/// capability/diagnostics boundary tests.
fn test_app_state_with_nvidia_gpu(
    gpu: control_ofc_daemon::hwmon::nvidia::NvidiaGpuIdentity,
) -> Arc<AppState> {
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    Arc::new(AppState {
        cache: Arc::new(StateCache::new()),
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        nvidia_gpus: vec![gpu],
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
        )),
    })
}

#[tokio::test]
async fn capabilities_includes_nvidia_gpu_absent_by_default() {
    // The additive `nvidia_gpu` capability object (DEC-204) must always be
    // present for the GUI parser; with no NVIDIA GPU it reports present:false
    // and — being read-only — never carries a fan_write_supported field.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    let nvidia = &json["devices"]["nvidia_gpu"];
    assert_eq!(nvidia["present"], false);
    assert_eq!(nvidia["fan_control_method"], "none");
    assert_eq!(nvidia["display_label"], "NVIDIA D-GPU");
    assert!(nvidia.get("fan_write_supported").is_none());

    // The diagnostics `nvidia_gpu` block is Option/skip-when-None — with no
    // NVIDIA GPU it must be entirely ABSENT from the wire (not `null`), so an
    // older client sees no unexpected key.
    let (status, diag_json) = uds_get(&path, "/diagnostics/hardware").await;
    assert_eq!(status, 200);
    assert!(diag_json.get("nvidia_gpu").is_none());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capabilities_and_diagnostics_report_nvidia_gpu_when_present() {
    // The handlers must read `state.nvidia_gpus` into both the capability and
    // the diagnostics surfaces (DEC-204), read-only.
    use control_ofc_daemon::hwmon::nvidia::NvidiaGpuIdentity;
    let bdf = "0000:03:00.0";
    let state = test_app_state_with_nvidia_gpu(NvidiaGpuIdentity {
        pci_bdf: bdf.into(),
        driver: "nvidia",
        model_name: Some("NVIDIA GeForce RTX 4080".into()),
        driver_version: Some("565.77".into()),
        has_fan: true,
        fan_rpm_available: false,
    });
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    let cap = &json["devices"]["nvidia_gpu"];
    assert_eq!(cap["present"], true);
    assert_eq!(cap["display_label"], "NVIDIA GeForce RTX 4080");
    // Kernel module name, not the "nvml" library (DEC-204, contract Finding 2).
    assert_eq!(cap["driver"], "nvidia");
    assert_eq!(cap["driver_version"], "565.77");
    assert_eq!(cap["fan_control_method"], "read_only");
    assert_eq!(cap["fan_rpm_available"], false);
    assert_eq!(cap["is_discrete"], true);
    // Both pci_id (legacy alias) and pci_bdf must carry the BDF for the GUI's
    // _coalesce_pci_bdf tolerance (M11) — a dropped alias would be silent.
    assert_eq!(cap["pci_bdf"], bdf);
    assert_eq!(cap["pci_id"], bdf);

    let (status, json) = uds_get(&path, "/diagnostics/hardware").await;
    assert_eq!(status, 200);
    let diag = &json["nvidia_gpu"];
    assert_eq!(diag["pci_bdf"], bdf);
    assert_eq!(diag["pci_id"], bdf);
    assert_eq!(diag["driver"], "nvidia");
    assert_eq!(diag["driver_version"], "565.77");
    assert_eq!(diag["model_name"], "NVIDIA GeForce RTX 4080");
    assert_eq!(diag["fan_control_method"], "read_only");
    assert!(diag["fan_control_note"]
        .as_str()
        .unwrap()
        .contains("read-only"));

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capabilities_nouveau_backed_nvidia_gpu_generic_label() {
    // A nouveau-backed NVIDIA GPU (open driver) at the HTTP boundary: generic
    // label, driver "nouveau", and the None model_name/driver_version must be
    // OMITTED from the wire (skip_serializing_if), not leaked as null.
    use control_ofc_daemon::hwmon::nvidia::NvidiaGpuIdentity;
    let state = test_app_state_with_nvidia_gpu(NvidiaGpuIdentity {
        pci_bdf: "0000:03:00.0".into(),
        driver: "nouveau",
        model_name: None,
        driver_version: None,
        has_fan: true,
        fan_rpm_available: true,
    });
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    let cap = &json["devices"]["nvidia_gpu"];
    assert_eq!(cap["present"], true);
    assert_eq!(cap["display_label"], "NVIDIA D-GPU");
    assert_eq!(cap["driver"], "nouveau");
    assert_eq!(cap["fan_control_method"], "read_only");
    // None identity fields must be absent, not null.
    assert!(cap.get("model_name").is_none());
    assert!(cap.get("driver_version").is_none());

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

/// DELETE with no body — used by `DELETE /diagnostics/characterization`
/// (AIO-MB Phase 3), which carries its arguments in the path alone.
async fn uds_delete(socket_path: &str, path: &str) -> (u16, serde_json::Value) {
    let stream = UnixStream::connect(socket_path).await.unwrap();
    let io = TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = Request::builder()
        .method("DELETE")
        .uri(path)
        .header("host", "localhost")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();

    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let resp_body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&resp_body).unwrap_or(serde_json::Value::Null);

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
        role: control_ofc_daemon::hwmon::roles::HeaderRole::Unknown,
        role_source: control_ofc_daemon::hwmon::roles::RoleSource::None,
        ..Default::default()
    }
}

fn test_app_state_with_hwmon() -> Arc<AppState> {
    test_app_state_with_headers(vec![
        make_test_header("h1", "CHA_FAN1", 20),
        make_test_header("h2", "CPU_FAN", 30),
    ])
}

/// Same fixture, with the header descriptors supplied by the caller.
///
/// Exists so a test can point the sysfs paths somewhere it controls.
/// `make_test_header` hardcodes `/sys/class/hwmon/hwmon0/pwm1`, and although
/// every *write* goes to `HwmonMockWriter`, `read_header_state` reads those
/// paths for real — so on a host where `hwmon0` happens to be a live PWM chip
/// publishing `pwm_enable != 1`, a sweep can take `run_sweep`'s reclaim branch
/// and abort for a reason that has nothing to do with what the test asserts.
fn test_app_state_with_headers(headers: Vec<PwmHeaderDescriptor>) -> Arc<AppState> {
    let cache = Arc::new(StateCache::new());
    let lease_mgr = LeaseManager::new();
    let ctrl =
        HwmonPwmController::new(headers, lease_mgr, Box::new(HwmonMockWriter), cache.clone());

    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: Some(Arc::new(Mutex::new(ctrl))),
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
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
    // TEST-4 (2026-07-21 audit): the GUI's startup gate blocks ALL control
    // against a daemon lacking control.autonomous_control (2.0.0
    // sole-writer). Pin it in this BASELINE (store-less) capabilities shape,
    // not only in the store-enabled test — a regression dropping it from the
    // no-store response would otherwise stay green here while gating every
    // user without a profile store.
    assert_eq!(json["control"]["autonomous_control"], true);
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

/// Seed one CPU sensor above the 85 °C verify/calibrate limit onto a base state.
fn make_hot(state: &Arc<AppState>) {
    state.cache.update_sensors(vec![CachedSensorReading {
        id: "hwmon:k10temp:hot:Tccd".into(),
        kind: SensorKind::CpuTemp,
        label: "Tccd".into(),
        value_c: 95.0, // over CALIBRATION_MAX_TEMP_C (85)
        source: DeviceLabel::Hwmon,
        updated_at: Instant::now(),
        rate_c_per_s: None,
        session_min_c: None,
        session_max_c: None,
        chip_name: "k10temp".into(),
        temp_type: None,
        thresholds: None,
    }]);
}

#[tokio::test]
async fn hwmon_verify_refused_when_hot() {
    // Phase 6 (DEC-201): a fan verify must not run while the system is hot — it
    // pauses the engine's write phase (incl. the thermal force) for its
    // window. A sensor over the 85 °C limit → 409 thermal_abort, before the
    // controller/header is even consulted (a global safety gate).
    let state = test_app_state();
    make_hot(&state);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(&path, "/hwmon/fake:header/verify", &serde_json::json!({})).await;

    assert_eq!(status, 409);
    assert_eq!(json["error"]["code"], "thermal_abort");
    assert_eq!(json["error"]["source"], "hardware");
    assert_eq!(json["error"]["retryable"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn gpu_verify_refused_when_hot() {
    // The GPU verify drives the fan away from its commanded duty, so it must
    // refuse to start while hot (DEC-201). NOT because it suppresses the thermal
    // the thermal force — it does not, and DEC-297 corrected that claim wherever
    // it appeared; `force_all_with_floor` runs before the `verify_active()` gate.
    let state = test_app_state();
    make_hot(&state);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/gpu/0000:99:00.0/fan/verify",
        &serde_json::json!({}),
    )
    .await;

    assert_eq!(status, 409);
    assert_eq!(json["error"]["code"], "thermal_abort");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn verify_refused_while_thermal_safety_is_forcing() {
    // DEC-297 (295-a). `verify_thermal_guard` tested TEMPERATURE only, at 85C.
    // The emergency latches at 105C or higher (per-machine since DEC-308) and
    // releases at <=80C, so the band
    // 80 < T <= 85 passed it while the engine was still forcing every fan — and
    // a verify then drives its target to a test duty against that force.
    //
    // The fixture sits at a NORMAL temperature on purpose: it proves the guard
    // keys on the forced STATE, and could not fail if it keyed on temperature.
    let state = test_app_state_with_hwmon();
    state.cache.record_engine_tick(
        "emergency",
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
    );
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(&path, "/hwmon/h1/verify", &serde_json::json!({})).await;
    assert_eq!(
        status, 409,
        "a forced thermal state must refuse a verify: {json}"
    );
    assert_eq!(json["error"]["code"], "validation_error");
    assert_eq!(
        json["error"]["retryable"], true,
        "the force clears by itself, so the client may retry"
    );
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("emergency"),
        "the message must name the state, or it reads as an unexplained refusal \
         on a cool machine: {json}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn gpu_verify_refused_while_thermal_safety_is_forcing() {
    // DEC-297 (295-a), the GPU arm of the same gate.
    let state = test_app_state();
    state.cache.record_engine_tick(
        "recovery",
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
    );
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/gpu/0000:99:00.0/fan/verify",
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 409, "{json}");
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

    let cache = Arc::new(StateCache::new());
    let unsupported = AmdGpuInfo {
        pci_bdf: pci_bdf.into(),
        pci_device_id: 0x0000,
        pci_revision: 0x00,
        pci_class: 0x030000,
        marketing_name: Some("Fake unsupported GPU".into()),
        hwmon_path: std::path::PathBuf::from("/nonexistent/hwmon"),
        fan_curve_path: None,
        fan_zero_rpm_path: None,
        is_discrete: true,
        has_fan_rpm: false,
        has_pwm: false,
        has_pwm_enable: false,
        overdrive_enabled: false,
    };
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: vec![unsupported],
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
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

    let cache = Arc::new(StateCache::new());
    let read_only = AmdGpuInfo {
        pci_bdf: pci_bdf.into(),
        pci_device_id,
        pci_revision: 0xC0,
        pci_class: 0x030000,
        marketing_name: Some("RX 9070 XT".into()),
        hwmon_path: std::path::PathBuf::from("/nonexistent/hwmon"),
        fan_curve_path: None,
        fan_zero_rpm_path: None,
        is_discrete: true,
        has_fan_rpm: true,
        has_pwm: true,         // pwm1 exists
        has_pwm_enable: false, // but pwm1_enable does NOT — this is the bug shape
        overdrive_enabled: false,
    };
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: vec![read_only],
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
        )),
    })
}

/// Helper: AppState with a PMFW-capable GPU whose fan_curve lives at `curve_path`.
/// Point it at a nonexistent path to make the reset fail (DEC-254).
fn test_app_state_with_pmfw_gpu(pci_bdf: &str, curve_path: std::path::PathBuf) -> Arc<AppState> {
    test_app_state_with_amd_gpu(
        pci_bdf,
        Some(curve_path),
        std::path::PathBuf::from("/nonexistent/hwmon"),
    )
}

/// AppState with an AMD GPU. `curve_path: None` + a real `hwmon_path` exercises
/// the legacy-PWM reset arm, which had no coverage of any kind (DEC-255).
fn test_app_state_with_amd_gpu(
    pci_bdf: &str,
    curve_path: Option<std::path::PathBuf>,
    hwmon_path: std::path::PathBuf,
) -> Arc<AppState> {
    let is_legacy = curve_path.is_none();
    use control_ofc_daemon::hwmon::gpu_detect::AmdGpuInfo;

    let cache = Arc::new(StateCache::new());
    let gpu = AmdGpuInfo {
        pci_bdf: pci_bdf.into(),
        pci_device_id: 0x7550,
        pci_revision: 0xC0,
        pci_class: 0x030000,
        marketing_name: Some("RX 9070 XT".into()),
        hwmon_path,
        fan_curve_path: curve_path,
        fan_zero_rpm_path: None,
        is_discrete: true,
        has_fan_rpm: true,
        has_pwm: true, // pwm1 exists
        // Legacy (pre-RDNA3) GPUs expose pwm1_enable; the PMFW shape does not.
        has_pwm_enable: is_legacy,
        overdrive_enabled: true,
    };
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: vec![gpu],
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
        )),
    })
}

#[tokio::test]
async fn gpu_reset_that_fails_does_not_strand_the_fan() {
    // DEC-254. The reset now relinquishes BEFORE writing, so the flag covers the
    // whole sysfs write and an in-flight engine write cannot land on top of
    // firmware-auto. That reordering owes a rollback: if the write then fails,
    // leaving the flag set would strand the fan — not reset, and no longer
    // driven by the engine either, until the next profile activation.
    let bdf = "0000:03:00.0";
    let state =
        test_app_state_with_pmfw_gpu(bdf, std::path::PathBuf::from("/nonexistent/dir/fan_curve"));
    let cache = state.cache.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, _json) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;

    assert_eq!(status, 503, "a failed reset reports hardware_unavailable");
    assert!(
        !cache.is_gpu_fan_relinquished(&format!("amd_gpu:{bdf}")),
        "a reset that failed must hand the fan back to the engine"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_failed_reset_does_not_undo_an_earlier_successful_one() {
    // DEC-255. THE sequential bug, no concurrency required. The claim was
    // unconditional and the rollback was too, so a second reset that failed
    // cleared the flag the first, *successful* reset owned — handing the fan
    // back to the engine after the API told the user it was reset.
    let bdf = "0000:03:00.0";
    let dir = tempfile::tempdir().unwrap();
    let curve = dir.path().join("fan_curve");
    std::fs::write(&curve, "").unwrap();
    let state = test_app_state_with_pmfw_gpu(bdf, curve);
    let cache = state.cache.clone();
    let (path, shutdown, _tmp) = start_test_server(state).await;
    let fan_id = format!("amd_gpu:{bdf}");

    let (status, _) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 200, "first reset succeeds");
    assert!(
        cache.is_gpu_fan_relinquished(&fan_id),
        "success must stand the engine off"
    );

    // Make the very next reset fail, leaving the first one's claim in place.
    // Only the curve's directory — `_tmp` holds the server socket.
    dir.close().unwrap();
    let (status, _) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 503, "second reset fails");
    assert!(
        cache.is_gpu_fan_relinquished(&fan_id),
        "a failed reset must not undo a reset the API already confirmed"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn legacy_pwm_reset_arm_relinquishes_and_rolls_back() {
    // DEC-255: the legacy (pre-RDNA3 `pwm1_enable`) arm had no test of any kind,
    // success or failure, so its copy of the claim/rollback logic was unguarded.
    let bdf = "0000:04:00.0";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pwm1_enable"), "1\n").unwrap();
    let state = test_app_state_with_amd_gpu(bdf, None, dir.path().to_path_buf());
    let cache = state.cache.clone();
    let (path, shutdown, _tmp) = start_test_server(state).await;
    let fan_id = format!("amd_gpu:{bdf}");

    let (status, _) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("pwm1_enable")).unwrap(),
        "2\n",
        "legacy reset writes pwm1_enable=2"
    );
    assert!(cache.is_gpu_fan_relinquished(&fan_id));

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_failed_legacy_reset_hands_the_fan_back() {
    let bdf = "0000:04:00.0";
    let state =
        test_app_state_with_amd_gpu(bdf, None, std::path::PathBuf::from("/nonexistent/dir"));
    let cache = state.cache.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, _) = uds_post(
        &path,
        &format!("/gpu/{bdf}/fan/reset"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 503);
    assert!(!cache.is_gpu_fan_relinquished(&format!("amd_gpu:{bdf}")));

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// `config_test_state` with real hwmon headers (`h1`, `h2`), so the setters that
/// validate a header id — `POST /config/header-role` — can be driven end to end.
fn config_test_state_with_hwmon() -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let admin = tmp.path().join("daemon.toml");
    std::fs::write(&admin, "").unwrap();

    let mut state = test_app_state_with_hwmon();
    let inner = Arc::get_mut(&mut state).unwrap();
    inner.config_path = admin.to_str().unwrap().to_string();
    inner.runtime_config_path = tmp.path().join("runtime.toml");
    (state, tmp)
}

#[tokio::test]
async fn concurrent_config_setters_do_not_lose_an_edit() {
    // `AIO1-d`. Every `POST /config/*` is load the WHOLE file -> change one key
    // -> write the WHOLE file back. Unserialised, two setters that load the same
    // base each write their own key over the other's, the later `save_to` wins
    // the file, and BOTH answer `updated: true` — so the caller is told an edit
    // landed that no longer exists.
    //
    // Six distinct keys, fired concurrently, none of which needs hardware. With
    // the `config_write` lock each request loads the previous winner's file, so
    // all six survive. Without it, the persist (`write` + `fsync` + `rename` +
    // directory `fsync`, off-runtime) is the interleaving window.
    let (state, tmp) = config_test_state("");
    let rc = state.runtime_config_path.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let requests: Vec<(&str, serde_json::Value)> = vec![
        (
            "/config/poll-interval",
            serde_json::json!({"poll_interval_ms": 1234}),
        ),
        (
            "/config/serial-port",
            serde_json::json!({"port": "/dev/ttyACM7"}),
        ),
        (
            "/config/serial-timeout",
            serde_json::json!({"timeout_ms": 421}),
        ),
        (
            "/config/startup-delay",
            serde_json::json!({"delay_secs": 17}),
        ),
        (
            "/config/allow-port-probe",
            serde_json::json!({"enabled": true}),
        ),
        (
            "/config/nvidia-telemetry",
            serde_json::json!({"enabled": true}),
        ),
    ];

    let mut tasks = Vec::new();
    for (route, body) in requests {
        let path = path.clone();
        tasks.push(tokio::spawn(async move {
            let (status, json) = uds_post(&path, route, &body).await;
            (route, status, json)
        }));
    }
    for t in tasks {
        let (route, status, json) = t.await.unwrap();
        assert_eq!(status, 200, "{route}: {json}");
        assert_eq!(json["updated"], true, "{route}: {json}");
    }

    // Assert the realised artefact — the bytes on disk — rather than each
    // response, because every response said `updated: true` in the defect too.
    let written = std::fs::read_to_string(&rc).unwrap();
    for expected in [
        "poll_interval_ms = 1234",
        "/dev/ttyACM7",
        "timeout_ms = 421",
        "delay_secs = 17",
        "allow_port_probe = true",
        "enable_nvidia_telemetry = true",
    ] {
        assert!(
            written.contains(expected),
            "a concurrent setter lost the edit `{expected}`; every request \
             reported success. File was:\n{written}"
        );
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
    drop(tmp);
}

#[tokio::test]
async fn a_cooling_device_write_never_drops_a_concurrent_header_role() {
    // `AIO1-d`, the [SAFETY] case, and the reason this is a lock rather than a
    // note. `header_roles` is the only `/config/*` key that is a safety input:
    // on a board whose Super-I/O publishes no `pwmN_label` files, a user's
    // `pump` assignment is the ONLY evidence a header drives a pump, so losing
    // that edit removes its 30% floor and its identify protection at the next
    // daemon restart.
    //
    // These two routes are the realistic pair rather than an invented one: since
    // DEC-316 the GUI's Configure-AIO flow posts `/config/header-role` and then
    // `/config/cooling-device` in ONE user action, so the interleaving window is
    // opened by ordinary use. The asymmetry matters too — the cooling-device
    // write is metadata the engine never reads, but landing it from a stale base
    // drops the role edit that preceded it.
    let (state, tmp) = config_test_state_with_hwmon();
    let rc = state.runtime_config_path.clone();
    let roles = state.header_roles.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (p1, p2) = (path.clone(), path.clone());
    let role = tokio::spawn(async move {
        uds_post(
            &p1,
            "/config/header-role",
            &serde_json::json!({"header_id": "h1", "role": "pump"}),
        )
        .await
    });
    let device = tokio::spawn(async move {
        uds_post(
            &p2,
            "/config/cooling-device",
            &serde_json::json!({
                "id": "aio-1",
                "name": "AIO",
                "kind": "aio_liquid",
                "pump_member": "h1",
                "radiator_members": ["h2"]
            }),
        )
        .await
    });
    let (role_status, role_json) = role.await.unwrap();
    let (dev_status, dev_json) = device.await.unwrap();
    assert_eq!(role_status, 200, "{role_json}");
    assert_eq!(dev_status, 200, "{dev_json}");

    let written = std::fs::read_to_string(&rc).unwrap();
    assert!(
        written.contains("[hardware.header_roles]") && written.contains(r#"h1 = "pump""#),
        "the pump role was lost from runtime.toml by a concurrent \
         cooling-device write — at the next restart h1 has no 30% floor. \
         File was:\n{written}"
    );
    assert!(
        written.contains("aio-1"),
        "the cooling device was lost instead. File was:\n{written}"
    );
    // The in-memory commit the engine actually reads must agree with the file.
    assert!(
        roles.read().contains_key("h1"),
        "runtime.toml and the engine's live role map disagree"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
    drop(tmp);
}

#[tokio::test]
async fn concurrent_search_dir_edits_are_not_lost() {
    // `AIO1-d`, the limb the first cut of the fix MISSED — found by
    // `ofc:concurrency-reviewer` and confirmed against the code.
    //
    // `concurrent_config_setters_do_not_lose_an_edit` fires six *different*
    // routes, so it can only catch a lost update across keys. This route is the
    // one of the twelve whose persisted value is derived from **shared mutable
    // state** rather than from the request body: `add`/`remove` are a delta
    // merged onto the current list. While that merge base was read *before* the
    // lock, two concurrent callers both merged onto the same stale list and the
    // second silently discarded the first's directory — the lost update
    // surviving inside the very lock added to remove it.
    let (state, tmp) = config_test_state("");
    let rc = state.runtime_config_path.clone();
    // `profile_search_dirs` is a bare `RwLock` on `AppState`, so hold the `Arc`
    // rather than cloning the lock out of it.
    let state_ref = state.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (_ka, a) = addable_dir();
    let (_kb, b) = addable_dir();

    let (p1, p2) = (path.clone(), path.clone());
    let (a1, b1) = (a.clone(), b.clone());
    let ta = tokio::spawn(async move {
        uds_post(
            &p1,
            "/config/profile-search-dirs",
            &serde_json::json!({"add": [a1]}),
        )
        .await
    });
    let tb = tokio::spawn(async move {
        uds_post(
            &p2,
            "/config/profile-search-dirs",
            &serde_json::json!({"add": [b1]}),
        )
        .await
    });
    let (sa, ja) = ta.await.unwrap();
    let (sb, jb) = tb.await.unwrap();
    assert_eq!(sa, 200, "{ja}");
    assert_eq!(sb, 200, "{jb}");

    // Both requests reported success, so assert the realised artefact — the file
    // on disk, and the in-memory list the profile loader actually reads.
    let written = std::fs::read_to_string(&rc).unwrap();
    for (label, dir) in [("first", &a), ("second", &b)] {
        assert!(
            written.contains(dir.as_str()),
            "the {label} concurrent search-dir add was lost from runtime.toml \
             while its request reported success. File was:\n{written}"
        );
    }
    let live: Vec<String> = state_ref
        .profile_search_dirs
        .read()
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    for dir in [&a, &b] {
        assert!(
            live.iter().any(|d| d == dir),
            "runtime.toml and the in-memory search-dir list disagree: {live:?}"
        );
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
    drop(tmp);
}

// Holding the guard across the barrier's `await` is the POINT of this test — it is
// standing in for a sysfs write wedged in the kernel, which is precisely what
// `await_holding_lock` exists to warn about in production. Suppressed here rather
// than restructured onto a helper thread, because moving the wedge off this thread
// would also move the thing being simulated.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wedged_hwmon_header_does_not_stall_unrelated_config_writes() {
    // `AIO1-d`, the second limb the review caught. `POST /config/header-role`
    // builds its response with `resolved_header_role`, which takes
    // `hwmon_controller` — the one lock the profile engine holds across a
    // BLOCKING sysfs write (DEC-278/289). While the config guard was still alive
    // at that point, a single wedged header parked ALL TWELVE `/config/*` write
    // routes instead of the one request that touched it. On the `role: null`
    // clear path this is the handler's only `hwmon_controller` acquisition, so
    // the pre-lock validation above does not cover it.
    //
    // **The discriminator is `config_write`, not latency.** Fixed and broken both
    // leave the clear request blocked on the wedge for ever; they differ in
    // whether it is still holding the config lock while it blocks. The first cut
    // of this test raced two requests and asserted the unrelated one completed —
    // and it **passed with the fix deleted**, because nothing ordered the two, so
    // the unrelated request simply won the lock first. It asserted nothing.
    //
    // `multi_thread` is required: the wedge is a blocking `parking_lot` lock, and
    // on the default current-thread runtime it stops the runtime itself.
    let (state, tmp) = config_test_state_with_hwmon();
    let ctrl = state
        .hwmon_controller
        .clone()
        .expect("harness has a controller");
    let state_ref = state.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Give h1 a role first, so CLEARING it is an observable state change we can
    // use as a barrier. Done before the wedge — this request must complete.
    let (status, json) = uds_post(
        &path,
        "/config/header-role",
        &serde_json::json!({"header_id": "h1", "role": "pump"}),
    )
    .await;
    assert_eq!(status, 200, "{json}");
    assert!(state_ref.header_roles().contains_key("h1"));

    // Wedge the header the way a stuck sysfs write does: hold the controller lock.
    let wedge = ctrl.lock();

    let p1 = path.clone();
    let blocked = tokio::spawn(async move {
        uds_post(
            &p1,
            "/config/header-role",
            &serde_json::json!({"header_id": "h1", "role": null}),
        )
        .await
    });

    // Barrier: wait until the clear has PERSISTED AND COMMITTED. Past that point
    // the handler's only remaining work is the response — i.e. it is at (or in)
    // `resolved_header_role`, blocked on the wedge. Spinning on observable state
    // rather than sleeping for a guessed duration.
    let mut committed = false;
    for _ in 0..600 {
        if !state_ref.header_roles().contains_key("h1") {
            committed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        committed,
        "the clear never reached its in-memory commit; the barrier this test \
         depends on did not hold, so the assertion below would be vacuous"
    );

    // THE ASSERTION. The request is blocked on a wedged header. It must not still
    // be holding the lock that every other `/config/*` route needs.
    let free = state_ref.config_write.try_lock();
    let held = free.is_err();
    drop(free);
    drop(wedge);
    let _ = blocked.await;

    assert!(
        !held,
        "a request blocked on ONE wedged hwmon header is still holding \
         `config_write`, so all twelve /config/* write routes are stalled behind \
         it — the guard must be dropped before the response reads \
         `resolved_header_role`"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
    drop(tmp);
}

#[tokio::test]
async fn status_reports_a_runtime_config_that_failed_to_load() {
    // `AUD3-m`. `RuntimeConfig::load_from` degrades SILENTLY to defaults, and
    // the default carries no `header_roles` — so a boot that cannot parse
    // `runtime.toml` removes every user-assigned pump role's 30% floor, its stop
    // exemption and its pump-safe identify, with one `warn!` in the journal as
    // the entire notification. Nothing on any endpoint said so.
    let (mut state, tmp) = config_test_state("");
    Arc::get_mut(&mut state).unwrap().runtime_config_degraded = Arc::new(parking_lot::RwLock::new(
        Some(control_ofc_daemon::runtime_config::RuntimeConfigDegraded {
            reason: "malformed".into(),
            path: "/var/lib/control-ofc/runtime.toml".into(),
            detail: "expected `]`".into(),
            phase: "startup".into(),
        }),
    ));
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Must ride BOTH surfaces: the GUI is poll-only at 1 Hz (DEC-165), so a
    // field that appeared on `/status` alone would never be seen.
    for route in ["/status", "/poll"] {
        let (status, json) = uds_get(&path, route).await;
        assert_eq!(status, 200, "{route}: {json}");
        let d = if route == "/poll" {
            &json["status"]["runtime_config_degraded"]
        } else {
            &json["runtime_config_degraded"]
        };
        assert_eq!(d["reason"], "malformed", "{route}: {json}");
        assert_eq!(d["phase"], "startup", "{route}: {json}");
        assert_eq!(d["detail"], "expected `]`", "{route}: {json}");
        assert!(d["path"].is_string(), "{route}: {json}");
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
    drop(tmp);
}

#[tokio::test]
async fn status_omits_runtime_config_degraded_when_the_config_is_healthy() {
    // The field is additive and absent-means-fine, so an older daemon's omission
    // reads exactly as today's behaviour (no warning) rather than as a warning
    // that cannot be dismissed. Asserted as absence of the KEY, not a null.
    let (state, tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/status").await;
    assert_eq!(status, 200);
    assert!(
        json.get("runtime_config_degraded").is_none(),
        "a healthy daemon must not carry the key at all: {json}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
    drop(tmp);
}

#[tokio::test]
async fn a_config_setter_quarantines_an_unreadable_file_and_still_applies() {
    // DEC-255, end-to-end through HTTP. The review found that NOTHING exercised
    // this wiring: a handler could be reverted to the old load_from (which
    // silently replaces every other setting with a default) and the whole suite
    // stayed green. This test fails if any setter stops going through
    // `runtime_for_update`.
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join("runtime.toml");
    let original = "[polling]\npoll_interval_ms = 900\n[garbage\n";
    std::fs::write(&rc, original).unwrap();

    let state = test_app_state_with_runtime_config(rc.clone());
    let (path, shutdown, _tmp) = start_test_server(state).await;

    let (status, _json) = uds_post(
        &path,
        "/config/poll-interval",
        &serde_json::json!({"poll_interval_ms": 750}),
    )
    .await;

    assert_eq!(
        status, 200,
        "an unparseable file must not dead-end the setter"
    );
    assert!(
        std::fs::read_to_string(&rc)
            .unwrap()
            .contains("poll_interval_ms = 750"),
        "the new value is written"
    );

    let quarantined: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("runtime.toml.invalid-")
        })
        .collect();
    assert_eq!(
        quarantined.len(),
        1,
        "the old file is preserved, not destroyed"
    );
    assert_eq!(
        std::fs::read_to_string(quarantined[0].path()).unwrap(),
        original,
        "byte for byte"
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
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    let state = Arc::new(AppState {
        cache: cache.clone(),
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
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
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
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

    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    let state = Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: vec![pmfw],
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
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
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    let state = Arc::new(AppState {
        cache,
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: Some(Arc::new(Mutex::new(ctrl))),
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
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
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    Arc::new(AppState {
        cache: Arc::new(StateCache::new()),
        staleness_config: StalenessConfig::default(),
        daemon_version: "0.1.0-test".into(),
        fan_controller: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: std::sync::atomic::AtomicBool::new(false),
        characterization: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        validation: std::sync::Arc::new(Default::default()),
        characterization_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_path: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: std::sync::Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
        last_openfan_rescan: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_handles: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(dirs),
        config_path: String::new(),
        runtime_config_path: std::path::PathBuf::new(),
        sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
            std::collections::HashMap::new(),
        ))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        allow_port_probe: false,
        running_config: Default::default(),
        readiness_rollup: readiness_rollup.clone(),
        config_write: Default::default(),
        runtime_config_degraded: Default::default(),
        assessment: Arc::new(control_ofc_daemon::api::handlers::AssessmentCache::new(
            readiness_rollup,
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

#[tokio::test]
async fn profile_activate_parse_error_returns_generic_message_without_path() {
    // DEC-173 posture on the activate path (2026-07-21 audit): a corrupt
    // stored profile must produce a generic envelope — the store's absolute
    // path and the serde parser detail stay server-side (log only).
    let tmp = tempfile::tempdir().unwrap();
    let search_dir = tmp.path().join("search");
    std::fs::create_dir_all(&search_dir).unwrap();

    let profile_path = search_dir.join("corrupt.json");
    std::fs::write(&profile_path, "{ not json").unwrap();

    let state = test_app_state_with_profile_dirs(vec![search_dir.clone()]);
    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let body = serde_json::json!({
        "profile_path": profile_path.display().to_string(),
    });
    let (status, json) = uds_post(&path, "/profile/activate", &body).await;

    assert_eq!(
        status, 400,
        "corrupt profile must 400, got {status}: {json}"
    );
    assert_eq!(json["error"]["code"], "validation_error");
    assert_eq!(
        json["error"]["message"].as_str().unwrap_or_default(),
        "profile could not be read or parsed",
        "message must be the generic text, got: {json}"
    );
    assert!(
        !json.to_string().contains(&search_dir.display().to_string()),
        "envelope must not leak the store path: {json}"
    );
    assert!(
        state.active_profile.lock().is_none(),
        "a failed activation must not install a profile"
    );

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
    // `WIRE-ac`: anchored to the constant, not to the number. This assertion was
    // a literal `"2.0.0"` and is exactly why moving the floor is a change and not
    // a typo fix — see `capabilities_publish_the_gui_pairing_floor_from_one_source`
    // for the contract, and `CLAUDE.md`'s rule that a threshold spelled into a
    // test drifts every time the threshold moves.
    assert_eq!(
        body["control"]["min_supported_gui"],
        control_ofc_daemon::constants::MIN_SUPPORTED_GUI
    );
}

#[tokio::test]
async fn hardware_diagnostics_report_the_boards_firmware_declared_counts() {
    // `X87-d`, asserted at the CALL SITE. The decode is unit-tested in
    // `hwmon::gigabyte_siv`; a helper nothing reads is a rule with no consumer,
    // which is `CLAUDE.md`'s most-repeated failure here.
    //
    // Written as a RELATIONSHIP against the reader rather than against this
    // host's descriptor, so it means the same thing on a Gigabyte board (where
    // the file exists) and on every other machine (where it does not). A test
    // asserting `fan_count == 8` would pass only on the board the row was opened
    // from and would be skipped-by-accident everywhere else.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/diagnostics/hardware").await;
    assert_eq!(status, 200);

    let expected = control_ofc_daemon::hwmon::gigabyte_siv::read_siv(std::path::Path::new(
        control_ofc_daemon::hwmon::gigabyte_siv::GIGABYTE_SIV_PATH,
    ));
    match expected {
        Some(siv) => {
            let got = &json["board_firmware_counts"];
            assert!(
                got.is_object(),
                "this host publishes a SIV, so the field must be present: {json}"
            );
            assert_eq!(got["fan_count"], siv.fan_count);
            assert_eq!(got["temp_count"], siv.temp_count);
            assert_eq!(got["volt_count"], siv.volt_count);
            assert_eq!(got["platform"], siv.platform);
        }
        None => {
            // ABSENT, not zero. A defaulted `fan_count: 0` would read as "this
            // board has no fan headers" — a far stronger and wrong claim than
            // "the firmware did not say", and the reason the field is an Option.
            assert!(
                json.get("board_firmware_counts").is_none(),
                "no SIV on this host: the key must be omitted, never defaulted: {json}"
            );
        }
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn status_and_poll_report_whether_the_engine_is_writing() {
    // `WIRE-n`. The engine keeps EVALUATING during a verify / characterisation /
    // calibration / validation sweep — it publishes every control's duty in
    // `control_outputs[]` — and simply does not write it. Without this field a
    // client renders a duty nothing is applying.
    //
    // Both surfaces are asserted because they build the field independently:
    // `/status` and `/poll` each read it inside their own `read_with` closure,
    // and a fix applied to one is not a fix applied to the other.
    //
    // Every assertion is a RELATIONSHIP against `cache.verify_active()`, never a
    // literal `true`/`false`. A literal passes on a handler that hardcodes the
    // field, which is the failure this field exists to prevent one layer down;
    // the relationship only passes if the wire really is derived from the slot.
    let state = test_app_state_with_hwmon();
    let cache = state.cache.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Idle: nothing holds the slot, so both surfaces must say the engine writes.
    assert!(!cache.verify_active(), "precondition: slot starts free");
    let (_, status_json) = uds_get(&path, "/status").await;
    let (_, poll_json) = uds_get(&path, "/poll").await;
    assert_eq!(status_json["verify_active"], cache.verify_active());
    assert_eq!(poll_json["status"]["verify_active"], cache.verify_active());
    assert_eq!(
        status_json["verify_active"], false,
        "the idle branch must actually be observed, or this test asserts nothing"
    );

    // Held: claim the slot the way a verify handler does.
    assert!(cache
        .try_begin_verify(std::time::Duration::from_secs(30))
        .is_some());
    assert!(cache.verify_active(), "precondition: the slot is now held");
    let (_, status_json) = uds_get(&path, "/status").await;
    let (_, poll_json) = uds_get(&path, "/poll").await;
    assert_eq!(
        status_json["verify_active"],
        cache.verify_active(),
        "/status must report the live slot: {status_json}"
    );
    assert_eq!(
        poll_json["status"]["verify_active"],
        cache.verify_active(),
        "/poll must report the live slot: {poll_json}"
    );
    assert_eq!(
        status_json["verify_active"], true,
        "the held branch must actually be observed, or the field could be a constant false"
    );

    // The DEADMAN is part of the predicate, not an afterthought (DEC-296): a
    // leaked guard leaves `verify_in_progress` set forever while the engine
    // resumes writing, and a client told "still verifying" past that point would
    // blank its cards against a daemon that was commanding. The test seam expires
    // the claim in place, so no sleep is needed and nothing is timing-dependent.
    cache.expire_verify_claim_for_test();
    assert!(!cache.verify_active(), "precondition: the deadman elapsed");
    let (_, status_json) = uds_get(&path, "/status").await;
    assert_eq!(
        status_json["verify_active"], false,
        "an elapsed deadman means the engine is writing again: {status_json}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hwmon_verify_rejects_concurrent_with_409() {
    // DEC-165 single-flight: while one hardware verify holds the slot, a second
    // verify must be rejected with 409 rather than clobbering the first's engine
    // pause / "verify" lease. Pre-occupy the slot as if a verify were in flight.
    let state = test_app_state_with_hwmon();
    assert!(state
        .cache
        .try_begin_verify(std::time::Duration::from_secs(30))
        .is_some());
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(&path, "/hwmon/h1/verify", &serde_json::json!({})).await;
    assert_eq!(status, 409, "concurrent verify must be rejected: {json}");
    assert_eq!(json["error"]["code"], "validation_error");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hwmon_verify_is_not_rejected_once_the_slot_deadman_elapsed() {
    // DEC-296, and the mirror of the test above: the 409 must be BOUNDED.
    //
    // Pre-DEC-296 a slot whose holder never released it rejected every later
    // verify and calibration for the process lifetime. The three unit tests for
    // this live on `StateCache`; this one pins it where it was user-visible — at
    // the HTTP layer — so a regression at a CALL SITE (a caller passing a wrong
    // epoch, or `begin_verify_pause` reverting to a bool) cannot leave them all
    // green. `Duration::ZERO` is elapsed by construction, so no sleep is needed.
    let state = test_app_state_with_hwmon();
    assert!(state
        .cache
        .try_begin_verify(std::time::Duration::ZERO)
        .is_some());
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(&path, "/hwmon/h1/verify", &serde_json::json!({})).await;
    assert_ne!(
        status, 409,
        "an elapsed verify deadman must free the slot, not reject forever: {json}"
    );

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
    let _ = state.cache.relinquish_gpu_fan("amd_gpu:0000:03:00.0");

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
async fn activating_different_profile_does_not_bleed_override_onto_same_id_control() {
    // B4 / DEC-189: an override taken while profile A is active must NOT carry onto
    // the same-id "cpu" control when a DIFFERENT profile B is activated. The
    // existing activate test only re-activates the SAME profile (least likely to
    // regress) — this exercises a real profile switch.
    let tmp = tempfile::tempdir().unwrap();
    let search_dir = tmp.path().join("search");
    std::fs::create_dir_all(&search_dir).unwrap();

    // Two DISTINCT profiles (ids "pa"/"pb"), each carrying a control "cpu".
    let path_a = search_dir.join("a.json");
    let path_b = search_dir.join("b.json");
    std::fs::write(
        &path_a,
        r#"{"id":"pa","name":"A","version":7,
            "controls":[{"id":"cpu","name":"CPU","curve_id":"c1"}],
            "curves":[{"id":"c1","name":"C1","type":"flat","flat_output_pct":50}]}"#,
    )
    .unwrap();
    std::fs::write(
        &path_b,
        r#"{"id":"pb","name":"B","version":7,
            "controls":[{"id":"cpu","name":"CPU","curve_id":"c2"}],
            "curves":[{"id":"c2","name":"C2","type":"flat","flat_output_pct":30}]}"#,
    )
    .unwrap();

    let state = test_app_state_with_profile_dirs(vec![search_dir.clone()]);
    let (sock, shutdown, _d) = start_test_server(state.clone()).await;
    let activate_a = serde_json::json!({ "profile_path": path_a.display().to_string() });
    let activate_b = serde_json::json!({ "profile_path": path_b.display().to_string() });

    // Activate A, take an override on "cpu", capture the token.
    let (st, j) = uds_post(&sock, "/profile/activate", &activate_a).await;
    assert_eq!(st, 200, "activate A: {j}");
    let (st, j) = uds_post(
        &sock,
        "/control/cpu/override",
        &serde_json::json!({"pwm_percent": 80}),
    )
    .await;
    assert_eq!(st, 200, "override take: {j}");
    let token = j["override_token"].clone();

    // Sanity: /status shows the override under A.
    let (_st, status) = uds_get(&sock, "/status").await;
    assert_eq!(status["overrides"][0]["control_id"], "cpu");

    // Activate the DIFFERENT profile B.
    let (st, j) = uds_post(&sock, "/profile/activate", &activate_b).await;
    assert_eq!(st, 200, "activate B: {j}");

    // (a) No overrides bleed onto B's same-id "cpu".
    let (_st, status) = uds_get(&sock, "/status").await;
    assert!(
        status.get("overrides").is_none(),
        "cross-profile activation must clear standing overrides, got {status}"
    );

    // (b) The old token no longer renews.
    let (st, body) = uds_post(
        &sock,
        "/control/cpu/override/renew",
        &serde_json::json!({ "override_token": token }),
    )
    .await;
    assert_eq!(st, 404, "stale renew after cross-profile activate: {body}");
    assert_eq!(body["error"]["code"], "override_expired");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn deactivate_profile_clears_standing_overrides() {
    // DEC-218: deactivation relinquishes curve-driven control, so it must clear
    // standing manual overrides too — symmetric with activation (DEC-189). An
    // override left behind would bleed onto a same-id control in the next
    // profile, and its token would still renew after deactivation.
    let tmp = tempfile::tempdir().unwrap();
    let search_dir = tmp.path().join("search");
    std::fs::create_dir_all(&search_dir).unwrap();
    let profile_path = search_dir.join("p.json");
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

    // Activate, then pin "cpu" with a manual override and capture its token.
    let (st, j) = uds_post(&sock, "/profile/activate", &activate).await;
    assert_eq!(st, 200, "activate: {j}");
    let (st, j) = uds_post(
        &sock,
        "/control/cpu/override",
        &serde_json::json!({"pwm_percent": 80}),
    )
    .await;
    assert_eq!(st, 200, "override take: {j}");
    let token = j["override_token"].clone();

    // Sanity: /status surfaces the live override.
    let (_st, status) = uds_get(&sock, "/status").await;
    assert_eq!(status["overrides"][0]["control_id"], "cpu");

    // Deactivate — DEC-218 must clear the override under the active_profile lock.
    let (st, j) = uds_post(&sock, "/profile/deactivate", &serde_json::json!({})).await;
    assert_eq!(st, 200, "deactivate: {j}");

    // The override is gone from /status ...
    let (_st, status) = uds_get(&sock, "/status").await;
    assert!(
        status.get("overrides").is_none(),
        "deactivation must clear standing overrides, got {status}"
    );
    // ... and its token no longer renews (the card would revert on next renew).
    let (st, body) = uds_post(
        &sock,
        "/control/cpu/override/renew",
        &serde_json::json!({ "override_token": token }),
    )
    .await;
    assert_eq!(st, 404, "renew after deactivate: {body}");
    assert_eq!(body["error"]["code"], "override_expired");

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

/// DEC-205: peer-uid confinement of `POST /config/profile-search-dirs`.
///
/// The request travels over the real Unix socket, so the handler sees the test
/// process's own uid via `SO_PEERCRED`. A non-root caller adding a directory
/// outside its home is rejected with `400 validation_error` *before* any
/// persistence — proving the peer uid is delivered end-to-end and confinement
/// fires. Root is exempt (it bypasses confinement and then hits the test's
/// unwritable runtime path), so it is asserted only to NOT be the confinement
/// 400.
#[tokio::test]
async fn profile_search_dirs_confines_non_root_to_home() {
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // `/tmp` exists (so canonicalize succeeds) but is not within any user's home.
    let (status, json) = uds_post(
        &path,
        "/config/profile-search-dirs",
        &serde_json::json!({ "add": ["/tmp"] }),
    )
    .await;

    // SAFETY: getuid() takes no arguments and always succeeds.
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        // Root is exempt from confinement, so it falls through to the persist
        // step — which fails against this test state's empty runtime path,
        // yielding 503. The point: root is NEVER the confinement 400, and it
        // clearly reached persistence (proving the exemption fired).
        assert_eq!(
            status, 503,
            "root must bypass confinement and reach persistence: {json}"
        );
        assert_eq!(json["error"]["code"], "persistence_failed");
    } else {
        assert_eq!(
            status, 400,
            "non-root out-of-home dir must be rejected: {json}"
        );
        assert_eq!(json["error"]["code"], "validation_error");
        // The message must come from the home-confinement logic specifically —
        // "within your home directory …" (home resolved, /tmp is outside) or, in
        // an exotic passwd-less env, "cannot resolve the home directory …". Both
        // contain "home directory"; the None-uid fail-closed branch ("cannot
        // identify the requesting user") and any generic pre-filter 400 do NOT.
        // This proves the peer uid reached the handler AND the confinement path
        // executed end-to-end, not merely that some 400 occurred.
        let msg = json["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("home directory"),
            "expected a home-confinement rejection reached via SO_PEERCRED, got: {msg}"
        );
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── Search-dir removal (2.23.0) ──────────────────────────────────────────
// `POST /config/profile-search-dirs` was add-only and merge-only, so the list
// could only grow: the GUI re-registers on every connect and added a fresh entry
// every time the user repointed their profiles directory, leaving stale entries
// in runtime.toml that no UI could reach and only a root hand-edit could remove.

/// The home directory the search-dir confinement resolves for this test process.
///
/// The handler resolves it from the passwd database (`getpwuid_r`), not from the
/// environment; `$HOME` is the same value on any normal Linux host and in CI. If
/// the two ever diverged these tests would fail loudly rather than pass
/// vacuously, which is the failure mode worth having.
fn confinement_home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").expect("HOME must be set to run these tests"))
}

/// A real directory the *add* path will accept: inside this user's home, because
/// `confine_added_dirs` requires both. Root is exempt from confinement, so the
/// system temp dir is fine there and avoids writing into `/root`.
fn addable_dir() -> (tempfile::TempDir, String) {
    // SAFETY: getuid() takes no arguments and always succeeds.
    let dir = if unsafe { libc::getuid() } == 0 {
        tempfile::tempdir().unwrap()
    } else {
        tempfile::Builder::new()
            .prefix(".control-ofc-test-")
            .tempdir_in(confinement_home())
            .unwrap()
    };
    let path = dir.path().to_string_lossy().into_owned();
    (dir, path)
}

/// A path inside this user's home that does NOT exist — the stale entry a user
/// actually needs to prune.
fn vanished_dir() -> String {
    // SAFETY: getuid() takes no arguments and always succeeds.
    let root = if unsafe { libc::getuid() } == 0 {
        std::path::PathBuf::from("/root")
    } else {
        confinement_home()
    };
    root.join(format!(
        ".control-ofc-vanished-{}/profiles",
        std::process::id()
    ))
    .to_string_lossy()
    .into_owned()
}

#[tokio::test]
async fn profile_search_dirs_requires_add_or_remove() {
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) =
        uds_post(&path, "/config/profile-search-dirs", &serde_json::json!({})).await;
    assert_eq!(status, 400, "an empty edit must be rejected: {json}");
    assert_eq!(json["error"]["code"], "validation_error");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("add") && msg.contains("remove"),
        "the rejection must name both operations: {msg}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn profile_search_dirs_rejects_a_non_string_entry() {
    // REGRESSION: the old parser silently dropped non-strings, so `[null]` was
    // indistinguishable from `[]` and the handler reported success having done
    // nothing at all.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/config/profile-search-dirs",
        &serde_json::json!({ "add": [serde_json::Value::Null] }),
    )
    .await;
    assert_eq!(status, 400, "a non-string entry must be rejected: {json}");
    assert_eq!(json["error"]["code"], "validation_error");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn profile_search_dirs_refuses_to_remove_the_system_dir() {
    // Checked before confinement, so the message is the useful one at any uid:
    // "the system profile directory cannot be removed", not "not in your home".
    let (state, _tmp) = config_test_state("");
    *state.profile_search_dirs.write() = vec![std::path::PathBuf::from(
        control_ofc_daemon::config::SYSTEM_PROFILE_DIR,
    )];
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/config/profile-search-dirs",
        &serde_json::json!({ "remove": [control_ofc_daemon::config::SYSTEM_PROFILE_DIR] }),
    )
    .await;
    assert_eq!(status, 400, "the system dir must be protected: {json}");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains(control_ofc_daemon::config::SYSTEM_PROFILE_DIR),
        "the rejection must name the directory: {msg}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn profile_search_dirs_removes_an_entry_that_no_longer_exists() {
    // THE case the feature exists for, end to end over the socket: a stale entry
    // whose directory is gone must be prunable. Reusing the *add* confinement
    // predicate (which canonicalizes, and so requires existence) would reject
    // exactly this request.
    let (state, tmp) = config_test_state("");
    let stale = vanished_dir();
    *state.profile_search_dirs.write() = vec![
        std::path::PathBuf::from(control_ofc_daemon::config::SYSTEM_PROFILE_DIR),
        std::path::PathBuf::from(&stale),
    ];
    let runtime_path = tmp.path().join("runtime.toml");
    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let (status, json) = uds_post(
        &path,
        "/config/profile-search-dirs",
        &serde_json::json!({ "remove": [stale] }),
    )
    .await;
    assert_eq!(
        status, 200,
        "a vanished in-home entry must be prunable: {json}"
    );
    assert_eq!(
        json["search_dirs"],
        serde_json::json!([control_ofc_daemon::config::SYSTEM_PROFILE_DIR]),
        "the response must report the pruned list"
    );

    // In-memory state committed…
    let live: Vec<String> = state
        .profile_search_dirs
        .read()
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(live, vec![control_ofc_daemon::config::SYSTEM_PROFILE_DIR]);
    // …and persisted, or the entry returns on the next restart.
    let persisted = std::fs::read_to_string(&runtime_path).expect("runtime.toml must be written");
    assert!(
        !persisted.contains(&stale),
        "the pruned dir must be gone from runtime.toml:\n{persisted}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn profile_search_dirs_add_and_remove_is_a_single_move() {
    // What the GUI sends when the user repoints their profiles directory. Before
    // this, only the `add` half happened and the old entry accumulated forever.
    let (state, _tmp) = config_test_state("");
    let old = vanished_dir();
    let (_keep_alive, new) = addable_dir();
    *state.profile_search_dirs.write() = vec![
        std::path::PathBuf::from(control_ofc_daemon::config::SYSTEM_PROFILE_DIR),
        std::path::PathBuf::from(&old),
    ];
    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let (status, json) = uds_post(
        &path,
        "/config/profile-search-dirs",
        &serde_json::json!({ "add": [new], "remove": [old] }),
    )
    .await;
    assert_eq!(status, 200, "the move must be accepted: {json}");
    assert_eq!(
        json["search_dirs"],
        serde_json::json!([control_ofc_daemon::config::SYSTEM_PROFILE_DIR, new]),
        "the old entry must be gone and the new one present"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn profile_search_dirs_refuses_an_edit_that_would_empty_the_path() {
    // `activate_profile` resolves against this list; emptying it would be an
    // unrecoverable soft-lock reachable from an unprivileged API call.
    let (state, _tmp) = config_test_state("");
    let only = vanished_dir();
    *state.profile_search_dirs.write() = vec![std::path::PathBuf::from(&only)];
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/config/profile-search-dirs",
        &serde_json::json!({ "remove": [only] }),
    )
    .await;
    assert_eq!(
        status, 400,
        "emptying the search path must be refused: {json}"
    );
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("at least one"), "unexpected message: {msg}");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn profile_search_dirs_refuses_to_remove_the_profile_store() {
    // REGRESSION (DEC-285 self-review P1). `profile.rs::store_dir()` is
    // `profile_search_dirs.first()` and it is the write target for profile
    // create and delete, so dropping it would silently redirect every profile
    // write for the rest of the process's life. Root/CLI callers are exempt from
    // peer-uid confinement, so the guard has to be in the merge, not in the
    // confinement.
    let (state, _tmp) = config_test_state("");
    let store = "/var/lib/control-ofc/profiles";
    *state.profile_search_dirs.write() = vec![
        std::path::PathBuf::from(store),
        std::path::PathBuf::from(control_ofc_daemon::config::SYSTEM_PROFILE_DIR),
        std::path::PathBuf::from(vanished_dir()),
    ];
    let (path, shutdown, _dir) = start_test_server(state.clone()).await;

    let (status, json) = uds_post(
        &path,
        "/config/profile-search-dirs",
        &serde_json::json!({ "remove": [store] }),
    )
    .await;
    assert_eq!(status, 400, "the profile store must be protected: {json}");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains(store), "the rejection must name it: {msg}");

    // …and the live list is untouched, so profile writes still land where they did.
    assert_eq!(
        state
            .profile_search_dirs
            .read()
            .first()
            .map(|p| p.display().to_string())
            .as_deref(),
        Some(store)
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── DEC-311 / AIO-MB Phase 1: header roles + pump-safe identify ──────────

#[tokio::test]
async fn capabilities_advertise_header_roles() {
    // Load-bearing for TRUTHFULNESS, not just for hiding a button: a GUI that
    // says "the pump will briefly change speed" is lying against a pre-2.28.0
    // daemon, which drives the pump to 0 instead.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    assert_eq!(
        json["control"]["header_roles"], true,
        "this daemon classifies header roles and protects pumps: {json}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capabilities_advertise_the_five_previously_flagless_features() {
    // `WIRE-k`. These five routes shipped before this block had keys for them, so
    // a client detected them by comparing the daemon's VERSION STRING or by
    // reading a 404 off the route. The comments on `pwm_characterization` and
    // `validation_sessions` already record why a probe is not a contract: the
    // route fallback's 404 is indistinguishable from a handler's own 404 for an
    // unknown id.
    //
    // Asserted against the ROUTES rather than as five bare `true`s: a flag that
    // claims a feature the daemon does not serve is worse than no flag, and five
    // literals would pass on a handler that hardcoded them with the routes gone.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, caps) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    for flag in [
        "gpu_fan_verify",
        "hardware_readiness",
        "superio_port_probe",
        "preferred_sensors",
        "daemon_config_report",
    ] {
        assert_eq!(
            caps["control"][flag], true,
            "control.{flag} must be advertised: {caps}"
        );
    }

    // Each advertised GET must actually route. A 404 `not_found` from the
    // fallback is the exact failure the flags exist to make undetectable-by-probe,
    // so it is the thing this asserts against — any other status (including a
    // 503 for absent hardware) means the route is served.
    for route in [
        "/inventory/hardware-readiness",
        "/inventory/hwmon",
        "/config",
    ] {
        let (status, body) = uds_get(&path, route).await;
        assert_ne!(status, 404, "{route} is advertised but not routed: {body}");
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capabilities_publish_the_gui_pairing_floor_from_one_source() {
    // `WIRE-ac`. Three numbers claimed to be this contract: the handler said
    // `2.0.0`, ~30 release notes said `>= 2.23.0`, and the 2.16.0 entry said
    // `>= 2.38.0`. Asserted as a RELATIONSHIP against the constant, not as the
    // literal "2.23.0" — a literal here would have to be edited in lockstep with
    // the constant, which is the drift this row is about, one level up.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, caps) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    assert_eq!(
        caps["control"]["min_supported_gui"],
        control_ofc_daemon::constants::MIN_SUPPORTED_GUI,
        "the handler must publish the constant verbatim: {caps}"
    );
    assert_ne!(
        caps["control"]["min_supported_gui"], "",
        "an empty floor means 'no floor enforced' and is not what this daemon means"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn header_role_assignment_round_trips_and_validates() {
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    // An unrecognised role token is REJECTED, never silently defaulted — a typo
    // that became `unknown` would drop a pump's protection while the response
    // said "updated".
    let (status, json) = uds_post(
        &path,
        "/config/header-role",
        &serde_json::json!({ "header_id": "hwmon:x:pwm1:PUMP", "role": "impeller" }),
    )
    .await;
    assert_eq!(status, 400, "unknown role token must be rejected: {json}");
    assert_eq!(json["error"]["code"], "validation_error");

    // A missing `role` key is a 400 (distinct from an explicit null, which clears).
    let (status, _) = uds_post(
        &path,
        "/config/header-role",
        &serde_json::json!({ "header_id": "hwmon:x:pwm1:PUMP" }),
    )
    .await;
    assert_eq!(status, 400);

    // A missing/empty header_id is a 400.
    let (status, _) = uds_post(
        &path,
        "/config/header-role",
        &serde_json::json!({ "role": "pump" }),
    )
    .await;
    assert_eq!(status, 400);

    // Assigning to a header this daemon has never discovered is a 400 rather
    // than a silently-stored assignment that can never take effect.
    let (status, json) = uds_post(
        &path,
        "/config/header-role",
        &serde_json::json!({ "header_id": "hwmon:nope:pwm9:X", "role": "pump" }),
    )
    .await;
    assert_eq!(status, 400, "unknown header id must be rejected: {json}");

    // Clearing is always allowed, even for an id that no longer exists — a
    // stale assignment must never become unreachable.
    let (status, json) = uds_post(
        &path,
        "/config/header-role",
        &serde_json::json!({ "header_id": "hwmon:nope:pwm9:X", "role": null }),
    )
    .await;
    assert_eq!(status, 200, "a clear must always be possible: {json}");
    assert_eq!(json["updated"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn hwmon_headers_carry_role_and_role_source() {
    // The GUI joins fan → header by id to learn a fan is a pump, so the field
    // has to be on this response (and on /inventory/hwmon, which shares the
    // same entry type — one struct, so one assertion covers the shape).
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/hwmon/headers").await;
    assert_eq!(status, 200);
    // The test state has no hwmon controller, so the list is empty; assert the
    // envelope rather than inventing hardware. The per-field shape is pinned by
    // the roles unit tests and the serde round-trip test.
    assert!(json["headers"].is_array(), "{json}");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn identify_reports_the_mode_it_actually_used() {
    // DEC-311: the client asks for "stop"; the DAEMON decides what that means
    // and says which it did. An unclassified fan still stops — the
    // "existing ordinary fan behaviour remains intact" acceptance criterion.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/fans/openfan:ch00/identify",
        &serde_json::json!({ "action": "stop" }),
    )
    .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(
        json["mode"], "stop",
        "an unclassified fan still stops: {json}"
    );
    assert_eq!(json["identify_pwm_percent"], 0);

    // Restore omits the mode/duty fields entirely (nothing is being held).
    let (status, json) = uds_post(
        &path,
        "/fans/openfan:ch00/identify",
        &serde_json::json!({ "action": "restore" }),
    )
    .await;
    assert_eq!(status, 200);
    assert!(json.get("mode").is_none(), "restore holds nothing: {json}");
    assert!(json.get("identify_pwm_percent").is_none());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capabilities_advertise_search_dir_removal() {
    // A client MUST gate on this flag: an older daemon does not 404 a `remove`,
    // it parses only `add` and ignores the rest, so probing reads a partial
    // success as a whole one.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    assert_eq!(
        json["control"]["profile_search_dir_remove"], true,
        "this daemon supports removal and must say so: {json}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── DEC-243: readable + extended-writable daemon configuration ───────────
// `GET /config` exists because the writable knobs were previously write-only:
// the GUI kept a local mirror and pushed it on save, so a fresh client against a
// daemon set to 10 s displayed 0 s. Every assertion here is about the API being
// *truthful* — the value, where it came from, and whether it is actually live.

/// AppState with real config paths, so the config handlers hit real files.
fn config_test_state(admin_toml: &str) -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let admin = tmp.path().join("daemon.toml");
    std::fs::write(&admin, admin_toml).unwrap();
    let runtime = tmp.path().join("runtime.toml");

    let mut state = test_app_state();
    let inner = Arc::get_mut(&mut state).unwrap();
    inner.config_path = admin.to_str().unwrap().to_string();
    inner.runtime_config_path = runtime;
    inner.running_config =
        control_ofc_daemon::config::DaemonConfig::from_toml(admin_toml).unwrap_or_default();
    (state, tmp)
}

#[tokio::test]
async fn get_config_reports_defaults_with_source() {
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/config").await;
    assert_eq!(status, 200);
    assert_eq!(json["api_version"], 1);
    assert!(json["admin_config_path"].is_string());
    assert!(json["runtime_config_path"].is_string());

    let keys = json["keys"].as_array().unwrap();
    let delay = keys
        .iter()
        .find(|k| k["key"] == "startup.delay_secs")
        .expect("startup.delay_secs must be reported");
    assert_eq!(delay["value"], 0);
    assert_eq!(delay["source"], "default", "nothing set it — not 'admin'");
    assert_eq!(delay["mutable"], true);
    assert_eq!(delay["restart_pending"], false);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn get_config_key_set_and_mutability_are_pinned() {
    // The GUI's Settings page must offer a control for every `mutable: true` key
    // — `tests/test_daemon_config_coverage.py` over there enforces that against a
    // declared fixture. Nothing on THIS side stopped a key arriving unnoticed,
    // which is how `profiles.search_dirs` came to be editable over the API with
    // no UI anywhere and no way to prune what the GUI kept adding.
    //
    // Adding a key is deliberate work: update this list, the GUI's
    // `tests/fixtures/daemon_config_keys.json`, and `docs/08` § Config
    // management. Order is asserted too — `keys[]` is a list, and the GUI's
    // fixture is diffed against it.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (_status, json) = uds_get(&path, "/config").await;
    let reported: Vec<(String, bool)> = json["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| {
            (
                k["key"].as_str().unwrap().to_string(),
                k["mutable"].as_bool().unwrap(),
            )
        })
        .collect();

    let expected: Vec<(String, bool)> = [
        ("profiles.search_dirs", true),
        ("startup.delay_secs", true),
        ("polling.poll_interval_ms", true),
        ("serial.port", true),
        ("serial.timeout_ms", true),
        ("detection.allow_port_probe", true),
        ("detection.enable_nvidia_telemetry", true),
        // Read-only by design: a bad socket path locks every client out
        // permanently, and moving the state dir orphans runtime.toml and the
        // profile store.
        ("ipc.socket_path", false),
        ("state.state_dir", false),
    ]
    .iter()
    .map(|(k, m)| (k.to_string(), *m))
    .collect();

    assert_eq!(
        reported, expected,
        "GET /config's key set changed — update the GUI fixture and docs/08 too"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn startup_delay_response_carries_the_shared_setter_shape() {
    // `POST /config/startup-delay` predates DEC-243 and answered with only
    // `delay_secs`, so it could not go through the same client-side parser as
    // every other `POST /config/*` — which is why the GUI drove it from a local
    // mirror instead of the shared write path. `delay_secs` is retained for
    // older clients.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/config/startup-delay",
        &serde_json::json!({ "delay_secs": 7 }),
    )
    .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["key"], "startup.delay_secs");
    assert_eq!(json["value"], 7);
    assert_eq!(json["delay_secs"], 7, "the legacy field must remain");
    assert!(json["note"]
        .as_str()
        .unwrap_or_default()
        .contains("restart"));

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn get_config_distinguishes_admin_from_default() {
    // `source` must reflect the *key*, not merely the presence of its section
    // header — a section holding only a sibling key must not read as "admin".
    let (state, _tmp) = config_test_state("[startup]\ndelay_secs = 5\n");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (_status, json) = uds_get(&path, "/config").await;
    let keys = json["keys"].as_array().unwrap();
    let delay = keys
        .iter()
        .find(|k| k["key"] == "startup.delay_secs")
        .unwrap();
    assert_eq!(delay["value"], 5);
    assert_eq!(delay["source"], "admin");

    let poll = keys
        .iter()
        .find(|k| k["key"] == "polling.poll_interval_ms")
        .unwrap();
    assert_eq!(poll["source"], "default", "unset key in an absent section");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn admin_source_is_per_key_not_per_section() {
    // `source` drives the GUI's provenance note ("set in daemon.toml"). Matching
    // on the section header instead of the exact key would make serial.port
    // claim admin provenance merely because [serial] timeout_ms exists — a false
    // statement in the card whose entire job is provenance.
    let (state, _tmp) = config_test_state("[serial]\ntimeout_ms = 600\n");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (_status, json) = uds_get(&path, "/config").await;
    let keys = json["keys"].as_array().unwrap();

    let timeout = keys
        .iter()
        .find(|k| k["key"] == "serial.timeout_ms")
        .unwrap();
    assert_eq!(timeout["source"], "admin", "this key IS set in daemon.toml");

    let port = keys.iter().find(|k| k["key"] == "serial.port").unwrap();
    assert_eq!(
        port["source"], "default",
        "a sibling key in the same section must not confer admin provenance"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn range_bounds_are_inclusive_at_both_edges() {
    // The validators use `..=`; nothing pinned the inclusive edges, so a silent
    // `..` would reject the documented maximum.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    for (route, key, v) in [
        ("/config/poll-interval", "poll_interval_ms", 250u64),
        ("/config/poll-interval", "poll_interval_ms", 2000),
        ("/config/serial-timeout", "timeout_ms", 50),
        ("/config/serial-timeout", "timeout_ms", 1000),
    ] {
        let (status, body) = uds_post(&path, route, &serde_json::json!({key: v})).await;
        assert_eq!(status, 200, "{key}={v} is a documented bound: {body}");
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn detection_opt_in_requires_the_enabled_key() {
    // A `{}` body must not silently persist `false` — that would DISABLE an
    // opt-in the caller believes they enabled.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    for route in ["/config/allow-port-probe", "/config/nvidia-telemetry"] {
        let (status, json) = uds_post(&path, route, &serde_json::json!({})).await;
        assert_eq!(status, 400, "{route} must reject a body with no 'enabled'");
        assert_eq!(json["error"]["code"], "validation_error");
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn get_config_marks_danger_keys_immutable() {
    // An unprivileged client must not be able to move the socket (self-lockout)
    // or the state dir (orphans runtime.toml and the profile store).
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (_status, json) = uds_get(&path, "/config").await;
    let keys = json["keys"].as_array().unwrap();
    for key in ["ipc.socket_path", "state.state_dir"] {
        let entry = keys.iter().find(|k| k["key"] == key).unwrap();
        assert_eq!(entry["mutable"], false, "{key} must be read-only");
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn get_config_flags_the_privilege_gated_opt_ins() {
    // Setting the flag is half the requirement; the drop-in is the other half.
    // The API must say so or a client will claim the feature is on when it isn't.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (_status, json) = uds_get(&path, "/config").await;
    let keys = json["keys"].as_array().unwrap();
    for key in [
        "detection.allow_port_probe",
        "detection.enable_nvidia_telemetry",
    ] {
        let entry = keys.iter().find(|k| k["key"] == key).unwrap();
        assert!(
            entry["requires_privilege"].is_string(),
            "{key} must declare the drop-in requirement"
        );
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn write_then_read_reports_runtime_source_and_restart_pending() {
    // The whole point of the endpoint: a persisted-but-unapplied value must be
    // distinguishable from a live one, and the client must not have to remember
    // what it posted to know that.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, _) = uds_post(
        &path,
        "/config/poll-interval",
        &serde_json::json!({"poll_interval_ms": 1500}),
    )
    .await;
    assert_eq!(status, 200);

    let (_status, json) = uds_get(&path, "/config").await;
    let keys = json["keys"].as_array().unwrap();
    let poll = keys
        .iter()
        .find(|k| k["key"] == "polling.poll_interval_ms")
        .unwrap();
    assert_eq!(poll["value"], 1500, "on-disk value = what a restart gives");
    assert_eq!(
        poll["running_value"], 1000,
        "the process still runs the old one"
    );
    assert_eq!(poll["source"], "runtime");
    assert_eq!(poll["restart_pending"], true);
    assert_eq!(json["restart_pending"], true, "rolled up to the top level");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn poll_interval_rejects_out_of_range() {
    // A tiny interval is a self-inflicted DoS on the hardware the daemon guards.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    for bad in [0, 10, 249, 2_001, 10_000, 999_999] {
        let (status, json) = uds_post(
            &path,
            "/config/poll-interval",
            &serde_json::json!({"poll_interval_ms": bad}),
        )
        .await;
        assert_eq!(status, 400, "poll_interval_ms={bad} must be rejected");
        assert_eq!(json["error"]["code"], "validation_error");
    }

    let (status, _) = uds_post(&path, "/config/poll-interval", &serde_json::json!({})).await;
    assert_eq!(status, 400, "a missing key is a validation error");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn serial_port_is_confined_to_dev() {
    // The daemon opens this path as root. Without confinement an unprivileged
    // client could point it at any file on the system.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    // /dev/shm and /dev/mqueue are the world-writable, symlink-capable dirs
    // under /dev — the old `starts_with("/dev/")` test let them through and only
    // the transport's allowlist caught them. The API now uses that same list.
    for bad in [
        "/etc/shadow",
        "relative/path",
        "/dev/../etc/passwd",
        "",
        "/dev/shm/evil",
        "/dev/mqueue/x",
        "/dev/null",
        "/dev/sda",
    ] {
        let (status, json) = uds_post(
            &path,
            "/config/serial-port",
            &serde_json::json!({"port": bad}),
        )
        .await;
        assert_eq!(status, 400, "serial port {bad:?} must be rejected");
        assert_eq!(json["error"]["code"], "validation_error");
    }

    let (status, _) = uds_post(
        &path,
        "/config/serial-port",
        &serde_json::json!({"port": "/dev/ttyACM0"}),
    )
    .await;
    assert_eq!(status, 200);

    // null clears the override and returns to auto-detection.
    let (status, _) = uds_post(
        &path,
        "/config/serial-port",
        &serde_json::json!({"port": serde_json::Value::Null}),
    )
    .await;
    assert_eq!(status, 200);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn detection_opt_ins_disclose_the_drop_in_requirement() {
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    for route in ["/config/allow-port-probe", "/config/nvidia-telemetry"] {
        let (status, json) = uds_post(&path, route, &serde_json::json!({"enabled": true})).await;
        assert_eq!(status, 200, "{route} should accept a boolean");
        assert!(
            json["requires_privilege"].is_string(),
            "{route} must say the systemd drop-in is still needed"
        );

        let (status, json) = uds_post(&path, route, &serde_json::json!({"enabled": "yes"})).await;
        assert_eq!(status, 400, "{route} must reject a non-boolean");
        assert_eq!(json["error"]["code"], "validation_error");
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn serial_timeout_rejects_out_of_range() {
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    for bad in [0, 49, 1_001, 5_000] {
        let (status, _) = uds_post(
            &path,
            "/config/serial-timeout",
            &serde_json::json!({"timeout_ms": bad}),
        )
        .await;
        assert_eq!(status, 400, "timeout_ms={bad} must be rejected");
    }
    let (status, _) = uds_post(
        &path,
        "/config/serial-timeout",
        &serde_json::json!({"timeout_ms": 750}),
    )
    .await;
    assert_eq!(status, 200);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn advertised_stop_timeout_tracks_the_constant() {
    // DEC-243: this was a hardcoded literal 8 next to a STOP_TIMEOUT constant of
    // 8 s — correct by coincidence and silently wrong the moment the constant
    // moves. Clients size their identify/stop UI timeouts from the advertised
    // value, so a drift there strands the UI waiting on a fan that already
    // restarted (or gives up before it does).
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    assert_eq!(
        json["limits"]["openfan_stop_timeout_s"].as_u64().unwrap(),
        control_ofc_daemon::constants::STOP_TIMEOUT.as_secs(),
        "advertised stop timeout must be derived from STOP_TIMEOUT, not restated"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn live_applied_key_does_not_claim_a_restart_is_owed() {
    // REGRESSION (DEC-243 review): `profiles.search_dirs` was declared
    // requires_restart=true, but its POST handler applies the change live
    // (`*state.profile_search_dirs.write() = ...`). `restart_pending` compares
    // against `running_config`, frozen at startup, so the key latched
    // restart_pending=true forever — and the GUI re-registers its profiles dir
    // on EVERY connect, so essentially every user would see a permanent, and
    // uncleaarable, "restart the daemon" banner for a change already in effect.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    // DEC-205 confines a non-root caller to dirs inside its own home, and the
    // dir must exist — so create a real one there rather than naming /etc.
    let home = std::env::var("HOME").expect("HOME must be set for this test");
    let added = tempfile::tempdir_in(&home).unwrap();
    let added_path = added.path().to_str().unwrap().to_string();

    let (status, body) = uds_post(
        &path,
        "/config/profile-search-dirs",
        &serde_json::json!({"add": [added_path]}),
    )
    .await;
    assert_eq!(status, 200, "search-dir add rejected: {body}");

    let (_status, json) = uds_get(&path, "/config").await;
    let keys = json["keys"].as_array().unwrap();
    let dirs = keys
        .iter()
        .find(|k| k["key"] == "profiles.search_dirs")
        .unwrap();
    assert_eq!(
        dirs["requires_restart"], false,
        "search dirs apply live — declaring otherwise manufactures a false banner"
    );
    assert_eq!(dirs["restart_pending"], false);
    assert_eq!(
        json["restart_pending"], false,
        "and it must not poison the top-level rollup"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn running_value_is_always_present_so_null_is_unambiguous() {
    // REGRESSION (DEC-243 review): `running_value` used to be skipped when equal
    // to `value`, with clients told "absent means same". That is unrepresentable
    // for `serial.port`, the one nullable key: a genuine null running value
    // serialises as `"running_value": null`, indistinguishable from omitted. A
    // client applying the absent-means-same rule then reports the FILE's port as
    // the one in use — on the first-use path of the feature. Always emitting it
    // makes null mean exactly one thing.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, _) = uds_post(
        &path,
        "/config/serial-port",
        &serde_json::json!({"port": "/dev/ttyACM0"}),
    )
    .await;
    assert_eq!(status, 200);

    let (_status, json) = uds_get(&path, "/config").await;
    let keys = json["keys"].as_array().unwrap();
    for key in keys {
        assert!(
            key.get("running_value").is_some(),
            "{} omitted running_value",
            key["key"]
        );
    }

    let port = keys.iter().find(|k| k["key"] == "serial.port").unwrap();
    assert_eq!(port["value"], "/dev/ttyACM0", "the file now names a port");
    assert!(
        port["running_value"].is_null(),
        "the process started with none — this must be an explicit null, not absence"
    );
    assert_eq!(port["restart_pending"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn serial_port_length_is_bounded() {
    // REGRESSION (DEC-243 security review): an unbounded value pushes
    // runtime.toml past the 4 MiB read cap, after which `load_from` treats the
    // file as malformed and reverts EVERY runtime setting to defaults — which
    // the next successful write makes permanent. The request body itself fits
    // under the 4 MiB DefaultBodyLimit, so the body cap does not cover this.
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    let huge = format!("/dev/ttyACM{}", "0".repeat(4096));
    let (status, json) = uds_post(
        &path,
        "/config/serial-port",
        &serde_json::json!({"port": huge}),
    )
    .await;
    assert_eq!(status, 400, "an oversized serial path must be rejected");
    assert_eq!(json["error"]["code"], "validation_error");

    // And the runtime config must still be readable afterwards.
    let (status, _) = uds_get(&path, "/config").await;
    assert_eq!(status, 200);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── DEC-265: POST /fans/openfan/rescan ──

#[tokio::test]
async fn openfan_rescan_short_circuits_when_a_controller_is_already_adopted() {
    // The idempotent path, and the only one testable without a serial device: it
    // must answer from the shared slot WITHOUT probing any tty. A rescan that
    // re-probed while connected would tear down a working controller to
    // rediscover it, and the sole PWM writer would lose its backend mid-tick.
    let state = test_app_state();
    // Adopt a controller the way the rescan handler does.
    struct DeadTransport;
    impl control_ofc_daemon::serial::transport::SerialTransport for DeadTransport {
        fn write_line(&mut self, _d: &str) -> Result<(), control_ofc_daemon::error::SerialError> {
            Ok(())
        }
        fn read_line(
            &mut self,
            _t: std::time::Duration,
        ) -> Result<String, control_ofc_daemon::error::SerialError> {
            Err(control_ofc_daemon::error::SerialError::Timeout { timeout_ms: 1 })
        }
    }
    let ctrl = control_ofc_daemon::serial::controller::FanController::new(
        Box::new(DeadTransport),
        state.cache.clone(),
        std::time::Duration::from_millis(50),
    );
    *state.fan_controller.write() = Some(Arc::new(parking_lot::Mutex::new(ctrl)));

    let (sock_str, _tx, _tmp) = start_test_server(state.clone()).await;

    let (code, body) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    assert_eq!(code, 200, "route must exist and answer: {body}");
    assert_eq!(body["already_connected"], true);
    assert_eq!(
        body["adopted"], false,
        "an already-adopted controller must not be replaced"
    );
}

/// DEC-291: the cooldown is checked BEFORE the already-connected return.
///
/// This ordering was deliberately the other way round until 2026-08-28 — the
/// original comment said so explicitly, so that the common success path never met
/// a cooldown. Reversing it is a knowing trade (see DEC-291), and this test is
/// what stops it being silently reverted by someone reading the old rationale.
///
/// Deterministic despite the host's real serial hardware: the stamped candidate
/// set is computed exactly the way the handler computes it, so the two match by
/// construction whatever is actually plugged in.
#[tokio::test]
async fn openfan_rescan_cooldown_outranks_the_already_connected_return() {
    let state = test_app_state();

    struct DeadTransport;
    impl control_ofc_daemon::serial::transport::SerialTransport for DeadTransport {
        fn write_line(&mut self, _d: &str) -> Result<(), control_ofc_daemon::error::SerialError> {
            Ok(())
        }
        fn read_line(
            &mut self,
            _t: std::time::Duration,
        ) -> Result<String, control_ofc_daemon::error::SerialError> {
            Err(control_ofc_daemon::error::SerialError::Timeout { timeout_ms: 1 })
        }
    }
    let ctrl = control_ofc_daemon::serial::controller::FanController::new(
        Box::new(DeadTransport),
        state.cache.clone(),
        std::time::Duration::from_millis(50),
    );
    *state.fan_controller.write() = Some(Arc::new(parking_lot::Mutex::new(ctrl)));

    // Stamp a rescan that just happened over the SAME ports the handler will see.
    // Built exactly as the handler builds it (DEC-291): NON-opening enumeration,
    // not the probing `auto_detect_port`. Using the probing one here made this
    // test's set differ from the handler's, so the cooldown never fired — which is
    // precisely the confusion DEC-291 removed from production.
    let configured = state.running_config.serial.port.clone();
    let candidates = control_ofc_daemon::serial::adoption::serial_port_candidates_enumerated(
        configured.as_deref(),
        control_ofc_daemon::serial::real_transport::enumerate_serial_candidates,
    );
    *state.last_openfan_rescan.lock() = Some(control_ofc_daemon::api::handlers::LastRescan {
        at: std::time::Instant::now(),
        candidates,
    });

    let (sock_str, _tx, _tmp) = start_test_server(state.clone()).await;
    let (code, body) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;

    assert_eq!(
        code, 409,
        "the cooldown must be checked before the already-connected return: {body}"
    );
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("moments ago"),
        "must be the cooldown refusal, not a leaked single-flight flag: {body}"
    );
    // The message must not claim the earlier probe found nothing — under this
    // ordering a controller may well be connected when the cooldown fires.
    assert!(
        !msg.contains("found nothing"),
        "the refusal states something it cannot know, and which is false here: {body}"
    );
}

#[tokio::test]
async fn openfan_rescan_releases_its_single_flight_flag_when_the_probe_ends() {
    // DEC-266. The flag is set by a CAS in the handler and cleared by a `Drop`
    // guard that now lives in a DETACHED task, so that a client disconnect cannot
    // release it early while the uncancellable probe still holds a tty. The risk
    // of moving it there is the opposite failure: never releasing it. That would
    // wedge this route at 409 for the whole process lifetime — on the one endpoint
    // whose entire purpose is recovering without a restart.
    //
    // Gutting `RescanGuard::drop` leaves every other test in the suite green.
    // This one fails.
    let state = test_app_state();
    let (sock_str, _tx, _tmp) = start_test_server(state.clone()).await;

    let (code, body) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    // No serial hardware in CI, so this is 503 "nothing found" — but assert on the
    // flag, not the outcome, so the test holds on a machine that does have one.
    assert_ne!(
        code, 409,
        "the first rescan cannot conflict with itself: {body}"
    );

    assert!(
        !state
            .openfan_rescanning
            .load(std::sync::atomic::Ordering::SeqCst),
        "the single-flight flag must be clear once the probe has finished"
    );

    // 10-e: clear the cooldown before the second probe. Without this the request
    // below is rejected by the rate limit, which runs BEFORE the single-flight
    // CAS — so it would never reach the flag at all and this test would assert
    // nothing while still looking like it did. Gutting `RescanGuard::drop` must
    // fail here, and it can only do that if the request gets far enough to try
    // the CAS.
    *state.last_openfan_rescan.lock() = None;

    let (code2, body2) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    assert_ne!(
        code2, 409,
        "a second rescan after the first completed must not be rejected as \
         'already in progress' — the flag leaked: {body2}"
    );
}

#[tokio::test]
async fn openfan_rescan_spaces_repeated_probes() {
    // 10-e. `openfan_rescanning` bounds concurrency; nothing bounded repetition,
    // and every probe asserts DTR across each candidate tty — which RESETS
    // Arduino-class boards. So a client looping on a failing rescan was holding
    // unrelated serial hardware in reset, indefinitely.
    //
    // Asserted as an OUTCOME (the second call is refused) rather than by reading
    // the timestamp: a cooldown that records a stamp nothing consults would pass
    // a state assertion and change no behaviour at all.
    let state = test_app_state();
    let (sock_str, _tx, _tmp) = start_test_server(state.clone()).await;

    let (code, body) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    assert_ne!(
        code, 409,
        "the first probe must not be rate-limited: {body}"
    );
    assert!(
        state.last_openfan_rescan.lock().is_some(),
        "a completed probe must stamp the cooldown, or nothing is ever spaced"
    );

    let (code2, body2) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    assert_eq!(
        code2, 409,
        "an immediate second probe must be refused: {body2}"
    );
    // Distinguish the two 409s. They are the same status by design (docs/08's
    // code set is a contract), so only the message separates "too soon" from
    // "already running" — and a test that cannot tell them apart would pass
    // against a cooldown that never fired but a leaked single-flight flag.
    let msg = body2["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("moments ago"),
        "the refusal must be the cooldown, not a leaked single-flight flag: {body2}"
    );

    // The 409 must advertise itself as RETRYABLE. This one clears on its own in
    // seconds and the message says so, so reporting `retryable: false` — the
    // default for `validation_error` — tells a client keying its backoff off that
    // field, which is the field's documented purpose, that the wait is permanent.
    assert_eq!(
        body2["error"]["retryable"], true,
        "a cooldown that expires in seconds must not present as permanent: {body2}"
    );

    // And it must expire rather than latch — a rate limit that never lifts is
    // the same wedged route DEC-266's guard exists to prevent.
    *state.last_openfan_rescan.lock() = Some(control_ofc_daemon::api::handlers::LastRescan {
        at: std::time::Instant::now() - std::time::Duration::from_secs(3600),
        candidates: Vec::new(),
    });
    let (code3, body3) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    assert_ne!(
        code3, 409,
        "the cooldown must lapse, not latch the route closed: {body3}"
    );
}

#[tokio::test]
async fn openfan_rescan_cooldown_yields_when_the_ports_change() {
    // 10-e, round 2. Rate-limiting on elapsed time ALONE refused the single most
    // likely legitimate retry: plug a controller in, click rescan. That is a
    // human action measured in seconds, so the device went unadopted and the GUI
    // showed nothing — transiently re-opening the "restart the daemon" mis-advice
    // DEC-265/266 exists to remove, on the one endpoint whose whole purpose is
    // recovery without a restart.
    //
    // The cooldown therefore applies only while the candidate port set is
    // UNCHANGED. A newly attached controller enumerates a new tty, so the sets
    // differ and the retry proceeds at once.
    let state = test_app_state();
    let (sock_str, _tx, _tmp) = start_test_server(state.clone()).await;

    let (code, body) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    assert_ne!(
        code, 409,
        "the first probe must not be rate-limited: {body}"
    );

    // Establish the PRESENCE of the cooldown before asserting it yields —
    // otherwise this passes against a build where the cooldown never fires at
    // all, proving nothing about the bypass (DEC-272).
    let (blocked, blocked_body) =
        uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    assert_eq!(
        blocked, 409,
        "precondition: an unchanged port set must still be spaced: {blocked_body}"
    );

    // Now claim the last probe walked a DIFFERENT set — the state a freshly
    // attached controller produces — with the clock left well inside the window.
    *state.last_openfan_rescan.lock() = Some(control_ofc_daemon::api::handlers::LastRescan {
        at: std::time::Instant::now(),
        candidates: vec!["/dev/ttyUSB-was-not-here-before".to_string()],
    });
    let (code2, body2) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;
    assert_ne!(
        code2, 409,
        "a changed candidate set must bypass the cooldown — otherwise plugging a \
         controller in and rescanning immediately is refused, which is exactly \
         the recovery this endpoint exists for: {body2}"
    );
}

#[tokio::test]
async fn openfan_rescan_rejects_a_second_rescan_while_one_is_running() {
    // The other half: the CAS must actually reject. Two concurrent probes would
    // open the same tty, and the loser would install a controller over the
    // winner's — leaving the engine writing through one transport while an
    // orphaned poll loop read another.
    let state = test_app_state();
    state
        .openfan_rescanning
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (sock_str, _tx, _tmp) = start_test_server(state.clone()).await;

    let (code, body) = uds_post(&sock_str, "/fans/openfan/rescan", &serde_json::json!({})).await;

    assert_eq!(
        code, 409,
        "a rescan already in flight must be rejected: {body}"
    );
    assert_eq!(body["error"]["code"], "validation_error");
    assert!(
        state
            .openfan_rescanning
            .load(std::sync::atomic::Ordering::SeqCst),
        "a rejected rescan must NOT clear the flag the in-flight one owns"
    );
}

#[tokio::test]
async fn capabilities_advertise_the_openfan_rescan_route() {
    // The GUI hides the action unless this is true, so an unadvertised route is
    // an unreachable one — and a client defaulting the missing field to false is
    // exactly how an older daemon is meant to read.
    let state = test_app_state();
    let (sock_str, _tx, _tmp) = start_test_server(state).await;

    let (code, body) = uds_get(&sock_str, "/capabilities").await;
    assert_eq!(code, 200);
    assert_eq!(body["control"]["openfan_rescan"], true);
}

// ── PWM/RPM characterisation (AIO-MB Phase 3) ────────────────────────

/// Poll `GET /diagnostics/characterization` until the run leaves `running`.
/// Bounded so a wedged sweep fails the test instead of hanging CI (the
/// tokio-test trap recorded in CLAUDE.md: an unbounded wait turns a red test
/// into a hung job).
async fn await_characterization(path: &str) -> serde_json::Value {
    for _ in 0..100 {
        let (status, json) = uds_get(path, "/diagnostics/characterization").await;
        assert_eq!(status, 200, "status endpoint should serve a started run");
        if json["state"] != "running" {
            return json;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("characterisation run never reached a terminal state");
}

/// Build the hwmon fixture with a deliberately short hwmon lease TTL.
///
/// The real TTL is 60 s, which no test can wait out. Shortening it is what makes
/// the lease-renewal call site testable at all.
fn test_app_state_with_short_lease(ttl: std::time::Duration) -> Arc<AppState> {
    let state = test_app_state_with_hwmon();
    if let Some(ctrl) = state.hwmon_controller.as_ref() {
        *ctrl.lock().lease_manager_mut() = LeaseManager::with_ttl(ttl);
    }
    state
}

/// [SAFETY] **The call-site test for the lease renewal.**
///
/// `run_sweep`'s unit test proves it calls `keepalive` once per point. It does
/// NOT prove the *handler's* keepalive renews the hwmon lease — that is the
/// "extracting a rule into a testable function does not test the call site"
/// trap, which this project has hit five times. This drives the real handler
/// through a sweep several times longer than the lease TTL.
///
/// The assertion that matters is `state == "complete"`: an expired lease fails
/// every point write, so a sweep that walked all three points and finished is a
/// sweep whose lease was renewed under it.
///
/// **What this does NOT prove, stated because the comment here used to claim it
/// did (`AUD2-c2`):** that the *restore* write survives the TTL. This fixture's
/// `pwm_path` is a real `/sys` path that does not exist under test, so
/// `original_pct` is `None` and the guard has nothing to write back — which the
/// `restore_outcome` below now says out loud, and which `restore_failed: false`
/// previously concealed. Covered instead by the unit test
/// `characterization::tests::the_restore_write_lands_while_the_lease_is_still_valid`.
#[tokio::test]
async fn characterize_renews_the_hwmon_lease_across_a_long_sweep() {
    let state = test_app_state_with_short_lease(std::time::Duration::from_secs(3));
    let (path, shutdown, _dir) = start_test_server(state).await;

    // 3 points x 2 s = ~6 s, twice the lease TTL, with renewals ~2 s apart.
    let (status, json) = uds_post(
        &path,
        "/hwmon/h1/characterize",
        &serde_json::json!({"points_pct": [40, 60, 80], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(status, 202, "{json}");

    let done = await_characterization(&path).await;
    assert_eq!(
        done["state"], "complete",
        "the sweep outlived the lease TTL and must have renewed it: {done}"
    );
    // `AUD2-c`: the fixture cannot read a pre-sweep duty, so there is nothing to
    // restore — and the run must SAY so rather than publishing the `false` that
    // used to read as "the header is back where it was".
    assert_eq!(
        done["restore_outcome"], "no_original_duty",
        "an unreadable pre-sweep duty must be reported, not silently passed off \
         as a successful restore: {done}"
    );
    assert_eq!(
        done["restore_failed"], true,
        "…and the boolean is derived from that reason, so it must agree: {done}"
    );
    assert_eq!(done["points"].as_array().unwrap().len(), 3);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn characterize_refused_when_hot() {
    let state = test_app_state_with_hwmon();
    make_hot(&state);
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(&path, "/hwmon/h1/characterize", &serde_json::json!({})).await;

    assert_eq!(status, 409, "{json}");
    assert_eq!(json["error"]["code"], "thermal_abort");
    assert_eq!(json["error"]["retryable"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// `AUD2-c` at the CALL SITE: the skip branches live in `RestoreOnDrop`, but the
/// two wire fields are derived in the handler's terminal publish. A unit test of
/// the guard proves the reason is *recorded*; only this proves it is *published*.
///
/// The ladder starts forcing AFTER the 202, which is the only way to reach the
/// mid-sweep skip — a ladder already forcing refuses the POST outright
/// (`characterize_refused_while_thermal_safety_is_forcing`, below).
#[tokio::test]
async fn characterize_reports_a_thermally_skipped_restore_rather_than_a_success() {
    let state = test_app_state_with_hwmon();
    let cache = state.cache.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // 3 x 2 s: the sweep is still inside its first settle when the ladder starts.
    let (status, json) = uds_post(
        &path,
        "/hwmon/h1/characterize",
        &serde_json::json!({"points_pct": [40, 60, 80], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(status, 202, "{json}");
    assert_eq!(
        json["restore_outcome"], "pending",
        "a live run has no outcome yet"
    );

    cache.record_engine_tick(
        "emergency",
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
    );

    let done = await_characterization(&path).await;
    assert_eq!(done["state"], "aborted", "{done}");
    assert_eq!(
        done["restore_outcome"], "skipped_thermal_force",
        "the restore was skipped because the ladder outranks a diagnostic: {done}"
    );
    assert_eq!(
        done["restore_failed"], true,
        "…and the boolean must agree with it — the header is NOT back where the \
         sweep found it: {done}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn characterize_refused_while_thermal_safety_is_forcing() {
    // The 80-85 °C band, and the `no_sensor_fallback` case: cool enough to pass
    // the temperature test while the ladder is still forcing every fan. The
    // fixture is at a normal temperature on purpose, so this cannot pass by
    // keying on temperature.
    let state = test_app_state_with_hwmon();
    state.cache.record_engine_tick(
        "emergency",
        control_ofc_daemon::constants::THERMAL_EMERGENCY_TRIGGER_C,
    );
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(&path, "/hwmon/h1/characterize", &serde_json::json!({})).await;

    assert_eq!(status, 409, "{json}");
    assert_eq!(json["error"]["code"], "validation_error");
    assert_eq!(json["error"]["retryable"], true);

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn characterize_unknown_header_is_404_and_no_controller_is_503() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;
    let (status, json) = uds_post(
        &path,
        "/hwmon/nope:missing/characterize",
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 404, "{json}");
    assert_eq!(json["error"]["code"], "validation_error");
    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);

    let (path2, shutdown2, _dir2) = start_test_server(test_app_state()).await;
    let (status2, json2) = uds_post(&path2, "/hwmon/h1/characterize", &serde_json::json!({})).await;
    assert_eq!(status2, 503, "{json2}");
    assert_eq!(json2["error"]["code"], "hardware_unavailable");
    let _ = shutdown2.send(());
    let _ = std::fs::remove_file(&path2);
}

/// [SAFETY] **The call-site test.** `resolve_points` having correct arithmetic
/// is not evidence the handler calls it, still less that it passes the pump
/// floor — this project has shipped that exact gap five times (CLAUDE.md:
/// "extracting a rule into a testable function does NOT test the call site").
///
/// Assigns `pump` to a header the hardware labels `CHA_FAN1`, asks for points
/// the daemon must refuse, and reads the clamped list back off the 202.
#[tokio::test]
async fn characterize_clamps_a_user_assigned_pump_to_the_hard_floor() {
    let state = test_app_state_with_hwmon();
    {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "h1".to_string(),
            control_ofc_daemon::hwmon::roles::HeaderRole::Pump,
        );
        *state.header_roles.write() = Arc::new(map);
    }
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/hwmon/h1/characterize",
        &serde_json::json!({"points_pct": [0, 5, 10, 25, 100], "settle_seconds": 2}),
    )
    .await;

    assert_eq!(status, 202, "{json}");
    let pts: Vec<u64> = json["requested_points_pct"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert!(
        pts.iter().all(|p| *p >= 30),
        "a pump-assigned header must never be swept below the 30% floor: {pts:?}"
    );
    assert!(!pts.contains(&0), "0% must be unreachable: {pts:?}");
    assert!(
        pts.windows(2).all(|w| w[0] < w[1]),
        "points must be ascending so an abort leaves the header high: {pts:?}"
    );
    assert_eq!(json["state"], "running");
    assert_eq!(json["settle_seconds"], 2);

    await_characterization(&path).await;
    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// [SAFETY] The second half of the call-site test: the terminal snapshot must
/// carry a `summary`, which only `characterization::summarise` produces. A
/// handler that derived verdicts inline — or forgot to summarise at all — would
/// pass every pure unit test in the module and fail here.
#[tokio::test]
async fn characterize_publishes_a_summary_derived_by_summarise() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, _) = uds_post(
        &path,
        "/hwmon/h1/characterize",
        &serde_json::json!({"points_pct": [40, 80], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(status, 202);

    let done = await_characterization(&path).await;
    assert_eq!(done["header_id"], "h1");
    assert!(
        done["summary"].is_object(),
        "a finished run must carry the derived summary: {done}"
    );
    // The three axes must be present and separate — collapsing them into one
    // pass/fail is the defect `AIO-Phase3.md` names explicitly.
    for axis in ["command_acceptance", "pwm_readback", "rpm_response"] {
        assert!(
            done["summary"][axis].is_string(),
            "summary is missing the {axis} axis: {done}"
        );
    }
    assert!(done["summary"]["possible_device_override"].is_boolean());
    assert!(done["summary"]["interference_detected"].is_boolean());

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// A characterisation claims the same single-flight slot as verify and
/// calibrate, so the three cannot drive hardware at once.
#[tokio::test]
async fn characterize_is_single_flight_against_itself_and_verify() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (first, _) = uds_post(
        &path,
        "/hwmon/h1/characterize",
        &serde_json::json!({"points_pct": [50], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(first, 202);

    let (again, j2) = uds_post(
        &path,
        "/hwmon/h2/characterize",
        &serde_json::json!({"points_pct": [50], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(again, 409, "a second sweep must be refused: {j2}");

    let (verify, j3) = uds_post(&path, "/hwmon/h2/verify", &serde_json::json!({})).await;
    assert_eq!(verify, 409, "a verify must be refused mid-sweep: {j3}");

    await_characterization(&path).await;
    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// The single-flight slot, from the ordinary direction: a second run is refused
/// until the first has fully published, and once accepted it carries its OWN id
/// and only its own points.
///
/// **This test does NOT exercise the `run_id` fence, and used to claim it did**
/// (register row `AUD2-i`). It awaits a terminal state before POSTing the
/// second run, so the two never coexist — delete `r.run_id == my_run_id` from
/// both fences in `hwmon_ctl.rs` and it passes unchanged, because run 1's task
/// has already finished when run 2 installs itself. The defect the fence closes
/// requires a supersession while run 1 is still finishing, which only the
/// DEC-296 expired-deadman steal permits. That case is covered by
/// [`a_superseded_run_cannot_publish_over_the_run_that_replaced_it`].
#[tokio::test]
async fn a_second_run_never_inherits_the_first_runs_points() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (s1, j1) = uds_post(
        &path,
        "/hwmon/h1/characterize",
        &serde_json::json!({"points_pct": [40, 60], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(s1, 202, "{j1}");
    let first_id = j1["run_id"].as_str().unwrap().to_string();

    let first = await_characterization(&path).await;
    assert_eq!(first["run_id"], first_id);
    assert_eq!(first["points"].as_array().unwrap().len(), 2);

    let (s2, j2) = uds_post(
        &path,
        "/hwmon/h2/characterize",
        &serde_json::json!({"points_pct": [80], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(
        s2, 202,
        "the slot must be free once the first run published: {j2}"
    );
    let second_id = j2["run_id"].as_str().unwrap().to_string();
    assert_ne!(second_id, first_id, "each run gets its own id");

    let second = await_characterization(&path).await;
    assert_eq!(
        second["run_id"], second_id,
        "the slot must hold the NEW run"
    );
    assert_eq!(second["header_id"], "h2");
    assert_eq!(
        second["points"].as_array().unwrap().len(),
        1,
        "the first run's points must not leak into the second: {second}"
    );
    assert_eq!(second["requested_points_pct"], serde_json::json!([80]));

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// [SAFETY] **The call-site test for the `run_id` fence** (register row `AUD2-i`).
///
/// Two characterisation runs can only ever coexist through one door: the DEC-296
/// steal, where a claim whose deadman elapsed is taken from under a holder that
/// is still alive. The fence at `hwmon_ctl.rs`'s per-point `publish` and its
/// terminal write is what stops the loser writing its points, its state and its
/// `detail` over the winner — reporting a live sweep as finished, with another
/// header's data, and leaving the run actually driving hardware uncancellable.
///
/// **The damage is transient, so the assertion has to be an invariant held
/// across the whole window rather than a check of the final state.** Run B's own
/// terminal write repairs `state` and `points` afterwards, so a snapshot taken
/// at the end passes with both fences deleted; what never repairs is what the
/// slot said *while B was still running*. Hence the polling loop below, which
/// asserts on every observation.
///
/// **Three preconditions, and each exists because without it the test loses
/// coverage silently rather than failing** (all three were raised by
/// `ofc:concurrency-reviewer` against the first draft):
///
///  1. **A must have landed its first write before the steal.** `run_sweep`
///     calls `keepalive()` at the *top* of each point, ahead of the write. If
///     A's task has not been polled when the steal lands, A's very first
///     keepalive fails, it aborts at index 0 with `measured` empty, and
///     **`publish` is never called at all** — the per-point fence would then be
///     untested and deletable with this test green. Waiting for
///     `last_commanded_pct("h1") == Some(40)` proves A cleared its keepalive
///     *and* its write, so a point-0 publish must follow.
///  2. **B's claim must be a steal, not a free take.** If A had already
///     terminated (any of `run_sweep`'s early exits), B would claim an unheld
///     slot, the runs would never coexist, and every assertion below would pass
///     vacuously. `assert_ne!(a_id, b_id)` does not discriminate that, so A is
///     asserted still `running` immediately before each attempt.
///  3. **The loop must still be polling when A tries to publish.** It exits on
///     B's first terminal snapshot, so coverage would otherwise rest on the
///     unasserted 2 s-vs-4 s settle asymmetry — change B's settle, or let A slip
///     under load, and the window is missed with `saw_b_running` still true.
///     The loop therefore keeps polling until A's whole sweep is provably over.
///
/// No `sleep`-based *assertion*: every wait is a bounded poll with a deadline,
/// and the sweep is driven by `tokio::time`, so nothing here can hang CI
/// (CLAUDE.md tokio traps 1-3).
#[tokio::test]
async fn a_superseded_run_cannot_publish_over_the_run_that_replaced_it() {
    // Sysfs paths under a directory that does not exist, rather than
    // `make_test_header`'s real `/sys/class/hwmon/hwmon0/pwm1`: reads are meant
    // to fail here (that is what the fixture has always relied on), and pointing
    // them at the host would make a fence assertion depend on what `hwmon0`
    // happens to be on the machine running the test.
    let headers = ["h1", "h2"]
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut h = make_test_header(id, "CHA_FAN1", 20);
            let base = format!("/nonexistent/control-ofc-test/hwmon{i}");
            h.pwm_path = format!("{base}/pwm1");
            h.enable_path = Some(format!("{base}/pwm1_enable"));
            h.rpm_path = Some(format!("{base}/fan1_input"));
            h
        })
        .collect();
    let state = test_app_state_with_headers(headers);
    let cache = state.cache.clone();
    let controller = state
        .hwmon_controller
        .clone()
        .expect("the hwmon fixture always installs a controller");
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Run A: the loser. Two points, so it reaches a second keepalive and learns
    // it was superseded; the minimum settle, so it is parked and harmless.
    let (s1, j1) = uds_post(
        &path,
        "/hwmon/h1/characterize",
        &serde_json::json!({"points_pct": [40, 60], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(s1, 202, "{j1}");
    let a_id = j1["run_id"].as_str().unwrap().to_string();
    let a_started = tokio::time::Instant::now();

    // Precondition 1: A is inside its settle, past keepalive and past its write.
    let mut a_wrote = false;
    for _ in 0..100 {
        if controller.lock().last_commanded_pct("h1") == Some(40) {
            a_wrote = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        a_wrote,
        "run A never landed its first point — without that it aborts before \
         calling `publish` at all, and the per-point fence goes untested"
    );

    // Run B: the winner. A longer settle so it is still RUNNING when A wakes up
    // and tries to publish over it — which is the only window in which the
    // per-point fence is observable at all.
    let mut b: Option<(String, serde_json::Value)> = None;
    for _ in 0..40 {
        // Precondition 2: the slot B is about to take is one A still holds.
        let (_, live) = uds_get(&path, "/diagnostics/characterization").await;
        assert_eq!(live["run_id"], a_id, "A must still own the slot: {live}");
        assert_eq!(
            live["state"], "running",
            "A must still be running, or B takes a free slot and the two runs \
             never coexist: {live}"
        );

        cache.expire_verify_claim_for_test();
        let (code, json) = uds_post(
            &path,
            "/hwmon/h2/characterize",
            &serde_json::json!({"points_pct": [80], "settle_seconds": 4}),
        )
        .await;
        if code == 202 {
            b = Some((json["run_id"].as_str().unwrap().to_string(), json));
            break;
        }
        assert_eq!(
            code, 409,
            "the only legitimate refusal is single-flight: {json}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (b_id, b_json) = b.expect(
        "run B never stole the expired claim — the DEC-296 steal is the only way \
         two runs coexist, and without it this test proves nothing",
    );
    assert_ne!(a_id, b_id, "each run gets its own id: {b_json}");

    // A's whole sweep: 2 points x 2 s of settle, plus margin. The loop must not
    // stop before this has elapsed, or it can miss A's publish attempts.
    let a_sweep_over = a_started + std::time::Duration::from_secs(6);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut saw_b_running = false;
    let mut b_terminal: Option<serde_json::Value> = None;
    while tokio::time::Instant::now() < deadline {
        let (status, snap) = uds_get(&path, "/diagnostics/characterization").await;
        assert_eq!(status, 200, "the slot must stay served: {snap}");

        assert_eq!(
            snap["run_id"], b_id,
            "A must never install itself over B: {snap}"
        );
        assert_eq!(
            snap["header_id"], "h2",
            "A's header must never appear: {snap}"
        );
        for point in snap["points"].as_array().unwrap() {
            assert_eq!(
                point["requested_pct"], 80,
                "A's points must never land in B's list: {snap}"
            );
        }
        // The exact discriminator for the terminal fence: with it removed, A's
        // outcome lands here verbatim. Matched on A's own text rather than on
        // `detail.is_null()`, so an abort from any other cause is not mistaken
        // for a fence failure.
        let detail = snap["detail"].as_str().unwrap_or("");
        assert!(
            !detail.contains("superseded"),
            "A's abort reason must never be attributed to B: {snap}"
        );

        if snap["state"] == "running" {
            saw_b_running = true;
        } else if tokio::time::Instant::now() >= a_sweep_over {
            // Precondition 3: B finished AND A's sweep is provably over, so the
            // loop above observed every window in which A could have published.
            b_terminal = Some(snap);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(saw_b_running, "B must have been observed mid-sweep");
    let done = b_terminal.expect("B never reached a terminal state within the deadline");
    assert_eq!(done["requested_points_pct"], serde_json::json!([80]));
    assert_eq!(done["run_id"], b_id, "{done}");

    // And a late write from A is still refused after everything has settled.
    let (recheck, after) = uds_get(&path, "/diagnostics/characterization").await;
    assert_eq!(recheck, 200);
    assert_eq!(
        after["run_id"], b_id,
        "a late write from A must still be refused: {after}"
    );
    assert!(
        !after["detail"]
            .as_str()
            .unwrap_or("")
            .contains("superseded"),
        "a late write from A must still be refused: {after}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// [SAFETY] `expire_verify_claim_for_test` must stay test-only, and this is what
/// makes that a rule rather than a request.
///
/// It is an ungated `pub fn` compiled into the production binary, and calling it
/// does two things: it un-pauses the profile engine's write phase while a
/// diagnostic is still driving a header, and it makes the single-flight slot
/// stealable — which the `run_id` fence makes safe for characterisation only,
/// since the hwmon verify, the GPU verify and calibrate share that slot with no
/// run id to fence on. Nothing under `daemon/src/` may call it.
///
/// Matched in **call position** (`.expire_verify_claim_for_test(`), not as a bare
/// substring: the definition and its own doc comment name it, and a substring
/// scan would match those and be permanently red. Same trap as the `polling.rs`
/// self-scanning guard recorded in `CLAUDE.md`.
#[test]
fn the_verify_deadman_test_seam_has_no_production_caller() {
    let mut offenders = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src"
    ))];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("daemon/src must be readable") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let body = std::fs::read_to_string(&path).expect("readable source file");
                if body.contains(".expire_verify_claim_for_test(") {
                    offenders.push(path);
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "`expire_verify_claim_for_test` is a test-only seam and must have no \
         production caller, but it is called from: {offenders:?}"
    );
}

#[tokio::test]
async fn characterize_status_is_404_before_any_run_and_cancel_is_409() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/diagnostics/characterization").await;
    assert_eq!(status, 404, "{json}");

    let (cancel, cj) = uds_delete(&path, "/diagnostics/characterization").await;
    assert_eq!(cancel, 409, "nothing to cancel: {cj}");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn characterize_cancel_stops_the_run_and_reports_cancelled() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, _) = uds_post(
        &path,
        "/hwmon/h1/characterize",
        &serde_json::json!({"points_pct": [30, 50, 70, 90], "settle_seconds": 2}),
    )
    .await;
    assert_eq!(status, 202);

    let (cancel, cj) = uds_delete(&path, "/diagnostics/characterization").await;
    assert_eq!(cancel, 202, "{cj}");

    let done = await_characterization(&path).await;
    assert_eq!(
        done["state"], "cancelled",
        "a cancelled sweep must say so rather than reporting a partial pass: {done}"
    );
    assert!(
        done["points"].as_array().unwrap().len() < 4,
        "cancellation must actually stop the sweep early: {done}"
    );
    assert!(
        done["summary"].is_object(),
        "a cancelled run still summarises what it measured"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn capabilities_advertises_pwm_characterization() {
    let state = test_app_state_with_hwmon();
    let (path, shutdown, _dir) = start_test_server(state).await;
    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    assert_eq!(
        json["control"]["pwm_characterization"], true,
        "clients gate the whole feature on this flag rather than probing"
    );
    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── AIO-MB Phase 4 (DEC-316): cooling-device topology + capability audit ─────

#[tokio::test]
async fn capabilities_advertise_cooling_devices() {
    // Gates the three topology endpoints. A client must branch on this rather
    // than probing: an older daemon 404s the POST, which is the same status the
    // route returns for an unknown device id on DELETE.
    let state = test_app_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_get(&path, "/capabilities").await;
    assert_eq!(status, 200);
    assert_eq!(
        json["control"]["cooling_devices"], true,
        "this daemon exposes cooling-device topology: {json}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn cooling_device_round_trips_through_the_api() {
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    // Nothing configured yet — an empty list, not an error, and the shipped
    // policies are advertised so a client need not hardcode them.
    let (status, json) = uds_get(&path, "/inventory/cooling-devices").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["cooling_devices"].as_array().unwrap().len(), 0);
    let policies = json["available_policies"].as_array().unwrap();
    assert!(
        policies.iter().any(|p| p["id"] == "generic_pump"),
        "the generic pump policy must be advertised: {json}"
    );

    // The brief's topology requirement: one pump, several radiator fans, and
    // no coolant sensor — which is the normal motherboard-AIO case.
    let (status, json) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({
            "id": "aio-1",
            "name": "AIO Cooling System",
            "kind": "aio_liquid",
            "pump_member": "hwmon:it8696:isa-0a40:pwm5:PUMP",
            "radiator_members": [
                "hwmon:it8696:isa-0a40:pwm1:CPU_FAN",
                "hwmon:it8696:isa-0a40:pwm2:CPU_OPT"
            ]
        }),
    )
    .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["updated"], true);

    let (status, json) = uds_get(&path, "/inventory/cooling-devices").await;
    assert_eq!(status, 200);
    let dev = &json["cooling_devices"][0];
    assert_eq!(dev["id"], "aio-1");
    assert_eq!(dev["kind"], "aio_liquid");
    assert_eq!(dev["radiator_members"].as_array().unwrap().len(), 2);
    assert_eq!(
        dev["coolant_telemetry"], "unavailable",
        "a missing coolant sensor is a supported configuration, not an error"
    );
    assert_eq!(
        dev["device_policy"]["id"], "generic_pump",
        "a device naming no policy resolves to the conservative default"
    );
    assert_eq!(dev["device_policy"]["supports_stop"], false);

    // Same id replaces rather than duplicating.
    let (status, _) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({ "id": "aio-1", "name": "Renamed" }),
    )
    .await;
    assert_eq!(status, 200);
    let (_, json) = uds_get(&path, "/inventory/cooling-devices").await;
    assert_eq!(json["cooling_devices"].as_array().unwrap().len(), 1);
    assert_eq!(json["cooling_devices"][0]["name"], "Renamed");

    // Delete, then a second delete is a 404 rather than a silent success.
    let (status, _) = uds_delete(&path, "/config/cooling-device/aio-1").await;
    assert_eq!(status, 200);
    let (status, json) = uds_delete(&path, "/config/cooling-device/aio-1").await;
    assert_eq!(status, 404, "{json}");
    assert_eq!(json["error"]["code"], "not_found");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// [SAFETY] `AIO7-d`, the CALL-SITE test — the helper's own honesty test lives in
/// `device_policy::tests` and would not have caught a bad call site.
///
/// Reproduces the divergence as measured on an X870E AORUS MASTER: a
/// `radiator_members` entry of a cooling device whose policy resolves to
/// `GENERIC_PUMP` was published `stop_permitted: false` — inheriting the
/// *device's* policy — while identify branches on `header_is_pump_protected`,
/// which has no membership term, and would stop it. The GUI reads
/// `not stop_permitted` as that predicate (`services/pump_protection.py`), so the
/// wire was telling every client a fan was protected that the daemon would stop.
#[tokio::test]
async fn a_radiator_member_is_not_published_as_unstoppable() {
    let (state, tmp) = config_test_state_with_hwmon();
    let state_ref = state.clone();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // h1 is the pump (role assigned); h2 is a radiator fan with no role at all.
    let (status, json) = uds_post(
        &path,
        "/config/header-role",
        &serde_json::json!({"header_id": "h1", "role": "pump"}),
    )
    .await;
    assert_eq!(status, 200, "{json}");
    let (status, json) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({
            "id": "aio-1",
            "name": "AIO",
            "kind": "aio_liquid",
            "pump_member": "h1",
            "radiator_members": ["h2"]
        }),
    )
    .await;
    assert_eq!(status, 200, "{json}");

    let (status, json) = uds_get(&path, "/hwmon/headers").await;
    assert_eq!(status, 200, "{json}");
    let headers = json["headers"].as_array().expect("headers array");
    let find = |id: &str| {
        headers
            .iter()
            .find(|h| h["id"] == id)
            .unwrap_or_else(|| panic!("header {id} missing from {json}"))
    };

    // The pump: protected by its assigned role, so unstoppable — unchanged.
    assert_eq!(
        find("h1")["stop_permitted"],
        serde_json::json!(false),
        "the pump must still publish as unstoppable"
    );
    // The radiator fan: NOT pump-protected, so the wire must say so rather than
    // inheriting the device's pump policy.
    assert_eq!(
        find("h2")["stop_permitted"],
        serde_json::json!(true),
        "a radiator member with no pump role is published as unstoppable while \
         identify would stop it — the wire must report what the daemon does"
    );

    // And the published field must agree with the predicate identify obeys, for
    // BOTH members. This is the relationship, not a literal: it is what stays
    // true if the roles or the policy change.
    for id in ["h1", "h2"] {
        let published_stoppable = find(id)["stop_permitted"].as_bool().unwrap();
        let protected = state_ref.header_is_pump_protected(id);
        assert_eq!(
            published_stoppable, !protected,
            "{id}: wire says stoppable={published_stoppable} but identify's \
             predicate header_is_pump_protected={protected}"
        );
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
    drop(tmp);
}

/// **The brief's trust-model requirement**: "a normal user profile must not be
/// able to submit `minimum_safe_pwm = 1` and bypass pump protections."
///
/// Two independent defences, and this asserts both. `DevicePolicy` derives no
/// `Deserialize`, so the number is unconstructible from a payload — that is the
/// compile-time half, pinned by a unit test. This is the runtime half: the
/// endpoint rejects the key by name rather than ignoring it, because a caller
/// that believes it tightened a pump floor when it did not is the more
/// dangerous outcome.
#[tokio::test]
async fn a_cooling_device_payload_cannot_set_a_safety_number() {
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    for key in [
        "minimum_safe_pwm",
        "minimum_safe_pwm_pct",
        "supports_stop",
        "startup_override_seconds",
        "effective_min_pwm_pct",
        "stop_permitted",
        "device_policy",
    ] {
        let mut body = serde_json::json!({ "id": "aio-1" });
        body[key] = serde_json::json!(1);
        let (status, json) = uds_post(&path, "/config/cooling-device", &body).await;
        assert_eq!(status, 400, "'{key}' must be rejected, got: {json}");
        assert_eq!(json["error"]["code"], "validation_error");
    }

    // A policy id this daemon does not ship is rejected too — a client must not
    // be able to name a relaxed policy into existence.
    let (status, json) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({ "id": "aio-1", "device_policy_id": "nl-lc1-validated-20pct" }),
    )
    .await;
    assert_eq!(status, 400, "{json}");

    // Nothing was stored by any of the rejected calls.
    let (_, json) = uds_get(&path, "/inventory/cooling-devices").await;
    assert_eq!(
        json["cooling_devices"].as_array().unwrap().len(),
        0,
        "a rejected payload must store nothing: {json}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn cooling_device_rejects_malformed_topology() {
    let (state, _tmp) = config_test_state("");
    let (path, shutdown, _dir) = start_test_server(state).await;

    for (body, why) in [
        (serde_json::json!({}), "missing id"),
        (serde_json::json!({ "id": "" }), "empty id"),
        (serde_json::json!({ "id": "has/slash" }), "path-unsafe id"),
        (serde_json::json!({ "id": ".." }), "dot-dot id"),
        (
            serde_json::json!({ "id": "a", "kind": "thermosiphon" }),
            "unknown kind",
        ),
        (
            serde_json::json!({ "id": "a", "radiator_members": [""] }),
            "empty member id",
        ),
        (
            serde_json::json!({ "id": "a", "radiator_members": "not-an-array" }),
            "member list of the wrong shape",
        ),
        (
            serde_json::json!({
                "id": "a",
                "pump_member": "hwmon:x:pwm1:PUMP",
                "radiator_members": ["hwmon:x:pwm1:PUMP"]
            }),
            "one header claimed twice",
        ),
    ] {
        let (status, json) = uds_post(&path, "/config/cooling-device", &body).await;
        assert_eq!(status, 400, "{why} must be rejected, got: {json}");
        assert_eq!(json["error"]["code"], "validation_error", "{why}");
    }

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── AUD3-h: a cooling device may name an OpenFan fan, not hwmon headers alone ──

/// A state that has discovered BOTH sources, plus a writable runtime config.
///
/// Both halves are load-bearing. `config_test_state` has no hwmon controller, so
/// the member check short-circuits on its documented "nothing discovered" escape
/// and never runs — which is precisely why the existing Phase 4 tests pass while
/// naming header ids the daemon has never seen, and why they could not have
/// caught `AUD3-h`.
fn cooling_device_test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = test_app_state_with_hwmon();
    {
        let inner = Arc::get_mut(&mut state).unwrap();
        inner.runtime_config_path = tmp.path().join("runtime.toml");
    }
    state.cache.update_openfan_fans(vec![OpenFanState {
        channel: 0,
        rpm: 1200,
        last_commanded_pwm: Some(128),
        updated_at: Instant::now(),
        rpm_polled: true,
    }]);
    (state, tmp)
}

/// **The regression test for `AUD3-h`, at the call site.**
///
/// The GUI's radiator picker offers OpenFan outputs alongside writable hwmon
/// headers (`build_radiator_candidates`), and the wizard posts the selection
/// verbatim. The handler validated every member against hwmon PWM headers only,
/// so on any machine with hwmon headers — i.e. every motherboard-AIO machine,
/// the hardware this feature exists for — Apply failed with
/// `400 unknown hwmon header id: openfan:ch00`, naming an id the GUI itself chose.
#[tokio::test]
async fn a_cooling_device_accepts_an_openfan_radiator_member() {
    let (state, _tmp) = cooling_device_test_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    // The precondition that makes this test non-vacuous: hwmon IS discovered, so
    // the member check genuinely runs. Without this a blanket skip would pass.
    let (status, json) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({ "id": "probe", "pump_member": "definitely-not-a-header" }),
    )
    .await;
    assert_eq!(
        status, 400,
        "hwmon must be discovered for this test to prove anything: {json}"
    );

    // The actual case: a pump on a motherboard header, radiators split across an
    // hwmon header and an OpenFan channel.
    let (status, json) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({
            "id": "aio-1",
            "name": "AIO Cooling System",
            "kind": "aio_liquid",
            "pump_member": "h2",
            "radiator_members": ["h1", "openfan:ch00"]
        }),
    )
    .await;
    assert_eq!(
        status, 200,
        "an OpenFan radiator fan is a member the GUI offers and must be accepted: {json}"
    );
    assert_eq!(json["updated"], true);

    let (status, json) = uds_get(&path, "/inventory/cooling-devices").await;
    assert_eq!(status, 200);
    let members = json["cooling_devices"][0]["radiator_members"]
        .as_array()
        .unwrap();
    assert!(
        members.iter().any(|m| m == "openfan:ch00"),
        "the OpenFan member must survive the round trip: {json}"
    );

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// The widening must not become a blanket accept: an OpenFan channel that is not
/// attached is still rejected, so a typo is still caught at author time.
#[tokio::test]
async fn a_cooling_device_still_rejects_an_undiscovered_openfan_channel() {
    let (state, _tmp) = cooling_device_test_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({
            "id": "aio-1",
            "pump_member": "h2",
            "radiator_members": ["openfan:ch07"]
        }),
    )
    .await;
    assert_eq!(status, 400, "channel 7 is not attached: {json}");
    assert_eq!(json["error"]["code"], "validation_error");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

// ── DEC-320: free-text fields on the validation routes are bounded at ingest ──

/// Redirect the process-wide state directory into a temp dir.
///
/// A validation session persists on start, and without this it tries to create
/// `/var/lib/control-ofc/validation` and fails with `503 persistence_failed`.
/// `init_state_dir` is a `OnceLock`, so the first caller in this binary wins;
/// nothing else here reads the state dir (only `validation/store.rs` does, from
/// a handler path), so redirecting it is safe and is what the Phase 5 test
/// binary already does.
fn ipc_temp_state_dir() {
    use std::sync::OnceLock;
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = tempfile::tempdir().unwrap();
        control_ofc_daemon::daemon_state::init_state_dir(d.path().to_str().unwrap());
        d
    });
}

/// The bounds exist because `prune` now DELETES a session too large to read.
/// Unbounded event/measurement text could push a document past the store's read
/// cap, at which point the new reclaim path would destroy an operator's evidence
/// rather than merely leak disk. Raised by `ofc:security-reviewer`.
///
/// Driven through the real routes, not through `too_long` directly: these are the
/// only two call sites, and a helper proven in isolation says nothing about
/// whether the handlers actually call it.
#[tokio::test]
async fn validation_ingest_rejects_over_long_free_text() {
    ipc_temp_state_dir();
    let (state, _tmp) = cooling_device_test_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let (status, json) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({"id": "aio-1", "pump_member": "h2", "radiator_members": ["h1"]}),
    )
    .await;
    assert_eq!(status, 200, "{json}");

    let (status, json) = uds_post(
        &path,
        "/validation/session",
        &serde_json::json!({"cooling_device_id": "aio-1"}),
    )
    .await;
    assert_eq!(
        status, 200,
        "a session must start for this test to mean anything: {json}"
    );

    let long = "x".repeat(control_ofc_daemon::constants::VALIDATION_MAX_TEXT_FIELD_BYTES + 1);

    // A note within the bound is accepted — so the rejection below is the bound
    // firing, not the route being broken.
    let (status, json) = uds_post(
        &path,
        "/validation/session/measurement",
        &serde_json::json!({"kind": "rpm", "value": 1200.0, "note": "ok"}),
    )
    .await;
    assert_eq!(
        status, 200,
        "a normal measurement must still be accepted: {json}"
    );

    for body in [
        serde_json::json!({"kind": "rpm", "value": 1200.0, "note": long.clone()}),
        serde_json::json!({"kind": long.clone(), "value": 1200.0}),
    ] {
        let (status, json) = uds_post(&path, "/validation/session/measurement", &body).await;
        assert_eq!(
            status, 400,
            "an over-long measurement field must be rejected: {json}"
        );
        assert_eq!(json["error"]["code"], "validation_error");
    }

    let (status, json) = uds_post(
        &path,
        "/validation/session/event",
        &serde_json::json!({"detail": long.clone()}),
    )
    .await;
    assert_eq!(
        status, 400,
        "an over-long event detail must be rejected: {json}"
    );

    let _ = uds_delete(&path, "/validation/session").await;
    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}

/// A cooling device's sensor ids are copied into every validation sample, so an
/// unbounded one scaled the session document without bound — the route by which
/// `AUD3-i` was still reproducible inside its own fix.
#[tokio::test]
async fn a_cooling_device_rejects_an_over_long_sensor_id() {
    let (state, _tmp) = cooling_device_test_state();
    let (path, shutdown, _dir) = start_test_server(state).await;

    let long = "s".repeat(control_ofc_daemon::hwmon::cooling_device::MAX_DEVICE_TEXT_BYTES + 1);
    let (status, json) = uds_post(
        &path,
        "/config/cooling-device",
        &serde_json::json!({"id": "aio-1", "pump_member": "h2", "preferred_sensor": long}),
    )
    .await;
    assert_eq!(
        status, 400,
        "an over-long preferred_sensor must be rejected: {json}"
    );
    assert_eq!(json["error"]["code"], "validation_error");

    let _ = shutdown.send(());
    let _ = std::fs::remove_file(&path);
}
