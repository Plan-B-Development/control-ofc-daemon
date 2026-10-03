# Developer Handover

## Project overview

Control-OFC is a fan control system for Linux desktops, consisting of:
- **Rust daemon** (`daemon/`) — hardware communication, safety logic, IPC server
- **Python GUI** (`control-ofc-gui` repo) — PySide6 fan curve editor and monitor

The daemon owns all hardware access and exposes a stable HTTP-over-Unix-socket API.

## Repository layout

```
Cargo.toml          Workspace manifest: two members, two binaries
daemon/             control-ofc-daemon, the service (src/, tests/)
tray/               control-ofc-tray, a system-tray API client (DEC-352).
                    A separate crate so it cannot reach daemon internals
man/                scdoc sources for both man pages
completions/        bash / zsh / fish completions for both binaries
packaging/          PKGBUILD, systemd unit, restore script, sleep hook,
                    Super-I/O guard, modules-load.d, udev and drop-in examples
docs/               USER_GUIDE.md, this file, ADRs/
daemon.md           Architecture: module map, data flow, safety model, API
```

**The per-file map is `daemon.md` § Module Map**, for both crates. It is not
repeated here: this file used to carry a second copy, and it fell about twenty
modules and the whole tray crate behind.

## Build and test

**The gate commands are the ones CI runs: the `cargo` steps in
`.github/workflows/ci.yml`** (format, clippy with `-D warnings`, the tests, the
doc tests, and `cargo deny`). They are not restated here — an earlier copy here
drifted to a variant that tested a different set. The toolchain is pinned in
`rust-toolchain.toml`, so a local run uses the compiler CI uses.

To build both binaries (from the repository root; they land in
`target/release/`):

```bash
cargo build --release
```

## Running the daemon

**Never run the daemon by hand while the service is active.** A second
`control-ofc-daemon` refuses to start (DEC-467): it takes an `flock` on
`{state_dir}/daemon.lock` before it writes anything, and will not remove a socket
something is still serving on, so `cargo run` or a bare `sudo control-ofc-daemon`
just exits with *another control-ofc-daemon is already running*. (Older daemons
deleted the service's socket and ran a second profile engine: two writers on the
same fans.) The reverse is not guarded: starting the service while a hand-started
daemon runs makes the unit fail, and its `ExecStopPost` hands back headers the
hand-started daemon is driving (register row `LIFE-a`). Stop the service first
(`sudo systemctl stop control-ofc-daemon`), and stop your own daemon before you
start the service again.

The supported way to run your own build is to install the package once and then
swap in your binary, so the unit, `control-ofc-restore-auto` (the crash-time
hand-back), the sleep hook, the Super-I/O guard and `modules-load.d` all stay in
place:

```bash
# once: install the package (README.md § Install), then
cargo build --release
sudo install -m755 target/release/control-ofc-daemon /usr/bin/control-ofc-daemon
sudo systemctl restart control-ofc-daemon
journalctl -u control-ofc-daemon -f
```

pacman overwrites `/usr/bin/control-ofc-daemon` on the next upgrade of the
package (and `pacman -Qkk control-ofc-daemon` reports it as modified until then).
To go back to the released binary, reinstall the package:
`sudo pacman -S control-ofc-daemon`. The config file is
`/etc/control-ofc/daemon.toml`; `docs/USER_GUIDE.md` § Configuration says what an
edit can break.

## IPC socket

- Default path: `/run/control-ofc/control-ofc.sock`
- Configurable via `[ipc] socket_path` in TOML config
- The daemon creates the parent directory and, on start, deletes whatever socket
  file is already at the path — a live one included, which is why § Running the
  daemon says to stop the service first
- GUI discovers the socket via config or the default path

## API endpoints (v1)

