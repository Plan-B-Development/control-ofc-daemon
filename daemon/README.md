# control-ofc-daemon

Rust-based fan control daemon for the Control-OFC system. Manages hardware access (hwmon sysfs, OpenFanController serial, AMD GPU PMFW), runs safety rules, serves an HTTP API over a Unix socket, and optionally evaluates fan curve profiles autonomously.

## Build

```bash
cd daemon
cargo build --release
```

Binary: `target/release/control-ofc-daemon`

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
sudo cp target/release/control-ofc-daemon /usr/local/bin/
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

**v1.15.0–v1.17.0 (profile schema v5 → v7):** Each step only *adds* a curve type — v5 Stepped, v6 Trigger, v7 Mix/Sync composites. They are purely additive: the daemon reads older profiles unchanged, and the GUI re-stamps a profile to v7 the next time it is saved. **No operator action required.**

**v1.6.0 (profile schema v4):** Profiles authored before v4 auto-migrate on load (role-aware `minimum_pct` floor lifted to 30 % for CPU/pump-labelled hwmon members, 20 % for chassis/openfan, 0 % for GPU-only). No file edit required; the migrated profile is re-saved when the user next persists it.

**v1.2.0 (legacy config sections removed):** Parsing `[profiles]` or `[startup]` from `daemon.toml` is now a hard error. Those sections were moved to the daemon-managed `runtime.toml` (ADR-002). If you upgraded from v1.1.x or earlier and your `daemon.toml` still has either section, delete the relevant block and reload — the daemon will recreate state in `runtime.toml` on first runtime mutation.

**Pre-v1.2 telemetry / polling:** `[telemetry]` and the `publish_interval_ms` field under `[polling]` were removed in the v0.7.x series. Anyone still upgrading from a pre-v0.8 install must delete those lines before starting the v1.x daemon.

For full upgrade details and the per-version contract changes, see `docs/USER_GUIDE.md` and the `CHANGELOG.md` at the repo root.

## Tests

```bash
cargo test
```
