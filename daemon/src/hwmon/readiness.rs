//! Structured hardware-readiness model (Phase 3).
//!
//! A read-only, diagnose-and-guide model that turns the CPU/hwmon/PWM inventory
//! into a concise list of actionable [`ReadinessItem`]s for the GUI's first-run
//! guide and status surface. It never mutates the system — every item is a
//! statement of fact plus a recommended user action.
//!
//! Follows the `KernelWarning{id, severity, message}` shape (a stable machine
//! `code`, a serialized-lowercase severity, daemon-owned wording), extended with
//! the brief's per-item flags and a positive `Ok` severity so the list reads as
//! a green-and-amber checklist rather than a bag of problems.
//!
//! [`build_readiness`] is a pure function over primitive facts ([`ReadinessInputs`])
//! that the handler derives from the live cache + discovery — no sysfs access
//! here, so it is trivially testable.

use serde::Serialize;

/// Readiness severity, ordered informational-to-critical. `Ok` is a positive
/// "this works" state (the KernelWarning enum has no such level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadinessSeverity {
    Ok,
    Info,
    Warning,
    Critical,
}

impl ReadinessSeverity {
    /// Ordering rank for the overall rollup (higher = more severe).
    fn rank(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Info => 1,
            Self::Warning => 2,
            Self::Critical => 3,
        }
    }
}

/// One structured readiness finding (the brief's field set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessItem {
    /// Stable machine-readable key (e.g. `"cpu_sensor_missing"`) the GUI keys
    /// knowledge-base entries and acknowledgement state off.
    pub code: String,
    pub severity: ReadinessSeverity,
    /// Which subsystem this concerns: `cpu` | `hwmon` | `pwm` | `sensor`.
    pub component: String,
    /// One-line plain-English summary.
    pub summary: String,
    /// Longer technical detail.
    pub detail: String,
    /// Recommended user action (empty for purely-positive items).
    pub recommended_action: String,
    /// Whether the daemon could safely automate the fix (always false in
    /// Phase 3 — discovery is read-only and never auto-remediates).
    pub can_automate: bool,
    pub blocks_monitoring: bool,
    pub blocks_control: bool,
    pub affects_safety: bool,
    pub reboot_may_be_required: bool,
}

impl ReadinessItem {
    /// Base item: all flags false and no recommended action — chain the
    /// builder methods below to set the fields that differ.
    fn new(
        code: &str,
        severity: ReadinessSeverity,
        component: &str,
        summary: &str,
        detail: String,
    ) -> Self {
        Self {
            code: code.into(),
            severity,
            component: component.into(),
            summary: summary.into(),
            detail,
            recommended_action: String::new(),
            can_automate: false,
            blocks_monitoring: false,
            blocks_control: false,
            affects_safety: false,
            reboot_may_be_required: false,
        }
    }

    fn action(mut self, action: &str) -> Self {
        self.recommended_action = action.into();
        self
    }

    fn blocks_control(mut self) -> Self {
        self.blocks_control = true;
        self
    }

    fn affects_safety(mut self) -> Self {
        self.affects_safety = true;
        self
    }

    fn reboot(mut self) -> Self {
        self.reboot_may_be_required = true;
        self
    }
}

/// Read-only facts the readiness builder needs, derived by the handler from the
/// live cache + discovery. Primitive so the builder stays pure and testable.
#[derive(Debug, Clone, Default)]
pub struct ReadinessInputs {
    /// Number of sensors classified as any CPU sub-class.
    pub cpu_sensor_count: usize,
    /// The default-CPU recommendation: `None` = no CPU sensor at all;
    /// `Some(true)` = a high-confidence pick; `Some(false)` = a best-guess pick.
    pub default_cpu_confident: Option<bool>,
    /// Total discovered PWM headers.
    pub pwm_total: usize,
    /// PWM headers whose `pwmN` file is writable.
    pub pwm_writable: usize,
    /// Monitor-only fan tachometers (RPM inputs with no matching `pwmN`).
    pub monitor_only_fan_count: usize,
    /// Sensors quarantined as present-but-unreadable (DEC-193).
    pub unavailable_sensor_count: usize,
    /// Sensors that classified as `unknown_temp`.
    pub unknown_sensor_count: usize,
    /// Persisted preferred CPU sensor status (Phase 5): `None` = none selected;
    /// `Some(true)` = selected and present; `Some(false)` = selected but missing.
    pub selected_cpu_present: Option<bool>,
    /// Persisted preferred motherboard sensor status (same tri-state).
    pub selected_mb_present: Option<bool>,
}

