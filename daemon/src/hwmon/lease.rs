//! Single-writer arbitration for hwmon PWM writes (DEC-197).
//!
//! At most one writer holds the arbiter at a time; reads and discovery need no
//! token. This is a daemon-INTERNAL arbiter between three in-process actors
//! ([`HwmonWriter`]) — the pre-2.0.0 client "lease" protocol was fully retired
//! (DEC-165), so there is no external/GUI holder. A short TTL bounds a
//! force-take eviction and keeps a token valid across the DEC-154 per-header
//! re-lock; the profile engine renews per tick. (The `LeaseManager`/`HwmonLease`
//! names are retained to keep the diff focused — read them as "write arbiter" /
//! "write token".)

use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default token TTL (60 seconds).
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(60);

// Monotonic clock seam, injectable so expiry tests can advance time
// deterministically instead of sleeping (audit P2-F). Promoted to the shared
// `crate::clock` module so the manual-override deadman reuses the same
// abstraction; re-exported here to keep `hwmon::lease::Clock` referencing intact.
pub use crate::clock::{Clock, SystemClock};

/// The three in-process actors that can hold the hwmon write arbiter (DEC-197).
/// Replaces the pre-2.0.0 free-form `owner_hint` strings — there is no client
/// lease anymore (DEC-165), only these daemon-internal writers. `Display`
/// renders the historical owner strings so log lines are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwmonWriter {
    /// The profile engine's 1 Hz control tick — the routine writer.
    Engine,
    /// A hardware-verify handler's controlled test/restore writes.
    Verify,
    /// The thermal-safety force (thermal emergency / no-CPU-sensor fallback).
    ThermalSafety,
}

impl std::fmt::Display for HwmonWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Engine => "profile-engine",
            Self::Verify => "verify",
            Self::ThermalSafety => "thermal-safety",
        })
    }
}

/// A token granting exclusive write permission for hwmon PWM outputs.
#[derive(Debug, Clone)]
pub struct HwmonLease {
    /// Opaque token identifier.
    pub lease_id: String,
    /// Which in-process writer holds this token.
    pub owner: HwmonWriter,
    /// When this token expires.
    pub expires_at: Instant,
}

/// Error from arbiter operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// A token is already held by another writer.
    AlreadyHeld {
        owner: HwmonWriter,
        ttl_seconds: u64,
    },
    /// The provided token id does not match the active token.
    InvalidLease,
    /// The provided token id matches the active token, but the token's TTL
    /// has elapsed. Distinct from `InvalidLease` (id mismatch) so callers
    /// can tell "you sent the wrong id" from "your id is right but stale".
    Expired,
    /// No token is currently held (for release).
    NoLease,
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyHeld { owner, ttl_seconds } => write!(
                f,
                "lease already held by '{owner}' (expires in {ttl_seconds}s)"
            ),
            Self::InvalidLease => write!(f, "invalid lease id"),
            Self::Expired => write!(f, "lease expired"),
            Self::NoLease => write!(f, "no active lease to release"),
        }
    }
}

/// Manages the single hwmon write token.
pub struct LeaseManager {
    active: Option<HwmonLease>,
    ttl: Duration,
    next_id: u64,
    clock: Arc<dyn Clock>,
}

impl LeaseManager {
    /// Create a new manager with default TTL and the real system clock.
    pub fn new() -> Self {
        Self::with_clock(DEFAULT_LEASE_TTL, Arc::new(SystemClock))
    }

