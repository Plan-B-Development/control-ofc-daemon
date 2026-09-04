//! AIO-MB Phase 5 — validation sessions, evidence, and result semantics.
//!
//! Each test names the §18 bullet it discharges. The bias throughout is toward
//! asserting the *specific* value a rule produces rather than that two states
//! merely differ — a summariser that returned `unknown` for everything would
//! satisfy "a finding exists", and that is exactly the bug these guard against.

use control_ofc_daemon::validation::session::*;
use control_ofc_daemon::validation::{store, summary};

// ── Fixtures ────────────────────────────────────────────────────────────────

fn policy() -> DevicePolicySnapshot {
    DevicePolicySnapshot {
        id: "generic_pump".into(),
        display_name: "Generic pump".into(),
        minimum_safe_pwm_pct: 30.0,
        supports_stop: false,
        startup_override_seconds: None,
        expected_rpm_min: None,
        expected_rpm_max: None,
        internal_control_possible: true,
    }
}

fn member(id: &str, kind: &str) -> MemberRoleSnapshot {
    MemberRoleSnapshot {
        member_id: id.into(),
        label: format!("{id} label"),
        role: if kind == MEMBER_PUMP {
            "pump"
        } else {
            "radiator_fan"
        }
        .into(),
        member_kind: kind.into(),
        pump_protected: kind == MEMBER_PUMP,
        effective_min_pwm_pct: Some(if kind == MEMBER_PUMP { 30 } else { 20 }),
        stop_permitted: Some(kind != MEMBER_PUMP),
        writable: true,
    }
}

/// A device with one pump and two radiators — the two-radiator case §3 names
/// explicitly, so identity-preservation is testable.
fn metadata() -> SessionMetadata {
    SessionMetadata {
        cooling_device_id: "dev-1".into(),
        device_name: "Test AIO".into(),
        device_kind: "aio_liquid".into(),
        pump_member: Some("hwmon:it87:pwm2:PUMP".into()),
        radiator_members: vec!["hwmon:it87:pwm3:RAD1".into(), "hwmon:it87:pwm4:RAD2".into()],
        auxiliary_members: vec![],
        temperature_sensor: Some("cpu:package".into()),
        coolant_sensor: None,
        coolant_telemetry: "unavailable".into(),
        device_policy: policy(),
        members: vec![
            member("hwmon:it87:pwm2:PUMP", MEMBER_PUMP),
            member("hwmon:it87:pwm3:RAD1", MEMBER_RADIATOR),
            member("hwmon:it87:pwm4:RAD2", MEMBER_RADIATOR),
        ],
        active_profile_id: Some("p1".into()),
        active_profile_name: Some("Balanced".into()),
        daemon_version: "2.32.0".into(),
        user_metadata: Default::default(),
    }
}

fn session() -> ValidationSession {
    ValidationSession {
        session_id: "val-1000-0".into(),
        kind: KIND_VALIDATION.into(),
        state: STATE_RECORDING.into(),
        started_unix_ms: 1_000,
        completed_unix_ms: None,
        metadata: metadata(),
        requested_diagnostics: vec![],
        sweep_members: vec![],
        samples: vec![],
        events: vec![],
        evidence: vec![],
        external_measurements: vec![],
        findings: vec![],
        sample_limit_reached: false,
        interrupted_reason: None,
        truncated_at_unix_ms: None,
    }
}

fn member_sample(
    id: &str,
    kind: &str,
    requested: Option<u8>,
    readback: Option<u8>,
    rpm: Option<u16>,
) -> MemberSample {
    MemberSample {
        member_id: id.into(),
        role: kind.into(),
        requested_pct: requested,
        readback_pct: readback,
        rpm,
        pwm_enable_mode: Some(1),
        alarm: Some(false),
        enable_revert_count: 0,
        ownership: OWNERSHIP_DAEMON.into(),
    }
}

fn sample_at(ms: u64, members: Vec<MemberSample>) -> ValidationSample {
    ValidationSample {
        elapsed_ms: ms,
        unix_ms: 1_000 + ms,
        temperature_c: Some(45.0),
        temperature_sensor: Some("cpu:package".into()),
        coolant_c: None,
        thermal_state: "normal".into(),
        members,
    }
}

fn find<'a>(f: &'a [ValidationFinding], id: &str) -> &'a ValidationFinding {
    f.iter().find(|x| x.id == id).unwrap_or_else(|| {
        panic!(
            "no finding '{id}' in {:?}",
            f.iter().map(|x| &x.id).collect::<Vec<_>>()
        )
    })
}

fn find_for<'a>(f: &'a [ValidationFinding], id: &str, member: &str) -> &'a ValidationFinding {
    f.iter()
        .find(|x| x.id == id && x.member_id.as_deref() == Some(member))
        .unwrap_or_else(|| panic!("no finding '{id}' for member '{member}'"))
}

// ── §18: result semantics ───────────────────────────────────────────────────

/// §18 "result summary preserves NOT_TESTED/UNKNOWN/UNAVAILABLE semantics", and
/// §7's central rule: absence of a diagnostic is NOT_TESTED, never PASS.
#[test]
fn a_session_with_no_diagnostics_reports_not_tested_and_never_pass() {
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    let findings = summary::summarise(&s);

    for id in [
        F_PWM_HEADER_CONTROL,
        F_PWM_RESPONSE,
        F_DEVICE_OVERRIDE,
        F_RESPONSE_LATENCY,
    ] {
        let f = find(&findings, id);
        assert_eq!(
            f.state, RESULT_NOT_TESTED,
            "{id} must be not_tested when no diagnostic ran"
        );
        // The explicit half: it must not have been silently promoted.
        assert_ne!(f.state, RESULT_PASS, "{id} must never read as a pass");
    }
}

/// §18 "missing telemetry is represented as unavailable rather than zero/failure".
#[test]
fn a_header_with_no_tach_reports_unavailable_not_zero_and_not_fail() {
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    // Present, sampled, but the driver exposes no RPM for the pump.
    s.samples.push(sample_at(
        0,
        vec![
            member_sample(
                "hwmon:it87:pwm2:PUMP",
                MEMBER_PUMP,
                Some(50),
                Some(50),
                None,
            ),
            member_sample(
                "hwmon:it87:pwm3:RAD1",
                MEMBER_RADIATOR,
                Some(50),
                Some(50),
                Some(900),
            ),
            member_sample(
                "hwmon:it87:pwm4:RAD2",
                MEMBER_RADIATOR,
                Some(50),
                Some(50),
                Some(910),
            ),
        ],
    ));
    let findings = summary::summarise(&s);

    let pump = find_for(&findings, F_PUMP_RPM, "hwmon:it87:pwm2:PUMP");
    assert_eq!(pump.state, RESULT_UNAVAILABLE);
    assert_ne!(
        pump.state, RESULT_FAIL,
        "absent telemetry is not a failure (§7)"
    );
    // And the radiator that DOES report is a pass, so the check above is not
    // passing merely because everything is unavailable.
    assert_eq!(
        find_for(&findings, F_RADIATOR_RPM, "hwmon:it87:pwm3:RAD1").state,
        RESULT_PASS
    );
}

/// §18 "telemetry samples preserve pump/radiator/member identity" + §3's
/// explicit "do not flatten multiple radiators into an invented single value".
#[test]
fn two_radiators_produce_two_named_findings_never_an_average() {
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    s.samples.push(sample_at(
        0,
        vec![
            member_sample(
                "hwmon:it87:pwm2:PUMP",
                MEMBER_PUMP,
                Some(50),
                Some(50),
                Some(2400),
            ),
            member_sample(
                "hwmon:it87:pwm3:RAD1",
                MEMBER_RADIATOR,
                Some(50),
                Some(50),
                Some(800),
            ),
            // A deliberately different RPM: an averaging bug would show as 900.
            member_sample(
                "hwmon:it87:pwm4:RAD2",
                MEMBER_RADIATOR,
                Some(50),
                Some(50),
                Some(1000),
            ),
        ],
    ));
    let findings = summary::summarise(&s);

    let radiators: Vec<_> = findings.iter().filter(|f| f.id == F_RADIATOR_RPM).collect();
    assert_eq!(radiators.len(), 2, "each radiator keeps its own finding");
    let ids: Vec<_> = radiators
        .iter()
        .filter_map(|f| f.member_id.as_deref())
        .collect();
    assert!(ids.contains(&"hwmon:it87:pwm3:RAD1"));
    assert!(ids.contains(&"hwmon:it87:pwm4:RAD2"));

    // The samples themselves keep both series distinct.
    let rpms: Vec<_> = s.samples[0]
        .members
        .iter()
        .filter(|m| m.role == MEMBER_RADIATOR)
        .filter_map(|m| m.rpm)
        .collect();
    assert_eq!(rpms, vec![800, 1000]);
}

