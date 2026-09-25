# Control-OFC Daemon — Architecture Overview

## What this is

A Rust daemon (`control-ofc-daemon`) that controls PC fans via three backends:
- **OpenFan** — custom serial (USB) fan controller
- **hwmon** — motherboard fans via Linux sysfs (`/sys/class/hwmon/`)
- **AMD GPU** — RDNA3+ PMFW fan curves or legacy hwmon PWM

Exposes an HTTP API over a Unix domain socket for the PySide6 GUI.

**The workspace also builds a second binary, `control-ofc-tray` (DEC-352).** It
is a KDE/freedesktop StatusNotifierItem *client* that ships in the same package:
it reads `GET /status` and `GET /profiles` and can `POST /profile/activate` /
`POST /profile/deactivate`, and does nothing else — no lease, no PWM write, no
curve evaluation, no hardware read. **Nothing in this document depends on it.**
The daemon has no knowledge of the tray and no dependency on it; it is a
separate crate specifically so that boundary is enforced by the crate graph.
See `tray/src/lib.rs` and `man control-ofc-tray`.

## Module Map

```
tray/src/              — control-ofc-tray: an API client, not part of the daemon
  main.rs              — arg parsing, single-instance guard, tray registration
  client.rs            — blocking HTTP/1.1 over the Unix socket + wire models
  menu.rs              — the StatusNotifierItem: what is shown, what clicks do
  launch.rs            — starting control-ofc-gui, detached
  single_instance.rs   — one tray per user (flock in $XDG_RUNTIME_DIR)

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
    adoption.rs        — [SAFETY] the single path deciding which port becomes the fan
                         controller, shared by boot adoption and POST /fans/openfan/rescan
                         (DEC-265). One copy on purpose — two would be two chances to skip
                         the DEC-250 identity handshake. Detailed below

  hwmon/
    mod.rs             — hwmon subsystem re-exports
    discovery.rs       — sensor enumeration + stable ID generation
    reader.rs          — temperature reading from sysfs
    plausibility.rs    — [SAFETY] cross-sensor plausibility filter for CPU temps (`294-c`).
                         reader.rs's [-50, 250]C bound (DEC-288) is per-sensor, so it
                         cannot see a value that is absurd only beside its neighbours. The
                         bogus-LOW CPU channel is the dangerous case because it is silent:
                         the sensor IS present, so the no-sensor 40% floor never engages
    types.rs           — SensorKind, SensorReading, SensorDescriptor
    inventory.rs       — structured read-only hwmon inventory: temps + PWM headers + monitor-only tachometers (DEC-200)
    classify.rs        — refines each temp sensor's CPU/motherboard classification for the inventory (DEC-200)
    readiness.rs       — turns the inventory into an actionable hardware-readiness list (DEC-200)
    pwm_discovery.rs   — PWM header discovery (fan outputs)
    pwm_control.rs     — HwmonPwmController + SysfsWriter trait; write coalescing with engine duty reconciliation (DEC-073/DEC-406); shared-report sibling priming (DEC-425)
    lease.rs           — LeaseManager (exclusive write access)
    aio.rs             — liquid-cooler (AIO/custom-loop) recognition: coolant-sensor + is_aio flag + aio_hwmon cap (DEC-156)
    roles.rs           — per-channel header role inference + resolution, and THE
                          pump-protection union `is_pump_protected` (DEC-311/312/316;
                          the active profile's pump-labelled members joined it in DEC-384).
                          `AppState::header_is_pump_protected` is the lookup wrapper
                          around it; callers holding the controller lock must use the
                          function directly or they deadlock
    cooling_device.rs  — cooling-device topology: a pump + radiator fans + an advisory
                          sensor as one named assembly (DEC-316). Metadata only — the
                          profile engine never reads one, and a `pump_member` confers
                          no floor
    device_policy.rs   — trusted, compiled-in device capability policy (DEC-316).
                          Derives NO `Deserialize`, so no inbound payload can set a
                          safety number. Generic entries only in 2.31.0, so no floor moves
    header_caps.rs     — read-only header capability audit (DEC-316): pwmN_freq,
                          pwmN_enable, fanN_min/max, fanN_pulses + a cited
                          supported-mode table. Pure reads; adds no write path
    gpu_detect.rs      — AMD GPU detection via sysfs/DRM
    intel_gpu_detect.rs— Intel discrete GPU (Arc) detection, read-only (DEC-121)
    nouveau_detect.rs  — NVIDIA discrete GPU detection via the open nouveau driver, read-only (DEC-204)
    nvidia.rs          — unified NVIDIA GPU identity (nouveau + NVML) for /capabilities + /diagnostics (DEC-204)
    nvml.rs            — opt-in read-only NVIDIA telemetry backend (trait + Real/Fake/Disabled), proprietary driver (DEC-204)
    nvml_sys.rs        — isolated unsafe FFI to libnvidia-ml.so.1 via libloading (the only NVIDIA unsafe, DEC-204)
    gpu_fan.rs         — PMFW fan curve read/write/reset
    kernel_warnings.rs — kernel-version regression catalog
                          (drm/amd #4765 MES eviction hang on RDNA3/4,
                          6.17.9–6.17.13 + 6.18.0–6.18.6, DEC-422)
                          surfaced via /capabilities.amd_gpu.kernel_warnings
    superio.rs         — passive Super-I/O chip detection (DMI + hwmon + /proc/modules + kmsg + ACPI evidence, DEC-202)
    superio_probe.rs   — opt-in active /dev/port Super-I/O probe, off by default (DEC-203)
    chip_db.rs         — Super-I/O chip → expected-driver knowledge base (DEC-202)
    gigabyte_siv.rs    — Gigabyte SIV decode: the board's firmware-declared fan/temp/volt
                         counts, published as `board_firmware_counts` on
                         `/diagnostics/hardware` (`X87-d`). A measurement, where
                         `chip_db`'s board table is an inference. Read-only sysfs
    voltages.rs        — board voltage rails from hwmon `inN_input`, published as
                         `voltages` on `/diagnostics/hardware` (`WIRE-ag`, DEC-331).
                         Display-only: nothing in the daemon reads a rail. Each entry
                         carries `identified` — true only where the driver labelled the
                         channel; an unlabelled channel is a raw ADC pin whose reading
                         is NOT the rail voltage. Read-only sysfs, no port I/O
    power.rs           — CPU package power, read only inside a validation session
                         (DEC-335). Never on the 1 Hz poll and never consulted by the
                         control path. Deliberately NOT a SensorReading — a watt in
                         `value_c` would lie to curve binding and the thermal path
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
      hwmon_ctl.rs     — hwmon header list, rescan, PWM-verify + characterize endpoints
      validation.rs    — validation-session endpoints + the diagnostic orchestrator
                         (DEC-317). Calls the verify/characterize handlers above as
                         functions — it owns no lease, no floor and no PWM write
      profile.rs       — profile activation + CRUD endpoints
      control.rs       — manual-override + fan-identify endpoints (DEC-163/166)
      config.rs        — runtime config endpoints (search dirs, startup delay)
      hw_diagnostics.rs — hardware diagnostics endpoint
      inventory.rs     — /inventory/{hwmon,readiness,superio,hardware-readiness} reads + Super-I/O probe; shared assessment snapshot + coalesced scan (DEC-200/202/203/207)
      assessment.rs    — hardware-assessment cache + single-flight coordinator (DEC-207)
      path_confine.rs  — SO_PEERCRED search-dir confinement predicate (DEC-205)
      discovery.rs     — /diagnostics/preflight + the control-path routes (DEC-333)
    responses.rs       — response structs (Serialize)
    calibration.rs     — OpenFan calibration sweep
    diagnostics.rs     — hardware-diagnostics scanning logic behind /diagnostics/hardware
    stats.rs           — pure statistics over retained tach samples (DEC-334): mean,
                         median, sigma, CV, dropouts, robust (median/MAD) outliers,
                         update-based settling with a trend term (DEC-405/411),
                         plateaus, effective range, hysteresis.
                         No I/O, no locks, no clock — and written to be reused by
                         Batch 3's steady-state detector, which is its temperature twin
    characterization.rs — PWM/RPM response sweep (DEC-313), reused by validation.
                         Owns RestoreOnDrop, which the discovery sweep reuses verbatim.
                         Its reads are bounded at DIAGNOSTIC_READ_BUDGET (2 s) on the
                         blocking pool, as the stall probe's are, and after a read that
                         does not return it writes nothing more — no restore, unless the
                         header became a pump (`skipped_unresponsive`, DEC-420)
    discovery.rs       — PWM-to-tach control-path sweep (DEC-333). Perturbs one header
                         away from the nearer rail, watches every tach incl. monitor-only
    preflight.rs       — the shared diagnostic safety predicates + typed report (DEC-333).
                         CONSUMES the existing guards rather than restating them, which is
                         why the three older diagnostics needed no edit
    diagnostic_gates.rs — [SAFETY] the per-step write gates (shutdown, cancel, the three
                         thermal gates, keepalive), defined once (DEC-407) and called by
                         characterisation and the stall probe
    stall_probe.rs     — [SAFETY] the stall/restart probe (DEC-407): the ONLY diagnostic
                         that writes below 20 %. Pure eligibility + timing rules, and the
                         adaptive loop (baseline → descent → ascent → kick) over the shared
                         gates and RestoreOnDrop

  validation/            — AIO-MB Phase 5 (DEC-317). Split by who may have side effects.
    mod.rs             — subsystem re-exports + the safety posture, stated once
    session.rs         — pure data model + stable wire tokens (no side effects)
    summary.rs         — pure derivation of the evidence summary (no side effects)
    store.rs           — {state_dir}/validation/*.json; the boot interrupted-sweep
    recorder.rs        — the 1 Hz engine. Reads live state; NEVER writes hardware

  pwm.rs               — shared percent_to_raw / raw_to_percent conversion
  clock.rs             — injectable monotonic clock (lease/override/identify TTLs; deterministic in tests)
  atomic_io.rs         — crash-safe atomic file write (tmp+fsync+rename)
  profile.rs           — profile JSON loading + curve evaluation
  profile_store.rs     — daemon-owned profile storage (store of record, DEC-160)
  pwm_baselines.rs     — {state_dir}/pwm_baselines.json: learned per-duty RPM bands
                         (DEC-334 §6). Widened by each completed run, never replaced;
                         pruned at boot by the same stable-header-id rule. NOTHING in
                         the control path reads it — it is diagnostic evidence only
  control_paths.rs     — {state_dir}/control_paths.json: discovered PWM-to-tach
                         relationships, keyed by stable header id and pruned at boot to
                         whatever discovery can still see (DEC-333)
  profile_engine/      — headless 1Hz curve evaluation loop (DEC-135)
    mod.rs             — loop body / coordinator: orchestrates safety_tick + curve_eval + tuning + backends
    curve_eval.rs      — deadband + trigger latch + Mix/Sync composites (topological order)
    tuning.rs          — offset→floor→step-rate→stop-snap→start-kick→clamp + floor policy
    safety_tick.rs     — thermal ladder + no-sensor fallback as ONE exhaustive decision table (DEC-386)
    backends.rs        — WriteBackend per fan backend (gating/coalescing)
    skipped.rs         — debounced tracking of controls that cannot be resolved (273-i)
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

**hwmon writes coalesce, and a coalesced engine write checks its readback
(DEC-073, DEC-406).** `HwmonPwmController::set_pwm` skips a write whose duty
equals the header's last command while manual mode is still set. Since DEC-406 an
*engine* write that would coalesce first reads `pwmN` back; if it is further than
`READBACK_TOLERANCE_PCT` (2 points, the same tolerance characterisation uses) from
the duty the header **took** — what `pwmN` read immediately after the daemon's last
write, or the command where that read failed — the duty is written again: a
*correction*, counted per header since boot. Comparing with what the header took
rather than with the command is what keeps a coarse driver (`dell_smm`'s three
levels, `thinkpad_acpi`'s eight) or a clamping chip from reading as drift after
every write; a clamp stays visible through `verify_mismatch_counts`. A correction
"did not hold" when the next tick still disagrees (one whose write failed did not
land and is not counted — and since DEC-420 any failed duty write clears the
header's manual flag, so the next command re-takes the header and is written
rather than coalesced, which restarts the count, `TS-au`); after `DUTY_CORRECTION_ATTEMPTS` (3) of those in a row the engine stops rewriting that
header and flags it `duty_not_holding`, so it cannot fight a persistent second
writer indefinitely. It resumes when the command changes, and the flag
clears when a coalesced readback agrees again (or the header is handed back or the
profile deactivated). One drift episode logs at most one WARN for its first
correction, one WARN for the give-up and one INFO for the recovery. An unreadable
duty is unknown, never a mismatch. Scope: writes under a `Verify` lease (verify,
characterise, discover) are never reconciled — a diagnostic gets exactly the duty
it asked for — and the thermal force is unaffected, because it clears
`manual_mode_set` first (`forget_manual_mode`) and so never coalesces. Both
figures are published through the cache on every hwmon `/fans` and `/poll` entry
(`duty_corrections`, `duty_not_holding`), gated by `control.duty_reconciliation`.

**A write to one channel of a shared-report chip primes the others (DEC-425).** The
`arctic_fan` driver (`SHARED_REPORT_CHIPS`) sends all ten channels in every write,
filling the ones not being written from a cache that is 0 at probe and after resume,
so a write to one channel used to command 0 % on the other nine. Before each write to
such a header — in `set_pwm`, which the engine, overrides, identify, every diagnostic
and the thermal force all reach, and in `apply_exit_floor` — the controller sets to
100 % every sibling (same hwmon directory) whose cache reads 0, unless the daemon last
commanded that sibling to 0 itself and the exit floor has not latched it above that.
It is a readback, so it re-arms by itself after a resume. The batch stops at the
first write the device does not answer, and that device is not primed again until
one of its writes succeeds; that write then primes the rest at once, because an
unchanged duty coalesces and the next write may be long in coming. A dead device
therefore costs one failed priming write (the driver's 1 s ACK timeout), not nine on
every write. On a healthy device the first write after probe or resume carries up to
nine extra reports (up to ~0.56 s each), under the controller lock.

## Startup Sequence — OpenFan adoption (DEC-291 / DEC-361)

The OpenFanController is **optional** hardware, and nothing on the critical path
may assume it exists. `main` therefore makes **exactly one** adoption attempt and
then gets out of the way:

```
main
 │
 ├─ serial_port_candidates_enumerated()      # libudev + path scan, OPENS NOTHING
 │    configured [serial] port first, but never the only candidate (DEC-250)
 │
 ├─ first_openfan_port()                     # opens each candidate AT MOST ONCE,
 │    accepts only one answering `ReadAllRpm` (DEC-250 identity handshake)
 │
 ├─ spawn hwmon poll · spawn profile_engine · server::serve()
 │    ↑ these run whether or not a controller was adopted.
 │    `openfan_poll_loop` does NOT — it is gated on `Some(transport)`
 │    (`main.rs:1885`), which is why adoption must spawn it (DEC-266)
 │
 └─ post_boot_adoption_loop()                # ONLY if nothing was adopted
      detached, after the IPC server is already answering
