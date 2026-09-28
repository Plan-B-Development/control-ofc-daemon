//! Persisted PWM ↔ tach relationships (AIO Phase 8 Batch 1 §6.3).
//!
//! Stored at `{state_dir}/control_paths.json` — one document, keyed by header id.
//!
//! # Why the daemon holds this and not the GUI
//!
//! §6.3 wants a "Last validated: `<timestamp>`" row on the header card, and warns
//! against persisting a mapping "as unquestioned truth if the underlying hardware
//! identity changes". Both halves point here rather than at client settings.
//!
//! The invalidation is structural rather than a policy anyone has to remember:
//! the key is the header's **stable id**, which already embeds chip, device,
//! `pwmN` and label (`hwmon:<chip>:<device>:pwm<N>:<LABEL>`). Swap the board,
//! change the driver, or have the chip start publishing labels, and the id
//! changes with it — so a stale record simply stops matching any live header and
//! [`prune_to_live`] drops it. (The it87 v2.0 board suffix is not such a change:
//! the chip segment is canonical, and a suffixed record is re-keyed at load,
//! DEC-442.) Nothing has to detect "the hardware changed",
//! because a record that survives *is* a record whose hardware did not.
//!
//! # Bounds
//!
//! Two, and they are a pair (DEC-320). The file is read **stat-first** against
//! [`constants::CONTROL_PATHS_MAX_BYTES`], and every stored string is truncated
//! at ingest to [`constants::CONTROL_PATH_MAX_TEXT_BYTES`]. The ingest bound is
//! what makes the file bound safe to act on: because this daemon cannot write an
//! over-size document, "too large" can only mean "written by something else", so
//! discarding and deleting it is correct rather than data loss. Without the
//! ingest bound the two could disagree and a legitimately full store would become
//! permanently unreadable — which is the failure DEC-320 recorded, running the
//! other way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_io;
use crate::constants;

const STORE_FILE: &str = "control_paths.json";

/// One header's discovered relationship, as published on the wire and stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlPathRecord {
    pub header_id: String,
    /// `confirmed` | `probable` | `ambiguous` | `no_tach_response` |
    /// `multiple_responses`.
    pub relationship: String,
    /// `high` | `medium` | `low` | `unknown`.
    pub confidence: String,
    /// Responding tach ids, strongest first. Empty for `no_tach_response`, which
    /// is a real result and is stored rather than discarded — "we looked and
    /// nothing answered" is worth keeping.
    pub tach_ids: Vec<String>,
    /// Display labels, parallel to `tach_ids`.
    pub tach_labels: Vec<String>,
    /// `positive` | `negative` | empty when nothing responded.
    pub direction: String,
    pub baseline_rpm: Option<u16>,
    pub perturbed_rpm: Option<u16>,
    pub change_pct: Option<f64>,
    /// The run that produced this, for cross-referencing a validation session's
    /// `evidence[]`.
    pub run_id: String,
    /// Wall-clock stamp for §6.3's "Last validated" row.
    pub validated_unix_ms: u64,
}

/// The whole store.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ControlPathStore {
    /// Keyed by header id. A `BTreeMap` so the serialised document has a stable
    /// key order and two identical stores produce identical bytes — which is what
    /// makes "did this actually change?" answerable without a semantic diff.
    #[serde(default)]
    pub records: BTreeMap<String, ControlPathRecord>,
}

impl ControlPathStore {
    /// Re-key every record by canonical header id and canonicalise the tach
    /// ids it carries (DEC-442) — see `chip_name::canonicalize_keyed`.
    pub fn canonicalize_ids(mut self) -> Self {
        use crate::hwmon::chip_name::{canonical_hwmon_id, canonicalize_keyed};
        self.records = canonicalize_keyed(std::mem::take(&mut self.records), |id, r| {
            r.header_id = id.to_string();
            for tach in &mut r.tach_ids {
                *tach = canonical_hwmon_id(tach).into_owned();
            }
        });
        self
    }

