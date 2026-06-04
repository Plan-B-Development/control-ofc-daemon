# Changelog

## [1.12.1] — 2026-06-05

### Internal
- Deduplicated the verify-wait constant. The hwmon and GPU verify endpoints now
  share a single `constants::VERIFY_WAIT_SECONDS` (6 s) — `api/handlers/hwmon_ctl.rs`
  aliases it instead of defining a parallel `6` literal, and `gpu.rs` references
  the same constant — so the two settle windows can no longer silently drift
  apart (which would have violated the GUI's `VERIFY_PAUSE_SAFETY_MS` ≥9 s
  headroom). Behaviour is unchanged (still 6 s); the compile-time
  `assert!(VERIFY_WAIT_SECONDS >= 4)` floor is retained. (DEC-101)

## [1.12.0] — 2026-06-04

Intel discrete GPU (Arc) monitoring (**DEC-121**, daemon-relevant subset). Pairs
with **GUI v1.24.0**. Additive wire-contract change — older GUIs ignore the new
fields.

### Added
- **Intel discrete GPU detection** (`hwmon::intel_gpu_detect`) for the `xe`
  (Battlemage / Arc B-series) and `i915` (Alchemist / Arc A-series) drivers.
  Detection keys on the hwmon chip name, which both drivers register **only for
  discrete GPUs** — an unambiguous discrete-GPU signal.
- New sensor `source: "intel_gpu"` (kind `gpu_temp`) for `xe`/`i915` temps;
  `SensorSource::IntelGpu` / `DeviceLabel::IntelGpu`.
- New read-only GPU fan entities `intel_gpu:{pci_bdf}` in `GET /fans` (RPM from
  `fan1_input`; never a commanded PWM).
- Additive `intel_gpu` object on `GET /capabilities` and an `intel_gpu` block on
  `GET /diagnostics/hardware` (with a truthful firmware-managed explanation).

### Notes
- **Read-only by design.** The kernel `xe`/`i915` hwmon interface exposes fan
  RPM and temperature only — there is no PWM/write path (fan control is
  firmware-managed). `fan_control_method` is always `"read_only"` or `"none"`;
  no lease, PMFW curve, ppfeaturemask, overdrive, or shutdown reset apply.
- Only device ID `0xE20B` (Arc B580) maps to a marketing name; other IDs report
  the generic "Intel D-GPU".

## [1.11.0] — 2026-06-04

GPU fan active verification endpoint (**DEC-120**, daemon-relevant subset).
Pairs with **GUI v1.23.0**. Additive wire-contract change — older GUIs ignore
the new route.

### Added
- `POST /gpu/{gpu_id}/fan/verify` (no lease) — drives a test speed biased
  upward (so cooling is never reduced on a hot GPU), waits
  `GPU_VERIFY_WAIT_SECONDS = 6 s`, reads back the applied PMFW `fan_curve` (or
  legacy `pwm1`) plus `fan1_input` RPM and `fan_zero_rpm_enable`, restores the
  prior state, and classifies the outcome: `effective`, `curve_not_applied`,
  `no_rpm_effect`, `zero_rpm_suppressed`, `rpm_unavailable`, `write_failed`, or
  (legacy path) `pwm_enable_reverted`. Catches the silent GPU failures static
  diagnostics miss (`ppfeaturemask` bit 14 unset, SMU mismatch, BIOS overdrive
  lock) without flagging a healthy zero-RPM idle or the OD_RANGE clamp as a
  fault.
- `hwmon::gpu_fan` helpers `flat_speed_pct`, `parse_zero_rpm_enabled` /
  `read_zero_rpm_enabled`; constant `GPU_VERIFY_WAIT_SECONDS`.

## [1.10.0] — 2026-06-04

GPU detection/diagnostics hardening + headless per-member GPU floor
(**DEC-119**, daemon-relevant subset). Pairs with **GUI v1.22.0**. The
diagnostics additions are an additive wire-contract change — older GUIs ignore
the new keys.

### Changed
- **The profile engine no longer soft-floors GPU fans in a mixed control.**
  When a profile runs headless, a GPU member grouped with chassis/CPU fans now
  idles to its own 0% floor while the non-GPU members keep the control's
  `minimum_pct` — the same per-member flooring the GUI does, so headless and
  GUI-driven evaluation stay consistent (DEC-096). The PMFW write path still
  clamps to the firmware OD_RANGE (~15%) and honours `fan_zero_rpm`.

### Added
- **`/diagnostics/hardware` now reports AMD GPUs that exist in PCI space but
  have no `amdgpu` driver bound.** New top-level `amd_pci_devices` (per-device
  `pci_bdf` / `pci_device_id` / `driver` / `amdgpu_bound` / `hwmon_present`)
  scans `/sys/bus/pci/devices` independently of hwmon, so a blacklisted /
  KMS-failed / vfio-pci-passed-through GPU — which produces no hwmon node and
  was previously invisible — is now surfaced. New top-level
  `amdgpu_module_loaded` (`/sys/module/amdgpu`) distinguishes "module not
  loaded" from "loaded but unbound".
- **`GpuDiagnostics` gains firmware-context fields:** `fan_speed_min_pct` /
  `fan_speed_max_pct` (the PMFW `fan_curve` `OD_RANGE` fan-speed bounds — the
  firmware-enforced ~15% minimum), `fan_minimum_pwm` (best-effort parse of
  `gpu_od/fan_ctrl/fan_minimum_pwm`), `amdgpu_driver_bound`, and
  `kernel_warnings` (the same advisory catalogue as
  `/capabilities.amd_gpu.kernel_warnings`, duplicated so the support bundle is
  self-contained).

All new fields use `#[serde(default)]` / `skip_serializing_if`, so the change
is additive and non-breaking. No new dependencies; the daemon never writes
`fan_minimum_pwm`.

## [1.9.0] — 2026-06-03

Curated hwmon temperature-threshold attributes on `SensorEntry` (**DEC-117**).
Pairs with **GUI v1.20.0**, which surfaces the new fields in the
Diagnostics > Sensors detail dialog and the inline alarm chip. Additive
wire-contract change — older GUIs ignore the new key.

### Added
- **`thresholds: Option<SensorThresholdsResponse>` on every `SensorEntry`**
  in `/sensors` and `/poll`. The daemon reads a curated subset of
  hwmon temperature-threshold sysfs attributes once at discovery time and
  on every `POST /hwmon/rescan`:
  - `tempN_max`, `tempN_min`, `tempN_crit`, `tempN_crit_hyst`,
    `tempN_emergency`, `tempN_emergency_hyst`, `tempN_lcrit`,
    `tempN_offset` — temperature thresholds in °C.
  - `tempN_alarm`, `tempN_max_alarm`, `tempN_crit_alarm`,
    `tempN_fault` — alarm/fault bits (sampled at discovery, not refreshed
    per poll cycle).
- **Daemon-side plausibility filter** for threshold values. Anything
  outside `[-50, 200] °C` is dropped (catches kernel INT_MIN/INT_MAX
  placeholders), and the `it87`-family `tempN_max == 0` "register not
  configured" sentinel is dropped specifically for `it8*` chips. Every
  threshold sub-field uses `#[serde(skip_serializing_if = "Option::is_none")]`
  so the wire shape is the minimal honest set — a sensor with only `crit`
  configured emits `{"thresholds": {"crit_c": 105.0}}`, not 12 null fields.
- New `SensorThresholds` struct in `hwmon::types` + serialisation twin
  `SensorThresholdsResponse` in `api::responses`. The values propagate
  through `SensorReading` → `polling::to_cached` → `CachedSensorReading`
  → `build_sensor_entries` → `SensorEntry`.

### Tested
- 6 new unit tests in `hwmon::discovery` covering the curated attribute
  set, k10temp-empty handling, garbage-value filtering, the `it87`
  `max=0` quirk (and that the filter is scoped to `it87`-family chips
  only), alarm-bit reading, and graceful handling of a malformed alarm
  bit. 1 new schema test in `api::responses` verifying the JSON shape
  with and without thresholds.
- All 458 daemon unit tests pass.

## [1.8.4] — 2026-06-03

Internal efficiency and code-health fixes from a full cross-stack audit. No
behaviour or wire-contract changes; pairs with **GUI v1.19.1**.

### Changed
- **`/diagnostics/hardware` no longer blocks the async runtime.** The handler's
  ~6 blocking sysfs/procfs reads (`/proc/modules`, `/proc/ioports`, DMI,
  `/proc/cpuinfo`, kmsg, `ppfeaturemask`) now run on `spawn_blocking`, mirroring
  the OpenFan write handlers (DEC-099), so a slow read can't stall a Tokio worker.
- **Profile-engine 1 Hz loop trims per-tick state clones.** Added cheap
  `StateCache::gui_active()` / `sensors_snapshot()` / `gpu_fans_snapshot()`
  accessors; the GUI-active + GPU-write-suppression site no longer deep-clones
  the full sensor map every tick just to read a bool and the GPU-fan map.
- **`take_resume_flag()` is now the single resume-flag consumer.** The hwmon PWM
  controller calls it instead of an inline `resume_detected.swap()`, removing the
  duplicated atomic incantation.

### Tested
- New unit tests for the three `StateCache` accessors and `take_resume_flag`'s
  swap-and-clear semantics, plus an end-to-end IPC test for
  `GET /diagnostics/hardware`.

## [1.8.3] — 2026-06-02

Kernel-warning catalogue correctness fix (**DEC-114**), from a cross-repo
documentation audit that re-verified every externally-sourced hardware
claim against primary sources. Safety-relevant: the RDNA3/RDNA4 hard-hang
warning was too narrow and recommended an also-affected kernel. Pairs with
**GUI v1.19.0** (which bundles the never-separately-released v1.18.1), landing
the matching guidance text, a device-ID fix, and the doc corrections + citations.

### Fixed
- **RDNA3/RDNA4 hard-hang warning now covers kernel 6.18 _and_ 6.19.** The
  regression affects both series (Phoronix EOY 2025; ROCm #6101 reports
  kernel panics on 6.18.20 and 6.19.10), but `detect_kernel_warnings` only
  fired on 6.19.x and the message recommended "roll back to 6.18 LTS" — an
  also-affected kernel. It now fires on 6.18.x and 6.19.x and recommends a
  verified-safe **6.15–6.17** longterm kernel. The id was renamed
  `rdna_hang_kernel_6_19_x` → `rdna_hang_kernel_6_18_6_19` so the GUI
  re-prompts users who acknowledged the earlier, unsafe advice.
- **R9700 SMU-mismatch warning re-characterised and de-scoped from 7.0.x.**
  ROCm #6101 is an SMU interface-version mismatch (firmware v50 vs driver
  v46) that leaves no working fan-control path — `pwm1` is read-only and
  commanded changes have no effect — and it persists across every tested
  kernel (6.14, 6.17, 7.0), not just 7.0.x. The warning is now scoped by PCI
  device ID (`0x7551`) rather than kernel version (suppressed only inside the
  6.18/6.19 hang range, where the hang warning dominates), the "accepts
  writes but silently ignores them" wording is corrected, and the id was
  renamed `smu_mismatch_navi48_r9700_kernel_7_0` → `smu_mismatch_navi48_r9700`.
- **nct6687/nct6775 collision wording.** The diagnostics banner and the
  `ModuleCollisionInfo` doc now state that the out-of-tree driver's `0xd450`
  claim was _historical_ and was removed upstream in Fred78290/nct6687d
  PR #164 (2026); the detector still fires (already-loaded modules and
  not-yet-updated packages remain at risk) and the remediation now points to
  updating the driver as the durable fix.

### Tested
- `kernel_warnings` tests updated for the new ranges and ids: `rdna4_on_6_18`
  now asserts the hang warning fires (was: asserts empty — this is the
  unsafe-regression guard), `r9700_on_7_1` now asserts the SMU warning
  persists, and new `r9700_on_6_18_warns_hang_only` / `r9700_on_6_17_warns_smu`
  cover hang-vs-SMU precedence and the broadened device scope.

## [1.8.2] — 2026-06-01

