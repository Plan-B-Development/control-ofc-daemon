//! Daemon-owned hardware-assessment cache + single-flight coordinator (DEC-207).
//!
//! One passive scan (`compute_hardware_assessment`, in the inventory handler)
//! produces a [`HardwareAssessment`] that every readiness / Super-I/O consumer
//! shares, so the expensive work (cache snapshot + `/sys` walk + `runtime.toml`
//! read + Super-I/O detect) runs once instead of three times.
//!
//! [`AssessmentCache`] holds the latest snapshot, mirrors its compact rollup into
//! the *same* `Arc` the 1 Hz poll reads (so the two caches cannot drift), and
//! serialises would-be scanners behind an async single-flight gate so a burst of
//! requests coalesces into ONE blocking scan. Read-only; a scan failure keeps the
//! last-good result and never touches fan control. The 1 Hz poll path never calls
//! into this module — it only clones the mirrored rollup.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::hwmon::readiness::{HardwareAssessment, ReadinessRollup};

/// How long a completed scan stays "fresh" for a non-forced request. This is a
/// coalescing window (so a burst of opens reuses one scan), NOT a staleness
/// guarantee — a `force` request always bypasses it.
pub const ASSESSMENT_TTL: Duration = Duration::from_secs(3);

/// Shared hardware-assessment cache + single-flight coordinator (DEC-207).
pub struct AssessmentCache {
    /// Latest completed assessment. Cheap clone-on-read (`Arc`); `None` until the
    /// first scan completes.
    current: Mutex<Option<Arc<HardwareAssessment>>>,
    /// The SAME `Arc` as `AppState.readiness_rollup` — [`Self::store`] keeps the
    /// 1 Hz poll mirror in lockstep with `current`, so the two never drift.
    rollup_mirror: Arc<Mutex<Option<ReadinessRollup>>>,
    /// Single-flight gate. An async mutex so a scan can be awaited while it is
    /// held without blocking a runtime worker. The poll path never touches this.
    scan_gate: tokio::sync::Mutex<()>,
    /// Monotonic scan id, assigned in [`Self::store`].
    generation: AtomicU64,
}

impl AssessmentCache {
    /// Build a cache that mirrors its rollup into `rollup_mirror`. Pass the same
    /// `Arc` stored on `AppState.readiness_rollup` so the poll path stays in sync.
    pub fn new(rollup_mirror: Arc<Mutex<Option<ReadinessRollup>>>) -> Self {
        Self {
            current: Mutex::new(None),
            rollup_mirror,
            scan_gate: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
        }
    }

    /// The current assessment if it is fresher than `ttl`. Clones the `Arc` and
    /// drops the guard immediately (never held across an `.await`).
    fn fresh_current(&self, ttl: Duration) -> Option<Arc<HardwareAssessment>> {
        let g = self.current.lock();
        g.as_ref().filter(|a| a.scanned_at.elapsed() < ttl).cloned()
    }

    /// The current assessment if its generation is newer than `gen` (a peer
    /// scanned after we decided to). Lets a forced caller coalesce onto a burst.
    fn current_if_newer_than(&self, gen: u64) -> Option<Arc<HardwareAssessment>> {
        let g = self.current.lock();
        g.as_ref().filter(|a| a.generation > gen).cloned()
    }

    /// The last-good assessment (`None` only before the first successful scan).
    fn last_good(&self) -> Option<Arc<HardwareAssessment>> {
        self.current.lock().clone()
    }

    /// Store a freshly-scanned assessment as the new `current` and mirror its
    /// rollup into the poll cache. THE single writer of both — assigns the
    /// generation, then sets `current` and `rollup_mirror`, each guard taken
    /// tightly (never nested, never across an `.await`), so the two caches cannot
    /// drift. Returns the stored `Arc`.
    fn store(&self, mut a: HardwareAssessment) -> Arc<HardwareAssessment> {
        a.generation = self
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let rollup = a.rollup.clone();
        let arc = Arc::new(a);
        *self.current.lock() = Some(arc.clone());
        *self.rollup_mirror.lock() = Some(rollup);
        arc
    }

