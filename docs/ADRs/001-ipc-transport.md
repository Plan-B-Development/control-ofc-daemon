# ADR-001: IPC Transport Choice

**Status:** Accepted
**Date:** 2026-03-09

## Context
The daemon needs a local-only IPC mechanism for the Python GUI to query state (health, sensors, fans) and later send control commands.

## Decision
**HTTP over Unix domain socket** using axum + tokio.

## Options Considered
1. **HTTP over UDS (chosen)** — familiar request/response model, excellent tooling (curl, httpie), easy to test, mature ecosystem (axum/tower).
2. **Custom JSON-over-UDS** — simpler framing but requires custom client, no ecosystem tooling.
3. **gRPC over UDS** — strong schemas but heavy dependency (tonic/protobuf), overkill for local IPC.
4. **D-Bus** — Linux-native but complex API surface, poor fit for structured data queries.

## Rationale
- HTTP semantics map cleanly to read/write operations
- Unix socket provides OS-level access control (file permissions)
- axum is lightweight, tower-based, and has built-in UDS support
- GUI can use any HTTP client (httpx in Python already proven in legacy codebase)
- Easy to test with curl: `curl --unix-socket /run/control-ofc/control-ofc.sock http://localhost/status`

## Socket Path
- Default: `/run/control-ofc/control-ofc.sock` (configurable via `ipc.socket_path` in config)
- Directory created on startup if missing
- Stale socket file removed on startup
- Socket file cleaned up on shutdown

## Permission Strategy
- Socket owned by daemon user, permissions set to `0666` for non-root GUI access (DEC-049)
- GUI connects as desktop user via the world-accessible socket

## Schema Versioning
- All responses include `api_version: 1`
- v1 fields are stable; changes are additive only
- Breaking changes require v2 namespace

## Consequences
- Adds tokio, axum, serde_json, tower as dependencies
- Daemon moves from sync to async main
- IPC never blocks hardware polling (axum handles connections on separate tasks)
