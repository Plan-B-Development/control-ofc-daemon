# Control-OFC Daemon — Architecture Overview

## What this is

A Rust daemon (`control-ofc-daemon`) that controls PC fans via three backends:
- **OpenFan** — custom serial (USB) fan controller
- **hwmon** — motherboard fans via Linux sysfs (`/sys/class/hwmon/`)
- **AMD GPU** — RDNA3+ PMFW fan curves or legacy hwmon PWM

Exposes an HTTP API over a Unix domain socket for the PySide6 GUI.

## Module Map

```
daemon/src/
  main.rs              — startup, config, signal handling, shutdown
  config.rs            — TOML config parsing + validation
  runtime_config.rs    — daemon-mutable runtime.toml (ADR-002)
  constants.rs         — centralized operational tuning values
  lib.rs               — crate re-exports

  serial/
    mod.rs             — serial subsystem re-exports
    transport.rs       — SerialTransport trait + mock
    real_transport.rs  — serialport impl + auto-detect
    protocol.rs        — OpenFan wire protocol encode/decode
    controller.rs      — FanController (set_pwm, read_rpm, calibration)

  hwmon/
    mod.rs             — hwmon subsystem re-exports
    discovery.rs       — sensor enumeration + stable ID generation
    reader.rs          — temperature reading from sysfs
    types.rs           — SensorKind, SensorReading, SensorDescriptor
    pwm_discovery.rs   — PWM header discovery (fan outputs)
    pwm_control.rs     — HwmonPwmController + SysfsWriter trait
    lease.rs           — LeaseManager (exclusive write access)
    gpu_detect.rs      — AMD GPU detection via sysfs/DRM
    gpu_fan.rs         — PMFW fan curve read/write/reset
    util.rs            — shared sysfs path helpers

  health/
    mod.rs             — health subsystem re-exports
    cache.rs           — StateCache (RwLock snapshot-clone)
    state.rs           — CachedSensorReading, CachedFanReading types
    staleness.rs       — Freshness enum + age thresholds
    history.rs         — HistoryRing (per-entity time-series)

  api/
    mod.rs             — API subsystem re-exports
    server.rs          — Axum router + UDS listener
    handlers.rs        — all endpoint handler functions
    responses.rs       — response structs (Serialize)
    sse.rs             — Server-Sent Events stream
    calibration.rs     — OpenFan calibration sweep

  profile.rs           — profile JSON loading + curve evaluation
  profile_engine.rs    — headless 1Hz curve evaluation loop
  daemon_state.rs      — persistent state (active profile pointer)
  safety.rs            — ThermalSafetyRule (CPU emergency override)
  polling.rs           — hwmon + OpenFan polling loops
  error.rs             — error types (thiserror)
```

## Data Flow

```
[hwmon sysfs] ──read──> polling loops ──> StateCache ──> API handlers ──> GUI
[serial USB]  ──read──>                                  SSE stream  ──>
[GPU sysfs]   ──read──>

GUI ──POST──> API handlers ──write──> [hwmon sysfs]
                                      [serial USB]
                                      [GPU sysfs]

profile_engine ──read──> StateCache
               ──eval──> curves
               ──write──> [all backends]
```

## Safety Model

1. **ThermalSafetyRule** (`safety.rs`): Emergency CPU override
   - Triggers at hottest CpuTemp >= 105C, forces ALL fans to 100%
   - Holds until CpuTemp <= 80C (25C hysteresis)
   - One-cycle 60% recovery floor after release
   - If no CpuTemp sensor found for 5 consecutive cycles, forces fans to 40%

2. **Lease system** (`lease.rs`): Exclusive hwmon write access
   - 60s TTL, holder must renew periodically
   - Prevents GUI and profile engine from conflicting

3. **Stop timeout** (`controller.rs`): OpenFan 0% time limit
   - 8 seconds at 0% PWM, then rejects further 0% commands

4. **ExecStopPost restore** (`packaging/control-ofc-restore-auto.sh`):
   - Restores `pwm_enable=2` (auto) on ANY service stop (including SIGKILL)
   - Resets GPU fan curves to automatic

## Running

**Always start the daemon via systemd.** The binary under `/usr/bin/control-ofc-daemon`
is not meant to be invoked directly — it requires root, and the runtime
(`/run/control-ofc/`) and state (`/var/lib/control-ofc/`) directories are
prepared by systemd via `RuntimeDirectory=` and `StateDirectory=` in the
unit file. Running the binary by hand as a regular user hits `EACCES` on
the IPC socket and exits immediately with an actionable message.

```
sudo systemctl enable --now control-ofc-daemon
```

