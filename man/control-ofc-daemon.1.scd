control-ofc-daemon(1)

# NAME

control-ofc-daemon - fan control daemon for OpenFan, hwmon, and AMD GPU hardware

# SYNOPSIS

*control-ofc-daemon* [OPTIONS]

# DESCRIPTION

Control-OFC daemon is a Rust service that manages hardware fan control via
hwmon sysfs, OpenFanController serial protocol, and AMD GPU PMFW interfaces.
It serves an HTTP API over a Unix domain socket for client applications
(such as control-ofc-gui and control-ofc-tray) and is the only thing that
writes fan speeds: while a profile is active it evaluates the profile's fan
curves itself, once a second, whether or not a client is running. Clients
only activate profiles, take short manual overrides and run diagnostics.

The daemon owns thermal safety: the emergency limit (at least 105°C, raised
per-machine to min(CPU-reported design ceiling + 5°C, 115°C) where the kernel
publishes the ceiling) drives every OpenFan channel and writable hwmon header
the machine has to 100% PWM, holds that until a fresh reading at or below
80°C (a CPU sensor that stops updating or disappears does not release it),
then returns to active control at once, and gives every other fan it took back
to what it was doing before. With no emergency latched, holds the fans the
active profile controls at 40% or more if no CPU sensor is reachable for 5
consecutive poll cycles; a fan whose curve can no longer be read keeps its last
speed, and fans no profile controls stay under their firmware curve. (An
ARCTIC Fan Controller has no firmware curve: every write to it carries all
ten channels, so once the daemon writes any of them, each channel reading 0
that it did not choose is set to 100% first.)

Both duties are *floors* over the active profile's output, never
replacements for it: each output receives max(commanded, forced), and at 100% an
output no control commands still receives the forced duty. The ladder can therefore
only raise a fan, never lower one. GPU fans are excluded by design — there is no
GPU emergency threshold, because AMD PMFW firmware owns GPU thermal protection
independently of OS fan control.

# OPTIONS

*--config* <path>
	Path to daemon.toml configuration file.
	Default: /etc/control-ofc/daemon.toml

*--profile* <name>
	Load a named profile from configured search directories at startup.
	_name_ is the file stem (quiet for quiet.json), not the profile's
	display name.

*--profile-file* <path>
	Load a profile from an absolute file path at startup.

*--allow-non-root*
	Skip the root privilege check. For development and testing only.

*--version*
	Print the daemon version and exit.

*-h*, *--help*
	Print a usage summary and exit.

Any other argument, a flag given without its value, or a value joined to its
flag with *=* (*--profile=quiet*) is refused: the daemon prints the usage and
exits with status 2, without starting.

Only one daemon runs at a time. A second one exits with status 1 before it
touches the socket or a fan: it holds an advisory lock on _daemon.lock_ in the
state directory, and never removes a socket another daemon is serving on. Do not
run the binary by hand while the service is active; to start with a profile, set
*--profile* or *OPENFAN_PROFILE* in a drop-in (*systemctl edit
control-ofc-daemon*).

The startup profile is the first that loads of: *--profile* or
*--profile-file*, then *OPENFAN_PROFILE*, then the profile saved in
_daemon_state.json_. A source that is missing or invalid is logged and the
next is tried. Neither *--profile* nor *OPENFAN_PROFILE* is saved.

# FILES

_/etc/control-ofc/daemon.toml_
	Main configuration file (admin-owned, never modified by the daemon).
	Installed with every key commented out. It is strict: an unknown key
	or an out-of-range value makes the daemon exit at startup, and systemd
	keeps restarting it, with nothing controlling the fans, until the file
	is fixed. After an edit, *systemctl reload control-ofc-daemon* applies
	the profile search directories and the exit floor; every other key
	needs a restart. Moving _state_dir_ or _socket_path_ also needs a
	*ReadWritePaths=* drop-in, because the unit uses *ProtectSystem=strict*.
	See _/usr/share/doc/control-ofc-daemon/USER_GUIDE.md_.

_/var/lib/control-ofc/runtime.toml_
	Settings changed through the API, written by the daemon (0600) and
	overlaid on daemon.toml. *It holds the fan header roles you assign*
	(*[hardware] header_roles*): on a board whose chip publishes no fan
	labels, a *pump* assignment there is the only thing that gives that
	header its 30% floor and keeps fan identify from stopping it. It also
	holds the cooling devices (*[[cooling_devices]]*), the preferred
	sensors, the exit floor, and any profile search directory, startup
	delay, *[serial]*, *[polling]* or *[detection]* value set through the
	API. Do not delete it or edit it by hand. If it cannot be read, the
	daemon still starts, on defaults with no header roles, and reports
	*runtime_config_degraded* on *GET /status*; the GUI shows a banner.
	If a setting is saved while it is unreadable, the daemon keeps the
	original as _runtime.toml.invalid-<timestamp>_ and writes a new file
	carrying the header roles and cooling devices it is running with;
	copy anything else you need back from the kept copy and restart.

_/var/lib/control-ofc/daemon_state.json_
	Persistent state: the profile last activated or deactivated through the
	API. Survives daemon restarts. Never written by *--profile* or
	*OPENFAN_PROFILE*. Atomic writes with 0600 permissions.

_/var/lib/control-ofc/profiles/_
	The daemon's profile store: profiles saved through the API. Searched
	before every other profile directory.

_/etc/control-ofc/profiles/_
	Admin-installed profiles, including the example _quiet.json_, which
	controls no fan as shipped.

_/var/lib/control-ofc/validation/_, _control_paths.json_, _pwm_baselines.json_
	Diagnostic evidence under _/var/lib/control-ofc/_: validation sessions,
	measured PWM-to-tach relationships and learned RPM ranges. Nothing that
	controls a fan reads them.

