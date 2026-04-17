control-ofc-daemon(1)

# NAME

control-ofc-daemon - fan control daemon for OpenFan, hwmon, and AMD GPU hardware

# SYNOPSIS

*control-ofc-daemon* [OPTIONS]

# DESCRIPTION

Control-OFC daemon is a Rust service that manages hardware fan control via
hwmon sysfs, OpenFanController serial protocol, and AMD GPU PMFW interfaces.
It serves an HTTP API over a Unix domain socket for client applications
(such as control-ofc-gui) and can optionally evaluate fan curve profiles
autonomously.

The daemon owns thermal safety: 105°C triggers 100% PWM on all fans, holds
until 80°C, recovers at 60°C. Forces 40% if no CPU sensor is found for 5
consecutive poll cycles.

# OPTIONS

*--config* <path>
	Path to daemon.toml configuration file.
	Default: /etc/control-ofc/daemon.toml

*--profile* <name>
	Load a named profile from configured search directories at startup.

*--profile-file* <path>
	Load a profile from an absolute file path at startup.

*--allow-non-root*
	Skip the root privilege check. For development and testing only.

# FILES

_/etc/control-ofc/daemon.toml_
	Main configuration file (admin-owned, never modified by daemon).

_/var/lib/control-ofc/runtime.toml_
	Daemon-mutable runtime configuration (profile search dirs, startup
	delay). Written by the daemon via API endpoints.

_/var/lib/control-ofc/daemon_state.json_
	Persistent state: active profile selection. Survives daemon restarts.
	Atomic writes with 0600 permissions.

_/run/control-ofc/control-ofc.sock_
	Unix domain socket for the HTTP API. Permissions: 0666 (world-readable
	for non-root GUI connections).

# ENVIRONMENT

*RUST_LOG*
	Log level filter: error, warn, info, debug, trace.
	Default: info

*CONTROL_OFC_CONFIG*
	Path to daemon.toml. Overridden by --config CLI flag.

*OPENFAN_PROFILE*
	Profile name to load at startup. Overridden by --profile CLI flag.

# SIGNALS

*SIGHUP*
	Reload configuration from daemon.toml and runtime.toml without
	restarting. Profile search directories and polling intervals are
	refreshed.

# SEE ALSO

*control-ofc-gui*(1)

Project: https://github.com/Plan-B-Development/control-ofc-daemon

# AUTHORS

Plan B Development
