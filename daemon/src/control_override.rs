//! Daemon-owned manual-override and fan-identify state (DEC-163 / DEC-166).
//!
//! Replaces the GUI's two in-process transient manual mechanisms — the
//! per-control Manual card (`_manual_controls`) and the wizard's per-fan
//! stop/restore — with daemon-owned, expiring, fencing-guarded control intent
//! that **fails safe**: an override reverts to autonomous curve control when it
//! is not renewed, and a fan-identify stop auto-restores. Expiry is judged on
//! the daemon's own monotonic clock (never a client timestamp), so a frozen,
//! crashed, slept, or half-open GUI cannot strand fans.
//!
//! ## Override (per logical control)
//! Pins a control's members to a fixed PWM, pausing that control's curve
//! evaluation. Each grant carries a monotonically increasing `token` that
//! serves as BOTH the grant identity and the fencing token: because the daemon
//! is simultaneously the lock service and the resource (Kleppmann, "How to do
//! distributed locking"), an atomic check that rejects any token which is not
//! the control's current (highest-issued) token is sufficient fencing — a
//! thawed GUI holding a stale token cannot silently re-pin fans. D4 originally
//! specified a separate `override_id` + `fencing_token`; here they collapse to
//! one field because the fence reduces to a per-resource counter.
//!
//! ## Identify (per fan)
//! Holds a single fan at a distinguishable duty for physical identification,
//! auto-restoring after a short deadman TTL. Bounded by the deadman and the
//! thermal force. Restore is simply removal — the engine recomputes the fan's
//! curve value next tick, so no prior-PWM is remembered, and a request that
//! fails part-way through leaves nothing to unwind.
//!
//! **The hold duty is role-dependent (DEC-311).** An ordinary fan is held at 0
//! and remains floor-exempt: stopping it is both safe and the clearest possible
//! signal. A `role: Pump` header is instead **perturbed** — moved to a duty at
//! least [`crate::profile::HARD_PUMP_CPU_FLOOR_PCT`] away from its baseline and
//! never below that floor. This supersedes DEC-166's "floor-exempt by necessity
//! (you must be able to stop a pump to find it)": losing coolant flow to find a
//! header is not a trade the daemon should make on the user's behalf, and an
//! audible RPM *change* identifies a pump just as well as a stop.
//!
//! Safety ordering enforced by the engine tick:
//! `thermal force  >  identify (floor-exempt for fans, floored for pumps)
//!   >  override (floor-clamped)  >  curve`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::{Clock, SystemClock};

/// Why an override renew/release was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideReject {
    /// The presented token is not the control's current token (it was
    /// superseded by a newer take, or never issued). A thawed GUI must not be
    /// able to re-pin fans with a stale token (Kleppmann fencing).
    StaleToken,
    /// No live override exists for this control — it expired (deadman fired) or
    /// was never taken. The caller should re-take, not renew.
    NotActive,
}

impl std::fmt::Display for OverrideReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleToken => write!(f, "stale or superseded override token"),
            Self::NotActive => write!(f, "no active override for this control"),
        }
    }
}

/// A live override on one control.
#[derive(Debug, Clone)]
struct ControlOverride {
    token: u64,
    pwm_percent: u8,
    expires_at: Instant,
}

/// How a fan is being held for identification (DEC-311).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifyMode {
    /// Driven to 0 — the ordinary-fan behaviour, unchanged since DEC-166.
    Stop,
    /// Shifted to a distinguishable duty that never goes below the pump floor.
    PumpPerturb,
}

impl IdentifyMode {
    /// Wire token for `IdentifyResponse.mode` / `IdentifyStatusEntry.mode`.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentifyMode::Stop => "stop",
            IdentifyMode::PumpPerturb => "pump_perturb",
        }
    }
}