/// §18 "startup/device-override evidence can be represented without false
/// failure" and §10's "do not treat this automatically as PWM write failure".
#[test]
fn a_possible_device_override_is_observed_evidence_and_never_a_failure() {
    use control_ofc_daemon::api::characterization::{CharPoint, CharSummary, CharacterizationRun};

    let mut s = session();
    s.state = STATE_COMPLETED.into();
    s.evidence.push(EvidenceRef {
        kind: DIAG_CHARACTERIZATION.into(),
        member_id: "hwmon:it87:pwm2:PUMP".into(),
        run_id: Some("char-1".into()),
        started_unix_ms: 1_000,
        completed_unix_ms: Some(2_000),
        outcome: RESULT_OBSERVED.into(),
        detail: None,
        characterization: Some(CharacterizationRun {
            run_id: "char-1".into(),
            header_id: "hwmon:it87:pwm2:PUMP".into(),
            state: "complete".into(),
            requested_points_pct: vec![30, 50, 100],
            settle_seconds: 5,
            points: vec![CharPoint {
                requested_pct: 30,
                command_accepted: true,
                readback_pct: Some(30),
                readback_raw: Some(76),
                pwm_enable: Some(1),
                rpm_before: Some(2400),
                rpm_after: Some(2400),
                settle_ms: 5_000,
                first_change_ms: None,
                readback_verdict: "pass".into(),
                rpm_verdict: "no_effect".into(),
            }],
            summary: Some(CharSummary {
                command_acceptance: "pass".into(),
                pwm_readback: "pass".into(),
                rpm_response: "fail".into(),
                min_tested_pct: Some(30),
                max_tested_pct: Some(100),
                min_rpm: Some(2400),
                max_rpm: Some(2400),
                monotonic: Some(false),
                dead_zone_upper_pct: None,
                clamp_pct: None,
                // The signature §10 describes: control and readback are valid,
                // the physical response is not what was asked for.
                possible_device_override: true,
                interference_detected: false,
            }),
            original_pct: Some(50),
            restore_failed: false,
            restore_outcome: "restored".into(),
            detail: None,
        }),
        verify: None,
    });

    let findings = summary::summarise(&s);

    // The classification is preserved as OBSERVED evidence...
    let ovr = find(&findings, F_DEVICE_OVERRIDE);
    assert_eq!(ovr.state, RESULT_OBSERVED);
    assert_ne!(
        ovr.state, RESULT_FAIL,
        "§10: cautious semantics, never a failure"
    );

    // ...and the working half of the same run is NOT dragged down with it.
    // This is the "without false failure" half of the bullet: motherboard PWM
    // control was valid and must still report as valid.
    assert_eq!(find(&findings, F_PWM_HEADER_CONTROL).state, RESULT_PASS);
    assert_eq!(find(&findings, F_PWM_READBACK).state, RESULT_PASS);
}

/// §18 "existing characterisation results can be associated/referenced without
/// rerunning or reimplementing the test" (§6).
#[test]
fn characterisation_evidence_is_carried_verbatim_not_recomputed() {
    use control_ofc_daemon::api::characterization::{CharSummary, CharacterizationRun};

    let mut s = session();
    s.state = STATE_COMPLETED.into();
    let run = CharacterizationRun {
        run_id: "char-7".into(),
        header_id: "hwmon:it87:pwm2:PUMP".into(),
        state: "complete".into(),
        requested_points_pct: vec![40, 80],
        settle_seconds: 4,
        points: vec![],
        summary: Some(CharSummary {
            command_acceptance: "pass".into(),
            pwm_readback: "clamped".into(),
            rpm_response: "pass".into(),
            min_tested_pct: Some(40),
            max_tested_pct: Some(80),
            min_rpm: Some(700),
            max_rpm: Some(1800),
            monotonic: Some(true),
            dead_zone_upper_pct: None,
            clamp_pct: Some(60),
            possible_device_override: false,
            interference_detected: false,
        }),
        original_pct: Some(50),
        restore_failed: false,
        restore_outcome: "restored".into(),
        detail: None,
    };
    s.evidence.push(EvidenceRef {
        kind: DIAG_CHARACTERIZATION.into(),
        member_id: "hwmon:it87:pwm2:PUMP".into(),
        run_id: Some("char-7".into()),
        started_unix_ms: 1_000,
        completed_unix_ms: Some(2_000),
        outcome: RESULT_OBSERVED.into(),
        detail: None,
        characterization: Some(run.clone()),
        verify: None,
    });

    // The attached run is bit-for-bit the one Phase 3 produced.
    assert_eq!(s.evidence[0].characterization.as_ref().unwrap(), &run);

    // And the summariser reports Phase 3's own verdict rather than deriving a
    // second opinion: `clamped` is preserved as OBSERVED, not turned into a fail.
    let findings = summary::summarise(&s);
    assert_eq!(find(&findings, F_PWM_READBACK).state, RESULT_OBSERVED);
    assert_eq!(find(&findings, F_PWM_RESPONSE).state, RESULT_PASS);
}

/// §7: `UNAVAILABLE` must not become `FAIL` when the hardware simply does not
/// expose the capability. A refused diagnostic is the same case.
#[test]
fn a_refused_diagnostic_is_unavailable_not_fail() {
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    s.evidence.push(EvidenceRef {
        kind: DIAG_CHARACTERIZATION.into(),
        member_id: "hwmon:it87:pwm2:PUMP".into(),
        run_id: None,
        started_unix_ms: 1_000,
        completed_unix_ms: Some(1_100),
        // The thermal ladder was forcing, so the sweep never started.
        outcome: RESULT_UNAVAILABLE.into(),
        detail: Some("thermal_abort".into()),
        characterization: None,
        verify: None,
    });
    let findings = summary::summarise(&s);
    for id in [F_PWM_HEADER_CONTROL, F_PWM_RESPONSE, F_DEVICE_OVERRIDE] {
        assert_ne!(find(&findings, id).state, RESULT_FAIL, "{id}");
    }
}

/// The divergence rule needs a real duty swing, or the question is unanswerable
/// — a pump idling correctly at its floor must not be reported as divergent.
#[test]
fn divergence_is_not_tested_until_the_duty_actually_moves() {
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    for i in 0..5u64 {
        s.samples.push(sample_at(
            i * 1000,
            vec![member_sample(
                "hwmon:it87:pwm2:PUMP",
                MEMBER_PUMP,
                Some(30),
                Some(30),
                Some(2400),
            )],
        ));
    }
    let findings = findings_of(&s);
    let f = find_for(&findings, F_PWM_RPM_DIVERGENCE, "hwmon:it87:pwm2:PUMP");
    assert_eq!(f.state, RESULT_NOT_TESTED, "steady duty proves nothing");
}

#[test]
fn divergence_is_observed_when_rpm_does_not_follow_a_real_pwm_swing() {
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    for (i, duty) in [30u8, 50, 80, 100].iter().enumerate() {
        s.samples.push(sample_at(
            i as u64 * 1000,
            vec![member_sample(
                "hwmon:it87:pwm2:PUMP",
                MEMBER_PUMP,
                Some(*duty),
                Some(*duty),
                // RPM pinned: the device is running its own control.
                Some(2400),
            )],
        ));
    }
    let findings = findings_of(&s);
    let f = find_for(&findings, F_PWM_RPM_DIVERGENCE, "hwmon:it87:pwm2:PUMP");
    assert_eq!(f.state, RESULT_OBSERVED);

    // The converse, so the assertion above is discriminating rather than
    // constant: RPM that tracks the same swing is NOT flagged.
    let mut ok = session();
    ok.state = STATE_COMPLETED.into();
    for (i, (duty, rpm)) in [(30u8, 900u16), (50, 1500), (80, 2200), (100, 2900)]
        .iter()
        .enumerate()
    {
        ok.samples.push(sample_at(
            i as u64 * 1000,
            vec![member_sample(
                "hwmon:it87:pwm2:PUMP",
                MEMBER_PUMP,
                Some(*duty),
                Some(*duty),
                Some(*rpm),
            )],
        ));
    }
    let ok_findings = findings_of(&ok);
    assert_eq!(
        find_for(&ok_findings, F_PWM_RPM_DIVERGENCE, "hwmon:it87:pwm2:PUMP").state,
        RESULT_NOT_OBSERVED
    );
}

fn findings_of(s: &ValidationSession) -> Vec<ValidationFinding> {
    summary::summarise(s)
}

