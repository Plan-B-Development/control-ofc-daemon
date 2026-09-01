//! Per-backend write paths for the profile engine (DEC-135).
//!
//! Each fan-control backend (OpenFan serial, AMD GPU PMFW, motherboard
//! hwmon) implements [`WriteBackend`]. ALL per-backend gating — write
//! coalescing/thresholds, failure caching, lease handling — lives behind
//! `apply`, so each rule exists in exactly one place per backend. The
//! engine loop is reduced to: safety tick → profile evaluation → `apply`
//! per backend. (The GUI-deferral gate was removed at 2.0.0 — DEC-165.)
//!
//! Backends that participate in forced safety writes (thermal emergency,
//! no-CPU-sensor fallback) additionally implement [`SafetyWriteBackend`].
//! [`GpuBackend`] deliberately does NOT (DEC-130): there is no GPU
//! emergency threshold. AMD PMFW firmware owns GPU thermal protection
//! (junction-temp throttling, firmware fan ramp) independently of OS fan
//! control, and forcing PMFW curve commits from a CPU emergency would add
//! SMU churn without improving GPU safety.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use super::PwmCommand;
use crate::clock::Clock;
use crate::constants;
use crate::health::cache::StateCache;
use crate::hwmon::lease::HwmonWriter;
use crate::hwmon::pwm_control::HwmonControlError;
use crate::serial::protocol::NUM_CHANNELS;

/// How long the engine will wait for one backend's blocking write before it
/// stops waiting and carries on with the tick (DEC-289).
///
/// One nominal tick. The point of the bound is **not** to decide that a write is
/// broken — `health::staleness::engine_health` already distinguishes a slow tick
/// from a stuck one, using thresholds derived in DEC-259 — it is to stop one
/// backend's slow or wedged write from holding the loop, and with it thermal
/// safety and every *other* backend. A write that has not returned within a tick
/// has already missed the tick it belonged to, so there is nothing left to wait
/// for.
const WRITE_JOIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(1);

/// A `spawn_blocking` write whose join is **bounded**, so a write wedged in the
/// kernel cannot freeze the engine loop (DEC-289).
///
/// Before this existed the engine awaited every backend join unconditionally. A
/// sysfs write blocked in a driver therefore froze the *whole* loop: the tick
/// never completed, so the thermal `force_all_with_floor` never ran again — and because the
/// task was still **alive**, DEC-266's death supervision never fired either. The
/// daemon sat there holding the fans at whatever duty they happened to have.
///
/// The critical detail is that the handle is **held and re-awaited**, never
/// re-spawned. `spawn_blocking` cannot be cancelled, so issuing a fresh write
/// each tick against a wedged device would strand one blocking thread per tick
/// and exhaust tokio's 512-thread pool in ~8.5 minutes — starving the very
/// writer this guards. That is the DEC-272 trap, and it is why `run` returns
/// `None` rather than retrying: while a write is outstanding the caller must not
/// issue another one.
///
/// This mirrors the read side, which DEC-272 bounded the same way in
/// `polling.rs`; the write path was simply never swept.
pub(crate) struct BoundedWrite<T> {
    /// A write issued on an earlier tick that has not returned yet. Held so the
    /// next tick re-awaits THIS write instead of stacking another behind it.
    pending: Option<tokio::task::JoinHandle<T>>,
    /// Did the most recent [`Self::run`] see any write COMPLETE? (DEC-298)
    ///
    /// Load-bearing, not bookkeeping. Since DEC-298 a tick re-issues immediately
    /// after harvesting, so against a device slower than the budget `pending` is
    /// never `None` at the point the loop samples it — and
    /// `record_engine_write_stall` only clears its stamp on `false`. Reporting
    /// raw outstanding-ness would therefore pin `engine_writes_stalled_since`
    /// forever and trip the 30x `crit` "writes wedged" on a device that is
    /// writing perfectly well, every 1.5 s. "Wedged" has to mean *nothing is
    /// completing*, not *something is in flight*.
    completed_last_run: bool,
}

impl<T> Default for BoundedWrite<T> {
    fn default() -> Self {
        Self {
            pending: None,
            completed_last_run: false,
        }
    }
}

impl<T: Send + 'static> BoundedWrite<T> {
    /// True while a write issued earlier has still not returned.
    ///
    /// The engine reads this to decide whether to stamp the tick as *completed*:
    /// a tick with a write still in flight has not finished its work, and saying
    /// otherwise would erase the one signal `/status` already gives an operator
    /// for this condition.
    pub(crate) fn outstanding(&self) -> bool {
        // `is_finished`, NOT `is_some`. A handle is only harvested by the next
        // `run()`, and several loop paths skip the write phase entirely — no
        // profile loaded, a verify in progress. Reporting "held" as "still
        // running" there would pin `/status` at "writes wedged" forever for a
        // daemon that is idle by design and whose write completed seconds ago,
        // clearing only on re-activation or restart.
        self.pending.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// True when writes are **not getting through**: something is outstanding and
    /// nothing completed on the last run (DEC-298).
    ///
    /// This, not [`Self::outstanding`], is what the engine reports as the write
    /// stall — see `completed_last_run`. A device merely slower than the budget
    /// completes a write every run and is therefore never stalled; a wedged one
    /// completes nothing and is stalled from the first tick.
    pub(crate) fn stalled(&self) -> bool {
        self.outstanding() && !self.completed_last_run
    }

    /// Record that a tick made no write progress (DEC-299).
    ///
    /// For a caller that returns WITHOUT reaching [`Self::run`] while a write is
    /// still outstanding — `GpuBackend` does exactly that when the write lock is
    /// unavailable, which is precisely when a wedged write is holding it.
    /// Without this, `completed_last_run` keeps whatever the last `run` left, so
    /// a permanently wedged GPU write reports `stalled() == false` forever —
    /// which *clears* `engine_writes_stalled_since` and shows a healthy engine
    /// while the fan holds its last duty. It cannot reintroduce DEC-298's false
    /// crit: the caller invokes it only when something is genuinely outstanding.
    pub(crate) fn note_no_progress(&mut self) {
        self.completed_last_run = false;
    }

    /// Await any outstanding write, bounded by `deadline`, at shutdown.
    ///
    /// Load-bearing for hardware restore, not tidiness. `main.rs`'s shutdown
    /// drains the engine's task handle and previously got the backend writes for
    /// free, because the loop could not end a tick with a write in flight. It can
    /// now — that is the whole point of the bound — so without this the drain
    /// resolves instantly while a detached write still holds the controller
    /// mutex. `restore_hwmon_to_auto` would then miss its `try_lock_for`, take
    /// the fallback path, and the still-running `set_pwm` would re-assert manual
    /// mode *after* the restore handed the fans back: fans latched in manual
    /// after exit, silently, on the `Restart=on-failure` path where
    /// `ExecStopPost` never runs.
    pub(crate) async fn drain(&mut self, deadline: std::time::Duration) {
        let Some(handle) = self.pending.take() else {
            return;
        };
        if tokio::time::timeout(deadline, handle).await.is_err() {
            log::warn!(
                "shutdown: a backend write did not return within {}s — it still \
                 holds the controller lock, so the hardware restore may fall back",
                deadline.as_secs()
            );
        }
    }

    /// Harvest an outstanding write without issuing a new one (DEC-298).
    ///
    /// For a tick that genuinely has nothing to command. `run` would otherwise
    /// spawn a closure that does setup — taking and renewing a lease — for a
    /// write that commands nothing.
    pub(crate) async fn harvest_only(&mut self, budget: std::time::Duration) -> WriteProgress<T> {
        let Some(mut outstanding) = self.pending.take() else {
            self.completed_last_run = false;
            return WriteProgress {
                harvested: None,
                issued: None,
                in_flight: false,
            };
        };
        match tokio::time::timeout(budget, &mut outstanding).await {
            Ok(joined) => {
                self.completed_last_run = true;
                WriteProgress {
                    harvested: Some(joined),
                    issued: None,
                    in_flight: false,
                }
            }
            Err(_) => {
                self.pending = Some(outstanding);
                self.completed_last_run = false;
                WriteProgress {
                    harvested: None,
                    issued: None,
                    in_flight: true,
                }
            }
        }
    }

    /// Harvest any outstanding write, then issue `f`, all bounded by `budget`.
    ///
    /// **DEC-298 changed this.** It used to drop `f` unread whenever it harvested
    /// a pending handle, even when that handle completed immediately — so a tick
    /// that cleared a slow write issued nothing of its own. Concretely: an
    /// ordinary `apply` is pending at tick N, the CPU crosses the trip point at N+1,
    /// `force_all_with_floor` harvests the apply's result and returns having written
    /// nothing, and the first forced write is issued at N+2. Against a device
    /// consistently slower than `budget`, forced writes alternated every other
    /// tick. A delay, not a loss of reach — a wedged write holds the controller
    /// mutex, so a separately spawned `force_all_with_floor` would have blocked on
    /// `ctrl.lock()` and reached no header either — but a delay on the emergency
    /// path is worth removing.
    ///
    /// **The tick cost is unchanged**: the harvest and the newly issued write
    /// share ONE budget, so `run` still returns within `budget`.
    ///
    /// **The DEC-272 invariant is unchanged and is the reason for the shape**:
    /// `spawn_blocking` cannot be cancelled, so issuing a fresh write each tick
    /// against a wedged device would strand one blocking thread per tick and
    /// exhaust tokio's 512-thread pool in ~8.5 minutes — starving the very writer
    /// this guards. `f` is therefore spawned **only when `pending` is `None`
    /// after the harvest**, so at most one write is ever outstanding. A harvest
    /// that times out still drops `f`, exactly as before.
    pub(crate) async fn run<F>(&mut self, budget: std::time::Duration, f: F) -> WriteProgress<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let started = std::time::Instant::now();
        let mut harvested = None;

        if let Some(mut outstanding) = self.pending.take() {
            // `&mut outstanding` so a timeout does not consume the handle — that
            // is what makes "hold and re-await" possible rather than "re-spawn".
            match tokio::time::timeout(budget, &mut outstanding).await {
                Ok(joined) => harvested = Some(joined),
                Err(_) => {
                    self.pending = Some(outstanding);
                    self.completed_last_run = false;
                    return WriteProgress {
                        harvested: None,
                        issued: None,
                        in_flight: true,
                    };
                }
            }
        }

        debug_assert!(
            self.pending.is_none(),
            "DEC-272: never spawn while a write is still outstanding"
        );
        // Residual, not a fresh budget: the whole call stays within `budget`. It
        // can be zero when the harvest used it all, and that is fine — the point
        // of this phase is that the write is ISSUED (`spawn_blocking` starts
        // running immediately), not that this tick waits for it.
        let residual = budget.saturating_sub(started.elapsed());
        let mut handle = tokio::task::spawn_blocking(f);
        match tokio::time::timeout(residual, &mut handle).await {
            Ok(joined) => {
                self.completed_last_run = true;
                WriteProgress {
                    harvested,
                    issued: Some(joined),
                    in_flight: false,
                }
            }
            Err(_) => {
                self.pending = Some(handle);
                // A harvest counts: a write DID complete on this run, even though
                // the one just issued has not.
                self.completed_last_run = harvested.is_some();
                WriteProgress {
                    harvested,
                    issued: None,
                    in_flight: true,
                }
            }
        }
    }
}

/// What one [`BoundedWrite::run`] call did (DEC-298).
///
/// Two results can land in a single call — a harvested write from an earlier
/// tick and the one issued now — which is why this is a struct rather than the
/// `Option<Result<..>>` it replaced. Callers must route **both**, or a failing
/// member's streak is neither advanced nor reset for that tick.
pub(crate) struct WriteProgress<T> {
    /// A write issued on an earlier tick that completed during this call.
    pub(crate) harvested: Option<Result<T, tokio::task::JoinError>>,
    /// This call's own write, if it also completed within the residual budget.
    pub(crate) issued: Option<Result<T, tokio::task::JoinError>>,
    /// A write is still outstanding after this call. The caller must not issue
    /// another; the next `run` re-awaits it.
    pub(crate) in_flight: bool,
}

impl<T> WriteProgress<T> {
    /// Every write that completed during this call, oldest first.
    ///
    /// One accessor so the two fields cannot be routed inconsistently by four
    /// call sites — the harvested result was dropped on the floor once already
    /// (DEC-289 fixed that for one arm), and this makes the same omission
    /// impossible to write.
    pub(crate) fn completed(self) -> impl Iterator<Item = Result<T, tokio::task::JoinError>> {
        [self.harvested, self.issued].into_iter().flatten()
    }
}

/// One fan-control backend the profile engine writes through.
///
/// To add a backend: implement this trait, give the implementation sole
/// ownership of its gating rules (coalescing, failure caching, lease),
/// and call it from the loop's apply sequence in `profile_engine_loop`.
pub(crate) trait WriteBackend {
    /// Apply this backend's share of the profile commands.
    ///
    /// The engine is the sole authoritative writer (DEC-165): there is no GUI
    /// deferral. Each backend still owns its coalescing, failure caching, and
    /// lease handling behind this call.
    async fn apply(&mut self, commands: &[PwmCommand]);
}

/// Backends that participate in forced safety writes (thermal emergency /
/// no-sensor fallback). No GUI deferral, no coalescing shortcuts beyond the
/// controller's own exact-match skip.
pub(crate) trait SafetyWriteBackend: WriteBackend {
    /// Drive every output to **at least** `pct`, using `commands` as the
    /// per-output baseline.
    ///
    /// [SAFETY] D1-j / DEC-307. This was `force_all(pct)`, which drove every
    /// output *to* `pct` — a **replacement** of the profile's own duty, not a
    /// floor over it. For the 100% emergency the two coincide, because 100 is
    /// the maximum. For the other two rungs of the ladder they do not, and the
    /// replacement could *reduce* cooling: the 60% recovery step fired while a
    /// CPU was coming down through the release point, overriding a curve asking
    /// for far more than 60%, and the 40% no-sensor fallback did the same to a
    /// control driven by a still-healthy GPU or coolant sensor.
    ///
    /// Each output now gets `max(commanded, pct)`; an output no control
    /// commands still gets `pct`. That is what keeps the emergency's **reach**
    /// intact — clamping the command list alone would shrink the force to
    /// controlled fans only, which is the v2.38.0 P1 shape. The result is
    /// monotone against the old behaviour: no output is ever driven lower than
    /// `force_all(pct)` would have driven it.
    ///
    /// Async since DEC-146 P3-8: implementations run their blocking
    /// serial/sysfs writes on the blocking pool instead of pinning a tokio
    /// worker for up to `channels × serial-timeout` during an emergency.
    async fn force_all_with_floor(&mut self, pct: u8, commands: &[PwmCommand]);
}

// ─── OpenFan (serial) ────────────────────────────────────────────────────

pub(crate) struct OpenFanBackend {
    ctrl: Arc<Mutex<crate::serial::controller::FanController>>,
    /// Per-channel consecutive write-failure streaks (audit P3-5). Replaces a
    /// single shared counter that reset on ANY channel's success, so a
    /// persistent single-channel fault among healthy channels never tripped the
    /// SAFETY alert. Reset per channel by that channel's own success.
    channel_failures: HashMap<u8, u32>,
    /// Consecutive ticks where EVERY attempted channel failed (or the blocking
    /// write task panicked): the whole-link "serial link down" signal (audit
    /// P3-5), kept distinct from the per-channel streaks. Reset when any channel
    /// succeeds.
    link_down_streak: u32,
    /// Engine write-pause gate (DEC-165). Re-checked inside the blocking write
    /// (DEC-191) so an OpenFan calibration sweep that claims the pause mid-tick
    /// is not overwritten by an engine tick already in flight.
    cache: Arc<StateCache>,
    /// Bounded join for this backend's blocking writes (DEC-289).
    ///
    /// Shared by `apply` and `force_all_with_floor` deliberately. They never run in the same
    /// tick — the emergency path `continue`s before the apply phase — but they do
    /// run in *consecutive* ticks, and a `force_all_with_floor` still wedged when the
    /// emergency clears must not let `apply` strand a second blocking thread
    /// against the same stuck device.
    writes: BoundedWrite<Vec<(u8, Result<(), String>)>>,
    /// Edge-trigger for the "write still in flight" safety log (DEC-289), so a
    /// legitimately slow emergency write reports its transition once instead of
    /// once per tick for the whole emergency-to-release hold.
    stall_logged: bool,
}