/// Choose the identify hold duty for a fan, from its role and current duty
/// (DEC-311, AIO-MB Phase 1).
///
/// [SAFETY] **The only production path that computes an identify target.** Both
/// the clamp and the role decision live here so there is exactly one place that
/// can get this wrong, and one place to test.
///
/// - Non-pump roles → [`IdentifyMode::Stop`] at `0`. Unchanged since DEC-166:
///   stopping an ordinary fan is safe and is the clearest possible signal.
/// - `role: Pump` → [`IdentifyMode::PumpPerturb`]. Shifted by
///   [`IDENTIFY_PUMP_DELTA_PCT`], **upward wherever there is headroom** so the
///   pump never moves toward its stall floor, and clamped into
///   `[HARD_PUMP_CPU_FLOOR_PCT, 100]` on **both** branches.
///
/// Clamping the upward branch matters and is not defensive noise: a baseline of
/// 0 (nothing commanded yet) computes `0 + 25 = 25`, which is *below* the 30%
/// pump floor. Without the clamp the pump-safe path would itself under-drive a
/// pump — the precise failure `AIO1-a` records on the verify path.
pub fn identify_target_for_role(
    role: crate::hwmon::roles::HeaderRole,
    last_commanded_pct: Option<u8>,
) -> (u8, IdentifyMode) {
    if !role.is_pump() {
        return (0, IdentifyMode::Stop);
    }
    let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
    let baseline =
        last_commanded_pct.unwrap_or(crate::constants::IDENTIFY_PUMP_BASELINE_FALLBACK_PCT);
    let delta = crate::constants::IDENTIFY_PUMP_DELTA_PCT;
    let raised = baseline.saturating_add(delta);
    let target = if raised <= 100 {
        raised
    } else {
        baseline.saturating_sub(delta)
    };
    (target.clamp(floor, 100), IdentifyMode::PumpPerturb)
}

/// A live identify hold on one fan.
#[derive(Debug, Clone)]
struct IdentifyEntry {
    expires_at: Instant,
    /// The duty the engine pins this fan to while the hold is live. `0` for
    /// [`IdentifyMode::Stop`]; `>= HARD_PUMP_CPU_FLOOR_PCT` for a perturbation.
    ///
    /// Stored as an absolute value computed **once, when the identify was
    /// taken**, rather than recomputed per tick against the live curve output.
    /// A per-tick recomputation would chase a moving baseline: as the curve
    /// ramped, the "perturbed" duty would drift along with it and the
    /// difference the user is listening for would shrink to nothing.
    target_pct: u8,
    mode: IdentifyMode,
}

/// Grant returned to the caller on a successful override take/renew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideGrant {
    pub token: u64,
    pub ttl_secs: u64,
    pub expires_in_secs: u64,
}

/// Read-only view of currently-live entries, consumed by the engine tick.
#[derive(Debug, Clone, Default)]
pub struct OverrideSnapshot {
    /// control_id → pinned PWM. Curve + tuning are skipped for these controls;
    /// only the per-member hard safety floor is applied at eval.
    pub controls: HashMap<String, u8>,
    /// Fan member_id → the duty its identify hold pins it to (DEC-311).
    ///
    /// Was a `HashSet` of ids forced to 0. It carries the duty now because the
    /// duty is role-dependent: 0 for an ordinary fan, a floored perturbation
    /// for a pump. Applied at eval **after** all other resolution and, for a
    /// stop, floor-exempt — the pump case does not need the exemption because
    /// its target is already at or above the floor by construction.
    pub identify: HashMap<String, u8>,
}

/// One active override row for the `/status` poll surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideStatusRow {
    pub control_id: String,
    pub pwm_percent: u8,
    pub expires_in_secs: u64,
}

/// One active identify row for the `/status` poll surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifyStatusRow {
    pub fan_id: String,
    pub expires_in_secs: u64,
    pub mode: IdentifyMode,
    pub identify_pwm_percent: u8,
}

