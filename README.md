# control-ofc-daemon

**Latest release:** v2.31.0 — 2026-09-03. Pairs with `control-ofc-gui` ≥ v2.23.0 (the recommended capability floor; the package itself only hard-blocks GUIs < 2.0.0, the sole-writer cutover). v2.42.0 or newer drives the `POST /fans/openfan/rescan` route, v2.44.0 or newer surfaces the `skipped_controls[]` field as a "Not controlled" badge, v2.45.0 or newer consumes the `control_outputs[]` field and the `controls` health subsystem, v2.49.0 or newer removes a profile search directory via `control.profile_search_dir_remove`, v2.49.2 or newer shows the thermal-forcing verify refusal as a soft notice rather than an error, v2.49.5 or newer mirrors the widened `nct67xx` `CPUTIN` warning list this daemon acts on, and v2.49.6 or newer documents `emergency_threshold_c` as per-machine rather than a fixed 105 °C; older GUIs ignore or mis-word them but keep working. **None of 2.26.0, 2.27.0 or 2.28.0 requires a new GUI floor.** 2.26.0 made the thermal trip point per-machine (DEC-308) on a field every GUI already renders verbatim, and made the ladder's forced duties floors over profile output (DEC-307), with no wire change at all. 2.27.0 (DEC-309) stops reporting fan telemetry nothing measured — `rpm` is omitted rather than zero for an unpolled OpenFan channel, a GPU fan's `age_ms` is no longer reset by a command, and a hwmon fan that stops reading is evicted rather than published forever. `rpm` was already optional and a fan's absence already meant "not currently readable", so every released GUI handles all three unchanged. 2.28.0 (DEC-311) adds per-channel PWM header roles (`role`/`role_source` on the header and inventory responses), `POST /config/header-role`, and the `control.header_roles` capability; fan identify no longer stops a header the daemon knows to be a pump, perturbing its speed instead, and `POST /hwmon/{id}/verify` no longer drives one below the 30 % pump floor. Every field is additive and the identify request shape is unchanged, so an older GUI keeps working and — because the daemon, not the client, decides what a `stop` means — cannot ask for a pump stop even by accident; v2.50.0 or newer words the wizard's prompts accordingly. 2.28.1 is documentation only — the binary is identical to 2.28.0 — correcting the shipped `USER_GUIDE.md`, which stated a constant-speed pump as fact; GUI v2.51.0 retracted that rule and can assign the `pump` role from its Configure-AIO dialog. 2.29.0 adds PWM/RPM response characterisation — `POST /hwmon/{id}/characterize` plus the `GET`/`DELETE /diagnostics/characterization` pair, gated on the new `control.pwm_characterization` capability. It sits **alongside** the ~6 s verify rather than replacing it: it sweeps a header across a series of duties and reports command acceptance, PWM readback and physical RPM response as three independent verdicts, because a pump that overrides PWM during its startup or self-bleeding period reports a correct readback with its speed pinned high, and collapsing the three would call that a write failure. **0% is unreachable through the endpoint for any header**, a pump is never swept below its 30 % floor, points ascend so an abort leaves the header high, and the pre-sweep duty is restored on every exit path on which nothing else owns the header — the sweep runs daemon-side precisely so that restoration does not depend on the client surviving. Additive throughout; GUI v2.52.0 or newer drives it and older GUIs ignore the flag. 2.30.0 fixes what that sweep *said* about the two exits where it deliberately does not restore: a shutdown, a thermal force, or a pre-sweep duty it could never read all reported `restore_failed: false`, which the field's own contract defines as "the header is back where it was". It now reports the truth, with a new `restore_outcome` token saying which — and the reason is load-bearing rather than cosmetic, because under a thermal force the header is high on purpose and the one action a bare "restore failed" invites is the one a client must not take. Additive; GUI v2.53.1 or newer words each reason for the user, and an older GUI keeps the message it had. 2.31.0 (DEC-316) makes a cooler a first-class **cooling device**: `GET /inventory/cooling-devices` plus `POST`/`DELETE /config/cooling-device`, gated on the new `control.cooling_devices` capability, describe a pump header, its radiator fans and an advisory temperature source as one named assembly, persisted as a top-level `[[cooling_devices]]` array in `runtime.toml`. **Topology is metadata and the profile engine never reads it** — naming a header as a device's `pump_member` confers no pump protection, which is still `POST /config/header-role`. It also adds a trusted device-capability policy whose numbers are compiled into the binary and selected by id (the Rust type derives no `Deserialize`, so no payload can construct one, and the endpoint rejects `minimum_safe_pwm` and its siblings by name rather than ignoring them); headers now report `effective_min_pwm_pct` and `stop_permitted` so a client can display the enforced floor instead of re-deriving it; and `/poll` carries each hwmon header's `fan_alarm` and live `pwm_enable_mode`. **Only generic policies ship, so no floor moves** — the generic pump's floor is the 30 % constant the engine already enforced. Every field is additive and optional, so an older GUI keeps working; GUI v2.54.0 or newer drives the new surface. See [`CHANGELOG.md`](CHANGELOG.md) for the full history.

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