/// §1 is explicit that coolant telemetry is NOT required — a motherboard-PWM AIO
/// on CPU temperature is a valid validation target.
#[test]
fn absent_coolant_telemetry_is_unavailable_and_the_session_is_still_valid() {
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    s.samples.push(sample_at(0, vec![]));
    let findings = summary::summarise(&s);
    let f = find(&findings, F_COOLANT_TELEMETRY);
    assert_eq!(f.state, RESULT_UNAVAILABLE);
    assert_ne!(f.state, RESULT_FAIL);
}

// ── §18: interruption ───────────────────────────────────────────────────────

/// §18 "session interruption is represented explicitly" + §15 "do not fabricate
/// telemetry for periods where no data was collected".
#[test]
fn a_recording_session_left_on_disk_becomes_interrupted_without_invented_samples() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session();
    s.samples.push(sample_at(0, vec![]));
    s.samples.push(sample_at(1000, vec![]));
    store::save_to(dir.path(), &s).unwrap();

    let repaired = store::sweep_interrupted(dir.path(), "daemon_restart");
    assert_eq!(repaired, vec!["val-1000-0".to_string()]);

    let back = store::load_from(dir.path(), "val-1000-0").unwrap().unwrap();
    assert_eq!(back.state, STATE_INTERRUPTED);
    assert_eq!(back.interrupted_reason.as_deref(), Some("daemon_restart"));
    // Truncated exactly at the last real sample...
    assert_eq!(back.truncated_at_unix_ms, Some(2_000));
    // ...and not one sample was invented to fill the gap.
    assert_eq!(
        back.samples.len(),
        2,
        "no telemetry may be fabricated (§15)"
    );
    // The findings say `interrupted`, not `not_tested` — the distinction §7
    // exists to preserve.
    assert_eq!(
        find(&back.findings, F_PWM_HEADER_CONTROL).state,
        RESULT_INTERRUPTED
    );
}

/// A session that finished normally must be left alone by the boot sweep.
#[test]
fn the_boot_sweep_does_not_touch_a_completed_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    store::save_to(dir.path(), &s).unwrap();

    assert!(store::sweep_interrupted(dir.path(), "daemon_restart").is_empty());
    let back = store::load_from(dir.path(), "val-1000-0").unwrap().unwrap();
    assert_eq!(back.state, STATE_COMPLETED);
}

// ── §18: serialization ──────────────────────────────────────────────────────

/// §18 "JSON/session serialization correctness".
#[test]
fn a_session_round_trips_through_json_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session();
    s.state = STATE_COMPLETED.into();
    s.samples.push(sample_at(
        0,
        vec![member_sample(
            "hwmon:it87:pwm2:PUMP",
            MEMBER_PUMP,
            Some(40),
            Some(41),
            Some(2400),
        )],
    ));
    s.events.push(ValidationEvent {
        elapsed_ms: 0,
        unix_ms: 1_000,
        kind: EV_SESSION_STARTED.into(),
        detail: None,
        member_id: None,
    });
    s.external_measurements.push(ExternalMeasurement {
        unix_ms: 1_500,
        kind: "supply_voltage_v".into(),
        value: 12.03,
        unit: "V".into(),
        member_id: None,
        note: Some("bench meter".into()),
    });
    s.findings = summary::summarise(&s);

    store::save_to(dir.path(), &s).unwrap();
    let back = store::load_from(dir.path(), &s.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        back, s,
        "the stored session must survive a round trip exactly"
    );
}

/// §18 "unrelated system information is not included" (§3's "do not collect
/// unrelated system data").
#[test]
fn the_serialized_session_contains_only_cooling_relevant_keys() {
    let mut s = session();
    s.samples.push(sample_at(
        0,
        vec![member_sample(
            "hwmon:it87:pwm2:PUMP",
            MEMBER_PUMP,
            Some(40),
            Some(41),
            Some(2400),
        )],
    ));
    let json: serde_json::Value = serde_json::to_value(&s).unwrap();
    let obj = json.as_object().unwrap();

    // An allowlist, not a denylist: a new field must be considered rather than
    // silently shipped, which is the only way this test can catch a leak.
    const ALLOWED: &[&str] = &[
        "session_id",
        "kind",
        "state",
        "started_unix_ms",
        "completed_unix_ms",
        "metadata",
        "requested_diagnostics",
        "sweep_members",
        "samples",
        "events",
        "evidence",
        "external_measurements",
        "findings",
        "sample_limit_reached",
        "interrupted_reason",
        "truncated_at_unix_ms",
    ];
    for key in obj.keys() {
        assert!(
            ALLOWED.contains(&key.as_str()),
            "unexpected top-level key '{key}' — is it cooling-relevant? (§3)"
        );
    }

    // And nothing resembling host/user identity rode along. Asserted against
    // the KEY SET, not the raw text: a bare substring search for "uid" matches
    // inside "aio_liquid", which is the same trap as a filtered grep matching
    // its own subject.
    let mut keys = Vec::new();
    collect_keys(&json, &mut keys);
    for forbidden in [
        "hostname",
        "username",
        "uid",
        "gid",
        "user",
        "home",
        "kernel_version",
        "cmdline",
        "environment",
        "path",
    ] {
        assert!(
            !keys.iter().any(|k| k == forbidden),
            "serialized session must not carry a '{forbidden}' key; keys were {keys:?}"
        );
    }

    // Values must not leak a filesystem path either — hwmon ids are opaque
    // stable ids, never sysfs paths.
    let text = serde_json::to_string(&s).unwrap();
    for forbidden in ["/home/", "/sys/", "/proc/", "/var/lib/"] {
        assert!(
            !text.contains(forbidden),
            "serialized session must not carry the path '{forbidden}'"
        );
    }
}

/// Every object key in the document, at any depth.
fn collect_keys(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, child) in m {
                out.push(k.clone());
                collect_keys(child, out);
            }
        }
        serde_json::Value::Array(a) => {
            for child in a {
                collect_keys(child, out);
            }
        }
        _ => {}
    }
}

// ── §18: retention and bounds ───────────────────────────────────────────────

#[test]
fn pruning_keeps_the_newest_and_never_deletes_a_recording_session() {
    let dir = tempfile::tempdir().unwrap();
    // One live session, plus four completed ones at increasing start times.
    let mut live = session();
    live.session_id = "val-live".into();
    live.started_unix_ms = 1; // oldest, so a naive prune would take it
    store::save_to(dir.path(), &live).unwrap();

    for i in 0..4u64 {
        let mut s = session();
        s.session_id = format!("val-done-{i}");
        s.state = STATE_COMPLETED.into();
        s.started_unix_ms = 100 + i;
        store::save_to(dir.path(), &s).unwrap();
    }

    store::prune(dir.path(), 2);
    let left: Vec<String> = store::list_from(dir.path())
        .into_iter()
        .map(|s| s.session_id)
        .collect();

    assert!(
        left.contains(&"val-live".to_string()),
        "a recording session is never pruned"
    );
    assert!(left.contains(&"val-done-3".to_string()));
    assert!(left.contains(&"val-done-2".to_string()));
    assert!(!left.contains(&"val-done-0".to_string()));
    assert!(!left.contains(&"val-done-1".to_string()));
}

/// A session id reaches the store from a URL path segment. It must never be
/// interpolated into a filename without confinement.
#[test]
fn a_traversing_session_id_cannot_escape_the_session_directory() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["../escape", "a/b", "..", ".", "", "with space", "x/../../y"] {
        assert!(!is_safe_session_id(bad), "'{bad}' must be rejected");
        assert!(
            store::load_from(dir.path(), bad).unwrap().is_none(),
            "'{bad}' must not resolve to a file"
        );
    }
    assert!(is_safe_session_id("val-1000-0"));
    assert!(is_safe_session_id("val_1000_0"));
}

// ── §18: the safety claims, asserted structurally ───────────────────────────

/// §18 "session logging does not alter PWM/control state" and §2 "validation
/// recording must not acquire a second PWM ownership path".
///
/// Asserted against the source, because that is the only way to prove an
/// *absence*. A behavioural test can show that the paths we thought of do not
/// write; this shows that no path does, including one added later.
#[test]
fn the_recorder_contains_no_hardware_write_path() {
    let recorder = include_str!("../src/validation/recorder.rs");
    let body = strip_comments(recorder);

    for forbidden in [
        "set_pwm",
        "write_file",
        "force_all_with_floor",
        "acquire_lease",
        "std::fs::write",
        "OpenOptions",
    ] {
        assert!(
            !body.contains(forbidden),
            "the recorder must not reference '{forbidden}' — it is an observer (§2)"
        );
    }

    // The summariser and the data model are likewise inert.
    for (name, src) in [
        ("summary", include_str!("../src/validation/summary.rs")),
        ("session", include_str!("../src/validation/session.rs")),
    ] {
        let b = strip_comments(src);
        assert!(!b.contains("set_pwm"), "{name} must not command a duty");
        assert!(
            !b.contains("std::fs::"),
            "{name} must not touch the filesystem"
        );
    }
}

