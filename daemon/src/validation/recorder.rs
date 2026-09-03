//! The validation-session engine (§2, §4, §5).
//!
//! # A pure observer of state that already exists
//!
//! The recorder plants **no hooks** in the profile engine, the PWM write path, or
//! any handler. It performs **zero sysfs I/O**. Once per second it reads the
//! `StateCache` snapshot the poll loop already fills, plus four cheap handles
//! ([`RecorderContext`]), and derives every §5 event marker by **diffing
//! consecutive observations**.
//!
//! That shape is what satisfies §15's "do not destabilise existing control
//! behaviour merely to integrate validation logging" *structurally* rather than
//! by care: there is no code path by which a fault in this module can perturb a
//! control decision, and a panic here cannot take down the sensor feed.
//!
//! # What it is NOT
//!
//! It is not a second PWM owner (§2). Where a session orchestrates a diagnostic,
//! it invokes the **existing** Phase 3 characterisation or PWM verify, which
//! already own the hwmon lease, the pump floor clamp, the thermal guard and
//! restore-on-drop. The engine attaches the result as evidence (§6). It never
//! writes a duty itself, and there is deliberately no code here that could.

use super::session::*;
use super::{store, summary};
use crate::constants;
use crate::control_override::OverrideTable;
use crate::health::cache::StateCache;
use crate::hwmon::pwm_control::HwmonPwmController;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The handles the recorder reads. Deliberately not `AppState`: the engine needs
/// four things, and taking only those keeps it constructible in a unit test.
#[derive(Clone)]
pub struct RecorderContext {
    pub cache: Arc<StateCache>,
    pub hwmon_controller: Option<Arc<Mutex<HwmonPwmController>>>,
    pub override_table: Arc<Mutex<OverrideTable>>,
    pub characterization: crate::api::characterization::RunSlot,
}

/// Baselines the event derivation diffs against.
///
/// Seeded at session **start**, not at engine construction — otherwise the first
/// tick of every session would emit a spurious event for each signal whose value
/// happened to differ from the process-start default.
#[derive(Default)]
struct Watch {
    profile_epoch: u64,
    resume_generation: u64,
    thermal_state: Option<String>,
    overrides: HashSet<String>,
    enable_reverts: HashMap<String, u64>,
    enable_modes: HashMap<String, u8>,
    char_run: Option<(String, String)>,
    ticks_since_flush: u32,
}

/// Why a session could not be started.
#[derive(Debug, PartialEq)]
pub enum StartError {
    /// One session at a time (§2).
    AlreadyRecording,
    /// No such cooling device.
    UnknownDevice(String),
    /// A `sweep_members` entry is not a member of the named device.
    NotAMember(String),
    /// An unrecognised diagnostic token.
    UnknownDiagnostic(String),
    /// A bound was exceeded.
    TooMany(String),
    /// The session could not be persisted.
    Persistence(String),
}

/// Flush to disk every this many ticks. Flushing every tick would rewrite the
/// whole document once a second; flushing only at the end would lose the whole
/// recording to a crash. On an interruption the file simply ends at the last
/// flush — samples are lost, never invented (§15).
///
/// **Volume, corrected 2026-09-04 (`AUD3-i`).** This said a capped session is
/// "~1 MB" and derived "~240 writes averaging ~0.5 MB, or ~120 MB spread across
/// two hours (~17 KB/s)". The input was wrong by up to an order of magnitude, so
/// the conclusion was too: a capped session is 3.6 MiB at one member and 7.8 MiB
/// at three, which makes the real figure **~240 writes averaging ~3.9 MiB, or
/// ~940 MiB across two hours (~133 KB/s)** for a three-member cooler, and ~1.4 GB
/// at the `VALIDATION_MAX_SAMPLE_BYTES` ceiling. Each flush rewrites the whole
/// document, so the total is quadratic in session length.
///
/// That is still bounded and it is still why the interval is 30 s and not 1 s —
/// but "bounded and unremarkable" was a judgement made against 120 MB, and it has
/// not been re-made against 1 GB. Recorded as `AUD3-x` rather than changed here:
/// altering the cadence trades crash-loss against write volume and is a design
/// decision, not a correction.
const FLUSH_EVERY_TICKS: u32 = 30;

