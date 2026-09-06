//! Persisted PWM response baselines — the "learned expected range" of
//! `AIO-Phase8 Batch 2 §6`.
//!
//! Stored at `{state_dir}/pwm_baselines.json`, one document keyed by header id.
//!
//! # Why persist at all
//!
//! `§6`'s worked example compares a **new** observation against a **previously
//! learned** model ("Learned expected 900-1150 RPM / Observed 3350 RPM"). An
//! in-run band cannot do that: it would only ever say a reading disagreed with
//! its own siblings. Answering "has this fan changed since last month" needs the
//! model to outlive the run, and therefore the process.
//!
//! # What it is NOT
//!
//! **Nothing in the control path reads this.** It is diagnostic evidence, not a
//! control input: DEC-334 rules that out explicitly, because a learned model
//! consulted on the 1 Hz poll would put a derived statistic between a sensor and
//! a fan. It is read at exactly one place — building a characterisation summary.
//!
//! # Invalidation, and why nothing has to detect a hardware change
//!
//! Identical to the control-path store, deliberately: the key is the header's
//! **stable id**, which embeds chip, device, `pwmN` and label. Swap the board or
//! change the driver and the id changes with it, so a stale record stops matching
//! any live header and [`PwmBaselineStore::prune_to_live`] drops it at boot.
//!
//! # Bounds
//!
//! The pair DEC-320 requires. Stat-first against
//! [`constants::PWM_BASELINES_MAX_BYTES`], and every string truncated at ingest
//! to [`constants::PWM_BASELINE_MAX_TEXT_BYTES`] with the point list capped at
//! [`constants::PWM_BASELINE_MAX_POINTS`]. The ingest bound is what makes acting
//! on the file bound safe: this daemon cannot write an over-size document, so
//! "too large" can only mean "written by something else".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_io;
use crate::constants;

const STORE_FILE: &str = "pwm_baselines.json";

/// One duty's learned RPM band.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PwmBaselinePoint {
    pub duty_pct: u8,
    pub rpm_min: u16,
    pub rpm_max: u16,
}

/// What one header's characterisation history has established.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PwmBaselineRecord {
    pub header_id: String,
    /// Ascending by duty, one entry per duty.
    #[serde(default)]
    pub points: Vec<PwmBaselinePoint>,
    #[serde(default)]
    pub min_rpm: Option<u16>,
    #[serde(default)]
    pub max_rpm: Option<u16>,
    #[serde(default)]
    pub worst_cv_pct: Option<f64>,
    #[serde(default)]
    pub bidirectional: bool,
    /// The run that last contributed. Cross-references a session's `evidence[]`.
    #[serde(default)]
    pub run_id: String,
    pub validated_unix_ms: u64,
    /// How many completed runs have contributed to this band.
    #[serde(default)]
    pub runs: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PwmBaselineStore {
    #[serde(default)]
    pub records: BTreeMap<String, PwmBaselineRecord>,
}

impl PwmBaselineStore {
    pub fn get(&self, header_id: &str) -> Option<&PwmBaselineRecord> {
        self.records.get(header_id)
    }

    /// Merge a completed run's observations into this header's band.
    ///
    /// **Widening, not replacement, and that is the cautious direction.** A band
    /// that only ever grows produces fewer OUTSIDE LEARNED RANGE reports as it
    /// accumulates runs, and `§6` is emphatic that this diagnostic must not cry
    /// wolf — "never state that an internal override definitely occurred",
    /// "do not label unexpected RPM as pump failure". Replacement would make the
    /// band track the most recent run and report every normal variation.
    ///
    /// Evicts the oldest record when full, like the control-path store: a
    /// baseline is a current fact about hardware, so the stale one is expendable.
    pub fn merge(&mut self, mut incoming: PwmBaselineRecord) {
        incoming.clamp();
        let key = incoming.header_id.clone();
        match self.records.get_mut(&key) {
            Some(existing) => existing.widen_with(incoming),
            None => {
                if self.records.len() >= constants::PWM_BASELINES_MAX_ENTRIES {
                    if let Some(oldest) = self
                        .records
                        .values()
                        .min_by_key(|r| r.validated_unix_ms)
                        .map(|r| r.header_id.clone())
                    {
                        log::info!(
                            "PWM baseline store is full ({} entries); evicting the oldest \
                             record for {oldest}",
                            constants::PWM_BASELINES_MAX_ENTRIES
                        );
                        self.records.remove(&oldest);
                    }
                }
                let mut fresh = incoming;
                fresh.runs = 1;
                self.records.insert(key, fresh);
            }
        }
    }