Developers who need to run the binary out-of-band can pass the hidden
`--allow-non-root` flag and override `ipc.socket_path` + `state.state_dir`
in `daemon.toml` to user-writable locations. This is not supported for
end users.

## Configuration

Configuration lives in two files (see `docs/ADRs/002-runtime-config-split.md`):

- **Admin config** — `/etc/control-ofc/daemon.toml`
  (override: `--config` or `$CONTROL_OFC_CONFIG`).
  Hand-edited by the operator. Never rewritten by the daemon. Holds static
  topology: serial port, polling interval, socket path, state dir.
- **Runtime config** — `{state_dir}/runtime.toml`
  (default `/var/lib/control-ofc/runtime.toml`).
  Managed by the daemon. Holds the keys that API endpoints mutate at
  runtime: `[profiles] search_dirs`, `[startup] delay_secs`. Written with
  0600 permissions via atomic tmp+rename.

On startup the daemon loads `daemon.toml`, then overlays `runtime.toml` on
top; runtime values win. SIGHUP re-reads both and re-applies the overlay.

Other paths:

- **Profile loading**: `--profile <name>` | `--profile-file <path>` | `$OPENFAN_PROFILE` | persisted state
- **Socket**: `/run/control-ofc/control-ofc.sock` (configurable via `ipc.socket_path`)
- **Persisted state**: `/var/lib/control-ofc/daemon_state.json` (configurable via `state.state_dir`)

### Migration (1.0.x → 1.1.x)

The 1.1.x release window still parses `[profiles]` and `[startup]` from
`daemon.toml` for backward compatibility. On first start after upgrade the
daemon copies those sections into `runtime.toml` if the runtime file does
not already contain them. The legacy sections in `daemon.toml` are not
deleted — the daemon never rewrites admin-owned config — but they are
shadowed by `runtime.toml` from that point forward. In 1.2.0 parsing
`[profiles]` / `[startup]` from `daemon.toml` becomes a hard error.

## API Endpoints

Full route table (source of truth: `daemon/src/api/server.rs`).

### Read endpoints

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/status` | Subsystem health + freshness |
| GET | `/sensors` | All temperature readings |
| GET | `/fans` | Fan RPM + last commanded PWM |
| GET | `/poll` | Batch: status + sensors + fans |
| GET | `/sensors/history` | Per-entity time-series (ring buffer) |
| GET | `/events` | Server-Sent Events stream (`event: update`, 5s heartbeat) |
| GET | `/capabilities` | Device list, feature flags, limits |
| GET | `/hwmon/headers` | Controllable motherboard PWM outputs |
| GET | `/hwmon/lease/status` | Lease holder + TTL |
| GET | `/profile/active` | Current active profile or `{"active": false}` |

### Write endpoints — OpenFan

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/fans/openfan/{channel}/pwm` | Set a single OpenFan channel PWM |
| POST | `/fans/openfan/pwm` | Set all OpenFan channels in one call |
| POST | `/fans/openfan/{channel}/target_rpm` | Closed-loop RPM target (not used by V1 GUI) |
| POST | `/fans/openfan/{channel}/calibrate` | PWM→RPM sweep (long-running, thermal-aborting) |

### Write endpoints — GPU

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/gpu/{gpu_id}/fan/pwm` | Set GPU fan to static speed (5% change threshold) |
| POST | `/gpu/{gpu_id}/fan/reset` | Restore GPU fan to automatic / re-enable zero-RPM |

### Write endpoints — hwmon

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/hwmon/lease/take` | Acquire exclusive write lease (60s TTL) |
| POST | `/hwmon/lease/renew` | Renew held lease |
| POST | `/hwmon/lease/release` | Release held lease |
| POST | `/hwmon/{header_id}/pwm` | Set hwmon PWM (lease required) |
| POST | `/hwmon/rescan` | Re-enumerate hwmon devices and return fresh header list |

### Write endpoints — profile / config

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/profile/activate` | Switch active profile by id or path |
| POST | `/config/profile-search-dirs` | Additively register profile search directories (persists to `runtime.toml`; 503 `persistence_failed` on write error) |
| POST | `/config/startup-delay` | Set startup delay seconds (persists to `runtime.toml`, takes effect on restart; 503 `persistence_failed` on write error) |

Error envelope (all errors):

```json
{
  "error": {
    "code": "string",
    "message": "string",
    "details": "any | omitted",
    "retryable": true,
    "source": "validation | internal | hardware"
  }
}
```

Codes: `validation_error` (400), `lease_required` (400 for invalid/expired lease, 403 for missing lease on write),
`not_found` (404), `lease_already_held` (409), `thermal_abort` (409),
`internal_error` (500), `hardware_unavailable` (503), `persistence_failed` (503).
