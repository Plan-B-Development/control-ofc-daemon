//! AIO Phase 8 Batch 1 — safety, control-path discovery and reporting.
//!
//! Every item in `AIO-Phase7-Batch1.md` §7 that the daemon owns maps to a named
//! test here. The GUI half lives in the paired repo's
//! `tests/test_aio_mb_phase8_batch1.py`.
//!
//! Two disciplines from `CLAUDE.md § Hard-won lessons` run through this file and
//! are worth reading before editing it:
//!
//! * **Assert the realised artefact, not a re-derivation.** The clamp tests read
//!   the duties the sweep ACTUALLY WROTE out of a recording write closure. A test
//!   that recomputed `perturbation_target` and compared it to itself would hold
//!   by construction and pass with the clamp deleted — the DEC-320 defect.
//! * **Stamp ages by construction, never by waiting.** `std::time::Instant` does
//!   not advance under `#[tokio::test(start_paused)]`, so a staleness test that
//!   slept would age by ~0 ms and pass vacuously (tokio trap 1).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use control_ofc_daemon::api::calibration as cal;
use control_ofc_daemon::api::characterization::{RestoreOutcome, RestoreReport};
use control_ofc_daemon::api::discovery as disc;
use control_ofc_daemon::api::preflight as pf;
use control_ofc_daemon::api::responses::HwmonVerifyState;
use control_ofc_daemon::constants;
use control_ofc_daemon::health::cache::StateCache;
use control_ofc_daemon::health::state::{CachedSensorReading, DeviceLabel};
use control_ofc_daemon::hwmon::types::SensorKind;

/// The pump floor, read from the daemon's own published policy rather than
/// written as `30`.
///
/// A literal here would be satisfied by a call site that hardcodes the same
/// literal, which is exactly the DEC-324 trap: the assertion must be a
/// RELATIONSHIP against the value the daemon actually resolves.
fn pump_floor() -> u8 {
    control_ofc_daemon::hwmon::device_policy::resolve_policy_floor(
        &control_ofc_daemon::hwmon::device_policy::GENERIC_PUMP,
        true,
    ) as u8
}

// ── Harness ──────────────────────────────────────────────────────────

fn cache_at(temp_c: f64, thermal_state: Option<&str>) -> StateCache {
    let cache = StateCache::new();
    cache.update_sensors(vec![CachedSensorReading {
        id: "cpu".into(),
        kind: SensorKind::CpuTemp,
        label: "Tctl".into(),
        value_c: temp_c,
        source: DeviceLabel::Hwmon,
        updated_at: Instant::now(),
        rate_c_per_s: None,
        session_min_c: None,
        session_max_c: None,
        chip_name: "k10temp".into(),
        temp_type: None,
        thresholds: None,
    }]);
    if let Some(s) = thermal_state {
        cache.record_engine_tick(s, constants::THERMAL_EMERGENCY_TRIGGER_C);
    }
    cache
}

/// A cache whose one CPU reading is `age` old, stamped **by construction**.
///
/// Never by waiting: `std::time::Instant` does not advance under
/// `#[tokio::test(start_paused)]`, so a sleeping staleness test would age by
/// ~0 ms and pass while asserting nothing (tokio trap 1, and the reason this
/// file's module doc names it).
fn cache_aged(temp_c: f64, age: Duration) -> StateCache {
    let cache = StateCache::new();
    cache.update_sensors(vec![CachedSensorReading {
        id: "cpu".into(),
        kind: SensorKind::CpuTemp,
        label: "Tctl".into(),
        value_c: temp_c,
        source: DeviceLabel::Hwmon,
        updated_at: Instant::now() - age,
        rate_c_per_s: None,
        session_min_c: None,
        session_max_c: None,
        chip_name: "k10temp".into(),
        temp_type: None,
        thresholds: None,
    }]);
    cache
}

fn channels(specs: &[(&str, bool)]) -> Vec<disc::TachChannel> {
    specs
        .iter()
        .map(|(id, is_target)| disc::TachChannel {
            tach_id: (*id).to_string(),
            label: (*id).to_string(),
            monitor_only: !*is_target && id.contains("fan"),
            is_target_header: *is_target,
        })
        .collect()
}

/// A scripted machine: the duty the sweep writes decides what every tach reads.
///
/// `rpm_for` is the model of the hardware — `|tach_index, duty| -> Option<u16>`.
/// Recording the writes is what lets the clamp assertions look at the realised
/// artefact rather than at arithmetic they performed themselves.
/// How the modelled driver reports `pwm_enable`.
#[derive(Clone, Copy)]
enum EnableModel {
    /// Whatever value is set, at every duty.
    Fixed(Option<u8>),
    /// The DEC-326 driver: reports `0` to mean "full speed" **only while 100 %
    /// is commanded**, and `1` otherwise. Modelling it as a permanent `0` would
    /// be modelling a driver that does not exist, and the resulting test would
    /// assert the wrong thing about the reclaim rule.
    AliasAtFull,
}

struct Rig {
    written: Arc<Mutex<Vec<u8>>>,
    duty: Arc<Mutex<u8>>,
    enable: Arc<Mutex<EnableModel>>,
    reads: Arc<AtomicUsize>,
    /// How many times the sweep proved liveness. [SAFETY] This is the DEC-296
    /// cadence, and it is asserted rather than assumed: renewing too rarely lets
    /// the engine-pause deadman expire mid-run.
    keepalives: Arc<AtomicUsize>,
    /// When `Some(n)`, the n-th keepalive (0-based) refuses — modelling this run
    /// having been superseded by a later diagnostic.
    keepalive_fails_at: Option<usize>,
}

impl Rig {
    fn new(initial_duty: u8) -> Self {
        Self {
            written: Arc::new(Mutex::new(Vec::new())),
            duty: Arc::new(Mutex::new(initial_duty)),
            enable: Arc::new(Mutex::new(EnableModel::Fixed(Some(1)))),
            reads: Arc::new(AtomicUsize::new(0)),
            keepalives: Arc::new(AtomicUsize::new(0)),
            keepalive_fails_at: None,
        }
    }
    fn keepalive_count(&self) -> usize {
        self.keepalives.load(Ordering::SeqCst)
    }
    fn writes(&self) -> Vec<u8> {
        self.written.lock().unwrap().clone()
    }
}

/// Deterministic mid-run thermal trip.
///
/// The ladder is engaged from inside the READ closure at a chosen read index
/// rather than from a timer, so the moment it fires is a function of the sweep's
/// own progress and not of wall-clock timing. A `sleep`-based trigger would be
/// flaky, and under `start_paused` it would not fire at all.
struct ThermalTrip {
    /// Engage once this duty has been commanded.
    ///
    /// Keyed off the duty rather than a read index deliberately: the number of
    /// sub-samples in a window depends on the window length and the sample
    /// interval, so an index would silently stop landing where intended the
    /// moment either changed — and the test would go green for the wrong reason.
    once_duty_is: u8,
    state: &'static str,
}

