# control-ofc-daemon

Rust-based fan control daemon for the Control-OFC system. Manages hardware access (hwmon sysfs, OpenFanController serial, AMD GPU PMFW), runs safety rules, serves an HTTP API over a Unix socket, and **owns runtime fan control** — as of 2.0.0 its profile engine evaluates fan-curve profiles and is the sole writer of every backend (DEC-159/DEC-165). The GUI is an editor/viewer/controller-of-intent that never writes PWM.

## Build

```bash
cargo build --release
```

Run from the repository root. This is a Cargo workspace with two binaries, so
both land in the workspace-root `target/release/`: `control-ofc-daemon` and
`control-ofc-tray`.

## Install

> **Before installing**, verify the host has the kernel modules,
> DKMS drivers, BIOS settings, and (for RDNA3+ AMD GPUs) the kernel
> parameter described in the [Prerequisites section of the top-level
> README](../README.md#prerequisites). The package declares the common
> DKMS drivers as `optdepends`, but the user action items
> (BIOS / kernel command line) cannot be automated.

**Packaged (recommended):** install from the signed `[control-ofc]` pacman
repository — installs to `/usr/bin/`, and upgrades then arrive with your normal
`sudo pacman -Syu`. The setup commands (trust the key, add the repository,
install) are in the [Install section of the top-level
README](../README.md#install), which also covers the one-off `pacman -U` path
using the clean-room package attached to every release.

> **The AUR package is no longer updated** (DEC-240). `control-ofc-daemon` was
> published to the AUR through v2.13.0 and is frozen there; releases now go to
> GitHub only. The top-level README has the migration note for existing
> `paru -S control-ofc-daemon` installs.

**Build the package yourself:** `cd packaging && makepkg -si` builds the tagged
release from source and installs it. It fetches the release tarball, not your
checkout.

**Do not install by copying files.** The unit runs `/usr/bin/control-ofc-daemon`
and, after every stop, `/usr/bin/control-ofc-restore-auto`, which gives each fan
header back to what it was doing before the daemon took it. The package also
installs the sleep hook, the Super-I/O guard and
`/etc/modules-load.d/control-ofc.conf`. A binary copied to `/usr/local/bin` gets
none of these. To run a build of your own checkout, see
`docs/DEVELOPER_HANDOVER.md` § Running the daemon.

**Uninstall:** `sudo pacman -R control-ofc-daemon` — see `docs/USER_GUIDE.md`
§ Uninstall for what it leaves behind.

## CLI

```
control-ofc-daemon [OPTIONS]

Options:
  --config <path>         Path to daemon.toml (default: /etc/control-ofc/daemon.toml)
  --profile <name>        Load a named profile from search paths
  --profile-file <path>   Load a profile from an absolute file path
  --version               Print the daemon version and exit
  -h, --help              Print a usage summary and exit
```

Any other argument, a flag without its value, or a joined `--profile=quiet` is
refused with the usage and exit status 2 (DEC-467; older daemons ignored it, so
`--version` started a full daemon).

**Do not run the binary by hand while the service is active.** Only one daemon runs
at a time: a second one exits before it touches the socket or a fan, because it
cannot take the lock on `{state_dir}/daemon.lock` and will not remove a socket
another daemon is serving on.

The startup profile is the first of these that loads: `--profile` or
`--profile-file`, then `OPENFAN_PROFILE`, then the profile last activated through
the API (DEC-435). One that is missing or invalid is logged and the next is tried.
`<name>` is the file stem (`quiet` for `quiet.json`), not the profile's display
name. Neither is saved as the active profile, so removing the flag brings back the
last profile activated from the GUI. Under systemd, set either in a drop-in
(`systemctl edit control-ofc-daemon`) — see `docs/USER_GUIDE.md`.

`--allow-non-root` (development only, listed in `man control-ofc-daemon`) skips
the root-privilege check but not file or socket access checks. It is not for
production use.

## Environment variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Log level: `error`, `warn`, `info`, `debug`, `trace` |
| `CONTROL_OFC_CONFIG` | Path to daemon.toml (overridden by `--config` CLI arg) |
| `OPENFAN_PROFILE` | Profile file stem to load at startup; tried after `--profile`/`--profile-file` and before the saved profile |

## Configuration

Config file: `/etc/control-ofc/daemon.toml` — see `../packaging/daemon.toml.example`.

## API

HTTP over Unix socket at `/run/control-ofc/control-ofc.sock`.

```bash
curl --unix-socket /run/control-ofc/control-ofc.sock http://localhost/status
```

See `docs/DEVELOPER_HANDOVER.md` for developer onboarding and `daemon.md` for the architecture overview.

## Upgrade notes

For routine upgrades, the daemon reads forward-compatible config and migrates state in place. The notes below cover changes that require an operator action.

**v2.41.0 (thermal observation and startup recording, DEC-335):** Adds a `thermal_observation` session kind, CPU package power on a session sample, steady-state detection and a per-member startup fingerprint. All additive and gated on the new `control.thermal_observation` capability — **no operator action required.**

One new opt-in, **off by default**: `[startup] record_startup = true` in `/etc/control-ofc/daemon.toml` (**not** `runtime.toml`, whose `[startup]` section takes `delay_secs` alone and rejects the whole file if it sees anything else) makes the daemon record a short lifecycle session automatically at start, so a cooler's power-on behaviour can be captured without somebody sitting at the machine. It writes no hardware and takes no lease. It never blocks you: starting a session yourself takes the slot immediately and the partial recording is still saved, and these recordings are retained separately so they cannot displace sessions you made by hand. Needs no systemd drop-in.

Package power comes from a CPU chip's hwmon power attribute where one exists, else the kernel's powercap RAPL counter (root-only, which the daemon already is). **Many machines expose neither** — AMD's `k10temp` publishes no power attribute at all — and the value is then reported as unknown rather than as zero.

**v2.8.0 (NVIDIA read-only GPU support, DEC-204):** Read-only NVIDIA monitoring. The open **nouveau** driver is picked up automatically via hwmon (temperatures + fan RPM) — **no action required.** The proprietary **NVML** backend (temperatures + firmware-measured fan duty) is **opt-in, off by default**: set `[detection] enable_nvidia_telemetry = true` and install the `nvidia-telemetry.conf.example` systemd drop-in that grants NVML device access. No NVIDIA fan-write path exists in either mode. The other opt-in — the active Super-I/O port probe (v2.7.0, DEC-203) — is likewise off by default: enable it with `[detection] allow_port_probe = true` + the `superio-port-probe.conf.example` drop-in (`CAP_SYS_RAWIO`). Both example drop-ins ship under `/usr/share/doc/control-ofc-daemon/`.

**v1.18.0 (liquid-cooler / AIO support — Phase 1, DEC-156):** Adds hwmon-only AIO recognition — a `CoolantTemp` sensor kind, an `is_aio` flag on PWM headers, and a dynamic `aio_hwmon` capability `{present, status, pump_writable, coolant_available}` (an additive superset of the old `{present, status}`). That release added no coolant safety rule; DEC-443 (3.0.0) later added the coolant emergency. Purely additive; **no operator action required.** USB-only coolers remain out of scope (`aio_usb` stays `unsupported`).

**v1.15.0–v1.17.0 (profile schema v5 → v7):** Each step only *adds* a curve type — v5 Stepped, v6 Trigger, v7 Mix/Sync composites. They are purely additive: the daemon reads older profiles unchanged, and the GUI re-stamps a profile to v7 the next time it is saved. **No operator action required.**

**v1.6.0 (profile schema v4):** Profiles authored before v4 auto-migrate on load (role-aware `minimum_pct` floor lifted to 30 % for CPU/pump-labelled hwmon members, 20 % for chassis/openfan, 0 % for GPU-only). No file edit required; the migrated profile is re-saved when the user next persists it.

**`daemon.toml` and `runtime.toml` — no action required:** These sections stay valid; they are the admin-owned **base** defaults for the profile search dirs and startup delay, and the daemon still parses them. When an API call mutates one of those keys (`POST /config/profile-search-dirs` / `POST /config/startup-delay`) the daemon writes a `runtime.toml` whose keys **overlay** the `daemon.toml` defaults (runtime wins, ADR-002). The two files coexist — nothing is copied, no section is removed, and having these sections in either file is never an error (an unknown key or an out-of-range value in `daemon.toml` still is: see `docs/USER_GUIDE.md` § Configuration). If `runtime.toml` ends up shadowing a non-default `daemon.toml` value, the daemon notes it once in an `info` log at startup. **v2.16.0 (DEC-243)** widened the set of keys this applies to — `[polling] poll_interval_ms`, `[serial] port`/`timeout_ms` and the two `[detection]` opt-ins are now settable through the API as well, and `GET /config` reports every key with its value, its source (`runtime`/`admin`/`default`) and whether a saved change is still waiting on a restart. `ipc.socket_path` and `state.state_dir` remain read-only by design. **v2.23.0 (DEC-285)** made `POST /config/profile-search-dirs` accept a `remove` array as well as `add`, so a stale search directory can be pruned through the API instead of only ever added — the endpoint was add-only, and a client that re-registered a moved profiles directory left the old entry behind permanently. `/etc/control-ofc/profiles` and the last remaining entry are refused; the capability flag is `control.profile_search_dir_remove`. **No operator action required.**

**Pre-v1.2 telemetry / polling:** `[telemetry]` and the `publish_interval_ms` field under `[polling]` were removed in the v0.7.x series. Anyone still upgrading from a pre-v0.8 install must delete those lines before starting the v1.x daemon.

For full upgrade details and the per-version contract changes, see `docs/USER_GUIDE.md` and the `CHANGELOG.md` at the repo root.

## Quality gates

**The gate commands are the ones CI runs: see the `cargo` steps in
`.github/workflows/ci.yml`.** They are not repeated here — a second copy of a
command list is a second thing to keep true, and an earlier copy here drifted.

CI also runs `cargo deny` against `deny.toml` (the `deny` job in the same file);
`cargo audit` is run by hand at release time. `deny.toml` encodes the project's license/advisory policy (DEC-043
no-LGPL, DEC-155 serialport MPL-2.0).
