# User Guide

## What is Control-OFC?

Control-OFC is a fan control daemon for Linux desktops. It communicates with:
- **OpenFanController** — a USB fan controller (up to 10 channels)
- **Motherboard fan headers** — via the Linux hwmon sysfs interface (ITE, NCT Super I/O chips)

The daemon is the **autonomous, sole controller** of your fans. When a profile is active its built-in profile engine evaluates the fan curves at 1 Hz and writes every backend (OpenFan, hwmon, AMD GPU) itself — it keeps fans controlled through GUI close, crash, or sleep. The GUI is an editor/viewer/controller-of-intent; it never writes PWM and is poll-only (DEC-159 / DEC-165). With no profile active the daemon evaluates no curves — fans stay wherever their hardware left them, and only the thermal-safety override acts on its own.

The daemon provides a local API that a GUI (or scripts) can use to monitor temperatures, read fan RPM, express control intent (activate a profile, take an expiring manual override, identify a fan), and run diagnostics. Direct PWM writes are not part of the surface — control flows through the profile engine.

> **New to Control-OFC?** The GUI manual has friendly, step-by-step guides aimed at first-time users — [OpenFan Controller](https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/openfan-controller.md), [Understanding Motherboard Fan Control](https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/understanding-fan-control.md), and the ordered [Setup Checklist](https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/setup-checklist.md).

## Supported hardware

| Device | Read | Write |
|---|---|---|
| CPU temperature (k10temp, coretemp) | Yes | N/A |
| AMD GPU temperature (amdgpu) | Yes | N/A |
| Intel Arc discrete GPU temperature (`xe` / `i915`) | Yes | N/A |
| NVIDIA discrete GPU temperature (`nouveau` / opt-in NVML) | Yes | N/A |
| Disk temperature (NVMe) | Yes | N/A |
| Motherboard temperature (ITE, NCT) | Yes | N/A |
| OpenFanController fans (RPM) | Yes | Yes (daemon-driven) |
| Motherboard fan headers (hwmon) | Yes | Yes (daemon-driven; daemon holds the lease internally) |
| AMD GPU fans (RDNA3+, PMFW) | Yes | Yes (daemon-driven via PMFW fan curve) |
| AMD GPU fans (pre-RDNA3) | Yes | Verify and reset only — no profile drives them; the card's firmware curve stays in charge (DEC-445) |
| Intel Arc discrete GPU fans (`xe` / `i915`) | Yes (RPM) | No — firmware-managed, no kernel PWM interface (DEC-121) |
| NVIDIA discrete GPU fans (`nouveau` / opt-in NVML) | Yes (RPM; firmware-measured duty via NVML) | No — read-only, no kernel/PMFW write path (DEC-204) |
| ARCTIC Fan Controller (`arctic_fan`, Linux 7.2+) | Yes | Yes (daemon-driven). Every write carries all ten channels, so since 2.56.1 the daemon sets each channel reading 0 that it did not choose to 100 % first; a channel your profile does not control therefore runs at full speed. Control all ten to keep them quiet |
| AIO coolers (hwmon-attached) | Yes — coolant temp (`CoolantTemp` kind) + pump RPM (DEC-156) | Yes — hwmon pump PWM via the guided Configure-AIO flow; fixed speed or a temperature curve, always floored at 30% (DEC-157, DEC-312). A motherboard-connected pump is configured the same way once its header carries the `pump` role (GUI ≥ v2.51.0 assigns it; `POST /config/header-role` otherwise) |

## Installation

