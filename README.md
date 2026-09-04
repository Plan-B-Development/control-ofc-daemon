# control-ofc-daemon

**Latest release:** v2.35.3 — 2026-09-04. Pairs with `control-ofc-gui` ≥ v2.23.0 (the recommended capability floor; the package itself only hard-blocks GUIs < 2.0.0, the sole-writer cutover). v2.42.0 or newer drives the `POST /fans/openfan/rescan` route, v2.44.0 or newer surfaces the `skipped_controls[]` field as a "Not controlled" badge, v2.45.0 or newer consumes the `control_outputs[]` field and the `controls` health subsystem, v2.49.0 or newer removes a profile search directory via `control.profile_search_dir_remove`, v2.49.2 or newer shows the thermal-forcing verify refusal as a soft notice rather than an error, v2.49.5 or newer mirrors the widened `nct67xx` `CPUTIN` warning list this daemon acts on, and v2.49.6 or newer documents `emergency_threshold_c` as per-machine rather than a fixed 105 °C; older GUIs ignore or mis-word them but keep working. **None of 2.26.0, 2.27.0 or 2.28.0 requires a new GUI floor.** 2.26.0 made the thermal trip point per-machine (DEC-308) on a field every GUI already renders verbatim, and made the ladder's forced duties floors over profile output (DEC-307), with no wire change at all. 2.27.0 (DEC-309) stops reporting fan telemetry nothing measured — `rpm` is omitted rather than zero for an unpolled OpenFan channel, a GPU fan's `age_ms` is no longer reset by a command, and a hwmon fan that stops reading is evicted rather than published forever. `rpm` was already optional and a fan's absence already meant "not currently readable", so every released GUI handles all three unchanged. 2.28.0 (DEC-311) adds per-channel PWM header roles (`role`/`role_source` on the header and inventory responses), `POST /config/header-role`, and the `control.header_roles` capability; fan identify no longer stops a header the daemon knows to be a pump, perturbing its speed instead, and `POST /hwmon/{id}/verify` no longer drives one below the 30 % pump floor. Every field is additive and the identify request shape is unchanged, so an older GUI keeps working and — because the daemon, not the client, decides what a `stop` means — cannot ask for a pump stop even by accident; v2.50.0 or newer words the wizard's prompts accordingly. 2.28.1 is documentation only — the binary is identical to 2.28.0 — correcting the shipped `USER_GUIDE.md`, which stated a constant-speed pump as fact; GUI v2.51.0 retracted that rule and can assign the `pump` role from its Configure-AIO dialog. 2.29.0 adds PWM/RPM response characterisation — `POST /hwmon/{id}/characterize` plus the `GET`/`DELETE /diagnostics/characterization` pair, gated on the new `control.pwm_characterization` capability. It sits **alongside** the ~6 s verify rather than replacing it: it sweeps a header across a series of duties and reports command acceptance, PWM readback and physical RPM response as three independent verdicts, because a pump that overrides PWM during its startup or self-bleeding period reports a correct readback with its speed pinned high, and collapsing the three would call that a write failure. **0% is unreachable through the endpoint for any header**, a pump is never swept below its 30 % floor, points ascend so an abort leaves the header high, and the pre-sweep duty is restored on every exit path on which nothing else owns the header — the sweep runs daemon-side precisely so that restoration does not depend on the client surviving. Additive throughout; GUI v2.52.0 or newer drives it and older GUIs ignore the flag. 2.30.0 fixes what that sweep *said* about the two exits where it deliberately does not restore: a shutdown, a thermal force, or a pre-sweep duty it could never read all reported `restore_failed: false`, which the field's own contract defines as "the header is back where it was". It now reports the truth, with a new `restore_outcome` token saying which — and the reason is load-bearing rather than cosmetic, because under a thermal force the header is high on purpose and the one action a bare "restore failed" invites is the one a client must not take. Additive; GUI v2.53.1 or newer words each reason for the user, and an older GUI keeps the message it had. 2.31.0 (DEC-316) makes a cooler a first-class **cooling device**: `GET /inventory/cooling-devices` plus `POST`/`DELETE /config/cooling-device`, gated on the new `control.cooling_devices` capability, describe a pump header, its radiator fans and an advisory temperature source as one named assembly, persisted as a top-level `[[cooling_devices]]` array in `runtime.toml`. **Topology is metadata and the profile engine never reads it** — naming a header as a device's `pump_member` confers no pump protection, which is still `POST /config/header-role`. It also adds a trusted device-capability policy whose numbers are compiled into the binary and selected by id (the Rust type derives no `Deserialize`, so no payload can construct one, and the endpoint rejects `minimum_safe_pwm` and its siblings by name rather than ignoring them); headers now report `effective_min_pwm_pct` and `stop_permitted` so a client can display the enforced floor instead of re-deriving it; and `/poll` carries each hwmon header's `fan_alarm` and live `pwm_enable_mode`. **Only generic policies ship, so no floor moves** — the generic pump's floor is the 30 % constant the engine already enforced. Every field is additive and optional, so an older GUI keeps working; GUI v2.54.0 or newer drives the new surface. 2.32.0 (DEC-317) adds **validation sessions**: `POST`/`GET`/`DELETE /validation/session` plus its `stop`, `event` and `measurement` sub-routes and `GET /validation/sessions[/{id}]`, gated on the new `control.validation_sessions` capability. A session records an already-configured cooling device at 1 Hz — PWM command, PWM readback, RPM, temperature, ownership and thermal state — derives a timeline of the lifecycle events the daemon can genuinely observe, and finalises into a typed evidence summary whose result states keep *unavailable* and *not tested* distinct from *fail*, so hardware that simply does not expose a capability is never reported as broken. **The engine is an observer that may orchestrate, and never a second writer**: it performs no sysfs I/O and plants no hooks in the profile engine or the write path, and where a session is asked to run a diagnostic it invokes the existing PWM verify or characterisation, which already own the hwmon lease, the pump floor, the thermal refusal and restore-on-drop — so nothing here can lower a floor or stop a pump, and both parity oracles are byte-identical. Sessions persist under `{state_dir}/validation/` (last five); a session interrupted by a restart is recorded as `interrupted` at its last real sample rather than having the gap filled in. Also adds `pwm_readback_pct` on `/fans` and `/poll` — the hardware readback, distinct from the commanded duty — and both diagnostic handlers now refuse once the daemon is shutting down. Every field and route is additive, so an older GUI keeps working; GUI v2.55.0 or newer carries the typed models for it. 2.33.0 (DEC-318) adds one optional field, `FanEntry.pwm_commanded_pct` on `/fans` and `/poll` — the duty the daemon last **commanded** for a motherboard fan header, as distinct from `pwm_readback_pct`, the duty the hardware reports back. The two have to be separate numbers for a client to tell a failed write from a BIOS/EC reclaim from a device applying its own internal control, and until now they were not separable: the older `last_commanded_pwm` carries whichever of the poll's readback and the engine's command wrote last, so for an *uncontrolled* header it reports a readback despite its name. That field is deliberately unchanged — repairing it in place would alter what it reports for such a header. The new value is not new information: the daemon already tracked it and the validation recorder already read it; it simply was not on the wire. Published through the state cache rather than read on demand, so `/poll` never waits on the lock the engine holds across a sysfs write. **No routes, capabilities, floors, thresholds or safety rules change, and both parity oracles are byte-identical**; a client that ignores the field sees byte-identical behaviour. GUI v2.56.0 or newer displays it as the Hardware page's *Requested PWM*, beside *Readback PWM*. 2.33.1 (DEC-320) is a bug-fix release closing two P1 defects on the write paths the AIO-MB programme added; **no new routes, no new capability flag, no floor, threshold or safety-rule change, and both parity oracles are byte-identical**, so the GUI floor is unchanged and no client needs updating. `POST /config/cooling-device` validated member ids against hwmon PWM headers alone, so an OpenFan radiator fan — which the GUI's own radiator picker offers and its Fan Wizard posts verbatim — was rejected on any machine that had *any* hwmon header, i.e. every motherboard-AIO machine. Membership is now checked **per source**, deliberately not as a flat union: a union would have tightened the hwmon-absent case and rejected hwmon members accepted today. Separately, a validation session's sample cap bounded the row count while the file size scaled with the cooling device's member count (3.6 MiB at one member, 5.7 at two, 7.8 at three) against a 4 MiB read cap — so from two members up the session was written successfully and then invisible to every read path at once: it 500'd on fetch, vanished from the listing, could never be swept to `interrupted` and could never be pruned, leaking disk permanently. The effective cap is now derived per topology against a byte budget, with a compile-time assertion tying the write budget to the read cap, and the free-text fields of both write paths are bounded at ingest — which is also what makes "too large to read" mean "written by an older daemon", and therefore safe for prune to delete. **No behaviour change for any realistic cooler**: a pump plus up to four radiator fans still records the full 7200 samples. GUI v2.57.1 is the documentation counterpart and carries no code change. 2.34.0 (DEC-321) closes three defects that are one failure story: a user-assigned `pump` role could vanish from `runtime.toml` and nothing reported it — and on the boards this programme exists for (a Super-I/O publishing no `pwmN_label` files) that assignment is the only evidence a header drives a pump, so losing it removes that header's 30 % floor, its stop exemption and its pump-safe identify. `atomic_io::write_atomic` derived a **fixed** `{path}.tmp` scratch file and truncated it, so two concurrent writers could publish a hybrid document; the scratch name is now unique per call, which fixes all five call sites at once rather than leaving the hazard as a rule each caller must independently know. `POST`/`DELETE /config/*` are now serialised daemon-side, after two concurrent config writes could each overwrite the other's key while **both** answered `200 {"updated": true}`. And one additive `/status` + `/poll` field, `runtime_config_degraded`, reports that the daemon fell back to defaults because its own `runtime.toml` would not read or parse — deliberately **not** raised for a *missing* file, which is first boot, and sticky until a restart, so a successful `POST /config/*` does not clear it. No new routes, no new capability flag, no floor or threshold change, and both parity oracles are byte-identical; GUI v2.57.2 is the documentation counterpart. 2.35.0 (DEC-322) closes three more on the same theme — the daemon could drive or describe a pump wrongly. A diagnostic could restore a pump to a **stop**: both the verify restore and `RestoreOnDrop` wrote the captured pre-sweep duty straight into `set_pwm`, which applies no floor of its own, so a pump-protected header whose duty read 0 was swept correctly and then restored to 0 with `pwm_enable=1` asserted by the write — until the engine's next tick if it is a controlled member, and indefinitely if no profile is active. Both restores now clamp to the 30 % pump floor for pump-protected headers **only**; an ordinary fan is still put back exactly where it was found, 0 included. Separately, `stop_permitted` was published from the cooling *device's* policy rather than from the predicate the daemon actually obeys, so every member of an AIO inherited the pump's `supports_stop: false` — a radiator fan was advertised unstoppable while identify stopped it and, in the dangerous direction, a header named as a `pump_member` *without* a pump role was promised protection and then driven to 0. The published value is now exactly `!header_is_pump_protected`, which is what `docs/08_API_Integration_Contract.md` has always said it was. **`stop_permitted` therefore changes value for one population** — radiator and auxiliary members of a cooling device that are not themselves pump-protected, which now correctly report `true`. No client needs updating (the GUI already reads it as `not stop_permitted` meaning "pump protected"), but there is no capability flag separating the old behaviour from the new, so a client that must distinguish them should branch on the daemon version. Also adds a third cross-repo parity oracle, `header_role_classification.json`, gating the GUI's hand-mirrored copy of this daemon's `classify_header_role`. No new routes and no new capability flag; GUI v2.57.3 is the documentation counterpart. 2.35.1 (DEC-323) closes three more `/ofc:audit` P2s that are one story: a validation session did not own the things it started. Ending a session left a characterisation **it had started** running — still sweeping the header and still holding the profile engine’s write-pause, for up to 20 × 15 s after the user stopped the session; the orchestrator now cancels that sweep, **fenced on the `run_id` it was handed at 202**, so a run begun outside the session is never aborted. Thermal safety never depended on it and that was checked rather than assumed — the forced-duty branch runs above the `verify_active` gate, so a paused engine still floors every output; what was lost was control intent, not cooling. `POST /validation/session` no longer holds the session slot across an unbounded blocking acquisition of the hwmon controller lock, and the four `store::save` sites that ran inline on the runtime the 1 Hz engine shares now go through the blocking pool. Two review findings ship with it: a finaliser that broke answered `404 not_found` while the session was **still installed and still recording**, and now answers `500 internal_error` — `404` on those two routes keeps its narrower meaning of "no session has ever been started"; and the shutdown flush bypassed the write-ordering lock, so a `stop` in flight at shutdown could be resurrected as `recording` and then marked `interrupted` on the next boot. **No new routes, no new capability flag, no floor, threshold or safety-rule change, and both parity oracles are byte-identical.** One client-visible behaviour changes with no capability flag to separate it: `GET /diagnostics/characterization` reports a session-orchestrated run as `cancelled` rather than `complete`, and the cancel is cooperative, so it stays `running` for up to one settle after the session has returned its summary — branch on the daemon version if a client must distinguish the old behaviour. GUI v2.57.4 is the documentation counterpart and carries no code change. The GUI floor is unchanged at v2.23.0. 2.35.2 (DEC-324) is **tests only and the binary behaves identically**: it gives the characterisation `run_id` fence a regression test that can actually fail. The test named for that fence awaited a terminal state before starting the second run, so the two runs never coexisted and both fences could be deleted with it green; the new one supersedes a live run through the DEC-296 expired-deadman steal and asserts across *both* sweeps, because the damage repairs itself — run B's own terminal write restores the state and points an end-state snapshot would check. It adds one `#[doc(hidden)]` test seam, `StateCache::expire_verify_claim_for_test()`, plus a source scan asserting nothing under `daemon/src/` calls it. No routes, capabilities, floors, thresholds or safety rules change and both parity oracles are byte-identical; GUI v2.57.5 ships alongside it and the GUI floor is unchanged at v2.23.0. 2.35.3 (DEC-326 and DEC-327) is a bug-fix release with **no wire-format, route, capability, floor, threshold or ladder change**, so the GUI floor is unchanged at v2.23.0 and no client needs updating. It stops the daemon calling a successful 100% write a BIOS reclaim: the `it87` driver reports the PWM mode it *computes* rather than the one it was set to, answering `pwm_enable = 0` ("full speed") at full duty, which every `enable != 1` check read as firmware taking the header. Four things were wrong as a result and all four are fixed — the write path logged a reclaim every tick at 100% and inflated the revert count on `/diagnostics/hardware`; `POST /fans/characterize` **aborted its own sweep at the 100% point** and reported `interference_detected: true`, a false positive in the one diagnostic built to answer "does this header accept writes?"; `POST /hwmon/{id}/verify` returned `pwm_enable_reverted` for a pump header, reachable for any pump idling at 60–65% because the verify duty is exactly 100 there; and a validation session recorded `control_reclaimed` and published the `bios_ec_control_reclaim` finding whenever a member curve reached 100% under load. The discriminator carries **no chip table** — `pwm_enable == 0` is ignored only while the duty register still holds the 100% the daemon itself commanded, so a reclaim to *automatic* (mode 2) is unaffected and detection resumes on the first observation below 100%. **Cooling behaviour is unchanged in every case**: the only suppressed state is one in which the fan is already at maximum. Wider than it looks — the kernel condition is `(!has_fanctl_onoff || nr >= 3) && duty == 0xff`, so `pwm4` and above are affected on *every* ITE chip. Three further defects found while fixing it also ship: a validation session could report `control_restored` for a restore that never happened (and because the `control_restoration` finding counts reclaims and restores across the whole session rather than per member, one member's ordinary ride to 100% could have paid for a *different* member's genuine, never-restored reclaim — turning a FAIL into a PASS); the recorder read a cached "last commanded" value that is never cleared when the daemon stops driving a header, so a stale command could mask a real reclaim, and it now reads the controller's own value; and `POST /hwmon/{id}/verify` no longer passes a header the firmware already pins at full speed. Separately (DEC-327), the unbound-chip hint no longer tells you to install a driver build you are already running — an out-of-tree `it87` is detected from the module's taint flag (`/sys/module/<name>/taint`), because the daemon's own sandbox makes `/lib/modules` unreadable (`ProtectKernelModules=true` mounts an inaccessible directory over it) — and it no longer recommends `mmio=on`, which is already the driver default. The "Super-I/O driver not loaded" readiness card no longer says the driver is not loaded when it is; its `code` is unchanged, so nothing in a GUI moves. The dual-ITE row now reports the outcome **per board rather than per family** — X870E AORUS ELITE is owner-confirmed working, while on X870E AORUS MASTER the secondary is unreachable with no local fix. GUI v2.57.6 ships alongside it as the counterpart correction to the same guidance. See [`CHANGELOG.md`](CHANGELOG.md) for the full history.

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
| Current `it87-dkms-git` build (2026-03+; older builds need `/etc/modprobe.d/it87.conf` with `options it87 mmio=on`) | Dual-IT-chip Gigabyte boards (DEC-101/DEC-144). **Outcome varies by board, not by family:** X870E AORUS ELITE is owner-confirmed with both chips controllable (it87 #89), while on **X870E AORUS MASTER** the secondary answers device-ID `0x8883` and is unreachable with no local fix (measured 2026-09-04, DEC-326 — `mmio` is already the driver default, so it is not a setting you can change) | User action; the GUI surfaces the exact remediation when the dual-chip case is detected. (Counter-cases: IT8665E boards need `mmio=off` on current builds — it87 #106; and the `0x8883` case above has no remediation to surface) |

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
