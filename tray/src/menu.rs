//! The StatusNotifierItem itself: what the tray shows and what clicking it does.

use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

use crate::client::{DaemonApi, ProfileSummary, Status};
use crate::launch::GuiLauncher;

/// Freedesktop icon name, installed by the daemon package as
/// `/usr/share/icons/hicolor/scalable/apps/control-ofc-symbolic.svg`.
pub const ICON_NAME: &str = "control-ofc-symbolic";

/// Stable across sessions, per the `Tray::id` contract.
pub const TRAY_ID: &str = "control-ofc-tray";

/// A `RadioGroup.selected` that matches no option.
///
/// ksni renders the checkmark with `toggle_state: if idx == group.selected`
/// (ksni-0.3.6 `src/menu.rs:948`), so any out-of-range index simply leaves every
/// option unchecked. That is exactly what "no profile is active" needs, and
/// there is no in-band way to say it. Do not "fix" this to 0 — that would put
/// the checkmark on whichever profile happens to sort first and claim it is
/// running.
const NO_SELECTION: usize = usize::MAX;

/// What the last refresh saw. `status: None` means the daemon was unreachable.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub status: Option<Status>,
    pub profiles: Vec<ProfileSummary>,
    /// The profile list could not be read, as distinct from being empty.
    /// Rendering "No profiles available" for a failed fetch would be a false
    /// statement about the machine, which is the one thing the UI must not do.
    pub profiles_unreadable: bool,
}

pub struct ControlOfcTray {
    api: Box<dyn DaemonApi>,
    launcher: Box<dyn GuiLauncher>,
    snapshot: Snapshot,
    /// Why the user's last profile action did not happen, rendered at the top of
    /// the next menu build (`T1-k`).
    ///
    /// **Deliberately NOT a `Snapshot` field**, which is where `T1-k` proposed
    /// putting it. `Snapshot` is documented as "what the last refresh saw" and
    /// `refresh()` replaces it wholesale — on both the success and the
    /// daemon-unreachable path — so an error stored there would be erased by the
    /// very `refresh()` that runs immediately after the failed POST. Storing it
    /// beside the snapshot rather than inside it is what makes it survive.
    last_action_error: Option<String>,
    quit: Box<dyn Fn() + Send>,
}

impl ControlOfcTray {
    pub fn new(api: Box<dyn DaemonApi>, launcher: Box<dyn GuiLauncher>) -> Self {
        Self {
            api,
            launcher,
            snapshot: Snapshot::default(),
            last_action_error: None,
            // Ending the process IS the correct shutdown for a tray: there is
            // nothing to flush, and the single-instance lock is released by the
            // kernel however we exit.
            quit: Box::new(|| std::process::exit(0)),
        }
    }

    /// Replace what "Quit tray" does.
    ///
    /// Exists so the menu can be exercised end-to-end in tests without the
    /// quit item terminating the test runner — an untestable callback sitting
    /// in the middle of the menu is a trap for every future test that walks it.
    pub fn with_quit(mut self, quit: Box<dyn Fn() + Send>) -> Self {
        self.quit = quit;
        self
    }

    /// Read-only view of what the menu will render. Test seam.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Re-read the daemon.
    ///
    /// This is the *only* thing that talks to the daemon on a timer-free path:
    /// it runs when the menu is about to be shown, so an idle tray issues no
    /// requests at all (brief §10).
    pub fn refresh(&mut self) {
        match self.api.status() {
            Ok(status) => {
                let (profiles, profiles_unreadable) = match self.api.profiles() {
                    Ok(profiles) => (profiles, false),
                    Err(e) => {
                        log::warn!("could not list profiles: {e}");
                        (Vec::new(), true)
                    }
                };
                self.snapshot = Snapshot {
                    status: Some(status),
                    profiles,
                    profiles_unreadable,
                };
            }
            Err(e) => {
                log::debug!("daemon unavailable: {e}");
                self.snapshot = Snapshot::default();
            }
        }
    }