### Read-only
| Endpoint | Description |
|---|---|
| `GET /capabilities` | Device capabilities, feature flags, safety limits, `devices.amd_gpu.kernel_warnings` (DEC-098) |
| `GET /status` | Health status + subsystem freshness |
| `GET /sensors` | Cached temperature readings |
| `GET /fans` | Fan RPM + last commanded PWM |
| `GET /poll` | Batch: status + sensors + fans in one call |
| `GET /sensors/history` | Per-entity time-series history |
| `GET /hwmon/headers` | Discovered controllable PWM headers |
| `GET /profiles`, `GET /profiles/{id}` | Daemon-stored profiles (store of record, DEC-160) |
| `GET /profile/active` | Currently active profile info |
| `GET /diagnostics/hardware` | Hardware readiness: hwmon chips, GPU detection, thermal-safety state, kernel modules, ACPI conflicts, board info, kernel warnings |
| `GET /inventory/hwmon` | Structured hwmon inventory — temps, fan tachs, PWM metadata (DEC-200) |
| `GET /inventory/readiness` | Readiness items (`blocks_monitoring`/`blocks_control`, `reboot_may_be_required`; DEC-200) |
| `GET /inventory/superio` | Passive Super-I/O chip detection (DEC-202) |
| `GET /inventory/hardware-readiness` | Combined readiness + Super-I/O snapshot from one shared scan (DEC-207) |
| `GET /config` | Effective merged configuration: per key its value, running value, `source`, `mutable`, `requires_restart`, `restart_pending` (DEC-243) |
| `GET /diagnostics/characterization` | Current or most recent characterisation run, with the points measured so far (DEC-313) |
| `GET /diagnostics/preflight` | Typed safety verdict for one header + one diagnostic, before anything is driven (DEC-333). Read-only: no lease, no slot, nothing reserved |
| `GET /diagnostics/control-path` | Current/most recent control-path discovery run, plus every persisted relationship (DEC-333). Records are keyed by stable header id and pruned at boot to whatever discovery still sees |

### Write
The profile engine is the **sole writer** as of 2.0.0 (DEC-159/DEC-165); the GUI sends intent + diagnostics calls. Bare PWM/lease endpoints were retired (note below).