```

**Why one attempt.** This used to be a ladder of up to six tries sleeping
1+2+4+8+16 s, and it ran *ahead* of `axum::serve`, both poll loops and the
profile engine — so the daemon answered no API request and evaluated no thermal
safety for ~31 s, on every machine, including the overwhelming majority that have
no controller at all.

**Enumerate, then identify.** The two halves are separate functions because the
difference is a hardware side effect: `open(2)` on a tty asserts DTR, which
resets Arduino-class boards. `enumerate_serial_candidates` is a libudev/sysfs read
plus `Path::exists`; `auto_detect_port` — which opens — has exactly one remaining
caller, the OpenFan poll loop's reconnect probe, and that runs only after a
controller that was *already adopted* has dropped off.

**The detached search** (`post_boot_adoption_loop`, `api/handlers/openfan.rs`):

| | |
|---|---|
| Window | `post_boot_adoption_window(configured)` — **60s**, or **180s** with `[serial] port` set |
| Tick | `POST_BOOT_ADOPTION_INTERVAL` = 5s, `MissedTickBehavior::Skip` |
| Probes when | the enumerated candidate set differs from the one boot last tried — seeded with `boot_candidates` |
| Handshake retries | `POST_BOOT_HANDSHAKE_RETRIES` = 3, spent only on a probe that **actually ran**, read from the cooldown stamp either side of the call (`OFN-w`) |
| Stops on | first adoption, window expiry, or shutdown — including mid-probe (`OFN-v`) |

Both windows are *longer* than the ~31 s ladder they replace, because waiting now
costs nothing: the daemon is fully serving throughout.

**[SAFETY] It drives `openfan_rescan_handler` rather than probing directly.** A
second probe-and-install path would be a second chance to skip the DEC-250
identity handshake, the DEC-266 conditional install, the poll-loop spawn or the
277-c handle registration. It also inherits the handler's single-flight guard, so
this loop and a user clicking *Rescan Hardware* can never probe the same ports
concurrently.

**The loop owns its own "has anything changed?" test, and must.** Do not
re-derive it from `OPENFAN_RESCAN_COOLDOWN`: that predicate is
`elapsed < COOLDOWN && same_port_set(..)`, an **AND**, so it *spaces* repeat
probes to one per ten seconds and never skips one. Leaning on it would have
opened every unrelated tty ~6 times per boot (18 with a configured port) — worse
than the 12 DEC-361 set out to remove.

**Why any of this is a safety concern.** `force_all_with_floor` reaches OpenFan
fans only through the adopted backend (`force_present_backends`, DEC-371), so an
adoption that never happens is the thermal emergency losing its only route to
those fans. That is why the retry budget, the probe accounting and the shutdown
gating each have their own register rows and regression tests.

## Safety Model

1. **ThermalSafetyRule** (`safety.rs`): Emergency CPU override
   - **Every duty below is a FLOOR over the active profile's output, not a
     replacement for it (DEC-307).** The engine calls
     `force_all_with_floor(pct, &commands, reach)`: each output in reach gets
     `max(commanded, pct)`. At 100 % the reach is every OpenFan channel and
     writable hwmon header, and one no control commands still gets the bare
     `pct` — that is what preserves the emergency's reach. Below 100 % the reach
     is the profile's own outputs only (DEC-382, `ForceReach::for_duty`): a
     sub-100 duty on a header nothing controls would replace a firmware curve
     that may be running it faster, and every output an earlier 100 % tick took
     that the profile does not name is given back in the same write. The ladder can therefore only ever raise a fan. Until DEC-307 these
     were replacements, so the 60% and 40% rungs could drive a fan *below* what
     its curve was asking for; the 100% emergency was never affected, because
     100 is the maximum
   - Triggers at hottest CpuTemp >= the trip point, forcing every OpenFan
     channel and writable hwmon header THE MACHINE HAS to 100% (DEC-371 — the
     log line names the backends actually driven; do not restate this
     enumeration in a message)
   - **The trip point is per-machine (DEC-308).** 105C is the floor and the
     fallback; where the kernel publishes the CPU's own design ceiling
     (`tempN_crit` — `coretemp` documents it as the maximum junction temperature)
     the engine derives `min(ceiling + 5, 115)` and uses that instead. A part is
     *designed* to hold its ceiling under sustained load, so a trip point at or
     below it fires on a healthy machine and then latches forever, because
     release needs a reading the part never produces. Raise-only, capped, and
     gated on authoritative CPU chips (`k10temp`/`coretemp`/`sbtsi_temp`) — a
     Super-I/O `CPUTIN` publishes a `crit` too and it means something else.
     Intel-only in practice: `k10temp` on Zen publishes no `crit`, so AMD keeps
     the 105 floor, which is right — with a ~95C ceiling it was never the broken
     case. `/diagnostics/hardware` reports the value actually acted on
   - **A backstop, not a cooling-failure detector (`TS-m`).** The trip point
     sits above the CPU's own throttle point on purpose, and a CPU holds itself
     there by throttling, so a stopped pump or stalled fans show up as a CPU
     pinned at its ceiling — not as an emergency. The ladder catches a CPU that
     can no longer protect itself, not a cooling fault
   - GPU fans are deliberately excluded (DEC-130) — there is no GPU emergency
     threshold; AMD PMFW firmware protects the GPU by throttling its clocks on
     junction temperature, independently of OS fan control. It does not ramp a
     fan past a curve the daemon has committed (`TS-i`), so while the daemon
     drives a GPU fan, throttling is the GPU's protection; the curve returns to
     firmware when the daemon exits. Excluded from the force, not from control:
     a GPU-bound member keeps following its own curve on every forced tick,
     never the forced duty (DEC-399 — until then a forced tick wrote no GPU fan)
   - Holds until CpuTemp <= 80C. The release threshold is a genuine constant
     (THERMAL_EMERGENCY_RELEASE_C), but the hysteresis SPAN is not — it follows
     the per-machine trip point: 25C at the 105 floor, 35C at the 115 cap. Do
     not restate the span as a fixed number (DEC-292/305)
   - Release needs a FRESH reading at or below 80C, and hands control straight
     back to the profile — there is no recovery rung since DEC-386 (it held 60%
     for two 1 Hz ticks, which was thermally meaningless). Every other output the
     emergency took is given back at the release — an hwmon header to its
     recorded mode, an OpenFan channel to its pre-emergency duty (DEC-382)
   - One fresh reading at or above the trip point is enough to latch, and a
     sensor stuck in [trip point, 250C] keeps the latch for as long as it keeps
     reporting that. There is no plausibility gate, maximum latch time or sibling
     cross-check, by decision (`TS-s`, DEC-400): a safety function that has
     tripped stays tripped until its reset (IEC 61511-1 11.2.7). A stuck sensor
     fails loud, at 100%; the remedy for a known one is at classification, as
     DEC-294's vendor-gated CPUTIN demotion is
   - A latched emergency whose CPU sensor goes stale OR vanishes holds 100%
     until that fresh reading — losing sight of a sensor must never lower an
     already-forced safety output (DEC-269; DEC-386 retired DEC-190's 40% for a
     vanished sensor)
   - With nothing latched: if no CpuTemp reading is fresh (DEC-267: older than 5
     poll intervals counts as absent) for 5 consecutive cycles, floors the
     profile's outputs at 40% (DEC-382: outputs no profile controls stay under
     firmware, and with no profile nothing is forced) — unless the last stale
     reading was at or above release, when curves keep running on it (DEC-269). A
     control skipped that tick keeps its fans at their last duty under the floor
     (DEC-386, `TS-p`); an OpenFan channel whose duty a reconnect or resume lost
     goes to 100% instead, and any other unknown duty gets the bare floor (DEC-401)
   - Override state is surfaced as `thermal_state` in `GET /status`
     (`normal` | `emergency` | `no_sensor_fallback`, DEC-132; `recovery` was
     emitted before DEC-386)
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

3. **Unresolvable controls are reported, not silent** (`profile_engine::skipped`, 273-i)
   - A control the engine cannot resolve is SKIPPED: no command is produced and
     its fans hold their last commanded duty. For a transient cause that is
     correct and invisible by design — the next tick fixes it
   - The case that never fixes itself was silent. A Mix naming a curve id the
     profile no longer has, or a Sync whose target is skipped, is unresolvable
     for as long as the profile says so, and the daemon said nothing: the one
     skip that logged at all used `log::debug!`, below the shipped
     `RUST_LOG=info`, and no API surface carried it. The fan simply stopped
     responding
   - After `SKIP_DEBOUNCE_TICKS` (3) consecutive skipped ticks the control is
     logged once at WARN and listed on `/status` + `/poll` as
     `skipped_controls[] = {control_id, control_name, reason, skipped_for_ms}`.
     It is logged once more when it resolves. `reason` is a stable token —
     `curve_not_found` | `sensor_unavailable` | `mix_unresolvable` |
     `sync_unresolvable` | `backend_unavailable` — and the client owns the wording
   - The debounce is load-bearing, not politeness: `curve_eligible`'s freshness
     budget floors at 5 s, so a sensor on that boundary flaps, and edge-triggering
     at 1 Hz would reproduce exactly the journal spam DEC-193 was written to stop
   - The list is published EXACTLY ONCE per tick, from `TickCompletion::drop`, so
     every exit path — both `continue`s, the mid-tick `break` and the normal end —
     publishes, and a tick that evaluates nothing publishes "nothing skipped"
     rather than leaving the previous tick's claim standing. Two earlier shapes
     were rejected: three explicit publishes (a fourth `continue` added later
     would silently freeze the list), and then clear-at-top/refill-at-bottom,
     which satisfied that but opened a window where a client polling mid-tick was
     told nothing was wrong — two free-running 1 Hz clocks drift through each
     other, so it is hit periodically rather than never. Same lesson as DEC-249
   - Display-only. It changes no control decision, and a skipped control's fans
     still report RPM — what is unknown is only whether anything is commanding
     them, which is what the list says

4. **Lease system** (`lease.rs`): Exclusive hwmon write access
   - 60s TTL, holder must renew periodically
   - A daemon-internal single-writer token (`HwmonWriter::{Engine,Verify,ThermalSafety}`,
     DEC-197) arbitrating the three in-process writers — the profile-engine tick, a hardware
     verify, and the thermal-safety force. Not a client lease: the GUI holds nothing (DEC-165).
     A thermal force-take evicts a verify mid-scan, so the verify's stale token is refused.

5. **Stop timeout** (`controller.rs`): OpenFan 0% wire-write limit
   - Rejects a *wire-bound* 0% write against a stop timer older than 8 s.
     A steady 0% hold coalesces — same-value repeats never reach the wire or
     the timeout (CONC-2, 2026-07-21 audit; the old order errored every tick
     past 8 s, inflating failure streaks) — so this is defence-in-depth
     against channel-tracking drift, not a periodic re-arm requirement

6. **ExecStopPost restore** (`packaging/control-ofc-restore-auto.sh`):
   - Replays the hwmon hand-back record (DEC-382): each header the daemon took gets back exactly what it had — its recorded `pwm_enable`, or its duty if it was already manual — confirmed by read-back, with `fancontrol`'s full-speed fallback; headers the daemon never took are not touched. One switch cannot be read back: `dell_smm`'s global `pwm1_enable` is write-only, so the daemon records `2` (BIOS control) for it and the write itself is the confirmation (DEC-398). Runs once the daemon has exited, whatever ended it — a requested stop, a crash, a SIGKILL, a watchdog kill (DEC-387)
   - Resets GPU fan curves to automatic
   - Replays the legacy GPU hand-back record (DEC-414): the pre-RDNA3 GPU verify is the one path that puts an amdgpu fan in manual mode (`pwm1_enable=1`), so it records the card's original mode in `gpu-handback` before that write and drops the line once it has restored the card. A verify the daemon did not live to finish is given back here; a card no line names — including one another tool put in manual mode — is not touched
   - Re-enables `fan_zero_rpm_enable=1` for every GPU exposing it (DEC-100 — closes the SIGKILL/OOM path the panic hook can't cover)
   - **What it cannot do is end a stall, and this qualification is load-bearing.** `ExecStopPost` runs after *every* exit, the `Restart=on-failure` path included — systemd runs it before scheduling the restart (`systemd.service(5)`: also when the service "exited unexpectedly"; measured on systemd 261, DEC-387). But it runs only once the process HAS exited, so a restore stalled inside the daemon would hold it off for good. That is why the in-process restore in `main.rs` is bounded (DEC-278/279: `restore_gpu_fans_to_auto` then `hand_back_hwmon`, each on its own deadline), and why, since DEC-387, a self-stop announces itself with `STOPPING=1` so that systemd's `TimeoutStopSec=` bounds it too. OpenFan channels are out of `ExecStopPost`'s reach — USB-serial, with no firmware mode to return to: a clean stop leaves them at the exit floor (item 11), a crash at their last duty. *(Corrected by DEC-387, `TS-k`: 278-b said this script "does not run at all" when the daemon exits non-zero and is restarted. It does; the load-bearing fact was always the stall, not the path.)*

7. **Kernel-version regression catalogue** (`hwmon/kernel_warnings.rs`, DEC-098):
   - Curated list of published amdgpu regressions keyed by kernel version + GPU PCI device ID
   - Currently flags `rdna_mes_hang_drm_amd_4765` (DEC-422): the drm/amd #4765 MES eviction hang on every RDNA3 / RDNA3.5 / RDNA4 GPU. It is present on 6.17.9–6.17.13 (a backport never fixed before 6.17 went end-of-life) and 6.18.0–6.18.6, and fixed in 6.18.7 and 6.19. DEC-422 retired `rdna_hang_kernel_6_18_6_19`, whose advice to pin 6.15–6.17 was wrong, and `smu_mismatch_navi48_r9700`, whose premise, a benign SMU interface-version message, was refuted. Daemon v2.56.0 and older still raise both.
   - Surfaced via `GET /capabilities` (`devices.amd_gpu.kernel_warnings`); each entry carries `id` (stable knowledge-base key), `severity` (`info` / `medium` / `high` / `critical`), and `message` (pre-formatted user-visible text). The daemon owns the wording so a message update doesn't require coordinated GUI redeploys.
   - The field uses `#[serde(skip_serializing_if = "Vec::is_empty")]` so older clients that don't know about it see no change in the wire shape
   - The GUI raises a one-time `QMessageBox` for `high` and `critical` warnings; the user's acknowledgement is persisted in `app_settings.acknowledged_kernel_warnings` so the popup does not re-fire on every reconnect
   - Adding a new regression entry is a 30-line PR against `kernel_warnings.rs`; no schema or contract change required