impl OpenFanBackend {
    pub(crate) fn new(
        ctrl: Arc<Mutex<crate::serial::controller::FanController>>,
        cache: Arc<StateCache>,
    ) -> Self {
        Self {
            ctrl,
            channel_failures: HashMap::new(),
            link_down_streak: 0,
            cache,
            writes: BoundedWrite::default(),
            stall_logged: false,
        }
    }

    /// True while a write issued on an earlier tick has not returned (DEC-289).
    pub(crate) fn writes_stalled(&self) -> bool {
        self.writes.stalled()
    }

    /// Await any in-flight write at shutdown (DEC-289). See [`BoundedWrite::drain`].
    pub(crate) async fn drain_writes(&mut self, deadline: std::time::Duration) {
        self.writes.drain(deadline).await;
    }

    /// Record this tick's per-channel write outcomes and fire SAFETY alerts
    /// (audit P3-5). Two independent signals, each edge-triggered at
    /// [`constants::OPENFAN_FAIL_ALERT_THRESHOLD`] so a persistent fault does not
    /// re-log every 1 Hz tick:
    /// - **per-channel** — a channel's consecutive-failure streak hitting the
    ///   threshold ("ch{n} not responding"); reset only by that channel's own
    ///   success, so a single dead channel among healthy ones is no longer
    ///   masked by the others.
    /// - **whole-link** — every attempted channel failing for the threshold
    ///   consecutively ("serial link appears down"); reset the moment any
    ///   channel succeeds.
    ///
    /// `results` holds only channels actually attempted this tick — channels
    /// skipped by the in-flight verify/calibration pause are absent, so they
    /// neither count as a failure nor reset a streak.
    fn note_outcomes(&mut self, results: &[(u8, Result<(), String>)]) {
        if results.is_empty() {
            return;
        }
        let mut any_ok = false;
        for (ch, res) in results {
            match res {
                Err(e) => {
                    let streak = self.channel_failures.entry(*ch).or_insert(0);
                    *streak += 1;
                    let n = *streak;
                    log::warn!(
                        "Profile engine: OpenFan ch{ch} write failed ({n} consecutive): {e}"
                    );
                    if n == constants::OPENFAN_FAIL_ALERT_THRESHOLD {
                        log::error!(
                            "SAFETY: OpenFan ch{ch} not responding \
                             ({n} consecutive write failures)"
                        );
                    }
                }
                Ok(()) => {
                    any_ok = true;
                    self.channel_failures.remove(ch);
                }
            }
        }
        if any_ok {
            self.link_down_streak = 0;
        } else {
            self.link_down_streak += 1;
            if self.link_down_streak == constants::OPENFAN_FAIL_ALERT_THRESHOLD {
                log::error!(
                    "SAFETY: OpenFan serial link appears down \
                     (all {} channels failing for {} consecutive ticks)",
                    results.len(),
                    self.link_down_streak
                );
            }
        }
    }

    /// Account a panicked blocking write task as a whole-link failure (no
    /// per-channel results exist — the task died). Returns the new link-down
    /// streak for the caller's alert log.
    fn note_task_panic(&mut self) -> u32 {
        self.link_down_streak += 1;
        let n = self.link_down_streak;
        // A persistent panic mode must also trip the whole-link SAFETY alert,
        // not just the per-tick "task panicked" log (audit P3-5 follow-up).
        if n == constants::OPENFAN_FAIL_ALERT_THRESHOLD {
            log::error!(
                "SAFETY: OpenFan serial link appears down \
                 (write task panicking for {n} consecutive ticks)"
            );
        }
        n
    }

    #[cfg(test)]
    fn channel_failure_streak(&self, ch: u8) -> u32 {
        self.channel_failures.get(&ch).copied().unwrap_or(0)
    }

    #[cfg(test)]
    fn link_down_streak(&self) -> u32 {
        self.link_down_streak
    }
}

impl WriteBackend for OpenFanBackend {
    /// OpenFan writes (serial I/O on the blocking pool — lock per command).
    ///
    /// Exact-match coalescing lives below this in `serial::controller`.
    ///
    /// DEC-146 P3-8: serial writes block up to the configured timeout
    /// (500 ms default) per channel, so the batch runs on `spawn_blocking`
    /// (matching `GpuBackend::apply` and both poll loops) instead of pinning
    /// a tokio worker. The mutex is still taken per command (DEC-099) so
    /// concurrent API requests interleave exactly as before.
    async fn apply(&mut self, commands: &[PwmCommand]) {
        let chans: Vec<(u8, u8)> = commands
            .iter()
            .filter(|c| c.source == "openfan")
            .filter_map(|cmd| {
                let Some(ch_str) = cmd.member_id.strip_prefix("openfan:ch") else {
                    log::warn!(
                        "Profile engine: dropping openfan command with malformed member_id: {:?}",
                        cmd.member_id
                    );
                    return None;
                };
                let Ok(ch) = ch_str.parse::<u8>() else {
                    log::warn!(
                        "Profile engine: dropping openfan command with unparseable channel: {:?}",
                        cmd.member_id
                    );
                    return None;
                };
                Some((ch, cmd.pwm_percent))
            })
            .collect();
        // DEC-289: only a true no-op when nothing is outstanding either. With a
        // write still pending, this call is what re-awaits it — returning here
        // would leave a finished write unharvested and its stall stamp set
        // forever, reporting `crit` for a device that had recovered.
        if chans.is_empty() && !self.writes.outstanding() {
            return;
        }
        let ctrl = self.ctrl.clone();
        let cache = self.cache.clone();
        let join = self
            .writes
            .run(WRITE_JOIN_BUDGET, move || {
                chans
                    .into_iter()
                    .filter_map(|(ch, pct)| {
                        // Lock per command (DEC-099) so GUI API requests can
                        // interleave between channel writes.
                        let mut guard = ctrl.lock();
                        // DEC-191: re-check the engine write-pause while HOLDING the
                        // controller lock, so the check-and-write is atomic against a
                        // concurrent OpenFan calibration sweep (whose test writes take
                        // this same lock). An engine tick already in flight when the
                        // sweep claims the pause must not overwrite the sweep's test
                        // PWM; checking before the lock left a narrow window where one
                        // channel's write could still land just after the sweep
                        // claimed the pause, corrupting its first RPM readback. A
                        // skipped channel records no outcome (it was not attempted),
                        // so it neither counts as a failure nor resets a streak.
                        if cache.verify_active() {
                            return None;
                        }
                        let res = guard
                            .set_pwm(ch, pct)
                            .map(|_| ())
                            .map_err(|e| e.to_string());
                        Some((ch, res))
                    })
                    .collect::<Vec<(u8, Result<(), String>)>>()
            })
            .await;
        // DEC-298: route EVERY write that completed — a harvested one from an
        // earlier tick and this call's own. A channel that was not attempted
        // (nothing completed) records no outcome, so its failure streak neither
        // advances nor resets, exactly as DEC-289 intended.
        //
        // `&mut self` state can't cross into the 'static closure, so the
        // per-channel + whole-link failure bookkeeping runs here on the returned
        // results (audit P3-5).
        for joined in join.completed() {
            match joined {
                Ok(results) => self.note_outcomes(&results),
                Err(e) => {
                    // Concurrency review D3: a panic inside the blocking task
                    // must not be silent. The whole write task died, so account
                    // it as a whole-link failure (audit P3-5) and alert now.
                    let n = self.note_task_panic();
                    log::error!(
                        "SAFETY: Profile engine OpenFan write task panicked: {e} \
                         (link-down streak {n})"
                    );
                }
            }
        }
    }
}

impl SafetyWriteBackend for OpenFanBackend {
    /// Drive every OpenFan channel to at least `pct` (D1-j).
    ///
    /// Every channel is written, including ones no control commands — that is
    /// the reach the emergency depends on. A commanded channel gets
    /// `max(commanded, pct)`.
    ///
    /// DEC-099: drop the lock between channels so GUI requests can
    /// interleave during a long emergency scan; if the GUI overrides a
    /// safety value briefly, the next 1Hz tick re-asserts the forced value.
    ///
    /// DEC-146 P3-8: runs on the blocking pool — worst case is
    /// `NUM_CHANNELS × serial-timeout` (10 × 500 ms default), far too long
    /// to pin a tokio worker during a thermal emergency.
    async fn force_all_with_floor(&mut self, pct: u8, commands: &[PwmCommand]) {
        let ctrl = self.ctrl.clone();
        // D1-j: this tick's profile duty per channel, so `pct` acts as a floor
        // over it rather than replacing it. Parsed silently — `apply` owns the
        // malformed-`member_id` warning, and a forced tick `continue`s before
        // `apply`, so warning here too would repeat it at 1 Hz for the whole
        // hold. A channel missing from this map is simply uncommanded and gets
        // the bare floor, which is the pre-D1-j behaviour for every channel.
        let floors: HashMap<u8, u8> = commands
            .iter()
            .filter(|c| c.source == "openfan")
            .filter_map(|c| {
                let ch = c.member_id.strip_prefix("openfan:ch")?.parse::<u8>().ok()?;
                Some((ch, c.pwm_percent))
            })
            .collect();
        // Returns the same outcome type as `apply` so both share one
        // `BoundedWrite` (DEC-289); the emergency path logs inline, so the vec is
        // always empty and nothing consumes it.
        let join = self
            .writes
            .run(WRITE_JOIN_BUDGET, move || {
                for ch in 0..NUM_CHANNELS {
                    let duty = floors
                        .get(&ch)
                        .map_or(pct, |commanded| (*commanded).max(pct));
                    let mut guard = ctrl.lock();
                    if let Err(e) = guard.set_pwm(ch, duty) {
                        log::error!("THERMAL SAFETY: OpenFan ch{ch} write FAILED: {e}");
                    }
                }
                Vec::new()
            })
            .await;
        // DEC-298: "stalled" is *nothing completed*, not *something in flight* —
        // the same distinction the write-stall stamp draws. A device merely
        // slower than the budget completes a write every tick and must not be
        // reported as stalled.
        let stalled = join.in_flight && join.harvested.is_none() && join.issued.is_none();
        // A harvested `apply` result arrives here when force_all_with_floor re-awaits a
        // pending ordinary write. Route it through the normal accounting rather
        // than dropping it, or a failing channel's streak is neither advanced nor
        // reset for that tick (DEC-289). Since DEC-298 this call can also have
        // issued and completed its OWN write, so both are routed.
        for joined in join.completed() {
            match joined {
                Ok(outcomes) => self.note_outcomes(&outcomes),
                // Concurrency review D3: never swallow a panicked safety write —
                // the next 1 Hz tick retries, but the operator must see this.
                Err(e) => {
                    log::error!("THERMAL SAFETY: OpenFan force_all_with_floor task panicked: {e}")
                }
            }
        }
        // DEC-289: still in flight. The write was NOT abandoned — the handle is
        // held and re-awaited next tick — but the loop is released so the safety
        // ladder and the other backends keep running instead of freezing behind
        // this one device.
        //
        // Edge-triggered: a legitimately slow force_all_with_floor spans several ticks over
        // a degraded link, and an emergency-to-release hold can last minutes, so a per-tick
        // line would bury the transition it exists to report. Every other safety
        // log in this file is throttled the same way.
        if stalled {
            if !self.stall_logged {
                self.stall_logged = true;
                log::error!(
                    "THERMAL SAFETY: OpenFan force_all_with_floor still in flight after \
                     {}s — the emergency write has not reached the controller yet",
                    WRITE_JOIN_BUDGET.as_secs()
                );
            }
        } else if self.stall_logged {
            // DEC-298: the falling edge. Without it the journal asserted a
            // condition that had cleared minutes earlier, and an emergency-to-release hold can
            // last minutes.
            self.stall_logged = false;
            log::warn!("THERMAL SAFETY: OpenFan force_all_with_floor writes are landing again");
        }
    }
}

// ─── AMD GPU (PMFW fan_curve / legacy pwm1) ──────────────────────────────

/// GPU fan writes via the PMFW `fan_curve` interface.
///
/// Body of the GPU write blocking task (CONC-1, 2026-07-21 audit).
///
/// Re-checks the engine write-pause at the last moment before the sysfs
/// write: the loop-level gate (`mod.rs`) and the per-fan pre-check in
/// [`GpuBackend`]'s `apply` both run on the async worker, so a GPU fan verify
/// (`POST /gpu/{id}/fan/verify`) can claim the pause after those checks and
/// before this task executes on the blocking pool — its test value must not
/// be overwritten. GPU writes hold no lock (DEC-045), so this mirrors the
/// OpenFan in-closure re-check (DEC-191) as closely as a lockless path can;
/// the remaining window is the gap between this check and the write syscall.
///
/// Returns `None` when the pause was held (skipped — no outcome: neither a
/// success nor a failure for the fail-cache), `Some(write result)` otherwise.
/// A named fn rather than a closure so the in-task guard is unit-testable.
fn gpu_blocking_write(
    cache: &StateCache,
    fan_curve_path: &std::path::Path,
    zero_rpm_path: Option<&std::path::Path>,
    speed_pct: u8,
    preserve_zero_rpm: bool,
    fan_id: &str,
) -> Option<Result<(), ()>> {
    if cache.verify_active() {
        return None;
    }
    // DEC-254: the same last-moment re-check for the *other* racer. `apply`
    // tests this on the async worker before dispatching here, so a
    // `POST /gpu/{id}/fan/reset` landing in between used to let this write
    // overwrite firmware-auto with the profile's flat curve — and because the
    // fan is relinquished by then, `apply` skips it on every later tick, so
    // nothing ever corrects it. The GPU stays pinned on a stale curve until the
    // next profile activation or a restart. Unlike the verify race above, whose
    // cost is one lost test value, that outcome is permanent.
    if cache.is_gpu_fan_relinquished(fan_id) {
        return None;
    }
    Some(
        match crate::hwmon::gpu_fan::set_static_speed_with_zero_rpm(
            fan_curve_path,
            zero_rpm_path,
            speed_pct,
            constants::GPU_PMFW_NUM_CURVE_POINTS,
            preserve_zero_rpm,
        ) {
            Ok(()) => {
                cache.set_gpu_fan_commanded_pct(fan_id, speed_pct);
                Ok(())
            }
            Err(e) => {
                log::warn!("GPU fan write failed: {e}");
                Err(())
            }
        },
    )
}

/// Budget for one GPU write batch (DEC-299).
///
/// Separate from `WRITE_JOIN_BUDGET` because the units differ: a PMFW curve
/// write is N point writes plus a commit, per fan, and the batch may cover more
/// than one card. Kept at the same 1 s as the others for now — the value is not
/// the point, the bound is.
const GPU_WRITE_JOIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(1);

/// One GPU fan's outcome: which fan, the duty ATTEMPTED, and what happened —
/// `None` when the in-task re-check skipped it (DEC-299).
///
/// The pct travels WITH the outcome rather than being looked up per tick,
/// because a harvested result belongs to an earlier tick's batch.
type GpuWriteOutcome = (String, u8, Option<Result<(), ()>>);

/// One fan's pending GPU write, resolved on the async side so the blocking
/// closure owns everything it needs (DEC-299).
#[derive(Clone)]
struct GpuFanWrite {
    fan_id: String,
    path: std::path::PathBuf,
    zero_rpm: Option<std::path::PathBuf>,
    pct: u8,
    preserve_zero_rpm: bool,
}

/// Deliberately NOT a [`SafetyWriteBackend`] (DEC-130) — see the module
/// docs. GPU thermal protection is the firmware's job.
pub(crate) struct GpuBackend {
    cache: Arc<StateCache>,
    gpu_infos: Arc<Vec<crate::hwmon::gpu_detect::AmdGpuInfo>>,
    /// GPU writes that failed — skip retry until the speed changes or a
    /// cooldown elapses. Prevents 1/sec journal spam when PMFW rejects the
    /// value. Key: fan_id, Value: (failed_speed_pct, failure_instant).
    fail_cache: HashMap<String, (u8, std::time::Instant)>,
    /// Monotonic clock for the [`constants::GPU_FAIL_COOLDOWN`] TTL (P3-7).
    /// Injectable so the 60 s cooldown can be exercised under deterministic
    /// fake time, mirroring `OverrideTable`/`LeaseManager`; production uses
    /// [`crate::clock::SystemClock`].
    clock: Arc<dyn Clock>,
    /// Bounded join for this backend's blocking writes (DEC-299), the third and
    /// last backend to get one. DEC-289 bounded hwmon and OpenFan and left this
    /// path alone for a structural reason: the GPU task is handed an **owned**
    /// `lock_gpu_writes` guard (DEC-255), so holding a handle across ticks would
    /// hold the GPU write lock across ticks and the next tick would block
    /// acquiring it — moving the freeze rather than removing it. Taking the lock
    /// with a bounded wait and skipping the tick when it is unavailable is what
    /// makes the bound work here.
    writes: BoundedWrite<Vec<GpuWriteOutcome>>,
    /// Edge-trigger for the "write still in flight" log, as on the other two.
    stall_logged: bool,
    /// Edge-trigger for LOCK CONTENTION, kept separate from `stall_logged`
    /// (DEC-299): a verify holding the lock and a wedged write are different
    /// conditions, and one flag for both made the recovery line report a
    /// stall that had never happened.
    lock_wait_logged: bool,
    /// The batch currently in flight, for the panic path only (DEC-299).
    ///
    /// A `JoinError` says the whole task died without saying which fans it was
    /// carrying, and a harvested panic belongs to an EARLIER tick's batch — so
    /// attributing it to the current tick's commands caches failures against the
    /// wrong fans at the wrong duty. Ordinary outcomes carry their own pct in the
    /// tuple; this covers only the case where no outcome comes back at all.
    outstanding_batch: Vec<(String, u8)>,
}