#[allow(clippy::too_many_arguments)]
async fn sweep(
    rig: &Rig,
    cache: &Arc<StateCache>,
    chans: &[disc::TachChannel],
    baseline: u8,
    perturbed: u8,
    cycles: u8,
    restore_floor: u8,
    pump_protected: bool,
    rpm_for: impl Fn(usize, u8) -> Option<u16> + Send + Sync + 'static,
    write_fails_on: Option<u8>,
    trip: Option<ThermalTrip>,
    cancel: &AtomicBool,
    report: &RestoreReport,
) -> disc::DiscoveryOutcome {
    let written = rig.written.clone();
    let duty_w = rig.duty.clone();
    let duty_r = rig.duty.clone();
    let enable = rig.enable.clone();
    let reads = rig.reads.clone();
    let n = chans.len();

    let write_fn = move |pct: u8| -> Result<(), String> {
        if Some(pct) == write_fails_on {
            return Err("simulated write failure".into());
        }
        written.lock().unwrap().push(pct);
        *duty_w.lock().unwrap() = pct;
        Ok(())
    };
    let keepalives = rig.keepalives.clone();
    let fails_at = rig.keepalive_fails_at;
    let keepalive = move || {
        let n = keepalives.fetch_add(1, Ordering::SeqCst);
        fails_at != Some(n)
    };

    let trip_cache = cache.clone();
    let read_fn = move || {
        reads.fetch_add(1, Ordering::SeqCst);
        let d = *duty_r.lock().unwrap();
        if let Some(t) = &trip {
            if d == t.once_duty_is {
                trip_cache.record_engine_tick(t.state, constants::THERMAL_EMERGENCY_TRIGGER_C);
            }
        }
        disc::DiscoverySample {
            header: HwmonVerifyState {
                pwm_enable: match *enable.lock().unwrap() {
                    EnableModel::Fixed(v) => v,
                    EnableModel::AliasAtFull if d == 100 => Some(0),
                    EnableModel::AliasAtFull => Some(1),
                },
                pwm_raw: Some(((d as u16 * 255) / 100) as u8),
                pwm_percent: Some(d),
                rpm: rpm_for(usize::MAX, d),
            },
            tachs: (0..n).map(|i| rpm_for(i, d)).collect(),
        }
    };

    disc::run_discovery(
        cache.as_ref(),
        "hwmon:nct6798:isa:pwm2:AIO_PUMP",
        chans,
        baseline,
        perturbed,
        "up",
        cycles,
        restore_floor,
        pump_protected,
        Duration::from_millis(20),
        write_fn,
        read_fn,
        cancel,
        || false,
        keepalive,
        report,
        |_| {},
    )
    .await
}

// ── §7: ambiguous role receives pump-safe limits ─────────────────────

/// [SAFETY] **0 % is unreachable through discovery, for every header.**
///
/// Exhaustive over every `(baseline, delta, floor)` triple rather than sampled,
/// because a sampled check cannot prove "never" — the same discipline
/// `characterization::resolve_points` is held to.
#[test]
fn perturbation_never_reaches_zero_or_crosses_the_floor() {
    for floor in [0u8, pump_floor(), 50, 100] {
        for baseline in 0..=100u8 {
            for delta in [
                constants::DISCOVERY_DELTA_MIN_PCT,
                constants::DISCOVERY_DELTA_PCT,
                constants::DISCOVERY_DELTA_MAX_PCT,
            ] {
                let (target, _) = disc::perturbation_target(baseline, delta, floor);
                let lo = floor.max(constants::DISCOVERY_MIN_PCT);
                assert!(
                    target > 0,
                    "baseline={baseline} delta={delta} floor={floor}"
                );
                assert!(
                    target >= lo,
                    "target {target} below floor {lo} (baseline={baseline} delta={delta})"
                );
                assert!(target <= 100);
                // The baseline the sweep returns to is clamped by the same rule,
                // so the between-cycle write cannot undercut the floor either.
                let base = disc::resolve_baseline(Some(baseline), floor);
                assert!(base >= lo && base > 0);
            }
        }
    }
}

/// §1: "Treat ambiguous fan/pump classification conservatively as pump-safe."
///
/// Asserted as a RELATIONSHIP against the floor the daemon resolves, not against
/// the literal 30 — a literal is satisfied by a call site that hardcodes it,
/// which is the DEC-324 trap.
#[test]
fn a_pump_protected_header_is_perturbed_within_its_own_floor() {
    let floor = pump_floor();
    // A pump idling just above its floor: the only safe direction is UP.
    let (target, direction) =
        disc::perturbation_target(floor + 2, constants::DISCOVERY_DELTA_PCT, floor);
    assert_eq!(direction, "up");
    assert!(target > floor + 2);
    // ...and one near the ceiling goes DOWN, but never through the floor.
    let (target, direction) = disc::perturbation_target(98, constants::DISCOVERY_DELTA_PCT, floor);
    assert_eq!(direction, "down");
    assert!(target >= floor);
    assert!(target < 98);
}

/// The direction rule is "away from the NEARER rail". Both branches, because a
/// predicate stuck on one answer passes a one-sided test.
#[test]
fn perturbation_direction_moves_away_from_the_nearer_rail() {
    let floor = 0u8;
    let lo = constants::DISCOVERY_MIN_PCT;
    // Nearer the floor → up.
    assert_eq!(disc::perturbation_target(lo + 1, 25, floor).1, "up");
    // Nearer the ceiling → down.
    assert_eq!(disc::perturbation_target(99, 25, floor).1, "down");
    // Exactly mid-range prefers up, which never walks a pump toward a stall.
    let mid = lo + (100 - lo) / 2;
    assert_eq!(disc::perturbation_target(mid, 25, floor).1, "up");
}

/// Clamps on caller-supplied tuning. A client may ASK; the daemon decides.
#[test]
fn caller_supplied_tuning_is_clamped_server_side() {
    assert_eq!(
        disc::resolve_delta(Some(0)),
        constants::DISCOVERY_DELTA_MIN_PCT
    );
    assert_eq!(
        disc::resolve_delta(Some(255)),
        constants::DISCOVERY_DELTA_MAX_PCT
    );
    assert_eq!(disc::resolve_delta(None), constants::DISCOVERY_DELTA_PCT);
    // Cycles floor at 2: repeatability is a confidence input, so a one-cycle run
    // must not be able to claim it tested for it.
    assert_eq!(
        disc::resolve_cycles(Some(1)),
        constants::DISCOVERY_DEFAULT_CYCLES
    );
    assert_eq!(
        disc::resolve_cycles(Some(255)),
        constants::DISCOVERY_MAX_CYCLES
    );
}

// ── §7: the realised artefact — what was actually written ────────────

/// [SAFETY] The duties the sweep REALLY WROTE never go below the pump floor and
/// never reach 0.
///
/// This reads the recording write closure rather than re-deriving
/// `perturbation_target`, because a test that recomputes production's own model
/// shares production's blind spot by construction (DEC-320).
#[tokio::test]
async fn every_duty_actually_written_respects_the_pump_floor() {
    let rig = Rig::new(35);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);
    let (perturbed, _) =
        disc::perturbation_target(35, constants::DISCOVERY_DELTA_PCT, pump_floor());

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        35,
        perturbed,
        2,
        pump_floor(),
        true,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_COMPLETE);
    let writes = rig.writes();
    assert!(!writes.is_empty(), "the sweep wrote nothing at all");
    for w in &writes {
        assert!(*w >= pump_floor(), "wrote {w}% to a pump-protected header");
        assert!(*w > 0);
    }
}

// ── §7: state restoration on success, error and cancellation ─────────

#[tokio::test]
async fn restoration_occurs_on_success() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_COMPLETE);
    assert_eq!(report.get(), RestoreOutcome::Restored);
    assert!(!report.get().header_left_moved());
    assert_eq!(
        rig.writes().last().copied(),
        Some(45),
        "the last write must be the captured pre-run duty"
    );
}

#[tokio::test]
async fn restoration_occurs_on_a_failed_write() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    // Fail the PERTURBED write, so the run dies mid-cycle with the header
    // already moved.
    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        Some(70),
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_FAILED);
    assert_eq!(report.get(), RestoreOutcome::Restored);
    assert_eq!(rig.writes().last().copied(), Some(45));
}

