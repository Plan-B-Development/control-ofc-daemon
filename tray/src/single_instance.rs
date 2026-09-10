//! One tray per user (brief §12).
//!
//! # Why an abstract-namespace socket
//!
//! On a modern Plasma session this guard is belt-and-braces: systemd's
//! `xdg-autostart-generator` turns `/etc/xdg/autostart/control-ofc-tray.desktop`
//! into a single `app-*@autostart.service` unit, which is already a hard
//! single-instance guarantee. The guard covers everything else — a manual
//! launch, a non-systemd session, another desktop environment.
//!
//! A Linux abstract-namespace socket is the cheapest mechanism with the one
//! property that matters: **it is released by the kernel when the process
//! exits**, however it exits. There is no file to leave behind and no stale
//! lock to recover from after a crash or a SIGKILL.
//!
//! It is also the only candidate that is fully testable where this code has to
//! be tested. `packaging/PKGBUILD`'s `check()` runs `cargo test --frozen` for
//! the whole workspace inside a clean-room container with **no D-Bus session
//! bus**, so a guard built on a session-bus well-known name could not have its
//! real acquisition path exercised there at all — only a stub of the decision
//! around it, which is precisely the "testing the extracted rule instead of the
//! call site" trap this project keeps paying for.
//!
//! # Granularity, deliberately stated
//!
//! The abstract namespace is per **network namespace**, not per login session,
//! so the name is keyed on the uid to keep two different users independent.
//! Two concurrent graphical sessions for the *same* user therefore share one
//! tray rather than getting one each. That is a conscious trade: it is the same
//! granularity as `$XDG_RUNTIME_DIR`, the case is rare, and one tray serving
//! both sessions is harmless — the tray holds no per-session state.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};

/// Held for the lifetime of the process. Dropping it releases the name.
#[derive(Debug)]
pub struct InstanceGuard {
    _listener: UnixListener,
}

#[derive(Debug)]
pub enum AcquireError {
    /// Another tray already holds the name.
    AlreadyRunning,
    /// The namespace could not be used at all (not Linux, sandbox, seccomp).
    Unavailable(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::AlreadyRunning => {
                write!(f, "another Control-OFC tray is already running")
            }
            AcquireError::Unavailable(e) => write!(f, "could not claim the instance name: {e}"),
        }
    }
}

impl std::error::Error for AcquireError {}

/// Claim `name` in the abstract namespace.
///
/// Returns [`AcquireError::AlreadyRunning`] if another process holds it.
pub fn acquire(name: &str) -> Result<InstanceGuard, AcquireError> {
    let addr =
        SocketAddr::from_abstract_name(name.as_bytes()).map_err(AcquireError::Unavailable)?;
    match UnixListener::bind_addr(&addr) {
        Ok(listener) => Ok(InstanceGuard {
            _listener: listener,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Err(AcquireError::AlreadyRunning),
        Err(e) => Err(AcquireError::Unavailable(e)),
    }
}

/// The name a real tray claims: per-uid, so users do not collide.
pub fn default_instance_name() -> String {
    match current_uid() {
        Some(uid) => format!("control-ofc-tray.{uid}"),
        // /proc unreadable: fall back to a shared name rather than to no guard
        // at all. Erring towards "one tray" is the safer failure here.
        None => "control-ofc-tray".to_string(),
    }
}

/// Real uid, without taking a `libc` dependency for one call.
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}
