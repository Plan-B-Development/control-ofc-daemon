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
    aio.rs             — liquid-cooler (AIO/custom-loop) recognition: coolant-sensor + is_aio flag + aio_hwmon cap (DEC-156)
    gpu_detect.rs      — AMD GPU detection via sysfs/DRM
    intel_gpu_detect.rs— Intel discrete GPU (Arc) detection, read-only (DEC-121)
    gpu_fan.rs         — PMFW fan curve read/write/reset
    kernel_warnings.rs — kernel-version regression catalog
                          (RDNA3/4 hard hang on 6.19, R9700 SMU on 7.0)
                          surfaced via /capabilities.amd_gpu.kernel_warnings
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
    handlers/
      mod.rs           — AppState, shared helpers, submodule re-exports
      status.rs        — read endpoints (status, sensors, fans, poll, capabilities, history)
      openfan.rs       — OpenFan serial write endpoints + calibration handler
      gpu.rs           — AMD GPU fan set/reset endpoints
      hwmon_ctl.rs     — hwmon header list, rescan, PWM-verify endpoints
      profile.rs       — profile activation + CRUD endpoints
      control.rs       — manual-override + fan-identify endpoints (DEC-163/166)
      config.rs        — runtime config endpoints (search dirs, startup delay)
      hw_diagnostics.rs — hardware diagnostics endpoint
    responses.rs       — response structs (Serialize)
    sse.rs             — Server-Sent Events stream
    calibration.rs     — OpenFan calibration sweep

  pwm.rs               — shared percent_to_raw / raw_to_percent conversion
  clock.rs             — injectable monotonic clock (lease/override/identify TTLs; deterministic in tests)
  atomic_io.rs         — crash-safe atomic file write (tmp+fsync+rename)
  profile.rs           — profile JSON loading + curve evaluation
  profile_store.rs     — daemon-owned profile storage (store of record, DEC-160)
  profile_engine/      — headless 1Hz curve evaluation loop (DEC-135)
    mod.rs             — safety tick + profile evaluation + loop
    backends.rs        — WriteBackend per fan backend (gating/coalescing)
  control_override.rs  — manual-override + fan-identify state (expiring, fencing-guarded, deadman; DEC-163/166)
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

profile_engine ──read──> StateCache        (SOLE writer, 2.0.0+ — DEC-159/DEC-165)
               ──eval──> curves
               ──write──> [all backends: hwmon sysfs, serial USB, GPU sysfs]

GUI ──POST intent──> API handlers ──> profile_engine
     (activate profile / override / identify — never a direct PWM write)
