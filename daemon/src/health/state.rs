//! Canonical state model for the daemon cache.
//!
//! All IPC responses and safety logic draw from these types.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::hwmon::types::{SensorKind, SensorThresholds};

/// Device/source label identifying where a reading originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceLabel {
    /// OpenFanController USB serial device.
    OpenFan,
    /// Kernel hwmon sysfs device (motherboard sensors/fans).
    Hwmon,
    /// AMD discrete GPU via amdgpu hwmon/PMFW.
    AmdGpu,
    /// Intel discrete GPU (Arc) via the `xe`/`i915` hwmon node. Read-only
    /// telemetry — no fan write path exists in the kernel (DEC-121).
    IntelGpu,
    /// NVIDIA discrete GPU via the `nouveau` hwmon node. Read-only telemetry;
    /// the writable nouveau `pwm1` is excluded from hwmon discovery (DEC-204).
    NvidiaGpu,
    /// AIO cooler exposed via hwmon (future).
    AioHwmon,
    /// AIO cooler exposed via USB/HID (future).
    AioUsb,
}

impl std::fmt::Display for DeviceLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenFan => write!(f, "openfan"),
            Self::Hwmon => write!(f, "hwmon"),
            Self::AmdGpu => write!(f, "amd_gpu"),
            Self::IntelGpu => write!(f, "intel_gpu"),
            Self::NvidiaGpu => write!(f, "nvidia_gpu"),
            Self::AioHwmon => write!(f, "aio_hwmon"),
            Self::AioUsb => write!(f, "aio_usb"),
        }
    }
}

/// Cached state for a single OpenFanController fan channel.
#[derive(Debug, Clone)]
pub struct OpenFanState {
    /// Channel index (0–9).
    pub channel: u8,
    /// Last known RPM reading from the device.
    pub rpm: u16,
    /// Last PWM value commanded by the daemon (firmware doesn't report this).
    pub last_commanded_pwm: Option<u8>,
    /// When this channel's RPM was last READ.
    ///
    /// Published as the fan's `age_ms` by `build_fan_entries`, and reduced over
    /// by `poll_subsystem_health` to answer openfan data freshness. **A command
    /// must never refresh it** — `set_openfan_commanded_pwm` deliberately leaves
    /// it alone (OFS-i). This comment said only "when this reading was taken"
    /// from the initial import while the code refreshed it on writes too, which
    /// is how the contradiction survived: the field's own doc was right and
    /// nothing enforced it.
    pub updated_at: Instant,
    /// True after the first real RPM poll. Prevents false stall alerts
    /// when a PWM write creates the entry before any RPM data arrives.
    pub rpm_polled: bool,
}

