//! JSON response types for the IPC API.
//!
//! All types derive `Serialize` for JSON output. Field names are stable
//! within API v1 — changes must be additive only.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const API_VERSION: u32 = 1;

/// Response for `/status` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub api_version: u32,
    pub daemon_version: String,
    pub overall_status: String,
    pub subsystems: Vec<SubsystemStatus>,
    /// Seconds since daemon process started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    /// Thermal safety override state: `"normal"` | `"emergency"` |
    /// `"no_sensor_fallback"` (forced 40% when no CPU sensor is reachable and
    /// nothing is latched). `"recovery"` was emitted before DEC-386 removed the
    /// recovery rung; clients still render it for older daemons.
    /// Mirrors the value the profile engine reports each tick (the same string
    /// `/diagnostics/hardware` exposes) so the GUI can surface a thermal-safety
    /// banner while the daemon is forcing safety PWM (DEC-132; the DEC-165
    /// cutover left the GUI with no control loop to stand down — it is
    /// display-only). Additive field — API_VERSION unchanged.
    pub thermal_state: String,
    /// Active manual overrides (DEC-163), each with remaining TTL. Omitted when
    /// none are active so the common-case `/status` wire shape is unchanged
    /// (additive). Poll-authoritative surface for the GUI's override UI.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<OverrideStatusEntry>,
    /// Active fan-identify holds (DEC-166), each with remaining deadman TTL.
    /// Omitted when none are active.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fan_identify: Vec<IdentifyStatusEntry>,
    /// Sensors discovered but currently unreadable (DEC-193) — e.g. an `ath12k`
    /// WiFi temperature while the radio is soft-blocked. Display-only: they are
    /// evicted from the `sensors` array so a stale value is never served. Omitted
    /// when none, so the common-case wire shape is unchanged (additive).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable_sensors: Vec<UnavailableSensorEntry>,
    /// Controls the profile engine cannot resolve, so is not commanding (273-i)
    /// — e.g. a Mix naming a curve id the profile no longer has. Their fans hold
    /// their last commanded duty; a skip never lowers a fan (DEC-269). Before
    /// this field such a control was silent: no log at the shipped level, and
    /// nothing on the API. Omitted when none, so the common-case wire shape is
    /// unchanged (additive) — an older daemon omits it and a client reads `[]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_controls: Vec<SkippedControlEntry>,
    /// Per-control applied output from the engine's last evaluating tick (277-k).
    /// Omitted when empty, so the common-case wire shape is unchanged (additive)
    /// — an older daemon omits it and a client reads `[]`, leaving its cards on
    /// whatever they render for "no value", which is what they already did.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub control_outputs: Vec<ControlOutputEntry>,
    /// Set when this daemon's `runtime.toml` could not be read or parsed and it
    /// fell back to **defaults** (`AUD3-m`, daemon >= 2.34.0).
    ///
    /// [SAFETY] `RuntimeConfig::default()` carries no `header_roles`, so a
    /// `phase: "startup"` degradation means every user-assigned pump role is
    /// gone — no 30% floor, no stop exemption, no pump-safe identify — on
    /// exactly the boards (no `pwmN_label` files) where a user assignment is the
    /// only evidence a header drives a pump. Until this field there was no
    /// endpoint on which that was visible; it was one `warn!` in the journal.
    ///
    /// Omitted when the config loaded cleanly, so the common-case wire shape is
    /// unchanged (additive) and an older daemon's omission reads the same as
    /// "fine" — which is the safe direction, since it is exactly the (absent)
    /// warning such a daemon shows today. A **missing** `runtime.toml` is not a
    /// degradation: that is first boot, and defaults are the right answer.
    ///
    /// Sticky for the daemon's lifetime — see `AppState::runtime_config_degraded`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_config_degraded: Option<crate::runtime_config::RuntimeConfigDegraded>,
    /// The current or most recent validation session, in miniature (AIO-MB
    /// Phase 5). Rides the poll so a client's live panel needs no second
    /// request — the DEC-316 static-vs-dynamic split, applied again: the
    /// session's *topology* is static and fetched once from
    /// `GET /validation/session`, while its progress is state and belongs here.
    ///
    /// Omitted when no session has ever run, so the common-case wire shape is
    /// unchanged and an older daemon's omission reads as "none".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_session: Option<ValidationSessionSummary>,
    /// Active profile id + display name, mirrored onto the `/status` + `/poll`
    /// surface so an external activation (CLI `--profile`, another client,
    /// systemd) is reflected within one 1 Hz poll instead of the GUI's slow
    /// `/profile/active` refresh (DEC-194). Both omitted when no profile is
    /// active, so the common-case wire shape is unchanged (additive) — a client
    /// treats an absent key as "unknown" and falls back to `/profile/active`.
    ///
    /// **Which of the two reasons for that absence applies is answered by
    /// `has_active_profile` below, not by these fields.** Do not read an absent
    /// id as "no profile is active": until daemon 2.45.0 there was no way to
    /// tell that apart from "this daemon predates the mirror", and a client that
    /// guessed left a stale profile named in its UI forever (`CTRL-d`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile_name: Option<String>,
    /// Whether a profile is active (`CTRL-d`, daemon >= 2.45.0) — the
    /// authoritative answer that the two `skip_serializing_if` fields above
    /// cannot give.
    ///
    /// Always serialised, including when false, for the `verify_active`
    /// reason: it is the *presence of the key* that tells a client this daemon
    /// reports the state at all. So there are three readings, not two:
    ///
    /// | wire | meaning |
    /// |---|---|
    /// | key absent | daemon < 2.45.0 — unknown, fall back to `/profile/active` |
    /// | `false` | **authoritatively nothing is active** — clear the cached id/name |
    /// | `true` | the id/name above are present and current |
    ///
    /// Derived from `active_profile_id.is_some()` in the same statement that
    /// builds it, never from a sibling fact about the daemon, so the two can
    /// never disagree (the DEC-325 rule). The redundancy when `true` is the
    /// point — it is what makes `false` mean something.
    ///
    /// The wire shape of `active_profile_id` itself is deliberately NOT changed
    /// to an always-serialised `""`: `control-ofc-tray` reads
    /// `active_profile_id.is_none()` as "no profile active"
    /// (`tray/src/menu.rs`), and an empty string is `is_some()`.
    pub has_active_profile: bool,
    /// Compact hardware-readiness rollup (DEC-206) for the GUI Dashboard health
    /// chip: overall severity + per-severity counts + the most-severe item's
    /// summary/code (for a deep-link). Cached in `AppState` and mirrored here so
    /// the 1 Hz poll stays cheap — the full item list stays on
    /// `/inventory/readiness`. Omitted by daemons predating DEC-206 (and until the
    /// startup refresh runs), so a client treats an absent key as "no rollup" and
    /// hides the chip (additive — API_VERSION unchanged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<crate::hwmon::readiness::ReadinessRollup>,
    /// True while a hardware verify, PWM characterisation, OpenFan calibration
    /// or validation sweep owns the engine's **write pause** (`WIRE-n`, daemon
    /// >= 2.36.0).
    ///
    /// The engine keeps *evaluating* during such a session — it computes every
    /// control's duty and publishes it in `control_outputs[]` — and simply does
    /// not write it. Without this field a client cannot tell the two apart, so
    /// its Controls cards report a duty that nothing is applying, which is the
    /// exact failure the `control_outputs` absence rule already prevents for a
    /// thermal force. Narrow but reachable: the dialogs are modal, yet a
    /// validation session keeps recording after its dialog closes.
    ///
    /// **Not a safety signal, and must not be read as one.** The thermal
    /// emergency's `force_all_with_floor` runs well before the `verify_active()`
    /// gate (DEC-297), so a paused write phase never gates the ladder — a client
    /// that suppressed a thermal banner on this field would be hiding a live
    /// emergency. Mark or blank the *commanded duty*, nothing else.
    ///
    /// Always serialised, including when false, so a client can distinguish
    /// "this daemon says no session is running" from "this daemon does not
    /// report sessions" — the field is not optional-with-a-safe-default the way
    /// the `skip_serializing_if` arrays above are, because its safe default
    /// (`false`, "writes are landing") is also its common value. An older daemon
    /// omits it and a client reads `false`, which is what it renders today.
    pub verify_active: bool,
}

/// One present-but-unreadable sensor on the `/status` + `/poll` surface
/// (DEC-193).
#[derive(Debug, Clone, Serialize)]
pub struct UnavailableSensorEntry {
    pub id: String,
    pub label: String,
    /// Human-readable cause — the daemon's hwmon read error.
    pub reason: String,
    /// Milliseconds since the sensor was quarantined as unreadable.
    pub unavailable_for_ms: u64,
}

/// One control the profile engine cannot resolve, on the `/status` poll surface
/// (273-i). Display-only: its fans are still reporting, they are simply not
/// being commanded, and they hold their last duty rather than falling.
#[derive(Debug, Clone, Serialize)]
pub struct SkippedControlEntry {
    pub control_id: String,
    pub control_name: String,
    /// Stable token, not prose: `curve_not_found` | `sensor_unavailable` |
    /// `mix_unresolvable` | `sync_unresolvable` | `backend_unavailable`. The
    /// last is the only one that is not a curve-resolution failure — the curve
    /// resolved and every member's backend is absent (`OFN-j`). The client
    /// renders the wording,
    /// so it can be styled and localised there; the daemon's own sentence goes
    /// to the journal. Adding a token is additive; renaming one is breaking.
    pub reason: String,
    /// Milliseconds since the control was first listed as skipped.
    pub skipped_for_ms: u64,
}

/// One control's applied output on the `/status` poll surface (277-k).
///
/// The value the engine actually applied this tick, whatever produced it — a
/// curve evaluation or a live manual override — because the question the client
/// renders it to answer is "what are the fans doing?".
///
/// Display-only, and **not** a per-fan duty: a member can sit below this (a
/// role-aware floor, or the DEC-119 GPU divergence). Per-fan duty comes from each
/// fan's own `last_commanded_pwm` on `/fans` + `/poll`.
///
/// A control absent from this list is not being evaluated — no profile active, a
/// thermal force driving the fans directly, or the control is listed in
/// `skipped_controls`. Absence is meaningful; the client must not carry a
/// previous value forward.
#[derive(Debug, Clone, Serialize)]
pub struct ControlOutputEntry {
    pub control_id: String,
    /// Applied control-wide output, 0-100.
    pub output_pct: f64,
}

/// One active manual override on the `/status` poll surface (DEC-163).
#[derive(Debug, Clone, Serialize)]
pub struct OverrideStatusEntry {
    pub control_id: String,
    pub pwm_percent: u8,
    pub expires_in_secs: u64,
}

/// One active fan-identify hold on the `/status` poll surface (DEC-166).
#[derive(Debug, Clone, Serialize)]
pub struct IdentifyStatusEntry {
    pub fan_id: String,
    pub expires_in_secs: u64,
    /// `"stop"` or `"pump_perturb"` (DEC-311) — so a GUI that polls into an
    /// identify it did not initiate still describes it truthfully.
    pub mode: String,
    /// The duty the fan is being held at for the identify.
    pub identify_pwm_percent: u8,
}

/// Per-subsystem health status.
#[derive(Debug, Clone, Serialize)]
pub struct SubsystemStatus {
    pub name: String,
    pub status: String,
    pub age_ms: Option<u64>,
    pub reason: String,
}

/// Response for `/sensors` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct SensorsResponse {
    pub api_version: u32,
    pub sensors: Vec<SensorEntry>,
}

/// A single sensor reading in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct SensorEntry {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub value_c: f64,
    pub source: String,
    pub age_ms: u64,
    /// Temperature change rate in degrees C per second (smoothed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_c_per_s: Option<f64>,
    /// Session minimum temperature since daemon start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_min_c: Option<f64>,
    /// Session maximum temperature since daemon start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_max_c: Option<f64>,
    /// Hwmon chip name (e.g. "k10temp", "nct6683", "it8696"). Always present —
    /// discovery requires a readable `name` attribute or the device is skipped
    /// ("amdgpu" for AMD GPU sources, "xe"/"i915" for Intel GPU). (audit P2-D)
    pub chip_name: String,
    /// Sysfs `tempN_type` value if present (3=diode, 4=thermistor, 5=AMD TSI, 6=Intel PECI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_type: Option<u8>,
    /// Curated hwmon temperature-threshold sysfs attributes (DEC-117). Omitted
    /// from the JSON when the driver exposes none of the attributes for this
    /// sensor. Alarm flags are sampled at discovery time only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<SensorThresholdsResponse>,
    /// False when this temperature must not be offered as a fan-curve source
    /// (DEC-193) — currently set only for wireless-radio PHY temps (e.g.
    /// `ath12k` WiFi), which read `ENETDOWN` whenever the radio is down and
    /// would strand a curve. Display is unaffected. Always present here; clients
    /// talking to a pre-2.3.0 daemon that omits it must default to `true`.
    pub control_eligible: bool,
}

/// JSON serialization shape for the curated hwmon threshold attributes
/// exposed on `SensorEntry` (DEC-117).
///
/// Every field is omitted from the JSON when `None`, so a sensor with only
/// `crit` available emits `{"thresholds": {"crit_c": 105.0}}` rather than
/// twelve null fields. Mirrors `crate::hwmon::types::SensorThresholds`.
#[derive(Debug, Clone, Serialize)]
pub struct SensorThresholdsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit_hyst_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emergency_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emergency_hyst_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcrit_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alarm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_alarm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit_alarm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault: Option<bool>,
}

impl From<&crate::hwmon::types::SensorThresholds> for SensorThresholdsResponse {
    fn from(t: &crate::hwmon::types::SensorThresholds) -> Self {
        Self {
            max_c: t.max_c,
            min_c: t.min_c,
            crit_c: t.crit_c,
            crit_hyst_c: t.crit_hyst_c,
            emergency_c: t.emergency_c,
            emergency_hyst_c: t.emergency_hyst_c,
            lcrit_c: t.lcrit_c,
            offset_c: t.offset_c,
            alarm: t.alarm,
            max_alarm: t.max_alarm,
            crit_alarm: t.crit_alarm,
            fault: t.fault,
        }
    }
}

/// Response for `/fans` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct FansResponse {
    pub api_version: u32,
    pub fans: Vec<FanEntry>,
}

/// A single fan in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct FanEntry {
    pub id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_commanded_pwm: Option<u8>,
    /// Firmware-reported current fan duty % (measured, not commanded) — present
    /// only for sources that expose a duty readback (NVIDIA via NVML, DEC-204).
    /// Distinct from `last_commanded_pwm`. May exceed 100 (NVML expresses it as
    /// a % of max noise tolerance). Additive/optional (API v1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duty_pct: Option<u8>,
    pub age_ms: u64,
    /// True when RPM is 0 but last_commanded_pwm is above the safety floor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_detected: Option<bool>,
    /// The **live** `pwmN_enable` mode for an hwmon header (AIO-MB Phase 4).
    ///
    /// On the poll rather than on `/hwmon/headers` because the daemon writes
    /// this attribute itself when it takes a header over: a discovery-time
    /// snapshot would report the pre-takeover mode for the process lifetime,
    /// while the field's whole diagnostic value is answering "is something else
    /// controlling this header *now*?". A different attribute from
    /// `PwmHeaderEntry::pwm_mode` (`pwmN_mode`, DC vs PWM).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pwm_enable_mode: Option<u8>,
    /// The driver's own `fanN_alarm` bit for this header (AIO-MB Phase 4).
    ///
    /// Rides the 1 Hz poll rather than `/hwmon/headers` because it is *state*:
    /// clients refetch headers only occasionally, so an alarm carried there
    /// would read "clear" while a fan is failing. Absent when the driver
    /// exposes no alarm attribute — which is not the same as "no alarm".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fan_alarm: Option<bool>,
    /// The **hardware readback** of `pwmN`, as a percent (AIO-MB Phase 5).
    ///
    /// Distinct from `last_commanded_pwm`, which for an hwmon header carries
    /// whichever of the poll's readback and the engine's command wrote last
    /// (AIO5-a). AIO-MB Phase 5 §3 needs the two as separate columns and §10
    /// classifies a device-side override from `command low + readback low + RPM
    /// high`, neither of which is expressible while they share a field.
    ///
    /// hwmon only — an OpenFan channel and a GPU fan have no equivalent
    /// attribute, and both emit `None` rather than echoing the command back as
    /// though it were a reading. Absent means "the daemon did not say", never 0%.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pwm_readback_pct: Option<u8>,
    /// The duty the daemon last **COMMANDED** for this hwmon header, as a
    /// percent (AIO-MB Phase 6).
    ///
    /// The command half of the pair whose readback half is `pwm_readback_pct`.
    /// Single-producer: only the hwmon write path sets it, so unlike
    /// `last_commanded_pwm` — which for an hwmon header carries whichever of the
    /// poll's readback and the engine's command wrote last (AIO5-a) — it is
    /// always a value the daemon actually chose.
    ///
    /// AIO-MB Phase 6 §6 requires requested PWM and hardware readback as
    /// separate numbers on the Hardware page ("do not collapse requested PWM and
    /// hardware readback into one number"), which diagnoses a write failure, a
    /// BIOS/EC reclaim and a device-side override apart from one another. Absent
    /// means "the daemon has never commanded this header" — never 0%.
    ///
    /// hwmon only. An OpenFan channel and a GPU fan already report an
    /// unambiguous single-producer command in `last_commanded_pwm` (their
    /// firmware reports no duty back), so they emit `None` here rather than
    /// duplicating it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pwm_commanded_pct: Option<u8>,
    /// Duty corrections the engine has written to this hwmon header since the
    /// daemon started (DEC-406): coalesced ticks whose `pwmN` readback disagreed
    /// with the command beyond the readback tolerance, so the duty was written
    /// again. A correction is evidence that something else wrote the header, or
    /// that it does not hold the duty it is given.
    ///
    /// hwmon only, and present on EVERY hwmon entry from a daemon with
    /// `control.duty_reconciliation` — `0` for a header never corrected. Absent
    /// means an older daemon, or an OpenFan/GPU entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duty_corrections: Option<u32>,
    /// The engine has stopped correcting this hwmon header's duty (DEC-406):
    /// several corrections in a row did not hold. Cleared when the command
    /// changes, the readback agrees again, or the engine stops commanding the
    /// header. Same presence rule as `duty_corrections`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duty_not_holding: Option<bool>,
}

/// A validation session's progress, for the poll surface (AIO-MB Phase 5).
///
/// Deliberately tiny: ids and counts, no samples. The full session — metadata,
/// telemetry, events, evidence and findings — is `GET /validation/session`, and
/// putting any of it here would put a megabyte on a 1 Hz poll.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationSessionSummary {
    pub session_id: String,
    pub kind: String,
    /// `idle` | `recording` | `completed` | `cancelled` | `interrupted` | `error`.
    /// A client must render an unrecognised token rather than dropping the row.
    pub state: String,
    pub elapsed_ms: u64,
    pub sample_count: usize,
    pub event_count: usize,
    pub sample_limit_reached: bool,
    pub cooling_device_id: String,
}