#[tokio::test]
async fn restoration_occurs_on_cancellation() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(true); // cancelled before the first cycle

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_CANCELLED);
    assert_eq!(report.get(), RestoreOutcome::Restored);
    assert!(!report.get().header_left_moved());
    // The sweep never reached a perturbation, and the drop guard still put the
    // captured duty back — a restore is written on EVERY exit, not only after a
    // run that moved something.
    assert!(
        !rig.writes().contains(&70),
        "a cancelled run must never reach its perturbed duty: {:?}",
        rig.writes()
    );
    assert_eq!(rig.writes(), vec![45]);
}

/// §1: the ladder outranks a diagnostic. The run aborts AND the restore stands
/// down rather than fighting it — `skipped_thermal_force`, never "restored".
///
/// The ladder is engaged **mid-run**, after the sweep has genuinely moved the
/// header. That ordering IS the test: with the ladder already forcing at entry
/// nothing is ever written, and `RestoreOnDrop` then correctly reports
/// `Restored` because the header was never moved — so a version of this test
/// that tripped the ladder up front would assert the wrong outcome and would
/// pass with the thermal skip deleted. (The entry case is its own test below.)
#[tokio::test]
async fn a_forcing_ladder_aborts_and_the_restore_stands_down() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        // Fires once cycle 1 has commanded the PERTURBED duty, so the header is
        // provably moved before the ladder engages.
        Some(ThermalTrip {
            once_duty_is: 70,
            state: "emergency",
        }),
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_ABORTED);
    assert!(
        outcome
            .detail
            .as_deref()
            .unwrap()
            .contains("thermal safety"),
        "detail was {:?}",
        outcome.detail
    );
    assert_eq!(report.get(), RestoreOutcome::SkippedThermalForce);
    assert!(report.get().header_left_moved(), "the header IS left moved");
    // Precondition: the run really did move the header before the trip. Without
    // this the assertions above could hold for a run that never wrote.
    assert!(
        rig.writes().contains(&70),
        "the sweep never perturbed, so the skip was not exercised: {:?}",
        rig.writes()
    );
    // ...and it did NOT write the captured duty back over the forced duty.
    assert_ne!(rig.writes().last().copied(), Some(45));
}

/// The ladder forcing at ENTRY refuses before writing anything, and the guard
/// then reports `Restored` because the header was never moved. The pair of tests
/// is what distinguishes "stood down from a restore" from "had nothing to
/// restore" — `AUD2-c`'s exact distinction.
#[tokio::test]
async fn a_ladder_already_forcing_refuses_before_writing() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(40.0, Some("emergency")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_ABORTED);
    assert!(rig.writes().is_empty(), "nothing may be written at all");
    assert_eq!(report.get(), RestoreOutcome::Restored);
    assert!(!report.get().header_left_moved());
}

/// A sensor over the diagnostic limit refuses too — the OTHER thermal limb.
/// Both are checked because they are not the same test (DEC-297).
#[tokio::test]
async fn an_overheating_sensor_aborts_the_run() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(
        constants::CALIBRATION_MAX_TEMP_C + 1.0,
        Some("normal"),
    ));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_ABORTED);
    // Refused before any perturbation; the drop guard still restored.
    assert!(!rig.writes().contains(&70));
    assert_eq!(rig.writes(), vec![45]);
    assert_eq!(report.get(), RestoreOutcome::Restored);
}

// ── §7: ownership loss aborts active testing ─────────────────────────

#[tokio::test]
async fn ownership_loss_aborts_active_testing() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);
    // BIOS takes the header back: pwm_enable flips to 2 and stays there.
    *rig.enable.lock().unwrap() = EnableModel::Fixed(Some(2));

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_ABORTED);
    assert!(
        outcome.detail.as_deref().unwrap().contains("reclaimed"),
        "detail was {:?}",
        outcome.detail
    );
    // ...and the header is still put back, which is the whole point of the guard.
    assert_eq!(rig.writes().last().copied(), Some(45));
}

/// The DEC-326 full-speed-alias exemption survives: `pwm_enable=0` reflecting a
/// 100 % write is OUR write coming back, not somebody else's reclaim. Without
/// this limb the abort above would fire on every healthy driver of that kind.
#[tokio::test]
async fn the_full_speed_alias_is_not_treated_as_a_reclaim() {
    // The alias is `pwm_enable == 0 && requested == 100 && readback == 100`, so
    // the run must genuinely perturb TO 100 for the exemption to be in play —
    // asserted as a precondition, or this test proves nothing.
    //
    // Reaching 100 needs a header whose floor is high enough that "away from the
    // nearer rail" still points up, i.e. a device policy declaring a high safe
    // minimum. That is also the only configuration where discovery commands 100
    // at all, which is a safety property worth noting: on an ordinary header the
    // rule keeps the perturbation clear of both rails.
    let high_floor = 80u8;
    let (perturbed, direction) =
        disc::perturbation_target(high_floor, constants::DISCOVERY_DELTA_PCT, high_floor);
    assert_eq!(direction, "up");
    assert_eq!(
        perturbed, 100,
        "this test needs the perturbation to reach 100%"
    );

    let rig = Rig::new(high_floor);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);
    *rig.enable.lock().unwrap() = EnableModel::AliasAtFull;

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        high_floor,
        perturbed,
        2,
        high_floor,
        true,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(
        outcome.state,
        disc::STATE_COMPLETE,
        "a full-speed alias was misread as a reclaim: {:?}",
        outcome.detail
    );
}

// ── §7: pump tach disappearance aborts safely ────────────────────────

/// [SAFETY] The new abort predicate, at the sweep level.
#[tokio::test]
async fn a_pump_whose_tach_disappears_aborts_and_restores() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    // The tach reports at the baseline duty and vanishes at the perturbed one.
    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        pump_floor(),
        true,
        |_, duty| if duty >= 70 { None } else { Some(900) },
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_ABORTED);
    assert!(
        outcome.detail.as_deref().unwrap().contains("tachometer"),
        "detail was {:?}",
        outcome.detail
    );
    assert_eq!(rig.writes().last().copied(), Some(45), "not restored");
}

/// ...and the same disappearance on a NON-pump header does not abort. Without
/// this branch a predicate stuck on `true` would pass the test above.
#[tokio::test]
async fn a_chassis_fan_whose_tach_disappears_does_not_abort() {
    let rig = Rig::new(45);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("chassis", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| if duty >= 70 { None } else { Some(900) },
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_COMPLETE);
}

/// The predicate itself, all four corners. A pump with no tach at ALL must not
/// abort every run on that board.
#[test]
fn pump_tach_lost_requires_a_tach_that_was_there_to_begin_with() {
    assert!(disc::pump_tach_lost(true, true, None));
    assert!(!disc::pump_tach_lost(true, true, Some(900)));
    assert!(
        !disc::pump_tach_lost(true, false, None),
        "a pump with no tach must not abort"
    );
    assert!(
        !disc::pump_tach_lost(false, true, None),
        "a chassis fan is not pump-protected"
    );
}

// ── DEC-296: liveness cadence and supersession ───────────────────────

