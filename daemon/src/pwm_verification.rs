//! Persisted per-header PWM-control verification (DEC-456, W-DIAGG `PTR-l`).
//!
//! Stored at `{state_dir}/pwm_verification.json` — one document, keyed by header
//! id. It is what the readiness item `pwm_control_unverified` counts against, and
//! what `/hwmon/headers` publishes per header as `pwm_verification`.
//!
//! # Why the daemon holds this and not a client
//!
//! "Is PWM control verified on this machine?" was answered by the daemon's
//! readiness item, which never cleared, and separately by one GUI page's own
//! machine-wide setting, which only that page's sweep wrote (`PTR-l`). Tests run
//! from the Hardware page, the PWM Test Report or a validation session reached
//! neither. Every one of them goes through the daemon's verify or
//! characterisation handler, so the record belongs there: one owner, every
//! client, and it survives a restart.
//!
//! # What a record says
//!
//! The **latest conclusive verdict** for a header, `verified` or `failed`. An
//! inconclusive result — no tach, no readback, a verify stopped because the
//! header became pump-protected, a sweep that proved neither — leaves the record
//! as it was: it has not refuted a previous pass and has not confirmed one
//! either. The two ways in are [`verdict_for_verify`] and [`verdict_for_sweep`],
//! and they are the only places those rules live.
//!
//! # Invalidation and bounds
//!
//! Exactly the control-path store's (see `control_paths.rs`): keyed by the
//! header's stable id, which embeds chip, device, `pwmN` and label, so a record
//! whose hardware changed simply stops matching and the boot prune drops it; a
//! cap on entries and on every stored string at ingest, and a file cap derived
//! from both (DEC-320).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_io;
use crate::constants;

const STORE_FILE: &str = "pwm_verification.json";

/// The last conclusive test showed the write takes effect.
pub const STATE_VERIFIED: &str = "verified";
/// The last conclusive test showed it does not.
pub const STATE_FAILED: &str = "failed";
/// Established by `POST /hwmon/{id}/verify`.
pub const METHOD_VERIFY: &str = "verify";
/// Established by `POST /hwmon/{id}/characterize`.
pub const METHOD_CHARACTERIZATION: &str = "characterization";

/// Sweep `result` tokens. A verify's `result` is its own wire token verbatim.
pub const SWEEP_PASS: &str = "sweep_pass";
pub const SWEEP_READBACK_REVERTED: &str = "readback_reverted";
pub const SWEEP_INTERFERENCE: &str = "interference";

/// One header's latest conclusive verdict, as stored and as published.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PwmVerificationRecord {
    pub header_id: String,
    /// [`STATE_VERIFIED`] | [`STATE_FAILED`]. Opaque to a client: render an
    /// unrecognised token rather than dropping it (273-i).
    pub state: String,
    /// [`METHOD_VERIFY`] | [`METHOD_CHARACTERIZATION`].
    pub method: String,
    /// The verify's own `result` token, or a sweep token ([`SWEEP_PASS`] …).
    pub result: String,
    /// The characterisation run that produced it; empty for a verify.
    #[serde(default)]
    pub run_id: String,
    pub verified_unix_ms: u64,
}

/// The whole store.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PwmVerificationStore {
    /// Keyed by header id; a `BTreeMap` for a stable serialised order.
    #[serde(default)]
    pub records: BTreeMap<String, PwmVerificationRecord>,
}

/// The verdict a verify `result` token carries, or `None` for an inconclusive one.
///
/// Mirrors the GUI's evidence table (`verify_view._OUTCOMES`): `effective`
/// proves control; a reverted mode, a clamped value or no RPM effect disproves
/// it; `rpm_unavailable`, `pwm_readback_unavailable`, `pump_protected_mid_run`
/// and any token this build does not know prove nothing either way.
pub fn verdict_for_verify(result: &str) -> Option<&'static str> {
    match result {
        "effective" => Some(STATE_VERIFIED),
        "pwm_enable_reverted" | "pwm_value_clamped" | "no_rpm_effect" => Some(STATE_FAILED),
        _ => None,
    }
}

