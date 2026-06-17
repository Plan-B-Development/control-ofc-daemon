//! Daemon-owned profile storage (DEC-160).
//!
//! The daemon is the store of record for GUI-authored profiles. Profiles are
//! persisted as `{store_dir}/{id}.json` under the daemon's state dir
//! (`/var/lib/control-ofc/profiles/` by default) — daemon-private, read by
//! clients via the API, never written by the GUI directly.
//!
//! By convention the **store dir is the FIRST entry of the profile search
//! dirs**; `main` prepends it (see `with_store_dir`) so it survives config
//! reload. Read-only package presets live in later search dirs (e.g.
//! `/etc/control-ofc/profiles/`) — they are discoverable and *shadowable* by a
//! stored profile of the same id, but are never written or deleted here.
//!
//! Writes go through [`crate::atomic_io::write_atomic`] (tmp + fsync + rename +
//! parent-dir fsync), so a crash leaves either the previous or the new complete
//! file, never a partial one. We persist the profile document as supplied
//! (round-tripped through `serde_json::Value`) rather than a re-serialized
//! [`DaemonProfile`], so fields the daemon model doesn't yet know are preserved
//! (forward compatibility).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::api::responses::ProfileSummary;
use crate::atomic_io::write_atomic;
use crate::profile::{is_safe_profile_id, DaemonProfile};

/// Path of a stored profile within `dir`. `None` if the id is unsafe.
fn profile_path(dir: &Path, id: &str) -> Option<PathBuf> {
    is_safe_profile_id(id).then(|| dir.join(format!("{id}.json")))
}

/// Persist a profile document to `{store_dir}/{id}.json`.
///
/// `bytes` is the validated profile document (the caller serializes the
/// uploaded `serde_json::Value`, not a re-serialized model). The id must be
/// filename-safe and must equal the document's `id` field (the caller checks).
pub fn save_raw(store_dir: &Path, id: &str, bytes: &[u8]) -> Result<(), String> {
    let path = profile_path(store_dir, id).ok_or_else(|| format!("unsafe profile id: {id:?}"))?;
    std::fs::create_dir_all(store_dir)
        .map_err(|e| format!("create profile store dir '{}': {e}", store_dir.display()))?;
    write_atomic(&path, bytes)
}

/// Whether a profile with `id` already exists in the store (the write
/// location). Store-scoped: a read-only preset of the same id in another search
/// dir is NOT a conflict — it can be shadowed by a created profile.
pub fn exists_in_store(store_dir: &Path, id: &str) -> bool {
    profile_path(store_dir, id)
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Delete a stored profile. `Ok(true)` if a file was removed, `Ok(false)` if
/// none existed (idempotent). Only the store dir is touched — presets in other
/// search dirs cannot be deleted.
pub fn delete(store_dir: &Path, id: &str) -> Result<bool, String> {
    let Some(path) = profile_path(store_dir, id) else {
        return Err(format!("unsafe profile id: {id:?}"));
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("delete profile '{}': {e}", path.display())),
    }
}

