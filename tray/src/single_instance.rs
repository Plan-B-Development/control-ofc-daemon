//! One tray per user (brief §12).
//!
//! # Why an advisory file lock, and not the abstract namespace
//!
//! On a modern Plasma session this guard is belt-and-braces: systemd's
//! `xdg-autostart-generator` turns `/etc/xdg/autostart/control-ofc-tray.desktop`
//! into a single `app-*@autostart.service` unit, which is already a hard
//! single-instance guarantee. The guard covers everything else — a manual
//! launch, another desktop environment — **wherever `$XDG_RUNTIME_DIR` is set**.
//!
//! That proviso is a real narrowing against the abstract-namespace socket this
//! replaced, which needed no environment at all, and it is stated rather than
//! glossed: in a session where nothing sets `$XDG_RUNTIME_DIR` (a bare `startx`
//! with no logind or elogind) there is no directory this can trust, so
//! `acquire_default` returns `Unavailable`, `main` logs the reason at `warn`, and
//! two trays can start. That is the deliberate trade — the alternative was
//! keeping a mechanism any local user could deny (`T1-h`) — and the cost is a
//! duplicate icon in a configuration that is already unusual. Register row
//! `T1-n`.
//!
//! This was an abstract-namespace socket until 2026-09-11. It was replaced
//! because **the abstract namespace has no permission check and the name was
//! fully predictable** (`control-ofc-tray.<uid>`), so any local user in the same
//! network namespace could pre-bind another user's name; the victim's tray then
//! took `AddrInUse` at every login, logged one line and exited 0 — a silent,
//! persistent, cross-user denial that systemd recorded as a clean start. That is
//! register row `T1-h`.
//!
//! `flock(2)` on a file inside `$XDG_RUNTIME_DIR` keeps every property that made
//! the abstract namespace the right choice, and adds the one it lacked.
//! **Measured on rustc 1.98.0 before this was written**, because the whole
//! argument rests on these three and none of them is worth taking on trust:
//!
//! * **Atomic.** A second claim while one is held fails with `WouldBlock`. The
//!   kernel arbitrates, so there is no probe-then-bind window — which is why
//!   this is *not* the path socket `T1-h` originally proposed. That shape would
//!   have imported the GUI half's accepted race (`T1-a`) into a module that
//!   today has a real atomic guarantee.
//! * **Released by the kernel however the process exits**, SIGKILL included.
//!   The lock *file* survives a crash, and that is harmless: the claim is the
//!   lock, never the file's existence, so there is no stale state to recover
//!   from and no unlink to race over.
//! * **Needs no D-Bus and no network namespace.** `PKGBUILD`'s `check()` runs
//!   `cargo test --frozen` for the whole workspace in a clean-room container
//!   with no session bus, so the guard has to be testable on `open(2)` and
//!   `flock(2)` alone. It is: the tests below drive the real acquisition path,
//!   not a stub of the decision around it.
//!
//! `$XDG_RUNTIME_DIR` is what supplies the missing property. logind creates it
//! `0700` and owned by the user, so no other user can place a file in it at all
//! — and this module verifies that rather than assuming it, because the variable
//! is only an environment variable and a misconfigured one pointing somewhere
//! shared would otherwise reopen exactly the hole being closed.
//!
//! # Granularity, deliberately stated
//!
//! `$XDG_RUNTIME_DIR` is per **user**, not per login session, so two concurrent
//! graphical sessions for the same user share one tray rather than getting one
//! each. That is the same granularity the abstract namespace gave and the same
//! conscious trade: the case is rare and one tray serving both sessions is
//! harmless, since the tray holds no per-session state.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Basename of the lock file inside `$XDG_RUNTIME_DIR`.
///
/// The uid is **not** in the name: the directory is already per-user, and
/// putting it back would suggest the name is what separates users when in fact
/// the directory's ownership is.
const LOCK_FILE_NAME: &str = "control-ofc-tray.lock";

/// Held for the lifetime of the process. Dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceGuard {
    _lock: File,
}

#[derive(Debug)]
pub enum AcquireError {
    /// Another tray already holds the lock.
    AlreadyRunning,
    /// The lock could not be used at all — no usable `$XDG_RUNTIME_DIR`, a
    /// read-only filesystem, a sandbox. The caller runs without a guard.
    Unavailable(String),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::AlreadyRunning => {
                write!(f, "another Control-OFC tray is already running")
            }
            AcquireError::Unavailable(why) => write!(f, "could not claim the instance lock: {why}"),
        }
    }
}

