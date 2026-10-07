//! One outstanding filesystem operation per key, with a hung holder skipped
//! rather than waited on (FFA-b).
//!
//! Profile reads touch directories any local user can register, and a
//! directory on a FUSE or network mount can stop answering. `spawn_blocking`
//! cannot cancel a read that hangs, so its thread stays parked until the
//! filesystem answers. A [`Gate`] keeps that to one thread per key: the hung
//! operation holds the key, and a later caller that finds the key held for at
//! least `hung_after` gives up on it at once instead of parking a second
//! thread. A caller that finds a young holder waits for it until the holder
//! is `hung_after` old, and never past its own deadline, so healthy concurrent
//! operations on one key just run one after the other. A holder can be made
//! strict ([`Claim::make_strict`]): it is waited for until the deadline and
//! never skipped, for a key an ordered lookup must not go past.
//! [`Gate::try_claim`] never waits.
//!
//! Callers run on the blocking pool: [`Gate::claim`] can wait on a condvar.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

/// A key's holder: since when, and whether it may be skipped.
#[derive(Clone, Copy)]
struct Holder {
    since: Instant,
    strict: bool,
}

/// At most one claim per key; see the module docs.
pub struct Gate<K> {
    held: Mutex<Option<HashMap<K, Holder>>>,
    released: Condvar,
}

/// Why [`Gate::claim`] did not claim a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Held at least `hung_after` by a holder that may be skipped: go on
    /// without it.
    Hung,
    /// Still held at the deadline. `strict` when the holder may not be skipped
    /// ([`Claim::make_strict`]): an ordered lookup must then stop rather than
    /// go past the key.
    Busy { strict: bool },
}

impl<K: Eq + Hash + Clone> Gate<K> {
    /// An empty gate, usable in a `static`.
    pub const fn new() -> Self {
        Self {
            held: Mutex::new(None),
            released: Condvar::new(),
        }
    }

    /// Claim `key`, waiting for its holder to release it: a holder that may be
    /// skipped until it has held the key `hung_after`, a strict holder until
    /// `deadline`. Never waits past `deadline`.
    pub fn claim(
        &self,
        key: K,
        hung_after: Duration,
        deadline: Instant,
    ) -> Result<Claim<'_, K>, Refusal> {
        let mut held = self.held.lock();
        loop {
            let map = held.get_or_insert_with(HashMap::new);
            let Some(holder) = map.get(&key).copied() else {
                map.insert(
                    key.clone(),
                    Holder {
                        since: Instant::now(),
                        strict: false,
                    },
                );
                return Ok(Claim { gate: self, key });
            };
            let hung_at = holder.since + hung_after;
            let now = Instant::now();
            if !holder.strict && now >= hung_at {
                return Err(Refusal::Hung);
            }
            if now >= deadline {
                return Err(Refusal::Busy {
                    strict: holder.strict,
                });
            }
            let limit = if holder.strict {
                deadline
            } else {
                hung_at.min(deadline)
            };
            // Woken by a release, or at the limit; either way look again.
            self.released.wait_until(&mut held, limit);
        }
    }

    /// Claim `key` only if no one holds it; never waits.
    pub fn try_claim(&self, key: K) -> Option<Claim<'_, K>> {
        let mut held = self.held.lock();
        let map = held.get_or_insert_with(HashMap::new);
        if map.contains_key(&key) {
            return None;
        }
        map.insert(
            key.clone(),
            Holder {
                since: Instant::now(),
                strict: false,
            },
        );
        Some(Claim { gate: self, key })
    }

    /// How long `key` has been held, or `None` when it is free.
    pub fn held_for(&self, key: &K) -> Option<Duration> {
        self.held
            .lock()
            .as_ref()
            .and_then(|map| map.get(key))
            .map(|holder| holder.since.elapsed())
    }

    /// Whether `key` is held now. Tests only: the answer can be stale at once.
    #[cfg(test)]
    pub(crate) fn is_held(&self, key: &K) -> bool {
        self.held
            .lock()
            .as_ref()
            .is_some_and(|map| map.contains_key(key))
    }
}

impl<K: Eq + Hash + Clone> Default for Gate<K> {
    fn default() -> Self {
        Self::new()
    }
}

