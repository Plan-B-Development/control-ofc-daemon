# ADR-002: Runtime config split — `daemon.toml` vs `runtime.toml`

**Status:** Accepted (v1.1.0). The originally-planned v1.2.0 removal of legacy
`[profiles]` / `[startup]` parsing was **superseded** — those sections are
retained as valid admin-layer *defaults* (the base layer that `runtime.toml`
overlays), parsed and never a parse error. See `daemon.md` for the current state.
**Last reviewed:** 2026-07.

> **Status as of v1.6.x:** This ADR is now historical for migration
> purposes — the v1.0 → v1.1 → v1.2 transition has been baked into the
> codebase for years of release cycles. The split it documents (admin-owned
> `daemon.toml` vs daemon-managed `runtime.toml`) is the current
> architecture. The migration sections below describe what *happened* once,
> not what an operator upgrading from v1.5 → v1.6 needs to do (nothing).
> Pre-1.0 operators see the daemon `README.md` § Upgrade notes.

## Context

The daemon has two kinds of configuration:

1. **Static topology** — serial port, polling interval, IPC socket
   path, state directory, log level. Edited by the operator. Should
   never be rewritten by the daemon.
2. **Runtime-mutable settings** — the keys that API endpoints can
   change at runtime, such that the change must persist across
   restarts. These are currently:
   - `[profiles] search_dirs` (mutated by
     `POST /config/profile-search-dirs`);
   - `[startup] delay_secs` (mutated by `POST /config/startup-delay`);
   - `[hardware] preferred_cpu_sensor` / `preferred_mb_sensor` (mutated by
     `POST /config/preferred-cpu-sensor` / `-mb-sensor`; DEC-200).

In v1.0.x both kinds lived in a single file at
`/etc/control-ofc/daemon.toml`. The daemon's `POST /config/*`
handlers would round-trip the file: read, mutate, write. Two problems
emerged:

- **Comment loss**: any operator comments in `daemon.toml` were
  stripped by the round-trip because the TOML serializer doesn't
  preserve them. Operators using inline annotations (very common for
  a config file in `/etc/`) lost them on the first API write.
- **Permission ownership clash**: `/etc/control-ofc/daemon.toml` is
  pacman-owned (declared in `backup=()`). Pacman's `.pacnew` mechanism
  assumes the daemon never writes the file; if the daemon does, an
  upgrade that ships a new default surfaces a `.pacnew` against a file
  the daemon has already mutated, producing confusing diffs.

We need a clean split where `/etc/control-ofc/daemon.toml` is read-only
to the daemon and runtime mutations land somewhere else.

## Decision

**Two configuration files.**

| File | Owner | Path | Mutated by |
|---|---|---|---|
| `daemon.toml` | admin | `/etc/control-ofc/daemon.toml` | hand-edit only |
| `runtime.toml` | daemon | `{state_dir}/runtime.toml` (default `/var/lib/control-ofc/runtime.toml`) | `POST /config/*` |

This mirrors the **NetworkManager pattern**:

- `/etc/NetworkManager/NetworkManager.conf` (admin-owned) +
- `/var/lib/NetworkManager/NetworkManager-intern.conf` (daemon-owned,
  read last, shadows admin).

### Loading and precedence

On startup:

1. `DaemonConfig` is loaded from `/etc/control-ofc/daemon.toml`.
2. `RuntimeConfig` is loaded from `{state_dir}/runtime.toml`.
3. For any key present in both, the runtime value wins.

`SIGHUP` re-runs the same sequence and re-applies the overlay.

### Writes

- `runtime.toml` is written via atomic tmp+rename with mode `0o600`.
- The daemon never writes to `daemon.toml`. Period.
- If `runtime.toml` write fails, the API returns
  `503 persistence_failed` with `retryable: true` so the GUI can
  surface the error and retry.

### Migration (1.0.x → 1.1.x → 1.2.0)

- **1.1.x**: `[profiles]` and `[startup]` in `daemon.toml` are parsed as the
  base admin-layer defaults; `runtime.toml` overlays them when an API call
  mutates a runtime-mutable key. The daemon never deletes the `daemon.toml`
  sections and — contrary to an earlier draft of this ADR — does **not** copy
  them into `runtime.toml`: the two files simply coexist.
- **1.2.0+ (plan superseded)**: the original plan was to make parsing
  `[profiles]` / `[startup]` from `daemon.toml` a hard error at startup,
  auto-stripped by a PKGBUILD `post_upgrade`. That was **not** shipped. The
  sections remain valid admin-layer defaults — `config.rs` still parses them
  (guarded by the `parse_profiles_section` / `parse_startup_delay_section`
  tests) and they never become a parse error. See `daemon.md` §
  "`daemon.toml` `[profiles]` / `[startup]` vs `runtime.toml`" for the
  authoritative current behaviour.

## Consequences

**Positive**

- Operator comments in `daemon.toml` are preserved across the daemon's
  lifetime — the daemon never touches the file.
- Pacman's `.pacnew` workflow stays clean. `daemon.toml` is in
  `backup=()` and the upstream-shipped defaults can change without
  confusing the operator's edits.
- `/var/lib/control-ofc/` is already declared as the daemon's state
  directory (`StateDirectory=control-ofc` in the unit), so the runtime
  file lives in a path systemd guarantees exists and is writable.
- The split is invisible to API consumers — endpoints continue to read
  `merged.profiles.search_dirs` etc.

**Negative**

- Two files instead of one. An operator now has to understand which
  file holds which key. Mitigated by a comment in the shipped
  `daemon.toml.example` and by `man control-ofc-daemon`.
- A backup/restore script that previously copied `daemon.toml` alone
  now also has to copy `runtime.toml` to fully preserve daemon state.
  The Operations Guide documents this.
- Migration window: 1.1.x → 1.2.0 needs the post_upgrade strip to run
  successfully. The strip is `awk` over a `cp -a` backup and is
  re-runnable; failure modes leave the legacy sections in place with
  a printed warning.

## Alternatives considered

- **Single file, comment-preserving serializer** (`toml_edit`):
  technically possible but the round-trip still risks subtle reorders
  and whitespace changes, and it doesn't solve the
  pacman-`.pacnew`-ownership problem. Rejected.
- **Runtime values in `daemon_state.json`**: `daemon_state.json`
  already exists for active-profile persistence. Bundling
  configuration in there confuses two concerns ("what was the user's
  selection?" vs "what knobs has the user dialed?") and forces a
  schema migration when settings change. Rejected.
- **Drop runtime persistence entirely; require a daemon restart for
  every settings change**: would break a UX requirement
  (`POST /config/profile-search-dirs` takes effect immediately).
  Rejected.
- **Per-section files** (`runtime/profiles.toml`,
  `runtime/startup.toml`): more files for negligible benefit. Rejected.

## References

- `daemon/src/runtime_config.rs` — type and serde model.
- `daemon/src/api/handlers/config.rs` — write path with atomic
  tmp+rename and `503 persistence_failed` envelope.
- `daemon/src/main.rs` — `apply_runtime_overlay()` (defined ~`main.rs:257`, invoked at startup ~`main.rs:552`).
- `packaging/control-ofc-daemon.install` —
  `_strip_legacy_runtime_sections` (1.1.x → 1.2.0 migration).
- `daemon.md` § Configuration — operator-facing summary.
