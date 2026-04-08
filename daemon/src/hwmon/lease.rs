//! Lease/token model for exclusive hwmon PWM write access.
//!
//! At most one active lease exists at a time. Reads and discovery
//! do not require a lease. Leases expire after a configurable TTL.

use std::time::{Duration, Instant};

/// Default lease TTL (60 seconds).
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(60);

/// A lease granting exclusive write permission for hwmon PWM outputs.
#[derive(Debug, Clone)]
pub struct HwmonLease {
    /// Opaque lease identifier.
    pub lease_id: String,
    /// Optional human-readable owner hint (e.g. "gui-session-1").
    pub owner_hint: String,
    /// When this lease was created.
    pub created_at: Instant,
    /// When this lease expires.
    pub expires_at: Instant,
}

impl HwmonLease {
    /// Check whether this lease has expired.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// Remaining TTL in seconds (0 if expired).
    pub fn ttl_seconds(&self) -> u64 {
        self.expires_at
            .saturating_duration_since(Instant::now())
            .as_secs()
    }
}

/// Error from lease operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// A lease is already held by another client.
    AlreadyHeld {
        owner_hint: String,
        ttl_seconds: u64,
    },
    /// The provided lease ID does not match the active lease.
    InvalidLease,
    /// No lease is currently held (for release).
    NoLease,
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyHeld {
                owner_hint,
                ttl_seconds,
            } => write!(
                f,
                "lease already held by '{owner_hint}' (expires in {ttl_seconds}s)"
            ),
            Self::InvalidLease => write!(f, "invalid or expired lease"),
            Self::NoLease => write!(f, "no active lease to release"),
        }
    }
}

/// Manages the single hwmon write lease.
pub struct LeaseManager {
    active: Option<HwmonLease>,
    ttl: Duration,
    next_id: u64,
}

impl LeaseManager {
    /// Create a new lease manager with default TTL.
    pub fn new() -> Self {
        Self {
            active: None,
            ttl: DEFAULT_LEASE_TTL,
            next_id: 1,
        }
    }