/// [SAFETY] The engine-pause deadman is renewed before **every observation
/// window**, not once per cycle.
///
/// This is the assertion that catches the P1 this batch shipped and then fixed:
/// a cycle holds TWO windows, so renewing per cycle makes the renewal interval
/// `2 × window`, which at the documented maximum settle (15 s) equals
/// `VERIFY_PAUSE_DEADMAN` (30 s) before any I/O overhead. The pause then expires
/// mid-run, the engine's write phase resumes, and `try_begin_verify`'s steal
/// branch lets a second diagnostic force-take this run's lease — so even the
/// restore write fails and the header is parked at the perturbed duty.
///
/// Asserted as a RELATIONSHIP to the windows actually held, not as a literal:
/// `cycles × 2` is what "once per window" means, and a literal would still pass
/// if the cycle count changed.
#[tokio::test]
async fn liveness_is_proved_before_every_observation_window() {
    for cycles in [2u8, 3] {
        let rig = Rig::new(45);
        let cache = Arc::new(cache_at(40.0, Some("normal")));
        let chans = channels(&[("pump", true)]);
        let report = RestoreReport::new();
        let cancel = AtomicBool::new(false);

        let outcome = sweep(
            &rig,
            &cache,
            &chans,
            45,
            70,
            cycles,
            0,
            false,
            |_, duty| Some(u16::from(duty) * 20),
            None,
            None,
            &cancel,
            &report,
        )
        .await;

        assert_eq!(outcome.state, disc::STATE_COMPLETE);
        assert_eq!(
            rig.keepalive_count(),
            usize::from(cycles) * 2,
            "a cycle holds two windows, so it must renew twice — renewing once \
             per cycle doubles the interval past the deadman"
        );
    }
}

/// The worst-case run's renewal interval must fit inside the deadman with margin.
///
/// Arithmetic rather than a timed run, deliberately: a real 90 s sweep in the
/// test suite would be intolerable, and the property being checked is a bound on
/// the interval rather than anything about wall-clock behaviour.
#[test]
fn the_worst_case_renewal_interval_fits_inside_the_pause_deadman() {
    // One window per renewal — the invariant the sweep above enforces.
    let interval = constants::CHARACTERIZATION_SETTLE_MAX_S;
    let deadman = constants::VERIFY_PAUSE_DEADMAN.as_secs();
    assert!(
        interval * 2 <= deadman,
        "a {interval}s renewal interval leaves no margin inside a {deadman}s deadman"
    );
    // ...and the per-CYCLE interval, which is what a regression would restore,
    // does NOT fit. Without this limb the assertion above passes either way and
    // proves nothing about the distinction.
    assert!(
        interval * 2 * 2 > deadman,
        "the per-cycle interval must NOT fit, or this test cannot tell the two apart"
    );
}

/// A refused keepalive means a later diagnostic superseded this run: it aborts,
/// says so, and still restores.
#[tokio::test]
async fn a_superseded_run_aborts_and_restores() {
    let mut rig = Rig::new(45);
    // Refuse the SECOND renewal — after the run has genuinely moved the header,
    // so the restore path is the one being exercised rather than a no-op exit.
    rig.keepalive_fails_at = Some(1);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_ABORTED);
    assert!(
        outcome.detail.as_deref().unwrap().contains("superseded"),
        "detail was {:?}",
        outcome.detail
    );
    // Precondition: the run really had moved the header before it was superseded.
    assert!(rig.writes().contains(&45), "the sweep never wrote at all");
    assert_eq!(rig.writes().last().copied(), Some(45), "not restored");
    assert_eq!(report.get(), RestoreOutcome::Restored);
}

/// A run superseded before its FIRST write leaves the header untouched.
#[tokio::test]
async fn a_run_superseded_before_writing_touches_nothing() {
    let mut rig = Rig::new(45);
    rig.keepalive_fails_at = Some(0);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        45,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_ABORTED);
    // The sweep never reached a perturbation. The drop guard still writes the
    // captured duty back — a restore is attempted on EVERY exit, not only after
    // a run that moved something — and reports the header as un-moved, which is
    // `AUD2-c`'s distinction.
    assert!(
        !rig.writes().contains(&70),
        "a superseded run must not reach its perturbed duty: {:?}",
        rig.writes()
    );
    assert!(!report.get().header_left_moved());

    // Honest limit of this rig: in production a superseded run's lease has been
    // force-taken, so that restore write would fail `InvalidLease` and the guard
    // would report `write_failed`. The rig's write closure always succeeds, so
    // this asserts the sweep's CONTROL FLOW under supersession, not the lease
    // outcome. The lease behaviour is covered by the daemon's own lease tests.
}

// ── §7: single mapping, multiple mappings, no response ───────────────

/// One PWM → one tach, cleanly, in every cycle.
#[tokio::test]
async fn discovery_identifies_a_simulated_single_mapping() {
    let rig = Rig::new(40);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true), ("fan2", false), ("fan3", false)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        40,
        65,
        2,
        0,
        false,
        // Only channel 0 follows the duty; the others are steady.
        |i, duty| match i {
            0 => Some(u16::from(duty) * 20),
            1 => Some(1500),
            _ => Some(800),
        },
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_COMPLETE);
    let sum = disc::summarise(
        &chans,
        &outcome.cycles,
        None,
        outcome.observed_resolution_ms,
        outcome.sample_count,
    );
    assert_eq!(sum.relationship, disc::REL_CONFIRMED);
    assert_eq!(sum.confidence, disc::CONF_HIGH);
    assert_eq!(sum.candidates.len(), 1);
    assert_eq!(sum.candidates[0].tach_id, "pump");
    assert_eq!(sum.candidates[0].direction, "positive");
    assert_eq!(sum.candidates[0].cycles_responded, 2);
}

/// §2: "Do not assume one PWM always maps to exactly one tach." A splitter puts
/// two fans on one header, and BOTH must be reported.
///
/// This is the case the obvious "target must move 3× more than any other
/// channel" design gets wrong — it would reject both and report no response.
#[tokio::test]
async fn multiple_responding_tachs_are_represented() {
    let rig = Rig::new(40);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true), ("fan2", false), ("fan3", false)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        40,
        65,
        2,
        0,
        false,
        |i, duty| match i {
            0 => Some(u16::from(duty) * 20),
            1 => Some(u16::from(duty) * 18),
            _ => Some(800),
        },
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    let sum = disc::summarise(
        &chans,
        &outcome.cycles,
        None,
        outcome.observed_resolution_ms,
        outcome.sample_count,
    );
    assert_eq!(sum.relationship, disc::REL_MULTIPLE);
    assert_eq!(sum.candidates.len(), 2);
    let ids: Vec<&str> = sum.candidates.iter().map(|c| c.tach_id.as_str()).collect();
    assert!(
        ids.contains(&"pump") && ids.contains(&"fan2"),
        "got {ids:?}"
    );
}

/// §5/§7: no response is `no_tach_response`, and it is NOT a failure. The
/// Overview is explicit — do not label an unexpected RPM response as hardware
/// failure unless evidence supports it.
#[tokio::test]
async fn the_no_response_case_is_represented_without_a_false_failure() {
    let rig = Rig::new(40);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true), ("fan2", false)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        40,
        65,
        2,
        0,
        false,
        |_, _| Some(1200), // perfectly steady: a device under its own control
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_COMPLETE, "the RUN succeeded");
    let sum = disc::summarise(
        &chans,
        &outcome.cycles,
        None,
        outcome.observed_resolution_ms,
        outcome.sample_count,
    );
    assert_eq!(sum.relationship, disc::REL_NO_RESPONSE);
    assert!(sum.candidates.is_empty());
    // Distinguishable from "we could not measure": the tachs were readable.
    assert_eq!(sum.confidence, disc::CONF_LOW);
    assert!(!sum.confidence_notes.is_empty());
}