/// How long a tick will wait for the hwmon controller before giving up on the
/// commanded duties for that sample. Short by design — see the lock-order note
/// in `tick`; the recorder must never queue behind a wedged sysfs write.
const CONTROLLER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

/// The eight scalars `/status` and `/poll` need, kept separately from the session.
///
/// The poll is the GUI's hottest path and runs once a second. Reading these from
/// the session itself meant cloning every sample and event to produce them —
/// order 10^5 allocations for a full two-hour session, growing linearly as it
/// ran, **while holding the lock the recorder tick needs**. This struct is small,
/// fixed-size, and lives behind its own mutex that is never held across anything.
#[derive(Debug, Clone, Default)]
pub struct LiveSummary {
    pub session_id: String,
    pub kind: String,
    pub state: String,
    pub elapsed_ms: u64,
    pub sample_count: usize,
    pub event_count: usize,
    pub sample_limit_reached: bool,
    pub cooling_device_id: String,
}

/// Holds at most one session and derives its timeline.
pub struct ValidationEngine {
    slot: Mutex<Option<ValidationSession>>,
    watch: Mutex<Watch>,
    /// The poll surface. Separate lock, tiny payload — see [`LiveSummary`].
    live: Mutex<Option<LiveSummary>>,
    /// The session's member ids, cached at start.
    ///
    /// They are fixed for a session's life, and caching them is what lets `tick`
    /// acquire the hwmon controller lock **before** the session lock rather than
    /// while holding it. See the lock-order note on `tick`.
    member_ids: Mutex<Vec<String>>,
    /// Serialises persistence.
    ///
    /// The periodic flush and the finaliser both clone under the slot lock,
    /// release it, and then write — so without this they can reach
    /// `atomic_io::write_atomic` concurrently. That helper uses a **fixed**
    /// `{path}.tmp` name opened with `File::create`, so two writers truncate each
    /// other and can publish a hybrid document; and a flush that started first
    /// could rename its stale `recording` copy over a `completed` one, which the
    /// next boot sweep would then "repair" to `interrupted` despite a clean stop.
    save_lock: Mutex<()>,
    /// The session's effective sample cap, derived at start from its topology.
    ///
    /// `VALIDATION_MAX_SAMPLES` bounds the sample COUNT; this bounds the
    /// persisted document's SIZE, which is not the same thing once a sample
    /// carries one entry per cooling-device member (`AUD3-i`). Cached for the
    /// same reason `member_ids` is: it is fixed for the session's life and
    /// deriving it costs a serialisation, which has no business running per tick.
    max_samples: Mutex<usize>,
}