    /// Create a lease manager with a custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            active: None,
            ttl,
            next_id: 1,
        }
    }

    /// Attempt to take the lease.
    ///
    /// Returns the new lease on success, or `LeaseError::AlreadyHeld` if
    /// a valid (non-expired) lease already exists.
    pub fn take_lease(&mut self, owner_hint: &str) -> Result<HwmonLease, LeaseError> {
        // Clean up expired lease first
        if let Some(ref lease) = self.active {
            if !lease.is_expired() {
                return Err(LeaseError::AlreadyHeld {
                    owner_hint: lease.owner_hint.clone(),
                    ttl_seconds: lease.ttl_seconds(),
                });
            }
        }

        let now = Instant::now();
        let lease_id = format!("lease-{}", self.next_id);
        self.next_id += 1;

        let lease = HwmonLease {
            lease_id,
            owner_hint: owner_hint.to_string(),
            created_at: now,
            expires_at: now + self.ttl,
        };

        self.active = Some(lease.clone());
        Ok(lease)
    }

    /// Release the lease. The provided `lease_id` must match the active lease.
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

    /// Validate that the provided `lease_id` matches the active, non-expired lease.
    pub fn validate_lease(&self, lease_id: &str) -> Result<(), LeaseError> {
        match &self.active {
            Some(lease) if lease.lease_id == lease_id && !lease.is_expired() => Ok(()),
            Some(lease) if lease.lease_id == lease_id => {
                // Lease matched but expired
                Err(LeaseError::InvalidLease)
            }
            _ => Err(LeaseError::InvalidLease),
        }
    }

    /// Renew the lease, extending the TTL. The provided `lease_id` must match.
    pub fn renew_lease(&mut self, lease_id: &str) -> Result<HwmonLease, LeaseError> {
        match &mut self.active {
            Some(lease) if lease.lease_id == lease_id && !lease.is_expired() => {
                lease.expires_at = Instant::now() + self.ttl;
                Ok(lease.clone())
            }
            Some(lease) if lease.lease_id == lease_id => Err(LeaseError::InvalidLease),
            _ => Err(LeaseError::InvalidLease),
        }
    }

    /// Force-take the lease, evicting any current holder.
    ///
    /// Used when a higher-priority client (e.g. GUI) needs the lease and
    /// the current holder (e.g. profile engine) must yield. Always succeeds.
    pub fn force_take_lease(&mut self, owner_hint: &str) -> HwmonLease {
        let now = Instant::now();
        let lease_id = format!("lease-{}", self.next_id);
        self.next_id += 1;

        let lease = HwmonLease {
            lease_id,
            owner_hint: owner_hint.to_string(),
            created_at: now,
            expires_at: now + self.ttl,
        };

        if let Some(ref old) = self.active {
            log::info!(
                "Lease force-taken: evicting '{}' for '{}'",
                old.owner_hint,
                owner_hint
            );
        }

        self.active = Some(lease.clone());
        lease
    }

    /// Get the current active lease (if any and non-expired).
    pub fn active_lease(&self) -> Option<&HwmonLease> {
        self.active.as_ref().filter(|lease| !lease.is_expired())
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

    #[test]
    fn take_lease_succeeds_when_no_lease() {
        let mut mgr = LeaseManager::new();
        let lease = mgr.take_lease("gui").unwrap();
        assert_eq!(lease.owner_hint, "gui");
        assert!(lease.lease_id.starts_with("lease-"));
    }

    #[test]
    fn take_lease_fails_when_lease_held() {
        let mut mgr = LeaseManager::new();
        mgr.take_lease("gui").unwrap();

        let err = mgr.take_lease("other-gui").unwrap_err();
        match err {
            LeaseError::AlreadyHeld { owner_hint, .. } => {
                assert_eq!(owner_hint, "gui");
            }
            _ => panic!("expected AlreadyHeld"),
        }
    }

    #[test]
    fn take_lease_succeeds_after_expiry() {
        let mut mgr = LeaseManager::with_ttl(Duration::from_millis(1));
        mgr.take_lease("gui").unwrap();

        // Wait for lease to expire
        std::thread::sleep(Duration::from_millis(5));

        let lease = mgr.take_lease("new-gui").unwrap();
        assert_eq!(lease.owner_hint, "new-gui");
    }

    #[test]
    fn release_lease_succeeds_with_correct_id() {
        let mut mgr = LeaseManager::new();
        let lease = mgr.take_lease("gui").unwrap();
        let id = lease.lease_id.clone();

        mgr.release_lease(&id).unwrap();
        assert!(mgr.active_lease().is_none());
    }

    #[test]
    fn release_lease_fails_with_wrong_id() {
        let mut mgr = LeaseManager::new();
        mgr.take_lease("gui").unwrap();

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
        let lease = mgr.take_lease("gui").unwrap();
        mgr.validate_lease(&lease.lease_id).unwrap();
    }

    #[test]
    fn validate_lease_fails_when_expired() {
        let mut mgr = LeaseManager::with_ttl(Duration::from_millis(1));
        let lease = mgr.take_lease("gui").unwrap();

        std::thread::sleep(Duration::from_millis(5));

        let err = mgr.validate_lease(&lease.lease_id).unwrap_err();
        assert_eq!(err, LeaseError::InvalidLease);
    }

    #[test]
    fn validate_lease_fails_with_wrong_id() {
        let mut mgr = LeaseManager::new();
        mgr.take_lease("gui").unwrap();

        let err = mgr.validate_lease("wrong-id").unwrap_err();
        assert_eq!(err, LeaseError::InvalidLease);
    }

    #[test]
    fn active_lease_returns_none_when_expired() {
        let mut mgr = LeaseManager::with_ttl(Duration::from_millis(1));
        mgr.take_lease("gui").unwrap();

        std::thread::sleep(Duration::from_millis(5));

        assert!(mgr.active_lease().is_none());
    }

    #[test]
    fn lease_ids_are_unique() {
        let mut mgr = LeaseManager::with_ttl(Duration::from_millis(1));
        let l1 = mgr.take_lease("a").unwrap();

        std::thread::sleep(Duration::from_millis(5));

        let l2 = mgr.take_lease("b").unwrap();
        assert_ne!(l1.lease_id, l2.lease_id);
    }

    #[test]
    fn renew_lease_extends_ttl() {
        let mut mgr = LeaseManager::with_ttl(Duration::from_secs(60));
        let lease = mgr.take_lease("gui").unwrap();
        let id = lease.lease_id.clone();

        std::thread::sleep(Duration::from_millis(10));

        let renewed = mgr.renew_lease(&id).unwrap();
        // TTL should be close to full again
        assert!(renewed.ttl_seconds() >= 58);
    }

    #[test]
    fn renew_lease_fails_with_wrong_id() {
        let mut mgr = LeaseManager::new();
        mgr.take_lease("gui").unwrap();
        let err = mgr.renew_lease("wrong").unwrap_err();
        assert_eq!(err, LeaseError::InvalidLease);
    }

    #[test]
    fn renew_lease_fails_when_expired() {
        let mut mgr = LeaseManager::with_ttl(Duration::from_millis(1));
        let lease = mgr.take_lease("gui").unwrap();
        let id = lease.lease_id.clone();
        std::thread::sleep(Duration::from_millis(5));
        let err = mgr.renew_lease(&id).unwrap_err();
        assert_eq!(err, LeaseError::InvalidLease);
    }

    #[test]
    fn ttl_seconds_reports_remaining() {
        let mut mgr = LeaseManager::with_ttl(Duration::from_secs(60));
        let lease = mgr.take_lease("gui").unwrap();
        // Should be close to 60 (allow some slack for test execution)
        assert!((55..=60).contains(&lease.ttl_seconds()));
    }
}