/// ...and an entirely unreadable tach set is UNKNOWN, not a low-confidence
/// no-response. §5: lack of evidence must not become a result.
#[tokio::test]
async fn unreadable_tachs_report_unknown_rather_than_no_response() {
    let rig = Rig::new(40);
    let cache = Arc::new(cache_at(40.0, Some("normal")));
    let chans = channels(&[("pump", true)]);
    let report = RestoreReport::new();
    let cancel = AtomicBool::new(false);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        40,
        65,
        2,
        0,
        false,
        |_, _| None,
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    let sum = disc::summarise(
        &chans,
        &outcome.cycles,
        None,
        outcome.observed_resolution_ms,
        outcome.sample_count,
    );
    assert_eq!(sum.relationship, disc::REL_NO_RESPONSE);
    assert_eq!(sum.confidence, disc::CONF_UNKNOWN);
}

/// A channel that answers in only some cycles is `ambiguous` — the whole reason
/// two cycles are run.
#[tokio::test]
async fn an_inconsistent_responder_is_ambiguous_not_confirmed() {
    let chans = channels(&[("pump", true), ("fan2", false)]);
    // Hand-built cycles: responded in cycle 1, not in cycle 2.
    let mk = |cycle: u8, responded: bool| disc::DiscoveryCycle {
        cycle,
        baseline_pct: 40,
        perturbed_pct: 65,
        direction: "up".into(),
        observations: vec![
            disc::TachObservation {
                tach_id: "pump".into(),
                baseline_rpm: Some(800),
                perturbed_rpm: Some(if responded { 1300 } else { 805 }),
                delta_rpm: Some(if responded { 500 } else { 5 }),
                noise_floor_rpm: 50,
                responded,
            },
            disc::TachObservation {
                tach_id: "fan2".into(),
                baseline_rpm: Some(1000),
                perturbed_rpm: Some(1000),
                delta_rpm: Some(0),
                noise_floor_rpm: 50,
                responded: false,
            },
        ],
    };
    let cycles = vec![mk(1, true), mk(2, false)];
    let sum = disc::summarise(&chans, &cycles, None, None, 8);
    assert_eq!(sum.relationship, disc::REL_AMBIGUOUS);
    assert_eq!(sum.candidates[0].confidence, disc::CONF_LOW);
    assert_eq!(sum.candidates[0].cycles_responded, 1);
    assert_eq!(sum.candidates[0].cycles_total, 2);
}

// ── §2: response and noise-floor predicates ──────────────────────────

#[test]
fn response_needs_both_the_noise_floor_and_the_relative_threshold() {
    // Clears the absolute floor but not 10 % of a fast fan's baseline.
    assert!(!disc::responded(Some(2000), Some(2060), 50));
    // Clears both on a slow pump.
    assert!(disc::responded(Some(300), Some(400), 50));
    // Clears the relative test but not a NOISY channel's measured floor.
    assert!(!disc::responded(Some(1000), Some(1105), 300));
    // Unreadable is never a response.
    assert!(!disc::responded(None, Some(1200), 50));
    assert!(!disc::responded(Some(1200), None, 50));
}

#[test]
fn the_noise_floor_is_measured_and_never_below_the_absolute_minimum() {
    // A steady channel still gets the absolute floor, or any flicker would read
    // as a response.
    assert_eq!(
        disc::noise_floor(&[Some(1000), Some(1000), Some(1000)]),
        constants::DISCOVERY_MIN_NOISE_FLOOR_RPM
    );
    // A jittery channel earns a higher bar than the minimum.
    let noisy = disc::noise_floor(&[Some(900), Some(1200), Some(950)]);
    assert_eq!(noisy, 300);
    assert!(noisy > constants::DISCOVERY_MIN_NOISE_FLOOR_RPM);
    // All-unreadable falls back to the minimum rather than panicking.
    assert_eq!(
        disc::noise_floor(&[None, None]),
        constants::DISCOVERY_MIN_NOISE_FLOOR_RPM
    );
}

// ── §4: measurement resolution ───────────────────────────────────────

/// §4: "If driver update cadence cannot be established, mark it UNKNOWN rather
/// than guessing."
#[test]
fn measurement_resolution_is_unknown_when_nothing_ever_changed() {
    let flat = [(0u64, Some(1000u16)), (500, Some(1000)), (1000, Some(1000))];
    assert_eq!(disc::measurement_resolution_ms(&flat), None);
}

#[test]
fn measurement_resolution_is_the_smallest_gap_between_changes() {
    // Changes at 1000 and 3000 → the driver refreshes about every 2 s, so
    // sub-second timings from a 500 ms sampler are not meaningful.
    let samples = [
        (0u64, Some(1000u16)),
        (500, Some(1000)),
        (1000, Some(1200)),
        (1500, Some(1200)),
        (2000, Some(1200)),
        (2500, Some(1200)),
        (3000, Some(1400)),
    ];
    assert_eq!(disc::measurement_resolution_ms(&samples), Some(2000));
}

/// A single change yields UNKNOWN, not a cadence.
///
/// The gap between the first sample and the first change measures when we
/// started looking, not how often the driver refreshes — counting it would
/// under-report the cadence, which is the false precision §4 exists to prevent.
#[test]
fn one_observed_change_is_not_enough_to_establish_a_cadence() {
    let one = [
        (0u64, Some(1000u16)),
        (500, Some(1000)),
        (1000, Some(1200)),
        (1500, Some(1200)),
    ];
    assert_eq!(disc::measurement_resolution_ms(&one), None);
}

/// The driver's own `update_interval` outranks the observed estimate (§4).
#[test]
fn a_declared_driver_cadence_outranks_the_observed_one() {
    let chans = channels(&[("pump", true)]);
    let sum = disc::summarise(&chans, &[], Some(1000), Some(4000), 12);
    assert_eq!(sum.measurement_resolution_ms, Some(1000));
    // ...and with no declaration, the observed value is used.
    let sum = disc::summarise(&chans, &[], None, Some(4000), 12);
    assert_eq!(sum.measurement_resolution_ms, Some(4000));
}

// ── §7: confidence values serialize correctly ────────────────────────

#[test]
fn a_run_round_trips_through_json_with_its_confidence_intact() {
    let chans = channels(&[("pump", true), ("fan2", false)]);
    let cycles = vec![disc::DiscoveryCycle {
        cycle: 1,
        baseline_pct: 40,
        perturbed_pct: 65,
        direction: "up".into(),
        observations: vec![disc::TachObservation {
            tach_id: "pump".into(),
            baseline_rpm: Some(800),
            perturbed_rpm: Some(1300),
            delta_rpm: Some(500),
            noise_floor_rpm: 50,
            responded: true,
        }],
    }];
    let summary = disc::summarise(&chans, &cycles, Some(1000), None, 8);
    let run = disc::ControlPathRun {
        run_id: "path-1".into(),
        header_id: "hwmon:nct6798:isa:pwm2:AIO_PUMP".into(),
        state: disc::STATE_COMPLETE.into(),
        delta_pct: 25,
        requested_cycles: 2,
        window_seconds: 6,
        baseline_pct: 40,
        perturbed_pct: 65,
        direction: "up".into(),
        channels: chans,
        cycles,
        summary: Some(summary.clone()),
        original_pct: Some(40),
        restore_failed: false,
        restore_outcome: "restored".into(),
        detail: None,
        completed_unix_ms: Some(1_700_000_000_000),
    };
    let json = serde_json::to_string(&run).expect("serialise");
    let back: disc::ControlPathRun = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(back, run);
    assert_eq!(back.summary.unwrap().confidence, summary.confidence);
    // The tokens themselves must survive verbatim — the client keys off them.
    assert!(json.contains("\"confidence\""));
    assert!(json.contains("\"measurement_resolution_ms\":1000"));
    assert!(json.contains("\"sample_interval_ms\""));
}

