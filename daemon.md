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
    inventory.rs       — structured read-only hwmon inventory: temps + PWM headers + monitor-only tachometers (DEC-200)
    classify.rs        — refines each temp sensor's CPU/motherboard classification for the inventory (DEC-200)
    readiness.rs       — turns the inventory into an actionable hardware-readiness list (DEC-200)
    pwm_discovery.rs   — PWM header discovery (fan outputs)
    pwm_control.rs     — HwmonPwmController + SysfsWriter trait
    lease.rs           — LeaseManager (exclusive write access)
    aio.rs             — liquid-cooler (AIO/custom-loop) recognition: coolant-sensor + is_aio flag + aio_hwmon cap (DEC-156)
    gpu_detect.rs      — AMD GPU detection via sysfs/DRM
    intel_gpu_detect.rs— Intel discrete GPU (Arc) detection, read-only (DEC-121)
    nouveau_detect.rs  — NVIDIA discrete GPU detection via the open nouveau driver, read-only (DEC-204)
    nvidia.rs          — unified NVIDIA GPU identity (nouveau + NVML) for /capabilities + /diagnostics (DEC-204)
    nvml.rs            — opt-in read-only NVIDIA telemetry backend (trait + Real/Fake/Disabled), proprietary driver (DEC-204)
    nvml_sys.rs        — isolated unsafe FFI to libnvidia-ml.so.1 via libloading (the only NVIDIA unsafe, DEC-204)
    gpu_fan.rs         — PMFW fan curve read/write/reset
    kernel_warnings.rs — kernel-version regression catalog
                          (RDNA3/4 hard hang on 6.18.x + 6.19.x; R9700/Navi48
                          0x7551 SMU mismatch — device-scoped, not kernel-tied)
                          surfaced via /capabilities.amd_gpu.kernel_warnings
    superio.rs         — passive Super-I/O chip detection (DMI + hwmon + /proc/modules + kmsg + ACPI evidence, DEC-202)
    superio_probe.rs   — opt-in active /dev/port Super-I/O probe, off by default (DEC-203)
    chip_db.rs         — Super-I/O chip → expected-driver knowledge base (DEC-202)
    util.rs            — shared sysfs path helpers

  health/
    mod.rs             — health subsystem re-exports
    cache.rs           — StateCache (RwLock snapshot-clone)
    state.rs           — CachedSensorReading, CachedFanReading types
    staleness.rs       — Freshness enum + age thresholds
    history.rs         — HistoryRing (per-entity time-series)
    sensor_failure.rs  — SensorFailureTracker: quarantines present-but-unreadable sensors (DEC-193)

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
      inventory.rs     — /inventory/{hwmon,readiness,superio,hardware-readiness} reads + Super-I/O probe; shared assessment snapshot + coalesced scan (DEC-200/202/203/207)
      assessment.rs    — hardware-assessment cache + single-flight coordinator (DEC-207)
      path_confine.rs  — SO_PEERCRED search-dir confinement predicate (DEC-205)
    responses.rs       — response structs (Serialize)
    calibration.rs     — OpenFan calibration sweep
    diagnostics.rs     — hardware-diagnostics scanning logic behind /diagnostics/hardware

  pwm.rs               — shared percent_to_raw / raw_to_percent conversion
  clock.rs             — injectable monotonic clock (lease/override/identify TTLs; deterministic in tests)
  atomic_io.rs         — crash-safe atomic file write (tmp+fsync+rename)
  profile.rs           — profile JSON loading + curve evaluation
  profile_store.rs     — daemon-owned profile storage (store of record, DEC-160)
  profile_engine/      — headless 1Hz curve evaluation loop (DEC-135)
    mod.rs             — loop body / coordinator: orchestrates safety_tick + curve_eval + tuning + backends
    curve_eval.rs      — deadband + trigger latch + Mix/Sync composites (topological order)
    tuning.rs          — offset→floor→step-rate→stop-snap→start-kick→clamp + floor policy
    safety_tick.rs     — 105/80/60 °C thermal ladder + no-sensor fallback (DEC-190)
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
[serial USB]  ──read──>
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
   - If no CpuTemp sensor is found — or none is still updating (DEC-267: a
     reading older than 5 poll intervals counts as absent) — for 5 consecutive
     cycles, forces all
     OpenFan+hwmon fans to 40%; a sensor that *vanishes* while an emergency is
     latched forces 40% immediately (from the first missing cycle) and reports
     `no_sensor_fallback` rather than dropping to profile control (DEC-190),
     whereas one that merely goes *stale* holds the emergency's own 100% output
     — losing sight of a sensor must never lower an already-forced safety
     output (DEC-269)
   - Override state is surfaced as `thermal_state` in `GET /status`
     (`normal` | `recovery` | `emergency` | `no_sensor_fallback`, DEC-132)
     so the GUI shows a poll-driven thermal banner (DEC-165 — there is no GUI
     loop to stand down; the daemon owns control)

