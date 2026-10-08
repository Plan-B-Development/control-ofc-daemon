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
//! **Why the state directory, not `/run/control-ofc`.** systemd empties a
//! `RuntimeDirectory=` when the unit stops unless the unit preserves it — this
//! one does since DEC-487, but a drop-in or `systemctl clean --what=runtime` can
//! undo that — so a lock file there could be unlinked while another process
//! holds it, and a third daemon would then lock a fresh inode and run. The
//! `StateDirectory=` is preserved, owned by root and `0700`, so nobody else can
//! create or replace the file. The lock *file*
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

/// The exit status of a daemon refused because another one is running — it
/// holds the lock, serves on the socket, or bound the socket path between this
/// daemon's probe and its own bind (`LIFE-a`). `EX_TEMPFAIL` from
/// `sysexits.h`, which systemd shows as `TEMPFAIL`; no other path exits with it.
///
/// `control-ofc-restore-auto` (`ExecStopPost`) returns early on it: the
/// hand-back records and the socket in the runtime directory belong to the
/// daemon that is running, and replaying them would hand back the headers it
/// drives. Every refusal happens before this process writes anything there.
/// The unit still restarts on it, deliberately (DEC-487): the holder may be the
/// unit's own previous process, kept alive by a thread stuck in a wedged sysfs
/// write, and a retry is what brings the service back once that lock frees.
pub const ANOTHER_INSTANCE_EXIT_CODE: i32 = 75;

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

/// Why [`check_socket_dir`] refused the directories on the way to the socket.
#[derive(Debug)]
pub enum UnsafeSocketDir {
    /// `path` belongs to `uid`, which is neither root nor this process.
    Owner { path: PathBuf, uid: u32 },
    /// Users other than its owner can add entries to `path` (`mode`).
    Writable { path: PathBuf, mode: u32 },
    /// `path` could not be examined.
    Unreadable { path: PathBuf, error: io::Error },
}

impl std::fmt::Display for UnsafeSocketDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owner { path, uid } => write!(
                f,
                "IPC socket directory '{}' is owned by uid {uid}, neither root nor this user",
                path.display()
            ),
            Self::Writable { path, mode } => write!(
                f,
                "IPC socket directory '{}' is writable by other users (mode {mode:04o})",
                path.display()
            ),
            Self::Unreadable { path, error } => write!(
                f,
                "cannot examine IPC socket directory '{}': {error}",
                path.display()
            ),
        }
    }
}

/// Refuse a socket path another user could plant a socket at first (`LIFE-b`).
///
/// [`probe_socket`] reads a socket that answers as a running daemon and the
/// daemon then refuses to start, so whoever can create an entry where the
/// socket goes can keep the daemon from starting. Nobody but root (or `euid`,
/// for a developer's `--allow-non-root` run) may own a directory on the way,
/// and the directory the socket — or the first directory the daemon has to
/// create for it — goes in must not be writable by anyone else, sticky bit or
/// not: the sticky bit stops others removing entries, not adding them. An
/// ancestor further up may be writable by others only if it is sticky, so
/// nobody can rename the directory below it away and put their own in its place.
///
/// A symlink on the way is judged by its owner, who can repoint it, and then
/// the path it leads to is walked by the same rules — every link of a chain,
/// and the directories each sits in. The default `/run/control-ofc` (root
/// `0755`) passes.
pub fn check_socket_dir(socket_path: &Path, euid: u32) -> Result<(), UnsafeSocketDir> {
    let socket_path = std::path::absolute(socket_path).map_err(|e| unreadable(socket_path, e))?;
    // The socket's directory, or — if it is not there yet — the nearest one
    // that is, where the daemon creates the missing ones.
    for dir in socket_path.ancestors().skip(1) {
        match std::fs::symlink_metadata(dir) {
            Ok(_) => return walk_socket_dirs(dir, euid, true, &mut 0),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(unreadable(dir, e)),
        }
    }
    Ok(())
}

/// Symlinks [`check_socket_dir`] follows before giving up, as the kernel's
/// `ELOOP` limit does.
const MAX_SYMLINK_HOPS: u32 = 40;

