//! The one-tray-per-user guard.
//!
//! This exercises the **real** acquisition path, not a stub of the decision
//! around it. That is only possible because the guard is an abstract-namespace
//! socket rather than a D-Bus well-known name: `PKGBUILD`'s `check()` runs this
//! suite inside a clean-room container with no session bus, so a bus-based
//! guard could not be tested where it has to be tested.
//!
//! Every test uses a name unique to this process, because the abstract
//! namespace is shared across the whole network namespace and the test binary
//! runs its tests in parallel threads.

use control_ofc_tray::single_instance::{acquire, default_instance_name, AcquireError};

fn unique(tag: &str) -> String {
    format!("control-ofc-tray-test.{}.{tag}", std::process::id())
}

#[test]
fn the_first_claim_succeeds_and_the_second_is_refused() {
    let name = unique("contested");

    let first = acquire(&name).expect("the first tray must start");
    match acquire(&name) {
        Err(AcquireError::AlreadyRunning) => {}
        Ok(_) => panic!("a second tray must not be able to claim the same name"),
        Err(e) => panic!("expected AlreadyRunning, got {e:?}"),
    }
    drop(first);
}

#[test]
fn different_users_do_not_contend() {
    // Distinct names are what keeps two logged-in users independent; the real
    // names differ by uid.
    let a = acquire(&unique("user-a")).expect("first name");
    let b = acquire(&unique("user-b")).expect("second name must be independent");
    drop(a);
    drop(b);
}

#[test]
fn releasing_the_guard_frees_the_name_for_the_next_tray() {
    let name = unique("recycled");

    let first = acquire(&name).expect("claim");
    drop(first);

    // This is the property that makes the abstract namespace the right choice:
    // no file is left behind, so a crashed tray never blocks its replacement.
    acquire(&name).expect("the name must be reusable once the holder is gone");
}

#[test]
fn the_default_name_is_scoped_to_the_current_user() {
    let name = default_instance_name();
    assert!(
        name.starts_with("control-ofc-tray"),
        "unexpected instance name: {name}"
    );

    let uid = std::fs::metadata("/proc/self")
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.uid()
        })
        .expect("/proc/self is readable in any environment this runs in");
    assert_eq!(
        name,
        format!("control-ofc-tray.{uid}"),
        "the name must carry the uid, or two users on one machine would contend"
    );
}