    /// Create a manager with a custom TTL and the real system clock.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self::with_clock(ttl, Arc::new(SystemClock))
    }

    /// Create a manager with an injected clock (tests use a fake,
    /// advanceable clock to exercise expiry deterministically — audit P2-F).
    pub fn with_clock(ttl: Duration, clock: Arc<dyn Clock>) -> Self {
        Self {
            active: None,
            ttl,
            next_id: 1,
            clock,
        }
    }

    /// Attempt to take the token.
    ///
    /// Returns the new token on success, or `LeaseError::AlreadyHeld` if
    /// a valid (non-expired) token already exists.
    pub fn take_lease(&mut self, owner: HwmonWriter) -> Result<HwmonLease, LeaseError> {
        let now = self.clock.now();
        // Clean up expired token first
        if let Some(ref lease) = self.active {
            if now < lease.expires_at {
                return Err(LeaseError::AlreadyHeld {
                    owner: lease.owner,
                    ttl_seconds: lease.expires_at.saturating_duration_since(now).as_secs(),
                });
            }
        }

        let lease_id = format!("lease-{}", self.next_id);
        self.next_id += 1;

        let lease = HwmonLease {
            lease_id,
            owner,
            expires_at: now + self.ttl,
        };

        self.active = Some(lease.clone());
        Ok(lease)
    }

    /// Release the token. The provided `lease_id` must match the active token.
    pub fn release_lease(&mut self, lease_id: &str) -> Result<(), LeaseError> {
        match &self.active {
            Some(lease) if lease.lease_id == lease_id => {
                self.active = None;
                Ok(())
            }
            Some(_) => Err(LeaseError::InvalidLease),
            None => Err(LeaseError::NoLease),
        }
    }

    /// Validate that the provided `lease_id` matches the active, non-expired token.
    pub fn validate_lease(&self, lease_id: &str) -> Result<(), LeaseError> {
        let now = self.clock.now();
        match &self.active {
            Some(lease) if lease.lease_id == lease_id && now < lease.expires_at => Ok(()),
            Some(lease) if lease.lease_id == lease_id => Err(LeaseError::Expired),
            _ => Err(LeaseError::InvalidLease),
        }
    }

    /// Renew the token, extending the TTL. The provided `lease_id` must match.
    pub fn renew_lease(&mut self, lease_id: &str) -> Result<HwmonLease, LeaseError> {
        let now = self.clock.now();
        match &mut self.active {
            Some(lease) if lease.lease_id == lease_id && now < lease.expires_at => {
                lease.expires_at = now + self.ttl;
                Ok(lease.clone())
            }
            Some(lease) if lease.lease_id == lease_id => Err(LeaseError::Expired),
            _ => Err(LeaseError::InvalidLease),
        }
    }

    /// Force-take the token, evicting any current holder. Always succeeds.
    ///
    /// The two safety-critical daemon writers use this to preempt: a hardware
    /// verify takes it as [`HwmonWriter::Verify`], and the thermal-safety force
    /// takes it as [`HwmonWriter::ThermalSafety`] — the latter mid-verify, so the
    /// evicted verify's in-flight restore write then fails `validate_lease`
    /// (`InvalidLease`) rather than overwriting the emergency 100 % (DEC-197,
    /// invariant preserved from the pre-refactor lease). The profile engine
    /// never force-takes; it yields via `take_lease`.
    pub fn force_take_lease(&mut self, owner: HwmonWriter) -> HwmonLease {
        let now = self.clock.now();
        let lease_id = format!("lease-{}", self.next_id);
        self.next_id += 1;

        let lease = HwmonLease {
            lease_id,
            owner,
            expires_at: now + self.ttl,
        };

        if let Some(ref old) = self.active {
            log::info!(
                "hwmon write arbiter force-taken: evicting '{}' for '{}'",
                old.owner,
                owner
            );
        }

        self.active = Some(lease.clone());
        lease
    }

    /// Get the current active token (if any and non-expired).
    pub fn active_lease(&self) -> Option<&HwmonLease> {
        let now = self.clock.now();
        self.active.as_ref().filter(|lease| now < lease.expires_at)
    }
}

