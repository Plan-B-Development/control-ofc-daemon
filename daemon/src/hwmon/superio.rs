//! Passive Super-I/O hardware-monitor chip detection (DEC-202).
//!
//! Report-only. This module answers "which motherboard Super-I/O sensor/fan
//! chip does this machine have, is its kernel driver bound, and — if not —
//! which allowlisted module should the user load, with what caveats". It is
//! the daemon-owned half of the Super-I/O feature; the GUI only displays the
//! result and the copy-paste remediation.
//!
//! ## Safety posture (non-negotiable)
//! - **Passive only.** Detection composes read-only evidence the kernel already
//!   exposes (DMI board table, bound hwmon chips, `/proc/modules`, `/dev/kmsg`,
//!   `/proc/ioports`) via [`crate::hwmon::chip_db`]. It never probes an I/O
//!   port, never loads a module, never writes any sysfs attribute, and never
//!   touches PWM. (Active `/dev/port` probing and module loading are separate,
//!   opt-in, later phases — not here.)
//! - **Detection proves "chip present", never "fan control available".** A
//!   detected chip may have unrouted PWM pins or a BIOS/EC/ACPI that overrides
//!   writes; the report says so and never claims control works.
//! - **x86 only.** On non-x86 the detector returns `arch_supported = false`.
//! - **Allowlisted recommendations.** A "load this driver" suggestion is only
//!   ever emitted for a module in [`SUPERIO_ALLOWLIST`], and never suggests a
//!   risky module parameter (`force_id`, `ignore_resource_conflict`).
//!
//! The detector is dependency-injected via [`SuperIoEvidence`] so it is a pure,
//! deterministic function over primitive facts — tested with fake hardware, no
//! real sysfs. [`SysfsSuperIoEvidence`] is the production adapter that gathers
//! the real evidence from `chip_db`.

use crate::hwmon::chip_db;
use crate::hwmon::classify::Confidence;
use std::collections::BTreeMap;

/// Whether Super-I/O detection is supported on the build target. Detection
/// relies on x86 Super-I/O / ISA semantics; on any other architecture the
/// detector short-circuits to an empty, `arch_supported = false` report.
pub const SUPERIO_ARCH_SUPPORTED: bool = cfg!(any(target_arch = "x86", target_arch = "x86_64"));

/// Super-I/O silicon vendor (display grouping only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperIoVendor {
    Ite,
    Nuvoton,
    Winbond,
    Smsc,
    National,
    Fintek,
    Unknown,
}

impl std::fmt::Display for SuperIoVendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Ite => "ite",
            Self::Nuvoton => "nuvoton",
            Self::Winbond => "winbond",
            Self::Smsc => "smsc",
            Self::National => "national",
            Self::Fintek => "fintek",
            Self::Unknown => "unknown",
        })
    }
}

/// Where a chip's presence was observed. A chip may carry more than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// Listed for this board in the DMI dual-chip table (DEC-101).
    DmiBoardTable,
    /// The kernel logged it during its Super-I/O scan (`/dev/kmsg`).
    KernelLog,
    /// Currently bound and exposing an hwmon device.
    BoundHwmon,
    /// Identified by the opt-in active `/dev/port` probe (DEC-203).
    PortProbe,
}

impl std::fmt::Display for Evidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::DmiBoardTable => "dmi_board_table",
            Self::KernelLog => "kernel_log",
            Self::BoundHwmon => "bound_hwmon",
            Self::PortProbe => "port_probe",
        })
    }
}

/// A concrete "load this driver" suggestion for an unbound chip. Only ever
/// constructed for an allowlisted module and never names a risky parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperIoRecommendation {
    /// The allowlisted kernel module to load.
    pub module: String,
    /// Whether mainline supports this specific chip (false ⇒ needs the DKMS
    /// build; delegated to [`chip_db::chip_driver_in_mainline`], DEC-144).
    pub in_mainline: bool,
    /// Copy-paste remediation text (modules-load.d / modprobe, or DKMS steps).
    pub load_hint: String,
    /// Why this is being suggested (which evidence, and that it is unbound).
    pub reason: String,
    /// Caveats the user must weigh before loading (ACPI conflict, DEC-106
    /// collision, DKMS-required, per-driver risk).
    pub risk_notes: Vec<String>,
}

/// One Super-I/O chip the detector concluded is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperIoChip {
    /// hwmon-style chip name, e.g. `"it8696"`, `"nct6799"`.
    pub chip_name: String,
    pub vendor: SuperIoVendor,
    /// Evidence sources, deduplicated and in a stable order.
    pub evidence: Vec<Evidence>,
    /// Confidence the chip is physically present (bound/kernel-logged ⇒ High;
    /// board-table-only ⇒ Medium).
    pub confidence: Confidence,
    /// The module *inferred* to have bound this chip (`Some` only when bound) —
    /// derived from `expected_module`, not observed from sysfs. For split-module
    /// drivers such as nct6775 (whose platform half is `nct6775_platform`) this
    /// is the top-level module name, not the sub-module that performed the bind.
    pub bound_driver: Option<String>,
    /// The kernel module expected to drive it (`"unknown"` if unrecognized).
    pub expected_module: String,
    /// Whether `expected_module` is currently loaded.
    pub module_loaded: bool,
    /// Whether the chip is currently exposing an hwmon device.
    pub hwmon_present: bool,
    /// A load recommendation, present only when the chip is unbound and its
    /// module is allowlisted.
    pub recommendation: Option<SuperIoRecommendation>,
    /// Non-actionable observations (e.g. "unrecognized chip").
    pub caveats: Vec<String>,
}

