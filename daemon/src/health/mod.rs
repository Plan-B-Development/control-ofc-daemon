//! Cache, staleness tracking, and health model.
//!
//! - `state` — canonical types for fans, sensors, AIO stats
//! - `cache` — thread-safe in-memory cache with batch updates
//! - `staleness` — health computation from timestamps and thresholds
//! - `sensor_failure` — per-sensor read-failure quarantine (DEC-193)

pub mod cache;
pub mod history;
pub mod sensor_failure;
pub mod staleness;
pub mod state;