impl Default for LeaseManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake clock for deterministic expiry tests (audit P2-F): advance virtual
    /// time instead of `thread::sleep`.
    struct TestClock {
        now: std::sync::Mutex<Instant>,
    }

    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: std::sync::Mutex::new(Instant::now()),
            })
        }
        fn advance(&self, d: Duration) {
            *self.now.lock().expect("clock mutex") += d;
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            *self.now.lock().expect("clock mutex")
        }
    }

    #[test]
    fn take_lease_succeeds_when_no_lease() {
        let mut mgr = LeaseManager::new();
        let lease = mgr.take_lease(HwmonWriter::Engine).unwrap();
        assert_eq!(lease.owner, HwmonWriter::Engine);
        assert!(lease.lease_id.starts_with("lease-"));
    }

    #[test]
    fn take_lease_fails_when_lease_held() {
        let mut mgr = LeaseManager::new();
        mgr.take_lease(HwmonWriter::Engine).unwrap();

        let err = mgr.take_lease(HwmonWriter::Verify).unwrap_err();
        match err {
            LeaseError::AlreadyHeld { owner, .. } => {
                assert_eq!(owner, HwmonWriter::Engine);
            }
            _ => panic!("expected AlreadyHeld"),
        }
    }

    #[test]
    fn take_lease_succeeds_after_expiry() {
        let clock = TestClock::new();
        let mut mgr = LeaseManager::with_clock(Duration::from_secs(60), clock.clone());
        mgr.take_lease(HwmonWriter::Engine).unwrap();

        clock.advance(Duration::from_secs(61)); // past TTL

        let lease = mgr.take_lease(HwmonWriter::Verify).unwrap();
        assert_eq!(lease.owner, HwmonWriter::Verify);
    }

    #[test]
    fn release_lease_succeeds_with_correct_id() {
        let mut mgr = LeaseManager::new();
        let lease = mgr.take_lease(HwmonWriter::Engine).unwrap();
        let id = lease.lease_id.clone();

        mgr.release_lease(&id).unwrap();
        assert!(mgr.active_lease().is_none());
    }

    #[test]
    fn release_lease_fails_with_wrong_id() {
        let mut mgr = LeaseManager::new();
        mgr.take_lease(HwmonWriter::Engine).unwrap();

        let err = mgr.release_lease("wrong-id").unwrap_err();
        assert_eq!(err, LeaseError::InvalidLease);
    }

    #[test]
    fn release_lease_fails_when_no_lease() {
        let mut mgr = LeaseManager::new();
        let err = mgr.release_lease("any").unwrap_err();
        assert_eq!(err, LeaseError::NoLease);
    }

    #[test]
    fn validate_lease_succeeds() {
        let mut mgr = LeaseManager::new();
        let lease = mgr.take_lease(HwmonWriter::Engine).unwrap();
        mgr.validate_lease(&lease.lease_id).unwrap();
    }

    #[test]
    fn validate_lease_fails_when_expired() {
        let clock = TestClock::new();
        let mut mgr = LeaseManager::with_clock(Duration::from_secs(60), clock.clone());
        let lease = mgr.take_lease(HwmonWriter::Engine).unwrap();

        clock.advance(Duration::from_secs(61));

        let err = mgr.validate_lease(&lease.lease_id).unwrap_err();
        assert_eq!(err, LeaseError::Expired);
    }

    #[test]
    fn validate_lease_fails_with_wrong_id() {
        let mut mgr = LeaseManager::new();
        mgr.take_lease(HwmonWriter::Engine).unwrap();

        let err = mgr.validate_lease("wrong-id").unwrap_err();
        assert_eq!(err, LeaseError::InvalidLease);
    }

    #[test]
    fn validate_lease_distinguishes_wrong_id_from_expired() {
        // T2 (test-tests audit): wrong-id and expired must be observably distinct.
        // Previously both returned LeaseError::InvalidLease, so the match guard
        // could be flipped (== ↔ !=) without any test noticing.
        let clock = TestClock::new();
        let mut mgr = LeaseManager::with_clock(Duration::from_secs(60), clock.clone());
        let lease = mgr.take_lease(HwmonWriter::Engine).unwrap();
        let valid_id = lease.lease_id.clone();

        // Path A: token still valid, but caller sends the wrong id.
        let wrong_err = mgr.validate_lease("not-the-real-id").unwrap_err();
        assert_eq!(wrong_err, LeaseError::InvalidLease);

        // Path B: caller sends the right id, but TTL has elapsed.
        clock.advance(Duration::from_secs(61));
        let expired_err = mgr.validate_lease(&valid_id).unwrap_err();
        assert_eq!(expired_err, LeaseError::Expired);

        // The two paths must yield distinct variants — locks down the guard.
        assert_ne!(wrong_err, expired_err);
    }

    #[test]
    fn renew_lease_distinguishes_wrong_id_from_expired() {
        let clock = TestClock::new();
        let mut mgr = LeaseManager::with_clock(Duration::from_secs(60), clock.clone());
        let lease = mgr.take_lease(HwmonWriter::Engine).unwrap();
        let valid_id = lease.lease_id.clone();

        let wrong_err = mgr.renew_lease("not-the-real-id").unwrap_err();
        assert_eq!(wrong_err, LeaseError::InvalidLease);

        clock.advance(Duration::from_secs(61));
        let expired_err = mgr.renew_lease(&valid_id).unwrap_err();
        assert_eq!(expired_err, LeaseError::Expired);

        assert_ne!(wrong_err, expired_err);
    }

    #[test]
    fn active_lease_returns_none_when_expired() {
        let clock = TestClock::new();
        let mut mgr = LeaseManager::with_clock(Duration::from_secs(60), clock.clone());
        mgr.take_lease(HwmonWriter::Engine).unwrap();

        clock.advance(Duration::from_secs(61));

        assert!(mgr.active_lease().is_none());
    }

    #[test]
    fn lease_ids_are_unique() {
        let clock = TestClock::new();
        let mut mgr = LeaseManager::with_clock(Duration::from_secs(60), clock.clone());
        let l1 = mgr.take_lease(HwmonWriter::Engine).unwrap();

        clock.advance(Duration::from_secs(61)); // expire so the second take succeeds

        let l2 = mgr.take_lease(HwmonWriter::Verify).unwrap();
        assert_ne!(l1.lease_id, l2.lease_id);
    }

    #[test]
    fn force_take_lease_ids_are_unique() {
        // `lease_ids_are_unique` only exercises take_lease's counter; the
        // increment inside force_take_lease was untested, so `next_id += 1`
        // could become a no-op and hand out duplicate ids undetected
        // (/test-tests audit P2). force_take_lease is time-independent — it
        // always succeeds and evicts — so no sleeps are needed.
        let mut mgr = LeaseManager::new();
        let l1 = mgr.force_take_lease(HwmonWriter::Engine);
        let l2 = mgr.force_take_lease(HwmonWriter::Verify);
        assert_ne!(l1.lease_id, l2.lease_id);
        // Counter is shared with take_lease — a third id must differ from both.
        let l3 = mgr.force_take_lease(HwmonWriter::ThermalSafety).lease_id;
        assert_ne!(l3, l1.lease_id);
        assert_ne!(l3, l2.lease_id);
    }

    #[test]
    fn force_take_invalidates_the_evicted_holders_token() {
        // DEC-197 invariant (c): a thermal-safety force-take must invalidate the
        // evicted writer's token, so a verify's in-flight restore write
        // (`validate_lease` → `set_pwm`) fails `InvalidLease` instead of
        // overwriting the emergency 100 %. This is the single behaviour the
        // typed-arbiter refactor most must not lose.
        let mut mgr = LeaseManager::new();
        let verify = mgr.force_take_lease(HwmonWriter::Verify);
        assert!(mgr.validate_lease(&verify.lease_id).is_ok());

        // Thermal fires mid-verify and force-takes the arbiter.
        let thermal = mgr.force_take_lease(HwmonWriter::ThermalSafety);
        // ...and the new thermal token IS usable — guards against a degenerate
        // force-take that clears state instead of replacing it.
        assert!(mgr.validate_lease(&thermal.lease_id).is_ok());

        // The verify's now-stale token no longer validates → its restore is refused.
        assert_eq!(
            mgr.validate_lease(&verify.lease_id),
            Err(LeaseError::InvalidLease)
        );
    }

    #[test]
    fn lease_error_display_strings() {
        // Only AlreadyHeld reaches the HTTP envelope via `e.to_string()` (the
        // other variants use hardcoded handler messages), so its Display format
        // was unpinned (/test-tests audit P3). Assert all four to keep Display
        // and the handler strings in lockstep.
        assert_eq!(
            LeaseError::AlreadyHeld {
                owner: HwmonWriter::Engine,
                ttl_seconds: 42,
            }
            .to_string(),
            "lease already held by 'profile-engine' (expires in 42s)",
        );
        assert_eq!(LeaseError::InvalidLease.to_string(), "invalid lease id");
        assert_eq!(LeaseError::Expired.to_string(), "lease expired");
        assert_eq!(
            LeaseError::NoLease.to_string(),
            "no active lease to release"
        );
    }

    #[test]
    fn hwmon_writer_display_matches_historical_owner_strings() {
        // The Display strings are load-bearing for log continuity; pin them.
        assert_eq!(HwmonWriter::Engine.to_string(), "profile-engine");
        assert_eq!(HwmonWriter::Verify.to_string(), "verify");
        assert_eq!(HwmonWriter::ThermalSafety.to_string(), "thermal-safety");
    }

    #[test]
    fn renew_lease_extends_ttl() {
        let clock = TestClock::new();
        let mut mgr = LeaseManager::with_clock(Duration::from_secs(60), clock.clone());
        let lease = mgr.take_lease(HwmonWriter::Engine).unwrap();
        let id = lease.lease_id.clone();

        // Advance almost to the original expiry, then renew → the window resets.
        clock.advance(Duration::from_secs(59));
        mgr.renew_lease(&id).unwrap();

        // Past the ORIGINAL 60 s expiry the token is still valid (renew extended it).
        clock.advance(Duration::from_secs(10));
        assert!(mgr.validate_lease(&id).is_ok());
    }

    #[test]
    fn renew_lease_fails_with_wrong_id() {
        let mut mgr = LeaseManager::new();
        mgr.take_lease(HwmonWriter::Engine).unwrap();
        let err = mgr.renew_lease("wrong").unwrap_err();
        assert_eq!(err, LeaseError::InvalidLease);
    }

    #[test]
    fn renew_lease_fails_when_expired() {
        let clock = TestClock::new();
        let mut mgr = LeaseManager::with_clock(Duration::from_secs(60), clock.clone());
        let lease = mgr.take_lease(HwmonWriter::Engine).unwrap();
        let id = lease.lease_id.clone();
        clock.advance(Duration::from_secs(61));
        let err = mgr.renew_lease(&id).unwrap_err();
        assert_eq!(err, LeaseError::Expired);
    }
}
