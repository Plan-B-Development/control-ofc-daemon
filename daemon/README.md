# control-ofc-daemon

Rust-based fan control daemon for the Control-OFC system. Manages hardware access (hwmon sysfs, OpenFanController serial, AMD GPU PMFW), runs safety rules, serves an HTTP API over a Unix socket, and optionally evaluates fan curve profiles autonomously.

## Build

```bash
cd daemon
cargo build --release
```

Binary: `target/release/control-ofc-daemon`

## Install

**AUR (recommended):** `paru -S control-ofc-daemon` — installs to `/usr/bin/`.

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
  --allow-non-root        Skip root privilege check (dev/testing only)
```

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

**v1.6.0 (profile schema v4):** Profiles authored before v4 auto-migrate on load (role-aware `minimum_pct` floor lifted to 30 % for CPU/pump-labelled hwmon members, 20 % for chassis/openfan, 0 % for GPU-only). No file edit required; the migrated profile is re-saved when the user next persists it.

**v1.2.0 (legacy config sections removed):** Parsing `[profiles]` or `[startup]` from `daemon.toml` is now a hard error. Those sections were moved to the daemon-managed `runtime.toml` (ADR-002). If you upgraded from v1.1.x or earlier and your `daemon.toml` still has either section, delete the relevant block and reload — the daemon will recreate state in `runtime.toml` on first runtime mutation.

**Pre-v1.2 telemetry / polling:** `[telemetry]` and the `publish_interval_ms` field under `[polling]` were removed in the v0.7.x series. Anyone still upgrading from a pre-v0.8 install must delete those lines before starting the v1.x daemon.

For full upgrade details and the per-version contract changes, see `docs/USER_GUIDE.md` and the `CHANGELOG.md` at the repo root.

## Tests

```bash
cargo test
```