/// The write path must not have grown a validation call site — the other
/// direction of the same claim.
#[test]
fn the_pwm_write_path_does_not_call_into_validation() {
    for (name, src) in [
        ("pwm_control", include_str!("../src/hwmon/pwm_control.rs")),
        (
            "profile_engine",
            include_str!("../src/profile_engine/mod.rs"),
        ),
        ("safety", include_str!("../src/safety.rs")),
    ] {
        let body = strip_comments(src);
        assert!(
            !body.contains("validation::"),
            "{name} must not call into the validation engine — logging is an \
             observer, and a hook here is how it would stop being one (§15)"
        );
    }
}

/// The pump floor and the thermal refusal are the existing handlers' job, and
/// the orchestrator must not reimplement (or bypass) either.
#[test]
fn the_orchestrator_delegates_rather_than_reimplementing_safety() {
    let src = include_str!("../src/api/handlers/validation.rs");
    let body = strip_comments(src);

    // It calls the existing handlers...
    assert!(body.contains("hwmon_verify_handler"));
    assert!(body.contains("hwmon_characterize_handler"));
    // ...and contains no floor arithmetic, lease handling or duty write of its own.
    for forbidden in [
        "HARD_PUMP_CPU_FLOOR_PCT",
        "resolve_policy_floor(policy, pump_protected)\n        .max",
        "set_pwm",
        "force_take",
        "begin_verify_pause",
    ] {
        assert!(
            !body.contains(forbidden),
            "the orchestrator must delegate '{forbidden}', not reimplement it (§6)"
        );
    }
}

/// `AUD3-j`, at the call site. The fence's four cases have their own unit tests
/// in `api::handlers::validation::tests`; this asserts that the path which
/// abandons a run actually *uses* them.
///
/// `CLAUDE.md` records "extracting a rule into a testable function does NOT test
/// the call site" as having recurred six times, and this is exactly that shape:
/// a correct `cancel_run_fenced` reached from nowhere would leave the sweep
/// running, every unit test still green. The assertion is positional rather than
/// a mere `contains` — between the session fence and the `return` it guards,
/// the cancel must appear — because a cancel that had drifted out of that block
/// would satisfy a whole-file search and stop nothing.
#[test]
fn abandoning_a_run_cancels_it_rather_than_leaving_it_sweeping() {
    let body = strip_comments(include_str!("../src/api/handlers/validation.rs"));

    // `Some(session_id)` exactly: the sibling fence in `spawn_orchestration`
    // writes `Some(session_id.as_str())` and has no run in flight to cancel.
    let marker = "recording_session_id().as_deref() != Some(session_id)";
    let at = body
        .find(marker)
        .expect("the orchestrator's session fence must still be there");
    let after = &body[at..];
    let ret = after
        .find("return;")
        .expect("the session fence must still abandon the run");
    assert!(
        after[..ret].contains("cancel_run_fenced("),
        "the orchestrator returns without cancelling the sweep it started — the \
         header keeps being driven and the engine write-pause keeps being renewed \
         for up to CHARACTERIZATION_MAX_POINTS x CHARACTERIZATION_SETTLE_MAX_S \
         after the session ended (`AUD3-j`)"
    );
}

/// `AUD3-n`. The session document is written with `write` + `fsync` + `rename` +
/// a directory `fsync`, over up to ~5.7 MiB (`AUD3-i`). None of that belongs on
/// the worker threads the 1 Hz profile engine — and therefore the thermal-safety
/// decision — is scheduled on.
///
/// **This is a wiring guard, and deliberately not a runtime measurement.** The
/// property "this call did not block a tokio worker" is only observable as a
/// latency difference, and discriminating a ~10 ms inline write from a scheduled
/// one needs a timing threshold — which `CLAUDE.md` forbids ("no flaky timing")
/// and which CI would decide by load rather than by correctness. What can be
/// asserted honestly is that each call site still goes through the wrapper, which
/// is the thing a regression would undo.
#[test]
fn the_session_lifecycle_writes_stay_off_the_async_runtime() {
    let body = strip_comments(include_str!("../src/api/handlers/validation.rs"));

    // The finalisers: no direct engine call left in either handler...
    for direct in ["state.validation.stop()", "state.validation.cancel()"] {
        assert!(
            !body.contains(direct),
            "`{direct}` finalises AND persists inline on the request path — hand \
             it to `finalise_off_runtime` (`AUD3-n`)"
        );
    }
    // ...and the wrapper they share is what carries the hop.
    let at = body
        .find("async fn finalise_off_runtime")
        .expect("the off-runtime finaliser must still exist");
    let wrapper = &body[at..];
    let end = wrapper.find("\n}\n").unwrap_or(wrapper.len());
    assert!(
        wrapper[..end].contains("spawn_blocking"),
        "`finalise_off_runtime` no longer goes off the runtime"
    );

    // The start path wraps the whole engine call, so `start`'s
    // admit-only-if-persisted rollback stays inside the engine.
    let at = body
        .find("pub async fn start_session_handler")
        .expect("the start handler must still exist");
    let handler = &body[at..];
    let call = handler
        .find("engine.start(session, &ctx)")
        .expect("the start handler must still start a session");
    assert!(
        handler[..call].contains("spawn_blocking"),
        "`start` writes the session document inline on the request path (`AUD3-n`)"
    );

    // And the 1 Hz recorder tick, whose 30th flush rewrites the whole document.
    let main_body = strip_comments(include_str!("../src/main.rs"));
    let tick = main_body
        .lines()
        .find(|l| l.contains(".tick(&c)") || l.contains("engine.tick(&ctx)"))
        .expect("the recorder task must still tick the engine");
    assert!(
        tick.contains("spawn_blocking"),
        "the recorder tick flushes the session document on a tokio worker \
         (`AUD3-n`): {tick}"
    );
}

/// A source-scanning guard matches its own explanation unless comments are
/// stripped first — the `polling.rs` precedent. Attribute-position matching does
/// not help here because the tokens are ordinary identifiers, so strip instead.
fn strip_comments(src: &str) -> String {
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("*") && !t.starts_with("/*")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── §18: non-AIO operation unaffected ───────────────────────────────────────

/// §18 "non-AIO operation remains unaffected". The engine holds no session until
/// one is started, and every accessor is safe in that state.
#[test]
fn an_engine_with_no_session_is_inert() {
    use control_ofc_daemon::validation::recorder::ValidationEngine;
    let engine = ValidationEngine::new();

    assert!(engine.snapshot().is_none());
    assert!(!engine.is_recording());
    assert!(!engine.push_event(EV_USER_MARKER, None, None));
    assert!(engine.stop().is_none());
    assert!(engine.cancel().is_none());
    assert!(!engine.add_measurement(ExternalMeasurement {
        unix_ms: 0,
        kind: "x".into(),
        value: 1.0,
        unit: "V".into(),
        member_id: None,
        note: None,
    }));
}

// ── Token hygiene ───────────────────────────────────────────────────────────

/// Only the two diagnostics the session knows how to run are accepted; an
/// unknown token is rejected at the door rather than silently ignored, which
/// would look like a diagnostic that ran and found nothing.
#[test]
fn only_known_diagnostics_are_accepted() {
    assert!(is_known_diagnostic(DIAG_VERIFY));
    assert!(is_known_diagnostic(DIAG_CHARACTERIZATION));
    for bad in ["", "characterize", "pwm_char", "verify", "gpu_verify"] {
        assert!(!is_known_diagnostic(bad), "'{bad}' must not be accepted");
    }
}

// ── Cap-and-stop (§4, §9) ───────────────────────────────────────────────────

use control_ofc_daemon::validation::recorder::{RecorderContext, ValidationEngine};
use std::sync::Arc;

/// Point the process's state dir at a temp directory so the recorder's periodic
/// flush writes somewhere harmless. `init_state_dir` is a `OnceLock`, so the
/// first test to call it wins and the rest share it — which is fine, they all
/// want the same thing.
fn temp_state_dir() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let d = DIR.get_or_init(|| {
        let d = tempfile::tempdir().unwrap();
        control_ofc_daemon::daemon_state::init_state_dir(d.path().to_str().unwrap());
        d
    });
    d.path()
}