/// A single PWM header in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct PwmHeaderEntry {
    pub id: String,
    pub label: String,
    pub chip_name: String,
    /// Device identifier (PCI BDF or platform device name).
    pub device_id: String,
    pub pwm_index: u8,
    pub supports_enable: bool,
    pub rpm_available: bool,
    pub min_pwm_percent: u8,
    pub max_pwm_percent: u8,
    /// Whether the pwmN file is writable (checked at discovery).
    pub is_writable: bool,
    /// PWM/DC mode from pwmN_mode (0=DC, 1=PWM), absent if file not exposed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pwm_mode: Option<u8>,
    /// Whether this header belongs to a liquid cooler (NZXT Kraken /
    /// Aquacomputer) — daemon-authoritative AIO hint for the GUI (AIO Phase 1).
    /// Pre-1.18.0 daemons omit this; consumers default it to `false`.
    pub is_aio: bool,
    /// What this channel drives (AIO-MB Phase 1, daemon >= 2.28.0), with the
    /// user's `POST /config/header-role` assignment already applied.
    ///
    /// Per-channel, unlike the chip-level `is_aio`: a pump on a motherboard
    /// `AIO_PUMP` header is `role: "pump", is_aio: false`. Clients **must**
    /// render an unrecognised token rather than dropping the header (the 273-i
    /// rule). Pre-2.28.0 daemons omit this; consumers default to `"unknown"`.
    pub role: crate::hwmon::roles::HeaderRole,
    /// How `role` was established: `"none"`, `"label"`, `"chip_mapping"` or
    /// `"user_assigned"`. Lets a client show *why* a header is a pump, and
    /// distinguish a confident classification from a guess it should ask about.
    pub role_source: crate::hwmon::roles::RoleSource,

    // ── AIO-MB Phase 4 (DEC-316) ──────────────────────────────────────────
    // Every field below is OPTIONAL on the wire and absent when this daemon or
    // this driver cannot answer. That is load-bearing for `effective_min_pwm_pct`
    // in particular: a non-optional integer would parse as 0 against a pre-2.31
    // daemon, i.e. a client would believe a 0% floor on a pump. Absent means
    // "fall back to your own reconstruction", never "zero".
    /// The duty floor the daemon will actually enforce for this header, in
    /// percent — the resolved device policy clamped by the absolute pump
    /// backstop. Lets a client display the enforced number instead of
    /// re-deriving it from labels and chip names.
    ///
    /// **`0` for any header that is not pump-protected, whatever its device's
    /// policy declares (`WIRE-b`).** No enforcement site applies a policy floor
    /// to such a header — every one keys on `header_is_pump_protected`, in which
    /// cooling-device membership is not a term — so reporting the policy's own
    /// number would advertise a floor nothing honours. That is what a radiator
    /// fan in an AIO published until this was fixed: `30`, beside
    /// `stop_permitted: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_min_pwm_pct: Option<u8>,
    /// Whether this header may be driven to 0 at all. False wherever
    /// `header_is_pump_protected` holds, whatever a policy claims.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_permitted: Option<bool>,
    /// The cooling device that claims this header, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooling_device_id: Option<String>,
    /// PWM base frequency in Hz, from `pwmN_freq`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pwm_freq_hz: Option<u32>,
    /// The `pwmN_enable` values this chip's driver accepts. Empty means
    /// **unknown** — nothing in sysfs reports this, so it comes from driver
    /// knowledge and a client must not render an empty list as "no modes".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported_pwm_enable_modes: Vec<u8>,
    /// Low RPM alarm threshold from `fanN_min`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm_min_threshold: Option<u16>,
    /// High RPM threshold from `fanN_max`. Absent on most Super-I/O chips.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm_max_threshold: Option<u16>,
    /// Tachometer pulses per revolution from `fanN_pulses`. Absent on `it87`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tach_pulses_per_rev: Option<u8>,
}

impl PwmHeaderEntry {
    /// Single source of truth for the descriptor → wire-entry mapping
    /// (DEC-146 P3-12). Previously duplicated field-for-field in the
    /// headers and rescan handlers, which drifts when fields are added —
    /// this contract recently grew `pwm_mode` and `is_writable`.
    ///
    /// `assigned` is the user's `POST /config/header-role` override for this
    /// header, or `None`. It is a **required argument rather than an overlay a
    /// caller applies afterwards**, deliberately: the descriptor carries only
    /// the inference (discovery cannot see `runtime.toml`), so a call site that
    /// forgot the overlay would silently report a user-assigned pump as
    /// `unknown` — and the GUI would then offer to stop it. Taking it here
    /// makes that a compile error instead of a safety bug.
    ///
    /// `pump_protected` and `device` are required for exactly the same reason
    /// (AIO-MB Phase 4). `pump_protected` must be the daemon's own union
    /// predicate `AppState::header_is_pump_protected` — **never** `role ==
    /// Pump`, which DEC-312 records as a bug: a user may assign `chassis_fan`
    /// to a header the hardware labels `PUMP`, and the display role then reads
    /// `chassis_fan` while the daemon still refuses to stop it. Deriving the
    /// published floor from the display role would advertise a stoppable
    /// header the daemon will not stop.
    pub fn from_descriptor(
        h: &crate::hwmon::pwm_discovery::PwmHeaderDescriptor,
        assigned: Option<crate::hwmon::roles::HeaderRole>,
        pump_protected: bool,
        device: Option<&crate::hwmon::cooling_device::CoolingDeviceConfig>,
    ) -> Self {
        let (role, role_source) =
            crate::hwmon::roles::resolve_role(assigned, (h.role, h.role_source));
        // The policy a header resolves under. With no device the default
        // depends on whether the header is pump-protected: defaulting every
        // header to the pump policy would advertise a 30% floor on ordinary
        // chassis fans that the engine does not enforce.
        let policy = match device {
            Some(d) => d.resolved_policy(),
            None if pump_protected => &crate::hwmon::device_policy::GENERIC_PUMP,
            None => &crate::hwmon::device_policy::GENERIC_FAN,
        };
        let floor = crate::hwmon::device_policy::resolve_policy_floor(policy, pump_protected);
        PwmHeaderEntry {
            id: h.id.clone(),
            label: h.label.clone(),
            chip_name: h.chip_name.clone(),
            device_id: h.device_id.clone(),
            pwm_index: h.pwm_index,
            supports_enable: h.supports_enable,
            rpm_available: h.rpm_available,
            min_pwm_percent: h.min_pwm_percent,
            max_pwm_percent: h.max_pwm_percent,
            is_writable: h.is_writable,
            pwm_mode: h.pwm_mode,
            is_aio: h.is_aio,
            role,
            role_source,
            effective_min_pwm_pct: Some(floor.round() as u8),
            stop_permitted: Some(crate::hwmon::device_policy::stop_permitted(pump_protected)),
            cooling_device_id: device.map(|d| d.id.clone()),
            pwm_freq_hz: h.caps.pwm_freq_hz,
            supported_pwm_enable_modes: crate::hwmon::header_caps::supported_pwm_enable_modes(
                &h.chip_name,
            )
            .to_vec(),
            rpm_min_threshold: h.caps.rpm_min_threshold,
            rpm_max_threshold: h.caps.rpm_max_threshold,
            tach_pulses_per_rev: h.caps.tach_pulses_per_rev,
        }
    }
}

/// Response for `GET /hwmon/headers`.
#[derive(Debug, Clone, Serialize)]
pub struct PwmHeadersResponse {
    pub api_version: u32,
    pub headers: Vec<PwmHeaderEntry>,
}

/// The resolved device policy a cooling device operates under (AIO-MB Phase 4).
///
/// Published so a client can *show* the policy, never so it can set one: the
/// values come from `hwmon::device_policy`'s compiled-in table, selected by id.
/// `DevicePolicy` derives no `Deserialize`, so nothing inbound can construct one.
#[derive(Debug, Clone, Serialize)]
pub struct DevicePolicySummary {
    pub id: &'static str,
    pub display_name: &'static str,
    /// The policy's own declared floor. The floor a given *header* actually
    /// gets is `PwmHeaderEntry::effective_min_pwm_pct`, which additionally
    /// applies the absolute pump backstop.
    pub minimum_safe_pwm_pct: u8,
    pub supports_stop: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_override_seconds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_rpm_min: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_rpm_max: Option<u16>,
    pub internal_control_possible: bool,
}

impl DevicePolicySummary {
    pub fn from_policy(p: &'static crate::hwmon::device_policy::DevicePolicy) -> Self {
        DevicePolicySummary {
            id: p.id,
            display_name: p.display_name,
            minimum_safe_pwm_pct: p.minimum_safe_pwm.round() as u8,
            supports_stop: p.supports_stop,
            startup_override_seconds: p.startup_override_seconds,
            expected_rpm_min: p.expected_rpm_min,
            expected_rpm_max: p.expected_rpm_max,
            internal_control_possible: p.internal_control_possible,
        }
    }
}

/// One cooling device on `GET /inventory/cooling-devices` (AIO-MB Phase 4).
#[derive(Debug, Clone, Serialize)]
pub struct CoolingDeviceEntry {
    pub id: String,
    pub name: String,
    /// `unknown` | `aio_liquid` | `air_cooler` | `custom_loop`. Presentation
    /// only — no daemon branch reads it.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pump_member: Option<String>,
    pub radiator_members: Vec<String>,
    pub auxiliary_members: Vec<String>,
    /// Advisory. A curve keeps its own `sensor_id`; nothing in the control path
    /// reads this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_sensor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_sensor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coolant_sensor: Option<String>,
    /// `available` | `unavailable`. Unavailable is the normal case for a
    /// motherboard-connected AIO and is **not** an error or a readiness item.
    pub coolant_telemetry: &'static str,
    pub device_policy: DevicePolicySummary,
}

impl CoolingDeviceEntry {
    pub fn from_config(d: &crate::hwmon::cooling_device::CoolingDeviceConfig) -> Self {
        CoolingDeviceEntry {
            id: d.id.clone(),
            name: d.name.clone(),
            kind: d.resolved_kind().as_str(),
            pump_member: d.pump_member.clone(),
            radiator_members: d.radiator_members.clone(),
            auxiliary_members: d.auxiliary_members.clone(),
            preferred_sensor: d.preferred_sensor.clone(),
            fallback_sensor: d.fallback_sensor.clone(),
            coolant_sensor: d.coolant_sensor.clone(),
            coolant_telemetry: d.coolant_telemetry(),
            device_policy: DevicePolicySummary::from_policy(d.resolved_policy()),
        }
    }
}

/// Response for `GET /inventory/cooling-devices`.
#[derive(Debug, Clone, Serialize)]
pub struct CoolingDevicesResponse {
    pub api_version: u32,
    pub cooling_devices: Vec<CoolingDeviceEntry>,
    /// Every policy this daemon ships, so a client can offer the real choices
    /// rather than hardcoding a list that drifts from the binary.
    pub available_policies: Vec<DevicePolicySummary>,
}

/// A monitor-only fan tachometer in the hwmon inventory (Phase 1) — an
/// `fanN_input` with no matching `pwmN` control, i.e. RPM-readable but not
/// controllable. Controllable fans appear under `pwm_controls` instead (with
/// `rpm_available = true`). Structural only: the live RPM value is served via
/// `/fans` + `/poll`, not here.
#[derive(Debug, Clone, Serialize)]
pub struct FanInputEntry {
    pub id: String,
    /// Always `"hwmon"` — monitor-only fans are an hwmon-subsystem concept.
    pub source: String,
    pub chip_name: String,
    pub label: String,
    /// The N in `fanN_input`.
    pub fan_index: u8,
}

impl From<&crate::hwmon::inventory::FanInputDescriptor> for FanInputEntry {
    fn from(f: &crate::hwmon::inventory::FanInputDescriptor) -> Self {
        FanInputEntry {
            id: f.id.clone(),
            source: "hwmon".into(),
            chip_name: f.chip_name.clone(),
            label: f.label.clone(),
            fan_index: f.fan_index,
        }
    }
}

/// An inventory temperature sensor (Phase 2): the standard `SensorEntry` fields
/// plus a fine-grained classification refinement. The three extra fields are
/// flattened alongside the `SensorEntry` fields, so each `temp_sensors` object
/// carries `id`/`kind`/`value_c`/… *and* `classification`/`confidence`/
/// `rationale`. The refinement is advisory — `kind` and the daemon's thermal
/// safety are unchanged.
#[derive(Debug, Clone, Serialize)]
pub struct InventoryTempSensor {
    #[serde(flatten)]
    pub sensor: SensorEntry,
    /// Fine class: `cpu_package` | `cpu_core` | `cpu_tctl` | `cpu_tdie` |
    /// `motherboard_temp` | `vrm_temp` | `chipset_temp` | `gpu_temp` |
    /// `disk_temp` | `coolant_temp` | `unknown_temp`. A refinement of `kind`.
    pub classification: String,
    /// Classifier confidence: `high` | `medium` | `low` | `unknown`.
    pub confidence: String,
    /// Plain-English reason for the classification (daemon-owned wording).
    pub rationale: String,
}

/// The daemon's default-CPU-sensor recommendation. Advisory only: thermal safety
/// still uses the hottest CpuTemp, and this never silently replaces a user's
/// stored choice. Omitted from the response when no CPU temperature sensor was
/// found. `source` is `"user"` when it echoes the persisted preferred CPU sensor
/// (Phase 5), else `"auto"` for the deterministic auto-pick (Phase 2).
#[derive(Debug, Clone, Serialize)]
pub struct DefaultCpuEntry {
    pub sensor_id: String,
    pub confidence: String,
    pub rationale: String,
    pub source: String,
}

/// The user's persisted hardware selections echoed on the inventory (Phase 5).
/// Each id is the raw stored preference and may be stale — check the sensor list
/// or the readiness `selected_*_sensor_missing` items. The whole object is
/// omitted when nothing is persisted (additive).
#[derive(Debug, Clone, Serialize)]
pub struct InventoryPreferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_sensor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mb_sensor_id: Option<String>,
}

/// Response for `GET /inventory/hwmon` — a structured, read-only inventory of
/// hwmon-visible hardware for the GUI. Composed from existing discovery plus the
/// live cache; the daemon never writes hardware to build it.
///
/// - `temp_sensors`: the live sensor set (the standard `SensorEntry` fields plus
///   the Phase-2 `classification`/`confidence`/`rationale` refinement).
/// - `pwm_controls`: discovered controllable PWM headers (same as
///   `/hwmon/headers`).
/// - `monitor_only_fans`: RPM tachometers with no controllable `pwmN`
///   (previously invisible to the API). Omitted when empty (additive).
/// - `default_cpu`: the deterministic default-CPU recommendation, omitted when
///   no CPU sensor is present (additive).
#[derive(Debug, Clone, Serialize)]
pub struct HwmonInventoryResponse {
    pub api_version: u32,
    pub temp_sensors: Vec<InventoryTempSensor>,
    pub pwm_controls: Vec<PwmHeaderEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub monitor_only_fans: Vec<FanInputEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_cpu: Option<DefaultCpuEntry>,
    /// The user's persisted preferred CPU/motherboard sensors (Phase 5), omitted
    /// when none are set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferences: Option<InventoryPreferences>,
}

/// Response for `GET /inventory/readiness` — the structured hardware-readiness
/// list (Phase 3). Read-only diagnose-and-guide; the daemon never mutates the
/// system to produce it. `overall` is the most severe item's severity.
#[derive(Debug, Clone, Serialize)]
pub struct ReadinessResponse {
    pub api_version: u32,
    pub overall: crate::hwmon::readiness::ReadinessSeverity,
    pub items: Vec<crate::hwmon::readiness::ReadinessItem>,
}

/// Response for `GET /inventory/superio` (DEC-202) — the passive Super-I/O
/// detection report. Read-only; the daemon never probes I/O ports, loads
/// modules, or writes hardware to produce it. `arch_supported` is false on
/// non-x86 (with an empty `chips` list). Absent route ⇒ daemon predates the
/// feature (clients gate on 404, mirroring the other `/inventory/*` endpoints).
#[derive(Debug, Clone, Serialize)]
pub struct SuperIoResponse {
    pub api_version: u32,
    pub arch_supported: bool,
    pub chips: Vec<SuperIoChipEntry>,
    /// Driver names whose ISA I/O range collides with an ACPI OperationRegion.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub acpi_conflict_drivers: Vec<String>,
    /// Report-level notes (always carries the "present ≠ controllable" caveat).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// DEC-203: whether the opt-in active `/dev/port` probe
    /// (`POST /inventory/superio/probe`) can run right now. Off by default; the
    /// GUI gates its "probe" affordance on this.
    pub port_probe_available: bool,
    /// Plain-English reason for `port_probe_available` (`"available"`, or why not
    /// — flag off / no CAP_SYS_RAWIO / kernel lockdown / no `/dev/port`).
    pub port_probe_reason: String,
}

/// One detected Super-I/O chip in a [`SuperIoResponse`].
#[derive(Debug, Clone, Serialize)]
pub struct SuperIoChipEntry {
    pub chip_name: String,
    pub vendor: String,
    /// Evidence sources: `dmi_board_table` | `kernel_log` | `bound_hwmon`.
    pub evidence: Vec<String>,
    /// Presence confidence: `high` | `medium` | `low` | `unknown`.
    pub confidence: String,
    /// The module inferred to have bound this chip (present only when bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_driver: Option<String>,
    pub expected_module: String,
    pub module_loaded: bool,
    pub hwmon_present: bool,
    /// A load recommendation, present only for an unbound, allowlisted chip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<SuperIoRecommendationEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

/// A "load this driver" recommendation for an unbound chip.
#[derive(Debug, Clone, Serialize)]
pub struct SuperIoRecommendationEntry {
    pub module: String,
    pub in_mainline: bool,
    pub load_hint: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risk_notes: Vec<String>,
}

/// Response for `GET /inventory/hardware-readiness` (DEC-207) — the combined
/// readiness + Super-I/O snapshot the merged "Cooling Hardware Readiness" GUI page
/// fetches in ONE atomic request, so both halves come from a single shared passive
/// scan (no cross-endpoint drift, no redundant detection). Read-only; the daemon
/// never mutates the system to produce it. Absent route ⇒ daemon predates the
/// feature (clients gate on 404, mirroring the other `/inventory/*` endpoints).
/// The compact `rollup` mirrors the one on `/status` + `/poll`; `items` matches
/// `GET /inventory/readiness`; `superio` matches `GET /inventory/superio`.
#[derive(Debug, Clone, Serialize)]
pub struct HardwareReadinessResponse {
    pub api_version: u32,
    /// Compact rollup (same shape mirrored onto `/status` + `/poll`).
    pub rollup: crate::hwmon::readiness::ReadinessRollup,
    /// The overall severity (== `rollup.overall`), for convenience.
    pub overall: crate::hwmon::readiness::ReadinessSeverity,
    /// The full readiness list (== `GET /inventory/readiness`).
    pub items: Vec<crate::hwmon::readiness::ReadinessItem>,
    /// The passive Super-I/O report (== `GET /inventory/superio`).
    pub superio: SuperIoResponse,
    /// Milliseconds since the underlying passive scan completed (matches the API's
    /// `age_ms` freshness convention; the GUI renders a "last scanned" time).
    pub scanned_age_ms: u64,
    /// Monotonic scan id; changes exactly when a new scan is served, so the GUI
    /// can detect a fresh assessment without diffing.
    pub generation: u64,
}

/// One entry in the `GET /profiles` listing — a lightweight summary parsed from
/// each stored/preset profile (the full document is fetched via
/// `GET /profiles/{id}`).
#[derive(Debug, Clone, Serialize)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Response for `GET /profiles`.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileListResponse {
    pub api_version: u32,
    pub profiles: Vec<ProfileSummary>,
}

/// Response for `GET /capabilities`.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilitiesResponse {
    pub api_version: u32,
    pub daemon_version: String,
    pub ipc_transport: &'static str,
    pub devices: DeviceCapabilities,
    pub features: FeatureFlags,
    pub limits: Limits,
    /// Control-execution capability (GUI→daemon control migration). Additive
    /// top-level field (DEC-159/160) — pre-1.20 daemons omit it, and the GUI
    /// must treat its absence as "all false" (old behaviour). The GUI keys its
    /// daemon-owned-control startup gate on this block.
    pub control: ControlCapability,
}

