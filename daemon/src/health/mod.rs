//! Cache, staleness tracking, and health model.
//!
//! - `state` — canonical types for fans, sensors, AIO stats
//! - `cache` — thread-safe in-memory cache with batch updates
//! - `staleness` — health computation from timestamps and thresholds

pub mod cache;
pub mod history;
pub mod staleness;
pub mod state;
