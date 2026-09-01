//! Read-only hwmon inventory endpoint (Phase 1).
//!
//! `GET /inventory/hwmon` returns a structured, read-only inventory of
//! hwmon-visible hardware for the GUI: temperature sensors (live, mirroring
//! `/sensors`), controllable PWM headers (mirroring `/hwmon/headers`), and
//! monitor-only fan tachometers — `fanN_input` files with no matching `pwmN`,
//! which are otherwise invisible to the API. The daemon never writes hardware
//! to build this report.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::{build_sensor_entries, error_response, json_ok, AppState, ASSESSMENT_TTL};
use crate::api::responses::*;
use crate::health::state::DaemonState;
use crate::hwmon::classify::{
    classify_temp_sensor, is_cpu_class, select_default_cpu, Confidence, TempClass,
    TempClassification,
};
use crate::hwmon::inventory::discover_monitor_only_fans;
use crate::hwmon::readiness::{build_readiness, ReadinessInputs};
use crate::hwmon::superio;
use crate::hwmon::superio_probe;
use crate::hwmon::HWMON_SYSFS_ROOT;

/// GET /inventory/hwmon — structured, read-only hardware inventory.
///
/// Runs on the blocking pool because monitor-only-fan discovery walks
/// `/sys/class/hwmon` (mirrors `/diagnostics/hardware`, DEC-099). Read-only.
pub async fn hwmon_inventory_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match tokio::task::spawn_blocking(move || build_hwmon_inventory(&state)).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("hwmon inventory task failed: {e}")),
        ),
    }
}

/// Assemble the inventory response. Synchronous and (for the fan scan) blocking
/// — invoked via `spawn_blocking` from the handler above.
fn build_hwmon_inventory(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    // Temperature sensors — the live cache projection (identical fields to
    // `/sensors`), enriched with the Phase-2 classification refinement.
    let now = std::time::Instant::now();
    let snap = state.cache.snapshot();
    let classified = classify_cache_sensors(&snap, now);

    // Persisted user selections (Phase 5): the preferred CPU sensor wins over the
    // auto-pick when present; both selections are echoed under `preferences`.
    let runtime = crate::runtime_config::RuntimeConfig::load_from(&state.runtime_config_path);
    let preferred_cpu = runtime.preferred_cpu_sensor().map(str::to_string);
    let preferred_mb = runtime.preferred_mb_sensor().map(str::to_string);
    let default_cpu = build_default_cpu(&classified, preferred_cpu.as_deref());
    let preferences = if preferred_cpu.is_some() || preferred_mb.is_some() {
        Some(InventoryPreferences {
            cpu_sensor_id: preferred_cpu,
            mb_sensor_id: preferred_mb,
        })
    } else {
        None
    };

    let temp_sensors: Vec<InventoryTempSensor> = classified
        .into_iter()
        .map(|(sensor, c)| InventoryTempSensor {
            classification: c.class.to_string(),
            confidence: c.confidence.to_string(),
            rationale: c.rationale,
            sensor,
        })
        .collect();

    // Controllable PWM headers — the controller's discovered set, identical to
    // `/hwmon/headers`. Empty when no controller was constructed at startup.
    let pwm_controls: Vec<PwmHeaderEntry> = match &state.hwmon_controller {
        Some(controller) => {
            let ctrl = controller.lock();
            let assigned = state.header_roles();
            ctrl.headers()
                .into_iter()
                .map(|h| PwmHeaderEntry::from_descriptor(h, assigned.get(&h.id).copied()))
                .collect()
        }
        None => Vec::new(),
    };

    // Monitor-only fan tachometers — the one genuinely-new Phase-1 scan:
    // `fanN_input` with no matching `pwmN`. A scan failure (e.g. no
    // `/sys/class/hwmon` under a sandbox) degrades to an empty list, not an
    // error, so the sensors/PWM inventory still returns.
    let monitor_only_fans: Vec<FanInputEntry> =
        match discover_monitor_only_fans(std::path::Path::new(HWMON_SYSFS_ROOT)) {
            Ok(fans) => fans.iter().map(FanInputEntry::from).collect(),
            Err(e) => {
                log::warn!("hwmon inventory: monitor-only fan scan failed: {e}");
                Vec::new()
            }
        };

    json_ok(
        StatusCode::OK,
        HwmonInventoryResponse {
            api_version: API_VERSION,
            temp_sensors,
            pwm_controls,
            monitor_only_fans,
            default_cpu,
            preferences,
        },
    )
}

