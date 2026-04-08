# Changelog

## [0.7.2] — 2026-04-08

### R70 — Pre-release Security Hardening (V5 Phase 6)

Addresses Rust daemon findings from the V5 Phase 6 security & dependencies audit.

- **S3 (P2):** State file (`daemon_state.json`) now explicitly set to 0o600 (owner-only) before atomic rename. Defense-in-depth — parent dir is already 0o700 via systemd `StateDirectory=`. Added permission verification test.
- **S4 (P3):** Documented why root is required and why `CapabilityBoundingSet` is intentionally deferred in `onlyfans-daemon.service`.
- **S5 (P3):** Documented that sysfs path inclusion in error responses is intentional (public paths, local-only socket, diagnostic value).

## [0.7.1] — 2026-04-08

### R68 — Pre-release API Contract Cleanup (V5 Phase 3)

Resolves F1 and F3 from the V5 Phase 3 cross-boundary API contract audit.

- **F1 (P2):** Removed dead `publish_interval_ms` field from `PollingConfig`. This was a telemetry vestige — never referenced by runtime code after the R52 telemetry de-scope. The field, its default function, validation rule (`must be >= poll_interval_ms`), startup log line, config example, and user guide entry are all removed. **Breaking:** existing `daemon.toml` files containing `publish_interval_ms` under `[polling]` will now fail to parse (`deny_unknown_fields`). Remove the line to fix.
- **F3 (P3):** Fixed health module docstring — replaced stale "telemetry stats" with "AIO stats".
- Deleted `rejects_publish_less_than_poll` test (tested only the removed validation).
- Updated 5 tests to remove `publish_interval_ms` from test TOML strings and assertions.

## [0.7.0] — 2026-04-07

### R67 — Pre-release Rust Daemon Quality Remediation (V5 Phase 1)

Resolves all 17 findings from the V5 Phase 1 Rust daemon code review.

#### Quality gates (F1)
- `cargo fmt` and `cargo clippy -D warnings` now pass cleanly.
- Derived `Default` on `StartupConfig`; use `.is_multiple_of()` idiom.

#### P1 — Fixed before release
- **F2:** Replaced all `serde_json::to_value().unwrap()/expect()` in handlers with a `json_ok()` helper that returns HTTP 500 with proper error envelope on serialization failure.
- **F3:** Error suppression in polling loops now logs every 60th consecutive error (~1/min) instead of going permanently silent after 4 failures.
- **F4:** Migrated `profile_search_dirs` from `std::sync::RwLock` to `parking_lot::RwLock` — no more poison-panic risk, consistent with rest of codebase.

#### P2 — Fixed soon
- **F5:** Thermal safety thresholds kept as compile-time constants (decision: configurability adds risk without clear demand). Documented in audit.
- **F6:** Created `constants.rs` module — consolidated 12+ scattered operational constants (stall threshold, SSE limits, GPU coalescing, serial baud, probe range, stop timeout, GUI activity timeout, etc.). Eliminated duplication (stall threshold x6 -> x1, channel count x2 -> x1).
- **F7:** Extracted `build_sensor_entries()` and `build_fan_entries()` helpers, eliminating ~120 lines of duplication between `sensors_handler`/`fans_handler` and `poll_handler`.
- **F8:** Config parse failures now log a warning before overwriting with defaults (was silent `unwrap_or_default`).
- **F9:** SSE client connection limit (max 5) with HTTP 503 rejection and proper error envelope. `GuardedStream` wrapper ensures counter accuracy on client disconnect.
- **F10:** `SystemTime` before-epoch fallback now logs and skips the sample instead of recording timestamp 0.
- **F11:** Removed duplicate `cmd.member_id.clone()` in GPU profile engine path.
- **F12:** Eliminated unnecessary full `PwmHeaderDescriptor` clone in `set_pwm()` — extracts only the needed fields.

#### P3 — Fixed for convenience
- **F13:** Replaced magic `255` in PWM raw conversion with `protocol::MAX_PWM`; replaced magic `5` GPU retry with `constants::GPU_PMFW_WRITE_RETRIES`; moved calibration `MAX_TEMP_C` to `constants::CALIBRATION_MAX_TEMP_C` with documented relationship to safety trigger.
- **F14:** Calibration parameter clamping now logs when user-requested values are modified.
- **F15:** Serial timeout error messages now report actual configured timeout instead of hardcoded 500ms.
- **F16:** `read_hwmon_fan_states` now logs at debug level when sysfs reads fail or headers are dropped.
- **F17:** Profile engine logs a warning when openfan commands are dropped due to malformed `member_id`.

#### Tests
- 6 new tests (json_ok, build_sensor/fan_entries, stall detection, malformed config), 290 total (268 unit + 22 integration).
- 4 compile-time const assertions guard safety invariants.