/// The verdict a finished characterisation sweep carries, with its `result`
/// token, or `None` when it proved neither (DEC-456, the user's strict rule).
///
/// **Verified** only when the run completed, every write was accepted, the duty
/// read back as written at every point, the RPM followed the duty, and no point
/// saw another controller holding the header. **Failed** when the run completed
/// and the readback showed the mode reverted, or another controller took the
/// header. Everything else — an aborted or cancelled run, a clamped readback, no
/// RPM response (which the daemon deliberately does not call a fault,
/// `possible_device_override`), no tach — leaves the record alone.
pub fn verdict_for_sweep(
    run: &crate::api::characterization::CharacterizationRun,
) -> Option<(&'static str, &'static str)> {
    if run.state != crate::api::characterization::STATE_COMPLETE {
        return None;
    }
    let s = run.summary.as_ref()?;
    if s.pwm_readback == "reverted" {
        return Some((STATE_FAILED, SWEEP_READBACK_REVERTED));
    }
    if s.interference_detected {
        return Some((STATE_FAILED, SWEEP_INTERFERENCE));
    }
    if s.command_acceptance == "pass" && s.pwm_readback == "pass" && s.rpm_response == "responsive"
    {
        return Some((STATE_VERIFIED, SWEEP_PASS));
    }
    None
}

impl PwmVerificationStore {
    /// Re-key every record by canonical header id (DEC-442).
    pub fn canonicalize_ids(mut self) -> Self {
        use crate::hwmon::chip_name::canonicalize_keyed;
        self.records = canonicalize_keyed(std::mem::take(&mut self.records), |id, r| {
            r.header_id = id.to_string();
        });
        self
    }

    pub fn get(&self, header_id: &str) -> Option<&PwmVerificationRecord> {
        self.records.get(header_id)
    }

    /// Insert or replace one header's record, applying the ingest bounds. At
    /// capacity the oldest record goes: a verdict is a current fact about
    /// hardware, so the stale one is the expendable one.
    pub fn upsert(&mut self, mut record: PwmVerificationRecord) {
        record.clamp_text();
        if !self.records.contains_key(&record.header_id)
            && self.records.len() >= constants::PWM_VERIFICATION_MAX_ENTRIES
        {
            if let Some(oldest) = self
                .records
                .values()
                .min_by_key(|r| r.verified_unix_ms)
                .map(|r| r.header_id.clone())
            {
                self.records.remove(&oldest);
            }
        }
        self.records.insert(record.header_id.clone(), record);
    }

    /// Drop every record whose header is no longer discoverable, on a chip
    /// discovery saw (`PTR-af`, `chip_name::prune_to_live_chips`). Returns how
    /// many went, so an unchanged store costs no disk write at boot.
    pub fn prune_to_live(&mut self, live_header_ids: &[String]) -> usize {
        crate::hwmon::chip_name::prune_to_live_chips(&mut self.records, live_header_ids)
    }

    /// `(verified, failed)` among `header_ids` — the readiness item's inputs.
    pub fn counts<'a>(&self, header_ids: impl IntoIterator<Item = &'a str>) -> (usize, usize) {
        let mut verified = 0;
        let mut failed = 0;
        for id in header_ids {
            match self.get(id).map(|r| r.state.as_str()) {
                Some(STATE_VERIFIED) => verified += 1,
                Some(STATE_FAILED) => failed += 1,
                _ => {}
            }
        }
        (verified, failed)
    }
}

impl PwmVerificationRecord {
    /// Bound every string at ingest, so the file cap is reachable but never
    /// exceeded by anything this daemon writes.
    fn clamp_text(&mut self) {
        use crate::text::truncate;
        let cap = constants::PWM_VERIFICATION_MAX_TEXT_BYTES;
        truncate(&mut self.header_id, cap);
        truncate(&mut self.state, cap);
        truncate(&mut self.method, cap);
        truncate(&mut self.result, cap);
        truncate(&mut self.run_id, cap);
    }
}

/// Path of the store inside a given state directory.
pub fn store_path_in(dir: &Path) -> PathBuf {
    dir.join(STORE_FILE)
}

