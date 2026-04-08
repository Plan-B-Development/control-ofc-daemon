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

4. **ExecStopPost restore** (`packaging/control_ofc-restore-auto.sh`):
   - Restores `pwm_enable=2` (auto) on ANY service stop (including SIGKILL)
   - Resets GPU fan curves to automatic

## Configuration

- **Config file**: `/etc/control_ofc/daemon.toml` (override: `--config` or `$CONTROL-OFC_CONFIG`)
- **Profile loading**: `--profile <name>` | `--profile-file <path>` | `$OPENFAN_PROFILE` | persisted state
- **Socket**: `/run/control_ofc/control_ofc.sock` (configurable via `ipc.socket_path`)
- **State**: `/var/lib/control_ofc/daemon_state.json` (configurable via `state.state_dir`)

## API Endpoints

See `daemon/src/api/server.rs` for the full route table. Key endpoints:

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/status` | Subsystem health |
| GET | `/poll` | Batch: status + sensors + fans |
| GET | `/capabilities` | Device list, feature flags, limits |
| GET | `/sensors/history` | Per-entity time-series |
| GET | `/events` | SSE real-time stream |
| POST | `/fans/openfan/{ch}/pwm` | Set OpenFan PWM |
| POST | `/gpu/{gpu_id}/fan/pwm` | Set GPU fan speed |
| POST | `/hwmon/{header_id}/pwm` | Set hwmon PWM (lease required) |
| POST | `/profile/activate` | Switch active profile |