/// A session with a unique id. The recorder tests share one temp state dir and
/// run in parallel, so a shared id would have two tests writing the same file.
fn unique_session(tag: &str) -> ValidationSession {
    let mut s = session();
    s.session_id = format!("val-{tag}");
    s
}

fn test_context() -> RecorderContext {
    RecorderContext {
        cache: Arc::new(control_ofc_daemon::health::cache::StateCache::new()),
        hwmon_controller: None,
        override_table: Arc::new(parking_lot::Mutex::new(
            control_ofc_daemon::control_override::OverrideTable::new(),
        )),
        characterization: Arc::new(parking_lot::Mutex::new(None)),
    }
}

/// §4 "keep in-memory/session storage bounded" — and §9's reason it must be
/// cap-and-stop rather than a ring: the OLDEST samples are the startup evidence,
/// so evicting them would discard exactly what the session was recording.
#[test]
fn reaching_the_sample_cap_finalises_the_session_and_keeps_the_earliest_samples() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();
    // A memberless device: the cap is about sample COUNT, and an empty member
    // list keeps each periodic flush small so this test costs a second rather
    // than fifteen.
    let mut s = unique_session("cap");
    s.metadata.members.clear();
    engine.start(s, &ctx).unwrap();

    let cap = control_ofc_daemon::constants::VALIDATION_MAX_SAMPLES;
    for _ in 0..cap + 10 {
        if !engine.tick(&ctx) {
            break;
        }
    }

    let s = engine.snapshot().expect("a session must still be readable");
    assert_eq!(
        s.state, STATE_COMPLETED,
        "the cap finalises rather than evicting"
    );
    assert!(s.sample_limit_reached, "the reason must be recorded");
    assert_eq!(s.samples.len(), cap, "bounded at exactly the cap");

    // The startup evidence survived: sample 0 is the FIRST tick, not a
    // survivor of eviction. A ring buffer would have dropped it.
    assert_eq!(
        s.samples[0].elapsed_ms,
        s.samples.iter().map(|x| x.elapsed_ms).min().unwrap()
    );
    assert!(
        s.events.iter().any(|e| e.kind == EV_SAMPLE_LIMIT),
        "the timeline must say why it stopped"
    );

    // And it really stopped — further ticks record nothing.
    let before = s.samples.len();
    assert!(!engine.tick(&ctx), "a finalised session must not resume");
    assert_eq!(engine.snapshot().unwrap().samples.len(), before);
}

/// §2 "only one active session should control/record a given logical validation
/// target" — single-flight, enforced at the door.
#[test]
fn a_second_session_is_refused_while_one_is_recording() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();
    engine.start(unique_session("single-flight"), &ctx).unwrap();

    let mut second = session();
    second.session_id = "val-second".into();
    let err = engine.start(second, &ctx).unwrap_err();
    assert_eq!(
        err,
        control_ofc_daemon::validation::recorder::StartError::AlreadyRecording
    );

    // After it finishes, a new one is allowed.
    engine.stop().unwrap();
    let mut third = session();
    third.session_id = "val-third".into();
    assert!(engine.start(third, &ctx).is_ok());
}

/// §18 "validation session start/stop/finalisation", and §8: the summary is
/// computed at finalisation, by the summariser.
///
/// This is the **call-site test** — `summarise` has its own unit coverage above,
/// but `CLAUDE.md` records "extracting a rule into a testable function does NOT
/// test the call site" as having recurred six times. A `stop()` that forgot to
/// call it would leave `findings` empty and every test above would still pass.
#[test]
fn stopping_a_session_populates_the_findings_via_the_summariser() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();
    engine.start(unique_session("findings"), &ctx).unwrap();
    engine.tick(&ctx);

    let done = engine.stop().expect("stop returns the finalised session");
    assert_eq!(done.state, STATE_COMPLETED);
    assert!(done.completed_unix_ms.is_some());
    assert!(
        !done.findings.is_empty(),
        "stop() must run the summariser, not merely change the state"
    );

    // The findings are the summariser's own output for this session, not a
    // placeholder — compare against a direct call.
    let expected = summary::summarise(&done);
    let got: Vec<_> = done.findings.iter().map(|f| (&f.id, &f.state)).collect();
    let want: Vec<_> = expected.iter().map(|f| (&f.id, &f.state)).collect();
    assert_eq!(got, want);

    // A stopped session carries the closing marker.
    assert!(done.events.iter().any(|e| e.kind == EV_SESSION_STOPPED));
    assert!(done.events.iter().any(|e| e.kind == EV_SESSION_STARTED));
}

/// §15: "loss/restart of the GUI must not corrupt an active daemon-side session"
/// and §18's "GUI disconnect/restart does not create a second control path".
///
/// The session lives entirely in the daemon, so a client vanishing is simply an
/// absence of further requests. Recording continues, and nothing about a client
/// reconnecting can start a second one while the first is live.
#[test]
fn a_client_vanishing_and_reconnecting_cannot_fork_the_session() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();
    let started = engine.start(unique_session("reconnect"), &ctx).unwrap();

    // "GUI closes" — no calls at all for a while. Recording is unaffected.
    engine.tick(&ctx);
    engine.tick(&ctx);
    assert!(engine.is_recording());
    assert_eq!(engine.snapshot().unwrap().samples.len(), 2);

    // "GUI reconnects" and naively tries to start again: refused, and the
    // original session is untouched.
    let mut reconnect = session();
    reconnect.session_id = "val-from-new-client".into();
    assert!(engine.start(reconnect, &ctx).is_err());
    let now = engine.snapshot().unwrap();
    assert_eq!(now.session_id, started.session_id);
    assert_eq!(now.samples.len(), 2, "the live recording was not disturbed");
}

/// §5: a user marker lands on the same timeline as the engine's own events.
#[test]
fn a_user_marker_is_recorded_only_while_a_session_is_live() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();
    engine.start(unique_session("marker"), &ctx).unwrap();

    assert!(engine.push_event(EV_USER_MARKER, Some("pump to Quiet".into()), None));
    let s = engine.snapshot().unwrap();
    let marker = s
        .events
        .iter()
        .find(|e| e.kind == EV_USER_MARKER)
        .expect("the marker must be on the timeline");
    assert_eq!(marker.detail.as_deref(), Some("pump to Quiet"));

    engine.stop();
    assert!(
        !engine.push_event(EV_USER_MARKER, Some("too late".into()), None),
        "a finalised session must not accept new events"
    );
}

/// §14: external measurements are stored and returned, and are read by nothing.
#[test]
fn external_measurements_are_stored_untrusted_and_bounded() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();
    engine.start(unique_session("measurements"), &ctx).unwrap();

    assert!(engine.add_measurement(ExternalMeasurement {
        unix_ms: 1,
        kind: "supply_voltage_v".into(),
        value: 11.94,
        unit: "V".into(),
        member_id: Some("hwmon:it87:pwm2:PUMP".into()),
        note: None,
    }));
    let s = engine.snapshot().unwrap();
    assert_eq!(s.external_measurements.len(), 1);
    assert_eq!(s.external_measurements[0].value, 11.94);

    // Bounded — a client cannot grow the session without limit.
    for i in 0..control_ofc_daemon::constants::VALIDATION_MAX_EXTERNAL_MEASUREMENTS + 50 {
        engine.add_measurement(ExternalMeasurement {
            unix_ms: i as u64,
            kind: "pwm_duty_pct".into(),
            value: 50.0,
            unit: "%".into(),
            member_id: None,
            note: None,
        });
    }
    assert_eq!(
        engine.snapshot().unwrap().external_measurements.len(),
        control_ofc_daemon::constants::VALIDATION_MAX_EXTERNAL_MEASUREMENTS
    );
}

// ── Interleavings (added after the DEC-317 concurrency review) ──────────────
//
// The suite above is entirely synchronous, and that is exactly why four defects
// survived it: a session fence, a lock order, a save race and a rollback. Each
// test below reproduces one deterministically — with channels and held locks,
// never a wall-clock sleep, so none can flake and none depends on timing.