## [0.6.1] — 2026-04-07

### R65 — Configurable Startup Delay
- **Feature:** `[startup] delay_secs` in daemon.toml — configurable delay before device detection after boot (0-30s, default 0).
- **Feature:** `POST /config/startup-delay` API endpoint — GUI can set the delay, daemon persists to daemon.toml. Takes effect on next restart.
- 3 new config tests (parse, default, validation), 284 total.

## [0.6.0] — 2026-04-07

### R64 — Runtime Config Reload + Profile Search Dirs API
- **Feature:** SIGHUP config reload — daemon re-reads `daemon.toml` and updates profile search dirs in memory. Enables `systemctl reload onlyfans-daemon`.
- **Feature:** `POST /config/profile-search-dirs` API endpoint — GUI (or any client) can add profile search directories at runtime. Daemon validates, updates in-memory state, and persists to `daemon.toml` atomically.
- **Feature:** Multi-user support — each GUI user can register their profile directory via the API; the daemon merges all dirs and preserves `/etc/onlyfans/profiles`.
- **Fix:** `profile_search_dirs` in AppState is now `RwLock<Vec<PathBuf>>` — safely mutable at runtime.
- Added `ExecReload=/bin/kill -HUP $MAINPID` to systemd service file.
- Added `config_path` to AppState so handlers can persist config changes.
- 2 new config persistence tests, 281 total.

## [0.5.9] — 2026-04-07

### R63 — Fix Profile Activation Path Validation (completes R62)
- **Fix:** `default_profile_search_dirs()` now falls back to `/root/.config/onlyfans/profiles` when neither `HOME` nor `XDG_CONFIG_HOME` is set (common for systemd services running as root without `User=`).
- **Fix:** systemd service file now sets `Environment=HOME=/root` so the daemon's environment always has HOME.
- **Fix:** `activate_profile_handler` logs a warning when all configured search directories fail canonicalization (empty allowed list).
- 2 new config tests (HOME unset fallback, HOME set preference), 279 total.

## [0.5.8] — 2026-04-07

### R62 — Configurable Profile Search Directories
- **Feature:** Profile search directories now configurable via `[profiles] search_dirs` in `daemon.toml`. Replaces hardcoded HOME-based detection that failed when daemon runs as root.
- **Fix:** Path validation now canonicalizes both the incoming profile path AND each search directory before comparison (CWE-22 hardening).
- **Fix:** `find_profile()` now accepts explicit search dirs instead of using hardcoded paths internally.
- Updated `daemon.toml.example` with `[profiles]` section documentation.
- 3 new config tests, 274 total (251 unit + 22 integration + 1 existing).

## [0.3.0] — 2026-03-31

### Release Generalisation — Cross-System Readiness
- **Config path override:** Daemon config path now overridable via `--config` CLI arg or `$ONLYFANS_CONFIG` env var (default: `/etc/onlyfans/daemon.toml`). Supports container deployments and dev testing.
- **Serial fallback expanded:** Direct probe fallback now scans `/dev/ttyUSB0-9` in addition to `/dev/ttyACM0-9`, covering FTDI/CH340 adapters when libudev is unavailable.
- **Service file portability:** `DeviceAllow` now uses `char-ttyACM` and `char-ttyUSB` class wildcards instead of hardcoded `/dev/ttyACM0-1`. `SupplementaryGroups` includes both `uucp` (Arch) and `dialout` (Debian) — systemd ignores missing groups.
- **Documentation:** Added serial setup instructions, VID/PID discovery, udev rule configuration, and config override usage to USER_GUIDE and DEVELOPER_HANDOVER.
- 1 new test: `load_from_custom_path` (254 total)

### R50 — Daemon Persisted-State Hardening
- **Fix:** `daemon_state.json` writes failed with `EROFS (Read-only file system, os error 30)` under systemd `ProtectSystem=strict` sandbox
- **Root cause:** systemd service file was missing `StateDirectory=onlyfans` and `/var/lib/onlyfans` was not in `ReadWritePaths`
- Added `StateDirectory=onlyfans` to systemd unit — systemd now creates and manages `/var/lib/onlyfans` with correct ownership
- Added `/var/lib/onlyfans` to `ReadWritePaths` for belt-and-suspenders protection
- State directory now configurable via `[state] state_dir` in `daemon.toml` (default: `/var/lib/onlyfans`)
- `daemon_state.rs` rewritten to use `OnceLock<String>` for runtime-configurable state path
- State directory initialized from config at startup before any load/save operations