/// The full passive detection report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperIoReport {
    /// False on non-x86 targets (detector returns empty).
    pub arch_supported: bool,
    /// Detected chips, ordered by chip name.
    pub chips: Vec<SuperIoChip>,
    /// Driver names whose I/O range collides with an ACPI OperationRegion.
    pub acpi_conflict_drivers: Vec<String>,
    /// Report-level notes (always includes the "present ≠ controllable" caveat).
    pub notes: Vec<String>,
}

/// One hwmon chip currently bound, as seen by the daemon's live cache. Passed
/// into the detector so it need not read the sensor/pwm cache itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundChip {
    /// hwmon `name` (e.g. `"nct6799"`).
    pub chip_name: String,
    /// Stable device identifier (used for DEC-106 dual-chip disambiguation).
    pub device_id: String,
}

/// Read-only evidence the detector composes. Injected so the detector is a
/// pure function testable without real hardware.
pub trait SuperIoEvidence {
    /// DMI `(board_vendor, board_name)`.
    fn board(&self) -> (String, String);
    /// hwmon chips currently bound (from the daemon's live cache).
    fn bound_chips(&self) -> Vec<BoundChip>;
    /// Names of currently-loaded known hwmon modules.
    fn loaded_modules(&self) -> Vec<String>;
    /// Chip names the kernel logged during its Super-I/O scan. NOTE: the
    /// production reader (`read_kernel_detected_chips`) parses only ITE `it87:`
    /// log lines, so in practice only ITE chips surface here; non-ITE chips
    /// reach the detector via `bound_chips` (or, for ITE boards, the DMI table).
    /// The trait itself is source-agnostic — tests may inject any chip name.
    fn kmsg_chips(&self) -> Vec<String>;
    /// Driver names with an ACPI I/O-port conflict.
    fn acpi_conflict_drivers(&self) -> Vec<String>;
}

// ── Module allowlist ────────────────────────────────────────────────
//
// The daemon must never *recommend* (nor, in later phases, load) a module
// outside this vetted set. Entries are the Super-I/O sensor/fan drivers this
// project supports; every module `expected_driver_for_chip` can return has an
// entry here so a recognized chip always has a rationale, and an unrecognized
// one is safely dropped. Mainline-vs-DKMS truth is NOT duplicated here — it is
// delegated to `chip_db::chip_driver_in_mainline` (DEC-144). No entry ever
// carries a module parameter.

struct AllowlistEntry {
    module: &'static str,
    vendor: SuperIoVendor,
    /// Baseline per-driver risk note, appended to a recommendation's
    /// `risk_notes` (empty string ⇒ none).
    risk_note: &'static str,
}

const SUPERIO_ALLOWLIST: &[AllowlistEntry] = &[
    AllowlistEntry {
        module: "it87",
        vendor: SuperIoVendor::Ite,
        risk_note: "Newer ITE chips (e.g. IT8686E/IT8688E/IT8696E) are supported only by the \
                    out-of-tree it87-dkms-git driver; mainline it87 will not bind them. Never use \
                    force_id — it misroutes every register access.",
    },
    AllowlistEntry {
        module: "nct6775",
        vendor: SuperIoVendor::Nuvoton,
        risk_note: "On MSI boards exposing an NCT6687, do not load nct6775 alongside the \
                    out-of-tree nct6687 driver — chip-ID overlap can brick a fan header (DEC-106).",
    },
    AllowlistEntry {
        module: "nct6687",
        vendor: SuperIoVendor::Nuvoton,
        risk_note: "Out-of-tree nct6687d (DKMS), common on MSI boards. Do not load alongside \
                    in-kernel nct6775 (DEC-106 brick risk); use a build post-Fred78290 PR #164.",
    },
    AllowlistEntry {
        module: "nct6683",
        vendor: SuperIoVendor::Nuvoton,
        risk_note: "Mainline nct6683 is monitoring-only — its firmware disables writes, so it \
                    reads temps/fans but offers no PWM control. For fan control on these MSI/ASRock \
                    chips, the out-of-tree nct6687d (module `nct6687`) is needed instead.",
    },
    AllowlistEntry {
        module: "f71882fg",
        vendor: SuperIoVendor::Fintek,
        risk_note: "",
    },
    AllowlistEntry {
        module: "f71805f",
        vendor: SuperIoVendor::Fintek,
        risk_note: "",
    },
    AllowlistEntry {
        module: "w83627ehf",
        vendor: SuperIoVendor::Winbond,
        risk_note: "",
    },
    AllowlistEntry {
        module: "w83627hf",
        vendor: SuperIoVendor::Winbond,
        risk_note: "Older Winbond family; monitoring-focused.",
    },
    AllowlistEntry {
        module: "smsc47m1",
        vendor: SuperIoVendor::Smsc,
        risk_note: "",
    },
    AllowlistEntry {
        module: "smsc47b397",
        vendor: SuperIoVendor::Smsc,
        risk_note: "Monitoring-only driver — exposes temperatures and fan tachometers but no PWM \
                    output.",
    },
    AllowlistEntry {
        module: "dme1737",
        vendor: SuperIoVendor::Smsc,
        risk_note: "",
    },
    AllowlistEntry {
        module: "sch5627",
        vendor: SuperIoVendor::Smsc,
        risk_note: "",
    },
    AllowlistEntry {
        module: "sch5636",
        vendor: SuperIoVendor::Smsc,
        risk_note: "",
    },
    AllowlistEntry {
        module: "pc87360",
        vendor: SuperIoVendor::National,
        risk_note: "The kernel driver warns that careless PWM values can stop a fan and cause \
                    irreversible damage. (This tool only detects — it never writes PWM.)",
    },
    AllowlistEntry {
        module: "pc87427",
        vendor: SuperIoVendor::National,
        risk_note: "",
    },
];