/// **The session fence.** An orchestration outlives a cancel, and without a fence
/// it appends the previous session's evidence to whatever session is live now.
///
/// This is fabricated evidence in the one artefact whose contract says nothing is
/// fabricated, and it changes the new session's findings.
#[test]
fn evidence_from_a_cancelled_session_cannot_land_on_its_successor() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();

    engine.start(unique_session("fence-a"), &ctx).unwrap();
    let a_id = "val-fence-a".to_string();

    // The user cancels A and immediately starts B — legal, and the exact
    // interleaving an in-flight orchestration sees.
    engine.cancel().unwrap();
    engine.start(unique_session("fence-b"), &ctx).unwrap();

    // Orchestration A, still running, tries to file its result.
    let accepted = engine.attach_evidence_for(
        &a_id,
        EvidenceRef {
            kind: DIAG_CHARACTERIZATION.into(),
            member_id: "hwmon:it87:pwm2:PUMP".into(),
            run_id: Some("char-from-A".into()),
            started_unix_ms: 1,
            completed_unix_ms: Some(2),
            outcome: RESULT_OBSERVED.into(),
            detail: None,
            characterization: None,
            verify: None,
        },
    );
    let event_accepted = engine.push_event_for(&a_id, EV_CHAR_COMPLETED, None, Some("pump".into()));

    assert!(
        !accepted,
        "A's evidence must be refused once A is not the live session"
    );
    assert!(!event_accepted, "and so must A's events");

    let b = engine.snapshot().unwrap();
    assert_eq!(b.session_id, "val-fence-b");
    assert!(
        b.evidence.is_empty(),
        "B must not carry a diagnostic it never requested"
    );
    assert!(
        !b.events.iter().any(|e| e.kind == EV_CHAR_COMPLETED),
        "nor an event from A's run"
    );

    // The fence is on identity, not on refusing everything: B's own evidence
    // still lands, so this test cannot pass by rejecting all writes.
    assert!(engine.attach_evidence_for(
        &b.session_id,
        EvidenceRef {
            kind: DIAG_VERIFY.into(),
            member_id: "hwmon:it87:pwm2:PUMP".into(),
            run_id: None,
            started_unix_ms: 3,
            completed_unix_ms: Some(4),
            outcome: RESULT_OBSERVED.into(),
            detail: None,
            characterization: None,
            verify: None,
        },
    ));
    assert_eq!(engine.snapshot().unwrap().evidence.len(), 1);
}

/// **The lock order.** A wedged hwmon write must not stall the recorder or the poll.
///
/// The recorder reads the controller once a second. Doing that *while holding the
/// session lock* — which `/status` and `/poll` also take — means a header wedged
/// in `std::fs::write` blocks every 1 Hz poll task in a non-cancellable
/// `parking_lot` acquisition on a tokio worker, and once the workers are
/// exhausted the profile-engine loop cannot be polled at all.
///
/// **The assertion is that `tick` RETURNS**, not that `live_summary` is fast.
/// The first version of this test measured `live_summary` latency from the main
/// thread and passed with the defect reinstated, because it usually won the race
/// to the lock before the ticker ever took it. `tick` completing is deterministic:
/// under the correct order it gives up on the controller after a short timeout,
/// and under the bad order it cannot return until the wedge releases.
///
/// **The wedge is now established BEFORE `start`, which is `AUD3-k`.** Until
/// 2026-09-04 this test started the session first, so `tick` was the only entry
/// point ever exercised against a wedged controller — and `start` was meanwhile
/// performing the exact inversion the comment above forbids, holding the session
/// slot across a bare blocking `c.lock()` inside `seed_watch`. A `POST
/// /validation/session` arriving during a DEC-278/289 wedge therefore parked a
/// tokio worker indefinitely *while holding the slot*, blocking the recorder,
/// `GET /validation/session` and `recording_session_id()` behind it. Both entry
/// points are asserted here now, in the order a real session meets them.
#[test]
fn a_wedged_controller_stalls_neither_the_recorder_nor_the_poll() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    temp_state_dir();
    let engine = Arc::new(ValidationEngine::new());
    let mut ctx = test_context();

    // No headers, so it can never write anything — the recorder only reads
    // `last_commanded_pct` and `enable_revert_counts`, both header-keyed. No
    // mock writer needed.
    let controller = Arc::new(parking_lot::Mutex::new(
        control_ofc_daemon::hwmon::pwm_control::HwmonPwmController::new(
            Vec::new(),
            control_ofc_daemon::hwmon::lease::LeaseManager::new(),
            Box::new(control_ofc_daemon::hwmon::pwm_control::RealSysfsWriter),
            ctx.cache.clone(),
        ),
    ));
    ctx.hwmon_controller = Some(controller.clone());

    // Wedge the controller from another thread, BEFORE the session starts. The
    // self-releasing deadline is deliberate: a failed assertion below skips this
    // test's own cleanup, and an unbounded held lock would turn a red test into a
    // hung CI job (DEC-272).
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let held = controller.clone();
    let wedge = std::thread::spawn(move || {
        let _guard = held.lock();
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });

    // Precondition: the wedging thread genuinely holds it. Without this the
    // test can pass by measuring a lock nobody ever took.
    let mut waited = 0;
    while controller.try_lock().is_some() {
        std::thread::sleep(Duration::from_millis(5));
        waited += 1;
        assert!(waited < 400, "the wedging thread never took the lock");
    }

    // THE FIRST assertion (`AUD3-k`): `start` returns while the controller is
    // wedged. On another thread with a bounded wait, for the same reason `tick`
    // is below — a `join` on the bad ordering hangs the job instead of failing it.
    let (start_tx, start_rx) = mpsc::channel::<bool>();
    {
        let engine = engine.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let ok = engine.start(unique_session("lockorder"), &ctx).is_ok();
            let _ = start_tx.send(ok);
        });
    }
    let admitted = start_rx.recv_timeout(Duration::from_secs(3)).expect(
        "start() did not return while the controller was wedged — it is \
                 holding the session lock across an unbounded controller acquisition",
    );
    assert!(
        admitted,
        "the session must still be admitted: a missed watchdog baseline costs one \
         spurious first-tick event, and is never a reason to refuse a recording"
    );

    // Tick on another thread, reporting completion through a channel so we can
    // bound the wait instead of blocking forever on `join`.
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    {
        let engine = engine.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let recorded = engine.tick(&ctx);
            let _ = done_tx.send(recorded);
        });
    }

    // THE assertion. Correct order: the tick abandons the controller after
    // `CONTROLLER_READ_TIMEOUT` and returns. Bad order: it holds the session lock
    // and blocks here until the 10 s wedge releases.
    let recorded = done_rx.recv_timeout(Duration::from_secs(3)).expect(
        "tick() did not return while the controller was wedged — it is \
                 holding the session lock across the controller acquisition",
    );
    assert!(recorded, "the tick still recorded a sample");

    // And the poll surface stays responsive throughout.
    let start = Instant::now();
    let summary = engine.live_summary();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "the poll surface blocked behind a wedged controller"
    );
    assert!(
        summary.is_some(),
        "a started session must be visible to the poll"
    );

    let _ = release_tx.send(());
    wedge.join().unwrap();

    // It recorded honestly: with the controller unavailable the commanded duty
    // is absent rather than invented.
    let s = engine.snapshot().unwrap();
    let sample = s.samples.last().expect("a sample was recorded");
    assert!(
        sample.members.iter().all(|m| m.requested_pct.is_none()),
        "an unreadable controller must yield `None`, never a fabricated duty"
    );
}

/// **The rejected start must not disturb the live session's baselines** —
/// the hazard `AUD3-k`'s fix introduces, and the reason the seed is computed
/// into a local rather than written straight into `self.watch`.
///
/// Moving `seed_watch` above the slot guard also moves it above the
/// `AlreadyRecording` check, so a second `POST /validation/session` now reads the
/// live values on its way to being refused. If it *installed* them, it would
/// silently re-baseline the recording session and the next tick would report no
/// change — a thermal failsafe entered between the two calls would vanish from
/// the timeline of the session that was running when it happened.
///
/// Asserted through the event the baseline produces, because `Watch` is private:
/// an intact baseline says "normal", sees "emergency", and emits. A clobbered one
/// says "emergency", sees "emergency", and says nothing.
#[test]
fn a_refused_start_does_not_reseed_the_recording_session() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();

    // Baseline: normal, at session start.
    ctx.cache.record_engine_tick("normal", 105.0);
    engine.start(unique_session("reseed-a"), &ctx).unwrap();

    // The ladder trips while the session is recording, and BEFORE the next tick
    // observes it — the window a re-seed would erase.
    ctx.cache.record_engine_tick("emergency", 105.0);

    // A second start arrives in that window and is refused.
    let err = engine.start(unique_session("reseed-b"), &ctx).unwrap_err();
    assert_eq!(
        err,
        control_ofc_daemon::validation::recorder::StartError::AlreadyRecording
    );

    // The refused start must have left A's baselines alone.
    assert!(engine.tick(&ctx), "A is still recording");
    let s = engine.snapshot().unwrap();
    assert!(
        s.events.iter().any(|e| e.kind == EV_THERMAL_ENTERED),
        "the failsafe A was recording through must still reach A's timeline; a \
         refused start re-seeded its baselines"
    );
    assert_eq!(
        s.session_id, "val-reseed-a",
        "and the refused start must not have installed itself"
    );

    engine.stop();
}