8. **Pump-stop guard** (`profile.rs`, DEC-167): a control with a pump/CPU member
   may not be configured to stop. A non-zero `stop_pct` on such a control is
   rejected at profile-validate time (a `PUMP_STOP_FORBIDDEN` error in the
   validation report → `400 validation_error`); for any profile that reaches the
   engine un-validated (boot-load / hand-edit), the eval-time stop-snap is skipped
   for pump/CPU members. Stopping a pump risks coolant-flow loss and rapid thermal
   runaway. GPU- and chassis-only controls are unaffected.

9. **AIO / coolant surface, no coolant safety rule** (`hwmon/aio.rs`, DEC-156):
   liquid-cooler coolant temperatures are classified as the `CoolantTemp` sensor
   kind and AIO PWM headers carry an `is_aio` flag (surfaced via the dynamic
   `aio_hwmon` capability). This is detection only — there is **deliberately no
   coolant thermal-override rule**; the CPU-only `ThermalSafetyRule` is the sole
   emergency backstop. Scope is hwmon-only (USB-only coolers are out of scope).

10. **Engine liveness watchdog** (`sd_notify.rs`, DEC-387): the unit is
    `Type=notify` with `WatchdogSec=15`. DEC-266 restarts the daemon when the
    engine task *dies*; this covers the engine that is alive but no longer
    ticking — a deadlock, an await that never resolves, a starved runtime —
    which holds every fan at its last duty with no thermal ladder. The
    keep-alive (`WATCHDOG=1`) is sent from `TickCompletion::drop` and nowhere
    else, so it measures exactly "a tick completed". A slow or wedged *device*
    does not stop it: since DEC-289 the loop keeps ticking past a write that has
    not returned. On a timeout systemd asks the daemon to stop (`WatchdogSignal=SIGTERM`,
    DEC-388 — so the graceful stop, exit floor included, runs) and SIGKILLs it
    after `TimeoutAbortSec=10` if it cannot, then runs `ExecStopPost` and
    restarts it; restarts back off exponentially (3 s → 60 s) instead of
    hitting a start limit, and the daemon sends `RESTART_RESET=1` after five
    minutes of completed ticks so an unrelated later fault restarts fast again.
    `READY=1` is sent once the engine is ticking, the API is serving and SIGTERM
    reaches the graceful path; before it, each boot-time serial probe extends the
    start deadline by its own bound (`EXTEND_TIMEOUT_USEC`), so start-up is limited
    by progress rather than by how many serial devices the machine has.
    `STOPPING=1` with `WATCHDOG_USEC=0` opens every shutdown, because systemd
    re-arms its watchdog on any keep-alive whatever the unit's state, and a late
    tick would otherwise arm a fresh timer over the hardware restore; if systemd's
    queue refused that disarm, it is sent again once the IPC server has stopped,
    before the task drains (`TS-ay`, DEC-402), and once more just before the
    restore (`TS-ap`, DEC-396). System sleep (`TS-ao`, DEC-396): user space is frozen while devices
    suspend and resume, and that stretch counts against the watchdog, so the
    package's `system-sleep` hook sends `SIGUSR1` before a sleep and `SIGUSR2`
    after it. The daemon answers the first with `WATCHDOG_USEC=` widened to
    `max(configured, 120 s)` and acknowledges in `/run/control-ofc/sleep-hook.ack`
    (the hook holds the sleep until it does, for at most 2 s), and the second by
    putting the configured value back — or does that itself 120 s after the widen
    if no resume signal arrives. The hook signals only the PID the daemon wrote to
    `/run/control-ofc/sleep-hook.pid` once its handlers were live, and only while
    systemd reports it as the unit's MainPID, because `SIGUSR1`'s default action
    would kill a daemon that cannot handle it. No widen is sent once stopping.

