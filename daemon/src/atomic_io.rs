//! Crash-safe atomic file write helper.
//!
//! Persists `bytes` to `path` so readers always see either the previous
//! complete file or the new complete file — never a partial or zero-length
//! file. Survives:
//!
//! - **Process crash mid-write** — the destination is only renamed once the
//!   temp file is fully written and fsynced.
//! - **Kernel panic / power loss between rename and disk flush** — the temp
//!   file's data is fsynced *before* the rename, so a crash after rename
//!   cannot expose an empty file (which is the classic ext4/btrfs failure
//!   mode for `write + rename` without fsync). The parent directory is also
//!   fsynced after rename so the rename itself is durable.
//! - **Two writers racing on the same destination** (AUD3-b) — each call gets
//!   its own uniquely named scratch file, so neither can truncate the other's
//!   partial content and rename a hybrid document into place. Which writer wins
//!   the destination is still a race (that is `AIO1-d`, serialised by the
//!   `/config/*` write lock), but the winner's document is always whole.
//!
//! `std::fs::write` alone does NOT provide either guarantee — it returns as
//! soon as the kernel has buffered the write, before any data hits the disk.
//! `std::fs::rename` is POSIX-atomic but only over the *current* filesystem
//! state; the rename can land in the journal while the data is still in the
//! page cache, leaving a zero-length file on the next mount.
//!
//! The shape mirrors the Python GUI's `paths.atomic_write` helper (see
//! `control-ofc-gui/src/control_ofc/paths.py`) and the established
//! "tmp + fsync + rename + dir fsync" pattern documented by Dan Luu and the
//! `atomic-write-file` crate.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Atomically write `bytes` to `path` with crash safety guarantees.
///
/// On Unix the resulting file has `0o600` permissions (owner read/write
/// only). Both call sites in this crate persist daemon-private state so
/// they want owner-only perms; if a future caller needs a different mode,
/// expose it as an argument.
///
/// Errors are returned as a human-readable `String` (matching the existing
/// call sites in `daemon_state.rs` and `runtime_config.rs`). The parent
/// directory must exist — callers create it explicitly so they can report
/// dir-creation failures distinctly from write failures.
///
/// **Concurrency (AUD3-b).** The temp file is uniquely named per call, so two
/// concurrent writers to the same destination never share a scratch file. Each
/// still races to `rename(2)`, so the *last* writer wins the destination — a
/// lost update, which is `AIO1-d`'s problem and is serialised by the
/// `/config/*` write lock — but neither can any longer truncate the other's
/// half-written scratch file and publish a hybrid document.
///
/// A caller therefore no longer needs a private lock *for corruption*. It may
/// still need one for **ordering** — `validation::recorder::save_lock` is kept
/// for exactly that (a stale `recording` flush must not rename over a
/// `completed` document), so do not read this note as licence to remove it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = tmp_path_for(path);
    match write_via_tmp(&tmp, path, bytes) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort cleanup on EVERY failure, not just a failed rename.
            // A unique temp name is what makes this necessary: the old fixed
            // `{path}.tmp` was self-limiting because the next write truncated
            // whatever a failed one left behind, so at most one stale file
            // existed per destination. Unique names remove that ceiling, and a
            // leak per failed write would be a new defect introduced by the fix.
            // A crash between create and rename can still leak one file — that
            // is the residual case, and it is bounded by hard-kill frequency.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Steps 1-4 of the write. Split out of [`write_atomic`] so a failure at *any*