2. **Curve sensor freshness** (`profile_engine::curve_eligible`, DEC-272)
   - The rule above is CPU-only. Every *other* sensor driving a fan curve — GPU
     edge, coolant, VRM, drive — is age-filtered before curve evaluation: a
     reading older than the same freshness budget stops driving its curve, so a
     frozen GPU or coolant sensor can no longer command a fan forever while
     `thermal_state` reports `normal`
   - A filtered-out sensor makes its curve unresolvable. For a single-sensor
     curve the control is SKIPPED and its fans hold at their last commanded duty
     — never 0%, and never a lower value
   - A Mix curve combines whatever inputs it still has and is then forbidden to
     COMMAND LESS than it last did, until every input is back. Both halves are
     needed and neither alone is right: recombining the survivors on its own
     lowers the duty when the lost input was the hot one (measured 100% -> 36% in
     one tick), and skipping the control on its own freezes the fan when a
     SURVIVING input is hot and rising — including a fresh CPU reading, because
     `CpuTemp` is exempt from the filter and a Mix has one fan set, not one per
     input. A Mix whose inputs never resolve at all still holds
   - Consequently a Mix naming a sensor this machine does not have still drives
     its fans from the inputs that do exist, rather than going silent
   - `CpuTemp` is deliberately EXEMPT. The thermal ladder above is the sole
     authority on a stale CPU reading and has already adjudicated both halves;
     filtering it here would freeze a control mid-ramp instead of letting it keep
     climbing toward a hot target
   - Readings for sensors that have genuinely VANISHED (driver unloaded, device
     removed) are evicted from the cache rather than ageing in it forever, which
     is what makes the "no CpuTemp sensor" branch above reachable at all
   - "Could not read" is not "gone", and the distinction is drawn PER CHIP: a
     scan that cannot read one chip protects that chip's cached readings and goes
     on evicting every other chip's. Suspending eviction wholesale would be worse
     than it sounds — a chip contributing no descriptors can never produce a read
     failure and so never re-triggers a scan, so a single unreadable chip could
     switch eviction off for the rest of the process. A chip whose sysfs
     directory has gone is removed, not unreadable, and still evicts at once
   - The same rule applies one level down: a `tempN_label` that exists but will
     not read fails its whole chip for that scan rather than defaulting to an
     empty label, because the label feeds both the sensor's stable id and its
     CPU/motherboard classification

3. **Lease system** (`lease.rs`): Exclusive hwmon write access
   - 60s TTL, holder must renew periodically
   - A daemon-internal single-writer token (`HwmonWriter::{Engine,Verify,ThermalSafety}`,
     DEC-197) arbitrating the three in-process writers — the profile-engine tick, a hardware
     verify, and the thermal-safety force. Not a client lease: the GUI holds nothing (DEC-165).
     A thermal force-take evicts a verify mid-scan, so the verify's stale token is refused.

4. **Stop timeout** (`controller.rs`): OpenFan 0% wire-write limit
   - Rejects a *wire-bound* 0% write against a stop timer older than 8 s.
     A steady 0% hold coalesces — same-value repeats never reach the wire or
     the timeout (CONC-2, 2026-07-21 audit; the old order errored every tick
     past 8 s, inflating failure streaks) — so this is defence-in-depth
     against channel-tracking drift, not a periodic re-arm requirement