/// Build the structured readiness list from the read-only inventory facts.
pub fn build_readiness(inp: &ReadinessInputs) -> Vec<ReadinessItem> {
    let mut items = Vec::new();

    // ── CPU temperature source (safety-relevant) ──
    if inp.cpu_sensor_count == 0 {
        items.push(
            ReadinessItem::new(
                "cpu_sensor_missing",
                ReadinessSeverity::Critical,
                "cpu",
                "No CPU temperature sensor detected",
                "No hwmon sensor was classified as a CPU temperature. The daemon's thermal \
                 safety cannot track CPU heat and will force a 40% fan floor after five cycles \
                 without a CPU sensor."
                    .into(),
            )
            .action(
                "Ensure the CPU temperature driver is loaded (coretemp on Intel, k10temp on AMD), \
                 or load the motherboard Super-I/O driver (e.g. it87 / nct6775).",
            )
            .affects_safety(),
        );
    } else {
        items.push(
            ReadinessItem::new(
                "cpu_sensor_present",
                ReadinessSeverity::Ok,
                "cpu",
                "CPU temperature source found",
                format!(
                    "{} CPU temperature sensor(s) detected and available for fan curves and \
                     thermal safety.",
                    inp.cpu_sensor_count
                ),
            )
            .affects_safety(),
        );
        if inp.default_cpu_confident == Some(false) {
            items.push(
                ReadinessItem::new(
                    "cpu_default_low_confidence",
                    ReadinessSeverity::Info,
                    "cpu",
                    "Default CPU sensor is a best guess",
                    "The recommended CPU temperature sensor was chosen heuristically (not \
                     high-confidence); it may not be the most representative sensor."
                        .into(),
                )
                .action("Review the recommended CPU sensor and pick a different one if needed."),
            );
        }
    }

    // ── PWM control availability ──
    if inp.pwm_total == 0 {
        items.push(
            ReadinessItem::new(
                "no_pwm_controls",
                ReadinessSeverity::Warning,
                "pwm",
                "No motherboard PWM fan controls detected",
                "No hwmon pwmN control was discovered, so motherboard-header fan control is \
                 unavailable (any OpenFan / GPU paths are separate)."
                    .into(),
            )
            .action(
                "Load the Super-I/O driver for your chip (e.g. it87 via DKMS, or nct6775). A \
                 reboot or module reload may be required.",
            )
            .blocks_control()
            .reboot(),
        );
    } else {
        items.push(ReadinessItem::new(
            "pwm_controls_present",
            ReadinessSeverity::Ok,
            "pwm",
            "Motherboard PWM fan controls detected",
            format!(
                "{} PWM header(s) discovered, {} writable.",
                inp.pwm_total, inp.pwm_writable
            ),
        ));
        if inp.pwm_writable < inp.pwm_total {
            items.push(
                ReadinessItem::new(
                    "pwm_read_only",
                    ReadinessSeverity::Warning,
                    "pwm",
                    "Some PWM headers are read-only",
                    format!(
                        "{} of {} PWM header(s) are not writable (the kernel exposes them \
                         read-only), so those fans cannot be controlled.",
                        inp.pwm_total - inp.pwm_writable,
                        inp.pwm_total
                    ),
                )
                .action(
                    "A read-only header usually means the driver supports monitoring but not \
                     control for that channel; an updated/different driver may help, or the \
                     channel is simply not controllable.",
                )
                .blocks_control(),
            );
        }
        if inp.pwm_writable > 0 {
            items.push(
                ReadinessItem::new(
                    "pwm_control_unverified",
                    ReadinessSeverity::Info,
                    "pwm",
                    "PWM control not yet verified",
                    "Writable PWM headers were found, but whether a write actually changes fan \
                     speed has not been verified on this hardware."
                        .into(),
                )
                .action(
                    "Run fan-control verification for each header to confirm it drives the fan.",
                ),
            );
        }
    }

    // ── Monitor-only fan tachometers ──
    if inp.monitor_only_fan_count > 0 {
        items.push(ReadinessItem::new(
            "monitor_only_fans_present",
            ReadinessSeverity::Info,
            "hwmon",
            "Monitor-only fan tachometers detected",
            format!(
                "{} fan RPM input(s) have no matching PWM control — their speed can be read but \
                 not set.",
                inp.monitor_only_fan_count
            ),
        ));
    }

    // ── Quarantined (present-but-unreadable) sensors (DEC-193) ──
    if inp.unavailable_sensor_count > 0 {
        items.push(
            ReadinessItem::new(
                "sensors_unavailable",
                ReadinessSeverity::Warning,
                "sensor",
                "Some sensors are present but unreadable",
                format!(
                    "{} discovered sensor(s) fail every read and have been quarantined (e.g. a \
                     WiFi-radio temperature while the radio is off).",
                    inp.unavailable_sensor_count
                ),
            )
            .action(
                "These are display-only and safe to ignore unless one you need is listed — see \
                 Diagnostics ▸ Sensors.",
            ),
        );
    }

    // ── Unclassified sensors ──
    if inp.unknown_sensor_count > 0 {
        items.push(ReadinessItem::new(
            "unknown_sensors_present",
            ReadinessSeverity::Info,
            "sensor",
            "Some temperature sensors could not be classified",
            format!(
                "{} sensor(s) are on unrecognised chips with no classifying label; they still \
                 work but are labelled \"unknown\".",
                inp.unknown_sensor_count
            ),
        ));
    }

    // ── Persisted user selections (Phase 5) — flag stale ones ──
    if inp.selected_cpu_present == Some(false) {
        items.push(
            ReadinessItem::new(
                "selected_cpu_sensor_missing",
                ReadinessSeverity::Warning,
                "cpu",
                "Your selected CPU sensor is no longer present",
                "The CPU temperature sensor you chose is not in the current sensor set — the \
                 hardware or its driver may have changed since you selected it. The daemon has \
                 fallen back to its automatic CPU-sensor pick."
                    .into(),
            )
            .action("Pick a new default CPU sensor."),
        );
    }
    if inp.selected_mb_present == Some(false) {
        items.push(
            ReadinessItem::new(
                "selected_mb_sensor_missing",
                ReadinessSeverity::Warning,
                "sensor",
                "Your selected motherboard sensor is no longer present",
                "The case/motherboard temperature sensor you chose is not in the current sensor \
                 set — the hardware or its driver may have changed since you selected it."
                    .into(),
            )
            .action("Pick a new motherboard sensor."),
        );
    }

    items
}