    fn activate_profile(&mut self, profile_id: &str) {
        let outcome = self.api.activate_profile(profile_id);
        if let Err(e) = &outcome {
            log::warn!("could not activate profile '{profile_id}': {e}");
        }
        // Re-read rather than assume the request did what was asked: the daemon
        // validates, and it is the authority on what is now active.
        self.refresh();
        // Then decide what to SAY from that re-read, never from the reply.
        //
        // The distinction is load-bearing and not defensive. `DEFAULT_TIMEOUT`
        // bounds a whole call at 300 ms, and the daemon applies an activation
        // *before* it answers — so a slow-but-successful switch comes back as
        // `Unavailable("request timed out")` while the profile really is running.
        // Keying the message off the error would then print "the previous one is
        // still active" directly beneath a correctly ticked new profile: a false
        // claim about what is driving the fans, which is the whole thing `T1-k`
        // exists to prevent, reintroduced from the other side.
        self.last_action_error = if outcome.is_ok() || self.active_profile_id() == Some(profile_id)
        {
            None
        } else {
            Some(match self.active_profile_label() {
                // Truthful because it comes from the re-read, not the request.
                Some(active) => format!("⚠ Profile not switched — {active} is still active"),
                // The re-read failed too, so nothing may be claimed about what
                // is running; say only what is known.
                None => "⚠ Profile switch failed, and the daemon is not answering".to_string(),
            })
        };
    }

    fn deactivate_profile(&mut self) {
        let outcome = self.api.deactivate_profile();
        if let Err(e) = &outcome {
            log::warn!("could not deactivate the active profile: {e}");
        }
        self.refresh();
        // Same rule as `activate_profile`: adjudicate against the re-read. Here
        // "it worked" means the re-read reports nothing active at all.
        let stopped = self
            .snapshot
            .status
            .as_ref()
            .is_some_and(|s| s.active_profile_id.is_none());
        self.last_action_error = if outcome.is_ok() || stopped {
            None
        } else {
            Some(match self.active_profile_label() {
                Some(active) => format!("⚠ Not stopped — {active} is still controlling the fans"),
                None => "⚠ Stop failed, and the daemon is not answering".to_string(),
            })
        };
    }

    /// The active profile id as of the last refresh, if the daemon answered.
    fn active_profile_id(&self) -> Option<&str> {
        self.snapshot.status.as_ref()?.active_profile_id.as_deref()
    }

    /// How to name the active profile to a human. Prefers the daemon's name and
    /// falls back to the id, matching what the radio group shows.
    fn active_profile_label(&self) -> Option<String> {
        let status = self.snapshot.status.as_ref()?;
        let id = status.active_profile_id.as_deref()?;
        Some(match status.active_profile_name.as_deref() {
            Some(name) if !name.is_empty() => format!("'{name}'"),
            _ => format!("'{id}'"),
        })
    }

    fn open_gui(&self) {
        if let Err(e) = self.launcher.launch() {
            // Never fatal (brief §9).
            log::warn!("{e}");
        }
    }

    fn header_label(&self) -> String {
        match &self.snapshot.status {
            Some(status) if !status.daemon_version.is_empty() => {
                format!("Control-OFC · daemon {}", status.daemon_version)
            }
            Some(_) => "Control-OFC · daemon version unknown".to_string(),
            None => "Control-OFC · daemon unavailable".to_string(),
        }
    }

    fn profile_items(&self) -> Vec<MenuItem<Self>> {
        let Some(status) = &self.snapshot.status else {
            return vec![disabled("Daemon unavailable")];
        };
        if self.snapshot.profiles.is_empty() {
            return vec![disabled(if self.snapshot.profiles_unreadable {
                "Profiles could not be read"
            } else {
                "No profiles available"
            })];
        }

        let labels = profile_labels(&self.snapshot.profiles);
        let ids: Vec<String> = self
            .snapshot
            .profiles
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let selected = status
            .active_profile_id
            .as_ref()
            .and_then(|active| ids.iter().position(|id| id == active))
            .unwrap_or(NO_SELECTION);

        let options = labels
            .into_iter()
            .map(|label| RadioItem {
                label,
                ..Default::default()
            })
            .collect();

        // The ids are captured rather than re-read from `self` in the callback,
        // so a click always applies to the list the user actually saw.
        let click_ids = ids;
        vec![
            RadioGroup {
                selected,
                select: Box::new(
                    move |tray: &mut Self, index: usize| match click_ids.get(index) {
                        Some(id) => {
                            let id = id.clone();
                            tray.activate_profile(&id);
                        }
                        None => log::warn!("profile index {index} is out of range"),
                    },
                ),
                options,
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Stop profile control".into(),
                // Only meaningful when something is active.
                enabled: status.active_profile_id.is_some(),
                activate: Box::new(|tray: &mut Self| tray.deactivate_profile()),
                ..Default::default()
            }
            .into(),
        ]
    }
}

impl ksni::Tray for ControlOfcTray {
    fn id(&self) -> String {
        TRAY_ID.to_string()
    }

