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

The daemon needs read/write access to the serial device. The systemd service file includes `SupplementaryGroups=uucp dialout` to support both Arch-based (`uucp`) and Debian-based (`dialout`) distributions.

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

The daemon enforces safety floors to prevent hardware damage:
- OpenFanController channels: minimum 20% PWM (0% allowed for max 8 seconds)
- Motherboard chassis fans: minimum 20% PWM
- Motherboard CPU/pump fans: minimum 30% PWM (0% never allowed)

These floors are enforced by the daemon and cannot be bypassed by the GUI.
