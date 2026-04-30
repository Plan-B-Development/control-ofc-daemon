# ADR-002: Runtime config split — `daemon.toml` vs `runtime.toml`

**Status:** Accepted (v1.1.0; legacy parsing removed in v1.2.0).
**Last reviewed:** 2026-04 (no change; PKGBUILD `post_upgrade` strips
remaining legacy sections automatically).

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
   - `[startup] delay_secs` (mutated by `POST /config/startup-delay`).

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

- **1.1.x**: still parses `[profiles]` and `[startup]` from
  `daemon.toml` for backward compatibility. On first start after
  upgrade, the daemon copies those sections into `runtime.toml` if the
  runtime file does not already contain them. The legacy sections in
  `daemon.toml` are not deleted by the daemon — that would violate
  the rule above — but they are shadowed.
- **1.2.0+**: parsing `[profiles]` / `[startup]` from `daemon.toml` is
  a hard error at startup. The PKGBUILD's `post_upgrade` function
  (`_strip_legacy_runtime_sections`) auto-removes them on package
  upgrade and saves a backup at `daemon.toml.pre-1.1.2.bak`.

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
- `daemon/src/main.rs:452` — startup overlay logic.
- `packaging/control-ofc-daemon.install` —
  `_strip_legacy_runtime_sections` (1.1.x → 1.2.0 migration).
- `daemon.md` § Configuration — operator-facing summary.