// ── §1/§6.1: preflight ───────────────────────────────────────────────

fn ok_inputs(diagnostic: pf::Diagnostic) -> pf::PreflightInputs {
    pf::PreflightInputs {
        header_id: "hwmon:nct6798:isa:pwm2:AIO_PUMP".into(),
        diagnostic,
        header_known: true,
        is_writable: true,
        readback_pct: Some(45),
        pwm_enable: Some(1),
        role: "pump".into(),
        pump_protected: true,
        effective_floor_pct: pump_floor(),
        slot_busy: false,
        enable_revert_count: 0,
        temperature: pf::TemperatureFreshness {
            total: 2,
            fresh: 2,
            newest_age_ms: Some(300),
            newest_id: Some("cpu".into()),
        },
        thermal_forcing: None,
        too_hot: None,
        supporting: pf::SupportingCooling {
            applicable: true,
            device_id: Some("aio0".into()),
            siblings: 2,
            siblings_running: 2,
            siblings_unknown: 0,
        },
    }
}

fn check<'a>(report: &'a pf::PreflightReport, id: &str) -> &'a pf::PreflightCheck {
    report
        .checks
        .iter()
        .find(|c| c.check_id == id)
        .unwrap_or_else(|| panic!("no check {id} in {:?}", report.checks))
}

#[test]
fn a_healthy_header_is_ready() {
    let r = pf::build_report(&ok_inputs(pf::Diagnostic::ControlPathDiscovery));
    assert_eq!(r.verdict, pf::VERDICT_READY, "{:?}", r.checks);
    assert!(r.blocking.is_empty());
    // Every check §1 lists is present — a report that silently omitted one would
    // read as "ready" for a rule nobody evaluated.
    for id in [
        pf::CHECK_TARGET,
        pf::CHECK_ROLE,
        pf::CHECK_WRITABLE,
        pf::CHECK_READBACK,
        pf::CHECK_OWNERSHIP,
        pf::CHECK_SAFE_MINIMUM,
        pf::CHECK_TEMPERATURE,
        pf::CHECK_THERMAL,
        pf::CHECK_RECLAIM,
        pf::CHECK_ORIGINAL_STATE,
        pf::CHECK_SUPPORTING,
    ] {
        assert_eq!(check(&r, id).state, pf::CHECK_PASS, "check {id}");
    }
}

/// §7: "preflight blocks unsafe active diagnostics."
#[test]
fn preflight_blocks_every_unsafe_condition() {
    for (mutate, expect_blocked_id) in [
        (
            Box::new(|i: &mut pf::PreflightInputs| i.header_known = false) as Box<dyn Fn(&mut _)>,
            pf::CHECK_TARGET,
        ),
        (
            Box::new(|i: &mut pf::PreflightInputs| i.is_writable = false),
            pf::CHECK_WRITABLE,
        ),
        (
            Box::new(|i: &mut pf::PreflightInputs| i.slot_busy = true),
            pf::CHECK_OWNERSHIP,
        ),
        (
            Box::new(|i: &mut pf::PreflightInputs| i.thermal_forcing = Some("emergency".into())),
            pf::CHECK_THERMAL,
        ),
        (
            Box::new(|i: &mut pf::PreflightInputs| i.too_hot = Some(("cpu".into(), 92.0, 85.0))),
            pf::CHECK_THERMAL,
        ),
    ] {
        let mut inputs = ok_inputs(pf::Diagnostic::ControlPathDiscovery);
        mutate(&mut inputs);
        let r = pf::build_report(&inputs);
        assert_eq!(
            r.verdict,
            pf::VERDICT_BLOCKED,
            "expected {expect_blocked_id} to block"
        );
        assert!(
            r.blocking.contains(&expect_blocked_id.to_string()),
            "blocking was {:?}",
            r.blocking
        );
        assert_eq!(check(&r, expect_blocked_id).state, pf::CHECK_FAIL);
    }
}

/// §7: "stale required temperature blocks/aborts appropriately."
///
/// **Both branches**, because the rule is per-diagnostic: it BLOCKS the new
/// diagnostic and only WARNS for the two that shipped without a staleness gate.
/// A test on one branch alone would pass with the distinction deleted.
#[test]
fn a_stale_temperature_source_blocks_discovery_and_warns_the_others() {
    let stale = pf::TemperatureFreshness {
        total: 2,
        fresh: 0,
        newest_age_ms: Some(45_000),
        newest_id: Some("cpu".into()),
    };

    let mut inputs = ok_inputs(pf::Diagnostic::ControlPathDiscovery);
    inputs.temperature = stale.clone();
    let r = pf::build_report(&inputs);
    assert_eq!(r.verdict, pf::VERDICT_BLOCKED);
    assert_eq!(check(&r, pf::CHECK_TEMPERATURE).state, pf::CHECK_FAIL);

    for diag in [pf::Diagnostic::Verify, pf::Diagnostic::Characterization] {
        let mut inputs = ok_inputs(diag);
        inputs.temperature = stale.clone();
        let r = pf::build_report(&inputs);
        assert_eq!(
            check(&r, pf::CHECK_TEMPERATURE).state,
            pf::CHECK_WARN,
            "{} must not gain a refusal it does not perform",
            diag.token()
        );
        assert_eq!(r.verdict, pf::VERDICT_WARN);
        assert!(r.blocking.is_empty());
    }
}

/// The freshness predicate itself. Ages are stamped BY CONSTRUCTION — a test
/// that waited would age by ~0 ms under paused time and pass vacuously.
#[test]
fn temperature_freshness_counts_only_readings_inside_the_age_bound() {
    let now = Instant::now();
    let max = constants::DIAGNOSTIC_TEMP_MAX_AGE;
    let readings = vec![
        ("fresh".to_string(), now - Duration::from_millis(500)),
        ("stale".to_string(), now - (max + Duration::from_secs(5))),
    ];
    let f = pf::temperature_freshness(&readings, max, now);
    assert_eq!(f.total, 2);
    assert_eq!(f.fresh, 1);
    assert!(f.is_usable());
    assert_eq!(f.newest_id.as_deref(), Some("fresh"));

    // Every reading stale → not usable. The opposite branch, or a predicate
    // stuck on `true` passes the assertion above.
    let all_stale = vec![("a".to_string(), now - (max + Duration::from_secs(1)))];
    let f = pf::temperature_freshness(&all_stale, max, now);
    assert_eq!(f.fresh, 0);
    assert!(!f.is_usable());

    // No sensors at all is distinguishable from stale sensors.
    let f = pf::temperature_freshness(&[], max, now);
    assert_eq!(f.total, 0);
    assert!(!f.is_usable());
    assert_eq!(f.newest_age_ms, None);
}

/// §5's mirror rule: lack of evidence must not become a FAIL either.
#[test]
fn unknown_and_not_applicable_do_not_block() {
    let mut inputs = ok_inputs(pf::Diagnostic::ControlPathDiscovery);
    inputs.supporting = pf::SupportingCooling::default(); // not part of a device
    inputs.pwm_enable = None; // driver exposes no pwm_enable
    let r = pf::build_report(&inputs);
    assert_eq!(
        check(&r, pf::CHECK_SUPPORTING).state,
        pf::CHECK_NOT_APPLICABLE
    );
    assert_eq!(r.verdict, pf::VERDICT_READY);
    assert!(r.blocking.is_empty());
}