/// Cached state for a motherboard hwmon fan header.
#[derive(Debug, Clone)]
pub struct HwmonFanState {
    /// Stable fan ID (e.g. `it8696:fan1`).
    pub id: String,
    /// RPM reading if available.
    pub rpm: Option<u16>,
    /// Last PWM value commanded by the daemon (if controlled).
    ///
    /// **This field has two producers and they mean different things** — the
    /// poll writes the sysfs readback here, while `HwmonPwmController::set_pwm`
    /// writes the value it commanded. They agree only while writes are landing.
    /// The divergence is recorded as `AIO5-a`; nothing depends on resolving it,
    /// because anything needing the true readback reads `pwm_readback_pct` and
    /// anything needing the true command reads the controller's own
    /// `last_commanded_pct`. Do not "tidy" this by pointing the poll at the new
    /// field only — that would change what `last_commanded_pwm` reports for an
    /// uncontrolled header, which is a wire-visible behaviour change.
    pub last_commanded_pwm: Option<u8>,
    /// The **hardware readback** of `pwmN`, as a percent (AIO-MB Phase 5).
    ///
    /// Unambiguous, unlike `last_commanded_pwm` above: only the poll ever writes
    /// it, and it is always what sysfs reported. `None` means "the daemon did
    /// not say" — never 0% — which is why it is carried forward across a write
    /// refresh rather than being reset to `None` by it.
    ///
    /// AIO-MB Phase 5 §3 requires "pump requested PWM" and "pump PWM readback"
    /// as separate columns, and §10 classifies a device-side override from
    /// `command low + readback low + RPM high`. Neither is expressible while the
    /// two axes share one field.
    pub pwm_readback_pct: Option<u8>,
    /// The **live** value of `pwmN_enable` (AIO-MB Phase 4).
    ///
    /// Sampled on the poll, not at discovery: the daemon writes
    /// `PWM_ENABLE_MANUAL` to this very attribute when it takes a header over,
    /// and the BIOS-reclaim watchdog shows it changing underneath at runtime,
    /// so a discovery-time snapshot reports the pre-takeover mode for the whole
    /// process lifetime. `None` means not known — see `alarm`.
    pub pwm_enable_mode: Option<u8>,
    /// The driver's `fanN_alarm` bit, when it exposes one (AIO-MB Phase 4).
    ///
    /// `None` means "not known right now", never "no alarm" — it is `None` both
    /// when the driver has no alarm attribute and briefly after a PWM write
    /// refreshes this entry without re-reading it. The next 1 Hz poll restores
    /// the real value.
    pub alarm: Option<bool>,
    /// When this reading was taken.
    pub updated_at: Instant,
}

/// Cached state for a temperature sensor.
#[derive(Debug, Clone)]
pub struct CachedSensorReading {
    /// Stable sensor ID.
    pub id: String,
    /// Sensor classification.
    pub kind: SensorKind,
    /// Human-friendly label.
    pub label: String,
    /// Temperature in degrees Celsius.
    pub value_c: f64,
    /// Source device label.
    pub source: DeviceLabel,
    /// When this reading was taken.
    pub updated_at: Instant,
    /// Temperature rate of change (°C/s), smoothed.
    pub rate_c_per_s: Option<f64>,
    /// Session minimum temperature since daemon start.
    pub session_min_c: Option<f64>,
    /// Session maximum temperature since daemon start.
    pub session_max_c: Option<f64>,
    /// Hwmon chip name (e.g. "k10temp", "nct6683", "it8696").
    pub chip_name: String,
    /// Sysfs `tempN_type` value if present (3=diode, 4=thermistor, 5=AMD TSI, 6=Intel PECI).
    pub temp_type: Option<u8>,
    /// Curated hwmon threshold attribute snapshot from discovery (DEC-117).
    pub thresholds: Option<SensorThresholds>,
}

/// Cached state for a discrete GPU fan (one per GPU — hardware exposes a
/// single aggregate fan).
///
/// Shared by AMD, Intel, and NVIDIA discrete GPUs, distinguished by the ID
/// prefix: `amd_gpu:` / `intel_gpu:` / `nvidia_gpu:<PCI_BDF>`. For read-only
/// sources (Intel DEC-121, NVIDIA DEC-204) `last_commanded_pct` is always
/// `None` — fan control is firmware-managed with no userspace write path.
#[derive(Debug, Clone)]
pub struct AmdGpuFanState {
    /// Stable fan ID: `<vendor>_gpu:<PCI_BDF>`.
    pub id: String,
    /// Current fan RPM if available (hwmon `fan1_input`, or NVML per driver R565+).
    pub rpm: Option<u16>,
    /// Last speed percentage commanded by the daemon via PMFW flat curve.
    /// Always `None` for read-only sources (Intel, NVIDIA).
    pub last_commanded_pct: Option<u8>,
    /// Firmware-**reported** current fan duty %, when the source exposes it
    /// (NVML `nvmlDeviceGetFanSpeed_v2`, DEC-204). A *measured* value distinct
    /// from `last_commanded_pct` — never conflate the two. `None` for AMD/Intel
    /// (which report RPM, not a duty readback). Per NVML this may exceed 100 (it
    /// is a % of the product's max-noise-tolerance fan speed).
    pub duty_pct: Option<u8>,
    /// When this reading was taken.
    pub updated_at: Instant,
}