/// Control-execution capability flags. The daemon advertises which control
/// responsibilities it can own. Each flag defaults to the pre-migration
/// behaviour when read by a client that doesn't understand it (AIP-180).
#[derive(Debug, Clone, Serialize, Default)]
pub struct ControlCapability {
    /// Daemon can store, validate, list, and delete GUI-authored profiles via
    /// the `/profiles` CRUD API. True since 1.19.0 (DEC-160).
    pub profile_storage: bool,
    /// Daemon can evaluate fan curves headlessly (always true — the engine has
    /// done this since the profile engine landed; see DEC-096).
    pub curve_evaluation: bool,
    /// Daemon exposes a manual-override API. False until the override API lands
    /// (DEC-163).
    pub manual_override: bool,
    /// Daemon exposes a fan-identify API. False until that API lands (DEC-166).
    pub fan_identify: bool,
    /// True only when the daemon engine is the sole authoritative fan writer —
    /// the 2.0.0 cutover (DEC-165) deleted the `gui_active` defer machinery, so
    /// the engine writes every tick a profile is active. A loop-less GUI may run
    /// against the daemon ONLY when this is true; a pre-2.0 daemon omits the
    /// field, so a client defaults it to `false` (AIP-180) and refuses to take
    /// control rather than silently leaving fans uncontrolled. The
    /// safety-critical version gate.
    pub autonomous_control: bool,
    /// Minimum GUI version this daemon supports, or empty when no floor is
    /// enforced. **This handler is the single source of that number**
    /// (`WIRE-ac`) — before daemon 2.36.0 it said `2.0.0` while every release
    /// note since 2.23.0 declared a `>= 2.23.0` pairing floor, so the contract
    /// had two answers and a client reading either one could be right.
    ///
    /// The floor is the GUI's, not the daemon's: it says which GUI versions this
    /// daemon is willing to be driven by, the opposite direction from
    /// `autonomous_control` (DEC-257). Raise it only for a genuine GUI-side
    /// requirement, and move the release-note line in the same change — the
    /// whole point of naming this the single source is that the prose now
    /// *quotes* it rather than asserting a second number.
    pub min_supported_gui: String,
    /// Daemon exposes `POST /fans/openfan/rescan` — adopting an OpenFanController
    /// that appeared after boot, without a restart (DEC-265). An older daemon
    /// omits the field, so a client defaults it to `false` and hides the action
    /// rather than offering a button that 404s.
    ///
    /// **Hardcoded `true`, and correctly so — unlike the `devices.openfan` fields
    /// this flag does NOT describe hardware** (`OFN-k`). It advertises the
    /// *endpoint's* existence, which is a property of the build, so it stays
    /// `true` on a machine that has never had a controller; the endpoint's job
    /// there is to answer `503 hardware_unavailable`. The name invites the other
    /// reading, which is why it is written down here: do not "fix" it to track
    /// `openfan_present`, or a client on a controller-less machine loses the one
    /// action that could adopt one.
    #[serde(default)]
    pub openfan_rescan: bool,
    /// `POST /config/profile-search-dirs` accepts a `remove` array, so a client
    /// can prune a stale profile search directory instead of only ever adding
    /// one. True since 2.23.0 (DEC-285).
    ///
    /// This flag is load-bearing in a way `openfan_rescan`'s is not: an older
    /// daemon does not 404 a `remove`, it parses only `add` and **silently
    /// ignores** the rest. A client that probed instead of checking would read
    /// that partial success as a whole one and tell the user a directory had
    /// been pruned when it had not.
    #[serde(default)]
    pub profile_search_dir_remove: bool,
    /// Daemon classifies PWM headers by role, protects `role: "pump"` headers
    /// from being stopped by identify or under-driven by verify, and accepts
    /// `POST /config/header-role`. True since 2.28.0 (DEC-311, AIO-MB Phase 1).
    ///
    /// Load-bearing for truthfulness, not just for hiding a button. A GUI that
    /// tells the user "the pump will briefly change speed" is **lying against a
    /// pre-2.28.0 daemon**, which drives the pump to 0 instead. So the wizard
    /// keys its copy on this flag and keeps the old "the fan will stop" wording
    /// when it is false — the honest description of what that daemon does.
    #[serde(default)]
    pub header_roles: bool,
    /// Daemon exposes `POST /hwmon/{id}/characterize` plus the
    /// `GET`/`DELETE /diagnostics/characterization` pair — the deeper PWM/RPM
    /// response sweep that sits alongside the quick verify. True since 2.29.0
    /// (AIO-MB Phase 3).
    ///
    /// A client MUST gate on this rather than probing: an older daemon 404s the
    /// POST — the same *status* this route returns for an unknown header id. The
    /// two differ only in `error.code` (`not_found` from the route fallback vs
    /// `validation_error` from the handler's own branch), and coupling feature
    /// detection to which error code came back is exactly what this flag exists
    /// to replace.
    #[serde(default)]
    pub pwm_characterization: bool,
    /// Daemon understands the AIO Phase 8 Batch 2 behaviour-characterisation
    /// inputs (`bidirectional`, `stability_seconds`) on
    /// `POST /hwmon/{id}/characterize`, publishes the `§2`-`§7` derivations on
    /// the run, and accepts the `pwm_behaviour_characterization` session
    /// diagnostic. True since 2.40.0 (DEC-334).
    ///
    /// **Separate from `pwm_characterization`, which keeps its old meaning.** An
    /// older daemon ignores the two new request fields rather than rejecting
    /// them, so a client that did not gate would silently get a plain ascending
    /// sweep and render empty hysteresis and stability panels as though the
    /// hardware had produced them.
    #[serde(default)]
    pub pwm_behaviour_characterization: bool,
    /// Daemon exposes the cooling-device topology surface:
    /// `GET /inventory/cooling-devices`, `POST /config/cooling-device` and
    /// `DELETE /config/cooling-device/{id}`. True since 2.31.0 (AIO-MB Phase 4).
    ///
    /// This gates the **endpoints** only. The additive header fields that came
    /// with them (`effective_min_pwm_pct`, `stop_permitted`, the capability
    /// audit) need no flag: each is optional on the wire, so absence already
    /// means "this daemon did not say" and a client falls back rather than
    /// believing a defaulted zero.
    #[serde(default)]
    pub cooling_devices: bool,
    /// Daemon exposes the validation-session surface: `POST`/`GET`/`DELETE
    /// /validation/session`, its `stop`/`event`/`measurement` sub-routes, and
    /// `GET /validation/sessions[/{id}]`. True since 2.32.0 (AIO-MB Phase 5).
    ///
    /// Gate on this rather than probing, for the same reason as
    /// `pwm_characterization` above: an older daemon 404s these routes from the
    /// fallback, which a client cannot tell from a genuine "no such session".
    #[serde(default)]
    pub validation_sessions: bool,
    /// AIO Phase 8 Batch 1, daemon >= 2.39.0. Gates
    /// `POST /hwmon/{id}/discover-control-path` and the
    /// `GET`/`DELETE /diagnostics/control-path` pair, plus the
    /// `control_path_discovery` token on a validation session's `diagnostics[]`.
    pub control_path_discovery: bool,
    /// AIO Phase 8 Batch 3a, daemon >= 2.41.0. Gates the
    /// `thermal_observation` session kind and the package/GPU power fields on a
    /// validation sample.
    ///
    /// **Required, not a convenience.** `POST /validation/session` falls back to
    /// `kind: "validation"` for a kind it does not recognise rather than
    /// rejecting it, so without this flag an older daemon accepts a thermal
    /// request, records an ordinary validation session, and returns 200 — and
    /// the client then labels and interprets it as a thermal observation. The
    /// flag is the only thing that distinguishes the two before the request.
    ///
    /// Carries `#[serde(default)]`, unlike the Batch 1 pair above: absent means
    /// "an older daemon", which is exactly a denial here.
    #[serde(default)]
    pub thermal_observation: bool,
    /// AIO Phase 8 Run 2, daemon >= 2.43.0. Gates the
    /// `stop_when_diagnostics_complete` field on `POST /validation/session` and
    /// its echo on the session document (`P8-az`).
    ///
    /// **Required, and for the same reason as `thermal_observation` above.**
    /// `serde` ignores an unknown field rather than rejecting it, so a daemon
    /// without this returns `200` for a request carrying the flag and then
    /// records for the full two-hour sample cap. Neither the status code nor the
    /// response body distinguishes the two — the echoed field is absent, which
    /// `#[serde(default)]` renders as `false` on a client that has the field and
    /// as nothing at all on one that does not. This flag is what lets a client
    /// decide whether to OFFER the option; the echoed field is what it renders
    /// once a session exists.
    #[serde(default)]
    pub validation_auto_stop: bool,
    /// Daemon applies an exit floor on a clean stop and accepts
    /// `POST /config/exit-floor`; `GET /config` reports
    /// `shutdown.exit_floor_pct` (DEC-388). A client must gate on this rather
    /// than probing: an older daemon 404s the POST and leaves every OpenFan
    /// channel at its last duty whatever the client shows.
    #[serde(default)]
    pub exit_floor: bool,
    /// AIO Phase 8 Batch 1, daemon >= 2.39.0. Gates `GET /diagnostics/preflight`.
    ///
    /// A SEPARATE flag from `control_path_discovery`, deliberately: preflight is
    /// read-only and applies to the pre-existing verify and characterise
    /// diagnostics as well, so a client may want it without offering discovery.
    pub diagnostic_preflight: bool,
    // ── `WIRE-k`: five features that predate their own flags ──────────────
    //
    // Each of the five below shipped before this block had a key for it, so a
    // client had two ways to detect them and both were wrong. It could compare
    // the daemon *version string* — which says when a feature first appeared,
    // not whether this build has it — or it could call the route and treat a
    // `404 not_found` as "unsupported". The comments on `pwm_characterization`
    // and `validation_sessions` above already state why probing is not a
    // contract: the route fallback's 404 is indistinguishable from a handler's
    // own 404 for an unknown id, so a probe cannot tell "no such feature" from
    // "no such header". Those two were given flags; these five never were.
    //
    // They are all `true` on any daemon new enough to serialise them, which is
    // the point — the flag's job is to let a client stop guessing, not to be
    // switchable. An older daemon omits them, they default `false`, and a client
    // falls back to exactly the probe-then-recover it does today.
    /// Daemon exposes `POST /gpu/{id}/fan/verify`. True since 1.11.0 (DEC-120).
    #[serde(default)]
    pub gpu_fan_verify: bool,
    /// Daemon exposes `GET /inventory/hardware-readiness`, the combined report
    /// that superseded `/inventory/readiness` and `/inventory/superio`. True
    /// since 2.11.0 (DEC-207).
    #[serde(default)]
    pub hardware_readiness: bool,
    /// Daemon exposes `POST /inventory/superio/probe`, the opt-in *active* port
    /// probe. True since 2.7.0 (DEC-203).
    ///
    /// Advertises the ROUTE, not that a probe will do anything: the probe is
    /// separately gated by daemon configuration and by `CAP_SYS_RAWIO`, and a
    /// disabled probe returns a normal report rather than an error. A client
    /// gates the *button* on this and still reads the report to learn whether
    /// the probe ran.
    #[serde(default)]
    pub superio_port_probe: bool,
    /// Daemon exposes `GET /inventory/hwmon` — the classified sensor inventory a
    /// client needs to offer preferred CPU/motherboard sensor selection. True
    /// since 2.6.0 (DEC-200).
    #[serde(default)]
    pub preferred_sensors: bool,
    /// Daemon exposes `GET /config` — its own effective merged configuration,
    /// so a client can read a knob back instead of keeping a local guess. True
    /// since 2.16.0 (DEC-243).
    #[serde(default)]
    pub daemon_config_report: bool,
    /// The engine checks a coalesced hwmon write's readback and rewrites a duty
    /// that did not hold, and publishes `duty_corrections` / `duty_not_holding`
    /// on every hwmon `/fans` and `/poll` entry. True since 2.53.0 (DEC-406).
    ///
    /// A client gates on this, not on the fields' presence, to say whether this
    /// daemon corrects drift at all: an older daemon omits both fields and
    /// leaves a second writer's duty standing for as long as the curve is
    /// steady, which is a claim the client must not make on its behalf.
    #[serde(default)]
    pub duty_reconciliation: bool,
    /// Daemon exposes `POST /hwmon/{id}/stall-probe` plus the
    /// `GET`/`DELETE /diagnostics/stall-probe` pair and the `pwm_stall_probe`
    /// preflight — the opt-in stall/restart probe below 20 %. True since 2.54.0
    /// (DEC-407). An older daemon 404s these routes and answers the preflight
    /// token with a 400.
    #[serde(default)]
    pub stall_probe: bool,
}

/// Per-device-group capability info.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCapabilities {
    pub openfan: OpenfanCapability,
    pub hwmon: HwmonCapability,
    pub amd_gpu: AmdGpuCapability,
    /// Intel discrete GPU (Arc) capability. Additive field (DEC-121) — older
    /// GUIs ignore it; read-only monitoring only, never fan-writable.
    pub intel_gpu: IntelGpuCapability,
    /// NVIDIA discrete GPU capability. Additive field (DEC-204) — older GUIs
    /// ignore it; read-only monitoring only, never fan-writable.
    pub nvidia_gpu: NvidiaGpuCapability,
    /// Liquid-cooler (AIO) hwmon capability — dynamic since 1.18.0 (DEC-156).
    pub aio_hwmon: AioHwmonCapability,
    /// USB-only coolers (liquidctl/USB-HID) are out of scope — always
    /// unsupported.
    pub aio_usb: UnsupportedCapability,
}

/// AMD discrete GPU capability details.
#[derive(Debug, Clone, Serialize)]
pub struct AmdGpuCapability {
    pub present: bool,
    /// Marketing name (e.g. "RX 9070 XT") or null if unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Compact display label (e.g. "9070XT" or "AMD D-GPU").
    pub display_label: String,
    /// PCI Bus:Device.Function address (legacy field name).
    ///
    /// Deprecated alias for `pci_bdf` — both fields carry the same value
    /// during the transition to canonical naming (M11 contract remediation,
    /// see GUI `CHANGELOG.md` v1.6.0 and `DECISIONS.md` DEC-042). New callers
    /// should prefer `pci_bdf`; this field will be removed in a future
    /// major version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<String>,
    /// PCI Bus:Device.Function address (canonical).
    ///
    /// Matches the field name already used by `GpuDiagnostics`, eliminating
    /// the `/capabilities` vs `/diagnostics/hardware` naming mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_bdf: Option<String>,
    /// PCI device ID (e.g. 0x7550 for Navi 48).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_device_id: Option<u16>,
    /// PCI revision (e.g. 0xC0 for XT variant).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_revision: Option<u8>,
    /// Fan control method: "pmfw_curve", "hwmon_pwm", or "none".
    pub fan_control_method: String,
    /// Whether PMFW fan curve is supported (RDNA3+).
    pub pmfw_supported: bool,
    /// Whether fan RPM reading is available.
    pub fan_rpm_available: bool,
    /// Whether this GPU has fan write capability (PMFW curve or hwmon pwm1+enable).
    pub fan_write_supported: bool,
    /// Whether this is a discrete (VGA) GPU vs render-only.
    pub is_discrete: bool,
    /// Whether the amdgpu overdrive feature is enabled (ppfeaturemask bit 14).
    pub overdrive_enabled: bool,
    /// Whether the PMFW zero-RPM sysfs file exists.
    pub gpu_zero_rpm_available: bool,
    /// Kernel-version advisories applicable to this GPU.
    ///
    /// Empty in normal operation. Populated when the running kernel matches a
    /// known amdgpu regression (e.g. RDNA3/RDNA4 hard-hang on 6.19, R9700 SMU
    /// mismatch on 7.0). The GUI surfaces high/critical entries as a one-time
    /// popup. See `crate::hwmon::kernel_warnings` for the catalog.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_warnings: Vec<crate::hwmon::kernel_warnings::KernelWarning>,
}

/// Intel discrete GPU (Arc) capability details (DEC-121).
///
/// Monitoring-only: Intel GPU fan control is firmware-managed and has no
/// userspace write path, so `fan_control_method` is always `"read_only"` (or
/// `"none"` when no fan is exposed) and there is no `fan_write_supported`
/// field — writes are never offered. Mirrors the read-only subset of
/// `AmdGpuCapability` without any of the PMFW/overdrive/zero-RPM fields.
#[derive(Debug, Clone, Serialize)]
pub struct IntelGpuCapability {
    pub present: bool,
    /// Marketing name (e.g. "Arc B580") or null if the device ID is unmapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Compact display label (e.g. "Arc B580" or "Intel D-GPU").
    pub display_label: String,
    /// PCI Bus:Device.Function address (legacy alias, kept symmetric with
    /// `amd_gpu` for the GUI's `_coalesce_pci_bdf` tolerance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<String>,
    /// PCI Bus:Device.Function address (canonical).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_bdf: Option<String>,
    /// PCI device ID (e.g. 0xE20B for Arc B580).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_device_id: Option<u16>,
    /// Kernel driver backing the GPU: "xe" or "i915".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Fan control method: always "read_only" (fan present) or "none".
    pub fan_control_method: String,
    /// Whether fan RPM reading is available (`fan1_input`).
    pub fan_rpm_available: bool,
    /// Whether this is a discrete (VGA) GPU. Always true for a detected Intel
    /// GPU (the hwmon node is DGFX-gated), emitted for symmetry.
    pub is_discrete: bool,
}

/// NVIDIA discrete GPU capability details (DEC-204).
///
/// Read-only, like the Intel Arc capability (DEC-121) — NVIDIA fan control is
/// never offered (nouveau's writable `pwm1` is excluded from discovery; the
/// NVML backend is telemetry-only), so there is no `fan_write_supported` field.
/// `model_name`/`driver_version` are populated only via the proprietary NVML
/// driver; the open `nouveau` leg yields a generic label.
#[derive(Debug, Clone, Serialize)]
pub struct NvidiaGpuCapability {
    pub present: bool,
    /// Product name (e.g. "NVIDIA GeForce RTX 4080") — NVML only, else null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Compact display label (model name, or "NVIDIA D-GPU").
    pub display_label: String,
    /// PCI Bus:Device.Function (legacy alias, kept symmetric with amd/intel_gpu
    /// for the GUI's `_coalesce_pci_bdf` tolerance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<String>,
    /// PCI Bus:Device.Function (canonical).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_bdf: Option<String>,
    /// Backing kernel driver: "nouveau" (open) or "nvidia" (proprietary). The
    /// proprietary GPU is read via the NVML library, but the kernel module — and
    /// so this field's value — is "nvidia" (mirrors Intel's `driver` semantics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// NVIDIA driver version (e.g. "565.77") — NVML only, else null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    /// Fan control method: "read_only" (fan present) or "none". Never writable.
    pub fan_control_method: String,
    /// Whether fan RPM reading is available.
    pub fan_rpm_available: bool,
    /// Whether this is a discrete GPU. Always true for a detected NVIDIA GPU.
    pub is_discrete: bool,
}

/// OpenFanController capability details.
///
/// **Every field describes the attached hardware, and every one is derived from
/// whether a controller is actually adopted (`OFN-k`, daemon >= 2.47.4).** Until
/// that fix `channels` and `rpm_support` were the literals `10` and `true`, so a
/// machine with no controller reported ten RPM-capable channels; a client that
/// checked `present` first was unaffected, and one that did not was lied to.
/// Read `present` first regardless — that is the field the other three qualify.
#[derive(Debug, Clone, Serialize)]
pub struct OpenfanCapability {
    /// Whether an OpenFanController is adopted on this machine.
    pub present: bool,
    /// Channel count of the attached controller, or `0` when none is attached.
    /// Fixed at `NUM_CHANNELS` (10) for OpenFan v1 hardware, which is the only
    /// hardware this daemon speaks to — so this varies with *presence*, not with
    /// the device.
    pub channels: u8,
    /// Whether the attached controller reports tachometer RPM. `false` when none
    /// is attached.
    pub rpm_support: bool,
    /// Whether PWM can be written to the attached controller. `false` when none
    /// is attached.
    pub write_support: bool,
}