fn allowlist_entry(module: &str) -> Option<&'static AllowlistEntry> {
    SUPERIO_ALLOWLIST.iter().find(|e| e.module == module)
}

/// Vendor for an allowlisted module (Unknown if not allowlisted).
fn vendor_for_module(module: &str) -> SuperIoVendor {
    allowlist_entry(module)
        .map(|e| e.vendor)
        .unwrap_or(SuperIoVendor::Unknown)
}

/// Per-chip accumulator while unioning evidence sources.
#[derive(Default)]
struct EvidenceAcc {
    dmi: bool,
    kmsg: bool,
    bound: bool,
}

/// Run passive Super-I/O detection over the injected evidence.
///
/// Deterministic and side-effect free. On non-x86 targets it returns an empty,
/// `arch_supported = false` report.
pub fn detect_superio(ev: &dyn SuperIoEvidence) -> SuperIoReport {
    // Always-present honesty note (AREA-8 / DEC-201): detection ≠ controllability.
    let present_not_control =
        "Detection confirms a chip is present; it does not prove fan control is available. \
         Whether pwmN control works depends on routed PWM pins and the BIOS/EC/ACPI not \
         overriding the chip — verify after the driver is bound."
            .to_string();

    if !SUPERIO_ARCH_SUPPORTED {
        return SuperIoReport {
            arch_supported: false,
            chips: Vec::new(),
            acpi_conflict_drivers: Vec::new(),
            notes: vec!["Super-I/O detection is only supported on x86 / x86_64.".to_string()],
        };
    }

    let loaded = ev.loaded_modules();
    let acpi_conflict_drivers = ev.acpi_conflict_drivers();

    // Union evidence into a name-keyed map (BTreeMap ⇒ deterministic ordering).
    let mut acc: BTreeMap<String, EvidenceAcc> = BTreeMap::new();
    for b in ev.bound_chips() {
        // Only BOUND chips that are recognised Super-I/O monitoring chips become
        // candidates. An ordinary sensor chip (k10temp/coretemp/amdgpu/nvme/
        // spd5118/…) is a legitimate hwmon device but NOT a Super-I/O chip, and
        // must not be reported as an "Unrecognized Super-I/O" card (DEC-207). The
        // DMI/kmsg evidence sources below are already Super-I/O-scoped, so only
        // the bound-hwmon source needs this gate.
        if !chip_db::is_known_superio_chip(&b.chip_name) {
            continue;
        }
        acc.entry(normalize(&b.chip_name)).or_default().bound = true;
    }
    for c in ev.kmsg_chips() {
        acc.entry(normalize(&c)).or_default().kmsg = true;
    }
    let (board_vendor, board_name) = ev.board();
    for c in chip_db::expected_chips_for_board(&board_vendor, &board_name) {
        acc.entry(normalize(&c)).or_default().dmi = true;
    }

    let mut chips = Vec::with_capacity(acc.len());
    for (chip_name, ev_acc) in acc {
        chips.push(build_chip(
            &chip_name,
            &ev_acc,
            &loaded,
            &acpi_conflict_drivers,
        ));
    }

    let mut notes = vec![present_not_control];
    if chips.is_empty() {
        notes.push(
            "No Super-I/O monitoring chip was detected from the board table, loaded drivers, or \
             kernel logs. On boards whose Super-I/O driver is not yet loaded, the chip is not \
             visible passively."
                .to_string(),
        );
    }

    SuperIoReport {
        arch_supported: true,
        chips,
        acpi_conflict_drivers,
        notes,
    }
}

fn normalize(chip: &str) -> String {
    chip.trim().to_lowercase()
}