/// A sensor that was discovered but currently fails every read (DEC-193).
///
/// Distinct from a *stale* reading (a sensor that read recently but not this
/// instant) and from a *vanished* descriptor (a device unbound from sysfs):
/// this is a descriptor that is still present but whose `temp*_input` read
/// fails persistently — e.g. an `ath12k` WiFi-radio temperature while the radio
/// is soft-blocked (`ENETDOWN`). The polling loop quarantines such sensors so
/// they stop spamming the journal, evicts their stale cached reading, and
/// surfaces them here for display-only (Diagnostics) visibility.
#[derive(Debug, Clone)]
pub struct UnavailableSensor {
    /// Stable sensor ID (matches what it would have on `/sensors` when readable).
    pub id: String,
    /// Human-friendly label from discovery.
    pub label: String,
    /// Human-readable cause — the hwmon read error (e.g.
    /// "read error: /sys/.../temp1_input: Network is down (os error 100)").
    pub reason: String,
    /// When the sensor was quarantined as unreadable.
    pub since: Instant,
}

/// Why a control could not be resolved.
///
/// These are stable wire tokens — [`SkipReason::as_token`] is what reaches
/// `/status`, and the GUI branches on it to render user-facing text. Renaming
/// one is a contract change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The control's `curve_id` names a curve the active profile does not have.
    CurveNotFound,
    /// The curve's sensor is absent from the map the engine was handed — either
    /// not discovered on this machine, or age-filtered out by `curve_eligible`.
    SensorUnavailable,
    /// A Mix that produced no value at all: no children, none resolvable, a
    /// `subtract` whose minuend is missing, a dependency cycle, or the depth
    /// backstop. A Mix with *some* children resolvable is NOT skipped — it runs
    /// on the survivors (DEC-272), so it never reaches here.
    MixUnresolvable,
    /// A Sync whose target is unset, is the control itself, or has not been
    /// computed this tick (target skipped, or a cycle).
    SyncUnresolvable,
}

impl SkipReason {
    /// The stable token published on `/status`.
    pub fn as_token(self) -> &'static str {
        match self {
            SkipReason::CurveNotFound => "curve_not_found",
            SkipReason::SensorUnavailable => "sensor_unavailable",
            SkipReason::MixUnresolvable => "mix_unresolvable",
            SkipReason::SyncUnresolvable => "sync_unresolvable",
        }
    }

    /// Operator-facing sentence for the journal. The API deliberately carries
    /// only [`SkipReason::as_token`]; user-facing wording belongs in the GUI,
    /// where it can be styled and worded for the person reading it.
    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::CurveNotFound => "its curve is not in the active profile",
            SkipReason::SensorUnavailable => "its sensor is not available",
            SkipReason::MixUnresolvable => "none of its combined inputs could be resolved",
            SkipReason::SyncUnresolvable => "the control it mirrors was not computed",
        }
    }
}

/// One control currently being skipped, for the `/status` surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedControl {
    pub control_id: String,
    pub control_name: String,
    pub reason: SkipReason,
    /// When the control was first listed (i.e. when the debounce was satisfied),
    /// not when it first skipped.
    pub since: Instant,
}

/// One control's applied output this tick, for the `/status` surface (277-k).
///
/// The value the engine actually applied, whatever produced it — a curve
/// evaluation or a live manual override. No `since` stamp and no debounce,
/// unlike [`SkippedControl`]: this is a *level*, republished every tick, and a
/// level that stopped being republished is meaningfully absent rather than
/// stale. A control the engine did not evaluate simply does not appear.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlOutput {
    pub control_id: String,
    /// Applied control-wide output, 0-100. Per-member duty can differ (a floor,
    /// or the DEC-119 GPU divergence) — those come from each fan's own
    /// `last_commanded_pwm`, never from this field.
    pub output_pct: f64,
}

