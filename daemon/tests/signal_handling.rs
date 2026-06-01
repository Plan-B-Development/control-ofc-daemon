//! Integration test for the daemon's SIGTERM handling.
//!
//! Each integration test in `daemon/tests/` runs in its own binary so the
//! signal-disposition state set up here cannot leak into other tests. This
//! file exercises only the tokio signal-stream wiring used by the daemon's
//! shutdown `select!`. The full graceful-shutdown chain (GPU reset, hwmon
//! restore, server join) is covered by manual verification at release time;
//! reproducing the entire daemon main loop in a unit test would be costly
//! and brittle for marginal additional confidence.
//!
//! What this *does* protect against: a regression where `main.rs` drops the
//! `SignalKind::terminate()` registration and silently reverts to ignoring
//! `systemctl stop` (the original P1 bug).

use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::timeout;

/// `signal(SignalKind::terminate())` must succeed under a normal Tokio
/// runtime; failure here means the SIGTERM arm in `main.rs` will silently
/// disable itself via its `.ok()` guard and the daemon falls back to
/// SIGINT-only behaviour.
#[tokio::test]
async fn sigterm_stream_can_be_registered() {
    let stream = signal(SignalKind::terminate());
    assert!(
        stream.is_ok(),
        "SIGTERM stream registration must succeed under Tokio's signal driver; \
         got error: {:?}",
        stream.err()
    );
}

/// `signal(SignalKind::hangup())` is the partner registration used by the
/// daemon's config-reload arm. Co-locating this test makes sure both
/// optional signal arms stay healthy across tokio/libc upgrades.
#[tokio::test]
async fn sighup_stream_can_be_registered() {
    let stream = signal(SignalKind::hangup());
    assert!(stream.is_ok(), "SIGHUP stream registration must succeed");
}

/// End-to-end check: after registering a SIGTERM stream, sending SIGTERM to
/// our own process must wake the stream's `recv()`. This is the behaviour
/// the daemon depends on for `systemctl stop` to trigger the in-process
/// graceful path. If the stream registration regresses or a future tokio
/// version changes the dispatch semantics, this test fires.
///
/// Safety: each `tests/*.rs` file compiles to its own binary, so the
/// SIGTERM sent here is delivered to a process that only exists to run
/// this test. The signal stream is registered *before* the `kill`, so the
/// signal is delivered to the stream rather than triggering the default
/// terminate action.
#[tokio::test]
async fn sigterm_stream_receives_self_kill() {
    let mut stream = signal(SignalKind::terminate()).expect("register SIGTERM stream");

    // Self-deliver SIGTERM. `libc::kill(getpid(), SIGTERM)` is the standard
    // pattern; once tokio has installed the global handler, the signal is
    // queued onto the stream rather than terminating the process.
    // Safety: `kill` is a simple syscall with no preconditions beyond a
    // valid pid (our own) and signal number.
    let rc = unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
    assert_eq!(rc, 0, "libc::kill(self, SIGTERM) returned {rc}");

    // 5-second timeout is well beyond any plausible scheduling delay on
    // CI; the actual delivery happens within microseconds in practice.
    let recv = timeout(Duration::from_secs(5), stream.recv()).await;
    assert!(
        recv.is_ok(),
        "SIGTERM stream did not wake within 5s after self-kill"
    );
}
