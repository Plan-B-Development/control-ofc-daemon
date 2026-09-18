//! Runtime-mutable daemon configuration — the "intern" file.
//!
//! Holds the subset of settings that the daemon itself may rewrite at runtime
//! in response to API calls (`POST /config/profile-search-dirs`,
//! `POST /config/startup-delay`). Stored at `{state_dir}/runtime.toml`,
//! never in `/etc/control-ofc/daemon.toml` — that file stays admin-owned.
//!
//! This split mirrors the NetworkManager pattern of `/etc/NetworkManager/
//! NetworkManager.conf` (admin) + `/var/lib/NetworkManager/NetworkManager-
//! intern.conf` (daemon-owned, read last, shadows admin). See ADR-002.
//!
//! Precedence at startup:
//!   1. `DaemonConfig` is loaded from `/etc/control-ofc/daemon.toml`
//!   2. `RuntimeConfig` is loaded from `{state_dir}/runtime.toml`
//!   3. Any key present in both is resolved to the runtime value
//!
//! Writes go through [`crate::atomic_io::write_atomic`], which does
//! tmp + fsync + rename + parent-dir fsync at 0o600 permissions — so a
//! process crash, kernel panic, or power loss mid-write leaves either the
//! previous complete file or the new complete file, never a zero-length file.

use crate::atomic_io::{create_dir_private, write_atomic};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Filename used inside the state directory.
pub const RUNTIME_CONFIG_FILE: &str = "runtime.toml";

/// Runtime-mutable subset of daemon configuration.
///
/// All fields are `Option<...>` so "not present in runtime.toml" is distinct
/// from "explicitly set to the default". Only fields that are `Some` shadow
/// the admin config.
/// **No `deny_unknown_fields` at this level — deliberate (DEC-243).**
///
/// `load_from` treats any parse error as "malformed → use defaults". With
/// `deny_unknown_fields` here, an *older* daemon started against a
/// `runtime.toml` written by a newer one would fail to parse a section it does
/// not know, fall back to `default()`, and thereby silently discard **every**
/// runtime setting — profile search dirs and startup delay included — which the
/// next successful write would then make permanent. Ignoring unknown *sections*
/// keeps the settings that older daemon still understands. It is not fully
/// lossless: there is no `#[serde(flatten)]` catch-all, so the unknown section
/// itself is still dropped on that daemon's next `save_to`. The point is that a
/// downgrade costs you only the newer keys, not all of them. Each section below
/// keeps `deny_unknown_fields`, so a typo *within* a known section still fails
/// loudly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<RuntimeProfiles>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<RuntimeStartup>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<RuntimeHardware>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<RuntimeSerial>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polling: Option<RuntimePolling>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<RuntimeDetection>,

    /// The exit floor (DEC-388). A new top-level section, so an older daemon
    /// ignores it rather than failing to parse the file (see the doc above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown: Option<RuntimeShutdown>,

    /// Cooling-device topology (AIO-MB Phase 4, DEC-316).
    ///
    /// **Top-level, deliberately** — not a key under `[hardware]` beside
    /// `header_roles`, even though that is the obvious precedent. `RuntimeHardware`
    /// carries `deny_unknown_fields`, so an older daemon meeting this key there
    /// would fail to parse the whole section, and `load_from` does not even
    /// quarantine at boot — it warns and falls back to `Default`. A downgrade
    /// would therefore drop the user's pump role assignment, and with it a 30%
    /// floor, leaving no artifact behind. Up here, this struct's own lack of
    /// `deny_unknown_fields` (see the doc comment above) means an older daemon
    /// ignores the array and `[hardware]` still parses: a downgrade costs the
    /// topology, which is metadata, and never a safety input.
    ///
    /// Declared last so the array-of-tables serialises after the plain tables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cooling_devices: Vec<crate::hwmon::cooling_device::CoolingDeviceConfig>,
}

/// Runtime overrides for `[serial]` (DEC-243). Both take effect at next start.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSerial {
    /// Serial port path; `None` here means "not overridden" (admin value wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl RuntimeSerial {
    fn is_empty(&self) -> bool {
        self.port.is_none() && self.timeout_ms.is_none()
    }
}

/// Runtime override for `[polling]` (DEC-243). Takes effect at next start.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePolling {
    pub poll_interval_ms: u64,
}

/// Runtime overrides for `[detection]` (DEC-243).
///
/// Setting either of these is only *half* the requirement — each also needs a
/// root-installed systemd drop-in granting the capability (`CAP_SYS_RAWIO` for
/// the port probe, `/dev/nvidia* rw` for NVML). The flag alone does not make the
/// feature work, and callers must not present it as if it does.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDetection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_port_probe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_nvidia_telemetry: Option<bool>,
}

impl RuntimeDetection {
    fn is_empty(&self) -> bool {
        self.allow_port_probe.is_none() && self.enable_nvidia_telemetry.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeProfiles {
    pub search_dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStartup {
    pub delay_secs: u64,
}

/// Runtime override for `[shutdown]` (DEC-388). Unlike the DEC-243 keys this
/// one applies live — `POST /config/exit-floor` updates the running daemon as
/// well as this file — because it is read only at the moment of a stop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeShutdown {
    pub exit_floor_pct: u8,
}

/// User-approved hardware selections (Phase 5). Persisted by stable sensor id
/// (never a volatile `hwmonN` path). Advisory only — the daemon's thermal safety
/// still uses the hottest CpuTemp; these drive the inventory's `default_cpu`
/// recommendation + the readiness "selected sensor missing" items.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeHardware {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_cpu_sensor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_mb_sensor: Option<String>,
    /// User-assigned PWM header roles (DEC-311), stable header id → role token
    /// (`"pump"`, `"cpu_fan"`, `"radiator_fan"`, `"chassis_fan"`, `"unknown"`).
    ///
    /// A `BTreeMap` rather than a `HashMap` so `runtime.toml` serialises in a
    /// stable order — this file is operator-editable and lands in diffs and
    /// support bundles, and a map that reshuffles on every unrelated write is
    /// noise no one can read past.
    ///
    /// Stored as a `String`, not a `HeaderRole`, deliberately: a hand-edited or
    /// future-version token must not make the whole `[hardware]` section
    /// unparseable (`deny_unknown_fields` is already strict about *keys*). An
    /// unrecognised value is dropped with a warning at load — see
    /// `header_roles_parsed`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub header_roles: std::collections::BTreeMap<String, String>,
}

impl RuntimeHardware {
    fn is_empty(&self) -> bool {
        self.preferred_cpu_sensor.is_none()
            && self.preferred_mb_sensor.is_none()
            && self.header_roles.is_empty()
    }
}

/// `runtime.toml` → `runtime.toml.invalid-<unix-ts>`.
///
/// Built by appending to the whole filename rather than `with_extension`, which
/// would replace `.toml` and yield `runtime.invalid-…` — losing the hint about
/// what the file was.
fn quarantine_path(path: &Path) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".invalid-{stamp}"));
    path.with_file_name(name)
}

/// When a runtime-config load was attempted. Reported on `/status` so a client
/// can tell which settings the degradation actually cost (`AUD3-m`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPhase {
    /// The boot-time load in `main`. Its overlay seeds **every** runtime-mutable
    /// key, including `header_roles` and `cooling_devices`.
    Startup,
    /// A `SIGHUP` reload. Narrower: it re-applies the overlay but only commits
    /// `profile_search_dirs`, so header roles keep whatever boot established.
    Reload,
    /// A `POST /config/*` setter found the file unreadable, kept the original
    /// as `runtime.toml.invalid-<unix-ts>` and replaced it
    /// ([`RuntimeConfig::load_for_update`], `TS-r`). The replacement carries the
    /// header roles and cooling devices the daemon is running with, so no role is
    /// lost; every other key that existed only in the original is not, and is
    /// gone from the next boot.
    Update,
}

impl LoadPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            LoadPhase::Startup => "startup",
            LoadPhase::Reload => "reload",
            LoadPhase::Update => "update",
        }
    }

    /// How much a degradation in this phase costs, for [`record_degraded`]'s
    /// most-severe-wins rule. `startup` drops every header role; `update`
    /// replaced the file, so settings that were only in it are gone from the
    /// next boot; `reload` drops nothing.
    fn severity(phase: &str) -> u8 {
        match phase {
            "startup" => 3,
            "update" => 2,
            _ => 1,
        }
    }
}