impl GpuBackend {
    pub(crate) fn new(
        cache: Arc<StateCache>,
        gpu_infos: Arc<Vec<crate::hwmon::gpu_detect::AmdGpuInfo>>,
    ) -> Self {
        Self::with_clock(cache, gpu_infos, Arc::new(crate::clock::SystemClock))
    }

    /// Construct on an injected clock. Tests advance a fake clock to exercise
    /// the fail-cooldown deterministically instead of sleeping 60 s.
    pub(crate) fn with_clock(
        cache: Arc<StateCache>,
        gpu_infos: Arc<Vec<crate::hwmon::gpu_detect::AmdGpuInfo>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            cache,
            gpu_infos,
            fail_cache: HashMap::new(),
            clock,
            writes: BoundedWrite::default(),
            stall_logged: false,
            lock_wait_logged: false,
            outstanding_batch: Vec::new(),
        }
    }

    #[cfg(test)]
    fn fail_cache_len(&self) -> usize {
        self.fail_cache.len()
    }

    /// True when the last `apply` returned through the LOCK-SKIPPED branch.
    ///
    /// A fixture check, not a behaviour: `lock_wait_logged` is set only in that
    /// branch and cleared only after a successful acquire, so reading it is a
    /// zero-cost way for a test to prove the tick it just drove actually took
    /// the path under test. Without it, a test can assert the *outcome*
    /// (`writes_stalled()`) while the tick reached it down some other path —
    /// which is exactly how the DEC-299 F1 test came to pass with its own fix
    /// removed (register row 299-a).
    #[cfg(test)]
    fn took_lock_skip(&self) -> bool {
        self.lock_wait_logged
    }
}

impl GpuBackend {
    /// True while a GPU write is not getting through (DEC-298 semantics: nothing
    /// completed, not merely something in flight).
    pub(crate) fn writes_stalled(&self) -> bool {
        self.writes.stalled()
    }

    /// Await any in-flight GPU write at shutdown (DEC-289/299).
    pub(crate) async fn drain_writes(&mut self, deadline: std::time::Duration) {
        self.writes.drain(deadline).await;
    }

    /// Route the batch outcomes into the fail-cache (DEC-299).
    ///
    /// `&mut self` cannot cross into the `'static` closure, so this runs on the
    /// returned outcomes — the same shape as `HwmonBackend::note_outcomes`.
    fn note_gpu_outcomes(&mut self, progress: WriteProgress<Vec<GpuWriteOutcome>>) {
        for joined in progress.completed() {
            match joined {
                Ok(outcomes) => {
                    for (fan_id, pct, outcome) in outcomes {
                        match outcome {
                            Some(Ok(())) => {
                                self.fail_cache.remove(&fan_id);
                            }
                            // CONC-1: a verify or a reset claimed the path between
                            // the per-fan check and the blocking task running — the
                            // write was SKIPPED, which is no outcome at all: not a
                            // success, and not a failure to cache.
                            None => {}
                            // DEC-299: the pct comes from the outcome itself, not
                            // from a per-tick map. A harvested result belongs to an
                            // EARLIER tick's batch, so looking the duty up against
                            // the current tick cached a failure for the wrong value
                            // — fabricating 0% when that fan was not commanded this
                            // tick, which then suppressed a genuine 0% command for a
                            // whole `GPU_FAIL_COOLDOWN` while the value that really
                            // failed was retried at 1 Hz.
                            Some(Err(())) => {
                                self.fail_cache.insert(fan_id, (pct, self.clock.now()));
                            }
                        }
                    }
                }
                // DEC-265: a panicking write task used to land silently. Report it
                // as the bug it is, and cache the failure for every fan the batch
                // was carrying — the batch died, so none of them was written.
                //
                // DEC-299: attributed to the batch that was actually OUTSTANDING,
                // not to whatever this tick happens to be commanding. A harvested
                // panic belongs to an earlier tick.
                Err(e) => {
                    log::error!(
                        "GPU fan write task panicked: {e} — the writes did not happen; \
                         caching the failures and continuing"
                    );
                    let now = self.clock.now();
                    for (fan_id, pct) in std::mem::take(&mut self.outstanding_batch) {
                        self.fail_cache.insert(fan_id, (pct, now));
                    }
                }
            }
        }
    }
}

impl WriteBackend for GpuBackend {
    /// GPU fan writes (async via spawn_blocking, no lease required).
    ///
    /// Suppresses writes whose delta from the last commanded value is below
    /// `GPU_COALESCE_DELTA_PCT`, mirroring the API handler so headless and
    /// imperative paths share DEC-070's single 5% threshold (DEC-131).
    async fn apply(&mut self, commands: &[PwmCommand]) {
        // One snapshot per tick — advisory write-suppression state, not
        // correctness-critical (a torn read vs. the API path is harmless:
        // the next tick re-evaluates).
        let gpu_fans = self.cache.gpu_fans_snapshot();
        let mut pending_writes: Vec<GpuFanWrite> = Vec::new();

        for cmd in commands.iter().filter(|c| c.source == "amd_gpu") {
            // P2-1: re-check the engine write-pause per fan. A GPU fan verify
            // (POST /gpu/{id}/fan/verify) sets the pause and force-writes a test
            // value mid-tick; an engine tick already past the loop-level
            // `verify_active` gate (mod.rs) must not overwrite it. GPU fans have
            // no lease (DEC-045), so this per-fan recheck is the only guard —
            // re-read each iteration so a verify starting mid-loop stops the
            // remaining fans too.
            if self.cache.verify_active() {
                continue;
            }
            // DEC-165: skip a GPU fan the operator relinquished to firmware-auto
            // via POST /gpu/{id}/fan/reset, so the reset is durable under an
            // active profile (the set is cleared on the next profile activation).
            if self.cache.is_gpu_fan_relinquished(&cmd.member_id) {
                continue;
            }
            if let Some(cached) = gpu_fans.get(&cmd.member_id) {
                if let Some(last_pct) = cached.last_commanded_pct {
                    let delta = (cmd.pwm_percent as i16 - last_pct as i16).unsigned_abs();
                    if delta < constants::GPU_COALESCE_DELTA_PCT {
                        continue;
                    }
                }
            }

            // Failure suppression: skip if the same speed already failed
            // recently.
            if let Some((failed_pct, failed_at)) = self.fail_cache.get(&cmd.member_id) {
                if *failed_pct == cmd.pwm_percent
                    && self.clock.now().saturating_duration_since(*failed_at)
                        < constants::GPU_FAIL_COOLDOWN
                {
                    continue;
                }
            }

            let Some(bdf) = cmd.member_id.strip_prefix("amd_gpu:") else {
                continue;
            };
            let Some(gpu) = self.gpu_infos.iter().find(|g| g.pci_bdf == bdf) else {
                continue;
            };
            let Some(ref curve_path) = gpu.fan_curve_path else {
                continue;
            };

            pending_writes.push(GpuFanWrite {
                fan_id: cmd.member_id.clone(),
                path: curve_path.clone(),
                zero_rpm: gpu.fan_zero_rpm_path.clone(),
                pct: cmd.pwm_percent,
                preserve_zero_rpm: cmd.gpu_fan_zero_rpm,
            });
        }

        // DEC-299: nothing to command, but an earlier write may still be
        // outstanding and must be harvested — the same shape as `HwmonBackend`.
        if pending_writes.is_empty() {
            if self.writes.outstanding() {
                let progress = self.writes.harvest_only(GPU_WRITE_JOIN_BUDGET).await;
                self.note_gpu_outcomes(progress);
            }
            return;
        }

        // DEC-299, blocking window TWO — the one the register row did not name.
        // `lock_gpu_writes()` is itself an unbounded `.await` on the mutex, and a
        // GPU verify holds that lock for its whole multi-second window (more so
        // since DEC-297 moved the verify into an uncancellable `spawn_blocking`).
        // The per-fan `verify_active()` check is read BEFORE the lock, so a
        // verify starting in that gap froze the engine loop on the mutex with no
        // wedged device at all. Bounded wait + skip the GPU leg for this tick.
        //
        // Skipping is safe and is what makes the join bound work: a detached
        // wedged write keeps the lock, so the next tick cannot take it and
        // therefore cannot spawn a second blocking task. That self-limits to one
        // outstanding task without holding a handle across ticks — the exact
        // reason DEC-289 could not bound this path.
        let Some(write_guard) = self
            .cache
            .lock_gpu_writes_soon(constants::GPU_RESET_LOCK_WAIT)
            .await
        else {
            // DEC-299 (F1): this path returns without reaching `run`, so nothing
            // would otherwise refresh the progress flag — and it is reached
            // precisely when a wedged write is holding the lock. Say "no
            // progress" explicitly or a permanent wedge reports healthy forever.
            if self.writes.outstanding() {
                self.writes.note_no_progress();
            }
            // A SEPARATE edge-trigger from the write stall (F6): lock contention
            // and a wedged write are different conditions, and sharing one flag
            // made the recovery line narrate an event that never happened.
            if !self.lock_wait_logged {
                self.lock_wait_logged = true;
                log::warn!(
                    "GPU fan writes skipped this tick — the GPU write lock is held \
                     (a verify, a reset, or an earlier write that has not returned)"
                );
            }
            return;
        };
        if self.lock_wait_logged {
            self.lock_wait_logged = false;
            log::info!("GPU write lock is available again");
        }

        // DEC-255: exclude `POST /gpu/{id}/fan/reset` for the duration of these
        // writes. A PMFW curve write is N point writes plus a `"c"` commit and a
        // reset is `"r"`+`"c"`; interleaving them can commit a curve that is
        // neither the profile's nor firmware-auto, which no later tick reconciles
        // because the reset relinquishes the fan and this loop then skips it. The
        // in-task relinquish re-check inside `gpu_blocking_write` narrows that
        // race; this removes it.
        //
        // DEC-299 note: the guard now spans the whole batch rather than one fan.
        // Coarser than before, and deliberately — one bounded task per tick is
        // what the join bound needs. In practice the batch is one or two GPUs of
        // sub-millisecond writes; `fan/reset` waits on the same bounded
        // `lock_gpu_writes_soon` and reports its documented 409 rather than
        // hanging.
        let cache_ref = self.cache.clone();
        self.outstanding_batch = pending_writes
            .iter()
            .map(|w| (w.fan_id.clone(), w.pct))
            .collect();
        let batch = pending_writes.clone();
        let progress = self
            .writes
            .run(GPU_WRITE_JOIN_BUDGET, move || {
                let _write_guard = write_guard;
                batch
                    .into_iter()
                    .map(|w| {
                        let outcome = gpu_blocking_write(
                            &cache_ref,
                            &w.path,
                            w.zero_rpm.as_deref(),
                            w.pct,
                            w.preserve_zero_rpm,
                            &w.fan_id,
                        );
                        (w.fan_id, w.pct, outcome)
                    })
                    .collect::<Vec<_>>()
            })
            .await;

        if progress.in_flight && progress.issued.is_none() {
            if !self.stall_logged {
                self.stall_logged = true;
                log::warn!(
                    "a GPU fan write has not returned within {}s — the fan is holding \
                     its last duty",
                    GPU_WRITE_JOIN_BUDGET.as_secs()
                );
            }
        } else if self.stall_logged {
            self.stall_logged = false;
            log::warn!("GPU fan writes are landing again");
        }
        self.note_gpu_outcomes(progress);
    }
}

// ─── Motherboard hwmon ───────────────────────────────────────────────────

pub(crate) struct HwmonBackend {
    ctrl: Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>,
    /// Per-member consecutive write-failure streaks (DEC-199). Without this the
    /// per-header `warn!` re-logged every 1 Hz tick, so a persistent hwmon write
    /// failure — canonically EROFS when the systemd sandbox's `ReadWritePaths=`
    /// carve-out does not cover the real `/sys/devices` inode — spammed journald
    /// at 1 Hz. We log the FIRST failure per member, then only a periodic summary
    /// every [`constants::HWMON_FAIL_SUMMARY_INTERVAL`] ticks, and an INFO
    /// recovery line when the member writes successfully again. Reset per member
    /// by its own success, so a single stuck header among healthy ones is tracked
    /// in isolation (mirrors [`OpenFanBackend`]'s `channel_failures`, audit P3-5).
    member_failures: HashMap<String, u32>,
    /// Bounded join for this backend's blocking writes (DEC-289). Shared by
    /// `apply` and `force_all_with_floor` for the reason documented on
    /// [`OpenFanBackend::writes`]: a wedged emergency write must not let the next
    /// tick's ordinary write strand a second blocking thread on the same header.
    writes: BoundedWrite<Vec<(String, Result<(), String>)>>,
    /// Edge-trigger for the "write still in flight" safety log (DEC-289), so a
    /// legitimately slow emergency write reports its transition once instead of
    /// once per tick for the whole emergency-to-release hold.
    stall_logged: bool,
}

impl HwmonBackend {
    pub(crate) fn new(ctrl: Arc<Mutex<crate::hwmon::pwm_control::HwmonPwmController>>) -> Self {
        Self {
            ctrl,
            member_failures: HashMap::new(),
            writes: BoundedWrite::default(),
            stall_logged: false,
        }
    }

    /// True while a write issued on an earlier tick has not returned (DEC-289).
    pub(crate) fn writes_stalled(&self) -> bool {
        self.writes.stalled()
    }

    /// Await any in-flight write at shutdown (DEC-289). See [`BoundedWrite::drain`].
    pub(crate) async fn drain_writes(&mut self, deadline: std::time::Duration) {
        self.writes.drain(deadline).await;
    }

