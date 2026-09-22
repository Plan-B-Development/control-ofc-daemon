//! AIO Phase 8 — `POST /validation/session` ingest bounds (`P8-s`, DEC-341).
//!
//! **A separate integration binary on purpose, and that is the whole reason this
//! file exists.** These tests drive the real `start_session_handler`, which ends
//! with `prune_sessions_off_runtime()` against the *process-global* state dir.
//! `validation_phase5.rs` shares one `temp_state_dir()` across ~19 parallel tests
//! whose fixtures all carry `started_unix_ms: 1_000`, so retention's sort key is
//! tied and the survivor set arbitrary — running these there deleted 7-11 of
//! their session files per run (measured: a tree that held 24 files held 13-17
//! afterwards, varying run to run) and would have failed a sibling intermittently
//! on CI with no relation to the code under test. Found by
//! `ofc:security-reviewer`.
//!
//! A cargo integration test is one binary per file, so this file gets its own
//! process, its own `OnceLock` state dir, and a prune that can only reach its own
//! sessions. Any future test that drives a handler which prunes belongs here.

use control_ofc_daemon::validation::session::*;
use control_ofc_daemon::validation::store;

/// The process-global state dir for this binary alone.
fn temp_state_dir() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = tempfile::tempdir().unwrap();
        control_ofc_daemon::daemon_state::init_state_dir(d.path().to_str().unwrap());
        d
    })
    .path()
}

// ── P8-s: `diagnostics` is bounded at ingest, and the CALL SITE proves it ─────

const PUMP_ID: &str = "hwmon:it8696:isa-0a40:pwm5:AIO_PUMP";

/// A minimal `AppState` carrying one cooling device, enough to drive the real
/// `start_session_handler`.
///
/// Built here rather than shared with `discovery_phase8.rs`'s harness: that one
/// exists to shape a preflight (a specific pump header, a writer that panics on
/// any write), and parameterising it to serve both would cost more than the field
/// list does. If a third surface needs one, **that** is the change that extracts
/// it — the rule from DEC-276, applied in the direction it actually points.
fn validation_app_state(
    device_id: &str,
) -> std::sync::Arc<control_ofc_daemon::api::handlers::AppState> {
    use control_ofc_daemon::api::handlers::AppState;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let device = control_ofc_daemon::hwmon::cooling_device::CoolingDeviceConfig {
        id: device_id.into(),
        name: "Test AIO".into(),
        kind: "aio".into(),
        pump_member: Some(PUMP_ID.into()),
        ..Default::default()
    };
    let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
    Arc::new(AppState {
        cache: Arc::new(Default::default()),
        staleness_config: control_ofc_daemon::health::staleness::StalenessConfig::default(),
        daemon_version: "0.0.0-test".into(),
        fan_controller: Arc::new(parking_lot::RwLock::new(None)),
        openfan_runtime: control_ofc_daemon::api::handlers::OpenFanRuntime {
            timeout: std::time::Duration::from_millis(500),
            interval: std::time::Duration::from_millis(1000),
            shutdown: tokio::sync::watch::channel(false).1,
        },
        hwmon_controller: None,
        start_time: std::time::Instant::now(),
        history: Arc::new(control_ofc_daemon::health::history::HistoryRing::new(250)),
        active_profile: Arc::new(parking_lot::Mutex::new(None)),
        calibrating: AtomicBool::new(false),
        characterization: Arc::new(parking_lot::Mutex::new(None)),
        validation: Arc::new(Default::default()),
        characterization_cancel: Arc::new(AtomicBool::new(false)),
        control_path: Arc::new(parking_lot::Mutex::new(None)),
        control_path_cancel: Arc::new(AtomicBool::new(false)),
        stall_probe: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        stall_probe_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        control_paths: Arc::new(parking_lot::RwLock::new(Default::default())),
        pwm_baselines: Default::default(),
        openfan_rescanning: AtomicBool::new(false),
        last_openfan_rescan: Arc::new(parking_lot::Mutex::new(None)),
        adopted_poll_tasks: Arc::new(parking_lot::Mutex::new(Default::default())),
        amd_gpus: Vec::new(),
        intel_gpus: Vec::new(),
        nvidia_gpus: Vec::new(),
        profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
        config_path: String::new(),
        runtime_config_path: Default::default(),
        sensor_rescan_requested: Arc::new(AtomicBool::new(false)),
        header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(Default::default()))),
        cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(vec![device]))),
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

