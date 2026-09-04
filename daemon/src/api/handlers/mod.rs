//! Request handlers for the IPC API.
//!
//! Read handlers read from the `StateCache` — no direct hardware access.
//! Write handlers dispatch through the `FanController`.

mod assessment;
pub mod config;
mod control;
mod gpu;
mod hw_diagnostics;
mod hwmon_ctl;
mod inventory;
mod openfan;
mod path_confine;
mod profile;
mod status;
pub mod validation;

pub use assessment::*;
pub use config::*;
pub use control::*;
pub use gpu::*;
pub use hw_diagnostics::*;
pub use hwmon_ctl::*;
pub use inventory::*;
pub use openfan::*;
pub use profile::*;
pub use status::*;

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::Json;

use crate::constants;
use crate::health::cache::StateCache;
use crate::health::staleness::StalenessConfig;
use crate::hwmon::pwm_control::HwmonPwmController;
use crate::serial::controller::FanController;

use super::responses::*;
use crate::health::state::DaemonState;

/// Build the sorted list of sensor entries from a cache snapshot.
pub(crate) fn build_sensor_entries(snap: &DaemonState, now: Instant) -> Vec<SensorEntry> {
    let mut entries: Vec<SensorEntry> = snap
        .sensors
        .values()
        .map(|s| {
            let age_ms = now.duration_since(s.updated_at).as_millis() as u64;
            SensorEntry {
                id: s.id.clone(),
                kind: s.kind.to_string(),
                label: s.label.clone(),
                value_c: s.value_c,
                source: s.source.to_string(),
                age_ms,
                rate_c_per_s: s.rate_c_per_s,
                session_min_c: s.session_min_c,
                session_max_c: s.session_max_c,
                chip_name: s.chip_name.clone(),
                temp_type: s.temp_type,
                thresholds: s.thresholds.as_ref().map(SensorThresholdsResponse::from),
                // DEC-193: wireless-radio PHY temps (e.g. ath12k WiFi) must not
                // drive a fan curve — derived from the chip name (the daemon
                // engine never consults this; it is an advisory hint the GUI uses
                // to filter its curve-source picker).
                control_eligible: !crate::hwmon::is_wireless_phy_chip(&s.chip_name),
            }
        })
        .collect();
    // DEC-146 P3-11: deterministic wire order, matching build_fan_entries —
    // and this function's doc comment, which promised "sorted" all along.
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// Build the sorted list of currently-unavailable sensor entries (DEC-193) from
/// a cache snapshot — sensors that exist but fail every read (e.g. an `ath12k`
/// WiFi temp while the radio is down). Surfaced on `/status` + `/poll` for
/// display only; they are absent from `build_sensor_entries` (evicted on
/// quarantine).
pub(crate) fn build_unavailable_entries(
    snap: &DaemonState,
    now: Instant,
) -> Vec<UnavailableSensorEntry> {
    let mut entries: Vec<UnavailableSensorEntry> = snap
        .unavailable_sensors
        .iter()
        .map(|u| UnavailableSensorEntry {
            id: u.id.clone(),
            label: u.label.clone(),
            reason: u.reason.clone(),
            unavailable_for_ms: now.duration_since(u.since).as_millis() as u64,
        })
        .collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// Build the sorted list of controls the engine cannot resolve (273-i) from a
/// cache snapshot. Display-only, and empty in every healthy configuration —
/// a non-empty list means some fan is not being commanded at all.
pub(crate) fn build_skipped_entries(snap: &DaemonState, now: Instant) -> Vec<SkippedControlEntry> {
    let mut entries: Vec<SkippedControlEntry> = snap
        .skipped_controls
        .iter()
        .map(|c| SkippedControlEntry {
            control_id: c.control_id.clone(),
            control_name: c.control_name.clone(),
            reason: c.reason.as_token().to_string(),
            skipped_for_ms: now.duration_since(c.since).as_millis() as u64,
        })
        .collect();
    entries.sort_by(|a, b| a.control_id.cmp(&b.control_id));
    entries
}

/// Build the per-control applied-output list from a cache snapshot (277-k).
///
/// Already sorted by `control_id` upstream (`ProfileEngineState::outputs_snapshot`)
/// — re-sorted here anyway so the wire ordering is a property of this boundary
/// and cannot silently depend on how the engine happened to build the map.
pub(crate) fn build_control_output_entries(snap: &DaemonState) -> Vec<ControlOutputEntry> {
    let mut entries: Vec<ControlOutputEntry> = snap
        .control_outputs
        .iter()
        .map(|c| ControlOutputEntry {
            control_id: c.control_id.clone(),
            output_pct: c.output_pct,
        })
        .collect();
    entries.sort_by(|a, b| a.control_id.cmp(&b.control_id));
    entries
}

/// Build the sorted list of fan entries from a cache snapshot.
pub(crate) fn build_fan_entries(snap: &DaemonState, now: Instant) -> Vec<FanEntry> {
    let mut fans: Vec<FanEntry> = Vec::new();

    // OpenFanController fans
    for (ch, fan) in &snap.openfan_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let stall = if fan.rpm_polled {
            fan.last_commanded_pwm
                .map(|pwm| fan.rpm == 0 && pwm > constants::STALL_PWM_THRESHOLD)
        } else {
            None
        };
        fans.push(FanEntry {
            id: format!("openfan:ch{ch:02}"),
            source: "openfan".into(),
            // OFS-l: a channel the daemon has only ever WRITTEN has `rpm == 0`
            // because that is the struct's initial value, not because anything
            // measured it. `rpm_polled` exists for exactly this distinction and
            // was already consulted for `stall` three lines up; publishing
            // `Some(0)` regardless made an unmeasured channel indistinguishable
            // from a genuinely stalled one. `rpm` is optional on the wire
            // (`skip_serializing_if`), and every other source already emits
            // `None` when it has no reading, so absence is the established shape
            // rather than a new one.
            rpm: fan.rpm_polled.then_some(fan.rpm),
            last_commanded_pwm: fan.last_commanded_pwm,
            duty_pct: None,
            age_ms,
            stall_detected: stall,
            fan_alarm: None,
            pwm_enable_mode: None,
            pwm_readback_pct: None,
            pwm_commanded_pct: None,
        });
    }

    // Hwmon fans
    for (id, fan) in &snap.hwmon_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let stall = match (fan.rpm, fan.last_commanded_pwm) {
            (Some(rpm), Some(pwm)) => Some(rpm == 0 && pwm > constants::STALL_PWM_THRESHOLD),
            _ => None,
        };
        fans.push(FanEntry {
            id: id.clone(),
            source: "hwmon".into(),
            rpm: fan.rpm,
            last_commanded_pwm: fan.last_commanded_pwm,
            duty_pct: None,
            age_ms,
            stall_detected: stall,
            fan_alarm: fan.alarm,
            pwm_enable_mode: fan.pwm_enable_mode,
            pwm_readback_pct: fan.pwm_readback_pct,
            pwm_commanded_pct: fan.pwm_commanded_pct,
        });
    }

    // Discrete GPU fans (AMD + Intel + NVIDIA share the gpu_fans map; the vendor
    // is encoded in the ID prefix — `amd_gpu:` / `intel_gpu:` / `nvidia_gpu:` —
    // DEC-121/DEC-204).
    for (id, fan) in &snap.gpu_fans {
        let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
        let source = if id.starts_with("intel_gpu:") {
            "intel_gpu"
        } else if id.starts_with("nvidia_gpu:") {
            "nvidia_gpu"
        } else {
            "amd_gpu"
        };
        fans.push(FanEntry {
            id: id.clone(),
            source: source.into(),
            rpm: fan.rpm,
            last_commanded_pwm: fan.last_commanded_pct,
            duty_pct: fan.duty_pct,
            age_ms,
            stall_detected: None,
            fan_alarm: None,
            pwm_enable_mode: None,
            pwm_readback_pct: None,
            pwm_commanded_pct: None,
        });
    }

    fans.sort_by(|a, b| a.id.cmp(&b.id));
    fans
}