/// The overall rollup severity = the most severe item (`Ok` when empty).
pub fn overall_severity(items: &[ReadinessItem]) -> ReadinessSeverity {
    items
        .iter()
        .map(|i| i.severity)
        .max_by_key(|s| s.rank())
        .unwrap_or(ReadinessSeverity::Ok)
}

/// Compact readiness rollup mirrored onto `/status` + `/poll` (DEC-206) so the
/// GUI Dashboard can show a single health chip without fetching the expensive
/// `/inventory/readiness` list on the 1 Hz poll. Derived from the same
/// [`ReadinessItem`] list the full endpoint returns, cached in `AppState`, and
/// refreshed only on discovery-changing events (startup / rescan /
/// preferred-sensor / readiness GET) — never recomputed on the poll path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessRollup {
    /// Rollup severity = the most severe item (see [`overall_severity`]).
    pub overall: ReadinessSeverity,
    /// Item counts by severity. `Ok` (positive) items are not counted — the
    /// chip's "N to fix" is `critical + warning`.
    pub critical: usize,
    pub warning: usize,
    pub info: usize,
    /// The most-severe item's one-line summary + stable `code`, so the chip can
    /// name the single most-important next step and deep-link to it. Both
    /// omitted when `overall` is `Ok` (nothing to fix).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_code: Option<String>,
}

/// Derive the compact [`ReadinessRollup`] from the full item list. Pure.
///
/// `top_*` = the first item whose severity equals `overall`, skipping `Ok`
/// (matches the GUI view's stable "most severe first" sort), so the chip's
/// headline equals the merged tab's first actionable row. When `overall` is
/// `Ok` there is nothing to fix ⇒ `top_*` are `None`.
pub fn derive_rollup(overall: ReadinessSeverity, items: &[ReadinessItem]) -> ReadinessRollup {
    let mut critical = 0;
    let mut warning = 0;
    let mut info = 0;
    for it in items {
        match it.severity {
            ReadinessSeverity::Critical => critical += 1,
            ReadinessSeverity::Warning => warning += 1,
            ReadinessSeverity::Info => info += 1,
            ReadinessSeverity::Ok => {}
        }
    }
    let (top_summary, top_code) = if overall == ReadinessSeverity::Ok {
        (None, None)
    } else {
        items
            .iter()
            .find(|it| it.severity == overall)
            .map(|it| (Some(it.summary.clone()), Some(it.code.clone())))
            .unwrap_or((None, None))
    };
    ReadinessRollup {
        overall,
        critical,
        warning,
        info,
        top_summary,
        top_code,
    }
}

