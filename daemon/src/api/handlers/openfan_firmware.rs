//! OpenFan firmware update endpoints (DEC-481).
//!
//! - `GET /fans/openfan/device` — the controller's identity and own reports,
//!   and whether an update could start now.
//! - `POST /fans/openfan/maintenance` — start one: 202 and a run id.
//! - `GET /fans/openfan/maintenance` — the current or most recent run.
//! - `DELETE /fans/openfan/maintenance` — stop one before the port is borrowed.
//!
//! The update itself is [`crate::openfan_maintenance`]; this module checks,
//! claims and starts it, and serves its record.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use super::{error_response, json_ok, AppState};
use crate::api::responses::*;
use crate::health::state::{MaintenanceRefusal, OpenFanLink};
use crate::openfan_maintenance::run::{ExpectedChannels, Opener, RunEnv, StageLimits};
use crate::openfan_maintenance::{
    journal, next_run_id, stage, CachedInfo, CancelOutcome, ClaimGuard, FirmwareClaim,
    MaintenanceRecord,
};
use crate::serial::protocol::{FW_INFO_OPCODE, HW_INFO_OPCODE, NUM_CHANNELS};
use crate::serial::usb_identity::{self as usb, TtyIdentity, UsbDevice};

/// How long a live `>05`/`>06` read made for `GET /fans/openfan/device` is
/// reused, so a client polling the route cannot keep the serial link busy.
const DEVICE_INFO_REUSE: Duration = Duration::from_secs(10);

/// The largest firmware file the GUI may describe: the plan's validator caps a
/// `.uf2` at 1 MiB.
const MAX_FIRMWARE_BYTES: u64 = 1024 * 1024;

/// `POST /fans/openfan/maintenance`.
#[derive(Debug, Deserialize)]
pub struct MaintenanceStartRequest {
    /// The USB serial the GUI read from `GET /fans/openfan/device` and showed
    /// the user. The update refuses a board that does not carry it.
    pub expected_usb_serial: String,
    pub firmware: FirmwareClaim,
}

/// A 409 with `details.reason`, retryable — the shape of every refusal here.
fn refused(reason: &str, message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    let mut e = ErrorEnvelope::validation(message);
    e.error.retryable = true;
    e.error.details = Some(serde_json::json!({ "reason": reason }));
    error_response(StatusCode::CONFLICT, &e)
}

fn bad_request(message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    error_response(StatusCode::BAD_REQUEST, &ErrorEnvelope::validation(message))
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Validate a start request, normalising hex to lower case.
fn validated(body: MaintenanceStartRequest) -> Result<(String, FirmwareClaim), String> {
    let serial = body.expected_usb_serial.trim().to_string();
    if serial.is_empty() || serial.len() > 64 || !serial.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err("expected_usb_serial must be 1-64 letters and digits".into());
    }
    let mut fw = body.firmware;
    fw.sha256 = fw.sha256.to_ascii_lowercase();
    if fw.sha256.len() != 64 || !is_hex(&fw.sha256) {
        return Err("firmware.sha256 must be 64 hexadecimal digits".into());
    }
    if fw.size == 0 || fw.size > MAX_FIRMWARE_BYTES || !fw.size.is_multiple_of(512) {
        return Err(format!(
            "firmware.size must be a multiple of 512 bytes, at most {MAX_FIRMWARE_BYTES}"
        ));
    }
    if let Some(d) = &mut fw.usb_config_descriptor_hex {
        *d = d.to_ascii_lowercase();
        if d.len() < 18 || d.len() > 1024 || !d.len().is_multiple_of(2) || !is_hex(d) {
            return Err(
                "firmware.usb_config_descriptor_hex must be 9 to 512 bytes of hexadecimal".into(),
            );
        }
    }
    if let Some(info) = &fw.info {
        if info.len() > 16 {
            return Err("firmware.info may carry at most 16 entries".into());
        }
        for (k, v) in info {
            let key_ok = !k.is_empty()
                && k.len() <= 32
                && k.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
            let value_ok = v.len() <= 64 && v.bytes().all(|b| (0x20..0x7f).contains(&b));
            if !key_ok || !value_ok {
                return Err(format!(
                    "firmware.info entry '{k}' is not a KEY:VALUE string"
                ));
            }
        }
    }
    Ok((serial, fw))
}

/// Where the update's handlers read sysfs and keep the journal, how a run
/// reopens the returned board, and the run's stage limits — the production
/// ones from [`MaintenanceIo::production`], a fixture's in tests.
pub(crate) struct MaintenanceIo {
    pub sys_root: PathBuf,
    pub journal_path: PathBuf,
    pub open: Opener,
    pub limits: StageLimits,
}

impl MaintenanceIo {
    fn production() -> Self {
        Self {
            sys_root: PathBuf::from(usb::SYSFS_ROOT),
            journal_path: journal::path(),
            open: real_opener(),
            limits: StageLimits::production(),
        }
    }
}

/// Read the board's identity and every RP2040 bootloader present, off the
/// runtime. Read-only sysfs; opens nothing.
async fn read_usb(sys: &Path, port: Option<String>) -> (Option<TtyIdentity>, Vec<UsbDevice>) {
    let sys = sys.to_path_buf();
    tokio::task::spawn_blocking(move || {
        (
            port.as_deref().and_then(|p| usb::tty_identity(&sys, p)),
            usb::bootloaders(&sys),
        )
    })
    .await
    .unwrap_or((None, Vec::new()))
}