/// What the last completed OpenFan rescan probe saw (register row 10-e).
///
/// The candidate list is half the value. Rate-limiting on elapsed time alone
/// refused the single most likely legitimate retry — plug a controller in, click
/// rescan — because that is a human action measured in seconds. Recording which
/// ports were probed lets the cooldown apply only while nothing has changed: a
/// newly attached controller enumerates a new tty, so the sets differ and the
/// retry proceeds immediately.
#[derive(Debug, Clone)]
pub struct LastRescan {
    /// When the probe finished (stamped by `RescanGuard::drop`, so the quiet
    /// period runs from the end of the last DTR assertion, not from the request
    /// that started it).
    pub at: Instant,
    /// The candidate ports that probe actually walked.
    pub candidates: Vec<String>,
}

/// Parameters needed to start an OpenFan poll loop for a late-adopted
/// controller (DEC-265). Cloned from config at boot; never discovery-dependent.
#[derive(Clone)]
pub struct OpenFanRuntime {
    /// Serial read/write timeout.
    pub timeout: std::time::Duration,
    /// Poll cadence.
    pub interval: std::time::Duration,
    /// Shutdown signal shared with the loops spawned at boot, so a loop started
    /// by a rescan stops with the rest of them.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

/// Shared application state passed to all handlers.
pub struct AppState {
    pub cache: Arc<StateCache>,
    pub staleness_config: StalenessConfig,
    pub daemon_version: String,
    /// Fan controller for OpenFanController write operations. `None` if not connected.
    /// Arc-wrapped to share between API handlers and the profile engine task.
    ///
    /// DEC-265: behind an `RwLock` so it can be filled in *after* startup. It used
    /// to be a plain `Option` set once during boot, which meant a controller that
    /// enumerated late — or failed its identity probe once — left the daemon with
    /// no OpenFan backend for the whole process lifetime, and no way to recover
    /// short of a restart. That is not only lost fan control: the profile engine's
    /// thermal `force_all_with_floor` is guarded by `if let Some(be) = openfan_be`, so the
    /// thermal emergency lost its reach to every OpenFan-attached fan too.
    /// `POST /fans/openfan/rescan` is what fills it. Read it through
    /// [`AppState::openfan`] rather than locking by hand.
    pub fan_controller: Arc<parking_lot::RwLock<Option<Arc<Mutex<FanController>>>>>,
    /// Everything needed to start the OpenFan poll loop for a controller adopted
    /// after boot (DEC-265). Present regardless of whether a device was found —
    /// these are configuration, not discovery.
    pub openfan_runtime: OpenFanRuntime,
    /// Hwmon PWM controller for motherboard fan header writes. `None` if no headers found.
    /// Arc-wrapped to share between API handlers and the profile engine task.
    pub hwmon_controller: Option<Arc<Mutex<HwmonPwmController>>>,
    /// Daemon process start time for uptime calculation.
    pub start_time: Instant,
    /// Per-entity time-series history ring buffer.
    pub history: Arc<crate::health::history::HistoryRing>,
    /// Active profile for headless curve evaluation.
    pub active_profile: Arc<Mutex<Option<crate::profile::DaemonProfile>>>,
    /// Prevents concurrent calibration sweeps from corrupting each other.
    pub calibrating: AtomicBool,
    /// The current or most recent PWM/RPM characterisation run (AIO-MB Phase 3),
    /// and the flag `DELETE /diagnostics/characterization` sets to ask it to stop.
    ///
    /// The run itself is a **detached** `tokio::spawn`, unlike every other
    /// hardware diagnostic here: the sweep is minutes long and the client polls
    /// `GET /diagnostics/characterization` rather than holding a request open.
    /// It claims the same single verify slot as verify and calibrate, so at most
    /// one of the three can be in flight; concurrency is bounded by that slot,
    /// not by this field.
    ///
    /// [SAFETY] Because the task is detached it is NOT in
    /// `main::shutdown_sequence`'s `task_handles`. What makes that safe is the
    /// shutdown check inside `characterization::RestoreOnDrop` — see its docs.
    pub characterization: crate::api::characterization::RunSlot,
    /// The validation-session engine (AIO-MB Phase 5).
    ///
    /// Holds at most one session and derives its event timeline by diffing the
    /// state cache. It is a **pure observer**: it performs no sysfs I/O, plants
    /// no hooks in the engine or the write path, and contains no code that
    /// commands a duty. Where a session runs a diagnostic it calls the existing
    /// verify/characterize handler, which already owns the lease, the pump floor
    /// and the thermal refusal — so this adds no second PWM ownership path (§2).
    pub validation: Arc<crate::validation::recorder::ValidationEngine>,
    pub characterization_cancel: Arc<AtomicBool>,
    /// Prevents concurrent `POST /fans/openfan/rescan` probes (DEC-265).
    /// Two racing probes would open the same tty, and the loser would install
    /// a controller over the winner's — orphaning a poll loop on a transport
    /// nothing writes through.
    pub openfan_rescanning: AtomicBool,
    /// What the last completed `POST /fans/openfan/rescan` probe saw (register
    /// row 10-e). `openfan_rescanning` above bounds **concurrency**; this bounds
    /// **repetition**, which is a different hazard: every probe asserts DTR on
    /// each candidate tty, and that *resets* Arduino-class boards. A caller
    /// looping on a failing rescan therefore holds unrelated hardware in reset.
    ///
    /// Carries the candidate set, not just a timestamp, because the cooldown is
    /// conditional on the world being unchanged — see `OPENFAN_RESCAN_COOLDOWN`.
    /// A successful adoption never reaches the check at all: the handler returns
    /// early once a controller is connected.
    pub last_openfan_rescan: Arc<Mutex<Option<LastRescan>>>,
    /// Poll-loop handles for OpenFan controllers adopted *after* boot (DEC-265,
    /// register row 277-c).
    ///
    /// `main` builds its `task_handles` list once at startup, long before a
    /// rescan can adopt anything, so a poll loop spawned by the rescan path had
    /// nowhere to be joined and `shutdown_sequence` never drained it. Harmless
    /// as shipped — that loop only reads status and RPM, so there is no PWM
    /// hazard — but "the restore is the guaranteed last writer" was simply not
    /// established for a rescan-adopted controller, and any future write added
    /// there would have fallen silently outside the drain invariant. Drained
    /// alongside `task_handles` so the invariant holds for both.
    pub adopted_poll_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// Detected AMD GPU info (populated at startup). Empty if no AMD GPU found.
    pub amd_gpus: Vec<crate::hwmon::gpu_detect::AmdGpuInfo>,
    /// Detected Intel discrete GPU info (populated at startup). Empty if none
    /// found. Read-only telemetry — no fan write path (DEC-121).
    pub intel_gpus: Vec<crate::hwmon::intel_gpu_detect::IntelGpuInfo>,
    /// Unified NVIDIA discrete GPU identity (nouveau + NVML legs), gathered at
    /// startup. Empty if none found. Read-only telemetry — no fan write path
    /// (DEC-204).
    pub nvidia_gpus: Vec<crate::hwmon::nvidia::NvidiaGpuIdentity>,
    /// Configured profile search directories (from daemon.toml [profiles] section).
    /// Wrapped in RwLock to allow runtime updates via SIGHUP reload or API endpoint.
    pub profile_search_dirs: parking_lot::RwLock<Vec<std::path::PathBuf>>,
    /// Path to the admin-owned daemon.toml (read-only to handlers).
    pub config_path: String,
    /// Path to the daemon-owned runtime.toml (read/write by handlers).
    /// Lives at `{state_dir}/runtime.toml`. See ADR-002.
    pub runtime_config_path: std::path::PathBuf,
    /// Serialises the whole load → mutate → persist → commit sequence of every
    /// `POST`/`DELETE /config/*` setter (`AIO1-d`) — **twelve write routes, served
    /// by eleven acquisition sites** (the two preferred-sensor routes share
    /// `set_preferred_sensor`).
    ///
    /// Each setter reads all of `runtime.toml`, changes one key, and writes the
    /// whole file back. Unserialised, two of them from the same base lose one
    /// edit: the later `save_to` wins the file and the later in-memory commit
    /// wins the cache, and **both requests answer `updated: true`**.
    ///
    /// [SAFETY] The consequence is asymmetric, which is why this is a lock and
    /// not a doc note. Losing a poll-interval edit is annoying; losing a
    /// `header_roles` edit drops a **pump's 30% floor** and its identify
    /// protection at the next daemon restart. Since DEC-316 the GUI's
    /// Configure-AIO flow posts `/config/header-role` and then
    /// `/config/cooling-device` in one user action, so the interleaving window
    /// is opened by ordinary use rather than by two operators racing.
    ///
    /// This is a `tokio::sync::Mutex` rather than a `parking_lot` one precisely
    /// because it is held across the `.await` on `persist_off_runtime`.
    ///
    /// **Lock order.** Acquired FIRST, before any other lock a setter takes, and
    /// held until the setter has persisted and committed. Inside it a setter
    /// takes only leaf locks — `profile_search_dirs`, `header_roles`,
    /// `cooling_devices`, `override_table` — none of which ever reaches back for
    /// this one, so no cycle exists.
    ///
    /// **It must never be held across `hwmon_controller`**, which is the one lock
    /// the profile engine holds across a blocking sysfs write (the DEC-278/289
    /// wedge): holding both would park every `/config/*` write route behind a
    /// wedged header rather than the one request that touched it, and
    /// `AppState::header_role_parts` already documents that pair as ABBA-sensitive.
    /// Setters that validate against live headers do so *before* acquiring this.
    /// `update_header_role_handler` additionally **drops the guard before building
    /// its response**, because `resolved_header_role` reaches `hwmon_controller`
    /// and, on the `role: null` clear path, is the handler's only acquisition of
    /// it — so the pre-lock validation does not cover it. An earlier revision of
    /// this comment claimed the invariant held by construction; it did not, and
    /// the drop is what makes it true.
    pub config_write: tokio::sync::Mutex<()>,
    /// Set when a `runtime.toml` load fell back to defaults, mirrored onto
    /// `/status` + `/poll` so a client can see it (`AUD3-m`).
    ///
    /// [SAFETY] `RuntimeConfig::load_from` degrades silently and the default it
    /// returns carries **no `header_roles`**, so a failed boot load removes
    /// every user-assigned pump role — its 30% floor, its stop exemption and its
    /// pump-safe identify — with one `warn!` in the journal as the only trace.
    /// Nothing on any endpoint reported it.
    ///
    /// **Sticky for the process lifetime, deliberately.** A later successful
    /// `POST /config/*` repairs the *file*, and `header_roles` /
    /// `cooling_devices` are re-committed live by their setters — but nearly
    /// every other runtime-mutable key is consumed once at boot, so the daemon
    /// genuinely is still running on defaults for those. Clearing this on the
    /// next successful write would therefore claim a recovery that did not
    /// happen. Latest-wins: a reload degradation overwrites a startup one.
    pub runtime_config_degraded:
        Arc<parking_lot::RwLock<Option<crate::runtime_config::RuntimeConfigDegraded>>>,
    /// Set by `POST /hwmon/rescan` to ask the sensor polling loop to refresh
    /// its cached descriptor set (labels, types, DEC-117 threshold snapshot)
    /// on its next tick. Swap-checked (and cleared) by the loop (DEC-133).
    pub sensor_rescan_requested: Arc<AtomicBool>,
    /// Daemon-owned manual-override + fan-identify state (DEC-163 / DEC-166).
    /// Mutated by the `/control/*/override` + `/fans/*/identify` handlers and
    /// swept + applied by the profile engine tick (both hold this same `Arc`).
    pub override_table: Arc<Mutex<crate::control_override::OverrideTable>>,
    /// DEC-311: user-assigned PWM header roles, keyed by the header's stable id
    /// (which for hwmon is also its fan id and its profile `member_id` — one key
    /// space, so a lookup works from any of the three).
    ///
    /// Loaded from `runtime.toml` at boot, replaced wholesale by
    /// `POST /config/header-role`, and read every tick by the profile engine
    /// (which holds this same `Arc`). Behind an `RwLock` because the read side
    /// is the 1 Hz engine plus every `/hwmon/headers` request while the write
    /// side is a rare operator action.
    ///
    /// [SAFETY] Stored as `Arc<HashMap>` *inside* the lock, not `HashMap`, so
    /// the engine's per-tick read is a clone of the `Arc` under a momentarily
    /// held read guard rather than a map clone — and, more importantly, so no
    /// guard is ever held across the evaluation. The engine must never hold a
    /// lock an API handler can block on while it is computing PWM.
    pub header_roles:
        Arc<parking_lot::RwLock<Arc<HashMap<String, crate::hwmon::roles::HeaderRole>>>>,
    /// Cooling-device topology (AIO-MB Phase 4, DEC-316).
    ///
    /// Cached here for the same reason as `header_roles`: `/hwmon/headers` is
    /// polled, and re-reading + parsing `runtime.toml` per request would put
    /// blocking file I/O on an async handler for data that changes only on an
    /// operator action.
    ///
    /// Unlike `header_roles`, this is **never read by the profile engine**.
    /// Topology is metadata; no evaluation, no floor and no write consults it.
    /// The only safety-adjacent thing a device carries is a `device_policy_id`,
    /// and that is a selector for a compiled-in policy, never a number.
    pub cooling_devices:
        Arc<parking_lot::RwLock<Arc<Vec<crate::hwmon::cooling_device::CoolingDeviceConfig>>>>,
    /// DEC-203: whether the opt-in active Super-I/O `/dev/port` probe is enabled
    /// (`[detection] allow_port_probe`). Off by default; the probe also needs the
    /// `CAP_SYS_RAWIO` drop-in to actually function.
    pub allow_port_probe: bool,
    /// The fully-resolved config this process is *running* on — `daemon.toml`
    /// with the `runtime.toml` overlay applied, captured at startup (DEC-243).
    ///
    /// `GET /config` compares this against a fresh read of the same two files to
    /// decide `restart_pending` per key. Nearly every runtime-mutable key is
    /// consumed once at process start, so "persisted" and "in effect" are
    /// genuinely different states and the API must not conflate them.
    pub running_config: crate::config::DaemonConfig,
    /// Cached compact readiness rollup (DEC-206) mirrored onto `/status` + `/poll`
    /// for the GUI Dashboard health chip. `None` until the first scan completes
    /// (startup seed). Written by [`AssessmentCache::store`] as the poll mirror of
    /// the full hardware-assessment snapshot (DEC-207) — refreshed only on
    /// discovery-changing events (startup / rescan / preferred-sensor /
    /// `/inventory/*` GET), never recomputed on the poll path. `build_status_response`
    /// only clones this small struct on the 1 Hz poll — it never re-runs the
    /// expensive scan (cache snapshot + sysfs walk + disk read + Super-I/O detect).
    pub readiness_rollup: Arc<Mutex<Option<crate::hwmon::readiness::ReadinessRollup>>>,
    /// Daemon-owned hardware-assessment cache + single-flight coordinator
    /// (DEC-207): ONE coalesced passive scan feeds the readiness rollup above,
    /// the `/inventory/readiness` + `/inventory/superio` compat readers, and the
    /// combined `/inventory/hardware-readiness` endpoint — so the expensive
    /// Super-I/O scan runs once instead of three times. Holds the SAME
    /// `readiness_rollup` `Arc` as its poll mirror. Never on the 1 Hz poll path.
    pub assessment: Arc<AssessmentCache>,
}

impl AppState {
    /// The OpenFan controller, if one is currently adopted (DEC-265).
    ///
    /// Clones out from under the read lock so callers never hold it across an
    /// `.await` — the field became a lock precisely so it could change at
    /// runtime, and a handler that held it open would block a rescan.
    pub fn openfan(&self) -> Option<Arc<Mutex<FanController>>> {
        self.fan_controller.read().clone()
    }

