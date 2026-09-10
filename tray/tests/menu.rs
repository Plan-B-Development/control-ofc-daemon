//! What the tray actually renders, exercised through the real `ksni::Tray`
//! methods rather than through helpers.
//!
//! Every profile assertion below is a *relationship* against what the fake
//! daemon reported, never a literal index or name. A literal would be satisfied
//! by a menu that ignores the wire and hardcodes the answer, which is the
//! failure these tests exist to catch.

mod common;

use common::*;
use control_ofc_tray::client::ClientError;
use ksni::Tray;

#[test]
fn the_checked_profile_is_the_one_the_daemon_reports_active() {
    let profiles = vec![
        profile("quiet", "Quiet"),
        profile("balanced", "Balanced"),
        profile("performance", "Performance"),
    ];
    let daemon = FakeDaemon::new(
        status("2.44.0", "normal", Some("balanced")),
        profiles.clone(),
    );
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();

    let items = tray.menu();
    let radio = profile_radio(&items).expect("profile submenu must hold a radio group");

    let active = tray
        .snapshot()
        .status
        .as_ref()
        .and_then(|s| s.active_profile_id.clone())
        .expect("fixture reports an active profile");
    let expected = profiles
        .iter()
        .position(|p| p.id == active)
        .expect("the active id must be in the list");

    // Precondition: if the active profile were first, `selected: 0` — the exact
    // defect the NO_SELECTION constant warns about — would pass this test.
    assert_ne!(
        expected, 0,
        "fixture must make the active profile something other than the first entry, \
         or this assertion cannot distinguish a correct menu from a hardcoded 0"
    );
    assert_eq!(
        radio.selected, expected,
        "the checkmark must sit on the profile the daemon reports as active"
    );
}

#[test]
fn no_active_profile_leaves_every_entry_unchecked() {
    let profiles = vec![profile("quiet", "Quiet"), profile("balanced", "Balanced")];
    let daemon = FakeDaemon::new(status("2.44.0", "normal", None), profiles);
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();

    let items = tray.menu();
    let radio = profile_radio(&items).expect("radio group");

    assert!(
        radio.selected >= radio.options.len(),
        "with no profile active the selected index must fall outside the option \
         list so nothing is checked; got {} against {} options",
        radio.selected,
        radio.options.len()
    );
}

#[test]
fn profiles_sharing_a_name_are_distinguishable() {
    // Not hypothetical: this machine's daemon returns two profiles named
    // "Balanced", because /profiles is a union across search dirs deduped by id.
    let profiles = vec![
        profile("balanced", "Balanced"),
        profile("c915b9b1", "Balanced"),
        profile("quiet", "Quiet"),
    ];
    let daemon = FakeDaemon::new(
        status("2.44.0", "normal", Some("c915b9b1")),
        profiles.clone(),
    );
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();

    let items = tray.menu();
    let radio = profile_radio(&items).expect("radio group");
    let labels: Vec<&str> = radio.options.iter().map(|o| o.label.as_str()).collect();

    assert_ne!(
        labels[0], labels[1],
        "duplicate names must not render identically: {labels:?}"
    );
    assert!(
        labels[0].contains("balanced") && labels[1].contains("c915b9b1"),
        "each duplicate must carry its own id so the user can tell them apart: {labels:?}"
    );
    // The unique name must NOT be suffixed — otherwise every label gets an id
    // and the test above would pass for the wrong reason.
    assert_eq!(
        labels[2], "Quiet",
        "a name that is already unique must render bare"
    );

    // And the checkmark still resolves by id, not by name.
    let expected = profiles.iter().position(|p| p.id == "c915b9b1").unwrap();
    assert_eq!(radio.selected, expected);
}

#[test]
fn the_header_reports_the_version_the_daemon_sent() {
    let version = "2.43.6";
    let daemon = FakeDaemon::new(status(version, "normal", None), vec![]);
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();

    let items = tray.menu();
    let header = &top_labels(&items)[0];

    assert!(
        header.contains(version),
        "header must render the wire's daemon_version; got {header:?}"
    );
    // The running daemon and the packaged tray are different versions by design
    // (brief §8) — the header must not be showing our own.
    assert_ne!(
        version,
        env!("CARGO_PKG_VERSION"),
        "fixture must use a version different from the tray's own, or this test \
         cannot tell the two apart"
    );
    assert!(
        !header.contains(env!("CARGO_PKG_VERSION")),
        "header must show the RUNNING daemon version, not the tray's: {header:?}"
    );
}