    /// Drop every record whose header is no longer discoverable.
    pub fn prune_to_live(&mut self, live_header_ids: &[String]) -> usize {
        let before = self.records.len();
        self.records
            .retain(|header_id, _| live_header_ids.iter().any(|live| live == header_id));
        before - self.records.len()
    }
}

impl PwmBaselineRecord {
    fn widen_with(&mut self, incoming: PwmBaselineRecord) {
        for p in incoming.points {
            match self.points.iter_mut().find(|e| e.duty_pct == p.duty_pct) {
                Some(existing) => {
                    existing.rpm_min = existing.rpm_min.min(p.rpm_min);
                    existing.rpm_max = existing.rpm_max.max(p.rpm_max);
                }
                None => self.points.push(p),
            }
        }
        self.points.sort_unstable_by_key(|p| p.duty_pct);
        self.points.truncate(constants::PWM_BASELINE_MAX_POINTS);
        self.min_rpm = match (self.min_rpm, incoming.min_rpm) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.max_rpm = match (self.max_rpm, incoming.max_rpm) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.worst_cv_pct = match (self.worst_cv_pct, incoming.worst_cv_pct) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.bidirectional |= incoming.bidirectional;
        self.run_id = incoming.run_id;
        self.validated_unix_ms = incoming.validated_unix_ms;
        self.runs = self.runs.saturating_add(1);
    }

    /// [SAFETY-adjacent] Bound every string and the point list at ingest, so one
    /// function decides this document's maximum size and the compile-time
    /// assertion in `constants.rs` can prove the file bound is reachable but not
    /// exceedable.
    fn clamp(&mut self) {
        let cap = constants::PWM_BASELINE_MAX_TEXT_BYTES;
        truncate(&mut self.header_id, cap);
        truncate(&mut self.run_id, cap);
        self.points.sort_unstable_by_key(|p| p.duty_pct);
        self.points.dedup_by_key(|p| p.duty_pct);
        self.points.truncate(constants::PWM_BASELINE_MAX_POINTS);
    }
}

/// Truncate on a **character** boundary, never a byte one: `String::truncate`
/// panics mid-codepoint, and a header label can legitimately contain non-ASCII.
fn truncate(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

pub fn store_path_in(dir: &Path) -> PathBuf {
    dir.join(STORE_FILE)
}

/// Load the store, or an empty one. Never an error — a diagnostic baseline is a
/// convenience, and failing daemon startup over it would be the wrong trade.
pub fn load_from(dir: &Path) -> PwmBaselineStore {
    let path = store_path_in(dir);
    let Ok(meta) = std::fs::metadata(&path) else {
        return PwmBaselineStore::default();
    };
    if meta.len() > constants::PWM_BASELINES_MAX_BYTES {
        log::warn!(
            "PWM baseline store {} is {} bytes, over the {} byte cap — discarding it. \
             This daemon cannot write a document that large, so it was written by \
             something else.",
            path.display(),
            meta.len(),
            constants::PWM_BASELINES_MAX_BYTES
        );
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("could not remove the over-size PWM baseline store: {e}");
        }
        return PwmBaselineStore::default();
    }
    match atomic_io::read_to_string_with_cap(&path, constants::PWM_BASELINES_MAX_BYTES) {
        Ok(text) => match serde_json::from_str::<PwmBaselineStore>(&text) {
            Ok(store) => store,
            Err(e) => {
                log::warn!(
                    "PWM baseline store {} will not parse ({e}); starting empty",
                    path.display()
                );
                PwmBaselineStore::default()
            }
        },
        Err(e) => {
            log::warn!(
                "PWM baseline store {} unreadable ({e}); starting empty",
                path.display()
            );
            PwmBaselineStore::default()
        }
    }
}