Intel platform foundation (DEC-110): adds CPU vendor detection on
`/diagnostics/hardware` and registers `intel_pch_thermal` in
`KNOWN_MODULES`. No control-loop, lease, or write-path behaviour
changes; additive wire-shape only. Pairs with **GUI v1.15.0**, which
consumes the new field for platform-scoped vendor quirks and ships
the Intel motherboard fan-control guide.

### Added
- **`cpu_vendor: String` field on `HardwareDiagnosticsResponse`**,
  populated by a new `read_cpu_vendor()` helper that parses the
  first `vendor_id` line from `/proc/cpuinfo`. Normalises
  `"GenuineIntel"` → `"Intel"`, `"AuthenticAMD"` and
  `"HygonGenuine"` → `"AMD"`, anything else → `""`. Serialised
  with `skip_serializing_if = "String::is_empty"` so the wire is
  unchanged when detection fails (hypervisors, unreadable file).
- **`intel_pch_thermal` row in `KNOWN_MODULES`** (`in_mainline =
  true`). The driver registers a hwmon device exposing the PCH
  temperature as `temp1_input`; it is sensor enrichment only, not
  a fan-control path. Adding it lets the diagnostics modules table
  honestly report the loaded module on Intel systems.

### Documented (in-code rationale)
- **`x86_pkg_temp` deliberately excluded** from `KNOWN_MODULES` with
  an inline comment. The kernel `x86_pkg_temp_thermal` driver
  registers with `.no_hwmon = true` and only appears as a thermal
  zone, never under `/sys/class/hwmon`. `coretemp` is the correct
  hwmon source for Intel CPU package temperature.

### Tested
- Nine new diagnostics unit tests (`api/diagnostics.rs` test
  module): GenuineIntel → "Intel", AuthenticAMD → "AMD",
  HygonGenuine → "AMD", KVM hypervisor → "", missing vendor_id →
  "", unreadable file → "", multi-CPU first-match selection,
  `intel_pch_thermal` present in `KNOWN_MODULES` with mainline=true,
  `x86_pkg_temp` confirmed absent from `KNOWN_MODULES`.

## [1.8.1] — 2026-06-01

Audit remediation pass following a cross-stack `/audit effort=max` sweep
on 2026-06-01. One safety-relevant signal-handling fix and three
documentation corrections. No new features, no wire-shape changes.
Pairs with **GUI v1.14.1**, which lands the matching `paths.atomic_write`
parity backport and dead-code sweep on the Python side.

### Fixed
- **Daemon now handles SIGTERM as a graceful-shutdown signal**
  (`daemon/src/main.rs`). Previously the `tokio::select!` block only
  registered `tokio::signal::ctrl_c()` (SIGINT), so `systemctl stop` —
  which sends SIGTERM by default — terminated the process before the
  in-process graceful path could run: `shutdown_tx.send`, GPU
  fan-curve reset, hwmon `pwm_enable=2` restore, and the IPC server
  join were all silently skipped. External safety was preserved by
  `ExecStopPost=control-ofc-restore-auto`, so this was a *cleanliness*
  bug rather than a *safety* bug, but the inline comment claiming
  "SIGINT/SIGTERM" was misleading. SIGTERM registration is fail-soft:
  if the handler cannot register (rare — unusual sandbox policies),
  the daemon logs a warning and falls back to SIGINT-only behaviour.
- A new integration test in `daemon/tests/signal_handling.rs` pins the
  SIGTERM dispatch behaviour: register the stream, self-deliver SIGTERM
  via `libc::kill(getpid(), SIGTERM)`, verify the stream wakes within
  5 s. Each integration-test file is its own binary, so the signal
  cannot leak into other tests.

### Documentation
- **`daemon.md` KernelWarning schema corrected**. The doc previously
  claimed the entry carried four fields — `id`, `severity` (with values
  `info / warn / high / critical`), `summary`, and `reference_url`. The
  actual wire shape per `daemon/src/hwmon/kernel_warnings.rs` is
  three fields — `id`, `severity` (values `info / medium / high /
  critical` — no `warn` variant), `message`. The GUI's contract spec
  (`08_API_Integration_Contract.md`) already had the correct shape;
  only `daemon.md` had drifted.
- **`docs/USER_GUIDE.md` verify-duration corrected** to ~6 s (was
  ~3 s). DEC-101 raised the verify wait to 6 s — slow-spinning fans
  need more settle time — and the GUI's per-call timeout was already
  bumped to 12 s in lockstep. The daemon's USER_GUIDE missed the
  update.
- **README "Latest release" line** refreshed to v1.8.1 (was 2 minor
  versions stale).