/// Put `problem` into the sticky `runtime_config_degraded` slot, keeping the more
/// severe record (`WIRE-ao`, `TS-r`).
///
/// [SAFETY] **Most-severe wins, not latest-wins.** A `startup` record survives
/// every later failure, because it is the one that says header roles — and with
/// them a hand-assigned pump's 30% floor — are not in force; letting a cheaper
/// record overwrite it would make `phase` under-report. An `update` record
/// survives a later failed `reload` for the same reason one step down: it says
/// the file was replaced. Latest-wins is kept *within* a phase, so a repeat
/// failure still refreshes `detail`.
///
/// The one place this rule lives. `apply_config_reload` and the `/config/*`
/// setters both write the slot through here.
pub fn record_degraded(
    slot: &parking_lot::RwLock<Option<RuntimeConfigDegraded>>,
    problem: RuntimeConfigDegraded,
) {
    let mut slot = slot.write();
    let existing_is_worse = slot.as_ref().is_some_and(|existing| {
        LoadPhase::severity(&existing.phase) > LoadPhase::severity(&problem.phase)
    });
    if !existing_is_worse {
        *slot = Some(problem);
    }
}

/// The assignments a running daemon holds live, handed to
/// [`RuntimeConfig::load_for_update`] so a quarantine cannot lose them (`TS-r`).
///
/// A required argument rather than something the caller patches in afterwards:
/// the setter that forgot to would drop every user-assigned pump role from the
/// engine's floor on the next tick, silently.
pub struct LiveAssignments<'a> {
    pub header_roles: &'a HashMap<String, crate::hwmon::roles::HeaderRole>,
    pub cooling_devices: &'a [crate::hwmon::cooling_device::CoolingDeviceConfig],
}

/// A runtime-config load that fell back to defaults, surfaced on `/status`
/// (`AUD3-m`).
///
/// [SAFETY] The reason this is on the wire at all: [`RuntimeConfig::load_from`]
/// degrades **silently** to `Self::default()`, and the default carries **no
/// `header_roles`**. On the boards the AIO-MB programme exists for — an it8696
/// publishing no `pwmN_label` files — a user's `pump` assignment is the only
/// evidence a header drives a pump, so a failed load removes its 30% floor, its
/// stop exemption and its pump-safe identify. Before this field, one `warn!` in
/// the journal was the entire notification: no client could tell.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuntimeConfigDegraded {
    /// `"unreadable"` (I/O error, or over the 4 MiB read cap) or `"malformed"`
    /// (the bytes were read but are not valid TOML for this daemon version).
    pub reason: String,
    /// The file that could not be loaded.
    pub path: String,
    /// The underlying I/O or TOML error, verbatim.
    pub detail: String,
    /// `"startup"`, `"reload"` or `"update"` — see [`LoadPhase`], which documents
    /// what each one costs. A `startup` degradation is the one that drops header
    /// roles.
    pub phase: String,
}

impl RuntimeConfig {
    /// Load runtime.toml from a specific file path.
    ///
    /// Returns `RuntimeConfig::default()` if the file does not exist. A
    /// malformed file logs a warning and also returns defaults — runtime
    /// config is regenerated on the next successful write, so a one-off
    /// corruption should not prevent the daemon from starting.
    ///
    /// **This discards the fact that it degraded.** That is correct for the
    /// callers that re-read the file only to *compare* it (`GET /config`'s
    /// `restart_pending`, the inventory readers), and wrong for the two that
    /// establish what the daemon is *running on* — they call
    /// [`Self::load_from_reporting`] instead (`AUD3-m`).
    pub fn load_from(path: &Path) -> Self {
        Self::load_from_reporting(path, LoadPhase::Startup).0
    }

    /// [`Self::load_from`], but also returning *why* it fell back to defaults.
    ///
    /// Identical behaviour and identical logging — the daemon still boots on a
    /// corrupt file, which is deliberate (`load_from`'s doc explains why). The
    /// only difference is that the degradation is now reportable, so `/status`
    /// can say the daemon is running on defaults instead of leaving it in the
    /// journal where no client can see it.
    ///
    /// A **missing** file is not a degradation: that is the first-boot case and
    /// defaults are the correct answer, not a fallback.
    pub fn load_from_reporting(
        path: &Path,
        phase: LoadPhase,
    ) -> (Self, Option<RuntimeConfigDegraded>) {
        let (reason, detail) = match crate::atomic_io::read_to_string_capped(path) {
            Ok(content) => match toml::from_str::<RuntimeConfig>(&content) {
                Ok(cfg) => {
                    log::info!("Loaded runtime config from {}", path.display());
                    return (cfg, None);
                }
                Err(e) => {
                    log::warn!(
                        "Malformed runtime config at {}: {e} — ignoring, will regenerate",
                        path.display()
                    );
                    ("malformed", e.to_string())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => {
                log::warn!(
                    "Failed to read runtime config at {}: {e} — using defaults",
                    path.display()
                );
                ("unreadable", e.to_string())
            }
        };

        (
            Self::default(),
            Some(RuntimeConfigDegraded {
                reason: reason.to_string(),
                path: path.display().to_string(),
                detail,
                phase: phase.as_str().to_string(),
            }),
        )
    }

    /// Load runtime.toml for a **read-modify-write** setter, quarantining a
    /// file we cannot understand rather than overwriting or refusing (DEC-255).
    ///
    /// [SAFETY] `load_from` deliberately falls back to defaults so a corrupt
    /// file can never stop the daemon booting. That fallback is wrong for a
    /// setter: every `POST /config/*` is load → mutate one key → `save_to`, so
    /// loading defaults and then writing does not merely *ignore* the unreadable
    /// file — it **overwrites every other setting in it with a default**, and the
    /// loss is permanent. This repo has shipped one settings-destruction bug
    /// already (DEC-244); a read that failed must not become a write that erases.
    ///
    /// Refusing outright was the first attempt and was worse than it looked. The
    /// realistic trigger is not corruption but a **daemon downgrade**: each
    /// section carries `deny_unknown_fields`, so once a newer daemon adds a key
    /// to an existing section, an older one cannot parse the file — and refusing
    /// leaves every setter returning 503 forever, with the boot path already
    /// silently running on defaults. Settings that are simultaneously not applied
    /// and not settable, with no documented way out.
    ///
    /// Quarantine keeps the property that matters — the user's bytes are never
    /// destroyed, just kept as `runtime.toml.invalid-<unix-ts>` — while letting
    /// the daemon carry on. `Err` is reserved for the two cases that cannot be
    /// done safely — the original could not be kept, or its replacement could
    /// not be written — and in both `path` is left exactly as it was.
    ///
    /// A *missing* file is not an error: that is the first-write case.
    ///
    /// [SAFETY] **A quarantine — or a missing file — starts from `live`, not from
    /// bare defaults (`TS-r`).** `POST /config/header-role` and the cooling-device setters
    /// commit memory by *rebuilding* it from the config returned here, so
    /// returning `Self::default()` plus the one key being set replaced the
    /// engine's whole role map: one role assignment while the file was
    /// unreadable dropped every other assigned pump role — and on a header with
    /// no pump label, its 30% floor and its stop exemption — on the next tick,
    /// with nothing on `/status`. The running daemon's own maps are the last good
    /// copy of those assignments, so they are carried in. Every other key starts
    /// at its default, because the in-memory value may have come from
    /// `daemon.toml` rather than from this file and cannot be told apart.
    ///
    /// A missing file takes the same path, because it is the same hazard one step
    /// on: a quarantine whose write then failed leaves no file at all, and so does
    /// a hand-deleted one — and the next setter would rebuild the role map from
    /// defaults exactly as above. On a genuine first write nothing is live yet
    /// (both maps come only from this file), so carrying `live` there changes
    /// nothing. A missing file is still not a degradation and publishes nothing.
    ///
    /// **The replacement is written HERE, before the caller runs.** A setter can
    /// return without writing after this call — a rejected request (the
    /// cooling-device cap, an unknown id, a bad search dir) or a failed write —
    /// and when the quarantine *renamed* the file away, such a return left no
    /// `runtime.toml` at all: the next boot read "missing" as a first boot, with
    /// defaults, no report and every assigned pump role gone. So the original is
    /// hard-linked aside (atomic, and it needs no free space) and the carried
    /// config replaces it atomically through `save_to`. `path` is never empty,
    /// and an `update` record always means a readable file holding the live
    /// assignments is on disk. A filesystem without hard links refuses instead,
    /// as every setter did before DEC-255.
    ///
    /// The second element is the degradation to publish (`phase: "update"`), so
    /// the caller can put it on `/status` through [`record_degraded`]. It is
    /// `None` whenever nothing was set aside.
    ///
    /// Blocking file I/O with fsyncs: call it off the async runtime (DEC-252).
    pub fn load_for_update(
        path: &Path,
        live: LiveAssignments<'_>,
    ) -> Result<(Self, Option<RuntimeConfigDegraded>), String> {
        Self::load_for_update_with(path, live, |cfg, p| cfg.save_to(p))
    }

    /// [`Self::load_for_update`] with the replacement's writer passed in, so a
    /// test can make it fail. Production always passes [`Self::save_to`].
    fn load_for_update_with(
        path: &Path,
        live: LiveAssignments<'_>,
        write: impl FnOnce(&Self, &Path) -> Result<(), String>,
    ) -> Result<(Self, Option<RuntimeConfigDegraded>), String> {
        let (reason, detail) = match crate::atomic_io::read_to_string_capped(path) {
            Ok(content) => match toml::from_str::<RuntimeConfig>(&content) {
                Ok(cfg) => return Ok((cfg, None)),
                Err(e) => ("malformed", e.to_string()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::carrying(live), None));
            }
            Err(e) => ("unreadable", e.to_string()),
        };
        let problem = format!("{reason} ({detail})");

        // Keep the original first, without ever removing `path`. A link to an
        // existing name fails rather than replacing it, so an earlier copy from
        // the same second is never overwritten.
        let quarantined = quarantine_path(path);
        std::fs::hard_link(path, &quarantined).map_err(|e| {
            format!(
                "existing runtime config at {} is {problem} and could not be kept as \
                 {} ({e}); refusing to overwrite it",
                path.display(),
                quarantined.display()
            )
        })?;

        let cfg = Self::carrying(live);
        if let Err(e) = write(&cfg, path) {
            // `save_to` replaces atomically, so `path` still holds the original.
            // Drop the link too, so nothing on disk changed.
            let _ = std::fs::remove_file(&quarantined);
            return Err(format!(
                "existing runtime config at {} is {problem} and its replacement \
                 could not be written ({e}); left it untouched",
                path.display()
            ));
        }
        log::error!(
            "Runtime config at {} is {problem}; the original is kept as {} and was \
             replaced with the header roles and cooling devices the daemon is \
             running with. Every other setting that was only in the original is \
             NOT carried — copy anything you need back and restart.",
            path.display(),
            quarantined.display()
        );

        Ok((
            cfg,
            Some(RuntimeConfigDegraded {
                reason: reason.to_string(),
                path: path.display().to_string(),
                detail,
                phase: LoadPhase::Update.as_str().to_string(),
            }),
        ))
    }

    /// Defaults, plus the assignments the running daemon holds — the starting
    /// point [`Self::load_for_update`] uses when there is no file it can trust.
    fn carrying(live: LiveAssignments<'_>) -> Self {
        let mut cfg = Self::default();
        for (id, role) in live.header_roles {
            cfg.set_header_role(id, Some(*role));
        }
        cfg.cooling_devices = live.cooling_devices.to_vec();
        cfg
    }

    /// Atomically persist runtime.toml. Creates the parent directory if needed.
    /// Sets owner-only (0o600) permissions before rename, matching daemon_state.json.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            create_dir_private(parent)?;
        }

