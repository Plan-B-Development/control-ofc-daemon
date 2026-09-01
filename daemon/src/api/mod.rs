//! IPC server (Unix socket) for GUI communication.
//!
//! HTTP over Unix domain socket using axum.
//! All responses come from the in-memory cache — no direct hardware I/O.

pub mod calibration;
pub mod characterization;
pub mod diagnostics;
pub mod handlers;
pub mod responses;
pub mod server;
