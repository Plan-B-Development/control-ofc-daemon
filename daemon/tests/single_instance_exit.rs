//! The exit status of a refused start, from the real binary (LIFE-a, DEC-487).
//!
//! `control-ofc-restore-auto` skips its replay on exactly one status,
//! `ANOTHER_INSTANCE_EXIT_CODE`, so which refusal exits with it is a
//! hardware-facing rule that no in-process test can reach:
//! `std::process::exit` ends the test runner. These run the binary against a
//! config in a temporary directory. Every case refuses before the socket and
//! before any hardware discovery; a deadline kills the child if a regression
//! ever lets one start.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use control_ofc_daemon::single_instance::{self, ANOTHER_INSTANCE_EXIT_CODE};

const DEADLINE: Duration = Duration::from_secs(30);

/// A config naming `state_dir` and `socket` and nothing else of the machine's.
fn config(dir: &Path, state_dir: &Path, socket: &Path) -> std::path::PathBuf {
    let path = dir.join("daemon.toml");
    std::fs::write(
        &path,
        format!(
            "[state]\nstate_dir = {:?}\n\n[ipc]\nsocket_path = {:?}\n",
            state_dir.display().to_string(),
            socket.display().to_string()
        ),
    )
    .unwrap();
    path
}

/// Run the daemon with `config` and return its exit status code and stderr.
fn run_daemon(config: &Path) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_control-ofc-daemon"))
        .arg("--config")
        .arg(config)
        .arg("--allow-non-root")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("RUNTIME_DIRECTORY")
        .env_remove("CONTROL_OFC_CONFIG")
        .env_remove("OPENFAN_PROFILE")
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the daemon binary must start");
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the daemon did not refuse within {DEADLINE:?}: it started");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    use std::io::Read;
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status.code(), stderr)
}

/// [SAFETY] Another daemon holds the lock: the refusal exits with the status
/// the restore script skips on, and says why.
#[test]
fn a_held_lock_refuses_with_the_another_instance_status() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let _held = single_instance::acquire(&state).expect("the test holds the lock");
    let cfg = config(dir.path(), &state, &dir.path().join("ofc.sock"));

    let (code, stderr) = run_daemon(&cfg);

    assert_eq!(code, Some(ANOTHER_INSTANCE_EXIT_CODE), "{stderr}");
    assert!(stderr.contains("already running"), "{stderr}");
}

/// [SAFETY] A daemon that predates the lock serves on the socket: the same
/// status, from the socket probe rather than the lock.
#[test]
fn a_served_socket_refuses_with_the_another_instance_status() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let socket = dir.path().join("ofc.sock");
    let _served = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let cfg = config(dir.path(), &state, &socket);

    let (code, stderr) = run_daemon(&cfg);

    assert_eq!(code, Some(ANOTHER_INSTANCE_EXIT_CODE), "{stderr}");
    assert!(stderr.contains("in use"), "{stderr}");
}

/// [SAFETY] The path is taken between this daemon's probe and its own bind —
/// another daemon won the race. A dangling symlink stands in for it
/// deterministically: the probe's connect finds no target (absent), and
/// `bind(2)` then refuses the existing path with `EADDRINUSE`. Exiting 1 there
/// would have `ExecStopPost` replay the winner's records.
#[test]
fn a_socket_path_taken_before_the_bind_refuses_with_the_another_instance_status() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let socket = dir.path().join("ofc.sock");
    std::os::unix::fs::symlink(dir.path().join("nothing"), &socket).unwrap();
    let cfg = config(dir.path(), &state, &socket);

    let (code, stderr) = run_daemon(&cfg);

    assert_eq!(code, Some(ANOTHER_INSTANCE_EXIT_CODE), "{stderr}");
    assert!(stderr.contains("failed to bind"), "{stderr}");
}

/// The opposite branch: a lock that cannot be taken at all is not another
/// daemon, so it exits 1 and `ExecStopPost` hands back as after any failure. A
/// symlink at the lock's name is refused by `O_NOFOLLOW`.
#[test]
fn an_unusable_lock_is_an_ordinary_failure() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::os::unix::fs::symlink(
        dir.path().join("elsewhere"),
        single_instance::lock_path(&state),
    )
    .unwrap();
    let cfg = config(dir.path(), &state, &dir.path().join("ofc.sock"));

    let (code, stderr) = run_daemon(&cfg);

    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("single-instance lock"), "{stderr}");
    assert_ne!(ANOTHER_INSTANCE_EXIT_CODE, 1);
}

/// `LIFE-b`: another user plants a socket that answers in a directory they can
/// write, where `ipc.socket_path` points. The probe would read it as a running
/// daemon and refuse with the another-instance status, so the daemon never
/// starts and `ExecStopPost` skips its hand-back. The directory is refused
/// first, as a configuration error (exit 1); the sticky bit does not help,
/// because it stops others removing entries, not adding them.
#[test]
fn a_socket_planted_in_a_directory_others_can_write_is_a_configuration_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    let socket = shared.join("ofc.sock");
    let _planted = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let cfg = config(dir.path(), &state, &socket);

    for mode in [0o777, 0o1777] {
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(mode)).unwrap();
        let (code, stderr) = run_daemon(&cfg);
        assert_eq!(code, Some(1), "mode {mode:o}: {stderr}");
        assert!(
            stderr.contains("writable by other users"),
            "mode {mode:o}: {stderr}"
        );
        assert!(socket.exists(), "the planted socket is left alone");
    }

    // The opposite branch: the same socket in a directory only its owner
    // writes is a running daemon's, as before.
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (code, stderr) = run_daemon(&cfg);
    assert_eq!(code, Some(ANOTHER_INSTANCE_EXIT_CODE), "{stderr}");
}