    pub fn get(&self, header_id: &str) -> Option<&ControlPathRecord> {
        self.records.get(header_id)
    }

    /// Insert or replace one record, applying the ingest bounds.
    ///
    /// Eviction, when at capacity, drops the **oldest** record by
    /// `validated_unix_ms`. Deliberately the opposite of the validation store's
    /// cap-and-stop: a session is evidence whose beginning is the interesting
    /// part, whereas a control path is a current fact about hardware and the
    /// stale one is the expendable one.
    pub fn upsert(&mut self, mut record: ControlPathRecord) {
        record.clamp_text();
        if !self.records.contains_key(&record.header_id)
            && self.records.len() >= constants::CONTROL_PATHS_MAX_ENTRIES
        {
            if let Some(oldest) = self
                .records
                .values()
                .min_by_key(|r| r.validated_unix_ms)
                .map(|r| r.header_id.clone())
            {
                log::info!(
                    "control-path store is full ({} entries); evicting the oldest record \
                     for {oldest}",
                    constants::CONTROL_PATHS_MAX_ENTRIES
                );
                self.records.remove(&oldest);
            }
        }
        self.records.insert(record.header_id.clone(), record);
    }

    /// Drop every record whose header is no longer discoverable (§6.3).
    ///
    /// Returns how many were dropped, so the caller can decide whether the store
    /// needs rewriting — a prune that changed nothing must not cost a disk write
    /// on every boot.
    pub fn prune_to_live(&mut self, live_header_ids: &[String]) -> usize {
        let before = self.records.len();
        self.records
            .retain(|header_id, _| live_header_ids.iter().any(|live| live == header_id));
        before - self.records.len()
    }
}

impl ControlPathRecord {
    /// [SAFETY-adjacent] Bound every string at ingest.
    ///
    /// `tach_ids` and `tach_labels` originate in hwmon discovery, but
    /// `relationship`, `confidence` and `direction` are only as bounded as the
    /// code that fills them, and `run_id` is generated. Truncating all of them
    /// here means one function decides the document's maximum size, and the
    /// compile-time assertion in `constants.rs` can then prove the file bound is
    /// reachable-but-not-exceedable.
    fn clamp_text(&mut self) {
        use crate::text::truncate;
        let cap = constants::CONTROL_PATH_MAX_TEXT_BYTES;
        truncate(&mut self.header_id, cap);
        truncate(&mut self.relationship, cap);
        truncate(&mut self.confidence, cap);
        truncate(&mut self.direction, cap);
        truncate(&mut self.run_id, cap);
        self.tach_ids
            .truncate(constants::CONTROL_PATH_MAX_TACH_REFS);
        self.tach_labels
            .truncate(constants::CONTROL_PATH_MAX_TACH_REFS);
        for s in self.tach_ids.iter_mut().chain(self.tach_labels.iter_mut()) {
            truncate(s, cap);
        }
    }
}

/// Path of the store inside a given state directory.
pub fn store_path_in(dir: &Path) -> PathBuf {
    dir.join(STORE_FILE)
}

/// Load the store, or an empty one.
///
/// Never an error: a missing file is the normal first-boot state, and an
/// unreadable or over-size one is discarded with a warning. A diagnostic record
/// is a convenience, and failing daemon startup over it would be the wrong
/// trade entirely.
pub fn load_from(dir: &Path) -> ControlPathStore {
    let path = store_path_in(dir);
    let Ok(meta) = std::fs::metadata(&path) else {
        return ControlPathStore::default();
    };
    if meta.len() > constants::CONTROL_PATHS_MAX_BYTES {
        log::warn!(
            "control-path store {} is {} bytes, over the {} byte cap — discarding it. \
             This daemon cannot write a document that large, so it was written by \
             something else.",
            path.display(),
            meta.len(),
            constants::CONTROL_PATHS_MAX_BYTES
        );
        // Safe to delete precisely BECAUSE of the ingest bound above: an
        // over-size document is not one we could have produced.
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("could not remove the over-size control-path store: {e}");
        }
        return ControlPathStore::default();
    }
    match atomic_io::read_to_string_with_cap(&path, constants::CONTROL_PATHS_MAX_BYTES) {
        Ok(text) => match serde_json::from_str::<ControlPathStore>(&text) {
            Ok(store) => store.canonicalize_ids(),
            Err(e) => {
                log::warn!(
                    "control-path store {} will not parse ({e}); starting empty",
                    path.display()
                );
                ControlPathStore::default()
            }
        },
        Err(e) => {
            log::warn!(
                "control-path store {} unreadable ({e}); starting empty",
                path.display()
            );
            ControlPathStore::default()
        }
    }
}

