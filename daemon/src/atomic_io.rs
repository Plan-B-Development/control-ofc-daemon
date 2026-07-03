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

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

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
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = tmp_path_for(path);

    // 1. Write the temp file and fsync its data + metadata before
    //    dropping the descriptor. sync_all() is fsync(2), not fdatasync —
    //    it covers both data and metadata, which is what we want before
    //    the file becomes durable under another name.
    {
        let mut f =
            File::create(&tmp).map_err(|e| format!("create tmp file '{}': {e}", tmp.display()))?;
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
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("set permissions on '{}': {e}", tmp.display()))?;
    }

    // 3. POSIX-atomic rename. On failure, best-effort clean up the temp
    //    file so a future save isn't blocked by a stale `.tmp` sibling.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "rename '{}' to '{}': {e}",
            tmp.display(),
            path.display()
        ));
    }

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

fn tmp_path_for(path: &Path) -> PathBuf {
    // Append `.tmp` literally so `daemon_state.json` → `daemon_state.json.tmp`
    // and `runtime.toml` → `runtime.toml.tmp`, matching the prior call
    // sites' behaviour.
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    s.into()
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Maximum size (bytes) of a daemon-read JSON config/state/profile file. Matches
/// the GUI's `MAX_IMPORT_BYTES` (paths.py). Files larger than this are rejected
/// rather than buffered whole — a local DoS / accidental-huge-file guard for a
/// long-lived root process.
pub const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// Read a file to a `String`, rejecting anything larger than [`MAX_CONFIG_BYTES`]
/// instead of buffering it whole. Drop-in for `std::fs::read_to_string` at the
/// daemon's JSON config/state/profile read sites (`profile`, `profile_store`,
/// `daemon_state`). Sysfs/proc reads stay uncapped — the kernel bounds them.
pub fn read_to_string_capped(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    File::open(path)?
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "file exceeds {MAX_CONFIG_BYTES}-byte cap: {}",
                path.display()
            ),
        ));
    }
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn does_not_leave_tmp_file_after_successful_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.txt");

        write_atomic(&path, b"data").unwrap();

        let tmp_sibling = tmp.path().join("file.txt.tmp");
        assert!(
            !tmp_sibling.exists(),
            "tmp sibling should be removed by rename"
        );
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

        let tmp_sibling = {
            let mut s = dst_dir.as_os_str().to_owned();
            s.push(".tmp");
            PathBuf::from(s)
        };
        assert!(
            !tmp_sibling.exists(),
            "tmp sibling should be cleaned up on rename failure"
        );
    }

    #[test]
    fn tmp_path_is_sibling_of_destination() {
        let p = tmp_path_for(Path::new("/var/lib/control-ofc/daemon_state.json"));
        assert_eq!(
            p,
            PathBuf::from("/var/lib/control-ofc/daemon_state.json.tmp")
        );

        let p = tmp_path_for(Path::new("/var/lib/control-ofc/runtime.toml"));
        assert_eq!(p, PathBuf::from("/var/lib/control-ofc/runtime.toml.tmp"));
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
