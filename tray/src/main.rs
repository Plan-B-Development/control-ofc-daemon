//! Entry point for `control-ofc-tray`.

use std::process::ExitCode;

use ksni::blocking::TrayMethods;

use control_ofc_tray::client::{DaemonClient, DEFAULT_SOCKET_PATH};
use control_ofc_tray::launch::ProcessLauncher;
use control_ofc_tray::menu::ControlOfcTray;
use control_ofc_tray::single_instance::{self, AcquireError};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default log filter: this crate at `info`, everything else at `warn`.
///
/// **A bare `"info"` here is a GLOBAL default, and that is the defect this
/// constant exists to prevent.** `ksni` reaches D-Bus through `zbus`, which logs
/// its connection handshake and *every dispatched method call* at INFO — raw
/// message bodies included, as byte arrays. Measured on a live Plasma session
/// 2026-09-11: one boot's journal held **11 lines under tag `control-ofc-tray`
/// and none of them were the tray's own**, which makes the two lines an operator
/// actually needs — an already-running tray, and a profile switch the daemon
/// refused — unfindable in the noise.
///
/// `warn` rather than `off` for the rest deliberately: a real `zbus` warning is
/// worth seeing. `RUST_LOG` still overrides the whole thing, as
/// control-ofc-tray(1) documents.
///
/// The daemon's own `main` carries the same bare-`info` line and is left alone:
/// it has no chatty D-Bus dependency, so the global default costs it nothing.
const DEFAULT_LOG_FILTER: &str = "control_ofc_tray=info,warn";

/// Build the logger for a filter spec.
///
/// Split out from `main` so the filter's **behaviour** is testable rather than
/// its spelling: the test builds through this same function with the same
/// constant and asserts which of five targets survive. Asserting on the string
/// would only prove `env_logger` parses it.
///
/// `Builder::new()` reads no environment at all, unlike the `from_env` this
/// replaced — which is the point, because it is what makes the test
/// deterministic whatever `RUST_LOG` the developer has exported. `RUST_LOG`
/// itself is therefore resolved by the caller. `RUST_LOG_STYLE` is not, so it is
/// re-honoured here: `from_env` applied it via `Env::get_write_style`, and
/// dropping it silently would have been an unannounced behaviour change.
fn log_builder(filter: &str) -> env_logger::Builder {
    let mut builder = env_logger::Builder::new();
    builder.parse_filters(filter);
    if let Ok(style) = std::env::var("RUST_LOG_STYLE") {
        builder.parse_write_style(&style);
    }
    builder
}

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
    // `RUST_LOG` wins where it is set, which is the documented escape hatch;
    // otherwise the crate-scoped default above. Read explicitly rather than via
    // `Env::default().default_filter_or(..)` so `log_builder` takes a resolved
    // spec and the test can drive it deterministically.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_LOG_FILTER.to_string());
    log_builder(&filter).init();

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
    // sessions. Held for the life of the process; the kernel releases the lock
    // however the process exits, SIGKILL included.
    //
    // `info` and not `warn` for the AlreadyRunning arm, deliberately. `T1-h`
    // offered raising the level as the *alternative* to moving the guard off the
    // abstract namespace, because there the branch could mean a stranger had
    // taken the name. The lock now lives in a verified-private
    // $XDG_RUNTIME_DIR, so this arm means what it says — your own tray is
    // already running — and promoting it would put routine noise back into the
    // journal the filter above was just narrowed to clear.
    let _guard = match single_instance::acquire_default() {
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

    /// The `T1-l` guard. This asserts the built logger's **behaviour** for five
    /// targets, not the spelling of the filter — a source scan or a string
    /// comparison would pass against any spec `env_logger` happens to accept,
    /// including the bare `"info"` this replaced.
    #[test]
    fn the_default_filter_admits_the_tray_and_silences_its_dbus_stack() {
        use log::{Level, Log, MetadataBuilder};

        let logger = log_builder(DEFAULT_LOG_FILTER).build();
        let enabled = |target: &str, level: Level| {
            logger.enabled(&MetadataBuilder::new().target(target).level(level).build())
        };

        // The tray's own lines must survive — this is the half a too-aggressive
        // filter would break, and without it the fix could "pass" by silencing
        // everything.
        assert!(
            enabled("control_ofc_tray", Level::Info),
            "the tray's own info lines must be logged"
        );
        assert!(
            enabled("control_ofc_tray::menu", Level::Info),
            "a module inside the tray must be logged too"
        );

        // The measured defect: zbus INFO drowning the above.
        assert!(
            !enabled("zbus::connection::handshake::common", Level::Info),
            "zbus INFO is what produced 11 journal lines and zero tray lines"
        );
        assert!(
            !enabled("tracing::span", Level::Info),
            "the tracing->log bridge is the other half of the same noise"
        );

        // But a genuine dependency warning must still reach the journal, or the
        // fix has traded one silence for another.
        assert!(
            enabled("zbus::connection", Level::Warn),
            "a real zbus warning must still be logged"
        );
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