/// Build the `default_cpu` recommendation: the persisted preferred CPU sensor
/// wins when it is present in the live set (`source: "user"`), otherwise the
/// deterministic auto-pick (`source: "auto"`). A set-but-absent preference falls
/// back to auto — never blindly applied — and the readiness model flags it stale.
fn build_default_cpu(
    classified: &[(SensorEntry, TempClassification)],
    preferred_cpu: Option<&str>,
) -> Option<DefaultCpuEntry> {
    if let Some(pref) = preferred_cpu {
        if let Some((s, c)) = classified.iter().find(|(s, _)| s.id == pref) {
            return Some(DefaultCpuEntry {
                sensor_id: s.id.clone(),
                confidence: c.confidence.to_string(),
                rationale: "user-selected preferred CPU sensor".into(),
                source: "user".into(),
            });
        }
    }
    select_default_cpu(classified.iter().map(|(s, c)| (s.id.as_str(), c))).map(|r| {
        DefaultCpuEntry {
            sensor_id: r.sensor_id,
            confidence: r.confidence.to_string(),
            rationale: r.rationale,
            source: "auto".into(),
        }
    })
}

/// Classify the live cache sensors once — shared by the inventory and readiness
/// handlers so their two views never disagree.
fn classify_cache_sensors(
    snap: &DaemonState,
    now: std::time::Instant,
) -> Vec<(SensorEntry, TempClassification)> {
    // DEC-294: read DMI once per request, never per sensor. Classification is
    // vendor-gated, so this view must agree with what discovery decided.
    let board_vendor = crate::hwmon::chip_db::read_board_info().vendor;
    build_sensor_entries(snap, now)
        .into_iter()
        .map(|s| {
            let c = classify_temp_sensor(&s.chip_name, &s.label, &board_vendor);
            (s, c)
        })
        .collect()
}

/// GET /inventory/readiness — structured, read-only hardware-readiness list.
///
/// Diagnoses the CPU/hwmon/PWM inventory into actionable items (severity +
/// recommended action + blocks-flags) for the GUI's first-run guide. Read-only;
/// never mutates the system. Serves the shared hardware-assessment snapshot
/// (DEC-207) with a `force` refresh, so "opening readiness" stays an authoritative
/// refresh of the Dashboard rollup — the underlying scan is still coalesced with
/// any concurrent Super-I/O / combined request.
pub async fn hwmon_readiness_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match ensure_assessment(state, true).await {
        Some(a) => json_ok(
            StatusCode::OK,
            ReadinessResponse {
                api_version: API_VERSION,
                overall: a.overall,
                items: a.items.clone(),
            },
        ),
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "hardware assessment is temporarily unavailable — retry",
            ),
        ),
    }
}

/// Return a fresh-enough shared
/// [`HardwareAssessment`](crate::hwmon::readiness::HardwareAssessment), running AT
/// MOST one coalesced blocking scan for any burst of callers (DEC-207).
///
/// `force` bypasses the freshness TTL (manual refresh / preferred-sensor / rescan
/// / opening readiness). The single passive scan runs on the blocking pool; a
/// burst of simultaneous requests coalesces to one scan; the 1 Hz poll path never
/// calls this. On success the cache's `store` also mirrors the compact
/// rollup into `readiness_rollup` for `/status` + `/poll`. Returns `None` only if
/// the scan task fails AND no prior scan ever succeeded (⇒ the caller answers
/// `503`); a scan failure otherwise keeps the last-good result and never affects
/// fan control.
pub async fn ensure_assessment(
    state: Arc<AppState>,
    force: bool,
) -> Option<Arc<crate::hwmon::readiness::HardwareAssessment>> {
    let cache = state.assessment.clone();
    cache
        .ensure_with(force, ASSESSMENT_TTL, || async move {
            match tokio::task::spawn_blocking(move || compute_hardware_assessment(&state)).await {
                Ok(a) => Some(a),
                Err(e) => {
                    log::warn!("hardware assessment scan task failed: {e}");
                    None
                }
            }
        })
        .await
}

/// Query for `GET /inventory/hardware-readiness`. `?refresh=true` (or `1`) forces
/// a fresh scan; anything else — including an absent or malformed value — serves
/// the cached assessment. `Option<String>` so the extractor can never `400` on
/// this parameter (DEC-207).
#[derive(serde::Deserialize, Default)]
pub struct HardwareReadinessQuery {
    #[serde(default)]
    refresh: Option<String>,
}

