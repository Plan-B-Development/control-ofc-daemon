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
    daemon_state.rs         Persistent state (configurable state_dir via OnceLock)
    error.rs                Structured error types
    api/
      handlers.rs           HTTP request handlers + AppState
      responses.rs          JSON response/request types (v1 schema)
      server.rs             Unix socket server lifecycle
      sse.rs                Server-Sent Events stream
      calibration.rs        OpenFan calibration sweep
    health/
      state.rs              Canonical state model (DaemonState)
      cache.rs              RwLock in-memory cache
      staleness.rs          Health computation (OK/Warn/Crit)
      history.rs            Per-entity time-series ring buffer
    hwmon/
      discovery.rs          hwmon sysfs sensor discovery
      reader.rs             hwmon temp reads
      types.rs              SensorKind, SensorReading, SensorDescriptor
      pwm_discovery.rs      PWM header discovery with stable IDs
      pwm_control.rs        PWM writes with lease enforcement
      lease.rs              Exclusive write lease (take/release/renew, 60s TTL)
      gpu_detect.rs         AMD GPU detection via sysfs/DRM
      gpu_fan.rs            PMFW fan curve read/write/reset (RDNA3+)
      util.rs               Shared sysfs path helpers
    serial/
      protocol.rs           OpenFanController protocol encode/decode
      transport.rs          Serial transport trait
      real_transport.rs     serialport impl + auto-detect
      controller.rs         Fan control logic (PWM, target RPM, coalescing)
    profile.rs              Profile JSON loading + curve evaluation
    profile_engine.rs       Headless 1Hz curve evaluation loop
    safety.rs               ThermalSafetyRule (CPU emergency override)
    polling.rs              hwmon + OpenFan polling loops
  tests/
    ipc_integration.rs      22 integration tests (UDS)
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
| `GET /capabilities` | Device capabilities, feature flags, safety limits |
| `GET /status` | Health status + subsystem freshness |
| `GET /sensors` | Cached temperature readings |
| `GET /fans` | Fan RPM + last commanded PWM |
| `GET /poll` | Batch: status + sensors + fans in one call |
| `GET /sensors/history` | Per-entity time-series history |
| `GET /events` | SSE real-time sensor/fan stream |
| `GET /hwmon/headers` | Discovered controllable PWM headers |
| `GET /hwmon/lease/status` | Lease held/TTL/owner |
| `GET /profile/active` | Currently active profile info |

### Write
| Endpoint | Description |
|---|---|
| `POST /fans/openfan/{ch}/pwm` | Set PWM on one OpenFanController channel |
| `POST /fans/openfan/pwm` | Set PWM on all channels |
| `POST /fans/openfan/{ch}/target_rpm` | Set target RPM (closed-loop) |
| `POST /fans/openfan/{ch}/calibrate` | Run a PWM-to-RPM calibration sweep |
| `POST /hwmon/lease/take` | Acquire exclusive hwmon write lease |
| `POST /hwmon/lease/release` | Release lease |
| `POST /hwmon/lease/renew` | Extend lease TTL |
| `POST /hwmon/{header_id}/pwm` | Set hwmon PWM (requires lease) |
| `POST /hwmon/rescan` | Re-enumerate hwmon devices |
| `POST /gpu/{gpu_id}/fan/pwm` | Set GPU fan speed (PMFW or hwmon) |
| `POST /gpu/{gpu_id}/fan/reset` | Reset GPU fan to automatic mode |
| `POST /profile/activate` | Switch active profile at runtime |
| `POST /config/profile-search-dirs` | Register additional profile search dirs (persists to `runtime.toml`) |
| `POST /config/startup-delay` | Set startup delay seconds (persists to `runtime.toml`) |

## Identity contract

Every sensor/fan/header includes:
- `id` — stable machine key (never depends on `hwmonN` index or `/dev/sdX`)
- `label` — best-effort human name
- `source` — `openfan` | `hwmon`
- `kind`/`type` where applicable

## Measured vs commanded

- `rpm` — measured from hardware (OpenFanController serial reads, hwmon `fanN_input`)
- `last_commanded_pwm` — daemon-tracked (firmware does not report PWM state)
- These are always separate fields, never ambiguous

## Safety invariants

- **Thermal safety** (`safety.rs`): hottest CpuTemp sensor triggers at 105°C → force all fans to 100%. Hold until 80°C (hysteresis), one-cycle 60% recovery floor. Forces 40% if no CpuTemp sensor found for 5 consecutive cycles.
- **OpenFan stop timeout**: 0% PWM allowed for max 8s, then rejected
- **hwmon PWM**: no daemon-enforced per-header floors (`min_pwm_percent: 0` for all). Safety floors are GUI-side profile constraints via `/capabilities` limits.
- **PWM enable mode** (`pwmN_enable=1`) set on first write per lease, reset on release
- **ExecStopPost**: restores `pwm_enable=2` (auto) and resets GPU fan curves on any service stop
- **GPU PMFW writes**: clamped to OD_RANGE from firmware PPTable (prevents EINVAL)

## Key design decisions

- ADR-001: IPC transport — HTTP over Unix domain socket (axum + tokio)
- Lease model: single exclusive lease for hwmon writes (60s TTL, renewable)
- Schema: additive-only within v1, stable keys and enums

## Test counts

312 total (290 unit + 22 integration). No tests require real hardware.