    /// The current header-role assignment map (DEC-311).
    ///
    /// Clones the inner `Arc` out from under the read lock — cheap, and it means
    /// no caller can hold the lock while doing real work.
    pub fn header_roles(&self) -> Arc<HashMap<String, crate::hwmon::roles::HeaderRole>> {
        Arc::clone(&self.header_roles.read())
    }

    /// Snapshot of the configured cooling devices (AIO-MB Phase 4).
    ///
    /// Clones the `Arc` under a momentarily held read guard, so no guard is
    /// held while a caller iterates — the same discipline as `header_roles`.
    /// Takes no other lock, so it composes with the controller lock in either
    /// order.
    pub fn cooling_devices(&self) -> Arc<Vec<crate::hwmon::cooling_device::CoolingDeviceConfig>> {
        Arc::clone(&self.cooling_devices.read())
    }

    /// The role in force for one header id: the user's assignment if any,
    /// otherwise the role discovery inferred.
    ///
    /// [SAFETY] The single lookup every consumer uses — the headers/inventory
    /// responses, pump-safe identify, and the role-aware verify duty. Going
    /// through one function is what stops a new caller reading the descriptor's
    /// inferred `role` directly and silently ignoring the user's assignment.
    ///
    /// This is the **display** role: a user assignment fully replaces the
    /// inference, which is the honest thing to report back. For the *safety*
    /// question — may this header be stopped or under-driven? — use
    /// [`AppState::header_is_pump_protected`], which unions instead.
    pub fn resolved_header_role(&self, header_id: &str) -> crate::hwmon::roles::HeaderRole {
        let (assigned, inferred) = self.header_role_parts(header_id);
        crate::hwmon::roles::resolve_role(assigned, inferred).0
    }