/// Placeholder for AIO pump state (future implementation).
#[derive(Debug, Clone, Default)]
pub struct AioPumpState {
    /// Whether an AIO device is detected.
    pub detected: bool,
    /// Pump RPM if available.
    pub pump_rpm: Option<u16>,
    /// Coolant temperature in °C if available.
    pub coolant_temp_c: Option<f64>,
    /// Pump duty percentage if available.
    pub pump_duty_pct: Option<f64>,
    /// Last commanded pump percentage.
    pub last_commanded_pct: Option<f64>,
    /// When this was last updated.
    pub updated_at: Option<Instant>,
}

/// Per-subsystem update timestamp tracking.
#[derive(Debug, Clone, Default)]
pub struct SubsystemTimestamps {
    /// Last time OpenFanController data was updated.
    pub openfan: Option<Instant>,
    /// Last time hwmon sensor data was updated.
    pub hwmon: Option<Instant>,
    /// Last time AIO data was updated.
    pub aio: Option<Instant>,
    /// When a backend write first went outstanding and stayed that way (DEC-289).
    ///
    /// `None` while writes are landing normally. Distinct from the two engine
    /// stamps on purpose: bounding the backend joins means the loop keeps
    /// ticking through a wedged device, so both `engine_started` and
    /// `engine_completed` keep advancing and correctly report a live engine —
    /// while nothing is actually reaching the hardware. That is a third state,
    /// and it needs its own stamp rather than a distortion of the other two.
    pub engine_writes_stalled_since: Option<Instant>,
    /// Last time the profile engine *began* a tick (DEC-249, split DEC-259).
    ///
    /// Liveness for the sole PWM writer. The other fields track *data* freshness
    /// from the poll loops; this one tracks whether the engine task is still
    /// running at all. Nothing supervises that task — it is spawned once and
    /// only awaited during shutdown — so a panic inside a tick used to end fan
    /// control and the thermal leg silently, while `/status` kept
    /// answering 200 with a frozen `thermal_state`. Stamped by
    /// [`StateCache::record_engine_tick`] in the same write as that thermal
    /// state, so the two can never drift apart.
    ///
    /// DEC-259: this is the tick's START. On its own it could not tell a stopped
    /// engine from a slow one, and the two want opposite reports — see
    /// `engine_completed`.
    pub engine_started: Option<Instant>,
    /// Last time the profile engine *finished* a tick (DEC-259).
    ///
    /// Stamped by a drop guard, so it fires on every exit from the loop body —
    /// the `continue` paths and the shutdown `break` included. That is what
    /// makes "started but not completed" mean the tick is genuinely still
    /// running, rather than that it took an exit nobody instrumented.
    ///
    /// The pair exists because a single timestamp reported a *slow* tick as a
    /// *dead* engine. `force_all_with_floor` walks all ten OpenFan channels, each bounded
    /// by `serial.timeout_ms` (up to 1 s via the API), so a degraded-but-open
    /// link makes a legitimate tick take 5-10 s — and the old single stamp then
    /// read "not ticking — fan control and thermal safety are stalled" while the
    /// engine was in the middle of driving the thermal emergency. Exactly
    /// backwards, in the one state where the surface most needs to be right.
    pub engine_completed: Option<Instant>,
}