    /// Record this tick's per-member hwmon write outcomes and throttle the log
    /// (DEC-199). `results` holds only members actually attempted — a member
    /// skipped read-only (DEC-102) or skipped because the engine held no lease
    /// this tick is absent, so it neither counts as a failure nor resets a
    /// streak. `&mut self` state can't cross into the `'static` blocking closure,
    /// so this bookkeeping runs on the returned outcomes after the join (mirrors
    /// [`OpenFanBackend::note_outcomes`]).
    fn note_outcomes(&mut self, results: &[(String, Result<(), String>)]) {
        for (member_id, res) in results {
            match res {
                Err(e) => {
                    let streak = self.member_failures.entry(member_id.clone()).or_insert(0);
                    *streak += 1;
                    let n = *streak;
                    if n == 1 {
                        log::warn!("hwmon write failed for {member_id}: {e}");
                    } else if n.is_multiple_of(constants::HWMON_FAIL_SUMMARY_INTERVAL) {
                        log::warn!(
                            "hwmon write still failing for {member_id} \
                             ({n} consecutive ticks): {e}"
                        );
                    }
                }
                Ok(()) => {
                    // Recovery edge: a member that had been failing wrote again.
                    if let Some(prev) = self.member_failures.remove(member_id) {
                        log::info!(
                            "hwmon write recovered for {member_id} \
                             (after {prev} consecutive failure(s))"
                        );
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn member_failure_streak(&self, member_id: &str) -> u32 {
        self.member_failures.get(member_id).copied().unwrap_or(0)
    }
}

impl WriteBackend for HwmonBackend {
    /// hwmon writes (auto-lease for headless profile mode).
    ///
    /// The profile engine auto-acquires the lease when writing hwmon members
    /// and is the steady-state holder (DEC-165 — the GUI no longer takes the
    /// lease). DEC-146 P3-8: the body runs on the blocking pool (matching the
    /// hwmon poll loop). DEC-154: the lease-acquire → per-header write → renew
    /// sequence locks the controller mutex PER COMMAND (like `force_all_with_floor` and
    /// `OpenFanBackend`), not once for the whole batch, so concurrent API
    /// requests are not starved for the duration of a multi-header tick. A
    /// thermal force-take mid-scan fails the remaining writes with InvalidLease;
    /// the next 1 Hz tick re-acquires.
    async fn apply(&mut self, commands: &[PwmCommand]) {
        let hwmon_cmds: Vec<(String, u8)> = commands
            .iter()
            .filter(|c| c.source == "hwmon")
            .map(|c| (c.member_id.clone(), c.pwm_percent))
            .collect();
        // DEC-289: see the note on `OpenFanBackend::apply` — an outstanding write
        // must still be re-awaited on a tick that has no commands of its own.
        if hwmon_cmds.is_empty() && !self.writes.outstanding() {
            return;
        }
        // DEC-298: harvest the outstanding write, but do NOT issue a new one when
        // there is nothing to command. Before this change `run` dropped the
        // closure on a harvest, so an empty command list cost nothing; now it
        // would be spawned, and it takes an Engine hwmon lease and renews it for
        // a tick that writes no PWM at all. No hardware effect — `take_lease` is
        // bookkeeping — but a 60 s lease acquired to command nothing is a lie
        // about who owns the header.
        if hwmon_cmds.is_empty() {
            let progress = self.writes.harvest_only(WRITE_JOIN_BUDGET).await;
            for joined in progress.completed() {
                match joined {
                    Ok(outcomes) => self.note_outcomes(&outcomes),
                    Err(e) => log::error!("Profile engine: hwmon write task panicked: {e}"),
                }
            }
            return;
        }
        let ctrl = self.ctrl.clone();
        let join = self
            .writes
            .run(WRITE_JOIN_BUDGET, move || {
                // Phase 1: acquire (or reuse) the profile-engine lease under a brief
                // lock, then release it so concurrent API requests can interleave
                // with the per-header writes below.
                let lease_id: Option<String> = {
                    let mut guard = ctrl.lock();
                    let existing = {
                        let mgr = guard.lease_manager();
                        // P2-1: reuse the engine's own lease or a transient
                        // thermal-safety force-take, but NEVER a hardware verify's
                        // lease. A verify force-takes the lease as "verify"
                        // (hwmon_ctl.rs) and sets the engine write-pause; if it
                        // starts *after* this tick passed the loop-level
                        // `verify_active` gate (mod.rs), the engine still reaches
                        // here. Adopting the verify's lease would let the engine
                        // write through it and clobber the test value — the bug the
                        // single up-front check did not close. Excluding the
                        // `Verify` owner makes `take_lease` below return AlreadyHeld ⇒
                        // `lease_id = None` ⇒ the engine skips its hwmon writes this
                        // tick; the verify's RAII guard releases the lease when it
                        // ends and the next tick re-acquires. Thermal-safety is NOT
                        // excluded — after an emergency the engine adopts and renews
                        // that lease as before, so there is no post-thermal stall.
                        // `None` ⇒ acquire.
                        mgr.active_lease()
                            .filter(|lease| lease.owner != HwmonWriter::Verify)
                            .map(|lease| lease.lease_id.clone())
                    };
                    existing.or_else(|| {
                        guard
                            .lease_manager_mut()
                            .take_lease(HwmonWriter::Engine)
                            .ok()
                            .map(|l| l.lease_id)
                    })
                };
                // No lease this tick (e.g. a hardware verify holds it) ⇒ nothing was
                // attempted; return an empty outcome set so no member's failure
                // streak is advanced or reset.
                let Some(lease_id) = lease_id else {
                    return Vec::new();
                };

                // Phase 2: one lock per header (DEC-154) so a concurrent reader or
                // lease op is not starved for the whole batch. A GUI/thermal
                // force-take mid-scan fails the remaining writes with InvalidLease;
                // the next 1 Hz tick re-acquires. Outcomes are collected here and the
                // (throttled) failure logging runs on `&mut self` after the join
                // (DEC-199) — the `'static` blocking closure cannot borrow self.
                let mut outcomes: Vec<(String, Result<(), String>)> =
                    Vec::with_capacity(hwmon_cmds.len());
                for (member_id, pwm_percent) in &hwmon_cmds {
                    let mut guard = ctrl.lock();
                    // DEC-102 backstop on the engine path: never attempt a write to
                    // a header discovered read-only (`is_writable == false`). The GUI
                    // member-picker and profile load drop these, but the un-validated
                    // boot-load path does not, so the engine must skip them itself —
                    // otherwise every tick would EACCES-spam the log and the member
                    // would silently never take effect. A skipped header records no
                    // outcome (it was not attempted).
                    if guard.header(member_id).is_some_and(|h| !h.is_writable) {
                        continue;
                    }
                    let res = guard
                        .set_pwm(member_id, *pwm_percent, &lease_id)
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    outcomes.push((member_id.clone(), res));
                }

                // Phase 3: renew under a brief lock to keep it alive for next cycle.
                if let Err(e) = ctrl.lock().lease_manager_mut().renew_lease(&lease_id) {
                    log::debug!("lease renewal failed (will re-acquire next cycle): {e}");
                }
                outcomes
            })
            .await;
        // DEC-298: route every completed write — a harvested one and this
        // call's own. Nothing completed means nothing was attempted, so record
        // no outcomes: a member's failure streak must neither advance nor reset
        // for a write that never happened (DEC-289).
        for joined in join.completed() {
            match joined {
                Ok(outcomes) => self.note_outcomes(&outcomes),
                // Concurrency review D3: surface panicked write tasks.
                Err(e) => log::error!("Profile engine: hwmon write task panicked: {e}"),
            }
        }
    }
}

impl SafetyWriteBackend for HwmonBackend {
    /// Drive every writable hwmon header to at least `pct` (D1-j),
    /// auto-leasing for safety writes.
    ///
    /// Every writable header is written, including ones no control commands —
    /// that is the reach the emergency depends on. A commanded header gets
    /// `max(commanded, pct)`.
    ///
    /// Force-takes the lease as thermal-safety, then re-locks the controller per
    /// header so concurrent GUI activity can proceed between writes (DEC-099).
    /// Because the lock is dropped between headers, a GUI hardware-verify can
    /// force-take the lease mid-scan and invalidate ours. Thermal safety
    /// outranks maintenance, so a write that fails with a lease error re-takes
    /// the lease and retries that header once — bounded, so a persistent
    /// preemptor cannot thrash — rather than leaving the remaining fans
    /// un-forced. (The lease system is hwmon-only; the OpenFan path has none.)
    /// DEC-146 P3-8: runs on the blocking pool; the re-lock-per-header structure
    /// (DEC-099) is preserved inside the closure.
    async fn force_all_with_floor(&mut self, pct: u8, commands: &[PwmCommand]) {
        let ctrl = self.ctrl.clone();
        // D1-j: this tick's profile duty per header, so `pct` acts as a floor
        // over it rather than replacing it. A header missing from this map is
        // uncommanded and gets the bare floor — the pre-D1-j behaviour for every
        // header. Read-only headers never reach the write below (DEC-295), so a
        // command naming one is irrelevant here either way.
        let floors: HashMap<String, u8> = commands
            .iter()
            .filter(|c| c.source == "hwmon")
            .map(|c| (c.member_id.clone(), c.pwm_percent))
            .collect();
        // Same outcome type as `apply` so both share one `BoundedWrite`
        // (DEC-289); this path logs inline, so the vec is always empty.
        let join = self.writes.run(WRITE_JOIN_BUDGET, move || {
            let (hdr_ids, mut lease_id) = {
                let mut guard = ctrl.lock();
                // DEC-295: skip headers discovered read-only, exactly as `apply`
                // does via the DEC-102 backstop a few hundred lines up. Without
                // it every read-only header attempted a write that could only
                // EACCES, and this path logs INLINE and unthrottled (see the
                // `Vec::new()` at the end of this closure) rather than through
                // `note_outcomes`' per-member streak throttle — so a single
                // read-only header emitted one THERMAL SAFETY ... FAILED line
                // per second for the whole emergency-to-release hold, burying real failures
                // at the moment they matter most. Not a loss of reach: the rule
                // is scoped to writable headers (`safety.rs`), so a read-only
                // one was never going to be driven.
                let hdr_ids: Vec<String> = guard
                    .headers()
                    .iter()
                    .filter(|h| h.is_writable)
                    .map(|h| h.id.clone())
                    .collect();
                let lease_id = guard
                    .lease_manager_mut()
                    .force_take_lease(HwmonWriter::ThermalSafety)
                    .lease_id;
                // Audit P1-E: a force-take is an ownership change the previous
                // holder was never notified of, so its coalescing state
                // (manual_mode_set) is stale. Reset it so thermal safety
                // unconditionally re-asserts pwm_enable=1 on its first forced
                // write — defense in depth alongside the per-write readback
                // watchdog in HwmonPwmController::set_pwm.
                guard.on_lease_released();
                (hdr_ids, lease_id)
            };
            for hdr_id in &hdr_ids {
                // D1-j: floor, not replacement. Resolved once per header so the
                // lease-retry below writes the identical duty.
                let duty = floors
                    .get(hdr_id)
                    .map_or(pct, |commanded| (*commanded).max(pct));
                let mut guard = ctrl.lock();
                match guard.set_pwm(hdr_id, duty, &lease_id) {
                    Ok(_) => {}
                    Err(HwmonControlError::Lease(_)) => {
                        // A concurrent GUI verify force-took the lease in the
                        // window DEC-099 leaves between headers, invalidating
                        // ours. Thermal safety outranks maintenance: re-take
                        // unconditionally and retry THIS header once. The re-take
                        // and retry share this one lock, so a preemptor cannot
                        // slip between them; the bound (one re-take per header)
                        // stops a persistent preemptor from thrashing the scan.
                        // force-take resets the new owner's coalescing state, so
                        // re-assert pwm_enable on the retry (Audit P1-E).
                        lease_id = guard
                            .lease_manager_mut()
                            .force_take_lease(HwmonWriter::ThermalSafety)
                            .lease_id;
                        guard.on_lease_released();
                        if let Err(e) = guard.set_pwm(hdr_id, duty, &lease_id) {
                            log::error!(
                                "THERMAL SAFETY: hwmon {hdr_id} write FAILED after lease re-take: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        log::error!("THERMAL SAFETY: hwmon {hdr_id} write FAILED: {e}");
                    }
                }
            }
            Vec::new()
        })
        .await;
        // DEC-298: "stalled" is *nothing completed*, not *something in flight* —
        // the same distinction the write-stall stamp draws. A device merely
        // slower than the budget completes a write every tick and must not be
        // reported as stalled.
        let stalled = join.in_flight && join.harvested.is_none() && join.issued.is_none();
        // A harvested `apply` result — see the OpenFan note. Since DEC-298 this
        // call may also have issued and completed its own write, so both route.
        for joined in join.completed() {
            match joined {
                Ok(outcomes) => self.note_outcomes(&outcomes),
                // Concurrency review D3: never swallow a panicked safety write.
                Err(e) => {
                    log::error!("THERMAL SAFETY: hwmon force_all_with_floor task panicked: {e}")
                }
            }
        }
        // DEC-289: still in flight — held and re-awaited next tick, never
        // re-issued. The loop is released so the ladder and the other backends
        // keep running instead of freezing behind this header.
        // Edge-triggered for the reason given on `OpenFanBackend::force_all_with_floor`.
        if stalled {
            if !self.stall_logged {
                self.stall_logged = true;
                log::error!(
                    "THERMAL SAFETY: hwmon force_all_with_floor still in flight after {}s — \
                     the emergency write has not reached the header yet",
                    WRITE_JOIN_BUDGET.as_secs()
                );
            }
        } else if self.stall_logged {
            // DEC-298: the falling edge — see `OpenFanBackend::force_all_with_floor`.
            self.stall_logged = false;
            log::warn!("THERMAL SAFETY: hwmon force_all_with_floor writes are landing again");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::HwmonError;
    use crate::hwmon::lease::LeaseManager;
    use crate::hwmon::pwm_control::{HwmonPwmController, SysfsWriter};
    use crate::hwmon::pwm_discovery::PwmHeaderDescriptor;

    // ── DEC-289: bounded write joins ────────────────────────────────
    //
    // The wedge is a real FIFO, not a sleep and not a held mutex. Opening a FIFO
    // for WRITE blocks in `open(2)` until a reader appears, which is the same
    // kind of uncancellable kernel block a sysfs write hits — the DEC-278 model.
    // A sleep would prove nothing here: the defect being guarded is precisely
    // that `spawn_blocking` work cannot be cancelled, and a sleep ends by itself.

    /// Create a FIFO on a path that OUTLIVES a panicking test.
    ///
    /// Deliberately NOT inside a `TempDir`. A failing assertion unwinds and drops
    /// the `TempDir`, which unlinks the FIFO — and a writer already blocked in
    /// `open(2)` can then never be paired with a reader, because the name it
    /// would be reached by is gone. The release backstop below is powerless at
    /// that point and the red test becomes a hung one. Measured, not theorised:
    /// that is exactly what a validity check of these tests did before this
    /// changed. The happy path unlinks it; a panicking run leaves one empty FIFO
    /// in the temp dir, which is the cheaper failure.
    fn make_fifo(tag: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("ofc-dec289-{tag}-{}.fifo", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(
            unsafe { libc::mkfifo(c.as_ptr(), 0o600) },
            0,
            "mkfifo failed"
        );
        path
    }

    /// Unconditional self-release backstop (DEC-272 trap 3).
    ///
    /// Dropping a tokio runtime blocks until its blocking tasks finish, and a
    /// failed assertion skips the test's own cleanup — so an unbounded wedge
    /// turns a RED test into a HUNG CI job. Deliberately not joined: it exists
    /// for the assertion-failure path, and the happy path releases sooner.
    fn release_backstop(path: std::path::PathBuf) {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(10));
            // Opened REPEATEDLY, not once. A reader open releases the writers
            // blocked at that instant and then closes again, so a single open
            // cannot free a pile-up. Found the hard way: bypassing the fix to
            // validate this test spawns one wedged writer per call, and a
            // one-shot backstop turned that red into a 90-second hang — the
            // DEC-272 trap 3 this backstop exists to prevent, in the harness
            // itself.
            for _ in 0..40 {
                let _ = std::fs::File::open(&path);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
    }

    fn wedge_on(path: std::path::PathBuf) -> impl FnOnce() + Send + 'static {
        move || {
            let _ = std::fs::OpenOptions::new().write(true).open(&path);
        }
    }

    /// The whole point: a write wedged in the kernel must not hold the caller.
    /// Before DEC-289 this join was unconditional, so the engine loop froze — and
    /// with it the thermal `force_all_with_floor`, on every backend, not just the stuck one.
    #[tokio::test]
    async fn a_wedged_write_releases_the_caller_instead_of_freezing_it() {
        let fifo = make_fifo("release");
        release_backstop(fifo.clone());

        let mut bw: BoundedWrite<()> = BoundedWrite::default();
        let started = std::time::Instant::now();
        let out = bw
            .run(
                std::time::Duration::from_millis(200),
                wedge_on(fifo.clone()),
            )
            .await;
        let waited = started.elapsed();

        assert!(
            out.in_flight,
            "a wedged write must report as still in flight"
        );
        assert_eq!(
            out.completed().count(),
            0,
            "nothing completed, so nothing may be routed as an outcome"
        );
        assert!(
            bw.outstanding(),
            "the handle must be retained for the next tick"
        );
        assert!(
            waited < std::time::Duration::from_secs(5),
            "the caller waited {waited:?} — the join is not bounded"
        );
        let _reader = std::fs::File::open(&fifo).unwrap();
        let _ = std::fs::remove_file(&fifo);
    }

    /// The trap this fix could easily have introduced. `spawn_blocking` cannot be
    /// cancelled, so re-issuing the write each tick against a wedged device would
    /// strand one blocking thread per tick and exhaust tokio's 512-thread pool in
    /// ~8.5 minutes — starving the very writer the bound exists to protect
    /// (DEC-272). The handle must be HELD and RE-AWAITED.
    #[tokio::test]
    async fn a_wedged_write_is_held_and_re_awaited_never_re_spawned() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fifo = make_fifo("respawn");
        release_backstop(fifo.clone());

        let spawned = Arc::new(AtomicUsize::new(0));
        let mut bw: BoundedWrite<()> = BoundedWrite::default();
        let budget = std::time::Duration::from_millis(150);

        for _ in 0..4 {
            let n = spawned.clone();
            let f = fifo.clone();
            let out = bw
                .run(budget, move || {
                    n.fetch_add(1, Ordering::SeqCst);
                    let _ = std::fs::OpenOptions::new().write(true).open(&f);
                })
                .await;
            assert!(out.in_flight, "the wedge should still be outstanding");
        }

        assert_eq!(
            spawned.load(Ordering::SeqCst),
            1,
            "the write was RE-SPAWNED once per call — this is the DEC-272 \
             thread-leak trap, and it would exhaust the blocking pool"
        );
        let _reader = std::fs::File::open(&fifo).unwrap();
        let _ = std::fs::remove_file(&fifo);
    }

    /// DEC-298, and the whole point of it: harvesting a pending write must not
    /// swallow the write this tick wanted to make.
    ///
    /// Before this, `run` took the pending handle and **dropped `f` unread** even
    /// when that handle completed immediately. So a tick that cleared a slow
    /// `apply` issued nothing of its own — and if that tick was the one where the
    /// CPU crossed the trip point, the first forced write was not issued until the tick
    /// after. `ran == 2` is the discriminator: before the fix it was 1.
    #[tokio::test]
    async fn a_harvested_write_still_issues_this_ticks_own_write() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let ran = Arc::new(AtomicUsize::new(0));
        let mut bw: BoundedWrite<u8> = BoundedWrite::default();

        // Tick N: a write slower than its budget, so it is left pending.
        let r1 = ran.clone();
        let first = bw
            .run(std::time::Duration::from_millis(20), move || {
                std::thread::sleep(std::time::Duration::from_millis(120));
                r1.fetch_add(1, Ordering::SeqCst);
                1u8
            })
            .await;
        assert!(
            first.in_flight,
            "fixture check: the first write must outlast its budget, or this test \
             never exercises the harvest path"
        );
        assert_eq!(first.completed().count(), 0);

        // Let it finish, so the next call HARVESTS rather than timing out again.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        // Tick N+1: the harvest succeeds — and this tick's own write must also be
        // issued, in the same call.
        let r2 = ran.clone();
        let second = bw
            .run(std::time::Duration::from_millis(500), move || {
                r2.fetch_add(1, Ordering::SeqCst);
                2u8
            })
            .await;

        assert!(!second.in_flight, "both writes completed inside the budget");
        assert_eq!(
            second
                .harvested
                .expect("the pending write must be harvested")
                .unwrap(),
            1
        );
        assert_eq!(
            second
                .issued
                .expect(
                    "this tick's write must ALSO be issued — before DEC-298 it was dropped unread"
                )
                .unwrap(),
            2
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            2,
            "both closures must have actually run"
        );
    }

    /// DEC-298 remediation, and the defect review found in the first cut.
    ///
    /// Because a tick now re-issues immediately after harvesting, `pending` is
    /// never `None` at the point the loop samples it — so reporting raw
    /// outstanding-ness pinned `engine_writes_stalled_since` forever for any
    /// device slower than the budget. `record_engine_write_stall` only clears its
    /// stamp on `false`, so after 30x the tick interval `engine_health` would
    /// return `crit` "writes wedged" for a device writing perfectly well every
    /// 1.5 s — the exact false alarm `staleness.rs` says its 30x threshold was
    /// chosen to avoid.
    ///
    /// "Stalled" therefore means *nothing completed*, not *something is in
    /// flight*.
    #[tokio::test]
    async fn a_slow_but_completing_write_clears_the_stall_flag_periodically() {
        let mut bw: BoundedWrite<u8> = BoundedWrite::default();
        let budget = std::time::Duration::from_millis(60);
        // 1.5x the budget — the reviewer's degraded-link case. Writes DO land,
        // just not within one budget, so harvests succeed on alternate calls.
        let slow = || {
            std::thread::sleep(std::time::Duration::from_millis(90));
            7u8
        };

        let mut harvests = 0;
        let mut clears = 0;
        for _ in 0..8 {
            let progress = bw.run(budget, slow).await;
            if progress.harvested.is_some() {
                harvests += 1;
            }
            if !bw.stalled() {
                clears += 1;
            }
        }

        assert!(
            harvests >= 2,
            "fixture check: writes must actually be landing ({harvests} harvests), \
             or this test is measuring a wedge"
        );
        // The property that matters is not "never stalled" — it is that the flag
        // CLEARS, because `record_engine_write_stall` only resets its stamp on
        // `false`. A flag that never clears accumulates to the 30x `crit`
        // "writes wedged" on a device that is writing fine.
        assert!(
            clears >= 2,
            "the stall flag never cleared ({clears} clears in 8 ticks) — \
             engine_writes_stalled_since would accumulate to a false `crit`"
        );
    }

    /// DEC-298: a genuinely wedged device must still report stalled, or the fix
    /// for the false positive would have created a false negative.
    #[tokio::test]
    async fn a_wedged_write_is_stalled_from_the_first_tick() {
        let fifo = make_fifo("stalled");
        release_backstop(fifo.clone());

        let mut bw: BoundedWrite<()> = BoundedWrite::default();
        for tick in 0..3 {
            let out = bw
                .run(std::time::Duration::from_millis(80), wedge_on(fifo.clone()))
                .await;
            assert!(out.in_flight, "tick {tick}");
            assert!(
                bw.stalled(),
                "tick {tick}: nothing has ever completed — this is what wedged means"
            );
        }
        let _reader = std::fs::File::open(&fifo).unwrap();
        let _ = std::fs::remove_file(&fifo);
    }

    /// DEC-298: the harvest and the newly issued write share ONE budget, so a
    /// harvest that consumes it all leaves no time to wait — but the write is
    /// still ISSUED, which is what the fix is about. `spawn_blocking` starts
    /// running the moment it is spawned; waiting for it is a separate question.
    ///
    /// Also pins that `run` stays within its budget, i.e. the tick cost did not
    /// double when this gained a second phase.
    #[tokio::test]
    async fn a_harvest_that_eats_the_budget_still_issues_the_write_within_budget() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let issued = Arc::new(AtomicBool::new(false));
        let mut bw: BoundedWrite<u8> = BoundedWrite::default();

        // Leave a write pending that will take ~150 ms to finish.
        let first = bw
            .run(std::time::Duration::from_millis(10), || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                1u8
            })
            .await;
        assert!(first.in_flight, "fixture check: must be left pending");

        // Budget generous enough to harvest but nearly all consumed doing so.
        let flag = issued.clone();
        let started = std::time::Instant::now();
        let budget = std::time::Duration::from_millis(300);
        let second = bw
            .run(budget, move || {
                flag.store(true, Ordering::SeqCst);
                2u8
            })
            .await;
        let elapsed = started.elapsed();

        assert!(
            second.harvested.is_some(),
            "the pending write should have been harvested"
        );
        assert!(
            elapsed <= budget + std::time::Duration::from_millis(150),
            "run took {elapsed:?} against a {budget:?} budget — the harvest and the \
             issued write must SHARE one budget, not take one each"
        );

        // Whether it completed inside the residual is timing-dependent; that it
        // was issued at all is not. Poll rather than assume.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !issued.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            issued.load(Ordering::SeqCst),
            "this tick's write was never issued — it was dropped, which is the defect"
        );
    }

    /// The bound must not break the ordinary path: a write that completes inside
    /// its budget returns its value and leaves nothing outstanding.
    #[tokio::test]
    async fn a_write_that_completes_reports_its_result_and_clears_outstanding() {
        let mut bw: BoundedWrite<u8> = BoundedWrite::default();
        let out = bw.run(std::time::Duration::from_secs(5), || 42u8).await;
        assert!(!out.in_flight, "a fast write must not report as in flight");
        assert!(out.harvested.is_none(), "nothing was pending to harvest");
        assert_eq!(
            out.issued
                .expect("a fast write must report as completed")
                .unwrap(),
            42
        );
        assert!(!bw.outstanding(), "nothing should remain outstanding");
    }

    fn cmd(member_id: &str, source: &str, pct: u8) -> PwmCommand {
        PwmCommand {
            member_id: member_id.into(),
            source: source.into(),
            pwm_percent: pct,
            gpu_fan_zero_rpm: false,
        }
    }

    // ── GPU backend ──────────────────────────────────────────────────

    fn fake_gpu(
        dir: &tempfile::TempDir,
    ) -> (crate::hwmon::gpu_detect::AmdGpuInfo, std::path::PathBuf) {
        let curve_path = dir.path().join("fan_curve");
        std::fs::write(&curve_path, "").unwrap();
        let gpu = crate::hwmon::gpu_detect::AmdGpuInfo {
            pci_bdf: "0000:03:00.0".into(),
            pci_device_id: 0x7550,
            pci_revision: 0xC0,
            pci_class: 0x030000,
            marketing_name: Some("RX 9070 XT".into()),
            hwmon_path: dir.path().to_path_buf(),
            fan_curve_path: Some(curve_path.clone()),
            fan_zero_rpm_path: None,
            is_discrete: true,
            has_fan_rpm: false,
            has_pwm: false,
            has_pwm_enable: false,
            overdrive_enabled: true,
        };
        (gpu, curve_path)
    }

    /// DEC-299 (AUD-a2): the last unbounded write join.
    ///
    /// `GpuBackend::apply` used to `spawn_blocking(...).await` per fan with no
    /// bound. A PMFW write wedged in the driver therefore meant `apply` never
    /// returned, the engine loop never reached `tick.tick()`, and the thermal
    /// `force_all_with_floor` never ran again — while the task stayed ALIVE, so DEC-266's
    /// death supervision never fired either.
    ///
    /// The wedge is a FIFO `fan_curve` with no reader: `set_static_speed` opens
    /// it for writing and blocks in `open(2)`. A `thread::sleep` would not fail
    /// the same way, which is the DEC-278 lesson.
    ///
    /// Asserts the caller is RELEASED, not that the write succeeded — the write
    /// is still stuck, and that is the point: the loop gets its thread back.
    #[tokio::test]
    async fn a_wedged_gpu_write_releases_the_caller_instead_of_freezing_it() {
        let fifo = make_fifo("gpu-wedge");
        // Detached releaser, opened READ+WRITE, starting after the assertions have
        // seen the wedge.
        //
        // `O_RDWR`, not the reader-only `release_backstop` next door, and the
        // difference is the whole reason this test took three attempts:
        // `set_static_speed_with_zero_rpm` calls `read_fan_curve` FIRST, so the
        // wedge is a blocked *reader*, and a reader-only releaser can never pair
        // with it. Opening a FIFO `O_RDWR` never blocks and frees either side.
        // Nothing opens it from the test's own thread — that blocks forever once
        // the other party has gone.
        {
            let f = fifo.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(3));
                for _ in 0..60 {
                    let _ = std::fs::OpenOptions::new().read(true).write(true).open(&f);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                // Removed HERE, not in the test body. The body finishes in ~1 s
                // while the wedged task is still blocked, and deleting the FIFO
                // then leaves this releaser opening a path that no longer exists
                // — the wedged reader waits forever and the runtime DROP blocks,
                // which is DEC-272 trap 3 arriving through the cleanup.
                let _ = std::fs::remove_file(&f);
            });
        }

        let dir = tempfile::tempdir().unwrap();
        let (mut gpu, _) = fake_gpu(&dir);
        gpu.fan_curve_path = Some(fifo.clone());

        let cache = Arc::new(StateCache::new());
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:03:00.0", 20);
        let mut be = GpuBackend::new(cache, Arc::new(vec![gpu]));

        let started = std::time::Instant::now();
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 90)])
            .await;
        let waited = started.elapsed();