impl Default for ValidationEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationEngine {
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            watch: Mutex::new(Watch::default()),
            live: Mutex::new(None),
            member_ids: Mutex::new(Vec::new()),
            save_lock: Mutex::new(()),
            max_samples: Mutex::new(constants::VALIDATION_MAX_SAMPLES),
        }
    }

    /// The current or most recent session, in full. Clones every sample — use
    /// [`live_summary`](Self::live_summary) for anything on the poll path.
    pub fn snapshot(&self) -> Option<ValidationSession> {
        self.slot.lock().clone()
    }

    /// The eight scalars for `/status` + `/poll`. Cheap, fixed-size, and behind
    /// its own lock, so a poll never contends with a recorder tick.
    pub fn live_summary(&self) -> Option<LiveSummary> {
        self.live.lock().clone()
    }

    fn refresh_live(&self, session: &ValidationSession) {
        *self.live.lock() = Some(LiveSummary {
            session_id: session.session_id.clone(),
            kind: session.kind.clone(),
            state: session.state.clone(),
            elapsed_ms: session.elapsed_ms(),
            sample_count: session.samples.len(),
            event_count: session.events.len(),
            sample_limit_reached: session.sample_limit_reached,
            cooling_device_id: session.metadata.cooling_device_id.clone(),
        });
    }

    /// Write a session to disk, serialised against every other writer.
    ///
    /// The re-read inside the lock is the load-bearing part: a periodic flush
    /// clones a `recording` copy, releases the slot, and may only reach this
    /// point after a concurrent `stop()` has already written `completed`.
    /// Publishing the stale copy would resurrect a finished session as
    /// `recording`, and the next boot sweep would then mark a cleanly-stopped
    /// session `interrupted` and discard its findings.
    fn persist(&self, session: &ValidationSession) {
        let _guard = self.save_lock.lock();
        if !session.is_terminal() {
            let superseded = self
                .slot
                .lock()
                .as_ref()
                .is_some_and(|cur| cur.session_id == session.session_id && cur.is_terminal());
            if superseded {
                return;
            }
        }
        if let Err(e) = store::save(session) {
            log::warn!(
                "Could not persist validation session {}: {e}",
                session.session_id
            );
        }
    }

    /// Is a session sampling right now?
    pub fn is_recording(&self) -> bool {
        self.slot.lock().as_ref().is_some_and(|s| s.is_recording())
    }

    /// Begin a session. The caller has already resolved and validated the
    /// device, members and diagnostics — this installs the built session and
    /// seeds the event baselines under one lock.
    pub fn start(
        &self,
        session: ValidationSession,
        ctx: &RecorderContext,
    ) -> Result<ValidationSession, StartError> {
        let mut slot = self.slot.lock();
        if slot.as_ref().is_some_and(|s| s.is_recording()) {
            return Err(StartError::AlreadyRecording);
        }
        let mut session = session;
        session.events.push(ValidationEvent {
            elapsed_ms: 0,
            unix_ms: session.started_unix_ms,
            kind: EV_SESSION_STARTED.to_string(),
            detail: None,
            member_id: None,
        });
        // Seed baselines from the live values so the first tick reports change,
        // not merely difference-from-default.
        *self.watch.lock() = seed_watch(ctx);
        // Fixed for this session's life, and cached so `tick` can take the hwmon
        // controller lock BEFORE the session lock instead of underneath it.
        let ids: Vec<String> = session
            .metadata
            .members
            .iter()
            .map(|m| m.member_id.clone())
            .collect();
        // Derived before the first tick from the session ITSELF, not from a
        // summary of it: the byte cost scales with the member ids AND with the
        // sensor id every sample carries, and the latter is client-supplied.
        // See `session::max_samples_for`.
        *self.max_samples.lock() =
            super::session::max_samples_for(&session, constants::VALIDATION_MAX_SAMPLE_BYTES);
        *self.member_ids.lock() = ids;
        let started = session.clone();
        self.refresh_live(&session);
        *slot = Some(session);
        drop(slot);

        if let Err(e) = store::save(&started) {
            // Roll the slot back: a session that cannot be persisted cannot be
            // marked `interrupted` after a restart either, so admitting it would
            // create exactly the silent gap §15 forbids.
            //
            // **Fenced on the id.** A slow failing save (a full or read-only
            // state dir — the very condition that makes it slow) can outlive its
            // own session: `stop()` finalises it, a second `POST` legitimately
            // admits a new one, and an unconditional `= None` here would then
            // silently wipe a session this call never installed.
            self.rollback(&started.session_id);
            return Err(StartError::Persistence(e));
        }
        Ok(started)
    }

    /// Finalise normally: compute the summary and persist (§8).
    pub fn stop(&self) -> Option<ValidationSession> {
        self.finish(STATE_COMPLETED, None)
    }

    /// End without finalising.
    pub fn cancel(&self) -> Option<ValidationSession> {
        self.finish(STATE_CANCELLED, None)
    }

    fn finish(&self, state: &str, reason: Option<String>) -> Option<ValidationSession> {
        let mut slot = self.slot.lock();
        let session = slot.as_mut()?;
        if !session.is_recording() {
            return Some(session.clone());
        }
        let now = unix_ms();
        session.events.push(ValidationEvent {
            elapsed_ms: now.saturating_sub(session.started_unix_ms),
            unix_ms: now,
            kind: EV_SESSION_STOPPED.to_string(),
            detail: reason.clone(),
            member_id: None,
        });
        session.state = state.to_string();
        session.completed_unix_ms = Some(now);
        session.interrupted_reason = reason;
        // The summary is derived once, here, by the pure summariser. Nothing
        // downstream recalculates backend meaning (§16).
        session.findings = summary::summarise(session);
        let done = session.clone();
        self.refresh_live(session);
        drop(slot);
        self.persist(&done);
        Some(done)
    }

    /// Append a user marker or an engine-generated event (§5).
    pub fn push_event(
        &self,
        kind: &str,
        detail: Option<String>,
        member_id: Option<String>,
    ) -> bool {
        let mut slot = self.slot.lock();
        let Some(session) = slot.as_mut() else {
            return false;
        };
        if !session.is_recording() {
            return false;
        }
        push_event_locked(session, kind, detail, member_id);
        self.refresh_live(session);
        true
    }

    /// Attach an externally measured observation (§14). Untrusted; read by
    /// nothing, and no control path may ever consult one.
    pub fn add_measurement(&self, m: ExternalMeasurement) -> bool {
        let mut slot = self.slot.lock();
        let Some(session) = slot.as_mut() else {
            return false;
        };
        if !session.is_recording() {
            return false;
        }
        if session.external_measurements.len() >= constants::VALIDATION_MAX_EXTERNAL_MEASUREMENTS {
            return false;
        }
        session.external_measurements.push(m);
        true
    }

    /// Record the result of an orchestrated diagnostic (§6), **fenced on the
    /// session it was started for**.
    ///
    /// The fence is not defensive tidiness. An orchestration task outlives a
    /// cancel: `cancel()` finalises immediately, a second `POST` then admits a
    /// new session, and the in-flight task would otherwise append the *previous*
    /// session's characterisation run — run id, points and all — to a session
    /// that never requested it, on a member it may not even sweep. That run then
    /// feeds `summarise` and changes the new session's findings. Fabricated
    /// evidence, in the one artefact whose contract is that nothing is
    /// fabricated (§15).
    pub fn attach_evidence_for(&self, session_id: &str, ev: EvidenceRef) -> bool {
        let mut slot = self.slot.lock();
        let Some(session) = slot.as_mut() else {
            return false;
        };
        if session.session_id != session_id || !session.is_recording() {
            return false;
        }
        session.evidence.push(ev);
        self.refresh_live(session);
        true
    }

    /// Append an event to a **named** session. See [`attach_evidence_for`] for
    /// why the orchestrator must not use the unfenced variant.
    ///
    /// [`attach_evidence_for`]: Self::attach_evidence_for
    pub fn push_event_for(
        &self,
        session_id: &str,
        kind: &str,
        detail: Option<String>,
        member_id: Option<String>,
    ) -> bool {
        let mut slot = self.slot.lock();
        let Some(session) = slot.as_mut() else {
            return false;
        };
        if session.session_id != session_id || !session.is_recording() {
            return false;
        }
        push_event_locked(session, kind, detail, member_id);
        self.refresh_live(session);
        true
    }

    /// Test-only: publish a session copy through the real persistence path,
    /// including its stale-write guard. Exercises [`persist`](Self::persist)
    /// without needing to win a race against a live recorder task.
    #[doc(hidden)]
    pub fn persist_for_test(&self, session: &ValidationSession) {
        self.persist(session);
    }

    /// Un-install a session, but **only if it is still the one installed**.
    ///
    /// `start`'s rollback path. A slow failing save can outlive its own session —
    /// `stop()` finalises it and a second `POST` legitimately admits a new one —
    /// so an unconditional clear would silently wipe a session this call never
    /// installed, leaving the new one un-recorded and its file `recording`
    /// forever.
    fn rollback(&self, session_id: &str) {
        let mut slot = self.slot.lock();
        if slot
            .as_ref()
            .is_some_and(|cur| cur.session_id == session_id)
        {
            *slot = None;
            drop(slot);
            *self.live.lock() = None;
        }
    }

    /// Test-only: exercise `start`'s rollback directly.
    ///
    /// Delegates to [`rollback`](Self::rollback) rather than restating its
    /// fence — a helper that reimplemented the rule would test the copy and
    /// leave the production path unproven, which is a failure mode this project
    /// has recorded six times.
    #[doc(hidden)]
    pub fn rollback_for_test(&self, session_id: &str) {
        self.rollback(session_id);
    }

    /// The id of the session currently recording, if any.
    pub fn recording_session_id(&self) -> Option<String> {
        self.slot
            .lock()
            .as_ref()
            .filter(|s| s.is_recording())
            .map(|s| s.session_id.clone())
    }

    /// One sampling tick. Returns `true` if a sample was recorded.
    pub fn tick(&self, ctx: &RecorderContext) -> bool {
        // Take the observations BEFORE the session lock, so a slow controller
        // lock never delays a start/stop request.
        let snap = ctx.cache.snapshot();
        let profile_epoch = ctx.cache.profile_activation_epoch();
        let resume_generation = ctx.cache.resume_generation();
        let overrides: HashSet<String> = ctx
            .override_table
            .lock()
            .snapshot()
            .controls
            .keys()
            .cloned()
            .collect();
        let char_run = ctx
            .characterization
            .lock()
            .as_ref()
            .map(|r| (r.run_id.clone(), r.state.clone()));

        // [LOCK ORDER] The controller lock is taken HERE, before the session
        // lock, and never while holding it.
        //
        // The engine holds this same mutex across `set_pwm`'s `std::fs::write`,
        // and a wedged sysfs write is a recorded failure mode (DEC-278/DEC-289).
        // Blocking on it while holding the session slot would put an unbounded
        // wait behind a lock that `/status` and `/poll` also take — so a wedged
        // header would block every 1 Hz poll task in a non-cancellable
        // `parking_lot` acquisition on a tokio worker, and once the workers were
        // exhausted the profile-engine loop could not be polled at all. A fault
        // the engine is built to survive would become an engine stall, thermal
        // ladder included. That is the recorder perturbing control by starvation
        // rather than by writing, which this module's whole design forbids.
        //
        // `try_lock_for` rather than `lock`: even in the correct order, blocking
        // a recorder tick on a wedged write buys nothing. A timeout records
        // `requested_pct: None`, which the field already defines as "not known" —
        // the honest answer, and the same one a daemon with no controller gives.
        let member_ids: Vec<String> = self.member_ids.lock().clone();
        let mut commanded: HashMap<String, u8> = HashMap::new();
        let mut reverts: HashMap<String, u64> = HashMap::new();
        if let Some(ctrl) = &ctx.hwmon_controller {
            if let Some(guard) = ctrl.try_lock_for(CONTROLLER_READ_TIMEOUT) {
                for id in &member_ids {
                    if let Some(pct) = guard.last_commanded_pct(id) {
                        commanded.insert(id.clone(), pct);
                    }
                }
                reverts = guard.enable_revert_counts().clone();
            } else {
                log::debug!(
                    "validation recorder: hwmon controller busy, recording this \
                     sample without commanded duties"
                );
            }
        }

        let mut slot = self.slot.lock();
        let Some(session) = slot.as_mut() else {
            return false;
        };
        if !session.is_recording() {
            return false;
        }

        let now = unix_ms();
        let elapsed = now.saturating_sub(session.started_unix_ms);

        // ── Sample ──────────────────────────────────────────────────────────
        let mut members = Vec::with_capacity(session.metadata.members.len());
        for m in &session.metadata.members {
            let fan = snap.hwmon_fans.get(&m.member_id);
            let enable = fan.and_then(|f| f.pwm_enable_mode);
            members.push(MemberSample {
                member_id: m.member_id.clone(),
                role: m.member_kind.clone(),
                requested_pct: commanded.get(&m.member_id).copied(),
                readback_pct: fan.and_then(|f| f.pwm_readback_pct),
                rpm: fan.and_then(|f| f.rpm),
                pwm_enable_mode: enable,
                alarm: fan.and_then(|f| f.alarm),
                enable_revert_count: reverts.get(&m.member_id).copied().unwrap_or(0),
                ownership: match enable {
                    Some(1) => OWNERSHIP_DAEMON.to_string(),
                    Some(_) => OWNERSHIP_EXTERNAL.to_string(),
                    None => OWNERSHIP_UNKNOWN.to_string(),
                },
            });
        }

        let temperature_sensor = session.metadata.temperature_sensor.clone();
        let temperature_c = temperature_sensor
            .as_ref()
            .and_then(|id| snap.sensors.get(id))
            .map(|s| s.value_c);
        let coolant_c = session
            .metadata
            .coolant_sensor
            .as_ref()
            .and_then(|id| snap.sensors.get(id))
            .map(|s| s.value_c);
        let thermal_state = snap
            .thermal_override_state
            .clone()
            .unwrap_or_else(|| "normal".to_string());

        session.samples.push(ValidationSample {
            elapsed_ms: elapsed,
            unix_ms: now,
            temperature_c,
            temperature_sensor,
            coolant_c,
            thermal_state: thermal_state.clone(),
            members,
        });

        // ── Events, by diffing against the watch baselines ──────────────────
        {
            let mut w = self.watch.lock();

            if profile_epoch != w.profile_epoch {
                w.profile_epoch = profile_epoch;
                push_event_locked(session, EV_PROFILE_ACTIVATED, None, None);
            }

            if resume_generation != w.resume_generation {
                w.resume_generation = resume_generation;
                // A resume is observed only after the fact — the daemon is not
                // running during the suspend, so `suspend` is emitted alongside
                // it as the inferred start of the gap rather than pretended to
                // have been noticed at the time.
                push_event_locked(
                    session,
                    EV_SUSPEND,
                    Some("inferred from the resume gap".to_string()),
                    None,
                );
                push_event_locked(session, EV_RESUME, None, None);
            }

            if w.thermal_state.as_deref() != Some(thermal_state.as_str()) {
                let was_emergency = matches!(
                    w.thermal_state.as_deref(),
                    Some("emergency") | Some("recovery")
                );
                let is_emergency = matches!(thermal_state.as_str(), "emergency" | "recovery");
                if is_emergency && !was_emergency {
                    push_event_locked(
                        session,
                        EV_THERMAL_ENTERED,
                        Some(thermal_state.clone()),
                        None,
                    );
                } else if was_emergency && !is_emergency {
                    push_event_locked(
                        session,
                        EV_THERMAL_CLEARED,
                        Some(thermal_state.clone()),
                        None,
                    );
                }
                w.thermal_state = Some(thermal_state);
            }

            for id in overrides.difference(&w.overrides) {
                push_event_locked(session, EV_OVERRIDE_STARTED, None, Some(id.clone()));
            }
            for id in w.overrides.difference(&overrides) {
                push_event_locked(session, EV_OVERRIDE_ENDED, None, Some(id.clone()));
            }
            w.overrides = overrides;

            // Reclaim: the daemon had it in manual mode and no longer does, or
            // the write path's own watchdog counted a revert. Two independent
            // detectors, because either can miss: the mode sample can fall
            // between two reclaims, and the counter only moves when the engine
            // writes.
            let member_ids: Vec<String> = session
                .metadata
                .members
                .iter()
                .map(|m| m.member_id.clone())
                .collect();
            for id in &member_ids {
                let now_mode = snap.hwmon_fans.get(id).and_then(|f| f.pwm_enable_mode);
                let was_mode = w.enable_modes.get(id).copied();
                if let Some(mode) = now_mode {
                    if was_mode == Some(1) && mode != 1 {
                        push_event_locked(
                            session,
                            EV_CONTROL_RECLAIMED,
                            Some(format!("pwm_enable {mode}")),
                            Some(id.clone()),
                        );
                    } else if was_mode.is_some_and(|w| w != 1) && mode == 1 {
                        push_event_locked(session, EV_CONTROL_RESTORED, None, Some(id.clone()));
                    }
                    w.enable_modes.insert(id.clone(), mode);
                }
                let now_rev = reverts.get(id).copied().unwrap_or(0);
                let was_rev = w.enable_reverts.get(id).copied().unwrap_or(0);
                if now_rev > was_rev {
                    push_event_locked(
                        session,
                        EV_CONTROL_RECLAIMED,
                        Some(format!("{} watchdog revert(s)", now_rev - was_rev)),
                        Some(id.clone()),
                    );
                }
                w.enable_reverts.insert(id.clone(), now_rev);
            }

            if char_run != w.char_run {
                match (&w.char_run, &char_run) {
                    (_, Some((run_id, state))) if state == "running" => {
                        push_event_locked(session, EV_CHAR_STARTED, Some(run_id.clone()), None);
                    }
                    (Some((_, prev)), Some((run_id, state)))
                        if prev == "running" && state != "running" =>
                    {
                        push_event_locked(
                            session,
                            EV_CHAR_COMPLETED,
                            Some(format!("{run_id}: {state}")),
                            None,
                        );
                    }
                    _ => {}
                }
                w.char_run = char_run;
            }

            w.ticks_since_flush += 1;
        }

        // ── Cap and flush ───────────────────────────────────────────────────
        // The topology-derived cap, never the raw sample count: a three-member
        // cooler at 7200 samples wrote a 7.8 MiB document the store could not
        // read back (`AUD3-i`). For a realistic AIO this is still 7200.
        let at_cap = session.samples.len() >= *self.max_samples.lock();
        let should_flush = {
            let mut w = self.watch.lock();
            if w.ticks_since_flush >= FLUSH_EVERY_TICKS {
                w.ticks_since_flush = 0;
                true
            } else {
                false
            }
        };
        let to_persist = if at_cap {
            session.sample_limit_reached = true;
            push_event_locked(session, EV_SAMPLE_LIMIT, None, None);
            None
        } else if should_flush {
            Some(session.clone())
        } else {
            None
        };
        self.refresh_live(session);
        drop(slot);

        if at_cap {
            // Cap-and-stop: finalise rather than evicting the oldest samples,
            // which are the startup evidence §9 exists to capture.
            self.finish(STATE_COMPLETED, None);
        } else if let Some(s) = to_persist {
            self.persist(&s);
        }
        true
    }
}