    /// The user's assignment for a header (if any) and the role discovery
    /// inferred for it. Split out so the display and safety questions read the
    /// same two facts and cannot drift apart.
    ///
    /// Takes the `header_roles` read lock and releases it *before* acquiring the
    /// controller lock — deliberately, and load-bearing: `hwmon_headers_handler`
    /// and the inventory handler take those two in the opposite order, so
    /// holding both here would complete an ABBA cycle with the 1 Hz engine in
    /// the middle of it.
    fn header_role_parts(
        &self,
        header_id: &str,
    ) -> (
        Option<crate::hwmon::roles::HeaderRole>,
        (
            crate::hwmon::roles::HeaderRole,
            crate::hwmon::roles::RoleSource,
        ),
    ) {
        let assigned = self.header_roles().get(header_id).copied();
        let inferred = self
            .hwmon_controller
            .as_ref()
            .and_then(|c| {
                c.lock()
                    .headers()
                    .into_iter()
                    .find(|h| h.id == header_id)
                    .map(|h| (h.role, h.role_source))
            })
            .unwrap_or_default();
        (assigned, inferred)
    }

    /// [SAFETY] Whether this header must never be stopped or driven below the
    /// pump floor — the predicate behind pump-safe identify (DEC-311) and the
    /// role-aware verify duty (`AIO1-a`).
    ///
    /// A **union**, exactly like [`crate::profile_engine::tuning`]'s floor: the
    /// header is protected if the role in force is `Pump` **or** if the daemon's
    /// own discovery evidence (a `PUMP`-ish label, or a known liquid-cooler
    /// chip) says pump. A user assignment can therefore ADD protection — which
    /// is the entire point on a board that publishes no labels — but it cannot
    /// REMOVE protection the hardware's own evidence established.
    ///
    /// Without the second term the daemon held two contradictory beliefs about
    /// the same header: `member_effective_floor` unions with
    /// `member_needs_hard_floor` and so kept a label-derived pump at its 30%
    /// floor, while identify consulted the fully-substituted role and would
    /// happily drive that same pump to 0. `POST /config/header-role
    /// {"role": "chassis_fan"}` on an `AIO_PUMP` header was all it took.
    pub fn header_is_pump_protected(&self, header_id: &str) -> bool {
        let (assigned, inferred) = self.header_role_parts(header_id);
        // One definition of the union, in `roles`. This wrapper only adds the
        // lookup — see `roles::is_pump_protected` for why a caller already
        // holding the controller lock must call that directly instead.
        crate::hwmon::roles::is_pump_protected(assigned, inferred)
    }
}

/// RAII guard that clears the profile engine's verify pause on drop (DEC-165),
/// so a dropped or panicked verify handler never leaves the engine paused.
/// Construct via [`begin_verify_pause`].
pub(crate) struct VerifyPauseGuard {
    cache: Arc<crate::health::cache::StateCache>,
    /// The claim this guard owns (DEC-296). `end_verify` ignores a release from
    /// a guard whose claim has since been superseded.
    epoch: u64,
}

impl VerifyPauseGuard {
    /// Prove this verify is still alive and keep the slot (DEC-296). `false`
    /// means the deadman already elapsed and another diagnostic superseded us —
    /// our lease has been force-taken and any write we still attempt will fail.
    pub(crate) fn renew(&self, window: std::time::Duration) -> bool {
        self.cache.renew_verify(self.epoch, window)
    }
}

impl Drop for VerifyPauseGuard {
    fn drop(&mut self) {
        self.cache.end_verify(self.epoch);
    }
}

/// Claim the single verify slot and pause the profile engine's write phase,
/// returning a guard that clears it on drop — or `None` if a verify is already
/// in progress (single-flight; the caller must reject with 409). While paused,
/// the engine skips its write phase so a verify's controlled test writes are not
/// overwritten. `window` is the deadman backstop; the guard is the normal clear
/// path.
pub(crate) fn begin_verify_pause(
    cache: &Arc<crate::health::cache::StateCache>,
    window: std::time::Duration,
) -> Option<VerifyPauseGuard> {
    cache
        .try_begin_verify(window)
        .map(|epoch| VerifyPauseGuard {
            cache: cache.clone(),
            epoch,
        })
}

/// Refuse to START a hardware fan verify when it would fight thermal safety.
///
/// **Corrected in DEC-297 (AUD-l).** This comment used to say a verify "pauses
/// the engine's write phase for its window, which also suppresses the thermal
/// thermal `force_all_with_floor`". **It does not, and never did.** `force_all_with_floor` runs at
/// `profile_engine/mod.rs:970/973` and `continue`s well BEFORE the
/// `verify_active()` gate at `:1120`, so an emergency outranks a verify and is
/// unaffected by the pause — which `profile_engine/mod.rs:1072-1074` has always
/// said. The guard is still worth having; its real justification is that a
/// verify drives a header AWAY from its commanded duty, which must not happen
/// while the system is hot or while the ladder is actively forcing.
///
/// Two conditions, and they are not the same test (DEC-297):
/// 1. **Too hot** — any sensor above the calibrate/verify limit (85 °C), via
///    `check_thermal_safety`, matching the calibrate sweep (DEC-201/DEC-134).
///    Returns `409 thermal_abort`.
/// 2. **The ladder is forcing** — `thermal_force_state` is `Some`. The
///    temperature test above does NOT cover this: the emergency latches at
///    at least 105 °C (per-machine since DEC-308) and releases only at ≤80 °C,
///    so the band 80 < T ≤ 85 passes it while
///    every fan is still being forced. Returns `409 validation_error` with
///    `retryable: true`, the shape DEC-295 established for the same refusal on
///    the calibrate endpoint.
///
/// `None` when it is safe to proceed.
pub(crate) fn verify_thermal_guard(
    cache: &crate::health::cache::StateCache,
) -> Option<(StatusCode, Json<serde_json::Value>)> {
    if let Err(crate::api::calibration::CalibrationError::ThermalAbort {
        sensor_id,
        temp_c,
        limit_c,
    }) = crate::api::calibration::check_thermal_safety(cache)
    {
        return Some(error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope::thermal_abort(format!(
                "Cannot run a fan verify while hot: {sensor_id} at {temp_c:.1}°C \
                 (limit {limit_c:.0}°C). Let the system cool, then retry."
            )),
        ));
    }
    // DEC-297 (295-a): the latched band the temperature test above cannot see.
    if let Some(state) = crate::api::calibration::thermal_force_state(cache) {
        return Some(error_response(
            StatusCode::CONFLICT,
            &ErrorEnvelope {
                error: ErrorBody {
                    code: "validation_error".into(),
                    message: format!(
                        "thermal safety is forcing fan output ({state}); a fan verify \
                         cannot run"
                    ),
                    retryable: true,
                    source: "validation".into(),
                    details: None,
                },
            },
        ));
    }
    None
}

