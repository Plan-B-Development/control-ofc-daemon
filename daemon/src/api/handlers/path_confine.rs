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
        // Whether this home can actually confine anything is `confining_root`'s
        // question, not this function's — deliberately one copy of that rule and
        // not two. A resolver returning a useless root is harmless because both
        // predicates run it through `confining_root` and fail closed.
        return Some(PathBuf::from(s));
    }
}

/// Turn a resolved home into a usable confinement root, or `None`.
///
/// A home of `/` confines **nothing**: `path_is_within` is component-wise and
/// every absolute path starts with the root component, so `/` as a root accepts
/// anything. 26 accounts on a stock Arch install have `/` as their home
/// (`nobody`, `http`, `cups`, `dbus`, `polkitd`, `qemu`, `git`…) and the socket
/// is 0666 (DEC-049), so any of them can connect. `/nonexistent` is Debian's
/// spelling of the same idea.
///
/// This lives beside the decision, not only inside [`home_dir_for_uid`], because
/// the resolver is **injected** — a check only in the real resolver is invisible
/// to every caller that supplies its own, which is exactly how the unit tests
/// reach this code and how a future caller would reintroduce the hole.
fn confining_root(home: PathBuf) -> Option<PathBuf> {
    if home.parent().is_none() || home.as_path() == Path::new("/nonexistent") {
        log::warn!(
            "a client's home directory ({}) cannot confine anything; refusing to \
             use it as a confinement root",
            home.display()
        );
        return None;
    }
    Some(home)
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
    // Nothing to confine. Matters since the endpoint accepts `remove` alone
    // (DEC-285): without this, a remove-only request from a caller whose uid or
    // home cannot be resolved is refused with "refusing to ADD profile search
    // directories", which is simply not what was asked. Fails closed either way
    // — this is message truthfulness, and it matches the sibling predicate.
    if dirs.is_empty() || peer_uid == Some(0) {
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
    let Some(home) = home_for_uid(uid).and_then(confining_root) else {
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

/// Decide whether a non-root client may *remove* the given profile search
/// directories (DEC-285).
///
/// Deliberately NOT [`confine_added_dirs`]. That predicate canonicalizes, which
/// requires the directory to exist and be readable — and a search-dir entry
/// worth pruning is very often one that no longer exists: a profiles folder the
/// user moved, or a stale entry an older GUI left behind when it re-registered
/// a new path without retiring the old one. Reusing the add predicate would
/// make exactly the entries this operation exists to clean up the ones it
/// refuses to touch.
///
/// Containment is therefore **lexical**, which is the right match for what is
/// stored: the add path canonicalizes to *validate* but persists the raw string
/// the caller sent. The handler's shape pre-filter has already rejected
/// relative paths and any `..`, so a lexical `starts_with` cannot be walked out
/// of. Both the raw and canonicalized spellings of the caller's home are
/// accepted as roots, because a symlinked home (`/home` -> `/var/home`) can
/// legitimately have been stored either way.
///
/// Same rules otherwise: root (uid 0) is exempt, and an unresolvable uid or
/// home fails closed.
pub(crate) fn confine_removed_dirs(
    dirs: &[String],
    peer_uid: Option<u32>,
    home_for_uid: impl Fn(u32) -> Option<PathBuf>,
) -> Result<(), String> {
    if dirs.is_empty() || peer_uid == Some(0) {
        return Ok(());
    }

    let Some(uid) = peer_uid else {
        return Err(
            "cannot identify the requesting user (SO_PEERCRED unavailable); \
             refusing to remove profile search directories"
                .to_string(),
        );
    };
    let Some(home) = home_for_uid(uid).and_then(confining_root) else {
        return Err(format!(
            "cannot resolve the home directory for uid {uid}; \
             refusing to remove profile search directories"
        ));
    };
    let mut roots = vec![home.clone()];
    if let Ok(canonical) = home.canonicalize() {
        if canonical != home {
            roots.push(canonical);
        }
    }

    for dir in dirs {
        // Raw OR canonical. The stored entry is the RAW string the adder sent,
        // but `confine_added_dirs` validated its *canonical* form — so the set of
        // storable paths is a strict superset of the lexically-containable ones,
        // and the difference is exactly the entries a user most needs to prune.
        // Two real shapes hit it: a dir added through a symlink
        // (`/tmp/link -> ~/profiles`, stored as `/tmp/link`), and a
        // `systemd-homed`-style layout where `pw_dir` is `/var/home/alice` while
        // `$HOME` — and therefore the stored path — is `/home/alice`. Both were
        // addable and permanently unremovable. `canonicalize` is *attempted*,
        // never required: a vanished directory still passes on its raw form,
        // which is the property this predicate exists for.
        let raw_ok = path_is_within(Path::new(dir), &roots);
        let canonical_ok = !raw_ok
            && Path::new(dir)
                .canonicalize()
                .is_ok_and(|c| path_is_within(&c, &roots));
        if !raw_ok && !canonical_ok {
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

    // ── confine_removed_dirs ──────────────────────────────────────────
    // Removal is confined like addition, but must NOT inherit the add
    // predicate's existence requirement — see the doc comment.

    #[test]
    fn removal_of_a_nonexistent_in_home_dir_is_allowed() {
        // THE regression this predicate exists for. `confine_added_dirs`
        // canonicalizes, so it rejects a dir that is gone; a stale entry left
        // by an older GUI is precisely the thing a user needs to prune, and is
        // precisely the thing that no longer exists.
        let home = tempfile::tempdir().unwrap();
        let missing = home.path().join("moved-away/profiles");
        let home_path = home.path().to_path_buf();

        let out = confine_removed_dirs(
            &[missing.to_string_lossy().into_owned()],
            Some(1000),
            |_| Some(home_path.clone()),
        );
        assert!(
            out.is_ok(),
            "a vanished in-home dir must still be removable, got {out:?}"
        );
        // And the add predicate must still refuse it, or the two have merged
        // and this predicate has no reason to exist.
        assert!(
            confine_added_dirs(
                &[missing.to_string_lossy().into_owned()],
                Some(1000),
                |_| Some(home_path.clone())
            )
            .is_err(),
            "confine_added_dirs must still require existence"
        );
    }

    #[test]
    fn removal_outside_home_is_rejected() {
        // A local user must not be able to prune another user's (or the
        // admin's) search dir out from under them.
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_path_buf();

        let out = confine_removed_dirs(
            &["/srv/someone-elses/profiles".to_string()],
            Some(1000),
            |_| Some(home_path.clone()),
        );
        let msg = out.expect_err("out-of-home removal must be rejected");
        assert!(
            msg.contains("within your home directory"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn removal_rejects_when_any_dir_is_out_of_home() {
        // Reject if ANY entry is out of home, not only the first.
        let home = tempfile::tempdir().unwrap();
        let inside = home.path().join("profiles");
        let home_path = home.path().to_path_buf();

        let out = confine_removed_dirs(
            &[
                inside.to_string_lossy().into_owned(),
                "/srv/elsewhere".to_string(),
            ],
            Some(1000),
            |_| Some(home_path.clone()),
        );
        assert!(
            out.is_err(),
            "a batch containing an out-of-home dir must be rejected"
        );
    }

    #[test]
    fn removal_by_root_is_exempt() {
        let out = confine_removed_dirs(&["/anywhere/at/all".to_string()], Some(0), |_| None);
        assert!(out.is_ok(), "root must be exempt, got {out:?}");
    }

    #[test]
    fn removal_fails_closed_without_a_resolvable_identity() {
        for (uid, resolver, needle) in [
            (
                None,
                (|_| Some(PathBuf::from("/home/whoever"))) as fn(u32) -> Option<PathBuf>,
                "cannot identify the requesting user",
            ),
            (
                Some(1000),
                (|_| None) as fn(u32) -> Option<PathBuf>,
                "cannot resolve the home directory",
            ),
        ] {
            let out = confine_removed_dirs(&["/home/whoever/x".to_string()], uid, resolver);
            let msg = out.expect_err("an unidentifiable caller must fail closed");
            assert!(msg.contains(needle), "unexpected message: {msg}");
        }
    }

    #[test]
    fn removal_of_an_empty_list_is_a_no_op_even_when_unidentifiable() {
        // Nothing to confine — an add-only request must not be rejected by the
        // removal predicate just because the caller's home cannot be resolved.
        let out = confine_removed_dirs(&[], None, |_| None);
        assert!(out.is_ok(), "an empty removal list must be permitted");
    }

    #[test]
    fn removal_accepts_the_uncanonicalized_home_spelling() {
        // A symlinked home (/home -> /var/home) can have been *stored* under
        // either spelling, because the add path persists the raw string. Both
        // must be removable.
        let real = tempfile::tempdir().unwrap();
        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("home-link");
        symlink(real.path(), &link).unwrap();

        let stored = link.join("profiles").to_string_lossy().into_owned();
        // Resolver returns the symlinked spelling; the stored path uses it too.
        let link_path = link.clone();
        assert!(
            confine_removed_dirs(std::slice::from_ref(&stored), Some(1000), |_| Some(
                link_path.clone()
            ))
            .is_ok(),
            "raw-spelling home must accept a raw-spelling entry"
        );
        // Stored under the canonical spelling, resolver still returns the link.
        let canonical_stored = real.path().join("profiles").to_string_lossy().into_owned();
        let link_path = link.clone();
        assert!(
            confine_removed_dirs(&[canonical_stored], Some(1000), |_| Some(link_path.clone()))
                .is_ok(),
            "a canonical-spelling entry must also be removable"
        );
    }

    #[test]
    fn a_root_home_confines_nothing_and_is_refused() {
        // REGRESSION (security review F1). `path_is_within` is component-wise and
        // EVERY absolute path starts with the root component, so a home of `/`
        // made both predicates accept anything. 26 accounts on a stock Arch
        // install have `/` as their home and the socket is 0666.
        assert!(
            path_is_within(Path::new("/etc/anything"), &[PathBuf::from("/")]),
            "precondition: a `/` root really does match every absolute path"
        );
        for home in ["/", "/nonexistent"] {
            let root = PathBuf::from(home);
            for out in [
                confine_added_dirs(&["/etc/passwd-dir".to_string()], Some(1000), |_| {
                    Some(root.clone())
                }),
                confine_removed_dirs(&["/home/someone-else/p".to_string()], Some(1000), |_| {
                    Some(root.clone())
                }),
            ] {
                let msg = out.expect_err("a non-confining home must fail closed");
                assert!(
                    msg.contains("cannot resolve the home directory"),
                    "unexpected message for home={home}: {msg}"
                );
            }
        }
    }

    #[test]
    fn removal_accepts_a_candidate_that_only_canonicalizes_into_home() {
        // REGRESSION (security review F2). The add path validates the CANONICAL
        // form but persists the RAW string, so an entry added through a symlink
        // is stored under a path that is not lexically inside home — and was
        // therefore permanently unremovable by the only user allowed to remove
        // it, which is precisely the stale-entry case this predicate exists for.
        let home = tempfile::tempdir().unwrap();
        let inside = home.path().join("profiles");
        std::fs::create_dir(&inside).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = elsewhere.path().join("link-to-profiles");
        symlink(&inside, &link).unwrap();
        let home_path = home.path().to_path_buf();
        let stored = link.to_string_lossy().into_owned();

        // Precondition: it really is storable — the add predicate accepts it.
        assert!(
            confine_added_dirs(std::slice::from_ref(&stored), Some(1000), |_| Some(
                home_path.clone()
            ))
            .is_ok(),
            "precondition: a symlinked in-home dir is addable, hence storable"
        );
        // Precondition: and it is NOT lexically inside home, so a raw-only check
        // would refuse it — this test would pass vacuously without that.
        assert!(
            !path_is_within(Path::new(&stored), std::slice::from_ref(&home_path)),
            "precondition: the stored raw path is outside home lexically"
        );

        assert!(
            confine_removed_dirs(std::slice::from_ref(&stored), Some(1000), |_| Some(
                home_path.clone()
            ))
            .is_ok(),
            "a storable entry must be removable by the user who stored it"
        );
    }

    #[test]
    fn a_canonicalizing_candidate_outside_home_is_still_rejected() {
        // The canonical fallback must not become a bypass: a symlink pointing
        // OUT of home resolves outside and stays refused.
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::create_dir(&secret).unwrap();
        let link = home.path().join("escape");
        symlink(&secret, &link).unwrap();
        let home_path = home.path().to_path_buf();

        // The raw path IS inside home, so this one is accepted on the raw leg —
        // which is correct: a removal only bites when it exactly string-matches
        // a stored entry, and that entry could only have been stored by this
        // user. What must NOT happen is the reverse.
        let outside_raw = secret.to_string_lossy().into_owned();
        assert!(
            confine_removed_dirs(std::slice::from_ref(&outside_raw), Some(1000), |_| Some(
                home_path.clone()
            ))
            .is_err(),
            "a path outside home on BOTH legs must stay refused"
        );
        let _ = link;
    }

    #[test]
    fn an_empty_add_list_is_a_no_op_even_when_unidentifiable() {
        // REGRESSION (security review F6). The endpoint accepts `remove` alone
        // now, so a remove-only request must not be refused with a message about
        // ADDING directories. Fails closed either way; this is truthfulness.
        assert!(
            confine_added_dirs(&[], None, |_| None).is_ok(),
            "an empty add list must not be refused"
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