/// Q13: supporting cooling is REPORTED. A device whose siblings are all stopped
/// warns; it never blocks, and nothing drives them.
#[test]
fn idle_supporting_cooling_warns_rather_than_blocking() {
    let mut inputs = ok_inputs(pf::Diagnostic::ControlPathDiscovery);
    inputs.supporting = pf::SupportingCooling {
        applicable: true,
        device_id: Some("aio0".into()),
        siblings: 2,
        siblings_running: 0,
        siblings_unknown: 0,
    };
    let r = pf::build_report(&inputs);
    assert_eq!(check(&r, pf::CHECK_SUPPORTING).state, pf::CHECK_WARN);
    assert_eq!(r.verdict, pf::VERDICT_WARN);
    assert!(r.blocking.is_empty());
}

#[test]
fn the_report_round_trips_and_names_its_diagnostic() {
    let r = pf::build_report(&ok_inputs(pf::Diagnostic::Verify));
    assert_eq!(r.diagnostic, "pwm_verify");
    let json = serde_json::to_string(&r).unwrap();
    let back: pf::PreflightReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back, r);
}

#[test]
fn diagnostic_tokens_round_trip() {
    for d in [
        pf::Diagnostic::Verify,
        pf::Diagnostic::Characterization,
        pf::Diagnostic::ControlPathDiscovery,
    ] {
        assert_eq!(pf::Diagnostic::from_token(d.token()), Some(d));
    }
    assert_eq!(pf::Diagnostic::from_token("nonsense"), None);
}

// ── §6.3: the persisted store ────────────────────────────────────────

use control_ofc_daemon::control_paths::{self, ControlPathRecord, ControlPathStore};

fn record(header: &str, when: u64) -> ControlPathRecord {
    ControlPathRecord {
        header_id: header.into(),
        relationship: disc::REL_CONFIRMED.into(),
        confidence: disc::CONF_HIGH.into(),
        tach_ids: vec!["fan2".into()],
        tach_labels: vec!["fan2".into()],
        direction: "positive".into(),
        baseline_rpm: Some(800),
        perturbed_rpm: Some(1300),
        change_pct: Some(62.5),
        run_id: "path-1".into(),
        validated_unix_ms: when,
    }
}

#[test]
fn the_store_round_trips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ControlPathStore::default();
    store.upsert(record("hwmon:a:b:pwm1:PUMP", 1000));
    control_paths::save_to(dir.path(), &store).unwrap();

    let back = control_paths::load_from(dir.path());
    assert_eq!(back, store);
    assert_eq!(
        back.get("hwmon:a:b:pwm1:PUMP").unwrap().relationship,
        disc::REL_CONFIRMED
    );
}

/// §6.3: "Do not persist indefinitely as unquestioned truth if the underlying
/// hardware identity changes." The header id embeds chip/device/pwmN/label, so a
/// changed board yields a changed id and the record is dropped.
#[test]
fn records_for_headers_that_no_longer_exist_are_pruned() {
    let mut store = ControlPathStore::default();
    store.upsert(record("hwmon:nct6798:isa:pwm2:AIO_PUMP", 1000));
    store.upsert(record("hwmon:it8688:isa:pwm3:CPU_FAN", 2000));

    // The board was swapped: only one id still exists.
    let dropped = store.prune_to_live(&["hwmon:it8688:isa:pwm3:CPU_FAN".to_string()]);
    assert_eq!(dropped, 1);
    assert!(store.get("hwmon:nct6798:isa:pwm2:AIO_PUMP").is_none());
    assert!(store.get("hwmon:it8688:isa:pwm3:CPU_FAN").is_some());

    // A prune that changes nothing reports zero, so a boot with unchanged
    // hardware costs no disk write.
    assert_eq!(
        store.prune_to_live(&["hwmon:it8688:isa:pwm3:CPU_FAN".to_string()]),
        0
    );
}

/// [SAFETY-adjacent] DEC-320: bound the input at ingest, and prove the bound by
/// the REALISED artefact — the file's length on disk — not by re-deriving the
/// arithmetic production used.
#[test]
fn the_store_stays_inside_its_byte_cap_when_completely_full() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ControlPathStore::default();
    let long = "x".repeat(constants::CONTROL_PATH_MAX_TEXT_BYTES * 4);
    for i in 0..constants::CONTROL_PATHS_MAX_ENTRIES * 2 {
        // The index goes FIRST: ingest truncates to the text bound, and a
        // trailing index would be cut off, collapsing every record onto one key
        // and quietly making this test assert nothing.
        let mut r = record(&format!("hwmon:{i:04}:{long}"), i as u64);
        r.relationship = long.clone();
        r.confidence = long.clone();
        r.direction = long.clone();
        r.run_id = long.clone();
        r.tach_ids = vec![long.clone(); constants::CONTROL_PATH_MAX_TACH_REFS * 4];
        r.tach_labels = vec![long.clone(); constants::CONTROL_PATH_MAX_TACH_REFS * 4];
        store.upsert(r);
    }
    // Entry count is bounded even though twice as many were offered...
    assert_eq!(store.records.len(), constants::CONTROL_PATHS_MAX_ENTRIES);
    // ...and so is each record's tach list, which is the field that made the
    // first version of the byte cap wrong by an order of magnitude.
    for r in store.records.values() {
        assert!(r.tach_ids.len() <= constants::CONTROL_PATH_MAX_TACH_REFS);
        assert!(r.tach_labels.len() <= constants::CONTROL_PATH_MAX_TACH_REFS);
    }

    control_paths::save_to(dir.path(), &store).expect("a bounded store must be writable");
    let len = std::fs::metadata(control_paths::store_path_in(dir.path()))
        .unwrap()
        .len();
    assert!(
        len <= constants::CONTROL_PATHS_MAX_BYTES,
        "realised file is {len} bytes, over the {} byte cap",
        constants::CONTROL_PATHS_MAX_BYTES
    );
    // ...and it reads back, which is the half DEC-320 recorded as missing: a
    // document the daemon can write but not read is the actual defect.
    let back = control_paths::load_from(dir.path());
    assert_eq!(back.records.len(), constants::CONTROL_PATHS_MAX_ENTRIES);
}

/// An over-size document is discarded rather than parsed — and deleting it is
/// only safe BECAUSE of the ingest bound above.
#[test]
fn an_over_size_store_is_discarded_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = control_paths::store_path_in(dir.path());
    std::fs::write(
        &path,
        "x".repeat(constants::CONTROL_PATHS_MAX_BYTES as usize + 1),
    )
    .unwrap();
    let store = control_paths::load_from(dir.path());
    assert!(store.records.is_empty());
    assert!(!path.exists(), "the unreadable store must be reclaimable");
}

#[test]
fn a_corrupt_store_starts_empty_rather_than_failing_boot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(control_paths::store_path_in(dir.path()), "{not json").unwrap();
    assert!(control_paths::load_from(dir.path()).records.is_empty());
    // A missing file is the normal first boot.
    let empty = tempfile::tempdir().unwrap();
    assert!(control_paths::load_from(empty.path()).records.is_empty());
}

/// Text is truncated on a CHARACTER boundary — `String::truncate` panics
/// mid-codepoint, and a fan label can legitimately be non-ASCII.
#[test]
fn ingest_truncation_does_not_split_a_codepoint() {
    let mut store = ControlPathStore::default();
    let mut r = record("h", 1);
    r.relationship = "é".repeat(constants::CONTROL_PATH_MAX_TEXT_BYTES);
    store.upsert(r);
    let kept = &store.get("h").unwrap().relationship;
    assert!(kept.len() <= constants::CONTROL_PATH_MAX_TEXT_BYTES);
    assert!(kept.chars().all(|c| c == 'é'));
}