/// Every reason an update could not start now, without claiming anything.
/// The POST decides again, atomically.
fn preview_refusals(
    state: &AppState,
    identity: Option<&TtyIdentity>,
    bootloaders: &[UsbDevice],
) -> Vec<OpenFanUpdateRefusal> {
    let mut out = Vec::new();
    let mut add = |reason: &str, message: String| {
        out.push(OpenFanUpdateRefusal {
            reason: reason.into(),
            message,
        })
    };
    if state.openfan().is_none() {
        add(
            "openfan_not_connected",
            "no OpenFan controller is connected".into(),
        );
        return out;
    }
    let (link, running, recovering, verify_held, emergency) = state.cache.read_with(|s| {
        (
            s.openfan_link,
            s.openfan_maintenance_running(),
            matches!(
                s.openfan_maintenance,
                Some(crate::health::state::OpenFanMaintenance::NeedsRecovery { .. })
            ),
            s.verify_in_progress,
            s.thermal_override_state.as_deref() == Some("emergency"),
        )
    });
    if running || state.openfan_maintenance.is_alive() {
        add(
            MaintenanceRefusal::MaintenanceActive.reason(),
            MaintenanceRefusal::MaintenanceActive.message(),
        );
    }
    if link != Some(OpenFanLink::Connected) || state.openfan_maintenance.lender().is_none() {
        let r = MaintenanceRefusal::LinkNotReady(link);
        add(r.reason(), r.message());
    } else if recovering {
        let r = MaintenanceRefusal::RecoveryPending;
        add(r.reason(), r.message());
    }
    if verify_held {
        let r = MaintenanceRefusal::DiagnosticActive;
        add(r.reason(), r.message());
    }
    if state.openfan_calibration.is_alive() {
        add(
            "calibration_active",
            "an OpenFan calibration is running — wait for it to finish".into(),
        );
    }
    if state.validation.is_recording() {
        add(
            "validation_recording",
            "a validation session is recording — stop it first".into(),
        );
    }
    if emergency {
        let r = MaintenanceRefusal::ThermalEmergency;
        add(r.reason(), r.message());
    }
    if !bootloaders.is_empty() {
        add("bootloader_present", bootloader_message(bootloaders));
    }
    if identity.is_none() {
        add(
            "usb_identity_unavailable",
            "the controller's USB identity could not be read from sysfs".into(),
        );
    }
    out
}

fn bootloader_message(bootloaders: &[UsbDevice]) -> String {
    let ports: Vec<&str> = bootloaders.iter().map(|d| d.port.as_str()).collect();
    format!(
        "a board is already in its USB bootloader (USB port {}) — unplug it or finish its update \
         first, so there is only one RPI-RP2 drive",
        ports.join(", ")
    )
}

/// `GET /fans/openfan/device`.
pub async fn openfan_device_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    describe_device(&state, Path::new(usb::SYSFS_ROOT)).await
}

async fn describe_device(state: &AppState, sys: &Path) -> (StatusCode, Json<serde_json::Value>) {
    let controller = state.openfan();
    let (link, running) = state.cache.read_with(|s| {
        (
            s.openfan_link_wire().map(str::to_string),
            s.openfan_maintenance_running(),
        )
    });
    let port = state.cache.openfan_port();
    let (identity, bootloaders) = read_usb(sys, port.clone()).await;

    // The board's own reports: through the controller, like any exchange, and
    // never while an update holds the port.
    let (hw_info, fw_info) = match (&controller, running) {
        (Some(ctrl), false) => {
            let slot = state.openfan_maintenance.clone();
            if let Some(c) = slot.cached_info(port.as_deref(), DEVICE_INFO_REUSE) {
                (c.hw_info, c.fw_info)
            } else {
                let ctrl = ctrl.clone();
                let read = tokio::task::spawn_blocking(move || {
                    let mut c = ctrl.lock();
                    let hw = c.read_info(HW_INFO_OPCODE).ok();
                    let fw = c.read_info(FW_INFO_OPCODE).ok();
                    (hw, fw)
                })
                .await
                .unwrap_or((None, None));
                let to_map = |p: Option<Vec<(String, String)>>| {
                    p.map(|p| p.into_iter().collect::<BTreeMap<_, _>>())
                };
                let (hw, fw) = (to_map(read.0), to_map(read.1));
                slot.store_info(CachedInfo {
                    at: Instant::now(),
                    port: port.clone(),
                    hw_info: hw.clone(),
                    fw_info: fw.clone(),
                });
                (hw, fw)
            }
        }
        _ => (None, None),
    };

    let refusals = preview_refusals(state, identity.as_ref(), &bootloaders);
    json_ok(
        StatusCode::OK,
        OpenFanDeviceResponse {
            api_version: API_VERSION,
            present: controller.is_some(),
            link,
            port,
            usb: identity.as_ref().map(|i| i.device.clone()),
            interface_number: identity.as_ref().map(|i| i.interface_number),
            hw_info,
            fw_info,
            update_available: refusals.is_empty(),
            update_refusals: refusals,
        },
    )
}

/// The channels whose settings must land before control counts as restored:
/// every channel while the thermal force is active, otherwise the active
/// profile's OpenFan channels.
fn expected_channels(state: &Arc<AppState>) -> ExpectedChannels {
    let cache = state.cache.clone();
    let profile = state.active_profile.clone();
    Arc::new(move || {
        if cache.read_with(|s| s.thermal_override_state.as_deref() == Some("emergency")) {
            return (0..NUM_CHANNELS).collect();
        }
        let mut channels: Vec<u8> = profile
            .lock()
            .as_ref()
            .map(|p| {
                p.controls
                    .iter()
                    .flat_map(|c| &c.members)
                    .filter(|m| m.source == "openfan")
                    .filter_map(|m| crate::serial::openfan_channel_of(&m.member_id).ok())
                    .filter(|ch| *ch < NUM_CHANNELS)
                    .collect()
            })
            .unwrap_or_default();
        channels.sort_unstable();
        channels.dedup();
        channels
    })
}

/// Release every override on a control that drives an OpenFan channel, and
/// every identify hold on one, as an update starts — logged, never silent.
fn release_openfan_overrides(state: &AppState, run_id: &str) {
    let controls: HashSet<String> = state
        .active_profile
        .lock()
        .as_ref()
        .map(|p| {
            p.controls
                .iter()
                .filter(|c| c.members.iter().any(|m| m.source == "openfan"))
                .map(|c| c.id.clone())
                .collect()
        })
        .unwrap_or_default();
    let (controls, fans) = state
        .override_table
        .lock()
        .release_for_maintenance(&controls, |id| {
            crate::serial::openfan_channel_of(id).is_ok()
        });
    if !controls.is_empty() {
        log::info!(
            "OpenFan firmware update {run_id}: released the manual override on {}",
            controls.join(", ")
        );
    }
    if !fans.is_empty() {
        log::info!(
            "OpenFan firmware update {run_id}: released the identify hold on {}",
            fans.join(", ")
        );
    }
}

