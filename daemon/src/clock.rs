//! Shared monotonic clock abstraction.
//!
//! Injectable so TTL/expiry logic (the hwmon write lease, the manual-override
//! and fan-identify deadman) can advance time deterministically in tests
//! instead of sleeping. Production uses [`SystemClock`] (real `Instant::now()`);
//! only tests inject a fake, advanceable clock.
//!
//! `Instant` is `CLOCK_MONOTONIC` on Linux: it never goes backwards and is
//! immune to wall-clock adjustments, and it pauses across system suspend —
//! which is the correct measure for a "revert if the controller is gone"
//! deadman (a suspended machine has no thermal load and the controlling GUI is
//! suspended too).

use std::time::Instant;

/// Monotonic clock source. The single point of time for all daemon expiry
/// logic so a fake clock can be injected in one place per subsystem.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Real monotonic clock — the production default.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}