11. **Exit floor** (`main.rs::apply_exit_floor`, DEC-388, `TS-j`/`TS-y`): on a
    clean stop, every output the daemon cannot give back to firmware is left at
    `max(its last duty, [shutdown] exit_floor_pct)` — default 50 %, settable live
    via `POST /config/exit-floor` — or at 100 % where the daemon wrote it but no
    longer knows its duty (a failed reply, a reconnect). That is each OpenFan
    channel the daemon has written (serial, no firmware curve) and each hwmon
    header with no `pwmN_enable`; outputs it never wrote are left alone, headers
    WITH a mode switch are DEC-382's hand-back, and `0` turns it off. It runs FIRST
    in the restore, because a watchdog stop's abort window is 10 s — enough while
    the hung engine is the only task that will not drain — and it latches: once it
    has run, `FanController::set_pwm` and `HwmonPwmController::set_pwm` (DEC-392)
    raise any lower command to it, so an OpenFan calibration still running inside
    its request, a verify restore, or an engine write that outlived the drains
    cannot take an output back down. `ExecStopPost`
    cannot repeat it — serial is out of its reach — so after a crash or SIGKILL
    those outputs keep their last duty. The hwmon hand-back that follows marks
    itself begun before its first write — as does the panic-time hand-back — and from
    then on `HwmonPwmController::set_pwm` refuses every write to a header WITH a mode
    switch (`HwmonControlError::ShuttingDown`, a retryable `503` at the API; DEC-420,
    `PTR-s`): the
    hand-back is the last writer, so a late engine or diagnostic write can no
    longer re-take a header it gave back. A write already past its last check and
    wedged in `write(2)` still lands when the driver lets it; `ExecStopPost`
    replays the record for that. A failed floor write, like a failed `set_pwm`
    duty write, leaves the next command to be written rather than coalesced
    (`TS-au`). On `arctic_fan` each floor write primes the device's unwritten
    channels first (DEC-425), so on a device whose cache a resume zeroed since the
    daemon's last write the step can outlast its 3 s bound (`BRD-t`).

