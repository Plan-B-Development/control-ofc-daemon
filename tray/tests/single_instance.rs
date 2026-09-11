//! The one-tray-per-user guard.
//!
//! This exercises the **real** acquisition path, not a stub of the decision
//! around it. That is only possible because the guard is an advisory file lock
//! rather than a D-Bus well-known name: `PKGBUILD`'s `check()` runs this suite
//! inside a clean-room container with no session bus, so a bus-based guard could
//! not be tested where it has to be tested.
//!
//! Every test works in its own temporary directory, so the suite's parallel
//! threads cannot contend and nothing depends on the machine's real
//! `$XDG_RUNTIME_DIR`.

use std::path::PathBuf;

use control_ofc_tray::single_instance::{acquire, default_lock_path, lock_path_in, AcquireError};

/// A private directory for one test, named after the test so a leftover is
/// traceable. Mode 0700 because `default_lock_path` refuses anything looser —
/// which is the `T1-h` fix, and a fixture at 0755 would silently test the
/// refusal path instead of the one it meant to.
fn private_dir(tag: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("ofc-tray-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("tighten the fixture directory");
    dir
}

#[test]
fn the_first_claim_succeeds_and_the_second_is_refused() {
    let path = private_dir("contested").join("tray.lock");

    let first = acquire(&path).expect("the first tray must start");
    match acquire(&path) {
        Err(AcquireError::AlreadyRunning) => {}
        Ok(_) => panic!("a second tray must not be able to claim the same lock"),
        Err(e) => panic!("expected AlreadyRunning, got {e:?}"),
    }
    drop(first);
}

#[test]
fn different_users_do_not_contend() {
    // Distinct directories are what keeps two logged-in users independent: the
    // real path is inside each user's own $XDG_RUNTIME_DIR.
    let a = acquire(&private_dir("user-a").join("tray.lock")).expect("first user");
    let b = acquire(&private_dir("user-b").join("tray.lock")).expect("second user is independent");
    drop(a);
    drop(b);
}

#[test]
fn releasing_the_guard_frees_the_lock_for_the_next_tray() {
    let path = private_dir("recycled").join("tray.lock");

    let first = acquire(&path).expect("claim");
    drop(first);

    acquire(&path).expect("the lock must be reusable once the holder is gone");
}

#[test]
fn a_lock_file_left_behind_by_a_crash_does_not_block_the_next_tray() {
    // This is the property that made the abstract namespace attractive and that
    // the file lock had to preserve: a crashed tray must not wedge its
    // replacement. The claim is the LOCK, not the file, so a file left on disk
    // is inert — and this test is only meaningful if the file really is still
    // there, hence the precondition.
    let path = private_dir("crashed").join("tray.lock");
    {
        let _dead = acquire(&path).expect("claim");
    } // dropped, as a process exit would drop it

    assert!(
        path.exists(),
        "precondition: the lock file must survive, or this test proves nothing \
         about recovering from a leftover one"
    );
    acquire(&path).expect("a leftover lock file must not block the next tray");
}

#[test]
fn the_lock_path_is_refused_when_the_directory_is_not_private() {
    // The `T1-h` fix. $XDG_RUNTIME_DIR is only an environment variable, so a
    // misconfigured one pointing somewhere group- or world-accessible would let
    // another user hold this user's lock and deny them their tray. Refusing is
    // the safe answer: `main` then runs with no guard, which costs at worst a
    // second icon.
    //
    // Driven through `lock_path_in`, which takes the directory, so this test
    // mutates NO environment — see that function's docstring for why that
    // matters more than convenience here.
    use std::os::unix::fs::PermissionsExt;
    let dir = private_dir("loose");

    // Tight: accepted, and the path lands inside it. Asserted first so the
    // refusal below cannot be a directory that was broken all along.
    let tight = lock_path_in(&dir).expect("a 0700 directory must be accepted");
    assert!(
        tight.starts_with(&dir),
        "the lock must live inside the directory it was given; got {}",
        tight.display()
    );

    for mode in [0o750, 0o707, 0o777] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))
            .expect("loosen the fixture directory");
        match lock_path_in(&dir) {
            Err(AcquireError::Unavailable(why)) => assert!(
                why.contains("other users"),
                "mode {mode:o} must be refused for being shared; got {why}"
            ),
            Ok(p) => panic!(
                "mode {mode:o} is reachable by another user and must be refused, got {}",
                p.display()
            ),
            Err(e) => panic!("mode {mode:o}: expected Unavailable, got {e:?}"),
        }
    }

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("restore");
}

#[test]
fn a_directory_that_is_not_a_directory_is_refused() {
    // A plain file where a directory is expected must not be adopted; the lock
    // would then be created at a path inside a non-directory and fail obscurely.
    let dir = private_dir("notadir");
    let file = dir.join("imposter");
    std::fs::write(&file, b"").expect("write the imposter");
    match lock_path_in(&file) {
        Err(AcquireError::Unavailable(why)) => assert!(
            why.contains("not a directory"),
            "the reason must say what was wrong; got {why}"
        ),
        Ok(p) => panic!("a file must not be accepted, got {}", p.display()),
        Err(e) => panic!("expected Unavailable, got {e:?}"),
    }
}

#[test]
fn the_default_lock_path_reads_the_runtime_directory_and_is_named_for_the_tray() {
    // Covers `default_lock_path`'s env read by READING the environment, never by
    // mutating it. Both branches are asserted, so this cannot pass vacuously
    // whichever way the test environment happens to be configured.
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => {
            let path =
                default_lock_path().expect("a real session's runtime directory must be accepted");
            assert!(
                path.starts_with(std::path::Path::new(&runtime)),
                "the lock must live in $XDG_RUNTIME_DIR; got {}",
                path.display()
            );
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("the lock has a filename");
            assert_eq!(
                name, "control-ofc-tray.lock",
                "an unrecognisable name in $XDG_RUNTIME_DIR is a support problem"
            );
            // The uid is deliberately NOT in the name: the directory is already
            // per-user, and a uid in the name would imply the name is what
            // separates users when the directory's ownership is.
            assert!(
                !name.contains(&current_uid().to_string()),
                "the uid belongs to the directory, not the filename; got {name}"
            );
        }
        None => {
            // `PKGBUILD`'s clean-room container is this branch.
            match default_lock_path() {
                Err(AcquireError::Unavailable(why)) => assert!(
                    why.contains("XDG_RUNTIME_DIR"),
                    "the reason must name the missing variable; got {why}"
                ),
                Ok(p) => panic!(
                    "with no runtime dir there is nothing to trust, got {}",
                    p.display()
                ),
                Err(e) => panic!("expected Unavailable, got {e:?}"),
            }
        }
    }
}

fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .expect("/proc/self is readable in any environment this runs in")
}