/// A daemon-owned hardware-assessment snapshot (DEC-207): the product of ONE
/// passive scan, shared by every readiness/Super-I/O consumer so the expensive
/// work (cache snapshot + `/sys` walk + `runtime.toml` read + Super-I/O detect)
/// runs once instead of three times. Read-only; produced off the 1 Hz poll path
/// and cached in `AppState`. `scanned_at` is a monotonic [`std::time::Instant`]
/// (so TTL math is clock-safe); the wire carries the relative `scanned_age_ms`.
#[derive(Debug, Clone)]
pub struct HardwareAssessment {
    /// Rollup severity == `rollup.overall` == `overall_severity(&items)`.
    pub overall: ReadinessSeverity,
    /// The full readiness list (base items + Super-I/O enrichment).
    pub items: Vec<ReadinessItem>,
    /// Compact rollup derived from `items` (mirrored onto `/status`+`/poll`).
    pub rollup: ReadinessRollup,
    /// The passive Super-I/O report the enrichment items were derived from.
    pub superio: crate::hwmon::superio::SuperIoReport,
    /// Monotonic scan id, assigned by the cache on store (0 before that).
    pub generation: u64,
    /// When this scan completed (monotonic clock).
    pub scanned_at: std::time::Instant,
}

impl HardwareAssessment {
    /// Compose an assessment from the base readiness items + the Super-I/O report,
    /// enforcing the `overall`/`rollup`/`items` invariant in one place: the
    /// Super-I/O enrichment is appended, then `overall` and the compact `rollup`
    /// are derived from the combined list. `generation` stays 0 until the cache
    /// assigns it on store; `scanned_at` is stamped now.
    pub fn from_parts(
        mut items: Vec<ReadinessItem>,
        superio: crate::hwmon::superio::SuperIoReport,
    ) -> Self {
        items.extend(superio_readiness_items(&superio));
        let overall = overall_severity(&items);
        let rollup = derive_rollup(overall, &items);
        Self {
            overall,
            items,
            rollup,
            superio,
            generation: 0,
            scanned_at: std::time::Instant::now(),
        }
    }
}