### Tests
- 485 tests passing (was 482; +3 new signal-handling integration tests).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo audit` reports 0 vulnerabilities across 143 crates.

## [1.8.0] — 2026-05-13

Pairs with **GUI v1.13.0**. Combined release of two prior `[Unreleased]`
waves: the DEC-107 mutation-driven test-tests hardening pass (+16 daemon
tests, two non-breaking internal-API additions) and the DEC-108 `/audit`
follow-up hardening pass (`cargo fmt` fix, crash-safe atomic writes,
`set_pwm_all` broadcast coalescing, and 3 new IPC integration tests for
`POST /profile/activate` path-traversal protection). All additions are
backward-compatible on the wire and the internal API; older GUIs
interoperate with the new daemon and the new GUI interoperates with
older daemons without behavioural change. Total daemon tests: 482
passing (435 unit + 44 IPC integration + 3 main).

### Wave 1 — DEC-107 test-tests audit hardening

Test-tests audit hardening pass. A `/test-tests` mutation-driven audit
identified equivalent-mutant survivors in several core modules; this
pass closes the highest-value gaps with 16 new daemon tests and two
small ergonomic additions to the controller's observable surface. Pairs
with **GUI v1.13.0 Wave 1** (see GUI DEC-107 for the cross-stack rationale).

### Added (non-breaking, internal-API)
- **`LeaseError::Expired` variant.** Previously `validate_lease` and
  `renew_lease` returned `LeaseError::InvalidLease` for both "wrong
  lease id" and "matched id but TTL expired"; mutation testing showed
  the match guard at `hwmon/lease.rs:140` (the `==` ↔ `!=` flip) was
  observationally invisible. Now: `InvalidLease` for id mismatch,
  `Expired` for TTL elapsed. HTTP wire shape unchanged — both still
  map to `403 lease_required` via the `_` wildcard in
  `hwmon_control_error_response`. Internal callers can now
  distinguish renew-vs-reacquire without log parsing.
- **`HwmonPwmController::verify_mismatch_counts()` accessor.** Mirrors
  the existing `enable_revert_counts()` pattern. Returns a
  `&HashMap<String, u64>` of cumulative PWM verify-after-write
  mismatches per header (write N, readback ≠ N). Strong signal of
  BIOS clamping, EC interference, or an in-process concurrent writer.
  No HTTP exposure yet — added primarily to make the existing
  mismatch path observable in unit tests; future
  `/diagnostics/hardware` work may surface it.

### Tests
- **9 new lease tests** in `hwmon::lease::tests` pinning the wrong-id
  vs expired distinction (`validate_lease_distinguishes_wrong_id_from_expired`,
  `renew_lease_distinguishes_wrong_id_from_expired`) plus updates to
  the prior `*_fails_when_expired` tests to assert the new variant.
- **4 new boundary tests** in `health::cache::tests` for `gui_active()`
  at the 30s deferral window: 1s inside, 29s inside, exactly at 30s
  (the strict `<` predicate makes this *inactive*), and 31s past.
  Catches `<` ↔ `<=` mutations on the deferral predicate.
- **1 new integration test** in `profile_engine::tests`
  (`loop_defers_openfan_writes_when_gui_active`) running the loop
  with `record_gui_write()` called beforehand and asserting that
  zero SetPwm commands reach the mock transport.
- **2 new interpolation tests** in `profile::tests`
  (`evaluate_graph_interpolation_asymmetric_mid_segment`,
  `evaluate_graph_picks_correct_segment_for_mid_curve_temp`) using
  asymmetric temperatures (47°C, 73°C, 67°C) so the formula's `+` and
  `-` operators can't be mutated invariantly via halfway-point symmetry.
- **2 new chip-classification tests** in `hwmon::discovery::tests`:
  a single table-driven test covering every chip-name arm and every
  label-keyword sub-branch (37 rows), plus a precedence test pinning
  "cpu wins over gpu" in the fallback heuristic.
- **1 new range-boundary test** in `hwmon::pwm_control::tests`
  (`set_pwm_boundary_100_accepted_101_rejected`) explicitly
  asserting `pwm_percent=100` succeeds and `=101` raises
  `Validation` with the expected message.
- **3 new verify-mismatch tests** in `hwmon::pwm_control::tests`
  (`verify_mismatch_increments_counter`,
  `verify_no_mismatch_when_readback_matches_write`,
  `verify_mismatch_accumulates_across_writes`) using a new
  `ClampingSysfsWriter` mock whose `read_file` returns a value
  different from the last write. Catches mutations to the
  `actual_raw != raw` predicate at `pwm_control.rs:390`.
- **1 new API-mapping test** in `api::handlers::hwmon_ctl::tests`
  (`hwmon_control_error_response_maps_expired_lease_to_403`)
  confirming the new variant preserves the existing HTTP contract.
- **Total: 451 → 467 tests pass** (407 unit + 41 IPC integration + 3 main
  → 423 unit + 41 IPC integration + 3 main).

### Documentation
- Cross-stack rationale recorded in GUI **DEC-107**. Daemon
  `DECISIONS.md` carries a daemon-specific mirror of the same DEC-ID.

---

### Wave 2 — DEC-108 `/audit` follow-up hardening pass

A post-v1.7.0 `/audit` of both repos surfaced multiple issues; this
section captures the daemon-side fixes. See DEC-108 for the full
rationale.

#### Fixed (daemon)
- **`cargo fmt --check` failure on `main` resolved.** DEC-107
  committed two files (`hwmon/discovery.rs` table-driven test row
  and `hwmon/pwm_control.rs` boundary test) without running
  `cargo fmt`. The next release's gate would have blocked. Fixed
  by `cargo fmt --all`. (P1-A)
- **Atomic writes are now crash-safe.** Both
  `daemon_state::save_state_to` and `RuntimeConfig::save_to` did
  `std::fs::write(tmp) + rename` — atomic against process crash
  but not against power loss (the rename can land in the journal
  while data is still in the page cache, leaving a zero-length
  file on next mount). New `daemon::atomic_io::write_atomic`
  helper does `File::create → write_all → sync_all →
  set_permissions(0o600) → rename → parent-dir fsync`. Both call
  sites now use it. (P1-B)

#### Added (daemon)
- **`pub mod atomic_io`** — new module with `write_atomic(path:
  &Path, bytes: &[u8]) -> Result<(), String>`. Mirrors the GUI's
  `paths.atomic_write` shape. 9 unit tests cover content,
  permissions, tmp-cleanup on rename failure, large payloads, and
  empty content.
- **`SetPwmAllResult.coalesced: bool`** (controller-internal) and
  **`SetPwmAllResponse.coalesced: bool`** (wire-visible). When
  every channel already holds the requested value, `set_pwm_all`
  returns `coalesced: true` with no serial write and no cache
  update. Mirrors the single-channel `SetPwmResult` pattern.
  Additive on the wire — older GUIs ignore the field. (P2-B)

#### Tests (daemon)
- **+9 atomic_io unit tests** locking the write/fsync/rename
  sequence, owner-only permissions, tmp-cleanup-on-rename-failure,
  empty/large-content correctness, and the tmp-path naming
  convention.
- **+3 controller tests** for the new `set_pwm_all` coalescing
  behaviour (`set_pwm_all_coalesces_when_all_channels_already_at_value`,
  `set_pwm_all_does_not_coalesce_when_value_changes`,
  `set_pwm_all_does_not_coalesce_when_one_channel_diverges`).
- **+3 IPC integration tests** for `POST /profile/activate`
  path-traversal protection (P2-A): outside any search dir → 400,
  symlink chained outside → 400 (post-canonicalize check), inside
  search dir → 200 + state mutation.
- **Total: 467 → 482 tests pass** (423 → 435 unit + 41 → 44 IPC
  + 3 main).

#### Documentation
- Cross-stack rationale recorded in GUI **DEC-108** (authoritative
  full log). This `CHANGELOG.md` and daemon `DECISIONS.md` carry
  the daemon-relevant subset.

---

## [1.7.0] — 2026-05-13

Pairs with **GUI v1.12.0**. Coordinated AMD-board-support hardening
release covering AM4 400-series (DEC-105) and AM4 500 / AM5 600 / AM5
800 (DEC-106) in one minor bump. Adds one new optional field on
`/diagnostics/hardware` (`module_collisions`); the rest is additive
chip-detection data and detector refinement. Older GUIs silently
ignore the new field; the daemon emits the same field shape regardless
of whether the suppression refinement is in effect.

### DEC-106 — AM4 500 / AM5 600 / AM5 800 dual-Nuvoton refinement

The wire shape of `/diagnostics/hardware.module_collisions` is
unchanged; the daemon behaviour is refined to emit FEWER entries on
legitimate dual-Nuvoton boards. Older daemons that predate this work
continue to emit the broader (sometimes false-positive) result on
boards like the ASRock X870E Taichi Lite.

#### Features
- **`detect_module_collisions` now accepts a `chips: &[ChipBinding]`
  slice.** When the canonical `(nct6687, nct6775)` pair is loaded AND
  `chips` shows two distinct nct6 chips at distinct `device_id`s, the
  collision is suppressed — each driver legitimately owns its own
  physical chip and no chip-ID overlap can occur. The original
  brick-risk detection (single chip + both modules loaded) is
  unchanged. Defensive: empty `chips` still emits CRITICAL (no
  evidence of separation → assume the brick shape).
- **Two new AM4 500-series & AM5 800-series Gigabyte AORUS dual-chip
  entries** in `GIGABYTE_DUAL_CHIP_BOARDS`: **B550 VISION D**
  (it8688 + it8792, verified against upstream lm-sensors config
  `configs/Gigabyte/GA-B550-VISION-D.conf`) and **B850-AI-TOP**
  (it8696 + it87952, per frankcrawford/it87 issue #93). The X870
  AORUS STEALTH ICE board is deliberately not in this table — its
  IT8883 secondary chip has no Linux driver and a permanent
  missing-chip warning would be useless.

#### Tests
- `api::diagnostics::tests` gains seven new tests:
  - `detect_module_collisions_suppressed_on_legitimate_dual_nuvoton_board`
    — Taichi Lite shape (NCT6686 + NCT6799 at distinct IDs) is
    silent.
  - `detect_module_collisions_still_critical_for_single_chip_collision`
    — single nct6 chip with both modules loaded still surfaces
    CRITICAL.
  - `detect_module_collisions_critical_when_chips_unknown` — empty
    `chips` defensively keeps the CRITICAL behaviour.
  - `detect_module_collisions_non_nct6_chips_ignored_for_suppression`
    — k10temp / amdgpu / other non-nct6 entries cannot satisfy the
    "two distinct nct6 chips" suppression rule.
  - `expected_chips_b550_vision_d_pairs_it8688_with_it8792`,
    `expected_chips_b850_ai_top_pairs_it8696_with_it87952`, and
    `expected_chips_x870_aorus_stealth_ice_not_in_table` cover the
    dual-chip table edits.

### DEC-105 — AM4 400-series hardening + new `module_collisions` field

Adds one new optional field on `/diagnostics/hardware` plus AM4
400-series additions to the chip-detection data tables. Older GUIs
silently ignore the new field; older daemons (without this work) just
return no key for it and the GUI defaults to `[]`. No breaking
contract change.

#### Features
- **`module_collisions: Vec<ModuleCollisionInfo>` on
  `GET /diagnostics/hardware` (DEC-105).** The daemon now scans
  `/proc/modules` for known-bad simultaneous-load pairs and reports
  any matches as a CRITICAL severity entry with a remediation string.
  The seed entry is `(nct6687, nct6775)` — the out-of-tree `nct6687`
  driver declares chip ID `0xd450`, the same ID assigned to legitimate
  NCT6797D by `drivers/hwmon/nct6775-platform.c`. When both modules
  are loaded the wrong driver can claim the chip and write into
  non-volatile fan registers, with at least one upstream report
  (ublue-os/bazzite #4498) documenting a permanently bricked CPU_FAN
  header on MSI hardware. NCT6797D is common on AM4 400-series MSI
  boards (B450M MORTAR, X470 GAMING PRO CARBON, MAG B450 TOMAHAWK
  MAX). The field is emitted only when non-empty
  (`skip_serializing_if = "Vec::is_empty"`).
- **AM4 400-series Gigabyte AORUS dual-chip lookups (DEC-105).**
  Added X470 AORUS ULTRA GAMING (verified against upstream lm-sensors
  config), X470 AORUS GAMING 5 WIFI, X470 AORUS GAMING 7 WIFI, and
  B450 AORUS PRO (substring covers PRO WIFI). All IT8686E + IT8792E.
  Pre-existing B450 AORUS PRO-CF entry preserved.
- **`asus_atk0110` recognised in `KNOWN_MODULES` (DEC-105).** Closes a
  real ASUS diagnostic gap — the diagnostics page can now report this
  driver as loaded and the GUI's chip-guidance entry advises that it
  is a read-only ACPI sensor path, not a PWM-write path.

#### Tests
- `api::diagnostics::tests` gains:
  - `detect_module_collisions_flags_nct6687_with_nct6775` /
    `_silent_when_only_*_loaded` / `_returns_empty_on_unreadable_path`
    covering the new helper.
  - `expected_chips_x470_aorus_ultra_gaming_pairs_it8686_with_it8792`
    confirming the AM4 chip pairing matches the upstream lm-sensors
    config.
  - `expected_chips_b450_aorus_pro_uses_am4_400_chip_pair` and
    `expected_chips_b450_aorus_pro_wifi_resolves_via_substring` for
    the B450 generation.
  - `asus_atk0110_recognised_in_known_modules` regression test.

#### Safety / framing
- **Remediation string for the `(nct6687, nct6775)` collision now
  requires chip-ID verification first.** The string emitted in
  `module_collisions[].remediation` instructs the user to run
  `cat /sys/class/hwmon/hwmon*/name` BEFORE any blacklist command, and
  shows both the `blacklist nct6775` (for genuine NCT6687-R boards) and
  `blacklist nct6687` (for NCT6797D / NCT6798D boards) paths as
  parallel alternatives. Protects users from blindly following the
  banner and inadvertently removing their working driver.

## [1.6.5] — 2026-05-08

Packaging-UX release. Pairs with **GUI v1.11.3**. Cuts the daemon's
post-install message from 106 lines to ~31 and removes a long-stale
v1.1.2 awk migration so paru's PKGBUILD-review pager has materially
less to show on a fresh install. No daemon behavioural changes — same
binary, same API, same systemd integration.

### Changed
- **`packaging/control-ofc-daemon.install` slimmed (DEC-104).**
  Post-install message is now four lines (start command, modules-load
  hint, pointer to `man control-ofc-daemon`, pointer to the GUI's
  Diagnostics → Fans → Hardware Readiness card) instead of the previous
  4-step walkthrough. The 30-line per-bootloader kernel-parameter guide
  was duplicated content — the man page already covers it. Pacman's
  install-script invocation is unchanged; only the user-facing text is
  shorter.
- **`hyper-util` moved from `[dependencies]` to `[dev-dependencies]`.**
  The crate is consumed solely by `daemon/tests/ipc_integration.rs`
  (`use hyper_util::rt::TokioIo;`). Pre-fix, the production binary
  linked it for nothing. No runtime impact; `cargo build --release`
  output is marginally smaller.

### Removed
- **`_strip_legacy_runtime_sections` awk migration (DEC-104).** The
  helper auto-stripped `[profiles]` and `[startup]` sections from
  `/etc/control-ofc/daemon.toml` on `post_upgrade` to protect users
  upgrading from <1.1.2 directly. With v1.6.5 five minor releases past
  v1.1.2, every upgrader has had the migration applied long ago, and
  the 50-line shell helper was dead weight in paru's review pager.
  Anyone leapfrogging from <1.1.2 → 1.6.5 (vanishingly rare in practice)
  will need to remove those sections manually if present.

### Added
- **Install-UX tip in `daemon/README.md` (DEC-104).** Footnote-style
  note describing paru's PKGBUILD-review pager (the "press `q`"
  prompt new users see on first install) and how to opt out via
  `paru -S --skipreview` or `SkipReview` in `~/.config/paru/paru.conf`.
  Phrased as a tip, not the canonical install command — paru's review
  is a security feature and we are not normalising "skip review by
  default" for an Arch audience.
- **`daemon/Cargo.toml` `[package]` metadata.** Added `license = "MIT"`,
  `description`, `repository`, and `authors` fields. Pre-fix, `cargo
  metadata` reported the daemon as `UNSPECIFIED` license, and SBOM
  tooling / `cargo-deny` / `cargo publish` would surface it as an
  unidentified package. Aligns the daemon crate with the `[package]`
  hygiene the AUR PKGBUILD already enforces externally.

### Documentation
- **DEC-104 added** in this repo and mirrored in the GUI repo. Records
  the investigation that traced the "press `q`" complaint to paru's
  default review pager (not a SHA256 issue), the alternatives considered
  (custom signed pacman repo rejected as disproportionate for a
  single-author project), and why the in-package fix is limited to
  cutting our own pager content + documenting paru's opt-out.

## [1.6.4] — 2026-05-08

Bug-fix release. Pairs with **GUI v1.11.1**. Stops a 1 Hz error storm
on RDNA3+ AMD GPU systems where the GPU's read-only hwmon `pwm1` shadow
was being treated as a controllable fan header.

### Fixed
- **AMD GPU `pwm1` excluded from hwmon discovery (DEC-102).**
  `hwmon::pwm_discovery::discover_device_pwm` skips entries whose
  `chip_name == "amdgpu"` early — before any `pwmN` enumeration. Pre-fix,
  the daemon advertised `hwmon:amdgpu:0000:XX:XX.X:pwm1:pwm1` in
  `GET /hwmon/headers`; clients (the GUI in particular) could bind it to
  a profile control and the resulting 1 Hz `POST /hwmon/.../pwm` flood
  produced a 503/`Permission denied (os error 13)` storm in the journal.
  RDNA3+ kernels expose that file read-only without `pwm1_enable`, so
  the write can never succeed; GPU fan control belongs on
  `/gpu/{id}/fan/...` with the `amd_gpu:` member prefix exclusively.
- **`POST /hwmon/{header_id}/pwm` returns `400 feature_unavailable`
  when the targeted header's discovered `is_writable=false` (DEC-102).**
  Defense-in-depth: any future chip exposing a read-only `pwmN`
  (BIOS-locked motherboard headers, etc.) now produces a clean
  non-retryable error envelope (DEC-094 shape, mirroring DEC-098's
  GPU handler) instead of mis-classifying kernel `EACCES` as
  `503 hardware_unavailable + retryable: true`. Lease-validation order
  preserves precedence so a permanently read-only header reports
  `feature_unavailable` even when the caller's lease is invalid.

### Tests
- 4 new integration tests in `daemon/tests/ipc_integration.rs`:
  `hwmon_set_pwm_read_only_header_returns_400_feature_unavailable`,
  `hwmon_set_pwm_read_only_header_takes_precedence_over_lease`,
  `hwmon_set_pwm_unknown_header_returns_404`, and
  `hwmon_discovery_excludes_amdgpu_end_to_end_via_ipc` (which builds a
  fake hwmon root with both `it8696` and `amdgpu` chips, runs real
  `discover_pwm_headers` against it, hands the result to a real
  `HwmonPwmController`, and confirms `GET /hwmon/headers` over the
  IPC socket returns only the motherboard chip).
- 3 new unit tests in `pwm_discovery::tests` —
  `discover_amdgpu_excluded`, `discover_amdgpu_excluded_even_with_enable_file`,
  `discover_amdgpu_excluded_alongside_motherboard_chip` — and
  `discover_without_enable_file` reworked against `nct6798` (the
  legitimate non-amdgpu case the test was meant to cover).

## [1.6.3] — 2026-05-07

Pairs with **GUI v1.11.0**. PWM verify timing fix and dual-chip
diagnostics support, plus a corrected `it87` mainline flag for the
Diagnostics modules table.

### Changed
- **PWM verify wait raised from 3 s to 6 s (DEC-101).** The
  `hwmon_ctl::VERIFY_WAIT_SECONDS` constant doubles so slow-spinning
  fans (pumps, large 140 mm chassis fans) settle their RPM in time to
  be classified correctly. Previous 3 s wait produced false
  `no_rpm_effect` verdicts. Worst-case round-trip is now ~7.5 s; the
  GUI's verify HTTP timeout (12 s) and pause-safety auto-resume (9 s)
  must stay above this value.
- **`KNOWN_MODULES` `it87` flag flipped from `true` to `false`
  (DEC-101).** The *module name* exists in mainline but every chip we
  target (IT8625E / IT8686E / IT8688E / IT8689E / IT8696E /
  IT87952E) requires the out-of-tree `frankcrawford/it87` DKMS build.
  Marking the module as not-mainline keeps the GUI's modules-table
  column truthful. The chip-level `chip_driver_in_mainline` is
  unchanged and still per-chip accurate.

### Added
- **`/diagnostics/hardware.expected_chips` (DEC-101).** Curated
  DMI-board → chip-list lookup covering known dual-IO Gigabyte boards
  (X570/X670/X870/Z690/Z790 AORUS series and TRX40). When a board is
  in the table, the field lists the chip names that should appear in
  hwmon. Empty for unknown boards. Skipped from the wire when empty
  so older clients see no shape change.
- **`/diagnostics/hardware.kernel_detected_chips` (DEC-101).** Best-
  effort chip names parsed from `/dev/kmsg` `it87:` log lines.
  Populated when the kernel ring buffer is readable
  (default Arch `kernel.dmesg_restrict=0`); empty otherwise. Useful
  for distinguishing "kernel saw the chip but driver did not bind"
  from "kernel never saw the chip"; not authoritative.

### Tests
- 11 new unit tests in `api::diagnostics::tests` covering the
  curated lookup table, the kmsg parser (deduplication, false-positive
  rejection, short-code rejection, empty input), the file-fixture
  driver, and the `it87` mainline flag.
- Two `ipc_integration.rs` HwmonVerifyResponse fixtures bumped from
  `wait_seconds: 3` to `6`.

## [1.6.2] — 2026-05-07

Audit-driven hygiene pass. Pairs with **GUI v1.10.2**. Three behavioural
fixes (one wire-contract addition, one safety-restore gap, one
dual-writer-guard bug) plus internal cleanup of misleading naming and
unused dependencies.

### Fixed
- **GPU `POST /gpu/{id}/fan/reset` now records GUI activity (DEC-100).**
  All other GPU/OpenFan/hwmon write handlers call `record_gui_write()`
  on success; the reset path was the lone exception. With a profile
  active, the next 1 Hz profile-engine tick (~1 s after the reset call)
  re-asserts the profile's commanded curve and silently undoes the
  user's reset. Both the PMFW reset arm and the legacy-pwm1 reset arm
  now call `record_gui_write()` so the profile engine defers for the
  same 30 s window it gives to a `set` call.
- **`POST /hwmon/{id}/verify` surfaces a failed restore-PWM write
  (DEC-100).** The handler previously did `let _ = ctrl.set_pwm(...)`
  on the post-verify restore — a `HwmonControlError` (lease expired
  mid-call, EINVAL, etc.) was silently swallowed and the header was
  left at the test value (20 % or 80 %) without any caller-visible
  signal. The handler now logs at warn-level and returns
  `restore_failed: true` in the response body so the GUI can prompt
  the operator to re-set the desired PWM. The new field is
  `#[serde(skip_serializing_if = "is_false")]`, so older clients see
  the same wire shape they always have.