/// Hwmon PWM capability details.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonCapability {
    pub present: bool,
    pub pwm_header_count: usize,
    pub write_support: bool,
}

/// AIO (all-in-one / liquid cooler) hwmon capability (AIO Phase 1, DEC-156).
///
/// A backward-compatible superset of [`UnsupportedCapability`]: `present` and
/// `status` are retained (pre-1.18.0 GUIs read only those), with
/// `pump_writable` / `coolant_available` added. Scope is hwmon-only — USB-only
/// coolers are out of scope and reported via the separate `aio_usb`, which
/// stays `unsupported` permanently.
///
/// `status` is honest about controllability: `"supported"` (a writable AIO
/// pump/fan header is present), `"monitor_only"` (a liquid cooler or coolant
/// sensor is detected but no writable AIO header exists — e.g. NZXT Kraken2, or
/// coolant sensing only; never presented as controllable), or `"unsupported"`
/// (nothing detected).
#[derive(Debug, Clone, Serialize)]
pub struct AioHwmonCapability {
    pub present: bool,
    pub status: &'static str,
    /// Whether at least one AIO header is writable (pump/fan controllable).
    pub pump_writable: bool,
    /// Whether a coolant-temperature sensor is available.
    pub coolant_available: bool,
}

impl AioHwmonCapability {
    /// Build the dynamic capability from discovered AIO headers + coolant
    /// sensing. `aio_total_headers` counts `is_aio` headers; `aio_writable`
    /// counts those that are also `is_writable`; `coolant_available` is whether
    /// any `CoolantTemp` sensor is cached.
    pub fn from_discovery(
        aio_total_headers: usize,
        aio_writable: usize,
        coolant_available: bool,
    ) -> Self {
        let present = aio_total_headers > 0 || coolant_available;
        let pump_writable = aio_writable > 0;
        let status = if !present {
            "unsupported"
        } else if pump_writable {
            "supported"
        } else {
            "monitor_only"
        };
        AioHwmonCapability {
            present,
            status,
            pump_writable,
            coolant_available,
        }
    }

    /// The "nothing detected" baseline (also the pre-discovery default).
    pub fn unsupported() -> Self {
        AioHwmonCapability {
            present: false,
            status: "unsupported",
            pump_writable: false,
            coolant_available: false,
        }
    }
}

/// Placeholder for unsupported device groups.
#[derive(Debug, Clone, Serialize)]
pub struct UnsupportedCapability {
    pub present: bool,
    pub status: &'static str,
}

/// Feature flags for the GUI.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureFlags {
    pub openfan_write_supported: bool,
    pub hwmon_write_supported: bool,
}

/// Policy-level limits the GUI should respect.
#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    pub pwm_percent_min: u8,
    pub pwm_percent_max: u8,
    pub openfan_stop_timeout_s: u8,
}

/// Response for `GET /sensors/history`.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryResponse {
    pub api_version: u32,
    pub entity_id: String,
    pub points: Vec<crate::health::history::HistorySample>,
}

/// Response for calibration sweep endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationResponse {
    pub api_version: u32,
    pub fan_id: String,
    pub points: Vec<crate::api::calibration::CalPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_pwm: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_pwm: Option<u8>,
    pub min_rpm: u16,
    pub max_rpm: u16,
}

/// Response for `POST /fans/openfan/rescan` (DEC-265).
///
/// Typed rather than an ad-hoc `json!` so the shape is declared in one place and
/// the `adopted: true` arm — which no test can reach without real hardware — is
/// still pinned by serialisation. It previously carried no `api_version` either,
/// silently breaking the "every success response carries one" general rule in
/// `docs/08` (DEC-266).
#[derive(Debug, Clone, Serialize)]
pub struct OpenFanRescanResponse {
    pub api_version: u32,
    /// A controller was found and installed by *this* call.
    pub adopted: bool,
    /// One was already connected, so nothing was probed.
    pub already_connected: bool,
    /// Present only when `adopted` — the port it was adopted on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    pub message: String,
}

/// Response for `GET /poll` — combined sensors, fans, and status in one call.
#[derive(Debug, Clone, Serialize)]
pub struct PollResponse {
    pub api_version: u32,
    pub status: StatusResponse,
    pub sensors: Vec<SensorEntry>,
    pub fans: Vec<FanEntry>,
}

// ── Hardware diagnostics ────────────────────────────────────────────

/// Response for `GET /diagnostics/hardware`.
#[derive(Debug, Clone, Serialize)]
pub struct HardwareDiagnosticsResponse {
    pub api_version: u32,
    pub hwmon: HwmonDiagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuDiagnostics>,
    /// Intel discrete GPU diagnostics (DEC-121). Additive — omitted when no
    /// Intel GPU is present; older clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intel_gpu: Option<IntelGpuDiagnostics>,
    /// NVIDIA discrete GPU diagnostics (DEC-204). Additive — omitted when no
    /// NVIDIA GPU is present; older clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvidia_gpu: Option<NvidiaGpuDiagnostics>,
    pub thermal_safety: ThermalSafetyInfo,
    pub kernel_modules: Vec<KernelModuleInfo>,
    pub acpi_conflicts: Vec<AcpiConflictInfo>,
    pub board: BoardInfo,
    /// DEC-405 (`PTR-f`). The running kernel's release
    /// (`/proc/sys/kernel/osrelease`), the one fact that separates a kernel
    /// update from a hardware change when two reports differ. `None` when
    /// unreadable. Length-capped.
    pub kernel_release: Option<String>,
    /// Chip names this DMI board is *expected* to expose, sourced from a
    /// curated dual-chip board lookup (`it87.c` DMI table + community
    /// reports). Empty when the board is not in the lookup. The GUI
    /// compares this against `hwmon.chips_detected[].chip_name` to detect
    /// missing chips that the driver failed to enumerate (DEC-101 — most
    /// commonly: the secondary IT87952E on Gigabyte X670/X870/Z790 boards
    /// that needs explicit `mmio=on` modparam to bind reliably).
    /// Older clients that don't know this field default to an empty list
    /// because the field is `skip_serializing_if = "Vec::is_empty"` on
    /// the wire and the GUI's `_filter_fields` parser tolerates it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_chips: Vec<String>,
    /// What the board's **firmware** declares it has, read from the Gigabyte SIV
    /// descriptor `it87` exports (`X87-d`, daemon >= 2.36.0).
    ///
    /// `expected_chips` above is a curated DMI lookup — an inference that is
    /// only ever as good as the table. This is a measurement from the board
    /// itself, so a client can state the deficit as a fact rather than deriving
    /// one from a hard-coded list.
    ///
    /// Compare `fan_count` against `hwmon.total_headers`, not against
    /// `writable_headers`: a BIOS-owned read-only header is discovered, and
    /// counting it as missing reports a phantom deficit on a working board.
    ///
    /// **`total_headers` is `pwmN`-capable headers only.** Monitor-only
    /// tachometers (a `fanN_input` with no matching `pwmN`) are a disjoint set
    /// and are not on this response at all — they live on `GET /inventory/hwmon`
    /// as `monitor_only_fans`. A client reading only this endpoint must therefore
    /// describe the difference as "headers with no controllable PWM", never as
    /// "unreachable headers": on a board with tach-only headers on a *detected*
    /// chip the latter overstates the deficit by exactly those headers.
    ///
    /// Absent on every non-Gigabyte board, when `it87` is not loaded, and when
    /// the descriptor does not decode — which is why it is an `Option` and never
    /// a defaulted zero. A zero `fan_count` would read as "this board has no fan
    /// headers", a far stronger claim than "the firmware did not say".
    ///
    /// It counts headers; it does not name chips. The DMI table remains the
    /// source for *which* chip should be carrying them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_firmware_counts: Option<crate::hwmon::gigabyte_siv::GigabyteSiv>,
    /// Best-effort kernel-level chip detection — chip names parsed out of
    /// `/dev/kmsg` `it87:` log lines (DEC-101). Populated when the daemon
    /// can read the kernel ring buffer (Arch default: `dmesg_restrict=0`).
    /// Empty when kmsg is not readable or no matches were found. Useful
    /// for the "kernel found chip but driver did not bind" diagnostic;
    /// not authoritative — the hwmon-bound chips in `chips_detected` are
    /// the source of truth for which PWM headers actually work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_detected_chips: Vec<String>,
    /// Pairs of currently loaded driver modules that are known to race for
    /// the same chip — distinct from `acpi_conflicts` (which is about I/O
    /// port ranges) and from the GUI's `CONFLICTING_MODULE_SETS` (which is
    /// a static name-pair table the GUI applies after the fact). Reported
    /// as CRITICAL severity for the canonical `(nct6687, nct6775)` case
    /// because writing PWM can corrupt the chip's non-volatile state — see
    /// `ModuleCollisionInfo` doc comment for the upstream incident.
    /// Empty (and omitted from the wire) on healthy systems.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_collisions: Vec<ModuleCollisionInfo>,
    /// CPU vendor read from `/proc/cpuinfo` `vendor_id` — `"Intel"`,
    /// `"AMD"`, or `""` (empty) when unknown. DEC-110: lets the GUI scope
    /// vendor-platform quirks (e.g. "this BIOS quirk applies on MSI Intel
    /// Z890 boards, not on MSI AMD X870E") without having to infer the
    /// platform from board name. Empty on hypervisors or when `/proc/cpuinfo`
    /// is unreadable; older clients that don't know the field skip it via
    /// `skip_serializing_if = "String::is_empty"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cpu_vendor: String,
    /// AMD VGA-class PCI devices and their driver binding (DEC-119). Detected
    /// independently of hwmon so a GPU whose `amdgpu` driver failed to bind
    /// (blacklist, KMS failure, vfio-pci passthrough) is still reported —
    /// such a device produces no hwmon node and is absent from `gpu`. Empty
    /// (and omitted) when no AMD VGA device exists; older clients skip it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub amd_pci_devices: Vec<AmdPciDeviceInfo>,
    /// Whether the `amdgpu` kernel module is loaded (`/sys/module/amdgpu`).
    /// Paired with `amd_pci_devices`: a device present with the module *not*
    /// loaded points at a blacklist or missing module; module loaded but
    /// device unbound points at a bind failure. Defaults to `false` for
    /// older clients that don't send the field.
    #[serde(default)]
    pub amdgpu_module_loaded: bool,
    /// Board voltage rails discovered from hwmon `inN_input` (`WIRE-ag`,
    /// daemon >= 2.37.0). Display-only: nothing in the daemon reads a rail, no
    /// control path consumes one, and they are deliberately **not** on
    /// `/status` or `/poll` — a rail moves by millivolts, so a 1 Hz copy would
    /// buy nothing for the payload it costs.
    ///
    /// **These are not temperature sensors and are not on `sensors[]`.** That
    /// array is temperature-shaped to the field name (`value_c`, `temp_type`,
    /// `thresholds.*_c`) and feeds curve binding and the thermal-safety path;
    /// a voltage in it would be a lie in a field those consumers trust. A rail
    /// can never be offered as a fan-curve source.
    ///
    /// Empty (and omitted) when no chip exposes an ADC channel, and on every
    /// older daemon — clients default it to an empty list.
    ///
    /// Read `identified` before rendering a value as a named rail: see
    /// [`VoltageEntry::identified`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voltages: Vec<VoltageEntry>,
}

/// One board voltage rail — an hwmon `inN_input` channel (`WIRE-ag`).
#[derive(Debug, Clone, Serialize)]
pub struct VoltageEntry {
    /// Stable id `hwmon:<chip>:<device_id>:in<N>`. The label is deliberately
    /// **not** embedded: a rail's label appears or changes when the user
    /// installs an `/etc/sensors.d` file, and an id that moved with it would
    /// break any client that had stored one.
    pub id: String,
    /// Hwmon chip name (e.g. `it8696`).
    pub chip_name: String,
    /// Channel index `N` from `inN_input`.
    pub channel: u8,
    /// `inN_label` where the driver publishes one, else `in{N}`.
    pub label: String,
    /// Volts **at the chip's input pin**, after whatever scaling the driver
    /// applies. This is the rail voltage only when `identified` is true — see
    /// that field.
    pub value_v: f64,
    /// True when the driver published an `inN_label` for this channel.
    ///
    /// **A client must render the two cases differently.** Boards routinely
    /// feed a rail through an external resistor divider the driver knows
    /// nothing about, so on an *unidentified* channel `value_v` is a genuine
    /// measurement of the pin and is **not** evidence of what any named rail is
    /// doing. Measured on the reference board (`it8696`): 10 channels, 3
    /// labelled. Presenting a divided 1.2 V reading with the same authority as
    /// a direct 3.3 V one is the specific failure this flag exists to prevent.
    pub identified: bool,
}

/// Hwmon chip diagnostics.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonDiagnostics {
    pub chips_detected: Vec<HwmonChipInfo>,
    pub total_headers: usize,
    pub writable_headers: usize,
    /// Cumulative BIOS pwm_enable reclaim events per header ID.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub enable_revert_counts: HashMap<String, u64>,
    /// Age in ms of the most recent counted reclaim, per header ID (DEC-360).
    ///
    /// Dates the counts beside it rather than replacing them. `enable_revert_counts`
    /// is monotonic for the life of the PWM controller and has no reset path, so a
    /// client reading it alone cannot tell an active BIOS fight from a single
    /// reclaim three weeks ago that the watchdog already remediated — and was
    /// therefore forced to present both identically, permanently.
    ///
    /// Shares a lifetime with the counts: both live on the controller and reset
    /// together, so an age here can never describe a different daemon's count.
    /// A header in `enable_revert_counts` but absent here means the age is
    /// **unknown**, never "just now" — clients must not default it to 0.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub enable_revert_last_seen_ms: HashMap<String, u64>,
}

/// Per-chip identification and driver info.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonChipInfo {
    pub chip_name: String,
    pub device_id: String,
    pub expected_driver: String,
    pub in_mainline_kernel: bool,
    pub header_count: usize,
}

/// GPU-specific diagnostics.
#[derive(Debug, Clone, Serialize)]
pub struct GpuDiagnostics {
    /// PCI Bus:Device.Function address (canonical).
    pub pci_bdf: String,
    /// Alias for `pci_bdf` — emitted so callers that consumed
    /// `/capabilities.amd_gpu.pci_id` can use the same field name here
    /// during the transition window (M11). Same string as `pci_bdf`.
    pub pci_id: String,
    pub pci_device_id: u16,
    pub pci_revision: u8,
    pub model_name: Option<String>,
    pub fan_control_method: String,
    pub overdrive_enabled: bool,
    pub ppfeaturemask: Option<String>,
    pub ppfeaturemask_bit14_set: bool,
    pub zero_rpm_available: bool,
    /// PMFW OD_RANGE fan-speed minimum (percent), parsed from the device's
    /// `fan_curve` `FAN_CURVE(fan speed)` range. The amdgpu driver rejects
    /// curve points below this with `EINVAL`, so it is the firmware-enforced
    /// reason a PMFW GPU fan cannot be driven to 0% via the curve (typically
    /// 15% on RDNA3+). `None` for non-PMFW GPUs. Additive field — older
    /// clients skip it (`skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_speed_min_pct: Option<u8>,
    /// PMFW OD_RANGE fan-speed maximum (percent), companion to
    /// `fan_speed_min_pct` (typically 100%). `None` for non-PMFW GPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_speed_max_pct: Option<u8>,
    /// PMFW `fan_minimum_pwm` setting (percent), best-effort parse of the
    /// `gpu_od/fan_ctrl/fan_minimum_pwm` sysfs attribute. `None` when the
    /// attribute is absent or unparseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_minimum_pwm: Option<u8>,
    /// Whether the `amdgpu` driver is bound to this GPU's PCI device. Always
    /// `true` here in practice (this struct is only built from an hwmon node,
    /// which requires a bound driver), but emitted for symmetry with
    /// `HardwareDiagnosticsResponse.amd_pci_devices` and forward-proofing.
    #[serde(default)]
    pub amdgpu_driver_bound: bool,
    /// Kernel-regression advisories for this GPU — the same catalog surfaced
    /// in `/capabilities.amd_gpu.kernel_warnings`, duplicated here so the
    /// diagnostics support bundle is self-contained. Empty (and omitted) when
    /// nothing applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_warnings: Vec<crate::hwmon::kernel_warnings::KernelWarning>,
}

/// Intel discrete GPU diagnostics (DEC-121).
///
/// Read-only by nature — there is no fan write path, ppfeaturemask, overdrive,
/// or PMFW curve, so this carries only identity, the backing driver, and a
/// fixed truthful explanation of why fan control is unavailable.
#[derive(Debug, Clone, Serialize)]
pub struct IntelGpuDiagnostics {
    /// PCI Bus:Device.Function address (canonical).
    pub pci_bdf: String,
    /// Alias for `pci_bdf`, emitted for symmetry with `GpuDiagnostics`.
    pub pci_id: String,
    pub pci_device_id: u16,
    pub pci_revision: u8,
    pub model_name: Option<String>,
    /// Kernel driver backing the GPU: "xe" or "i915".
    pub driver: String,
    /// Always "read_only" (fan present) or "none" — Intel never writable.
    pub fan_control_method: String,
    /// Whether `fan1_input` RPM reading is available.
    pub fan_rpm_available: bool,
    /// Truthful, user-facing explanation of why fan control is unavailable,
    /// suitable for direct display in the GUI System State page.
    pub fan_control_note: String,
}

/// NVIDIA discrete GPU diagnostics (DEC-204).
///
/// Read-only by nature — no fan write path exists (nouveau's `pwm1` is excluded
/// from discovery; the NVML backend is telemetry-only), so this carries only
/// identity, the backing driver leg, and a truthful explanation of why fan
/// control is unavailable.
#[derive(Debug, Clone, Serialize)]
pub struct NvidiaGpuDiagnostics {
    /// PCI Bus:Device.Function address (canonical).
    pub pci_bdf: String,
    /// Alias for `pci_bdf`, emitted for symmetry with the other GPU diagnostics.
    pub pci_id: String,
    /// Product name (NVML only) or null (nouveau).
    pub model_name: Option<String>,
    /// Backing kernel driver: "nouveau" (open) or "nvidia" (proprietary). The
    /// proprietary GPU is read via the NVML library, but the kernel module — and
    /// so this field's value — is "nvidia" (mirrors Intel's `driver` semantics).
    pub driver: String,
    /// NVIDIA driver version (NVML only) or null.
    pub driver_version: Option<String>,
    /// Always "read_only" (fan present) or "none" — NVIDIA never writable.
    pub fan_control_method: String,
    /// Whether fan RPM reading is available.
    pub fan_rpm_available: bool,
    /// Truthful, user-facing explanation of why fan control is unavailable.
    pub fan_control_note: String,
}

/// An AMD VGA-class PCI device and the driver bound to it (DEC-119).
///
/// Detected by scanning PCI space directly, independent of hwmon. Lets the
/// GUI distinguish "no AMD GPU installed" from "AMD GPU present but the
/// amdgpu driver is not bound" (blacklist, failed KMS, or vfio-pci
/// passthrough) — the latter produces no hwmon node, so the `gpu` field is
/// `None` and the GPU would otherwise be invisible.
#[derive(Debug, Clone, Serialize)]
pub struct AmdPciDeviceInfo {
    pub pci_bdf: String,
    pub pci_device_id: u16,
    /// Basename of the bound kernel driver, or `None` when unbound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Whether `amdgpu` is the bound driver.
    pub amdgpu_bound: bool,
    /// Whether this PCI device also surfaced an `amdgpu` hwmon node (i.e. it
    /// appears in the `gpu` field / fan-control path). `false` here while
    /// `amdgpu_bound` is `true` can indicate a very early-boot race or a
    /// render-only node without sensors.
    pub hwmon_present: bool,
}