fn unreadable(path: &Path, error: io::Error) -> UnsafeSocketDir {
    UnsafeSocketDir::Unreadable {
        path: path.to_path_buf(),
        error,
    }
}

/// Judge `start` and each of its ancestors, following every symlink among them
/// into its target. `holds_socket` marks `start` as the directory a new entry
/// will be created in, and passes to a symlink's target.
fn walk_socket_dirs(
    start: &Path,
    euid: u32,
    holds_socket: bool,
    hops: &mut u32,
) -> Result<(), UnsafeSocketDir> {
    for (i, dir) in start.ancestors().enumerate() {
        let meta = std::fs::symlink_metadata(dir).map_err(|e| unreadable(dir, e))?;
        let holds = holds_socket && i == 0;
        judge_socket_dir(dir, &meta, euid, holds)?;
        if meta.file_type().is_symlink() {
            *hops += 1;
            if *hops > MAX_SYMLINK_HOPS {
                return Err(unreadable(
                    dir,
                    io::Error::other("too many levels of symbolic links"),
                ));
            }
            let target = std::fs::read_link(dir).map_err(|e| unreadable(dir, e))?;
            // A relative target is resolved from the link's directory; joining
            // an absolute one replaces the base.
            let target = dir.parent().unwrap_or(Path::new("/")).join(target);
            walk_socket_dirs(&target, euid, holds, hops)?;
        }
    }
    Ok(())
}

/// The rule [`check_socket_dir`] applies to one entry on the way to the socket;
/// `holds_socket` marks the directory a new entry will be created in.
fn judge_socket_dir(
    path: &Path,
    meta: &std::fs::Metadata,
    euid: u32,
    holds_socket: bool,
) -> Result<(), UnsafeSocketDir> {
    use std::os::unix::fs::MetadataExt;
    judge_socket_dir_bits(
        path,
        meta.uid(),
        meta.mode(),
        meta.file_type().is_symlink(),
        euid,
        holds_socket,
    )
}