- **`packaging/control-ofc-restore-auto.sh` now restores
  `fan_zero_rpm_enable=1` (DEC-100).** The graceful-shutdown panic hook
  already re-enables PMFW zero-RPM on SIGTERM and panic, but
  `ExecStopPost` runs even when the process was SIGKILLed or OOM-killed
  before any userspace code ran. Without this, a daemon crash that
  occurred while zero-RPM was disabled (i.e. between a `set_static_speed`
  and the next `reset_to_auto`) would leave the GPU fan running
  continuously at idle. The script now writes `1\n` + `c\n` to every
  `fan_zero_rpm_enable` sysfs file alongside the existing `r\n` + `c\n`
  curve restore.
- **`SSE_MAX_CLIENTS` admission survives tight CAS contention
  (DEC-100).** `events_handler` now bounds admission to
  `SSE_ADMISSION_ATTEMPTS = 4` with a `tokio::task::yield_now()`
  between failures (whether the CAS lost to a concurrent change or the
  counter was at the limit). Concurrent disconnects and CAS losers can
  settle before we surface 503 to the caller. Single-client load and
  cold-start admission are unaffected.

### Renamed
- **`GPU_PMFW_WRITE_RETRIES` → `GPU_PMFW_NUM_CURVE_POINTS` (DEC-100).**
  Audit found the constant's doc comment described "PMFW write retries
  before giving up" — but the constant is the `num_points: u8` argument
  to `set_static_speed` / `set_static_speed_with_zero_rpm`, controlling
  how many curve-point indices are written. There is no retry logic in
  the GPU PMFW write path. A maintainer who reduces this to "suppress
  retries" would silently shrink the curve. The new name and doc
  comment reflect what it actually does.

### Documentation
- **`docs/USER_GUIDE.md`** now references the current v4 profile schema
  (was v3); the migration sentence makes clear that v3 and earlier
  profiles auto-migrate on load.
- **`packaging/profiles/quiet.json`** bumped to `version: 4`. The
  `members: []` shape is unchanged so the role-aware `minimum_pct`
  migration is a no-op for the example profile.

### Removed
- **Unused dependencies** — `ctrlc`, `tokio-stream`, `tower`. All three
  were declared in `daemon/Cargo.toml` but never imported. Confirmed
  via `cargo machete` and `grep`. Drops one dependency per
  `cargo audit` and shaves a small amount of build time / binary mass.

### Tests
- 4 new tests:
  - `gpu_reset_fan_records_gui_write` — integration test that exercises
    the full reset handler against a tempdir-backed PMFW GPU and
    asserts `cache.snapshot().gui_active()` becomes true.
  - `hwmon_verify_response_omits_restore_failed_when_false` — wire
    contract: older clients see no extra field.
  - `hwmon_verify_response_includes_restore_failed_when_true` — wire
    contract: newer clients see the flag and can warn the operator.
  - `test_app_state_with_writable_pmfw_gpu` (helper) — reusable test
    fixture for future PMFW-write-path coverage.
- 374 lib + 3 (config) + 37 IPC = **414 tests pass** (was 411 before
  this pass).
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo machete` reports no unused dependencies.

### Decisions
- **DEC-100 (cross-repo):** audit-pass-2 remediations covering
  warning-signal immediacy, verify `restore_failed` contract addition,
  GPU reset records `gui_active`, lease retry-timer suspend, GPU ZRP
  restore script, SSE admission retry, and the
  `GPU_PMFW_NUM_CURVE_POINTS` rename.

---

## [1.6.1] — 2026-05-06

Audit-driven correctness fixes. Pairs with **GUI v1.10.1**. No new user-
visible features — every change is either a contract correction or an
internal change that improves runtime fairness under load.

### Fixed
- **GPU `feature_unavailable` now actually fires for read-only RDNA3/4
  hardware (DEC-098).** The legacy hwmon dispatch arm in
  `gpu_set_fan_handler` and `gpu_reset_fan_handler` previously gated on
  `gpu.has_pwm` alone. RDNA3/RDNA4 GPUs booted without
  `amdgpu.ppfeaturemask=0xffffffff` expose `pwm1` read-only and lack
  `pwm1_enable`, so `set_legacy_pwm` would attempt to write
  `pwm1_enable`, fail with ENOENT, and surface
  `503 hardware_unavailable + retryable: true` — wrong code (capability
  gap, not transient hardware fault) and wrong retry semantics
  (permanent, not retryable). Both handlers now consult a canonical
  helper `AmdGpuInfo::can_write_legacy_pwm()` and return
  `400 feature_unavailable + retryable: false`. The capability scoring
  in `status::capabilities_handler` consumes the same helper so
  `/capabilities` and the actual handler outcome cannot disagree.
  When the GPU shape matches RDNA3/4 without overdrive, the error
  message includes the `amdgpu.ppfeaturemask=0xffffffff` hint that
  unlocks PMFW.
- **OpenFan and hwmon API write handlers now run on `spawn_blocking`
  (DEC-099).** `set_pwm_handler`, `set_pwm_all_handler`,
  `set_target_rpm_handler`, and `hwmon_set_pwm_handler` previously held
  their parking_lot locks on a tokio worker thread directly, pinning
  runtime workers for hundreds of milliseconds per write. They now
  match the convention already used by the GPU handlers and the
  polling loop. Callers see no behavioural change; the runtime stays
  responsive for other tasks while a serial or sysfs write is in
  flight.
- **Thermal-emergency override scan releases the `FanController` mutex
  between channels (DEC-099).** The scan previously held the mutex
  across all 10 OpenFan channels (~5 s upper bound) plus the hwmon
  headers, serialising every concurrent GUI write request behind it.
  Per-channel re-locking lets GUI requests interleave; if a GUI write
  briefly defeats safety, the next 1 Hz tick re-asserts the forced
  value, so the safety net still holds.

### Added
- **`amd_gpu.kernel_warnings` capability field (DEC-098).** Populated
  from a new `crate::hwmon::kernel_warnings` module that reads
  `/proc/sys/kernel/osrelease` and matches the running kernel against
  published amdgpu regressions:
  - `rdna_hang_kernel_6_19_x` (Critical) — kernel 6.19.x on any RDNA3
    or RDNA4 GPU; Phoronix-confirmed hard hang (Dec 2025).
  - `smu_mismatch_navi48_r9700_kernel_7_0` (Critical) — kernel 7.0.x
    on R9700 (PCI 0x7551) with PMFW exposed; ROCm Issue #6101 silent
    fan_curve write failure. Scoped narrowly to 0x7551; RX 9070 XT
    (0x7550) on the same kernel is not affected.

  The field uses `#[serde(skip_serializing_if = "Vec::is_empty")]`,
  so older clients that don't know about it see no change in
  `/capabilities` output.

### Tests
- Two new integration tests pin the read-only-RDNA case:
  `gpu_set_fan_read_only_rdna_returns_400_feature_unavailable` and
  `gpu_reset_fan_read_only_rdna_returns_400_feature_unavailable`,
  plus a new `test_app_state_with_read_only_gpu` fixture in
  `daemon/tests/ipc_integration.rs`. The fixture covers the
  `has_pwm=true, has_pwm_enable=false, fan_curve_path=None` shape that
  RDNA3/4 hardware without overdrive presents.
- 17 new unit tests cover `kernel_warnings` parsing, severity, and
  device-id scoping (positive cases for 6.19 + RDNA3/4, negative for
  RDNA2; positive for R9700 0x7551 on 7.0, negative for RX 9070 XT
  0x7550 on the same kernel).
- 374 lib + 34 integration + doc tests pass.

### Documentation
- `DECISIONS.md`: DEC-098 (legacy-PWM gate + kernel_warnings),
  DEC-099 (`spawn_blocking` + per-channel mutex).
- `CLAUDE.md` cross-stack updates land in the GUI repo (DEC-098 and
  DEC-099 are mirrored there as authoritative).

---

## [1.6.0] — 2026-05-02

Headless profile-mode parity and per-GPU zero-RPM control. Pairs with
**GUI v1.10.0**. The daemon can now drive fans autonomously per a
GUI-authored profile with the same audible behaviour as GUI-driven mode,
honour each GPU's user-chosen idle-fan-stop preference, and surface a
clean `/profile/deactivate` endpoint so the GUI can stop curve writes
without restarting the daemon.