impl std::error::Error for AcquireError {}

/// Claim the lock at `path`.
///
/// Returns [`AcquireError::AlreadyRunning`] if another process holds it. The
/// file is created if absent and never removed: its contents are irrelevant and
/// its presence carries no meaning, so leaving it costs nothing and removing it
/// would introduce the unlink race this mechanism exists to avoid.
pub fn acquire(path: &Path) -> Result<InstanceGuard, AcquireError> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| AcquireError::Unavailable(format!("{}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(InstanceGuard { _lock: file }),
        // `WouldBlock` is precisely "someone else holds it"; anything else is a
        // mechanism failure and must not be reported as a running tray, or a
        // filesystem that cannot lock would silently suppress the tray forever.
        Err(std::fs::TryLockError::WouldBlock) => Err(AcquireError::AlreadyRunning),
        Err(std::fs::TryLockError::Error(e)) => Err(AcquireError::Unavailable(format!(
            "{}: {e}",
            path.display()
        ))),
    }
}

/// Claim the lock a real tray claims.
pub fn acquire_default() -> Result<InstanceGuard, AcquireError> {
    acquire(&default_lock_path()?)
}

/// Where a real tray's lock lives: inside a verified-private `$XDG_RUNTIME_DIR`.
pub fn default_lock_path() -> Result<PathBuf, AcquireError> {
    let raw = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| AcquireError::Unavailable("XDG_RUNTIME_DIR is not set".into()))?;
    lock_path_in(Path::new(&raw))
}

/// The lock path inside `dir`, once `dir` is verified private to this user.
///
/// Split from [`default_lock_path`] so the verification is testable **without
/// mutating the environment**. That is not a convenience: `std::env::set_var` is
/// unsound against a concurrent `getenv` from another thread, `cargo test` runs a
/// binary's tests in parallel threads, and sibling tests here call
/// `std::env::temp_dir()` — which reads `TMPDIR`. An earlier draft serialised the
/// *writes* behind a mutex and claimed that was sufficient; it was not, and the
/// symptom would have been an intermittent crash in `PKGBUILD`'s clean-room
/// `check()`, i.e. at release time with no local reproduction.
pub fn lock_path_in(dir: &Path) -> Result<PathBuf, AcquireError> {
    Ok(verified_private_dir(dir)?.join(LOCK_FILE_NAME))
}

/// `dir`, checked to actually be private to this user.
///
/// The checks are the whole point of the change and are not defensive padding:
/// a directory another user can write to gives back the `T1-h` hole, since they
/// could create the lock file and hold it. Refusing is the safe answer — the
/// caller degrades to no guard, which costs at worst a second tray icon, where
/// trusting it costs the user their tray entirely.
///
/// `std::fs::metadata` deliberately FOLLOWS symlinks here, unlike the GUI half's
/// `lstat`: what matters is the directory the lock file actually lands in, and
/// `/run/user` is root-owned `0755` so no other user can substitute
/// `/run/user/<uid>` underneath it.
fn verified_private_dir(dir: &Path) -> Result<PathBuf, AcquireError> {
    let dir = dir.to_path_buf();
    let meta = std::fs::metadata(&dir)
        .map_err(|e| AcquireError::Unavailable(format!("{}: {e}", dir.display())))?;
    if !meta.is_dir() {
        return Err(AcquireError::Unavailable(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    // Refuse rather than guess. An earlier draft of this used `unwrap_or(0)` and
    // was wrong in a way worth recording: it would have compared the directory's
    // owner against *root* whenever `/proc/self` was unreadable, which is a false
    // claim about who we are made by the one function whose job is establishing
    // it. Not knowing our uid means we cannot verify privacy, and this module's
    // rule is that an unverifiable directory is not used.
    let uid = current_uid().ok_or_else(|| {
        AcquireError::Unavailable("/proc/self is unreadable, so the uid cannot be checked".into())
    })?;
    if meta.uid() != uid {
        return Err(AcquireError::Unavailable(format!(
            "{} is owned by uid {}, not {uid}",
            dir.display(),
            meta.uid(),
        )));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(AcquireError::Unavailable(format!(
            "{} is accessible to other users (mode {:o})",
            dir.display(),
            meta.mode() & 0o7777
        )));
    }
    Ok(dir)
}

/// Real uid, without taking a `libc` dependency for one call.
///
/// `None` means it could not be determined, which callers must treat as "cannot
/// verify" rather than substituting a default — see `private_runtime_dir`.
fn current_uid() -> Option<u32> {
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}