/// **The save race.** A periodic flush must never republish a stale `recording`
/// copy over a session that has already finalised.
///
/// If it does, the next boot sweep sees `recording`, marks a cleanly-stopped
/// session `interrupted`, and discards its findings.
#[test]
fn a_late_flush_cannot_resurrect_a_finalised_session() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();
    engine.start(unique_session("saverace"), &ctx).unwrap();
    engine.tick(&ctx);

    // Take the copy a flush would hold: still `recording`.
    let stale = engine.snapshot().unwrap();
    assert_eq!(stale.state, STATE_RECORDING);

    // The session finalises while that copy is in flight.
    let done = engine.stop().unwrap();
    assert_eq!(done.state, STATE_COMPLETED);
    assert!(!done.findings.is_empty());

    // Now the flush lands. It must be dropped, not published.
    engine.persist_for_test(&stale);

    let dir = temp_state_dir().join("validation");
    let back = store::load_from(&dir, &done.session_id).unwrap().unwrap();
    assert_eq!(
        back.state, STATE_COMPLETED,
        "a stale recording copy must not overwrite a finalised session"
    );
    assert!(!back.findings.is_empty(), "and its findings must survive");

    // The consequence the guard prevents: a boot sweep must not "repair" THIS
    // session. Scoped to it by id rather than asserting the sweep found nothing
    // at all — the temp state dir is shared with the other tests in this file,
    // several of which deliberately leave a session recording.
    let repaired = store::sweep_interrupted(&dir, "daemon_restart");
    assert!(
        !repaired.contains(&done.session_id),
        "a cleanly-stopped session must not be repaired to `interrupted`"
    );
}

/// **The shutdown flush shares the stale-write guard.** Found in review of DEC-323.
///
/// The recorder task's last write used to call `store::save` directly, bypassing
/// `persist`'s `save_lock` and its supersession check. That is a live race, and
/// DEC-323 made it likelier by moving the concurrent writer onto the blocking
/// pool: a `POST /validation/session/stop` in flight when shutdown is signalled
/// publishes `completed`, while the flush holds a `recording` snapshot taken
/// before it — and whichever `rename` lands second wins. The stale copy
/// resurrects a cleanly-stopped session as `recording`, and the next boot sweep
/// "repairs" it to `interrupted` and discards its findings.
///
/// Two halves, because the rule and its call site are different claims: the
/// engine method must obey the guard, and `main.rs` must actually use it.
#[test]
fn the_shutdown_flush_cannot_resurrect_a_finalised_session() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();
    engine
        .start(unique_session("shutdown-flush"), &ctx)
        .unwrap();
    engine.tick(&ctx);

    // While recording, it writes.
    assert!(
        engine.flush_recording(),
        "a recording session must be flushed at shutdown"
    );
    let dir = temp_state_dir().join("validation");
    let back = store::load_from(&dir, "val-shutdown-flush")
        .unwrap()
        .unwrap();
    assert_eq!(back.state, STATE_RECORDING);

    // Once finalised it writes nothing — and, critically, cannot publish a
    // `recording` copy over the `completed` one.
    let done = engine.stop().unwrap();
    assert_eq!(done.state, STATE_COMPLETED);
    assert!(
        !engine.flush_recording(),
        "a finalised session must not be re-flushed as `recording`"
    );
    let back = store::load_from(&dir, "val-shutdown-flush")
        .unwrap()
        .unwrap();
    assert_eq!(
        back.state, STATE_COMPLETED,
        "the shutdown flush must not resurrect a finalised session"
    );
    assert!(!back.findings.is_empty(), "and its findings must survive");

    // The call site: `main.rs` must reach the disk through the engine, so the
    // guard applies. A direct `store::save` there would satisfy every assertion
    // above and still carry the race.
    let main_body = strip_comments(include_str!("../src/main.rs"));
    assert!(
        main_body.contains("engine.flush_recording()"),
        "the recorder task's shutdown flush must go through the engine"
    );
    assert!(
        !main_body.contains("store::save"),
        "main.rs must not write a session document directly — that bypasses \
         `persist`'s save_lock and stale-write guard"
    );
}

/// **The rollback.** A failed persist must clear only the session it installed.
///
/// A slow failing save can outlive its own session — `stop()` finalises it and a
/// second `POST` legitimately admits a new one — and an unconditional clear would
/// then silently wipe a session this call never installed.
#[test]
fn a_failed_start_rollback_cannot_wipe_a_later_session() {
    temp_state_dir();
    let engine = ValidationEngine::new();
    let ctx = test_context();

    engine.start(unique_session("rollback-b"), &ctx).unwrap();
    let live = engine.snapshot().unwrap();

    // Simulate A's late rollback arriving with a foreign id.
    engine.rollback_for_test("val-rollback-a");

    let after = engine.snapshot().expect("B must still be installed");
    assert_eq!(after.session_id, live.session_id);
    assert!(engine.is_recording(), "B must still be recording");

    // The fence is on identity, not a no-op: rolling back B's own id does clear it.
    engine.rollback_for_test(&live.session_id);
    assert!(engine.snapshot().is_none());
}

// ── AUD3-i: the persisted document must be bounded in BYTES, not only in rows ──

/// Realistic AIO ids, at the length the daemon actually produces.
fn aio_member_ids(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("hwmon:it8696:isa-0a40:pwm{i}:CHA_FAN{i}"))
        .collect()
}

/// A session over `n` members whose samples carry `sensor` as their
/// `temperature_sensor` — the field `recorder.rs` copies into every sample.
fn sized_session(tag: &str, n: usize, sensor: &str) -> ValidationSession {
    let ids = aio_member_ids(n);
    let mut s = session();
    s.session_id = format!("val-{tag}");
    s.metadata.temperature_sensor = Some(sensor.to_string());
    s.metadata.members = ids
        .iter()
        .enumerate()
        .map(|(i, id)| member(id, if i == 0 { MEMBER_PUMP } else { MEMBER_RADIATOR }))
        .collect();
    s
}

/// Fill `s` to its derived cap with samples of the shape the recorder writes.
fn fill_to_cap(s: &mut ValidationSession) -> usize {
    let cap = max_samples_for(
        s,
        control_ofc_daemon::constants::VALIDATION_MAX_SAMPLE_BYTES,
    );
    let ids: Vec<String> = s
        .metadata
        .members
        .iter()
        .map(|m| m.member_id.clone())
        .collect();
    let sensor = s.metadata.temperature_sensor.clone();
    s.samples = (0..cap)
        .map(|i| ValidationSample {
            elapsed_ms: i as u64 * 1000,
            unix_ms: 1_757_000_000_000 + i as u64 * 1000,
            temperature_c: Some(65.5),
            temperature_sensor: sensor.clone(),
            coolant_c: Some(32.5),
            thermal_state: "normal".into(),
            members: ids
                .iter()
                .map(|id| member_sample(id, MEMBER_RADIATOR, Some(50), Some(50), Some(2100)))
                .collect(),
        })
        .collect();
    cap
}

/// Save `s` and return the realised file length.
fn saved_len(dir: &std::path::Path, s: &ValidationSession) -> u64 {
    store::save_to(dir, s).expect("a session at its own cap must be writable");
    std::fs::metadata(dir.join(format!("{}.json", s.session_id)))
        .unwrap()
        .len()
}

/// **The regression test for `AUD3-i`.** A pump plus two radiator fans, recorded
/// to the session's own cap, must still read back.
///
/// Before the fix the store wrote with no byte bound and read under
/// `atomic_io::MAX_CONFIG_BYTES` (4 MiB), while a three-member session at 7200
/// samples serialises to ~7.8 MiB. The file was written successfully and was then
/// invisible to *every* read path at once: `load_from` failed, `list_from` skipped
/// it, `GET /validation/sessions/{id}` 500'd, `sweep_interrupted` could never
/// repair it and `prune` could never delete it, so it also leaked disk for ever.
#[test]
fn a_session_at_its_derived_cap_still_reads_back_from_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = sized_session("bytes-cap", 3, "hwmon:k10temp:k10temp-pci-00c3:Tctl");
    let cap = fill_to_cap(&mut s);
    let len = saved_len(tmp.path(), &s);

    assert!(
        len <= control_ofc_daemon::constants::VALIDATION_MAX_SESSION_BYTES,
        "a session at its derived cap wrote {len} bytes, over the {} the store can read",
        control_ofc_daemon::constants::VALIDATION_MAX_SESSION_BYTES
    );
    // The precondition that makes this test non-vacuous: the file must actually
    // be big enough to have tripped the old 4 MiB cap, or it proves nothing.
    assert!(
        len > control_ofc_daemon::atomic_io::MAX_CONFIG_BYTES,
        "this fixture no longer reproduces the defect: {len} bytes is under the \
         old {} cap, so it would have round-tripped before the fix too",
        control_ofc_daemon::atomic_io::MAX_CONFIG_BYTES
    );

    let back = store::load_from(tmp.path(), "val-bytes-cap")
        .expect("a session the store wrote must be readable");
    assert!(back.is_some(), "the session must load, not vanish");
    assert_eq!(back.unwrap().samples.len(), cap);
    assert_eq!(
        store::list_from(tmp.path()).len(),
        1,
        "and it must be listed"
    );
}

