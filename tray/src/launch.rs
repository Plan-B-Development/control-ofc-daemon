//! Launching `control-ofc-gui` from the tray.
//!
//! Brief §9: no shell execution, no extra runtime dependency, and a failure to
//! launch must never destabilise the tray. Everything here returns `Result` and
//! the caller logs rather than propagates.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use std::os::unix::process::CommandExt;

/// The GUI's console-script name, from the GUI repo's
/// `pyproject.toml [project.scripts]`.
pub const GUI_BINARY: &str = "control-ofc-gui";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// Nothing executable by that name on `PATH`.
    NotFound(String),
    Spawn(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::NotFound(p) => write!(f, "{p} is not installed or not on PATH"),
            LaunchError::Spawn(e) => write!(f, "could not start the GUI: {e}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Starting the GUI, abstracted so the menu can be tested without spawning one.
pub trait GuiLauncher: Send {
    fn launch(&self) -> Result<(), LaunchError>;
    /// Whether the GUI looks launchable right now. Drives whether the
    /// "Open Control-OFC" item is enabled.
    fn is_available(&self) -> bool;
}

/// Spawns the real GUI, detached from the tray.
pub struct ProcessLauncher {
    program: OsString,
}

impl ProcessLauncher {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Default for ProcessLauncher {
    fn default() -> Self {
        Self::new(GUI_BINARY)
    }
}

impl GuiLauncher for ProcessLauncher {
    fn launch(&self) -> Result<(), LaunchError> {
        let program = self.program.to_string_lossy().into_owned();
        if find_in_path(Path::new(&self.program)).is_none() {
            return Err(LaunchError::NotFound(program));
        }

        let child = Command::new(&self.program)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own process group, so the GUI survives the tray exiting and
            // never receives a signal aimed at the tray.
            .process_group(0)
            .spawn()
            .map_err(|e| LaunchError::Spawn(e.to_string()))?;

        // Reap in the background. Without this the GUI becomes a zombie when it
        // exits, because the tray is its parent and never calls wait(). The
        // thread costs one parked stack for the GUI's lifetime and ends with it;
        // the alternative (ignoring SIGCHLD) would need a libc dependency the
        // tray otherwise does not have.
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });

        Ok(())
    }

    fn is_available(&self) -> bool {
        find_in_path(Path::new(&self.program)).is_some()
    }
}

/// Resolve `program` the way execvp would: absolute/relative paths as given,
/// bare names against `PATH`.
fn find_in_path(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return is_executable_file(program).then(|| program.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(program);
        is_executable_file(&candidate).then_some(candidate)
    })
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