/// Load the store, or an empty one. Never an error: a missing file is the
/// first-boot state, and an unreadable or over-size one is discarded with a
/// warning — a verification record is not worth failing startup over.
pub fn load_from(dir: &Path) -> PwmVerificationStore {
    let path = store_path_in(dir);
    let Ok(meta) = std::fs::metadata(&path) else {
        return PwmVerificationStore::default();
    };
    if meta.len() > constants::PWM_VERIFICATION_MAX_BYTES {
        log::warn!(
            "PWM verification store {} is {} bytes, over the {} byte cap — discarding it. \
             This daemon cannot write a document that large, so it was written by \
             something else.",
            path.display(),
            meta.len(),
            constants::PWM_VERIFICATION_MAX_BYTES
        );
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("could not remove the over-size PWM verification store: {e}");
        }
        return PwmVerificationStore::default();
    }
    match atomic_io::read_to_string_with_cap(&path, constants::PWM_VERIFICATION_MAX_BYTES) {
        Ok(text) => match serde_json::from_str::<PwmVerificationStore>(&text) {
            Ok(store) => store.canonicalize_ids(),
            Err(e) => {
                log::warn!(
                    "PWM verification store {} will not parse ({e}); starting empty",
                    path.display()
                );
                PwmVerificationStore::default()
            }
        },
        Err(e) => {
            log::warn!(
                "PWM verification store {} unreadable ({e}); starting empty",
                path.display()
            );
            PwmVerificationStore::default()
        }
    }
}