/// GET /inventory/hardware-readiness — the combined readiness + Super-I/O snapshot
/// (DEC-207). The merged "Cooling Hardware Readiness" GUI page fetches this in ONE
/// request, so both halves come from the same shared passive scan (no
/// cross-endpoint drift, no redundant detection). Read-only. `?refresh=true`
/// forces a fresh (coalesced) scan; otherwise the cached assessment is served.
/// Gated purely by 404 on older daemons, mirroring the other `/inventory/*`
/// endpoints.
pub async fn hardware_readiness_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<HardwareReadinessQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let force = q
        .refresh
        .as_deref()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    // Read the (Copy) probe-availability flag before moving `state` into the scan.
    let (avail, reason) = superio_probe::port_probe_available(state.allow_port_probe);
    match ensure_assessment(state, force).await {
        Some(a) => json_ok(
            StatusCode::OK,
            HardwareReadinessResponse {
                api_version: API_VERSION,
                rollup: a.rollup.clone(),
                overall: a.overall,
                items: a.items.clone(),
                superio: map_superio_report(&a.superio, avail, reason),
                scanned_age_ms: a.scanned_at.elapsed().as_millis() as u64,
                generation: a.generation,
            },
        ),
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "hardware assessment is temporarily unavailable — retry",
            ),
        ),
    }
}

/// Run ONE passive hardware assessment from the live, read-only inventory — the
/// single expensive scan (cache snapshot + `/sys/class/hwmon` walk + `runtime.toml`
/// read + Super-I/O detect) that every readiness / Super-I/O consumer shares
/// (DEC-207), so the work runs once instead of three times. Never mutates the
/// system. Invoked only via [`ensure_assessment`] on the blocking pool; keep it
/// OFF the 1 Hz poll path (that path only clones the cached
/// [`crate::hwmon::readiness::ReadinessRollup`]).
fn compute_hardware_assessment(state: &AppState) -> crate::hwmon::readiness::HardwareAssessment {
    let now = std::time::Instant::now();
    let snap = state.cache.snapshot();
    let classified = classify_cache_sensors(&snap, now);

    let cpu_sensor_count = classified
        .iter()
        .filter(|(_, c)| is_cpu_class(c.class))
        .count();
    let unknown_sensor_count = classified
        .iter()
        .filter(|(_, c)| c.class == TempClass::UnknownTemp)
        .count();
    let default_cpu_confident =
        select_default_cpu(classified.iter().map(|(s, c)| (s.id.as_str(), c)))
            .map(|r| r.confidence == Confidence::High);

    // PWM header counts (structural) — read the controller's discovered set.
    let (pwm_total, pwm_writable) = match &state.hwmon_controller {
        Some(controller) => {
            let ctrl = controller.lock();
            let headers = ctrl.headers();
            (
                headers.len(),
                headers.iter().filter(|h| h.is_writable).count(),
            )
        }
        None => (0, 0),
    };

    let monitor_only_fan_count = discover_monitor_only_fans(std::path::Path::new(HWMON_SYSFS_ROOT))
        .map(|v| v.len())
        .unwrap_or(0);

    // Persisted selections (Phase 5): present iff the stored id is in the live
    // set — a set-but-absent selection drives the readiness "missing" items.
    let runtime = crate::runtime_config::RuntimeConfig::load_from(&state.runtime_config_path);
    let selected_cpu_present = runtime
        .preferred_cpu_sensor()
        .map(|id| classified.iter().any(|(s, _)| s.id == id));
    let selected_mb_present = runtime
        .preferred_mb_sensor()
        .map(|id| classified.iter().any(|(s, _)| s.id == id));

    let inputs = ReadinessInputs {
        cpu_sensor_count,
        default_cpu_confident,
        pwm_total,
        pwm_writable,
        monitor_only_fan_count,
        unavailable_sensor_count: snap.unavailable_sensors.len(),
        unknown_sensor_count,
        selected_cpu_present,
        selected_mb_present,
    };

    let items = build_readiness(&inputs);
    // DEC-202/207: enrich with passive Super-I/O detection, reusing this snapshot
    // so every consumer observes the same chips — and KEEP the report on the
    // assessment (the Super-I/O view reads it instead of running its own scan).
    let superio = detect_superio_from(state, &snap, now);
    crate::hwmon::readiness::HardwareAssessment::from_parts(items, superio)
}

// ── Super-I/O detection endpoint (DEC-202) ──────────────────────────

