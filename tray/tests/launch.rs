//! Starting the GUI.
//!
//! Uses `sh` for the PATH lookup (guaranteed present anywhere makepkg runs) and
//! a temporary script for the spawn itself, so nothing here depends on
//! `control-ofc-gui` actually being installed.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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
const PROCESS_LIVENESS_BUDGET: Duration = Duration::from_secs(30);

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
    {
        let mut f = std::fs::File::create(&script).expect("create");
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "echo started > {}", marker.display()).unwrap();
    }
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

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
    {
        let mut f = std::fs::File::create(&script).expect("create");
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "echo x >> {}", counter.display()).unwrap();
    }
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

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