/// Thermal safety rule status.
#[derive(Debug, Clone, Serialize)]
pub struct ThermalSafetyInfo {
    pub state: String,
    pub cpu_sensor_found: bool,
    pub emergency_threshold_c: f64,
    pub release_threshold_c: f64,
}

/// Kernel module load status.
#[derive(Debug, Clone, Serialize)]
pub struct KernelModuleInfo {
    pub name: String,
    pub loaded: bool,
    pub in_mainline: bool,
    /// DEC-405 (`PTR-f`). `/sys/module/<name>/version` — present only for a
    /// loaded module built with `MODULE_VERSION`, so `None` is common and means
    /// "not published", never "unknown version". Length-capped.
    pub version: Option<String>,
    /// DEC-405. `/sys/module/<name>/srcversion`, the checksum of the module's
    /// source — what tells two builds of one version apart (a DKMS rebuild, a
    /// kernel update). `None` when not loaded or not published.
    pub srcversion: Option<String>,
    /// DEC-405. The kernel's `O` taint flag on the loaded module: built outside
    /// the kernel tree (a DKMS or hand build). `None` when the module is not
    /// loaded or its taint could not be read — never a guessed `false`.
    pub out_of_tree: Option<bool>,
}

/// Detected ACPI I/O port conflict.
#[derive(Debug, Clone, Serialize)]
pub struct AcpiConflictInfo {
    pub io_range: String,
    pub claimed_by: String,
    pub conflicts_with_driver: String,
}

/// Detected pair of simultaneously loaded driver modules that are known to
/// race for the same chip and can corrupt the chip's non-volatile fan
/// control state.
///
/// The flagship case (DEC-105) is `(nct6687, nct6775)` — older out-of-tree
/// `nct6687` builds declare chip ID 0xd450, which is also the legitimate
/// chip ID of the upstream-supported NCT6797D. (The 0xd450 claim was removed
/// upstream in Fred78290/nct6687d PR #164, 2026 — see DEC-114 — but
/// already-loaded modules and not-yet-updated packages remain at risk.)
/// When both modules load,
/// whichever binds first claims the chip and the other may scribble into
/// the wrong registers. The original Bazzite report (ublue-os/bazzite
/// #4498) documents a CPU fan header being bricked by this exact load
/// ordering on MSI MAG X570 TOMAHAWK WIFI. The same chip family (NCT6797D)
/// appears on AM4 400-series MSI boards (e.g. B450M MORTAR, X470 GAMING
/// PRO CARBON) so the trap is not 500-series-only.
///
/// Severity is reported as a string (`"critical" | "high" | "medium"`)
/// so the GUI can render the appropriate banner without translating
/// numeric levels.
#[derive(Debug, Clone, Serialize)]
pub struct ModuleCollisionInfo {
    pub module_a: String,
    pub module_b: String,
    pub severity: String,
    pub summary: String,
    pub remediation: String,
}

/// Motherboard identification from DMI/SMBIOS.
#[derive(Debug, Clone, Serialize)]
pub struct BoardInfo {
    pub vendor: String,
    pub name: String,
    pub bios_version: String,
    /// DEC-405 (`PTR-f`). DMI `bios_date` as the firmware reports it (commonly
    /// `MM/DD/YYYY`; passed through, not parsed). `None` when absent — unlike
    /// the three fields above, which predate the rule and read `""`.
    pub bios_date: Option<String>,
}

// ── PWM verification ──────────────────────────────────────────────

/// Response for `POST /hwmon/{header_id}/verify`.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonVerifyResponse {
    pub header_id: String,
    /// "effective", "pwm_enable_reverted", "pwm_value_clamped",
    /// "no_rpm_effect", "rpm_unavailable", or — daemon >= 2.48.0, `ACK-m` —
    /// "pwm_readback_unavailable".
    ///
    /// The last is the one case `rpm_unavailable` used to absorb: no usable
    /// tach reading AND a post-write readback of `pwmN` (or of a `pwmN_enable`
    /// that was readable before the write) that produced nothing, so the
    /// "PWM values held" the older token asserts was never established. It is
    /// inconclusive, not a failure. An older client that does not know the
    /// token must render it rather than drop it (273-i).
    pub result: String,
    pub initial_state: HwmonVerifyState,
    pub final_state: HwmonVerifyState,
    pub test_pwm_percent: u8,
    pub wait_seconds: u8,
    pub details: String,
    /// True if the post-verify restore-to-original-PWM write failed. Older
    /// clients that don't know the field default to ``false`` on the GUI side.
    /// When true, the header is left at the verify test value rather than the
    /// caller's prior PWM — the caller may want to write the desired PWM
    /// explicitly instead of trusting the verify endpoint to have done so.
    #[serde(default, skip_serializing_if = "is_false")]
    pub restore_failed: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Snapshot of sysfs state during a PWM verify operation.
#[derive(Debug, Clone, Serialize)]
pub struct HwmonVerifyState {
    pub pwm_enable: Option<u8>,
    pub pwm_raw: Option<u8>,
    pub pwm_percent: Option<u8>,
    pub rpm: Option<u16>,
}

// ── GPU fan verification ──────────────────────────────────────────

/// Response for `POST /gpu/{gpu_id}/fan/verify`. No `api_version` field —
/// symmetric with [`HwmonVerifyResponse`], its sibling verify endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct GpuVerifyResponse {
    pub gpu_id: String,
    /// One of: "effective", "curve_not_applied", "no_rpm_effect",
    /// "zero_rpm_suppressed", "rpm_unavailable", "write_failed",
    /// "pwm_enable_reverted" (legacy pwm1 path only).
    pub result: String,
    pub initial_state: GpuVerifyState,
    pub final_state: GpuVerifyState,
    /// The (OD_RANGE-clamped) speed the verify drove the fan to.
    pub test_speed_pct: u8,
    pub wait_seconds: u8,
    /// "pmfw_curve" or "hwmon_pwm" — which write path was exercised.
    pub fan_control_method: String,
    pub details: String,
    /// True if restoring the pre-verify fan state failed (the fan may be left
    /// at the test speed). Older clients default to `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub restore_failed: bool,
}

/// Snapshot of GPU fan state during a verify operation. Fields are optional and
/// path-dependent: `zero_rpm_enabled` is populated on the PMFW path, `pwm_enable`
/// on the legacy `pwm1` path. `applied_speed_pct` is the read-back commanded
/// speed (flat curve value for PMFW, `pwm1` percent for legacy).
#[derive(Debug, Clone, Serialize)]
pub struct GpuVerifyState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_speed_pct: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pwm_enable: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zero_rpm_enabled: Option<bool>,
}

/// Body of `POST /control/{id}/override` (DEC-163). `ttl_secs` is an optional
/// per-grant deadman, clamped server-side to `[1, OVERRIDE_TTL_SECS]` so a
/// client cannot request a long single TTL that would defeat the deadman — it
/// extends an override by renewing, not by a long initial grant.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideTakeRequest {
    pub pwm_percent: u8,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// Body of `POST /control/{id}/override/renew` and `DELETE /control/{id}/override`.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideTokenRequest {
    pub override_token: u64,
}

/// Body of `POST /fans/{fan_id}/identify` (DEC-166). `action` is `"stop"` or
/// `"restore"`; `ttl_secs` (stop only) is the deadman, clamped as above.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentifyRequest {
    pub action: String,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// Response to `POST /control/{id}/override` — a fresh grant. `override_token`
/// is the monotonic identity+fence the caller presents on renew/release
/// (DEC-163). `renew_secs` is the advisory interval at which the GUI should
/// renew (~⅓ TTL).
#[derive(Debug, Clone, Serialize)]
pub struct OverrideGrantResponse {
    pub api_version: u32,
    pub control_id: String,
    pub override_token: u64,
    pub pwm_percent: u8,
    pub ttl_secs: u64,
    pub renew_secs: u64,
    pub expires_in_secs: u64,
}

/// Response to `POST /control/{id}/override/renew` — extended TTL.
#[derive(Debug, Clone, Serialize)]
pub struct OverrideRenewResponse {
    pub api_version: u32,
    pub control_id: String,
    pub override_token: u64,
    pub ttl_secs: u64,
    pub expires_in_secs: u64,
}

/// Response to `DELETE /control/{id}/override` — reverted to curve control.
/// `released` is `false` when nothing live was held (idempotent no-op).
#[derive(Debug, Clone, Serialize)]
pub struct OverrideReleaseResponse {
    pub api_version: u32,
    pub control_id: String,
    pub released: bool,
}

/// Response to `POST /fans/{fan_id}/identify` (DEC-166). `expires_in_secs` is
/// present only for `action: "stop"` (the deadman); `"restore"` omits it.
///
/// DEC-311 (AIO-MB Phase 1) added `mode` + the two duty fields. A client asks
/// for `"stop"` as before; the **daemon** decides whether that means a stop or
/// a pump-safe perturbation, from the header's role, and reports back which it
/// did. Keeping the decision server-side is what makes an older GUI safe by
/// construction — it cannot ask for a pump stop even by accident.
#[derive(Debug, Clone, Serialize)]
pub struct IdentifyResponse {
    pub api_version: u32,
    pub fan_id: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
    /// `"stop"` (driven to 0) or `"pump_perturb"` (shifted to a distinguishable
    /// duty that never goes below the pump floor). Omitted for `"restore"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The duty the fan is held at for the identify. `0` for `"stop"`; always
    /// `>= HARD_PUMP_CPU_FLOOR_PCT` for `"pump_perturb"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identify_pwm_percent: Option<u8>,
    /// The duty it was running at when the identify was taken, so the GUI can
    /// say "60% → 85%". `None` when nothing had been commanded yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_pwm_percent: Option<u8>,
}

/// Standard error envelope for all error responses.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

/// Error body within the envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub retryable: bool,
    pub source: String,
}

