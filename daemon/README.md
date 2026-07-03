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
> README](../README.md#prerequisites). The AUR package declares the
> common DKMS drivers as `optdepends`, but the user action items
> (BIOS / kernel command line) cannot be automated.

**AUR (recommended):** `paru -S control-ofc-daemon` — installs to `/usr/bin/`.

> **Tip — first-time AUR install UX:** paru pages the `PKGBUILD` and `.install`
> through `less` and asks you to confirm before building. That is paru's default
> security review (press `q` to exit the pager, then `y` to proceed), not
> specific to this package. To install non-interactively, pass `--skipreview`
> to paru (`paru -S --skipreview control-ofc-daemon`), or add `SkipReview` to
> the `[options]` section of `~/.config/paru/paru.conf`.

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

See `docs/DEVELOPER_HANDOVER.md` for the full API reference.

## Upgrade notes

For routine upgrades, the daemon reads forward-compatible config and migrates state in place. The notes below cover changes that require an operator action.

**v1.18.0 (liquid-cooler / AIO support — Phase 1, DEC-156):** Adds hwmon-only AIO recognition — a `CoolantTemp` sensor kind, an `is_aio` flag on PWM headers, and a dynamic `aio_hwmon` capability `{present, status, pump_writable, coolant_available}` (an additive superset of the old `{present, status}`). There is **no coolant safety rule** — `safety.rs` stays CPU-only. Purely additive; **no operator action required.** USB-only coolers remain out of scope (`aio_usb` stays `unsupported`).

**v1.15.0–v1.17.0 (profile schema v5 → v7):** Each step only *adds* a curve type — v5 Stepped, v6 Trigger, v7 Mix/Sync composites. They are purely additive: the daemon reads older profiles unchanged, and the GUI re-stamps a profile to v7 the next time it is saved. **No operator action required.**

**v1.6.0 (profile schema v4):** Profiles authored before v4 auto-migrate on load (role-aware `minimum_pct` floor lifted to 30 % for CPU/pump-labelled hwmon members, 20 % for chassis/openfan, 0 % for GPU-only). No file edit required; the migrated profile is re-saved when the user next persists it.

**`daemon.toml` `[profiles]` / `[startup]` — no action required:** These sections stay valid; they are the admin-owned **base** defaults for the profile search dirs and startup delay, and the daemon still parses them. When an API call mutates one of those keys (`POST /config/profile-search-dirs` / `POST /config/startup-delay`) the daemon writes a `runtime.toml` whose keys **overlay** the `daemon.toml` defaults (runtime wins, ADR-002). The two files coexist — nothing is copied, no section is removed, and parsing either one is never an error. If `runtime.toml` ends up shadowing a non-default `daemon.toml` value, the daemon notes it once in an `info` log at startup.

**Pre-v1.2 telemetry / polling:** `[telemetry]` and the `publish_interval_ms` field under `[polling]` were removed in the v0.7.x series. Anyone still upgrading from a pre-v0.8 install must delete those lines before starting the v1.x daemon.

For full upgrade details and the per-version contract changes, see `docs/USER_GUIDE.md` and the `CHANGELOG.md` at the repo root.

## Quality gates

Standard gates (run on every change):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all --all-features
```

Release-time supply-chain gates (DEC-174): `deny.toml` encodes the project's
license/advisory policy (DEC-043 no-LGPL, DEC-155 serialport MPL-2.0).

```bash
cargo audit
cargo deny check
```