/// step unwinds through one cleanup path rather than four early returns.
fn write_via_tmp(tmp: &Path, path: &Path, bytes: &[u8]) -> Result<(), String> {
    // 1. Write the temp file and fsync its data + metadata before
    //    dropping the descriptor. sync_all() is fsync(2), not fdatasync —
    //    it covers both data and metadata, which is what we want before
    //    the file becomes durable under another name.
    {
        let mut f =
            File::create(tmp).map_err(|e| format!("create tmp file '{}': {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("write tmp file '{}': {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync tmp file '{}': {e}", tmp.display()))?;
    }

    // 2. Set owner-only permissions on the temp file before rename, so the
    //    visible final file is *never* observable with looser perms.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("set permissions on '{}': {e}", tmp.display()))?;
    }

    // 3. POSIX-atomic rename. The temp file ceases to exist under its own name
    //    on success, so the caller's cleanup only ever fires on failure.
    std::fs::rename(tmp, path)
        .map_err(|e| format!("rename '{}' to '{}': {e}", tmp.display(), path.display()))?;

    // 4. fsync the parent directory so the rename itself is durable. The
    //    file content is already on disk; if we skip this and the box loses
    //    power, the kernel may forget the rename and present the old name.
    //    Failure here is not fatal — log it and continue. The file content
    //    is intact.
    if let Some(parent) = path.parent() {
        if let Err(e) = fsync_dir(parent) {
            log::warn!(
                "atomic_io: parent dir fsync of '{}' failed (rename may not be \
                durable across power loss): {e}",
                parent.display()
            );
        }
    }

    Ok(())
}

/// Create `dir` (and any missing parents) as a daemon-private directory,
/// `0o700` on Unix — owner-only, matching the `0o600` files [`write_atomic`]
/// places inside it. Idempotent: an already-existing directory has its mode
/// tightened to `0o700`, so a directory a pre-hardening daemon created `0o755`
/// is migrated on the next write (DEC-173).
///
/// On non-Unix targets this is a plain recursive create (mode is a Unix
/// concept). Errors are returned as a human-readable `String`, matching the
/// other helpers in this module and the call sites in `profile_store.rs`,
/// `daemon_state.rs`, and `runtime_config.rs`.
pub fn create_dir_private(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        // Create race-free at 0o700: the directory is never briefly group/world
        // readable under the process umask in the window between create + chmod.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("create dir '{}': {e}", dir.display()))?;
        // `recursive(true)` is a no-op (Ok) on an existing directory and does
        // NOT re-apply the mode, so tighten an existing 0o755 dir explicitly.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("set dir mode '{}': {e}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map_err(|e| format!("create dir '{}': {e}", dir.display()))?;
    }
    Ok(())
}

/// A unique, hidden scratch path beside `path` (AUD3-b).
///
/// **This used to be a fixed `{path}.tmp`, and that was the defect.**
/// [`write_atomic`] opens it with `File::create`, which *truncates* — so two
/// writers to the same destination shared one scratch file and could each
/// overwrite the other's partial content, then rename the hybrid into place.
/// `validation::recorder` documented that hazard and guarded it with a private
/// save lock; the other four call sites (`runtime_config`, `daemon_state`,
/// `profile_store`, `validation::store`) did not, and could not reasonably be
/// expected to — a helper that creates a hazard every caller must independently
/// know about is the broken abstraction of DEC-276. Uniqueness belongs here, so
/// that all five call sites are fixed at once and a sixth cannot reintroduce it.
///
/// Shape: `.{filename}.tmp.{pid}.{counter}`. The **pid** separates two processes
/// writing the same path (a daemon and a test binary, or an old and a new daemon
/// mid-restart); the **counter** separates two threads or tasks inside one
/// process. `Relaxed` is sufficient and is not a shortcut: `fetch_add` is a
/// read-modify-write, so every caller observes a distinct value in the atomic's
/// modification order regardless of ordering — ordering would only matter if the
/// counter published *other* memory, and it does not.
///
/// The leading dot mirrors the GUI's twin helper (`paths.py::atomic_write`, which
/// uses `mkstemp(prefix=".")`) and keeps a leaked scratch file out of a plain
/// directory listing. Both directory readers in this crate filter on a `.json`
/// extension, so neither the old name nor this one can be mistaken for content.
fn tmp_path_for(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".tmp.{}.{n}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Maximum size (bytes) of a daemon-read config/state/profile file (JSON or
/// TOML). Matches the GUI's `MAX_IMPORT_BYTES` (paths.py). Files larger than
/// this are rejected rather than buffered whole — a local DoS /
/// accidental-huge-file guard for a long-lived root process.
pub const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// Read a file to a `String`, rejecting anything larger than [`MAX_CONFIG_BYTES`]
/// instead of buffering it whole. Drop-in for `std::fs::read_to_string` at the
/// daemon's config/state/profile read sites (`profile`, `profile_store`,
/// `daemon_state`, `runtime_config`). Sysfs/proc reads stay uncapped — the
/// kernel bounds them.
pub fn read_to_string_capped(path: &Path) -> std::io::Result<String> {
    read_to_string_with_cap(path, MAX_CONFIG_BYTES)
}

/// [`read_to_string_capped`] with a caller-chosen cap.
///
/// The validation session store reads through this rather than the config cap:
/// its documents are daemon-produced evidence whose size scales with the cooling
/// topology, and holding them to a *config* file's budget is what made a normal
/// multi-member session unreadable (`AUD3-i`). The cap is still enforced — an
/// unbounded read of a corrupt file is the thing this helper exists to prevent —
/// it is simply the store's own, tied to the store's write budget.
pub fn read_to_string_with_cap(path: &Path, cap: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    // `saturating_add`: a `u64::MAX` cap would wrap `take` to 0 and return an
    // empty string — a cap helper that fails OPEN. No caller passes that today.
    File::open(path)?
        .take(cap.saturating_add(1))
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {cap}-byte cap: {}", path.display()),
        ));
    }
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_with_cap_does_not_wrap_at_the_maximum_cap() {
        // `cap + 1` panics in debug and wraps to `take(0)` in release, so an
        // extreme cap would have returned `Ok("")` — a cap helper failing OPEN.
        // No caller passes this today; the helper is public, so it is bounded.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("max.json");
        std::fs::write(&p, b"{\"hello\": 1}").unwrap();
        assert_eq!(
            read_to_string_with_cap(&p, u64::MAX).unwrap(),
            "{\"hello\": 1}"
        );
    }

    #[test]
    fn read_capped_reads_a_normal_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("ok.json");
        std::fs::write(&p, b"{\"hello\": 1}").unwrap();
        assert_eq!(read_to_string_capped(&p).unwrap(), "{\"hello\": 1}");
    }

    #[test]
    fn read_capped_rejects_oversized_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("huge.json");
        // One byte over the 4 MiB cap must be refused, not read into memory.
        std::fs::write(&p, vec![b'a'; (MAX_CONFIG_BYTES + 1) as usize]).unwrap();
        let err = read_to_string_capped(&p).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn writes_content_to_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.txt");

        write_atomic(&path, b"hello").unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_eq!(content, b"hello");
    }

    /// Every entry in `dir`, including dotfiles — the scratch files are hidden,
    /// so a listing that skips them would assert nothing (AUD3-b).
    fn entries(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn does_not_leave_tmp_file_after_successful_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.txt");

        write_atomic(&path, b"data").unwrap();

        // Assert the realised directory contents rather than the absence of one
        // computed name: the scratch name is now unique, so a test that rebuilt
        // it here would be re-deriving production's naming rule and would share
        // its blind spot (DEC-320's lesson).
        assert_eq!(entries(tmp.path()), vec!["file.txt".to_string()]);
    }

    #[test]
    fn replaces_existing_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.txt");

        std::fs::write(&path, "old-content").unwrap();
        write_atomic(&path, b"new-content").unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_eq!(content, b"new-content");
    }

    #[cfg(unix)]
    #[test]
    fn written_file_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.txt");

        write_atomic(&path, b"data").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_private_creates_owner_only_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");

        create_dir_private(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_private_tightens_existing_loose_dir() {
        // A directory a pre-hardening daemon created 0o755 must be migrated to
        // 0o700 on the next write (DEC-173) — the create is a no-op but the mode
        // is still tightened.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        create_dir_private(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn create_dir_private_is_idempotent_and_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("b"); // missing parent → recursive create

        create_dir_private(&dir).unwrap();
        create_dir_private(&dir).unwrap(); // second call must not error

        assert!(dir.is_dir());
    }

    #[test]
    fn fails_if_parent_directory_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing").join("file.txt");

        // The helper does not create parent dirs — callers do so explicitly
        // before invoking it. Verify that absence is reported as an error
        // rather than silently succeeding.
        let err = write_atomic(&path, b"x").unwrap_err();
        assert!(
            err.contains("create tmp file"),
            "expected create-tmp error, got: {err}"
        );
    }

    #[test]
    fn rename_failure_cleans_up_tmp_file() {
        // Force rename to fail by making the destination a non-empty
        // directory: rename(file, dir) fails with EISDIR / ENOTEMPTY on
        // Linux. Assert that the helper does not leave a tmp file behind.
        let tmp = tempfile::tempdir().unwrap();
        let dst_dir = tmp.path().join("dst");
        std::fs::create_dir(&dst_dir).unwrap();
        // Put something inside so rename(tmp_file, dst_dir) cannot succeed
        // by replacing an empty directory.
        std::fs::write(dst_dir.join("blocker"), "x").unwrap();

        let err = write_atomic(&dst_dir, b"y").unwrap_err();
        assert!(err.contains("rename"), "expected rename error, got: {err}");

        // Only the destination directory remains — no scratch sibling. Checked
        // by listing rather than by rebuilding the (now unique) scratch name.
        assert_eq!(entries(tmp.path()), vec!["dst".to_string()]);
    }

    #[test]
    fn a_failed_write_leaves_no_scratch_file() {
        // The cleanup used to fire only on a failed *rename*. With a fixed
        // scratch name that was survivable — the next write truncated whatever
        // was left. With unique names it would leak one file per failed write,
        // so cleanup has to cover every failure step. Step 2 (set_permissions)
        // is the one reachable from a test: make the parent read-only after the
        // scratch file exists and... it is not, portably. Use the step-3 path,
        // which shares the single cleanup arm, and additionally assert the
        // *count* stays flat across repeated failures — that is what a per-call
        // leak would break and a single-shot assertion would not.
        let tmp = tempfile::tempdir().unwrap();
        let dst_dir = tmp.path().join("dst");
        std::fs::create_dir(&dst_dir).unwrap();
        std::fs::write(dst_dir.join("blocker"), "x").unwrap();

        for _ in 0..5 {
            write_atomic(&dst_dir, b"y").unwrap_err();
        }

        assert_eq!(
            entries(tmp.path()),
            vec!["dst".to_string()],
            "each failed write must clean up its own scratch file"
        );
    }

    #[test]
    fn tmp_path_is_a_hidden_sibling_of_destination() {
        // The rename must stay within one filesystem, so the scratch file has to
        // be a sibling. Asserted as a relationship, not as a literal name — the
        // name now carries a pid and a counter.
        for dst in [
            "/var/lib/control-ofc/daemon_state.json",
            "/var/lib/control-ofc/runtime.toml",
        ] {
            let dst = Path::new(dst);
            let p = tmp_path_for(dst);
            assert_eq!(p.parent(), dst.parent(), "scratch file must be a sibling");
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let stem = dst.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.starts_with(&format!(".{stem}.tmp.")),
                "scratch name should be a hidden, traceable derivative of the \
                 destination, got: {name}"
            );
            // Neither directory reader in this crate may mistake it for content:
            // both filter on a `.json` extension.
            assert_ne!(p.extension().and_then(|e| e.to_str()), Some("json"));
        }
    }

    #[test]
    fn tmp_paths_are_unique_per_call() {
        // The defect (AUD3-b): a fixed name meant two concurrent writers shared
        // one scratch file, and `File::create` truncates — so one could publish
        // a document half-overwritten by the other. Uniqueness is the fix, and
        // it is asserted here directly because the corruption it prevents is
        // only reachable through a race.
        let dst = Path::new("/var/lib/control-ofc/runtime.toml");
        let a = tmp_path_for(dst);
        let b = tmp_path_for(dst);
        assert_ne!(a, b, "two calls must not share a scratch path");
    }

    #[test]
    fn concurrent_writers_never_publish_a_hybrid_document() {
        // The call-site test for the property above: many threads writing
        // distinct, distinguishable payloads to ONE destination. Whoever wins
        // the rename is a race (that is `AIO1-d`), but the published bytes must
        // always be exactly one writer's whole payload — never a mixture, never
        // short. Asserts the realised artefact on disk, not a re-derivation of
        // the naming rule.
        //
        // Payloads differ in length as well as content, because the original
        // defect's signature is a long document with a shorter one written over
        // its head — which a same-length check would miss.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.toml");
        const WRITERS: usize = 8;
        let payloads: Vec<Vec<u8>> = (0..WRITERS)
            .map(|i| vec![b'a' + i as u8; 1024 * (i + 1)])
            .collect();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));
        std::thread::scope(|scope| {
            for payload in &payloads {
                let barrier = barrier.clone();
                let path = &path;
                scope.spawn(move || {
                    barrier.wait();
                    // A loser's rename can fail if its scratch file is gone; the
                    // point of the test is the published bytes, not the verdict.
                    let _ = write_atomic(path, payload);
                });
            }
        });

        let published = std::fs::read(&path).unwrap();
        assert!(
            payloads.contains(&published),
            "published document is not any single writer's payload: {} bytes \
             starting {:?}",
            published.len(),
            &published[..published.len().min(8)]
        );
        assert_eq!(
            entries(tmp.path()),
            vec!["runtime.toml".to_string()],
            "no scratch file may survive a concurrent write"
        );
    }

    #[test]
    fn empty_content_is_persisted_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.txt");

        write_atomic(&path, b"").unwrap();

        let content = std::fs::read(&path).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn large_content_is_persisted_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.bin");
        let body = vec![0xABu8; 256 * 1024]; // 256 KiB — exercises multi-block writes

        write_atomic(&path, &body).unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_eq!(content.len(), body.len());
        assert_eq!(content, body);
    }
}