/// Build one [`SuperIoChip`] from its accumulated evidence.
fn build_chip(
    chip_name: &str,
    ev: &EvidenceAcc,
    loaded: &[String],
    acpi_conflict_drivers: &[String],
) -> SuperIoChip {
    let expected_module = chip_db::expected_driver(chip_name).to_string();
    let vendor = vendor_for_module(&expected_module);
    let module_loaded = loaded.iter().any(|m| m == &expected_module);
    let hwmon_present = ev.bound;
    let bound_driver = if hwmon_present && expected_module != "unknown" {
        Some(expected_module.clone())
    } else {
        None
    };

    // Confidence: bound or kernel-logged ⇒ definitely present (High); a
    // board-table-only expectation is a strong-but-unconfirmed prior (Medium).
    let confidence = if ev.bound || ev.kmsg {
        Confidence::High
    } else if ev.dmi {
        Confidence::Medium
    } else {
        // Defensive: every candidate enters the accumulator via at least one
        // evidence source, so this arm is unreachable from the production
        // accumulator. Kept for total coverage of the enum.
        Confidence::Low
    };

    let mut evidence = Vec::new();
    if ev.dmi {
        evidence.push(Evidence::DmiBoardTable);
    }
    if ev.kmsg {
        evidence.push(Evidence::KernelLog);
    }
    if ev.bound {
        evidence.push(Evidence::BoundHwmon);
    }

    let mut caveats = Vec::new();
    let recommendation = if expected_module == "unknown" {
        caveats.push(
            "Unrecognized Super-I/O chip — no known in-kernel driver maps to this chip name."
                .to_string(),
        );
        None
    } else if hwmon_present {
        // Already bound and working — nothing to recommend.
        None
    } else {
        build_recommendation(
            chip_name,
            &expected_module,
            ev,
            module_loaded,
            loaded,
            acpi_conflict_drivers,
            &mut caveats,
        )
    };

    SuperIoChip {
        chip_name: chip_name.to_string(),
        vendor,
        evidence,
        confidence,
        bound_driver,
        expected_module,
        module_loaded,
        hwmon_present,
        recommendation,
        caveats,
    }
}

/// Build a load recommendation for a present-but-unbound chip whose module is
/// allowlisted. Returns `None` (with a caveat) if the module is not on the
/// allowlist — the safety gate that stops the daemon ever recommending an
/// unvetted module.
fn build_recommendation(
    chip_name: &str,
    module: &str,
    ev: &EvidenceAcc,
    module_loaded: bool,
    loaded: &[String],
    acpi_conflict_drivers: &[String],
    caveats: &mut Vec<String>,
) -> Option<SuperIoRecommendation> {
    let Some(entry) = allowlist_entry(module) else {
        caveats.push(format!(
            "{module} is not in the vetted Super-I/O module allowlist; no load recommendation is \
             offered."
        ));
        return None;
    };

    let in_mainline = chip_db::chip_driver_in_mainline(chip_name);

    let load_hint = if module_loaded {
        // The driver is already loaded but the chip produced no hwmon device —
        // telling the user to "load it" would be wrong. Point at the real
        // causes of a failed bind instead. The it87-dkms / mmio / force_id
        // notes are ITE-specific, so only add them for the it87 driver.
        let ite_tail = if module == "it87" {
            " For newer Gigabyte ITE boards the in-tree it87 often cannot drive the chip — install \
             the it87-dkms-git build (with `mmio=on`). Do not pass force_id."
        } else {
            ""
        };
        format!(
            "The `{module}` driver is already loaded but no hwmon device appeared — the chip did \
             not bind. Common causes: an ACPI resource conflict (see caveats) or a reboot being \
             needed.{ite_tail}"
        )
    } else if in_mainline {
        format!(
            "Enable it at boot: `echo {module} | sudo tee /etc/modules-load.d/{module}.conf`, or \
             load it now with `sudo modprobe {module}`. A reboot or module reload may be needed \
             before the fans/sensors appear."
        )
    } else {
        format!(
            "This chip needs the out-of-tree DKMS driver — install the vendor DKMS package (e.g. \
             `it87-dkms-git` for ITE, `nct6687d-dkms-git` for MSI Nuvoton), then load `{module}`. \
             Do not pass force_id to bind an unrecognized chip ID — it misroutes every register \
             access."
        )
    };

    let reason = if module_loaded {
        format!(
            "The `{module}` driver is loaded but no hwmon device appeared for this chip — it did \
             not bind."
        )
    } else {
        match (ev.dmi, ev.kmsg) {
            (true, true) => format!(
                "Your board's chip table lists this chip and the kernel logged it, but no {module} \
                 driver is bound."
            ),
            (true, false) => {
                format!("Your board's chip table lists this chip, but no {module} driver is bound.")
            }
            (false, true) => format!(
                "The kernel logged this chip during its Super-I/O scan, but no {module} driver is \
                 bound."
            ),
            // Defensive: build_recommendation is only reached for an unbound
            // chip that entered the accumulator, so at least one of dmi/kmsg is
            // set here. Unreachable in practice; kept for a total match.
            (false, false) => {
                format!("This chip appears present, but no {module} driver is bound.")
            }
        }
    };

    let mut risk_notes = Vec::new();
    if !entry.risk_note.is_empty() {
        risk_notes.push(entry.risk_note.to_string());
    }
    if acpi_conflict_drivers.iter().any(|d| d == module) {
        risk_notes.push(format!(
            "ACPI firmware claims this chip's I/O ports; under acpi_enforce_resources=strict (the \
             default) the {module} driver may refuse to bind. Do not switch to lax or force the \
             driver without understanding the race it reintroduces."
        ));
    }
    if let Some(other) = chip_db::conflicting_loaded_module(module, loaded) {
        if module_loaded {
            // Both drivers are loaded right now — the collision is ACTIVE, not
            // a future risk of loading.
            risk_notes.push(format!(
                "Both `{module}` and the conflicting driver `{other}` are currently loaded — this \
                 is exactly the chip-ID clash that can brick a fan header (DEC-106). Blacklist one \
                 driver and reboot now; do not leave both loaded."
            ));
        } else {
            risk_notes.push(format!(
                "The conflicting driver `{other}` is already loaded — also loading `{module}` \
                 risks a chip-ID clash that can brick a fan header (DEC-106). Identify the chip \
                 first and blacklist the wrong driver; do not load both."
            ));
        }
    }

    Some(SuperIoRecommendation {
        module: module.to_string(),
        in_mainline,
        load_hint,
        reason,
        risk_notes,
    })
}