```

The engine keeps per-control cross-tick state (step-rate anchors, the 2°C
falling-temperature deadband DEC-096, trigger latches). Two rules stop that state
from masking a change the user just made (DEC-188): an explicit
`POST /profile/activate` — **including re-applying the same profile id** after
editing its curve — re-anchors all of it on the next tick (an activation-epoch
counter on `StateCache`, bumped and read under the `active_profile` lock so the
swap and the bump are observed together), and the deadband self-releases for one
tick after `DEADBAND_MAX_HOLD_CYCLES` (~30 s) so a temperature that settles just
inside the band cannot pin the pre-settle fan speed indefinitely.

## Safety Model

1. **ThermalSafetyRule** (`safety.rs`): Emergency CPU override
   - Triggers at hottest CpuTemp >= 105C, forces all OpenFan channels and
     writable hwmon headers to 100%
   - GPU fans are deliberately excluded (DEC-130) — there is no GPU emergency
     threshold; AMD PMFW firmware owns GPU thermal protection (junction-temp
     throttling, firmware fan ramp) independently of OS fan control
   - Holds until CpuTemp <= 80C (25C hysteresis)
   - 60% recovery floor for two cycles after release (the release cycle + a
     one-cycle recovery floor), then control returns to the profile
   - If no CpuTemp sensor found for 5 consecutive cycles, forces all
     OpenFan+hwmon fans to 40%
   - Override state is surfaced as `thermal_state` in `GET /status`
     (`normal` | `recovery` | `emergency` | `no_sensor_fallback`, DEC-132)
     so the GUI shows a poll-driven thermal banner (DEC-165 — there is no GUI
     loop to stand down; the daemon owns control)

2. **Lease system** (`lease.rs`): Exclusive hwmon write access
   - 60s TTL, holder must renew periodically
   - Held internally by the profile engine (sole writer, 2.0.0+); guards against
     conflicting external hwmon writers. The GUI holds no lease (DEC-165).

3. **Stop timeout** (`controller.rs`): OpenFan 0% time limit
   - 8 seconds at 0% PWM, then rejects further 0% commands

4. **ExecStopPost restore** (`packaging/control-ofc-restore-auto.sh`):
   - Restores `pwm_enable=2` (auto) on ANY service stop (including SIGKILL)
   - Resets GPU fan curves to automatic
   - Re-enables `fan_zero_rpm_enable=1` for every GPU exposing it (DEC-100 — closes the SIGKILL/OOM path the panic hook can't cover)

5. **Kernel-version regression catalogue** (`hwmon/kernel_warnings.rs`, DEC-098):
   - Curated list of published amdgpu regressions keyed by kernel version + GPU PCI device ID
   - Currently flags `rdna_hang_kernel_6_18_6_19` (RDNA3/4 hard hang on **both** 6.18.x and 6.19.x, Phoronix-confirmed) and `smu_mismatch_navi48_r9700` (R9700-only SMU interface-version mismatch — no working fan-control path — across all current kernels, ROCm Issue #6101); see DEC-114 for the correctness fix
   - Surfaced via `GET /capabilities` (`devices.amd_gpu.kernel_warnings`); each entry carries `id` (stable knowledge-base key), `severity` (`info` / `medium` / `high` / `critical`), and `message` (pre-formatted user-visible text). The daemon owns the wording so a message update doesn't require coordinated GUI redeploys.
   - The field uses `#[serde(skip_serializing_if = "Vec::is_empty")]` so older clients that don't know about it see no change in the wire shape
   - The GUI raises a one-time `QMessageBox` for `high` and `critical` warnings; the user's acknowledgement is persisted in `app_settings.acknowledged_kernel_warnings` so the popup does not re-fire on every reconnect
   - Adding a new regression entry is a 30-line PR against `kernel_warnings.rs`; no schema or contract change required

6. **Pump-stop guard** (`profile.rs`, DEC-167): a control with a pump/CPU member
   may not be configured to stop. A non-zero `stop_pct` on such a control is
   rejected at profile-validate time (a `PUMP_STOP_FORBIDDEN` error in the
   validation report → `400 validation_error`); for any profile that reaches the
   engine un-validated (boot-load / hand-edit), the eval-time stop-snap is skipped
   for pump/CPU members. Stopping a pump risks coolant-flow loss and rapid thermal
   runaway. GPU- and chassis-only controls are unaffected.

7. **AIO / coolant surface, no coolant safety rule** (`hwmon/aio.rs`, DEC-156):
   liquid-cooler coolant temperatures are classified as the `CoolantTemp` sensor
   kind and AIO PWM headers carry an `is_aio` flag (surfaced via the dynamic
   `aio_hwmon` capability). This is detection only — there is **deliberately no
   coolant thermal-override rule**; the CPU-only `ThermalSafetyRule` is the sole
   emergency backstop. Scope is hwmon-only (USB-only coolers are out of scope).

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
| GET | `/sensors` | All temperature readings (each entry optionally carries a curated hwmon `thresholds` object — DEC-117) |
| GET | `/fans` | Fan RPM + last commanded PWM |
| GET | `/poll` | Batch: status + sensors + fans |
| GET | `/sensors/history` | Per-entity time-series (ring buffer) |
| GET | `/events` | Server-Sent Events stream (`event: update`, 5s heartbeat) |
| GET | `/capabilities` | Device list, feature flags, limits, `amd_gpu.kernel_warnings` (kernel-version regression catalogue, DEC-098) |
| GET | `/hwmon/headers` | Controllable motherboard PWM outputs |
| GET | `/profiles`, `/profiles/{id}` | Daemon-stored profiles (store of record — DEC-160) |
| GET | `/profile/active` | Current active profile or `{"active": false}` |
| GET | `/diagnostics/hardware` | Hardware readiness report (hwmon chips, GPU, thermal safety, kernel modules, ACPI conflicts, board info) |