    /// Return a fresh-enough shared assessment, running AT MOST one coalesced scan
    /// for any burst of callers.
    ///
    /// `scan` produces a new assessment (in production a `spawn_blocking` scan; in
    /// tests an injected one) or `None` if the scan failed — on failure the
    /// last-good assessment is returned unchanged (a scan failure never overwrites
    /// a good result and never fails control). `force` bypasses the freshness
    /// `ttl`. Returns `None` only when the scan fails AND no prior scan ever
    /// succeeded. **Never blocks the 1 Hz poll path.**
    pub async fn ensure_with<F, Fut>(
        &self,
        force: bool,
        ttl: Duration,
        scan: F,
    ) -> Option<Arc<HardwareAssessment>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<HardwareAssessment>>,
    {
        // Fast path: a fresh cached scan satisfies a non-forced caller with no
        // gate contention.
        if !force {
            if let Some(a) = self.fresh_current(ttl) {
                return Some(a);
            }
        }
        // Capture the generation BEFORE queuing so a forced caller can accept a
        // scan that completed after its intent (coalesces a burst of forced
        // refreshes into one scan).
        let gen_before = self.generation.load(Ordering::SeqCst);

        // Serialise would-be scanners on the async gate (held across the scan).
        let _guard = self.scan_gate.lock().await;

        // Double-check under the gate: a peer may have just scanned.
        if force {
            if let Some(a) = self.current_if_newer_than(gen_before) {
                return Some(a);
            }
        } else if let Some(a) = self.fresh_current(ttl) {
            return Some(a);
        }

        // We are the elected scanner: run the ONE scan.
        match scan().await {
            Some(a) => Some(self.store(a)),
            None => self.last_good(), // keep last-good; never overwrite / fail control
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwmon::readiness::ReadinessSeverity;
    use crate::hwmon::superio::SuperIoReport;
    use std::sync::atomic::AtomicUsize;

    fn empty_superio() -> SuperIoReport {
        SuperIoReport {
            arch_supported: true,
            chips: Vec::new(),
            acpi_conflict_drivers: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn fake_assessment() -> HardwareAssessment {
        HardwareAssessment::from_parts(Vec::new(), empty_superio())
    }

    fn cache() -> AssessmentCache {
        AssessmentCache::new(Arc::new(Mutex::new(None)))
    }

    #[tokio::test]
    async fn concurrent_callers_coalesce_to_one_scan() {
        let cache = Arc::new(cache());
        let scans = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let (c, s) = (cache.clone(), scans.clone());
            handles.push(tokio::spawn(async move {
                c.ensure_with(false, ASSESSMENT_TTL, || async move {
                    s.fetch_add(1, Ordering::SeqCst);
                    // Yield while holding the single-flight gate so the other
                    // callers queue on it — genuinely exercising the coalescing
                    // path rather than a serial fast-path race.
                    tokio::task::yield_now().await;
                    Some(fake_assessment())
                })
                .await
            }));
        }
        let mut gens = Vec::new();
        for h in handles {
            gens.push(h.await.unwrap().expect("assessment present").generation);
        }

        assert_eq!(
            scans.load(Ordering::SeqCst),
            1,
            "a burst of concurrent requests must launch exactly one scan"
        );
        assert!(
            gens.iter().all(|&g| g == gens[0]),
            "all coalesced callers must share one generation: {gens:?}"
        );
    }

    #[tokio::test]
    async fn fresh_cache_is_served_without_a_scan() {
        let cache = cache();
        let scans = Arc::new(AtomicUsize::new(0));
        let scan = |scans: Arc<AtomicUsize>| {
            move || {
                let scans = scans.clone();
                async move {
                    scans.fetch_add(1, Ordering::SeqCst);
                    Some(fake_assessment())
                }
            }
        };
        cache
            .ensure_with(false, ASSESSMENT_TTL, scan(scans.clone()))
            .await;
        cache
            .ensure_with(false, ASSESSMENT_TTL, scan(scans.clone()))
            .await;
        assert_eq!(
            scans.load(Ordering::SeqCst),
            1,
            "a second request within the TTL must reuse the cached scan"
        );
    }

    #[tokio::test]
    async fn force_bypasses_ttl_and_bumps_generation() {
        let cache = cache();
        let scans = Arc::new(AtomicUsize::new(0));
        let scan = |scans: Arc<AtomicUsize>| {
            move || {
                let scans = scans.clone();
                async move {
                    scans.fetch_add(1, Ordering::SeqCst);
                    Some(fake_assessment())
                }
            }
        };
        let a1 = cache
            .ensure_with(false, ASSESSMENT_TTL, scan(scans.clone()))
            .await
            .unwrap();
        let a2 = cache
            .ensure_with(true, ASSESSMENT_TTL, scan(scans.clone()))
            .await
            .unwrap();
        assert_eq!(scans.load(Ordering::SeqCst), 2, "force must run a new scan");
        assert!(
            a2.generation > a1.generation,
            "force must bump the generation"
        );
    }

    #[tokio::test]
    async fn scan_failure_keeps_last_good_and_never_panics() {
        let cache = cache();
        let good = cache
            .ensure_with(true, ASSESSMENT_TTL, || async { Some(fake_assessment()) })
            .await
            .unwrap();
        // A forced refresh whose scan fails must return the last-good assessment
        // (same generation), not None and not a panic.
        let after = cache
            .ensure_with(true, ASSESSMENT_TTL, || async { None })
            .await
            .expect("last-good returned on scan failure");
        assert_eq!(after.generation, good.generation);
    }

    #[tokio::test]
    async fn scan_failure_with_no_prior_scan_returns_none() {
        let cache = cache();
        let r = cache
            .ensure_with(true, ASSESSMENT_TTL, || async { None })
            .await;
        assert!(
            r.is_none(),
            "no last-good and a failed scan ⇒ None (handler → 503)"
        );
    }

    #[tokio::test]
    async fn store_mirrors_rollup_into_shared_arc() {
        let mirror = Arc::new(Mutex::new(None));
        let cache = AssessmentCache::new(mirror.clone());
        cache
            .ensure_with(true, ASSESSMENT_TTL, || async { Some(fake_assessment()) })
            .await
            .unwrap();
        // The poll path reads this same Arc — it must carry the stored rollup.
        let mirrored = mirror.lock().clone().expect("rollup mirrored");
        assert_eq!(mirrored.overall, ReadinessSeverity::Ok);
    }
}