/// GET /inventory/superio — passive Super-I/O chip detection report.
///
/// Read-only and passive: composes the DMI board table, bound hwmon chips,
/// `/proc/modules`, `/dev/kmsg`, and ACPI I/O-port conflicts into a per-chip
/// presence report with allowlisted "load this driver" recommendations. Never
/// probes I/O ports, loads modules, or writes hardware. Runs on the blocking
/// pool (sysfs/procfs reads). Gated purely by 404 on older daemons, mirroring
/// the other `/inventory/*` endpoints.
pub async fn superio_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Serve the Super-I/O report from the shared assessment (DEC-207): a passive
    // GET reuses the recent readiness/combined scan within the coalescing TTL
    // instead of running its own detection. GET is passive only — report whether
    // the active probe *could* run, but never touch a port here (the POST below).
    let (avail, reason) = superio_probe::port_probe_available(state.allow_port_probe);
    match ensure_assessment(state, false).await {
        Some(a) => json_ok(
            StatusCode::OK,
            map_superio_report(&a.superio, avail, reason),
        ),
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "hardware assessment is temporarily unavailable — retry",
            ),
        ),
    }
}

/// Run the passive detector against the live cache. Shared by the Super-I/O
/// endpoint and the readiness enrichment so they observe the same chips.
fn detect_superio_from(
    state: &AppState,
    snap: &DaemonState,
    now: std::time::Instant,
) -> superio::SuperIoReport {
    let bound = gather_bound_chips(state, snap, now);
    superio::detect_superio(&superio::SysfsSuperIoEvidence::new(bound))
}

/// Collect the currently-bound hwmon chips from the live cache: PWM headers
/// (authoritative `device_id`) unioned with sensor-only chips (so a
/// monitoring-only Super-I/O chip with no writable `pwmN` is still seen).
fn gather_bound_chips(
    state: &AppState,
    snap: &DaemonState,
    now: std::time::Instant,
) -> Vec<superio::BoundChip> {
    let header_chips: Vec<(String, String)> = match &state.hwmon_controller {
        Some(controller) => controller
            .lock()
            .headers()
            .iter()
            .map(|h| (h.chip_name.clone(), h.device_id.clone()))
            .collect(),
        None => Vec::new(),
    };
    let sensor_chips: Vec<String> = build_sensor_entries(snap, now)
        .into_iter()
        .map(|s| s.chip_name)
        .collect();
    merge_bound_chips(header_chips, sensor_chips)
}

/// Deduplicate `(chip_name, device_id)` PWM-header chips and sensor-only chip
/// names into one bound-chip set, keyed by lowercased chip name. A header's
/// `device_id` is authoritative; a sensor-only chip (no header) uses its own
/// name as a stand-in `device_id` (the detector keys on chip name — no consumer
/// needs a real I/O address here). Deterministic order (BTreeMap) for stable
/// output.
fn merge_bound_chips(
    header_chips: Vec<(String, String)>,
    sensor_chips: Vec<String>,
) -> Vec<superio::BoundChip> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for name in sensor_chips {
        let key = name.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        map.entry(key.clone()).or_insert(key);
    }
    for (name, device_id) in header_chips {
        let key = name.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        map.insert(key, device_id); // header device_id wins
    }
    map.into_iter()
        .map(|(chip_name, device_id)| superio::BoundChip {
            chip_name,
            device_id,
        })
        .collect()
}

fn map_superio_report(
    report: &superio::SuperIoReport,
    port_probe_available: bool,
    port_probe_reason: String,
) -> SuperIoResponse {
    SuperIoResponse {
        api_version: API_VERSION,
        arch_supported: report.arch_supported,
        chips: report.chips.iter().map(map_superio_chip).collect(),
        acpi_conflict_drivers: report.acpi_conflict_drivers.clone(),
        notes: report.notes.clone(),
        port_probe_available,
        port_probe_reason,
    }
}

fn map_superio_chip(c: &superio::SuperIoChip) -> SuperIoChipEntry {
    SuperIoChipEntry {
        chip_name: c.chip_name.clone(),
        vendor: c.vendor.to_string(),
        evidence: c.evidence.iter().map(|e| e.to_string()).collect(),
        confidence: c.confidence.to_string(),
        bound_driver: c.bound_driver.clone(),
        expected_module: c.expected_module.clone(),
        module_loaded: c.module_loaded,
        hwmon_present: c.hwmon_present,
        recommendation: c
            .recommendation
            .as_ref()
            .map(|r| SuperIoRecommendationEntry {
                module: r.module.clone(),
                in_mainline: r.in_mainline,
                load_hint: r.load_hint.clone(),
                reason: r.reason.clone(),
                risk_notes: r.risk_notes.clone(),
            }),
        caveats: c.caveats.clone(),
    }
}

// ── Active port probe (DEC-203, opt-in) ─────────────────────────────

