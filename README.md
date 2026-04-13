# control-ofc-daemon

Rust workspace for the Control-OFC fan control daemon.

> A privileged Linux daemon that manages fan hardware (hwmon sysfs, OpenFanController
> serial, AMD GPU PMFW) and serves an HTTP API over a Unix socket for the
> `control-ofc-gui` PySide6 desktop application. Runs headless with autonomous
> profile evaluation, or as a passive backend for the GUI.

## Workspace layout

```text
.
├── Cargo.toml                # workspace manifest
├── daemon/                   # control-ofc-daemon crate (the binary)
│   ├── src/                  # daemon source (see daemon.md for module map)
│   └── README.md             # build, install, CLI, env vars, API quick-start
├── packaging/                # systemd unit, udev rules, shutdown restore script
├── docs/                     # user + developer documentation
│   ├── USER_GUIDE.md
│   ├── DEVELOPER_HANDOVER.md
│   └── ADRs/                 # architecture decision records
├── daemon.md                 # architecture overview (module map, data flow, safety)
├── CHANGELOG.md              # release history
└── LICENSE                   # MIT
```

## Quick start

```bash
# Build
cd daemon
cargo build --release

# Install
sudo cp target/release/control-ofc-daemon /usr/local/bin/
sudo cp ../packaging/control-ofc-daemon.service /etc/systemd/system/
sudo mkdir -p /etc/control-ofc
sudo cp ../packaging/daemon.toml.example /etc/control-ofc/daemon.toml
sudo systemctl daemon-reload
sudo systemctl enable --now control-ofc-daemon

# Verify
curl --unix-socket /run/control-ofc/control-ofc.sock http://localhost/status
```

Full build / install / CLI / environment reference lives in
[`daemon/README.md`](daemon/README.md).

## Documentation index

| Document | Audience | Purpose |
|---|---|---|
| [`daemon.md`](daemon.md) | all | Architecture overview, module map, data flow, safety model, full API endpoint table |
| [`daemon/README.md`](daemon/README.md) | operators | Build, install, CLI flags, env vars, config |
| [`docs/USER_GUIDE.md`](docs/USER_GUIDE.md) | end users | Configuration, profiles, upgrade notes |
| [`docs/DEVELOPER_HANDOVER.md`](docs/DEVELOPER_HANDOVER.md) | contributors | Developer onboarding, full API reference |
| [`docs/ADRs/`](docs/ADRs/) | contributors | Architecture decision records |
| [`CHANGELOG.md`](CHANGELOG.md) | all | Release history |

## Architecture summary

- **Three fan backends**: OpenFanController (serial/USB), motherboard hwmon (sysfs
  PWM), and AMD GPU (RDNA3+ PMFW fan curves, legacy hwmon PWM for pre-RDNA3).
- **HTTP over Unix domain socket** at `/run/control-ofc/control-ofc.sock`, exposing
  both snapshot reads (`/poll`) and a real-time SSE stream (`/events`).
- **Thermal safety** is daemon-enforced: 105°C CPU trigger → 100% fans, 25°C
  hysteresis, 40% fallback when no CPU sensor reports for 5 cycles.
- **Headless profile engine** (`profile_engine.rs`) evaluates fan curves autonomously
  on a 1 Hz loop; defers to the GUI when the GUI has written in the last 30 seconds
  (DEC-071, DEC-074).
- **Lease system** provides exclusive hwmon write access (60 s TTL) to prevent
  GUI/profile-engine write races.
- **Systemd-hardened** (`ProtectHome=read-only`, `ProtectSystem=strict`,
  `SystemCallFilter=@system-service`, etc.); shutdown restores
  `pwm_enable=2` and GPU fan curves to automatic via `ExecStopPost`.

## Pairing with the GUI

The GUI repo lives at `control-ofc-gui` (separate repository).
GUI ↔ daemon is a strict client/server boundary: the GUI is **never** permitted to
touch hardware directly. All reads and writes flow through this daemon's HTTP API.
The full contract is documented in the GUI repo's `docs/08_API_Integration_Contract.md`.

## License

MIT — see [`LICENSE`](LICENSE).
