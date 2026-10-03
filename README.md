# control-ofc-daemon

**Latest release:** v3.3.1 — 2026-10-03. Pairs with `control-ofc-gui` ≥ v2.23.0 (the recommended capability floor; the package itself only hard-blocks GUIs < 2.0.0, the sole-writer cutover). [CHANGELOG.md](CHANGELOG.md) records which version introduced each capability.

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
├── daemon/                   # control-ofc-daemon crate (the service binary)
│   ├── src/                  # daemon source (see daemon.md for module map)
│   └── README.md             # build, install, CLI, env vars, API quick-start
├── tray/                     # control-ofc-tray crate — KDE system-tray CLIENT
│                             #   (DEC-352). Ships in the same package; talks to
│                             #   the daemon only over the API. The daemon has no
│                             #   dependency on it.
├── man/                      # scdoc sources for both man pages
├── packaging/                # systemd unit, udev rules, shutdown restore script,
│                             #   tray autostart entry + panel icon
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
| Super I/O kernel module loaded — `nct6775`, `it87`, `w83627ehf`, `drivetemp` | Motherboard fan / sensor control | The package ships `/etc/modules-load.d/control-ofc.conf`. Loaded at next boot, or immediately via `sudo systemctl restart systemd-modules-load` (`start` does nothing once the unit has run). To stop loading one, comment out its line: pacman restores a renamed or deleted copy at the next upgrade |
| Out-of-tree DKMS driver — `it87-dkms-git` (Gigabyte ITE boards; also ASUS AM4 300/400-series boards, which carry an ITE IT8665E), `nct6687d-dkms-git` (MSI NCT6687D boards; some ASRock NCT6686D boards) | Most newer (2022+) Gigabyte / MSI boards and many ASRock boards — fan control is read-only or absent without these | Install the matching AUR package. There is **no** AUR package for the ASRock-specific `asrock-nct6683` or `nct6686d` sources — build those from their repositories if your board needs them. The GUI's Hardware page readiness report identifies the chip and recommends the exact package |
| `dkms` + `linux-headers` matching the running kernel | Building any of the DKMS drivers above | Pulled in transitively via the DKMS packages, but `linux-headers` must match the kernel you actually boot |
| UEFI Secure Boot disabled, or DKMS modules signed | Loading any `*-dkms-git` driver with Secure Boot enabled | Unsigned out-of-tree modules build but fail to load (`Key was rejected by service`). Detection and options (disable vs sign, CachyOS caveat): [GUI manual — Driver Setup § Secure Boot][gui-secure-boot] |
| A sane BIOS fan curve | Every board | A current driver takes each header over from the BIOS curve, so usually nothing needs changing. The BIOS curve still runs the fans at boot and whenever the daemon is not controlling them, so **never give it a 0% point**; a header's "Full Speed" setting is a fail-safe (100% whenever the firmware owns the fan), not a fix — on some boards it also locks Linux out of that header. See the [vendor-by-vendor BIOS guide][vendor-bios] |
| `amdgpu.ppfeaturemask=0xffffffff` on the kernel command line | RDNA3+ (RX 7000 / RX 9000) GPU fan-curve writes | Add to your bootloader; see `man control-ofc-daemon` for per-bootloader instructions. Pre-RDNA3 cards do not require this |
| `options it87 ignore_resource_conflict=1` (ITE), or `acpi_enforce_resources=lax` (Nuvoton drivers have no driver-local option) | Some Gigabyte / ASUS / ASRock boards with ACPI OpRegion conflicts | Prefer the it87 driver-local option — the system-wide parameter can cause boot failures on some systems. The daemon's `/diagnostics/hardware` endpoint and the GUI's Hardware page detect the conflict and surface the remediation. The it87 option goes in `/etc/modprobe.d/it87.conf` and takes effect when the module next loads (reboot, or reload `it87` and restart the daemon) |
| Current `it87-dkms-git` build (2026-03+; older builds need `/etc/modprobe.d/it87.conf` with `options it87 mmio=on`) | Dual-IT-chip Gigabyte boards (DEC-101/DEC-144/DEC-421). **Outcome varies by board, not by family:** X870E AORUS ELITE X3D is owner-confirmed with both chips controllable (it87 #89), while on **X870E AORUS MASTER** the secondary can be masked by an ITE eSPI→LPC bridge latched into configuration mode (it answers device-ID `0x8883`, visible only with it87 dynamic debug on). **That is recoverable** (measured 2026-09-05, DEC-332): it is caused by `nct6775`/`w83627ehf` (or `sensors-detect`) writing a config-mode unlock to 0x2E/0x4E, so the package ships a modprobe guard that keeps those modules off every Gigabyte board (all use ITE Super-I/O; DEC-424), and off a listed board whose firmware reports no vendor. Each suppression is logged (`journalctl -b -t control-ofc-superio-guard`); the only way to turn the guard off is an **empty** `/etc/modprobe.d/control-ofc-superio.conf` — a file of any other name does not reliably win. Stop the trigger and reboot; if the secondary is still missing, power down **at the wall** — the bridge runs on standby power. `mmio` is already the driver default, so it is never the fix. **Builds from 2026-09-09 (it87 v2.0) rename Gigabyte chips** (e.g. `it8696_a008090a`); from 3.0.0 the daemon strips that board suffix, so no fan header's id changes (DEC-442). On an older daemon the rename changes every fan header's id — update the daemon first, or build commit `c567739` (2026-08-25: the same driver code as the last build before the rename, including the IT8689E fixes of PR #128) | User action; the GUI surfaces the remediation when a missing chip is detected, and links the full step-by-step recovery. (IT8665E boards needed `mmio=off` only on builds older than 2026-07-22 — it87 PR #120 fixed that; no parameter is needed now) |

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

Once the package is installed and the service enabled (see Install above), check
that the daemon is answering:

```bash
curl --unix-socket /run/control-ofc/control-ofc.sock http://localhost/status
```

Install the package, not a hand-copied binary. The unit runs
`/usr/bin/control-ofc-daemon` and, after every stop, `/usr/bin/control-ofc-restore-auto`,
which gives each fan header back to what it was doing before the daemon took it;
the package also installs the sleep hook, the Super-I/O guard and
`/etc/modules-load.d/control-ofc.conf`. A binary copied to `/usr/local/bin` gets
none of these, so the unit cannot start it or cannot hand the fans back. To run a
build of your own checkout, see
[`docs/DEVELOPER_HANDOVER.md`](https://github.com/Plan-B-Development/control-ofc-daemon/blob/main/docs/DEVELOPER_HANDOVER.md) § Running the daemon.

CLI / environment reference: [`daemon/README.md`](https://github.com/Plan-B-Development/control-ofc-daemon/blob/main/daemon/README.md).

## Documentation index

| Document | Audience | Purpose |
|---|---|---|
| [`daemon.md`](daemon.md) | all | Architecture overview, module map, data flow, safety model, full API endpoint table |
| [`daemon/README.md`](https://github.com/Plan-B-Development/control-ofc-daemon/blob/main/daemon/README.md) | operators | Build, install, CLI flags, env vars, config |
| [`docs/USER_GUIDE.md`](https://github.com/Plan-B-Development/control-ofc-daemon/blob/main/docs/USER_GUIDE.md) | end users | Configuration, profiles, upgrade notes |
| [`docs/DEVELOPER_HANDOVER.md`](https://github.com/Plan-B-Development/control-ofc-daemon/blob/main/docs/DEVELOPER_HANDOVER.md) | contributors | Developer onboarding (architecture overview: `daemon.md`) |
| [`docs/ADRs/`](https://github.com/Plan-B-Development/control-ofc-daemon/tree/main/docs/ADRs) | contributors | Architecture decision records |
| [`CHANGELOG.md`](CHANGELOG.md) | all | Release history |
| [GUI manual — OpenFan Controller][gui-openfan] | end users | What the OpenFan Controller is and how Control-OFC drives it through the daemon (detection, serial access, stable paths, troubleshooting) |
| [GUI manual — Understanding Motherboard Fan Control][gui-understanding-fans] | end users | Plain-English primer on hwmon, Super I/O, and PWM for new users |

[gui-openfan]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/openfan-controller.md
[gui-understanding-fans]: https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/understanding-fan-control.md

## Architecture summary

- **Three fan backends**: OpenFanController (serial/USB), motherboard hwmon (sysfs
  PWM), and AMD GPU (RDNA3+ PMFW fan curves; a pre-RDNA3 card's legacy hwmon PWM
  is used by the GPU fan verify and reset only).
- **HTTP over Unix domain socket** at `/run/control-ofc/control-ofc.sock`, exposing
  snapshot reads (`/poll`) — the GUI's 1 Hz poll path (the unused `/events` SSE
  stream was removed at v2.5.1, DEC-198).
- **Thermal safety** is daemon-enforced: at the CPU trip point → every OpenFan channel and
  writable motherboard (hwmon) header the machine has to 100%, hysteresis down to 80°C, and a 40% floor on the
  fans the active profile controls when no CPU sensor reports for 5 cycles (fans no profile controls stay under
  their firmware curve — except on an ARCTIC Fan Controller, which has none: since 2.56.1, once the daemon
  writes any of its channels, each channel reading 0 that it did not choose runs at 100%). The trip point is **per-machine** — at least 105°C,
  raised to `min(ceiling + 5 °C, 115 °C)` where the kernel publishes the CPU's own
  design ceiling (DEC-308) — and every duty is a **floor** over the active profile's output
  rather than a replacement for it (DEC-307), so the ladder can only raise a fan. GPU fans are excluded — AMD PMFW firmware owns
  GPU thermal protection independently of OS fan control (DEC-130). A **coolant sensor** at or above the coolant
  limit (default 60 °C, `[safety] coolant_limit_c`) takes the same 100 % force; a profile pump that stalls is driven
  to full speed; a pump on a DC-mode header is never driven below 70 %; and a CPU held at its ceiling with every fan
  slow raises an advisory (DEC-443).
- **Headless profile engine** (`profile_engine/`) evaluates the active profile's
  fan curves autonomously on a 1 Hz loop and is the **sole writer** of every
  backend (2.0.0+, DEC-159/DEC-165). There is no GUI defer window — the 30 s
  `gui_active` defer (DEC-071/074) was deleted at the 2.0.0 cutover; the GUI never
  writes PWM.
- **Lease system** — a daemon-internal token (60 s TTL) that decides which of the
  daemon's own three hwmon writers may write at a time: the profile engine, a
  hardware diagnostic, or the thermal-safety force, which can take it from a
  diagnostic mid-run (DEC-197). It does not stop another program writing the
  same fan, and nothing can: the engine rewrites a duty that another writer
  moved, but gives up after three corrections that do not hold and flags the
  header `duty_not_holding` (DEC-406). Stop other fan tools first (see
  Prerequisites). The GUI holds no lease (DEC-165).
- **Systemd-hardened** (`ProtectHome=read-only`, `ProtectSystem=strict`,
  `SystemCallFilter=@system-service`, etc.); on stop, every motherboard fan
  header the daemon took and that has a mode switch (`pwmN_enable`) goes back to
  exactly what it was doing before (its BIOS mode, or its duty if it was already
  manual), and each GPU fan curve the daemon drove to automatic, leaving alone a
  card another tool manages — in-process, and again via `ExecStopPost`, which
  replays the daemon's records. Fans with no firmware behaviour to go back to —
  every OpenFan channel, and a header with no mode switch — are left on a clean
  stop at their last speed or the **exit floor** (default 50 %,
  `[shutdown] exit_floor_pct`), whichever is higher (DEC-388); a crash or SIGKILL
  cannot apply it, so those keep their last speed.

## Pairing with the GUI

The GUI repo lives at `control-ofc-gui` (separate repository).
GUI ↔ daemon is a strict client/server boundary: the GUI is **never** permitted to
touch hardware directly. All reads and writes flow through this daemon's HTTP API.
The full contract is documented in the GUI repo's `docs/08_API_Integration_Contract.md`.

## License

MIT — see [`LICENSE`](https://github.com/Plan-B-Development/control-ofc-daemon/blob/main/LICENSE).