/// POST /inventory/superio/probe — the opt-in active `/dev/port` probe.
///
/// A deliberate, one-shot action (never polled): identifies an UNBOUND
/// Super-I/O chip by reading its config port, so the user can be told which
/// driver to load. Gated by `[detection] allow_port_probe` + `CAP_SYS_RAWIO`;
/// refuses to touch a port claimed by a bound driver or ACPI. Returns the same
/// `SuperIoResponse` shape, enriched with any probe-detected chips.
pub async fn superio_probe_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Base passive report from the shared assessment (DEC-207) — the probe never
    // triggers its own passive scan; it only appends any actively-probed chips.
    let base = ensure_assessment(state.clone(), false)
        .await
        .map(|a| a.superio.clone());
    match tokio::task::spawn_blocking(move || build_superio_probe_response(&state, base)).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorEnvelope::internal(format!("superio probe task failed: {e}")),
        ),
    }
}

/// Serializes active probes (single-flight) and records the last run time
/// (cooldown). The 0666 socket (DEC-049) means any local client could otherwise
/// loop the probe, and some firmware reacts poorly to rapid config-mode cycles
/// (SEC review, DEC-203).
static PROBE_GATE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
const PROBE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);

fn build_superio_probe_response(
    state: &AppState,
    base: Option<superio::SuperIoReport>,
) -> (StatusCode, Json<serde_json::Value>) {
    let now = std::time::Instant::now();
    let snap = state.cache.snapshot();
    // Reuse the shared passive report as the base (DEC-207); fall back to a fresh
    // detection only if the assessment was unavailable (rare).
    let mut report = base.unwrap_or_else(|| detect_superio_from(state, &snap, now));

    if !state.allow_port_probe {
        let (_avail, reason) = superio_probe::port_probe_available(false);
        report
            .notes
            .push(format!("Active port probe not run: {reason}"));
        return json_ok(StatusCode::OK, map_superio_report(&report, false, reason));
    }

    // Single-flight + cooldown: holding this lock across the probe serializes
    // concurrent probes; the timestamp rate-limits repeated ones.
    let mut gate = PROBE_GATE.lock().unwrap_or_else(|e| e.into_inner());
    if gate.is_some_and(|last| last.elapsed() < PROBE_COOLDOWN) {
        report.notes.push(
            "Active port probe is on a brief cooldown — try again in a few seconds.".to_string(),
        );
        return json_ok(
            StatusCode::OK,
            map_superio_report(&report, true, "available".to_string()),
        );
    }

    // Open /dev/port ONCE — this doubles as the availability check (no TOCTOU
    // double-open, SEC F5). A failure here is reported as unavailable.
    let reader = match superio_probe::port_probe_open() {
        Ok(r) => r,
        Err(reason) => {
            report
                .notes
                .push(format!("Active port probe not run: {reason}"));
            return json_ok(StatusCode::OK, map_superio_report(&report, false, reason));
        }
    };

    match safe_probe_bases(state, &snap, now) {
        Err(skip) => report
            .notes
            .push(format!("Active port probe skipped: {skip}")),
        Ok(bases) if bases.is_empty() => report.notes.push(
            "Active port probe: both Super-I/O config ports are reserved (ACPI); skipped."
                .to_string(),
        ),
        Ok(bases) => {
            let probed = superio_probe::probe_ports(&reader, &bases);
            if probed.is_empty() {
                report.notes.push(
                    "Active port probe found no unbound Super-I/O chip at 0x2E/0x4E.".to_string(),
                );
            }
            for p in &probed {
                let chip = probed_to_superio_chip(p);
                // Fold a probe hit into an existing same-name passive card
                // (union the PortProbe evidence) rather than emitting a duplicate
                // — the same physical chip can surface both passively (DMI/kmsg,
                // while unbound) and via the active probe (DEC-207). Distinct
                // chips (different names) still get their own card.
                if let Some(existing) = report
                    .chips
                    .iter_mut()
                    .find(|c| c.chip_name.eq_ignore_ascii_case(&chip.chip_name))
                {
                    if !existing.evidence.contains(&superio::Evidence::PortProbe) {
                        existing.evidence.push(superio::Evidence::PortProbe);
                    }
                } else {
                    report.chips.push(chip);
                }
            }
        }
    }

    *gate = Some(std::time::Instant::now());
    json_ok(
        StatusCode::OK,
        map_superio_report(&report, true, "available".to_string()),
    )
}