/// Map a passive Super-I/O detection report (DEC-202) into readiness items, so
/// board-specific "your chip has no driver loaded" guidance surfaces in the
/// existing readiness list alongside the generic `no_pwm_controls` item. Lives
/// here (not in the handler) because `ReadinessItem`'s builders are private to
/// this module. The handler appends the result and recomputes `overall`.
///
/// Two aggregate items at most (unique codes — the GUI keys off `code`):
/// `superio_driver_unloaded` when any detected chip is present-but-unbound with
/// a load recommendation, and `superio_acpi_conflict` when a Super-I/O driver's
/// I/O range collides with an ACPI OperationRegion.
pub fn superio_readiness_items(
    report: &crate::hwmon::superio::SuperIoReport,
) -> Vec<ReadinessItem> {
    let mut items = Vec::new();

    let unbound: Vec<String> = report
        .chips
        .iter()
        .filter(|c| c.recommendation.is_some())
        .map(|c| format!("{} → {}", c.chip_name, c.expected_module))
        .collect();
    if !unbound.is_empty() {
        // [HOST-d / DEC-327] This item fires for BOTH "no driver loaded" and
        // "driver loaded but it did not bind", and it used to describe only the
        // first — so on a machine whose driver is loaded it asserted the
        // opposite of the truth, directly beside a per-chip recommendation that
        // (since this change) correctly says the driver is already there.
        //
        // The `code` is deliberately NOT renamed: the GUI keys off it, so a
        // rename is a contract change. Only the human-readable half moves.
        let any_loaded = report
            .chips
            .iter()
            .any(|c| c.recommendation.is_some() && c.module_loaded);
        let (title, tail) = if any_loaded {
            (
                "Motherboard Super-I/O chip detected but its driver did not bind",
                "The driver is loaded; it did not attach to the chip. See the per-chip \
                 recommendation for what that means on this board — on some it is an ACPI \
                 resource conflict, and on others no available driver can reach the chip.",
            )
        } else {
            (
                "Motherboard Super-I/O chip detected without its driver loaded",
                "Loading the driver is what makes the motherboard fan headers and sensors \
                 appear.",
            )
        };
        items.push(
            ReadinessItem::new(
                "superio_driver_unloaded",
                ReadinessSeverity::Warning,
                "hwmon",
                title,
                format!(
                    "{} Super-I/O chip(s) were detected but no matching kernel driver is bound: \
                     {}. {tail}",
                    unbound.len(),
                    unbound.join(", ")
                ),
            )
            .action(if any_loaded {
                // [security-reviewer finding 4] The loaded branch deliberately
                // offers no copy-paste command, so promising one here would be
                // the same defect this change fixes, one field over — an
                // incomplete correction of exactly the kind `CLAUDE.md`
                // records as its own defect class.
                "Open Diagnostics ▸ Super-I/O and read the per-chip recommendation — it says \
                 whether anything can be done on this board, and on some there is nothing to \
                 configure."
            } else {
                "Open Diagnostics ▸ Super-I/O for the exact module and copy-paste command; a \
                 reboot or module reload may be needed."
            })
            .reboot(),
        );
    }

    if !report.acpi_conflict_drivers.is_empty() {
        items.push(
            ReadinessItem::new(
                "superio_acpi_conflict",
                ReadinessSeverity::Warning,
                "hwmon",
                "ACPI claims a Super-I/O chip's I/O ports",
                format!(
                    "ACPI firmware has reserved the I/O ports used by {}. Under \
                     acpi_enforce_resources=strict (the default) the driver may refuse to bind, so \
                     the chip's sensors/fans stay unavailable.",
                    report.acpi_conflict_drivers.join(", ")
                ),
            )
            .action(
                "See Diagnostics ▸ Super-I/O. Do not switch to acpi_enforce_resources=lax without \
                 understanding the driver/ACPI race it reintroduces.",
            ),
        );
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(items: &[ReadinessItem], code: &str) -> bool {
        items.iter().any(|i| i.code == code)
    }

    fn get<'a>(items: &'a [ReadinessItem], code: &str) -> &'a ReadinessItem {
        items.iter().find(|i| i.code == code).expect("item present")
    }

    #[test]
    fn no_cpu_sensor_is_critical_and_safety() {
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 0,
            default_cpu_confident: None,
            pwm_total: 2,
            pwm_writable: 2,
            ..Default::default()
        });
        let it = get(&items, "cpu_sensor_missing");
        assert_eq!(it.severity, ReadinessSeverity::Critical);
        assert!(it.affects_safety);
        assert!(!it.can_automate);
        assert!(!has(&items, "cpu_sensor_present"));
        assert_eq!(overall_severity(&items), ReadinessSeverity::Critical);
    }

    #[test]
    fn cpu_present_high_confidence_has_no_low_conf_note() {
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 1,
            default_cpu_confident: Some(true),
            pwm_total: 2,
            pwm_writable: 2,
            ..Default::default()
        });
        assert_eq!(
            get(&items, "cpu_sensor_present").severity,
            ReadinessSeverity::Ok
        );
        assert!(get(&items, "cpu_sensor_present").affects_safety);
        assert!(!has(&items, "cpu_default_low_confidence"));
    }

    #[test]
    fn cpu_low_confidence_adds_info_note() {
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 2,
            default_cpu_confident: Some(false),
            pwm_total: 1,
            pwm_writable: 1,
            ..Default::default()
        });
        assert_eq!(
            get(&items, "cpu_default_low_confidence").severity,
            ReadinessSeverity::Info
        );
    }

    #[test]
    fn no_pwm_warns_blocks_control_and_needs_reboot() {
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 1,
            default_cpu_confident: Some(true),
            pwm_total: 0,
            pwm_writable: 0,
            ..Default::default()
        });
        let it = get(&items, "no_pwm_controls");
        assert_eq!(it.severity, ReadinessSeverity::Warning);
        assert!(it.blocks_control);
        assert!(it.reboot_may_be_required);
        assert!(!has(&items, "pwm_control_unverified"));
    }

    #[test]
    fn all_writable_pwm_has_no_read_only_warning() {
        // B3: when every header is writable (pwm_writable == pwm_total) the
        // read-only warning must be ABSENT. Kills `pwm_writable < pwm_total` → `<=`,
        // which would emit `pwm_read_only` (with blocks_control) on healthy hardware.
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 1,
            default_cpu_confident: Some(true),
            pwm_total: 3,
            pwm_writable: 3,
            ..Default::default()
        });
        assert!(has(&items, "pwm_controls_present"));
        assert!(!has(&items, "pwm_read_only"));
        // At least one writable header → the unverified note IS present.
        assert!(has(&items, "pwm_control_unverified"));
    }

    #[test]
    fn some_read_only_pwm_warns_but_still_lists_writable() {
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 1,
            default_cpu_confident: Some(true),
            pwm_total: 3,
            pwm_writable: 1,
            ..Default::default()
        });
        assert!(has(&items, "pwm_controls_present"));
        assert_eq!(
            get(&items, "pwm_read_only").severity,
            ReadinessSeverity::Warning
        );
        assert!(get(&items, "pwm_read_only").blocks_control);
        // At least one writable header → the unverified note is present.
        assert!(has(&items, "pwm_control_unverified"));
    }

    #[test]
    fn all_read_only_pwm_has_no_unverified_note() {
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 1,
            default_cpu_confident: Some(true),
            pwm_total: 2,
            pwm_writable: 0,
            ..Default::default()
        });
        assert!(has(&items, "pwm_read_only"));
        assert!(!has(&items, "pwm_control_unverified"));
    }

    #[test]
    fn monitor_unavailable_and_unknown_surface_as_items() {
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 1,
            default_cpu_confident: Some(true),
            pwm_total: 1,
            pwm_writable: 1,
            monitor_only_fan_count: 2,
            unavailable_sensor_count: 1,
            unknown_sensor_count: 3,
            ..Default::default()
        });
        assert_eq!(
            get(&items, "monitor_only_fans_present").severity,
            ReadinessSeverity::Info
        );
        assert_eq!(
            get(&items, "sensors_unavailable").severity,
            ReadinessSeverity::Warning
        );
        assert_eq!(
            get(&items, "unknown_sensors_present").severity,
            ReadinessSeverity::Info
        );
    }

    #[test]
    fn overall_is_max_severity() {
        let mixed = vec![
            ReadinessItem::new("a", ReadinessSeverity::Ok, "x", "s", "d".into()),
            ReadinessItem::new("b", ReadinessSeverity::Warning, "x", "s", "d".into()),
            ReadinessItem::new("c", ReadinessSeverity::Info, "x", "s", "d".into()),
        ];
        assert_eq!(overall_severity(&mixed), ReadinessSeverity::Warning);
        assert_eq!(overall_severity(&[]), ReadinessSeverity::Ok);
        let crit = vec![ReadinessItem::new(
            "d",
            ReadinessSeverity::Critical,
            "x",
            "s",
            "d".into(),
        )];
        assert_eq!(overall_severity(&crit), ReadinessSeverity::Critical);
    }

    #[test]
    fn selected_sensor_missing_items_flag_stale_selections() {
        // Selected-but-missing → warning items; present / None → nothing.
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 1,
            default_cpu_confident: Some(true),
            pwm_total: 1,
            pwm_writable: 1,
            selected_cpu_present: Some(false),
            selected_mb_present: Some(false),
            ..Default::default()
        });
        assert_eq!(
            get(&items, "selected_cpu_sensor_missing").severity,
            ReadinessSeverity::Warning
        );
        assert_eq!(
            get(&items, "selected_mb_sensor_missing").severity,
            ReadinessSeverity::Warning
        );

        let present = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 1,
            default_cpu_confident: Some(true),
            pwm_total: 1,
            pwm_writable: 1,
            selected_cpu_present: Some(true),
            selected_mb_present: None,
            ..Default::default()
        });
        assert!(!has(&present, "selected_cpu_sensor_missing"));
        assert!(!has(&present, "selected_mb_sensor_missing"));
    }

    #[test]
    fn severity_serialises_lowercase() {
        assert_eq!(
            serde_json::to_value(ReadinessSeverity::Critical).unwrap(),
            serde_json::json!("critical")
        );
        assert_eq!(
            serde_json::to_value(ReadinessSeverity::Ok).unwrap(),
            serde_json::json!("ok")
        );
    }

    #[test]
    fn superio_items_emitted_for_unbound_chip_and_acpi_conflict() {
        use crate::hwmon::superio::{
            Evidence, SuperIoChip, SuperIoRecommendation, SuperIoReport, SuperIoVendor,
        };
        let report = SuperIoReport {
            arch_supported: true,
            chips: vec![SuperIoChip {
                chip_name: "it8688".into(),
                vendor: SuperIoVendor::Ite,
                evidence: vec![Evidence::DmiBoardTable],
                confidence: crate::hwmon::classify::Confidence::Medium,
                bound_driver: None,
                expected_module: "it87".into(),
                module_loaded: false,
                hwmon_present: false,
                recommendation: Some(SuperIoRecommendation {
                    module: "it87".into(),
                    in_mainline: false,
                    load_hint: "install it87-dkms-git".into(),
                    reason: "board lists it8688".into(),
                    risk_notes: vec![],
                }),
                caveats: vec![],
            }],
            acpi_conflict_drivers: vec!["it87".into()],
            notes: vec![],
        };
        let items = superio_readiness_items(&report);
        let codes: Vec<&str> = items.iter().map(|i| i.code.as_str()).collect();
        assert!(codes.contains(&"superio_driver_unloaded"));
        assert!(codes.contains(&"superio_acpi_conflict"));
        let unloaded = items
            .iter()
            .find(|i| i.code == "superio_driver_unloaded")
            .unwrap();
        assert!(unloaded.detail.contains("it8688 → it87"));
        assert!(unloaded.reboot_may_be_required);
        assert_eq!(unloaded.severity, ReadinessSeverity::Warning);
    }

    /// [HOST-d / DEC-327] The item fires for two different states and must not
    /// describe both as the first. Asserted as a RELATIONSHIP between the two
    /// renderings, not against a literal, and with both branches present — one
    /// branch alone passes with the condition stuck on either answer.
    #[test]
    fn the_unbound_item_says_did_not_bind_when_the_driver_is_actually_loaded() {
        use crate::hwmon::superio::{
            Evidence, SuperIoChip, SuperIoRecommendation, SuperIoReport, SuperIoVendor,
        };
        let report = |module_loaded: bool| SuperIoReport {
            arch_supported: true,
            chips: vec![SuperIoChip {
                chip_name: "it8696".into(),
                vendor: SuperIoVendor::Ite,
                evidence: vec![Evidence::DmiBoardTable],
                confidence: crate::hwmon::classify::Confidence::Medium,
                bound_driver: None,
                expected_module: "it87".into(),
                module_loaded,
                hwmon_present: false,
                recommendation: Some(SuperIoRecommendation {
                    module: "it87".into(),
                    in_mainline: false,
                    load_hint: "irrelevant here".into(),
                    reason: "board lists it8696".into(),
                    risk_notes: vec![],
                }),
                caveats: vec![],
            }],
            acpi_conflict_drivers: vec![],
            notes: vec![],
        };
        let item = |loaded: bool| {
            superio_readiness_items(&report(loaded))
                .into_iter()
                .find(|i| i.code == "superio_driver_unloaded")
                .expect("an unbound chip with a recommendation must emit the item")
        };
        let loaded = item(true);
        let not_loaded = item(false);

        // The state this host is actually in.
        assert!(
            loaded.summary.contains("did not bind"),
            "a loaded driver that failed to attach must not be described as \
             unloaded. got: {}",
            loaded.summary
        );
        assert!(
            !loaded.detail.contains("Loading the driver is what makes"),
            "...and must not be told that loading it is the remedy"
        );
        // The opposite branch, unchanged — deleting it would be a regression.
        assert!(not_loaded.summary.contains("without its driver loaded"));
        assert!(not_loaded
            .detail
            .contains("Loading the driver is what makes"));
        // The two must genuinely differ, or the condition is stuck.
        assert_ne!(loaded.summary, not_loaded.summary);
        // [security-reviewer finding 4] The action must not promise a
        // copy-paste command on the branch that deliberately offers none.
        assert!(
            !loaded.recommended_action.contains("copy-paste command"),
            "the loaded branch offers no command; promising one is the same \
             defect one field over. got: {}",
            loaded.recommended_action
        );
        assert!(
            not_loaded.recommended_action.contains("copy-paste command"),
            "...but the not-loaded branch genuinely has one and must keep it"
        );
        assert_ne!(loaded.recommended_action, not_loaded.recommended_action);
        // The GUI keys off `code`: renaming it is a contract change, so pin it.
        assert_eq!(loaded.code, not_loaded.code);
        assert_eq!(loaded.severity, not_loaded.severity);
    }

    #[test]
    fn superio_items_empty_when_nothing_unbound_and_no_conflict() {
        use crate::hwmon::superio::SuperIoReport;
        let report = SuperIoReport {
            arch_supported: true,
            chips: vec![],
            acpi_conflict_drivers: vec![],
            notes: vec![],
        };
        assert!(superio_readiness_items(&report).is_empty());
    }

    // ── ReadinessRollup / derive_rollup (DEC-206) ──

    #[test]
    fn derive_rollup_counts_and_top_from_readiness_list() {
        // No CPU sensor (critical) + read-only PWM (warning) + unknown (info).
        let items = build_readiness(&ReadinessInputs {
            cpu_sensor_count: 0,
            default_cpu_confident: None,
            pwm_total: 3,
            pwm_writable: 1,
            unknown_sensor_count: 2,
            ..Default::default()
        });
        let overall = overall_severity(&items);
        let rollup = derive_rollup(overall, &items);
        assert_eq!(rollup.overall, ReadinessSeverity::Critical);
        // Deterministic for these inputs: cpu_sensor_missing (crit);
        // pwm_read_only (warn); pwm_control_unverified + unknown_sensors_present
        // (info); pwm_controls_present (ok, uncounted).
        assert_eq!(rollup.critical, 1);
        assert_eq!(rollup.warning, 1);
        assert_eq!(rollup.info, 2);
        // top_* = the first item at the overall (critical) severity.
        assert_eq!(rollup.top_code.as_deref(), Some("cpu_sensor_missing"));
        assert_eq!(
            rollup.top_summary.as_deref(),
            Some("No CPU temperature sensor detected")
        );
    }

    #[test]
    fn derive_rollup_top_is_first_item_at_overall_severity_skipping_ok() {
        // ok, then two warnings — top must be the FIRST warning, not the ok item.
        let items = vec![
            ReadinessItem::new("ok_one", ReadinessSeverity::Ok, "cpu", "fine", "d".into()),
            ReadinessItem::new(
                "warn_one",
                ReadinessSeverity::Warning,
                "pwm",
                "first warning",
                "d".into(),
            ),
            ReadinessItem::new(
                "warn_two",
                ReadinessSeverity::Warning,
                "pwm",
                "second warning",
                "d".into(),
            ),
        ];
        let rollup = derive_rollup(overall_severity(&items), &items);
        assert_eq!(rollup.overall, ReadinessSeverity::Warning);
        assert_eq!(rollup.warning, 2);
        assert_eq!(rollup.top_code.as_deref(), Some("warn_one"));
        assert_eq!(rollup.top_summary.as_deref(), Some("first warning"));
    }

    #[test]
    fn derive_rollup_all_ok_has_no_top_and_zero_issue_counts() {
        let items = vec![
            ReadinessItem::new(
                "cpu_sensor_present",
                ReadinessSeverity::Ok,
                "cpu",
                "s",
                "d".into(),
            ),
            ReadinessItem::new(
                "pwm_controls_present",
                ReadinessSeverity::Ok,
                "pwm",
                "s",
                "d".into(),
            ),
        ];
        let rollup = derive_rollup(overall_severity(&items), &items);
        assert_eq!(rollup.overall, ReadinessSeverity::Ok);
        assert_eq!((rollup.critical, rollup.warning, rollup.info), (0, 0, 0));
        assert!(rollup.top_summary.is_none());
        assert!(rollup.top_code.is_none());
    }

    #[test]
    fn derive_rollup_empty_is_ok_with_no_top() {
        let rollup = derive_rollup(overall_severity(&[]), &[]);
        assert_eq!(rollup.overall, ReadinessSeverity::Ok);
        assert_eq!((rollup.critical, rollup.warning, rollup.info), (0, 0, 0));
        assert!(rollup.top_code.is_none());
    }

    #[test]
    fn rollup_serialises_lowercase_and_omits_top_when_none() {
        let ok = derive_rollup(ReadinessSeverity::Ok, &[]);
        let v = serde_json::to_value(&ok).unwrap();
        assert_eq!(v["overall"], serde_json::json!("ok"));
        assert_eq!(v["critical"], serde_json::json!(0));
        assert!(v.get("top_summary").is_none());
        assert!(v.get("top_code").is_none());
    }
}