12. **The stall/restart probe is the one diagnostic below 20 %** (`api::stall_probe`,
    DEC-407). Every other diagnostic clamps to `max(20, header floor)`, and still does.
    The probe is opt-in per header, takes no tunables and needs an explicit
    acknowledgement; it refuses a pump-protected header (the full union, checked before
    the display role), `cpu_fan` and `unknown` roles, and a machine with no fresh CPU
    temperature. Eligibility is re-checked before **every** write and on every sample,
    and a pump answer at any point raises the restore to the pump floor. Every sample
    runs the diagnostic gates plus a 5 °C rise gate on the hottest fresh CPU reading,
    each read is bounded at 2 s, an unreadable sample ends the run, and the time below
    20 % is budgeted from the header's own tach refresh (capped at 180 s). Every abort
    and cancel ends with a 100 % recovery kick — never while shutting down, when a
    write after the hand-back would re-take the header — and then the shared restore.

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
  runtime: `[profiles] search_dirs`, `[startup] delay_secs`,
  `[shutdown] exit_floor_pct` (DEC-388) and
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
- **Only `profiles.search_dirs` and `shutdown.exit_floor_pct` are re-applied live** (by their own POST handlers and on SIGHUP) — so `GET /config` reports them `requires_restart: false` and reads their running values from the live lock and the cache, not the startup snapshot. A SIGHUP reload runs under `config_write`, the lock every setter holds (DEC-412, `TS-aq`), in a task of its own so a setter's fsync cannot delay SIGTERM; before 2.55.0 a reload that read the files before a concurrent setter wrote them applied the stale value after it. Everything else
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
| GET | `/status` | Subsystem health + freshness; `thermal_state`; `unavailable_sensors[]` (present-but-unreadable sensors, DEC-193); `skipped_controls[]` (controls the engine cannot resolve, so is not commanding — 273-i); `runtime_config_degraded` (set when `runtime.toml` failed to load and the daemon is running on defaults — `AUD3-m`); `active_profile_id`/`active_profile_name` (active profile, DEC-194 — both **omitted** when none is active); `has_active_profile` (**always serialised**; the only field that tells "nothing is active" apart from "this daemon predates the mirror", so an absent KEY means a daemon < 2.45.0 while `false` is authoritative — DEC-355); `readiness` (compact cached hardware-readiness rollup for the GUI Dashboard chip — `{overall, critical, warning, info, top_summary, top_code}`, DEC-206); `validation_session` (the live session in miniature, DEC-317); `verify_active` (a verify / characterisation / calibration / validation sweep owns the engine's WRITE PAUSE — the engine keeps evaluating and keeps publishing `control_outputs[]`, it simply does not apply them, `WIRE-n`. **Not a safety signal:** the thermal force runs before this gate, DEC-297) |
| GET | `/sensors` | All temperature readings (each entry optionally carries a curated hwmon `thresholds` object — DEC-117; each also carries `control_eligible: bool` — DEC-193) |
| GET | `/fans` | Fan RPM + last commanded PWM (+ `stall_detected`, `fan_alarm`, `pwm_enable_mode`, `pwm_readback_pct` — the hardware readback, DEC-317 — and `pwm_commanded_pct` — the single-producer command, DEC-318; the two are the separate axes `last_commanded_pwm` conflates for an hwmon header) |
| GET | `/poll` | Batch: status (incl. `unavailable_sensors[]`, `skipped_controls[]`, `runtime_config_degraded`, `active_profile_*`, `readiness` rollup, `verify_active` — the engine is evaluating but NOT writing, `WIRE-n`) + sensors (incl. `control_eligible`) + fans |
| GET | `/sensors/history` | Per-entity time-series (ring buffer) |
| GET | `/capabilities` | Device list, feature flags, limits, `amd_gpu.kernel_warnings` (kernel-version regression catalogue, DEC-098). `control.min_supported_gui` is the **single source** of the GUI pairing floor (`WIRE-ac`, `constants::MIN_SUPPORTED_GUI`); `WIRE-k` added flags for five older features (`gpu_fan_verify`, `hardware_readiness`, `superio_port_probe`, `preferred_sensors`, `daemon_config_report`) — an absent flag means "this daemon predates the key", never "unsupported" |
| GET | `/config` | Effective merged configuration (DEC-243): per key its on-disk `value`, the `running_value` this process started with, `source` (`runtime`/`admin`/`default`), `mutable`, `requires_restart`, `restart_pending`, and `requires_privilege` where a drop-in is also needed. `/capabilities` carries no configuration at all — this is the only read side |
| GET | `/hwmon/headers` | Controllable motherboard PWM outputs |
| GET | `/profiles`, `/profiles/{id}` | Daemon-stored profiles (store of record — DEC-160) |
| GET | `/profile/active` | Current active profile or `{"active": false}` |
| GET | `/diagnostics/hardware` | Hardware readiness report (hwmon chips, GPU, thermal safety, kernel modules, ACPI conflicts, board info, and `board_firmware_counts` — the board's own firmware-declared fan/temp/volt counts where `it87` publishes the Gigabyte SIV, `X87-d`; a measurement beside `expected_chips`' DMI-table inference; compare against `hwmon.total_headers`, which is `pwmN`-capable headers only — monitor-only tachometers are disjoint and live on `/inventory/hwmon`). **DEC-405 (2.52.0):** also `kernel_release`, `board.bios_date` and per-module `version`/`srcversion`/`out_of_tree` (loaded modules only) — each `null` when absent, capped at 128 bytes |
| GET | `/inventory/hwmon` | Read-only structured inventory: temp sensors (each with a fine `classification`/`confidence`/`rationale` + an advisory `default_cpu`), controllable PWM headers, and monitor-only fan tachometers (`fanN_input` with no matching `pwmN`) |
| GET | `/diagnostics/preflight?header=&diagnostic=` | The daemon's own safety verdict for one header and one diagnostic, **before anything is driven** (DEC-333, 2.39.0+, `control.diagnostic_preflight`). Read-only: no lease, no slot, nothing reserved — a `ready` verdict describes *now*, and the diagnostic's own POST still runs its own guards. Returns `{verdict, checks[], blocking[]}` with `verdict` in `ready`\|`warn`\|`blocked`. A stale temperature source **blocks** every diagnostic since DEC-385 (`TS-q`), and each POST refuses on it from the same predicate — until then it only warned for verify and characterisation, whose handlers did not refuse. "Stale" is exactly the thermal ladder's own CPU trust window (5 poll intervals; DEC-395 removed the flat 10 s floor that let a diagnostic run on a reading the ladder had stopped acting on). `diagnostic=pwm_stall_probe` (DEC-407, 2.54.0+) adds a `stall_probe_eligible` row — the same eligibility rule the probe's POST refuses on — and only for that diagnostic |
| GET | `/inventory/cooling-devices` | Configured cooling-device topology + every device policy the daemon ships (DEC-316). Metadata — the profile engine never reads a device |
| GET | `/validation/session` | The current or most recent validation session in full — metadata, samples, event timeline, referenced diagnostics, findings (DEC-317). `404` when none has ever run. **DEC-405 (2.52.0):** verify evidence carries its real readings, its `result` token and `restore_failed`, and never maps to `fail` — a refused verify is `unavailable` |
| GET | `/validation/sessions`, `/validation/sessions/{id}` | The retained session index (last 5, newest first) and one session in full (DEC-317) |
| GET | `/inventory/readiness` | Structured hardware-readiness list (`items[]` with code/severity/component/action + blocks-flags; `overall` rollup). Read-only diagnose-and-guide |
| GET | `/inventory/superio` | Passive Super-I/O chip detection report — DMI/hwmon/`/proc/modules`/kmsg/ACPI evidence → per-chip presence + allowlisted driver recommendations; `port_probe_available` flags the opt-in active probe. Read-only, never touches an I/O port (DEC-202) |
| GET | `/inventory/hardware-readiness` | Combined readiness + Super-I/O snapshot from ONE shared passive scan (DEC-207): the readiness `rollup`/`overall`/`items`, the `superio` report, `scanned_age_ms`, and a monotonic `generation`. The GUI's merged "Cooling Hardware Readiness" page fetches this in a single request; `?refresh=true` forces a fresh (coalesced) scan. Read-only, 404-gated |

As of 2.0.0 the profile engine is the **sole writer** (DEC-159/DEC-165); the GUI sends intent (activate / override / identify) and a few diagnostics calls — there is no bare PWM write surface.

**Sensor quarantine (DEC-193, additive):** a sensor that is discovered but fails
every read (canonically an `ath12k`/`iwlwifi` WiFi temperature returning
`ENETDOWN` while the radio is off, or — since DEC-288 — one reporting an
implausible temperature outside [-50, 250]°C) is logged once, then evicted from `sensors`
and surfaced on `/status` + `/poll` as `unavailable_sensors[] = {id, label,
reason, unavailable_for_ms}`. Each live `sensors` entry also carries
`control_eligible: bool` (derived from `is_wireless_phy_chip(chip_name)`). Both
fields are additive — older clients ignore them; the GUI defaults
`control_eligible = true` and `unavailable_sensors = []` when absent.

**Bounded backend writes (DEC-289, daemon >= 2.23.5).** Each backend's blocking
write join is bounded to one tick, so a write wedged in a kernel driver cannot
freeze the engine loop — previously it did, and took thermal safety and *every
other backend* down with it while DEC-266's death supervision stayed silent (a
wedged task is alive, not dead). The wedged write is held and re-awaited, never
re-issued: `spawn_blocking` is uncancellable, so retrying each tick would strand
one blocking thread per tick. While a write is outstanding the engine records
`engine_writes_stalled_since`, and `/status`'s `engine` subsystem reports `warn`
then `crit` — without it a wedged writer would look healthy, because the loop is
now still ticking. **The GPU backend is not covered** (its task holds an owned
write lock, so handle retention would just move the freeze); tracked as `AUD-a2`.

**Controls that cannot be resolved (273-i).** A control whose curve will not
resolve is skipped — no command, fans hold their last duty. After three
consecutive skipped ticks it is logged once at WARN and surfaced on `/status` +
`/poll` as `skipped_controls[] = {control_id, control_name, reason,
skipped_for_ms}`, and logged once more when it resolves. `reason` is a stable
token (`curve_not_found` | `sensor_unavailable` | `mix_unresolvable` |
`sync_unresolvable` | `backend_unavailable`); the client owns the wording.
`backend_unavailable` (2.47.0, `OFN-j`) is the odd one out: the curve resolved
and an output was computed, and every member's BACKEND is absent, so the control
commands nothing — canonically an `openfan:` member with no OpenFanController
adopted, though an `hwmon:` member on a board with no writable header reports
identically from **2.49.0** (`OFN-ah`, DEC-376 — until then the engine took an
hwmon backend from any discovered header, so the hwmon half of this promise was
unreachable). From **2.55.0** it is resolved per MEMBER (`OFN-al`, DEC-412): an
`hwmon:` member is undeliverable when its header is outside the writable set
`HwmonBackend::new` measured (read-only, or never discovered on this board), so a
control bound only to such headers is listed on a board with writable ones too
— except on a boot where `HwmonBackend::new` could not take the controller lock
within 250 ms, which leaves the set unmeasured (`HwmonTargets::Unmeasured`) and
every `hwmon:` member deliverable, the per-backend answer, for that boot. Raised only when EVERY member is undeliverable; a partly-live
control is still commanding fans and is logged once per activation instead. Additive and omitted when
empty, so an older client sees the wire shape it always did and a newer client
reads `skipped_controls = []` from an older daemon. See Safety Model item 3.