impl ErrorEnvelope {
    pub fn not_found(path: &str) -> Self {
        Self {
            error: ErrorBody {
                code: "not_found".into(),
                message: format!("endpoint not found: {path}"),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "validation_error".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    pub fn hardware_unavailable(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "hardware_unavailable".into(),
                message: message.into(),
                details: None,
                retryable: true,
                source: "hardware".into(),
            },
        }
    }

    /// The endpoint exists and the addressed device exists, but this device
    /// lacks the capability the caller is trying to exercise (e.g. a GPU with
    /// no PMFW `fan_curve` and no legacy `pwm1` write path). Distinct from
    /// `hardware_unavailable` (transient / retryable hardware failure) and
    /// `validation_error` (malformed request shape). Returned with HTTP 400
    /// and `retryable: false` — the condition is permanent for this device.
    pub fn feature_unavailable(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "feature_unavailable".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    /// A renew/release presented a stale or superseded override token — a
    /// thawed/resumed GUI cannot re-pin fans it no longer owns (Kleppmann
    /// fencing, DEC-163). HTTP 409 Conflict, `retryable: false`.
    pub fn stale_fencing_token(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "stale_fencing_token".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    /// A renew/release targeted an override that has expired (its deadman fired)
    /// or was never taken — the caller should re-take, not renew (DEC-163).
    /// HTTP 404 Not Found, `retryable: false`.
    pub fn override_expired(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "override_expired".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "internal_error".into(),
                message: message.into(),
                details: None,
                retryable: true,
                source: "internal".into(),
            },
        }
    }

    /// A runtime config write failed. Returned with HTTP 503 by
    /// `POST /config/*` handlers so the caller knows the change did not
    /// persist and can retry. See ADR-002.
    pub fn persistence_failed(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "persistence_failed".into(),
                message: message.into(),
                details: None,
                retryable: true,
                source: "internal".into(),
            },
        }
    }

    /// A profile with the requested id already exists in the daemon store.
    /// Returned with HTTP 409 by `POST /profiles` so the caller distinguishes
    /// a duplicate create from a malformed request (DEC-160). The check is
    /// store-scoped — a read-only preset of the same id is shadowable, not a
    /// conflict.
    pub fn already_exists(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "already_exists".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    /// The profile is currently active and so cannot be deleted. Returned with
    /// HTTP 409 by `DELETE /profiles/{id}` (DEC-160). Deactivate or activate a
    /// different profile first.
    pub fn profile_in_use(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "profile_in_use".into(),
                message: message.into(),
                details: None,
                retryable: false,
                source: "validation".into(),
            },
        }
    }

    /// A `409 thermal_abort` — a hardware fan action (verify / calibrate) was
    /// refused because a sensor is too hot to run it safely. Retryable once the
    /// system cools. Phase 6 / DEC-201 wires this into the verify handlers;
    /// mirrors the calibrate sweep's abort (DEC-134).
    pub fn thermal_abort(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: "thermal_abort".into(),
                message: message.into(),
                details: None,
                retryable: true,
                source: "hardware".into(),
            },
        }
    }

    /// A `validation_error` carrying structured per-field violations in
    /// `details` (DEC-160). The envelope shape is unchanged — `details` is the
    /// existing free-form field — so older clients that read only
    /// `code`/`message` keep working while newer clients read
    /// `details.field_violations[]`.
    pub fn validation_with_details(message: impl Into<String>, details: serde_json::Value) -> Self {
        Self {
            error: ErrorBody {
                code: "validation_error".into(),
                message: message.into(),
                details: Some(details),
                retryable: false,
                source: "validation".into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_envelope_serializes() {
        let env = ErrorEnvelope::not_found("/nonexistent");
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["error"]["code"], "not_found");
        assert_eq!(json["error"]["retryable"], false);
        assert_eq!(json["error"]["source"], "validation");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/nonexistent"));
        // details should be absent (skip_serializing_if)
        assert!(json["error"].get("details").is_none());
    }

    #[test]
    fn internal_error_is_retryable() {
        let env = ErrorEnvelope::internal("something broke");
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["error"]["code"], "internal_error");
        assert_eq!(json["error"]["retryable"], true);
    }

    #[test]
    fn feature_unavailable_is_not_retryable() {
        let env = ErrorEnvelope::feature_unavailable("GPU has no fan write path");
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["error"]["code"], "feature_unavailable");
        assert_eq!(json["error"]["retryable"], false);
        assert_eq!(json["error"]["source"], "validation");
    }

    #[test]
    fn override_token_request_requires_token() {
        // `override_token` has no serde default — a token-less body must fail to
        // deserialize, so a renew/release handler can never treat a missing token
        // as 0. Pins the DEC-163 fencing contract at the type level (cheaper and
        // more direct than asserting axum's plain-text extractor rejection).
        assert!(serde_json::from_str::<OverrideTokenRequest>("{}").is_err());
        let ok: OverrideTokenRequest = serde_json::from_str(r#"{"override_token": 7}"#).unwrap();
        assert_eq!(ok.override_token, 7);
    }

    /// Every key the GUI-consumed structs put on the wire, pinned **against the
    /// shared oracle** `tests/fixtures/wire_fields.json` (`P8-cb`).
    ///
    /// That file is the single declaration. This test reads it and asserts each
    /// struct's serialised key set against the declared `fields`; the GUI's
    /// `tests/test_wire_field_coverage.py` asserts the same lists against its
    /// dataclasses. So a rename here reds this test naming the fixture, and there
    /// is no second list to forget.
    ///
    /// **This used to be a workflow and not an interlock, and that is what
    /// `P8-cb` closed.** The `want` lists lived here as 29 literal arrays, the
    /// fixture declared the same lists over there, each was checked only against
    /// its own side, and nothing compared the two — so a rename fixed here and
    /// forgotten in the fixture left the fixture stale with both suites green.
    /// That is the pairing the 2026-09-05 wire sweep found missing — 41
    /// divergences, none of which anything compared (register row `WIRE-aj`).
    ///
    /// The oracle is shared in the `parity_vectors.json` shape (DEC-126): one
    /// byte-identical copy per repo, compared by the GUI's
    /// `test_fixture_copies_are_byte_identical` when both repos are siblings and
    /// by `.github/workflows/parity.yml` in single-repo CI.
    ///
    /// Every `Option` is `Some` and every `Vec` non-empty on purpose: the structs
    /// in THIS module are dense with `skip_serializing_if`, so a `None` would drop
    /// the key and the assertion would silently stop covering it. The 16 Phase 8
    /// structs added by `P8-ca` carry none, so their key set is unconditional and
    /// populating them is realism rather than necessity — but keep doing it, since
    /// a future `skip_serializing_if` would otherwise change what this pins.
    ///
    /// Scope covers the structs behind `/sensors`, `/fans`, `/poll`,
    /// `/hwmon/headers`, `/inventory/hwmon`, `/inventory/cooling-devices`,
    /// `/capabilities` (`Limits`) and `/diagnostics/hardware` (`VoltageEntry`),
    /// plus the Phase 8 diagnostic surfaces — preflight, control-path discovery,
    /// PWM characterisation, steady state and the startup fingerprint.
    ///
    /// **All 29 fixture entries have a daemon-side arm (`P8-ca`), and the
    /// coverage is now asserted BOTH WAYS at the foot of this test.** A struct
    /// declared in the fixture with no arm here fails, and so does an arm here
    /// for a struct the fixture does not declare — the first is the `P8-ca` gap
    /// (16 entries `G33` declared on the GUI side alone, which catches the GUI
    /// dropping a field it should model and never the daemon renaming one), and
    /// the second is its mirror. Adding a struct is an arm here plus a fixture
    /// entry; it is not automatic, but forgetting either half is no longer
    /// silent. **Keep the scope list above current** — it is what the next person
    /// reads to decide whether a struct belongs.
    #[test]
    fn wire_field_surface_is_pinned() {
        use std::collections::BTreeSet;

        fn keys(v: &serde_json::Value) -> Vec<String> {
            let mut k: Vec<String> = v
                .as_object()
                .expect("expected a JSON object")
                .keys()
                .cloned()
                .collect();
            k.sort();
            k
        }

        // The ONE declaration, shared byte-for-byte with the GUI (`P8-cb`).
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/wire_fields.json"
        );
        let text =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read wire oracle: {e}"));
        let oracle: serde_json::Value = serde_json::from_str(&text).expect("parse wire oracle");
        let structs = oracle["structs"].as_array().expect("structs array");
        assert!(!structs.is_empty(), "an empty oracle asserts nothing");

        let declared = |name: &str| -> Vec<String> {
            let mut lists: Vec<Vec<String>> = structs
                .iter()
                .filter(|s| s["daemon"] == name)
                .map(|s| {
                    s["fields"]
                        .as_array()
                        .unwrap_or_else(|| panic!("{name}: fixture entry has no fields array"))
                        .iter()
                        .map(|f| f.as_str().expect("field name is a string").to_string())
                        .collect()
                })
                .collect();
            let first = lists.pop().unwrap_or_else(|| {
                panic!(
                    "{name} is pinned here but absent from wire_fields.json — add the \
                     fixture entry, or this arm asserts against nothing"
                )
            });
            // One wire struct may be modelled by two GUI dataclasses, and then it
            // gets two fixture entries. They must declare ONE key set: the two
            // drifting apart while both looked maintained IS `WIRE-h`
            // (`PwmHeaderEntry` as `HwmonHeader` and as `InventoryPwmControl`).
            for other in &lists {
                assert_eq!(
                    other, &first,
                    "{name}: two fixture entries for one wire struct declare \
                     different field sets — one shape, one list"
                );
            }
            first
        };

        // `RefCell` so both closures below can be `Fn` and one can call the other.
        let exercised: std::cell::RefCell<BTreeSet<String>> = Default::default();
        let expect_at = |v: &serde_json::Value, name: &str, at: &str| {
            let mut w = declared(name);
            w.sort();
            exercised.borrow_mut().insert(name.to_string());
            assert_eq!(
                keys(v),
                w,
                "{at}: serialised key set has drifted from wire_fields.json. That \
                 fixture is the single declaration both repos check against, so fix \
                 the struct or the fixture (both copies, byte-identical) — and docs/08."
            );
        };
        let expect = |v: &serde_json::Value, name: &str| expect_at(v, name, name);

        let thresholds = SensorThresholdsResponse {
            max_c: Some(90.0),
            min_c: Some(0.0),
            crit_c: Some(95.0),
            crit_hyst_c: Some(3.0),
            emergency_c: Some(100.0),
            emergency_hyst_c: Some(2.0),
            lcrit_c: Some(-5.0),
            offset_c: Some(0.0),
            alarm: Some(false),
            max_alarm: Some(false),
            crit_alarm: Some(false),
            fault: Some(false),
        };
        let sensor = SensorEntry {
            id: "hwmon:k10temp:pci0:Tctl".into(),
            kind: "cpu_temp".into(),
            label: "Tctl".into(),
            value_c: 48.0,
            source: "hwmon".into(),
            age_ms: 120,
            rate_c_per_s: Some(0.25),
            session_min_c: Some(31.0),
            session_max_c: Some(91.5),
            chip_name: "k10temp".into(),
            temp_type: Some(5),
            thresholds: Some(thresholds),
            control_eligible: true,
        };
        expect(&serde_json::to_value(&sensor).unwrap(), "SensorEntry");

        let fan = FanEntry {
            id: "hwmon:it8696:pci0:pwm1".into(),
            source: "hwmon".into(),
            rpm: Some(900),
            last_commanded_pwm: Some(40),
            duty_pct: Some(41),
            age_ms: 100,
            stall_detected: Some(false),
            pwm_enable_mode: Some(1),
            fan_alarm: Some(false),
            pwm_readback_pct: Some(40),
            pwm_commanded_pct: Some(40),
            duty_corrections: Some(2),
            duty_not_holding: Some(false),
        };
        expect(&serde_json::to_value(&fan).unwrap(), "FanEntry");

        let header = PwmHeaderEntry {
            id: "hwmon:it8696:pci0:pwm3:AIO_PUMP".into(),
            label: "AIO_PUMP".into(),
            chip_name: "it8696".into(),
            device_id: "pci0".into(),
            pwm_index: 3,
            supports_enable: true,
            rpm_available: true,
            min_pwm_percent: 0,
            max_pwm_percent: 100,
            is_writable: true,
            pwm_mode: Some(1),
            is_aio: false,
            role: crate::hwmon::roles::HeaderRole::Pump,
            role_source: crate::hwmon::roles::RoleSource::Label,
            effective_min_pwm_pct: Some(30),
            stop_permitted: Some(false),
            cooling_device_id: Some("dev-1".into()),
            pwm_freq_hz: Some(25000),
            supported_pwm_enable_modes: vec![0, 1, 2],
            rpm_min_threshold: Some(300),
            rpm_max_threshold: Some(3000),
            tach_pulses_per_rev: Some(2),
        };
        expect(&serde_json::to_value(&header).unwrap(), "PwmHeaderEntry");

        // `temp_sensors[]` flattens the whole SensorEntry alongside the
        // refinement — the exact shape whose six dropped keys were `WIRE-h`.
        let inv_sensor = InventoryTempSensor {
            sensor: sensor.clone(),
            classification: "cpu_tctl".into(),
            confidence: "high".into(),
            rationale: "k10temp Tctl".into(),
        };
        expect(
            &serde_json::to_value(&inv_sensor).unwrap(),
            "InventoryTempSensor",
        );

        let fan_input = FanInputEntry {
            id: "hwmon:it8696:pci0:fan5".into(),
            source: "hwmon".into(),
            chip_name: "it8696".into(),
            label: "SYS_FAN5".into(),
            fan_index: 5,
        };
        expect(&serde_json::to_value(&fan_input).unwrap(), "FanInputEntry");

        let default_cpu = DefaultCpuEntry {
            sensor_id: "hwmon:k10temp:pci0:Tctl".into(),
            confidence: "high".into(),
            rationale: "k10temp Tctl".into(),
            source: "auto".into(),
        };
        expect(
            &serde_json::to_value(&default_cpu).unwrap(),
            "DefaultCpuEntry",
        );

        let inventory = HwmonInventoryResponse {
            api_version: API_VERSION,
            temp_sensors: vec![inv_sensor],
            pwm_controls: vec![header.clone()],
            monitor_only_fans: vec![fan_input],
            default_cpu: Some(default_cpu),
            preferences: Some(InventoryPreferences {
                cpu_sensor_id: Some("hwmon:k10temp:pci0:Tctl".into()),
                mb_sensor_id: Some("hwmon:it8696:pci0:temp2".into()),
            }),
        };
        expect(
            &serde_json::to_value(&inventory).unwrap(),
            "HwmonInventoryResponse",
        );
        // `pwm_controls[]` is this same PwmHeaderEntry — one wire struct behind
        // two GUI names, which is how half of `WIRE-h` stayed invisible.
        expect_at(
            &serde_json::to_value(&inventory).unwrap()["pwm_controls"][0],
            "PwmHeaderEntry",
            "HwmonInventoryResponse.pwm_controls[]",
        );

        let policy = DevicePolicySummary {
            id: "aio_generic",
            display_name: "Generic AIO",
            minimum_safe_pwm_pct: 30,
            supports_stop: false,
            startup_override_seconds: Some(10),
            expected_rpm_min: Some(1200),
            expected_rpm_max: Some(3000),
            internal_control_possible: false,
        };
        expect(
            &serde_json::to_value(&policy).unwrap(),
            "DevicePolicySummary",
        );

        let device = CoolingDeviceEntry {
            id: "dev-1".into(),
            name: "Kraken X63".into(),
            kind: "aio_liquid",
            pump_member: Some("hwmon:it8696:pci0:pwm3:AIO_PUMP".into()),
            radiator_members: vec!["hwmon:it8696:pci0:pwm1:CHA_FAN1".into()],
            auxiliary_members: vec!["hwmon:it8696:pci0:pwm2:CHA_FAN2".into()],
            preferred_sensor: Some("hwmon:k10temp:pci0:Tctl".into()),
            fallback_sensor: Some("hwmon:it8696:pci0:temp2".into()),
            coolant_sensor: Some("hwmon:z53:usb-3-2:temp1".into()),
            coolant_telemetry: "available",
            device_policy: policy.clone(),
        };
        expect(
            &serde_json::to_value(&device).unwrap(),
            "CoolingDeviceEntry",
        );

        let limits = Limits {
            pwm_percent_min: 0,
            pwm_percent_max: 100,
            openfan_stop_timeout_s: 8,
        };
        expect(&serde_json::to_value(&limits).unwrap(), "Limits");

        let devices = CoolingDevicesResponse {
            api_version: API_VERSION,
            cooling_devices: vec![device],
            available_policies: vec![policy],
        };
        expect(
            &serde_json::to_value(&devices).unwrap(),
            "CoolingDevicesResponse",
        );

        // `WIRE-ag`. Pinned from the first release that publishes it, so the
        // GUI cannot silently fail to model a rail — the WIRE-h failure mode.
        let rail = VoltageEntry {
            id: "hwmon:it8696:pci0:in7".into(),
            chip_name: "it8696".into(),
            channel: 7,
            label: "3VSB".into(),
            value_v: 3.288,
            identified: true,
        };
        expect(&serde_json::to_value(&rail).unwrap(), "VoltageEntry");

        // ── `P8-ca`: the 16 structs `G33` pinned on the GUI side ONLY ──
        //
        // `G33` declared these in `tests/fixtures/wire_fields.json` so the GUI
        // cannot drop a field it should model. That half is fixture-driven and
        // never queries a live daemon, so it catches the GUI dropping a field and
        // NOT the daemon renaming one — a rename left the declared list stale and
        // the GUI test green. These arms are the other half.
        //
        // Unlike the structs above, none of these carries `skip_serializing_if`,
        // so their serialised key set is unconditional and populating the
        // `Option`s is realism rather than necessity.
        use crate::api::characterization::{
            CharPoint, CharSummary, EstimatedRpm, PlateauSpan, PointStability,
        };
        use crate::api::discovery::{
            ControlPathCandidate, ControlPathRun, DiscoveryCycle, DiscoverySummary, TachChannel,
            TachObservation,
        };
        use crate::api::preflight::{PreflightCheck, PreflightReport};
        use crate::api::stats::SteadyState;
        use crate::control_paths::ControlPathRecord;
        use crate::validation::session::StartupFingerprint;

        let check = PreflightCheck {
            check_id: "thermal_state".into(),
            state: "pass".into(),
            detail: "thermal_state=normal".into(),
        };
        expect(&serde_json::to_value(&check).unwrap(), "PreflightCheck");

        let report = PreflightReport {
            header_id: "hwmon:it8696:isa-0a40:pwm1:CPU_FAN".into(),
            diagnostic: "control_path_discovery".into(),
            verdict: "blocked".into(),
            checks: vec![check],
            blocking: vec!["pump_protected".into()],
        };
        expect(&serde_json::to_value(&report).unwrap(), "PreflightReport");

        let observation = TachObservation {
            tach_id: "hwmon:it8696:isa-0a40:fan1".into(),
            baseline_rpm: Some(820),
            perturbed_rpm: Some(1180),
            delta_rpm: Some(360),
            noise_floor_rpm: 30,
            responded: true,
            noise_floor_from_cycle_1: false,
        };
        expect(
            &serde_json::to_value(&observation).unwrap(),
            "TachObservation",
        );

        let channel = TachChannel {
            tach_id: "hwmon:it8696:isa-0a40:fan1".into(),
            label: "CPU_FAN".into(),
            monitor_only: false,
            is_target_header: true,
        };
        expect(&serde_json::to_value(&channel).unwrap(), "TachChannel");

        let cycle = DiscoveryCycle {
            cycle: 1,
            baseline_pct: 40,
            perturbed_pct: 70,
            direction: "up".into(),
            observations: vec![observation],
            baseline_settled: Some(true),
            settle_wait_ms: 8_000,
        };
        expect(&serde_json::to_value(&cycle).unwrap(), "DiscoveryCycle");

        let candidate = ControlPathCandidate {
            tach_id: "hwmon:it8696:isa-0a40:fan1".into(),
            label: "CPU_FAN".into(),
            monitor_only: false,
            confidence: "high".into(),
            direction: "up".into(),
            baseline_rpm: Some(820),
            perturbed_rpm: Some(1180),
            change_pct: Some(43.9),
            cycles_responded: 2,
            cycles_total: 2,
        };
        expect(
            &serde_json::to_value(&candidate).unwrap(),
            "ControlPathCandidate",
        );

        let summary = DiscoverySummary {
            relationship: "drives".into(),
            confidence: "high".into(),
            candidates: vec![candidate],
            measurement_resolution_ms: Some(500),
            sample_interval_ms: 500,
            sample_count: 24,
            confidence_notes: vec!["two of two cycles responded".into()],
        };
        expect(&serde_json::to_value(&summary).unwrap(), "DiscoverySummary");

        let run = ControlPathRun {
            run_id: "cpd-1".into(),
            header_id: "hwmon:it8696:isa-0a40:pwm1:CPU_FAN".into(),
            state: "completed".into(),
            delta_pct: 30,
            requested_cycles: 2,
            window_seconds: 12,
            baseline_pct: 40,
            perturbed_pct: 70,
            direction: "up".into(),
            channels: vec![channel],
            cycles: vec![cycle],
            summary: Some(summary),
            original_pct: Some(40),
            restore_failed: false,
            restore_outcome: "restored".into(),
            detail: Some("two cycles, one responder".into()),
            completed_unix_ms: Some(1_757_000_000_000),
        };
        expect(&serde_json::to_value(&run).unwrap(), "ControlPathRun");

        let record = ControlPathRecord {
            header_id: "hwmon:it8696:isa-0a40:pwm1:CPU_FAN".into(),
            relationship: "drives".into(),
            confidence: "high".into(),
            tach_ids: vec!["hwmon:it8696:isa-0a40:fan1".into()],
            tach_labels: vec!["CPU_FAN".into()],
            direction: "up".into(),
            baseline_rpm: Some(820),
            perturbed_rpm: Some(1180),
            change_pct: Some(43.9),
            run_id: "cpd-1".into(),
            validated_unix_ms: 1_757_000_000_000,
        };
        expect(&serde_json::to_value(&record).unwrap(), "ControlPathRecord");

        let stability = PointStability {
            samples: 24,
            usable: 23,
            dropouts: 1,
            outliers: 0,
            mean_rpm: Some(1180.4),
            median_rpm: Some(1181),
            min_rpm: Some(1160),
            max_rpm: Some(1200),
            stddev_rpm: Some(11.2),
            cv_pct: Some(0.95),
            verdict: "stable".into(),
            sample_interval_ms: 500,
            dwell_ms: 12_000,
            window_start_ms: 5_400,
            update_interval_ms: Some(2_000),
        };
        expect(&serde_json::to_value(&stability).unwrap(), "PointStability");

        let estimated = EstimatedRpm {
            value: 1180,
            provenance: "measured".into(),
            correction_factor: 1.0,
            correction_source: "none".into(),
        };
        expect(&serde_json::to_value(&estimated).unwrap(), "EstimatedRpm");

        let plateau = PlateauSpan {
            from_pct: 20,
            to_pct: 35,
            rpm_min: 640,
            rpm_max: 700,
        };
        expect(&serde_json::to_value(plateau).unwrap(), "PlateauSpan");

        let point = CharPoint {
            requested_pct: 70,
            command_accepted: true,
            readback_pct: Some(70),
            readback_raw: Some(178),
            pwm_enable: Some(1),
            rpm_before: Some(820),
            rpm_after: Some(1180),
            settle_ms: 6_000,
            first_change_ms: Some(400),
            readback_verdict: "exact".into(),
            rpm_verdict: "responded".into(),
            direction: "up".into(),
            step_index: 3,
            settled_ms: Some(5_400),
            stability: Some(stability),
            estimated_physical_rpm: Some(estimated),
        };
        expect(&serde_json::to_value(&point).unwrap(), "CharPoint");

        let char_summary = CharSummary {
            command_acceptance: "accepted".into(),
            pwm_readback: "exact".into(),
            rpm_response: "monotonic".into(),
            min_tested_pct: Some(20),
            max_tested_pct: Some(100),
            min_rpm: Some(640),
            max_rpm: Some(1900),
            monotonic: Some(true),
            monotonic_falling: Some(true),
            monotonic_rising: Some(true),
            dead_zone_upper_pct: Some(15),
            clamp_pct: None,
            possible_device_override: false,
            interference_detected: false,
            hysteresis_pct: Some(3.2),
            hysteresis_verdict: "low".into(),
            hysteresis_worst_duty_pct: Some(45),
            hysteresis_worst_delta_rpm: Some(80),
            hysteresis_compared_points: 9,
            min_responsive_pct: Some(20),
            max_responsive_pct: Some(95),
            low_plateau_to_pct: Some(35),
            saturation_from_pct: Some(95),
            plateaus: vec![plateau],
            stability_verdict: "stable".into(),
            worst_cv_pct: Some(1.8),
            total_dropouts: 1,
            total_outliers: 0,
            measurement_resolution_ms: Some(500),
            typical_response_ms: Some(400),
            typical_settling_ms: Some(5_400),
            outside_learned_range: Some(false),
            learned_range_note: Some("within the learned range".into()),
            interpretation_states: vec!["monotonic".into()],
        };
        expect(&serde_json::to_value(&char_summary).unwrap(), "CharSummary");

        let steady = SteadyState {
            verdict: "steady".into(),
            start_ms: Some(240_000),
            warmup_ms: Some(240_000),
            slope_c_per_min: Some(0.04),
            stddev_c: Some(0.31),
            mean_c: Some(48.2),
            peak_c: Some(49.1),
            confidence: "high".into(),
            criterion: "slope_and_stddev".into(),
        };
        expect(&serde_json::to_value(&steady).unwrap(), "SteadyState");

        let fingerprint = StartupFingerprint {
            member_id: "hwmon:it8696:isa-0a40:pwm5:PUMP".into(),
            role: "pump".into(),
            override_observed: true,
            override_duration_ms: Some(4_200),
            peak_rpm: Some(2_900),
            requested_pct_during: Some(30),
            readback_pct_during: Some(100),
            post_override_rpm: Some(1_400),
            transition_ms: Some(900),
            interpretation: "device_startup_override".into(),
        };
        expect(
            &serde_json::to_value(&fingerprint).unwrap(),
            "StartupFingerprint",
        );

        // DEC-405 (`PTR-e`): a session's verify evidence, enrolled when the GUI
        // started reading it — the struct whose fields were always `None`.
        let verify_evidence = crate::validation::session::VerifyEvidence {
            header_id: "hwmon:it87:isa-0a30:pwm2:PUMP".into(),
            write_ok: true,
            readback_pct: Some(80),
            requested_pct: Some(80),
            rpm_before: Some(1200),
            rpm_after: Some(1932),
            detail: Some("x".into()),
            result: Some("effective".into()),
            restore_failed: false,
        };
        expect(
            &serde_json::to_value(&verify_evidence).unwrap(),
            "VerifyEvidence",
        );

        // DEC-404 Stage 4 (`PTR-r`): `GET /diagnostics/hardware`, enrolled when the
        // GUI's PWM Test Report became the first reader of DEC-405's environment
        // facts. Every optional top-level field is populated, so every declared
        // key serialises; the nested structs are pinned at this level only.
        let board = BoardInfo {
            vendor: "Gigabyte".into(),
            name: "X870E AORUS MASTER".into(),
            bios_version: "F14c".into(),
            bios_date: Some("08/14/2025".into()),
        };
        expect(&serde_json::to_value(&board).unwrap(), "BoardInfo");
        let kernel_module = KernelModuleInfo {
            name: "it87".into(),
            loaded: true,
            in_mainline: false,
            version: Some("v1.0-160".into()),
            srcversion: Some("0123456789ABCDEF01234567".into()),
            out_of_tree: Some(true),
        };
        expect(
            &serde_json::to_value(&kernel_module).unwrap(),
            "KernelModuleInfo",
        );
        let hardware = HardwareDiagnosticsResponse {
            api_version: API_VERSION,
            hwmon: HwmonDiagnostics {
                chips_detected: Vec::new(),
                total_headers: 8,
                writable_headers: 8,
                enable_revert_counts: HashMap::new(),
                enable_revert_last_seen_ms: HashMap::new(),
            },
            gpu: Some(GpuDiagnostics {
                pci_bdf: "0000:03:00.0".into(),
                pci_id: "1002:744c".into(),
                pci_device_id: 0x744c,
                pci_revision: 0xc8,
                model_name: Some("Radeon RX 7900 XTX".into()),
                fan_control_method: "pmfw_curve".into(),
                overdrive_enabled: true,
                ppfeaturemask: Some("0xffffffff".into()),
                ppfeaturemask_bit14_set: true,
                zero_rpm_available: true,
                fan_speed_min_pct: Some(15),
                fan_speed_max_pct: Some(100),
                fan_minimum_pwm: Some(15),
                amdgpu_driver_bound: true,
                kernel_warnings: Vec::new(),
            }),
            intel_gpu: Some(IntelGpuDiagnostics {
                pci_bdf: "0000:04:00.0".into(),
                pci_id: "8086:56a0".into(),
                pci_device_id: 0x56a0,
                pci_revision: 0x08,
                model_name: Some("Arc A770".into()),
                driver: "xe".into(),
                fan_control_method: "read_only".into(),
                fan_rpm_available: true,
                fan_control_note: "x".into(),
            }),
            nvidia_gpu: Some(NvidiaGpuDiagnostics {
                pci_bdf: "0000:05:00.0".into(),
                pci_id: "10de:2684".into(),
                model_name: Some("RTX 4090".into()),
                driver: "nvidia".into(),
                driver_version: Some("580.95".into()),
                fan_control_method: "read_only".into(),
                fan_rpm_available: true,
                fan_control_note: "x".into(),
            }),
            thermal_safety: ThermalSafetyInfo {
                state: "normal".into(),
                cpu_sensor_found: true,
                emergency_threshold_c: 110.0,
                release_threshold_c: 80.0,
            },
            kernel_modules: vec![kernel_module],
            acpi_conflicts: Vec::new(),
            board,
            kernel_release: Some("6.18.2-1-cachyos".into()),
            expected_chips: vec!["it8696".into()],
            board_firmware_counts: Some(crate::hwmon::gigabyte_siv::GigabyteSiv {
                platform: 1,
                special: 0,
                fan_count: 8,
                temp_count: 6,
                volt_count: 10,
            }),
            kernel_detected_chips: vec!["it8696".into()],
            module_collisions: vec![ModuleCollisionInfo {
                module_a: "nct6687".into(),
                module_b: "nct6775".into(),
                severity: "critical".into(),
                summary: "x".into(),
                remediation: "y".into(),
            }],
            cpu_vendor: "AMD".into(),
            amd_pci_devices: vec![AmdPciDeviceInfo {
                pci_bdf: "0000:03:00.0".into(),
                pci_device_id: 0x744c,
                driver: Some("amdgpu".into()),
                amdgpu_bound: true,
                hwmon_present: true,
            }],
            amdgpu_module_loaded: true,
            voltages: vec![VoltageEntry {
                id: "hwmon:it8696:pci0:in0".into(),
                chip_name: "it8696".into(),
                channel: 0,
                label: "in0".into(),
                value_v: 1.236,
                identified: false,
            }],
        };
        expect(
            &serde_json::to_value(&hardware).unwrap(),
            "HardwareDiagnosticsResponse",
        );

        // DEC-404 Stage 4 (`PTR-u`): the DEC-407 stall probe, enrolled when the
        // GUI's PWM Test Report modelled and read it.
        let probe_point = crate::api::stall_probe::ProbePoint {
            phase: "descent".into(),
            step_index: 3,
            commanded_pct: 8,
            command_accepted: true,
            readback_pct: Some(8),
            pwm_enable: Some(1),
            rpm_before: Some(420),
            rpm_after: Some(0),
            held_ms: 6_000,
            confirmed_at_ms: Some(4_000),
            samples: 12,
            zero_samples: 8,
            observation: "stalled".into(),
        };
        expect(&serde_json::to_value(&probe_point).unwrap(), "ProbePoint");
        let probe_run = crate::api::stall_probe::StallProbeRun {
            run_id: "probe-1".into(),
            header_id: "hwmon:it8696:it87.2624:pwm2:SYS_FAN1".into(),
            state: "complete".into(),
            outcome: Some("stall_and_restart_found".into()),
            points: vec![probe_point],
            ..Default::default()
        };
        expect(&serde_json::to_value(&probe_run).unwrap(), "StallProbeRun");

        // **Both directions, and this is what makes the oracle an interlock
        // rather than a workflow (`P8-cb`).** A struct declared in the fixture
        // with no arm here is the `P8-ca` gap — sixteen entries the GUI checked
        // against its own dataclasses while nothing checked them against the
        // daemon. An arm here for a struct the fixture does not declare is the
        // mirror: a key set pinned on this side that the GUI never models.
        // Neither can be reported by an equality that only runs per-arm, because
        // a missing arm runs nothing at all.
        let all: BTreeSet<String> = structs
            .iter()
            .map(|s| {
                s["daemon"]
                    .as_str()
                    .expect("daemon name is a string")
                    .to_string()
            })
            .collect();
        assert_eq!(
            *exercised.borrow(),
            all,
            "wire_fields.json and this test cover different struct sets. Declared \
             but not pinned here: {:?}. Pinned here but not declared: {:?}.",
            all.difference(&exercised.borrow()).collect::<Vec<_>>(),
            exercised.borrow().difference(&all).collect::<Vec<_>>(),
        );
    }

    /// `WIRE-ag`: the rail list is additive — omitted entirely when no chip
    /// exposes an ADC channel, so an older client sees the response it always
    /// saw and a client on a board without rails is not handed an empty array
    /// to distinguish from "this daemon does not publish them".
    #[test]
    fn voltages_are_omitted_from_the_wire_when_empty() {
        // Asserted against the REALISED response, not against a re-derivation of
        // `skip_serializing_if` — a test that rebuilds production's own model
        // shares production's blind spot by construction (`CLAUDE.md`, DEC-320).
        fn response(voltages: Vec<VoltageEntry>) -> serde_json::Value {
            serde_json::to_value(HardwareDiagnosticsResponse {
                api_version: API_VERSION,
                hwmon: HwmonDiagnostics {
                    chips_detected: Vec::new(),
                    total_headers: 0,
                    writable_headers: 0,
                    enable_revert_counts: HashMap::new(),
                    enable_revert_last_seen_ms: HashMap::new(),
                },
                gpu: None,
                intel_gpu: None,
                nvidia_gpu: None,
                thermal_safety: ThermalSafetyInfo {
                    state: "normal".into(),
                    cpu_sensor_found: true,
                    emergency_threshold_c: 105.0,
                    release_threshold_c: 80.0,
                },
                kernel_modules: Vec::new(),
                acpi_conflicts: Vec::new(),
                board: BoardInfo {
                    vendor: "Gigabyte".into(),
                    name: "X870E AORUS MASTER".into(),
                    bios_version: "F14c".into(),
                    bios_date: Some("08/14/2025".into()),
                },
                kernel_release: Some("6.18.2-1-cachyos".into()),
                expected_chips: Vec::new(),
                board_firmware_counts: None,
                kernel_detected_chips: Vec::new(),
                module_collisions: Vec::new(),
                cpu_vendor: String::new(),
                amd_pci_devices: Vec::new(),
                amdgpu_module_loaded: false,
                voltages,
            })
            .unwrap()
        }

        assert!(
            response(Vec::new()).get("voltages").is_none(),
            "an empty rail list must be omitted, not published as [] — an older \
             client must see the response it always saw"
        );

        let populated = response(vec![VoltageEntry {
            id: "hwmon:it8696:pci0:in0".into(),
            chip_name: "it8696".into(),
            channel: 0,
            label: "in0".into(),
            value_v: 1.236,
            identified: false,
        }]);
        let rails = populated["voltages"].as_array().expect("voltages array");
        assert_eq!(rails.len(), 1);
        // An unidentified rail still carries a real measurement and says so.
        assert_eq!(rails[0]["identified"], false);
        assert_eq!(rails[0]["label"], "in0");
        assert!((rails[0]["value_v"].as_f64().unwrap() - 1.236).abs() < 1e-9);
    }

    #[test]
    fn hwmon_inventory_response_schema() {
        // Phase 2: temp_sensors flatten the standard SensorEntry fields with the
        // classification refinement; default_cpu + monitor_only_fans are omitted
        // when absent (additive).
        let empty = HwmonInventoryResponse {
            api_version: API_VERSION,
            temp_sensors: Vec::new(),
            pwm_controls: Vec::new(),
            monitor_only_fans: Vec::new(),
            default_cpu: None,
            preferences: None,
        };
        let json = serde_json::to_value(&empty).unwrap();
        assert_eq!(json["api_version"], 1);
        assert!(json["temp_sensors"].is_array());
        assert!(json["pwm_controls"].is_array());
        assert!(
            json.get("monitor_only_fans").is_none(),
            "monitor_only_fans must be omitted when empty (additive)"
        );
        assert!(
            json.get("default_cpu").is_none(),
            "default_cpu must be omitted when no CPU sensor (additive)"
        );

        let populated = HwmonInventoryResponse {
            api_version: API_VERSION,
            temp_sensors: vec![InventoryTempSensor {
                sensor: SensorEntry {
                    id: "hwmon:k10temp:0000:00:18.3:Tctl".into(),
                    kind: "cpu_temp".into(),
                    label: "Tctl".into(),
                    value_c: 55.0,
                    source: "hwmon".into(),
                    age_ms: 100,
                    rate_c_per_s: None,
                    session_min_c: None,
                    session_max_c: None,
                    chip_name: "k10temp".into(),
                    temp_type: None,
                    thresholds: None,
                    control_eligible: true,
                },
                classification: "cpu_tctl".into(),
                confidence: "high".into(),
                rationale: "k10temp Tctl control temperature".into(),
            }],
            pwm_controls: Vec::new(),
            monitor_only_fans: vec![FanInputEntry {
                id: "hwmon:nct6798:isa:fan3:PUMP_TACH".into(),
                source: "hwmon".into(),
                chip_name: "nct6798".into(),
                label: "PUMP_TACH".into(),
                fan_index: 3,
            }],
            default_cpu: Some(DefaultCpuEntry {
                sensor_id: "hwmon:k10temp:0000:00:18.3:Tctl".into(),
                confidence: "high".into(),
                rationale: "only CPU candidate".into(),
                source: "auto".into(),
            }),
            preferences: Some(InventoryPreferences {
                cpu_sensor_id: Some("hwmon:k10temp:0000:00:18.3:Tctl".into()),
                mb_sensor_id: None,
            }),
        };
        let json = serde_json::to_value(&populated).unwrap();
        // Flatten: the SensorEntry fields sit at the same level as the
        // classification refinement on each temp_sensors entry.
        assert_eq!(
            json["temp_sensors"][0]["id"],
            "hwmon:k10temp:0000:00:18.3:Tctl"
        );
        assert_eq!(json["temp_sensors"][0]["kind"], "cpu_temp");
        assert_eq!(json["temp_sensors"][0]["classification"], "cpu_tctl");
        assert_eq!(json["temp_sensors"][0]["confidence"], "high");
        // default_cpu present with the recommended sensor id.
        assert_eq!(
            json["default_cpu"]["sensor_id"],
            "hwmon:k10temp:0000:00:18.3:Tctl"
        );
        assert_eq!(json["default_cpu"]["confidence"], "high");
        assert_eq!(json["default_cpu"]["source"], "auto");
        // Phase 5: persisted preferences echoed (mb omitted when unset).
        assert_eq!(
            json["preferences"]["cpu_sensor_id"],
            "hwmon:k10temp:0000:00:18.3:Tctl"
        );
        assert!(json["preferences"].get("mb_sensor_id").is_none());
        // monitor_only_fans still surfaces unchanged.
        assert_eq!(json["monitor_only_fans"][0]["fan_index"], 3);
    }

    #[test]
    fn status_response_schema() {
        let resp = StatusResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            overall_status: "ok".into(),
            subsystems: vec![SubsystemStatus {
                name: "openfan".into(),
                status: "ok".into(),
                age_ms: Some(500),
                reason: "readings fresh".into(),
            }],
            uptime_seconds: Some(3600),
            thermal_state: "normal".into(),
            overrides: Vec::new(),
            fan_identify: Vec::new(),
            unavailable_sensors: Vec::new(),
            skipped_controls: Vec::new(),
            control_outputs: Vec::new(),
            runtime_config_degraded: None,
            validation_session: None,
            active_profile_id: None,
            active_profile_name: None,
            has_active_profile: false,
            readiness: None,
            verify_active: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["overall_status"], "ok");
        assert_eq!(json["subsystems"][0]["name"], "openfan");
        assert_eq!(json["subsystems"][0]["age_ms"], 500);
        // DEC-170: the counters envelope was removed entirely (its only field,
        // last_error_summary, was permanently dead).
        assert!(json.get("counters").is_none());
        // DEC-132: thermal_state is always serialized (additive field).
        assert_eq!(json["thermal_state"], "normal");
        // DEC-163/166: override + identify arrays omitted when empty (additive).
        assert!(json.get("overrides").is_none());
        assert!(json.get("fan_identify").is_none());
        // DEC-193: unavailable_sensors omitted when empty (additive).
        assert!(json.get("unavailable_sensors").is_none());
        // DEC-194: active_profile_* omitted when no profile is active (additive).
        assert!(json.get("active_profile_id").is_none());
        assert!(json.get("active_profile_name").is_none());
        // `CTRL-d`: has_active_profile is always serialised, including when
        // false — its presence is what marks a daemon that reports the state at
        // all, exactly as for verify_active below. Asserted as a relationship
        // against the id, so a hardcoded literal would not satisfy it.
        assert_eq!(
            json["has_active_profile"],
            serde_json::Value::Bool(json.get("active_profile_id").is_some())
        );
        // DEC-206: readiness rollup omitted when None (old daemon / pre-startup).
        assert!(json.get("readiness").is_none());
    }

    #[test]
    fn status_response_serializes_readiness_rollup_when_present() {
        // DEC-206: the cached rollup rides /status (and /poll) for the Dashboard
        // health chip. Present ⇒ overall + counts + top_* serialize.
        use crate::hwmon::readiness::{ReadinessRollup, ReadinessSeverity};
        let resp = StatusResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            overall_status: "ok".into(),
            subsystems: Vec::new(),
            uptime_seconds: Some(1),
            thermal_state: "normal".into(),
            overrides: Vec::new(),
            fan_identify: Vec::new(),
            unavailable_sensors: Vec::new(),
            skipped_controls: Vec::new(),
            control_outputs: Vec::new(),
            runtime_config_degraded: None,
            validation_session: None,
            active_profile_id: None,
            active_profile_name: None,
            has_active_profile: false,
            readiness: Some(ReadinessRollup {
                overall: ReadinessSeverity::Warning,
                critical: 0,
                warning: 2,
                info: 1,
                top_summary: Some("No motherboard PWM fan controls detected".into()),
                top_code: Some("no_pwm_controls".into()),
            }),
            verify_active: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["readiness"]["overall"], "warning");
        assert_eq!(json["readiness"]["warning"], 2);
        assert_eq!(json["readiness"]["info"], 1);
        assert_eq!(json["readiness"]["top_code"], "no_pwm_controls");
        assert_eq!(
            json["readiness"]["top_summary"],
            "No motherboard PWM fan controls detected"
        );
    }

    /// 273-i: additive means additive. An older client, and every healthy
    /// machine, must see the exact wire shape they saw before this field
    /// existed — so the key is absent, not `[]`.
    #[test]
    fn status_response_omits_skipped_controls_when_empty() {
        let resp = StatusResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            overall_status: "ok".into(),
            subsystems: Vec::new(),
            uptime_seconds: Some(1),
            thermal_state: "normal".into(),
            overrides: Vec::new(),
            fan_identify: Vec::new(),
            unavailable_sensors: Vec::new(),
            skipped_controls: Vec::new(),
            control_outputs: Vec::new(),
            runtime_config_degraded: None,
            validation_session: None,
            active_profile_id: None,
            active_profile_name: None,
            has_active_profile: false,
            readiness: None,
            verify_active: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json.get("skipped_controls").is_none(),
            "an empty list must not appear on the wire at all: {json}"
        );
    }

    /// 273-i: and when something IS skipped, every field the GUI needs is there
    /// — including the stable `reason` token it branches on to render wording.
    #[test]
    fn status_response_serializes_skipped_controls_when_present() {
        let resp = StatusResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            overall_status: "ok".into(),
            subsystems: Vec::new(),
            uptime_seconds: Some(1),
            thermal_state: "normal".into(),
            overrides: Vec::new(),
            fan_identify: Vec::new(),
            unavailable_sensors: Vec::new(),
            skipped_controls: vec![SkippedControlEntry {
                control_id: "ctl-front".into(),
                control_name: "Front intake".into(),
                reason: "mix_unresolvable".into(),
                skipped_for_ms: 9000,
            }],
            control_outputs: Vec::new(),
            runtime_config_degraded: None,
            validation_session: None,
            active_profile_id: None,
            active_profile_name: None,
            has_active_profile: false,
            readiness: None,
            verify_active: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["skipped_controls"][0]["control_id"], "ctl-front");
        assert_eq!(json["skipped_controls"][0]["control_name"], "Front intake");
        assert_eq!(json["skipped_controls"][0]["reason"], "mix_unresolvable");
        assert_eq!(json["skipped_controls"][0]["skipped_for_ms"], 9000);
    }

    #[test]
    fn status_response_serializes_unavailable_sensors_when_present() {
        // DEC-193: a quarantined sensor surfaces on /status (and /poll via the
        // embedded status) with its cause and how long it has been unreadable.
        let resp = StatusResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            overall_status: "ok".into(),
            subsystems: Vec::new(),
            uptime_seconds: Some(1),
            thermal_state: "normal".into(),
            overrides: Vec::new(),
            fan_identify: Vec::new(),
            unavailable_sensors: vec![UnavailableSensorEntry {
                id: "hwmon:ath12k_hwmon:phy0:temp1".into(),
                label: "temp1".into(),
                reason: "read error: Network is down (os error 100)".into(),
                unavailable_for_ms: 4200,
            }],
            skipped_controls: Vec::new(),
            control_outputs: Vec::new(),
            runtime_config_degraded: None,
            validation_session: None,
            active_profile_id: None,
            active_profile_name: None,
            has_active_profile: false,
            readiness: None,
            verify_active: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json["unavailable_sensors"][0]["id"],
            "hwmon:ath12k_hwmon:phy0:temp1"
        );
        assert_eq!(json["unavailable_sensors"][0]["label"], "temp1");
        assert!(json["unavailable_sensors"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("Network is down"));
        assert_eq!(json["unavailable_sensors"][0]["unavailable_for_ms"], 4200);
    }

    #[test]
    fn sensor_entry_schema() {
        let entry = SensorEntry {
            id: "hwmon:k10temp:0000:00:18.3:Tctl".into(),
            kind: "cpu_temp".into(),
            label: "Tctl".into(),
            value_c: 55.0,
            source: "hwmon".into(),
            age_ms: 123,
            rate_c_per_s: Some(0.5),
            session_min_c: Some(32.0),
            session_max_c: Some(78.5),
            chip_name: "k10temp".into(),
            temp_type: None,
            thresholds: None,
            control_eligible: true,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["id"], "hwmon:k10temp:0000:00:18.3:Tctl");
        assert_eq!(json["kind"], "cpu_temp");
        assert_eq!(json["value_c"], 55.0);
        assert_eq!(json["chip_name"], "k10temp");
        // temp_type absent when None
        assert!(json.get("temp_type").is_none());
        // DEC-117: thresholds omitted entirely when None
        assert!(json.get("thresholds").is_none());
        // DEC-193: control_eligible is always serialized (a real sensor is true).
        assert_eq!(json["control_eligible"], true);
    }

    #[test]
    fn sensor_entry_with_temp_type() {
        let entry = SensorEntry {
            id: "hwmon:nct6683:nodev:AMD TSI Addr 98h".into(),
            kind: "cpu_temp".into(),
            label: "AMD TSI Addr 98h".into(),
            value_c: 48.0,
            source: "hwmon".into(),
            age_ms: 50,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "nct6683".into(),
            temp_type: Some(5),
            thresholds: None,
            control_eligible: true,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["chip_name"], "nct6683");
        assert_eq!(json["temp_type"], 5);
    }

    #[test]
    fn sensor_entry_serializes_thresholds_when_present() {
        // DEC-117: when the daemon reads at least one threshold attribute,
        // the JSON includes a `thresholds` object holding ONLY the
        // attributes that were actually readable (others omitted).
        let entry = SensorEntry {
            id: "hwmon:amdgpu:0000:03:00.0:edge".into(),
            kind: "gpu_temp".into(),
            label: "edge".into(),
            value_c: 42.0,
            source: "hwmon".into(),
            age_ms: 12,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: "amdgpu".into(),
            temp_type: None,
            thresholds: Some(SensorThresholdsResponse {
                max_c: None,
                min_c: None,
                crit_c: Some(110.0),
                crit_hyst_c: Some(100.0),
                emergency_c: Some(115.0),
                emergency_hyst_c: None,
                lcrit_c: None,
                offset_c: None,
                alarm: None,
                max_alarm: None,
                crit_alarm: Some(false),
                fault: None,
            }),
            control_eligible: true,
        };
        let json = serde_json::to_value(&entry).unwrap();
        let thresholds = &json["thresholds"];
        assert_eq!(thresholds["crit_c"], 110.0);
        assert_eq!(thresholds["crit_hyst_c"], 100.0);
        assert_eq!(thresholds["emergency_c"], 115.0);
        assert_eq!(thresholds["crit_alarm"], false);
        // Unset fields are omitted, not null.
        assert!(thresholds.get("max_c").is_none());
        assert!(thresholds.get("min_c").is_none());
        assert!(thresholds.get("alarm").is_none());
        assert!(thresholds.get("fault").is_none());
    }

    #[test]
    fn hardware_readiness_response_serializes_combined_snapshot() {
        // DEC-207: the combined endpoint's DTO carries the rollup + readiness items
        // + the Super-I/O report + freshness/generation, all additive. Nested
        // skip_serializing_if is honored (rollup.top_* when None; empty superio
        // vecs), and `api_version` stays 1.
        use crate::hwmon::readiness::{ReadinessRollup, ReadinessSeverity};
        let resp = HardwareReadinessResponse {
            api_version: API_VERSION,
            rollup: ReadinessRollup {
                overall: ReadinessSeverity::Ok,
                critical: 0,
                warning: 0,
                info: 0,
                top_summary: None,
                top_code: None,
            },
            overall: ReadinessSeverity::Ok,
            items: Vec::new(),
            superio: SuperIoResponse {
                api_version: API_VERSION,
                arch_supported: true,
                chips: Vec::new(),
                acpi_conflict_drivers: Vec::new(),
                notes: Vec::new(),
                port_probe_available: false,
                port_probe_reason: "flag off".into(),
            },
            scanned_age_ms: 42,
            generation: 7,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["overall"], "ok");
        assert_eq!(json["rollup"]["overall"], "ok");
        // An all-ok rollup omits top_* (skip_serializing_if).
        assert!(json["rollup"].get("top_summary").is_none());
        assert!(json["rollup"].get("top_code").is_none());
        assert!(json["items"].as_array().unwrap().is_empty());
        // The nested SuperIoResponse: empty vecs omitted; chips is an empty array.
        assert_eq!(json["superio"]["arch_supported"], true);
        assert!(json["superio"].get("acpi_conflict_drivers").is_none());
        assert!(json["superio"].get("notes").is_none());
        assert_eq!(json["superio"]["chips"].as_array().unwrap().len(), 0);
        assert_eq!(json["scanned_age_ms"], 42);
        assert_eq!(json["generation"], 7);
    }

    #[test]
    fn capabilities_response_schema() {
        let resp = CapabilitiesResponse {
            api_version: API_VERSION,
            daemon_version: "0.1.0".into(),
            ipc_transport: "uds/http",
            devices: DeviceCapabilities {
                openfan: OpenfanCapability {
                    present: true,
                    channels: 10,
                    rpm_support: true,
                    write_support: true,
                },
                hwmon: HwmonCapability {
                    present: true,
                    pwm_header_count: 3,
                    write_support: true,
                },
                amd_gpu: AmdGpuCapability {
                    present: true,
                    model_name: Some("RX 9070 XT".into()),
                    display_label: "9070XT".into(),
                    pci_id: Some("0000:2d:00.0".into()),
                    pci_bdf: Some("0000:2d:00.0".into()),
                    pci_device_id: Some(0x7550),
                    pci_revision: Some(0xC0),
                    fan_control_method: "pmfw_curve".into(),
                    pmfw_supported: true,
                    fan_rpm_available: true,
                    fan_write_supported: true,
                    is_discrete: true,
                    overdrive_enabled: true,
                    gpu_zero_rpm_available: true,
                    kernel_warnings: Vec::new(),
                },
                intel_gpu: IntelGpuCapability {
                    present: false,
                    model_name: None,
                    display_label: "Intel D-GPU".into(),
                    pci_id: None,
                    pci_bdf: None,
                    pci_device_id: None,
                    driver: None,
                    fan_control_method: "none".into(),
                    fan_rpm_available: false,
                    is_discrete: false,
                },
                nvidia_gpu: NvidiaGpuCapability {
                    present: true,
                    model_name: Some("NVIDIA GeForce RTX 4080".into()),
                    display_label: "NVIDIA GeForce RTX 4080".into(),
                    pci_id: Some("0000:03:00.0".into()),
                    pci_bdf: Some("0000:03:00.0".into()),
                    driver: Some("nvidia".into()),
                    driver_version: Some("565.77".into()),
                    fan_control_method: "read_only".into(),
                    fan_rpm_available: false,
                    is_discrete: true,
                },
                aio_hwmon: AioHwmonCapability::unsupported(),
                aio_usb: UnsupportedCapability {
                    present: false,
                    status: "unsupported",
                },
            },
            features: FeatureFlags {
                openfan_write_supported: true,
                hwmon_write_supported: true,
            },
            limits: Limits {
                pwm_percent_min: 0,
                pwm_percent_max: 100,
                openfan_stop_timeout_s: 8,
            },
            control: ControlCapability {
                profile_storage: true,
                curve_evaluation: true,
                manual_override: false,
                fan_identify: false,
                autonomous_control: false,
                min_supported_gui: String::new(),
                gpu_fan_verify: false,
                hardware_readiness: false,
                superio_port_probe: false,
                preferred_sensors: false,
                daemon_config_report: false,
                openfan_rescan: false,
                profile_search_dir_remove: false,
                header_roles: false,
                pwm_characterization: true,
                control_path_discovery: true,
                diagnostic_preflight: true,
                cooling_devices: false,
                validation_sessions: false,
                ..Default::default()
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["ipc_transport"], "uds/http");
        // Control-execution capability block (DEC-159/160).
        assert_eq!(json["control"]["profile_storage"], true);
        assert_eq!(json["control"]["curve_evaluation"], true);
        assert_eq!(json["control"]["manual_override"], false);
        assert_eq!(json["devices"]["openfan"]["present"], true);
        assert_eq!(json["devices"]["openfan"]["channels"], 10);
        assert_eq!(json["devices"]["hwmon"]["pwm_header_count"], 3);
        // DEC-170: the lease capability surface is gone — neither the per-header
        // flag nor the feature flag is emitted any more.
        assert!(json["devices"]["hwmon"].get("lease_required").is_none());
        assert!(json["features"]
            .get("lease_required_for_hwmon_writes")
            .is_none());
        // M11: both pci_id (legacy) and pci_bdf (canonical) must be emitted
        // with the same BDF string so clients on either name keep working.
        assert_eq!(json["devices"]["amd_gpu"]["pci_id"], "0000:2d:00.0");
        assert_eq!(json["devices"]["amd_gpu"]["pci_bdf"], "0000:2d:00.0");
        // NVIDIA capability (DEC-204): additive, read-only (no fan_write_supported).
        assert_eq!(json["devices"]["nvidia_gpu"]["present"], true);
        assert_eq!(
            json["devices"]["nvidia_gpu"]["display_label"],
            "NVIDIA GeForce RTX 4080"
        );
        // `driver` is the kernel module name ("nvidia"), not the NVML library.
        assert_eq!(json["devices"]["nvidia_gpu"]["driver"], "nvidia");
        assert_eq!(json["devices"]["nvidia_gpu"]["driver_version"], "565.77");
        assert_eq!(
            json["devices"]["nvidia_gpu"]["fan_control_method"],
            "read_only"
        );
        assert!(json["devices"]["nvidia_gpu"]
            .get("fan_write_supported")
            .is_none());
        // AIO Phase 1 (DEC-156): aio_hwmon is the dynamic capability — the
        // back-compat present+status plus pump_writable/coolant_available.
        assert_eq!(json["devices"]["aio_hwmon"]["present"], false);
        assert_eq!(json["devices"]["aio_hwmon"]["status"], "unsupported");
        assert_eq!(json["devices"]["aio_hwmon"]["pump_writable"], false);
        assert_eq!(json["devices"]["aio_hwmon"]["coolant_available"], false);
        // USB-only coolers remain out of scope.
        assert_eq!(json["devices"]["aio_usb"]["status"], "unsupported");
    }

    #[test]
    fn aio_hwmon_capability_from_discovery_status() {
        // Nothing detected → unsupported.
        let none = AioHwmonCapability::from_discovery(0, 0, false);
        assert!(!none.present);
        assert_eq!(none.status, "unsupported");

        // Coolant sensing only, no writable AIO header → monitor_only.
        let mon = AioHwmonCapability::from_discovery(0, 0, true);
        assert!(mon.present);
        assert!(!mon.pump_writable);
        assert!(mon.coolant_available);
        assert_eq!(mon.status, "monitor_only");

        // AIO header present but none writable → monitor_only (honest).
        let ro = AioHwmonCapability::from_discovery(1, 0, true);
        assert_eq!(ro.status, "monitor_only");
        assert!(!ro.pump_writable);

        // A writable AIO pump/fan header → supported.
        let sup = AioHwmonCapability::from_discovery(2, 2, true);
        assert!(sup.present);
        assert!(sup.pump_writable);
        assert_eq!(sup.status, "supported");
    }

    #[test]
    fn gpu_capability_absent_gpu_omits_both_pci_fields() {
        // M11: when no GPU is present, both pci_id and pci_bdf should be
        // absent from the JSON (skip_serializing_if = is_none).
        let cap = AmdGpuCapability {
            present: false,
            model_name: None,
            display_label: "AMD D-GPU".into(),
            pci_id: None,
            pci_bdf: None,
            pci_device_id: None,
            pci_revision: None,
            fan_control_method: "none".into(),
            pmfw_supported: false,
            fan_rpm_available: false,
            fan_write_supported: false,
            is_discrete: false,
            overdrive_enabled: false,
            gpu_zero_rpm_available: false,
            kernel_warnings: Vec::new(),
        };
        let json = serde_json::to_value(&cap).unwrap();
        assert!(json.get("pci_id").is_none());
        assert!(json.get("pci_bdf").is_none());
        // DEC-098: kernel_warnings is omitted when empty so older clients
        // without the field don't see an unexpected null/array.
        assert!(json.get("kernel_warnings").is_none());
    }

    #[test]
    fn gpu_diagnostics_emits_both_pci_names() {
        // M11: GpuDiagnostics emits both pci_bdf (canonical) and pci_id
        // (alias) with identical BDF strings during the transition.
        let diag = GpuDiagnostics {
            pci_bdf: "0000:03:00.0".into(),
            pci_id: "0000:03:00.0".into(),
            pci_device_id: 0x7550,
            pci_revision: 0xC0,
            model_name: Some("RX 9070 XT".into()),
            fan_control_method: "pmfw_curve".into(),
            overdrive_enabled: true,
            ppfeaturemask: Some("0x4000".into()),
            ppfeaturemask_bit14_set: true,
            zero_rpm_available: true,
            fan_speed_min_pct: Some(15),
            fan_speed_max_pct: Some(100),
            fan_minimum_pwm: None,
            amdgpu_driver_bound: true,
            kernel_warnings: Vec::new(),
        };
        let json = serde_json::to_value(&diag).unwrap();
        assert_eq!(json["pci_bdf"], "0000:03:00.0");
        assert_eq!(json["pci_id"], "0000:03:00.0");
        // DEC-119: OD_RANGE bounds are surfaced; the unparsed fan_minimum_pwm
        // and empty advisory list are omitted from the wire so older GUIs
        // don't see unexpected nulls.
        assert_eq!(json["fan_speed_min_pct"], 15);
        assert_eq!(json["amdgpu_driver_bound"], true);
        assert!(json.get("fan_minimum_pwm").is_none());
        assert!(json.get("kernel_warnings").is_none());
    }

    #[test]
    fn fan_entry_optional_fields() {
        let entry = FanEntry {
            id: "openfan:ch00".into(),
            source: "openfan".into(),
            rpm: Some(1200),
            last_commanded_pwm: None,
            pwm_readback_pct: None,
            pwm_commanded_pct: None,
            duty_pct: None,
            age_ms: 50,
            stall_detected: None,
            fan_alarm: None,
            pwm_enable_mode: None,
            duty_corrections: None,
            duty_not_holding: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["rpm"], 1200);
        // last_commanded_pwm + duty_pct absent when None — old GUIs see no new nulls.
        assert!(json.get("last_commanded_pwm").is_none());
        assert!(json.get("duty_pct").is_none());

        // A read-only NVIDIA fan surfaces its measured duty % (DEC-204).
        let nvidia = FanEntry {
            id: "nvidia_gpu:0000:03:00.0".into(),
            source: "nvidia_gpu".into(),
            rpm: None,
            last_commanded_pwm: None,
            pwm_readback_pct: None,
            pwm_commanded_pct: None,
            duty_pct: Some(47),
            age_ms: 10,
            stall_detected: None,
            fan_alarm: None,
            pwm_enable_mode: None,
            duty_corrections: None,
            duty_not_holding: None,
        };
        let json = serde_json::to_value(&nvidia).unwrap();
        assert_eq!(json["duty_pct"], 47);
        assert_eq!(json["source"], "nvidia_gpu");
        // Read-only NVIDIA fan: never a commanded PWM on the wire.
        assert!(json.get("last_commanded_pwm").is_none());
    }

    #[test]
    fn openfan_rescan_response_shape_is_pinned_on_both_arms() {
        // DEC-266. The adopted arm cannot be reached by any test without real
        // serial hardware, so its field names were asserted nowhere and could
        // drift away from the GUI's reader and `docs/08` unnoticed. Pinning the
        // serialisation is the part that does not need a device.
        let adopted = OpenFanRescanResponse {
            api_version: API_VERSION,
            adopted: true,
            already_connected: false,
            port: Some("/dev/ttyACM0".into()),
            message: "OpenFanController adopted on /dev/ttyACM0".into(),
        };
        let json = serde_json::to_value(&adopted).unwrap();
        assert_eq!(json["api_version"], API_VERSION);
        assert_eq!(json["adopted"], true);
        assert_eq!(json["already_connected"], false);
        assert_eq!(json["port"], "/dev/ttyACM0");

        // Already-connected arm: `port` is absent, not null — an older GUI must
        // not start seeing a new null key.
        let existing = OpenFanRescanResponse {
            api_version: API_VERSION,
            adopted: false,
            already_connected: true,
            port: None,
            message: "an OpenFanController is already connected".into(),
        };
        let json = serde_json::to_value(&existing).unwrap();
        assert_eq!(json["api_version"], API_VERSION);
        assert_eq!(json["adopted"], false);
        assert!(
            json.get("port").is_none(),
            "port must be omitted when nothing was adopted"
        );
    }

    /// **`WIRE-b` — the CALL SITE, not the helper.** `resolve_policy_floor` has
    /// its own unit tests; testing it alone would prove only that the rule was
    /// extracted, never that `/hwmon/headers` applies it. This asserts what the
    /// entry actually carries for the shape measured on an X870E AORUS MASTER: a
    /// radiator fan belonging to an AIO cooling device, which resolves that
    /// *device's* policy (`generic_pump` by default) while
    /// `header_is_pump_protected` is false for it, because membership is not a
    /// term in that union.
    ///
    /// The assertion is a RELATIONSHIP, not a literal: a header the daemon will
    /// stop on request is exactly a header that must carry no floor. Both
    /// branches are present, or a predicate stuck at one value would pass.
    #[test]
    fn a_cooling_device_member_carries_a_floor_only_when_it_is_pump_protected() {
        let device = crate::hwmon::cooling_device::CoolingDeviceConfig {
            id: "aio0".into(),
            pump_member: Some("hwmon:it8696:it87.2624:pwm2:PUMP".into()),
            radiator_members: vec!["hwmon:it8696:it87.2624:pwm1".into()],
            ..Default::default()
        };
        assert_eq!(
            device.resolved_policy().id,
            "generic_pump",
            "precondition: a device with no explicit policy must resolve the pump \
             policy, or this test is not exercising the shape it exists for"
        );

        let radiator = crate::hwmon::pwm_discovery::PwmHeaderDescriptor {
            id: "hwmon:it8696:it87.2624:pwm1".into(),
            label: "pwm1".into(),
            ..Default::default()
        };
        let entry = PwmHeaderEntry::from_descriptor(&radiator, None, false, Some(&device));
        assert_eq!(
            entry.stop_permitted,
            Some(true),
            "precondition: the radiator fan must be stoppable, or the pairing below \
             is not the case this test was written for"
        );
        assert_eq!(
            entry.effective_min_pwm_pct,
            Some(0),
            "a header the daemon will drive to 0 on request must not also advertise \
             a safety floor: no enforcement site applies one to it"
        );

        // The opposite branch — the pump member of the very same device.
        let pump = crate::hwmon::pwm_discovery::PwmHeaderDescriptor {
            id: "hwmon:it8696:it87.2624:pwm2:PUMP".into(),
            label: "PUMP".into(),
            ..Default::default()
        };
        let entry = PwmHeaderEntry::from_descriptor(&pump, None, true, Some(&device));
        assert_eq!(entry.stop_permitted, Some(false));
        assert_eq!(
            entry.effective_min_pwm_pct,
            Some(crate::profile::HARD_PUMP_CPU_FLOOR_PCT.round() as u8),
            "the pump member of the same device must keep the enforced floor"
        );
    }
}

