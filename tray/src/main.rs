//! Entry point for `control-ofc-tray`.

use std::process::ExitCode;

use ksni::blocking::TrayMethods;

use control_ofc_tray::client::{DaemonClient, DEFAULT_SOCKET_PATH};
use control_ofc_tray::launch::ProcessLauncher;
use control_ofc_tray::menu::ControlOfcTray;
use control_ofc_tray::single_instance::{self, AcquireError};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
control-ofc-tray — system-tray client for the Control-OFC daemon

USAGE:
    control-ofc-tray [OPTIONS]

OPTIONS:
    --socket <PATH>    Daemon socket (default: /run/control-ofc/control-ofc.sock)
    --version          Print the tray version and exit
    -h, --help         Print this help and exit

The tray normally starts itself through /etc/xdg/autostart/control-ofc-tray.desktop.
On a systemd session it is a user unit; see control-ofc-tray(1) for how to stop,
start and read the logs of a running tray.
";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let socket_path = match parse_args(std::env::args().skip(1)) {
        Ok(Action::Run { socket_path }) => socket_path,
        Ok(Action::Exit(message)) => {
            println!("{message}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    // Belt-and-braces: on a systemd session the generated autostart unit already
    // guarantees one instance. This covers a manual launch and non-systemd
    // sessions. Held for the life of the process; released by the kernel however
    // the process exits.
    let _guard = match single_instance::acquire(&single_instance::default_instance_name()) {
        Ok(guard) => Some(guard),
        Err(AcquireError::AlreadyRunning) => {
            log::info!("a Control-OFC tray is already running for this user; exiting");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            // Not fatal: a tray without the guard is still a working tray.
            log::warn!("{e}; continuing without a single-instance guard");
            None
        }
    };

    let tray = ControlOfcTray::new(
        Box::new(DaemonClient::new(socket_path)),
        Box::new(ProcessLauncher::default()),
    );

    // `assume_sni_available(true)` is load-bearing for autostart. The tray is
    // launched at login and can easily win the race against plasmashell
    // registering org.kde.StatusNotifierWatcher; with the default `false` that
    // race makes `spawn()` fail and the icon never appears for the whole
    // session. With `true`, a missing watcher is routed to `watcher_offline`
    // and ksni registers as soon as the shell is up.
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(handle) => handle,
        Err(e) => {
            log::error!("could not start the tray service: {e}");
            return ExitCode::FAILURE;
        }
    };

    // ksni runs the service on its own thread. Park here rather than polling:
    // nothing ever unparks us, so this costs no wakeups and no CPU. "Quit tray"
    // exits the process directly; the `is_closed` check only matters if a
    // spurious wakeup coincides with the service having shut down.
    loop {
        std::thread::park();
        if handle.is_closed() {
            return ExitCode::SUCCESS;
        }
    }
}

enum Action {
    Run { socket_path: String },
    Exit(String),
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Action, String> {
    let mut socket_path = DEFAULT_SOCKET_PATH.to_string();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                socket_path = args
                    .next()
                    .ok_or_else(|| "error: --socket requires a path".to_string())?;
            }
            "--version" => return Ok(Action::Exit(format!("control-ofc-tray {VERSION}"))),
            "-h" | "--help" => return Ok(Action::Exit(USAGE.trim_end().to_string())),
            other => return Err(format!("error: unrecognised argument '{other}'")),
        }
    }
    Ok(Action::Run { socket_path })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Action, String> {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn no_arguments_uses_the_daemon_default_socket() {
        match parse(&[]).expect("should parse") {
            Action::Run { socket_path } => assert_eq!(socket_path, DEFAULT_SOCKET_PATH),
            Action::Exit(_) => panic!("bare invocation must run, not exit"),
        }
    }

    #[test]
    fn socket_flag_overrides_the_default() {
        match parse(&["--socket", "/tmp/x.sock"]).expect("should parse") {
            Action::Run { socket_path } => assert_eq!(socket_path, "/tmp/x.sock"),
            Action::Exit(_) => panic!("--socket must run, not exit"),
        }
    }

    #[test]
    fn socket_flag_without_a_value_is_an_error() {
        assert!(parse(&["--socket"]).is_err());
    }

    #[test]
    fn unrecognised_arguments_are_rejected_rather_than_ignored() {
        assert!(parse(&["--wat"]).is_err());
    }

    #[test]
    fn version_reports_the_crate_version() {
        match parse(&["--version"]).expect("should parse") {
            Action::Exit(message) => assert!(
                message.contains(VERSION),
                "--version must print {VERSION}, got {message:?}"
            ),
            Action::Run { .. } => panic!("--version must exit, not run"),
        }
    }
}