/// Persist the store atomically.
pub fn save_to(dir: &Path, store: &ControlPathStore) -> Result<(), String> {
    let path = store_path_in(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|e| format!("serialise control-path store: {e}"))?;
    // Belt and braces against the compile-time assertion: if a future field ever
    // makes a bounded store exceed the file cap, fail the WRITE rather than
    // producing a document the next boot will discard.
    if bytes.len() as u64 > constants::CONTROL_PATHS_MAX_BYTES {
        return Err(format!(
            "control-path store would be {} bytes, over the {} byte cap",
            bytes.len(),
            constants::CONTROL_PATHS_MAX_BYTES
        ));
    }
    atomic_io::write_atomic(&path, &bytes)
}

/// Wall-clock milliseconds since the epoch.
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(header: &str, tach: &str, run_id: &str) -> ControlPathRecord {
        ControlPathRecord {
            header_id: header.into(),
            relationship: "confirmed".into(),
            confidence: "high".into(),
            tach_ids: vec![tach.into()],
            tach_labels: vec!["fan5".into()],
            direction: "positive".into(),
            baseline_rpm: Some(900),
            perturbed_rpm: Some(1400),
            change_pct: Some(55.0),
            run_id: run_id.into(),
            validated_unix_ms: 1,
        }
    }

    /// `BRD-a`: the boot prune deletes every record whose key is not a live
    /// header id. A record a pre-DEC-442 daemon saved under the it87 v2.0
    /// suffixed spelling must survive it, re-keyed — so the load and the prune
    /// are driven together, the way `main.rs` runs them.
    #[test]
    fn a_suffixed_record_survives_the_boot_prune_under_its_canonical_id() {
        let dir = tempfile::tempdir().unwrap();
        let suffixed = "hwmon:it8696_a008090a:it87.2624:pwm5:pwm5";
        let canonical = "hwmon:it8696:it87.2624:pwm5:pwm5";
        let mut store = ControlPathStore::default();
        store.records.insert(
            suffixed.into(),
            record(suffixed, "hwmon:it8696_a008090a:it87.2624:pwm5:pwm5", "r1"),
        );
        save_to(dir.path(), &store).unwrap();

        let loaded = load_from(dir.path());
        let pruned =
            crate::api::handlers::discovery::prune_store_to_live(&loaded, &[canonical.into()]);
        let kept = pruned
            .get(canonical)
            .expect("the record must survive the prune");
        assert_eq!(kept.header_id, canonical);
        assert_eq!(kept.tach_ids, vec![canonical.to_string()]);
    }

    #[test]
    fn where_both_spellings_hold_a_record_the_suffixed_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        let suffixed = "hwmon:it8696_a008090a:it87.2624:pwm5:pwm5";
        let canonical = "hwmon:it8696:it87.2624:pwm5:pwm5";
        let mut store = ControlPathStore::default();
        store.records.insert(
            canonical.into(),
            record(canonical, canonical, "before-rebuild"),
        );
        store
            .records
            .insert(suffixed.into(), record(suffixed, suffixed, "after-rebuild"));
        save_to(dir.path(), &store).unwrap();

        let loaded = load_from(dir.path());
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.get(canonical).unwrap().run_id, "after-rebuild");
    }
}
