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

/// Who is editing the search path, for confinement (DEC-205): root is
/// unconfined; anyone else is confined to their own home, by its raw and its
/// real spelling.
#[derive(Debug, Clone)]
pub(crate) enum Confinement {
    Unconfined,
    Home { home: PathBuf, roots: Vec<PathBuf> },
}

impl Confinement {
    /// Fail closed when the caller cannot be identified or has no home that
    /// confines anything. Filesystem access (the home's real path) for a
    /// non-root caller: run it off the async workers.
    pub(crate) fn of(
        peer_uid: Option<u32>,
        home_for_uid: impl Fn(u32) -> Option<PathBuf>,
    ) -> Result<Self, String> {
        let action = "edit";
        if peer_uid == Some(0) {
            return Ok(Self::Unconfined);
        }
        let Some(uid) = peer_uid else {
            return Err(format!(
                "cannot identify the requesting user (SO_PEERCRED unavailable); \
                 refusing to {action} profile search directories"
            ));
        };
        let Some(home) = home_for_uid(uid).and_then(confining_root) else {
            return Err(format!(
                "cannot resolve the home directory for uid {uid}; \
                 refusing to {action} profile search directories"
            ));
        };
        // A symlinked home (/home -> /var/home) is matched in both spellings.
        let mut roots = vec![home.clone()];
        if let Ok(real) = home.canonicalize() {
            if real != home {
                roots.push(real);
            }
        }
        Ok(Self::Home { home, roots })
    }

    /// Lexical: `path` must already be the value being decided on. Never
    /// resolve it again here — a second resolution can differ from the first
    /// (security review of FFA-c).
    pub(crate) fn permits(&self, path: &Path) -> bool {
        match self {
            Self::Unconfined => true,
            Self::Home { roots, .. } => path_is_within(path, roots),
        }
    }

    fn outside_home(&self, dir: &str) -> String {
        let home = match self {
            Self::Unconfined => Path::new("/"),
            Self::Home { home, .. } => home.as_path(),
        };
        format!(
            "profile search directory must be within your home directory ({}): {dir}",
            home.display()
        )
    }
}

/// Resolve and confine the profile search directories being added (DEC-205),
/// returning the form each is stored in (FFA-c).
///
/// `dirs` have already passed the absolute-path + no-`..` shape check.
/// `peer_uid` is the client's uid from `SO_PEERCRED` (`None` if it could not be
/// read); `home_for_uid` resolves a uid to its home (see [`home_dir_for_uid`]).
///
/// Each directory is resolved **once**, and that real path is both what is
/// confined and what is stored. Resolving it again for the check would let a
/// directory swapped in between be stored unconfined.
///
/// Rules:
/// - **root (uid 0) is exempt**, preserving the pre-DEC-205 admin/CLI
///   behaviour; a directory root names that does not exist is stored in its
///   lexical normal form.
/// - a **non-root** caller may only add directories that exist and whose real
///   path lies within its own home directory.
/// - if the uid or its home cannot be resolved, **fail closed**.
///
/// A refusal names the directory as the caller sent it, never its real path:
/// the daemon resolves as root, so the real path of a link inside a directory
/// the caller cannot read, or of `/proc/<pid>/cwd`, would otherwise leak
/// (DEC-173).
pub(crate) fn confine_adds(dirs: &[String], who: &Confinement) -> Result<Vec<String>, String> {
    dirs.iter()
        .map(|dir| match (resolve_client_path(Path::new(dir)), who) {
            (Ok(real), _) if who.permits(&real) => Ok(real.display().to_string()),
            (Ok(_), _) => Err(who.outside_home(dir)),
            (Err(_), Confinement::Unconfined) => {
                Ok(crate::profile_store::normalize_lexically(Path::new(dir))
                    .display()
                    .to_string())
            }
            (Err(_), Confinement::Home { .. }) => Err(format!(
                "profile search directory must exist and be readable: {dir}"
            )),
        })
        .collect()
}

/// Confine the profile search directories being removed (DEC-285), returning
/// every spelling a stored entry may match.
///
/// Deliberately NOT [`confine_added_dirs`]: a search-dir entry worth pruning is
/// very often one that no longer exists — a profiles folder the user moved, or
/// a stale entry an older GUI left behind — so removal must not require the
/// directory to exist.
///
/// A directory passes on its spelling as sent (lexically within the caller's
/// home) or on its real path, which `real_path_of` supplies when the caller
/// resolved it. The real path is needed because daemons ≤ 4.0.0 stored the raw
/// string of an addition they had confined by its real path, so an entry added
/// through a symlink (or under a `systemd-homed` layout where `pw_dir` and
/// `$HOME` spell the home differently) would otherwise be storable and never
/// removable. The real path is checked as given, never resolved again, and
/// added as a spelling only when it too is the caller's to remove: otherwise
/// `~/link -> <another user's dir>` would prune that user's entry.
///
/// No filesystem access except through `real_path_of` and the home lookup, so
/// a removal of a directory that stopped answering does not hang (FFA-b): the
/// caller resolves best-effort and passes `None` for what it could not.
///
/// Same rules otherwise: root (uid 0) is exempt, and an unresolvable uid or
/// home fails closed (in [`Confinement::of`]).
pub(crate) fn confine_removals(
    dirs: &[String],
    who: &Confinement,
    real_path_of: impl Fn(&str) -> Option<PathBuf>,
) -> Result<Vec<String>, String> {
    let mut spellings = Vec::with_capacity(dirs.len() * 2);
    for dir in dirs {
        let as_sent = who.permits(Path::new(dir));
        let real = real_path_of(dir).filter(|real| who.permits(real));
        if !as_sent && real.is_none() {
            return Err(who.outside_home(dir));
        }
        spellings.push(dir.clone());
        if let Some(real) = real {
            spellings.push(real.display().to_string());
        }
    }
    Ok(spellings)
}