/// The complete daemon state snapshot.
#[derive(Debug, Clone)]
pub struct DaemonState {
    /// When this snapshot was created.
    pub snapshot_at: Instant,
    /// OpenFanController fan states, keyed by channel index.
    pub openfan_fans: HashMap<u8, OpenFanState>,
    /// Motherboard hwmon fan states, keyed by stable fan ID.
    pub hwmon_fans: HashMap<String, HwmonFanState>,
    /// AMD GPU fan states, keyed by `amd_gpu:<PCI_BDF>`.
    pub gpu_fans: HashMap<String, AmdGpuFanState>,
    /// Temperature sensor readings, keyed by stable sensor ID.
    pub sensors: HashMap<String, CachedSensorReading>,
    /// AIO pump state (placeholder).
    pub aio: AioPumpState,
    /// Per-subsystem last-update timestamps.
    pub subsystem_timestamps: SubsystemTimestamps,
    /// Thermal safety override state: "normal", "emergency", or "recovery".
    pub thermal_override_state: Option<String>,
    /// The emergency trip point the engine ACTED on at its last tick (DEC-308).
    /// Per-machine since the trigger is derived from the CPU's own reported
    /// design ceiling; `None` until the first tick, where readers fall back to
    /// `constants::THERMAL_EMERGENCY_TRIGGER_C`.
    pub thermal_emergency_trigger_c: Option<f64>,
    /// True while a hardware verify (hwmon or GPU) is in progress. Held for the
    /// verify's entire lifetime by the handler's RAII guard, so the engine pause
    /// outlasts a slow verify rather than expiring on a fixed timer. Single-
    /// flight: a second verify is rejected (409) while this is set (DEC-165).
    pub verify_in_progress: bool,
    /// Monotonic ownership token for the verify slot (DEC-296). Incremented on
    /// every successful claim; `end_verify` releases only if the caller still
    /// holds the current one, so a stranded claimant returning late cannot
    /// release its SUCCESSOR's pause.
    pub verify_epoch: u64,
    /// Generous deadman backing `verify_in_progress`: the RAII guard always
    /// clears the flag on drop/panic/cancel, but if it somehow does not, the
    /// pause self-clears after this instant so a verify can never strand control.
    pub verify_active_until: Option<Instant>,
    /// GPU fan ids the daemon has relinquished to firmware-auto via
    /// `POST /gpu/{id}/fan/reset` (DEC-165). The engine skips writing these so a
    /// reset is durable under an active profile; cleared on profile activation.
    pub relinquished_gpu_fans: HashSet<String>,
    /// Sensors discovered but currently unreadable (DEC-193). Maintained by the
    /// hwmon poll loop's `SensorFailureTracker`; surfaced on `/status` + `/poll`
    /// for display. Sensors listed here are evicted from `sensors` so a stale
    /// value is never served.
    pub unavailable_sensors: Vec<UnavailableSensor>,
    /// Controls the profile engine cannot resolve, so is not commanding (273-i).
    /// Maintained by the engine tick's `SkippedControlTracker`; surfaced on
    /// `/status` + `/poll` for display. Their fans hold their last commanded
    /// duty — a skip never lowers a fan (DEC-269).
    pub skipped_controls: Vec<SkippedControl>,
    /// Per-control applied output from the engine's last evaluating tick (277-k).
    /// Surfaced on `/status` + `/poll` so a live Controls card can answer "what
    /// are the fans doing?" — before this it had no live output feed at all and
    /// sat at "—" forever, with `set_output` reachable only in demo mode.
    /// Empty whenever no profile is evaluating, including for the duration of a
    /// thermal force (which drives fans directly, bypassing every control).
    pub control_outputs: Vec<ControlOutput>,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            snapshot_at: Instant::now(),
            openfan_fans: HashMap::new(),
            hwmon_fans: HashMap::new(),
            gpu_fans: HashMap::new(),
            sensors: HashMap::new(),
            aio: AioPumpState::default(),
            subsystem_timestamps: SubsystemTimestamps::default(),
            thermal_override_state: None,
            thermal_emergency_trigger_c: None,
            verify_in_progress: false,
            verify_epoch: 0,
            verify_active_until: None,
            relinquished_gpu_fans: HashSet::new(),
            unavailable_sensors: Vec::new(),
            skipped_controls: Vec::new(),
            control_outputs: Vec::new(),
        }
    }
}
