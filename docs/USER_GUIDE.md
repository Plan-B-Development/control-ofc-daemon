# User Guide

## What is Control-OFC?

Control-OFC is a fan control daemon for Linux desktops. It communicates with:
- **OpenFanController** — a USB fan controller (up to 10 channels)
- **Motherboard fan headers** — via the Linux hwmon sysfs interface (ITE, NCT Super I/O chips)

The daemon is the **autonomous, sole controller** of your fans. When a profile is active its built-in profile engine evaluates the fan curves at 1 Hz and writes every backend (OpenFan, hwmon, AMD GPU) itself — it keeps fans controlled through GUI close, crash, or sleep. The GUI is an editor/viewer/controller-of-intent; it never writes PWM and is poll-only (DEC-159 / DEC-165). With no profile active the daemon is purely imperative — it holds fans at whatever the hardware last had and only the thermal-safety override acts on its own.

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
| AMD GPU fans (pre-RDNA3) | Yes | Yes (daemon-driven via pwm1) |
| Intel Arc discrete GPU fans (`xe` / `i915`) | Yes (RPM) | No — firmware-managed, no kernel PWM interface (DEC-121) |
| NVIDIA discrete GPU fans (`nouveau` / opt-in NVML) | Yes (RPM; firmware-measured duty via NVML) | No — read-only, no kernel/PMFW write path (DEC-204) |
| AIO coolers (hwmon-attached) | Yes — coolant temp (`CoolantTemp` kind) + pump RPM (DEC-156) | Yes — hwmon pump PWM via the guided Configure-AIO flow; fixed speed or a temperature curve, always floored at 30% (DEC-157, DEC-312). A motherboard-connected pump is configured the same way once its header carries the `pump` role (GUI ≥ v2.51.0 assigns it; `POST /config/header-role` otherwise) |

## Installation

