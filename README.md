# control-ofc-daemon

**Latest release:** v2.8.0 — 2026-07-08. Pairs with `control-ofc-gui` ≥ v2.11.0. See [`CHANGELOG.md`](CHANGELOG.md) for the full history.

Rust workspace for the Control-OFC fan control daemon.

> A privileged Linux daemon that manages fan hardware (hwmon sysfs, OpenFanController
> serial, AMD GPU PMFW) and serves an HTTP API over a Unix socket for the
> `control-ofc-gui` PySide6 desktop application. It is the **autonomous sole
> controller** (2.0.0+): its profile engine evaluates the active profile and is
> the only writer of every backend, keeping fans controlled headless through GUI
> close, crash, or sleep. The GUI is an editor/viewer/controller-of-intent that
> never writes PWM.

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

These prerequisites change kernel modules, firmware (UEFI/BIOS) settings,
and boot parameters. They are informational, provided as-is without
warranty, and applied at your own risk — the project accepts no liability
(MIT License). For guided, sourced walkthroughs see the GUI manual's
[Setup Checklist][setup-checklist] and [Driver Setup][gui-driver-setup]
pages.

| Prerequisite | Required for | How it is satisfied |
|---|---|---|
| Linux kernel ≥ 5.10, hwmon sysfs, `cdc_acm` module | All operation | Standard on every supported distro; the systemd unit pulls `cdc_acm` for OpenFan |
| Super I/O kernel module loaded — `nct6775`, `it87`, `w83627ehf`, `drivetemp` | Motherboard fan / sensor control | The package ships `/etc/modules-load.d/control-ofc.conf`. Loaded at next boot, or immediately via `sudo systemctl start systemd-modules-load` |
| Out-of-tree DKMS driver — `it87-dkms-git`, `nct6687d-dkms-git`, `nct6686d-dkms-git` | Most newer (2022+) Gigabyte / MSI / ASRock boards — fan control is read-only without these | Install the matching AUR package; declared as `optdepends`. The GUI's Diagnostics → Troubleshooting readiness report identifies the chip and recommends the exact package |
| `dkms` + `linux-headers` matching the running kernel | Building any of the DKMS drivers above | Pulled in transitively via the DKMS packages, but `linux-headers` must match the kernel you actually boot |
| UEFI Secure Boot disabled, or DKMS modules signed | Loading any `*-dkms-git` driver with Secure Boot enabled | Unsigned out-of-tree modules build but fail to load (`Key was rejected by service`). Detection and options (disable vs sign, CachyOS caveat): [GUI manual — Driver Setup § Secure Boot][gui-secure-boot] |
| BIOS configured for Linux fan control | Most Gigabyte / MSI boards, some ASRock | "Smart Fan" disabled or set to a degenerate (max) curve. See the [vendor-by-vendor BIOS guide][vendor-bios] |
| `amdgpu.ppfeaturemask=0xffffffff` on the kernel command line | RDNA3+ (RX 7000 / RX 9000) GPU fan-curve writes | Add to your bootloader; see `man control-ofc-daemon` for per-bootloader instructions. Pre-RDNA3 cards do not require this |
| `acpi_enforce_resources=lax` (or `it87 ignore_resource_conflict=1`) | Some Gigabyte / ASUS boards with ACPI OpRegion conflicts | The daemon's `/diagnostics/hardware` endpoint and the GUI's Hardware Readiness card detect the conflict and surface the remediation |
| Current `it87-dkms-git` build (2026-03+; older builds need `/etc/modprobe.d/it87.conf` with `options it87 mmio=on`) | Dual-IT-chip Gigabyte boards (e.g. X870E AORUS MASTER, DEC-101/DEC-144) — current builds enumerate and control the secondary chip by default | User action; the GUI surfaces the exact remediation when the dual-chip case is detected. (One counter-case: IT8665E boards need `mmio=off` on current builds — frankcrawford/it87 issue #106) |

If your board is already working under any other Linux fan control tool
(fancontrol, lm_sensors with pwmconfig, CoolerControl, CoreCtrl, fan2go),
the right driver is almost certainly already loaded and the daemon will
inherit that configuration — but **stop and disable those tools before
the daemon takes over the same headers**: PWM sysfs values have one
writer at a time, and two controllers fight each other (see the GUI
manual's [Setup Checklist][setup-checklist], step 5). After installation,
**Diagnostics → Troubleshooting** in the GUI is the most reliable way to
discover what your specific system needs without trial and error.

[vendor-bios]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/docs/21_AMD_Motherboard_Fan_Control_Guide.md
[setup-checklist]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/setup-checklist.md
[gui-driver-setup]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/driver-setup.md
[gui-secure-boot]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/driver-setup.md#secure-boot-and-dkms-modules

## Quick start

```bash
# Build (workspace member — binary lands in the workspace-root target/)
cd daemon
cargo build --release

# Install (run from inside daemon/ — the binary is one level up)
sudo cp ../target/release/control-ofc-daemon /usr/local/bin/
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
| [GUI manual — OpenFan Controller][gui-openfan] | end users | What the OpenFan Controller is and how Control-OFC drives it through the daemon (detection, serial access, stable paths, troubleshooting) |
| [GUI manual — Understanding Motherboard Fan Control][gui-understanding-fans] | end users | Plain-English primer on hwmon, Super I/O, and PWM for new users |

[gui-openfan]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/openfan-controller.md
[gui-understanding-fans]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/understanding-fan-control.md

## Architecture summary

- **Three fan backends**: OpenFanController (serial/USB), motherboard hwmon (sysfs
  PWM), and AMD GPU (RDNA3+ PMFW fan curves, legacy hwmon PWM for pre-RDNA3).
- **HTTP over Unix domain socket** at `/run/control-ofc/control-ofc.sock`, exposing
  both snapshot reads (`/poll`) and a real-time SSE stream (`/events`).
- **Thermal safety** is daemon-enforced: 105°C CPU trigger → all OpenFan and
  motherboard (hwmon) fans to 100%, 25°C hysteresis, 40% fallback when no CPU
  sensor reports for 5 cycles. GPU fans are excluded — AMD PMFW firmware owns
  GPU thermal protection independently of OS fan control (DEC-130).
- **Headless profile engine** (`profile_engine/`) evaluates the active profile's
  fan curves autonomously on a 1 Hz loop and is the **sole writer** of every
  backend (2.0.0+, DEC-159/DEC-165). There is no GUI defer window — the 30 s
  `gui_active` defer (DEC-071/074) was deleted at the 2.0.0 cutover; the GUI never
  writes PWM.
- **Lease system** provides exclusive hwmon write access (60 s TTL), held
  **internally** by the profile engine, to guard against conflicting external
  hwmon writers. The GUI holds no lease (DEC-165).
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