/// The Super-I/O config bases (0x2E/0x4E) safe to probe. Thin wrapper over the
/// pure [`pick_probe_bases`] that gathers the live inputs.
fn safe_probe_bases(
    state: &AppState,
    snap: &DaemonState,
    now: std::time::Instant,
) -> Result<Vec<u16>, String> {
    let names: Vec<String> = gather_bound_chips(state, snap, now)
        .into_iter()
        .map(|c| c.chip_name)
        .collect();
    let ioports = std::fs::read_to_string("/proc/ioports").ok();
    pick_probe_bases(&names, ioports.as_deref())
}

/// Decide which config bases are safe to probe (pure, so it is unit-tested).
///
/// - `ioports = None` (the `/proc/ioports` read failed) ⇒ **refuse** rather than
///   probe blind — the driver/ACPI port-claim fence must never silently vanish
///   (SEC F1).
/// - Refuse ALL probing if any recognized Super-I/O chip is already bound
///   (probing would race its driver, DEC-203). Note: an unrecognized bound chip
///   is caught only if it reserves the port in `/proc/ioports` (SEC F2 residual).
/// - Drop any base reserved in `/proc/ioports`.
fn pick_probe_bases(
    bound_chip_names: &[String],
    ioports: Option<&str>,
) -> Result<Vec<u16>, String> {
    let Some(ioports) = ioports else {
        return Err(
            "/proc/ioports could not be read — refusing to probe without the \
                    driver/ACPI port-claim check"
                .to_string(),
        );
    };
    if bound_chip_names
        .iter()
        .any(|n| crate::hwmon::chip_db::expected_driver(n) != "unknown")
    {
        return Err(
            "a Super-I/O driver is already bound — refusing to probe its config port".to_string(),
        );
    }
    Ok(superio_probe::SIO_BASES
        .iter()
        .copied()
        .filter(|&b| !superio_probe::base_claimed(ioports, b))
        .collect())
}