/// The predicates the handler composes, as one call each — tests only. An empty
/// list confines nothing, so an unidentifiable caller is not refused for an
/// edit it did not make (the handler likewise skips a list that is empty).
#[cfg(test)]
fn confine_added_dirs(
    dirs: &[String],
    peer_uid: Option<u32>,
    home_for_uid: impl Fn(u32) -> Option<PathBuf>,
) -> Result<Vec<String>, String> {
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    confine_adds(dirs, &Confinement::of(peer_uid, home_for_uid)?)
}

/// See [`confine_added_dirs`]; resolves each removal here, where the handler
/// resolves best-effort.
#[cfg(test)]
fn confine_removed_dirs(
    dirs: &[String],
    peer_uid: Option<u32>,
    home_for_uid: impl Fn(u32) -> Option<PathBuf>,
) -> Result<Vec<String>, String> {
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    confine_removals(dirs, &Confinement::of(peer_uid, home_for_uid)?, |d| {
        Path::new(d).canonicalize().ok()
    })
}

/// Resolve a path a client named: its real path. Filesystem access — run it off
/// the async workers. A test can make it block for a chosen path.
pub(crate) fn resolve_client_path(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(test)]
    test_hook::on_resolve(path);
    path.canonicalize()
}

/// Test-only: block the resolution of a chosen path, standing in for a mount
/// that stopped answering. Keyed by path, so parallel tests do not interfere.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    type Hook = Arc<dyn Fn() + Send + Sync>;
    static HOOKS: parking_lot::Mutex<Option<HashMap<PathBuf, Hook>>> =
        parking_lot::Mutex::new(None);

    /// Run `hook` in every resolution of `path` until the guard drops.
    pub(crate) fn block_resolution_of(
        path: &Path,
        hook: impl Fn() + Send + Sync + 'static,
    ) -> Guard {
        HOOKS
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(path.to_path_buf(), Arc::new(hook));
        Guard(path.to_path_buf())
    }

    pub(crate) struct Guard(PathBuf);

    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(hooks) = HOOKS.lock().as_mut() {
                hooks.remove(&self.0);
            }
        }
    }

    pub(super) fn on_resolve(path: &Path) {
        let hook = HOOKS
            .lock()
            .as_ref()
            .and_then(|hooks| hooks.get(path).cloned());
        if let Some(hook) = hook {
            hook();
        }
    }
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
    fn a_refused_add_names_the_path_as_sent_never_its_real_path() {
        // Security review S1: the daemon resolves as root, so the real path of a
        // link inside a directory the caller cannot read (or of /proc/<pid>/cwd)
        // must not come back in the refusal.
        let home = tempfile::tempdir().unwrap();
        let hidden = tempfile::tempdir().unwrap();
        let target = hidden.path().join("only-root-may-know");
        std::fs::create_dir(&target).unwrap();
        let link = home.path().join("link");
        symlink(&target, &link).unwrap();
        let sent = link.to_string_lossy().into_owned();
        let home_path = home.path().to_path_buf();

        let msg = confine_added_dirs(std::slice::from_ref(&sent), Some(1000), |_| {
            Some(home_path.clone())
        })
        .expect_err("a link out of the home must be refused");
        assert!(msg.contains(&sent), "the caller's own spelling: {msg}");
        assert!(
            !msg.contains("only-root-may-know"),
            "the resolved target must not leak: {msg}"
        );
    }

    #[test]
    fn an_add_is_stored_as_the_real_path_it_was_confined_by() {
        let home = tempfile::tempdir().unwrap();
        let real = home.path().join("profiles");
        std::fs::create_dir(&real).unwrap();
        let link = home.path().join("via-link");
        symlink(&real, &link).unwrap();
        let home_path = home.path().to_path_buf();
        let stored = confine_added_dirs(&[link.to_string_lossy().into_owned()], Some(1000), |_| {
            Some(home_path.clone())
        })
        .unwrap();
        assert_eq!(
            stored,
            vec![real.canonicalize().unwrap().display().to_string()]
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