/// Run a blocking, fsync-ing persistence call off the async worker threads
/// (DEC-252).
///
/// `atomic_io::write_atomic` does `write` + `fsync` + `rename` + a directory
/// `fsync`. That is unbounded wall-clock time on whichever tokio worker thread
/// polls the handler — the same runtime the 1 Hz profile engine, and therefore
/// the thermal-safety decision, is scheduled on.
///
/// Severity, stated honestly: the runtime is multi-threaded with one worker per
/// core (`#[tokio::main]` with no arguments), so a single write cannot starve
/// the engine on its own — every other worker keeps polling. This removes the
/// coupling rather than leaving the engine's timing dependent on how many cores
/// the machine happens to have and how many writes arrive at once. It also
/// matches what `gpu.rs` and `hw_diagnostics.rs` already do for their blocking
/// sysfs work.
pub(crate) async fn persist_off_runtime<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        // The closure panicked or the runtime is shutting down. Report it as a
        // persistence failure rather than unwrapping — a panicking write must
        // not take an API worker down with it.
        Err(e) => Err(format!("persistence task failed: {e}")),
    }
}

pub(crate) fn build_status_response(
    state: &AppState,
    thermal_state: String,
    unavailable_sensors: Vec<UnavailableSensorEntry>,
    skipped_controls: Vec<SkippedControlEntry>,
    control_outputs: Vec<ControlOutputEntry>,
    health: crate::health::staleness::HealthSummary,
) -> StatusResponse {
    // `AUD3-m`. Hoisted out of the struct literal deliberately: a temporary in a
    // field initialiser lives until the end of the whole statement, so reading it
    // inline would hold this guard across `state.validation.live_summary()` (which
    // takes the recorder's `live` mutex) and across the `override_table` lock
    // below. No cycle exists today — nothing takes those and then reaches for this
    // — but a guard held across two unrelated acquisitions on the 1 Hz poll path is
    // exactly the latent ordering hazard this file warns about elsewhere. Cloning
    // into a local makes it a genuine leaf read, which is what the field's doc
    // claims it is.
    let degraded = state.runtime_config_degraded.read().clone();

    let subsystems = health
        .subsystems
        .into_iter()
        .map(|s| SubsystemStatus {
            name: s.name,
            status: s.status.to_string(),
            age_ms: s.age_ms,
            reason: s.reason,
        })
        .collect();

    let uptime = state.start_time.elapsed().as_secs();

    // Daemon-owned override + identify state (DEC-163/DEC-166) — poll surface.
    let (override_rows, identify_rows) = state.override_table.lock().status_rows();
    let overrides = override_rows
        .into_iter()
        .map(|r| OverrideStatusEntry {
            control_id: r.control_id,
            pwm_percent: r.pwm_percent,
            expires_in_secs: r.expires_in_secs,
        })
        .collect();
    let fan_identify = identify_rows
        .into_iter()
        .map(|r| IdentifyStatusEntry {
            fan_id: r.fan_id,
            expires_in_secs: r.expires_in_secs,
            // DEC-311: a GUI polling into an identify it did not initiate still
            // needs to describe it truthfully — "stopped" vs "held at 85%".
            mode: r.mode.as_str().into(),
            identify_pwm_percent: r.identify_pwm_percent,
        })
        .collect();

    // Active profile (DEC-194) — mirror id+name onto the poll surface so an
    // external activation shows within one poll. Tight lock: clone out and drop
    // the guard within this statement; the override_table lock above is already
    // released, so lock order (EFF-1) is preserved.
    let (active_profile_id, active_profile_name) = state
        .active_profile
        .lock()
        .as_ref()
        .map(|p| (Some(p.id.clone()), Some(p.name.clone())))
        .unwrap_or((None, None));

    // DEC-206: mirror the cached readiness rollup for the GUI Dashboard chip.
    // Cheap — clones a small `Option<ReadinessRollup>` under a tight lock (no
    // sysfs/disk; the rollup is refreshed off the poll path). Independent lock,
    // taken and released within this statement, so lock order is preserved.
    let readiness = state.readiness_rollup.lock().clone();

    StatusResponse {
        api_version: API_VERSION,
        daemon_version: state.daemon_version.clone(),
        overall_status: health.overall.to_string(),
        subsystems,
        uptime_seconds: Some(uptime),
        // DEC-132: surface the profile engine's thermal override state. The
        // caller extracts it from the cache (defaulting "normal" before the
        // engine's first tick) so this builder no longer needs a `DaemonState`
        // snapshot — only the `override_table` lock, which must stay OUTSIDE any
        // cache read guard to preserve the lock order (EFF-1).
        thermal_state,
        overrides,
        fan_identify,
        unavailable_sensors,
        skipped_controls,
        control_outputs,
        // `live_summary`, NOT `snapshot`: this runs on `/status` AND `/poll`, the
        // GUI's 1 Hz path. `snapshot` clones every sample and event to produce
        // eight scalars — order 10^5 allocations for a full two-hour session,
        // growing as it records — while holding the very lock the recorder tick
        // needs. The summary is fixed-size and lives behind its own mutex.
        runtime_config_degraded: degraded,
        validation_session: state
            .validation
            .live_summary()
            .map(|s| ValidationSessionSummary {
                session_id: s.session_id,
                kind: s.kind,
                state: s.state,
                elapsed_ms: s.elapsed_ms,
                sample_count: s.sample_count,
                event_count: s.event_count,
                sample_limit_reached: s.sample_limit_reached,
                cooling_device_id: s.cooling_device_id,
            }),
        active_profile_id,
        active_profile_name,
        readiness,
    }
}