### Write-Path Sanity Check — hwmon + OpenFan Audit
- **hwmon coalescing:** Added per-header write state tracking (`last_commanded_pct`, `manual_mode_set`). Identical PWM writes now skip sysfs entirely (0 ops instead of 4). `pwm_enable` written once per lease instead of every call. State reset on lease release.
- **OpenFan gui_active check:** Profile engine now skips OpenFan writes when GUI was active in the last 30s, matching the existing GPU write deferral (prevents dual-writer contention).
- **No issues found:** sysfs scalar parsing is correct for standard hwmon files; hwmon dual-writer conflict properly handled by lease mechanism; serial transport mutex prevents concurrent writes; reconnect logic is write-free.

### R53 — GPU Fan Curve EINVAL Fix
- **Fix (P0):** `set_static_speed()` now reads the device's OD_RANGE before writing PMFW curve points. Speed is clamped to the device minimum (typically 15%) instead of passing through unchecked values that the driver rejects with EINVAL.
- **Root cause:** Profile engine evaluated curves to low percentages (e.g., 5-10% at idle), but PMFW firmware rejects fan speed below `OverDriveLimitsMin` (typically 15%). Point 0 failed first, aborting the entire write. Temperature values now use the device's actual range instead of hardcoded 25-100°C.
- **Fix (P1):** Profile engine now tracks failed GPU writes and suppresses retry until the speed changes or a 60-second cooldown elapses. Previously, a failed write was retried every second with no backoff (1 WARN/sec in journal).
- **Fix (P2):** Write error messages now include the actual values written (temp°C, speed%) for diagnosability.
- 1 new test: `set_static_speed_clamps_below_od_range_minimum` (253 total)

### R52 — Syslog/Telemetry De-Scope
- **Removed:** Complete telemetry module (syslog.rs, queue.rs, aggregator.rs, exporter.rs) — ~1,133 lines
- **Removed:** `TelemetryConfig` from daemon config, `/telemetry/status` and `/telemetry/config` endpoints
- **Removed:** Telemetry types from health state, staleness computation, capabilities response
- **Removed:** 49 telemetry-specific tests (301 → 252 total)
- **Breaking:** `daemon.toml` files with `[telemetry]` section now fail to parse — remove the section
- **Removed:** `[telemetry]` section from `daemon.toml.example`

### V4 Comprehensive Audit — Safety Fixes
- **Fix (P0):** Daemon now restores `pwm_enable=2` (automatic) for all hwmon headers on shutdown. Previously only GPU fans were reset — motherboard fans could be stuck in manual mode after a daemon crash.
- **Fix (P1):** Thermal safety override now logs errors at ERROR level instead of silently discarding them. Failed writes during thermal emergency use "THERMAL SAFETY" prefix for operator visibility.
- **Fix (P2):** Pre-RDNA3 GPU fallback path now propagates `pwm1_enable` write error instead of silently discarding it. The amdgpu driver rejects `pwm1` writes when not in manual mode — previously the enable error was dropped, causing a redundant `pwm1` write that also failed.
- 4 new tests (272 unit + 29 integration = 301 total)

## [0.2.0] — 2026-03-18

### Protocol Fix — OpenFanController Response Parsing
- **Fix:** Protocol decoder now accepts responses without closing `>` bracket (real Karanovic OpenFan firmware omits it)
- Auto-detect fallback: probes `/dev/ttyACM0..9` directly when libudev enumeration fails (systemd sandbox)
- FanController and polling loop now share a single serial connection (was incorrectly opening two)
- All serial probe attempts logged at INFO level for diagnostics
- Systemd unit updated with `DeviceAllow` for serial device access

### Hardware Polling & Serial Port Support
- Added hardware polling loops (`polling.rs`) — hwmon sensors and OpenFanController fans now polled on `poll_interval_ms`
- Added real serial transport (`serial/real_transport.rs`) using `serialport` crate (115200 baud)
- Added auto-detection of OpenFanController on `/dev/ttyACM*` and `/dev/ttyUSB*` (probes with `ReadAllRpm`)
- Daemon now initializes hwmon PWM controller from sysfs discovery at startup
- Daemon now initializes OpenFanController from configured or auto-detected serial port
- Polling preserves `last_commanded_pwm` from cache when updating RPM readings
- Log suppression after 3 consecutive poll errors per subsystem

### M8 — Finalisation: GUI-Ready Daemon Contract (v1)
- Added `GET /capabilities` endpoint — device capabilities, feature flags, safety limits
  - Devices: openfan (channels, RPM, write), hwmon (headers, lease), aio_hwmon/aio_usb (unsupported)
  - Features: write support flags, lease requirement, telemetry support/enabled
  - Limits: PWM ranges, safety floors, interval bounds
- Added `GET /hwmon/lease/status` — shows held/TTL/owner for GUI lease display
- Added `POST /hwmon/lease/renew` — extend lease TTL without release/retake
- Identity contract: all sensors/fans/headers include stable `id`, `label`, `source`, `kind`
- Measured vs commanded: `rpm` (hardware) and `last_commanded_pwm` (daemon-tracked) always separate
- Added systemd unit file (`packaging/onlyfans-daemon.service`) with security hardening
- Added `docs/DEVELOPER_HANDOVER.md` and `docs/USER_GUIDE.md`
- 11 new tests (219 total, incl. 29 integration)