/// A held key, released when dropped — whenever the operation really ends.
pub struct Claim<'a, K: Eq + Hash + Clone> {
    gate: &'a Gate<K>,
    key: K,
}

impl<K: Eq + Hash + Clone> Claim<'_, K> {
    /// From now on, waiters wait for this holder until their deadline and are
    /// never told to skip it.
    pub fn make_strict(&self) {
        if let Some(holder) = self
            .gate
            .held
            .lock()
            .as_mut()
            .and_then(|map| map.get_mut(&self.key))
        {
            holder.strict = true;
        }
    }
}

impl<K: Eq + Hash + Clone> Drop for Claim<'_, K> {
    fn drop(&mut self) {
        if let Some(map) = self.gate.held.lock().as_mut() {
            map.remove(&self.key);
        }
        self.gate.released.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const HUNG: Duration = Duration::from_millis(200);

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn take(gate: &Gate<u32>, key: u32) -> Claim<'_, u32> {
        let Ok(claim) = gate.claim(key, HUNG, far()) else {
            panic!("key {key} should be free");
        };
        claim
    }

    #[test]
    fn a_free_key_is_claimed_and_released_on_drop() {
        let gate: Gate<u32> = Gate::new();
        let claim = take(&gate, 1);
        assert!(gate.claim(2, HUNG, far()).is_ok(), "keys are independent");
        assert!(
            gate.try_claim(1).is_none(),
            "try_claim never takes a held key"
        );
        drop(claim);
        assert!(gate.claim(1, HUNG, far()).is_ok(), "released on drop");
        assert!(gate.try_claim(1).is_some(), "try_claim takes a free key");
    }

    #[test]
    fn a_young_holder_is_waited_for() {
        let gate: Arc<Gate<u32>> = Arc::new(Gate::new());
        let claim = take(&gate, 1);
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || gate.claim(1, Duration::from_secs(5), far()).is_ok())
        };
        std::thread::sleep(Duration::from_millis(50));
        drop(claim);
        assert!(
            waiter.join().unwrap(),
            "the waiter gets the key once it is free"
        );
    }

    #[test]
    fn a_hung_holder_is_skipped_at_once() {
        let gate: Gate<u32> = Gate::new();
        let _hung = take(&gate, 1);
        std::thread::sleep(HUNG + Duration::from_millis(20));
        let started = Instant::now();
        assert_eq!(gate.claim(1, HUNG, far()).err(), Some(Refusal::Hung));
        assert!(
            started.elapsed() < HUNG / 2,
            "a hung holder must not be waited on: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_holder_is_waited_for_only_until_it_counts_as_hung() {
        let gate: Gate<u32> = Gate::new();
        let _held = take(&gate, 1);
        let started = Instant::now();
        assert_eq!(gate.claim(1, HUNG, far()).err(), Some(Refusal::Hung));
        let waited = started.elapsed();
        assert!(waited >= HUNG - Duration::from_millis(5), "{waited:?}");
        assert!(waited < HUNG * 3, "{waited:?}");
    }

    #[test]
    fn a_wait_ends_at_the_callers_deadline() {
        let gate: Gate<u32> = Gate::new();
        let _held = take(&gate, 1);
        let started = Instant::now();
        let deadline = started + HUNG / 4;
        assert_eq!(
            gate.claim(1, Duration::from_secs(5), deadline).err(),
            Some(Refusal::Busy { strict: false })
        );
        let waited = started.elapsed();
        assert!(
            waited < HUNG,
            "the deadline must cut the wait short: {waited:?}"
        );
    }

    #[test]
    fn a_strict_holder_is_waited_for_and_never_skipped() {
        // Security review of FFA-b: an ordered lookup must not go past the
        // store because someone keeps it busy.
        let gate: Gate<u32> = Gate::new();
        let held = take(&gate, 1);
        held.make_strict();
        std::thread::sleep(HUNG + Duration::from_millis(20));
        let started = Instant::now();
        let deadline = started + HUNG;
        assert_eq!(
            gate.claim(1, HUNG, deadline).err(),
            Some(Refusal::Busy { strict: true }),
            "a strict holder is never reported hung"
        );
        assert!(
            started.elapsed() >= HUNG - Duration::from_millis(5),
            "it is waited for until the deadline: {:?}",
            started.elapsed()
        );
    }
}
