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

**v0.7.1:** The `publish_interval_ms` field under `[polling]` has been removed. If your `daemon.toml` contains it, the daemon will fail to start. Remove the line: `sudo sed -i '/publish_interval_ms/d' /etc/control-ofc/daemon.toml`

**v0.7.0:** The `[telemetry]` config section has been removed. Remove it from your `daemon.toml` if present.

See `docs/USER_GUIDE.md` for full upgrade details.

## Tests

```bash
cargo test
```
