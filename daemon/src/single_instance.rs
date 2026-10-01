//! One daemon per machine (`DC-a`, DEC-467).
//!
//! A second `control-ofc-daemon` is a second writer: it used to delete the
//! running service's socket, bind its own, and start a second profile engine on
//! the same fans. Two independent guards now stop that, because each one covers
//! a case the other cannot.
//!
//! * **[`acquire`] — an `flock(2)` on `{state_dir}/daemon.lock`, held until the
//!   shutdown has handed the hardware back.** The kernel arbitrates, so two
//!   daemons starting together cannot both win (a check-then-act on the socket
//!   could), and the kernel releases it however the process exits, SIGKILL
//!   included. It is taken before the daemon touches anything a running instance
//!   owns — the validation-session sweep, `runtime.toml`, the socket, any
//!   hardware. A graceful shutdown releases it explicitly
//!   ([`InstanceGuard::release`]) once every writer has stopped, because a
//!   process whose leaked blocking thread is stuck in a driver keeps its
//!   descriptors — and so the lock — after it exits, which would refuse the
//!   restart systemd starts next.
//! * **[`probe_socket`] — a connect to the IPC socket, right after the lock and
//!   again before the socket is replaced.** The lock cannot see a daemon that
//!   predates it: during an upgrade the service still running is the old binary,
//!   which holds no lock, and a hand-started new one would otherwise delete its
//!   socket. A socket something answers on is never removed.
//!
//! **Why the state directory, not `/run/control-ofc`.** systemd empties the
//! `RuntimeDirectory=` when the unit stops, so a lock file there could be
//! unlinked while another process holds it, and a third daemon would then lock a
//! fresh inode and run. The `StateDirectory=` is preserved, owned by root and
//! `0700`, so nobody else can create or replace the file. The lock *file*
//! surviving a crash is harmless: the claim is the lock, never the file's
//! existence, so there is no stale state to clean up and no unlink to race.
//!
//! **Its limit, stated.** The lock path follows `state.state_dir` in the config,
//! so a daemon started with a config naming a different state directory takes a
//! different lock. If it also names the same socket, [`probe_socket`] still
//! refuses it.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// Basename of the lock file inside the state directory.
pub const LOCK_FILE_NAME: &str = "daemon.lock";

/// How long [`probe_socket`] waits for a connect to complete. A blocking
/// connect to a Unix socket whose listener has a full backlog waits for room,
/// indefinitely, so the probe is bounded; running out of time counts as live.
pub const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Holds the lock. Dropping it, or [`InstanceGuard::release`], releases it.
#[derive(Debug)]
pub struct InstanceGuard {
    lock: File,
}

impl InstanceGuard {
    /// Release the lock while the guard stays alive (it lives in a static that
    /// is never dropped). Idempotent; a failure is ignored, because the process
    /// is about to exit and the kernel releases the lock then anyway.
    pub fn release(&self) {
        let _ = self.lock.unlock();
    }
}

#[derive(Debug)]
pub enum AcquireError {
    /// Another process holds the lock at `path`.
    AlreadyRunning { path: PathBuf },
    /// The lock could not be used at all: the file could not be opened, or the
    /// filesystem refused the lock. The daemon refuses to start rather than run
    /// without the guard (DEC-467).
    Unavailable { path: PathBuf, error: io::Error },
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::AlreadyRunning { path } => write!(
                f,
                "another control-ofc-daemon is already running (it holds {})",
                path.display()
            ),
            AcquireError::Unavailable { path, error } => write!(
                f,
                "cannot take the single-instance lock {}: {error}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for AcquireError {}

/// The lock file a daemon whose state directory is `state_dir` claims.
pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LOCK_FILE_NAME)
}

/// Claim the single-instance lock in `state_dir`.
///
/// The file is created `0600` if absent and never removed. It is opened with
/// `O_NOFOLLOW`, so a symlink planted at that name is refused rather than
/// followed. Its contents are never read or written.
pub fn acquire(state_dir: &Path) -> Result<InstanceGuard, AcquireError> {
    let path = lock_path(state_dir);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| AcquireError::Unavailable {
            path: path.clone(),
            error,
        })?;
    match file.try_lock() {
        Ok(()) => Ok(InstanceGuard { lock: file }),
        // `WouldBlock` is precisely "another process holds it". Anything else is
        // the mechanism failing and must not be reported as a running daemon.
        Err(std::fs::TryLockError::WouldBlock) => Err(AcquireError::AlreadyRunning { path }),
        Err(std::fs::TryLockError::Error(error)) => Err(AcquireError::Unavailable { path, error }),
    }
}