fn judge_socket_dir_bits(
    path: &Path,
    uid: u32,
    mode: u32,
    is_symlink: bool,
    euid: u32,
    holds_socket: bool,
) -> Result<(), UnsafeSocketDir> {
    if uid != 0 && uid != euid {
        return Err(UnsafeSocketDir::Owner {
            path: path.to_path_buf(),
            uid,
        });
    }
    // A symlink's own mode means nothing; where it leads is walked separately.
    if is_symlink {
        return Ok(());
    }
    let mode = mode & 0o7777;
    let others_can_add = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    if others_can_add && (holds_socket || !sticky) {
        return Err(UnsafeSocketDir::Writable {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
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

    fn euid() -> u32 {
        // SAFETY: `geteuid` reads immutable per-process state; no pointers.
        unsafe { libc::geteuid() }
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The path `check_socket_dir` refused as writable by others, or a panic.
    fn writable_dir(socket: &Path) -> PathBuf {
        match check_socket_dir(socket, euid()) {
            Err(UnsafeSocketDir::Writable { path, .. }) => path,
            other => panic!(
                "{} must be refused as writable, got {other:?}",
                socket.display()
            ),
        }
    }

    /// `LIFE-b`'s opposite branch: a directory only its owner writes passes,
    /// with or without directories the daemon still has to create.
    #[test]
    fn a_socket_in_a_private_directory_passes() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        check_socket_dir(&dir.path().join("d.sock"), euid()).expect("0755, own uid");
        check_socket_dir(&dir.path().join("new/deeper/d.sock"), euid())
            .expect("created inside a 0755 directory");
    }

    /// `LIFE-b`: anyone who can add an entry where the socket
    /// goes can plant one that answers, and the daemon then refuses to start.
    /// The sticky bit does not stop the planting, so it does not help here.
    #[test]
    fn a_socket_directory_others_can_write_is_refused_sticky_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let socket = shared.join("d.sock");
        for mode in [0o777, 0o1777, 0o770, 0o1733] {
            chmod(&shared, mode);
            assert_eq!(writable_dir(&socket), shared, "mode {mode:o}");
        }
        chmod(&shared, 0o755);
        check_socket_dir(&socket, euid()).expect("precondition: 0755 passes");
    }

    /// A directory the daemon must create is judged by where it would be
    /// created: another user could create it there first.
    #[test]
    fn a_missing_socket_directory_is_judged_where_it_would_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        chmod(&shared, 0o1777);
        assert_eq!(writable_dir(&shared.join("ofc/d.sock")), shared);
    }

    /// Further up, a sticky world-writable directory (`/tmp`) is fine — nobody
    /// can rename the root-owned directory inside it away — and a plain one is not.
    #[test]
    fn an_ancestor_others_can_write_passes_only_when_sticky() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        let private = shared.join("ofc");
        std::fs::create_dir_all(&private).unwrap();
        chmod(&private, 0o755);
        let socket = private.join("d.sock");
        chmod(&shared, 0o1777);
        check_socket_dir(&socket, euid()).expect("sticky ancestor");
        chmod(&shared, 0o777);
        assert_eq!(writable_dir(&socket), shared);
    }

    /// A symlink on the way leads to directories judged by the same rules.
    #[test]
    fn a_symlinked_socket_directory_is_judged_by_where_it_leads() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        let shared = dir.path().join("shared");
        let private = dir.path().join("private");
        std::fs::create_dir(&shared).unwrap();
        std::fs::create_dir(&private).unwrap();
        chmod(&shared, 0o777);
        chmod(&private, 0o755);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        check_socket_dir(&link.join("d.sock"), euid()).expect("leads to 0755");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&shared, &link).unwrap();
        assert_eq!(writable_dir(&link.join("d.sock")), shared);
    }

    /// The middle of a chain counts too (the review of `LIFE-b`): `link` leads
    /// through `hub/link2`, and whoever can write `hub` can repoint `link2`.
    #[test]
    fn every_link_of_a_symlink_chain_is_judged() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        let hub = dir.path().join("hub");
        let private = dir.path().join("private");
        std::fs::create_dir(&hub).unwrap();
        std::fs::create_dir(&private).unwrap();
        chmod(&private, 0o755);
        std::os::unix::fs::symlink(&private, hub.join("link2")).unwrap();
        // A relative target, resolved from the link's own directory.
        std::os::unix::fs::symlink("hub/link2", dir.path().join("link")).unwrap();
        let socket = dir.path().join("link/d.sock");
        chmod(&hub, 0o755);
        check_socket_dir(&socket, euid()).expect("every link in a 0755 directory");
        chmod(&hub, 0o777);
        assert_eq!(writable_dir(&socket), hub);
    }

    #[test]
    fn a_symlink_loop_is_refused_not_followed_for_ever() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("b", dir.path().join("a")).unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("b")).unwrap();
        assert!(matches!(
            check_socket_dir(&dir.path().join("a/d.sock"), euid()),
            Err(UnsafeSocketDir::Unreadable { .. })
        ));
    }

    /// Ownership needs another uid, which a test cannot create; the rule itself.
    #[test]
    fn an_entry_owned_by_another_user_is_refused_even_a_symlink() {
        let p = Path::new("/x");
        let other = euid().wrapping_add(1).max(1);
        for (is_symlink, mode) in [(false, 0o700), (true, 0o777)] {
            assert!(matches!(
                judge_socket_dir_bits(p, other, mode, is_symlink, euid(), true),
                Err(UnsafeSocketDir::Owner { uid, .. }) if uid == other
            ));
            judge_socket_dir_bits(p, 0, mode, is_symlink, euid(), true).expect("root");
            judge_socket_dir_bits(p, euid(), mode, is_symlink, euid(), true).expect("self");
        }
        // As root, only root.
        assert!(matches!(
            judge_socket_dir_bits(p, 1000, 0o755, false, 0, false),
            Err(UnsafeSocketDir::Owner { .. })
        ));
    }
}
