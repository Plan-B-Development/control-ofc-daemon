//! The API's handle on the hwmon PWM controller (FFA-h).
//!
//! The controller's mutex is the lock the engine's and the thermal force's
//! hwmon writes take, and a write that does not return keeps it held
//! (DEC-455). An async handler that took it parked a tokio worker; enough of
//! them starved the engine, and the watchdog killed the daemon. So a handler
//! never takes it on a worker:
//! - **header facts** — the descriptors, frozen at discovery — come from a
//!   snapshot taken when the handle is built, with no lock at all;
//! - **live controller state** (a lease, a revert count) is reached through
//!   [`HwmonHandle::with_controller`], on the blocking pool, waiting at most
//!   [`HWMON_CONTROLLER_WAIT`] and refused as busy after that.
//!
//! Code already on the blocking pool (a diagnostic's bounded writes, the
//! hardware diagnostics build) takes [`HwmonHandle::controller`] directly.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::constants::HWMON_CONTROLLER_WAIT;
use crate::hwmon::pwm_control::{forced_target_ids_of, HwmonPwmController};
use crate::hwmon::pwm_discovery::PwmHeaderDescriptor;

/// The hwmon controller and a lock-free copy of its headers.
#[derive(Clone)]
pub struct HwmonHandle {
    controller: Arc<Mutex<HwmonPwmController>>,
    /// In `HwmonPwmController::headers` order. Exact for the controller's
    /// whole life: nothing changes its header set or a descriptor after
    /// construction, and `/hwmon/rescan` never replaces a running controller.
    headers: Arc<[PwmHeaderDescriptor]>,
}

/// The controller's lock did not come free within [`HWMON_CONTROLLER_WAIT`]
/// (or the closure run under it panicked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerBusy;

impl HwmonHandle {
    /// Wrap a controller nothing else holds yet.
    pub fn new(controller: HwmonPwmController) -> Self {
        let headers = controller.headers().into_iter().cloned().collect();
        Self {
            controller: Arc::new(Mutex::new(controller)),
            headers,
        }
    }

    /// The controller itself. **Never lock it on a tokio worker** — use
    /// [`Self::with_controller`] there; this is for the engine, the shutdown
    /// path and code already on the blocking pool.
    pub fn controller(&self) -> &Arc<Mutex<HwmonPwmController>> {
        &self.controller
    }

    /// Every discovered header, as `HwmonPwmController::headers` lists them.
    pub fn headers(&self) -> &[PwmHeaderDescriptor] {
        &self.headers
    }

    /// One header by id.
    pub fn header(&self, id: &str) -> Option<&PwmHeaderDescriptor> {
        self.headers.iter().find(|h| h.id == id)
    }

    /// `HwmonPwmController::forced_target_ids`, from the same definition.
    pub fn forced_target_ids(&self) -> Vec<String> {
        forced_target_ids_of(self.headers.iter())
    }

    /// Run `f` under the controller's lock on the blocking pool, waiting at
    /// most [`HWMON_CONTROLLER_WAIT`] for it. A wait that runs out takes
    /// nothing and runs nothing — `f` never lands late — and the worker that
    /// awaits this is free throughout. `f` must not block: it runs while the
    /// engine may be waiting for the same lock.
    pub async fn with_controller<T, F>(&self, f: F) -> Result<T, ControllerBusy>
    where
        F: FnOnce(&mut HwmonPwmController) -> T + Send + 'static,
        T: Send + 'static,
    {
        let controller = self.controller.clone();
        let ran = tokio::task::spawn_blocking(move || {
            controller
                .try_lock_for(HWMON_CONTROLLER_WAIT)
                .map(|mut guard| f(&mut guard))
        })
        .await;
        match ran {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(ControllerBusy),
            Err(e) => {
                log::error!("hwmon controller call failed on the blocking pool: {e}");
                Err(ControllerBusy)
            }
        }
    }
}