#[test]
fn an_unreachable_daemon_says_so_and_still_offers_the_gui() {
    let daemon = FakeDaemon::unavailable();
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();

    let items = tray.menu();
    let labels = top_labels(&items);

    assert!(
        labels[0].contains("unavailable"),
        "header must state the daemon is unreachable; got {labels:?}"
    );

    let submenu = find_submenu(&items, "Profile").expect("Profile submenu still present");
    assert!(
        profile_radio(&items).is_none(),
        "no profiles may be offered when the daemon cannot be read"
    );
    assert_eq!(submenu.submenu.len(), 1, "exactly one explanatory line");

    let open = find_standard(&items, "Open Control-OFC").expect("Open item");
    assert!(
        open.enabled,
        "the GUI does not need the daemon to launch, so this stays enabled"
    );
}

#[test]
fn an_unreadable_profile_list_is_not_reported_as_an_empty_one() {
    // "No profiles available" is a claim about the machine. Saying it when the
    // fetch merely failed would be a false statement, and the daemon answering
    // /status but not /profiles is exactly when a client is most likely to
    // invent one.
    let daemon = FakeDaemon::new(status("2.44.0", "normal", None), vec![]);
    let launcher = RecordingLauncher::new(true);

    // Genuinely empty.
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();
    let items = tray.menu();
    let empty_line = find_submenu(&items, "Profile")
        .expect("submenu")
        .submenu
        .len();
    assert_eq!(empty_line, 1, "one explanatory line either way");
    let empty_label = match &find_submenu(&items, "Profile").unwrap().submenu[0] {
        ksni::menu::MenuItem::Standard(s) => s.label.clone(),
        _ => panic!("expected a text line"),
    };

    // Unreadable.
    daemon.set_profiles(Err(ClientError::Unavailable("boom".into())));
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();
    let items = tray.menu();
    let failed_label = match &find_submenu(&items, "Profile").unwrap().submenu[0] {
        ksni::menu::MenuItem::Standard(s) => s.label.clone(),
        _ => panic!("expected a text line"),
    };

    assert_ne!(
        empty_label, failed_label,
        "a failed fetch must not render identically to a genuinely empty list"
    );
    assert!(
        failed_label.to_lowercase().contains("could not be read"),
        "the failure must say so; got {failed_label:?}"
    );
    assert!(
        !empty_label.to_lowercase().contains("could not"),
        "a real empty list must not claim a failure; got {empty_label:?}"
    );
}

#[test]
fn the_thermal_line_appears_only_when_the_state_is_abnormal() {
    let launcher = RecordingLauncher::new(true);

    // Absent for the normal case.
    let daemon = FakeDaemon::new(status("2.44.0", "normal", None), vec![]);
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();
    let normal_labels = top_labels(&tray.menu());
    assert!(
        !normal_labels.iter().any(|l| l.contains("Thermal")),
        "no thermal line while the daemon reports normal: {normal_labels:?}"
    );

    // Present for every abnormal state the daemon can publish, plus a token it
    // cannot yet — an unrecognised value must render, not vanish.
    for state in [
        "emergency",
        "recovery",
        "no_sensor_fallback",
        "some_future_state",
    ] {
        let daemon = FakeDaemon::new(status("2.44.0", state, None), vec![]);
        let mut tray = make_tray(&daemon, &launcher);
        tray.refresh();
        let labels = top_labels(&tray.menu());
        let line = labels
            .iter()
            .find(|l| l.contains("Thermal") || l.contains("CPU temperature"))
            .unwrap_or_else(|| panic!("state {state:?} must surface a line; got {labels:?}"));

        // The trip point is per-machine (DEC-308), so no label may name a
        // temperature. Any run of digits would be one.
        assert!(
            !line.chars().any(|c| c.is_ascii_digit()),
            "thermal wording must not embed a temperature: {line:?}"
        );
    }
}

#[test]
fn opening_the_menu_re_reads_the_daemon() {
    // The wiring test. Everything else here asserts what `menu()` does with a
    // snapshot; this asserts the snapshot is actually refreshed by the ksni
    // callback Plasma invokes, which is the half most easily left unconnected.
    let profiles = vec![profile("quiet", "Quiet"), profile("balanced", "Balanced")];
    let daemon = FakeDaemon::new(status("2.44.0", "normal", Some("quiet")), profiles.clone());
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);

    tray.menu_about_to_show();
    let items = tray.menu();
    let before = profile_radio(&items).expect("radio").selected;

    // The daemon changes underneath us, exactly as it would if the profile were
    // switched from the GUI while the tray sat idle.
    daemon.set_status(Ok(status("2.44.0", "emergency", Some("balanced"))));

    tray.menu_about_to_show();
    let items = tray.menu();
    let after = profile_radio(&items).expect("radio").selected;

    assert_ne!(
        before, after,
        "menu_about_to_show must re-read the daemon rather than serve the \
         snapshot taken at the previous open"
    );
    assert_eq!(
        after,
        profiles.iter().position(|p| p.id == "balanced").unwrap(),
        "the refreshed menu must check the newly active profile"
    );
    assert!(
        top_labels(&items).iter().any(|l| l.contains("Thermal")),
        "a thermal state that appeared between opens must show on the next open"
    );
}