/// The production opener for the returned board's serial interface.
fn real_opener() -> Opener {
    Arc::new(|path: &str, timeout: Duration| {
        crate::serial::real_transport::RealSerialTransport::open(path, timeout)
            .map(|t| Box::new(t) as Box<dyn crate::serial::transport::SerialTransport + Send>)
    })
}

/// `POST /fans/openfan/maintenance` — check, claim, and start an update.
pub async fn openfan_maintenance_start_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MaintenanceStartRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    start_update(&state, body, MaintenanceIo::production()).await
}

async fn start_update(
    state: &Arc<AppState>,
    body: MaintenanceStartRequest,
    io: MaintenanceIo,
) -> (StatusCode, Json<serde_json::Value>) {
    if *state.openfan_runtime.shutdown.borrow() || state.openfan_maintenance.is_closed() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "the daemon is shutting down — no firmware update was started",
            ),
        );
    }
    let (serial, firmware) = match validated(body) {
        Ok(v) => v,
        Err(e) => return bad_request(e),
    };
    let Some(controller) = state.openfan() else {
        return refused(
            "openfan_not_connected",
            "no OpenFan controller is connected",
        );
    };
    // One run's task at a time, for its whole life.
    let Some(alive) = state.openfan_maintenance.claim() else {
        let r = MaintenanceRefusal::MaintenanceActive;
        return refused(r.reason(), r.message());
    };
    if state.openfan_calibration.is_alive() {
        return refused(
            "calibration_active",
            "an OpenFan calibration is running — wait for it to finish",
        );
    }
    let Some(lender) = state.openfan_maintenance.lender() else {
        let r = MaintenanceRefusal::LinkNotReady(state.cache.openfan_link());
        return refused(r.reason(), r.message());
    };

    let (identity, bootloaders) = read_usb(&io.sys_root, state.cache.openfan_port()).await;
    if !bootloaders.is_empty() {
        return refused("bootloader_present", bootloader_message(&bootloaders));
    }
    let Some(identity) = identity else {
        return refused(
            "usb_identity_unavailable",
            "the controller's USB identity could not be read from sysfs",
        );
    };
    if identity.device.serial.as_deref() != Some(serial.as_str()) {
        return refused(
            "identity_mismatch",
            format!(
                "the connected board's USB serial is {}, not the {serial} the update was \
                 prepared for",
                identity.device.serial.as_deref().unwrap_or("unknown")
            ),
        );
    }

    // [SAFETY] The claim: one decision under the lock the diagnostic pause
    // uses. From here no diagnostic, calibration, rescan, identify or override
    // on an OpenFan channel starts; the run suspends OpenFan writes when it
    // parks the channels.
    let run_id = next_run_id();
    if let Err(r) = state
        .cache
        .try_begin_openfan_maintenance(&run_id, stage::PREPARING)
    {
        return refused(r.reason(), r.message());
    }
    let claim = ClaimGuard::new(state.cache.clone(), run_id.clone());
    // After the claim, never before: a session start checks the claim under
    // the recorder's slot lock, which `is_recording` takes too.
    if state.validation.is_recording() {
        claim.release(None);
        return refused(
            "validation_recording",
            "a validation session is recording — stop it first",
        );
    }
    release_openfan_overrides(state, &run_id);
    state
        .openfan_maintenance
        .set_record(MaintenanceRecord::new(run_id.clone(), serial, firmware));

    let env = RunEnv {
        cache: state.cache.clone(),
        slot: state.openfan_maintenance.clone(),
        controller,
        lender,
        sys_root: io.sys_root,
        journal_path: io.journal_path,
        limits: io.limits,
        serial_timeout: state.openfan_runtime.timeout,
        open: io.open,
        expected_channels: expected_channels(state),
        shutdown: state.openfan_runtime.shutdown.clone(),
    };
    // The parts stay out here until the closure takes them, so a registration
    // refused by a shutdown that began a moment ago releases the claim
    // deliberately rather than through the guard's "never released" path.
    let mut parts = Some((env, claim, alive));
    let registered = state.openfan_maintenance.register(|| {
        let (env, claim, alive) = parts
            .take()
            .expect("register calls its closure at most once");
        crate::openfan_maintenance::run::spawn(env, claim, alive)
    });
    if !registered {
        if let Some((_, claim, _)) = parts.take() {
            claim.release(None);
        }
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorEnvelope::hardware_unavailable(
                "the daemon is shutting down — no firmware update was started",
            ),
        );
    }
    log::info!("OpenFan firmware update {run_id} started");
    json_ok(
        StatusCode::ACCEPTED,
        OpenFanMaintenanceStartResponse {
            api_version: API_VERSION,
            run_id,
            stage: stage::PREPARING.into(),
        },
    )
}

/// `GET /fans/openfan/maintenance` — the current or most recent run.
pub async fn openfan_maintenance_status_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.openfan_maintenance.record() {
        Some(record) => json_ok(
            StatusCode::OK,
            OpenFanMaintenanceResponse {
                api_version: API_VERSION,
                record,
            },
        ),
        None => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no OpenFan firmware update has run"),
        ),
    }
}

/// `DELETE /fans/openfan/maintenance` — stop a run before the port is borrowed.
pub async fn openfan_maintenance_cancel_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let run_id = state
        .openfan_maintenance
        .record()
        .map(|r| r.run_id)
        .unwrap_or_default();
    match state.openfan_maintenance.request_cancel() {
        CancelOutcome::Requested => json_ok(
            StatusCode::ACCEPTED,
            OpenFanMaintenanceCancelResponse {
                api_version: API_VERSION,
                run_id,
                cancel_requested: true,
            },
        ),
        CancelOutcome::TooLate => {
            let mut e = ErrorEnvelope::validation(
                "the update is past the point where it can be stopped — the board has been asked \
                 to enter its bootloader",
            );
            e.error.details = Some(serde_json::json!({ "reason": "not_cancellable" }));
            error_response(StatusCode::CONFLICT, &e)
        }
        CancelOutcome::NotRunning => error_response(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::not_found("no OpenFan firmware update is running"),
        ),
    }
}

/// How often the startup watch re-reads sysfs.
const RECOVERY_WATCH_POLL: Duration = Duration::from_secs(2);