/// What is at the IPC socket path.
#[derive(Debug)]
pub enum SocketProbe {
    /// Nothing is there.
    Absent,
    /// Something accepted a connection, or kept the connect waiting past
    /// [`SOCKET_PROBE_TIMEOUT`]. A daemon is serving on it; never remove it.
    Live,
    /// The connect was refused: a socket file left by a daemon that exited
    /// (or any other file). Safe to remove.
    Stale,
    /// The connect failed for another reason, such as a permission error. Not
    /// known to be stale, so it is not removed.
    Unknown(io::Error),
}

/// Find out whether a daemon is serving on the socket at `path`, waiting at
/// most `timeout` for the connect.
pub fn probe_socket(path: &Path, timeout: Duration) -> SocketProbe {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SocketProbe::Absent,
        Err(e) => return SocketProbe::Unknown(e),
        Ok(_) => {}
    }
    // The connect runs on its own thread so a full backlog cannot hang startup.
    // On a timeout the thread is left blocked in connect(2); the daemon refuses
    // to start in that case, so it ends with the process.
    let (tx, rx) = mpsc::channel();
    let target = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("socket-probe".into())
        .spawn(move || {
            let _ = tx.send(UnixStream::connect(&target).map(drop));
        });
    if let Err(e) = spawned {
        return SocketProbe::Unknown(e);
    }
    classify_connect(rx.recv_timeout(timeout).ok())
}

/// The decision [`probe_socket`] makes from a connect's outcome; `None` means
/// the connect did not finish in time.
fn classify_connect(outcome: Option<io::Result<()>>) -> SocketProbe {
    match outcome {
        None | Some(Ok(())) => SocketProbe::Live,
        Some(Err(e)) => match e.kind() {
            io::ErrorKind::ConnectionRefused => SocketProbe::Stale,
            io::ErrorKind::NotFound => SocketProbe::Absent,
            _ => SocketProbe::Unknown(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    #[test]
    fn a_second_claim_while_one_is_held_is_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire(dir.path()).expect("the first claim succeeds");
        match acquire(dir.path()) {
            Err(AcquireError::AlreadyRunning { path }) => {
                assert_eq!(path, dir.path().join(LOCK_FILE_NAME));
            }
            other => panic!("a second claim must be refused as already running, got {other:?}"),
        }
    }

    #[test]
    fn the_lock_is_released_when_the_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        drop(acquire(dir.path()).expect("first claim"));
        acquire(dir.path()).expect("a claim after the holder is gone succeeds");
    }

    /// The shutdown path: the guard outlives the release (it is in a static),
    /// so only an explicit unlock lets the restarted daemon in.
    #[test]
    fn release_lets_a_new_daemon_claim_while_the_guard_is_still_alive() {
        let dir = tempfile::tempdir().unwrap();
        let held = acquire(dir.path()).expect("first claim");
        assert!(
            matches!(
                acquire(dir.path()),
                Err(AcquireError::AlreadyRunning { .. })
            ),
            "precondition: held"
        );
        held.release();
        acquire(dir.path()).expect("a claim after release succeeds");
        drop(held);
    }

    #[test]
    fn a_lock_file_left_by_a_dead_daemon_does_not_block_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LOCK_FILE_NAME), b"").unwrap();
        acquire(dir.path()).expect("the file's existence is not the claim");
    }

    #[test]
    fn the_lock_file_is_created_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(LOCK_FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "group/other bits must be clear, got {mode:o}"
        );
    }

    #[test]
    fn a_symlink_at_the_lock_path_is_refused_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(LOCK_FILE_NAME)).unwrap();
        assert!(
            matches!(acquire(dir.path()), Err(AcquireError::Unavailable { .. })),
            "O_NOFOLLOW must refuse the symlink"
        );
    }

    #[test]
    fn a_missing_state_dir_is_unavailable_not_already_running() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            acquire(&dir.path().join("absent")),
            Err(AcquireError::Unavailable { .. })
        ));
    }

    #[test]
    fn a_socket_something_listens_on_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(matches!(
            probe_socket(&path, SOCKET_PROBE_TIMEOUT),
            SocketProbe::Live
        ));
    }

    #[test]
    fn a_socket_file_whose_listener_is_gone_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(
            path.exists(),
            "precondition: the socket file outlives its listener"
        );
        assert!(matches!(
            probe_socket(&path, SOCKET_PROBE_TIMEOUT),
            SocketProbe::Stale
        ));
    }

    #[test]
    fn no_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe_socket(&dir.path().join("d.sock"), SOCKET_PROBE_TIMEOUT),
            SocketProbe::Absent
        ));
    }

    #[test]
    fn a_connect_that_never_finishes_counts_as_live() {
        assert!(matches!(classify_connect(None), SocketProbe::Live));
    }

    #[test]
    fn a_connect_error_other_than_refused_is_unknown_not_stale() {
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(matches!(
            classify_connect(Some(Err(denied))),
            SocketProbe::Unknown(_)
        ));
    }
}