// ── DEC-243: daemon configuration surface ────────────────────────────────
// `GET /config` exists so the GUI can *read* daemon configuration. Before it,
// `/capabilities` carried devices/features/limits/control and nothing about
// configuration at all, and the two writable knobs were write-only: the GUI kept
// a local mirror and pushed it on save, so a fresh GUI against a daemon set to
// 10 s displayed 0 s. The value was a guess, not a reading.

/// One configuration key as the daemon sees it.
///
/// `value` is what the *files* say (admin config plus the runtime overlay) —
/// i.e. what this key would be after a restart. `running_value` is what this
/// process actually started with. They differ exactly when a write has been
/// persisted but not yet applied, which is what `restart_pending` reports; the
/// GUI must not infer that state by remembering what it posted.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigKey {
    /// Dotted path, e.g. `startup.delay_secs`.
    pub key: String,
    /// Effective on-disk value, JSON-typed (string, integer, boolean, or array).
    pub value: serde_json::Value,
    /// Value this process is actually running with.
    ///
    /// **Always emitted, never omitted** — deliberately. It was previously
    /// skipped when equal to `value`, with clients told to read "absent" as
    /// "same". That protocol is unrepresentable for a nullable key: `serial.port`
    /// is `Option<String>`, so a genuine null running value serialises as
    /// `"running_value": null`, which a client cannot distinguish from the field
    /// being skipped — and a client applying the absent-means-same rule then
    /// reports the *file's* port as the one in use. Always sending it makes
    /// `null` mean exactly one thing: not set.
    pub running_value: serde_json::Value,
    /// Which layer supplied `value`: `runtime`, `admin`, or `default`.
    pub source: String,
    /// Whether a `POST /config/*` route exists for this key.
    pub mutable: bool,
    /// Whether changing it only takes effect when the daemon restarts.
    pub requires_restart: bool,
    /// True when `value` and `running_value` disagree — a restart is owed.
    pub restart_pending: bool,
    /// Present when the key needs more than a config write to work — currently
    /// the two `[detection]` opt-ins, each of which also needs a root-installed
    /// systemd drop-in. Setting the flag alone does not enable the feature, and
    /// a client must not present it as if it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_privilege: Option<String>,
}

/// Response for `GET /config` (DEC-243).
#[derive(Debug, Clone, Serialize)]
pub struct ConfigResponse {
    pub api_version: u32,
    /// Admin-owned file. The daemon never writes it (ADR-002).
    pub admin_config_path: String,
    /// Daemon-owned overlay, written only by `POST /config/*` (ADR-002).
    pub runtime_config_path: String,
    /// True when any key has a persisted-but-unapplied value.
    pub restart_pending: bool,
    pub keys: Vec<ConfigKey>,
}