Before installing, work through the table below. The Arch package handles
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
| Out-of-tree DKMS driver — `it87-dkms-git`, `nct6687d-dkms-git`, `nct6686d-dkms-git` | Most newer (2022+) Gigabyte / MSI / ASRock boards — fan control is read-only without these | Install the matching AUR package; declared as `optdepends`. The GUI's Hardware page readiness report identifies the chip and recommends the exact package |
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
the **Hardware** page in the GUI is the most reliable way to
discover what your specific system needs without trial and error.

[vendor-bios]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/docs/21_AMD_Motherboard_Fan_Control_Guide.md
[setup-checklist]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/setup-checklist.md
[gui-driver-setup]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/driver-setup.md
[gui-secure-boot]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/driver-setup.md#secure-boot-and-dkms-modules

## Install

**Signed pacman repository (recommended).** Set it up once; the daemon then
upgrades with your normal `sudo pacman -Syu`. Arch / x86_64.

```bash
# 1. trust the signing key
curl -fsSL https://raw.githubusercontent.com/Plan-B-Development/pacman-repo/main/keys/control-ofc.gpg \
  | sudo pacman-key --add -
sudo pacman-key --lsign-key 4AAD6D2DE40D0D10773BF770BC27C5EB2831FCDA

# 2. add the repository — run once; `tee -a` would append a duplicate block
grep -q '^\[control-ofc\]' /etc/pacman.conf || sudo tee -a /etc/pacman.conf <<'EOF'

[control-ofc]
SigLevel = Required
Server = https://github.com/Plan-B-Development/pacman-repo/releases/download/repo
EOF

# 3. install
sudo pacman -Syu control-ofc-daemon
sudo systemctl enable --now control-ofc-daemon
```

There is also a signed `bootstrap.sh` that does all of the above (and checks the
signing key's fingerprint before trusting it) — see
[pacman-repo § Install](https://github.com/Plan-B-Development/pacman-repo#install).

`SigLevel = Required` means pacman refuses any package or database not signed by
that key. The repository also carries `control-ofc-gui`, so
`pacman -Syu control-ofc-gui` installs both. Details, upgrade and removal
instructions: [Plan-B-Development/pacman-repo](https://github.com/Plan-B-Development/pacman-repo).

**One-off install without touching `pacman.conf`:** every release also attaches
the same clean-room-built package the CI pipeline verifies (a full `cargo build
--release` + `cargo test`).

```bash
gh release download --repo Plan-B-Development/control-ofc-daemon --pattern '*.pkg.tar.zst'
sudo pacman -U ./control-ofc-daemon-*.pkg.tar.zst
sudo systemctl enable --now control-ofc-daemon
```

Upgrading then means repeating those commands — which is the chore the
repository above exists to remove. Each package additionally carries a keyless
[Sigstore](https://www.sigstore.dev/) build provenance attestation:

```bash
gh attestation verify ./control-ofc-daemon-*.pkg.tar.zst \
  --repo Plan-B-Development/control-ofc-daemon
```

**Build the package yourself** from the in-repo `PKGBUILD` instead — same
result, and it does not trust a prebuilt binary:

```bash
git clone https://github.com/Plan-B-Development/control-ofc-daemon.git
cd control-ofc-daemon/packaging
makepkg -si
```

> The in-repo `sha256sums` is `SKIP` rather than a pinned hash, so no
> `updpkgsums` step is needed. It cannot be a real hash: the tarball GitHub
> generates for a tag *contains* that `PKGBUILD`, so writing a sum into it
> changes the archive the sum is pinning. `makepkg` therefore trusts the HTTPS
> fetch from this repository's own tag. For a build whose input is pinned and
> verifiable, use the release asset and check its Sigstore attestation with the
> `gh attestation verify` command above.

> **The AUR package is no longer updated.** `control-ofc-daemon` was published
> to the AUR through v2.13.0 and is frozen there. The AUR is a third-party
> service that goes read-only for maintenance without warning — the 2026-08-02
> freeze took the *entire* AUR down to two accepted pushes in a day — so
> releases now go to GitHub only. If you installed with
> `paru -S control-ofc-daemon`, the prebuilt-package command above upgrades it
> in place: it is the same `control-ofc-daemon` package name, so `pacman -U`
> simply replaces the AUR copy, and no AUR helper will try to pull you back to
> the older frozen version. This applies to *this* package only — the
> out-of-tree DKMS drivers in the prerequisites table above are separate
> third-party AUR packages and are installed from the AUR as before.

## Quick start

Building and installing straight from a checkout, without going through the
package at all:

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
| [`docs/DEVELOPER_HANDOVER.md`](docs/DEVELOPER_HANDOVER.md) | contributors | Developer onboarding (architecture overview: `daemon.md`) |
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
  snapshot reads (`/poll`) — the GUI's 1 Hz poll path (the unused `/events` SSE
  stream was removed at v2.5.0, DEC-198).
- **Thermal safety** is daemon-enforced: at the CPU trip point → all OpenFan and
  motherboard (hwmon) fans to 100%, hysteresis down to 80°C, 40% floor when no CPU
  sensor reports for 5 cycles. The trip point is **per-machine** — at least 105°C,
  raised to match the CPU's own reported design ceiling where the kernel publishes
  it (DEC-308) — and every duty is a **floor** over the active profile's output
  rather than a replacement for it (DEC-307), so the ladder can only raise a fan. GPU fans are excluded — AMD PMFW firmware owns
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