        // Threshold BELOW the releaser's 3 s delay, deliberately. The bound
        // allows at most GPU_RESET_LOCK_WAIT (750 ms) + GPU_WRITE_JOIN_BUDGET
        // (1 s). A looser bound would let the UNBOUNDED version pass too — it
        // would simply block until the releaser freed it at 3 s — so the test
        // would prove nothing. Verified by bypassing the fix.
        assert!(
            waited < std::time::Duration::from_millis(2500),
            "apply() waited {waited:?} on a wedged GPU write — the join is not \
             bounded, and the engine loop is frozen behind it"
        );
        assert!(
            be.writes_stalled(),
            "a wedged GPU write must be reported as stalled, or /status shows a \
             healthy daemon while the fan holds its last duty"
        );
    }

    /// DEC-299 remediation (review F1): a permanently wedged GPU write must keep
    /// reporting stalled, including on the ticks that never reach `run`.
    ///
    /// The lock-unavailable early return is taken *precisely* when a wedged write
    /// is holding the lock — and it used to return without refreshing the
    /// progress flag. So once a tick had both harvested an old result and wedged
    /// a new one (`completed_last_run = true`), every later tick skipped on the
    /// lock and the flag was never cleared: `stalled()` stayed **false** forever,
    /// which *clears* `engine_writes_stalled_since` and shows a healthy engine
    /// while the fan holds its last duty. A false negative on the safety report,
    /// which is the mirror image of the false `crit` DEC-298 removed.
    ///
    /// Two release windows are needed to reach that state: the first lets a write
    /// complete so the next `apply` HARVESTS it (setting the flag true) while
    /// issuing a fresh write that wedges.
    ///
    /// The first cut of this test did NOT discriminate the fix — it passed with
    /// `note_no_progress` removed — and the reason was not the one the register
    /// row guessed (register row 299-a). It was not the lock: ticks 3-5 never
    /// reached the lock at all. They re-commanded the SAME duty that tick 1 had
    /// already failed, so the `GPU_FAIL_COOLDOWN` suppression at the top of
    /// `apply` emptied `pending_writes` and the tick returned through the
    /// `is_empty()` -> `harvest_only` arm — which sets `completed_last_run =
    /// false` on its own timeout, making `stalled()` true for a reason that has
    /// nothing to do with the branch under test.
    ///
    /// Two things fix that, and both are load-bearing:
    ///
    /// * Ticks 3-5 command a duty that escapes BOTH suppressors — different from
    ///   the cached failure (90) and at least `GPU_COALESCE_DELTA_PCT` from the
    ///   last commanded value (20). The wedged tick-2 task still owns
    ///   `lock_gpu_writes` via the guard moved into its closure, so the lock
    ///   genuinely is unavailable and the tick genuinely does skip on it.
    /// * `took_lock_skip()` is asserted inside the loop, so the test proves it
    ///   took the lock-skipped path rather than merely reaching the outcome. An
    ///   outcome assertion alone is what let the first cut pass down a different
    ///   path — the same trap as asserting a payload instead of the signal that
    ///   produced it.
    #[tokio::test]
    async fn a_wedged_gpu_write_keeps_reporting_stalled_on_lock_skipped_ticks() {
        let fifo = make_fifo("gpu-stall-persist");
        {
            let f = fifo.clone();
            std::thread::spawn(move || {
                // Window 1: release the first wedge so it can be harvested.
                std::thread::sleep(std::time::Duration::from_millis(2200));
                for _ in 0..10 {
                    let _ = std::fs::OpenOptions::new().read(true).write(true).open(&f);
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
                // Gap: the write issued next must wedge.
                std::thread::sleep(std::time::Duration::from_secs(9));
                // Window 2: cleanup only, so the runtime drop is not blocked.
                for _ in 0..60 {
                    let _ = std::fs::OpenOptions::new().read(true).write(true).open(&f);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let _ = std::fs::remove_file(&f);
            });
        }

        let dir = tempfile::tempdir().unwrap();
        let (mut gpu, _) = fake_gpu(&dir);
        gpu.fan_curve_path = Some(fifo.clone());
        let cache = Arc::new(StateCache::new());
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        // Tick 1: wedges. Nothing has completed, so this is honestly stalled.
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:03:00.0", 20);
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 90)])
            .await;
        assert!(be.writes_stalled(), "tick 1: nothing completed");

        // Let release window 1 free that write.
        tokio::time::sleep(std::time::Duration::from_millis(2800)).await;

        // Tick 2: harvests the completed write AND issues one that wedges. The
        // harvest legitimately sets "progress made" for this tick.
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 30)])
            .await;
        // FIXTURE CHECK, and the reason the first cut of this test was vacuous:
        // if tick 2 did not actually harvest, `completed_last_run` stays false
        // and every later tick reports stalled for the ordinary reason — so the
        // test passes with the fix removed and proves nothing. Assert the state
        // the test is ABOUT before asserting what must follow from it.
        assert!(
            !be.writes_stalled(),
            "fixture check: tick 2 must have harvested a completed write (which is \
             what sets the progress flag) AND wedged a new one; it did not, so the \
             ticks below would pass for the wrong reason"
        );

        // Tick 3+: the wedged write holds the lock, so these never reach `run`.
        //
        // 50, not 90: 90 is what tick 1 failed at, so re-commanding it hits the
        // 60 s `GPU_FAIL_COOLDOWN` and the tick never gets as far as the lock.
        // 50 differs from the cached failure AND clears `GPU_COALESCE_DELTA_PCT`
        // against the last commanded 20, so a write is genuinely pending and the
        // lock is genuinely what stops it.
        for tick in 3..=5 {
            be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 50)])
                .await;
            // Prove the PATH, not just the outcome — see the doc comment.
            assert!(
                be.took_lock_skip(),
                "fixture check, tick {tick}: this tick must have returned through \
                 the lock-skipped branch (a wedged write holds the lock). It did \
                 not, so the stall assertion below would pass down some other \
                 path and prove nothing about the fix"
            );
            assert!(
                be.writes_stalled(),
                "tick {tick}: a wedged GPU write reported NOT stalled — \
                 engine_writes_stalled_since would be cleared and /status would \
                 show a healthy engine while the fan holds its last duty"
            );
        }
    }

    /// DEC-299: the second blocking window, which the register row did not name.
    ///
    /// `lock_gpu_writes()` was itself an unbounded `.await` on the mutex, and a
    /// GPU verify holds that lock for its whole multi-second window — so a verify
    /// starting after the per-fan `verify_active()` check froze the loop with NO
    /// wedged device at all. The lock is now taken with a bounded wait and the
    /// GPU leg is skipped for that tick.
    #[tokio::test]
    async fn a_held_gpu_write_lock_skips_the_tick_instead_of_blocking_the_loop() {
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = Arc::new(StateCache::new());
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:03:00.0", 20);
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        // Someone else — a verify, a reset — holds the write lock.
        let _held = cache.lock_gpu_writes().await;

        let started = std::time::Instant::now();
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 90)])
            .await;
        let waited = started.elapsed();

        assert!(
            waited < constants::GPU_RESET_LOCK_WAIT + std::time::Duration::from_secs(2),
            "apply() waited {waited:?} for the GPU write lock — an unbounded wait \
             here freezes the loop for the whole verify window"
        );
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "the tick must be SKIPPED while the lock is held, not written anyway"
        );
    }

    #[tokio::test]
    async fn gpu_backend_suppresses_below_threshold_and_writes_at_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = Arc::new(StateCache::new());
        cache.set_gpu_fan_commanded_pct("amd_gpu:0000:03:00.0", 60);
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        // delta 4 < 5 → suppressed (DEC-131).
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 64)])
            .await;
        assert!(std::fs::read_to_string(&curve_path).unwrap().is_empty());

        // delta 5 ≥ 5 → written, cache updated.
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 65)])
            .await;
        assert!(!std::fs::read_to_string(&curve_path).unwrap().is_empty());
        assert_eq!(
            cache
                .gpu_fans_snapshot()
                .get("amd_gpu:0000:03:00.0")
                .and_then(|f| f.last_commanded_pct),
            Some(65)
        );
    }

    #[tokio::test]
    async fn gpu_backend_caches_failures_and_clears_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let (mut gpu, curve_path) = fake_gpu(&dir);
        // Point the curve at a non-existent directory so the write fails.
        let bad_path = dir.path().join("missing").join("fan_curve");
        gpu.fan_curve_path = Some(bad_path);
        let cache = Arc::new(StateCache::new());
        let mut be = GpuBackend::new(cache, Arc::new(vec![gpu.clone()]));

        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert_eq!(be.fail_cache_len(), 1, "failed write must be cached");

        // Same speed within the cooldown → suppressed (no second attempt
        // visible; the cache entry persists).
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert_eq!(be.fail_cache_len(), 1);

        // Repair the path via a fresh backend pointing at the good file —
        // a successful write clears the failure cache.
        let mut gpu_ok = gpu;
        gpu_ok.fan_curve_path = Some(curve_path);
        let cache = Arc::new(StateCache::new());
        let mut be = GpuBackend::new(cache, Arc::new(vec![gpu_ok]));
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert_eq!(be.fail_cache_len(), 0);
    }

    #[tokio::test]
    async fn gpu_backend_fail_cooldown_is_clock_gated() {
        // P3-7: with an injectable clock the 60 s GPU_FAIL_COOLDOWN is testable
        // deterministically. A failed write is retried only after the cooldown
        // elapses on the daemon's clock — advanced here instead of sleeping.
        use std::sync::atomic::{AtomicU64, Ordering};
        struct AdvanceClock {
            base: std::time::Instant,
            offset_ms: AtomicU64,
        }
        impl Clock for AdvanceClock {
            fn now(&self) -> std::time::Instant {
                self.base + std::time::Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
            }
        }
        let clock = Arc::new(AdvanceClock {
            base: std::time::Instant::now(),
            offset_ms: AtomicU64::new(0),
        });

        let dir = tempfile::tempdir().unwrap();
        let (mut gpu, _good) = fake_gpu(&dir);
        // Curve path under a not-yet-existing subdir → the first write fails.
        let sub = dir.path().join("sub");
        let curve_path = sub.join("fan_curve");
        gpu.fan_curve_path = Some(curve_path.clone());
        let cache = Arc::new(StateCache::new());
        let mut be = GpuBackend::with_clock(cache, Arc::new(vec![gpu]), clock.clone());

        // t=0: write fails (parent dir missing) → cached.
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert_eq!(be.fail_cache_len(), 1, "failed write must be cached");

        // Make the path writable so a *retry* could succeed.
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(&curve_path, "").unwrap();

        // Within the cooldown: same speed suppressed — no retry, file stays empty.
        clock.offset_ms.store(
            (constants::GPU_FAIL_COOLDOWN / 2).as_millis() as u64,
            Ordering::SeqCst,
        );
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "within cooldown the failed speed must not be retried"
        );
        assert_eq!(be.fail_cache_len(), 1);

        // Past the cooldown: the retry fires, succeeds, and clears the cache.
        clock.offset_ms.store(
            (constants::GPU_FAIL_COOLDOWN + std::time::Duration::from_secs(1)).as_millis() as u64,
            Ordering::SeqCst,
        );
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            !std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "after the cooldown the failed speed must be retried"
        );
        assert_eq!(be.fail_cache_len(), 0, "successful retry clears the cache");
    }

    // ── hwmon backend ────────────────────────────────────────────────

    type WriteLog = Arc<Mutex<Vec<(String, String)>>>;

    struct TestWriter {
        writes: WriteLog,
    }

    impl SysfsWriter for TestWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes.lock().push((path.into(), value.into()));
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, HwmonError> {
            Ok("128\n".into())
        }
    }

    fn make_header(id: &str) -> PwmHeaderDescriptor {
        PwmHeaderDescriptor {
            id: id.to_string(),
            label: "CHA_FAN1".to_string(),
            chip_name: "it8696".to_string(),
            device_id: "it87.2624".to_string(),
            pwm_index: 1,
            supports_enable: true,
            pwm_path: "/sys/class/hwmon/hwmon0/pwm1".to_string(),
            enable_path: Some("/sys/class/hwmon/hwmon0/pwm1_enable".to_string()),
            rpm_available: false,
            rpm_path: None,
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: None,
            is_aio: false,
            role: crate::hwmon::roles::HeaderRole::Unknown,
            role_source: crate::hwmon::roles::RoleSource::None,
        }
    }

    /// A writer that WEDGES on its first write by blocking in `open(2)` on a
    /// FIFO — the DEC-278 model. Counts entries so a test can prove how many
    /// blocking tasks were actually started.
    struct WedgingWriter {
        fifo: std::path::PathBuf,
        entered: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl SysfsWriter for WedgingWriter {
        fn write_file(&mut self, _path: &str, _value: &str) -> Result<(), HwmonError> {
            self.entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = std::fs::OpenOptions::new().write(true).open(&self.fifo);
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, HwmonError> {
            Ok("1".into())
        }
    }

    /// DEC-289: `apply` and `force_all_with_floor` deliberately SHARE one `BoundedWrite` per
    /// backend. They never run in the same tick, but they do run in consecutive
    /// ticks, and a `force_all_with_floor` still wedged when an emergency clears must not let
    /// the next `apply` start a SECOND uncancellable blocking task on the same
    /// device — that is the DEC-272 thread-leak trap.
    ///
    /// Without this test, a plausible "cleanup" giving `force_all_with_floor` its own field
    /// would reintroduce that trap and every other test would stay green.
    #[tokio::test]
    async fn force_all_and_apply_share_one_bounded_write_so_a_wedge_starts_one_task() {
        let fifo = make_fifo("shared");
        release_backstop(fifo.clone());
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(
            vec![make_header("hwmon:t:d:pwm1")],
            LeaseManager::new(),
            Box::new(WedgingWriter {
                fifo: fifo.clone(),
                entered: entered.clone(),
            }),
            cache,
        );
        let mut be = HwmonBackend::new(Arc::new(Mutex::new(ctrl)));

        // Tick 1: an emergency force_all_with_floor wedges.
        be.force_all_with_floor(100, &[]).await;
        // DEC-298: `writes_stalled` rather than the old `writes_outstanding` —
        // a STRONGER assertion for this test's intent. "Outstanding" is merely
        // "something is in flight"; "stalled" additionally requires that nothing
        // completed, which is what "wedged" means here.
        assert!(
            be.writes_stalled(),
            "the force_all_with_floor should be wedged"
        );

        // Tick 2: the emergency clears and ordinary control resumes.
        be.apply(&[cmd("hwmon:t:d:pwm1", "hwmon", 40)]).await;

        assert_eq!(
            entered.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a second blocking task was started against a device already wedged — \
             apply and force_all_with_floor are no longer sharing one BoundedWrite, which is \
             the DEC-272 thread-leak trap"
        );
        let _reader = std::fs::File::open(&fifo).unwrap();
        let _ = std::fs::remove_file(&fifo);
    }

    /// DEC-289: `outstanding()` must mean "still running", not "handle held".
    /// Several loop paths skip the write phase entirely (no profile loaded, a
    /// verify in progress), and nothing harvests the handle there — so reporting
    /// a finished write as outstanding would pin `/status` at "writes wedged"
    /// until the next activation or a restart.
    #[tokio::test]
    async fn a_finished_write_is_not_outstanding_even_before_it_is_harvested() {
        let mut bw: BoundedWrite<u8> = BoundedWrite::default();
        // Wedge just long enough to time out, then let it finish on its own.
        bw.run(std::time::Duration::from_millis(1), || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            7u8
        })
        .await;
        assert!(bw.outstanding(), "it really was still running at first");

        // Give the blocking task time to finish. Nothing harvests it: this models
        // a tick that skipped the write phase entirely.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !bw.outstanding(),
            "a FINISHED write still reports as outstanding — /status would sit at \
             'writes wedged' for a device that recovered"
        );
    }

    /// DEC-289: a write in flight at shutdown must be drained, not detached.
    /// `main.rs` drains the engine task handle and relies on that also draining
    /// backend writes; a detached write still holds the controller lock and would
    /// make the hardware restore fall back and then be overwritten.
    #[tokio::test]
    async fn shutdown_drains_an_in_flight_write_rather_than_detaching_it() {
        let mut bw: BoundedWrite<u8> = BoundedWrite::default();
        bw.run(std::time::Duration::from_millis(1), || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            3u8
        })
        .await;
        assert!(bw.outstanding());

        bw.drain(std::time::Duration::from_secs(5)).await;
        assert!(
            !bw.outstanding(),
            "drain left a write in flight — the restore would race it"
        );
    }

    fn hwmon_backend(headers: Vec<PwmHeaderDescriptor>) -> (HwmonBackend, WriteLog) {
        let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
        let writer = TestWriter {
            writes: writes.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(headers, LeaseManager::new(), Box::new(writer), cache);
        (HwmonBackend::new(Arc::new(Mutex::new(ctrl))), writes)
    }

    /// Like `TestWriter`, but reports `pwm_enable` as already manual (`1`) so the
    /// per-write readback watchdog in `set_pwm` does NOT fire. This isolates the
    /// thermal force-take coalescing reset (audit P1-E) from the watchdog, which
    /// would otherwise re-assert manual mode regardless.
    struct EnableManualWriter {
        writes: WriteLog,
    }

    impl SysfsWriter for EnableManualWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes.lock().push((path.into(), value.into()));
            Ok(())
        }
        fn read_file(&self, path: &str) -> Result<String, HwmonError> {
            if path.ends_with("_enable") {
                Ok("1\n".into())
            } else {
                Ok("128\n".into())
            }
        }
    }

    /// Like `make_header`, but at a distinct sysfs path per `index` so each
    /// header's write is independently observable in the write log.
    fn make_header_idx(id: &str, index: u8) -> PwmHeaderDescriptor {
        PwmHeaderDescriptor {
            id: id.to_string(),
            label: "CHA_FAN1".to_string(),
            chip_name: "it8696".to_string(),
            device_id: "it87.2624".to_string(),
            pwm_index: index,
            supports_enable: true,
            pwm_path: format!("/sys/class/hwmon/hwmon0/pwm{index}"),
            enable_path: Some(format!("/sys/class/hwmon/hwmon0/pwm{index}_enable")),
            rpm_available: false,
            rpm_path: None,
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: None,
            is_aio: false,
            role: crate::hwmon::roles::HeaderRole::Unknown,
            role_source: crate::hwmon::roles::RoleSource::None,
        }
    }

    /// A [`SysfsWriter`] that fails writes whose path contains `fail_fragment`
    /// with EROFS ("Read-only file system", os error 30) — the DEC-199 sandbox
    /// carve-out symptom. `fail_fragment` is swappable at runtime so a test can
    /// "repair" the sandbox and observe recovery, and is path-selective so one
    /// stuck header can fail while a sibling still writes.
    struct CarveoutFailWriter {
        writes: WriteLog,
        fail_fragment: Arc<Mutex<Option<String>>>,
    }

    impl SysfsWriter for CarveoutFailWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            let fails = {
                let guard = self.fail_fragment.lock();
                guard.as_deref().is_some_and(|frag| path.contains(frag))
            };
            if fails {
                return Err(HwmonError::WriteError {
                    path: path.into(),
                    message: "Read-only file system (os error 30)".into(),
                });
            }
            self.writes.lock().push((path.into(), value.into()));
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, HwmonError> {
            Ok("128\n".into())
        }
    }

    fn hwmon_backend_carveout(
        headers: Vec<PwmHeaderDescriptor>,
        fail_fragment: Option<&str>,
    ) -> (HwmonBackend, Arc<Mutex<Option<String>>>) {
        let frag = Arc::new(Mutex::new(fail_fragment.map(String::from)));
        let writer = CarveoutFailWriter {
            writes: Arc::new(Mutex::new(Vec::new())),
            fail_fragment: frag.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(headers, LeaseManager::new(), Box::new(writer), cache);
        (HwmonBackend::new(Arc::new(Mutex::new(ctrl))), frag)
    }

    #[tokio::test]
    async fn hwmon_apply_writes_every_header_in_batch() {
        // DEC-154 per-command locking must still write EVERY header in a batch
        // (none dropped by the per-header re-lock) and auto-acquire the lease.
        let (mut be, writes) = hwmon_backend(vec![
            make_header_idx("hwmon:it8696:pwm1", 1),
            make_header_idx("hwmon:it8696:pwm2", 2),
            make_header_idx("hwmon:it8696:pwm3", 3),
        ]);

        be.apply(&[
            cmd("hwmon:it8696:pwm1", "hwmon", 40),
            cmd("hwmon:it8696:pwm2", "hwmon", 55),
            cmd("hwmon:it8696:pwm3", "hwmon", 70),
        ])
        .await;

        let w = writes.lock();
        for pwm in ["pwm1", "pwm2", "pwm3"] {
            assert!(
                w.iter().any(|(p, _)| p.ends_with(pwm)),
                "per-command apply must write header {pwm}; got {w:?}"
            );
        }
        drop(w);

        // The profile engine auto-acquired the lease and still holds it.
        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert!(
            lease.is_some_and(|l| l.owner == HwmonWriter::Engine),
            "profile-engine lease must be active after apply"
        );
    }

    #[tokio::test]
    async fn force_all_reasserts_manual_mode_after_engine_write() {
        // Engine controls a header first (manual_mode_set = true), then thermal
        // safety force-takes the lease. The readback watchdog is held off (enable
        // reads back as 1), so WITHOUT the P1-E reset the stale manual_mode_set
        // would make thermal safety SKIP the pwm_enable write. With the reset it
        // must re-assert pwm_enable=1 on the forced write.
        let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
        let writer = EnableManualWriter {
            writes: writes.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(
            vec![make_header("hwmon:it8696:pwm1")],
            LeaseManager::new(),
            Box::new(writer),
            cache,
        );
        let mut be = HwmonBackend::new(Arc::new(Mutex::new(ctrl)));

        // 1. Engine controls the header → first pwm_enable=1 write.
        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 40)]).await;
        // 2. Thermal safety force-takes the lease and forces 100%.
        be.force_all_with_floor(100, &[]).await;

        let enable_writes = writes
            .lock()
            .iter()
            .filter(|(p, v)| p.ends_with("_enable") && v.trim() == "1")
            .count();
        assert_eq!(
            enable_writes, 2,
            "thermal force-take must reset coalescing so pwm_enable=1 is \
             re-asserted on the forced write (audit P1-E)"
        );
    }

    #[tokio::test]
    async fn hwmon_backend_auto_leases_and_writes() {
        let (mut be, writes) = hwmon_backend(vec![make_header("hwmon:it8696:pwm1")]);

        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;

        let w = writes.lock();
        assert!(
            w.iter().any(|(p, _)| p.ends_with("pwm1")),
            "expected a pwm write after auto-lease; got {w:?}"
        );
        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(lease.map(|l| l.owner), Some(HwmonWriter::Engine));
    }

    #[tokio::test]
    async fn hwmon_apply_skips_read_only_header() {
        // DEC-102 engine-path backstop: a read-only header (is_writable=false)
        // must be skipped, never EACCES-spammed; a writable sibling still writes.
        let mut ro = make_header_idx("hwmon:it8696:pwm1", 1);
        ro.is_writable = false;
        let rw = make_header_idx("hwmon:it8696:pwm2", 2);
        let (mut be, writes) = hwmon_backend(vec![ro, rw]);

        be.apply(&[
            cmd("hwmon:it8696:pwm1", "hwmon", 40),
            cmd("hwmon:it8696:pwm2", "hwmon", 55),
        ])
        .await;

        let w = writes.lock();
        assert!(
            !w.iter().any(|(p, _)| p.ends_with("pwm1")),
            "read-only header must be skipped (no write); got {w:?}"
        );
        assert!(
            w.iter().any(|(p, _)| p.ends_with("pwm2")),
            "writable sibling must still be written; got {w:?}"
        );
        drop(w);
        assert_eq!(
            be.member_failure_streak("hwmon:it8696:pwm1"),
            0,
            "a read-only header is skipped, never counted as a write failure (DEC-199)"
        );
    }

    #[tokio::test]
    async fn hwmon_failure_streak_increments_then_clears_on_recovery() {
        // DEC-199: a persistent hwmon write failure (canonically EROFS from a
        // misconfigured sandbox carve-out) must advance a per-member streak so
        // the log can be throttled, and clear the moment the member writes again.
        let (mut be, frag) =
            hwmon_backend_carveout(vec![make_header("hwmon:it8696:pwm1")], Some("pwm1"));

        for expected in 1..=3 {
            be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;
            assert_eq!(
                be.member_failure_streak("hwmon:it8696:pwm1"),
                expected,
                "each failing tick must advance the member's streak"
            );
        }

        // Sandbox carve-out repaired → the write succeeds → the streak clears.
        *frag.lock() = None;
        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;
        assert_eq!(
            be.member_failure_streak("hwmon:it8696:pwm1"),
            0,
            "a successful write must reset the member's failure streak"
        );
    }

    #[tokio::test]
    async fn hwmon_failure_streak_isolated_per_member() {
        // A single stuck header must not mask a healthy sibling: the failing
        // member accrues a streak while the writable one stays at zero (mirrors
        // the OpenFan per-channel isolation, audit P3-5).
        let (mut be, _frag) = hwmon_backend_carveout(
            vec![
                make_header_idx("hwmon:it8696:pwm1", 1),
                make_header_idx("hwmon:it8696:pwm2", 2),
            ],
            Some("pwm2"),
        );

        be.apply(&[
            cmd("hwmon:it8696:pwm1", "hwmon", 40),
            cmd("hwmon:it8696:pwm2", "hwmon", 55),
        ])
        .await;

        assert_eq!(
            be.member_failure_streak("hwmon:it8696:pwm2"),
            1,
            "the stuck header must accrue a failure streak"
        );
        assert_eq!(
            be.member_failure_streak("hwmon:it8696:pwm1"),
            0,
            "the healthy sibling must stay at zero"
        );
    }

    #[tokio::test]
    async fn gpu_backend_skips_relinquished_fan() {
        // DEC-165: a GPU fan relinquished to firmware-auto via reset must be
        // skipped by the engine so the reset is durable under an active profile.
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = Arc::new(StateCache::new());
        let _ = cache.relinquish_gpu_fan("amd_gpu:0000:03:00.0");
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "engine must not write a relinquished GPU fan (DEC-165)"
        );

        // Clearing the relinquish (e.g. on profile activation) resumes control.
        cache.clear_relinquished_gpu_fans();
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            !std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "engine must resume writing after the relinquish is cleared"
        );
    }

    #[tokio::test]
    async fn hwmon_force_all_takes_thermal_safety_lease_and_writes_every_header() {
        // Distinct pwm paths per header (make_header hardcodes pwm1) so the
        // value assertion below proves EVERY header — not just the first — was
        // driven to 100%.
        let (mut be, writes) = hwmon_backend(vec![header_with_paths(1), header_with_paths(2)]);
        // Even an engine-held lease is force-taken for safety writes.
        be.ctrl
            .lock()
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::Engine);

        be.force_all_with_floor(100, &[]).await;

        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(lease.map(|l| l.owner), Some(HwmonWriter::ThermalSafety));
        let w = writes.lock();
        // Pin the forced VALUE (100% → raw "255"), not just write-presence: a
        // force_all_with_floor that ignored its pct and wrote 40% would else pass. Assert
        // both headers' pwm data write (the pwm{i} path excludes pwm{i}_enable).
        for i in 1..=2 {
            let pwm_path = format!("/sys/class/hwmon/hwmon0/pwm{i}");
            let vals: Vec<_> = w
                .iter()
                .filter(|(p, _)| *p == pwm_path)
                .map(|(_, v)| v.trim())
                .collect();
            assert!(
                !vals.is_empty(),
                "header pwm{i} received no forced write; got {w:?}"
            );
            assert!(
                vals.iter().all(|v| *v == "255"),
                "header pwm{i} must be forced to 100% (raw 255); got {vals:?}"
            );
        }
    }

    /// Writer that signals once (on its first write) so a test can time a
    /// mid-scan lease preemption, then holds the lock long enough that the
    /// parked preemptor crosses parking_lot's eventual-fairness window and
    /// reliably acquires the lock in the gap after the first header.
    struct SignalOnFirstWriter {
        writes: WriteLog,
        tx: std::sync::mpsc::Sender<()>,
        signaled: bool,
    }

    impl SysfsWriter for SignalOnFirstWriter {
        fn write_file(&mut self, path: &str, value: &str) -> Result<(), HwmonError> {
            self.writes.lock().push((path.into(), value.into()));
            if !self.signaled {
                self.signaled = true;
                let _ = self.tx.send(());
                // Still holding the controller lock: park the preemptor long
                // enough that the next unlock hands off to it (fairness), not
                // to force_all_with_floor's own re-lock for the second header.
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(())
        }
        fn read_file(&self, _path: &str) -> Result<String, HwmonError> {
            Ok("128\n".into())
        }
    }

    fn header_with_paths(i: usize) -> PwmHeaderDescriptor {
        let mut h = make_header(&format!("hwmon:it8696:pwm{i}"));
        h.pwm_path = format!("/sys/class/hwmon/hwmon0/pwm{i}");
        h.enable_path = Some(format!("/sys/class/hwmon/hwmon0/pwm{i}_enable"));
        h
    }

    /// [SAFETY] D1-j / DEC-307: the hwmon twin of
    /// `the_recovery_rung_floors_a_curve_it_used_to_replace`.
    ///
    /// A commanded header keeps its own higher duty; an uncommanded one still
    /// gets the bare floor. The second half is what preserves the emergency's
    /// reach — flooring only the command list would leave every header no
    /// control happens to drive untouched during a thermal event, which is the
    /// v2.38.0 P1 shape.
    #[tokio::test]
    async fn hwmon_force_with_floor_raises_a_low_command_and_keeps_a_high_one() {
        let commanded = header_with_paths(1);
        let uncommanded = header_with_paths(2);
        let (mut be, writes) = hwmon_backend(vec![commanded, uncommanded]);

        be.force_all_with_floor(
            60,
            &[
                PwmCommand {
                    member_id: "hwmon:it8696:pwm1".into(),
                    source: "hwmon".into(),
                    pwm_percent: 84,
                    gpu_fan_zero_rpm: false,
                },
                // A command BELOW the floor must not lower the header; the floor
                // wins. This is the direction the pre-D1-j code got right and the
                // one a naive "always take the command" fix would break.
                PwmCommand {
                    member_id: "hwmon:it8696:pwm3".into(),
                    source: "hwmon".into(),
                    pwm_percent: 10,
                    gpu_fan_zero_rpm: false,
                },
            ],
        )
        .await;

        let w = writes.lock();
        let vals = |path: &str| -> Vec<String> {
            w.iter()
                .filter(|(p, _)| p == path)
                .map(|(_, v)| v.trim().to_string())
                .collect()
        };
        assert!(
            vals("/sys/class/hwmon/hwmon0/pwm1")
                .contains(&crate::pwm::percent_to_raw(84).to_string()),
            "a commanded header above the floor must keep its own duty (84%); got {:?}",
            vals("/sys/class/hwmon/hwmon0/pwm1")
        );
        assert!(
            vals("/sys/class/hwmon/hwmon0/pwm2")
                .contains(&crate::pwm::percent_to_raw(60).to_string()),
            "an uncommanded header must still get the bare floor (60%); got {:?}",
            vals("/sys/class/hwmon/hwmon0/pwm2")
        );
    }

    /// DEC-295: the `force_all_with_floor` twin of `hwmon_apply_skips_read_only_header`.
    ///
    /// `apply` has had the DEC-102 read-only backstop for a long time;
    /// `force_all_with_floor` did not, so during an emergency hold every read-only header
    /// attempted a write that could only EACCES — and this path logs INLINE,
    /// bypassing `note_outcomes`' streak throttle, so it emitted one
    /// `THERMAL SAFETY ... FAILED` line per second for the whole emergency-to-release hold.
    ///
    /// The writable-sibling half is the load-bearing one. This is the thermal
    /// path, and a filter that over-filtered would remove the emergency's REACH
    /// — the v2.38.0 P1 failure mode, which took OpenFan fans out of `force_all_with_floor`
    /// and was caught only by a review pass that no longer runs unconditionally.
    #[tokio::test]
    async fn hwmon_force_all_skips_read_only_headers_but_still_forces_writable_ones() {
        let mut ro = header_with_paths(1);
        ro.is_writable = false;
        let rw = header_with_paths(2);
        let (mut be, writes) = hwmon_backend(vec![ro, rw]);

        be.force_all_with_floor(100, &[]).await;

        let w = writes.lock();
        assert!(
            !w.iter().any(|(p, _)| *p == "/sys/class/hwmon/hwmon0/pwm1"),
            "read-only header must not be written during a thermal force; got {w:?}"
        );
        // REACH: the writable sibling must still be driven, and to the forced
        // VALUE — a filter that dropped everything would pass a presence-only
        // assertion.
        let vals: Vec<_> = w
            .iter()
            .filter(|(p, _)| *p == "/sys/class/hwmon/hwmon0/pwm2")
            .map(|(_, v)| v.trim())
            .collect();
        assert!(
            vals.contains(&"255"),
            "writable header must still be forced to 100% (raw 255); got {vals:?}"
        );
    }

    #[tokio::test]
    async fn hwmon_force_all_completes_every_header_despite_midscan_verify_preempt() {
        // Regression for the force_all_with_floor partial-write bug. DEC-099 drops the
        // controller lock between headers, so a GUI verify can force-take the
        // lease mid-scan and invalidate force_all_with_floor's. The retry-on-lease-error
        // fix re-takes thermal-safety and still forces EVERY header; without it
        // the header after the preemption is silently left un-forced during a
        // thermal emergency.
        const N: usize = 8;
        let writes: WriteLog = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let writer = SignalOnFirstWriter {
            writes: writes.clone(),
            tx,
            signaled: false,
        };
        let headers: Vec<_> = (1..=N).map(header_with_paths).collect();
        let cache = Arc::new(StateCache::new());
        let ctrl = HwmonPwmController::new(headers, LeaseManager::new(), Box::new(writer), cache);
        let mut be = HwmonBackend::new(Arc::new(Mutex::new(ctrl)));

        // Exactly one mid-scan preemption: a GUI verify force-takes the lease
        // once force_all_with_floor is past the first header.
        let ctrl_for_preempt = be.ctrl.clone();
        let preemptor = std::thread::spawn(move || {
            rx.recv()
                .expect("force_all_with_floor must write at least one header");
            ctrl_for_preempt
                .lock()
                .lease_manager_mut()
                .force_take_lease(HwmonWriter::Verify);
        });

        be.force_all_with_floor(100, &[]).await;
        preemptor.join().unwrap();

        // Every header must have been forced to 100% despite the mid-scan
        // preemption. Assert the VALUE (raw "255"), not just presence — the
        // failure message already claims "100%", so prove it.
        let w = writes.lock();
        for i in 1..=N {
            let pwm_path = format!("/sys/class/hwmon/hwmon0/pwm{i}");
            let vals: Vec<_> = w
                .iter()
                .filter(|(p, _)| *p == pwm_path)
                .map(|(_, v)| v.trim())
                .collect();
            assert!(
                !vals.is_empty(),
                "header pwm{i} was not forced (partial-write bug); writes={w:?}"
            );
            assert!(
                vals.iter().all(|v| *v == "255"),
                "header pwm{i} must be forced to 100% (raw 255); got {vals:?}"
            );
        }
        drop(w);

        // force_all_with_floor reclaimed the lease from the verify preemptor — proof the
        // re-take (not just a lucky race) carried the scan to completion.
        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(
            lease.map(|l| l.owner),
            Some(HwmonWriter::ThermalSafety),
            "force_all_with_floor must re-take thermal-safety after a mid-scan verify preempt"
        );
    }

    // ── OpenFan backend ──────────────────────────────────────────────

    struct SerialMock {
        written: Arc<Mutex<Vec<String>>>,
    }

    impl crate::serial::transport::SerialTransport for SerialMock {
        fn write_line(&mut self, data: &str) -> Result<(), crate::error::SerialError> {
            self.written.lock().push(data.to_string());
            Ok(())
        }
        fn read_line(
            &mut self,
            _timeout: std::time::Duration,
        ) -> Result<String, crate::error::SerialError> {
            // A protocol-valid SetPwm ACK (Phase-4 fix): the old "OK" was a
            // debug line, so every mocked set_pwm silently returned an ACK
            // error after its write — invisible to frame-only assertions but
            // fatal to any test observing outcomes/streaks.
            Ok("<02|00:0000;>".into())
        }
    }

    fn openfan_backend() -> (OpenFanBackend, Arc<Mutex<Vec<String>>>, Arc<StateCache>) {
        let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let transport = SerialMock {
            written: written.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(100),
        );
        (
            OpenFanBackend::new(Arc::new(Mutex::new(ctrl)), cache.clone()),
            written,
            cache,
        )
    }

    #[tokio::test]
    async fn openfan_backend_drops_malformed_member_ids() {
        let (mut be, written, _cache) = openfan_backend();

        be.apply(&[
            cmd("openfan:chXX", "openfan", 50),
            cmd("not-a-channel", "openfan", 50),
        ])
        .await;

        assert!(written.lock().is_empty());
    }

    #[tokio::test]
    async fn openfan_backend_writes_when_gui_inactive() {
        let (mut be, written, _cache) = openfan_backend();

        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;

        let w = written.lock();
        assert!(
            w.iter().any(|c| c.starts_with(">02")),
            "expected a SetPwm command; got {w:?}"
        );
    }

    #[tokio::test]
    async fn openfan_apply_translates_pct_to_exact_channel_frames() {
        // TEST-3 (2026-07-21 audit): pin the pct→wire translation end-to-end
        // through apply() — channel index and raw byte, not just the ">02"
        // prefix the smoke test checks. 100% → FF and 0% → 00 are the two
        // rounding-free anchors of the percent→raw map; ch03/ch00 pin the
        // channel-index hex encoding.
        let (mut be, written, _cache) = openfan_backend();

        be.apply(&[
            cmd("openfan:ch03", "openfan", 100),
            cmd("openfan:ch00", "openfan", 0),
        ])
        .await;

        let w = written.lock();
        assert!(
            w.iter().any(|f| f.trim_end() == ">0203FF"),
            "ch03 @ 100% must encode as >0203FF; got {w:?}"
        );
        assert!(
            w.iter().any(|f| f.trim_end() == ">020000"),
            "ch00 @ 0% must encode as >020000; got {w:?}"
        );
    }

    #[tokio::test]
    async fn openfan_coalesced_zero_hold_keeps_failure_streak_clear() {
        // CONC-2 propagation (2026-07-21 audit follow-up): a coalesced
        // same-value repeat returns Ok from set_pwm and must flow through
        // note_outcomes as a SUCCESS — keeping the per-channel failure streak
        // and the link-down streak clear. Guards the streak-inflation
        // regression the pre-CONC-2 check order caused for steady 0% holds.
        let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let transport = FlakySerial {
            written: written.clone(),
            fail: fail.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(100),
        );
        let mut be = OpenFanBackend::new(Arc::new(Mutex::new(ctrl)), cache);

        // Failing link: one 0% attempt starts both streaks.
        be.apply(&[cmd("openfan:ch00", "openfan", 0)]).await;
        assert_eq!(be.channel_failures.get(&0).copied(), Some(1));
        assert_eq!(be.link_down_streak, 1);

        // Link back: the 0% retry reaches the wire and clears both streaks.
        fail.store(false, std::sync::atomic::Ordering::Relaxed);
        be.apply(&[cmd("openfan:ch00", "openfan", 0)]).await;
        assert_eq!(be.channel_failures.get(&0), None);
        assert_eq!(be.link_down_streak, 0);
        let wire_after_success = written.lock().len();

        // Steady hold: the repeat coalesces (nothing new on the wire) and
        // still counts as success — streaks stay clear.
        be.apply(&[cmd("openfan:ch00", "openfan", 0)]).await;
        assert_eq!(
            written.lock().len(),
            wire_after_success,
            "repeat 0% must coalesce, not re-write"
        );
        assert_eq!(be.channel_failures.get(&0), None);
        assert_eq!(be.link_down_streak, 0);
    }

    #[tokio::test]
    async fn openfan_force_all_writes_every_channel() {
        let (mut be, written, _cache) = openfan_backend();

        be.force_all_with_floor(100, &[]).await;

        let w = written.lock();
        let set_pwm: Vec<_> = w.iter().filter(|c| c.starts_with(">02")).collect();
        assert_eq!(
            set_pwm.len(),
            NUM_CHANNELS as usize,
            "expected one forced SetPwm per channel; got {w:?}"
        );
        // Count alone can't catch a force_all_with_floor that ignores its pct argument and
        // sends e.g. 40% during a thermal emergency. Pin the VALUE: 100% → raw 255
        // → frame ">02{ch:02X}FF\n" for every channel.
        for frame in &set_pwm {
            assert!(
                frame.trim_end().ends_with("FF"),
                "thermal force must drive every OpenFan channel to 100% (raw FF); got {frame:?}"
            );
        }
    }

    /// A toggleable serial transport: `write_line` fails (link "vanished")
    /// while `fail` is set, otherwise records the write. Models an OpenFan
    /// controller being unplugged and re-plugged at runtime.
    struct FlakySerial {
        written: Arc<Mutex<Vec<String>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::serial::transport::SerialTransport for FlakySerial {
        fn write_line(&mut self, data: &str) -> Result<(), crate::error::SerialError> {
            if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(crate::error::SerialError::Timeout { timeout_ms: 100 });
            }
            self.written.lock().push(data.to_string());
            Ok(())
        }
        fn read_line(
            &mut self,
            _timeout: std::time::Duration,
        ) -> Result<String, crate::error::SerialError> {
            // Protocol-valid SetPwm ACK (see SerialMock::read_line).
            Ok("<02|00:0000;>".into())
        }
    }

    #[tokio::test]
    async fn openfan_backend_tolerates_vanish_then_resumes_on_reappear() {
        // Matrix row (OpenFan vanish/reappear): when the serial link drops, the
        // engine's OpenFan apply must NOT panic and must record no successful
        // write; when the link returns, writes resume. Post-flip the engine is
        // the sole writer (DEC-165), so this resilience is load-bearing.
        let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let transport = FlakySerial {
            written: written.clone(),
            fail: fail.clone(),
        };
        let cache = Arc::new(StateCache::new());
        let ctrl = crate::serial::controller::FanController::new(
            Box::new(transport),
            cache.clone(),
            std::time::Duration::from_millis(100),
        );
        let mut be = OpenFanBackend::new(Arc::new(Mutex::new(ctrl)), cache);

        // Vanished: writes fail; the engine no-ops without panicking.
        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;
        assert!(
            written.lock().is_empty(),
            "no successful OpenFan write while the link is down"
        );

        // Reappeared: writes resume on the next tick.
        fail.store(false, std::sync::atomic::Ordering::Relaxed);
        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;
        let w = written.lock();
        assert!(
            w.iter().any(|c| c.starts_with(">02")),
            "OpenFan writes must resume once the link reappears; got {w:?}"
        );
    }

    // ── Phase 2 / P-CAL additions (daemon /audit 2026-06-26) ──────────

    #[test]
    fn openfan_per_channel_failure_streak_not_masked_by_healthy_channel() {
        // P3-5: a persistent single-channel fault must climb its OWN streak to
        // the SAFETY threshold even while another channel succeeds every tick.
        // The pre-fix shared counter reset on ANY success, so it never tripped.
        let (mut be, _written, _cache) = openfan_backend();
        for _ in 0..constants::OPENFAN_FAIL_ALERT_THRESHOLD {
            be.note_outcomes(&[(0, Ok(())), (3, Err("link".into()))]);
        }
        assert_eq!(
            be.channel_failure_streak(3),
            constants::OPENFAN_FAIL_ALERT_THRESHOLD,
            "the dead channel's streak must reach the threshold despite ch0 succeeding"
        );
        assert_eq!(
            be.channel_failure_streak(0),
            0,
            "the healthy channel's streak stays at 0"
        );
        assert_eq!(
            be.link_down_streak(),
            0,
            "a partial fault is not a whole-link failure"
        );
        // The dead channel's own success — and only that — resets its streak.
        be.note_outcomes(&[(3, Ok(()))]);
        assert_eq!(be.channel_failure_streak(3), 0);
    }

    #[test]
    fn openfan_whole_link_down_trips_distinct_link_streak() {
        // P3-5: every attempted channel failing for the threshold consecutively
        // trips the distinct whole-link "serial down" streak; one success
        // anywhere resets it.
        let (mut be, _written, _cache) = openfan_backend();
        for _ in 0..constants::OPENFAN_FAIL_ALERT_THRESHOLD {
            be.note_outcomes(&[(0, Err("x".into())), (1, Err("x".into()))]);
        }
        assert_eq!(
            be.link_down_streak(),
            constants::OPENFAN_FAIL_ALERT_THRESHOLD
        );
        be.note_outcomes(&[(0, Ok(())), (1, Err("x".into()))]);
        assert_eq!(
            be.link_down_streak(),
            0,
            "any channel success resets the whole-link streak"
        );
    }

    #[tokio::test]
    async fn openfan_backend_skips_writes_while_engine_paused() {
        // DEC-191: while a verify/calibration holds the engine write-pause, the
        // OpenFan backend's in-flight recheck must skip writes (so a calibration
        // sweep's test PWM survives), then resume when the pause clears.
        let (mut be, written, cache) = openfan_backend();

        let verify_epoch = cache
            .try_begin_verify(std::time::Duration::from_secs(30))
            .expect("free slot");
        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;
        assert!(
            written.lock().is_empty(),
            "no OpenFan write may land while the engine is paused (DEC-191)"
        );

        cache.end_verify(verify_epoch);
        be.apply(&[cmd("openfan:ch00", "openfan", 50)]).await;
        assert!(
            written.lock().iter().any(|c| c.starts_with(">02")),
            "writes resume once the pause clears"
        );
    }

    #[tokio::test]
    async fn hwmon_apply_skips_while_a_verify_lease_is_held() {
        // P2-1: a hardware verify force-takes the lease as "verify". If it starts
        // after the loop-level gate, the engine still reaches apply; it must NOT
        // adopt the verify lease and write through it (clobbering the test value).
        // Option B: the engine skips its hwmon writes and leaves the lease intact.
        let (mut be, writes) = hwmon_backend(vec![make_header("hwmon:it8696:pwm1")]);
        be.ctrl
            .lock()
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::Verify);

        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;

        let w = writes.lock();
        assert!(
            !w.iter().any(|(p, _)| p.ends_with("pwm1")),
            "engine must not write hwmon while a verify holds the lease (P2-1); got {w:?}"
        );
        drop(w);
        let lease = be.ctrl.lock().lease_manager().active_lease().cloned();
        assert_eq!(
            lease.map(|l| l.owner),
            Some(HwmonWriter::Verify),
            "the verify lease must remain intact — the engine did not take over"
        );
    }

    #[tokio::test]
    async fn hwmon_apply_reuses_thermal_safety_lease_no_post_emergency_stall() {
        // P2-1 regression guard: Option B excludes ONLY the verify lease. A
        // non-verify foreign lease (e.g. "thermal-safety" left by force_all_with_floor after
        // an emergency) must still be reused so the engine resumes hwmon control
        // immediately instead of being locked out for the lease's 60 s TTL.
        let (mut be, writes) = hwmon_backend(vec![make_header("hwmon:it8696:pwm1")]);
        be.ctrl
            .lock()
            .lease_manager_mut()
            .force_take_lease(HwmonWriter::ThermalSafety);

        be.apply(&[cmd("hwmon:it8696:pwm1", "hwmon", 55)]).await;

        let w = writes.lock();
        assert!(
            w.iter().any(|(p, _)| p.ends_with("pwm1")),
            "engine must reuse a thermal-safety lease and write (no 60 s stall); got {w:?}"
        );
    }

    #[tokio::test]
    async fn gpu_backend_actually_takes_the_write_lock() {
        // DEC-260. DEC-255's headline fix is that `apply` acquires the shared GPU
        // write lock so a concurrent `POST /gpu/{id}/fan/reset` cannot interleave
        // its own multi-write commit with the engine's. That acquisition had NO
        // test at its call site: the pre-release review deleted the
        // `lock_gpu_writes()` line from `apply` and all 1043 tests stayed green,
        // because the only lock test exercised the primitive directly.
        //
        // This asserts the engine genuinely waits: hold the lock as a reset would,
        // and no curve byte may reach sysfs until it is released.
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = Arc::new(StateCache::new());
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        let held = cache.lock_gpu_writes().await;

        let cmds = vec![cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)];
        let writer = tokio::spawn(async move {
            be.apply(&cmds).await;
            be
        });

        // Give the engine every chance to write if it is not actually blocked.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "the engine wrote a GPU curve while the write lock was held — the \
             reset/engine race DEC-255 closed is open again"
        );

        drop(held);
        let _be = tokio::time::timeout(std::time::Duration::from_secs(5), writer)
            .await
            .expect("engine write must proceed once the lock is released")
            .expect("apply task must not panic");
        assert!(
            !std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "the write must land once the lock is free"
        );
    }

    #[tokio::test]
    async fn gpu_backend_skips_writes_while_engine_paused() {
        // P2-1: GPU fans have no lease (DEC-045), so the per-fan verify_active
        // recheck is the ONLY guard against the engine overwriting a GPU verify's
        // test value mid-tick. While the pause is held, no curve write may land.
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = Arc::new(StateCache::new());
        let verify_epoch = cache
            .try_begin_verify(std::time::Duration::from_secs(30))
            .expect("free slot");
        let mut be = GpuBackend::new(cache.clone(), Arc::new(vec![gpu]));

        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "engine must not write a GPU fan while a verify holds the pause (P2-1)"
        );

        cache.end_verify(verify_epoch);
        be.apply(&[cmd("amd_gpu:0000:03:00.0", "amd_gpu", 70)])
            .await;
        assert!(
            !std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "writes resume once the pause clears"
        );
    }

    #[test]
    fn gpu_blocking_write_skips_when_pause_claimed_mid_dispatch() {
        // CONC-1 (2026-07-21 audit): the in-task re-check guards the window
        // where a verify claims the pause AFTER the async-side per-fan check
        // but BEFORE the blocking task runs. The helper is a named fn for
        // exactly this test: pause held → None (skipped, no outcome), file
        // untouched; pause clear → Some(Ok), the write lands.
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = StateCache::new();

        let verify_epoch = cache
            .try_begin_verify(std::time::Duration::from_secs(30))
            .expect("free slot");
        let out = gpu_blocking_write(
            &cache,
            gpu.fan_curve_path.as_deref().unwrap(),
            gpu.fan_zero_rpm_path.as_deref(),
            70,
            true,
            "amd_gpu:0000:03:00.0",
        );
        assert!(out.is_none(), "pause held → skipped with no outcome");
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "the in-task guard must stop the write, not just relabel it"
        );

        cache.end_verify(verify_epoch);
        let out = gpu_blocking_write(
            &cache,
            gpu.fan_curve_path.as_deref().unwrap(),
            gpu.fan_zero_rpm_path.as_deref(),
            70,
            true,
            "amd_gpu:0000:03:00.0",
        );
        assert!(matches!(out, Some(Ok(()))), "clear pause → write proceeds");
        assert!(!std::fs::read_to_string(&curve_path).unwrap().is_empty());
    }

    #[test]
    fn gpu_blocking_write_skips_when_relinquished_mid_dispatch() {
        // DEC-254: the sibling of the CONC-1 guard above, for the *other*
        // racer. `apply` checks `is_gpu_fan_relinquished` on the async worker
        // before dispatching; a `POST /gpu/{id}/fan/reset` landing between that
        // check and this task used to let the profile's flat curve overwrite
        // firmware-auto. Worse than the verify race: the fan is relinquished by
        // then, so `apply` skips it on every later tick and nothing ever
        // corrects it — the GPU stays on the stale curve until the next profile
        // activation or a restart.
        let dir = tempfile::tempdir().unwrap();
        let (gpu, curve_path) = fake_gpu(&dir);
        let cache = StateCache::new();
        let fan_id = "amd_gpu:0000:03:00.0";

        let _ = cache.relinquish_gpu_fan(fan_id);
        let out = gpu_blocking_write(
            &cache,
            gpu.fan_curve_path.as_deref().unwrap(),
            gpu.fan_zero_rpm_path.as_deref(),
            70,
            true,
            fan_id,
        );
        assert!(out.is_none(), "relinquished → skipped with no outcome");
        assert!(
            std::fs::read_to_string(&curve_path).unwrap().is_empty(),
            "the in-task guard must stop the write reaching sysfs"
        );

        // A reset that failed hands the fan back, and writes resume.
        cache.unrelinquish_gpu_fan(fan_id);
        let out = gpu_blocking_write(
            &cache,
            gpu.fan_curve_path.as_deref().unwrap(),
            gpu.fan_zero_rpm_path.as_deref(),
            70,
            true,
            fan_id,
        );
        assert!(
            matches!(out, Some(Ok(()))),
            "un-relinquished → write proceeds"
        );
        assert!(!std::fs::read_to_string(&curve_path).unwrap().is_empty());
    }

    #[test]
    fn unrelinquish_is_scoped_to_one_fan() {
        // The rollback must not behave like `clear_relinquished_gpu_fans`, which
        // would also resurrect an unrelated, successful reset.
        let cache = StateCache::new();
        let _ = cache.relinquish_gpu_fan("amd_gpu:0000:03:00.0");
        let _ = cache.relinquish_gpu_fan("amd_gpu:0000:0a:00.0");

        cache.unrelinquish_gpu_fan("amd_gpu:0000:03:00.0");

        assert!(!cache.is_gpu_fan_relinquished("amd_gpu:0000:03:00.0"));
        assert!(
            cache.is_gpu_fan_relinquished("amd_gpu:0000:0a:00.0"),
            "the other GPU's reset must survive"
        );
    }
}