// ── DEC-336 / `P8-p`: the published refusal is PERFORMED ─────────────

/// [SAFETY] The refusal rule, asserted as the RELATIONSHIP it is derived from.
///
/// `blocks_on_stale_temperature()` is what `build_report` keys the `blocked`
/// verdict on, so the enforcement must key on the same thing. Asserting
/// `is_some()` for discovery and `is_none()` for the other two against literals
/// would pass for an implementation that hardcoded `matches!(d,
/// ControlPathDiscovery)` — the exact second copy that could later disagree with
/// the published verdict. The right-hand side is therefore the predicate itself,
/// evaluated per diagnostic, and BOTH branches are exercised (a rule stuck at
/// `true` fails the verify/characterisation arms).
#[test]
fn stale_temperature_refusal_tracks_the_predicate_the_verdict_is_published_from() {
    let stale = cache_aged(
        40.0,
        constants::DIAGNOSTIC_TEMP_MAX_AGE + Duration::from_secs(5),
    );
    let fresh = cache_aged(40.0, Duration::from_millis(0));

    let mut blocked_any = false;
    let mut warned_any = false;
    for d in [
        pf::Diagnostic::Verify,
        pf::Diagnostic::Characterization,
        pf::Diagnostic::ControlPathDiscovery,
    ] {
        let refused = cal::stale_temperature_refusal(&stale, d).is_some();
        assert_eq!(
            refused,
            d.blocks_on_stale_temperature(),
            "{} refuses on stale = {refused}, but blocks_on_stale_temperature() = {}",
            d.token(),
            d.blocks_on_stale_temperature()
        );
        blocked_any |= d.blocks_on_stale_temperature();
        warned_any |= !d.blocks_on_stale_temperature();

        // A fresh cache never refuses, for any diagnostic. Without this the
        // predicate could be stuck at "always refuse" and still pass above.
        assert!(
            cal::stale_temperature_refusal(&fresh, d).is_none(),
            "{} refused on a FRESH cache",
            d.token()
        );
    }
    // Precondition: both branches were actually observed, or the loop asserted
    // nothing about the distinction it exists to prove.
    assert!(blocked_any && warned_any);

    // An empty cache is distinguishable from a stale one, and both refuse.
    let empty = StateCache::new();
    let msg = cal::stale_temperature_refusal(&empty, pf::Diagnostic::ControlPathDiscovery)
        .expect("no readings at all must refuse");
    assert!(
        msg.contains("no temperature readings"),
        "message was {msg:?}"
    );
}

/// [SAFETY] The staleness the preflight publishes aborts the SWEEP, and the
/// abort is asserted on the writes that were actually issued.
///
/// The realised artefact, not a re-derivation: `rig.written` is what the sweep
/// really commanded. A test that only read `outcome.state` would pass for a run
/// that perturbed the header and *then* reported `aborted`.
#[tokio::test]
async fn a_stale_temperature_source_aborts_the_sweep_before_it_writes() {
    let cache = Arc::new(cache_aged(
        40.0,
        constants::DIAGNOSTIC_TEMP_MAX_AGE + Duration::from_secs(5),
    ));
    let rig = Rig::new(30);
    let cancel = AtomicBool::new(false);
    let report = RestoreReport::default();
    let chans = channels(&[("pump", true)]);

    let outcome = sweep(
        &rig,
        &cache,
        &chans,
        30,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel,
        &report,
    )
    .await;

    assert_eq!(outcome.state, disc::STATE_ABORTED);
    assert!(
        outcome.detail.as_deref().unwrap_or("").contains("stale"),
        "detail was {:?}",
        outcome.detail
    );
    // Not "wrote nothing": `RestoreOnDrop` puts the header back on EVERY exit
    // path, including this one, so the pre-run duty is legitimately re-commanded
    // and asserting an empty list would fail for the right behaviour. The safety
    // property is that the header was never moved OFF its pre-run duty.
    let wrote = rig.written.lock().unwrap().clone();
    assert!(
        wrote.iter().all(|&d| d == 30),
        "the sweep perturbed the header despite refusing: {wrote:?}"
    );

    // The discriminating control: the SAME sweep on a fresh cache does perturb.
    // Without it, this test would pass against a sweep that never writes at all.
    let fresh = Arc::new(cache_aged(40.0, Duration::from_millis(0)));
    let rig2 = Rig::new(30);
    let cancel2 = AtomicBool::new(false);
    let report2 = RestoreReport::default();
    let outcome2 = sweep(
        &rig2,
        &fresh,
        &chans,
        30,
        70,
        2,
        0,
        false,
        |_, duty| Some(u16::from(duty) * 20),
        None,
        None,
        &cancel2,
        &report2,
    )
    .await;
    assert_ne!(outcome2.state, disc::STATE_ABORTED);
    assert!(
        rig2.written.lock().unwrap().contains(&70),
        "the fresh-cache control never reached the perturbed duty, so the assertion \
         above proves nothing: {:?}",
        rig2.written.lock().unwrap()
    );
}

/// [SAFETY] **A diagnostic never refuses on a reading the thermal ladder is
/// still acting on** (DEC-336, the concurrency review's finding 2).
///
/// Asserted as that RELATIONSHIP across the supported cadence range, not as a
/// number: `diagnostic_temp_max_age` must be `>= cache.cpu_temp_stale_after()`
/// at every poll interval the daemon accepts, and never below the flat constant
/// at the default. A literal `10_000` would be satisfied by the fixed budget
/// this test exists to reject.
///
/// The interesting cadence is the slow end. `DaemonConfig::validate` bounds
/// `polling.poll_interval_ms` only from BELOW (>= 100 ms), and the runtime
/// overlay clamps it down to `MAX_SUPERVISABLE_POLL_INTERVAL_MS` — so 6 s is a
/// supported configuration, not a typo, and at 6 s the ladder trusts a reading
/// for 30 s while a flat 10 s budget would have aborted a healthy run.
#[test]
fn the_diagnostic_budget_never_undercuts_the_ladders_own_trust_window() {
    let mut saw_widened = false;
    let mut saw_floor = false;
    for interval_ms in [1000u64, 2000, 4000, 6000] {
        let cache = cache_aged(40.0, Duration::from_millis(0));
        cache.set_hwmon_poll_interval_ms(interval_ms);
        let budget = cal::diagnostic_temp_max_age(&cache);
        let ladder = cache.cpu_temp_stale_after();

        assert!(
            budget >= ladder,
            "at {interval_ms} ms the diagnostic would refuse at {budget:?} while the \
             ladder still acts on a reading up to {ladder:?} old"
        );
        assert!(
            budget >= constants::DIAGNOSTIC_TEMP_MAX_AGE,
            "at {interval_ms} ms the budget fell below the flat floor"
        );
        saw_widened |= budget > constants::DIAGNOSTIC_TEMP_MAX_AGE;
        saw_floor |= budget == constants::DIAGNOSTIC_TEMP_MAX_AGE;
    }
    // Preconditions: both regimes were actually observed. Without these the loop
    // passes for a budget that is always the constant (the defect) or always the
    // ladder window (which would silently move the default cadence's behaviour).
    assert!(saw_floor, "no cadence exercised the flat-floor regime");
    assert!(saw_widened, "no cadence exercised the widened regime");

    // And the default cadence is unchanged, which is what lets every other test
    // in this file keep asserting against the flat constant.
    let cache = cache_aged(40.0, Duration::from_millis(0));
    assert_eq!(
        cal::diagnostic_temp_max_age(&cache),
        constants::DIAGNOSTIC_TEMP_MAX_AGE
    );
}