// ── Production evidence adapter ─────────────────────────────────────

/// Gathers real Super-I/O evidence from `chip_db` (passive `/proc`, `/sys`,
/// `/dev/kmsg` reads). The bound-chip list comes from the daemon's live cache
/// and is supplied by the caller (the endpoint handler in a later phase), so
/// this adapter stays free of any cache coupling and remains a thin, honest
/// mapping over `chip_db`.
pub struct SysfsSuperIoEvidence {
    bound: Vec<BoundChip>,
}

impl SysfsSuperIoEvidence {
    /// Construct from the caller-supplied bound-chip list (from the sensor/PWM
    /// cache). All other evidence is read from `chip_db` on demand.
    pub fn new(bound_chips: Vec<BoundChip>) -> Self {
        Self { bound: bound_chips }
    }
}

impl SuperIoEvidence for SysfsSuperIoEvidence {
    fn board(&self) -> (String, String) {
        let b = chip_db::read_board_info();
        (b.vendor, b.name)
    }

    fn bound_chips(&self) -> Vec<BoundChip> {
        self.bound.clone()
    }

    fn loaded_modules(&self) -> Vec<String> {
        chip_db::detect_loaded_modules()
            .into_iter()
            .filter(|m| m.loaded)
            .map(|m| m.name)
            .collect()
    }

    fn kmsg_chips(&self) -> Vec<String> {
        chip_db::read_kernel_detected_chips()
    }