Most users should install the package rather than build from source: add the
signed `[control-ofc]` pacman repository once and the daemon then upgrades with
a normal `sudo pacman -Syu`. The setup commands, the one-off `pacman -U` path
using the package attached to every release, and the Sigstore verification step
are all in the [Install section of the README](https://github.com/Plan-B-Development/control-ofc-daemon/blob/main/README.md#install).

To build it yourself instead, build the package from the in-repo `PKGBUILD`:

```bash
git clone https://github.com/Plan-B-Development/control-ofc-daemon.git
cd control-ofc-daemon/packaging
makepkg -si
sudo systemctl enable --now control-ofc-daemon
```

`makepkg` builds the tagged release, not the checkout you cloned. **Do not install by copying the binary and unit file in by hand.** The unit runs `/usr/bin/control-ofc-daemon` and, after every stop, `/usr/bin/control-ofc-restore-auto`, which gives each fan header back to what it was doing before the daemon took it; the package also installs the sleep hook, the Super-I/O guard and `/etc/modules-load.d/control-ofc.conf`. A hand-copied binary gets none of these, so the unit cannot start it or cannot hand the fans back. To run a build of your own checkout, see `docs/DEVELOPER_HANDOVER.md` § Running the daemon.

## Hardware sensor modules

The daemon discovers sensors and fan headers by scanning `/sys/class/hwmon/`. For devices to appear there, the correct kernel modules must be loaded.

**Automatically handled:** The package installs `/etc/modules-load.d/control-ofc.conf`, which loads common Super I/O chipset modules at boot:

| Module | Chipset | Common boards |
|--------|---------|---------------|
| `nct6775` | Nuvoton NCT6775–NCT6799 (incl. NCT5585D and the ASUS NCT6701D, which it reports as `nct6799`) | ASUS (AM4 500-series onward, Intel), ASRock, older MSI (AM4 300/400, the original X570 boards, Intel ≤ 300-series) |
| `it87` | ITE IT87xx/IT86xx — the in-kernel driver covers only older parts plus IT8689E (7.1+) and IT87952E (6.3+); IT8686E/IT8688E/IT8696E/IT8665E need `it87-dkms-git` | Gigabyte; ASUS AM4 300/400-series (IT8665E) |
| `w83627ehf` | Winbond W83627EHF/DHG | Older boards |
| `drivetemp` | SATA/SAS drive temperature | All SATA drives |

CPU temperature modules (`coretemp` for Intel, `k10temp` for AMD) and SMBus adapter modules (`i2c-i801`, `i2c-piix4`) auto-load via PCI/ACPI matching — no configuration needed.

**The Super-I/O guard.** On a Gigabyte board the package stops `nct6775` and `w83627ehf` from loading, even though the file above lists them. Gigabyte boards use ITE Super-I/O chips, which those Nuvoton/Winbond drivers can never bind, and their probe writes a config-mode unlock that can latch the ITE bridge in front of a board's second fan chip, hiding it until a reboot, and on some boards until the machine is powered down at the wall. Since 2.56.1 the guard covers every Gigabyte board, and a board it lists whose firmware reports no vendor; before that, only the boards it listed. It is `/usr/lib/modprobe.d/control-ofc-superio.conf`, with its helper `/usr/lib/control-ofc/control-ofc-superio-guard`; on any other board it loads the module unchanged. Each time it declines a module it says so in the journal: `journalctl -b -t control-ofc-superio-guard`. To turn it off, create an **empty** `/etc/modprobe.d/control-ofc-superio.conf` — the same name masks it. A file of any other name does not reliably win: modprobe reads every directory's files in one name order, and for a module named twice the first `install` line read takes effect. The guard prevents the latch; it does not clear one already set. For that, see the GUI manual's [Hardware Troubleshooting](https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/hardware-troubleshooting.md) page.

**If your hardware is not detected:** check the GUI first — the **Hardware** page's readiness checklist and Super-I/O section identify your board's chips and the exact module or AUR package needed **without probing the hardware**. As a **last resort**, install `lm_sensors` and run:
```bash
sudo sensors-detect
```
This interactively probes for additional sensor chips and persists the results. Probing is at your own risk: it "can access chips in a way these chips do not like, causing problems ranging from SMBus lockup to permanent hardware damage (a rare case, thankfully)" — [sensors-detect(8)](https://man.archlinux.org/man/extra/lm_sensors/sensors-detect.8.en). Accept the conservative defaults, and never run it on a Gigabyte board: it writes a Super-I/O unlock to 0x4E that can wedge the ITE bridge in front of a second chip, which then vanishes — sometimes until the machine is powered down at the wall, because the bridge runs on standby power and survives a reboot. The package's guard cannot stop it: `sensors-detect` probes the ports itself rather than loading a module. Then restart the daemon:
```bash
sudo systemctl restart control-ofc-daemon
```

**ACPI conflicts:** If a Super I/O module fails to bind with `ACPI resource conflict` / "Device or resource busy" in the kernel log, update the driver first (current `it87-dkms-git` builds sidestep most conflicts through MMIO) — on a Gigabyte board, read the it87 v2.0 note under **Out-of-tree modules** below before you rebuild. For `it87`, prefer the driver-local `options it87 ignore_resource_conflict=1` in `/etc/modprobe.d/it87.conf` — the it87 documentation notes that the system-wide `acpi_enforce_resources=lax` can cause boot failures on some systems. The Nuvoton drivers (`nct6775`, `nct6687`) have no driver-local option, so there `acpi_enforce_resources=lax` on the kernel command line is the only kernel-side remedy.

**Out-of-tree modules:** Some newer motherboard chipsets require DKMS modules not yet in mainline (e.g. `it87` for newer ITE chips, `nct6687` for MSI NCT6687D boards and some ASRock NCT6686D boards). These are available from the AUR and must be installed separately. Two notes from the 2026-09 review. Builds of `it87-dkms-git` from 2026-09-09 (it87 v2.0) rename Gigabyte chips (e.g. `it8696_a008090a`); from 3.0.0 the daemon strips that board suffix where it reads the chip name, so no fan header, sensor or voltage id changes and pump roles, fan names and profile members keep working — an older daemon changes every id, so update the daemon before you rebuild the driver (DEC-442). And never load `nct6687` with `force=1` on a board whose chip is an NCT679x.

### What the daemon detects — and what it deliberately doesn't

The daemon exposes a structured, **read-only** view of your cooling hardware for the GUI (`GET /inventory/hwmon` and `GET /inventory/hardware-readiness`):

- **Detected automatically (read-only):** CPU and motherboard temperature sensors — each classified (e.g. *CPU Tctl*, *VRM*, *chipset*) with a confidence and a plain-English reason — a recommended default CPU sensor, every controllable PWM fan header, and **monitor-only fan tachometers** (fans whose RPM can be read but not controlled). The daemon also builds a readiness checklist that explains what works, what is missing, what is read-only, and what to do about it.
- **Deliberately NOT automated:** the daemon never runs `sensors-detect`, never loads kernel modules, never edits your bootloader/initramfs/udev, and **never writes to a fan during discovery**. Anything that could change system behaviour is left to you (with guidance), so discovery is safe to run at any time.
- **"Control unverified":** a writable PWM header only *appears* controllable until a fan-control verification confirms a write actually changes fan speed. Until then, the readiness list marks control as unverified.
- **Read-only fans:** some PWM channels are exposed read-only by the kernel driver; those fans can be monitored but not controlled, and the readiness list says so rather than pretending otherwise.
- **Reboot may be required:** loading a missing Super I/O or DKMS driver to gain fan control usually needs a reboot or module reload — the relevant readiness item flags this.
- **GPU is out of scope here:** GPU fan discovery and control are owned by the GPU subsystem, not this hwmon path (DEC-102 / DEC-130).

## Configuration

Configuration is optional. The package installs `/etc/control-ofc/daemon.toml` with every key commented out, so the defaults apply until you uncomment one, and pacman keeps your edits on upgrade. With no file at all the daemon also runs on defaults.

The file's location can be changed with `--config <path>` or `CONTROL_OFC_CONFIG` (the flag wins). Under systemd, set either in a drop-in (`sudo systemctl edit control-ofc-daemon`), as § Loading a profile shows for `OPENFAN_PROFILE`.

**Read this before editing.** Inside a section the file is strict: **an unknown or misspelt key, a wrongly typed value or an out-of-range value makes the daemon exit at startup**, and systemd restarts it on a back-off (3 s, then up to one start a minute) until the file is fixed. For all that time nothing controls the fans and the thermal emergency cannot run. `journalctl -u control-ofc-daemon` names the offending key. An unknown top-level section or key is different: the daemon logs a warning naming it and ignores it, so a section a newer release added does not stop an older one starting. That also means **a misspelt section name — `[saftey]` — is ignored and that section's defaults apply**, as is a key written above its section header; check the journal for `ignoring unknown top-level section` after an edit. Daemons up to 3.6.0 rejected an unknown section too; see § Upgrade notes, "Downgrading".

The keys, with their defaults:

```toml
[serial]
# port = "/dev/serial/by-id/usb-..."   # no default: auto-detect when unset
# timeout_ms = 500                      # 50 or more

[polling]
# poll_interval_ms = 1000   # 100 or more; slower than 6000 is clamped to 6000 (DEC-270)

[ipc]
# socket_path = "/run/control-ofc/control-ofc.sock"

[state]
# state_dir = "/var/lib/control-ofc"

[startup]
# delay_secs = 0            # 0-30
# record_startup = false    # record a short validation session at every start

[profiles]
# search_dirs = ["/etc/control-ofc/profiles", "/root/.config/control-ofc/profiles"]
#   The default shown is what the service gets (it sets HOME=/root); see
#   § Profile search directories. The daemon's own store is always searched first.

[detection]
# allow_port_probe = false          # also needs a systemd drop-in
# enable_nvidia_telemetry = false   # also needs a systemd drop-in

[shutdown]
# exit_floor_pct = 50       # 0-100; the exit minimum for fans with no firmware fallback (0 = off)

[safety]
# coolant_limit_c = 60      # 40-70 whole C; a coolant sensor at or above it forces every fan to 100 %
```

After an edit, `sudo systemctl reload control-ofc-daemon` applies the profile search directories, the exit minimum and the coolant limit at once; every other key needs `sudo systemctl restart control-ofc-daemon`. A reload that finds the file invalid logs the error and keeps the running values.

**Moving `state_dir` or `socket_path` needs a drop-in as well.** The service runs with `ProtectSystem=strict`, so outside its private `/tmp` it can write files only under `/run/control-ofc`, `/var/lib/control-ofc` and `/sys/devices`. Add the new directory with `ReadWritePaths=` in `sudo systemctl edit control-ofc-daemon`, or the daemon cannot create its socket or save its state there.

## The system tray

The package installs a second program, `control-ofc-tray`, alongside the daemon.
It puts a Control-OFC icon in the KDE Plasma system tray and starts
automatically when you log in.

- **Left click** opens the GUI.
- **Right click** shows the running daemon's version, the list of profiles with
  the active one marked, an action to stop profile control, and an action to
  open the GUI.
- If the daemon reports a thermal event, the menu says so. Note that switching
  to a quieter profile during one will not quieten the fans: the forced duties
  are floors *over* whatever the profile asks for.

It is purely a convenience. It never controls fans itself — it asks the daemon,
exactly as the GUI does — and fan control does not depend on it in any way. If
you close it, or it never starts, nothing about cooling changes.

It only starts if `control-ofc-gui` is installed, so a headless or
server install never runs it.

To turn it off permanently: **System Settings → Autostart**.

To start, stop or read its logs by hand:

```bash
systemctl --user status  'app-control\x2dofc\x2dtray@autostart.service'
systemctl --user restart 'app-control\x2dofc\x2dtray@autostart.service'
journalctl --user -u     'app-control\x2dofc\x2dtray@autostart.service'
```

Full detail: `man control-ofc-tray`.

## Checking daemon status

```bash
# Service status
sudo systemctl status control-ofc-daemon

# Logs
journalctl -u control-ofc-daemon -f

# Query the API (requires curl + jq)
curl --unix-socket /run/control-ofc/control-ofc.sock http://localhost/status | jq .
curl --unix-socket /run/control-ofc/control-ofc.sock http://localhost/capabilities | jq .
curl --unix-socket /run/control-ofc/control-ofc.sock http://localhost/sensors | jq .
curl --unix-socket /run/control-ofc/control-ofc.sock http://localhost/fans | jq .
```

## Service supervision

The package's systemd unit supervises the daemon so that fan control does not quietly stop.

- **Start.** The unit is `Type=notify`: `systemctl start` returns only once the profile engine is ticking and the API is answering.
- **Watchdog.** If the control loop stops completing its once-a-second tick for 15 s (`WatchdogSec=15`) — a deadlock, say — systemd sends `SIGTERM`, so the normal stop runs (exit minimum, then every header and GPU fan given back); it sends `SIGKILL` if that has not finished within 10 s (`TimeoutAbortSec=10`), runs `control-ofc-restore-auto`, and restarts the daemon. The journal shows `Watchdog timeout`. A slow or wedged *device* does not trip it: the loop keeps ticking past a write that has not returned.
- **Restarts.** A crash, a non-zero exit or a watchdog timeout restarts the daemon after 3 s, then about 5.5, 10, 18 and 33 s, then once a minute for as long as it keeps failing. There is no start limit, so it never gives up and never needs `systemctl reset-failed`. After five minutes of normal running the daemon resets the back-off, so a later, unrelated fault restarts fast again (systemd 258 or newer). **The back-off needs systemd 254 or newer.** Older releases — Debian 12 ships 252 — ignore `RestartSteps=` and `RestartMaxDelaySec=` with a warning, and restart every 3 s.
- **Stop.** A stop is allowed 40 s (`TimeoutStopSec=40`); the daemon bounds each hardware step itself so that it can always exit. After every stop, whatever ended it, `ExecStopPost` runs `control-ofc-restore-auto`, which gives back each header and GPU fan the daemon's records name (§ Safety).
- **Sleep.** The package installs a system-sleep hook, `/usr/lib/systemd/system-sleep/control-ofc-daemon`, because the watchdog keeps counting while user space is frozen for a suspend and resume. Before a sleep it sends the daemon `SIGUSR1`, which widens the watchdog to 120 s, and waits up to 2 s for the daemon to acknowledge; after the resume it sends `SIGUSR2`, which puts the 15 s back (the daemon does that itself after 120 s if no resume signal arrives). The handshake uses `/run/control-ofc/sleep-hook.pid` and `/run/control-ofc/sleep-hook.ack`. In the journal a normal sleep shows `system sleep: systemd watchdog widened for the suspend and resume` and then `system sleep: systemd watchdog restored`; the hook itself logs `control-ofc: the daemon could not widen its watchdog for this sleep` or `control-ofc: the daemon did not acknowledge the sleep within 2 s` when that went wrong, and the daemon may then be restarted by a slow resume.
- **After a resume or an OpenFanController reconnect**, the daemon cannot tell whether each OpenFan channel it had written still runs at that speed. The controller keeps its speeds when only USB drops, but restarts every fan at about 1000 rpm when its 12 V supply went off. So the daemon remembers the speed each channel had and writes it again: on the next tick for a channel whose control cannot run (its curve writes it otherwise), and wherever the daemon decides what to leave a fan at — the no-CPU-sensor fallback (§ Safety) holds it at no less than that speed, a clean stop, a profile that stops naming it and the end of a thermal emergency leave it there (or at the exit minimum, whichever is higher), and the end of a calibration restores it exactly (DEC-466). A channel whose speed was already unknown before the loss — its last command was never confirmed — stays unknown: a stop leaves it at 100 %, and the fallback gives it 40 %.

## API quick reference

The `/status`, `/capabilities`, `/sensors`, `/fans` examples above are the
read endpoints most operators reach for. The full operator-relevant
surface is below; see `daemon.md` § API Endpoints for the complete contract
including request/response shapes.

### Read

| Endpoint | Use |
|---|---|
| `GET /status` | Subsystem health + freshness, `thermal_state`, uptime, and any active manual overrides / fan-identify holds; one-line answer to "is the daemon happy?" |
| `GET /capabilities` | Device list, feature flags, safety limits, kernel-warning catalogue (`devices.amd_gpu.kernel_warnings`) |
| `GET /sensors` | All temperature readings |
| `GET /fans` | Fan RPM + last-commanded PWM |
| `GET /poll` | Combined status + sensors + fans in one round-trip (the GUI's primary 1 Hz read path) |
| `GET /sensors/history?id=...&last=N` | Time-series history for a sensor entity |
| `GET /hwmon/headers` | Controllable motherboard PWM outputs |
| `GET /profiles`, `GET /profiles/{id}` | List stored profiles / fetch one full profile document (daemon is the store of record — DEC-160) |
| `GET /profile/active` | Current active profile or `{"active": false}` |
| `GET /config` | Every configuration key: its value on disk, the value this process started with, where it came from (`runtime`, `admin` or `default`), whether the API can change it, and whether a saved change is waiting for a restart (DEC-243) |
| `GET /diagnostics/hardware` | **The central troubleshooting endpoint.** Hardware readiness report — hwmon chips, GPU detection, thermal-safety state, kernel modules, ACPI conflicts, board info, kernel warnings. Use this first when something looks wrong. |
| `GET /inventory/hwmon` | Structured hwmon inventory — temps, fan tachs, PWM metadata (DEC-200) |
| `GET /inventory/hardware-readiness` | **The readiness endpoint the GUI actually calls.** Readiness items with blocking flags *and* passive Super-I/O detection, from one shared coalesced hardware scan, so the two halves can never disagree (DEC-207) |
| `GET /inventory/readiness` | *Superseded by `/inventory/hardware-readiness` (DEC-207).* Still served for older clients; no shipped GUI calls it (DEC-257) |
| `GET /inventory/superio` | *Superseded by `/inventory/hardware-readiness` (DEC-207).* Still served for older clients; no shipped GUI calls it (DEC-257) |

### Write

As of 2.0.0 the profile engine is the **sole writer** (DEC-159 / DEC-165) — there are no bare PWM write endpoints. Clients express *intent* (activate a profile, take an expiring override, identify a fan) and run a few diagnostics / maintenance calls. The hwmon lease is held internally by the daemon; there is no client lease surface.

**Profiles (store of record — DEC-160):**

| Endpoint | Use |
|---|---|
| `POST /profiles` | Create a stored profile (`?validate_only=true` validates only; `409 already_exists` on a duplicate id) |
| `PUT /profiles/{id}` | Replace a stored profile's desired-state (re-activate to apply — no hot reload) |
| `DELETE /profiles/{id}` | Remove a stored profile (`409 profile_in_use` if it is the active profile) |
| `POST /profile/activate` | Activate a profile by id (`{"profile_id": "..."}`) or path (`{"profile_path": "..."}` — the file must lie inside a profile search directory). Clears every control override; on the next engine tick a motherboard header or AMD GPU the old profile drove and the new one does not name is given back (DEC-382, DEC-448), and an OpenFan channel or a header with no automatic mode (an ARCTIC hub channel) it does not name is left at its last duty or `[shutdown] exit_floor_pct`, whichever is higher — 100 % if its last duty is unknown (DEC-451) |
| `POST /profile/deactivate` | Clear the active profile; idempotent. Clears every control override, and on the next engine tick each motherboard header the daemon took goes back to what it was doing before (DEC-382), and each AMD GPU whose fan curve it wrote goes back to PMFW's own curve (DEC-448); each OpenFan channel and each header with no automatic mode it drove is left at its last duty or `[shutdown] exit_floor_pct`, whichever is higher — 100 % if its last duty is unknown (DEC-451); no fan curve runs until a profile is activated again |

**Live control intent (DEC-163 / DEC-166):**

| Endpoint | Use |
|---|---|
| `POST /control/{control_id}/override` | Pin a control's fans to a fixed PWM — expiring, floor-clamped, deadman auto-reverts to the curve. Body `{"pwm_percent": 0..100, "ttl_secs"?}`; `ttl_secs` is clamped to 1-15 (default 15) |
| `POST /control/{control_id}/override/renew` | Extend the override deadman (fresh TTL). Body `{"override_token": N}` |
| `DELETE /control/{control_id}/override` | Release the override, reverting to curve control immediately. Body `{"override_token": N}` |
| `POST /fans/{fan_id}/identify` | Hold or restore one fan for physical identification (deadman auto-restore). An ordinary fan is stopped; a header the daemon holds as a pump is perturbed instead, never stopped (DEC-311/384). Body `{"action": "stop"\|"restore", "ttl_secs"?}`; `ttl_secs` is clamped to 1-15 |
| `POST /config/header-role` | Assign or clear one PWM header's role (DEC-311). Body `{"header_id": "<id>", "role": "pump"\|"cpu_fan"\|"radiator_fan"\|"chassis_fan"\|"no_fan"\|"unknown"\|null}` (`no_fan`, nothing plugged in, since 3.4.0). The id may also be an OpenFan channel, `openfan:ch00`–`openfan:ch09` (`control.openfan_header_roles`): `pump` there keeps the channel at or above the 30 % floor, makes identify vary its speed instead of stopping it, and refuses calibration. `cpu_fan`, on a header or a channel, keeps it at or above the 30 % floor in the active profile and nothing more — identify still stops it (since 3.6.0, `control.cpu_fan_role_floor`). Takes effect at once; assigning `pump` to a header an identify is holding stopped releases that stop (DEC-419) |
| `GET /inventory/cooling-devices` | The configured cooling-device topology — a pump, its radiator fans and an advisory sensor as one named assembly — plus every device policy this daemon ships (DEC-316) |
| `GET /validation/session` | The current or most recent validation session — what a cooler did while it was recording, plus the evidence summary (DEC-317). `404` when none has ever run |
| `GET /validation/sessions` | The last five retained sessions, newest first (DEC-317) |
| `GET /validation/sessions/{id}` | One retained session in full; `404` for an unknown id |
| `POST /config/cooling-device` | Create or replace one cooling device by id (DEC-316). Body `{"id": "<id>", "name"?, "kind"?, "pump_member"?, "radiator_members"?, "device_policy_id"?, ...}`. Safety limits are **not** settable — a policy is chosen by id and `minimum_safe_pwm` & siblings are rejected |
| `DELETE /config/cooling-device/{id}` | Remove one cooling device (DEC-316) |
| `POST /validation/session` | Start recording one session against a cooling device (DEC-317). Body `{"cooling_device_id": "<id>", "kind"?, "diagnostics"?, "sweep_members"?, "metadata"?, "stop_when_diagnostics_complete"?}`. **Read the lifecycle note below before starting one** — a session does not end when its diagnostics do unless you ask it to. **From 2.43.3, a repeated `diagnostics` entry is dropped at ingest** (DEC-341): the started session's `requested_diagnostics` is what the daemon actually took, so read it back rather than assuming your request was kept verbatim |
| `POST /validation/session/stop` | Finalise the session: compute the summary and findings, and persist it. **This is how a session ends.** Returns the finalised document |
| `DELETE /validation/session` | Finalise the session and record it as `cancelled`. **Not a discard** — a cancelled session carries the same samples, analysis and summary as a stopped one and is persisted identically; only `state` differs |
| `POST /validation/session/event` | Place a user marker on the session timeline. Body `{"detail"?, "member_id"?}` |
| `POST /validation/session/measurement` | Attach an external instrument reading (a meter, a scope) to the session. Body `{"kind": "<kind>", "value": N, "unit"?, "member_id"?, "note"?}` |

#### How a validation session ends

This is the part that surprises people, so it is stated plainly.

A session **records until you stop it.** Finishing the diagnostics you asked for
does **not** end it, and neither does closing the GUI window — the recording
lives in the daemon. Left completely alone a session stops by itself only when
it reaches the daemon's sample cap, which is 7200 samples at one per second:
**two hours.** With every diagnostic enabled the diagnostics themselves finish in
roughly four minutes, so the remaining hour and fifty-six minutes is passive
recording. That is deliberate — it is what lets you run a workload and capture
what the cooler did — but it is not what most people expect.

There are three ways to end one:

1. **`POST /validation/session/stop`** (the GUI's *Stop & Save*) — finalise and
   keep the evidence. This is the normal way.
2. **`DELETE /validation/session`** (the GUI's *Stop & Mark Cancelled*) —
   also finalises and also keeps the evidence, recorded as `cancelled`.
3. **`"stop_when_diagnostics_complete": true` at start** — the session finalises
   itself the moment the diagnostics you asked for have all run. Requires
   `control.validation_auto_stop` (daemon 2.43.0+) and at least one entry in
   `diagnostics`; asking for it with none is rejected, because there would be
   nothing to complete. Opt-in: the default is `false`, so a client that does not
   send it gets exactly the behaviour described above.

A session that is interrupted — the daemon stopped, the machine rebooted — is
recorded as `interrupted` at the last sample it actually took. Samples are never
invented for the gap.

**Diagnostics / maintenance:**

| Endpoint | Use |
|---|---|
| `POST /fans/openfan/{ch}/calibration` | Start an OpenFan calibration (DEC-452; `control.openfan_calibration`). It finds where the fan **stops** as its speed falls and where it **starts again** as it rises, and its RPM across the range. It takes the fan down to a stop, so the request must say `{"acknowledge_below_floor": true}` — a channel assigned the `pump` role is refused (`400`, reason `pump_protected`) whatever the request says, and a channel assigned `pump` while the run is under way stops it and is put back no lower than the pump floor. Do not run it on an unassigned channel that powers a pump: assign it `pump` first. Optional `hold_seconds` per step (default 5, 2–15; lengthened to fit three OpenFan readings where `polling.poll_interval_ms` is slow — the run reports the hold it used as `hold_ms`). Returns `202`; poll `GET /diagnostics/openfan-calibration`, stop it with `DELETE` on the same path (within half a second while it is stepping; a full-speed kick or the final restore already under way finishes first). It stops by itself if any sensor passes 85 °C, thermal safety starts forcing fans, temperatures go stale, or the CPU warms by more than 5 °C. A fan left stopped by a stopped run is given a full-speed kick first; if the daemon is shutting down and there is no time for the kick, the fan is left at full speed instead of its earlier duty. When it ends the channel gets back the duty it had before; if that duty is unknown (never commanded, or lost to a reconnect or resume) it gets 100 %; while thermal safety is forcing fans it is left at the forced duty. A refused request changes nothing |
| `POST /fans/openfan/{ch}/calibrate` | **Deprecated** (DEC-452): the same calibration, answered only when it ends, in the old shape. Now needs `acknowledge_below_floor: true` as well; `steps` is ignored |
| `POST /hwmon/{header}/verify` | Behavioural test of PWM write effectiveness; ~6 s (raised from 3 s in DEC-101 — slow-spinning fans need more settle time); the daemon uses its own internal verify lease (no `lease_id`). Returns `restore_failed: true` if the post-test restore-to-original-PWM write fails (DEC-100). If the header becomes a pump while the test runs — you assign it `pump`, or activate a profile that names it a pump — the test stops and returns `result: "pump_protected_mid_run"`, and the restore is floored at 30 % (DEC-418). |
| `POST /hwmon/{header}/characterize` | Start a PWM/RPM response sweep (DEC-313, 2.29.0+). A *deeper* diagnostic beside the ~6 s verify above, not a replacement: it holds the header at several duties and reports **command acceptance, PWM readback and physical RPM response as three separate verdicts** — collapsing them would report a pump that overrides PWM during startup as a broken fan. Returns `202`; poll `GET /diagnostics/characterization`. Optional `{"points_pct": [...], "settle_seconds": N}`, both clamped server-side. **Since 2.40.0 (DEC-334), gated on `control.pwm_behaviour_characterization`:** `"bidirectional": true` walks the duties **down from the top and back up** so hysteresis can be measured — the run therefore *ends* high, which is what keeps an interrupted one benign — and `"stability_seconds": N` (5-60) adds a dwell at up to 3 daemon-chosen duties for tach stability statistics. The walked-step budget is unchanged, so the worst-case run length is too. **A pump is never swept below 30%, and 0% is unreachable for any header as a swept point (a *pump* is also never restored below 30% afterwards; an ordinary fan is put back exactly where it was found, 0 included). A header that becomes a pump mid-run — assigned `pump`, or named a pump by a profile activated during the run — stops the run as `aborted`, and its restore is floored at 30 % (DEC-418).** **Since 2.53.0 (DEC-405; built as 2.52.0, which was never released)** each point holds 12 s by default, and its settling time and stability describe the tach *after* it settled — judged on the tach register's own refreshes, so a slow chip reports "not settled" rather than a figure it never measured. |
| `GET /diagnostics/characterization` | Current or most recent characterisation run, with points measured so far |
| `DELETE /diagnostics/characterization` | Ask a running sweep to stop; the pre-sweep duty is restored on every exit path on which nothing else owns the header. The two skips — a thermal force, and daemon shutdown — are reported in `restore_outcome` and both leave the header *high*. A third, since 2.56.0: when a read of the header does not return within 2 s, the run writes nothing more to it and leaves it at the last test duty (never below 20 %, 30 % for a pump), reported as `skipped_unresponsive` — writing to a driver that has stopped answering could stall every hwmon fan. A header that became a pump during the run is still restored, raised to 30 %. Since 3.1.0 a **write** that does not return within 2 s ends the run as `failed` and is reported the same way: that write may still land when the driver answers, and anything written after it would only wait behind it. On a header with an automatic mode the daemon writes nothing more, even for a pump, and the fan loop hands the header back once the driver answers; on a header with none (an ARCTIC hub channel, for example) the restore is queued behind the stuck write instead, since nothing else would ever put it back A **stability dwell** honours the cancel mid-hold rather than making you wait it out; a settle window still finishes, as documented |
| `GET /diagnostics/preflight?header=&diagnostic=` | The daemon's own safety verdict for one header and one diagnostic, before anything is driven (DEC-333, 2.39.0+). **Read-only: no lease, no slot, nothing reserved** — a `ready` verdict describes *now*, and the diagnostic's own POST still runs its own guards. Returns `{verdict, checks[], blocking[]}`; `verdict` is `ready`\|`warn`\|`blocked`. A stale temperature source **blocks** every diagnostic, and each one's POST returns `409 validation_error` on it (a run already in flight aborts), from the same predicate the verdict is built from — discovery since 2.42.0 (DEC-336), verify and characterisation since DEC-385. Older daemons only warn for those two. OpenFan calibration, which has no preflight, refuses and aborts on the same condition. A reading counts as stale once it is older than the thermal safety rule's own window — 5 poll intervals, so 5 s at the default 1 s poll (a flat 10 s before DEC-395) |
| `POST /hwmon/{header}/discover-control-path` | Establish which tach channel(s) this PWM output actually drives, by measurement rather than by sysfs numbering (DEC-333, 2.39.0+). Returns `202`; poll `GET /diagnostics/control-path`. Optional `{"delta_pct": N, "cycles": N, "window_seconds": N}`, all clamped server-side. **Deliberately not `pwmconfig`'s stop-the-fan model**: the perturbation moves *away from the nearer rail* so there is always headroom, every commanded duty is clamped into `[max(20, header floor) .. 100]` — **0% is unreachable for any header** — and a pump-protected header never crosses its 30% floor. Two cycles run, because repeatability is a confidence input. A header that becomes a pump mid-run stops the run as `aborted`, and the return to its starting duty is floored at 30 % (DEC-418). **Since 2.53.0 (DEC-405; built as 2.52.0, which was never released)** a later cycle first waits (up to 15 s) for the fans to settle after the previous change, so a slow pump's recovery is not mistaken for noise; each window defaults to 12 s. A pump whose tachometer stops reporting mid-run aborts immediately and restores |
| `GET /diagnostics/control-path` | Current or most recent discovery run, **plus every persisted relationship**. Records survive a restart and are keyed by the header's stable id, so a board or driver change invalidates one by construction. `no_tach_response` is a legitimate result, **not** a fault: the header may drive no tach-reporting device, or one running under its own internal control |
| `DELETE /diagnostics/control-path` | Ask a running discovery to stop. Same restore semantics, and the same two deliberate skips, as the characterisation sweep above |
| `POST /hwmon/{header}/stall-probe` | Find where a fan stops and where it starts again (DEC-407, 2.54.0+). **The only diagnostic that drives a fan below 20 %, down to 0 %** — opt-in, one header at a time, and only on a `chassis_fan` or `radiator_fan` header that is not pump-protected (assign the role first if it reads `unknown`). Send `{"acknowledge_below_floor": true}`; there is nothing else to set. Takes up to about three minutes below 20 %; any abort or cancel runs the fan at 100 % until it spins, then puts it back (a daemon shutdown puts every fan back itself instead). Stay at the machine while it runs. Poll `GET /diagnostics/stall-probe` |
| `GET /diagnostics/stall-probe` | The current or most recent probe: the stall and restart duties, or why it stopped |
| `DELETE /diagnostics/stall-probe` | Stop a running probe; the fan gets the 100 % kick, then its previous duty back |
| `POST /hwmon/rescan` | Re-enumerate hwmon devices and return fresh header list |
| `POST /fans/openfan/rescan` | Look for an OpenFanController and adopt it without restarting the daemon |
| `GET /fans/openfan/roles` | Each OpenFan channel's role and whether the daemon protects it as a pump (`stop_permitted`, `effective_min_pwm_pct`) |
| `GET /fans/openfan/device` | The OpenFAN controller's USB identity, its hardware and firmware reports, its link state, and whether a firmware update could start now — with each reason it could not (DEC-481, 3.8.0+). `daemon_write` says whether the daemon may write the firmware itself: `{"available": false, "reason": "no_usb_access", ...}` until the opt-in drop-in is installed (DEC-483) |
| `PUT /fans/openfan/firmware` | Upload a firmware file — the raw `.uf2` bytes, at most 1 MiB (DEC-483, 3.8.0+; `control.openfan_firmware_write`). Answers `{sha256, size, release?, verdict, reason?, message}`: `verdict` is `daemon_write` (a published release the daemon knows — it keeps the file to write it), `manual_copy` (`unknown_build` or `invalid_image` — copied by hand) or `refused` (`firmware_known_broken`: the 2023 FW_01 binary). Touches no hardware |
| `POST /fans/openfan/maintenance` | Start an OpenFAN firmware update (DEC-481, 3.8.0+; `control.openfan_firmware_maintenance`). Body `{"expected_usb_serial": "<serial>", "firmware": {"sha256": "<hex>", "size": N, ...}, "write": "manual"}` — a fingerprint of the file, never a path. Parks every OpenFAN channel at 100 %, sends the board into its bootloader, waits up to 15 minutes for you to copy the file onto the `RPI-RP2` drive, then checks the board and restores control. With `"write": "daemon"` (DEC-483) the daemon writes the file uploaded with `PUT /fans/openfan/firmware` itself and reads it back, and you copy nothing unless that write falls back to the copy. The FW_01 binary is refused (`firmware_known_broken`). `202` with the run id |
| `GET /fans/openfan/maintenance` | The current or most recent firmware update: each stage's timing, the outcome and the before/after evidence. Survives a restart |
| `DELETE /fans/openfan/maintenance` | Cancel a firmware update — only until the board is asked to enter its bootloader |
| `POST /gpu/{gpu_id}/fan/reset` | Restore GPU fan to firmware automatic and re-enable zero-RPM |
| `POST /gpu/{gpu_id}/fan/verify` | Behavioural test of GPU fan-control effectiveness; ~6 s, no lease (DEC-120). Drives a test speed biased upward, reads back the applied PMFW `fan_curve`/`pwm1` + RPM, then restores. Detects the silent failures static checks miss (`ppfeaturemask` bit 14 unset, SMU mismatch, BIOS overdrive lock). |
| `POST /config/profile-search-dirs` | Add and/or remove directories in the profile search path (immediate; persists to `runtime.toml`). `remove` needs ≥ 2.23.0 (DEC-285) |
| `POST /config/startup-delay` | Set startup-delay seconds, 0-30 (persisted to `runtime.toml`, takes effect on restart) |
| `POST /config/exit-floor` | Set the exit minimum, `{"exit_floor_pct": 0..100}` — the lowest speed a clean stop leaves an OpenFan channel or a header with no mode switch at (DEC-388). Applies at once |
| `POST /config/coolant-limit` | Set the coolant limit, `{"coolant_limit_c": 40..70}` whole °C — a coolant sensor at or above it forces every fan and pump to 100 % until it is 5 °C cooler (DEC-443, 3.0.0+). Applies at once; no off switch |
| `POST /config/poll-interval` | Set the poll interval, `{"poll_interval_ms": 250..2000}`; takes effect on restart. The ceiling bounds how stale a temperature the thermal emergency can act on |
| `POST /config/serial-port` | Set the OpenFanController port (`null` = auto-detect); takes effect on restart. A port that does not answer as an OpenFanController falls back to auto-detection |
| `POST /config/serial-timeout` | Set the serial read timeout, 50-1000 ms; takes effect on restart |
| `POST /config/allow-port-probe` | Opt into the active Super-I/O probe; also needs the `superio-port-probe.conf.example` drop-in |
| `POST /config/nvidia-telemetry` | Opt into read-only NVIDIA telemetry; also needs the `nvidia-telemetry.conf.example` drop-in |
| `POST /inventory/superio/probe` | Opt-in active Super-I/O `/dev/port` probe — off by default, needs `allow_port_probe` (DEC-203) |
| `POST /config/preferred-cpu-sensor` | Persist the preferred CPU temp sensor (persists to `runtime.toml`; DEC-200) |
| `POST /config/preferred-mb-sensor` | Persist the preferred motherboard temp sensor (persists to `runtime.toml`; DEC-200) |

**Retired at 2.0.0 (DEC-165):** the bare PWM writes (`/fans/openfan/{ch}/pwm`, `/fans/openfan/pwm`, `/hwmon/{id}/pwm`, `/gpu/{id}/fan/pwm`), `/fans/openfan/{ch}/target_rpm`, and the entire lease surface (`POST /hwmon/lease/take` / `/release` / `/renew` and `GET /hwmon/lease/status`). The daemon engine is the sole writer and self-leases.

All errors use a nested envelope: `{"error": {"code": "...", "message": "...", "retryable": bool, "source": "...", "details": ...}}`. See `daemon.md` § Error Envelope for the full code list.

## Setting fan speeds

The daemon, not the client, sets fan speeds. When a profile is active its profile engine evaluates the curves at 1 Hz and writes every backend (OpenFan, hwmon, AMD GPU) directly — across all backends in a single, coalesced control loop. There is no bare PWM write endpoint and no client-held lease (both retired at 2.0.0, DEC-165); the daemon holds the hwmon lease internally.

To change fan behaviour you have two levers:

- **Persistent:** author a profile (the GUI is the easiest way) and activate it. See **Fan profiles** below.
- **Temporary:** pin one control to a fixed speed with the **manual override** API. This is an expiring, deadman-guarded overlay on top of the active profile — see **Manual override** below.

### Manual override (temporary, per-control)

A manual override pins all fans in one of the active profile's logical *controls* to a fixed PWM. It is daemon-owned, expiring, and fencing-guarded (DEC-163): the override **reverts to autonomous curve control** if you stop renewing it (a deadman on the daemon's clock), and a superseded token cannot re-pin. The PWM is still clamped by the daemon's hard pump/CPU floor (≥30 %) and the GPU 0 % floor; the thermal force always wins.

```bash
SOCK="/run/control-ofc/control-ofc.sock"

# 1. Take an override on a control (control_id comes from the active profile).
#    Returns an override_token plus the TTL (15 s) and advised renew interval (~5 s).
TOKEN=$(curl -s --unix-socket $SOCK \
  -X POST -H "Content-Type: application/json" \
  -d '{"pwm_percent": 60}' \
  http://localhost/control/cpu_fans/override | jq -r .override_token)

# 2. Renew before the TTL lapses (repeat roughly every 5 s to hold it).
curl -s --unix-socket $SOCK \
  -X POST -H "Content-Type: application/json" \
  -d "{\"override_token\": $TOKEN}" \
  http://localhost/control/cpu_fans/override/renew | jq .

# 3. Release when done (reverts to the curve immediately).
curl -s --unix-socket $SOCK \
  -X DELETE -H "Content-Type: application/json" \
  -d "{\"override_token\": $TOKEN}" \
  http://localhost/control/cpu_fans/override | jq .
```

Active overrides also appear in `GET /status` (`overrides[]` of `{control_id, pwm_percent, expires_in_secs}`), but those entries carry no token — they can be displayed but only renewed or released by the client that created them.

### Identifying a fan (temporary stop/restore)

To find which physical fan is which, the fan-identify API changes a single fan briefly so you can spot the one that responded. It auto-restores on a deadman, so a crashed client can never leave a fan held (DEC-166).

**A pump is never stopped (DEC-311).** You always send `action: "stop"`; the daemon decides what that means from the header's role. An ordinary fan is driven to 0 (floor-exempt). A header the daemon holds as a pump — its role or its own label says so, or the active profile's name for it contains `pump` or `aio` (DEC-384) — is *perturbed* instead — shifted about 25 points clear of its current duty, upward wherever there is headroom, and never below the 30 % pump floor. The response's `mode` field says which happened (`"stop"` or `"pump_perturb"`), along with `identify_pwm_percent` and `baseline_pwm_percent`.

If your pump is on a header the daemon cannot classify — common on boards whose Super-I/O publishes no fan labels, where every header reads `role: "unknown"` — tell it explicitly first:

```bash
curl -s --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H 'Content-Type: application/json' \
  -d '{"header_id":"hwmon:it8696:it87.2624:pwm5:pwm5","role":"pump"}' \
  http://localhost/config/header-role
```

That assignment persists in `runtime.toml`, takes effect immediately, and also earns the header the 30 % pump floor and the stop-snap exemption.

Assigning `cpu_fan` instead (since 3.6.0) gives a header the same 30 % floor and stop-snap exemption in the active profile, as a `CPU_FAN` label does, but it is not a pump: identify still stops it, and `stop_permitted` stays `true`.

```bash
SOCK="/run/control-ofc/control-ofc.sock"

# Stop one fan (fan_id from GET /fans). Auto-restores after the deadman TTL.
curl -s --unix-socket $SOCK \
  -X POST -H "Content-Type: application/json" \
  -d '{"action": "stop"}' \
  http://localhost/fans/amd_gpu:0000:03:00.0/identify | jq .

# Restore it immediately (the engine resumes the fan's curve value).
curl -s --unix-socket $SOCK \
  -X POST -H "Content-Type: application/json" \
  -d '{"action": "restore"}' \
  http://localhost/fans/amd_gpu:0000:03:00.0/identify | jq .
```

## Serial device setup (OpenFanController)

**The OpenFanController is optional.** If you do not have one, nothing here applies
and nothing is wrong: the daemon notes once at `info` that it found none, and
motherboard and GPU fan control are unaffected.

At startup the daemon lists the `/dev/ttyACM*` and `/dev/ttyUSB*` devices that
exist, then opens each one in turn and asks it to identify itself. Only a device
that answers as an OpenFanController is adopted.

Two consequences worth knowing if you have **other** USB-serial hardware attached
(an Arduino, a 3D printer, a modem):

- Identifying a device means opening it, and on Linux opening a serial port
  asserts DTR — which **resets Arduino-class boards**. The daemon therefore opens
  each candidate at most once per attempt, and enumerates without opening
  wherever it can, but it cannot identify a device without opening it.
- Startup makes **one** attempt, then gets out of the way. The search continues
  in the background once the daemon is up and answering, for **60 seconds** by
  default or **180** if you have named a `[serial] port` — you have told it the
  device is there, so it stays interested longer. Nothing is delayed by this: the
  daemon is fully running throughout.
- The background search costs nothing when nothing changes. It compares the list
  of serial devices each time, and only opens anything when that list actually
  changes — so plugging the controller in during the window is picked up within a
  few seconds, while a machine whose devices never change is never re-probed.

If a controller that was working drops off — unplugged, or a USB reset — the
daemon keeps trying to get it back for as long as it runs, about every 30 seconds
at the default poll interval. Each try opens only the configured port, the
controller's own device node, and a serial device that has appeared since the
controller dropped off — on every try for its first minute, then once every five
minutes while the controller stays away. Other USB-serial devices that were
already attached are left alone, so an Arduino-class board beside the controller
is not reset while it is away. A controller that stops answering without dropping
off the USB bus is not recovered this way; restart the daemon
(`systemctl restart control-ofc-daemon`).

If a controller is attached but was not detected — it appeared after the window,
say — use **Rescan Hardware** in the GUI (`POST /fans/openfan/rescan`) rather than
restarting, or pin the port as below. For reliable detection across reboots, use a
stable device path:

```bash
# Find your device's stable path
ls -la /dev/serial/by-id/

# Example output:
# lrwxrwxrwx 1 root root ... usb-Karanovic_Research_OpenFan_...-if00 -> ../../ttyACM0

# Set the stable path in daemon.toml
# [serial]
# port = "/dev/serial/by-id/usb-Karanovic_Research_OpenFan_...-if00"
```

### Updating the OpenFAN firmware

From control-ofc-daemon 3.8.0 the GUI's **Update OpenFAN Firmware…** (Hardware page) installs a
`.uf2` file you downloaded. The daemon parks every OpenFAN channel at 100 %, sends the board into
its USB bootloader, waits while you copy the file onto the `RPI-RP2` drive, then checks the board
and gives the fans back to the profile. While an update runs, OpenFAN writes are skipped rather than
failed, calibrations and the other diagnostics are refused, and the `openfan` health entry says what
is happening. A run that leaves the board in its bootloader keeps that entry critical, and OpenFAN
writes off, until the board answers again; the run is recorded in
`/var/lib/control-ofc/openfan-maintenance.json`, so a daemon restart reports it rather than forgets
it.

**Letting the daemon write the firmware itself (opt-in, DEC-483).** For a published OpenFAN
release it knows by fingerprint, the daemon can write the file for you through the bootloader's
PICOBOOT USB interface: it reads the flash's unique id first and erases nothing unless that is
your board's serial number, writes only the sectors the file covers, reads every byte back, and
restarts the board — and the GUI then reports the exact build as verified, unless the board's
reports point at the old firmware instead. Anything that stops that write before the restart
gives the drive back to you and the update waits for the copy by hand, as above. It needs
read-write access to the USB device nodes, which the shipped unit does not grant; install the
opt-in drop-in once:

```bash
sudo install -Dm644 \
  /usr/share/doc/control-ofc-daemon/openfan-firmware-write.conf.example \
  /etc/systemd/system/control-ofc-daemon.service.d/openfan-firmware-write.conf
sudo systemctl daemon-reload
sudo systemctl restart control-ofc-daemon
```

The file says what it grants. With it installed, any local account can start an update that
writes one of those releases (never another file); without it, the update works as before.

The steps, results and recovery are in the GUI manual's
[Updating the OpenFAN firmware](https://github.com/Plan-B-Development/control-ofc-gui/blob/main/manual/openfan-controller.md#updating-the-openfan-firmware).

By hand, with the daemon stopped — it holds the port:

```bash
sudo systemctl stop control-ofc-daemon
ls /dev/serial/by-id/          # usb-Karanovic_Research_OpenFan_<serial>-if00 and -if02
stty -F /dev/serial/by-id/usb-Karanovic_Research_OpenFan_<serial>-if00 1200
# the RPI-RP2 drive appears: copy the .uf2 onto it; it disappears when the board restarts
sudo systemctl start control-ofc-daemon
```

### Serial permissions

The daemon runs as root, so no group membership is needed for it to open the serial device, and Debian/Ubuntu need no `dialout` drop-in. What does limit it is the unit's device allow-list, `DeviceAllow=char-ttyACM rw` and `DeviceAllow=char-ttyUSB rw`: the service can open only `/dev/ttyACM*` and `/dev/ttyUSB*` nodes (a `/dev/serial/by-id/` link to one of them is fine). A controller on any other kind of node would need a drop-in adding its device class:

```bash
sudo systemctl edit control-ofc-daemon
# Add, for example:
#   [Service]
#   DeviceAllow=char-ttyS rw
```

A udev rule is **not required** — the daemon auto-detects the OpenFanController on `/dev/ttyACM*` and `/dev/ttyUSB*` at startup. Use this only if you want a specific group/mode on the device node. For a stable path, use the `/dev/serial/by-id/` link above: the daemon opens only `/dev/ttyS*`, `ttyUSB*`, `ttyACM*`, `ttyAMA*` and `/dev/serial/` paths, so a custom udev symlink such as `/dev/control-ofc-controller` is refused. The example no longer creates one.

The package installs the example as documentation-only at `/usr/share/doc/control-ofc-daemon/99-control-ofc.rules.example`. To enable it, copy into `/etc/udev/rules.d/` and edit there (do not edit the shipped example — pacman will overwrite it on upgrade):
```bash
sudo install -m644 \
  /usr/share/doc/control-ofc-daemon/99-control-ofc.rules.example \
  /etc/udev/rules.d/99-control-ofc.rules

# Find VID/PID for your device:
udevadm info --attribute-walk --name=/dev/ttyACM0 | grep -E "idVendor|idProduct"

# Edit /etc/udev/rules.d/99-control-ofc.rules and replace XXXX/YYYY, then:
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=tty
```

## GPU fan control

AMD discrete GPU fans are supported. The control method depends on GPU generation:

- **RDNA3+ (RX 7000/9000 series):** Uses PMFW `fan_curve` sysfs interface. Requires `amdgpu.ppfeaturemask` kernel parameter with bit 14 set (`0x4000`, `PP_OVERDRIVE_MASK`). The recommended value enables every PP feature flag — narrower masks are also valid as long as bit 14 is set, but `0xffffffff` is what the daemon's diagnostics and GUI suggest, what most distros document, and what the daemon's runtime error message points users at:
  ```
  amdgpu.ppfeaturemask=0xffffffff
  ```
  PMFW is the **only** write path the daemon uses on these cards. An RX 7000 also exposes `pwm1` and `pwm1_enable`, but a write there can succeed and change nothing, so without a `fan_curve` the daemon reports the fan as `read_only` (DEC-430).

- **Pre-RDNA3 (RX 6000 and older):** The daemon writes the traditional `pwm1_enable=1` + `pwm1` pair from `POST /gpu/{id}/fan/verify` and `/fan/reset` only. The profile engine does not drive these fans, so `/capabilities` reports `fan_control_method: "hwmon_pwm"` with `fan_write_supported: false`, and a profile control bound only to one is listed in `skipped_controls` as `backend_unavailable` (DEC-445; daemons before this reported `true`, although no engine has ever written the card).

**Known kernel regressions.** The daemon matches the running kernel against a short list of published amdgpu regressions and reports any that apply in `GET /capabilities` (`devices.amd_gpu.kernel_warnings`); the GUI shows a high or critical one as a popup once per session. Since 2.56.1 the list holds one entry, `rdna_mes_hang_drm_amd_4765` (critical): on Linux 6.17.9–6.17.13 and 6.18.0–6.18.6, an RDNA3, RDNA3.5 or RDNA4 GPU can hang when a compute job runs beside a 3D workload, and nothing can change a fan's speed while the system is hung. Every AMD GPU in the machine is checked — an integrated one beside a discrete card too — and the message names the cards it applies to. It is fixed in 6.18.7 and 6.19 — update to the latest 6.18 longterm or a current 7.x kernel. The check uses the version number only, so a distribution kernel that already carries the fix may still be flagged. Daemon 2.56.0 and older raise two different advisories instead, both retired because their advice was wrong; the GUI manual's hardware-troubleshooting page says what to do about each.

GPU fans are driven by the daemon engine when a profile owns them — the bare `POST /gpu/{id}/fan/pwm` write was retired at 2.0.0 (DEC-165). For live manual control use the override API (DEC-163); to physically identify a GPU fan use the identify API (DEC-166); both are shown under **Setting fan speeds** above. GPU writes require no lease, and the daemon applies a 5% minimum-change threshold to avoid SMU firmware churn (DEC-070). When the daemon stops, each GPU fan curve it drove is put back to automatic; a card it never drove is left alone (§ Safety).

If a GPU fan has been left in a manual state and you want the firmware to take back over, reset it to automatic:

```bash
# Restore GPU fan to firmware automatic (re-enables zero-RPM)
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST http://localhost/gpu/0000:03:00.0/fan/reset | jq .
```

The GPU id is the card's bare PCI address — its entry's `pci_bdf` in `devices.amd_gpus` of `GET /capabilities` (`devices.amd_gpu.pci_bdf` is the primary card only) — not the fan id (`amd_gpu:0000:03:00.0`) or anything with a prefix, which answers `404`.

**A reset hands the fan to the firmware until the next profile activation.** The daemon stops writing that fan, even while the active profile names it, until a profile is activated again (the same one included); deactivating does not return it, and a daemon restart does. The GUI's *Restore GPU Fan to Automatic* is disabled while the active profile drives the card.

## Fan profiles

The daemon can autonomously evaluate fan curve profiles at 1 Hz. Profiles use the **v7** schema (GUI v1.38.0 / daemon v1.17.0 and later). The GUI authors and upgrades profiles; the daemon reads them forward-compatibly — newer fields are accepted and missing fields are defaulted — so you do not need a matching daemon version to load a newer profile. The daemon logs a warning only for profiles older than v3 (v4 introduced the `fan_zero_rpm` member flag the daemon relies on). An example ships at `/etc/control-ofc/profiles/quiet.json`. **It controls no fan as shipped**: its one control has an empty `members` list, so activating it unchanged drives nothing. The GUI's three starter profiles (Quiet, Balanced, Performance) are the same — one *All Fans* control with no members and a curve with no sensor. Copy one, add your fans and a sensor, then activate it.

### Loading a profile

**Do not run `control-ofc-daemon` by hand while the service is active.** A second daemon refuses to start — it exits with *another control-ofc-daemon is already running* before it touches the socket or a fan (DEC-467) — so it achieves nothing. (Daemons older than that deleted the service's socket, took it over, and ran a second profile engine on the same fans.) Starting the service while a daemon you started by hand is still running is still harmful, so stop that one first. To start with a particular profile, set `--profile` or `OPENFAN_PROFILE` in a systemd drop-in (below), or activate one through the API:

```bash
# Via API at runtime (saved, so it is also used at the next start)
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"profile_id": "quiet"}' \
  http://localhost/profile/activate | jq .

# Check active profile
curl --unix-socket /run/control-ofc/control-ofc.sock \
  http://localhost/profile/active | jq .
```

At startup the daemon uses the first of these that loads (DEC-435):

1. `--profile <name>` or `--profile-file <path>` — whichever comes first on the command line.
2. `OPENFAN_PROFILE`.
3. The profile last activated through the API (the GUI, the tray, or `POST /profile/activate`), saved in `/var/lib/control-ofc/daemon_state.json`.

A source that names no file, or a file that will not load, is logged and the next one is tried, so a mistyped `--profile` falls back to your saved profile instead of starting with none. `<name>` is the **file name without `.json`** (`quiet` for `quiet.json`) in a profile search directory — not the name the GUI shows, which can differ.

`--profile` and `OPENFAN_PROFILE` are **not saved**. While either is set it wins on every start — including every restart the watchdog or a crash triggers — over a profile you activated from the GUI; remove it and the next start uses the saved profile again. Activating a profile from the GUI while the daemon runs still takes effect at once and is saved.

Under systemd, set either in a drop-in. The environment variable is the simpler one:

```bash
sudo systemctl edit control-ofc-daemon
# add:
#   [Service]
#   Environment=OPENFAN_PROFILE=quiet
sudo systemctl restart control-ofc-daemon
```

To pass `--profile` instead, the drop-in must clear the unit's command line first (`ExecStart=` on its own line, then `ExecStart=/usr/bin/control-ofc-daemon --profile quiet`).

### Profile storage (CRUD)

Since v1.19.0 the daemon is the profile **store of record** (DEC-160): stored profiles live under `/var/lib/control-ofc/profiles/`, and the full document is served and edited over the API. The GUI uses this surface; scripts can too.

```bash
SOCK="/run/control-ofc/control-ofc.sock"

# List stored profiles (id / name / description summaries only)
curl -s --unix-socket $SOCK http://localhost/profiles | jq .

# Fetch one full profile document
curl -s --unix-socket $SOCK http://localhost/profiles/quiet | jq .

# Create a profile from a local JSON file
curl -s --unix-socket $SOCK \
  -X POST -H "Content-Type: application/json" \
  --data @my-profile.json \
  http://localhost/profiles | jq .

# Validate a document without storing it (?validate_only=true)
curl -s --unix-socket $SOCK \
  -X POST -H "Content-Type: application/json" \
  --data @my-profile.json \
  'http://localhost/profiles?validate_only=true' | jq .

# Replace a stored profile (re-activate afterwards to apply — no hot reload)
curl -s --unix-socket $SOCK \
  -X PUT -H "Content-Type: application/json" \
  --data @my-profile.json \
  http://localhost/profiles/my-profile | jq .

# Delete a stored profile (fails 409 profile_in_use if it is active)
curl -s --unix-socket $SOCK \
  -X DELETE http://localhost/profiles/my-profile | jq .
```

Profile ids are filesystem-safe stems (non-empty, ≤128 bytes, no `/`, `\`, `..`, or control characters — DEC-173). Validation returns hard `errors` (which reject the profile) and soft `warnings` (which are accepted); an unknown `sensor_id` is a warning, not an error, so profiles stay portable across machines.

### Profile search directories

The daemon searches for profiles in (highest priority first):
1. `/var/lib/control-ofc/profiles` — the daemon-owned **store of record**, prepended at startup so CRUD-created profiles are always found first (DEC-160)
2. `/etc/control-ofc/profiles` (always included)
3. `$XDG_CONFIG_HOME/control-ofc/profiles` if that is set, else `$HOME/.config/control-ofc/profiles` — `/root/.config/control-ofc/profiles` under the service, which sets `HOME=/root`

Items 2 and 3 are the default for `[profiles] search_dirs` in `daemon.toml`; setting that key replaces them, and the store in item 1 is still searched first. Additional directories can be registered at runtime via the API — the GUI registers its own profile folder this way every time it connects:

```bash
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"add": ["/home/user/.config/control-ofc/profiles"]}' \
  http://localhost/config/profile-search-dirs | jq .
```

A stale directory can be pruned the same way (daemon >= 2.23.0), and the two
operations combine into a single atomic "move" — removals are applied first:

```bash
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"add": ["/home/user/profiles-new"], "remove": ["/home/user/profiles-old"]}' \
  http://localhost/config/profile-search-dirs | jq .
```

`/etc/control-ofc/profiles` cannot be removed, and neither can the last
remaining entry — profile activation resolves against this list, so an empty one
would leave the daemon unable to find any profile at all. Both are
`400 validation_error`. A non-root caller may only touch directories under its
own home (DEC-205); removal does **not** require the directory to still exist,
which is the point — a stale entry usually no longer does.

### Profile engine ownership

While a profile is active the profile engine is the **sole writer** of every backend among the daemon's clients (DEC-159 / DEC-165) — no client writes a fan. It cannot stop *another program* writing the same fan (fancontrol, CoolerControl, a vendor tool): when a duty it set moves, it writes it again, and after three corrections that do not hold it stops and flags the header `duty_not_holding` (DEC-406; see § Troubleshooting). The GUI never writes PWM; it only sends intent (activate / override / identify). A manual override (DEC-163) overlays the curve for the controls it targets until it is released or its deadman expires; everything else keeps curve-controlling. The thermal-safety override always takes priority over both the active profile and any manual override — as a **floor**, not a replacement (DEC-307): each fan receives `max(commanded, forced)`. A fan no control commands receives the forced duty only in the 100 % emergency; the 40 % no-CPU-sensor floor reaches the active profile's fans alone (DEC-382, § Safety).

## Runtime configuration

Configuration is split between two files (see `docs/ADRs/002-runtime-config-split.md`):

- **`/etc/control-ofc/daemon.toml`** — admin-owned, hand-edited. Contains static topology: serial port, polling interval, socket path, state directory. Never rewritten by the daemon.
- **`/var/lib/control-ofc/runtime.toml`** — daemon-managed, written with 0600 permissions via atomic rename. It holds everything set through the API:
  - **the fan header roles you assign** (`[hardware] header_roles`, `POST /config/header-role`). On a board whose Super-I/O publishes no fan labels, a `pump` assignment here is the **only** evidence that a header drives a pump, so it is what gives that header the 30 % floor and keeps fan identify from stopping it;
  - the cooling devices (`[[cooling_devices]]`, `POST /config/cooling-device`);
  - the preferred CPU/motherboard temp sensors (DEC-200) and the exit minimum (DEC-388);
  - any profile search directory, startup delay, `[serial]`, `[polling]` or `[detection]` value changed through the API.

  **Do not delete this file or edit it by hand.** Back it up with `daemon.toml` — a restore that copies only `daemon.toml` loses your pump roles.

On startup the daemon loads `daemon.toml`, then overlays `runtime.toml` on top (runtime values win). `SIGHUP` / `systemctl reload` re-reads both files, but only the **profile search directories**, the **exit minimum** and the **coolant limit** are applied live — changes to the startup delay, serial port, polling interval, or socket path are read but take effect only on the next restart.

### When `runtime.toml` cannot be read

The daemon still starts — a damaged settings file must never leave the fans with no controller — but it runs on **defaults, with no header roles**, so a pump you assigned by hand has no 30 % floor and can be stopped by fan identify. It says so on `GET /status` and `GET /poll` as `runtime_config_degraded = {reason, path, detail, phase, kept_as?}`, and the GUI shows a banner. `phase` says what the failure cost:

| `phase` | What happened | What to do |
|---|---|---|
| `startup` | The file could not be read at start. Every setting in it, header roles included, is **not in effect**. | Repair the file (or restore it from a backup), then restart the daemon. Saving a setting does not bring the old ones back. If the record also carries `kept_as`, a setting was saved since: the file at `path` is now a new one with no roles, and your old settings are only in the `kept_as` copy — stop the daemon, repair that copy and move it back over `path`, then start it. |
| `reload` | A `SIGHUP` reload could not read it. Nothing changes: header roles, the profile search directories, the exit minimum and the coolant limit keep the values the daemon was running with, and an edit to them in either file waits for a reload that can read it. | Repair the file, then reload or restart. |
| `update` | A setting was saved while the file could not be read. The daemon kept the original as `runtime.toml.invalid-<unix-time>` beside it and replaced it with a new file carrying **the header roles and cooling devices it is running with**, plus the new setting. | Copy any other setting you need back from the `.invalid-` copy, then restart the daemon. |

`reason` is `unreadable` (an I/O error, or a file over 4 MiB) or `malformed` (read, but not valid for this daemon version). The daemon's full error is in `journalctl -u control-ofc-daemon`. A **missing** file is not a failure: that is a first boot.

### Startup delay

A configurable delay before the daemon begins device detection, useful for waiting for USB or hwmon devices to appear after boot:

```bash
# Set via API (takes effect on next restart, persists to runtime.toml)
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"delay_secs": 3}' \
  http://localhost/config/startup-delay | jq .
```

The delay is capped at 30 seconds.

## Troubleshooting

`GET /status` (and `GET /poll`) and `GET /fans` report four conditions the daemon cannot fix by itself. The GUI shows each one; the fields are listed here for scripts and for reading a support bundle.

| Field | What it means | What to do |
|---|---|---|
| `duty_not_holding: true` on an hwmon fan in `GET /fans` | Something else is writing this header. The daemon rewrote the duty it had set three times and the header did not keep it, so it has stopped fighting. It tries again when its curve asks for a different speed, and the flag clears when the header keeps a duty again. `duty_corrections` counts the rewrites since the daemon started. | Stop the other fan-control program (`fancontrol`, CoolerControl, CoreCtrl, `fan2go`, a vendor tool), or a BIOS/EC feature that drives the header. |
| `skipped_controls[]` on `/status` | A control in the active profile has not been commanding its fans for at least 3 ticks, so they hold their last speed. `reason` says why: `curve_not_found` (the curve it names is gone), `sensor_unavailable` (its sensor is missing or has stopped updating), `mix_unresolvable` / `sync_unresolvable` (a combined curve cannot be worked out), or `backend_unavailable` (none of its fans can be reached — for example an OpenFan fan with no controller attached, or an AMD GPU the daemon cannot drive: an RX 6000 or older, or an RX 7000/9000 without its PMFW fan curve). | Fix the profile or the sensor. A frozen sensor is the usual cause: see § Safety, "Stalled sensors stop driving curves". |
| `unavailable_sensors[]` on `/status` | A sensor the daemon found fails every read (a Wi-Fi card's temperature while the radio is off is the usual one) or reports an impossible temperature. It is logged once and left out of `GET /sensors`. | Nothing, unless a curve needs that sensor — then pick another. |
| `runtime_config_degraded` on `/status` | The daemon could not read `runtime.toml`, which holds your header roles. | See § Runtime configuration, "When `runtime.toml` cannot be read". |

## Upgrade notes

### Downgrading: comment out sections the older daemon does not know

Daemons up to and including 3.6.0 refuse to start on a `daemon.toml` section they do not know, and later ones only warn. Before installing an older daemon, comment out every section it predates that you have uncommented:

| Section | Added in | Comment it out before installing |
| --- | --- | --- |
| `[shutdown]` | 2.50.0 | anything older than 2.50.0 |
| `[safety]` | 3.0.0 | anything older than 3.0.0 |

Only sections are tolerated: a key a newer release added inside a section the older daemon knows (for example `[startup] record_startup`, added in 2.41.0) stops it starting at any version, so comment out those keys as well. If you forgot, the daemon exits at startup and `journalctl -u control-ofc-daemon` names the section or key; comment it out and `sudo systemctl restart control-ofc-daemon`. A setting made from the GUI lives in `runtime.toml`, not `daemon.toml`, and never stops an older daemon starting.

### v0.7.1 — Breaking: `publish_interval_ms` removed

The `publish_interval_ms` field under `[polling]` was a telemetry vestige that was never used by runtime code. It has been removed in v0.7.1. **If your `daemon.toml` contains this field, the daemon will fail to start** (`deny_unknown_fields`).

**Fix:** Remove the `publish_interval_ms` line from your `daemon.toml`:
```bash
sudo sed -i '/publish_interval_ms/d' /etc/control-ofc/daemon.toml
```

### v0.7.0 — Telemetry fully removed

Syslog/telemetry was de-scoped in R52 (v0.5.8). Remove any `[telemetry]` section from your `daemon.toml` — it will cause a parse error.

## Uninstall

```bash
sudo pacman -R control-ofc-daemon
```

The package stops and disables the service before it is removed. Stopping the daemon gives every motherboard (hwmon) fan header it took back to what it was doing before — usually its BIOS fan mode — both in-process as it exits and again through `ExecStopPost`; OpenFan channels and headers with no mode switch are left at the exit minimum (§ Safety).

pacman leaves behind what it did not install or what you changed:

- `/var/lib/control-ofc/` — the daemon's state: `runtime.toml` (your header roles and API-set settings), the saved active profile, the profile store and diagnostic records;
- an edited `/etc/control-ofc/daemon.toml`, `/etc/control-ofc/profiles/quiet.json` or `/etc/modules-load.d/control-ofc.conf`, kept with a `.pacsave` suffix, and any profile you added under `/etc/control-ofc/profiles/`;
- anything you installed by hand: a udev rule in `/etc/udev/rules.d/`, or a drop-in under `/etc/systemd/system/control-ofc-daemon.service.d/`.

Remove those by hand if you no longer want them:

```bash
sudo rm -rf /var/lib/control-ofc/ /etc/control-ofc/
sudo rm -f /etc/udev/rules.d/99-control-ofc.rules && sudo udevadm control --reload-rules
sudo rm -rf /etc/systemd/system/control-ofc-daemon.service.d/ && sudo systemctl daemon-reload
```

## Safety

The daemon enforces the following safety rules:

- **Thermal emergency override** — if the hottest CPU temperature sensor reaches the emergency limit, every OpenFan channel and writable motherboard (hwmon) fan header the machine has is driven to 100%. **The limit is at least 105°C and is per-machine** (DEC-308): where the kernel reports the CPU's own design ceiling the daemon raises the limit to `min(ceiling + 5 °C, 115 °C)`, because a modern part is *meant* to sit at its ceiling under sustained load — a limit set *at* the ceiling would fire on a perfectly healthy machine and then latch, since release needs a reading at or below 80°C that such a part never produces. `GET /diagnostics/hardware` reports the limit in use. The override holds until a *fresh* CPU reading is at or below 80°C — a sensor that stops updating or disappears keeps it at 100% — and then control returns to the active profile at once (DEC-386 removed the 60% two-cycle recovery floor). One reading at or above the limit is enough to start it, and the daemon never decides afterwards that the reading was wrong: a faulty sensor stuck at or above the limit keeps those fans at 100% until it reads 80°C or below (DEC-400, following IEC 61511-1 — a safety action that has fired stays in force until its reset). If your fans stay at full speed with no real heat, look for the CPU sensor reporting the limit, and fix or report its driver. Every other fan the emergency took is given back at that point — a motherboard header to what it was doing before, an OpenFan channel to its duty from before the emergency (DEC-382). **Both duties are floors over the active profile's output, never replacements for it** (DEC-307) — the ladder can only raise a fan, never lower one. GPU fans are deliberately excluded: there is no GPU emergency threshold — AMD's PMFW firmware protects the GPU itself by throttling its clocks on junction temperature, independently of any OS fan control. It does not speed a fan up past a curve the daemon has set, so while the daemon drives a GPU fan the throttling is what protects the card; the fan returns to the firmware's own control when the daemon stops. A GPU fan in your profile keeps following its own curve throughout an emergency, as it does the rest of the time (DEC-399).
- **The CPU emergency is a backstop, not a cooling-failure detector.** A modern CPU protects itself by throttling at its own temperature ceiling, and the emergency limit sits above that ceiling on purpose, so a stopped pump or stalled fans usually show up as a CPU pinned at its ceiling and running slower rather than as an emergency. Since 3.0.0 (DEC-443) three cooling-failure checks cover that gap:
  - **Coolant emergency** — when the hottest *current* coolant reading (an AIO or custom-loop coolant sensor the daemon recognises) is at or above the coolant limit — `[safety] coolant_limit_c`, whole °C, 40-70, default 60, also settable from the GUI; no off switch — every fan is forced to 100 % exactly as in the CPU emergency, until a current reading is 5 °C below the limit. A stale or vanished coolant sensor holds a latched emergency and otherwise does nothing (most machines have no coolant sensor). `thermal_state` is `emergency` for either rule, and `emergency_causes` on `GET /status` says which fired.
  - **Pump stall response** — a pump in the active profile that the daemon has seen spinning, and that then reads 0 RPM for 10 s while commanded to 30 % or more, is driven to 100 % for 30 s. If it turns it returns to its curve; if not, it stays at 100 %. A second stall in one run holds it at 100 % until the daemon restarts or a profile is activated. Listed on `GET /status` as `pump_stalls[]`.
  - **Cooling advisory** — a CPU held at or above its ceiling (its own `crit`, else 85 °C) for a minute while no fan or pump is commanded at 50 % or more raises `cpu_at_ceiling_low_cooling` on `GET /status` `advisories[]`. It forces nothing.

  A CPU that sits at its ceiling under a load it used to handle is still the sign to check the pump and fans.
- **DC pump floor** — a pump on a motherboard header running in DC (voltage) mode is never driven below 70 %, rather than the 30 % pump floor, because a voltage-driven pump stalls much higher in its range (DEC-443). This applies wherever the pump floor does — curves, manual overrides, identify, and every diagnostic.
- **Missing sensor fallback** — if no CPU temperature sensor reports for 5 consecutive polling cycles and no emergency is under way, the fans your active profile controls are held at 40% or more as a defensive measure (GPU fans excluded, as above); a fan whose own curve can no longer be read keeps the speed it had (DEC-386). Fans no profile controls are left to their own firmware curve, which reads its own CPU sensor: a flat 40% there could run a fan *slower* than the BIOS would under load, and with no profile active nothing is forced at all. (An ARCTIC Fan Controller is the exception: it has no firmware curve, and every write to it carries all ten channels, so since 2.56.1 the daemon sets each channel reading 0 that it did not choose to 100 % before writing any of them. Control all ten channels to keep them quiet.) Only the 100% emergency takes every fan (DEC-382). A sensor that is still *listed* but has **stopped updating** counts as missing (DEC-267): a reading older than five polling intervals is not treated as current, because a frozen temperature can never rise and would otherwise hide a real emergency indefinitely. There are exceptions, all following one rule: losing sight of a sensor must never *reduce* cooling (DEC-269). If a thermal emergency is already active the 100% force continues, whether the sensor went quiet or disappeared (DEC-386); and if the last reading before the sensor went quiet was at or above the 80°C release temperature, fan curves simply keep running on it. The 40% fallback applies when the last thing the daemon knew was that the system was *cool* — which is the case it was written for.
- **Stalled sensors stop driving curves** — the rule above concerns the CPU. Every other sensor that drives a fan curve (GPU, coolant, VRM, drive) is also checked for freshness: if it stops updating, its curve stops running and the fans it controls **hold at their current speed** rather than tracking a temperature that is no longer real. They are never dropped to zero, and never quietly lowered. A curve that combines several sensors keeps running on the ones it can still see, but is not allowed to command *less* than it already was until they are all back — so losing one input can make your fans stay high, never fall. If one of the sensors a combined curve names does not exist on your machine at all, the curve simply runs on the others. If your fans stop responding to a rising GPU or coolant temperature, check that sensor: a frozen reading is the likely cause, and the daemon has deliberately stopped trusting it. Sensors that disappear entirely (a driver unloaded, hardware removed) are dropped from `GET /sensors` instead of lingering at their last value.
- **Override visibility** — the current thermal-override state is reported as `thermal_state` in `GET /status` (`normal`, `emergency`, or `no_sensor_fallback`; daemons before DEC-386 also send `recovery`); the GUI shows a poll-driven thermal banner from it (DEC-165). The GUI has no fan-control loop of its own to pause — the daemon owns control throughout.
- **OpenFanController stops are not time-limited** — a channel commanded to 0% stays stopped for as long as it is commanded; the daemon does not restart it after a fixed time. The 8-second stop timer (advertised as `limits.openfan_stop_timeout_s`) refuses only a 0% command that would reach the device while an earlier stop is still timed as running, which no normal sequence of commands produces, because a repeated 0% is never re-sent (DEC-426). A pump is never stopped by a profile: pump and CPU members are held at or above the 30% floor, whatever the curve asks for. An OpenFan channel counts as a pump only once you assign it the `pump` role (`POST /config/header-role`); the daemon has no other way to tell.
- **Per-member minimum floors (DEC-162)** — the daemon reports no per-*header* floor (`min_pwm_percent: 0` for every hwmon header), but it **does** enforce the role-aware minimum the GUI bakes into each control's `minimum_pct`. A profile whose pump/CPU control drops below the hard `HARD_PUMP_CPU_FLOOR_PCT` (30%) is rejected at validation with `400 validation_error` (`FLOOR_TOO_LOW`), and the profile engine re-clamps every member to its effective floor on each eval tick (`member_effective_floor`). So floor safety is daemon-enforced, not merely a GUI profile constraint.
- **GPU fan curves and hwmon headers** are given back on daemon shutdown — each AMD GPU whose fan curve the daemon wrote (a profile's curve, or a hardware verify) back to PMFW's own curve with zero-RPM idle stop re-enabled, and each motherboard header the daemon took to exactly what it was doing before (DEC-382): its recorded `pwm_enable`, or its duty if it was already in manual mode. A GPU the daemon never wrote a curve to is not touched, so a curve LACT or CoreCtrl put on a card your profile does not name survives a stop or a restart; so does one put on a card after **Restore GPU Fan to Automatic** handed it back (DEC-435). The daemon does not wait for a stop to give a GPU back: once the active profile no longer names a card it drove — the profile deactivated, or switched to one without that card — it puts the card back on PMFW's own curve on the next tick, and leaves it alone until a profile names it again (DEC-448). An older AMD card (RX 6000 or earlier) that the daemon was testing with a hardware verify when it died goes back to the fan mode it had before the test (DEC-414). A header whose mode cannot be restored is set to full speed instead, as lm-sensors `fancontrol` does. The daemon never writes a fixed "automatic" value: `2` is automatic on `it87` but Thermal Cruise on `nct6775`, and on an NZXT Kraken it applies an empty curve. The one exception is a Dell machine whose `dell_smm` driver has a single BIOS fan-control switch for every fan: that switch can be set but not read, so the daemon gives it `2`, which on that driver hands the fans back to the BIOS (DEC-398). On such a machine a profile should control all of its fans or none of them. Only the GUI enforces that (from 2.80.0 it will not save or activate a profile that controls some of them); the daemon does not, so a profile saved before then, imported, or written by another client is still started by the tray or at boot. While the daemon holds the switch, a fan no profile controls has nothing driving it. When the daemon gives the switch back, the BIOS takes every fan, including any a profile still controls. Two mechanisms cover this: the daemon does it **in-process** as it shuts down, and `ExecStopPost` in the systemd unit repeats it once the daemon has exited, whatever ended it — a normal stop, a crash, a SIGKILL the daemon could not respond to, or the watchdog restarting a daemon that stopped responding. The in-process one is bounded, so a restore that hangs cannot keep the daemon from exiting, and `ExecStopPost` from running.
- **OpenFan fans, and motherboard headers with no mode switch, are left at a minimum on stop** (DEC-388). These have no firmware behaviour to go back to — an OpenFan channel holds whatever it was last told, indefinitely — so on a clean stop (including `systemctl restart`, a reboot, or the watchdog restarting a daemon that stopped responding) each one the daemon drove is left at its last speed or the **exit minimum**, whichever is higher: 50 % unless you change it in the GUI (Settings → Daemon Configuration → Exit minimum) or with `[shutdown] exit_floor_pct`. A fan whose last speed the daemon lost track of is left at 100 %; a fan it never drove is not touched, except an ARCTIC Fan Controller channel set to 100 % as above; 0 turns the minimum off. A crash or SIGKILL cannot run this — `ExecStopPost` cannot reach the OpenFan controller — so after one those fans keep their last speed until the daemon is back.
- **Neither guarantees the hardware actually came back.** Each restore step gives up after a few seconds so the daemon can always exit; if a chip or card has stopped accepting writes, nothing can restore it and those fans hold their last speed until something takes them over again.