        let content =
            toml::to_string_pretty(self).map_err(|e| format!("serialize runtime config: {e}"))?;

        // Prepend a header so anyone opening the file sees its purpose.
        let body = format!(
            "# Control-OFC runtime config — managed by the daemon.\n\
             # DO NOT edit while the daemon is running; use the API instead.\n\
             # Source of truth for keys that the daemon rewrites at runtime.\n\
             # Admin-owned config lives at /etc/control-ofc/daemon.toml.\n\
             \n\
             {content}"
        );

        write_atomic(path, body.as_bytes())?;

        log::info!("Persisted runtime config to {}", path.display());
        Ok(())
    }

    /// Return the `profiles.search_dirs` value if present.
    pub fn profile_search_dirs(&self) -> Option<&[String]> {
        self.profiles.as_ref().map(|p| p.search_dirs.as_slice())
    }

    /// Return the `startup.delay_secs` value if present.
    pub fn startup_delay_secs(&self) -> Option<u64> {
        self.startup.as_ref().map(|s| s.delay_secs)
    }

    /// Return the `shutdown.exit_floor_pct` value if present (DEC-388).
    pub fn exit_floor_pct(&self) -> Option<u8> {
        self.shutdown.as_ref().map(|s| s.exit_floor_pct)
    }

    /// Set `shutdown.exit_floor_pct`, creating the section if absent.
    pub fn set_exit_floor_pct(&mut self, pct: u8) {
        self.shutdown = Some(RuntimeShutdown {
            exit_floor_pct: pct,
        });
    }

    /// Set `profiles.search_dirs`, creating the section if absent.
    pub fn set_profile_search_dirs(&mut self, dirs: Vec<String>) {
        self.profiles = Some(RuntimeProfiles { search_dirs: dirs });
    }

    /// Set `startup.delay_secs`, creating the section if absent.
    pub fn set_startup_delay_secs(&mut self, delay: u64) {
        self.startup = Some(RuntimeStartup { delay_secs: delay });
    }

    /// Preferred CPU temperature sensor (stable id), if set.
    pub fn preferred_cpu_sensor(&self) -> Option<&str> {
        self.hardware
            .as_ref()
            .and_then(|h| h.preferred_cpu_sensor.as_deref())
    }

    /// Preferred case/motherboard temperature sensor (stable id), if set.
    pub fn preferred_mb_sensor(&self) -> Option<&str> {
        self.hardware
            .as_ref()
            .and_then(|h| h.preferred_mb_sensor.as_deref())
    }

    /// Set (or clear, with `None`) the preferred CPU sensor. Preserves the mb
    /// selection; drops the whole `[hardware]` section when both are cleared.
    pub fn set_preferred_cpu_sensor(&mut self, id: Option<String>) {
        let mut hw = self.hardware.take().unwrap_or_default();
        hw.preferred_cpu_sensor = id;
        self.hardware = if hw.is_empty() { None } else { Some(hw) };
    }

    /// Set (or clear, with `None`) the preferred motherboard sensor. Preserves
    /// the CPU selection; drops the `[hardware]` section when both are cleared.
    pub fn set_preferred_mb_sensor(&mut self, id: Option<String>) {
        let mut hw = self.hardware.take().unwrap_or_default();
        hw.preferred_mb_sensor = id;
        self.hardware = if hw.is_empty() { None } else { Some(hw) };
    }

    /// The persisted header-role assignments, parsed into [`HeaderRole`]s
    /// (DEC-311).
    ///
    /// An unrecognised token is **dropped with a warning**, not defaulted to
    /// `Unknown` and not fatal. Both alternatives are worse: defaulting would
    /// silently downgrade a `"pump"` that a future version spells differently
    /// (losing a floor while reporting success), and failing the load would let
    /// one bad line take out every other runtime setting — including the sensor
    /// selections — on a file operators are invited to edit.
    ///
    /// Dropping is the honest middle: the assignment is gone, the log says so,
    /// and the header falls back to its inferred role, which is the same state
    /// it was in before anyone assigned anything.
    pub fn header_roles_parsed(&self) -> HashMap<String, crate::hwmon::roles::HeaderRole> {
        let Some(hw) = self.hardware.as_ref() else {
            return HashMap::new();
        };
        hw.header_roles
            .iter()
            .filter_map(
                |(id, token)| match crate::hwmon::roles::HeaderRole::from_token(token) {
                    Some(role) => Some((id.clone(), role)),
                    None => {
                        log::warn!(
                            "Ignoring unrecognised header role '{token}' for '{id}' in \
                             runtime configuration — the header keeps its detected role"
                        );
                        None
                    }
                },
            )
            .collect()
    }

    /// Every configured cooling device, with unusable entries dropped.
    ///
    /// Sanitised on **read** rather than on load, matching `header_roles_parsed`:
    /// a hand-edited file keeps its good devices, one bad device costs only
    /// itself, and nothing rewrites the user's file behind their back.
    pub fn cooling_devices(&self) -> Vec<crate::hwmon::cooling_device::CoolingDeviceConfig> {
        crate::hwmon::cooling_device::sanitize(self.cooling_devices.clone())
    }

    /// Create or replace a cooling device, keyed by id. Returns false when the
    /// table is already full and this would be a new device.
    ///
    /// **The cap counts the SANITISED list, not the raw one (`P8-cc`).** Devices
    /// are sanitised on read, so a persisted entry `sanitize` drops is published
    /// by nothing: it is absent from `/inventory/cooling-devices`, its id is
    /// therefore never disclosed, and `DELETE /config/cooling-device/{id}` cannot
    /// be aimed at it. Counting it here spent a slot the client could neither see
    /// nor free — sixteen devices with one bad entry published fifteen and
    /// answered the next create with `409 cooling device limit reached (16)`,
    /// which is unactionable advice. Counting what the client can see makes the
    /// error true and the remedy reachable.
    ///
    /// The raw list stays bounded regardless: the handler validates before
    /// calling this, so every device it admits survives `sanitize`, and the only
    /// excess is whatever invalid entries a hand-edited `runtime.toml` already
    /// held.
    pub fn set_cooling_device(
        &mut self,
        dev: crate::hwmon::cooling_device::CoolingDeviceConfig,
    ) -> bool {
        if let Some(slot) = self.cooling_devices.iter_mut().find(|d| d.id == dev.id) {
            *slot = dev;
            return true;
        }
        if self.cooling_devices().len() >= crate::hwmon::cooling_device::MAX_COOLING_DEVICES {
            return false;
        }
        self.cooling_devices.push(dev);
        true
    }

    /// Remove a cooling device by id. Returns whether anything was removed.
    pub fn remove_cooling_device(&mut self, id: &str) -> bool {
        let before = self.cooling_devices.len();
        self.cooling_devices.retain(|d| d.id != id);
        before != self.cooling_devices.len()
    }

    /// Set (or clear, with `None`) one header's role assignment. Drops the whole
    /// `[hardware]` section when nothing is left in it.
    pub fn set_header_role(
        &mut self,
        header_id: &str,
        role: Option<crate::hwmon::roles::HeaderRole>,
    ) {
        let mut hw = self.hardware.take().unwrap_or_default();
        match role {
            Some(r) => {
                hw.header_roles
                    .insert(header_id.to_string(), r.as_str().into());
            }
            None => {
                hw.header_roles.remove(header_id);
            }
        }
        self.hardware = if hw.is_empty() { None } else { Some(hw) };
    }

    // ── DEC-243 runtime-mutable admin keys ──────────────────────────────
    // Each getter returns `None` when unset, so `apply_runtime_overlay` can tell
    // "not overridden" from "explicitly set to the default value".

    pub fn serial_port(&self) -> Option<&str> {
        self.serial.as_ref().and_then(|s| s.port.as_deref())
    }

    pub fn serial_timeout_ms(&self) -> Option<u64> {
        self.serial.as_ref().and_then(|s| s.timeout_ms)
    }

    pub fn poll_interval_ms(&self) -> Option<u64> {
        self.polling.as_ref().map(|p| p.poll_interval_ms)
    }

    pub fn allow_port_probe(&self) -> Option<bool> {
        self.detection.as_ref().and_then(|d| d.allow_port_probe)
    }

    pub fn enable_nvidia_telemetry(&self) -> Option<bool> {
        self.detection
            .as_ref()
            .and_then(|d| d.enable_nvidia_telemetry)
    }

    /// Set (or clear, with `None`) the serial port override. Preserves the
    /// timeout; drops the `[serial]` section when both are cleared.
    pub fn set_serial_port(&mut self, port: Option<String>) {
        let mut s = self.serial.take().unwrap_or_default();
        s.port = port;
        self.serial = if s.is_empty() { None } else { Some(s) };
    }

    /// Set (or clear) the serial read timeout override.
    pub fn set_serial_timeout_ms(&mut self, timeout: Option<u64>) {
        let mut s = self.serial.take().unwrap_or_default();
        s.timeout_ms = timeout;
        self.serial = if s.is_empty() { None } else { Some(s) };
    }

    /// Set (or clear, with `None`) the poll-interval override.
    pub fn set_poll_interval_ms(&mut self, interval: Option<u64>) {
        self.polling = interval.map(|poll_interval_ms| RuntimePolling { poll_interval_ms });
    }

    /// Set (or clear) the Super-I/O port-probe opt-in.
    pub fn set_allow_port_probe(&mut self, allow: Option<bool>) {
        let mut d = self.detection.take().unwrap_or_default();
        d.allow_port_probe = allow;
        self.detection = if d.is_empty() { None } else { Some(d) };
    }

    /// Set (or clear) the NVML telemetry opt-in.
    pub fn set_enable_nvidia_telemetry(&mut self, enable: Option<bool>) {
        let mut d = self.detection.take().unwrap_or_default();
        d.enable_nvidia_telemetry = enable;
        self.detection = if d.is_empty() { None } else { Some(d) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_runtime_config_is_not_a_degradation() {
        // First boot. Defaults are the CORRECT answer here, not a fallback, so
        // reporting it would put a permanent scary banner on every fresh
        // install — and a warning that is always on is a warning nobody reads
        // when it matters. This is the case that separates `AUD3-m` from noise.
        let dir = tempfile::tempdir().unwrap();
        let (cfg, degraded) = RuntimeConfig::load_from_reporting(
            &dir.path().join("runtime.toml"),
            LoadPhase::Startup,
        );
        // `RuntimeConfig` derives no `PartialEq` and is not worth one for a
        // test, so assert the properties that matter rather than the whole value.
        assert!(cfg.header_roles_parsed().is_empty());
        assert!(cfg.poll_interval_ms().is_none());
        assert!(
            degraded.is_none(),
            "a missing file is first boot, not damage"
        );
    }

    #[test]
    fn a_malformed_runtime_config_reports_that_roles_were_dropped() {
        // [SAFETY] The whole point of `AUD3-m`. The daemon still boots on a
        // corrupt file — deliberate, see `load_from` — but the defaults it boots
        // on carry NO header roles, so a user-assigned pump loses its 30% floor.
        // Assert both halves: the silent-degradation behaviour is unchanged, and
        // the degradation is now reportable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(&path, "[hardware.header_roles]\nh1 = \"pump\"\n[garbage\n").unwrap();

        let (cfg, degraded) = RuntimeConfig::load_from_reporting(&path, LoadPhase::Startup);
        assert!(
            cfg.header_roles_parsed().is_empty(),
            "the fallback carries no roles — this is the loss being reported"
        );
        let d = degraded.expect("a malformed file must be reported");
        assert_eq!(d.reason, "malformed");
        assert_eq!(d.phase, "startup");
        assert!(d.path.ends_with("runtime.toml"));
        assert!(!d.detail.is_empty(), "the TOML error is carried verbatim");
    }

    #[test]
    fn an_oversized_runtime_config_reports_as_unreadable() {
        // The other limb: `read_to_string_capped` refuses anything over the
        // 4 MiB cap, which is how a `runtime.toml` written by a newer daemon (or
        // by a buggy client before the DEC-320 ingest bounds) presents. It is a
        // different `reason` from `malformed` because the remedy differs.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(
            &path,
            vec![b'a'; (crate::atomic_io::MAX_CONFIG_BYTES + 1) as usize],
        )
        .unwrap();

        let (_cfg, degraded) = RuntimeConfig::load_from_reporting(&path, LoadPhase::Reload);
        let d = degraded.expect("an unreadable file must be reported");
        assert_eq!(d.reason, "unreadable");
        assert_eq!(d.phase, "reload", "the phase is the caller's, not inferred");
    }

    #[test]
    fn a_healthy_runtime_config_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        cfg.set_poll_interval_ms(Some(750));
        cfg.save_to(&path).unwrap();

        let (loaded, degraded) = RuntimeConfig::load_from_reporting(&path, LoadPhase::Startup);
        assert_eq!(loaded.poll_interval_ms(), Some(750));
        assert!(degraded.is_none());
    }

    #[test]
    fn default_is_empty() {
        let cfg = RuntimeConfig::default();
        assert!(cfg.profiles.is_none());
        assert!(cfg.startup.is_none());
        assert!(cfg.profile_search_dirs().is_none());
        assert!(cfg.startup_delay_secs().is_none());
    }

    #[test]
    fn load_from_nonexistent_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("absent.toml");
        let cfg = RuntimeConfig::load_from(&path);
        assert!(cfg.profiles.is_none());
    }

    #[test]
    fn load_from_malformed_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.toml");
        std::fs::write(&path, "not = valid = toml === {{{{").unwrap();
        let cfg = RuntimeConfig::load_from(&path);
        assert!(cfg.profiles.is_none());
    }

    #[test]
    fn roundtrip_profile_search_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_profile_search_dirs(vec![
            "/etc/control-ofc/profiles".into(),
            "/home/alice/.config/control-ofc/profiles".into(),
        ]);
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(
            loaded.profile_search_dirs().unwrap(),
            &[
                "/etc/control-ofc/profiles".to_string(),
                "/home/alice/.config/control-ofc/profiles".to_string(),
            ]
        );
        assert!(loaded.startup_delay_secs().is_none());
    }

    #[test]
    fn roundtrip_exit_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        assert!(cfg.exit_floor_pct().is_none(), "absent until set");
        cfg.set_exit_floor_pct(65);
        cfg.save_to(&path).unwrap();
        assert_eq!(RuntimeConfig::load_from(&path).exit_floor_pct(), Some(65));
    }

    #[test]
    fn roundtrip_startup_delay() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_startup_delay_secs(7);
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.startup_delay_secs(), Some(7));
        assert!(loaded.profile_search_dirs().is_none());
    }

    #[test]
    fn both_fields_coexist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_profile_search_dirs(vec!["/p".into()]);
        cfg.set_startup_delay_secs(5);
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.profile_search_dirs().unwrap(), &["/p".to_string()]);
        assert_eq!(loaded.startup_delay_secs(), Some(5));
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("runtime.toml");
        assert!(!path.parent().unwrap().exists());

        let mut cfg = RuntimeConfig::default();
        cfg.set_startup_delay_secs(1);
        cfg.save_to(&path).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn save_rejects_unknown_fields_on_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            "[profiles]\nsearch_dirs = [\"/p\"]\nextra_field = 1\n",
        )
        .unwrap();
        // Falls through to default (not a panic): the *section-level*
        // deny_unknown_fields on RuntimeProfiles makes this a parse failure →
        // warn + default. Note the unknown key is inside a known section — the
        // top-level struct deliberately no longer denies unknown *sections*, so
        // a downgrade stays lossless (see unknown_section_is_ignored_not_fatal).
        let loaded = RuntimeConfig::load_from(&path);
        assert!(loaded.profiles.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_startup_delay_secs(3);
        cfg.save_to(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "runtime config must be owner-only");
    }

    #[test]
    fn save_to_readonly_path_returns_err() {
        // Use a path whose parent is a regular file — every plausible failure
        // mode (mkdir, tmp-file create) must surface as an Err rather than
        // silently succeeding or panicking.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_startup_delay_secs(1);
        let err = cfg.save_to(&path).unwrap_err();
        assert!(
            err.contains("create dir") // create_dir_private mkdir failure (DEC-173)
                || err.contains("write tmp")
                || err.contains("create tmp file"),
            "expected mkdir/write error, got: {err}"
        );
    }

    #[test]
    fn load_preserves_fields_written_by_previous_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_profile_search_dirs(vec!["/one".into(), "/two".into()]);
        cfg.save_to(&path).unwrap();

        let mut loaded = RuntimeConfig::load_from(&path);
        loaded.set_startup_delay_secs(2);
        loaded.save_to(&path).unwrap();

        let reloaded = RuntimeConfig::load_from(&path);
        assert_eq!(
            reloaded.profile_search_dirs().unwrap(),
            &["/one".to_string(), "/two".to_string()]
        );
        assert_eq!(reloaded.startup_delay_secs(), Some(2));
    }

    #[test]
    fn roundtrip_preferred_sensors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        cfg.set_preferred_cpu_sensor(Some("hwmon:k10temp:x:Tctl".into()));
        cfg.set_preferred_mb_sensor(Some("hwmon:nct6798:x:SYSTIN".into()));
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.preferred_cpu_sensor(), Some("hwmon:k10temp:x:Tctl"));
        assert_eq!(loaded.preferred_mb_sensor(), Some("hwmon:nct6798:x:SYSTIN"));
    }

    #[test]
    fn clearing_one_preferred_keeps_the_other() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_preferred_cpu_sensor(Some("cpu".into()));
        cfg.set_preferred_mb_sensor(Some("mb".into()));
        cfg.set_preferred_cpu_sensor(None);
        assert_eq!(cfg.preferred_cpu_sensor(), None);
        assert_eq!(cfg.preferred_mb_sensor(), Some("mb"));
    }

    #[test]
    fn clearing_both_preferred_drops_hardware_section() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_preferred_cpu_sensor(Some("cpu".into()));
        cfg.set_preferred_cpu_sensor(None);
        assert!(cfg.hardware.is_none());
    }

    /// [SAFETY] DEC-311. A `pump` assignment is a safety floor, so it has to
    /// survive a daemon restart — losing it silently would drop the header back
    /// to a 20% chassis floor with nothing to say so.
    #[test]
    fn header_roles_survive_a_save_load_round_trip() {
        use crate::hwmon::roles::HeaderRole;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        cfg.set_header_role("hwmon:it8696:it87.2624:pwm5:pwm5", Some(HeaderRole::Pump));
        cfg.set_header_role("hwmon:it8696:it87.2624:pwm1:pwm1", Some(HeaderRole::CpuFan));
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        let roles = loaded.header_roles_parsed();
        assert_eq!(
            roles.get("hwmon:it8696:it87.2624:pwm5:pwm5"),
            Some(&HeaderRole::Pump)
        );
        assert_eq!(
            roles.get("hwmon:it8696:it87.2624:pwm1:pwm1"),
            Some(&HeaderRole::CpuFan)
        );
    }

    #[test]
    fn clearing_the_last_header_role_drops_the_hardware_section() {
        use crate::hwmon::roles::HeaderRole;
        let mut cfg = RuntimeConfig::default();
        cfg.set_header_role("hwmon:x:pwm1:PUMP", Some(HeaderRole::Pump));
        assert!(cfg.hardware.is_some());
        cfg.set_header_role("hwmon:x:pwm1:PUMP", None);
        assert!(
            cfg.hardware.is_none(),
            "an empty [hardware] section must not be left behind"
        );
    }

    #[test]
    fn header_roles_coexist_with_the_preferred_sensors() {
        use crate::hwmon::roles::HeaderRole;
        let mut cfg = RuntimeConfig::default();
        cfg.set_preferred_cpu_sensor(Some("cpu".into()));
        cfg.set_header_role("hwmon:x:pwm1:PUMP", Some(HeaderRole::Pump));
        cfg.set_header_role("hwmon:x:pwm1:PUMP", None);
        assert_eq!(cfg.preferred_cpu_sensor(), Some("cpu"));
        cfg.set_preferred_cpu_sensor(None);
        assert!(cfg.hardware.is_none());
    }

    /// An unrecognised token — a hand-edit, or a downgrade from a version with
    /// more roles — drops that ONE assignment with a warning. It must not
    /// default to `unknown` (silently losing a pump's floor while reporting
    /// success) and must not fail the load (taking every other runtime setting
    /// with it, on a file operators are invited to edit).
    #[test]
    fn an_unrecognised_role_token_is_dropped_not_defaulted_and_not_fatal() {
        use crate::hwmon::roles::HeaderRole;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            "[hardware]\n\
             preferred_cpu_sensor = \"cpu\"\n\n\
             [hardware.header_roles]\n\
             \"hwmon:x:pwm1:PUMP\" = \"pump\"\n\
             \"hwmon:x:pwm2:A\" = \"impeller\"\n",
        )
        .unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        let roles = loaded.header_roles_parsed();
        assert_eq!(roles.get("hwmon:x:pwm1:PUMP"), Some(&HeaderRole::Pump));
        assert_eq!(
            roles.get("hwmon:x:pwm2:A"),
            None,
            "an unknown token must be dropped, never defaulted to Unknown"
        );
        assert_eq!(
            loaded.preferred_cpu_sensor(),
            Some("cpu"),
            "one bad role must not take the rest of the runtime config with it"
        );
    }

    #[test]
    fn preferred_sensors_coexist_with_other_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        cfg.set_profile_search_dirs(vec!["/p".into()]);
        cfg.set_preferred_cpu_sensor(Some("cpu".into()));
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.profile_search_dirs().unwrap(), &["/p".to_string()]);
        assert_eq!(loaded.preferred_cpu_sensor(), Some("cpu"));
    }

    // ── DEC-243: new runtime-mutable admin keys ──────────────────────────

    #[test]
    fn dec243_keys_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        cfg.set_poll_interval_ms(Some(2000));
        cfg.set_serial_port(Some("/dev/ttyACM1".into()));
        cfg.set_serial_timeout_ms(Some(750));
        cfg.set_allow_port_probe(Some(true));
        cfg.set_enable_nvidia_telemetry(Some(true));
        cfg.save_to(&path).unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(loaded.poll_interval_ms(), Some(2000));
        assert_eq!(loaded.serial_port(), Some("/dev/ttyACM1"));
        assert_eq!(loaded.serial_timeout_ms(), Some(750));
        assert_eq!(loaded.allow_port_probe(), Some(true));
        assert_eq!(loaded.enable_nvidia_telemetry(), Some(true));
    }

    #[test]
    fn unset_dec243_keys_are_none_not_defaults() {
        // The overlay must distinguish "not overridden" from "set to the
        // default", or an untouched key would shadow the admin config.
        let cfg = RuntimeConfig::default();
        assert!(cfg.poll_interval_ms().is_none());
        assert!(cfg.serial_port().is_none());
        assert!(cfg.serial_timeout_ms().is_none());
        assert!(cfg.allow_port_probe().is_none());
        assert!(cfg.enable_nvidia_telemetry().is_none());
    }

    #[test]
    fn clearing_one_serial_key_preserves_the_other() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_serial_port(Some("/dev/ttyACM0".into()));
        cfg.set_serial_timeout_ms(Some(600));
        cfg.set_serial_port(None);
        assert_eq!(cfg.serial_timeout_ms(), Some(600));
        assert!(cfg.serial_port().is_none());
    }

    #[test]
    fn clearing_both_serial_keys_drops_the_section() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_serial_port(Some("/dev/ttyACM0".into()));
        cfg.set_serial_timeout_ms(Some(600));
        cfg.set_serial_port(None);
        cfg.set_serial_timeout_ms(None);
        assert!(cfg.serial.is_none());
    }

    #[test]
    fn clearing_both_detection_keys_drops_the_section() {
        let mut cfg = RuntimeConfig::default();
        cfg.set_allow_port_probe(Some(true));
        cfg.set_enable_nvidia_telemetry(Some(true));
        cfg.set_allow_port_probe(None);
        cfg.set_enable_nvidia_telemetry(None);
        assert!(cfg.detection.is_none());
    }

    #[test]
    fn unknown_section_is_ignored_not_fatal() {
        // THE DOWNGRADE GUARD (DEC-243). `load_from` treats any parse failure as
        // "malformed -> defaults", so with `deny_unknown_fields` at the top level
        // an older daemon reading a newer runtime.toml would discard EVERY
        // setting — profile search dirs and startup delay included — and the next
        // write would make that loss permanent. Unknown sections must be skipped
        // while the known ones survive intact. (The unknown section itself is
        // still dropped on that daemon's next save_to — there is no flatten
        // catch-all — so a downgrade costs the newer keys, not all of them.)
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(
            &path,
            "[profiles]\nsearch_dirs = [\"/p\"]\n\n[startup]\ndelay_secs = 7\n\n\
             [from_the_future]\nsome_key = 1\n",
        )
        .unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert_eq!(
            loaded.profile_search_dirs().unwrap(),
            &["/p".to_string()],
            "a future section must not wipe the known ones"
        );
        assert_eq!(loaded.startup_delay_secs(), Some(7));
    }

    #[test]
    fn unknown_key_inside_a_known_section_still_fails_loudly() {
        // The flip side: dropping deny_unknown_fields at the top level must not
        // also silence typos *within* a section, or a misspelled key would be
        // accepted and silently do nothing.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        std::fs::write(&path, "[startup]\ndelay_sekonds = 7\n").unwrap();

        let loaded = RuntimeConfig::load_from(&path);
        assert!(
            loaded.startup_delay_secs().is_none(),
            "a typo in a known section must not be silently accepted"
        );
    }

    // ── DEC-252: a failed read must never become a destructive write ──────

    #[test]
    fn a_malformed_file_is_quarantined_not_destroyed_and_not_a_dead_end() {
        // DEC-255. Three properties at once, because they are the whole point:
        // the update proceeds (no permanent 503 wedge), the user's bytes survive
        // verbatim, and they survive under a name that says what happened.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        let original = "[polling]\npoll_interval_ms = 900\n[garbage\n";
        std::fs::write(&path, original).unwrap();

        let (loaded, degraded) =
            RuntimeConfig::load_for_update(&path, nothing_live()).expect("must not dead-end");
        assert!(loaded.polling.is_none(), "proceeds on defaults");
        let degraded = degraded.expect("a quarantine is published, never silent (TS-r)");
        assert_eq!(
            (degraded.reason.as_str(), degraded.phase.as_str()),
            ("malformed", "update")
        );
        assert_eq!(degraded.path, path.display().to_string());
        // `TS-r`: the path is REPLACED, never vacated — a readable file is there
        // before the caller does anything else.
        let (replacement, problem) = RuntimeConfig::load_from_reporting(&path, LoadPhase::Startup);
        assert!(problem.is_none(), "the replacement must load cleanly");
        assert!(
            replacement.polling.is_none(),
            "and carry none of the original's keys"
        );

        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("runtime.toml.invalid-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantined copy");
        assert_eq!(
            std::fs::read_to_string(quarantined[0].path()).unwrap(),
            original,
            "the user's bytes must survive verbatim"
        );
    }

    #[test]
    fn a_quarantined_file_does_not_block_the_next_write() {
        // The wedge this replaces: refusing left every setter returning 503
        // forever after a daemon downgrade, while the boot path already ran on
        // defaults. Prove the very next save succeeds.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(&path, "not = valid = toml [[[").unwrap();

        let (mut cfg, _) = RuntimeConfig::load_for_update(&path, nothing_live()).unwrap();
        cfg.set_poll_interval_ms(Some(1000));
        cfg.save_to(&path)
            .expect("the setter must be able to write");

        let (reloaded, degraded) = RuntimeConfig::load_for_update(&path, nothing_live()).unwrap();
        assert_eq!(reloaded.polling.map(|p| p.poll_interval_ms), Some(1000));
        assert!(degraded.is_none(), "the rewritten file is healthy again");
    }

    #[test]
    fn load_for_update_accepts_a_missing_file() {
        // First write: defaults are exactly right, and refusing here would make
        // the very first setter call impossible.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        let (_, degraded) = RuntimeConfig::load_for_update(&path, nothing_live()).unwrap();
        assert!(degraded.is_none(), "first write is not a degradation");
    }

    #[test]
    fn a_valid_file_is_never_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(&path, "[polling]\npoll_interval_ms = 750\n").unwrap();

        let (cfg, degraded) = RuntimeConfig::load_for_update(&path, nothing_live()).unwrap();
        assert_eq!(cfg.polling.map(|p| p.poll_interval_ms), Some(750));
        assert!(degraded.is_none());
        assert!(path.exists(), "a file we understood must be left alone");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// No live assignments — the shape of a daemon that holds no roles.
    fn nothing_live() -> LiveAssignments<'static> {
        static ROLES: std::sync::LazyLock<HashMap<String, crate::hwmon::roles::HeaderRole>> =
            std::sync::LazyLock::new(HashMap::new);
        LiveAssignments {
            header_roles: &ROLES,
            cooling_devices: &[],
        }
    }

    // ── TS-r: a quarantine must not cost the roles the daemon is running with ──

    #[test]
    fn a_quarantine_carries_the_live_roles_and_devices_into_the_new_config() {
        // [SAFETY] `TS-r`. The header-role setter rebuilds the engine's role map
        // from what this returns, so bare defaults here dropped every OTHER
        // assigned pump role on the next tick. Two roles, so the one a setter
        // is about to change is never the only one carried.
        use crate::hwmon::roles::HeaderRole;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(&path, "[hardware]\nheader_roles = { broken").unwrap();

        let roles: HashMap<String, HeaderRole> = [
            ("hwmon:it8696:isa:pwm2:pwm2".to_string(), HeaderRole::Pump),
            (
                "hwmon:it8696:isa:pwm3:pwm3".to_string(),
                HeaderRole::RadiatorFan,
            ),
        ]
        .into();
        let devices = vec![aio_device()];
        let (cfg, degraded) = RuntimeConfig::load_for_update(
            &path,
            LiveAssignments {
                header_roles: &roles,
                cooling_devices: &devices,
            },
        )
        .unwrap();

        assert_eq!(
            cfg.header_roles_parsed(),
            roles,
            "every live role is carried"
        );
        assert_eq!(cfg.cooling_devices, devices, "the live topology is carried");
        assert_eq!(degraded.map(|d| d.phase), Some("update".to_string()));
        // Only those two: a key the daemon may hold from `daemon.toml` must not be
        // written into runtime.toml as though the user had set it here.
        assert!(cfg.polling.is_none() && cfg.profiles.is_none() && cfg.shutdown.is_none());
    }

    #[test]
    fn a_quarantine_leaves_the_live_roles_on_disk_even_if_the_caller_never_writes() {
        // [SAFETY] `TS-r`, the review's P1. A setter can return after this call
        // without writing (a rejected request, a failed write). When the
        // quarantine renamed the file away, that left NO runtime.toml, and the
        // next boot read it as a first boot: defaults, no report, every assigned
        // pump role gone. Assert what that boot would see.
        use crate::hwmon::roles::HeaderRole;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(&path, "[hardware\n").unwrap();

        let roles: HashMap<String, HeaderRole> = [("h1".to_string(), HeaderRole::Pump)].into();
        let _ = RuntimeConfig::load_for_update(
            &path,
            LiveAssignments {
                header_roles: &roles,
                cooling_devices: &[],
            },
        )
        .unwrap();
        // ...and the caller writes nothing.

        let (at_next_boot, problem) = RuntimeConfig::load_from_reporting(&path, LoadPhase::Startup);
        assert!(problem.is_none(), "the next boot finds a readable file");
        assert_eq!(
            at_next_boot.header_roles_parsed(),
            roles,
            "carrying the live roles"
        );
    }

    #[test]
    fn a_replacement_that_cannot_be_written_leaves_the_file_untouched() {
        // The refusal arm: nothing on disk may change. `save_to` replaces
        // atomically, so the original is still at `path`; the kept copy is
        // removed so no `.invalid-` file suggests a loss that did not happen.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        let original = "[polling]\npoll_interval_ms = 900\n[garbage\n";
        std::fs::write(&path, original).unwrap();

        let result = RuntimeConfig::load_for_update_with(&path, nothing_live(), |_, _| {
            Err("disk full".to_string())
        });

        assert!(result.is_err(), "a replacement that failed must be refused");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no kept copy may be left behind"
        );
    }

    #[test]
    fn a_missing_file_carries_the_live_roles_and_publishes_nothing() {
        // [SAFETY] `TS-r`, the same hazard one step on. A quarantine whose write
        // then failed leaves NO file, and the next setter used to rebuild the
        // role map from bare defaults. Nothing is degraded — first-write
        // semantics — but no live role may be dropped.
        use crate::hwmon::roles::HeaderRole;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");

        let roles: HashMap<String, HeaderRole> = [("h1".to_string(), HeaderRole::Pump)].into();
        let (cfg, degraded) = RuntimeConfig::load_for_update(
            &path,
            LiveAssignments {
                header_roles: &roles,
                cooling_devices: &[],
            },
        )
        .unwrap();

        assert_eq!(cfg.header_roles_parsed(), roles);
        assert!(degraded.is_none(), "a missing file is never a degradation");
        assert!(!path.exists(), "loading must not create the file");
    }

    #[test]
    fn a_readable_file_wins_over_the_live_maps() {
        // The carry-over is for a quarantine ONLY. On a healthy file the file is
        // the record, and a live map must never be written over it — otherwise a
        // hand-edit that removed a role and was not yet reloaded would be undone.
        use crate::hwmon::roles::HeaderRole;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(&path, "[hardware.header_roles]\n\"h1\" = \"cpu_fan\"\n").unwrap();

        let roles: HashMap<String, HeaderRole> = [("h2".to_string(), HeaderRole::Pump)].into();
        let (cfg, degraded) = RuntimeConfig::load_for_update(
            &path,
            LiveAssignments {
                header_roles: &roles,
                cooling_devices: &[],
            },
        )
        .unwrap();

        // Presence first: the file really was read, or the absence below is vacuous.
        assert_eq!(
            cfg.header_roles_parsed().get("h1"),
            Some(&HeaderRole::CpuFan)
        );
        assert!(!cfg.header_roles_parsed().contains_key("h2"));
        assert!(degraded.is_none());
    }

    fn degraded(phase: &str, detail: &str) -> RuntimeConfigDegraded {
        RuntimeConfigDegraded {
            reason: "malformed".into(),
            path: "/x/runtime.toml".into(),
            detail: detail.into(),
            phase: phase.into(),
        }
    }

    #[test]
    fn the_more_severe_degradation_record_stands() {
        // [SAFETY] `WIRE-ao` extended by `TS-r`: startup > update > reload, and
        // latest-wins within a phase. Each pair is asserted in BOTH orders, so a
        // rule that happened to keep whichever came first would fail.
        let order = ["reload", "update", "startup"];
        for (i, lower) in order.iter().enumerate() {
            for higher in &order[i + 1..] {
                let slot = parking_lot::RwLock::new(None);
                record_degraded(&slot, degraded(higher, "first"));
                record_degraded(&slot, degraded(lower, "second"));
                assert_eq!(
                    slot.read().as_ref().map(|d| d.phase.clone()),
                    Some(higher.to_string()),
                    "a later `{lower}` must not overwrite `{higher}`"
                );

                let slot = parking_lot::RwLock::new(None);
                record_degraded(&slot, degraded(lower, "first"));
                record_degraded(&slot, degraded(higher, "second"));
                assert_eq!(
                    slot.read().as_ref().map(|d| d.phase.clone()),
                    Some(higher.to_string()),
                    "a later `{higher}` must replace `{lower}`"
                );
            }
            let slot = parking_lot::RwLock::new(None);
            record_degraded(&slot, degraded(lower, "old"));
            record_degraded(&slot, degraded(lower, "new"));
            assert_eq!(
                slot.read().as_ref().map(|d| d.detail.clone()),
                Some("new".to_string()),
                "a repeat `{lower}` failure refreshes `detail`"
            );
        }
    }

    // ── AIO-MB Phase 4 (DEC-316): cooling-device topology ────────────────────

    use crate::hwmon::cooling_device::CoolingDeviceConfig;

    fn aio_device() -> CoolingDeviceConfig {
        CoolingDeviceConfig {
            id: "aio-1".into(),
            name: "AIO Cooling System".into(),
            kind: "aio_liquid".into(),
            pump_member: Some("hwmon:it8696:isa-0a40:pwm5:PUMP".into()),
            radiator_members: vec![
                "hwmon:it8696:isa-0a40:pwm1:CPU_FAN".into(),
                "hwmon:it8696:isa-0a40:pwm2:CPU_OPT".into(),
            ],
            ..Default::default()
        }
    }

    /// The brief's topology requirement, end to end through the file format:
    /// a pump plus several radiator fans survives a save/load round trip.
    #[test]
    fn cooling_devices_round_trip_through_runtime_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");

        let mut cfg = RuntimeConfig::default();
        assert!(cfg.set_cooling_device(aio_device()));
        cfg.set_header_role(
            "hwmon:it8696:isa-0a40:pwm5:PUMP",
            Some(crate::hwmon::roles::HeaderRole::Pump),
        );
        cfg.save_to(&path).unwrap();

        let back = RuntimeConfig::load_from(&path);
        let devices = back.cooling_devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "aio-1");
        assert_eq!(
            devices[0].pump_member.as_deref(),
            Some("hwmon:it8696:isa-0a40:pwm5:PUMP")
        );
        assert_eq!(devices[0].radiator_members.len(), 2);
        assert_eq!(devices[0].coolant_telemetry(), "unavailable");
        // The role assignment is untouched by the new section.
        assert_eq!(back.header_roles_parsed().len(), 1);

        // It really is a TOP-LEVEL array, not a key under [hardware]. If this
        // ever moves, the downgrade test below stops meaning anything.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[[cooling_devices]]"),
            "expected a top-level array of tables, got:\n{text}"
        );
        // `[hardware]` serialises as `[hardware.header_roles]` when the
        // preferred-sensor keys are unset, so match the prefix rather than the
        // bare header — the exact-string trap from `CLAUDE.md`.
        let hardware_at = text.find("[hardware").expect("hardware section");
        let devices_at = text.find("[[cooling_devices]]").unwrap();
        assert!(
            devices_at > hardware_at,
            "the array must serialise after the plain tables:\n{text}"
        );
    }

    /// **The reason this lives at the top level (Decision 4).**
    ///
    /// An older daemon has no `cooling_devices` field. Its `[hardware]` section
    /// carries `deny_unknown_fields`, so had the array been stored there the
    /// whole section would fail to parse — and `load_from` does not quarantine
    /// at boot, it warns and falls back to `Default`, silently dropping the
    /// user's pump role and its 30% floor with no artifact left behind.
    ///
    /// `PriorRuntimeConfig` models that daemon exactly: same shape, minus the
    /// field. The assertion is that it still reads `header_roles`.
    #[test]
    fn an_older_daemon_ignores_cooling_devices_and_keeps_header_roles() {
        #[derive(Debug, Deserialize)]
        struct PriorHardware {
            #[serde(default)]
            header_roles: std::collections::BTreeMap<String, String>,
        }
        #[derive(Debug, Deserialize)]
        struct PriorRuntimeConfig {
            #[serde(default)]
            hardware: Option<PriorHardware>,
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        let mut cfg = RuntimeConfig::default();
        cfg.set_header_role(
            "hwmon:it8696:isa-0a40:pwm5:PUMP",
            Some(crate::hwmon::roles::HeaderRole::Pump),
        );
        assert!(cfg.set_cooling_device(aio_device()));
        cfg.save_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let prior: PriorRuntimeConfig =
            toml::from_str(&text).expect("an older daemon must still parse this file");
        let roles = prior
            .hardware
            .expect("[hardware] must survive")
            .header_roles;
        assert_eq!(
            roles
                .get("hwmon:it8696:isa-0a40:pwm5:PUMP")
                .map(String::as_str),
            Some("pump"),
            "the pump role — and therefore its 30% floor — must survive a downgrade"
        );
    }

    /// The presence half of the test above: a `[hardware]` section that really
    /// does reject unknown keys, proving the hazard is real and the placement
    /// is what avoids it rather than the assertion passing vacuously.
    #[test]
    fn the_hardware_section_would_have_rejected_an_unknown_key() {
        let with_unknown = "[hardware]\nheader_roles = {}\ncooling_devices = []\n";
        let parsed: Result<RuntimeConfig, _> = toml::from_str(with_unknown);
        assert!(
            parsed.is_err(),
            "[hardware] must still reject unknown keys — if this passes,              deny_unknown_fields was removed and the downgrade argument is void"
        );
    }

    #[test]
    fn set_cooling_device_upserts_and_remove_reports_absence() {
        let mut cfg = RuntimeConfig::default();
        assert!(cfg.set_cooling_device(aio_device()));
        let renamed = CoolingDeviceConfig {
            name: "Renamed".into(),
            ..aio_device()
        };
        assert!(cfg.set_cooling_device(renamed));
        assert_eq!(cfg.cooling_devices.len(), 1, "same id must replace");
        assert_eq!(cfg.cooling_devices()[0].name, "Renamed");

        assert!(cfg.remove_cooling_device("aio-1"));
        assert!(
            !cfg.remove_cooling_device("aio-1"),
            "second remove is a no-op"
        );
        assert!(cfg.cooling_devices().is_empty());
    }

    #[test]
    fn set_cooling_device_refuses_to_exceed_the_cap() {
        let mut cfg = RuntimeConfig::default();
        for i in 0..crate::hwmon::cooling_device::MAX_COOLING_DEVICES {
            assert!(cfg.set_cooling_device(CoolingDeviceConfig {
                id: format!("dev-{i}"),
                ..Default::default()
            }));
        }
        assert!(
            !cfg.set_cooling_device(CoolingDeviceConfig {
                id: "one-too-many".into(),
                ..Default::default()
            }),
            "a new device beyond the cap must be refused"
        );
        // ...but replacing an existing one still works at the cap.
        assert!(cfg.set_cooling_device(CoolingDeviceConfig {
            id: "dev-0".into(),
            name: "still editable".into(),
            ..Default::default()
        }));
    }

    /// **The regression test for `P8-cc`.** A persisted device that `sanitize`
    /// drops must not spend a slot the client can neither see nor free.
    ///
    /// The invalid entry is absent from `cooling_devices()`, so it is absent
    /// from `/inventory/cooling-devices`, so its id is never disclosed and
    /// `DELETE /config/cooling-device/{id}` cannot be aimed at it. Counting it
    /// against the cap answered the next create with `409 cooling device limit
    /// reached (16)` while showing fifteen — an error whose remedy did not
    /// exist.
    #[test]
    fn a_device_sanitize_drops_does_not_spend_a_slot() {
        let cap = crate::hwmon::cooling_device::MAX_COOLING_DEVICES;

        // A hand-edited `runtime.toml`: the table full to the cap, one entry of
        // which `validate_device` rejects (a space is outside the id charset).
        let mut cfg = RuntimeConfig {
            cooling_devices: (0..cap)
                .map(|i| CoolingDeviceConfig {
                    id: if i == 0 {
                        "bad id".into()
                    } else {
                        format!("dev-{i}")
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };

        // Preconditions — or a passing assertion below says nothing. The raw
        // table is at the cap while the published one is one short, which is
        // exactly the divergence the defect lived in.
        assert_eq!(cfg.cooling_devices.len(), cap, "the raw table must be full");
        assert_eq!(
            cfg.cooling_devices().len(),
            cap - 1,
            "sanitize must really drop the bad entry, or this tests nothing"
        );

        assert!(
            cfg.set_cooling_device(CoolingDeviceConfig {
                id: "dev-new".into(),
                ..Default::default()
            }),
            "a create must be admitted while a slot is free in the list the \
             client can actually see"
        );
        assert_eq!(
            cfg.cooling_devices().len(),
            cap,
            "and it must be visible once admitted"
        );

        // The opposite branch: the cap still binds once the VISIBLE list is
        // full. Without this, counting nothing at all would pass the assertion
        // above.
        assert!(
            !cfg.set_cooling_device(CoolingDeviceConfig {
                id: "dev-one-too-many".into(),
                ..Default::default()
            }),
            "the cap must still refuse a create once the visible list is full"
        );
        // The invalid entry is still on disk and still reachable by id — the fix
        // stops it costing a slot, it does not delete the user's line.
        assert!(cfg.remove_cooling_device("bad id"));
    }

    /// A hand-edited file with one bad device keeps the good ones and never
    /// aborts the load.
    #[test]
    fn a_bad_hand_edited_device_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.toml");
        std::fs::write(
            &path,
            "[[cooling_devices]]\nid = \"good\"\n\n[[cooling_devices]]\nid = \"has/slash\"\n",
        )
        .unwrap();
        let cfg = RuntimeConfig::load_from(&path);
        let devices = cfg.cooling_devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "good");
    }
}
