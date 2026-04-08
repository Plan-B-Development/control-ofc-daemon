//! In-memory ring buffer for per-entity time-series history.
//!
//! Stores the last N samples per sensor/fan for the `/sensors/history` endpoint.
//! Memory-bounded: 250 samples × 30 entities × 16 bytes ≈ 118 KB.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single timestamped reading.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HistorySample {
    /// Unix timestamp in milliseconds.
    pub ts: u64,
    /// The value (temperature °C, RPM, or PWM %).
    pub v: f64,
}

/// Per-entity bounded ring buffer.
struct EntityHistory {
    samples: VecDeque<HistorySample>,
    max_size: usize,
}

impl EntityHistory {
    fn new(max_size: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(max_size),
            max_size,
        }
    }

    fn push(&mut self, value: f64) {
        let ts = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis() as u64,
            Err(e) => {
                log::warn!("system clock before UNIX epoch, skipping history sample: {e}");
                return;
            }
        };
        if self.samples.len() >= self.max_size {
            self.samples.pop_front();
        }
        self.samples.push_back(HistorySample { ts, v: value });
    }

    fn last_n(&self, n: usize) -> Vec<HistorySample> {
        let skip = self.samples.len().saturating_sub(n);
        self.samples.iter().skip(skip).cloned().collect()
    }
}

/// Thread-safe history store for all entities.
pub struct HistoryRing {
    inner: RwLock<HashMap<String, EntityHistory>>,
    max_per_entity: usize,
}

impl HistoryRing {
    pub fn new(max_per_entity: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            max_per_entity,
        }
    }

    /// Record a value for a sensor or fan.
    pub fn record(&self, entity_id: &str, value: f64) {
        let mut map = self.inner.write();
        map.entry(entity_id.to_string())
            .or_insert_with(|| EntityHistory::new(self.max_per_entity))
            .push(value);
    }

    /// Get the last N points for an entity.
    pub fn get_last(&self, entity_id: &str, n: usize) -> Vec<HistorySample> {
        let map = self.inner.read();
        map.get(entity_id).map(|h| h.last_n(n)).unwrap_or_default()
    }

    /// List all entity IDs with history.
    pub fn entity_ids(&self) -> Vec<String> {
        let map = self.inner.read();
        map.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_retrieve() {
        let ring = HistoryRing::new(5);
        ring.record("cpu", 42.0);
        ring.record("cpu", 43.0);
        ring.record("cpu", 44.0);

        let samples = ring.get_last("cpu", 10);
        assert_eq!(samples.len(), 3);
        assert!((samples[0].v - 42.0).abs() < f64::EPSILON);
        assert!((samples[2].v - 44.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evicts_oldest_when_full() {
        let ring = HistoryRing::new(3);
        for i in 0..5 {
            ring.record("s", i as f64);
        }
        let samples = ring.get_last("s", 10);
        assert_eq!(samples.len(), 3);
        // Should have 2, 3, 4 (oldest 0, 1 evicted)
        assert!((samples[0].v - 2.0).abs() < f64::EPSILON);
        assert!((samples[2].v - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn get_last_fewer_than_available() {
        let ring = HistoryRing::new(10);
        for i in 0..10 {
            ring.record("s", i as f64);
        }
        let samples = ring.get_last("s", 3);
        assert_eq!(samples.len(), 3);
        assert!((samples[0].v - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_entity_returns_empty() {
        let ring = HistoryRing::new(10);
        assert!(ring.get_last("nonexistent", 10).is_empty());
    }

    #[test]
    fn entity_ids_lists_recorded() {
        let ring = HistoryRing::new(10);
        ring.record("a", 1.0);
        ring.record("b", 2.0);
        let mut ids = ring.entity_ids();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
    }
}