_/run/control-ofc/control-ofc.sock_
	Unix domain socket for the HTTP API. Permissions: 0666 (world-readable
	and world-writable, so a non-root GUI can connect). There is no
	authentication: any local user can activate a profile or run a
	diagnostic.

_/run/control-ofc/hwmon-handback_, _gpu-handback_, _gpu-pmfw-handback_
	What the daemon took and must give back: each motherboard header's
	original mode or duty, and each GPU it put in manual mode or drove a
	fan curve on. Written before the first write that needs it, and
	replayed by *control-ofc-restore-auto* after every stop.

_/run/control-ofc/sleep-hook.pid_, _sleep-hook.ack_
	The handshake between the daemon and its system-sleep hook.

_/usr/bin/control-ofc-restore-auto_
	Run by the unit's *ExecStopPost=* after every stop, crash or kill: gives
	each header and GPU in the records above back to what it had.

_/usr/lib/systemd/system-sleep/control-ofc-daemon_
	The system-sleep hook (see *SIGUSR1*, *SIGUSR2*).

_/etc/modules-load.d/control-ofc.conf_
	Loads the common Super-I/O sensor drivers at boot. To stop loading one,
	comment out its line; do not rename or delete the file, which pacman
	restores at the next upgrade. Apply an edit with *systemctl restart
	systemd-modules-load* or a reboot.

_/usr/lib/modprobe.d/control-ofc-superio.conf_, _/usr/lib/control-ofc/control-ofc-superio-guard_
	The Super-I/O guard: stops *nct6775* and *w83627ehf* from probing on
	every Gigabyte board, and on a listed board whose firmware reports no
	vendor. Their probe can hide a second fan chip until a reboot, and on
	some boards until the machine is powered down at the wall. Each
	suppression is logged: *journalctl -b -t control-ofc-superio-guard*.
	To turn it off, create an empty
	_/etc/modprobe.d/control-ofc-superio.conf_; a file of any other name
	does not reliably override it.

# ENVIRONMENT

*RUST_LOG*
	Log level filter: error, warn, info, debug, trace.
	Default: info

*CONTROL_OFC_CONFIG*
	Path to daemon.toml. Overridden by --config CLI flag.

*OPENFAN_PROFILE*
	Profile file stem to load at startup. Tried after the *--profile* and
	*--profile-file* flags, and before the saved profile.

# SIGNALS

*SIGTERM*, *SIGINT*
	Stop. The daemon leaves every fan it cannot give back to firmware (an
	OpenFan channel, or a header with no mode switch) at its last speed or
	the exit floor, whichever is higher, then gives every other header and
	GPU fan back to what it had before the daemon took it. *systemctl stop*
	sends *SIGTERM*.

*SIGHUP*
	Reload configuration from daemon.toml and runtime.toml without
	restarting. Only the *profile search directories*, the *exit
	floor* (*[shutdown] exit_floor_pct*) and the *coolant limit*
	(*[safety] coolant_limit_c*) are re-applied to the running daemon. Every other key -- polling interval, serial port
	and timeout, socket path, state directory, startup delay, and the
	*[detection]* opt-ins -- is consumed once at process start, so
	changing it requires a restart. *GET /config* reports which keys have
	a persisted value that is not yet in effect.

*SIGUSR1*, *SIGUSR2*
	Sent by the package's systemd-sleep hook
	(/usr/lib/systemd/system-sleep/control-ofc-daemon) before and after a
	system sleep. Under systemd, *SIGUSR1* widens the service watchdog to
	120 seconds (never below *WatchdogSec*) so a slow device resume cannot
	draw a watchdog restart, and *SIGUSR2* restores it; the daemon restores
	it by itself if *SIGUSR2* never arrives. Not intended to be sent by hand.

# AMD GPU SUPPORT

The daemon writes AMD GPU fans through one of two sysfs interfaces:

. PMFW *fan_curve* -- required for RDNA3+ cards (RX 7000 / RX 9000 series).
  This is the only path a profile drives.
. Legacy *pwm1_enable* + *pwm1* pair -- RDNA2 and older (RX 6000 series and
  earlier). Written by the GPU fan verify and reset only; no profile drives
  these fans.

RDNA3+ cards do *not* accept *pwm1_enable=1* and must use the PMFW path.

The PMFW *fan_curve* sysfs node is gated behind the kernel command-line
parameter:

```
amdgpu.ppfeaturemask=0xffffffff
```

Without this, RDNA3+ fan control is read-only -- the GUI displays RPM
but cannot write a curve. Pre-RDNA3 cards do not require this parameter.

To set the kernel parameter:

. *GRUB* -- append to *GRUB_CMDLINE_LINUX_DEFAULT* in
  _/etc/default/grub_, then run
  *sudo grub-mkconfig -o /boot/grub/grub.cfg*.
. *systemd-boot* -- append to the *options* line in the active loader
  entry under _/boot/loader/entries/_.
. *rEFInd* -- append to the *options* line in _/boot/refind_linux.conf_
  or to the kernel argument list in _refind.conf_.
. *Limine* -- append to *KERNEL_CMDLINE* in _/etc/default/limine_,
  then run *sudo limine-mkinitcpio*.

A reboot is required for kernel parameter changes to take effect.

Verify with *cat /proc/cmdline* -- the parameter must appear in the
output. The GUI's Hardware page also reports whether the PMFW
fan_curve interface is reachable.

# SEE ALSO

*control-ofc-tray*(1), *control-ofc-gui*(1)

_/usr/share/doc/control-ofc-daemon/USER_GUIDE.md_ -- setup, sensor drivers,
configuration, troubleshooting.

Project: https://github.com/Plan-B-Development/control-ofc-daemon

# AUTHORS

Plan B Development
