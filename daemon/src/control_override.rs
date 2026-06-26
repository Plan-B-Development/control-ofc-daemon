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
//! Stops a single fan for physical identification, auto-restoring after a short
//! deadman TTL. Floor-exempt by necessity (you must be able to stop a pump to
//! find it); bounded by the deadman and the 105°C thermal force. Restore is
//! simply removal — the engine recomputes the fan's curve value next tick, so
//! no prior-PWM is remembered.
//!
//! Safety ordering enforced by the engine tick:
//! `105°C force  >  identify-stop (floor-exempt)  >  override (floor-clamped)  >  curve`.

use std::collections::{HashMap, HashSet};
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

/// A live identify-stop on one fan.
#[derive(Debug, Clone)]
struct IdentifyEntry {
    expires_at: Instant,
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
    /// Fan member_ids forced to 0 (identify-stop). Floor-exempt at eval.
    pub identify_stop: HashSet<String>,
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
}

/// IDs cleared by a `sweep`, so the engine can reset their cross-tick state.
#[derive(Debug, Clone, Default)]
pub struct SweepCleared {
    /// Controls whose override lapsed — their hysteresis must be reset so the
    /// resumed curve re-anchors instead of step-rate-clamping from the pin.
    pub controls: Vec<String>,
    /// Fans whose identify-stop lapsed (no state reset needed — the control
    /// kept evaluating; only the final command was zeroed).
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

    /// Stop a fan for identification, auto-restoring after `ttl` (deadman). A
    /// repeat call refreshes the deadman.
    pub fn identify_stop(&mut self, fan_id: &str, ttl: Duration) {
        let now = self.clock.now();
        self.identify.insert(
            fan_id.to_string(),
            IdentifyEntry {
                expires_at: now + ttl,
            },
        );
    }

    /// Restore a fan immediately (remove the identify-stop). Idempotent.
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
            identify_stop: self
                .identify
                .iter()
                .filter(|(_, e)| now < e.expires_at)
                .map(|(id, _)| id.clone())
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
        t.identify_stop("fan-1", ttl());

        // One tick before the deadline: both still live.
        clock.advance(ttl() - Duration::from_nanos(1));
        assert_eq!(t.snapshot().controls.get("ctrl-a"), Some(&70));
        assert!(t.snapshot().identify_stop.contains("fan-1"));

        // Land EXACTLY on `expires_at` (now == expires_at, so `now < expires_at`
        // is false): the entry is already expired everywhere it is read.
        clock.advance(Duration::from_nanos(1));
        let snap = t.snapshot();
        assert!(
            snap.controls.is_empty() && snap.identify_stop.is_empty(),
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
        // indefinitely; the 105°C force is the safety net, not a hard cap.
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

    #[test]
    fn identify_stop_auto_restores_after_deadman() {
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        t.identify_stop("openfan:ch00", Duration::from_secs(10));
        assert!(t.snapshot().identify_stop.contains("openfan:ch00"));

        clock.advance(Duration::from_secs(11));
        let cleared = t.sweep();
        assert_eq!(cleared.fans, vec!["openfan:ch00".to_string()]);
        assert!(t.snapshot().identify_stop.is_empty());
    }

    #[test]
    fn identify_restore_removes_immediately() {
        let mut t = OverrideTable::new();
        t.identify_stop("hwmon:nct6775:pwm1", Duration::from_secs(10));
        t.identify_restore("hwmon:nct6775:pwm1");
        assert!(t.snapshot().identify_stop.is_empty());
    }

    #[test]
    fn status_rows_report_remaining_ttl_sorted() {
        let clock = ManualClock::new();
        let mut t = OverrideTable::with_clock(clock.clone());
        t.take_override("ctrl-b", 60, Duration::from_secs(15));
        t.take_override("ctrl-a", 50, Duration::from_secs(15));
        t.identify_stop("fan-z", Duration::from_secs(10));
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
        t.identify_stop("fan-z", Duration::from_secs(10));

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
}