/// Whether the board a run was for is back on its USB port, running firmware.
fn board_back(sys: &Path, port: &str, serial: &str, interface: u8) -> bool {
    usb::device_at(sys, port)
        .is_some_and(|d| !d.is_rp2040_bootloader() && d.serial.as_deref() == Some(serial))
        && usb::tty_for(sys, port, interface).is_some()
}

/// Whether the board a run was for is still on its USB port, in either mode.
fn board_still_there(sys: &Path, port: &str, serial: &str) -> bool {
    usb::device_at(sys, port).is_some_and(|d| {
        d.is_rp2040_bootloader()
            || (d.vendor_id == usb::RP2040_VENDOR_ID && d.serial.as_deref() == Some(serial))
    })
}

/// At startup (DEC-481): load the journal into the slot, finishing a run the
/// last stop interrupted — never resuming it.
///
/// Returns the record to watch for when the last run left the board outside
/// normal control and boot adopted no controller. The `openfan` health entry
/// is then critical while that board is still on its USB port — in its
/// bootloader, or running firmware the daemon cannot talk to. A port that is
/// empty, or holds another device, means the board is gone, and a board that
/// is gone is reported the way any absent controller is.
pub fn recover_openfan_maintenance(
    state: &AppState,
    sys: &Path,
    journal_path: &Path,
) -> Option<MaintenanceRecord> {
    let recovered = journal::recover(journal_path)?;
    let record = recovered.record;
    state.openfan_maintenance.set_record(record.clone());
    let token = record
        .outcome
        .as_deref()
        .and_then(crate::openfan_maintenance::outcome::recovery_token)?;
    if state.openfan().is_some() {
        log::info!(
            "OpenFan firmware update {} had left the board outside normal control; it answers \
             again",
            record.run_id
        );
        return None;
    }
    let present = record
        .usb_port
        .as_deref()
        .is_some_and(|port| board_still_there(sys, port, &record.expected_usb_serial));
    if present {
        state.cache.restore_openfan_recovery(&record.run_id, token);
        log::warn!(
            "OpenFan firmware update {} left the board outside normal control ({token}) and it \
             is still on its USB port — OpenFan writes stay suspended until it answers",
            record.run_id
        );
    }
    Some(record)
}

/// At startup, after a run that left the board outside normal control: watch —
/// read-only — for that board to come back on its USB port, and adopt it
/// through the rescan path. The boot search and the post-boot loop give up
/// after a few minutes; a board left in its bootloader can come back much
/// later. Stops once a controller is adopted, or at shutdown.
pub async fn openfan_recovery_watch(
    state: Arc<AppState>,
    record: MaintenanceRecord,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let (Some(port), Some(interface)) = (record.usb_port.clone(), record.interface_number) else {
        return;
    };
    log::info!(
        "OpenFan firmware update {}: watching USB port {port} for the board to come back",
        record.run_id
    );
    let adopted = state.clone();
    watch_for_return(
        PathBuf::from(usb::SYSFS_ROOT),
        port,
        record.expected_usb_serial,
        interface,
        RECOVERY_WATCH_POLL,
        shutdown,
        move || adopted.openfan().is_some(),
        move || {
            let state = state.clone();
            async move {
                let _ = super::openfan_rescan_handler(State(state)).await;
            }
        },
    )
    .await;
}