/// The byte bound must hold **as a realised file**, across every topology —
/// including the many-member case where the derivation actually binds.
///
/// The first version of this test compared the derived cap against a per-sample
/// size measured with a *standalone* `to_vec_pretty`, which is strictly smaller
/// than the nested marginal cost the production code divides by — so it held by
/// construction and **passed with the nesting correction deleted**. That is this
/// project's recurring "the unit test proves you answered, never that you were
/// asked" shape, and it was caught by `ofc:security-reviewer`, not by the test.
/// Asserting the length of the file that is actually written cannot go vacuous
/// the same way.
#[test]
fn every_topology_writes_a_file_the_store_can_read_back() {
    let tmp = tempfile::tempdir().unwrap();
    let hard = control_ofc_daemon::constants::VALIDATION_MAX_SAMPLES;
    let budget = control_ofc_daemon::constants::VALIDATION_MAX_SAMPLE_BYTES;
    let cap_bytes = control_ofc_daemon::constants::VALIDATION_MAX_SESSION_BYTES;
    let mut derivation_bound_at_least_once = false;

    // 65 = the maximum a device may claim (1 pump + 2 x MAX_MEMBERS_PER_LIST).
    for n in [1usize, 3, 8, 65] {
        let mut s = sized_session(
            &format!("topo-{n}"),
            n,
            "hwmon:k10temp:k10temp-pci-00c3:Tctl",
        );
        let cap = fill_to_cap(&mut s);
        assert!(cap > 0, "{n} members must still record at least one sample");
        if cap < hard {
            derivation_bound_at_least_once = true;
        }
        let len = saved_len(tmp.path(), &s);
        assert!(
            len <= cap_bytes,
            "{n} members: {cap} samples wrote {len} bytes, over the {cap_bytes} cap"
        );

        // And the SAMPLES portion must stay inside its own budget, measured as
        // the difference the samples make to the realised file. The whole-file
        // check above is satisfied by the read cap's headroom and so cannot see
        // an over-spending derivation; this can. It is also the invariant the
        // `VALIDATION_MAX_SESSION_BYTES > SAMPLE + ANCILLARY` assertion rests
        // on — if samples may exceed their budget, the ancillary reservation is
        // not sound and the const assert is decoration.
        let mut empty = s.clone();
        empty.session_id = format!("{}-empty", s.session_id);
        empty.samples.clear();
        let samples_bytes = len - saved_len(tmp.path(), &empty);
        assert!(
            samples_bytes as usize <= budget,
            "{n} members: the samples array realised {samples_bytes} bytes, over its {budget} budget"
        );
        assert!(
            store::load_from(tmp.path(), &s.session_id)
                .unwrap()
                .is_some(),
            "{n} members: the file the store just wrote must read back"
        );
    }
    // Or the loop only ever exercised the clamp and proved nothing about the
    // derivation — the same precondition discipline as the test above.
    assert!(
        derivation_bound_at_least_once,
        "no topology in this sweep was actually bounded by the byte budget"
    );
}

/// **Regression for the defect found INSIDE the fix.** The per-sample probe used
/// a 128-byte placeholder for `temperature_sensor` on the reasoning that no real
/// sensor id is longer. But `recorder.rs` copies `metadata.temperature_sensor`
/// into every sample, and that string comes from `preferred_sensor` on
/// `POST /config/cooling-device` — client-supplied. A long one made the probe
/// under-count without bound and reproduced `AUD3-i` exactly. A guess is not a
/// bound; the probe now measures the session's own value.
#[test]
fn a_long_configured_sensor_id_cannot_break_the_byte_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let budget = control_ofc_daemon::constants::VALIDATION_MAX_SAMPLE_BYTES;
    let long = "s".repeat(control_ofc_daemon::hwmon::cooling_device::MAX_DEVICE_TEXT_BYTES);

    // Measured at a member count where the byte budget actually binds. At three
    // members the hard 7200 clamp masks the derivation entirely, so a comparison
    // there would pass whether or not the probe reads the sensor id at all —
    // pick the sample that can move (CLAUDE.md), or the check is blind.
    let long_cap = max_samples_for(&sized_session("l", 65, &long), budget);
    let short_cap = max_samples_for(&sized_session("s", 65, "hwmon:k10temp:Tctl"), budget);
    assert!(
        long_cap < short_cap,
        "the probe must read the session's own sensor id: {long_cap} vs {short_cap}"
    );

    let mut s = sized_session("long-sensor", 65, &long);
    let cap = fill_to_cap(&mut s);
    assert_eq!(cap, long_cap);
    let len = saved_len(tmp.path(), &s);
    assert!(
        len <= control_ofc_daemon::constants::VALIDATION_MAX_SESSION_BYTES,
        "a long configured sensor id produced {len} bytes, over the cap"
    );
    assert!(store::load_from(tmp.path(), "val-long-sensor")
        .unwrap()
        .is_some());
}

/// A realistic cooler keeps the documented two hours; only pathological
/// topologies shorten. Asserted as a relationship to the hard cap, never as a
/// sample count — the budget is allowed to move, the guarantee is not.
#[test]
fn the_derived_cap_is_the_full_two_hours_for_every_realistic_aio() {
    let budget = control_ofc_daemon::constants::VALIDATION_MAX_SAMPLE_BYTES;
    let hard = control_ofc_daemon::constants::VALIDATION_MAX_SAMPLES;
    let sensor = "hwmon:k10temp:k10temp-pci-00c3:Tctl";
    // Pump alone, through pump + four radiator fans — a 360 mm cooler is four
    // members, so this covers every consumer AIO with one member of margin.
    for n in 1..=5 {
        assert_eq!(
            max_samples_for(&sized_session("real", n, sensor), budget),
            hard,
            "a {n}-member cooler must still record the full sample cap"
        );
    }
    let pathological = max_samples_for(&sized_session("path", 65, sensor), budget);
    assert!(
        pathological < hard && pathological > 0,
        "a 65-member device must be bounded but still record: got {pathological}"
    );
}

/// An over-cap file is unreadable for ever, so retention can never reach it —
/// which is why `prune` must delete it rather than step over it.
#[test]
fn prune_deletes_a_session_too_large_to_read_but_spares_a_merely_corrupt_one() {
    let tmp = tempfile::tempdir().unwrap();
    let oversized = tmp.path().join("val-oversized.json");
    let corrupt = tmp.path().join("val-corrupt.json");
    std::fs::write(
        &oversized,
        vec![b'x'; control_ofc_daemon::constants::VALIDATION_MAX_SESSION_BYTES as usize + 1],
    )
    .unwrap();
    std::fs::write(&corrupt, b"{ not json").unwrap();

    store::prune(tmp.path(), 5);

    assert!(
        !oversized.exists(),
        "an unreadable oversized session must be reclaimed, or it occupies the \
         state directory permanently"
    );
    assert!(
        corrupt.exists(),
        "a merely unparseable session must NOT be deleted — a serde slip or a \
         transient read error would otherwise destroy every retained session"
    );
}

/// `prune` re-stats before removing, so a file that became readable between the
/// scan and the delete is spared.
///
/// Without this, a flush landing in that window could have its smaller, valid
/// replacement deleted — taking a live recording with it and leaving
/// `sweep_interrupted` nothing to mark `interrupted`, which is the one property
/// §15 requires the store to guarantee. Raised by `ofc:security-reviewer`.
#[test]
fn prune_spares_a_file_that_became_readable_since_the_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = sized_session("shrunk", 1, "hwmon:k10temp:Tctl");
    s.state = "completed".into();
    s.samples.clear();
    // A readable file at a path prune was told is oversized: the state prune
    // reaches when a flush lands between its scan and its delete.
    store::save_to(tmp.path(), &s).unwrap();
    store::prune(tmp.path(), 5);
    assert!(
        store::load_from(tmp.path(), "val-shrunk")
            .unwrap()
            .is_some(),
        "a readable session must never be pruned as oversized"
    );
}
