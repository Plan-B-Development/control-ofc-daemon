//! Filesystem-containment helpers shared by the profile-management and
//! runtime-config handlers.
//!
//! `path_is_within` is the canonicalized-containment check used both when
//! activating a profile by path and when confining a client's profile search
//! directories. `confine_added_dirs` (DEC-205) restricts a *non-root* Unix-
//! socket client to adding search directories under its own home directory, so
//! that on a multi-user host one user cannot point the daemon at another user's
//! files. Root (uid 0) — the daemon's own admin/CLI path — stays unrestricted.
//!
//! The confinement predicate is pure apart from an injected `home_for_uid`
//! resolver, so the decision logic is unit-tested without touching the real
//! password database or a live socket. `home_dir_for_uid` is the real resolver
//! (a thin, safe wrapper over `getpwuid_r`).

use std::ffi::CStr;
use std::path::{Path, PathBuf};

/// True if `candidate` equals or lives beneath any of `roots`.
///
/// Comparison is component-wise (`Path::starts_with`), so `/home/username` is
/// **not** treated as within `/home/user`. Both `candidate` and `roots` should
/// already be canonicalized by the caller so symlinks and `..` are resolved.
pub(crate) fn path_is_within(candidate: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| candidate.starts_with(root))
}

/// Resolve the home directory of `uid` via the system password database.
///
/// Returns `None` when the uid has no entry, the entry has no home, or the
/// lookup fails — callers treat that as "cannot confine" and fail closed. This
/// is the only impure part of the confinement path; the decision logic in
/// [`confine_added_dirs`] takes it as an injected function so it stays testable.
pub(crate) fn home_dir_for_uid(uid: u32) -> Option<PathBuf> {
    // Reentrant lookup with a growable scratch buffer. `getpwuid_r` reports
    // ERANGE when the buffer is too small; grow up to a sane cap. A `result`
    // of NULL with rc == 0 means "no such user".
    let mut buf = vec![0u8; 1024];
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `pwd` and `result` are valid out-pointers; `buf` is a valid
        // writable region of `buf.len()` bytes. `getpwuid_r` writes only within
        // these and never retains the pointers past this call.
        let rc = unsafe {
            libc::getpwuid_r(
                uid as libc::uid_t,
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < (1 << 20) {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc == libc::ERANGE {
            // Saturated the 1 MiB cap and still ERANGE — a pathological passwd
            // entry. We fail closed below, but leave a breadcrumb: otherwise
            // every non-root search-dir add would silently 400 with no
            // daemon-side clue as to why.
            log::warn!(
                "getpwuid_r for uid {uid} still ERANGE at {} bytes; giving up (home unresolved)",
                buf.len()
            );
        }
        if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
            // Lookup error, no such user, or an entry with no home directory.
            return None;
        }
        // SAFETY: `result` is non-null, so `pwd` was populated and `pw_dir`
        // points to a NUL-terminated string backed by `buf` for this scope.
        let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
        let s = dir.to_str().ok()?;
        if s.is_empty() {
            return None;
        }
        return Some(PathBuf::from(s));
    }
}

/// Decide whether a non-root client may add the given profile search
/// directories (DEC-205).
///
/// `dirs` are the directories being added; they have already passed the cheap
/// absolute-path + no-`..` text pre-filter in the handler. `peer_uid` is the
/// client's effective uid from `SO_PEERCRED` (`None` if it could not be read).
/// `home_for_uid` resolves a uid to its home directory (see
/// [`home_dir_for_uid`]).
///
/// Rules:
/// - **root (uid 0) is exempt** — unrestricted, preserving the pre-DEC-205
///   admin/CLI behaviour.
/// - a **non-root** caller may only add directories that *exist*, canonicalize
///   successfully (resolving symlinks/`..`), and lie within its own home
///   directory.
/// - if the uid or its home cannot be resolved, **fail closed** (reject).
///
/// Returns `Ok(())` if every directory is permitted, or `Err(message)` with a
/// user-facing reason for the first rejected directory.
pub(crate) fn confine_added_dirs(
    dirs: &[String],
    peer_uid: Option<u32>,
    home_for_uid: impl Fn(u32) -> Option<PathBuf>,
) -> Result<(), String> {
    // Root — the daemon's own admin/CLI path — stays unrestricted.
    if peer_uid == Some(0) {
        return Ok(());
    }

    // Non-root: identify the caller, or fail closed.
    let Some(uid) = peer_uid else {
        return Err(
            "cannot identify the requesting user (SO_PEERCRED unavailable); \
             refusing to add profile search directories"
                .to_string(),
        );
    };
    let Some(home) = home_for_uid(uid) else {
        return Err(format!(
            "cannot resolve the home directory for uid {uid}; \
             refusing to add profile search directories"
        ));
    };
    // Canonicalize the home root too, so a symlinked home (e.g. /home ->
    // /var/home) compares consistently with the canonicalized candidates.
    let home = home.canonicalize().unwrap_or(home);
    let roots = [home.clone()];

    for dir in dirs {
        // Canonicalize resolves symlinks and `.`/`..`, closing the text-only
        // bypass the pre-filter cannot. It requires the directory to exist and
        // be accessible — a confined caller may only add real directories.
        let canonical = match Path::new(dir).canonicalize() {
            Ok(c) => c,
            Err(_) => {
                return Err(format!(
                    "profile search directory must exist and be readable: {dir}"
                ));
            }
        };
        if !path_is_within(&canonical, &roots) {
            return Err(format!(
                "profile search directory must be within your home directory ({}): {dir}",
                home.display()
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn path_is_within_is_component_wise() {
        let roots = [PathBuf::from("/home/user")];
        assert!(path_is_within(Path::new("/home/user"), &roots));
        assert!(path_is_within(Path::new("/home/user/profiles"), &roots));
        // A sibling with a shared string prefix is NOT within — guards the
        // classic string-prefix bug (`/home/username` vs `/home/user`).
        assert!(!path_is_within(Path::new("/home/username"), &roots));
        assert!(!path_is_within(Path::new("/etc"), &roots));
    }

    #[test]
    fn root_is_exempt_from_confinement() {
        // uid 0 is allowed even with a nonexistent dir and a resolver that would
        // otherwise fail — the admin/CLI path is unrestricted.
        let out = confine_added_dirs(&["/nonexistent/anywhere".to_string()], Some(0), |_| None);
        assert!(out.is_ok(), "root must be exempt, got {out:?}");
    }

    #[test]
    fn non_root_dir_within_home_is_allowed() {
        let home = tempfile::tempdir().unwrap();
        let sub = home.path().join("profiles");
        std::fs::create_dir(&sub).unwrap();
        let home_path = home.path().to_path_buf();

        let out = confine_added_dirs(&[sub.to_string_lossy().into_owned()], Some(1000), |_| {
            Some(home_path.clone())
        });
        assert!(out.is_ok(), "in-home dir must be allowed, got {out:?}");
    }

    #[test]
    fn non_root_dir_outside_home_is_rejected() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let home_path = home.path().to_path_buf();

        let out = confine_added_dirs(
            &[outside.path().to_string_lossy().into_owned()],
            Some(1000),
            |_| Some(home_path.clone()),
        );
        let msg = out.expect_err("out-of-home dir must be rejected");
        assert!(
            msg.contains("within your home directory"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn non_root_rejects_when_any_dir_is_out_of_home() {
        // The loop must reject if ANY added dir is outside home, not only the
        // first — guards a short-circuit-on-first-success mutation of the loop.
        let home = tempfile::tempdir().unwrap();
        let inside = home.path().join("profiles");
        std::fs::create_dir(&inside).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let home_path = home.path().to_path_buf();

        let out = confine_added_dirs(
            &[
                inside.to_string_lossy().into_owned(),
                outside.path().to_string_lossy().into_owned(),
            ],
            Some(1000),
            |_| Some(home_path.clone()),
        );
        let msg = out.expect_err("a batch containing any out-of-home dir must be rejected");
        assert!(
            msg.contains("within your home directory"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn non_root_symlink_escape_is_rejected() {
        // A symlink inside home pointing outside must be rejected — canonicalize
        // resolves the link before the containment check.
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::create_dir(&secret).unwrap();
        let link = home.path().join("link");
        symlink(&secret, &link).unwrap();
        let home_path = home.path().to_path_buf();

        let out = confine_added_dirs(&[link.to_string_lossy().into_owned()], Some(1000), |_| {
            Some(home_path.clone())
        });
        let msg = out.expect_err("symlink escape must be rejected");
        assert!(
            msg.contains("within your home directory"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn non_root_nonexistent_dir_is_rejected() {
        // Confinement requires the dir to exist (canonicalize fails otherwise).
        let home = tempfile::tempdir().unwrap();
        let missing = home.path().join("does-not-exist");
        let home_path = home.path().to_path_buf();

        let out = confine_added_dirs(
            &[missing.to_string_lossy().into_owned()],
            Some(1000),
            |_| Some(home_path.clone()),
        );
        let msg = out.expect_err("nonexistent dir must be rejected");
        assert!(
            msg.contains("must exist and be readable"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn unresolved_home_fails_closed() {
        // A resolvable uid whose home cannot be found is rejected, not allowed.
        let existing = tempfile::tempdir().unwrap();
        let out = confine_added_dirs(
            &[existing.path().to_string_lossy().into_owned()],
            Some(1000),
            |_| None,
        );
        let msg = out.expect_err("unresolved home must fail closed");
        assert!(
            msg.contains("cannot resolve the home directory"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn missing_peer_uid_fails_closed() {
        // No SO_PEERCRED at all — treat as untrusted and reject.
        let existing = tempfile::tempdir().unwrap();
        let out = confine_added_dirs(
            &[existing.path().to_string_lossy().into_owned()],
            None,
            |_| Some(PathBuf::from("/home/whoever")),
        );
        let msg = out.expect_err("missing peer uid must fail closed");
        assert!(
            msg.contains("cannot identify the requesting user"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn home_dir_for_uid_resolves_current_user() {
        // Exercises the real getpwuid_r wrapper against the process's own uid,
        // which has a passwd entry on any real Linux host (including CI). Asserted
        // unconditionally so a broken resolver cannot pass this test vacuously.
        let uid = unsafe { libc::getuid() };
        let home = home_dir_for_uid(uid).expect("the current uid must resolve to a home directory");
        assert!(
            home.is_absolute(),
            "resolved home must be absolute: {}",
            home.display()
        );
    }
}