    fn acpi_conflict_drivers(&self) -> Vec<String> {
        let mut v: Vec<String> = chip_db::detect_acpi_conflicts()
            .into_iter()
            .map(|c| c.conflicts_with_driver)
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake evidence for deterministic, hardware-free tests.
    #[derive(Default)]
    struct FakeEvidence {
        board: (String, String),
        bound: Vec<BoundChip>,
        loaded: Vec<String>,
        kmsg: Vec<String>,
        acpi: Vec<String>,
    }

    impl SuperIoEvidence for FakeEvidence {
        fn board(&self) -> (String, String) {
            self.board.clone()
        }
        fn bound_chips(&self) -> Vec<BoundChip> {
            self.bound.clone()
        }
        fn loaded_modules(&self) -> Vec<String> {
            self.loaded.clone()
        }
        fn kmsg_chips(&self) -> Vec<String> {
            self.kmsg.clone()
        }
        fn acpi_conflict_drivers(&self) -> Vec<String> {
            self.acpi.clone()
        }
    }

    fn bound(name: &str, dev: &str) -> BoundChip {
        BoundChip {
            chip_name: name.to_string(),
            device_id: dev.to_string(),
        }
    }

    fn find<'a>(r: &'a SuperIoReport, chip: &str) -> &'a SuperIoChip {
        r.chips
            .iter()
            .find(|c| c.chip_name == chip)
            .unwrap_or_else(|| panic!("chip {chip} not in report: {:?}", r.chips))
    }

    #[test]
    fn report_always_carries_present_not_control_note() {
        let r = detect_superio(&FakeEvidence::default());
        assert!(r.arch_supported); // test host is x86
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("does not prove fan control")),
            "every report must carry the present≠control honesty note"
        );
    }

    #[test]
    fn empty_evidence_yields_no_chips_and_a_note() {
        let r = detect_superio(&FakeEvidence::default());
        assert!(r.chips.is_empty());
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("No Super-I/O monitoring chip")));
    }

    #[test]
    fn gigabyte_board_unbound_ite_recommends_dkms() {
        // X570 AORUS MASTER's DMI entry expects it8688 + it8792 (both DKMS-only
        // ITE chips). Nothing is bound/loaded → expect a DKMS recommendation.
        let ev = FakeEvidence {
            board: (
                "Gigabyte Technology Co., Ltd.".into(),
                "X570 AORUS MASTER".into(),
            ),
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let chip = find(&r, "it8688");
        assert_eq!(chip.expected_module, "it87");
        assert_eq!(chip.vendor, SuperIoVendor::Ite);
        assert_eq!(chip.confidence, Confidence::Medium); // DMI-only
        assert!(!chip.hwmon_present);
        assert_eq!(chip.evidence, vec![Evidence::DmiBoardTable]);
        let rec = chip
            .recommendation
            .as_ref()
            .expect("unbound chip → recommendation");
        assert_eq!(rec.module, "it87");
        assert!(!rec.in_mainline, "it8688 is DKMS-only (DEC-144)");
        assert!(rec.load_hint.contains("DKMS"));
        assert!(rec.risk_notes.iter().any(|n| n.contains("it87-dkms-git")));
    }

    #[test]
    fn bound_nuvoton_chip_needs_no_recommendation() {
        let ev = FakeEvidence {
            bound: vec![bound("nct6799", "isa-0290")],
            loaded: vec!["nct6775".into()],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let chip = find(&r, "nct6799");
        assert!(chip.hwmon_present);
        assert_eq!(chip.confidence, Confidence::High);
        assert_eq!(chip.bound_driver.as_deref(), Some("nct6775"));
        assert!(chip.module_loaded);
        assert!(
            chip.recommendation.is_none(),
            "a bound, working chip should not be told to load anything"
        );
        assert_eq!(chip.evidence, vec![Evidence::BoundHwmon]);
    }

    #[test]
    fn unbound_chip_with_conflicting_loaded_driver_warns_dec106() {
        // Exercises the recommender's collision path: an unbound Nuvoton chip
        // whose module (nct6775) is NOT loaded, but the conflicting nct6687 IS
        // → the recommendation must carry the DEC-106 brick warning. We inject
        // it via kmsg because the trait is source-agnostic; in production a
        // Nuvoton chip only surfaces this way once bound (the kmsg reader is
        // ITE-only), so this path is chiefly a guard for the future active
        // probe. `module_loaded` is false here → the "hypothetical" phrasing.
        let ev = FakeEvidence {
            kmsg: vec!["nct6799".into()],
            loaded: vec!["nct6687".into()],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let chip = find(&r, "nct6799");
        assert_eq!(chip.confidence, Confidence::High); // kernel-logged
        let rec = chip
            .recommendation
            .as_ref()
            .expect("unbound → recommendation");
        assert_eq!(rec.module, "nct6775");
        assert!(
            rec.risk_notes
                .iter()
                .any(|n| n.contains("DEC-106") && n.contains("nct6687")),
            "must warn that nct6687 is already loaded, risk_notes={:?}",
            rec.risk_notes
        );
    }

    #[test]
    fn active_collision_when_both_drivers_loaded_is_flagged_as_current() {
        // nct6775 IS loaded AND the conflicting nct6687 IS loaded, but the chip
        // did not bind → the DEC-106 note must say the collision is happening
        // NOW (blacklist + reboot), not frame it as a risk of a future load.
        let ev = FakeEvidence {
            kmsg: vec!["nct6799".into()],
            loaded: vec!["nct6775".into(), "nct6687".into()],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let chip = find(&r, "nct6799");
        assert!(chip.module_loaded);
        assert!(!chip.hwmon_present);
        let rec = chip
            .recommendation
            .as_ref()
            .expect("loaded-but-unbound colliding chip still needs guidance");
        assert!(
            rec.risk_notes.iter().any(|n| n.contains("currently loaded")
                && n.contains("DEC-106")
                && n.contains("reboot now")),
            "active collision must be framed as current, got {:?}",
            rec.risk_notes
        );
        assert!(
            !rec.risk_notes.iter().any(|n| n.contains("also loading")),
            "must not frame an already-active collision as a hypothetical future load"
        );
    }

    #[test]
    fn acpi_conflict_is_surfaced_in_recommendation() {
        let ev = FakeEvidence {
            board: (
                "Gigabyte Technology Co., Ltd.".into(),
                "X570 AORUS MASTER".into(),
            ),
            acpi: vec!["it87".into()],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        assert_eq!(r.acpi_conflict_drivers, vec!["it87".to_string()]);
        let rec = find(&r, "it8688").recommendation.as_ref().unwrap();
        assert!(
            rec.risk_notes
                .iter()
                .any(|n| n.contains("acpi_enforce_resources")),
            "ACPI conflict must appear as a caveat, got {:?}",
            rec.risk_notes
        );
    }

    #[test]
    fn unrecognized_chip_gets_no_recommendation() {
        let ev = FakeEvidence {
            kmsg: vec!["xyz9000".into()],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let chip = find(&r, "xyz9000");
        assert_eq!(chip.expected_module, "unknown");
        assert_eq!(chip.vendor, SuperIoVendor::Unknown);
        assert!(chip.recommendation.is_none());
        assert!(chip.caveats.iter().any(|c| c.contains("Unrecognized")));
    }

    #[test]
    fn recommendations_never_suggest_forbidden_parameters() {
        // Sweep unbound-chip scenarios across every recommendation-producing
        // family (ITE via DMI, plus Winbond/SMSC/National/Nuvoton/Fintek via
        // kmsg, which the trait treats source-agnostically). No recommendation
        // string — load_hint, reason, OR any risk_note — may propose a risky
        // parameter as an action.
        let mut evs: Vec<FakeEvidence> = vec![
            FakeEvidence {
                board: (
                    "Gigabyte Technology Co., Ltd.".into(),
                    "X570 AORUS MASTER".into(),
                ),
                ..Default::default()
            },
            FakeEvidence {
                board: (
                    "Gigabyte Technology Co., Ltd.".into(),
                    "X870E AORUS MASTER".into(),
                ),
                ..Default::default()
            },
        ];
        // One unbound chip per non-ITE family, surfaced via kmsg.
        for chip in [
            "nct6799",
            "w83627ehf",
            "smsc47m1",
            "pc87360",
            "f71805f",
            "dme1737",
        ] {
            evs.push(FakeEvidence {
                kmsg: vec![chip.to_string()],
                ..Default::default()
            });
        }
        let mut saw_recommendation = false;
        for ev in &evs {
            let r = detect_superio(ev);
            for chip in &r.chips {
                if let Some(rec) = &chip.recommendation {
                    saw_recommendation = true;
                    let mut fields = vec![rec.load_hint.clone(), rec.reason.clone()];
                    fields.extend(rec.risk_notes.iter().cloned());
                    for f in &fields {
                        assert!(
                            !f.contains("force_id=") && !f.contains("ignore_resource_conflict="),
                            "no recommendation field may propose a forbidden parameter, got: {f}"
                        );
                    }
                }
            }
        }
        assert!(
            saw_recommendation,
            "the sweep must actually exercise recommendations"
        );
    }

    #[test]
    fn loaded_but_unbound_chip_advises_binding_fix_not_a_reload() {
        // The common "DKMS installed, module loaded, but no hwmon device" state:
        // the recommendation must NOT tell the user to load an already-loaded
        // module — it must point at the real bind failure (ACPI/reboot/mmio).
        let ev = FakeEvidence {
            kmsg: vec!["it8696".into()],
            loaded: vec!["it87".into()], // module IS loaded
            bound: vec![],               // but the chip did not bind
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let chip = find(&r, "it8696");
        assert!(chip.module_loaded);
        assert!(!chip.hwmon_present);
        let rec = chip
            .recommendation
            .as_ref()
            .expect("loaded-but-unbound chip still needs guidance");
        assert!(
            rec.load_hint.contains("already loaded") && rec.load_hint.contains("did not bind"),
            "hint must address the failed bind, not say 'load it', got: {}",
            rec.load_hint
        );
        assert!(
            !rec.load_hint.contains("modprobe it87"),
            "must not tell the user to load an already-loaded module"
        );
        assert!(rec.reason.contains("did not bind"));
    }

    #[test]
    fn both_dmi_and_kmsg_unbound_uses_combined_reason() {
        // Exercises the (dmi=true, kmsg=true) reason branch: board table lists
        // the chip AND the kernel logged it, but nothing is bound.
        let ev = FakeEvidence {
            board: (
                "Gigabyte Technology Co., Ltd.".into(),
                "X570 AORUS MASTER".into(),
            ),
            kmsg: vec!["it8688".into()],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let chip = find(&r, "it8688");
        assert_eq!(
            chip.evidence,
            vec![Evidence::DmiBoardTable, Evidence::KernelLog]
        );
        let rec = chip.recommendation.as_ref().unwrap();
        assert!(
            rec.reason
                .contains("lists this chip and the kernel logged it"),
            "combined-source reason expected, got: {}",
            rec.reason
        );
    }

    #[test]
    fn same_chip_from_different_sources_and_cases_deduplicates() {
        // A chip seen as "NCT6799" in kmsg and "nct6799" when bound must collapse
        // to ONE entry (guards normalize()), carrying both evidence sources.
        let ev = FakeEvidence {
            kmsg: vec!["NCT6799".into()],
            bound: vec![bound("nct6799", "isa-0290")],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        assert_eq!(
            r.chips.len(),
            1,
            "case-different names must dedupe: {:?}",
            r.chips
        );
        let chip = &r.chips[0];
        assert_eq!(chip.chip_name, "nct6799");
        assert_eq!(
            chip.evidence,
            vec![Evidence::KernelLog, Evidence::BoundHwmon]
        );
        assert!(chip.hwmon_present);
    }

    #[test]
    fn bound_non_superio_sensor_chips_are_never_cards() {
        // DEC-207: ordinary sensor chips are legitimate hwmon devices but NOT
        // Super-I/O chips — none may appear as an "Unrecognized Super-I/O" card.
        let ev = FakeEvidence {
            bound: vec![
                bound("amdgpu", "pci-0000:03:00.0"),
                bound("k10temp", "pci-0000:00:18.3"),
                bound("nvme", "pci-0000:01:00.0"),
                bound("spd5118", "i2c-0-0050"),
                bound("coretemp", "isa-0000"),
            ],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        assert!(
            r.chips.is_empty(),
            "no non-Super-I/O sensor chip may become a card: {:?}",
            r.chips
        );
    }

    #[test]
    fn bound_genuine_superio_chips_are_cards() {
        for name in ["nct6799", "it8688"] {
            let ev = FakeEvidence {
                bound: vec![bound(name, "isa-0290")],
                ..Default::default()
            };
            let r = detect_superio(&ev);
            assert_eq!(r.chips.len(), 1, "{name} should be one card: {:?}", r.chips);
            assert_eq!(r.chips[0].chip_name, name);
            assert!(r.chips[0].hwmon_present);
        }
    }

    #[test]
    fn bound_mixed_keeps_only_superio_chips() {
        // A realistic host: a Super-I/O chip bound alongside ordinary sensor
        // chips. Only the Super-I/O chips are cards.
        let ev = FakeEvidence {
            bound: vec![
                bound("k10temp", "pci-0000:00:18.3"),
                bound("nct6799", "isa-0290"),
                bound("amdgpu", "pci-0000:03:00.0"),
                bound("it8688", "isa-0a40"),
            ],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let mut names: Vec<&str> = r.chips.iter().map(|c| c.chip_name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["it8688", "nct6799"]);
    }

    #[test]
    fn same_chip_via_bound_dmi_and_kmsg_is_one_card_with_all_evidence() {
        // The X570 AORUS MASTER DMI table expects it8688; the same chip is also
        // kernel-logged and bound. All three evidence sources must collapse into
        // ONE it8688 card (DEC-207 "combine evidence, don't duplicate").
        let ev = FakeEvidence {
            board: (
                "Gigabyte Technology Co., Ltd.".into(),
                "X570 AORUS MASTER".into(),
            ),
            kmsg: vec!["IT8688".into()],
            bound: vec![bound("it8688", "isa-0a40")],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        assert_eq!(
            r.chips.iter().filter(|c| c.chip_name == "it8688").count(),
            1,
            "it8688 must be a single card, not one per source: {:?}",
            r.chips
        );
        let chip = find(&r, "it8688");
        assert_eq!(
            chip.evidence,
            vec![
                Evidence::DmiBoardTable,
                Evidence::KernelLog,
                Evidence::BoundHwmon
            ]
        );
        assert!(chip.hwmon_present);
    }

    #[test]
    fn allowlist_covers_every_recognized_module() {
        // Safety-gate completeness: every non-"unknown" module that
        // chip_db::expected_driver can return must have an allowlist entry, so
        // a recognized chip always has a rationale and is never silently
        // dropped for lack of an entry.
        let sample_chips = [
            "it8688",
            "nct6799",
            "nct6687",
            "nct6683",
            "f71882fg",
            "f71805f",
            "w83627ehf",
            "w83627hf",
            "smsc47m1",
            "smsc47b397",
            "dme1737",
            "sch5627",
            "sch5636",
            "pc87360",
            "pc87427",
        ];
        for chip in sample_chips {
            let module = chip_db::expected_driver(chip);
            assert_ne!(module, "unknown", "test chip {chip} should be recognized");
            assert!(
                allowlist_entry(module).is_some(),
                "module {module} (for chip {chip}) must be in SUPERIO_ALLOWLIST"
            );
        }
    }

    #[test]
    fn multi_source_evidence_reinforces_high_confidence() {
        // A chip that is both board-listed AND bound is High confidence and
        // records both evidence sources (deterministic order dmi, kmsg, bound).
        let ev = FakeEvidence {
            board: (
                "Gigabyte Technology Co., Ltd.".into(),
                "X570 AORUS MASTER".into(),
            ),
            bound: vec![bound("it8688", "isa-0a40")],
            loaded: vec!["it87".into()],
            ..Default::default()
        };
        let r = detect_superio(&ev);
        let chip = find(&r, "it8688");
        assert_eq!(chip.confidence, Confidence::High);
        assert_eq!(
            chip.evidence,
            vec![Evidence::DmiBoardTable, Evidence::BoundHwmon]
        );
        assert!(chip.hwmon_present);
        assert!(chip.recommendation.is_none());
    }

    #[test]
    fn arch_gate_reflects_build_target() {
        // On the x86_64 CI/build host the detector reports arch_supported=true;
        // this pins the gate to the real build target (comparing a runtime
        // field, not a bare const) rather than hard-coding `true`.
        let r = detect_superio(&FakeEvidence::default());
        assert_eq!(r.arch_supported, SUPERIO_ARCH_SUPPORTED);
    }
}
