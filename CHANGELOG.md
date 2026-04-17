# Changelog

## [Unreleased]

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

### Project Rebrand — OnlyFans → Control-OFC

**BREAKING CHANGE:** Complete project rebrand. All paths, service names, and identifiers have changed.

- **Crate name:** `onlyfans-daemon` → `control-ofc-daemon`
- **Binary name:** `onlyfans-daemon` → `control-ofc-daemon`
- **systemd unit:** `onlyfans-daemon.service` → `control-ofc-daemon.service`
- **Socket path:** `/run/onlyfans/onlyfans.sock` → `/run/control-ofc/control-ofc.sock`
- **Config dir:** `/etc/onlyfans/` → `/etc/control-ofc/`
- **State dir:** `/var/lib/onlyfans/` → `/var/lib/control-ofc/`
- **Profile dirs:** `/etc/onlyfans/profiles/`, `~/.config/onlyfans/profiles/` → `/etc/control-ofc/profiles/`, `~/.config/control-ofc/profiles/`
- **Runtime dir:** `/run/onlyfans/` → `/run/control-ofc/`
- **Env var:** `ONLYFANS_CONFIG` → `CONTROL_OFC_CONFIG`
- **udev rules:** `99-onlyfans.rules` → `99-control-ofc.rules`
- **udev symlink:** `/dev/onlyfans-controller` → `/dev/control-ofc-controller`
- **Restore script:** `onlyfans-restore-auto` → `control-ofc-restore-auto`

**Migration:** Users upgrading from the OnlyFans-named installation must:
1. Stop and disable the old service: `sudo systemctl disable --now onlyfans-daemon`
2. Remove old service file: `sudo rm /etc/systemd/system/onlyfans-daemon.service`
3. Rename config: `sudo mv /etc/onlyfans /etc/control-ofc`
4. Rename state: `sudo mv /var/lib/onlyfans /var/lib/control-ofc`
5. Update profile search dirs in `daemon.toml` (replace `onlyfans` with `control-ofc`)
6. Install new binary, service, and udev rules
7. `sudo systemctl enable --now control-ofc-daemon`

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