/// Convert a port-probe hit into a `SuperIoChip` (unbound, evidence = PortProbe)
/// with a load recommendation. ITE chips get a precise module + DKMS status via
/// `chip_db`; the Nuvoton/Winbond family is reported at vendor level.
fn probed_to_superio_chip(p: &superio_probe::ProbedChip) -> superio::SuperIoChip {
    let (chip_name, expected_module, in_mainline) = match (&p.chip_name, p.vendor) {
        (Some(name), _) => (
            name.clone(),
            crate::hwmon::chip_db::expected_driver(name).to_string(),
            crate::hwmon::chip_db::chip_driver_in_mainline(name),
        ),
        (None, superio::SuperIoVendor::Nuvoton) => (
            format!("Nuvoton/Winbond Super-I/O (DEVID 0x{:04x})", p.devid),
            "nct6775".to_string(),
            true,
        ),
        (None, _) => (
            format!("Super-I/O (DEVID 0x{:04x})", p.devid),
            "unknown".to_string(),
            false,
        ),
    };

    let reason = format!(
        "An active port probe found this chip at base 0x{:04x} (DEVID 0x{:04x}) with no driver \
         bound.",
        p.base, p.devid
    );
    let recommendation = if expected_module == "unknown" {
        None
    } else {
        let load_hint = if p.vendor == superio::SuperIoVendor::Nuvoton {
            "Load the `nct6775` driver (or `w83627ehf` for a genuine Winbond chip): `sudo modprobe \
             nct6775`, or add it to /etc/modules-load.d/. A reboot or module reload may be needed."
                .to_string()
        } else if in_mainline {
            format!(
                "Enable it at boot: `echo {expected_module} | sudo tee \
                 /etc/modules-load.d/{expected_module}.conf`, or load it now with `sudo modprobe \
                 {expected_module}`."
            )
        } else {
            "This ITE chip needs the out-of-tree it87-dkms-git driver — install it, then load \
             `it87`. Do not pass force_id."
                .to_string()
        };
        Some(superio::SuperIoRecommendation {
            module: expected_module.clone(),
            in_mainline,
            load_hint,
            reason: reason.clone(),
            risk_notes: Vec::new(),
        })
    };
    let caveats = if expected_module == "unknown" {
        vec![format!(
            "Unrecognized Super-I/O chip (vendor {}, DEVID 0x{:04x}).",
            p.vendor, p.devid
        )]
    } else {
        Vec::new()
    };

    superio::SuperIoChip {
        chip_name,
        vendor: p.vendor,
        evidence: vec![superio::Evidence::PortProbe],
        confidence: Confidence::High,
        bound_driver: None,
        expected_module,
        module_loaded: false,
        hwmon_present: false,
        recommendation,
        caveats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensor(id: &str, chip: &str, label: &str) -> SensorEntry {
        SensorEntry {
            id: id.into(),
            kind: "cpu_temp".into(),
            label: label.into(),
            value_c: 50.0,
            source: "hwmon".into(),
            age_ms: 0,
            rate_c_per_s: None,
            session_min_c: None,
            session_max_c: None,
            chip_name: chip.into(),
            temp_type: None,
            thresholds: None,
            control_eligible: true,
        }
    }

    fn classified(pairs: &[(&str, &str, &str)]) -> Vec<(SensorEntry, TempClassification)> {
        pairs
            .iter()
            .map(|(id, chip, label)| {
                (
                    sensor(id, chip, label),
                    classify_temp_sensor(chip, label, ""),
                )
            })
            .collect()
    }

    #[test]
    fn default_cpu_user_preference_wins_when_present() {
        let c = classified(&[
            ("hwmon:k10temp:x:Tctl", "k10temp", "Tctl"),
            ("hwmon:coretemp:x:Package", "coretemp", "Package id 0"),
        ]);
        let d = build_default_cpu(&c, Some("hwmon:coretemp:x:Package")).unwrap();
        assert_eq!(d.sensor_id, "hwmon:coretemp:x:Package");
        assert_eq!(d.source, "user");
    }

    #[test]
    fn default_cpu_falls_back_to_auto_when_preference_absent() {
        // A set-but-absent preference must NOT be blindly applied — fall back to
        // the auto pick (the readiness model flags the stale selection).
        let c = classified(&[("hwmon:k10temp:x:Tctl", "k10temp", "Tctl")]);
        let d = build_default_cpu(&c, Some("hwmon:gone:x:Tctl")).unwrap();
        assert_eq!(d.sensor_id, "hwmon:k10temp:x:Tctl");
        assert_eq!(d.source, "auto");
    }

    #[test]
    fn default_cpu_auto_when_no_preference() {
        let c = classified(&[("hwmon:k10temp:x:Tctl", "k10temp", "Tctl")]);
        let d = build_default_cpu(&c, None).unwrap();
        assert_eq!(d.source, "auto");
        assert_eq!(d.sensor_id, "hwmon:k10temp:x:Tctl");
    }

    #[test]
    fn default_cpu_none_when_no_cpu_sensors() {
        let c = classified(&[("hwmon:nct6798:x:SYSTIN", "nct6798", "SYSTIN")]);
        assert!(build_default_cpu(&c, None).is_none());
    }

    #[test]
    fn merge_bound_chips_prefers_header_device_id_and_unions_sensors() {
        let headers = vec![("nct6799".to_string(), "isa-0290".to_string())];
        // Sensor list includes the same chip (mixed case) plus a monitoring-only
        // chip that has no PWM header.
        let sensors = vec!["NCT6799".to_string(), "smsc47b397".to_string()];
        let got = merge_bound_chips(headers, sensors);
        assert_eq!(got.len(), 2, "case-dupe collapses; sensor-only chip added");
        let nct = got.iter().find(|c| c.chip_name == "nct6799").unwrap();
        assert_eq!(
            nct.device_id, "isa-0290",
            "header device_id wins over sensor dup"
        );
        let smsc = got.iter().find(|c| c.chip_name == "smsc47b397").unwrap();
        assert_eq!(
            smsc.device_id, "smsc47b397",
            "sensor-only chip uses name as device_id"
        );
    }

    #[test]
    fn merge_bound_chips_skips_blank_names() {
        let got = merge_bound_chips(vec![(String::new(), "x".into())], vec!["   ".into()]);
        assert!(got.is_empty());
    }

    #[test]
    fn map_superio_report_stringifies_enums_and_maps_recommendation() {
        use crate::hwmon::superio::{
            Evidence, SuperIoChip, SuperIoRecommendation, SuperIoReport, SuperIoVendor,
        };
        let report = SuperIoReport {
            arch_supported: true,
            chips: vec![SuperIoChip {
                chip_name: "it8688".into(),
                vendor: SuperIoVendor::Ite,
                evidence: vec![Evidence::DmiBoardTable, Evidence::KernelLog],
                confidence: crate::hwmon::classify::Confidence::High,
                bound_driver: None,
                expected_module: "it87".into(),
                module_loaded: false,
                hwmon_present: false,
                recommendation: Some(SuperIoRecommendation {
                    module: "it87".into(),
                    in_mainline: false,
                    load_hint: "install it87-dkms-git".into(),
                    reason: "board lists it8688".into(),
                    risk_notes: vec!["risk".into()],
                }),
                caveats: vec![],
            }],
            acpi_conflict_drivers: vec!["it87".into()],
            notes: vec!["present != control".into()],
        };
        let dto = map_superio_report(&report, false, "disabled".to_string());
        assert!(dto.arch_supported);
        assert!(!dto.port_probe_available);
        assert_eq!(dto.port_probe_reason, "disabled");
        assert_eq!(dto.chips.len(), 1);
        let c = &dto.chips[0];
        assert_eq!(c.vendor, "ite");
        assert_eq!(c.evidence, vec!["dmi_board_table", "kernel_log"]);
        assert_eq!(c.confidence, "high");
        let rec = c.recommendation.as_ref().unwrap();
        assert_eq!(rec.module, "it87");
        assert!(!rec.in_mainline);
        assert_eq!(rec.risk_notes, vec!["risk".to_string()]);
        // The DTO serialises cleanly (skip_serializing_if honoured).
        let v = serde_json::to_value(&dto).unwrap();
        assert_eq!(v["arch_supported"], serde_json::json!(true));
        assert_eq!(v["chips"][0]["vendor"], serde_json::json!("ite"));
    }

    #[test]
    fn probed_ite_chip_maps_to_dkms_recommendation() {
        let p = superio_probe::ProbedChip {
            base: 0x2e,
            vendor: superio::SuperIoVendor::Ite,
            devid: 0x8688,
            chip_name: Some("it8688".to_string()),
        };
        let chip = probed_to_superio_chip(&p);
        assert_eq!(chip.chip_name, "it8688");
        assert_eq!(chip.evidence, vec![superio::Evidence::PortProbe]);
        assert!(!chip.hwmon_present);
        let rec = chip
            .recommendation
            .expect("unbound probed chip → recommendation");
        assert_eq!(rec.module, "it87");
        assert!(!rec.in_mainline, "it8688 is DKMS-only");
        assert!(rec.load_hint.contains("it87-dkms-git"));
        assert!(rec.reason.contains("active port probe"));
    }

    #[test]
    fn probed_nuvoton_family_maps_to_nct6775_at_vendor_level() {
        let p = superio_probe::ProbedChip {
            base: 0x4e,
            vendor: superio::SuperIoVendor::Nuvoton,
            devid: 0xd592,
            chip_name: None,
        };
        let chip = probed_to_superio_chip(&p);
        assert!(chip.chip_name.contains("DEVID 0xd592"));
        let rec = chip.recommendation.expect("recommendation");
        assert_eq!(rec.module, "nct6775");
        assert!(rec.in_mainline);
        assert!(rec.load_hint.contains("nct6775"));
    }

    #[test]
    fn probed_unknown_vendor_gets_no_recommendation_but_a_caveat() {
        let p = superio_probe::ProbedChip {
            base: 0x2e,
            vendor: superio::SuperIoVendor::Unknown,
            devid: 0x1234,
            chip_name: None,
        };
        let chip = probed_to_superio_chip(&p);
        assert_eq!(chip.expected_module, "unknown");
        assert!(chip.recommendation.is_none());
        assert!(chip.caveats.iter().any(|c| c.contains("Unrecognized")));
    }

    // ── The race gate (SEC review): pick_probe_bases ──

    #[test]
    fn pick_probe_bases_refuses_when_a_recognized_chip_is_bound() {
        let r = pick_probe_bases(&["nct6799".to_string()], Some(""));
        assert!(r.unwrap_err().contains("already bound"));
    }

    #[test]
    fn pick_probe_bases_refuses_when_ioports_unreadable() {
        // None = /proc/ioports read failed → refuse, don't probe blind (SEC F1).
        let r = pick_probe_bases(&[], None);
        assert!(r.unwrap_err().contains("ioports"));
    }

    #[test]
    fn pick_probe_bases_drops_a_base_reserved_in_ioports() {
        // No recognized SIO chip; 0x2e reserved → only 0x4e is probable.
        let bases =
            pick_probe_bases(&["k10temp".to_string()], Some("002e-002f : pnp 00:03\n")).unwrap();
        assert_eq!(bases, vec![0x4e]);
    }

    #[test]
    fn pick_probe_bases_allows_both_when_clean() {
        assert_eq!(pick_probe_bases(&[], Some("")).unwrap(), vec![0x2e, 0x4e]);
    }

    #[test]
    fn pick_probe_bases_unrecognized_bound_chip_passes_the_name_gate() {
        // A chip chip_db doesn't know passes the name gate (SEC F2 residual —
        // the /proc/ioports fence is the backstop for a port it actually holds).
        let bases = pick_probe_bases(&["brandnewchip99".to_string()], Some("")).unwrap();
        assert_eq!(bases, vec![0x2e, 0x4e]);
    }
}