/// IDs cleared by a `sweep`, so the engine can reset their cross-tick state.
#[derive(Debug, Clone, Default)]
pub struct SweepCleared {
    /// Controls whose override lapsed — their hysteresis must be reset so the
    /// resumed curve re-anchors instead of step-rate-clamping from the pin.
    pub controls: Vec<String>,
    /// Fans whose identify hold lapsed (no state reset needed — the control
    /// kept evaluating; only the final command was rewritten).
    pub fans: Vec<String>,
}

/// In-memory override + identify table. Pure state, swept on the daemon's
/// monotonic clock each engine tick. Shared (behind a `Mutex`) between the API
/// handlers (mutate) and the engine tick (sweep + snapshot).
pub struct OverrideTable {
    controls: HashMap<String, ControlOverride>,
    identify: HashMap<String, IdentifyEntry>,
    /// Strictly increasing, never reused — a token is globally unique and
    /// totally ordered. `take` issues `next_token += 1`.
    next_token: u64,
    clock: Arc<dyn Clock>,
}

impl Default for OverrideTable {
    fn default() -> Self {
        Self::new()
    }
}

impl OverrideTable {
    /// Production table on the real monotonic clock.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// Table on an injected clock (tests advance a fake clock to exercise
    /// expiry deterministically — same seam as the hwmon lease).
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            controls: HashMap::new(),
            identify: HashMap::new(),
            next_token: 0,
            clock,
        }
    }

    /// Take (or replace) an override on a control. Always succeeds and issues a
    /// fresh, strictly greater token; a re-take supersedes any prior grant, so
    /// the previous token becomes stale (fencing).
    pub fn take_override(
        &mut self,
        control_id: &str,
        pwm_percent: u8,
        ttl: Duration,
    ) -> OverrideGrant {
        let now = self.clock.now();
        self.next_token += 1;
        let token = self.next_token;
        self.controls.insert(
            control_id.to_string(),
            ControlOverride {
                token,
                pwm_percent,
                expires_at: now + ttl,
            },
        );
        OverrideGrant {
            token,
            ttl_secs: ttl.as_secs(),
            expires_in_secs: ttl.as_secs(),
        }
    }

    /// Renew an override, extending its TTL from now. Rejects a stale/superseded
    /// token (`StaleToken`) or a control with no live override (`NotActive` —
    /// expired or never taken).
    pub fn renew_override(
        &mut self,
        control_id: &str,
        token: u64,
        ttl: Duration,
    ) -> Result<OverrideGrant, OverrideReject> {
        let now = self.clock.now();
        match self.controls.get_mut(control_id) {
            Some(ov) if ov.token == token && now < ov.expires_at => {
                ov.expires_at = now + ttl;
                Ok(OverrideGrant {
                    token,
                    ttl_secs: ttl.as_secs(),
                    expires_in_secs: ttl.as_secs(),
                })
            }
            // A live override exists but the token differs — superseded/stale.
            Some(ov) if now < ov.expires_at => Err(OverrideReject::StaleToken),
            // Right token but expired, or no entry — re-take, don't renew.
            _ => Err(OverrideReject::NotActive),
        }
    }

    /// Release an override (revert to curve immediately). Rejects a stale token;
    /// returns `NotActive` if nothing live is held (treat as idempotent success
    /// at the API layer).
    pub fn release_override(&mut self, control_id: &str, token: u64) -> Result<(), OverrideReject> {
        let now = self.clock.now();
        match self.controls.get(control_id) {
            Some(ov) if ov.token == token && now < ov.expires_at => {
                self.controls.remove(control_id);
                Ok(())
            }
            Some(ov) if now < ov.expires_at => Err(OverrideReject::StaleToken),
            _ => Err(OverrideReject::NotActive),
        }
    }

    /// Clear every control-override (revert all pinned controls to curve).
    /// Called on profile activation (DEC-189): a freshly-activated profile owns
    /// its controls' intent, so an override taken against the previous profile
    /// must not bleed onto a same-id control in the new one. The engine resets
    /// the cleared controls' cross-tick state on its next tick via the DEC-188
    /// activation-epoch path (`engine_state.deactivate()`), not via `sweep`, so
    /// no cleared-id list is returned here.
    ///
    /// Identify-stops are deliberately **not** cleared: an identify is per
    /// *physical fan* (`openfan:ch00`, `hwmon:…`, `amd_gpu:…`) and
    /// profile-independent — it must survive a profile switch and auto-restore
    /// on its own deadman.
    pub fn clear_all_overrides(&mut self) {
        self.controls.clear();
    }

    /// Hold a fan at `target_pct` for identification, auto-restoring after
    /// `ttl` (deadman). A repeat call refreshes the deadman and re-pins the
    /// target.
    ///
    /// [SAFETY] The caller is responsible for choosing a `target_pct` that is
    /// legal for the fan's role — see `identify_target_for_role`, which is the
    /// only production path that computes one. This function does not clamp,
    /// because it cannot see the role; the clamp lives with the role lookup so
    /// there is exactly one place that decides.
    pub fn identify_hold(
        &mut self,
        fan_id: &str,
        target_pct: u8,
        mode: IdentifyMode,
        ttl: Duration,
    ) {
        let now = self.clock.now();
        self.identify.insert(
            fan_id.to_string(),
            IdentifyEntry {
                expires_at: now + ttl,
                target_pct,
                mode,
            },
        );
    }

    /// Restore a fan immediately (remove the identify hold). Idempotent.
    pub fn identify_restore(&mut self, fan_id: &str) {
        self.identify.remove(fan_id);
    }

    /// Drop every entry whose deadman has fired, judged on the daemon's clock.
    /// Returns the cleared ids so the engine can reset lapsed controls' state.
    pub fn sweep(&mut self) -> SweepCleared {
        let now = self.clock.now();
        let mut cleared = SweepCleared::default();
        self.controls.retain(|id, ov| {
            let live = now < ov.expires_at;
            if !live {
                cleared.controls.push(id.clone());
            }
            live
        });
        self.identify.retain(|id, e| {
            let live = now < e.expires_at;
            if !live {
                cleared.fans.push(id.clone());
            }
            live
        });
        cleared
    }

    /// Snapshot of currently-live entries for the engine tick. Filters expired
    /// entries defensively even between sweeps (the `expires_at` instant is the
    /// authoritative check; the 1 Hz sweep is just lazy cleanup).
    pub fn snapshot(&self) -> OverrideSnapshot {
        let now = self.clock.now();
        OverrideSnapshot {
            controls: self
                .controls
                .iter()
                .filter(|(_, ov)| now < ov.expires_at)
                .map(|(id, ov)| (id.clone(), ov.pwm_percent))
                .collect(),
            identify: self
                .identify
                .iter()
                .filter(|(_, e)| now < e.expires_at)
                .map(|(id, e)| (id.clone(), e.target_pct))
                .collect(),
        }
    }

    /// Active rows for the `/status` poll surface, each with remaining TTL.
    /// Sorted by id for deterministic wire output.
    pub fn status_rows(&self) -> (Vec<OverrideStatusRow>, Vec<IdentifyStatusRow>) {
        let now = self.clock.now();
        let mut overrides: Vec<OverrideStatusRow> = self
            .controls
            .iter()
            .filter(|(_, ov)| now < ov.expires_at)
            .map(|(id, ov)| OverrideStatusRow {
                control_id: id.clone(),
                pwm_percent: ov.pwm_percent,
                expires_in_secs: ov.expires_at.saturating_duration_since(now).as_secs(),
            })
            .collect();
        overrides.sort_by(|a, b| a.control_id.cmp(&b.control_id));

        let mut identify: Vec<IdentifyStatusRow> = self
            .identify
            .iter()
            .filter(|(_, e)| now < e.expires_at)
            .map(|(id, e)| IdentifyStatusRow {
                fan_id: id.clone(),
                expires_in_secs: e.expires_at.saturating_duration_since(now).as_secs(),
                mode: e.mode,
                identify_pwm_percent: e.target_pct,
            })
            .collect();
        identify.sort_by(|a, b| a.fan_id.cmp(&b.fan_id));

        (overrides, identify)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// Manually-advanceable monotonic clock for deterministic expiry tests
    /// (mirrors the hwmon lease's `TestClock`).
    struct ManualClock {
        now: Mutex<Instant>,
    }

    impl ManualClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(Instant::now()),
            })
        }
        fn advance(&self, d: Duration) {
            *self.now.lock() += d;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            *self.now.lock()
        }
    }

    fn ttl() -> Duration {
        Duration::from_secs(15)
    }

    #[test]
    fn take_issues_strictly_increasing_tokens() {
        let mut t = OverrideTable::new();
        let a = t.take_override("ctrl-a", 50, ttl());
        let b = t.take_override("ctrl-b", 60, ttl());
        let a2 = t.take_override("ctrl-a", 80, ttl());
        assert!(b.token > a.token, "global counter must increase");
        assert!(a2.token > b.token, "re-take supersedes with a newer token");
    }

    #[test]
    fn override_applied_while_renewed_then_reverts_when_renewals_stop() {
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        let g = t.take_override("ctrl-a", 75, ttl());

        // Renew three times well inside the window — stays live.
        for _ in 0..3 {
            clock.advance(Duration::from_secs(5));
            assert!(t.renew_override("ctrl-a", g.token, ttl()).is_ok());
        }
        assert_eq!(t.snapshot().controls.get("ctrl-a"), Some(&75));

        // Stop renewing — the deadman fires within one TTL.
        clock.advance(Duration::from_secs(16));
        let cleared = t.sweep();
        assert_eq!(cleared.controls, vec!["ctrl-a".to_string()]);
        assert!(t.snapshot().controls.is_empty(), "reverted to curve");
    }

    #[test]
    fn entries_expire_exactly_at_their_deadline() {
        // Expiry is judged strictly: an entry is live only while `now <
        // expires_at`, so at EXACTLY its deadline it is already expired. This
        // pins the `<` (not `<=`) boundary across the deadman sweep, the
        // engine-applied snapshot, and the renew/release lifecycle — a
        // one-tick-late deadman would hold a stale pin one tick past its TTL.
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        let g = t.take_override("ctrl-a", 70, ttl());
        t.identify_hold("fan-1", 0, IdentifyMode::Stop, ttl());

        // One tick before the deadline: both still live.
        clock.advance(ttl() - Duration::from_nanos(1));
        assert_eq!(t.snapshot().controls.get("ctrl-a"), Some(&70));
        assert!(t.snapshot().identify.contains_key("fan-1"));

        // Land EXACTLY on `expires_at` (now == expires_at, so `now < expires_at`
        // is false): the entry is already expired everywhere it is read.
        clock.advance(Duration::from_nanos(1));
        let snap = t.snapshot();
        assert!(
            snap.controls.is_empty() && snap.identify.is_empty(),
            "snapshot must exclude an entry at exactly its deadline"
        );
        // renew/release at the exact deadline see no live override → NotActive
        // (re-take), never a fencing rejection and never a successful renew.
        assert_eq!(
            t.renew_override("ctrl-a", g.token, ttl()),
            Err(OverrideReject::NotActive)
        );
        assert_eq!(
            t.release_override("ctrl-a", g.token),
            Err(OverrideReject::NotActive)
        );
        // ...and the sweep reports both as cleared so the engine resets their
        // cross-tick state (control re-anchors to its curve; fan resumes).
        let cleared = t.sweep();
        assert!(cleared.controls.contains(&"ctrl-a".to_string()));
        assert!(cleared.fans.contains(&"fan-1".to_string()));
    }

    #[test]
    fn continuously_renewed_override_never_expires() {
        // No absolute max-duration cap (D4-c): a live renewing client holds
        // indefinitely; the thermal force is the safety net, not a hard cap.
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        let g = t.take_override("ctrl-a", 100, ttl());
        for _ in 0..1000 {
            clock.advance(Duration::from_secs(5));
            t.renew_override("ctrl-a", g.token, ttl())
                .expect("healthy renewal must hold");
        }
        assert_eq!(t.snapshot().controls.get("ctrl-a"), Some(&100));
    }

    #[test]
    fn stale_token_rejected_on_renew_and_release() {
        let mut t = OverrideTable::new();
        let g1 = t.take_override("ctrl-a", 40, ttl());
        let g2 = t.take_override("ctrl-a", 90, ttl()); // supersedes g1
        assert_ne!(g1.token, g2.token);

        // A thawed GUI holding the old token cannot re-pin or release.
        assert_eq!(
            t.renew_override("ctrl-a", g1.token, ttl()),
            Err(OverrideReject::StaleToken)
        );
        assert_eq!(
            t.release_override("ctrl-a", g1.token),
            Err(OverrideReject::StaleToken)
        );
        // The current holder still works.
        assert!(t.renew_override("ctrl-a", g2.token, ttl()).is_ok());
    }

    #[test]
    fn renew_after_expiry_is_not_active_not_stale() {
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        let g = t.take_override("ctrl-a", 50, ttl());
        clock.advance(Duration::from_secs(16)); // lapsed
        assert_eq!(
            t.renew_override("ctrl-a", g.token, ttl()),
            Err(OverrideReject::NotActive),
            "correct token but expired → re-take, not a fencing rejection"
        );
    }

    #[test]
    fn release_reverts_immediately_and_is_idempotent() {
        let mut t = OverrideTable::new();
        let g = t.take_override("ctrl-a", 50, ttl());
        assert!(t.release_override("ctrl-a", g.token).is_ok());
        assert!(t.snapshot().controls.is_empty());
        // Releasing again with the same token → nothing live (idempotent).
        assert_eq!(
            t.release_override("ctrl-a", g.token),
            Err(OverrideReject::NotActive)
        );
    }

    #[test]
    fn a_token_cannot_act_on_another_controls_override() {
        let mut t = OverrideTable::new();
        let ga = t.take_override("ctrl-a", 50, ttl());
        let _gb = t.take_override("ctrl-b", 60, ttl());
        // ctrl-a's token used on ctrl-b is stale for ctrl-b.
        assert_eq!(
            t.renew_override("ctrl-b", ga.token, ttl()),
            Err(OverrideReject::StaleToken)
        );
    }

    /// [SAFETY] DEC-311, the headline invariant. No input — none — makes a pump
    /// identify target 0, or anything below the pump floor.
    ///
    /// Exhaustive over every possible baseline rather than a sample, because
    /// this is the assertion the whole feature rests on and the input space is
    /// 102 values wide. A sampled version of this test would have passed with
    /// the un-clamped upward branch (`0 + 25 = 25`) that the first draft had.
    #[test]
    fn pump_identify_never_targets_zero_or_below_the_floor() {
        let floor = crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8;
        for baseline in (0..=100u8).map(Some).chain(std::iter::once(None)) {
            let (target, mode) =
                identify_target_for_role(crate::hwmon::roles::HeaderRole::Pump, baseline);
            assert_eq!(mode, IdentifyMode::PumpPerturb, "baseline {baseline:?}");
            assert_ne!(target, 0, "baseline {baseline:?} produced a pump STOP");
            assert!(
                target >= floor,
                "baseline {baseline:?} produced {target}%, below the {floor}% pump floor"
            );
            assert!(target <= 100, "baseline {baseline:?} produced {target}%");
        }
    }

    /// The perturbation must actually be perceptible — a "safe" identify that
    /// moved the pump by 0 points would satisfy every safety assertion above
    /// and identify nothing at all.
    #[test]
    fn pump_identify_actually_moves_the_pump() {
        for baseline in 0..=100u8 {
            let (target, _) =
                identify_target_for_role(crate::hwmon::roles::HeaderRole::Pump, Some(baseline));
            // Below the floor the pump is not really running there; the clamp
            // to the floor is itself the change, and is checked above.
            if baseline >= crate::profile::HARD_PUMP_CPU_FLOOR_PCT as u8 {
                assert_ne!(
                    target, baseline,
                    "baseline {baseline}% produced an identify that changes nothing"
                );
            }
        }
    }

    /// Direction: up while there is headroom, down only when there is not.
    /// Upward first is the point — it never walks the pump toward its stall
    /// floor.
    #[test]
    fn pump_identify_prefers_the_upward_direction() {
        let delta = crate::constants::IDENTIFY_PUMP_DELTA_PCT;
        for baseline in 30..=(100 - delta) {
            let (target, _) =
                identify_target_for_role(crate::hwmon::roles::HeaderRole::Pump, Some(baseline));
            assert_eq!(
                target,
                baseline + delta,
                "with headroom, {baseline}% must perturb UPWARD"
            );
        }
        // No headroom → downward, clamped at the floor.
        for baseline in (100 - delta + 1)..=100 {
            let (target, _) =
                identify_target_for_role(crate::hwmon::roles::HeaderRole::Pump, Some(baseline));
            assert!(
                target < baseline,
                "without headroom, {baseline}% must perturb DOWNWARD"
            );
        }
    }

    /// Every non-pump role keeps the ordinary DEC-166 stop. This is the
    /// "existing fan behaviour remains intact" acceptance criterion.
    #[test]
    fn every_non_pump_role_still_stops_at_zero() {
        use crate::hwmon::roles::HeaderRole;
        for role in [
            HeaderRole::Unknown,
            HeaderRole::CpuFan,
            HeaderRole::RadiatorFan,
            HeaderRole::ChassisFan,
        ] {
            for baseline in [None, Some(0), Some(50), Some(100)] {
                assert_eq!(
                    identify_target_for_role(role, baseline),
                    (0, IdentifyMode::Stop),
                    "{role:?} at {baseline:?} must still stop"
                );
            }
        }
    }

    #[test]
    fn identify_hold_records_its_target_and_mode_on_the_status_row() {
        let mut t = OverrideTable::new();
        t.identify_hold("hwmon:x:pwm1:PUMP", 85, IdentifyMode::PumpPerturb, ttl());
        t.identify_hold("openfan:ch00", 0, IdentifyMode::Stop, ttl());
        let (_, rows) = t.status_rows();
        let pump = rows
            .iter()
            .find(|r| r.fan_id == "hwmon:x:pwm1:PUMP")
            .unwrap();
        assert_eq!(pump.mode, IdentifyMode::PumpPerturb);
        assert_eq!(pump.identify_pwm_percent, 85);
        let fan = rows.iter().find(|r| r.fan_id == "openfan:ch00").unwrap();
        assert_eq!(fan.mode, IdentifyMode::Stop);
        assert_eq!(fan.identify_pwm_percent, 0);
        // …and the engine-facing snapshot carries the duty, not just the id.
        assert_eq!(t.snapshot().identify.get("hwmon:x:pwm1:PUMP"), Some(&85));
    }

    #[test]
    fn identify_stop_auto_restores_after_deadman() {
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        t.identify_hold(
            "openfan:ch00",
            0,
            IdentifyMode::Stop,
            Duration::from_secs(10),
        );
        assert!(t.snapshot().identify.contains_key("openfan:ch00"));

        clock.advance(Duration::from_secs(11));
        let cleared = t.sweep();
        assert_eq!(cleared.fans, vec!["openfan:ch00".to_string()]);
        assert!(t.snapshot().identify.is_empty());
    }

    #[test]
    fn identify_restore_removes_immediately() {
        let mut t = OverrideTable::new();
        t.identify_hold(
            "hwmon:nct6775:pwm1",
            0,
            IdentifyMode::Stop,
            Duration::from_secs(10),
        );
        t.identify_restore("hwmon:nct6775:pwm1");
        assert!(t.snapshot().identify.is_empty());
    }

    #[test]
    fn status_rows_report_remaining_ttl_sorted() {
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        t.take_override("ctrl-b", 60, Duration::from_secs(15));
        t.take_override("ctrl-a", 50, Duration::from_secs(15));
        t.identify_hold("fan-z", 0, IdentifyMode::Stop, Duration::from_secs(10));
        clock.advance(Duration::from_secs(4));

        let (ovr, ident) = t.status_rows();
        assert_eq!(ovr.len(), 2);
        assert_eq!(ovr[0].control_id, "ctrl-a"); // sorted
        assert_eq!(ovr[0].expires_in_secs, 11); // 15 - 4
        assert_eq!(ident[0].fan_id, "fan-z");
        assert_eq!(ident[0].expires_in_secs, 6); // 10 - 4
    }

    #[test]
    fn status_rows_exclude_entries_at_exact_expiry() {
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        t.take_override("ctrl-a", 50, Duration::from_secs(10));
        t.identify_hold("fan-z", 0, IdentifyMode::Stop, Duration::from_secs(10));

        // One nanosecond before the deadline: both still reported.
        clock.advance(Duration::from_secs(10) - Duration::from_nanos(1));
        let (ovr, ident) = t.status_rows();
        assert_eq!(
            ovr.len(),
            1,
            "override just before expiry must still be reported"
        );
        assert_eq!(
            ident.len(),
            1,
            "identify just before expiry must still be reported"
        );

        // Land EXACTLY on expires_at (now == expires_at): `now < expires_at` is
        // false, so both status_rows filters must EXCLUDE the entry. Guards the
        // `<` -> `<=` off-by-one in the remaining-TTL status report.
        clock.advance(Duration::from_nanos(1));
        let (ovr, ident) = t.status_rows();
        assert!(
            ovr.is_empty(),
            "override at exact expiry must not appear in status_rows"
        );
        assert!(
            ident.is_empty(),
            "identify at exact expiry must not appear in status_rows"
        );
    }

    #[test]
    fn sweep_only_clears_expired_entries() {
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        t.take_override("short", 50, Duration::from_secs(5));
        t.take_override("long", 60, Duration::from_secs(30));
        clock.advance(Duration::from_secs(6));

        let cleared = t.sweep();
        assert_eq!(cleared.controls, vec!["short".to_string()]);
        assert_eq!(t.snapshot().controls.get("long"), Some(&60));
    }

    #[test]
    fn clear_all_overrides_drops_controls_but_keeps_identify() {
        // DEC-189: a profile activation clears EVERY control-override (so an
        // override taken against the old profile cannot bleed onto a same-id
        // control in the newly-activated one) but must NOT touch identify-stops,
        // which are per physical fan and profile-independent.
        let mut t = OverrideTable::new();
        let g1 = t.take_override("cpu", 80, ttl());
        t.take_override("gpu", 40, ttl());
        t.identify_hold("openfan:ch00", 0, IdentifyMode::Stop, ttl());

        t.clear_all_overrides();

        let snap = t.snapshot();
        assert!(
            snap.controls.is_empty(),
            "every control-override must be cleared on activation"
        );
        assert!(
            snap.identify.contains_key("openfan:ch00"),
            "identify-stops survive a profile activation (per-fan, not per-profile)"
        );

        // The cleared control's old token is dead — a renew now sees nothing
        // live (re-take, never a fencing rejection)...
        assert_eq!(
            t.renew_override("cpu", g1.token, ttl()),
            Err(OverrideReject::NotActive)
        );
        // ...and a fresh take still issues a strictly-greater token: clearing
        // the table must not reset the monotonic fence and let a pre-clear token
        // become valid again.
        let g2 = t.take_override("cpu", 55, ttl());
        assert!(
            g2.token > g1.token,
            "the fencing counter is monotonic across a clear, not reset"
        );
    }
}