pub fn save_to(dir: &Path, store: &PwmBaselineStore) -> Result<(), String> {
    let path = store_path_in(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let bytes =
        serde_json::to_vec_pretty(store).map_err(|e| format!("serialise baseline store: {e}"))?;
    // Belt and braces against the compile-time assertion: if a future field ever
    // makes a bounded store exceed the file cap, fail the WRITE rather than
    // producing a document the next boot will discard.
    if bytes.len() as u64 > constants::PWM_BASELINES_MAX_BYTES {
        return Err(format!(
            "PWM baseline store would be {} bytes, over the {} byte cap",
            bytes.len(),
            constants::PWM_BASELINES_MAX_BYTES
        ));
    }
    atomic_io::write_atomic(&path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(header: &str, pts: &[(u8, u16, u16)], when: u64) -> PwmBaselineRecord {
        PwmBaselineRecord {
            header_id: header.into(),
            points: pts
                .iter()
                .map(|&(duty_pct, rpm_min, rpm_max)| PwmBaselinePoint {
                    duty_pct,
                    rpm_min,
                    rpm_max,
                })
                .collect(),
            min_rpm: pts.iter().map(|p| p.1).min(),
            max_rpm: pts.iter().map(|p| p.2).max(),
            worst_cv_pct: None,
            bidirectional: true,
            run_id: "char-1".into(),
            validated_unix_ms: when,
            runs: 1,
        }
    }

    /// §6's band must WIDEN as runs accumulate. Replacement would make it track
    /// the most recent run and report every normal variation as outside range —
    /// the cry-wolf failure §6 warns against.
    #[test]
    fn a_second_run_widens_the_band_rather_than_replacing_it() {
        let mut store = PwmBaselineStore::default();
        store.merge(rec("h1", &[(50, 1900, 2000)], 10));
        store.merge(rec("h1", &[(50, 1850, 2100)], 20));
        let got = store.get("h1").expect("record");
        assert_eq!(got.points[0].rpm_min, 1850);
        assert_eq!(got.points[0].rpm_max, 2100);
        assert_eq!(
            got.runs, 2,
            "the band records how much evidence it rests on"
        );
        assert_eq!(got.validated_unix_ms, 20, "and when it was last confirmed");
    }

    /// A narrower second run must not narrow the band — that is the direction
    /// that manufactures false OUTSIDE LEARNED RANGE reports.
    #[test]
    fn a_narrower_run_never_shrinks_what_was_already_learned() {
        let mut store = PwmBaselineStore::default();
        store.merge(rec("h1", &[(50, 1000, 3000)], 10));
        store.merge(rec("h1", &[(50, 1900, 2000)], 20));
        let got = store.get("h1").expect("record");
        assert_eq!((got.points[0].rpm_min, got.points[0].rpm_max), (1000, 3000));
    }

    #[test]
    fn a_new_duty_extends_the_band_without_disturbing_the_others() {
        let mut store = PwmBaselineStore::default();
        store.merge(rec("h1", &[(30, 900, 950)], 10));
        store.merge(rec("h1", &[(60, 2000, 2100)], 20));
        let got = store.get("h1").expect("record");
        assert_eq!(got.points.len(), 2);
        assert_eq!(got.points[0].duty_pct, 30, "kept sorted by duty");
        assert_eq!(got.points[1].duty_pct, 60);
    }

    #[test]
    fn the_store_evicts_the_oldest_record_when_full() {
        let mut store = PwmBaselineStore::default();
        for i in 0..constants::PWM_BASELINES_MAX_ENTRIES {
            store.merge(rec(&format!("h{i}"), &[(50, 1000, 1100)], 1000 + i as u64));
        }
        assert_eq!(store.records.len(), constants::PWM_BASELINES_MAX_ENTRIES);
        store.merge(rec("newcomer", &[(50, 1000, 1100)], 9999));
        assert_eq!(store.records.len(), constants::PWM_BASELINES_MAX_ENTRIES);
        assert!(
            store.get("h0").is_none(),
            "the oldest is the expendable one"
        );
        assert!(store.get("newcomer").is_some());
    }

    /// A record survives only while its header does. The key embeds chip, device,
    /// `pwmN` and label, so nothing has to *detect* a hardware change.
    #[test]
    fn pruning_drops_records_whose_header_no_longer_exists() {
        let mut store = PwmBaselineStore::default();
        store.merge(rec("hwmon:it87:pwm1:PUMP", &[(50, 1000, 1100)], 1));
        store.merge(rec("hwmon:gone:pwm9:OLD", &[(50, 1000, 1100)], 2));
        let dropped = store.prune_to_live(&["hwmon:it87:pwm1:PUMP".to_string()]);
        assert_eq!(dropped, 1);
        assert!(store.get("hwmon:gone:pwm9:OLD").is_none());
        assert!(store.get("hwmon:it87:pwm1:PUMP").is_some());
    }

    /// [DEC-320] Bound the input at ingest, and assert the REALISED artefact —
    /// the serialised length — not a re-derivation of the cap's own arithmetic.
    #[test]
    fn a_maximally_full_store_still_fits_inside_its_own_file_cap() {
        let mut store = PwmBaselineStore::default();
        let long = "x".repeat(constants::PWM_BASELINE_MAX_TEXT_BYTES * 4);
        for i in 0..constants::PWM_BASELINES_MAX_ENTRIES {
            let pts: Vec<(u8, u16, u16)> = (0..constants::PWM_BASELINE_MAX_POINTS + 20)
                .map(|d| ((d % 100) as u8, u16::MAX, u16::MAX))
                .collect();
            let mut r = rec(&format!("{long}-{i}"), &pts, i as u64);
            r.run_id = long.clone();
            store.merge(r);
        }
        let bytes = serde_json::to_vec_pretty(&store).expect("serialise");
        assert!(
            (bytes.len() as u64) <= constants::PWM_BASELINES_MAX_BYTES,
            "a bounded store serialised to {} bytes, over the {} byte file cap — \
             the ingest bound and the file bound have drifted apart",
            bytes.len(),
            constants::PWM_BASELINES_MAX_BYTES
        );
    }

    #[test]
    fn ingest_truncates_oversized_text_and_caps_the_point_list() {
        let mut store = PwmBaselineStore::default();
        let long = "y".repeat(constants::PWM_BASELINE_MAX_TEXT_BYTES * 3);
        let pts: Vec<(u8, u16, u16)> = (1..=constants::PWM_BASELINE_MAX_POINTS as u8 + 30)
            .map(|d| (d, 100, 200))
            .collect();
        let mut r = rec(&long, &pts, 1);
        r.run_id = long.clone();
        store.merge(r);
        let got = store.records.values().next().expect("record");
        assert!(got.header_id.len() <= constants::PWM_BASELINE_MAX_TEXT_BYTES);
        assert!(got.run_id.len() <= constants::PWM_BASELINE_MAX_TEXT_BYTES);
        assert!(got.points.len() <= constants::PWM_BASELINE_MAX_POINTS);
    }

    /// Truncation must land on a character boundary; a UTF-8 label would panic a
    /// naive `String::truncate`.
    #[test]
    fn ingest_truncation_does_not_split_a_multibyte_character() {
        let mut store = PwmBaselineStore::default();
        let s = "é".repeat(constants::PWM_BASELINE_MAX_TEXT_BYTES);
        store.merge(rec(&s, &[(50, 1, 2)], 1));
        let got = store.records.values().next().expect("record");
        assert!(got.header_id.len() <= constants::PWM_BASELINE_MAX_TEXT_BYTES);
        assert!(got.header_id.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_missing_store_loads_empty_rather_than_failing() {
        let dir = std::env::temp_dir().join(format!("ofc-baseline-missing-{}", std::process::id()));
        assert!(load_from(&dir).records.is_empty());
    }

    #[test]
    fn a_round_trip_through_disk_preserves_the_band() {
        let dir = std::env::temp_dir().join(format!("ofc-baseline-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = PwmBaselineStore::default();
        store.merge(rec("h1", &[(30, 900, 950), (60, 2000, 2100)], 42));
        save_to(&dir, &store).expect("save");
        let back = load_from(&dir);
        assert_eq!(back, store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An over-size document can only have been written by something else,
    /// *because* of the ingest bound — which is what makes deleting it correct
    /// rather than data loss.
    #[test]
    fn an_oversize_document_is_discarded_and_removed() {
        let dir = std::env::temp_dir().join(format!("ofc-baseline-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = store_path_in(&dir);
        let filler = "z".repeat((constants::PWM_BASELINES_MAX_BYTES + 1024) as usize);
        std::fs::write(&path, filler).expect("write");
        assert!(load_from(&dir).records.is_empty());
        assert!(
            !path.exists(),
            "the unreadable document is removed, not left to fail every boot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_document_starts_empty_rather_than_failing_startup() {
        let dir = std::env::temp_dir().join(format!("ofc-baseline-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(store_path_in(&dir), "{not json").expect("write");
        assert!(load_from(&dir).records.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
