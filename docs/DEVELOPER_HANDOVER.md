# Developer Handover

## Project overview

Control-OFC is a fan control system for Linux desktops, consisting of:
- **Rust daemon** (`daemon/`) — hardware communication, safety logic, IPC server
- **Python GUI** (`control-ofc-gui` repo) — PySide6 fan curve editor and monitor

The daemon owns all hardware access and exposes a stable HTTP-over-Unix-socket API.

## Repository layout

```
daemon/                     Rust crate (control-ofc-daemon)
  src/
    main.rs                 Entrypoint (tokio async runtime)
    lib.rs                  Module exports
    config.rs               TOML config + validation (incl. [state] section)
    runtime_config.rs       Daemon-mutable runtime.toml (ADR-002)
    constants.rs            Centralized operational tuning values
    pwm.rs                  Shared PWM percent ↔ raw (0–255) conversion
    clock.rs                Injectable monotonic clock (deterministic TTL/expiry in tests)
    atomic_io.rs            Crash-safe atomic file write (tmp+fsync+rename)
    control_override.rs     Manual-override + fan-identify state (expiring, fencing-guarded; DEC-163/166)
    daemon_state.rs         Persistent state (configurable state_dir via OnceLock)
    error.rs                Structured error types
    api/
      handlers/             HTTP request handlers (split by concern)
        mod.rs              AppState, shared helpers, submodule re-exports
        status.rs           Read endpoints (status, sensors, fans, poll, capabilities)
        openfan.rs          OpenFan serial write + calibration handlers
        gpu.rs              AMD GPU fan set/reset handlers
        hwmon_ctl.rs        Hwmon header list, rescan, PWM-verify handlers
        profile.rs          Profile activation + CRUD handlers
        control.rs          Manual-override + fan-identify handlers (DEC-163/166)
        config.rs           Runtime config handlers
        hw_diagnostics.rs   Hardware diagnostics handler
      responses.rs          JSON response/request types (v1 schema)
      server.rs             Unix socket server lifecycle
      calibration.rs        OpenFan calibration sweep
    health/
      state.rs              Canonical state model (DaemonState)
      cache.rs              RwLock in-memory cache
      staleness.rs          Health computation (OK/Warn/Crit)
      history.rs            Per-entity time-series ring buffer
      sensor_failure.rs     SensorFailureTracker — quarantines present-but-unreadable sensors (DEC-193)
    hwmon/
      discovery.rs          hwmon sysfs sensor discovery
      reader.rs             hwmon temp reads
      types.rs              SensorKind, SensorReading, SensorDescriptor
      pwm_discovery.rs      PWM header discovery with stable IDs
      pwm_control.rs        PWM writes with lease enforcement (daemon-internal since 2.0.0)
      lease.rs              Exclusive write lease (take/release/renew, 60s TTL) — **internal-only since 2.0.0**: the profile engine self-leases; there is no client `/hwmon/lease/*` route (DEC-165)
      aio.rs                Liquid-cooler (AIO) recognition: coolant sensor + is_aio + aio_hwmon cap (DEC-156)
      gpu_detect.rs         AMD GPU detection via sysfs/DRM
      gpu_fan.rs            PMFW fan curve read/write/reset (RDNA3+)
      kernel_warnings.rs    Kernel-version regression catalog (DEC-098).
                            Matches running kernel against published amdgpu
                            regressions; surfaced via
                            /capabilities.amd_gpu.kernel_warnings.
      util.rs               Shared sysfs path helpers
    serial/
      protocol.rs           OpenFanController protocol encode/decode
      transport.rs          Serial transport trait
      real_transport.rs     serialport impl + auto-detect
      controller.rs         Fan control logic (per-channel PWM writes, coalescing)
    profile.rs              Profile JSON loading + curve evaluation
    profile_store.rs        Daemon-owned profile storage (store of record, DEC-160)
    profile_engine/         Headless 1Hz curve evaluation loop (DEC-135)
      mod.rs                  Safety tick + profile evaluation + loop body
      backends.rs             WriteBackend impls (OpenFan/GPU/hwmon gating)
    safety.rs               ThermalSafetyRule (CPU emergency override)
    polling.rs              hwmon + OpenFan polling loops
  tests/
    ipc_integration.rs      Integration tests over the UDS HTTP server
docs/
  ADRs/                     Architecture decision records
packaging/
  control-ofc-daemon.service   systemd unit file
  modules-load.d/control-ofc.conf  Super I/O module loading at boot
```

## Build and test

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all --all-features
cargo build --release
```

## Running the daemon

```bash
# Default config location (optional — daemon uses defaults if missing)
sudo mkdir -p /etc/control-ofc
sudo cp daemon.toml.example /etc/control-ofc/daemon.toml

# Run directly (default config path: /etc/control-ofc/daemon.toml)
RUST_LOG=info cargo run

# Override config path via CLI or env var
cargo run -- --config ./dev-config.toml
CONTROL_OFC_CONFIG=./dev-config.toml cargo run

# Or install and run via systemd
sudo cp target/release/control-ofc-daemon /usr/local/bin/
sudo cp packaging/control-ofc-daemon.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now control-ofc-daemon
```

## IPC socket

- Default path: `/run/control-ofc/control-ofc.sock`
- Configurable via `[ipc] socket_path` in TOML config
- The daemon creates the parent directory and cleans up stale sockets on start
- GUI discovers the socket via config or the default path

## API endpoints (v1)

### Read-only
| Endpoint | Description |
|---|---|
| `GET /capabilities` | Device capabilities, feature flags, safety limits, `amd_gpu.kernel_warnings` (DEC-098) |
| `GET /status` | Health status + subsystem freshness |
| `GET /sensors` | Cached temperature readings |
| `GET /fans` | Fan RPM + last commanded PWM |
| `GET /poll` | Batch: status + sensors + fans in one call |
| `GET /sensors/history` | Per-entity time-series history |
| `GET /hwmon/headers` | Discovered controllable PWM headers |
| `GET /profiles`, `GET /profiles/{id}` | Daemon-stored profiles (store of record, DEC-160) |
| `GET /profile/active` | Currently active profile info |
| `GET /diagnostics/hardware` | Hardware readiness: hwmon chips, GPU detection, thermal-safety state, kernel modules, ACPI conflicts, board info, kernel warnings |

### Write
The profile engine is the **sole writer** as of 2.0.0 (DEC-159/DEC-165); the GUI sends intent + diagnostics calls. Bare PWM/lease endpoints were retired (note below).

| Endpoint | Description |
|---|---|
| `POST /profiles`, `PUT`/`DELETE /profiles/{id}` | Profile CRUD + `?validate_only` — daemon is the store of record (DEC-160) |
| `POST /profile/activate` | Switch active profile at runtime |
| `POST /profile/deactivate` | Clear active profile (DEC-097); idempotent |
| `POST /control/{control_id}/override` (+ `/override/renew`, `DELETE`) | Expiring manual override — floor-clamped, deadman, monotonic fencing (DEC-163) |
| `POST /fans/{fan_id}/identify` | Per-fan stop/restore — floor-exempt, deadman auto-restore (DEC-166) |
| `POST /fans/openfan/{ch}/calibrate` | Run a PWM-to-RPM calibration sweep |
| `POST /hwmon/{header_id}/verify` | Behavioural test of PWM write effectiveness (~6 s; daemon's own internal lease); returns `restore_failed: bool` per DEC-100 |
| `POST /gpu/{gpu_id}/fan/verify` | Test GPU fan-control effectiveness (~6 s, no lease) |
| `POST /gpu/{gpu_id}/fan/reset` | Reset GPU fan to automatic |
| `POST /hwmon/rescan` | Re-enumerate hwmon devices |
| `POST /config/profile-search-dirs` | Register additional profile search dirs (persists to `runtime.toml`) |
| `POST /config/startup-delay` | Set startup delay seconds (persists to `runtime.toml`) |

**Retired at 2.0.0 (DEC-165):** bare PWM writes (`/fans/openfan/{ch}/pwm`, `/fans/openfan/pwm`, `/hwmon/{id}/pwm`, `/gpu/{id}/fan/pwm`), `/fans/openfan/{ch}/target_rpm`, and all `/hwmon/lease/*`.

## Identity contract

Every sensor/fan/header includes:
- `id` — stable machine key (never depends on `hwmonN` index or `/dev/sdX`)
- `label` — best-effort human name
- `source` — fan `source` is `openfan` | `hwmon` | `amd_gpu` | `intel_gpu` (the four `KNOWN_MEMBER_SOURCES`); GPU fan ids embed the PCI BDF (`amd_gpu:{bdf}` / `intel_gpu:{bdf}`). Sensor `source` is `hwmon` | `amd_gpu`. (`aio_hwmon` is an *internal* `DeviceLabel` classification, not a wire fan source — AIO pump fans surface as `hwmon`.)
- `kind`/`type` where applicable

## Measured vs commanded

- `rpm` — measured from hardware (OpenFanController serial reads, hwmon `fanN_input`)
- `last_commanded_pwm` — daemon-tracked (firmware does not report PWM state)
- These are always separate fields, never ambiguous

## Safety invariants

- **Thermal safety** (`safety.rs`): hottest CpuTemp sensor triggers at 105°C → force all OpenFan channels and writable hwmon headers to 100%. Hold until 80°C (hysteresis), then 60% for two cycles (the release cycle + a one-cycle recovery floor). Forces 40% if no CpuTemp sensor found for 5 consecutive cycles. GPU fans are excluded by design (DEC-130) — PMFW firmware owns GPU thermal protection; the exclusion is structural (`GpuBackend` does not implement `SafetyWriteBackend`).
- **AIO / coolant** (`hwmon/aio.rs`, DEC-156): coolant temperatures are classified as the `CoolantTemp` sensor kind and AIO PWM headers are flagged `is_aio` (dynamic `aio_hwmon` capability). Detection only — there is **deliberately no coolant thermal-override rule**; the CPU-only `ThermalSafetyRule` is the sole emergency backstop.
- **OpenFan stop timeout**: 0% PWM allowed for max 8s, then rejected
- **hwmon PWM**: no daemon-enforced per-header floors (`min_pwm_percent: 0` for all). The role-aware pump/CPU floor is GUI-baked and **daemon-enforced** (validate-time reject + eval-time clamp, DEC-162); the 105 °C thermal force is the absolute backstop.
- **Pump-stop guard** (`profile.rs`, DEC-167): a control with a pump/CPU member may not be set to stop — a non-zero `stop_pct` is rejected at profile-validate time (`PUMP_STOP_FORBIDDEN` → `400 validation_error`), and the eval-time stop-snap is skipped for pump/CPU members on any un-validated profile. Distinct from the DEC-162 *floor* above: this forbids *stopping*, not merely clamps the minimum.
- **PWM enable mode** (`pwmN_enable=1`) set on first write per lease, reset on release
- **ExecStopPost**: restores `pwm_enable=2` (auto) and resets GPU fan curves on any service stop
- **GPU PMFW writes**: clamped to OD_RANGE from firmware PPTable (prevents EINVAL)

## Key design decisions

- ADR-001: IPC transport — HTTP over Unix domain socket (axum + tokio)
- Lease model: single exclusive lease for hwmon writes (60s TTL, renewable)
- Schema: additive-only within v1, stable keys and enums

## Test counts

The suite is comprehensive and grows release-by-release; no test
requires real hardware (everything is mocked or driven against tempdirs).
For the current count consult the most recent `CHANGELOG.md` entry —
release notes record the exact `cargo test` totals for the matching
daemon version. Run `cargo test --all-targets --all-features` to see
the live count locally.