/// Append an event, bounded. Caller holds the slot lock.
fn push_event_locked(
    session: &mut ValidationSession,
    kind: &str,
    detail: Option<String>,
    member_id: Option<String>,
) {
    if session.events.len() >= constants::VALIDATION_MAX_EVENTS {
        return;
    }
    let now = unix_ms();
    session.events.push(ValidationEvent {
        elapsed_ms: now.saturating_sub(session.started_unix_ms),
        unix_ms: now,
        kind: kind.to_string(),
        detail,
        member_id,
    });
}

fn seed_watch(ctx: &RecorderContext) -> Watch {
    let snap = ctx.cache.snapshot();
    let mut enable_modes = HashMap::new();
    for (id, fan) in &snap.hwmon_fans {
        if let Some(mode) = fan.pwm_enable_mode {
            enable_modes.insert(id.clone(), mode);
        }
    }
    let enable_reverts = ctx
        .hwmon_controller
        .as_ref()
        .map(|c| c.lock().enable_revert_counts().clone())
        .unwrap_or_default();
    Watch {
        profile_epoch: ctx.cache.profile_activation_epoch(),
        resume_generation: ctx.cache.resume_generation(),
        thermal_state: snap.thermal_override_state.clone(),
        overrides: ctx
            .override_table
            .lock()
            .snapshot()
            .controls
            .keys()
            .cloned()
            .collect(),
        enable_reverts,
        enable_modes,
        char_run: ctx
            .characterization
            .lock()
            .as_ref()
            .map(|r| (r.run_id.clone(), r.state.clone())),
        ticks_since_flush: 0,
    }
}