/// Persist the store atomically.
pub fn save_to(dir: &Path, store: &PwmVerificationStore) -> Result<(), String> {
    let path = store_path_in(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|e| format!("serialise PWM verification store: {e}"))?;
    if bytes.len() as u64 > constants::PWM_VERIFICATION_MAX_BYTES {
        return Err(format!(
            "PWM verification store would be {} bytes, over the {} byte cap",
            bytes.len(),
            constants::PWM_VERIFICATION_MAX_BYTES
        ));
    }
    atomic_io::write_atomic(&path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::characterization::{CharSummary, CharacterizationRun, STATE_COMPLETE};

    fn record(header: &str, state: &str, at: u64) -> PwmVerificationRecord {
        PwmVerificationRecord {
            header_id: header.into(),
            state: state.into(),
            method: METHOD_VERIFY.into(),
            result: "effective".into(),
            run_id: String::new(),
            verified_unix_ms: at,
        }
    }

    fn sweep(state: &str, summary: CharSummary) -> CharacterizationRun {
        CharacterizationRun {
            state: state.into(),
            summary: Some(summary),
            ..Default::default()
        }
    }

    fn clean() -> CharSummary {
        CharSummary {
            command_acceptance: "pass".into(),
            pwm_readback: "pass".into(),
            rpm_response: "responsive".into(),
            ..Default::default()
        }
    }

    #[test]
    fn verify_tokens_split_into_pass_fail_and_inconclusive() {
        assert_eq!(verdict_for_verify("effective"), Some(STATE_VERIFIED));
        for fail in ["pwm_enable_reverted", "pwm_value_clamped", "no_rpm_effect"] {
            assert_eq!(verdict_for_verify(fail), Some(STATE_FAILED), "{fail}");
        }
        for none in [
            "rpm_unavailable",
            "pwm_readback_unavailable",
            "pump_protected_mid_run",
            "a_future_token",
        ] {
            assert_eq!(verdict_for_verify(none), None, "{none}");
        }
    }

    #[test]
    fn a_clean_complete_sweep_verifies() {
        assert_eq!(
            verdict_for_sweep(&sweep(STATE_COMPLETE, clean())),
            Some((STATE_VERIFIED, SWEEP_PASS))
        );
    }

    #[test]
    fn each_strict_condition_withholds_the_pass() {
        let mut partial = clean();
        partial.command_acceptance = "partial".into();
        let mut clamped = clean();
        clamped.pwm_readback = "clamped".into();
        let mut still = clean();
        still.rpm_response = "no_response".into();
        for (why, s) in [
            ("a write refused", partial),
            ("clamped", clamped),
            ("no RPM", still),
        ] {
            assert_eq!(verdict_for_sweep(&sweep(STATE_COMPLETE, s)), None, "{why}");
        }
        assert_eq!(
            verdict_for_sweep(&sweep("aborted", clean())),
            None,
            "not complete"
        );
        let mut none = sweep(STATE_COMPLETE, clean());
        none.summary = None;
        assert_eq!(verdict_for_sweep(&none), None, "no summary");
    }

    #[test]
    fn a_reverted_or_interfered_sweep_fails() {
        let mut reverted = clean();
        reverted.pwm_readback = "reverted".into();
        assert_eq!(
            verdict_for_sweep(&sweep(STATE_COMPLETE, reverted)),
            Some((STATE_FAILED, SWEEP_READBACK_REVERTED))
        );
        // Interference outranks an otherwise clean sweep: another controller
        // held the header, so the pass is not the daemon's.
        let mut grabbed = clean();
        grabbed.interference_detected = true;
        assert_eq!(
            verdict_for_sweep(&sweep(STATE_COMPLETE, grabbed)),
            Some((STATE_FAILED, SWEEP_INTERFERENCE))
        );
    }

    #[test]
    fn the_latest_verdict_replaces_the_earlier_one() {
        let mut store = PwmVerificationStore::default();
        store.upsert(record("h1", STATE_VERIFIED, 1));
        store.upsert(record("h1", STATE_FAILED, 2));
        assert_eq!(store.get("h1").unwrap().state, STATE_FAILED);
        assert_eq!(store.records.len(), 1);
    }

    #[test]
    fn counts_only_the_named_headers() {
        let mut store = PwmVerificationStore::default();
        store.upsert(record("a", STATE_VERIFIED, 1));
        store.upsert(record("b", STATE_FAILED, 1));
        store.upsert(record("gone", STATE_VERIFIED, 1));
        assert_eq!(store.counts(["a", "b", "c"]), (1, 1));
    }

    #[test]
    fn the_prune_drops_headers_that_are_gone() {
        let mut store = PwmVerificationStore::default();
        store.upsert(record("a", STATE_VERIFIED, 1));
        store.upsert(record("gone", STATE_VERIFIED, 1));
        assert_eq!(store.prune_to_live(&["a".into()]), 1);
        assert!(store.get("gone").is_none() && store.get("a").is_some());
    }

    /// `PTR-af`: a boot that found no headers keeps every verdict.
    #[test]
    fn an_empty_discovery_keeps_every_verdict() {
        let mut store = PwmVerificationStore::default();
        store.upsert(record("hwmon:it87:isa:pwm1:PUMP", STATE_VERIFIED, 1));
        assert_eq!(store.prune_to_live(&[]), 0);
        assert!(store.get("hwmon:it87:isa:pwm1:PUMP").is_some());
    }

    #[test]
    fn at_capacity_the_oldest_record_is_evicted() {
        let mut store = PwmVerificationStore::default();
        for i in 0..constants::PWM_VERIFICATION_MAX_ENTRIES {
            store.upsert(record(&format!("h{i}"), STATE_VERIFIED, 10 + i as u64));
        }
        store.upsert(record("new", STATE_VERIFIED, 1_000));
        assert_eq!(store.records.len(), constants::PWM_VERIFICATION_MAX_ENTRIES);
        assert!(store.get("h0").is_none(), "the oldest went");
        assert!(store.get("new").is_some());
    }

    #[test]
    fn a_full_store_of_maximal_records_round_trips_under_the_file_cap() {
        // DEC-320: assert the realised artefact, not a re-derivation of the cap.
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(constants::PWM_VERIFICATION_MAX_TEXT_BYTES * 4);
        let mut store = PwmVerificationStore::default();
        for i in 0..constants::PWM_VERIFICATION_MAX_ENTRIES {
            store.upsert(PwmVerificationRecord {
                header_id: format!("{i:03}{long}"),
                state: long.clone(),
                method: long.clone(),
                result: long.clone(),
                run_id: long.clone(),
                verified_unix_ms: u64::MAX,
            });
        }
        save_to(dir.path(), &store).unwrap();
        let len = std::fs::metadata(store_path_in(dir.path())).unwrap().len();
        assert!(len <= constants::PWM_VERIFICATION_MAX_BYTES, "{len}");
        assert_eq!(load_from(dir.path()).records.len(), store.records.len());
    }

    #[test]
    fn a_suffixed_record_is_rekeyed_to_its_canonical_id_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let suffixed = "hwmon:it8696_a008090a:it87.2624:pwm5:pwm5";
        let mut store = PwmVerificationStore::default();
        store.upsert(record(suffixed, STATE_VERIFIED, 1));
        save_to(dir.path(), &store).unwrap();
        let loaded = load_from(dir.path());
        let canonical = "hwmon:it8696:it87.2624:pwm5:pwm5";
        assert_eq!(loaded.get(canonical).unwrap().header_id, canonical);
    }
}