As of 2.0.0 the profile engine is the **sole writer** (DEC-159/DEC-165); the GUI sends intent (activate / override / identify) and a few diagnostics calls — there is no bare PWM write surface.

### Write endpoints — fans

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/fans/openfan/{channel}/calibrate` | PWM→RPM sweep (long-running, thermal-aborting) |
| POST | `/fans/{fan_id}/identify` | Per-fan stop/restore for identification — floor-exempt, deadman auto-restore (DEC-166) |

### Write endpoints — GPU

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/gpu/{gpu_id}/fan/reset` | Restore GPU fan to automatic / re-enable zero-RPM |
| POST | `/gpu/{gpu_id}/fan/verify` | Test GPU fan-control effectiveness (~6s, no lease; detects ppfeaturemask/SMU/BIOS silent failures) |

### Write endpoints — hwmon

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/hwmon/{header_id}/verify` | Test PWM write effectiveness (~6s; daemon uses its own internal lease, detects BIOS/EC interference) |
| POST | `/hwmon/rescan` | Re-enumerate hwmon devices and return fresh header list |

### Write endpoints — profile / control / config

| Method | Path | Purpose |
|--------|------|---------|
| POST/PUT/DELETE | `/profiles`, `/profiles/{id}` | Profile CRUD + `?validate_only` — daemon is the store of record (DEC-160) |
| POST | `/profile/activate` | Switch active profile by id or path |
| POST | `/profile/deactivate` | Clear active profile (DEC-097); idempotent |
| POST | `/control/{control_id}/override` (+`/override/renew`, `DELETE`) | Expiring manual override — floor-clamped, deadman, monotonic fencing (DEC-163) |
| POST | `/config/profile-search-dirs` | Additively register profile search directories (persists to `runtime.toml`; 503 `persistence_failed` on write error) |
| POST | `/config/startup-delay` | Set startup delay seconds (persists to `runtime.toml`, takes effect on restart; 503 `persistence_failed` on write error) |

**Retired at 2.0.0 (DEC-165):** bare PWM writes (`/fans/openfan/{ch}/pwm`, `/fans/openfan/pwm`, `/hwmon/{id}/pwm`, `/gpu/{id}/fan/pwm`), `/fans/openfan/{ch}/target_rpm`, and all `/hwmon/lease/*`.

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

Codes:
- `validation_error` (400, source: validation) — bad input shape, or unknown resource on a known route
- `feature_unavailable` (400, source: validation) — route + device exist, but the device lacks this capability (e.g. GPU fan write with neither PMFW `fan_curve` nor legacy `pwm1`)
- `not_found` (404, source: validation) — unknown route only
- `override_expired` (404, source: validation) — renew/release of a lapsed manual override (DEC-163); re-take
- `already_exists` (409, source: validation) — `POST /profiles` with a duplicate id (DEC-160)
- `profile_in_use` (409, source: validation) — `DELETE /profiles/{id}` of the active profile (DEC-160)
- `stale_fencing_token` (409, source: validation) — override renew/release bearing a superseded `override_token` (DEC-163)
- `thermal_abort` (409, source: hardware) — calibration aborted due to high temperature
- `internal_error` (500, source: internal)
- `hardware_unavailable` (503, source: hardware)
- `persistence_failed` (503, source: internal) — `POST /config/*` could not persist `runtime.toml`
- `too_many_clients` (503, source: internal) — SSE `GET /events` concurrent-client cap reached

The client-lease codes `lease_required` / `lease_already_held` were retired (DEC-165)
and fully removed at DEC-170 — a verify-path internal-lease lapse now returns
`503 hardware_unavailable`.
