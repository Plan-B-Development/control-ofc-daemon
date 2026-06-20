# ADR-001: IPC transport — HTTP over Unix domain socket

**Status:** Accepted (v1.0.0).
**Last reviewed:** 2026-04 (no change).

## Context

The daemon needs an inter-process control channel for at least one local
client (`control-ofc-gui`) and potentially future ones (CLI, scripts,
diagnostic tools). The channel must:

- be local-only (no network exposure);
- carry small request/response payloads (capabilities, sensor reads,
  PWM writes, lease operations);
- support an optional one-way push stream for live sensor/fan updates
  (the `GET /events` SSE endpoint, see DEC-024 in the GUI repo);
- be observable with standard tools (`curl`, `socat`, `journalctl`);
- impose no ceremony on the GUI (which is a Python/PySide6 app —
  introducing a custom binary protocol would be friction);
- support per-connection error envelopes with structured codes for
  retry/UX decisions on the GUI side.

The daemon runs as root; the GUI runs as a normal user. The transport
must allow that asymmetry without forcing the GUI to elevate.

## Decision

The daemon serves an **HTTP/1.1 API over a Unix domain socket** at
`/run/control-ofc/control-ofc.sock`. The socket file is `0666` after
bind (DEC-049) so non-root clients can connect.

Implementation:

- **axum 0.x** as the HTTP framework;
- **tokio** as the async runtime;
- the socket path is driven by `ipc.socket_path` in `daemon.toml`
  (default `/run/control-ofc/control-ofc.sock`, created by
  `RuntimeDirectory=control-ofc` in the systemd unit);
- request bodies are JSON; SSE responses are `text/event-stream`;
- a server-side concurrent-client cap (`SSE_MAX_CLIENTS = 5`) gates `GET /events`
  with `503 too_many_clients` to prevent fan-out-induced starvation of
  the control loop.

## Consequences

**Positive**

- The GUI uses `httpx` (already a dep) to talk to the daemon via a
  trivial Unix-socket transport. No custom protocol code on either side.
- `curl --unix-socket /run/control-ofc/control-ofc.sock http://./status`
  is the diagnostic shortcut. `journalctl -u control-ofc-daemon` shows
  axum's structured access logs.
- Errors are a uniform `{"error": {"code", "message", "retryable",
  "source", "details"}}` envelope. The GUI's `ApiClient` matches on
  `code` and renders deterministic UX. New error variants are additive.
- Permissions on the socket file are the only access-control surface,
  and they're managed by systemd via `RuntimeDirectory=`.

**Negative**

- Local-only: no remote management. Acceptable for V1 — this is a
  desktop fan controller, not a server tool.
- HTTP framing has overhead vs. raw line-protocol or protobuf. The
  payloads are small enough that this never shows up in profiles.
- A future move to D-Bus would be a hard break for any third-party
  client. We are accepting this; D-Bus would buy us policy integration
  but add significant complexity.

## Alternatives considered

- **D-Bus (system bus)**: idiomatic Linux IPC, comes with permission
  policy via `policy-kit`. Rejected because the per-method introspection
  surface and XML interface definitions are heavy for a project with
  one shipping client. Re-evaluate if a second non-trivial client lands.
- **gRPC over Unix socket**: type-safe, cross-language, but requires
  generated stubs and protoc tooling on the GUI side. Friction for a
  Python desktop app; the JSON envelope is already adequate.
- **Raw line protocol**: minimal, no framework dep, but every error
  envelope and SSE-equivalent must be hand-rolled. Reinventing HTTP.
- **TCP loopback**: same wire shape as HTTP-over-UDS but exposes the
  port to local users without an explicit ACL surface. UDS files have
  POSIX permissions; loopback ports do not.

## References

- `daemon/src/api/server.rs` — server bootstrap and concurrent-client
  cap.
- `daemon/src/api/responses.rs` — error envelope.
- DEC-024 (GUI repo): "GUI does not consume `/events` in V1; relies on
  1 Hz polling."
- DEC-049: "Socket must be `chmod 0666` after bind to allow non-root
  GUI connections."