/// The watch, with sysfs, the clock and the adoption injected.
///
/// One adoption attempt per return, once the board has stayed back for a
/// whole poll: a rescan opens every serial candidate (each open asserts DTR),
/// and new firmware that does not answer Control-OFC's commands would
/// otherwise have every one of them reopened every few seconds for as long as
/// the daemon runs. A board that goes away and comes back gets another
/// attempt; `POST /fans/openfan/rescan` is always there for one more.
// Eight: what to watch (four), how often, when to stop, and the two halves of
// "adopted" — the check and the attempt — injected so a test can count them.
#[allow(clippy::too_many_arguments)]
async fn watch_for_return<A, F>(
    sys: PathBuf,
    port: String,
    serial: String,
    interface: u8,
    poll: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    adopted: impl Fn() -> bool,
    mut attempt: A,
) where
    A: FnMut() -> F,
    F: std::future::Future<Output = ()>,
{
    // Polls in a row that found the board back; the attempt is made on the second.
    let mut back_for: u32 = 0;
    loop {
        if adopted() || *shutdown.borrow() {
            return;
        }
        let (root, p, s) = (sys.clone(), port.clone(), serial.clone());
        let back = tokio::task::spawn_blocking(move || board_back(&root, &p, &s, interface))
            .await
            .unwrap_or(false);
        back_for = if back { back_for.saturating_add(1) } else { 0 };
        if back_for == 2 {
            log::info!("OpenFan board {serial} is back on USB port {port} — adopting it");
            attempt().await;
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(poll) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(serial: &str, sha: &str, size: u64) -> MaintenanceStartRequest {
        MaintenanceStartRequest {
            expected_usb_serial: serial.into(),
            firmware: FirmwareClaim {
                sha256: sha.into(),
                size,
                usb_config_descriptor_hex: None,
                info: None,
            },
        }
    }

    #[test]
    fn a_well_formed_request_is_normalised() {
        let (serial, fw) =
            validated(request(" DE615CB14721492C ", &"AB".repeat(32), 107_520)).expect("valid");
        assert_eq!(serial, "DE615CB14721492C");
        assert_eq!(fw.sha256, "ab".repeat(32));
    }

    #[test]
    fn malformed_requests_are_refused_before_anything_happens() {
        let sha = "ab".repeat(32);
        for (r, why) in [
            (request("", &sha, 512), "empty serial"),
            (request("DE61/../x", &sha, 512), "serial with a path"),
            (request("S", "abc", 512), "short sha"),
            (request("S", &"zz".repeat(32), 512), "non-hex sha"),
            (request("S", &sha, 0), "empty file"),
            (request("S", &sha, 513), "not whole blocks"),
            (request("S", &sha, MAX_FIRMWARE_BYTES + 512), "too large"),
        ] {
            assert!(validated(r).is_err(), "{why}");
        }
        let mut odd = request("S", &sha, 512);
        odd.firmware.usb_config_descriptor_hex = Some("0902abc".into());
        assert!(validated(odd).is_err(), "odd-length descriptor");
        let mut lower = request("S", &sha, 512);
        lower.firmware.info = Some(BTreeMap::from([("hw_rev".into(), "03".into())]));
        assert!(
            validated(lower).is_err(),
            "a lower-case key is not a firmware string"
        );
    }

    #[test]
    fn the_startup_watch_knows_the_board_only_by_serial_on_its_port() {
        use crate::serial::usb_identity::fixture::Sysfs;
        let sys = Sysfs::new();
        assert!(!board_back(sys.root(), "8-8", "DE615CB14721492C", 0));
        sys.add_bootloader("8-8", "sdb");
        assert!(
            !board_back(sys.root(), "8-8", "DE615CB14721492C", 0),
            "still in its bootloader"
        );
        sys.remove_device("8-8");
        sys.add_openfan("8-8", "AAAAAAAAAAAAAAAA", 0x80, "ttyACM1", "ttyACM2");
        assert!(
            !board_back(sys.root(), "8-8", "DE615CB14721492C", 0),
            "another board on that port"
        );
        sys.remove_device("8-8");
        sys.add_openfan("8-8", "DE615CB14721492C", 0x80, "ttyACM1", "ttyACM2");
        assert!(board_back(sys.root(), "8-8", "DE615CB14721492C", 0));
    }

    // ── The handlers against a board on a fixture ────────────────────

    use crate::health::state::OpenFanMaintenance;
    use crate::openfan_maintenance::journal::JOURNAL_FILE;
    use crate::openfan_maintenance::{outcome, STATE_FINISHED};
    use crate::serial::usb_identity::fixture::Sysfs;
    use std::collections::VecDeque;

    const SERIAL: &str = "DE615CB14721492C";
    const TTY: &str = "/dev/ttyACM91";

    /// The controller's side of the link: answers what the firmware answers,
    /// recording every frame.
    struct Answering {
        frames: Arc<parking_lot::Mutex<Vec<String>>>,
        replies: VecDeque<String>,
    }

    impl crate::serial::transport::SerialTransport for Answering {
        fn write_line(&mut self, data: &str) -> Result<(), crate::error::SerialError> {
            self.frames.lock().push(data.trim_end().to_string());
            match &data[1..3] {
                "05" => self
                    .replies
                    .extend(["<05|", "HW_REV:03", "MCU:PICO2040", ""].map(|l| format!("{l}\r\n"))),
                "06" => self.replies.extend(
                    ["<06|FW_REV:01", "PROTOCOL_VERSION:01", ""].map(|l| format!("{l}\r\n")),
                ),
                _ => self
                    .replies
                    .push_back(crate::serial::protocol::firmware_echo_for(data)),
            }
            Ok(())
        }
        fn read_line(&mut self, _t: Duration) -> Result<String, crate::error::SerialError> {
            self.replies
                .pop_front()
                .ok_or(crate::error::SerialError::Timeout { timeout_ms: 1 })
        }
    }

    struct Fixture {
        state: Arc<AppState>,
        sys: Sysfs,
        journal: tempfile::TempDir,
        frames: Arc<parking_lot::Mutex<Vec<String>>>,
        /// Held so a borrow is queued, never answered: a started run ends
        /// without the port, having changed nothing.
        _loans: crate::serial::port_loan::LoanReceiver,
        _stop: tokio::sync::watch::Sender<bool>,
    }

    impl Fixture {
        fn io(&self) -> MaintenanceIo {
            MaintenanceIo {
                sys_root: self.sys.root().to_path_buf(),
                journal_path: self.journal.path().join(JOURNAL_FILE),
                open: Arc::new(|_: &str, _: Duration| {
                    Err(crate::error::SerialError::Protocol {
                        message: "nothing to open on the bench".into(),
                    })
                }),
                limits: StageLimits {
                    borrow_wait: Duration::from_millis(200),
                    sysfs_poll: Duration::from_millis(10),
                    check_retry: Duration::from_millis(20),
                    ..StageLimits::production()
                },
            }
        }

        async fn start(&self, body: MaintenanceStartRequest) -> (StatusCode, serde_json::Value) {
            let (st, Json(v)) = start_update(&self.state, body, self.io()).await;
            (st, v)
        }
    }

    /// An adopted, connected controller with its poll loop's lender in place.
    fn fixture() -> Fixture {
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let state = test_state(stop_rx);
        let sys = Sysfs::new();
        sys.add_openfan("8-8", SERIAL, 0x80, "ttyACM91", "ttyACM92");
        let frames = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let ctrl = crate::serial::controller::FanController::new(
            Box::new(Answering {
                frames: frames.clone(),
                replies: VecDeque::new(),
            }),
            state.cache.clone(),
            Duration::from_millis(50),
        );
        *state.fan_controller.write() = Some(Arc::new(parking_lot::Mutex::new(ctrl)));
        state.cache.set_openfan_port(TTY);
        state.cache.set_openfan_link(OpenFanLink::Connected);
        let (lender, loans) = crate::serial::port_loan::loan_channel();
        state.openfan_maintenance.set_lender(lender);
        Fixture {
            state,
            sys,
            journal: tempfile::tempdir().unwrap(),
            frames,
            _loans: loans,
            _stop: stop,
        }
    }

    fn test_state(shutdown: tokio::sync::watch::Receiver<bool>) -> Arc<AppState> {
        let readiness_rollup = Arc::new(parking_lot::Mutex::new(None));
        Arc::new(AppState {
            cache: Arc::new(crate::health::cache::StateCache::new()),
            staleness_config: crate::health::staleness::StalenessConfig::default(),
            daemon_version: "0.0.0-test".into(),
            fan_controller: Arc::new(parking_lot::RwLock::new(None)),
            openfan_runtime: crate::api::handlers::OpenFanRuntime {
                timeout: Duration::from_millis(50),
                interval: Duration::from_millis(1000),
                shutdown,
            },
            hwmon_controller: None,
            start_time: std::time::Instant::now(),
            history: Arc::new(crate::health::history::HistoryRing::new(10)),
            active_profile: Arc::new(parking_lot::Mutex::new(None)),
            openfan_calibration: Default::default(),
            characterization: Arc::new(parking_lot::Mutex::new(None)),
            validation: Arc::new(Default::default()),
            characterization_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            control_path: Arc::new(parking_lot::Mutex::new(None)),
            control_path_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stall_probe: Arc::new(parking_lot::Mutex::new(None)),
            stall_probe_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            control_paths: Arc::new(parking_lot::RwLock::new(Default::default())),
            pwm_baselines: Default::default(),
            pwm_verification: Default::default(),
            openfan_rescanning: std::sync::atomic::AtomicBool::new(false),
            last_openfan_rescan: Arc::new(parking_lot::Mutex::new(None)),
            adopted_poll_tasks: Arc::new(parking_lot::Mutex::new(Default::default())),
            openfan_maintenance: Default::default(),
            amd_gpus: Vec::new(),
            intel_gpus: Vec::new(),
            nvidia_gpus: Vec::new(),
            profile_search_dirs: parking_lot::RwLock::new(Vec::new()),
            config_path: String::new(),
            runtime_config_path: PathBuf::new(),
            sensor_rescan_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            header_roles: Arc::new(parking_lot::RwLock::new(Arc::new(
                std::collections::HashMap::new(),
            ))),
            cooling_devices: Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new()))),
            override_table: Arc::new(parking_lot::Mutex::new(
                crate::control_override::OverrideTable::new(),
            )),
            allow_port_probe: false,
            running_config: Default::default(),
            readiness_rollup: readiness_rollup.clone(),
            config_write: Default::default(),
            runtime_config_degraded: Default::default(),
            assessment: Arc::new(crate::api::handlers::AssessmentCache::new(readiness_rollup)),
        })
    }

    fn good() -> MaintenanceStartRequest {
        request(SERIAL, &"ab".repeat(32), 107_520)
    }

    fn reason(v: &serde_json::Value) -> &str {
        v["error"]["details"]["reason"].as_str().unwrap_or("")
    }

    #[tokio::test]
    async fn each_start_refusal_names_its_reason_and_claims_nothing() {
        let f = fixture();

        let calibration = f.state.openfan_calibration.claim().expect("free");
        let (st, v) = f.start(good()).await;
        assert_eq!(
            (st, reason(&v)),
            (StatusCode::CONFLICT, "calibration_active")
        );
        drop(calibration);

        let (st, v) = f
            .start(request("AAAAAAAAAAAAAAAA", &"ab".repeat(32), 512))
            .await;
        assert_eq!(
            (st, reason(&v)),
            (StatusCode::CONFLICT, "identity_mismatch")
        );

        f.sys.add_bootloader("9-1", "sdy");
        let (st, v) = f.start(good()).await;
        assert_eq!(
            (st, reason(&v)),
            (StatusCode::CONFLICT, "bootloader_present")
        );
        f.sys.remove_device("9-1");

        let epoch = f
            .state
            .cache
            .try_begin_verify(Duration::from_secs(30))
            .unwrap();
        let (st, v) = f.start(good()).await;
        assert_eq!(
            (st, reason(&v)),
            (StatusCode::CONFLICT, "diagnostic_active")
        );
        f.state.cache.end_verify(epoch);

        f.state
            .cache
            .record_engine_tick("emergency", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);
        let (st, v) = f.start(good()).await;
        assert_eq!(
            (st, reason(&v)),
            (StatusCode::CONFLICT, "thermal_emergency")
        );
        f.state
            .cache
            .record_engine_tick("normal", crate::constants::THERMAL_EMERGENCY_TRIGGER_C);

        f.state.cache.set_openfan_link(OpenFanLink::Unresponsive);
        let (st, v) = f.start(good()).await;
        assert_eq!(
            (st, reason(&v)),
            (StatusCode::CONFLICT, "openfan_link_not_ready")
        );
        assert_eq!(v["error"]["retryable"], true);

        assert!(
            f.state.cache.openfan_maintenance().is_none(),
            "nothing was claimed"
        );
        assert!(!f.state.openfan_maintenance.is_alive());
        assert!(f.state.openfan_maintenance.record().is_none());
        assert!(f.frames.lock().is_empty(), "and nothing was sent");
    }

    #[tokio::test]
    async fn a_start_runs_and_its_record_is_served_and_journaled() {
        let f = fixture();
        let (st, v) = f.start(good()).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let run_id = v["run_id"].as_str().unwrap().to_string();
        assert!(run_id.starts_with("ofmaint-"));
        assert_eq!(v["stage"], stage::PREPARING);

        let (st, v) = f.start(good()).await;
        assert_eq!(
            (st, reason(&v)),
            (StatusCode::CONFLICT, "maintenance_active")
        );

        // Nothing lends the port on the bench, so the run ends changing nothing.
        let slot = f.state.openfan_maintenance.clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        while slot.is_alive() {
            assert!(Instant::now() < deadline, "the run must end");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (st, Json(v)) = openfan_maintenance_status_handler(State(f.state.clone())).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["run_id"], run_id.as_str());
        assert_eq!(v["state"], STATE_FINISHED);
        assert_eq!(v["outcome"], outcome::NO_FIRMWARE_CHANGE);
        assert!(v["outcome_detail"]
            .as_str()
            .unwrap()
            .contains("could not be borrowed"));
        assert_eq!(v["expected_usb_serial"], SERIAL);
        assert_eq!(v["usb_port"], "8-8");
        assert!(v.get("api_version").is_some());
        assert_eq!(
            journal::load(&f.journal.path().join(JOURNAL_FILE)).map(|r| r.run_id),
            Some(run_id)
        );
        assert!(f.state.cache.openfan_maintenance().is_none());
        assert!(!f.state.cache.openfan_writes_suspended());
        let (st, _) = openfan_maintenance_cancel_handler(State(f.state.clone())).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "nothing is running");
    }

    #[tokio::test]
    async fn a_cancel_is_taken_until_the_window_closes() {
        let f = fixture();
        let _alive = f.state.openfan_maintenance.claim().unwrap();
        f.state
            .openfan_maintenance
            .set_record(MaintenanceRecord::new(
                "r1".into(),
                SERIAL.into(),
                FirmwareClaim::default(),
            ));
        let (st, Json(v)) = openfan_maintenance_cancel_handler(State(f.state.clone())).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        assert_eq!(
            (v["run_id"].as_str(), v["cancel_requested"].as_bool()),
            (Some("r1"), Some(true))
        );
        f.state.openfan_maintenance.close_cancel_window();
        let (st, Json(v)) = openfan_maintenance_cancel_handler(State(f.state.clone())).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(reason(&v), "not_cancellable");
    }

    #[tokio::test]
    async fn the_device_answer_reports_identity_reports_and_whether_an_update_could_start() {
        let f = fixture();
        let (st, Json(v)) = describe_device(&f.state, f.sys.root()).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["present"], true);
        assert_eq!(v["link"], "connected");
        assert_eq!(v["port"], TTY);
        assert_eq!(v["usb"]["serial"], SERIAL);
        assert_eq!(v["usb"]["port"], "8-8");
        assert_eq!(v["interface_number"], 0);
        assert_eq!(v["hw_info"]["HW_REV"], "03");
        assert_eq!(v["fw_info"]["FW_REV"], "01");
        assert_eq!(v["update_available"], true, "{v}");
        assert_eq!(v["update_refusals"], serde_json::json!([]));

        // A second look within the reuse window asks the board nothing.
        let asked = f.frames.lock().len();
        let _ = describe_device(&f.state, f.sys.root()).await;
        assert_eq!(f.frames.lock().len(), asked);

        let _calibration = f.state.openfan_calibration.claim().unwrap();
        let (_, Json(v)) = describe_device(&f.state, f.sys.root()).await;
        assert_eq!(v["update_available"], false);
        assert_eq!(v["update_refusals"][0]["reason"], "calibration_active");
    }

    #[tokio::test]
    async fn the_device_answer_asks_the_board_nothing_while_an_update_holds_it() {
        let f = fixture();
        f.state
            .cache
            .try_begin_openfan_maintenance("r1", stage::PREPARING)
            .unwrap();
        let (_, Json(v)) = describe_device(&f.state, f.sys.root()).await;
        assert_eq!(v["link"], "maintenance");
        assert!(v.get("hw_info").is_none() && v.get("fw_info").is_none());
        assert!(f.frames.lock().is_empty());
        assert_eq!(v["update_refusals"][0]["reason"], "maintenance_active");
    }

    /// The preview says what the POST would: a board the last run left needing
    /// recovery is refused even while the link still reads `connected`.
    #[tokio::test]
    async fn the_device_answer_refuses_a_board_left_needing_recovery() {
        let f = fixture();
        f.state
            .cache
            .try_begin_openfan_maintenance("r1", stage::PREPARING)
            .unwrap();
        f.state
            .cache
            .end_openfan_maintenance("r1", Some(outcome::NEEDS_RECOVERY));
        let (_, Json(v)) = describe_device(&f.state, f.sys.root()).await;
        assert_eq!(v["link"], "connected", "the stale link this guards");
        assert_eq!(v["update_available"], false, "{v}");
        assert_eq!(v["update_refusals"][0]["reason"], "openfan_link_not_ready");
        assert_eq!(
            v["update_refusals"][0]["message"],
            MaintenanceRefusal::RecoveryPending.message()
        );
        let (st, body) = f.start(good()).await;
        assert_eq!(st, StatusCode::CONFLICT, "{body}");
        assert_eq!(reason(&body), "openfan_link_not_ready");
    }

    fn interrupted_record(port: &str) -> MaintenanceRecord {
        let mut r = MaintenanceRecord::new("r0".into(), SERIAL.into(), FirmwareClaim::default());
        r.stage = stage::WAITING_FOR_FILE.into();
        r.bootloader_requested = true;
        r.usb_port = Some(port.into());
        r.interface_number = Some(0);
        r
    }

    #[test]
    fn a_board_left_in_its_bootloader_is_reported_and_watched_for_at_boot() {
        let (_stop, rx) = tokio::sync::watch::channel(false);
        let state = test_state(rx);
        let sys = Sysfs::new();
        sys.add_bootloader("8-8", "sdx");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(JOURNAL_FILE);
        journal::save(&path, &interrupted_record("8-8")).unwrap();

        let watch = recover_openfan_maintenance(&state, sys.root(), &path).expect("watched for");
        assert_eq!(watch.outcome.as_deref(), Some(outcome::NEEDS_RECOVERY));
        assert!(watch.interrupted);
        assert_eq!(
            state.openfan_maintenance.record().map(|r| r.state),
            Some(STATE_FINISHED.to_string())
        );
        assert!(matches!(
            state.cache.openfan_maintenance(),
            Some(OpenFanMaintenance::NeedsRecovery { .. })
        ));
        assert!(state.cache.openfan_writes_suspended());
    }

    #[test]
    fn a_board_that_is_gone_is_watched_for_but_not_reported() {
        let (_stop, rx) = tokio::sync::watch::channel(false);
        let state = test_state(rx);
        let sys = Sysfs::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(JOURNAL_FILE);
        journal::save(&path, &interrupted_record("8-8")).unwrap();
        assert!(recover_openfan_maintenance(&state, sys.root(), &path).is_some());
        assert!(
            state.cache.openfan_maintenance().is_none(),
            "an empty port is an absent controller, not a critical one"
        );
    }

    #[test]
    fn a_board_adopted_at_boot_needs_nothing_more() {
        let f = fixture();
        f.sys.remove_device("8-8");
        f.sys.add_bootloader("8-8", "sdx");
        let path = f.journal.path().join(JOURNAL_FILE);
        journal::save(&path, &interrupted_record("8-8")).unwrap();
        assert!(recover_openfan_maintenance(&f.state, f.sys.root(), &path).is_none());
        assert!(f.state.cache.openfan_maintenance().is_none());
        // A run that ended well needs nothing either.
        let (_stop, rx) = tokio::sync::watch::channel(false);
        let state = test_state(rx);
        let mut done = interrupted_record("8-8");
        done.state = STATE_FINISHED.into();
        done.outcome = Some(outcome::COMPLETED_BUILD_NOT_CONFIRMED.into());
        journal::save(&path, &done).unwrap();
        assert!(recover_openfan_maintenance(&state, f.sys.root(), &path).is_none());
        assert_eq!(
            state.openfan_maintenance.record().map(|r| r.run_id),
            Some("r0".into())
        );
    }

    #[tokio::test]
    async fn the_watch_tries_once_per_return_and_stops_once_adopted() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
        let sys = Sysfs::new();
        let (attempts, checks) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let adopted = Arc::new(AtomicBool::new(false));
        let (_stop, rx) = tokio::sync::watch::channel(false);
        let (c, a, n) = (checks.clone(), adopted.clone(), attempts.clone());
        let task = tokio::spawn(watch_for_return(
            sys.root().to_path_buf(),
            "8-8".into(),
            SERIAL.into(),
            0,
            Duration::from_millis(5),
            rx,
            move || {
                c.fetch_add(1, SeqCst);
                a.load(SeqCst)
            },
            move || {
                let n = n.clone();
                async move {
                    n.fetch_add(1, SeqCst);
                }
            },
        ));
        let wait = |what: &'static str, done: Box<dyn Fn() -> bool>| async move {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        };
        let after = |n: usize| {
            let c = checks.clone();
            let from = c.load(SeqCst);
            Box::new(move || c.load(SeqCst) >= from + n) as Box<dyn Fn() -> bool>
        };

        sys.add_bootloader("8-8", "sdx");
        wait("polls of a bootloader", after(5)).await;
        assert_eq!(
            attempts.load(SeqCst),
            0,
            "a bootloader is not a board to adopt"
        );

        sys.remove_device("8-8");
        sys.add_openfan("8-8", SERIAL, 0x81, "ttyACM91", "ttyACM92");
        let n = attempts.clone();
        wait("the attempt", Box::new(move || n.load(SeqCst) == 1)).await;
        wait("a while longer, unadopted", after(20)).await;
        assert_eq!(
            attempts.load(SeqCst),
            1,
            "one attempt per return, not one per poll"
        );

        sys.remove_device("8-8");
        wait("polls of an empty port", after(3)).await;
        sys.add_openfan("8-8", SERIAL, 0x81, "ttyACM91", "ttyACM92");
        let n = attempts.clone();
        wait(
            "the second return's attempt",
            Box::new(move || n.load(SeqCst) == 2),
        )
        .await;

        adopted.store(true, SeqCst);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("an adopted board ends the watch")
            .unwrap();
    }

    // ── An override or identify racing an update's start ─────────────

    /// A profile whose one control, `front`, drives OpenFan channel 3.
    fn openfan_profile() -> crate::profile::DaemonProfile {
        crate::profile::DaemonProfile {
            id: "p".into(),
            name: "p".into(),
            version: 7,
            description: String::new(),
            controls: vec![crate::profile::LogicalControl {
                id: "front".into(),
                name: "front".into(),
                mode: "manual".into(),
                curve_id: String::new(),
                manual_output_pct: 50.0,
                members: vec![crate::profile::ControlMember {
                    source: "openfan".into(),
                    member_id: "openfan:ch03".into(),
                    member_label: "Front".into(),
                    fan_zero_rpm: false,
                }],
                step_up_pct: 100.0,
                step_down_pct: 100.0,
                offset_pct: 0.0,
                minimum_pct: 20.0,
                start_pct: 0.0,
                stop_pct: 0.0,
            }],
            curves: vec![],
        }
    }

    type Answer = (StatusCode, Json<serde_json::Value>);

    /// Run a handler on its own thread and runtime, so the test can hold the
    /// locks it is about to wait on.
    fn on_a_thread(
        handler: impl std::future::Future<Output = Answer> + Send + 'static,
    ) -> std::thread::JoinHandle<Answer> {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(handler)
        })
    }

    /// Real time, bounded.
    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The take's update check and its insert are one critical section under
    /// `override_table`, the guard an update's start takes, after its claim,
    /// to release OpenFan overrides. Here the take is parked on that guard
    /// with its profile check done, and the claim lands. Checked before the
    /// profile guard (as it was), the take had already seen no update, and
    /// inserted after the start's release: an override that outlived it.
    #[test]
    fn an_override_racing_an_update_start_is_refused_not_left_behind() {
        let f = fixture();
        *f.state.active_profile.lock() = Some(openfan_profile());
        let table = f.state.override_table.lock();
        let take = on_a_thread(crate::api::handlers::control::override_take_handler(
            State(f.state.clone()),
            axum::extract::Path("front".to_string()),
            Json(OverrideTakeRequest {
                pwm_percent: 0,
                ttl_secs: None,
            }),
        ));
        // Holding the profile guard: everything before it is done.
        wait_for("the take to reach the override table", || {
            f.state.active_profile.is_locked()
        });
        f.state
            .cache
            .try_begin_openfan_maintenance("r-race", stage::PREPARING)
            .expect("the claim");
        drop(table);
        let (status, Json(body)) = take.join().expect("the take");
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["details"]["reason"], "openfan_maintenance");
        assert!(f.state.override_table.lock().snapshot().controls.is_empty());
    }

    /// The same for an identify stop on an OpenFan channel: its decision is
    /// taken again under the guard its hold is inserted under.
    #[test]
    fn an_identify_racing_an_update_start_is_refused_not_left_behind() {
        let f = fixture();
        f.state
            .cache
            .update_openfan_fans(vec![crate::health::state::OpenFanState {
                channel: 3,
                rpm: 900,
                last_commanded_pwm: Some(40),
                updated_at: Instant::now(),
                rpm_polled: true,
                poll_seq: 1,
            }]);
        let table = f.state.override_table.lock();
        let stop = on_a_thread(crate::api::handlers::control::fan_identify_handler(
            State(f.state.clone()),
            axum::extract::Path("openfan:ch03".to_string()),
            Json(IdentifyRequest {
                action: "stop".into(),
                ttl_secs: None,
            }),
        ));
        wait_for("the identify to reach the override table", || {
            f.state.active_profile.is_locked()
        });
        f.state
            .cache
            .try_begin_openfan_maintenance("r-race", stage::PREPARING)
            .expect("the claim");
        drop(table);
        let (status, Json(body)) = stop.join().expect("the identify");
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["details"]["reason"], "openfan_maintenance");
        assert!(f.state.override_table.lock().snapshot().identify.is_empty());
    }
}
