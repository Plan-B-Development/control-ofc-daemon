# control-ofc-daemon

Rust-based fan control daemon for the Control-OFC system. Manages hardware access (hwmon sysfs, OpenFanController serial, AMD GPU PMFW), runs safety rules, serves an HTTP API over a Unix socket, and **owns runtime fan control** — as of 2.0.0 its profile engine evaluates fan-curve profiles and is the sole writer of every backend (DEC-159/DEC-165). The GUI is an editor/viewer/controller-of-intent that never writes PWM.

## Build

```bash
cd daemon
cargo build --release
```

Binary: `../target/release/control-ofc-daemon` (this is a Cargo workspace
member — the build emits to the workspace-root `target/`, one level above
`daemon/`).

## Install

> **Before installing manually**, verify the host has the kernel modules,
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

**Manual:**

```bash
sudo cp ../target/release/control-ofc-daemon /usr/local/bin/
sudo cp ../packaging/control-ofc-daemon.service /etc/systemd/system/
sudo mkdir -p /etc/control-ofc
sudo cp ../packaging/daemon.toml.example /etc/control-ofc/daemon.toml
sudo systemctl daemon-reload
sudo systemctl enable --now control-ofc-daemon
```

> **Note:** The packaged AUR install places the binary at `/usr/bin/control-ofc-daemon`. Manual installs use `/usr/local/bin/`. The systemd service file references `/usr/bin/` — update `ExecStart` if you installed manually.

## CLI

```
control-ofc-daemon [OPTIONS]

Options:
  --config <path>         Path to daemon.toml (default: /etc/control-ofc/daemon.toml)
  --profile <name>        Load a named profile from search paths
  --profile-file <path>   Load a profile from an absolute file path
```

(A hidden `--allow-non-root` flag exists for development only — it skips the
root-privilege check but not file/socket access checks. It is intentionally
undocumented for production use.)

## Environment variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Log level: `error`, `warn`, `info`, `debug`, `trace` |
| `CONTROL_OFC_CONFIG` | Path to daemon.toml (overridden by `--config` CLI arg) |
| `OPENFAN_PROFILE` | Profile name to load at startup (fallback if no `--profile`) |

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

**v1.18.0 (liquid-cooler / AIO support — Phase 1, DEC-156):** Adds hwmon-only AIO recognition — a `CoolantTemp` sensor kind, an `is_aio` flag on PWM headers, and a dynamic `aio_hwmon` capability `{present, status, pump_writable, coolant_available}` (an additive superset of the old `{present, status}`). There is **no coolant safety rule** — `safety.rs` stays CPU-only. Purely additive; **no operator action required.** USB-only coolers remain out of scope (`aio_usb` stays `unsupported`).

**v1.15.0–v1.17.0 (profile schema v5 → v7):** Each step only *adds* a curve type — v5 Stepped, v6 Trigger, v7 Mix/Sync composites. They are purely additive: the daemon reads older profiles unchanged, and the GUI re-stamps a profile to v7 the next time it is saved. **No operator action required.**

**v1.6.0 (profile schema v4):** Profiles authored before v4 auto-migrate on load (role-aware `minimum_pct` floor lifted to 30 % for CPU/pump-labelled hwmon members, 20 % for chassis/openfan, 0 % for GPU-only). No file edit required; the migrated profile is re-saved when the user next persists it.

**`daemon.toml` and `runtime.toml` — no action required:** These sections stay valid; they are the admin-owned **base** defaults for the profile search dirs and startup delay, and the daemon still parses them. When an API call mutates one of those keys (`POST /config/profile-search-dirs` / `POST /config/startup-delay`) the daemon writes a `runtime.toml` whose keys **overlay** the `daemon.toml` defaults (runtime wins, ADR-002). The two files coexist — nothing is copied, no section is removed, and parsing either one is never an error. If `runtime.toml` ends up shadowing a non-default `daemon.toml` value, the daemon notes it once in an `info` log at startup. **v2.16.0 (DEC-243)** widened the set of keys this applies to — `[polling] poll_interval_ms`, `[serial] port`/`timeout_ms` and the two `[detection]` opt-ins are now settable through the API as well, and `GET /config` reports every key with its value, its source (`runtime`/`admin`/`default`) and whether a saved change is still waiting on a restart. `ipc.socket_path` and `state.state_dir` remain read-only by design. **v2.23.0 (DEC-285)** made `POST /config/profile-search-dirs` accept a `remove` array as well as `add`, so a stale search directory can be pruned through the API instead of only ever added — the endpoint was add-only, and a client that re-registered a moved profiles directory left the old entry behind permanently. `/etc/control-ofc/profiles` and the last remaining entry are refused; the capability flag is `control.profile_search_dir_remove`. **No operator action required.**

**Pre-v1.2 telemetry / polling:** `[telemetry]` and the `publish_interval_ms` field under `[polling]` were removed in the v0.7.x series. Anyone still upgrading from a pre-v0.8 install must delete those lines before starting the v1.x daemon.

For full upgrade details and the per-version contract changes, see `docs/USER_GUIDE.md` and the `CHANGELOG.md` at the repo root.

## Quality gates

**The canonical command set lives in `CLAUDE.md § Quality gates` at the repo root.**
Run it from there; it is not repeated here.

This section used to restate the commands and had drifted away from the canonical
set — it specified `cargo test --all`, where CLAUDE.md specified
`cargo test --all-targets`, and those are not equivalent (`--all-targets` suppresses
doctests). Both also carried an `--all-features` flag that did nothing, since this
crate has no `[features]` table. A second copy of a command list is a second thing to
keep true, and this one was not.

Release-time supply-chain gates are in the same CLAUDE.md block. For context on what
they enforce: `deny.toml` encodes the project's license/advisory policy (DEC-043
no-LGPL, DEC-155 serialport MPL-2.0).