### M7 — Telemetry Export (TCP Syslog, RFC5424)
- Added telemetry config model with 10 fields: poll/publish intervals, queue size, TCP timeouts, health interval, local log copy
- Added config validation with bounds checking (poll 500–5000ms, publish 1000–60000ms, queue ≥1, timeouts > 0)
- Added `Aggregator` — builds telemetry payloads from cache, filters to allowlisted temperature metrics only
- Added `TelemetryQueue` — bounded queue with drop-oldest backpressure and dropped counter
- Added RFC5424 syslog message builder with `<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID - JSON` format
- Added octet-counting TCP framing: `<len> <syslog-message>`
- Added `TelemetryHandle` — shared state for runtime config, queue, and error tracking
- Added `export_loop` — TCP connection lifecycle with exponential backoff + jitter (capped at 60s), rate-limited error logging
- Added `aggregation_loop` — async poll/publish loop with configurable intervals, health event emission
- Added API endpoints:
  - `GET /telemetry/status` — enabled flag, destination, connection state, queue depth, dropped count, error count
  - `POST /telemetry/config` — enable/disable, host/port, intervals; validates and rejects invalid configs
- Runtime config updates without daemon restart; disabling clears queue and closes connection immediately
- Missing/stale readings included as `null` with reason in telemetry payload
- 43 new tests (208 total, incl. TCP end-to-end integration test with local server)

### M6 — Motherboard (hwmon) PWM Control + Lease Model
- Added PWM header discovery (`hwmon/pwm_discovery.rs`) with stable IDs (`hwmon:<chip>:<device>:pwm<N>:<label>`)
- Added lease/token model (`hwmon/lease.rs`) for exclusive hwmon PWM write access
  - 60-second TTL with automatic expiry
  - Take/release/validate operations
- Added `HwmonPwmController` (`hwmon/pwm_control.rs`) with lease enforcement and safety floors
  - 20% minimum for chassis fans, 30% for CPU/pump headers
  - Automatic `pwmN_enable` mode switching on first write per lease
  - `SysfsWriter` trait for mocked filesystem testing
- Added API endpoints:
  - `GET /hwmon/headers` — list discovered controllable PWM outputs
  - `POST /hwmon/lease/take` — acquire exclusive write lease
  - `POST /hwmon/lease/release` — release write lease
  - `POST /hwmon/{header_id}/pwm` — set PWM (requires lease)
- Error mapping: `lease_required` (403), `lease_already_held` (409), `validation_error` (400), `hardware_unavailable` (503)
- 42 new tests (165 total)

### M5 — OpenFanController Fan Control (Write Paths)
- Added `FanController` (`serial/controller.rs`) with per-channel and all-channel PWM control
- Added target RPM support (closed-loop mode via EMC2305)
- Added POST endpoints: `/fans/openfan/{channel}/pwm`, `/fans/openfan/pwm`, `/fans/openfan/{channel}/target_rpm`
- PWM percent (0-100) converted to raw (0-255) at the protocol boundary
- Safety: 0% PWM allowed for max 8s (stop timeout), non-zero values clamped to 20% minimum
- Command coalescing: duplicate PWM commands are skipped (idempotent)
- Cache tracks `last_commanded_pwm` per channel
- Error envelope: `validation_error` (400), `hardware_unavailable` (503)
- 28 new tests (123 total)

### M4 — IPC v1 (Read-Only Endpoints)
- HTTP over Unix domain socket using axum + tokio
- GET endpoints: `/status`, `/sensors`, `/fans`
- Standard error envelope with structured error responses
- Graceful shutdown via oneshot channel
- ADR-001: IPC transport decision documented

### M3 — Cache, Staleness, and Health Model
- In-memory `StateCache` with `RwLock`, batch updates, snapshot reads
- Staleness thresholds: OK <=2x, Warn 2x-5x, Crit >5x expected interval
- Deterministic health computation with injected time

### M2 — Sensor Collection (Read-Only)
- hwmon sysfs discovery with stable sensor IDs
- Temperature reads (CPU, GPU, disk, motherboard) via hwmon
- Chip classification (k10temp, amdgpu, nvme, ite, nct)

### M1 — OpenFanController Protocol Layer
- Serial protocol encoding/decoding (ASCII hex pairs)
- Command types: ReadAllRpm, ReadRpm, SetPwm, SetAllPwm, SetTargetRpm
- Transport trait with mock support

### M0 — Repo + Scaffolding
- Rust workspace with `daemon/` crate
- Config scaffolding with TOML + validation
- Structured error types
- Module layout for all planned subsystems