**`stop_permitted` reports what identify does, not what the device policy says
(`AIO7-d`, fixed in 2.35.0).** `PwmHeaderEntry::from_descriptor` resolves one
policy for **every member** of a cooling device, so a radiator fan in an AIO
inherited `GENERIC_PUMP`'s `supports_stop: false` and was published unstoppable —
while identify branches on `header_is_pump_protected`, in which device membership
is not a term, and stopped it. The published value is now exactly
`!header_is_pump_protected`, pinned by
`stop_permitted_matches_identify_for_every_shipped_policy` (the sibling of the
floor honesty test, one field over). Do **not** reconcile the two by adding
membership to `header_is_pump_protected`: that would hand a 30% floor and
stop-refusal to every radiator and auxiliary fan in a device, which is a cooling
change rather than a reporting one.

**The floor field has since been brought into line the same way (`WIRE-b`, fixed
in 2.35.4).** `resolve_policy_floor` resolved `effective_min_pwm_pct` through the
device policy for every member too, so that same radiator fan published a 30%
floor no enforcement site applied — directly beside `stop_permitted: true`. It
now returns `0` for any header that is not pump-protected, exactly what an
ordinary chassis header already reported, and
`reported_floor_matches_enforced_floor_for_every_shipped_policy` asserts over
**both** values of `pump_protected` rather than only `true` — testing one branch
is why the invariant stayed green over the defect it exists to catch. Both fields
are now functions of `pump_protected` alone, but **that correlation is a property
of the shipped policy table, not a contract**: a future relaxing policy moves the
floor down toward the absolute backstop without changing `stop_permitted` at all,
so still do not derive one from the other.

**A diagnostic never restores a pump to a stop (`AUD3-l`, fixed in 2.35.0).**
`resolve_points` and `verify_test_duty` floor the duty written on the way *in*;
until 2.35.0 nothing floored the way *out*. Both restores wrote the captured
pre-sweep duty straight into `set_pwm`, which applies no floor, so a pump header
reading 0 was restored to 0 with `pwm_enable=1` asserted — a firmware-controlled
0 converted into a stopped pump with no writer. Both sites now clamp to
`max(HARD_PUMP_CPU_FLOOR_PCT, captured)` for pump-protected headers only; an
ordinary fan is still restored exactly as captured, 0 included.
Two boundaries stated rather than left implicit: a **CPU-labelled** header is
outside the clamp (`header_is_pump_protected` is `Pump` only, while the engine
floors CPU members at the same 30%) — `322-b`; and neither diagnostic restores
the captured `pwm_enable`, so a header taken from firmware control stays in
manual whatever duty it lands on — `322-c`. Both are pre-existing and recorded
rather than changed here. A consequence for clients: `restore_outcome:
"restored"` now means *restored, floor-clamped* on a pump, so the duty on the
hardware may exceed the original reported beside it.