### Added
- **`POST /profile/deactivate` endpoint (DEC-097).** Clears the active
  profile, persists the cleared state to `daemon_state.json`, releases
  any held `profile-engine` hwmon lease, and refreshes the GUI-activity
  marker so the engine doesn't immediately re-take a lease. Idempotent —
  calling on an already-deactivated daemon returns 200 with both
  `previous_profile_id` and `previous_profile_name` set to `null`.
  Distinct from `/profile/activate` with a missing path (which 404s).
  GUI leases (any owner other than `profile-engine`) are explicitly
  preserved so manual GUI writes continue uninterrupted.
- **2 °C falling-temperature deadband in the profile engine (DEC-096).**
  `evaluate_profile` now mirrors the GUI's
  `_evaluate_curve_with_hysteresis` (`HYSTERESIS_DEADBAND_C = 2.0`):
  when the current temperature has fallen ≤ 2 °C below the last
  transition anchor, the previous curve output is held instead of
  re-interpolated. Closes the audible parity gap where headless mode
  oscillated at curve transitions while GUI-driven mode behaved
  smoothly with the same profile. Per-control state lives in
  `ProfileEngineState` alongside the existing tuning state and is
  cleared on profile change or deactivation. Five new unit tests cover
  hold-on-fall, release below threshold, anchor advance on rise, anchor
  stationary for sub-0.5% deltas, and clearing on profile swap.
- **Per-GPU `fan_zero_rpm` flag in profile schema v4 (DEC-095).**
  `ControlMember` now carries an optional `fan_zero_rpm` boolean
  (default false). When true on an `amd_gpu` member, the daemon
  preserves PMFW `fan_zero_rpm_enable` while writing the curve so the
  GPU honours its idle fan-stop threshold. When false (or omitted on a
  legacy v3 profile), the daemon disables zero-RPM as before — the
  pre-1.6.0 behaviour is exactly preserved by the safe default.
- **`hwmon::gpu_fan::set_static_speed_with_zero_rpm` helper** with an
  explicit `preserve_zero_rpm: bool` parameter. The legacy
  `set_static_speed` is now a thin wrapper that hard-codes
  `preserve_zero_rpm=false`, so the manual-write path through
  `/gpu/{id}/fan/pwm` is unchanged. Two new unit tests cover the new
  path (preserve=true skips the disable; preserve=false matches
  legacy behaviour).

### Changed
- **Profile schema default version is now 4.** v3 profiles deserialise
  unchanged because all v4 fields use `#[serde(default)]`. The version
  warning still fires below 3 to flag truly legacy profiles. Two new
  unit tests in `profile.rs` verify that v3 profiles default
  `fan_zero_rpm` to false and v4 profiles round-trip the user's
  explicit choice.

### Profile engine state — new fields
`ProfileEngineState` gains `last_curve_output` and
`last_transition_temp` maps (per control id) to back the deadband.
`deactivate()` and `sync_profile_id()` clear them alongside the
existing `last_output` so the deadband doesn't leak across profile
swaps. New public read-only accessors (`last_curve_output`,
`last_transition_temp`) expose the state for tests.

### Tests
- Daemon test count: 357 unit + 32 integration + 3 doc = 392 (was
  347 + 28 + 3 = 378). +14 tests covering deadband behaviour,
  fan_zero_rpm round-tripping in `set_static_speed_with_zero_rpm`,
  v3-vs-v4 profile loading, and the four new `/profile/deactivate`
  integration scenarios (clears state, idempotent, releases the
  profile-engine lease, preserves GUI leases).
- The three existing tuning tests
  (`tuning_step_up_rate_limits_large_jump`,
  `tuning_step_down_rate_limits_large_drop`,
  `tuning_start_threshold_jumps_from_zero`) now bump the sensor
  temperature each cycle so the new deadband releases — they
  exercised the tuning pipeline at a fixed temperature, which the
  deadband would otherwise hold across.

### Why
Cross-stack audit found that headless profile-mode was technically
correct but audibly different from GUI-driven mode (no deadband),
that profile deletion left a phantom profile driving fans until
daemon restart, and that the daemon couldn't honour a user's choice
to keep zero-RPM idle on a GPU governed by a curve. This release
closes all three gaps. The role-aware safety floor decision (option B
from the audit) is GUI-side and lives in **GUI v1.10.0**; the daemon
intentionally does not enforce a per-role floor, preserving the
established "GUI owns curve safety policy, daemon owns thermal
emergency" architectural split (CLAUDE.md, DEC-022).

## [1.5.6] — 2026-04-30

Packaging hygiene release. No code, contract, or behaviour changes.

### Fixed
- **`depends=` now declares `libgcc` directly.** The release binary's
  ELF `NEEDED` entries include `libgcc_s.so.1`, which was previously
  satisfied only transitively through `systemd-libs → libgcc`. Declaring
  it directly is namcap best practice and removes the implicit-dependency
  warning. No effect on already-installed users (the resolver already
  pulled `libgcc` in via the chain), but the chain is no longer fragile
  if `systemd-libs` ever drops it.
- **`packaging/.SRCINFO` is now tracked in-repo.** Was previously in
  `.gitignore` and only maintained inside the AUR clone. The
  `.gitignore` entry is removed and the file is regenerated against
  the current PKGBUILD; the new pre-commit hook keeps it in sync going
  forward (see Tooling).

### Changed
- **`makedepends=` no longer lists `cargo`.** The `rust` package
  provides `cargo`; listing both is redundant.

### Tooling
- **New `.githooks/pre-commit`** that auto-regenerates
  `packaging/.SRCINFO` whenever `packaging/PKGBUILD` is staged for
  commit, so the in-repo file cannot drift again. Opt-in via:
  `git config core.hooksPath .githooks`. The hook is a no-op when
  `makepkg` is not on PATH (e.g. on non-Arch CI).

## [1.5.5] — 2026-04-30

Patch release fixing a man-page rendering bug. No code changes.

### Fixed
- **Man page em-dash rendering.** `man control-ofc-daemon` rendered every
  em-dash (U+2014) as a doubled `——` on systems with groff 1.24+ (current
  Arch). The groff 1.24 `tty.tmac` defines `.char \[em] \[em]\[em]` for
  UTF-8 output to approximate the typographic em-quad width on a
  half-width cell grid; passing literal U+2014 through scdoc therefore
  emits `——`. Replaced em-dashes in `man/control-ofc-daemon.1.scd`
  (NAME, AMD GPU SUPPORT, and bootloader sections) with the canonical
  man-page convention `--`. `man control-ofc-daemon` now reads
  correctly, including the bootloader checklist. Verified with
  `groff -man -K utf8 -Tutf8 -ww` (zero warnings) and `man -l`.
  Degree-sign rendering (`105°C`, `80°C`) is unaffected and continues
  to render as a single character.

## [1.5.4] — 2026-04-30

Install-experience and documentation packaging release. No changes to the
daemon binary's runtime behaviour, IPC contract, or persistence format.
Existing operator config (`/etc/control-ofc/daemon.toml`) and runtime
config (`/var/lib/control-ofc/runtime.toml`) load unchanged.

### Added
- **Man page ships.** `man control-ofc-daemon` now renders a manual that
  documents CLI flags, environment variables, files, signals, and a new
  AMD GPU section covering `amdgpu.ppfeaturemask=0xffffffff` per
  bootloader (GRUB, systemd-boot, rEFInd, Limine).
- **Shell completions ship** for bash, zsh, and fish under
  `/usr/share/{bash-completion,zsh/site-functions,fish/vendor_completions.d}/`.
  Tab-completion of `control-ofc-daemon --` works after install.
- **User documentation ships in `/usr/share/doc/control-ofc-daemon/`:**
  `README.md`, `CHANGELOG.md`, `daemon.md`, `USER_GUIDE.md`,
  `DEVELOPER_HANDOVER.md`, and the new `ADRs/` directory.
- **`docs/ADRs/001-ipc-transport.md`** and
  **`docs/ADRs/002-runtime-config-split.md`.** The codebase had eight
  outstanding cross-references to these ADRs; they are now written and
  the references resolve.
- **`SECURITY.md`** at the repo root, listing the supported version
  range, contact, and the daemon's privilege/sysfs/socket boundaries.

### Changed
- **`post_install` message is rewritten to remove the dead-end "Install
  the GUI: yay -S control-ofc-gui" line.** A user installing both
  packages via `paru -S control-ofc-daemon control-ofc-gui` previously
  saw a self-referential nudge to install the GUI they were already
  installing, with a different AUR helper. The new message focuses on
  the three things the daemon's installer can usefully tell the user:
  enable command, modules-load hint, and where to find the man page
  and USER_GUIDE.
- **AMD GPU kernel-parameter advice in `post_install` is narrower and
  pointed at the man page** rather than embedding bootloader-specific
  steps in the install transcript. The man page's new AMD GPU section
  now carries the per-bootloader detail.
- **Man page recovery wording corrected:** the daemon's safety floor
  is a 60 % PWM recovery floor applied for one cycle, not a "recover
  at 60°C" temperature threshold (which was never the implementation).
  Matches the wording in `daemon.md`, `USER_GUIDE.md`, and the
  daemon's `safety.rs`.

### Packaging
- Adds `scdoc` to `makedepends`. Builds the man page via
  `scdoc < man/control-ofc-daemon.1.scd` in `build()` and installs
  to `/usr/share/man/man1/control-ofc-daemon.1`.
- `sha256sums` switched to `SKIP` pending the post-tag-push checksum
  refresh — same pattern as previous releases.