| Endpoint | Description |
|---|---|
| `POST /profiles`, `PUT`/`DELETE /profiles/{id}` | Profile CRUD + `?validate_only` — daemon is the store of record (DEC-160) |
| `POST /profile/activate` | Switch active profile at runtime |
| `POST /profile/deactivate` | Clear active profile (DEC-097); idempotent. Clears every control override, and the next engine tick gives back each header the daemon took (DEC-382) |
| `POST /control/{control_id}/override` (+ `/override/renew`, `DELETE`) | Expiring manual override — floor-clamped, deadman, monotonic fencing (DEC-163) |
| `POST /fans/{fan_id}/identify` | Per-fan identify hold/restore — 0 for an ordinary fan (floor-exempt), a floored perturbation for a pump-protected header (DEC-311/312/384); deadman auto-restore (DEC-166) |
| `POST /config/header-role` | Assign/clear a PWM header's role; a `pump` assignment earns the 30% floor (DEC-311), a `cpu_fan` one the same engine floor without pump protection (`ROLE-a`) |
| `GET /inventory/cooling-devices` | Cooling-device topology + the shipped device policies (DEC-316). Metadata — the profile engine never reads a device |
| `GET /validation/session` | The current or most recent validation session in full (DEC-317). The engine is an observer that may orchestrate the existing verify/characterize handlers; it never writes a duty itself |
| `GET /validation/sessions`, `/validation/sessions/{id}` | Retained session index (last 5) and one session in full (DEC-317) |
| `POST /validation/session` | Start a session (DEC-317). `stop_when_diagnostics_complete` (2.43.0+, gated on `control.validation_auto_stop`) makes the orchestrator finalise the session through the id-fenced `stop_if` when its walk completes normally — **never** from the shutdown or superseded early-returns. Requires a non-empty `diagnostics[]`; the combination is rejected rather than ignored, because `spawn_orchestration` is not spawned for an empty one and there would be no task to carry the hop |
| `POST /validation/session/stop`, `DELETE /validation/session` | Finalise as `completed` / as `cancelled`. Both go through `finalise_in_place`, so a cancel is **not** a discard — same samples, analysis and summary, `state` is the only difference (`P8-bc`) |
| `POST /validation/session/event`, `/validation/session/measurement` | User marker and external instrument reading. Both free-text fields are bounded at `VALIDATION_MAX_TEXT_FIELD_BYTES` at ingest (DEC-320) |
| `POST /config/cooling-device` | Create/replace a cooling device. Confers **no** pump protection: that is still `/config/header-role` (DEC-316) |
| `DELETE /config/cooling-device/{id}` | Remove a cooling device (DEC-316) |
| `POST /fans/openfan/{ch}/calibration` + `GET`/`DELETE /diagnostics/openfan-calibration` | OpenFan calibration as a 202 + poll run (DEC-452): descent to the stall duty, ascent to the restart duty, gated on every 500 ms sample; requires `acknowledge_below_floor: true` |
| `POST /fans/openfan/{ch}/calibrate` | Deprecated (DEC-452): the same run, held open until it ends |
| `POST /hwmon/{header_id}/verify` | Behavioural test of PWM write effectiveness (~6 s; daemon's own internal lease); returns `restore_failed: bool` per DEC-100 |
| `DELETE /diagnostics/characterization` | Cooperative cancel of a running sweep; the pre-sweep duty is restored unless a thermal force or shutdown owns the header |
| `POST /hwmon/{header_id}/characterize` (behaviour inputs) | DEC-334, 2.40.0+, gated on `control.pwm_behaviour_characterization`. `bidirectional` walks down-then-up (so the run ends high); `stability_seconds` adds a dwell at up to 3 daemon-chosen duties. **The dwell renews the engine pause and the hwmon lease from inside its own loop** under a bound derived from `STABILITY_RENEW_INTERVAL_S` — the settle bound holds at exactly `15 x 2 == 30`, so a longer hold under per-step renewal would overrun the deadman at any dwell length. Statistics live in the pure `api/stats.rs`; learned bands in `pwm_baselines.rs`, read by nothing in the control path |
| `POST /hwmon/{header_id}/discover-control-path` | Establish which tach channel(s) this output drives, by measurement (DEC-333). Claims the **same** single-flight verify slot as verify/calibrate/characterize, so at most one of the four ever drives hardware. Perturbs away from the nearer rail; 0% unreachable for any header; a pump never crosses its floor. **DEC-336:** refuses (`409 validation_error`, retryable) and aborts in flight when every temperature reading is stale — the refusal the preflight publishes, from the same predicate |
| `DELETE /diagnostics/control-path` | Cooperative cancel; same restore semantics and the same two deliberate skips as the characterisation sweep |
| `POST /hwmon/{header_id}/stall-probe` | **`[SAFETY]`** stall/restart probe below 20 % (DEC-407). Same single-flight slot; eligibility (`stall_probe::ineligibility`, pump union first) re-checked before every write, on every sample and once at the end to floor the restore; gates shared with characterisation via `api::diagnostic_gates`; every read bounded at `DIAGNOSTIC_READ_BUDGET` (2 s, shared with characterisation since DEC-420) and an unreadable sample ends the run; every write bounded at `DIAGNOSTIC_WRITE_BUDGET` (2 s, all three diagnostics, DEC-455); every abort/cancel kicks to 100 % except while shutting down or after a write that did not return, which writes nothing more on a header with a mode switch and queues one 100 % write behind it on a header with none. `GET`/`DELETE /diagnostics/stall-probe` read and cancel it |
| `POST /gpu/{gpu_id}/fan/verify` | Test GPU fan-control effectiveness (~6 s, no lease) |
| `POST /gpu/{gpu_id}/fan/reset` | Reset GPU fan to automatic |
| `POST /hwmon/rescan` | Re-enumerate hwmon devices |
| `POST /fans/openfan/rescan` | Adopt an OpenFanController found after boot (DEC-265) |
| `POST /config/profile-search-dirs` | Add and/or remove profile search dirs (persists to `runtime.toml`); `remove` is >= 2.23.0, gated by `control.profile_search_dir_remove` (DEC-285) |
| `POST /config/startup-delay` | Set startup delay seconds (persists to `runtime.toml`) |
| `POST /config/exit-floor` | Set the exit floor, 0-100; applies live (DEC-388) |
| `POST /config/poll-interval` | Set the poll interval, 250-2000 ms; restart to apply (DEC-243). The ceiling bounds how stale a temperature the thermal ladder can act on |
| `POST /config/serial-port`, `/config/serial-timeout` | Set the OpenFan port (`null` = auto-detect) and the read timeout, 50-1000 ms; restart to apply (DEC-243) |
| `POST /config/allow-port-probe`, `/config/nvidia-telemetry` | The two `[detection]` opt-ins; each also needs its systemd drop-in (DEC-243) |
| `POST /inventory/superio/probe` | Opt-in active Super-I/O `/dev/port` probe (DEC-203) |
| `POST /config/preferred-cpu-sensor` | Persist the preferred CPU temp sensor (DEC-200) |
| `POST /config/preferred-mb-sensor` | Persist the preferred motherboard temp sensor (DEC-200) |

**Retired at 2.0.0 (DEC-165):** bare PWM writes (`/fans/openfan/{ch}/pwm`, `/fans/openfan/pwm`, `/hwmon/{id}/pwm`, `/gpu/{id}/fan/pwm`), `/fans/openfan/{ch}/target_rpm`, and all `/hwmon/lease/*`.

## Identity contract

Every sensor/fan/header includes:
- `id` — stable machine key (never depends on `hwmonN` index or `/dev/sdX`)
- `label` — best-effort human name
- `source` — fan `source` is `openfan` | `hwmon` | `amd_gpu` | `intel_gpu` | `nvidia_gpu` (the five `KNOWN_MEMBER_SOURCES`; `intel_gpu`/`nvidia_gpu` are read-only — DEC-121/DEC-204); GPU fan ids embed the PCI BDF (`amd_gpu:{bdf}` / `intel_gpu:{bdf}` / `nvidia_gpu:{bdf}`). Sensor `source` is `hwmon` | `amd_gpu` | `intel_gpu` | `nvidia_gpu`. (`aio_hwmon` is an *internal* `DeviceLabel` classification, not a wire fan source — AIO pump fans surface as `hwmon`.)
- `kind`/`type` where applicable

## Measured vs commanded

- `rpm` — measured from hardware (OpenFanController serial reads, hwmon `fanN_input`, or NVML per driver R565+)
- `last_commanded_pwm` — daemon-tracked (OpenFan firmware does not report PWM state). **For an hwmon header this field has two producers** (register row `AIO5-a`): the poll writes the sysfs readback, the engine writes the commanded value. Read `pwm_readback_pct` for the readback where the distinction matters
- `pwm_readback_pct` — the hardware readback of `pwmN` (hwmon only, DEC-317); one producer, always the poll
- `pwm_commanded_pct` — the duty the daemon last **commanded** (hwmon only, DEC-318); one producer, always the write path. With `pwm_readback_pct` these are the two clean axes; read them, not `last_commanded_pwm`, whenever command and readback must be told apart
- `duty_pct` — firmware-**reported** current fan duty % (NVIDIA via NVML, DEC-204); a *measured* value, present only where the source exposes a duty readback
- `stall_detected` — **derived**, not measured: 0 RPM under a duty above 20 %. For an hwmon header the duty is `pwm_commanded_pct` while the daemon commands it, otherwise `pwm_readback_pct`, and then only once the daemon has seen that header's fan spinning (`DaemonState::hwmon_seen_spinning`, DEC-458) — so a header with no fan reports `null`, never `true`
- These are always separate fields, never ambiguous

## Safety invariants

- **Thermal safety** (`safety.rs`): hottest CpuTemp sensor triggers at the trip point → drive every OpenFan channel and writable hwmon header the machine has to 100%. **The trip point is per-machine (DEC-308)**: 105°C is the floor and fallback, raised to `min(CPU-reported ceiling + 5, 115)` where the kernel publishes `tempN_crit`; `/diagnostics/hardware` reports the value acted on. Hold until a FRESH reading at or below 80°C — a stale or vanished CPU sensor keeps the emergency at 100% — then straight back to the profile (no recovery rung since DEC-386; the ladder is one exhaustive `match` in `safety_tick.rs`). **Every duty is a floor over profile output, not a replacement (DEC-307)** — `force_all_with_floor(pct, &commands, reach)` gives each output in reach `max(commanded, pct)`; at 100% an output no control commands still gets `pct`, which is what preserves the emergency's reach, and below 100% the reach is the profile's own outputs, with everything else the emergency took given back (DEC-382). With nothing latched, floors the profile's outputs at 40% if no CpuTemp reading is fresh (DEC-267: older than 5 poll intervals counts as absent) for 5 consecutive cycles — a control skipped that tick keeps its fans at their last duty under it (DEC-386, `TS-p`). GPU fans are excluded by design (DEC-130) — PMFW firmware owns GPU thermal protection; the exclusion is structural (`GpuBackend` does not implement `SafetyWriteBackend`).
- **AIO / coolant** (`hwmon/aio.rs`, DEC-156): coolant temperatures are classified as the `CoolantTemp` sensor kind and AIO PWM headers are flagged `is_aio` (dynamic `aio_hwmon` capability).
- **Cooling-failure detection** (DEC-443, `W-SAFE`): the hottest fresh `CoolantTemp` reading at or above `safety.coolant_limit_c` (default 60 °C, settable 40–70, no off switch) latches the same 100 % force as the CPU emergency and releases 5 °C below (`safety_tick::evaluate_coolant_tick` + `combine`, by maximum; `emergency_causes` on `/status`). A profile pump seen spinning that reads 0 RPM for 10 s while commanded ≥ 30 % is driven to 100 % for 30 s, held there if it does not turn or stalls twice (`profile_engine::pump_stall`, `pump_stalls[]`). A pump on a DC-mode header is floored at 70 % instead of 30 % everywhere the pump floor applies (`profile::pump_floor_pct`). A CPU held at its ceiling for 60 s under < 50 % non-GPU duty raises an advisory that forces nothing (`profile_engine::cooling_advisory`, `advisories[]`).
- **OpenFan stop timeout** (`serial/controller.rs::apply_safety`): defence in depth, not a cap on a stop. A repeated 0% coalesces BEFORE the timeout is checked (CONC-2), so a held stop is unbounded; the timer refuses only a wire-bound 0% against a stop that started ≥ `STOP_TIMEOUT` (8 s) ago, and a non-zero write, a failed reply or a reconnect all clear it (DEC-426, `DC-b`)
- **hwmon PWM**: no daemon-enforced per-header floors (`min_pwm_percent: 0` for all). The role-aware pump/CPU floor is GUI-baked and **daemon-enforced** (validate-time reject + eval-time clamp, DEC-162); the thermal force is the absolute backstop.
- **Pump-stop guard** (`profile.rs`, DEC-167): a control with a pump/CPU member may not be set to stop — a non-zero `stop_pct` is rejected at profile-validate time (`PUMP_STOP_FORBIDDEN` → `400 validation_error`), and the eval-time stop-snap is skipped for pump/CPU members on any un-validated profile. Distinct from the DEC-162 *floor* above: this forbids *stopping*, not merely clamps the minimum.
- **PWM enable mode** (`pwmN_enable=1`) set on the first write that takes a header. It is given back by `hwmon::handback` to the mode (or duty) recorded before that write — never to a fixed value such as `2`, which is Thermal Cruise on `nct6775` (DEC-382) — once nothing holds the header: after a force, a diagnostic, a deactivation, or a switch to a profile that no longer names it
- **ExecStopPost**: replays the hwmon hand-back record — each header the daemon took goes back to what it was doing before (DEC-382, `hwmon::handback`) — and the legacy GPU verify's record (`gpu-handback`, DEC-414: a pre-RDNA3 card a crashed verify left in manual mode gets its original mode back), and resets the PMFW fan curve on each GPU the daemon drove (`gpu-pmfw-handback`, DEC-435 — never every card) on any service stop
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
daemon version. To see the live count locally, run the test commands from
`.github/workflows/ci.yml` — deliberately not restated here, for the same
reason the section above gives: this line had drifted to `--all-features`,
which selects nothing (DEC-300).
