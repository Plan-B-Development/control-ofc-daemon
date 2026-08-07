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
until temperature falls below 80°C, then applies a 60% PWM recovery floor
for one cycle before returning to active control. Forces 40% if no CPU
sensor is reachable for 5 consecutive poll cycles.

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
	restarting. Only the *profile search directories* are re-applied to
	the running daemon. Every other key -- polling interval, serial port
	and timeout, socket path, state directory, startup delay, and the
	*[detection]* opt-ins -- is consumed once at process start, so
	changing it requires a restart. *GET /config* reports which keys have
	a persisted value that is not yet in effect.

# AMD GPU SUPPORT

The daemon controls AMD GPU fans via one of two sysfs interfaces:

. PMFW *fan_curve* -- required for RDNA3+ cards (RX 7000 / RX 9000 series).
. Legacy *pwm1_enable* + *pwm1* pair -- used on RDNA2 and older
  (RX 6000 series and earlier).

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

*control-ofc-gui*(1)

Project: https://github.com/Plan-B-Development/control-ofc-daemon

# AUTHORS

Plan B Development
