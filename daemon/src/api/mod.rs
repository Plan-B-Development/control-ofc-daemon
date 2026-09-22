//! IPC server (Unix socket) for GUI communication.
//!
//! HTTP over Unix domain socket using axum.
//! All responses come from the in-memory cache — no direct hardware I/O.

pub mod calibration;
pub mod characterization;
pub mod diagnostic_gates;
pub mod diagnostics;
pub mod discovery;
pub mod handlers;
pub mod preflight;
pub mod responses;
pub mod server;
pub mod stall_probe;
pub mod stats;
