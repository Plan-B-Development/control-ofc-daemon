# claude.md — Control-OFC Daemon (Rust) Development Brief

## 1) Goal
Build a Rust daemon that:
- Owns hardware communication:
  - OpenFanController USB serial protocol
  - hwmon sysfs discovery + reading (sensors)
  - hwmon sysfs PWM control (ITE Super I/O)
- Owns safety logic
- Provides polling, caching, staleness tracking, and health status
- Provides stable local-only IPC (Unix socket preferred) for a Python GUI

## 2) Non-goals
- No GUI implementation in this plan (GUI is separate)
- No cloud services

## 3) Authoritative repo docs
- `CLAUDE.md` is the authoritative agent instruction set and hard constraints.
- `docs/ADRs/` stores architecture decisions.

## 4) Reference material (informational)
- `old-resources\` Legacy/previous implementation source code (reference only).
- `KNOWLEDGE_EXPORT.md` (reference only, except protocol rules and explicit “must not regress” items).
- `project-context.xml` old project architecture. for informational use only.

## 5) Quality gates (must pass for every merge)
### Rust daemon
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-features`

### Python GUI (when applicable)
- `ruff format --check`
- `ruff check`
- `pytest`

## 6) Milestones (thin vertical slices)

### Milestone 0 — Repo + scaffolding (no hardware)
**Goal:** Rust daemon workspace + module layout + minimal runnable binary + basic error/config scaffolding.  
**Non-goals:** No IPC server, no hardware comms, no polling loops.

**Tasks**
- [x] M0.1 Create workspace/crate layout (`daemon/` or equivalent)
- [x] M0.2 Minimal daemon entrypoint (start, log, shutdown)
- [x] M0.3 Error type skeleton (structured, stable)
- [x] M0.4 Config scaffolding + validation skeleton
- [x] M0.5 Minimal unit tests (config validation, error formatting)
- [x] M0.6 Minimal developer notes (how to build/test/run)

**Exit criteria**
- [x] Builds and runs
- [x] All quality gates pass
- [x] Clear module placeholders exist for Milestone 1+

**Verification commands**
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-features`
- `cargo run`

---

### Milestone 1 — OpenFanController protocol layer (no polling loop)
**Goal:** Robust encoding/decoding + tests for serial protocol.  
**Non-goals:** No polling, no fan control logic beyond message encode/decode, no IPC, no hwmon.

**Tasks**
- [x] M1.1 Protocol types: `Command`, `Response`, `Channel`
- [x] M1.2 Encode commands (ASCII hex pairs + `\n`)
- [x] M1.3 Decode responses; ignore non-`<` lines
- [x] M1.4 Golden tests (encode + decode + error cases)
- [x] M1.5 Minimal transport wrapper (timeouts only; retries/backoff later)

**Exit criteria**
- [x] Encode is exact and tested
- [x] Decode handles valid/invalid inputs with structured errors
- [x] Golden tests exist and pass
- [x] All quality gates pass

**Verification commands**
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-features`

---

### Milestone 2 — Sensor collection (read-only)
**Goal:** Read CPU/GPU/disk/MB temps with stable IDs.  
**Non-goals:** No control writes, no IPC (unless explicitly needed).

**Tasks**
- [x] M2.1 hwmon discovery + stable sensor IDs (chip + label)
- [x] M2.2 hwmon temp reads
- [x] M2.3 disk temp strategy — NVMe temps via hwmon (no separate provider needed)
- [x] M2.4 GPU temp strategy — amdgpu temps via hwmon (no separate provider needed)
- [x] M2.5 Tests using fixture/mocked fs (tempfile crate, 19 tests)

**Exit criteria**
- [x] Allowlisted temps available as structured data with stable IDs
- [x] Tests cover discovery/parsing
- [x] All quality gates pass

---

### Milestone 3 — Cache + staleness + health model
**Goal:** In-memory cache with timestamps and computed health.  
**Tasks**
- [x] M3.1 Define state model (fans (motherboard and openfan), sensors, AIO pump) and labels (openfan, hwmon, aio_hwmon, aio_usb)
- [x] M3.2 Cache update + timestamps
- [x] M3.3 Staleness thresholds + computed health summary
- [x] M3.4 Tests for freshness/staleness logic (22 tests)

---

### Milestone 4 — IPC v1 (read-only first)
**Goal:** Unix socket IPC with stable schemas + structured errors.  
**Tasks**
- [x] M4.1 IPC transport choice — HTTP over UDS with axum+tokio (ADR-001)
- [x] M4.2 `/status` endpoint (freshness + subsystem health)
- [x] M4.3 `/sensors` endpoint (read-only)
- [x] M4.4 `/fans` endpoint (read-only RPM + last commanded PWM if tracked)
- [x] M4.5 Error envelope + mapping rules (validation vs unavailable)

---

### Milestone 5 — Fan control endpoints (OpenFanController)
**Goal:** Per-channel and all-channel PWM set paths with validation and safety.
**Tasks**
- [x] M5.1 set PWM per channel (POST /fans/openfan/{channel}/pwm)
- [x] M5.2 set all PWM (POST /fans/openfan/pwm)
- [x] M5.3 optional target RPM (POST /fans/openfan/{channel}/target_rpm)
- [x] M5.4 tests with mock transport (20 unit + 6 integration tests)

---

### Milestone 6 — hwmon PWM control + leasing
**Goal:** Discover + control motherboard headers with take/release lease model.
**Tasks**
- [x] M6.1 discover controllable PWM outputs (stable IDs, label discovery, safety floor classification)
- [x] M6.2 implement lease/token model (take/release/validate, 60s TTL, auto-expiry)
- [x] M6.3 set PWM with safety floors (20% chassis, 30% CPU/pump, pwmN_enable mode control)
- [x] M6.4 tests via mocked fs (12 discovery + 12 lease + 13 control + 7 integration tests)

---

### Milestone 7 — ~~Telemetry export~~ (removed in R52 de-scope)

---

### Milestone 8 — Finalisation: GUI-Ready Daemon Contract (v1)
**Goal:** Stable v1 contract for GUI development.
**Tasks**
- [x] M8.1 Capabilities endpoint (`GET /capabilities` with devices, features, limits)
- [x] M8.2 Identity + label contract audit (stable IDs, labels, source, kind on all objects)
- [x] M8.3 Lease UX surface (`GET /hwmon/lease/status`, `POST /hwmon/lease/renew`, consistent errors)
- [x] M8.4 Measured vs commanded audit (rpm vs last_commanded_pwm, always separate)
- [x] M8.5 Packaging + service story (systemd unit, socket discovery, stale cleanup)
- [x] M8.6 Documentation (`docs/DEVELOPER_HANDOVER.md`, `docs/USER_GUIDE.md`)

---

## 7) How to use Claude (agentic CLI) with this plan
- Always reference `CLAUDE.md` and keep tasks limited to **one milestone task** per request.
- Start each agent request with:
  - milestone + task ID (e.g., M1.2)
  - explicit non-goals
  - acceptance criteria
- Do not ask the agent to implement an entire milestone in one request.