#[test]
fn an_idle_tray_talks_to_the_daemon_only_when_the_menu_opens() {
    // Brief §10: no polling, no timers. Constructing the tray and rendering
    // must issue nothing; only the about-to-show callback may.
    let daemon = FakeDaemon::new(status("2.44.0", "normal", None), vec![]);
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);

    let _ = tray.menu();
    let _ = tray.title();
    let _ = tray.icon_name();
    assert!(
        daemon.calls().is_empty(),
        "rendering must not contact the daemon; got {:?}",
        daemon.calls()
    );

    tray.menu_about_to_show();
    assert_eq!(
        daemon.calls(),
        vec!["status".to_string(), "profiles".to_string()],
        "one status + one profiles read per menu open, and nothing else"
    );
}

#[test]
fn choosing_a_profile_asks_the_daemon_and_then_re_reads_it() {
    let profiles = vec![profile("quiet", "Quiet"), profile("balanced", "Balanced")];
    let daemon = FakeDaemon::new(status("2.44.0", "normal", Some("quiet")), profiles);
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();

    let items = tray.menu();
    let radio = profile_radio(&items).expect("radio");
    (radio.select)(&mut tray, 1);

    let calls = daemon.calls();
    let activate_at = calls
        .iter()
        .position(|c| c == "activate:balanced")
        .unwrap_or_else(|| panic!("the id the user clicked must be the id sent; got {calls:?}"));
    assert!(
        calls[activate_at..].contains(&"status".to_string()),
        "the tray must re-read after activating rather than assume it worked — \
         the daemon validates and is the authority on what is now active: {calls:?}"
    );
}

#[test]
fn stopping_profile_control_is_offered_only_while_something_is_active() {
    let profiles = vec![profile("quiet", "Quiet")];
    let launcher = RecordingLauncher::new(true);

    let active = FakeDaemon::new(status("2.44.0", "normal", Some("quiet")), profiles.clone());
    let mut tray_active = make_tray(&active, &launcher);
    tray_active.refresh();
    let items = tray_active.menu();
    let sub = find_submenu(&items, "Profile").expect("submenu");
    let stop = find_standard(&sub.submenu, "Stop profile control").expect("stop item");
    assert!(stop.enabled, "enabled while a profile is running");

    let idle = FakeDaemon::new(status("2.44.0", "normal", None), profiles);
    let mut tray_idle = make_tray(&idle, &launcher);
    tray_idle.refresh();
    let items = tray_idle.menu();
    let sub = find_submenu(&items, "Profile").expect("submenu");
    let stop = find_standard(&sub.submenu, "Stop profile control").expect("stop item");
    assert!(
        !stop.enabled,
        "disabled when there is nothing to stop, or the item lies about what it does"
    );

    // And it actually calls through when used.
    (stop.activate)(&mut tray_active);
    assert!(
        active.calls().contains(&"deactivate".to_string()),
        "the item must reach the daemon: {:?}",
        active.calls()
    );
}

#[test]
fn left_click_opens_the_gui_and_is_safe_to_repeat() {
    // Plasma has no double-click signal, so a double click calls activate()
    // twice. The tray's job is simply to ask twice; making that harmless is the
    // GUI's single-instance guard.
    let daemon = FakeDaemon::new(status("2.44.0", "normal", None), vec![]);
    let launcher = RecordingLauncher::new(true);
    let mut tray = make_tray(&daemon, &launcher);

    tray.activate(0, 0);
    assert_eq!(launcher.launches(), 1);
    tray.activate(0, 0);
    assert_eq!(
        launcher.launches(),
        2,
        "a second click must still ask; deduplication is not the tray's job"
    );
}

#[test]
fn a_missing_gui_disables_the_open_item_and_never_panics() {
    let daemon = FakeDaemon::new(status("2.44.0", "normal", None), vec![]);
    let launcher = RecordingLauncher::new(false);
    let mut tray = make_tray(&daemon, &launcher);
    tray.refresh();

    let items = tray.menu();
    let open = find_standard(&items, "Open Control-OFC").expect("Open item");
    assert!(
        !open.enabled,
        "an uninstalled GUI must render as unavailable rather than failing on click"
    );

    // Brief §9: a failed launch must not destabilise the tray.
    tray.activate(0, 0);
    assert_eq!(launcher.launches(), 1, "it still tried");
}