#[test]
fn normalise_diagnostics_drops_repeats_and_keeps_first_occurrence_order() {
    let out = normalise_diagnostics(&[
        DIAG_VERIFY.into(),
        DIAG_CHARACTERIZATION.into(),
        DIAG_VERIFY.into(),
        DIAG_CHARACTERIZATION.into(),
        DIAG_VERIFY.into(),
    ]);
    assert_eq!(out, vec![DIAG_VERIFY, DIAG_CHARACTERIZATION]);
    assert!(normalise_diagnostics(&[]).is_empty());
}

/// The bound is the token set's size, not a number — so it moves when the set
/// does. Asserted against `KNOWN_DIAGNOSTICS.len()` rather than `4`, which is
/// what makes adding a fifth diagnostic raise the bound instead of breaking it.
#[test]
fn a_normalised_list_cannot_exceed_the_known_diagnostic_set() {
    let flood: Vec<String> = std::iter::repeat_n(KNOWN_DIAGNOSTICS.to_vec(), 5000)
        .flatten()
        .map(|d| d.to_string())
        .collect();
    assert_eq!(flood.len(), 5000 * KNOWN_DIAGNOSTICS.len());
    let out = normalise_diagnostics(&flood);
    assert_eq!(out.len(), KNOWN_DIAGNOSTICS.len());
    assert!(
        out.iter().all(|d| is_known_diagnostic(d)),
        "normalisation must not invent a token"
    );
}

/// **The call-site test, and it is the point of this row.**
///
/// `CLAUDE.md` records extract-the-rule-but-not-the-call-site as having bitten
/// this project fourteen times, most recently as `P8-ab` — the row shipped one
/// package before this one. A thoroughly unit-tested `normalise_diagnostics` that
/// `start_session_handler` never calls is exactly that defect, and the unit tests
/// above cannot see it. So this drives the **real handler** and reads the
/// **persisted document**, which is where the unbounded list actually landed.
///
/// The assertion is a RELATIONSHIP, per DEC-324: `requested_diagnostics` must
/// equal what `normalise_diagnostics` makes of the request. A literal
/// `vec![DIAG_VERIFY]` would be satisfied by a handler that happened to receive one
/// token, which is not the property. And because sharing the helper on both sides
/// would go green if the helper itself became the identity function, the length
/// is *also* checked against the independent term — the size of the token set.
#[tokio::test]
async fn the_start_handler_bounds_the_diagnostics_it_persists() {
    use control_ofc_daemon::api::handlers::validation as vh;

    let dir = temp_state_dir();
    let state = validation_app_state("dedup-dev");

    // 2000 entries, every one of them a valid token — the shape that passed
    // every check before `P8-s`, and comfortably inside the 4 MiB body limit.
    let flood: Vec<String> = std::iter::repeat_n(KNOWN_DIAGNOSTICS.to_vec(), 500)
        .flatten()
        .map(|d| d.to_string())
        .collect();
    let request = flood.clone();

    let (code, _) = vh::start_session_handler(
        axum::extract::State(state.clone()),
        axum::Json(vh::StartSessionRequest {
            cooling_device_id: "dedup-dev".into(),
            diagnostics: flood,
            sweep_members: vec![PUMP_ID.into()],
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(
        code,
        axum::http::StatusCode::OK,
        "the start must be accepted"
    );

    let live = state
        .validation
        .snapshot()
        .expect("a started session must be installed");

    assert_eq!(
        live.requested_diagnostics,
        normalise_diagnostics(&request),
        "the handler persisted its own list rather than the normalised one — \
         `normalise_diagnostics` is not on the call path"
    );
    // The independent term: shared-helper agreement alone would survive the
    // helper becoming an identity function.
    assert_eq!(
        live.requested_diagnostics.len(),
        KNOWN_DIAGNOSTICS.len(),
        "2000 valid tokens must persist as at most one of each"
    );

    // And it must be bounded in the ARTEFACT, not merely in the live snapshot —
    // the persisted document is what `prune` measures and deletes.
    let on_disk = store::load_from(&dir.join("validation"), &live.session_id)
        .expect("the session must be readable")
        .expect("the session must have been written");
    assert_eq!(on_disk.requested_diagnostics, live.requested_diagnostics);

    // Leave the slot clean for the other tests sharing this state dir.
    state.validation.cancel();
}