5. **ExecStopPost restore** (`packaging/control-ofc-restore-auto.sh`):
   - Restores `pwm_enable=2` (auto) on ANY service stop (including SIGKILL)
   - Resets GPU fan curves to automatic
   - Re-enables `fan_zero_rpm_enable=1` for every GPU exposing it (DEC-100 — closes the SIGKILL/OOM path the panic hook can't cover)

6. **Kernel-version regression catalogue** (`hwmon/kernel_warnings.rs`, DEC-098):
   - Curated list of published amdgpu regressions keyed by kernel version + GPU PCI device ID
   - Currently flags `rdna_hang_kernel_6_18_6_19` (RDNA3/4 hard hang on **both** 6.18.x and 6.19.x, Phoronix-confirmed) and `smu_mismatch_navi48_r9700` (R9700-only SMU interface-version mismatch — no working fan-control path — across all current kernels, ROCm Issue #6101); see DEC-114 for the correctness fix
   - Surfaced via `GET /capabilities` (`devices.amd_gpu.kernel_warnings`); each entry carries `id` (stable knowledge-base key), `severity` (`info` / `medium` / `high` / `critical`), and `message` (pre-formatted user-visible text). The daemon owns the wording so a message update doesn't require coordinated GUI redeploys.
   - The field uses `#[serde(skip_serializing_if = "Vec::is_empty")]` so older clients that don't know about it see no change in the wire shape
   - The GUI raises a one-time `QMessageBox` for `high` and `critical` warnings; the user's acknowledgement is persisted in `app_settings.acknowledged_kernel_warnings` so the popup does not re-fire on every reconnect
   - Adding a new regression entry is a 30-line PR against `kernel_warnings.rs`; no schema or contract change required

7. **Pump-stop guard** (`profile.rs`, DEC-167): a control with a pump/CPU member
   may not be configured to stop. A non-zero `stop_pct` on such a control is
   rejected at profile-validate time (a `PUMP_STOP_FORBIDDEN` error in the
   validation report → `400 validation_error`); for any profile that reaches the
   engine un-validated (boot-load / hand-edit), the eval-time stop-snap is skipped
   for pump/CPU members. Stopping a pump risks coolant-flow loss and rapid thermal
   runaway. GPU- and chassis-only controls are unaffected.

8. **AIO / coolant surface, no coolant safety rule** (`hwmon/aio.rs`, DEC-156):
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
  runtime: `[profiles] search_dirs`, `[startup] delay_secs`, and
  `[hardware] preferred_cpu_sensor` / `preferred_mb_sensor` (DEC-200). Written
  with 0600 permissions via atomic tmp+rename.

On startup the daemon loads `daemon.toml`, then overlays `runtime.toml` on
top; runtime values win. SIGHUP re-reads both and re-applies the overlay.

Other paths:

- **Profile loading**: `--profile <name>` | `--profile-file <path>` | `$OPENFAN_PROFILE` | persisted state
- **Socket**: `/run/control-ofc/control-ofc.sock` (configurable via `ipc.socket_path`)
- **Persisted state**: `/var/lib/control-ofc/daemon_state.json` (configurable via `state.state_dir`)

### `daemon.toml` vs `runtime.toml` (the runtime overlay)

`daemon.toml`'s `[profiles]` and `[startup]` sections remain **valid admin
defaults** — the base layer. `config.rs` still parses them (see the
`parse_profiles_section` / `parse_startup_delay_section` tests); they are not
deprecated and never become a parse error. `runtime.toml` is written **only**
when an API call mutates a runtime-mutable key
(any `POST /config/*` route); when it
exists, its keys **overlay** the `daemon.toml` defaults (runtime wins — see the
overlay note above). There is no copy and no one-time migration: the two files
coexist, and if `runtime.toml` shadows a non-default `daemon.toml` key the daemon
surfaces it only via an `info` log at startup (`main.rs::apply_runtime_overlay`).


**DEC-243 widened the overlay.** `runtime.toml` now also carries `[serial]`
(`port`, `timeout_ms`), `[polling]` (`poll_interval_ms`) and `[detection]`
(`allow_port_probe`, `enable_nvidia_telemetry`). Two consequences worth knowing:

- **The top-level `RuntimeConfig` struct deliberately does *not* use
  `deny_unknown_fields`.** `load_from` treats any parse error as "malformed ->
  defaults", so denying unknown *sections* would make an older daemon reading a
  newer `runtime.toml` silently discard **every** runtime setting — and the next
  write would make that loss permanent. Unknown sections are skipped; each
  section keeps `deny_unknown_fields`, so a typo inside a known section still
  fails loudly.
- **Only `profiles.search_dirs` is re-applied live** (by its own POST handler and on SIGHUP) — so `GET /config` reports it `requires_restart: false` and reads its running value from the live lock, not the startup snapshot. Everything else
  is consumed once at process start, so the setters report "takes effect on next
  daemon restart" and `GET /config` exposes `restart_pending` per key by
  comparing the on-disk effective value against `AppState::running_config`.

`ipc.socket_path` and `state.state_dir` are **not** runtime-mutable: a bad socket
path locks every client out of the daemon, and moving the state dir orphans
`runtime.toml` and the profile store. `GET /config` reports them with
`mutable: false`.

## API Endpoints

Full route table (source of truth: `daemon/src/api/server.rs`).

### Read endpoints

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/status` | Subsystem health + freshness; `thermal_state`; `unavailable_sensors[]` (present-but-unreadable sensors, DEC-193); `active_profile_id`/`active_profile_name` (active profile, DEC-194); `readiness` (compact cached hardware-readiness rollup for the GUI Dashboard chip — `{overall, critical, warning, info, top_summary, top_code}`, DEC-206) |
| GET | `/sensors` | All temperature readings (each entry optionally carries a curated hwmon `thresholds` object — DEC-117; each also carries `control_eligible: bool` — DEC-193) |
| GET | `/fans` | Fan RPM + last commanded PWM (+ `stall_detected`) |
| GET | `/poll` | Batch: status (incl. `unavailable_sensors[]`, `active_profile_*`, `readiness` rollup) + sensors (incl. `control_eligible`) + fans |
| GET | `/sensors/history` | Per-entity time-series (ring buffer) |
| GET | `/capabilities` | Device list, feature flags, limits, `amd_gpu.kernel_warnings` (kernel-version regression catalogue, DEC-098) |
| GET | `/config` | Effective merged configuration (DEC-243): per key its on-disk `value`, the `running_value` this process started with, `source` (`runtime`/`admin`/`default`), `mutable`, `requires_restart`, `restart_pending`, and `requires_privilege` where a drop-in is also needed. `/capabilities` carries no configuration at all — this is the only read side |
| GET | `/hwmon/headers` | Controllable motherboard PWM outputs |
| GET | `/profiles`, `/profiles/{id}` | Daemon-stored profiles (store of record — DEC-160) |
| GET | `/profile/active` | Current active profile or `{"active": false}` |
| GET | `/diagnostics/hardware` | Hardware readiness report (hwmon chips, GPU, thermal safety, kernel modules, ACPI conflicts, board info) |
| GET | `/inventory/hwmon` | Read-only structured inventory: temp sensors (each with a fine `classification`/`confidence`/`rationale` + an advisory `default_cpu`), controllable PWM headers, and monitor-only fan tachometers (`fanN_input` with no matching `pwmN`) |
| GET | `/inventory/readiness` | Structured hardware-readiness list (`items[]` with code/severity/component/action + blocks-flags; `overall` rollup). Read-only diagnose-and-guide |
| GET | `/inventory/superio` | Passive Super-I/O chip detection report — DMI/hwmon/`/proc/modules`/kmsg/ACPI evidence → per-chip presence + allowlisted driver recommendations; `port_probe_available` flags the opt-in active probe. Read-only, never touches an I/O port (DEC-202) |
| GET | `/inventory/hardware-readiness` | Combined readiness + Super-I/O snapshot from ONE shared passive scan (DEC-207): the readiness `rollup`/`overall`/`items`, the `superio` report, `scanned_age_ms`, and a monotonic `generation`. The GUI's merged "Cooling Hardware Readiness" page fetches this in a single request; `?refresh=true` forces a fresh (coalesced) scan. Read-only, 404-gated |

As of 2.0.0 the profile engine is the **sole writer** (DEC-159/DEC-165); the GUI sends intent (activate / override / identify) and a few diagnostics calls — there is no bare PWM write surface.

**Sensor quarantine (DEC-193, additive):** a sensor that is discovered but fails
every read (canonically an `ath12k`/`iwlwifi` WiFi temperature returning
`ENETDOWN` while the radio is off) is logged once, then evicted from `sensors`
and surfaced on `/status` + `/poll` as `unavailable_sensors[] = {id, label,
reason, unavailable_for_ms}`. Each live `sensors` entry also carries
`control_eligible: bool` (derived from `is_wireless_phy_chip(chip_name)`). Both
fields are additive — older clients ignore them; the GUI defaults
`control_eligible = true` and `unavailable_sensors = []` when absent.

**Read-only hwmon discovery + readiness (DEC-200, additive, GUI-facing):** `GET
/inventory/hwmon` returns a structured, read-only snapshot — temperature sensors
(each with a fine `classification`/`confidence`/`rationale` that *refines* `kind`,
plus a deterministic advisory `default_cpu`), controllable PWM headers, and
monitor-only fan tachometers (`fanN_input` with no matching `pwmN`). `GET
/inventory/readiness` turns that snapshot into an actionable readiness list
(per-item `severity` + recommended action + `blocks_*`/`affects_safety`/
`reboot_may_be_required` flags, and an `overall` rollup). Both are **read-only —
discovery never writes hardware**; the classification is advisory (thermal safety
still keys off `kind`), and GPU fan control stays out of scope (owned by the GPU
subsystem — DEC-102 / DEC-130).

### Write endpoints — fans

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/fans/openfan/{channel}/calibrate` | PWM→RPM sweep (long-running, thermal-aborting; pauses the engine write phase for the sweep so an active profile cannot corrupt the readback — DEC-191) |
| POST | `/fans/{fan_id}/identify` | Per-fan stop/restore for identification — floor-exempt, deadman auto-restore (DEC-166) |

The floor-exempt identify `stop` on the world-writable socket (0666, DEC-049) lets any local
user hold any fan — including a pump-class header — stopped by re-issuing `stop` inside the
deadman window. **Accepted, bounded risk** (2026-07-21 audit: accept + document): identification
requires stopping any fan by design (DEC-166); the deadman auto-restore limits an abandoned stop
to one TTL; and a thermal emergency outranks the identify overlay entirely — the engine's
`force_all` path (105 °C emergency, and the no-sensor 40 % fallback) drives every OpenFan +
writable hwmon header directly, spinning a stalled pump back up regardless of standing stops.

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
| POST | `/fans/openfan/rescan` | Look for an OpenFanController and adopt it without a restart (DEC-265) |

### Write endpoints — inventory (opt-in active probe)

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/inventory/superio/probe` | Opt-in active Super-I/O `/dev/port` probe (DEC-203) — a deliberate one-shot that identifies an UNBOUND chip so the user can be told which driver to load. Refuses unless `[detection] allow_port_probe` + `CAP_SYS_RAWIO`; skips ports claimed by a driver/ACPI; single-flight + 10 s cooldown. Returns the `/inventory/superio` shape enriched with probe hits |

### Write endpoints — profile / control / config

| Method | Path | Purpose |
|--------|------|---------|
| POST/PUT/DELETE | `/profiles`, `/profiles/{id}` | Profile CRUD + `?validate_only` — daemon is the store of record (DEC-160) |
| POST | `/profile/activate` | Switch active profile by id or path; clears all active control-overrides, not identify-stops (DEC-189) |
| POST | `/profile/deactivate` | Clear active profile (DEC-097); also clears all active control-overrides, not identify-stops (DEC-218, ≥ 2.12.0); idempotent |
| POST | `/control/{control_id}/override` (+`/override/renew`, `DELETE`) | Expiring manual override — floor-clamped, deadman, monotonic fencing (DEC-163); cleared on profile activation/deactivation (DEC-189/DEC-218) |
| POST | `/config/profile-search-dirs` | Additively register profile search directories (persists to `runtime.toml`; 503 `persistence_failed` on write error) |
| POST | `/config/poll-interval` | Set the sensor/fan poll interval, 250-2000 ms (DEC-243; persists to `runtime.toml`, restart to apply). **[SAFETY]** the ceiling bounds how stale a temperature the 105 C rule can act on |
| POST | `/config/serial-port` | Set the OpenFan serial device (`null` = auto-detect). Validated against the transport's own allowlist and capped at 256 chars; a configured port that fails to open **or fails to answer the `ReadAllRpm` handshake** falls back to auto-detection, so neither a bad value nor a wrong-but-openable device can remove OpenFan control. DEC-243 / DEC-250; restart to apply |
| POST | `/config/serial-timeout` | Set the serial read timeout, 50-1000 ms (DEC-243; restart to apply). **[SAFETY]** bounds emergency `force_all` latency |
| POST | `/config/allow-port-probe` | Opt into the active Super-I/O probe (DEC-243). **Also needs the `CAP_SYS_RAWIO` drop-in** — the flag alone does not enable it |
| POST | `/config/nvidia-telemetry` | Opt into read-only NVML telemetry (DEC-243). **Also needs the `/dev/nvidia*` drop-in** |
| POST | `/config/startup-delay` | Set startup delay seconds (persists to `runtime.toml`, takes effect on restart; 503 `persistence_failed` on write error) |
| POST | `/config/preferred-cpu-sensor` | Persist the user's preferred CPU temperature sensor by stable id (`{"sensor_id":"<id>"}` sets, `null` clears; validated against the live sensor set). Advisory — reflected in `/inventory/hwmon` `default_cpu` (`source:"user"`) + `preferences` and the readiness `selected_cpu_sensor_missing` item (DEC-200) |
| POST | `/config/preferred-mb-sensor` | Persist the user's preferred case/motherboard temperature sensor (same shape) |

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

Codes (note `validation_error` is returned with **two** HTTP statuses):
- `validation_error` (**400**, source: validation) — bad input shape / payload on a known route
- `validation_error` (**404**, source: validation) — a known route naming a resource that does not exist: unknown profile id (`POST /profile/activate`, `GET`/`DELETE /profiles/{id}`), unknown control on the active profile (override take), unknown fan id (`/fans/{id}/identify`), unknown hwmon header, or unknown GPU id. The HTTP status is 404 but the envelope `code` stays `validation_error`, **not** `not_found`
- `feature_unavailable` (400, source: validation) — route + device exist, but the device lacks this capability (e.g. GPU fan write with neither PMFW `fan_curve` nor legacy `pwm1`)
- `not_found` (404, source: validation) — genuinely unknown *route* only (the catch-all fallback handler)
- `override_expired` (404, source: validation) — renew/release of a lapsed manual override (DEC-163); re-take
- `already_exists` (409, source: validation) — `POST /profiles` with a duplicate id (DEC-160)
- `profile_in_use` (409, source: validation) — `DELETE /profiles/{id}` of the active profile (DEC-160)
- `stale_fencing_token` (409, source: validation) — override renew/release bearing a superseded `override_token` (DEC-163)
- `thermal_abort` (409, source: hardware) — calibration aborted due to high temperature
- `validation_error` (409, source: validation) — `POST /fans/openfan/{ch}/calibrate` when a calibration **or** a hardware verify is already in progress; the sweep shares the verify single-flight pause (DEC-191)
- `internal_error` (500, source: internal)
- `hardware_unavailable` (503, source: hardware)
- `persistence_failed` (503, source: internal) — `POST /config/*` could not persist `runtime.toml`

The client-lease codes `lease_required` / `lease_already_held` were retired (DEC-165)
and fully removed at DEC-170 — a verify-path internal-lease lapse now returns
`503 hardware_unavailable`.
