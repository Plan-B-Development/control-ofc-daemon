//! Shared fixtures for the tray's integration tests.
//!
//! Nothing here touches D-Bus, a socket, or a real process. `PKGBUILD`'s
//! `check()` runs `cargo test --frozen` for the whole workspace inside a
//! clean-room container with no session bus and no desktop, so a test that
//! needed either would fail the *package build*, not merely CI.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use ksni::menu::{MenuItem, RadioGroup, StandardItem, SubMenu};

use control_ofc_tray::client::{ClientError, DaemonApi, ProfileSummary, Status};
use control_ofc_tray::launch::{GuiLauncher, LaunchError};
use control_ofc_tray::menu::ControlOfcTray;

/// A scripted daemon. Answers are swappable mid-test, which is what lets a test
/// prove the tray actually re-reads rather than serving a stale snapshot.
pub struct FakeDaemon {
    status: Mutex<Result<Status, ClientError>>,
    profiles: Mutex<Result<Vec<ProfileSummary>, ClientError>>,
    pub calls: Mutex<Vec<String>>,
}

impl FakeDaemon {
    pub fn new(status: Status, profiles: Vec<ProfileSummary>) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Ok(status)),
            profiles: Mutex::new(Ok(profiles)),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// A daemon that cannot be reached at all.
    pub fn unavailable() -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Err(ClientError::Unavailable("test".into()))),
            profiles: Mutex::new(Err(ClientError::Unavailable("test".into()))),
            calls: Mutex::new(Vec::new()),
        })
    }

    pub fn set_status(&self, status: Result<Status, ClientError>) {
        *self.status.lock().unwrap() = status;
    }

    pub fn set_profiles(&self, profiles: Result<Vec<ProfileSummary>, ClientError>) {
        *self.profiles.lock().unwrap() = profiles;
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

/// Newtype so the test crate may implement the tray's trait: the orphan rule
/// forbids `impl DaemonApi for Arc<FakeDaemon>` here, since both halves are
/// foreign to this crate.
pub struct SharedDaemon(pub Arc<FakeDaemon>);

impl DaemonApi for SharedDaemon {
    fn status(&self) -> Result<Status, ClientError> {
        self.0.calls.lock().unwrap().push("status".into());
        self.0.status.lock().unwrap().clone()
    }

    fn profiles(&self) -> Result<Vec<ProfileSummary>, ClientError> {
        self.0.calls.lock().unwrap().push("profiles".into());
        self.0.profiles.lock().unwrap().clone()
    }

    fn activate_profile(&self, profile_id: &str) -> Result<(), ClientError> {
        self.0
            .calls
            .lock()
            .unwrap()
            .push(format!("activate:{profile_id}"));
        Ok(())
    }

    fn deactivate_profile(&self) -> Result<(), ClientError> {
        self.0.calls.lock().unwrap().push("deactivate".into());
        Ok(())
    }
}

/// Records launches instead of starting a GUI.
pub struct RecordingLauncher {
    pub available: bool,
    pub launches: Mutex<usize>,
}

impl RecordingLauncher {
    pub fn new(available: bool) -> Arc<Self> {
        Arc::new(Self {
            available,
            launches: Mutex::new(0),
        })
    }

    pub fn launches(&self) -> usize {
        *self.launches.lock().unwrap()
    }
}

/// See [`SharedDaemon`] for why this is a newtype.
pub struct SharedLauncher(pub Arc<RecordingLauncher>);

impl GuiLauncher for SharedLauncher {
    fn launch(&self) -> Result<(), LaunchError> {
        *self.0.launches.lock().unwrap() += 1;
        if self.0.available {
            Ok(())
        } else {
            Err(LaunchError::NotFound("control-ofc-gui".into()))
        }
    }

    fn is_available(&self) -> bool {
        self.0.available
    }
}

/// Build a tray whose quit action is inert, so a test may walk the whole menu.
pub fn make_tray(daemon: &Arc<FakeDaemon>, launcher: &Arc<RecordingLauncher>) -> ControlOfcTray {
    ControlOfcTray::new(
        Box::new(SharedDaemon(Arc::clone(daemon))),
        Box::new(SharedLauncher(Arc::clone(launcher))),
    )
    .with_quit(Box::new(|| {}))
}

pub fn status(version: &str, thermal: &str, active: Option<&str>) -> Status {
    Status {
        daemon_version: version.to_string(),
        thermal_state: thermal.to_string(),
        active_profile_id: active.map(str::to_string),
        active_profile_name: active.map(str::to_string),
    }
}

pub fn profile(id: &str, name: &str) -> ProfileSummary {
    ProfileSummary {
        id: id.to_string(),
        name: name.to_string(),
    }
}

// --- menu inspection -------------------------------------------------------

pub fn top_labels(items: &[MenuItem<ControlOfcTray>]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            MenuItem::Standard(s) => s.label.clone(),
            MenuItem::SubMenu(s) => s.label.clone(),
            MenuItem::Checkmark(c) => c.label.clone(),
            MenuItem::RadioGroup(_) => "<radio>".into(),
            MenuItem::Separator => "<sep>".into(),
        })
        .collect()
}

pub fn find_standard<'a>(
    items: &'a [MenuItem<ControlOfcTray>],
    label: &str,
) -> Option<&'a StandardItem<ControlOfcTray>> {
    items.iter().find_map(|item| match item {
        MenuItem::Standard(s) if s.label == label => Some(s),
        _ => None,
    })
}

pub fn find_submenu<'a>(
    items: &'a [MenuItem<ControlOfcTray>],
    label: &str,
) -> Option<&'a SubMenu<ControlOfcTray>> {
    items.iter().find_map(|item| match item {
        MenuItem::SubMenu(s) if s.label == label => Some(s),
        _ => None,
    })
}

pub fn find_radio(items: &[MenuItem<ControlOfcTray>]) -> Option<&RadioGroup<ControlOfcTray>> {
    items.iter().find_map(|item| match item {
        MenuItem::RadioGroup(g) => Some(g),
        _ => None,
    })
}

/// The "Profile" submenu's radio group, which is where profiles actually live.
pub fn profile_radio(items: &[MenuItem<ControlOfcTray>]) -> Option<&RadioGroup<ControlOfcTray>> {
    find_radio(&find_submenu(items, "Profile")?.submenu)
}
