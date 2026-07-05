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
}
