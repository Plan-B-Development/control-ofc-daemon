# ADR-002: Runtime Config Split (admin daemon.toml + daemon runtime.toml)

**Status:** Accepted
**Date:** 2026-04-11

## Context

Two API endpoints mutate persistent daemon configuration at runtime:

- `POST /config/profile-search-dirs` — GUI registers its user profile directory.
- `POST /config/startup-delay` — GUI tunes the pre-device-detection delay.

Before 1.1.0 both writes went back into `/etc/control-ofc/daemon.toml`. They
failed silently under the hardened systemd unit: `ProtectSystem=strict` makes
`/etc` read-only, and `/etc/control-ofc` was never added to `ReadWritePaths=`.
The write returned `EROFS` → logged WARN → dropped. In-memory state updated;
next restart reverted. Users reported "the daemon forgets my profile
directory after a reboot", and the root cause was systemd sandboxing, not
anything in the handler.

## Options Considered

| # | Option | Pros | Cons |
|---|---|---|---|
| A | Add `/etc/control-ofc` to `ReadWritePaths=` | 1-line fix, keeps single config file | Daemon rewrites admin-owned config, comments and layout trampled, violates FHS (`/etc` is admin config, not daemon state), makes the unit file less sandbox-tight |
| B | Split admin config (`/etc`) from daemon-mutable config (`/var/lib`) | FHS-correct, admin-authored file stays pristine, sandbox stays tight, matches the NetworkManager / udev / systemd-networkd pattern | Two files instead of one; needs a migration shim for existing installs |
| C | Make the endpoints transient (runtime-only, lost on restart) | Simplest | GUI has to re-register every restart; startup delay can't actually be tuned (it's consumed before the GUI connects) |

Option B is the production norm. NetworkManager uses exactly this split:
`/etc/NetworkManager/NetworkManager.conf` is admin-owned; daemon-mutable keys
land in `/var/lib/NetworkManager/NetworkManager-intern.conf`, which is
read last and shadows admin config. systemd-networkd and udev follow the
same pattern with their `drop-in` directories under `/run` and `/etc`.

## Decision

**Option B.** Daemon-mutable configuration lives in `runtime.toml` inside
the configured state directory (default `/var/lib/control-ofc/runtime.toml`).
`/etc/control-ofc/daemon.toml` is admin-owned and never rewritten by the
daemon.

### File roles

| Path | Owner | Contents | Permissions |
|---|---|---|---|
| `/etc/control-ofc/daemon.toml` | admin / pacman | static topology: serial port, polling interval, socket path, state dir | 0644, in pacman `backup=()` |
| `/var/lib/control-ofc/runtime.toml` | daemon | `[profiles] search_dirs`, `[startup] delay_secs` | 0600, atomic tmp+rename |
| `/var/lib/control-ofc/daemon_state.json` | daemon | active profile selection | 0600, atomic tmp+rename |

### Precedence

1. `DaemonConfig` is loaded from `/etc/control-ofc/daemon.toml` (admin).
2. `RuntimeConfig` is loaded from `{state_dir}/runtime.toml` (daemon).
3. Any key present in both resolves to the **runtime** value (daemon wins).

The resolved view is what the rest of the daemon sees. SIGHUP re-reads both
files and re-applies the overlay.

### Persistence error contract

On a `POST /config/profile-search-dirs` or `POST /config/startup-delay` call,
the handler persists to `runtime.toml` **first** and only updates in-memory
state on success. A failed write returns:

```
HTTP/1.1 503 Service Unavailable
{
  "error": {
    "code": "persistence_failed",
    "message": "...",
    "retryable": true,
    "source": "internal"
  }
}
```

This is an explicit pessimistic contract: in-memory state never diverges
from on-disk state, and the GUI can surface the failure to the user.

## Migration

One-release shim (1.1.x only; removed in 1.2.0):

- `DaemonConfig` still parses `[profiles]` and `[startup]` sections in
  `daemon.toml` so pre-1.1.0 installations keep booting.
- On first start after upgrade, `migrate_legacy_runtime_keys` copies those
  sections from `daemon.toml` into `runtime.toml` if `runtime.toml` doesn't
  already contain the key. The legacy sections in `daemon.toml` are left
  untouched — we do not rewrite admin-owned files — but are effectively
  shadowed after migration.
- An INFO line logs which keys were migrated, so operators can clean up
  `daemon.toml` at their leisure.

In 1.2.0 we drop the shim: `[profiles]` / `[startup]` in `daemon.toml` will
become hard parse errors (`deny_unknown_fields`).

## Alternatives documented for future revision

### 200 OK + in-body advisory on persist failure

Instead of 503, return 200 with `{ "applied": true, "persisted": false,
"advisory": "..." }`. Pros: GUI still sees the new state applied; admin
gets a loud warning; no "API call looks failed but the daemon did apply it"
confusion. Cons: a non-trivial contract (clients must read an advisory
field to know that restarts will lose the setting); harder to distinguish
from success in monitoring. **Deferred** — revisit if users report they
cannot persist `search_dirs` due to disk-full or read-only `/var/lib`
scenarios and still want the in-memory change to take effect. Tracked for a
post-1.1 release.

### Full systemd credential / drop-in approach (`/etc/control-ofc/daemon.conf.d/`)

Systemd-style drop-in directories would keep everything under `/etc` and
solve the ordering by lexical filename priority. Rejected because (a) the
daemon still cannot write to `/etc` under `ProtectSystem=strict` without
widening the sandbox, so we'd be right back at Option A's trade-off, and
(b) we don't yet need the fan-out of multiple config sources that drop-in
dirs are designed for.

## Consequences

- The systemd unit's `ProtectSystem=strict` stays intact; `/etc/control-ofc`
  is not in `ReadWritePaths=`.
- Admin-owned config never gets rewritten, so hand-edited comments and
  formatting are preserved across upgrades.
- Operators upgrading from 1.0.x may still see the `[profiles]` section in
  their `daemon.toml` for the 1.1.x window — harmless, and a one-line
  migration note is printed by the `.install` script.
- Tests must not rely on the global `STATE_DIR` OnceLock; the `load_from` /
  `save_to` pair takes an explicit path for isolation under parallel test
  execution.

## References

- NetworkManager-intern.conf: `nm-settings-intern(5)`
- FHS 3.0 § 3.7 (`/etc`), § 5.8 (`/var/lib`)
- systemd.exec(5) `ProtectSystem=strict` / `StateDirectory=`
- Commit that revealed the root cause: `7285e47` context in journal
- Prior decisions: DEC-083, DEC-084, DEC-085 (earlier profile-search-dir work)