### Why
A `/audit documentation` pass on both repos plus a fresh
`paru -S control-ofc-daemon control-ofc-gui` test surfaced eight
install-experience defects, of which four were on the daemon side:
no man page despite the source being checked in, no shell completions,
a `/usr/share/doc/control-ofc-daemon/` directory with only a udev
rules example (the `post_install` message advertised a "Full setup
guide" there), and a self-referential `yay -S control-ofc-gui` line.
This release fixes all four. See the GUI repo v1.9.1 entry for the
matching client-side changes.

## [1.5.3] — 2026-04-28

Truthfulness patch for `POST /hwmon/{header}/verify` — the `details`
strings now acknowledge that a register change during the 3 s test
window can come from another in-process writer (lease holder,
thermal-safety override) and not only from BIOS/EC reclaim. Pairs with
**GUI v1.8.0** which also pauses the GUI control loop during a verify
call to eliminate the dominant racer (the GUI's own 1 Hz tick).

### Changed
- **`pwm_value_clamped` and `pwm_enable_reverted` `details` strings**
  in `classify_verify_result` now name BIOS/EC firmware as the most
  likely cause but call out the concurrent-writer alternative
  explicitly, with a "Re-run with no profile active and no other
  client writing" disambiguation hint. Wording-only — the response
  shape, the `result` enum values (`effective` / `pwm_enable_reverted` /
  `pwm_value_clamped` / `no_rpm_effect` / `rpm_unavailable`), HTTP
  status codes, and error envelope are all unchanged. The GUI's
  existing `hwmon_guidance.verification_guidance` lookup keeps
  matching without coordinated GUI redeploys.

### Unchanged (explicitly verified)
- Verify wait duration (3 s), test PWM choice (20 % or 80 % depending
  on initial), and classification thresholds (`delta > 10` raw for
  clamped) are all preserved.
- No new error-envelope variants. No schema changes.
- The pre-existing `pwm_enable_reverted` BIOS/EC reclaim story
  (DEC-074, AORUS Smart Fan watchdog) still applies — the wording
  change just acknowledges the second possible cause.

### Tests
- Three new unit tests in `hwmon_ctl.rs::tests` covering the new
  wording for both result variants and asserting the `result` enum
  values are unchanged after the rewording.

### Why
A `/investigate-bug` pass on the X870E AORUS MASTER traced the user's
"PWM control isn't working" report back to a misclassified verify
result. The board controls correctly; the GUI's own control loop was
racing the daemon's verify wait, and the classifier blamed BIOS/EC
even though the racer was an internal Linux writer. GUI v1.8.0 fixes
the race itself; this daemon patch fixes the wording so a residual
race (e.g. an external CLI tool) does not produce a misleading verdict.
See `PWM_VERIFY_REMEDIATION.md` in the GUI repo for the full
investigation and approved plan.

## [1.5.2] — 2026-04-25

Operator-experience patch: stop the journal-spam side effect of the
pwm_enable watchdog without weakening the watchdog itself. Pairs with
**GUI v1.7.1**.

### Changed
- **Throttled pwm_enable watchdog log emission.** `HwmonPwmController` no
  longer emits one `WARN` line per second per affected header when the
  BIOS/EC reclaims `pwm_enable`. Each header now produces a single `WARN`
  on the first reclaim, subsequent reverts log at `DEBUG`, and a single
  `INFO` summary fires every 60 s with the delta and cumulative count.
  On a Gigabyte X870E AORUS MASTER (IT8696E) this drops journal volume
  from ~3,600 entries/hour per active hwmon-controlled header to ~60/hr,
  while preserving the existing remediation behaviour.

### Unchanged (explicitly verified)
- The watchdog still acts on **every** reclaim event — only the log
  emission is throttled. Manual mode (`pwm_enable=1`) is re-written and
  the PWM value re-issued exactly as before.
- The cumulative `enable_revert_counts` figure exposed via
  `GET /diagnostics/hardware` increments per event, regardless of
  whether the event produced a `WARN`, `DEBUG`, or `INFO` line. Tests
  pin this invariant.

### Tests
- Six new unit tests in `pwm_control.rs` covering: first-event WARN,
  subsequent DEBUG within the 60 s window, single INFO summary at the
  interval boundary with correct delta, full one-hour schedule
  (`1 WARN + 59 INFO + 3540 DEBUG = 3600 events`), per-header state
  isolation, and the load-bearing "throttling never gates the counter"
  invariant.

## [1.5.1] — 2026-04-23

Follow-up audit remediation on v1.5.0. Pairs with **GUI v1.6.1**.
Three small wire-contract fixes plus documentation hygiene — no behavioural
change to the safety or control paths.

### Fixed
- **GPU "no fan path" error envelope.** `POST /gpu/{id}/fan/pwm` and
  `POST /gpu/{id}/fan/reset` now return HTTP 400 `feature_unavailable`
  (retryable:false, source:"validation") when the addressed GPU has
  neither a PMFW `fan_curve` nor legacy `pwm1` write path. Previously
  returned HTTP 400 with `hardware_unavailable` (retryable:true), which
  contradicts the documented contract (`hardware_unavailable` is a 503
  code and the condition is permanent for the device, not retryable).
  Two new integration tests lock in the new shape.
- **`POST /hwmon/{id}/verify` lease-expiry mapping.** The verify handler
  re-issues a PWM write after its up-front `validate_lease` check. If the
  lease TTL expired between those two points, the write error was being
  mapped to HTTP 500 `internal_error` instead of HTTP 403 `lease_required`.
  The handler now delegates to the shared `hwmon_control_error_response`
  mapper used by every sibling hwmon handler. Two new unit tests cover the
  mapping.
- **SSE `too_many_clients` source field.** `GET /events` now reports
  `source: "internal"` for the client-cap rejection (was `"validation"`,
  which is wrong for a transport-level condition — the request shape is
  fine, the server-side cap is the limiting factor).

### Added
- **`ErrorEnvelope::feature_unavailable`** — new helper for the "endpoint
  exists, device exists, device lacks this capability" case. Distinct
  from `hardware_unavailable` (transient / retryable) and
  `validation_error` (malformed request shape).

### Changed
- **Docs: stale working-doc link removed.** `AmdGpuCapability.pci_id`
  doc comment in `api/responses.rs` no longer points at the deleted
  GUI-side `docs/23_Contract_Mismatch_Backlog.md`; replaced with a
  reference to GUI `CHANGELOG.md` v1.6.0 and `DECISIONS.md` DEC-042.

## [1.5.0] — 2026-04-23

Contract-mismatch remediation (15-item cross-stack sweep). Pairs with
**GUI v1.6.0**. The headline change is M1 — the profile engine now applies
the full per-control tuning pipeline, so headless profile-mode output is
identical to GUI-driven output for the same profile. See the GUI's
`docs/23_Contract_Mismatch_Backlog.md` for the full investigation.

### Added
- **M1: Full tuning pipeline in the profile engine.** `evaluate_profile`
  previously applied only `offset_pct` and `minimum_pct`, silently ignoring
  `step_up_pct`, `step_down_pct`, `start_pct`, and `stop_pct` even though
  they were deserialised from the profile. The engine now applies all six
  stages in the same order as the GUI's `ControlLoopService._apply_tuning`:
  offset → minimum → step-rate limit → stop-snap → start-hysteresis →
  clamp. A new task-local `ProfileEngineState` tracks pre-rounding `f64`
  `last_output` per control across 1 Hz cycles, clears on profile-id
  change, and clears on deactivation. The wire PWM uses round-to-nearest
  so `49.6` becomes `50` (matches the GUI's `round(pwm_percent)`).
  Ten new unit tests cover the pipeline stages and state lifecycle.
- **M11: `/capabilities` and `/diagnostics/hardware` emit both `pci_id`
  and `pci_bdf`.** Same BDF string under both names so callers aligned to
  either convention keep working during the transition window. Legacy
  names are documented as deprecated; will be removed in a future major
  version. Three new serialization tests.
- **Integration tests for status-code consistency** (`daemon/tests/ipc_integration.rs`):
  `/hwmon/{id}/verify` returns 503 when no controller is present,
  `/gpu/{id}/fan/pwm` and `/gpu/{id}/fan/reset` return 404 for unknown
  GPU ids (validation, not hardware).

### Changed
- **M12: `/hwmon/{id}/verify` returns 503 `hardware_unavailable`** when
  the controller is absent, matching every sibling hwmon handler.
  Previously returned 404 `validation_error`, which implied the endpoint
  itself was missing.
- **M13: GPU fan write/reset `hardware_unavailable` now returns 503**,
  not 500. Four match arms in `gpu.rs` (legacy + PMFW, set + reset) were
  inconsistent with the documented contract. `spawn_blocking` task
  failures correctly remain 500 (`internal_error`); unknown GPU id
  correctly remains 404 (`validation_error`).

## [1.4.2] — 2026-04-22

Audit remediation. Pairs with GUI v1.5.2.

### Changed
- **Profile engine's hwmon phase now also respects `gui_active`.** Previously
  only OpenFan (DEC-074) and GPU (DEC-071) writes deferred when the GUI had
  written via the API in the last 30s — hwmon writes only skipped based on
  lease ownership, leaving a narrow race during GUI startup and lease
  lapses. The three phases now share `DaemonState::gui_active()` and behave
  uniformly (DEC-093).
- **Comment on `ControlMember.source`** extended from `"openfan" or "hwmon"`
  to also include `"amd_gpu"`, which the profile engine already dispatched on.

### Added
- **`DaemonState::gui_active()` helper** factored out of `profile_engine.rs`,
  covered by three unit tests (fresh cache, post-write, post-timeout).
- **Integration test for `GET /poll`** locking the top-level response shape
  consumed by the GUI's 1 Hz polling loop (`api_version`, `status`,
  `sensors`, `fans`).
- **Daemon `DECISIONS.md`** summarising daemon-relevant ADRs (DEC-049,
  DEC-053, DEC-070, DEC-071, DEC-073, DEC-074, DEC-093) and cross-referencing
  the authoritative GUI file.

## [1.4.1] — 2026-04-22

Code quality and maintainability improvements from comprehensive audit.

### Changed
- **Split `handlers.rs` monolith** (1930 lines) into 8 focused submodules:
  `status`, `openfan`, `gpu`, `hwmon_ctl`, `profile`, `config`,
  `hw_diagnostics`, and shared helpers in `mod.rs`. All API paths unchanged.
- **Deduplicated PWM conversion functions.** `percent_to_raw` and
  `raw_to_percent` consolidated into new `pwm` module, replacing 4 duplicate
  definitions across `serial/controller`, `hwmon/pwm_control`, `api/handlers`,
  and `polling`.
- **Extracted legacy GPU sysfs writes** from inline handler code into
  `gpu_fan::set_legacy_pwm()` and `gpu_fan::reset_legacy_to_auto()`. Pre-RDNA3
  GPU fan control is now testable and returns typed `HwmonError` instead of
  swallowing IO errors.
- **Deduplicated status-building logic.** `status_handler` and `poll_handler`
  now share `build_status_response()` instead of duplicating 30 lines of
  identical subsystem/uptime/GUI-last-seen construction.

### Removed
- **Dead code cleanup.** Removed unused `IpcError` and `ErrorKind` types from
  `error.rs` (never referenced outside their own module).

## [1.4.0] — 2026-04-21

Sensor metadata enrichment for GUI classification and tooltip support.

### Added
- **Sensor metadata enrichment.** `/sensors` and `/poll` API responses now
  include `chip_name` (hwmon driver name from sysfs) and `temp_type`
  (thermistor type code from `tempN_type` sysfs) fields for each sensor.
  Enables the GUI to classify sensors with provenance-aware confidence levels.
- **Expanded sensor classification coverage.** Daemon now reads and exposes
  driver metadata for nct6775 family, nct6683/6686/6687, asus_ec_sensors,
  asus_wmi_sensors, gigabyte_wmi, and sbtsi_temp drivers.
- **`tempN_type` sysfs reading** during sensor discovery. Type codes: 3 =
  thermal diode, 4 = thermistor, 5 = AMD TSI, 6 = Intel PECI. Absent when
  the driver does not expose type information.
- **Label-based heuristics** for sensor kind classification during discovery.
  AMD TSI labels map to CpuTemp kind, Intel PECI labels map to CpuTemp kind,
  improving automatic categorization without manual configuration.

## [1.3.0] — 2026-04-21

Motherboard PWM diagnostics: BIOS interference detection, board identification,
and PWM effectiveness verification.

### Added
- **`pwm_enable` watchdog.** Every `set_pwm()` call now reads back `pwm_enable`
  to detect BIOS/EC reclaim (Gigabyte SmartFan 5/6, MSI Smart Fan, etc.). If
  the firmware has overridden manual mode, the daemon re-writes `pwm_enable=1`
  and forces a full PWM re-write. Cumulative revert counts are tracked per
  header and exposed in `/diagnostics/hardware`.
- **DMI board identification.** `/diagnostics/hardware` now includes a `board`
  object with `vendor`, `name`, and `bios_version` from SMBIOS/DMI sysfs.
  Enables the GUI to provide vendor-specific guidance (e.g., Gigabyte SmartFan
  degenerate-curve workaround instructions).
- **`POST /hwmon/{header_id}/verify` endpoint.** Behavioural test that writes
  a test PWM value, waits 3 seconds, then reads back `pwm_enable`, PWM, and
  RPM to classify the result as `effective`, `pwm_enable_reverted`,
  `pwm_value_clamped`, `no_rpm_effect`, or `rpm_unavailable`. Requires a
  valid hwmon lease.
- **System suspend/resume detection.** The polling loop compares
  `CLOCK_BOOTTIME` vs `CLOCK_MONOTONIC` to detect resume events. On resume,
  all per-header `manual_mode_set` flags are cleared, forcing the next
  `set_pwm()` to re-establish manual mode. Combined with the watchdog, this
  handles the common "fans revert after suspend" problem.
- **`enable_revert_counts` in diagnostics.** The `hwmon` section of
  `/diagnostics/hardware` now includes a per-header map of cumulative BIOS
  reclaim events, allowing the GUI to surface interference warnings.

## [1.2.0] — 2026-04-21

Hardware diagnostics API expansion for the GUI's new hardware readiness feature.

### Added
- **`GET /diagnostics/hardware` endpoint.** Returns comprehensive hardware
  readiness data: detected hwmon chips with driver identification, kernel
  module load status, ACPI I/O port conflict detection, GPU diagnostic
  details (PCI device ID, revision, ppfeaturemask), and thermal safety
  rule state. Enables the GUI to surface actionable guidance for hardware
  that requires out-of-tree drivers or BIOS configuration.
- **`device_id` field in `/hwmon/headers` response.** Each PWM header now
  includes the stable device identifier (PCI BDF or platform device name)
  used for chip instance disambiguation.
- **GPU PCI details in `/capabilities` response.** `amd_gpu` section now
  includes `pci_device_id`, `pci_revision`, and `gpu_zero_rpm_available`
  fields for precise GPU identification and diagnostic display.
- **Thermal safety state in cache.** Profile engine now reports thermal
  override state ("normal", "emergency", "recovery") to the state cache,
  surfaced via the new diagnostics endpoint.
- **ACPI conflict detection.** Scans `/proc/ioports` at request time for
  I/O port range overlaps between ACPI OperationRegions and known Super
  I/O chip addresses (Nuvoton 0x0290–0x0299, ITE 0x0A40–0x0A4F, etc.).
- **Kernel module detection.** Reads `/proc/modules` to check which hwmon
  driver modules are loaded, cross-referenced against expected drivers for
  detected chip names. Identifies out-of-tree vs mainline status.

## [1.1.6] — 2026-04-17

Safety and robustness hardening from full audit pass.

### Added
- **Panic hook for hardware safety.** Installs a `std::panic::set_hook`
  handler at startup that attempts to restore GPU fan curves (write `r\n` +
  `c\n` to PMFW `fan_curve`) and reset hwmon `pwm_enable` to `2` (automatic)
  on unrecoverable panic. Uses `OnceLock` to share restore targets with the
  panic handler without locking.

### Fixed
- **GPU `reset_to_auto()` skips zero-RPM re-enable on curve reset failure.**
  If `fan_curve` reset (`r\n` + `c\n`) failed, the function returned early
  without re-enabling `fan_zero_rpm_enable`. Now always attempts zero-RPM
  re-enable regardless of curve reset outcome, since PMFW writes are
  non-atomic and partial failure is expected.
- **Silent `daemon_state.json` load failures.** `load_state()` used
  `unwrap_or_default()` which silently dropped parse and I/O errors. Now
  logs explicit warnings for both corrupt JSON and unreadable files before
  falling back to defaults.
- **Config tests flaky under parallel execution.** `profiles_default_*` tests
  mutated `HOME`/`XDG_CONFIG_HOME` env vars, causing races when tests ran
  concurrently. Extracted a pure `profile_search_dirs_for(home, xdg_config)`
  function and rewrote tests to call it directly without env var mutation.

## [1.1.5] — 2026-04-17

Packaging improvement. No daemon logic changes.

### Added
- **Automatic Super I/O module loading.** Ship
  `/etc/modules-load.d/control-ofc.conf` that loads `nct6775`, `it87`,
  `w83627ehf`, and `drivetemp` at boot via `systemd-modules-load`. These
  are ISA-port-based chipset drivers that the kernel cannot auto-detect
  — without them, motherboard fan headers and some sensors are invisible
  to the daemon. Loading a module for absent hardware is harmless.
- `lm_sensors` added as `optdepends` for users whose hardware requires
  `sensors-detect` beyond the built-in module list.
- Hardware sensor modules section added to `docs/USER_GUIDE.md`.
- Version bumped to 1.1.5 (`daemon/Cargo.toml`, `packaging/PKGBUILD`).

### Changed
- **Streamlined install messages.** `post_install` reduced from 31 lines to
  10 — essential action first (enable service), sensor module loading,
  next steps (install GUI, GPU kernel param, docs link). Niche content
  (udev rules, config paths, profile details) moved to `USER_GUIDE.md`.
- **`post_upgrade` trimmed.** Removed stale 1.1.0 migration message (shim
  already removed in 1.1.3). Auto-strip function retained as safety net.
- Cross-references between daemon and GUI packages in install messages.

## [1.1.3] — 2026-04-12

Security hardening, error handling cleanup, and test coverage pass.
All quality gates remain green at 312 tests (290 unit + 22 integration).

### Security
- **SEC-1:** Reject path traversal (`..`) in profile name lookup (`find_profile`).
- **SEC-2:** Bound serial `read_line` with `Read::take(4096)` to prevent OOM from
  a malfunctioning device sending data without a newline terminator.
- **SEC-4:** Reject `..` in profile search directory paths passed via API.
- **SEC-7:** Reject `..` and null bytes in serial port path validation
  (`real_transport.rs`).

### Fixed
- **SSE stream omitted GPU fans.** The SSE `events_handler` built fan entries
  inline instead of using the shared `build_fan_entries()` helper, so GPU fan
  state was missing from the real-time stream. Now shares the same builder as
  `/fans` and `/poll`.
- **SSE client limit had a TOCTOU race.** Replaced `fetch_add` counter with
  `compare_exchange` CAS loop so two clients arriving simultaneously cannot
  both pass the `SSE_MAX_CLIENTS` check.
- Calibration PWM restore failures now logged instead of silently dropped.
- Lease renewal failures now logged at WARN.
- SIGHUP config reload failures now logged at ERROR (previously only returned
  a string that was silently dropped in one branch).

### Changed
- **Removed stale `migrate_legacy_runtime_keys`.** The one-release migration
  shim for `[profiles]`/`[startup]` from `daemon.toml` → `runtime.toml` was
  past its v1.1.0 deadline. Removed dead code from `main.rs`.
- **Removed dead `ConfigError::NotFound` variant and unused `DaemonError` enum**
  from `error.rs`.
- **Service unit: `SupplementaryGroups` reduced to `uucp` only.** `dialout`
  (Debian/Ubuntu) was dropped because systemd rejects the entire directive if
  any named group does not exist on the host. On Arch/CachyOS (the primary
  target), only `uucp` exists. Debian/Ubuntu users should add `dialout` via
  a systemd drop-in override.
- Extracted `apply_config_reload()` from the SIGHUP handler for testability.
- Shared `build_fan_entries` / `build_sensor_entries` between REST handlers
  and SSE stream, eliminating ~60 lines of duplication.
- Version bumped to 1.1.3 (`daemon/Cargo.toml`, `packaging/PKGBUILD`).

### Added
- 7 new tests: SSE CAS client limiting, `GuardedStream` counter drop,
  config reload (3 unit tests), GPU fan profile engine member evaluation.

## [1.1.2] — 2026-04-11

Packaging / installation cleanup pass. No daemon code changes — all quality
gates (`fmt`, `clippy -D warnings`, `cargo test`) remain green at 305 tests.
Addresses P1/P2 findings from the installation & systemd-config audit.

### Changed
- **udev rules are now documentation-only.** The shipped
  `99-control-ofc.rules` moves from `/usr/lib/udev/rules.d/99-control-ofc.rules`
  (where it was matching nothing because it still contained XXXX/YYYY
  placeholders) to `/usr/share/doc/control-ofc-daemon/99-control-ofc.rules.example`.
  The daemon already auto-detects the OpenFanController via
  `serial/real_transport.rs::auto_detect_port`, so no udev rule is required
  for normal operation. Users who want a stable `/dev/control-ofc-controller`
  symlink can `install -m644` the example into `/etc/udev/rules.d/` and edit
  there — following the canonical override pattern from `udev(7)`, so edits
  survive package upgrades.
- **Example profile shipped.** `/etc/control-ofc/profiles/quiet.json` now
  exists on fresh installs as a schema-valid example with an intentionally
  empty `members` array — safe to leave in place, drives no fans until the
  user customises it. Added to `backup=()` so pacman preserves user edits
  across upgrades via the standard `.pacnew`/`.pacsave` flow.
- **Rewrote the udev rules file header** so it explicitly documents the
  override path (`/etc/udev/rules.d/` overrides `/usr/lib/udev/rules.d/`
  overrides `/usr/share/doc/...`), the VID/PID discovery command, and the
  fact that the rule is optional. Previously the header told users to
  `cp` a file that doesn't exist at the path it suggested.
- **Service unit: dropped redundant `ReadWritePaths=/run/control-ofc
  /var/lib/control-ofc`.** systemd.exec(5) guarantees `RuntimeDirectory=`
  and `StateDirectory=` paths are writable under `ProtectSystem=strict`
  without an explicit `ReadWritePaths=` entry
  ([systemd#29798](https://github.com/systemd/systemd/issues/29798)).
  Only the `/sys/class/hwmon` and `/sys/class/drm` paths still need
  explicit allow-listing.
- **`post_install` and `post_upgrade` echoes rewritten** to mention the
  example profile, the auto-detect behaviour (no udev rule needed), and
  the new docs-only rules path.

### Added
- **`post_upgrade` auto-strips legacy `[profiles]` / `[startup]`
  sections from `/etc/control-ofc/daemon.toml`.** ADR-002 marks those
  sections as hard parse errors in 1.2.0; previously users would hit a
  startup crash the moment they upgraded past the shim window. The hook
  now backs the original file up to `daemon.toml.pre-1.1.2.bak` and uses
  a conservative `awk` script (top-of-line section headers only) to
  rewrite it in place. Safe to re-run; no-op when the sections are
  already absent. Preserves mode/owner from the original via
  `chmod --reference=` / `chown --reference=`.

### Not changed (flagged in audit, verified OK as-is)
- `systemctl daemon-reload` and `udevadm control --reload-rules` on
  upgrade are already provided by the base `systemd` package via
  `/usr/share/libalpm/hooks/30-systemd-daemon-reload-system.hook` and
  `/usr/share/libalpm/hooks/35-systemd-udev-reload.hook`, which trigger
  on any file installed under `/usr/lib/systemd/system/*` or
  `/usr/lib/udev/rules.d/*`. Our PKGBUILD uses those paths, so duplicating
  the reload in our `.install` hook would fire the same hook twice per
  transaction. Dismissed as a false positive from the audit.
- `daemon_state.rs` and `main.rs::resolve_initial_profile` already log at
  INFO / WARN when the persisted profile path or state file is missing
  (`main.rs:406-408`, `daemon_state.rs:80-84`). The audit claim that
  `daemon_state.rs:170` hardcoded a runtime reference to `quiet.json`
  turned out to be a test string literal; no runtime fix needed.

### Risk notes
- The `post_upgrade` TOML rewrite touches admin-owned config, which the
  ADR-002 "daemon never rewrites admin config" rule normally prohibits.
  The rule is scoped to the daemon process; the pacman `.install` hook is
  the packaging system performing a documented migration, which is an
  established Arch pattern. The backup file makes the change reversible.
- No changes to thermal safety, sysfs writes, serial reconnect, profile
  engine, IPC server lifecycle, or any other safety-critical path.

## [1.1.1] — 2026-04-11

### Fixed
- **First-run failure when the binary is invoked directly.** A tester
  reported that running `control-ofc-daemon` from a terminal as a regular
  user produced `ERROR IPC server error: Permission denied (os error 13)`
  but the daemon kept running, with the profile engine and polling loops
  live but no way to reach them. Root cause: the IPC server task's
  `create_dir_all("/run/control-ofc")` / `UnixListener::bind` both require
  root and the systemd-managed `RuntimeDirectory=control-ofc`; the error
  was logged and then ignored instead of terminating the daemon. Fix:
  - New `preflight_check` in `main.rs` runs **before any subsystem
    spawns**. It verifies `geteuid() == 0`, creates and probes the state
    directory for writability, and binds the IPC socket (with stale-file
    removal and the 0o666 chmod). Any failure prints an actionable
    stderr message pointing to `sudo systemctl enable --now
    control-ofc-daemon` and exits(1) — no more half-started zombie.
  - `api::server::serve` now takes an already-bound `UnixListener`
    instead of a path, so the bind happens once, synchronously, at
    startup. The mkdir / stale-remove / bind / chmod dance moved out of
    the async task.
  - The main `tokio::select!` now watches an `ipc_dead_rx` channel; if
    the IPC task ever exits with an error post-startup, the main loop
    breaks and the daemon shuts down cleanly (GPU reset, hwmon restore,
    socket cleanup) instead of running headless.
  - Specific error messages for `PermissionDenied` (points to systemctl)
    and `AddrInUse` (points to `systemctl status control-ofc-daemon`).

### Added
- **`--allow-non-root` hidden developer flag.** Skips the preflight EUID
  check for devs who want to run the binary directly with overridden
  `ipc.socket_path` and `state.state_dir`. Not listed in user-facing docs;
  mentioned only in `daemon.md` under the Running section. Hwmon / GPU /
  serial writes still require root regardless, so this is strictly for
  local IPC experimentation.
- **`libc` dependency** (0.2) for the `geteuid` call. Tiny, stable,
  already transitively present.

### Changed
- **`post_install` hint reordered.** The "start via systemctl" line is
  now the first thing users see, with an explicit "do NOT run the binary
  directly" follow-up. Previously this hint sat fourth in the list and
  was easy to skip.
- **`daemon.md`** gained a "Running" section that explains systemd is the
  only supported invocation path and what the preflight failure looks
  like.
- **Version bumped to 1.1.1** (`daemon/Cargo.toml`, `packaging/PKGBUILD`).
  Per project policy: any change on top of a local 1.1.0 bumps to 1.1.1.

### Risk notes
- Pre-1.1.0 versions wrote `persist_profile_search_dirs` back to
  `daemon.toml` under `ProtectSystem=strict`, which would also have
  failed under systemd; the runtime.toml split in 1.1.0 already fixed
  that. 1.1.1 only closes the remaining "binary run by hand" failure
  mode.
- No changes to thermal safety, sysfs writers, profile engine, or shutdown
  cleanup paths. Scope is bounded to startup validation and IPC task
  lifecycle.

## [1.1.0] — 2026-04-11

### Added
- **Runtime config split (ADR-002).** Daemon-mutable settings now live in
  `/var/lib/control-ofc/runtime.toml`, separate from admin-owned
  `/etc/control-ofc/daemon.toml`. The split mirrors the NetworkManager
  `NetworkManager-intern.conf` pattern: admin config is loaded first, runtime
  config is overlaid on top, and only the runtime file is ever rewritten by
  the daemon. SIGHUP re-reads and re-applies both. Full rationale and
  alternatives in `docs/ADRs/002-runtime-config-split.md`.
- **`runtime_config.rs` module.** `RuntimeConfig` struct with
  `#[serde(deny_unknown_fields)]`, atomic `save_to` (tmp+rename, 0600), and
  11 unit tests covering load/save roundtrip, defaults, malformed handling,
  missing parent dir creation, and owner-only permissions.
- **`ErrorEnvelope::persistence_failed` constructor.** Returns the new
  `persistence_failed` error code with `retryable: true` and
  `source: "internal"` for handlers that cannot persist state to disk.
- **Packaging: `/etc/control-ofc/profiles` directory.** PKGBUILD now
  creates the admin profile drop-in directory so operators can deposit
  curves without a `mkdir -p` dance on first install.

### Fixed
- **`POST /config/profile-search-dirs` and `POST /config/startup-delay`
  were silently losing writes across restarts.** Under
  `ProtectSystem=strict`, `/etc/control-ofc` is not in `ReadWritePaths=`,
  so the previous handlers hit `EROFS` when rewriting `daemon.toml`. The
  write failure was logged at WARN and the in-memory state updated anyway,
  producing "daemon forgets my settings after reboot" reports. Handlers
  now persist to `runtime.toml` inside the state directory (which *is*
  a `StateDirectory=`-managed writable path), **persist before mutating
  in-memory state**, and return `HTTP 503 persistence_failed` on any
  write error so the GUI can surface the failure. State can no longer
  diverge between RAM and disk.
- **`daemon_state.rs` comment drift.** Stale comment claiming the parent
  state dir was 0o700 replaced with an accurate description of
  `StateDirectoryMode=` defaulting to 0o755 and the file's 0o600 bits
  being the actual confidentiality boundary.

### Changed
- **`daemon.toml` is no longer rewritten by the daemon.** Admin-authored
  comments and formatting are preserved across restarts and package
  upgrades. The `persist_profile_search_dirs` and `persist_startup_delay`
  functions (and their tests) have been deleted from `config.rs`.
- **`packaging/daemon.toml.example`** now documents only the admin-static
  keys and points to `runtime.toml` for the daemon-managed ones.
- **Version bumped to 1.1.0** (`daemon/Cargo.toml`, `packaging/PKGBUILD`).

### Migration (one-release shim; removed in 1.2.0)
- `DaemonConfig` still parses `[profiles]` and `[startup]` from
  `daemon.toml`. On first start after upgrade, `migrate_legacy_runtime_keys`
  copies those sections into `runtime.toml` if the runtime file does not
  already contain them. The legacy sections in `daemon.toml` are **not**
  deleted (the daemon never rewrites admin-owned config) but are shadowed
  from that point forward. An INFO line logs which keys were migrated.
- **1.2.0 will make `[profiles]` / `[startup]` in `daemon.toml` a hard
  parse error.** Operators should remove those sections at their leisure
  during the 1.1.x window.

### Future release candidate
- Optional `200 OK + { persisted: false, advisory: "..." }` contract for
  persistence failures, instead of 503. Documented in ADR-002 as deferred
  work; revisit if users report disk-full / read-only `/var/lib` scenarios
  where they still want the in-memory change to take effect.

## [1.0.1] — 2026-04-11

### Added
- **`.github/workflows/release-aur.yml`** — GitHub Actions workflow that publishes to the AUR automatically when a release tag (`v*.*.*`) is pushed. Strict verify-and-fail: refuses to publish if `packaging/PKGBUILD` was not bumped before tagging, or if its `sha256sums` does not match the GitHub release tarball. Delegates the AUR clone/commit/push to [`KSXGitHub/github-actions-deploy-aur@v4.1.2`](https://github.com/KSXGitHub/github-actions-deploy-aur), which runs inside an Arch container and regenerates `.SRCINFO` automatically. Requires a one-time `AUR_SSH_PRIVATE_KEY` repository secret.
- **`scripts/release-aur.sh`** — manual fallback that mirrors the workflow's behaviour. Verifies the GitHub tarball sha256 matches `packaging/PKGBUILD`, clones (or ff-pulls) `ssh://aur@aur.archlinux.org/control-ofc-daemon.git` into `~/Development/aur/control-ofc-daemon/`, regenerates `.SRCINFO` via `makepkg --printsrcinfo`, and commits/pushes with explicit confirmation prompts (`--yes` to skip, `--no-push` to stage only). Run from the repo root as `./scripts/release-aur.sh <version>` after bumping `packaging/PKGBUILD`.

### Fixed
- **Profile activation creates a write dead zone when the GUI was recently active.** `POST /profile/activate` swapped in the new profile but did not refresh `last_gui_write_at`, so if the GUI had written within the last `GUI_ACTIVITY_TIMEOUT` (30s) the profile engine continued deferring to the GUI while the GUI, believing nothing had changed (the profile name is identical), never issued a new write. Result: OpenFan fans held their previous PWM for up to a minute after activation. Fix: `activate_profile_handler` now calls `state.cache.record_gui_write()` immediately after applying the new profile, giving the GUI a fresh 30s window of exclusive write ownership over the new curves. The matching GUI-side fix (an explicit `reevaluate_now()` to bypass the suppressed `active_profile_changed` signal) is tracked in the GUI CHANGELOG.
- **Boot-time OpenFanController detection race.** On cold boot the daemon could
  start before the `cdc_acm` kernel module loaded, at which point systemd
  silently dropped `DeviceAllow=char-ttyACM rwm` (class unresolved in
  `/proc/devices`). The USB device then appeared shortly after, but every open
  returned `Operation not permitted` because the cgroup device filter never
  included a ttyACM rule. Manual `systemctl restart` masked the issue because
  `cdc_acm` was loaded by then. Fixed by adding
  `Wants=modprobe@cdc_acm.service` + `After=modprobe@cdc_acm.service` to the
  unit's `[Unit]` section, per the workaround documented in
  `systemd.resource-control(5)`. Reinstall the service file (or reinstall the
  package) and run `systemctl daemon-reload` to pick up the change.

## [1.0.0] — 2026-04-08

Content update establishing the daemon's canonical paths, service names, and
identifiers used from this release onward: the `control-ofc-daemon` crate and
binary, the `control-ofc-daemon.service` systemd unit, the
`/run/control-ofc/control-ofc.sock` Unix socket, the `/etc/control-ofc/` and
`/var/lib/control-ofc/` runtime directories, the `CONTROL_OFC_CONFIG`
environment variable, and the `99-control-ofc.rules` udev rules with the
`/dev/control-ofc-controller` symlink.

## [0.7.2] — 2026-04-08

### R70 — Pre-release Security Hardening (V5 Phase 6)

Addresses Rust daemon findings from the V5 Phase 6 security & dependencies audit.

- **S3 (P2):** State file (`daemon_state.json`) now explicitly set to 0o600 (owner-only) before atomic rename. Defense-in-depth — parent dir is already 0o700 via systemd `StateDirectory=`. Added permission verification test.
- **S4 (P3):** Documented why root is required and why `CapabilityBoundingSet` is intentionally deferred in `control-ofc-daemon.service`.
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
- **Feature:** SIGHUP config reload — daemon re-reads `daemon.toml` and updates profile search dirs in memory. Enables `systemctl reload control-ofc-daemon`.
- **Feature:** `POST /config/profile-search-dirs` API endpoint — GUI (or any client) can add profile search directories at runtime. Daemon validates, updates in-memory state, and persists to `daemon.toml` atomically.
- **Feature:** Multi-user support — each GUI user can register their profile directory via the API; the daemon merges all dirs and preserves `/etc/control-ofc/profiles`.
- **Fix:** `profile_search_dirs` in AppState is now `RwLock<Vec<PathBuf>>` — safely mutable at runtime.
- Added `ExecReload=/bin/kill -HUP $MAINPID` to systemd service file.
- Added `config_path` to AppState so handlers can persist config changes.
- 2 new config persistence tests, 281 total.

## [0.5.9] — 2026-04-07

### R63 — Fix Profile Activation Path Validation (completes R62)
- **Fix:** `default_profile_search_dirs()` now falls back to `/root/.config/control-ofc/profiles` when neither `HOME` nor `XDG_CONFIG_HOME` is set (common for systemd services running as root without `User=`).
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
- **Config path override:** Daemon config path now overridable via `--config` CLI arg or `$CONTROL_OFC_CONFIG` env var (default: `/etc/control-ofc/daemon.toml`). Supports container deployments and dev testing.
- **Serial fallback expanded:** Direct probe fallback now scans `/dev/ttyUSB0-9` in addition to `/dev/ttyACM0-9`, covering FTDI/CH340 adapters when libudev is unavailable.
- **Service file portability:** `DeviceAllow` now uses `char-ttyACM` and `char-ttyUSB` class wildcards instead of hardcoded `/dev/ttyACM0-1`. `SupplementaryGroups` includes both `uucp` (Arch) and `dialout` (Debian) — systemd ignores missing groups.
- **Documentation:** Added serial setup instructions, VID/PID discovery, udev rule configuration, and config override usage to USER_GUIDE and DEVELOPER_HANDOVER.
- 1 new test: `load_from_custom_path` (254 total)

### R50 — Daemon Persisted-State Hardening
- **Fix:** `daemon_state.json` writes failed with `EROFS (Read-only file system, os error 30)` under systemd `ProtectSystem=strict` sandbox
- **Root cause:** systemd service file was missing `StateDirectory=control-ofc` and `/var/lib/control-ofc` was not in `ReadWritePaths`
- Added `StateDirectory=control-ofc` to systemd unit — systemd now creates and manages `/var/lib/control-ofc` with correct ownership
- Added `/var/lib/control-ofc` to `ReadWritePaths` for belt-and-suspenders protection
- State directory now configurable via `[state] state_dir` in `daemon.toml` (default: `/var/lib/control-ofc`)
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
- Added systemd unit file (`packaging/control-ofc-daemon.service`) with security hardening
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

[Unreleased]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.6...HEAD
[1.1.6]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.5...v1.1.6
[1.1.5]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.4...v1.1.5
[1.1.4]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.3...v1.1.4
[1.1.3]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.2...v1.1.3
[1.1.2]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.0.0...v1.0.1