**A diagnostic re-reads the pump union while it runs (`TS-aw`, DEC-418).** Verify,
characterisation and control-path discovery plan their duties from
`header_is_pump_protected` at entry. Pump evidence can arrive mid-run — a profile
naming the header a pump is activated (DEC-384's term), or it is assigned `pump`
(DEC-311) — and until DEC-418 none of the three reconsidered it, so a run planned for
an ordinary fan could keep driving a now-protected pump below 30 % until it ended.
One shared `diagnostic_gates::PumpWatch` now re-reads the union before every write and
on every 500 ms sample (verify's 6 s settle is sliced to match), always with no lock
held and never once shutdown has been seen. A flip **stops the run** — the user's
choice, DEC-407's `eligibility_lost` precedent, not clamp-and-continue: a
characterisation or discovery ends `aborted` with a `detail` saying why, and a verify
returns the seventh `result`, `pump_protected_mid_run`. `RestoreOnDrop` (and verify's
own restore) re-reads the watch once more before it writes, so protection that arrives
after the last sample still floors the restore at 30 %. **A hold whose time has elapsed is
a completed measurement** (the user's rule at review): each loop consults the watch only
while its window is open, so a flip seen as a window ends leaves the ordinary verdict and
only floors the restore — nothing is written in between. The watch is sticky: once seen
protected it stays so for the run. It also records the last duty the run wrote, for the
one restore with nothing to floor: a header whose pre-run duty was unreadable
(`no_original_duty`) is left where the run left it, as before, but raised to 30 % if it
has become a pump; discovery's closing return to the baseline is raised to the floor the
same way. `api::handlers::discovery::start_control_path_discovery` takes the monitor-only
fan walk's root, which is what lets a test drive a discovery run end to end. A header already protected at entry is a pump run
from the start and is never re-read. The stall probe keeps its own per-sample
eligibility re-check (DEC-407), which refuses a pump outright.

**A third parity oracle, `header_role_classification.json` (`AUD3-c`).** The GUI
hand-mirrors `classify_header_role`'s label branches — it must, because a daemon
< 2.31.0 publishes no `stop_permitted` and the reconstruction is then the only
answer. The copies agreed; what was missing was the *gate*. 29 cases of
`(chip_name, pwm_index, label) -> (role, pump_protected)`, asserted on both sides
and compared byte-for-byte by `parity.yml`, which now covers three fixtures.

**A runtime config that failed to load (`AUD3-m`, daemon >= 2.34.0).**
`RuntimeConfig::load_from` degrades to `Self::default()` when `runtime.toml`
cannot be read or parsed, so a corrupt file can never stop the daemon booting.
That fallback is deliberate and unchanged — but the defaults it returns carry
**no `header_roles`**, and on a board whose Super-I/O publishes no `pwmN_label`
files a user's `pump` assignment is the only evidence a header drives a pump.
A failed load therefore removes that header's 30% floor, its stop exemption and
its pump-safe identify, and until this field the entire notification was one
`warn!` in the journal: no endpoint reported it. `/status` + `/poll` now carry
`runtime_config_degraded = {reason, path, detail, phase}` — `reason` is
`unreadable` (I/O error, or over the 4 MiB read cap) or `malformed` (read, but
not valid TOML for this daemon version); `phase` is `startup`, `reload` or
`update` and says what the degradation cost, since a startup load seeds every
key while a SIGHUP reload commits only `profile_search_dirs`. `update` (2.51.0,
`TS-r`) is a `/config/*` setter that found the file unreadable:
`RuntimeConfig::load_for_update` hard-links the original to
`runtime.toml.invalid-<unix-ts>` and atomically replaces it with the header roles
and cooling devices the daemon is running with (`LiveAssignments`), **before the
setter runs** — so a setter that is then refused, or fails to write, still leaves
a readable file carrying every role, and the setters that rebuild those maps from
the file cannot drop one. Only keys that existed solely in the original are lost.
If the replacement cannot be written nothing on disk changes and the setter
answers `503`. A setter that finds no file at all starts from the same live maps
and publishes nothing. When several
phases fail the more severe record stands — `startup` > `update` > `reload` —
through the one function `runtime_config::record_degraded`. Additive and **omitted when
the config loaded cleanly**, so an older daemon's omission reads exactly as
"fine" — which is the same (absent) warning such a daemon shows today. A
*missing* file is not a degradation: that is first boot. The field is sticky for
the process lifetime, because nearly every runtime-mutable key is consumed once
at startup and a later successful write does not retroactively apply them.

**Atomic writes are concurrency-safe as of 2.34.0 (`AUD3-b`).**
`atomic_io::write_atomic` derives a **unique, hidden** scratch path per call
(`.{name}.tmp.{pid}.{counter}`). It used to be a fixed `{path}.tmp` opened with
`File::create`, which truncates — so two concurrent writers to one destination
shared a scratch file and could publish a document half-overwritten by the
other. Which writer wins the destination is still a race (that is the
`/config/*` lock's job, below); the winner's document is now always whole. All
five call sites — `runtime_config`, `daemon_state`, `profile_store`,
`validation::store`, `validation::recorder` — are covered without any of them
knowing, which is the point: the previous shape required every caller to carry
its own save lock and only one did.

**`/config/*` setters are serialised as of 2.34.0 (`AIO1-d`).** Every setter is
load the whole file → change one key → write it back → commit in memory, and
nothing ordered two of them: the later write won the file, the later commit won
the cache, and **both requests answered `updated: true`**. All twelve write
routes — through eleven acquisition sites, since the two preferred-sensor routes
share a helper — now take one `tokio::sync::Mutex` inside `runtime_for_update`,
held to the end of the handler, so a new setter cannot load the config without
serialising. The lock is
taken **first** and covers only leaf locks (`header_roles`, `cooling_devices`,
`profile_search_dirs`, `override_table`). It must never be held across
`hwmon_controller`, which the engine holds across a blocking sysfs write — that
holds by construction for the setters that validate against live headers (they
validate before acquiring), and by an explicit `drop` in
`update_header_role_handler`, whose *response* reads `resolved_header_role` and
would otherwise park every config route behind a wedged header. One further
subtlety: `/config/profile-search-dirs` derives its value from the current list
rather than from the request body, so its merge base is read **inside** the
guard; read outside, the lost update survives the lock. The consequence being prevented is
asymmetric: a `/config/cooling-device` write landing from a stale base dropped
the `/config/header-role` edit before it, i.e. a pump's 30% floor, and since
2.31.0 the GUI posts those two back to back in a single Configure-AIO action.

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
| POST | `/fans/{fan_id}/identify` | Per-fan identify hold/restore — 0 for an ordinary fan (floor-exempt), a floored perturbation for a pump-protected header (DEC-311/312/384); deadman auto-restore (DEC-166) |
| POST | `/config/header-role` | Assign or clear one PWM header's role (DEC-311). `{"header_id","role"}`; `role: null` clears |
| POST | `/config/cooling-device` | Create or replace one cooling device by id (DEC-316). Safety numbers are **not** settable — a policy is chosen with `device_policy_id` and `minimum_safe_pwm` & siblings are rejected by name |
| DELETE | `/config/cooling-device/{id}` | Remove one cooling device (DEC-316). `404` when no device has that id |

The identify `stop` on the world-writable socket (0666, DEC-049) lets any local
user hold any fan at its identify duty by re-issuing `stop` inside the
deadman window. **A header the daemon knows to be a pump can no longer be held at 0** (DEC-311)
— the daemon substitutes a floored perturbation regardless of what the client asks
for. "Knows to be a pump" is a union of the header's own evidence (a `PUMP`-ish
label, or channel 1 of a liquid-cooler chip), the user's `POST /config/header-role`
assignment, and — since DEC-384 — a member of the active profile bound to the
header whose label names a pump (`pump`/`aio`, the evidence the 30 % floor already
acts on), so an assignment or a profile can add that protection but not remove it.
A header with none of these is treated as an ordinary fan and stopped. **Accepted, bounded risk** (2026-07-21 audit: accept + document): identification
requires stopping any fan by design (DEC-166); the deadman auto-restore limits an abandoned stop
to one TTL; and a thermal emergency outranks the identify overlay entirely — the engine's
`force_all_with_floor` path drives every OpenFan channel + writable hwmon header the machine has to **at
least** 100 % in an emergency (and, below 100 %, every output the profile controls — DEC-382), spinning a stalled pump back up regardless of
standing stops. Since DEC-307 that duty is a floor over the profile's own output rather than a
replacement for it, so a control already asking for more keeps its higher duty; an output no control
commands still gets the forced duty, which is what keeps the reach above true.

### Write endpoints — GPU

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/gpu/{gpu_id}/fan/reset` | Restore GPU fan to automatic / re-enable zero-RPM |
| POST | `/gpu/{gpu_id}/fan/verify` | Test GPU fan-control effectiveness (~6s, no lease; detects ppfeaturemask/SMU/BIOS silent failures). Not gated on stale temperatures, unlike the hwmon verify (DEC-385): the GPU is outside the ladder and the test biases upward |

### Write endpoints — hwmon

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/hwmon/{header_id}/verify` | Test PWM write effectiveness (~6s; daemon uses its own internal lease, detects BIOS/EC interference) |
| POST | `/hwmon/{header_id}/characterize` | Start a PWM/RPM response sweep (AIO-MB Phase 3, DEC-313). Returns `202` and runs detached; alongside the quick verify, never replacing it. Points clamped to `[max(20, header floor)..100]` — **0% is unreachable** — and swept ascending. Since 2.40.0 (DEC-334) `bidirectional` walks them down from the top and back up, ending high, and `stability_seconds` adds a dwell at up to 3 daemon-chosen duties; both gated on `control.pwm_behaviour_characterization`. **DEC-411 (2.55.0):** a settle must not be a one-way ramp; `rpm_verdict` is judged against the point's own settled σ; the run carries `current_step` while running. **DEC-405 (2.52.0):** settling is judged on tach-register updates and never before the first one; per-point `stability` covers the settled tail only (`window_start_ms`; `not_settled` when a point never settled); `measurement_resolution_ms` is the register's cadence (`update_interval`, else the median observed gap), not the 500 ms sampler; `monotonic` is judged per leg (`monotonic_falling`/`_rising`); the default settle is 12 s |
| GET | `/diagnostics/characterization` | Current or most recent characterisation run, including points measured so far (live progress) |
| DELETE | `/diagnostics/characterization` | Ask a running sweep to stop; the header is restored either way, except where something with more authority owns it (a thermal force, or shutdown) — reported as `restore_outcome`, never as a silent success |
| POST | `/hwmon/{header_id}/discover-control-path` | Establish which tach channel(s) this PWM output actually drives, **by measurement rather than by sysfs numbering** (AIO Phase 8 Batch 1, DEC-333, 2.39.0+, `control.control_path_discovery`). Returns `202` and runs detached; poll `GET /diagnostics/control-path`. Optional `{"delta_pct", "cycles", "window_seconds"}`, all clamped server-side. Deliberately **not** `pwmconfig`'s stop-the-fan model: the perturbation moves away from the nearer rail so there is always headroom, every commanded duty is clamped into `[max(20, header floor)..100]` — **0% is unreachable for any header** — and a pump-protected header never crosses its 30% floor. Claims the **same** single-flight verify slot as verify/calibrate/characterize, so at most one of the four ever drives hardware. **DEC-336 (2.42.0):** refuses with `409 validation_error` and aborts a run in flight when every temperature reading is stale — the refusal `GET /diagnostics/preflight` publishes, built from the same predicate. **DEC-339 (2.43.1):** that gate and the two beside it (85 °C voluntary abort, thermal-ladder force) run before **every** PWM write rather than once per cycle — a cycle issues two writes, so the second one used to be made on a reading up to one observation window old; the thermal cadence is now exactly the keepalive cadence. The end-of-run return-to-baseline write also obeys the force skip, so it can no longer move a header off a duty the ladder is holding **DEC-405 (2.52.0):** before any baseline window whose write moved the duty the run waits, bounded at 15 s, for every channel that can move to settle, as its own gated window (renewal + thermal gates again before the baseline) — so a recovery ramp is no longer measured as noise (`baseline_settled`, `settle_wait_ms`, `noise_floor_from_cycle_1` when a channel could not settle and borrowed cycle 1's floor); the wait's last reading gets the reclaim / lost-pump-tach check, and a cancel is honoured at every window boundary. Resolution is the header's own tach's median update interval; the default window is 12 s |
| GET | `/diagnostics/control-path` | Current or most recent discovery run, **plus every persisted relationship**. Records survive a restart, are keyed by the header's stable id — so a board or driver change invalidates one by construction — and are pruned at boot to whatever discovery still sees. `no_tach_response` is a legitimate result and **not** a fault: the header may drive no tach-reporting device, or one running under its own internal control |
| DELETE | `/diagnostics/control-path` | Ask a running discovery to stop. Same cooperative-cancel and restore semantics, and the same two deliberate skips, as the characterisation sweep above |
| POST | `/hwmon/{header_id}/stall-probe` | **`[SAFETY]` The only diagnostic that writes below 20 %** (DEC-407, 2.54.0+, `control.stall_probe`): finds a fan's stall and restart duties. Body must be exactly `{"acknowledge_below_floor": true}` — no tunables. Refuses a pump-protected header (the full union), `cpu_fan`, an `unknown` role, read-only, no tach, or no fresh CPU temperature, and re-checks eligibility before every write and on every sample. 20 % baseline (measures the tach refresh; a fan spinning before that reads 0 there is `stalled_at_or_above_20`), then 18 → 0 % in 2-point steps to a stall, then up to 20 % to a restart; dwell and budget derived from the refresh, budget capped at 180 s. Aborts on the diagnostic gates, a 5 °C CPU rise, lost eligibility, the budget, a reclaim, an unreadable tach sample, a read that does not return within 2 s, or a cancel — each ending with a 100 % recovery kick (never while shutting down), then the restore. Same single-flight slot. See `api::stall_probe` |
| GET | `/diagnostics/stall-probe` | Current or most recent probe run, in memory only: `outcome`, `abort_reason`, stall/restart duties, the derived timing, every held point, and the restore tokens |
| DELETE | `/diagnostics/stall-probe` | Ask a running probe to stop — honoured on the next sample, then the kick and the restore |
| POST | `/hwmon/rescan` | Re-enumerate hwmon devices and return fresh header list |
| POST | `/fans/openfan/rescan` | Look for an OpenFanController and adopt it without a restart (DEC-265) |

### Write endpoints — inventory (opt-in active probe)

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/inventory/superio/probe` | Opt-in active Super-I/O `/dev/port` probe (DEC-203) — a deliberate one-shot that identifies an UNBOUND chip so the user can be told which driver to load. Refuses unless `[detection] allow_port_probe` + `CAP_SYS_RAWIO`; skips ports claimed by a driver/ACPI; single-flight + 10 s cooldown. Reads the DEVID with no unlock and writes one only on `0xffff` (DEC-332), and on a board the DMI table says is ITE-only it **withholds the Nuvoton `0x87,0x87` leg entirely** — the sequence that latches the eSPI→LPC bridge, keyed on the same board list as the shipped modprobe guard (`X87-k`); the skip is reported in `notes[]`. Returns the `/inventory/superio` shape enriched with probe hits |

### Write endpoints — validation sessions (DEC-317, AIO-MB Phase 5)

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/validation/session` | Start recording against a configured cooling device. Optionally **orchestrates** `pwm_verify` / `pwm_characterization` against named `sweep_members` (default: the pump member). `409` if one is already recording. `stop_when_diagnostics_complete` (2.43.0+, `control.validation_auto_stop`) makes the session finalise itself when its orchestration walk completes — see **How a session ends** below; `400 validation_error` if it is set with an empty `diagnostics[]` |
| POST | `/validation/session/stop` | Finalise and compute the evidence summary. **Also ends the diagnostic the session started** — see below. `404` only when no session has ever been started; `500 internal_error` if the finaliser itself broke, in which case the session is **still recording** |
| DELETE | `/validation/session` | Finalise and persist, recording the session as `cancelled`. **Not a discard** (`P8-bc`): `cancel()` is `finish(STATE_CANCELLED)` and finalises exactly as `stop` does — same findings, same samples, same persistence; only `state` differs. Same three outcomes as `stop` |
| POST | `/validation/session/event` | Place a user marker on the timeline |
| POST | `/validation/session/measurement` | Attach an external measurement — **untrusted; no control path reads one** |

**How a session ends (`P8-az`, `P8-bf`).** No document said this before daemon 2.43.0, and
its absence was the defect: a session **records until it is stopped**, and finishing the
diagnostics it orchestrated does not stop it. The daemon's only unprompted finalise is the
sample cap — `VALIDATION_MAX_SAMPLES` (7200) x `VALIDATION_SAMPLE_INTERVAL` (1 s), a flat
**two hours**, and `max_samples_for` does not shorten it for a real cooler. With every
diagnostic requested the orchestration finishes in ~4 min (verify ~10 s, basic sweep ~50 s,
behaviour sweep ~2.5 min, discovery ~25 s, per member) and the recorder then runs for the
remaining ~1 h 56 m. That passive tail is the point of the feature — it is what captures a
workload — but nothing told the operator it was there.

So there are three ways one ends, and since 2.43.0 the third is the caller's choice:

1. `POST /validation/session/stop` — finalise and keep the evidence.
2. `DELETE /validation/session` — the same, recorded as `cancelled`. Not a discard.
3. `stop_when_diagnostics_complete: true` at start — `spawn_orchestration` finalises the
   session on its way out. **Only on normal completion**: neither the shutdown early-return
   nor the superseded early-return takes the hop, because a `completed` stamped on the way
   out of a shutdown would be a fabricated verdict where the boot sweep would otherwise
   report `interrupted` honestly, and a superseded task has nothing of its own left to stop.
   The finalise is `stop_if`, fenced on the session id under the same guard — an operator can
   stop and restart in the gap, and an unfenced `stop()` would finalise *their* session. It
   runs through `spawn_blocking` (`AUD3-n`), and prunes afterwards exactly as the manual stop
   does, so an auto-stop is indistinguishable from the operator pressing Stop. Default
   `false`, so a client that does not send it sees the behaviour above unchanged.

**A session is an observer that may orchestrate, and never a second writer.** The recorder
plants no hooks in the engine or the write path, and every sysfs read it performs is
**read-only and outside the session slot guard** — the narrowed invariant that replaced
"performs no sysfs I/O" when DEC-335 added power sampling (`P8-ao`), and which DEC-342
(`P8-v`) made true at `start` as well as in `tick`. Where a session
runs a diagnostic it invokes the **existing** verify/characterize handler, which already owns
the hwmon lease, the pump floor clamp, the thermal refusal and restore-on-drop. There is no
code in `validation/` or `api/handlers/validation.rs` that commands a duty, and tests assert
that absence rather than leaving it to review.

**Ending a session ends the diagnostic it started** (`AUD3-j`, daemon 2.35.1). A
characterisation sweep runs detached and renews the engine's write-pause once per point, so
until 2.35.1 stop/cancel finalised the record while the sweep kept driving the header — and
kept curve control suspended — for up to `CHARACTERIZATION_MAX_POINTS ×
CHARACTERIZATION_SETTLE_MAX_S` afterwards. The orchestrator now asks the sweep to stop,
**fenced on the `run_id` it was handed at 202**, so a run started by anyone else is never
aborted. The cancel is cooperative, as `DELETE /diagnostics/characterization` has always
been: the current point finishes its settle and the header is restored, which is what the
caller actually wants. `GET /diagnostics/characterization` therefore reports `cancelled` for
that run. Thermal safety never depended on this — the forced-duty branch runs *above* the
`verify_active` gate, so a paused engine still floors every output.

### Write endpoints — profile / control / config

| Method | Path | Purpose |
|--------|------|---------|
| POST/PUT/DELETE | `/profiles`, `/profiles/{id}` | Profile CRUD + `?validate_only` — daemon is the store of record (DEC-160) |
| POST | `/profile/activate` | Switch active profile by id or path; clears all active control-overrides, not identify holds (DEC-189) — except an identify stop on a header the new profile names a pump, which is released (DEC-394) |
| POST | `/profile/deactivate` | Clear active profile (DEC-097); also clears all active control-overrides, not identify holds (DEC-218, ≥ 2.12.0); idempotent |
| POST | `/control/{control_id}/override` (+`/override/renew`, `DELETE`) | Expiring manual override — floor-clamped, deadman, monotonic fencing (DEC-163); cleared on profile activation/deactivation (DEC-189/DEC-218) |
| POST | `/config/profile-search-dirs` | Edit the profile search path: `{"add": [...]}` and/or `{"remove": [...]}`, at least one required. Removals apply before additions, so `add`+`remove` is one atomic "move" (DEC-285, `remove` is ≥ 2.23.0 and gated by `control.profile_search_dir_remove`). `/etc/control-ofc/profiles` and the last remaining entry cannot be removed. Applies live; persists to `runtime.toml`; 503 `persistence_failed` on write error |
| POST | `/config/poll-interval` | Set the sensor/fan poll interval, 250-2000 ms (DEC-243; persists to `runtime.toml`, restart to apply). **[SAFETY]** the ceiling bounds how stale a temperature the thermal-emergency rule can act on |
| POST | `/config/serial-port` | Set the OpenFan serial device (`null` = auto-detect). Validated against the transport's own allowlist and capped at 256 chars; a configured port that fails to open **or fails to answer the `ReadAllRpm` handshake** falls back to auto-detection, so neither a bad value nor a wrong-but-openable device can remove OpenFan control. DEC-243 / DEC-250; restart to apply |
| POST | `/config/serial-timeout` | Set the serial read timeout, 50-1000 ms (DEC-243; restart to apply). **[SAFETY]** bounds emergency `force_all_with_floor` latency |
| POST | `/config/exit-floor` | Set the exit floor, `{"exit_floor_pct": 0-100}` (DEC-388): the lowest speed a clean stop leaves a fan the daemon cannot hand back to firmware at. **Applies live** — persisted, then put in force — so `GET /config` reports `shutdown.exit_floor_pct` with `requires_restart: false`. Gated by `control.exit_floor`; 503 `persistence_failed` on write error |
| POST | `/config/allow-port-probe` | Opt into the active Super-I/O probe (DEC-243). **Also needs the `CAP_SYS_RAWIO` drop-in** — the flag alone does not enable it |
| POST | `/config/nvidia-telemetry` | Opt into read-only NVML telemetry (DEC-243). **Also needs the `/dev/nvidia*` drop-in** |
| POST | `/config/startup-delay` | Set startup delay seconds, 0-30 (persists to `runtime.toml`, takes effect on restart; 503 `persistence_failed` on write error). Since 2.23.0 the reply also carries the shared DEC-243 setter shape (`key`/`value`) alongside the original `delay_secs`, so one client parser covers every `POST /config/*` |
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


## AIO Phase 8 Batch 3a — thermal observation and steady state (DEC-335, v2.41.0)

Three additions, all of them **recorders or pure derivations**. Nothing here writes hardware,
claims the verify slot, takes the hwmon lease, or can suppress a forced duty or lower a floor.

**`hwmon/power.rs` (new).** CPU package power, read only inside a validation session and never
on the 1 Hz poll. Two sources: a CPU chip's hwmon `powerN_input`/`powerN_average` (allow-listed
chips only — a bare "any non-GPU chip" filter would report a PSU or VRM rail as CPU package
power), else a powercap RAPL `package-*` zone's `energy_uj`. Subzones (`core`, `uncore`, `dram`)
are excluded: they measure a *part* of the package.

Three RAPL properties shape the module and are worth knowing before touching it:

- it is a cumulative **energy** counter, so one reading is not a power and the first sample of a
  session yields `None`;
- it **wraps often** — `max_energy_range_uj` is 65 532.6 J on the reference host, i.e. every
  ~5.5 min at 200 W, ~22 times in a two-hour observation. Wrap handling is a routine path, and
  the arithmetic is a pure clock-free function so the wrap is testable at all;
- a counter **reset** is indistinguishable from a wrap by inspection, so a plausibility ceiling
  (`POWER_MAX_PLAUSIBLE_W`) discards the resulting spike. The residual blind spot is recorded as
  `P8-l`.

GPU power is read through `gpu_detect::read_gpu_power_w` from the path the GPU subsystem already
resolved — **not** from the hwmon walk, which excludes `amdgpu` by design.

**`api/stats.rs` gained the temperature-domain twin its own header promised.** `steady_state()`
applies a least-squares slope over a rolling window plus a variance band, held over two
consecutive non-overlapping windows, after a minimum observation. Conservative on purpose:
`insufficient_data` and `not_established` are distinct answers, and neither is ever a fault.
The criterion string is **built from the constants** and travels with every verdict.

**`validation/summary.rs` derives two new things at finalisation.** `derive_analysis()` fills the
per-member startup fingerprint and the steady-state result before `summarise()` runs, because two
findings read what it writes. A startup override whose commanded duty was honoured is reported as
a *device behaviour* — never as failed PWM control.

**The opt-in startup auto-record** (`[startup] record_startup` in `daemon.toml`, default `false`) is the only
autonomous behaviour added. `ValidationEngine::start` pre-empts it for any operator-started
session, finalising rather than discarding it, and `store::prune` retains auto-records in their
own slot. Both properties are validity-checked in `validation_phase5.rs`. It is also the first
and only emitter of `daemon_restart_observed`, a token that has existed since Phase 5 with no
producer.

New capability: `control.thermal_observation`. **Required, not a convenience** — an unrecognised
session `kind` falls back to `"validation"` and returns 200, so without the flag a client cannot
tell a supporting daemon from an older one.
