# control-ofc-daemon

**Latest release:** v1.8.1 — 2026-06-01. Pairs with `control-ofc-gui` ≥ v1.14.1. See [`CHANGELOG.md`](CHANGELOG.md) for the full history.

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

## Prerequisites

Before installing, work through the table below. The AUR package handles
most items via `depends`, `optdepends`, and a shipped
`/etc/modules-load.d/control-ofc.conf`. A few rows remain user actions
that no package can perform safely (BIOS settings, kernel command line).

| Prerequisite | Required for | How it is satisfied |
|---|---|---|
| Linux kernel ≥ 5.10, hwmon sysfs, `cdc_acm` module | All operation | Standard on every supported distro; the systemd unit pulls `cdc_acm` for OpenFan |
| Super I/O kernel module loaded — `nct6775`, `it87`, `w83627ehf`, `drivetemp` | Motherboard fan / sensor control | The package ships `/etc/modules-load.d/control-ofc.conf`. Loaded at next boot, or immediately via `sudo systemctl start systemd-modules-load` |
| Out-of-tree DKMS driver — `it87-dkms-git`, `nct6687d-dkms-git`, `nct6686d-dkms-git` | Most newer (2022+) Gigabyte / MSI / ASRock boards — fan control is read-only without these | Install the matching AUR package; declared as `optdepends`. The GUI's Diagnostics → Fans → Hardware Readiness card identifies the chip and recommends the exact package |
| `dkms` + `linux-headers` matching the running kernel | Building any of the DKMS drivers above | Pulled in transitively via the DKMS packages, but `linux-headers` must match the kernel you actually boot |
| BIOS configured for Linux fan control | Most Gigabyte / MSI boards, some ASRock | "Smart Fan" disabled or set to a degenerate (max) curve. See the [vendor-by-vendor BIOS guide][vendor-bios] |
| `amdgpu.ppfeaturemask=0xffffffff` on the kernel command line | RDNA3+ (RX 7000 / RX 9000) GPU fan-curve writes | Add to your bootloader; see `man control-ofc-daemon` for per-bootloader instructions. Pre-RDNA3 cards do not require this |
| `acpi_enforce_resources=lax` (or `it87 ignore_resource_conflict=1`) | Some Gigabyte / ASUS boards with ACPI OpRegion conflicts | The daemon's `/diagnostics/hardware` endpoint and the GUI's Hardware Readiness card detect the conflict and surface the remediation |
| `/etc/modprobe.d/it87.conf` with `options it87 mmio=on` | Some dual-IT-chip Gigabyte boards (e.g. X870E AORUS MASTER, DEC-101) | User action; the GUI surfaces the exact remediation when the dual-chip case is detected |

If your board is already working under any other Linux fan control tool
(fancontrol, lm_sensors with pwmconfig, CoolerControl, CoreCtrl), the
right driver is almost certainly already loaded and the daemon will
inherit that configuration. After installation, **Diagnostics → Fans →
Hardware Readiness** in the GUI is the most reliable way to discover
what your specific system needs without trial and error.

[vendor-bios]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/docs/21_AMD_Motherboard_Fan_Control_Guide.md

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
