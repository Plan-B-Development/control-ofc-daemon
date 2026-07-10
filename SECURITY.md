# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Control-OFC daemon, please
report it privately:

1. **Email:** chomeop@gmail.com
2. **Subject:** `[SECURITY] Control-OFC daemon — <brief description>`

Please include:
- Description of the vulnerability
- Steps to reproduce
- Affected version(s)
- Potential impact

We will acknowledge receipt within 48 hours and aim to provide a fix
within 7 days for critical issues.

## Scope

The daemon runs as root, controls hardware fan headers, and serves an
HTTP API over a local Unix domain socket at
`/run/control-ofc/control-ofc.sock`. The primary security boundaries
are:

| Boundary | Concern |
|----------|---------|
| Unix socket | World-readable (`0666`) by design (DEC-049) — connections must not require any local-trust beyond "non-root user on this host". General client authentication is out of scope for V1; individual state-changing endpoints apply peer-uid checks where cross-user impact is possible (see *Profile search dirs* below). |
| Profile search dirs | `POST /config/profile-search-dirs` is peer-uid-confined (DEC-205): a non-root caller may only add directories that exist and canonicalize to within its **own home directory** (uid read from `SO_PEERCRED`); root / CLI callers are exempt, and it fails closed when the uid or home cannot be resolved. This stops one user on a shared host from pointing the daemon's profile search at another user's files. |
| Sysfs writes | The daemon writes to `/sys/class/hwmon/*/pwm*`, `/sys/class/hwmon/*/pwm*_enable`, and `/sys/class/drm/*/device/gpu_od/fan_ctrl/fan_curve`. All writes are bounded by hardcoded validation (PWM 0–255, percent 0–100). |
| Serial I/O | `/dev/ttyACM*` and `/dev/ttyUSB*` access for the OpenFanController, scoped via systemd `DeviceAllow=` and `SupplementaryGroups=uucp`. |
| Opt-in `/dev/port` (Super-I/O probe) | The active Super-I/O probe (DEC-203) identifies an *unbound* chip by reading its chip-ID registers at the two config ports (`0x2E`/`0x4E`), writing only the vendor-standard unlock/exit sequences needed to enter and cleanly leave config mode (hence the drop-in grants `/dev/port rw`). **Off by default**; requires `[detection] allow_port_probe` **and** `CAP_SYS_RAWIO`, granted only via the opt-in `superio-port-probe.conf.example` systemd drop-in. It never writes a configuration value or `force_id`, and refuses any port claimed by a bound driver or ACPI. |
| Opt-in `/dev/nvidia*` (NVML) | The NVML telemetry backend (DEC-204) loads `libnvidia-ml.so.1`, which needs **`rw`** access to the NVIDIA device nodes (standard for NVML, even for pure telemetry — hence the drop-in grants `rw`). The daemon resolves and calls **only read-side** NVML symbols (temperatures + fan duty); no fan-write or state-changing path exists. **Off by default**; requires `[detection] enable_nvidia_telemetry` plus the opt-in `nvidia-telemetry.conf.example` drop-in. The open `nouveau` driver needs neither (read via hwmon). |
| Config parsing | TOML deserialization of `daemon.toml` / `runtime.toml`. These config structs refuse unknown fields (`#[serde(deny_unknown_fields)]`). |
| Profile parsing | JSON deserialization of operator-supplied profiles. Profiles **intentionally do not** use `deny_unknown_fields`: every field is `#[serde(default)]` and unknown curve types fall through, so newer profiles stay forward-compatible on older daemons. Safety comes from explicit validation (bounds, floors, filesystem-safe ids — DEC-173), not from rejecting unknown keys. |
| Persistence | Atomic tmp+rename writes with `0600` permissions for `daemon_state.json` and `runtime.toml`. |

The daemon never executes operator-supplied code, never opens network
sockets, and never modifies files outside its declared write paths
(`ReadWritePaths=/sys/devices` — the device tree the fan `pwm*`/`fan_curve`
nodes physically live under; the `/sys/class/hwmon` and `/sys/class/drm`
paths are only symlinks that resolve here — and the
`StateDirectory=`/`RuntimeDirectory=` directories provided by systemd).

The systemd unit applies a hardening set (`ProtectSystem=strict`,
`NoNewPrivileges=true`, `RestrictNamespaces=true`,
`SystemCallFilter=@system-service`, etc.) — see
`packaging/control-ofc-daemon.service` for the full list.

## Supported Versions

| Version | Supported |
|---------|-----------|
| 2.x     | Yes       |
| 1.x     | No        |
| 0.x     | No        |

Only the latest 2.x release receives security fixes. Older patch
versions are not separately supported — upgrade to the current release.
The 1.x line is end-of-life (superseded by the 2.0.0 daemon-control
cutover); upgrade to 2.x.

## Companion project

The desktop GUI lives at
[control-ofc-gui](https://github.com/Plan-B-Development/control-ofc-gui)
with its own security policy. The two projects share an email contact.
