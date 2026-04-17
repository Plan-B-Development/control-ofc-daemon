# User Guide

## What is Control-OFC?

Control-OFC is a fan control daemon for Linux desktops. It communicates with:
- **OpenFanController** — a USB fan controller (up to 10 channels)
- **Motherboard fan headers** — via the Linux hwmon sysfs interface (ITE, NCT Super I/O chips)

The daemon provides a local API that a GUI (or scripts) can use to monitor temperatures, read fan RPM, and set fan speeds.

## Supported hardware

| Device | Read | Write |
|---|---|---|
| CPU temperature (k10temp, coretemp) | Yes | N/A |
| GPU temperature (amdgpu) | Yes | N/A |
| Disk temperature (NVMe) | Yes | N/A |
| Motherboard temperature (ITE, NCT) | Yes | N/A |
| OpenFanController fans (RPM) | Yes | Yes (PWM, target RPM) |
| Motherboard fan headers (hwmon) | Yes | Yes (PWM, requires lease) |
| AMD GPU fans (RDNA3+, PMFW) | Yes | Yes (static speed, no lease) |
| AMD GPU fans (pre-RDNA3) | Yes | Yes (pwm1, no lease) |
| AIO coolers | Not yet | Not yet |

## Installation

```bash
# Build
cd daemon
cargo build --release

# Install binary
sudo cp target/release/control-ofc-daemon /usr/local/bin/

# Install systemd service
sudo cp packaging/control-ofc-daemon.service /etc/systemd/system/
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

**If your hardware is not detected:** Install `lm_sensors` and run:
```bash
sudo sensors-detect
```
This interactively probes for additional sensor chips and persists the results. Then restart the daemon:
```bash
sudo systemctl restart control-ofc-daemon
```

**ACPI conflicts:** Some boards (particularly Gigabyte) require the `acpi_enforce_resources=lax` kernel parameter for Super I/O modules to bind. Add it to your bootloader kernel command line if you see `ACPI resource conflict` messages in `dmesg`.

**Out-of-tree modules:** Some newer motherboard chipsets require DKMS modules not yet in mainline (e.g. `it87` for newer ITE chips, `nct6687` for some MSI/ASUS boards). These are available from the AUR and must be installed separately.

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
# poll_interval_ms = 1000

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

## Setting fan speeds

### OpenFanController

```bash
# Set channel 0 to 50% PWM
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"pwm_percent": 50}' \
  http://localhost/fans/openfan/0/pwm | jq .

# Set all channels to 75%
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"pwm_percent": 75}' \
  http://localhost/fans/openfan/pwm | jq .
```

### Motherboard fan headers (hwmon)

Hwmon writes require an exclusive lease:

```bash
# 1. Take lease
LEASE=$(curl -s --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"owner_hint": "manual"}' \
  http://localhost/hwmon/lease/take | jq -r .lease_id)

# 2. Set PWM (use header ID from /hwmon/headers)
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d "{\"pwm_percent\": 60, \"lease_id\": \"$LEASE\"}" \
  http://localhost/hwmon/hwmon:it8696:0000:00:1f.0:pwm1:CHA_FAN1/pwm | jq .

# 3. Release lease when done
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d "{\"lease_id\": \"$LEASE\"}" \
  http://localhost/hwmon/lease/release | jq .
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

- **RDNA3+ (RX 7000/9000 series):** Uses PMFW `fan_curve` sysfs interface. Requires `amdgpu.ppfeaturemask` kernel parameter with bit 14 set (0x4000). Add to your kernel command line:
  ```
  amdgpu.ppfeaturemask=0xfffd7fff
  ```
  Or the common permissive value: `amdgpu.ppfeaturemask=0xffffffff`

- **Pre-RDNA3 (RX 6000 and older):** Uses traditional `pwm1_enable=1` + `pwm1` control.

GPU fan writes do not require a lease. The daemon uses a 5% minimum change threshold to avoid SMU firmware churn. Fan curves are restored to automatic mode on daemon shutdown.

```bash
# Set GPU fan to 60%
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"speed_pct": 60}' \
  http://localhost/gpu/gpu:amd:0000:03:00.0/fan/pwm | jq .

# Restore GPU fan to automatic
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST http://localhost/gpu/gpu:amd:0000:03:00.0/fan/reset | jq .
```

The GPU ID is available from `GET /capabilities`.

## Fan profiles

The daemon can autonomously evaluate fan curve profiles at 1 Hz. Profiles are JSON files compatible with the GUI's Profile v3 format. An example ships at `/etc/control-ofc/profiles/quiet.json`.

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

### Profile search directories

The daemon searches for profiles in:
1. `/etc/control-ofc/profiles` (always included)
2. `$HOME/.config/control-ofc/profiles` (or `$XDG_CONFIG_HOME/control-ofc/profiles`)

Additional directories can be registered at runtime via the API:

```bash
curl --unix-socket /run/control-ofc/control-ofc.sock \
  -X POST -H "Content-Type: application/json" \
  -d '{"add": ["/home/user/.config/control-ofc/profiles"]}' \
  http://localhost/config/profile-search-dirs | jq .
```

### Profile engine and GUI coexistence

When the GUI has written a fan command within the last 30 seconds, the profile engine defers its writes to avoid dual-writer contention. The thermal safety override always takes priority over both the GUI and the profile engine.

## Runtime configuration

Configuration is split between two files (see `docs/ADRs/002-runtime-config-split.md`):

- **`/etc/control-ofc/daemon.toml`** — admin-owned, hand-edited. Contains static topology: serial port, polling interval, socket path, state directory. Never rewritten by the daemon.
- **`/var/lib/control-ofc/runtime.toml`** — daemon-managed. Contains settings that API endpoints mutate at runtime: profile search directories and startup delay. Written with 0600 permissions via atomic rename.

On startup the daemon loads `daemon.toml`, then overlays `runtime.toml` on top (runtime values win). `SIGHUP` / `systemctl reload` re-reads both files.

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

- **Thermal emergency override** — if the hottest CPU temperature sensor reaches 105°C, all fans (OpenFan, hwmon, GPU) are forced to 100%. The override holds until CPU temperature drops to 80°C (25°C hysteresis), then applies a one-cycle 60% recovery floor before returning control to the active profile.
- **Missing sensor fallback** — if no CPU temperature sensor reports for 5 consecutive polling cycles, all fans are forced to 40% as a defensive measure.
- **OpenFanController stop timeout** — 0% PWM is allowed for a maximum of 8 seconds per channel, after which further 0% commands are rejected until a non-zero value is sent.
- **Hwmon PWM headers** — the daemon does not enforce per-header minimum floors. Safety limits are expressed via the `/capabilities` endpoint and enforced by the GUI's profile constraints.
- **GPU fan curves** are restored to automatic mode on daemon shutdown (via `ExecStopPost` in the systemd service file).
- **Hwmon headers** are restored to automatic mode (`pwm_enable=2`) on daemon shutdown so the BIOS regains thermal control.