Most users should install the package rather than build from source: add the
signed `[control-ofc]` pacman repository once and the daemon then upgrades with
a normal `sudo pacman -Syu`. The setup commands, the one-off `pacman -U` path
using the package attached to every release, and the Sigstore verification step
are all in the [Install section of the README](../README.md#install).

Building from a checkout instead:

```bash
# Build
cd daemon
cargo build --release

# Install (run from inside daemon/ — this is a Cargo workspace, so the binary
# and packaging files are one level up, under the workspace root)
sudo cp ../target/release/control-ofc-daemon /usr/local/bin/

# Install systemd service + example config
sudo cp ../packaging/control-ofc-daemon.service /etc/systemd/system/
sudo mkdir -p /etc/control-ofc
sudo cp ../packaging/daemon.toml.example /etc/control-ofc/daemon.toml
sudo systemctl daemon-reload
sudo systemctl enable --now control-ofc-daemon
```

## Hardware sensor modules

The daemon discovers sensors and fan headers by scanning `/sys/class/hwmon/`. For devices to appear there, the correct kernel modules must be loaded.

**Automatically handled:** The package installs `/etc/modules-load.d/control-ofc.conf`, which loads common Super I/O chipset modules at boot:

| Module | Chipset | Common boards |
|--------|---------|---------------|
| `nct6775` | Nuvoton NCT6775/6776/6779/6798 | ASUS, Gigabyte, MSI |
| `it87` | ITE IT8686/8688/8689/8696 | Gigabyte, ASRock |
| `w83627ehf` | Winbond W83627EHF/DHG | Older boards |
| `drivetemp` | SATA/SAS drive temperature | All SATA drives |

CPU temperature modules (`coretemp` for Intel, `k10temp` for AMD) and SMBus adapter modules (`i2c-i801`, `i2c-piix4`) auto-load via PCI/ACPI matching — no configuration needed.

**If your hardware is not detected:** check the GUI's readiness report first (the **System State** page) — it identifies your board's chips and the exact module or AUR package needed **without probing the hardware**. As a **last resort**, install `lm_sensors` and run:
```bash
sudo sensors-detect
```
This interactively probes for additional sensor chips and persists the results. Probing is at your own risk: it "can access chips in a way these chips do not like, causing problems ranging from SMBus lockup to permanent hardware damage (a rare case, thankfully)" — [sensors-detect(8)](https://man.archlinux.org/man/extra/lm_sensors/sensors-detect.8.en). Accept the conservative defaults, and never run it after boot on a dual-chip Gigabyte board (it can wedge the Super-I/O bridge so the secondary chip vanishes until reboot). Then restart the daemon:
```bash
sudo systemctl restart control-ofc-daemon
```

**ACPI conflicts:** Some boards (particularly Gigabyte) require the `acpi_enforce_resources=lax` kernel parameter for Super I/O modules to bind. Add it to your bootloader kernel command line if you see `ACPI resource conflict` messages in `dmesg`.

**Out-of-tree modules:** Some newer motherboard chipsets require DKMS modules not yet in mainline (e.g. `it87` for newer ITE chips, `nct6687` for some MSI/ASUS boards). These are available from the AUR and must be installed separately.

### What the daemon detects — and what it deliberately doesn't

The daemon exposes a structured, **read-only** view of your cooling hardware for the GUI (`GET /inventory/hwmon` and `GET /inventory/hardware-readiness`):

- **Detected automatically (read-only):** CPU and motherboard temperature sensors — each classified (e.g. *CPU Tctl*, *VRM*, *chipset*) with a confidence and a plain-English reason — a recommended default CPU sensor, every controllable PWM fan header, and **monitor-only fan tachometers** (fans whose RPM can be read but not controlled). The daemon also builds a readiness checklist that explains what works, what is missing, what is read-only, and what to do about it.
- **Deliberately NOT automated:** the daemon never runs `sensors-detect`, never loads kernel modules, never edits your bootloader/initramfs/udev, and **never writes to a fan during discovery**. Anything that could change system behaviour is left to you (with guidance), so discovery is safe to run at any time.
- **"Control unverified":** a writable PWM header only *appears* controllable until a fan-control verification confirms a write actually changes fan speed. Until then, the readiness list marks control as unverified.
- **Read-only fans:** some PWM channels are exposed read-only by the kernel driver; those fans can be monitored but not controlled, and the readiness list says so rather than pretending otherwise.
- **Reboot may be required:** loading a missing Super I/O or DKMS driver to gain fan control usually needs a reboot or module reload — the relevant readiness item flags this.
- **GPU is out of scope here:** GPU fan discovery and control are owned by the GPU subsystem, not this hwmon path (DEC-102 / DEC-130).

## Configuration

Configuration is optional. The daemon uses sensible defaults if no config file exists.

The config file path can be overridden:
```bash
# CLI argument (highest priority)
control-ofc-daemon --config /path/to/daemon.toml

# Environment variable
CONTROL_OFC_CONFIG=/path/to/daemon.toml control-ofc-daemon

# Default (used when neither is set)
# /etc/control-ofc/daemon.toml
```

Create `/etc/control-ofc/daemon.toml`:

```toml
[serial]
# port = "/dev/ttyACM0"   # auto-detect if omitted
# timeout_ms = 500

[polling]
# poll_interval_ms = 1000   # 100-6000; slower is clamped to 6000 (DEC-270)

[ipc]
# socket_path = "/run/control-ofc/control-ofc.sock"

[state]
# state_dir = "/var/lib/control-ofc"
```

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

## API quick reference

The `/status`, `/capabilities`, `/sensors`, `/fans` examples above are the
read endpoints most operators reach for. The full operator-relevant
surface is below; see `daemon.md` § API Endpoints for the complete contract
including request/response shapes.

### Read

| Endpoint | Use |
|---|---|
| `GET /status` | Subsystem health + freshness, `thermal_state`, uptime, and any active manual overrides / fan-identify holds; one-line answer to "is the daemon happy?" |
| `GET /capabilities` | Device list, feature flags, safety limits, kernel-warning catalogue (`amd_gpu.kernel_warnings`) |
| `GET /sensors` | All temperature readings |
| `GET /fans` | Fan RPM + last-commanded PWM |
| `GET /poll` | Combined status + sensors + fans in one round-trip (the GUI's primary 1 Hz read path) |
| `GET /sensors/history?id=...&last=N` | Time-series history for a sensor entity |
| `GET /hwmon/headers` | Controllable motherboard PWM outputs |
| `GET /profiles`, `GET /profiles/{id}` | List stored profiles / fetch one full profile document (daemon is the store of record — DEC-160) |
| `GET /profile/active` | Current active profile or `{"active": false}` |
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
| `POST /profile/activate` | Activate a profile by id (`{"profile_id": "..."}`) or path (`{"profile_path": "..."}`) |
| `POST /profile/deactivate` | Clear the active profile; releases the daemon's internal `profile-engine` lease (DEC-097); idempotent |

**Live control intent (DEC-163 / DEC-166):**

| Endpoint | Use |
|---|---|
| `POST /control/{control_id}/override` | Pin a control's fans to a fixed PWM — expiring, floor-clamped, deadman auto-reverts to the curve. Body `{"pwm_percent": 0..100, "ttl_secs"?}` |
| `POST /control/{control_id}/override/renew` | Extend the override deadman (fresh TTL). Body `{"override_token": N}` |
| `DELETE /control/{control_id}/override` | Release the override, reverting to curve control immediately. Body `{"override_token": N}` |
| `POST /fans/{fan_id}/identify` | Hold or restore one fan for physical identification (deadman auto-restore). An ordinary fan is stopped; a `role: pump` header is perturbed instead, never stopped (DEC-311). Body `{"action": "stop"\|"restore", "ttl_secs"?}` |
| `POST /config/header-role` | Assign or clear one PWM header's role (DEC-311). Body `{"header_id": "<id>", "role": "pump"\|"cpu_fan"\|"radiator_fan"\|"chassis_fan"\|"unknown"\|null}` |
| `GET /inventory/cooling-devices` | The configured cooling-device topology — a pump, its radiator fans and an advisory sensor as one named assembly — plus every device policy this daemon ships (DEC-316) |
| `GET /validation/session` | The current or most recent validation session — what a cooler did while it was recording, plus the evidence summary (DEC-317). `404` when none has ever run |
| `GET /validation/sessions` | The last five retained sessions, newest first (DEC-317) |
| `POST /config/cooling-device` | Create or replace one cooling device by id (DEC-316). Body `{"id": "<id>", "name"?, "kind"?, "pump_member"?, "radiator_members"?, "device_policy_id"?, ...}`. Safety limits are **not** settable — a policy is chosen by id and `minimum_safe_pwm` & siblings are rejected |
| `DELETE /config/cooling-device/{id}` | Remove one cooling device (DEC-316) |

**Diagnostics / maintenance:**

| Endpoint | Use |
|---|---|
| `POST /fans/openfan/{ch}/calibrate` | Long-running PWM→RPM calibration sweep; restores pre-calibration PWM on every exit path, aborts on thermal limit (DEC-134) |
| `POST /hwmon/{header}/verify` | Behavioural test of PWM write effectiveness; ~6 s (raised from 3 s in DEC-101 — slow-spinning fans need more settle time); the daemon uses its own internal verify lease (no `lease_id`). Returns `restore_failed: true` if the post-test restore-to-original-PWM write fails (DEC-100). |
| `POST /hwmon/{header}/characterize` | Start a PWM/RPM response sweep (DEC-313, 2.29.0+). A *deeper* diagnostic beside the ~6 s verify above, not a replacement: it holds the header at several duties and reports **command acceptance, PWM readback and physical RPM response as three separate verdicts** — collapsing them would report a pump that overrides PWM during startup as a broken fan. Returns `202`; poll `GET /diagnostics/characterization`. Optional `{"points_pct": [...], "settle_seconds": N}`, both clamped server-side. **Since 2.40.0 (DEC-334), gated on `control.pwm_behaviour_characterization`:** `"bidirectional": true` walks the duties **down from the top and back up** so hysteresis can be measured — the run therefore *ends* high, which is what keeps an interrupted one benign — and `"stability_seconds": N` (5-60) adds a dwell at up to 3 daemon-chosen duties for tach stability statistics. The walked-step budget is unchanged, so the worst-case run length is too. **A pump is never swept below 30%, and 0% is unreachable for any header as a swept point (a *pump* is also never restored below 30% afterwards; an ordinary fan is put back exactly where it was found, 0 included).** |
| `GET /diagnostics/characterization` | Current or most recent characterisation run, with points measured so far |
| `DELETE /diagnostics/characterization` | Ask a running sweep to stop; the pre-sweep duty is restored on every exit path on which nothing else owns the header. The two skips — a thermal force, and daemon shutdown — are reported in `restore_outcome` and both leave the header *high*. A **stability dwell** honours the cancel mid-hold rather than making you wait it out; a settle window still finishes, as documented |
| `GET /diagnostics/preflight?header=&diagnostic=` | The daemon's own safety verdict for one header and one diagnostic, before anything is driven (DEC-333, 2.39.0+). **Read-only: no lease, no slot, nothing reserved** — a `ready` verdict describes *now*, and the diagnostic's own POST still runs its own guards. Returns `{verdict, checks[], blocking[]}`; `verdict` is `ready`\|`warn`\|`blocked`. A stale temperature source **blocks** control-path discovery and only **warns** for verify and characterisation, because those two do not refuse on it and a preflight must not promise a refusal the daemon will not perform |
| `POST /hwmon/{header}/discover-control-path` | Establish which tach channel(s) this PWM output actually drives, by measurement rather than by sysfs numbering (DEC-333, 2.39.0+). Returns `202`; poll `GET /diagnostics/control-path`. Optional `{"delta_pct": N, "cycles": N, "window_seconds": N}`, all clamped server-side. **Deliberately not `pwmconfig`'s stop-the-fan model**: the perturbation moves *away from the nearer rail* so there is always headroom, every commanded duty is clamped into `[max(20, header floor) .. 100]` — **0% is unreachable for any header** — and a pump-protected header never crosses its 30% floor. Two cycles run, because repeatability is a confidence input. A pump whose tachometer stops reporting mid-run aborts immediately and restores |
| `GET /diagnostics/control-path` | Current or most recent discovery run, **plus every persisted relationship**. Records survive a restart and are keyed by the header's stable id, so a board or driver change invalidates one by construction. `no_tach_response` is a legitimate result, **not** a fault: the header may drive no tach-reporting device, or one running under its own internal control |
| `DELETE /diagnostics/control-path` | Ask a running discovery to stop. Same restore semantics, and the same two deliberate skips, as the characterisation sweep above |
| `POST /hwmon/rescan` | Re-enumerate hwmon devices and return fresh header list |
| `POST /fans/openfan/rescan` | Look for an OpenFanController and adopt it without restarting the daemon |
| `POST /gpu/{gpu_id}/fan/reset` | Restore GPU fan to firmware automatic and re-enable zero-RPM |
| `POST /gpu/{gpu_id}/fan/verify` | Behavioural test of GPU fan-control effectiveness; ~6 s, no lease (DEC-120). Drives a test speed biased upward, reads back the applied PMFW `fan_curve`/`pwm1` + RPM, then restores. Detects the silent failures static checks miss (`ppfeaturemask` bit 14 unset, SMU mismatch, BIOS overdrive lock). |
| `POST /config/profile-search-dirs` | Add and/or remove directories in the profile search path (immediate; persists to `runtime.toml`). `remove` needs ≥ 2.23.0 (DEC-285) |
| `POST /config/startup-delay` | Set startup-delay seconds (persisted to `runtime.toml`, takes effect on restart) |
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

**A pump is never stopped (DEC-311).** You always send `action: "stop"`; the daemon decides what that means from the header's role. An ordinary fan is driven to 0 (floor-exempt). A header whose role resolves to `pump` is *perturbed* instead — shifted about 25 points clear of its current duty, upward wherever there is headroom, and never below the 30 % pump floor. The response's `mode` field says which happened (`"stop"` or `"pump_perturb"`), along with `identify_pwm_percent` and `baseline_pwm_percent`.

If your pump is on a header the daemon cannot classify — common on boards whose Super-I/O publishes no fan labels, where every header reads `role: "unknown"` — tell it explicitly first:

```bash
curl -s --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H 'Content-Type: application/json' \
  -d '{"header_id":"hwmon:it8696:it87.2624:pwm5:pwm5","role":"pump"}' \
  http://localhost/config/header-role
```

That assignment persists in `runtime.toml`, takes effect immediately, and also earns the header the 30 % pump floor and the stop-snap exemption.

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

The daemon auto-detects the OpenFanController by probing `/dev/ttyACM*` and `/dev/ttyUSB*` devices. For reliable detection across reboots, use a stable device path:

```bash
# Find your device's stable path
ls -la /dev/serial/by-id/

# Example output:
# lrwxrwxrwx 1 root root ... usb-Karanovic_Research_OpenFan_...-if00 -> ../../ttyACM0

# Set the stable path in daemon.toml
# [serial]
# port = "/dev/serial/by-id/usb-Karanovic_Research_OpenFan_...-if00"
```

### Serial permissions

The daemon needs read/write access to the serial device. The systemd service file includes `SupplementaryGroups=uucp` for Arch-based distributions. Debian/Ubuntu users (where the serial group is `dialout`) should add a systemd drop-in override:

```bash
sudo systemctl edit control-ofc-daemon
# Add:
#   [Service]
#   SupplementaryGroups=uucp dialout
```

A udev rule is **not required** — the daemon auto-detects the OpenFanController on `/dev/ttyACM*` and `/dev/ttyUSB*` at startup. Use this only if you want a stable `/dev/control-ofc-controller` symlink or a specific group/mode on the device node.

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

- **Pre-RDNA3 (RX 6000 and older):** Uses traditional `pwm1_enable=1` + `pwm1` control.

GPU fans are driven by the daemon engine when a profile owns them — the bare `POST /gpu/{id}/fan/pwm` write was retired at 2.0.0 (DEC-165). For live manual control use the override API (DEC-163); to physically identify a GPU fan use the identify API (DEC-166); both are shown under **Setting fan speeds** above. GPU writes require no lease, and the daemon applies a 5% minimum-change threshold to avoid SMU firmware churn (DEC-070). Fan curves are restored to automatic mode on daemon shutdown.

If a GPU fan has been left in a manual state and you want the firmware to take back over, reset it to automatic:

```bash
# Restore GPU fan to firmware automatic (re-enables zero-RPM)
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST http://localhost/gpu/gpu:amd:0000:03:00.0/fan/reset | jq .
```

The GPU ID is available from `GET /capabilities`.

## Fan profiles

The daemon can autonomously evaluate fan curve profiles at 1 Hz. Profiles use the **v7** schema (GUI v1.38.0 / daemon v1.17.0 and later). The GUI authors and upgrades profiles; the daemon reads them forward-compatibly — newer fields are accepted and missing fields are defaulted — so you do not need a matching daemon version to load a newer profile. The daemon logs a warning only for profiles older than v3 (v4 introduced the `fan_zero_rpm` member flag the daemon relies on). An example ships at `/etc/control-ofc/profiles/quiet.json`.

### Loading a profile

```bash
# Via CLI (highest priority)
control-ofc-daemon --profile quiet
control-ofc-daemon --profile-file /path/to/custom.json

# Via environment variable
OPENFAN_PROFILE=quiet control-ofc-daemon

# Via API at runtime
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"profile_id": "quiet"}' \
  http://localhost/profile/activate | jq .

# Check active profile
curl --unix-socket /run/control-ofc/control-ofc.sock \
  http://localhost/profile/active | jq .
```

The daemon persists the active profile selection to `/var/lib/control-ofc/daemon_state.json`, so it survives restarts.

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
3. `$HOME/.config/control-ofc/profiles` (or `$XDG_CONFIG_HOME/control-ofc/profiles`; `/root/.config/...` when `HOME` is unset for the systemd service)

Additional directories can be registered at runtime via the API:

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

While a profile is active the profile engine is the **sole writer** of every backend (DEC-159 / DEC-165) — there is no second writer to coordinate with. The GUI never writes PWM; it only sends intent (activate / override / identify). A manual override (DEC-163) overlays the curve for the controls it targets until it is released or its deadman expires; everything else keeps curve-controlling. The thermal-safety override always takes priority over both the active profile and any manual override — as a **floor**, not a replacement (DEC-307): each fan receives `max(commanded, forced)`, and a fan no control commands still receives the forced duty.

## Runtime configuration

Configuration is split between two files (see `docs/ADRs/002-runtime-config-split.md`):

- **`/etc/control-ofc/daemon.toml`** — admin-owned, hand-edited. Contains static topology: serial port, polling interval, socket path, state directory. Never rewritten by the daemon.
- **`/var/lib/control-ofc/runtime.toml`** — daemon-managed. Contains settings that API endpoints mutate at runtime: profile search directories, startup delay, and the preferred CPU/motherboard temp sensors (DEC-200). Written with 0600 permissions via atomic rename.

On startup the daemon loads `daemon.toml`, then overlays `runtime.toml` on top (runtime values win). `SIGHUP` / `systemctl reload` re-reads both files, but only the **profile search directories** are applied live — changes to the startup delay, serial port, polling interval, or socket path are read but take effect only on the next restart.

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

## Upgrade notes

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
# Stop and disable the service
sudo systemctl stop control-ofc-daemon
sudo systemctl disable control-ofc-daemon

# Remove files
sudo rm /etc/systemd/system/control-ofc-daemon.service
sudo rm /usr/local/bin/control-ofc-daemon
sudo rm /usr/local/bin/control-ofc-restore-auto  # if installed

# Remove config and state (optional — preserves your settings if omitted)
sudo rm -rf /etc/control-ofc/
sudo rm -rf /var/lib/control-ofc/

# Remove udev rules if installed
sudo rm -f /etc/udev/rules.d/99-control-ofc.rules
sudo udevadm control --reload-rules

# Reload systemd
sudo systemctl daemon-reload
```

After stopping the daemon, hwmon fans are automatically restored to automatic mode (via `ExecStopPost` in the service file).

## Safety

The daemon enforces the following safety rules:

- **Thermal emergency override** — if the hottest CPU temperature sensor reaches the emergency limit, all OpenFan channels and writable motherboard (hwmon) fan headers are driven to 100%. **The limit is at least 105°C and is per-machine** (DEC-308): where the kernel reports the CPU's own design ceiling the daemon raises the limit to `min(ceiling + 5 °C, 115 °C)`, because a modern part is *meant* to sit at its ceiling under sustained load — a limit set *at* the ceiling would fire on a perfectly healthy machine and then latch, since release needs a reading at or below 80°C that such a part never produces. `GET /diagnostics/hardware` reports the limit in use. The override holds until CPU temperature drops to 80°C, then holds a 60% floor for two cycles (the release cycle plus a one-cycle recovery floor) before returning control to the active profile. **All three duties are floors over the active profile's output, never replacements for it** (DEC-307) — the ladder can only raise a fan, never lower one. GPU fans are deliberately excluded: there is no GPU emergency threshold — AMD's PMFW firmware protects the GPU itself (junction-temperature throttling and its own fan ramp) independently of any OS fan control.
- **Missing sensor fallback** — if no CPU temperature sensor reports for 5 consecutive polling cycles, all OpenFan and hwmon fans are forced to 40% as a defensive measure (GPU fans excluded, as above). A sensor that is still *listed* but has **stopped updating** counts as missing (DEC-267): a reading older than five polling intervals is not treated as current, because a frozen temperature can never rise and would otherwise hide a real emergency indefinitely. There are exceptions, all following one rule: losing sight of a sensor must never *reduce* cooling (DEC-269). If a thermal emergency is already active the 100% force continues; if the daemon is in its post-emergency recovery the 60% floor continues; and if the last reading before the sensor went quiet was at or above the 80°C release temperature, fan curves simply keep running on it. The 40% fallback applies when the last thing the daemon knew was that the system was *cool* — which is the case it was written for.
- **Stalled sensors stop driving curves** — the rule above concerns the CPU. Every other sensor that drives a fan curve (GPU, coolant, VRM, drive) is also checked for freshness: if it stops updating, its curve stops running and the fans it controls **hold at their current speed** rather than tracking a temperature that is no longer real. They are never dropped to zero, and never quietly lowered. A curve that combines several sensors keeps running on the ones it can still see, but is not allowed to command *less* than it already was until they are all back — so losing one input can make your fans stay high, never fall. If one of the sensors a combined curve names does not exist on your machine at all, the curve simply runs on the others. If your fans stop responding to a rising GPU or coolant temperature, check that sensor: a frozen reading is the likely cause, and the daemon has deliberately stopped trusting it. Sensors that disappear entirely (a driver unloaded, hardware removed) are dropped from `GET /sensors` instead of lingering at their last value.
- **Override visibility** — the current thermal-override state is reported as `thermal_state` in `GET /status` (`normal`, `recovery`, `emergency`, or `no_sensor_fallback`); the GUI shows a poll-driven thermal banner from it (DEC-165). The GUI has no fan-control loop of its own to pause — the daemon owns control throughout.
- **OpenFanController stop timeout** — 0% PWM is allowed for a maximum of 8 seconds per channel, after which further 0% commands are rejected until a non-zero value is sent.
- **Per-member minimum floors (DEC-162)** — the daemon reports no per-*header* floor (`min_pwm_percent: 0` for every hwmon header), but it **does** enforce the role-aware minimum the GUI bakes into each control's `minimum_pct`. A profile whose pump/CPU control drops below the hard `HARD_PUMP_CPU_FLOOR_PCT` (30%) is rejected at validation with `400 validation_error` (`FLOOR_TOO_LOW`), and the profile engine re-clamps every member to its effective floor on each eval tick (`member_effective_floor`). So floor safety is daemon-enforced, not merely a GUI profile constraint.
- **GPU fan curves and hwmon headers** are restored to automatic mode on daemon shutdown — GPU curves back to PMFW control, motherboard headers to `pwm_enable=2` so the BIOS regains thermal control. Two mechanisms cover this, and they cover different paths: the daemon does it **in-process** as it shuts down, and `ExecStopPost` in the systemd unit repeats it for any *stop job*, including a SIGKILL the daemon could not respond to. The in-process one is what covers a self-restart after an internal failure, where systemd runs no stop job at all and `ExecStopPost` therefore does not run.
- **Neither guarantees the hardware actually came back.** Each restore step gives up after a few seconds so the daemon can always exit; if a chip or card has stopped accepting writes, nothing can restore it and those fans hold their last speed until something takes them over again.