/// Fetch a stored/preset profile as a lossless JSON value (preserves any fields
/// the daemon model doesn't know). Resolves across all `search_dirs` in order
/// (store first), so a stored profile shadows a same-id preset.
pub fn get_raw(search_dirs: &[PathBuf], id: &str) -> Option<serde_json::Value> {
    let path = crate::profile::find_profile(id, search_dirs)?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// List profiles across `search_dirs` (store ∪ presets ∪ …), deduped by id with
/// earlier dirs winning (the store, being first, shadows same-id presets).
/// Unreadable or unparseable files are skipped.
pub fn list(search_dirs: &[PathBuf]) -> Vec<ProfileSummary> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<ProfileSummary> = Vec::new();
    for dir in search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut json_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        json_files.sort(); // deterministic order within a directory
        for path in json_files {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(profile) = serde_json::from_str::<DaemonProfile>(&content) else {
                continue;
            };
            if seen.insert(profile.id.clone()) {
                out.push(ProfileSummary {
                    id: profile.id,
                    name: profile.name,
                    description: profile.description,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str =
        r#"{"id":"%ID%","name":"%NAME%","description":"d","version":7,"controls":[],"curves":[]}"#;

    fn sample(id: &str, name: &str) -> Vec<u8> {
        SAMPLE
            .replace("%ID%", id)
            .replace("%NAME%", name)
            .into_bytes()
    }

    #[test]
    fn save_then_get_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        save_raw(dir.path(), "abc123", &sample("abc123", "Mine")).unwrap();
        assert!(dir.path().join("abc123.json").exists());
        let got = get_raw(&[dir.path().to_path_buf()], "abc123").unwrap();
        assert_eq!(got["id"], "abc123");
        assert_eq!(got["name"], "Mine");
    }

    #[test]
    fn save_raw_is_lossless_for_unknown_fields() {
        // Forward-compat: a field the daemon model doesn't know must survive a
        // store→get round-trip (we persist the document, not a re-serialized
        // DaemonProfile).
        let dir = tempfile::tempdir().unwrap();
        let doc =
            br#"{"id":"x","name":"X","version":99,"future_field":42,"controls":[],"curves":[]}"#;
        save_raw(dir.path(), "x", doc).unwrap();
        let got = get_raw(&[dir.path().to_path_buf()], "x").unwrap();
        assert_eq!(got["future_field"], 42);
        assert_eq!(got["version"], 99);
    }

    #[test]
    fn save_raw_rejects_unsafe_id() {
        let dir = tempfile::tempdir().unwrap();
        assert!(save_raw(dir.path(), "../escape", b"{}").is_err());
        assert!(save_raw(dir.path(), "a/b", b"{}").is_err());
        assert!(save_raw(dir.path(), "", b"{}").is_err());
        // Nothing escaped the store dir.
        assert!(!dir.path().parent().unwrap().join("escape.json").exists());
    }

    #[test]
    fn delete_is_idempotent_and_store_scoped() {
        let dir = tempfile::tempdir().unwrap();
        save_raw(dir.path(), "p", &sample("p", "P")).unwrap();
        assert!(delete(dir.path(), "p").unwrap()); // removed
        assert!(!delete(dir.path(), "p").unwrap()); // already gone (idempotent)
        assert!(delete(dir.path(), "../escape").is_err());
    }

    #[test]
    fn exists_in_store_is_store_scoped() {
        let store = tempfile::tempdir().unwrap();
        let presets = tempfile::tempdir().unwrap();
        std::fs::write(presets.path().join("quiet.json"), sample("quiet", "Quiet")).unwrap();
        // Present in presets but NOT in the store → not a store conflict.
        assert!(!exists_in_store(store.path(), "quiet"));
        save_raw(store.path(), "quiet", &sample("quiet", "My Quiet")).unwrap();
        assert!(exists_in_store(store.path(), "quiet"));
    }

    #[test]
    fn list_unions_store_and_presets_store_wins() {
        let store = tempfile::tempdir().unwrap();
        let presets = tempfile::tempdir().unwrap();
        // Preset "quiet" + preset-only "perf"; store has its own "quiet" + "mine".
        std::fs::write(
            presets.path().join("quiet.json"),
            sample("quiet", "Preset Quiet"),
        )
        .unwrap();
        std::fs::write(presets.path().join("perf.json"), sample("perf", "Perf")).unwrap();
        save_raw(store.path(), "quiet", &sample("quiet", "My Quiet")).unwrap();
        save_raw(store.path(), "mine", &sample("mine", "Mine")).unwrap();

        // Search order: store first, then presets (mirrors with_store_dir).
        let dirs = vec![store.path().to_path_buf(), presets.path().to_path_buf()];
        let listed = list(&dirs);

        let ids: HashSet<&str> = listed.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, HashSet::from(["quiet", "perf", "mine"])); // deduped union
                                                                   // The store's "quiet" shadows the preset's.
        let quiet = listed.iter().find(|p| p.id == "quiet").unwrap();
        assert_eq!(quiet.name, "My Quiet");
    }

    #[test]
    fn list_skips_unparseable_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.json"), sample("good", "Good")).unwrap();
        std::fs::write(dir.path().join("bad.json"), b"not json").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), b"{}").unwrap();
        let listed = list(&[dir.path().to_path_buf()]);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "good");
    }
}