    fn title(&self) -> String {
        // Static: a hover label built from cached state would go stale between
        // menu opens, and the tray refreshes only when the menu is shown.
        "Control-OFC".to_string()
    }

    fn icon_name(&self) -> String {
        ICON_NAME.to_string()
    }

    /// Left click. Plasma has no double-click signal — `StatusNotifierItem.qml`
    /// routes `Qt.LeftButton` straight to `Activate`, and the SNI interface has
    /// no double-click member — so a double click simply calls this twice.
    /// Opening the GUI must therefore be idempotent, which is what the GUI's
    /// single-instance guard provides.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.open_gui();
    }

    /// Plasma calls this on every context-menu open
    /// (`libdbusmenuqt/dbusmenuimporter.cpp` issues `AboutToShow`, then
    /// re-fetches the layout), which is what lets the tray poll nothing.
    fn menu_about_to_show(&mut self) {
        self.refresh();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = vec![disabled(&self.header_label())];

        if let Some(status) = &self.snapshot.status {
            if status.thermal_is_abnormal() {
                items.push(disabled(&thermal_label(&status.thermal_state)));
            }
        }

        // AFTER the thermal line, not before it: `CLAUDE.md`'s visible-warning
        // hierarchy puts the more severe fact first, and fans forced to maximum
        // outranks a click that did not take. They are adjacent on purpose —
        // a force-all is a plausible *reason* for a refused switch (503).
        //
        // Sticky until an action succeeds: "the last thing you asked for did not
        // happen and nothing has since" stays true until it does, and `menu()`
        // takes `&self` so it could not clear after rendering anyway.
        if let Some(error) = &self.last_action_error {
            items.push(disabled(error));
        }

        items.push(MenuItem::Separator);
        items.push(
            SubMenu {
                label: "Profile".into(),
                submenu: self.profile_items(),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Open Control-OFC".into(),
                enabled: self.launcher.is_available(),
                activate: Box::new(|tray: &mut Self| tray.open_gui()),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit tray".into(),
                activate: Box::new(|tray: &mut Self| (tray.quit)()),
                ..Default::default()
            }
            .into(),
        );
        items
    }

    /// Keep running when the shell's watcher disappears — `plasmashell
    /// --replace` and a Plasma restart both do this, and the watcher comes back
    /// within seconds. Returning `false` here would make a shell restart
    /// silently lose the tray until the next login.
    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::info!("status-notifier watcher went offline ({reason:?}); waiting for it to return");
        true
    }

    fn watcher_online(&self) {
        log::info!("status-notifier watcher is back; re-registering");
    }
}

/// A non-interactive line of text in the menu.
fn disabled(label: &str) -> MenuItem<ControlOfcTray> {
    StandardItem {
        label: label.to_string(),
        enabled: false,
        ..Default::default()
    }
    .into()
}

/// Human wording for a `thermal_state` token.
///
/// Two rules here, both learned the hard way:
///
/// * **No temperature appears in any of these strings.** The trip point is
///   per-machine since DEC-308 — it is the CPU-reported ceiling where the kernel
///   publishes one — so a number baked into a label is wrong on most hardware.
/// * **An unrecognised token renders rather than disappearing**, the same rule
///   the GUI follows for `skipped_controls`. A daemon that gains a new state
///   must not go quiet in the tray.
pub fn thermal_label(state: &str) -> String {
    match state {
        "emergency" => "⚠ Thermal emergency — fans forced to maximum".to_string(),
        "recovery" => "⚠ Thermal recovery — fans held elevated".to_string(),
        "no_sensor_fallback" => "⚠ No CPU temperature — fans held at a safety floor".to_string(),
        other => format!("⚠ Thermal state: {other}"),
    }
}

/// Menu labels for a profile list, disambiguating repeated names.
///
/// The daemon's `/profiles` is a union across every search directory deduped by
/// **id**, so two different profiles sharing a `name` is normal, not a defect —
/// this machine has two called "Balanced". Without the suffix the menu would
/// show two identical entries and give the user no way to tell which is which.
pub fn profile_labels(profiles: &[ProfileSummary]) -> Vec<String> {
    profiles
        .iter()
        .map(|profile| {
            if profile.name.is_empty() {
                return profile.id.clone();
            }
            let duplicated = profiles
                .iter()
                .filter(|other| other.name == profile.name)
                .count()
                > 1;
            if duplicated {
                format!("{} ({})", profile.name, profile.id)
            } else {
                profile.name.clone()
            }
        })
        .collect()
}