/// Serialize any `Serialize` value into a JSON response, returning HTTP 500
/// with a proper error envelope if serialization unexpectedly fails.
pub(crate) fn json_ok(
    status: StatusCode,
    val: impl serde::Serialize,
) -> (StatusCode, Json<serde_json::Value>) {
    match serde_json::to_value(val) {
        Ok(v) => (status, Json(v)),
        Err(e) => {
            log::error!("response serialization failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {
                        "code": "internal_error",
                        "message": "response serialization failed",
                        "retryable": true,
                        "source": "internal"
                    }
                })),
            )
        }
    }
}

/// Helper to serialize an ErrorEnvelope into a JSON value response.
pub(crate) fn error_response(
    status: StatusCode,
    envelope: &ErrorEnvelope,
) -> (StatusCode, Json<serde_json::Value>) {
    json_ok(status, envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::state::DaemonState;
    use std::time::Instant;

    #[test]
    fn json_ok_serializes_valid_struct() {
        let val = serde_json::json!({"key": "value"});
        let (status, Json(body)) = json_ok(StatusCode::OK, &val);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["key"], "value");
    }

    #[test]
    fn build_sensor_entries_returns_empty_for_empty_state() {
        let state = DaemonState::default();
        let entries = build_sensor_entries(&state, Instant::now());
        assert!(entries.is_empty());
    }

    #[test]
    fn build_fan_entries_returns_empty_for_empty_state() {
        let state = DaemonState::default();
        let entries = build_fan_entries(&state, Instant::now());
        assert!(entries.is_empty());
    }

    /// OFS-l: a channel the daemon has only ever WRITTEN must not publish a
    /// fabricated `rpm: 0`.
    ///
    /// `OpenFanState.rpm` starts at 0 because that is the struct's initial value,
    /// not because anything measured it. `rpm_polled` records the difference and
    /// was already consulted for `stall_detected` — publishing `Some(0)` anyway
    /// made an unmeasured channel indistinguishable on the wire from a genuinely
    /// stalled one.
    #[test]
    fn a_never_polled_openfan_channel_publishes_no_rpm() {
        let mut state = DaemonState::default();
        state.openfan_fans.insert(
            3,
            crate::health::state::OpenFanState {
                channel: 3,
                rpm: 0,
                last_commanded_pwm: Some(70),
                updated_at: Instant::now(),
                rpm_polled: false,
            },
        );
        state.openfan_fans.insert(
            4,
            crate::health::state::OpenFanState {
                channel: 4,
                rpm: 0,
                last_commanded_pwm: Some(70),
                updated_at: Instant::now(),
                rpm_polled: true,
            },
        );

        let entries = build_fan_entries(&state, Instant::now());
        let ch3 = entries.iter().find(|e| e.id == "openfan:ch03").unwrap();
        let ch4 = entries.iter().find(|e| e.id == "openfan:ch04").unwrap();

        assert_eq!(
            ch3.rpm, None,
            "a channel nothing has polled must report no rpm, not a measured-looking zero"
        );
        assert_eq!(
            ch4.rpm,
            Some(0),
            "a channel that WAS polled and genuinely read 0 must still report it — \
             the fix must not suppress a real stalled-fan reading"
        );
        assert_eq!(
            ch4.stall_detected,
            Some(true),
            "precondition: the polled zero is the stall signal, and it must survive"
        );
        assert_eq!(
            ch3.stall_detected, None,
            "precondition: an unpolled channel already reported no stall verdict"
        );
    }

    #[test]
    fn build_sensor_entries_sorts_by_id() {
        // DEC-146 P3-11: deterministic wire order across restarts/rescans.
        let mut state = DaemonState::default();
        let now = Instant::now();
        for id in ["z_temp", "a_temp", "m_temp"] {
            state.sensors.insert(
                id.into(),
                crate::health::state::CachedSensorReading {
                    id: id.into(),
                    kind: crate::hwmon::types::SensorKind::CpuTemp,
                    label: "t".into(),
                    value_c: 40.0,
                    source: crate::health::state::DeviceLabel::Hwmon,
                    updated_at: now,
                    rate_c_per_s: None,
                    session_min_c: None,
                    session_max_c: None,
                    chip_name: "k10temp".into(),
                    temp_type: None,
                    thresholds: None,
                },
            );
        }
        let ids: Vec<String> = build_sensor_entries(&state, now)
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["a_temp", "m_temp", "z_temp"]);
    }

    #[test]
    fn build_fan_entries_sorts_by_id() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        // Insert fans in reverse order
        state.hwmon_fans.insert(
            "hwmon:z_fan".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:z_fan".into(),
                rpm: Some(1000),
                last_commanded_pwm: None,
                pwm_readback_pct: None,
                pwm_commanded_pct: None,
                updated_at: now,
                alarm: None,
                pwm_enable_mode: None,
            },
        );
        state.hwmon_fans.insert(
            "hwmon:a_fan".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:a_fan".into(),
                rpm: Some(500),
                last_commanded_pwm: None,
                pwm_readback_pct: None,
                pwm_commanded_pct: None,
                updated_at: now,
                alarm: None,
                pwm_enable_mode: None,
            },
        );

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "hwmon:a_fan");
        assert_eq!(entries[1].id, "hwmon:z_fan");
    }

    #[test]
    fn build_fan_entries_routes_gpu_source_by_id_prefix() {
        // The wire `source` the GUI keys on is derived from the gpu_fans id
        // prefix. Each vendor prefix must map to the right source string
        // (DEC-121/DEC-204); a transposed branch would mislabel GPU fans.
        let mut state = DaemonState::default();
        let now = Instant::now();
        let cases = [
            ("amd_gpu:0000:03:00.0", "amd_gpu"),
            ("intel_gpu:0000:04:00.0", "intel_gpu"),
            ("nvidia_gpu:0000:05:00.0", "nvidia_gpu"),
        ];
        for (id, _) in cases {
            state.gpu_fans.insert(
                id.into(),
                crate::health::state::AmdGpuFanState {
                    id: id.into(),
                    rpm: Some(1200),
                    last_commanded_pct: None,
                    duty_pct: Some(33),
                    updated_at: now,
                },
            );
        }

        let entries = build_fan_entries(&state, now);
        for (id, expected_source) in cases {
            let e = entries.iter().find(|e| e.id == id).unwrap();
            assert_eq!(e.source.as_str(), expected_source, "source for {id}");
            // GPU fan telemetry here is read-only — no commanded PWM.
            assert_eq!(e.last_commanded_pwm, None);
            // The measured duty % must route from the cache to the wire (DEC-204).
            assert_eq!(e.duty_pct, Some(33), "duty_pct for {id}");
        }
    }

    /// AIO-MB Phase 6: the command and the readback must reach the wire as two
    /// independent numbers, and an hwmon header is the only source that has both.
    ///
    /// Asserts the disagreeing case on purpose. Equal values would pass with the
    /// two fields wired to the same cache member, which is the bug this split
    /// exists to prevent — and is what `last_commanded_pwm` still does (AIO5-a).
    #[test]
    fn hwmon_fan_entry_publishes_command_and_readback_separately() {
        let mut state = DaemonState::default();
        let now = Instant::now();
        state.hwmon_fans.insert(
            "hwmon:it8696:isa-0a40:pwm5:PUMP".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:it8696:isa-0a40:pwm5:PUMP".into(),
                rpm: Some(1284),
                last_commanded_pwm: Some(30),
                pwm_readback_pct: Some(30),
                pwm_commanded_pct: Some(45),
                updated_at: now,
                alarm: None,
                pwm_enable_mode: Some(1),
            },
        );

        let entries = build_fan_entries(&state, now);
        let e = &entries[0];
        assert_eq!(e.pwm_commanded_pct, Some(45), "the daemon commanded 45%");
        assert_eq!(e.pwm_readback_pct, Some(30), "the hardware reports 30%");
        assert_ne!(
            e.pwm_commanded_pct, e.pwm_readback_pct,
            "the two axes must not be wired to the same source"
        );
    }

    /// Absent, never 0%, and never an echo of the command: an OpenFan channel
    /// and a GPU fan have no hwmon command/readback split to report, and
    /// synthesising one would make a fabricated number indistinguishable from a
    /// measured one.
    #[test]
    fn non_hwmon_fan_entries_omit_the_pwm_command_split() {
        let mut state = DaemonState::default();
        let now = Instant::now();
        state.openfan_fans.insert(
            3,
            crate::health::state::OpenFanState {
                channel: 3,
                rpm: 900,
                last_commanded_pwm: Some(50),
                updated_at: now,
                rpm_polled: true,
            },
        );
        state.gpu_fans.insert(
            "amd_gpu:0000:03:00.0".into(),
            crate::health::state::AmdGpuFanState {
                id: "amd_gpu:0000:03:00.0".into(),
                rpm: Some(1500),
                last_commanded_pct: Some(60),
                duty_pct: None,
                updated_at: now,
            },
        );

        for e in build_fan_entries(&state, now) {
            assert_eq!(
                e.pwm_commanded_pct, None,
                "{} must not synthesise a commanded-PWM readback split",
                e.id
            );
            assert_eq!(e.pwm_readback_pct, None, "{}", e.id);
            // Their own single-producer command field is untouched.
            assert!(e.last_commanded_pwm.is_some(), "{}", e.id);
        }
    }

    #[test]
    fn stall_detection_uses_constant_threshold() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        // Fan at PWM=20 with RPM=0 should NOT be stalled (threshold is >20)
        state.hwmon_fans.insert(
            "hwmon:fan1".into(),
            crate::health::state::HwmonFanState {
                id: "hwmon:fan1".into(),
                rpm: Some(0),
                last_commanded_pwm: Some(constants::STALL_PWM_THRESHOLD),
                pwm_readback_pct: None,
                pwm_commanded_pct: None,
                updated_at: now,
                alarm: None,
                pwm_enable_mode: None,
            },
        );

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries[0].stall_detected, Some(false));

        // Fan at PWM=21 with RPM=0 SHOULD be stalled
        state
            .hwmon_fans
            .get_mut("hwmon:fan1")
            .unwrap()
            .last_commanded_pwm = Some(constants::STALL_PWM_THRESHOLD + 1);

        let entries = build_fan_entries(&state, now);
        assert_eq!(entries[0].stall_detected, Some(true));
    }

    #[test]
    fn build_sensor_entries_includes_chip_name_and_temp_type() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        state.sensors.insert(
            "hwmon:nct6683:nodev:SYSTIN".into(),
            crate::health::state::CachedSensorReading {
                id: "hwmon:nct6683:nodev:SYSTIN".into(),
                kind: crate::hwmon::types::SensorKind::MbTemp,
                label: "SYSTIN".into(),
                value_c: 42.0,
                source: crate::health::state::DeviceLabel::Hwmon,
                updated_at: now,
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
                chip_name: "nct6683".into(),
                temp_type: Some(3),
                thresholds: None,
            },
        );

        let entries = build_sensor_entries(&state, now);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].chip_name, "nct6683");
        assert_eq!(entries[0].temp_type, Some(3));

        // Verify JSON serialization includes the fields
        let json = serde_json::to_value(&entries[0]).unwrap();
        assert_eq!(json["chip_name"], "nct6683");
        assert_eq!(json["temp_type"], 3);
    }

    #[test]
    fn build_sensor_entries_marks_wireless_phy_not_control_eligible() {
        // DEC-193: an ath12k WiFi temp is surfaced for display but flagged
        // control_eligible=false so the GUI won't offer it as a curve source;
        // a real motherboard/CPU sensor stays eligible.
        let mut state = DaemonState::default();
        let now = Instant::now();
        let mk = |id: &str, chip: &str| crate::health::state::CachedSensorReading {
            id: id.into(),
            kind: crate::hwmon::types::SensorKind::MbTemp,
            label: "temp1".into(),
            value_c: 44.0,
            source: crate::health::state::DeviceLabel::Hwmon,
            updated_at: now,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: chip.into(),
            temp_type: None,
            thresholds: None,
        };
        state.sensors.insert(
            "hwmon:ath12k_hwmon:phy0:temp1".into(),
            mk("hwmon:ath12k_hwmon:phy0:temp1", "ath12k_hwmon"),
        );
        state.sensors.insert(
            "hwmon:k10temp:nodev:Tctl".into(),
            mk("hwmon:k10temp:nodev:Tctl", "k10temp"),
        );

        let entries = build_sensor_entries(&state, now);
        let wifi = entries
            .iter()
            .find(|e| e.chip_name == "ath12k_hwmon")
            .unwrap();
        let cpu = entries.iter().find(|e| e.chip_name == "k10temp").unwrap();
        assert!(
            !wifi.control_eligible,
            "wireless PHY must not be a curve source"
        );
        assert!(cpu.control_eligible, "real sensors stay control-eligible");
    }

    #[test]
    fn build_unavailable_entries_sorts_and_computes_age() {
        // DEC-193: unavailable sensors are surfaced sorted by id, with a
        // millisecond age since quarantine.
        let mut state = DaemonState::default();
        let now = Instant::now();
        let since = now - std::time::Duration::from_millis(1500);
        state.unavailable_sensors = vec![
            crate::health::state::UnavailableSensor {
                id: "z_sensor".into(),
                label: "z".into(),
                reason: "Network is down".into(),
                since,
            },
            crate::health::state::UnavailableSensor {
                id: "a_sensor".into(),
                label: "a".into(),
                reason: "Network is down".into(),
                since,
            },
        ];
        let entries = build_unavailable_entries(&state, now);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "a_sensor");
        assert_eq!(entries[1].id, "z_sensor");
        assert!(entries[0].unavailable_for_ms >= 1500);
    }

    #[test]
    fn build_sensor_entries_omits_temp_type_when_none() {
        let mut state = DaemonState::default();
        let now = Instant::now();

        state.sensors.insert(
            "hwmon:k10temp:nodev:Tctl".into(),
            crate::health::state::CachedSensorReading {
                id: "hwmon:k10temp:nodev:Tctl".into(),
                kind: crate::hwmon::types::SensorKind::CpuTemp,
                label: "Tctl".into(),
                value_c: 55.0,
                source: crate::health::state::DeviceLabel::Hwmon,
                updated_at: now,
                rate_c_per_s: None,
                session_min_c: None,
                session_max_c: None,
                chip_name: "k10temp".into(),
                temp_type: None,
                thresholds: None,
            },
        );

        let entries = build_sensor_entries(&state, now);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].chip_name, "k10temp");
        assert_eq!(entries[0].temp_type, None);

        // Verify JSON serialization omits temp_type when None
        let json = serde_json::to_value(&entries[0]).unwrap();
        assert_eq!(json["chip_name"], "k10temp");
        assert!(json.get("temp_type").is_none());
    }
}

#[cfg(test)]
mod persist_tests {
    use super::persist_off_runtime;

    #[tokio::test]
    async fn a_panicking_persistence_task_becomes_an_error_not_a_dead_worker() {
        // DEC-255: `persist_off_runtime`'s doc claims "a panicking write must not
        // take an API worker down with it". That property had no test, and it is
        // cheap and deterministic to pin.
        let result: Result<(), String> =
            persist_off_runtime(|| panic!("simulated write panic")).await;
        assert!(
            result.is_err(),
            "a panic must surface as Err, not unwind the handler"
        );
        assert!(
            result.unwrap_err().contains("persistence task failed"),
            "and must be distinguishable from an ordinary IO failure"
        );
    }

    #[tokio::test]
    async fn a_successful_persistence_task_passes_its_value_through() {
        let result = persist_off_runtime(|| Ok::<u8, String>(7)).await;
        assert_eq!(result, Ok(7));
    }
}
