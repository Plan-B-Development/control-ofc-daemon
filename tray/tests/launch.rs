//! Starting the GUI.
//!
//! Uses `sh` for the PATH lookup (guaranteed present anywhere makepkg runs) and
//! a temporary script for the spawn itself, so nothing here depends on
//! `control-ofc-gui` actually being installed.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use control_ofc_tray::launch::{GuiLauncher, LaunchError, ProcessLauncher, GUI_BINARY};

/// How long a spawned process gets to prove it ran (`T1-r`).
///
/// This is a **failure** budget, not a success cost. `wait_until` returns on the
/// first true poll, so the happy path pays one 10 ms tick and only a run that
/// was going to fail anyway spends the difference — measured 2026-09-17, the
/// whole of this test binary finishes in 0.01 s idle.
///
/// Widened from 5 s because 5 s is not enough under a saturated
/// `cargo test --all-targets`: `launching_twice_starts_two_processes` failed
/// once in two consecutive full-gate runs during DEC-370, then passed on re-run,
/// passed alone, and passed 6/6 concurrently as a standalone binary — wall-clock
/// contention, not a defect in `ProcessLauncher`. 30 s matches the in-repo
/// precedent at `daemon/src/main.rs` (*"probe child did not exit within 30s"*).
///
/// **This widens the wall clock; it does not remove it.** The structural fix —
/// waiting on the spawned `Child` — is not reachable: `GuiLauncher::launch`
/// returns `Result<(), LaunchError>` and the `Child` is moved into a detached
/// reaper thread (`tray/src/launch.rs`), so no handle ever reaches a test, and
/// getting one would mean changing a production trait for a test's benefit.
///
/// One const, not two literals, because **both** process-liveness tests share
/// the hazard: `an_absolute_path_is_launched_and_reaped` waits on the same
/// wall clock and was simply the one not yet observed failing.
///
/// The `T1-s` failures whose message was captured were not this budget at
/// all: they were `ETXTBSY` at spawn, before any wait began — see
/// [`write_script`]. `T1-r`'s original failure may well have been the same
/// race. Do not widen this again.
const PROCESS_LIVENESS_BUDGET: Duration = Duration::from_secs(30);

/// Write an executable script **without this process ever holding a write fd
/// on it** (`T1-s`).
///
/// `execve` refuses a file that any process holds open for writing
/// (`ETXTBSY`, "Text file busy"). A script written here with `File::create` is
/// closed by the time this thread executes it — but a test thread that forks
/// while the handle is open copies it into its child, and that copy lives
/// until the child execs, because `O_CLOEXEC` closes it only then. Executing
/// the script inside that window fails at spawn (rust-lang/rust#114554), and
/// the launch tests fork concurrently, so it reddened the canonical gate
/// routinely. Here the only write fd belongs to a short-lived `sh`, which has
/// exited before this returns, so no child of this process can inherit one.
fn write_script(path: &Path, body: &str) {
    let status = Command::new("sh")
        .arg("-c")
        .arg(r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#)
        .arg("sh")
        .arg(body)
        .arg(path)
        .status()
        .expect("spawn sh to write the script");
    assert!(status.success(), "writing {path:?} failed: {status}");
}

/// Wait for a condition on a deadline. Never a bare sleep: a bare sleep either
/// flakes under load or wastes the difference on every run.
fn wait_until(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

#[test]
fn the_gui_binary_name_matches_the_guis_console_script() {
    // The GUI declares this in pyproject.toml [project.scripts]; if it ever
    // changes, the tray's headline action silently stops working.
    assert_eq!(GUI_BINARY, "control-ofc-gui");
}

#[test]
fn a_binary_on_path_is_reported_available() {
    assert!(ProcessLauncher::new("sh").is_available());
}

#[test]
fn a_missing_binary_is_not_available() {
    assert!(!ProcessLauncher::new("control-ofc-definitely-not-installed").is_available());
}

#[test]
fn launching_a_missing_binary_reports_not_found_without_panicking() {
    let launcher = ProcessLauncher::new("control-ofc-definitely-not-installed");
    match launcher.launch() {
        Err(LaunchError::NotFound(name)) => {
            assert!(name.contains("definitely-not-installed"), "{name}")
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn a_non_executable_file_does_not_count_as_available() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("not-executable");
    std::fs::write(&path, "#!/bin/sh\n").expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

    assert!(
        !ProcessLauncher::new(&path).is_available(),
        "a readable but non-executable file must not be offered as launchable"
    );
}

#[test]
fn an_absolute_path_is_launched_and_reaped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("fake-gui");
    let marker = dir.path().join("ran");
    write_script(
        &script,
        &format!("#!/bin/sh\necho started > {}\n", marker.display()),
    );

    let launcher = ProcessLauncher::new(&script);
    assert!(
        launcher.is_available(),
        "precondition: the script is runnable"
    );
    launcher.launch().expect("should spawn");

    assert!(
        wait_until(PROCESS_LIVENESS_BUDGET, || marker.exists()),
        "the launched process must actually run"
    );
}

#[test]
fn launching_twice_starts_two_processes() {
    // The tray deliberately does not deduplicate: Plasma turns a double click
    // into two Activate calls, and making that harmless is the GUI's
    // single-instance guard, not the tray's.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("fake-gui");
    let counter = dir.path().join("count");
    write_script(
        &script,
        &format!("#!/bin/sh\necho x >> {}\n", counter.display()),
    );

    let launcher = ProcessLauncher::new(&script);
    launcher.launch().expect("first");
    launcher.launch().expect("second");

    assert!(
        wait_until(PROCESS_LIVENESS_BUDGET, || {
            std::fs::read_to_string(&counter)
                .map(|s| s.lines().count() == 2)
                .unwrap_or(false)
        }),
        "both launches must reach the process, got {:?}",
        std::fs::read_to_string(&counter)
    );
}

#[test]
fn a_written_script_runs_while_other_threads_are_forking() {
    // `T1-s` on purpose. The flake needed another thread to fork while a
    // script's write fd was open, which a quiet run seldom arranges and a
    // saturated full gate sometimes does. Here several threads fork without
    // pause, so a `write_script` that ever held a write fd in this process
    // would lose the race — and the one that hands the write to `sh` has no fd
    // to lose, however loaded the machine is.
    const FORKERS: usize = 4;
    const SCRIPTS: usize = 200;

    let stop = Arc::new(AtomicBool::new(false));
    let forkers: Vec<_> = (0..FORKERS)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut forks = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    // Count only forks that happened, or a missing `true`
                    // would satisfy the precondition below without one.
                    if Command::new("true").status().is_ok() {
                        forks += 1;
                    }
                }
                forks
            })
        })
        .collect();

    let dir = tempfile::tempdir().expect("tempdir");
    let mut failure = None;
    for i in 0..SCRIPTS {
        let script = dir.path().join(format!("script-{i}"));
        write_script(&script, "#!/bin/sh\nexit 0\n");
        match Command::new(&script).status() {
            Ok(status) if status.success() => {}
            other => {
                failure = Some((i, format!("{other:?}")));
                break;
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    let forks: u64 = forkers
        .into_iter()
        .map(|h| h.join().expect("forker thread"))
        .sum();

    assert!(
        forks > 0,
        "precondition: the other threads must have forked while the scripts ran"
    );
    assert!(
        failure.is_none(),
        "a script could not be executed while other threads forked \
         ({forks} forks): {failure:?}"
    );
}
