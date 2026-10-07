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
//! (forward compatibility). The document is stored compact and refused past the
//! read cap, so everything stored can be read back (FFA-j).
//!
//! **Reading is confined to the search directories (FFA-a, FFA-b).** Any local
//! user can register a directory in their own home (DEC-205), and `GET
//! /profiles/{id}` returns the file it finds there. So a read through
//! `SearchDir` refuses three things a plain `File::open` follows:
//! - a profile that is a symlink, a FIFO or anything else but a regular file
//!   (opened `O_NOFOLLOW | O_NONBLOCK`, then `fstat`): a link could name any file
//!   root can read, and a FIFO blocked the reading thread until a writer came;
//! - a profile whose owner is not the directory's owner: a hard link to another
//!   user's file;
//! - a directory whose real path is not the path registered. A user could
//!   otherwise swap their registered directory for a symlink to `/root` after
//!   it was confined. Since FFA-c new entries are stored as their real path.
//!   An older entry that goes through a symlink is skipped with a warning until
//!   it is registered again.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::api::responses::ProfileSummary;
use crate::atomic_io::{create_dir_private, write_atomic, MAX_CONFIG_BYTES};
use crate::constants::{
    MAX_PROFILES_LISTED_PER_DIR, MAX_STORED_PROFILES, MAX_STORE_BYTES, PROFILE_DIR_HUNG_AFTER,
    PROFILE_IO_BUDGET,
};
use crate::io_gate::{Claim, Gate, Refusal};
use crate::profile::{is_safe_profile_id, DaemonProfile};
use std::time::Instant;

/// Serialises every check-then-act on the store (FFA-j): create's "does it
/// exist" and "is it full" with its write, delete's "is it active" with its
/// unlink, activation's "is the store file still there" with its swap and
/// persist, and deactivation's swap with its persist. Without it two creates of
/// one id both answered `201`, a delete could land between an activation's read
/// and its swap (leaving a saved active profile with no file behind it), and an
/// activation's persist could land after a later deactivation's. Holders take
/// it through `api::handlers::under_store_lock`, in a task of their own, and
/// never across a read of a user's search directory. A tokio mutex, because
/// holders await their blocking writes under it.
pub static STORE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Path of a stored profile within `dir`. `None` if the id is unsafe.
fn profile_path(dir: &Path, id: &str) -> Option<PathBuf> {
    is_safe_profile_id(id).then(|| dir.join(format!("{id}.json")))
}

/// `path` with `.` components, repeated separators and a trailing separator
/// removed. Never touches the filesystem. `..` is kept; the search-dir editor
/// refuses it before anything is compared.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    path.components().collect()
}

/// A profile search directory opened for reading and confirmed to be the
/// directory its entry names (see the module docs).
struct SearchDir {
    /// Held open so `handle` keeps naming this directory.
    _dir: File,
    /// `/proc/self/fd/N`: the opened directory, whatever its path names later.
    handle: PathBuf,
    owner: u32,
}

impl SearchDir {
    fn open(path: &Path) -> io::Result<Self> {
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK)
            .open(path)?;
        let handle = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        // The real path of the directory actually opened. Comparing it with the
        // entry after the open, rather than resolving the entry first, leaves no
        // window in which the entry can be swapped.
        // A failure here is not "the directory is absent": keep it out of
        // `NotFound`, which `open_search_dir` does not log.
        let real = std::fs::read_link(&handle).map_err(|e| {
            io::Error::other(format!("its real path could not be read from /proc: {e}"))
        })?;
        if real != normalize_lexically(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "it resolves to {}; a search directory must be registered by its real \
                     path",
                    real.display()
                ),
            ));
        }
        let owner = dir.metadata()?.uid();
        Ok(Self {
            _dir: dir,
            handle,
            owner,
        })
    }

    /// Read the profile file `name`, a single path component, from this
    /// directory.
    fn read(&self, name: &str) -> io::Result<String> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a file name",
            ));
        }
        let file = crate::atomic_io::open_regular_nonblocking(&self.handle.join(name), true)?;
        let file_owner = file.metadata()?.uid();
        if file_owner != self.owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "owned by uid {file_owner}, but its directory by uid {}",
                    self.owner
                ),
            ));
        }
        crate::atomic_io::read_open_file_with_cap(file, MAX_CONFIG_BYTES, Path::new(name))
    }

    /// The `*.json` names in this directory, sorted so a listing is
    /// deterministic.
    /// The first `cap` `*.json` names in sorted order, and how many there
    /// are. Memory stays bounded by `cap` however many the directory holds
    /// (FFA-c); an enumeration still running at `deadline` stops with what it
    /// has.
    fn json_file_names(&self, cap: usize, deadline: Instant) -> io::Result<FileNames> {
        let mut smallest = std::collections::BinaryHeap::with_capacity(cap + 1);
        let mut total = 0usize;
        let mut complete = true;
        for entry in std::fs::read_dir(&self.handle)? {
            if Instant::now() >= deadline {
                complete = false;
                break;
            }
            let Some(name) = entry.ok().and_then(|e| e.file_name().into_string().ok()) else {
                continue;
            };
            if Path::new(&name).extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            total += 1;
            smallest.push(name);
            if smallest.len() > cap {
                smallest.pop();
            }
        }
        Ok(FileNames {
            names: smallest.into_sorted_vec(),
            total,
            complete,
        })
    }
}

/// What [`SearchDir::json_file_names`] found.
struct FileNames {
    /// The first names in sorted order, at most the cap.
    names: Vec<String>,
    /// How many `*.json` names it saw.
    total: usize,
    /// Whether it read the whole directory before the deadline.
    complete: bool,
}

/// One operation at a time per search directory (FFA-b, see `io_gate`): a read
/// that hangs on a directory that stopped answering keeps that directory, and
/// later readers skip it instead of parking a thread each behind it.
static DIR_GATE: Gate<PathBuf> = Gate::new();

/// A search directory opened under its claim; dropping it releases the
/// directory once the operation is over.
struct ClaimedDir {
    opened: SearchDir,
    _claim: Claim<'static, PathBuf>,
}

/// A walk of the search directories ran out of time before it was done.
#[derive(Debug, PartialEq, Eq)]
pub struct OutOfTime;

/// The deadline a read given no deadline of its own works to.
fn default_deadline() -> Instant {
    Instant::now() + PROFILE_IO_BUDGET
}

/// What became of claiming and opening one search directory.
enum Opened {
    Dir(ClaimedDir),
    /// Absent or refused (logged): go on without it.
    Skipped,
    /// A user's directory that stopped answering (logged): go on without it.
    Hung,
    /// Time ran out. `strict` when it ran out waiting on a directory an
    /// ordered lookup may not go past.
    OutOfTime {
        strict: bool,
    },
}

impl ClaimedDir {
    /// Whether an ordered lookup may go past this directory when it is busy.
    /// Root's directories — the store, the system presets — may not: they come
    /// first and shadow the users' (security review of FFA-b). A user's own
    /// directory may: its place in the order is the user's choice anyway.
    fn strict(&self, dir: &Path) -> bool {
        #[cfg(test)]
        if test_hook::is_root_owned(dir) {
            return true;
        }
        let _ = dir;
        self.opened.owner == 0
    }
}

/// Claim and open a search directory. A configured directory that does not
/// exist is ordinary (the defaults name one in root's home that is usually
/// absent), so that case is not logged.
fn open_search_dir(dir: &Path, deadline: Instant) -> Opened {
    if Instant::now() >= deadline {
        return Opened::OutOfTime { strict: false };
    }
    let claim = match DIR_GATE.claim(normalize_lexically(dir), PROFILE_DIR_HUNG_AFTER, deadline) {
        Ok(claim) => claim,
        Err(Refusal::Busy { strict }) => return Opened::OutOfTime { strict },
        Err(Refusal::Hung) => {
            log::warn!(
                "profile search directory {} skipped: an earlier read of it has not returned",
                dir.display()
            );
            return Opened::Hung;
        }
    };
    let claimed = match SearchDir::open(dir) {
        Ok(opened) => ClaimedDir {
            opened,
            _claim: claim,
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Opened::Skipped,
        Err(e) => {
            log::warn!("profile search directory {} skipped: {e}", dir.display());
            return Opened::Skipped;
        }
    };
    if claimed.strict(dir) {
        claimed._claim.make_strict();
    }
    #[cfg(test)]
    test_hook::on_open(dir);
    Opened::Dir(claimed)
}

/// Whether a read of `dir` holds its claim now. Tests only.
#[cfg(test)]
pub(crate) fn dir_in_use(dir: &Path) -> bool {
    DIR_GATE.is_held(&normalize_lexically(dir))
}

/// Test-only: block an open of a chosen directory, standing in for a mount
/// that stopped answering. Keyed by directory, so tests running in parallel
/// do not see each other's hooks.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    type Hook = Arc<dyn Fn() + Send + Sync>;
    static HOOKS: parking_lot::Mutex<Option<HashMap<PathBuf, Hook>>> =
        parking_lot::Mutex::new(None);
    static ROOT_OWNED: parking_lot::Mutex<Vec<PathBuf>> = parking_lot::Mutex::new(Vec::new());

    /// Treat `dir` as root's (strict, never skipped) for the life of the
    /// guard: tests cannot make a root-owned directory.
    pub(crate) fn treat_as_root_owned(dir: &Path) -> RootOwned {
        let key = super::normalize_lexically(dir);
        ROOT_OWNED.lock().push(key.clone());
        RootOwned(key)
    }

    pub(crate) struct RootOwned(PathBuf);

    impl Drop for RootOwned {
        fn drop(&mut self) {
            ROOT_OWNED.lock().retain(|d| d != &self.0);
        }
    }

    pub(super) fn is_root_owned(dir: &Path) -> bool {
        ROOT_OWNED.lock().contains(&super::normalize_lexically(dir))
    }

    /// Run `hook` in every open of `dir`, after it is opened and claimed, until
    /// the returned guard drops.
    pub(crate) fn block_opens_of(dir: &Path, hook: impl Fn() + Send + Sync + 'static) -> Guard {
        let key = super::normalize_lexically(dir);
        HOOKS
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(key.clone(), Arc::new(hook));
        Guard(key)
    }

    pub(crate) struct Guard(PathBuf);

    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(hooks) = HOOKS.lock().as_mut() {
                hooks.remove(&self.0);
            }
        }
    }

    pub(super) fn on_open(dir: &Path) {
        let hook = HOOKS
            .lock()
            .as_ref()
            .and_then(|hooks| hooks.get(&super::normalize_lexically(dir)).cloned());
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Read `name` from an opened search directory: `Ok(None)` when it is absent,
/// `Err` with the reason when it was refused or unreadable.
fn read_entry(opened: &SearchDir, name: &str) -> Result<Option<String>, io::Error> {
    match opened.read(name) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// [`read_entry`], with a refusal logged.
fn read_logged(opened: &SearchDir, dir: &Path, name: &str) -> Option<String> {
    read_entry(opened, name).unwrap_or_else(|e| {
        log::warn!("profile {name} in {} skipped: {e}", dir.display());
        None
    })
}

/// Persist a profile document to `{store_dir}/{id}.json`.
///
/// `bytes` is the validated profile document (the caller serializes the
/// uploaded `serde_json::Value`, not a re-serialized model). The id must be
/// filename-safe and must equal the document's `id` field (the caller checks).
pub fn save_raw(store_dir: &Path, id: &str, bytes: &[u8]) -> Result<(), String> {
    let path = profile_path(store_dir, id).ok_or_else(|| format!("unsafe profile id: {id:?}"))?;
    create_dir_private(store_dir)?;
    write_atomic(&path, bytes)
}

/// Why [`store`] refused or failed.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The id is already stored and the caller asked not to replace it.
    AlreadyExists,
    /// The id is new and the store already holds [`MAX_STORED_PROFILES`].
    Full,
    /// The store would hold more than [`MAX_STORE_BYTES`] in all.
    TooLarge,
    /// The write failed; the detail is for the log.
    Io(String),
}

/// Create or replace `{store_dir}/{id}.json` (FFA-j). `overwrite: false` refuses
/// an id already stored; a new id is refused once the store is full. Call it
/// holding [`STORE_LOCK`]: only then are the checks and the write one step.
pub fn store(store_dir: &Path, id: &str, bytes: &[u8], overwrite: bool) -> Result<(), StoreError> {
    let exists = exists_in_store(store_dir, id);
    if exists && !overwrite {
        return Err(StoreError::AlreadyExists);
    }
    let (count, other_bytes) = store_usage(store_dir, id);
    if !exists && count >= MAX_STORED_PROFILES {
        return Err(StoreError::Full);
    }
    if other_bytes.saturating_add(bytes.len() as u64) > MAX_STORE_BYTES {
        return Err(StoreError::TooLarge);
    }
    save_raw(store_dir, id, bytes).map_err(StoreError::Io)
}

/// How many profiles the store holds, and their bytes leaving out `id`'s (the
/// file a replace overwrites). An unreadable store counts as empty: the write
/// that follows reports the real problem.
fn store_usage(store_dir: &Path, id: &str) -> (usize, u64) {
    let replaced = format!("{id}.json");
    let Ok(entries) = std::fs::read_dir(store_dir) else {
        return (0, 0);
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .fold((0, 0), |(count, bytes), e| {
            let size = if e.file_name().to_str() == Some(replaced.as_str()) {
                0
            } else {
                e.metadata().map(|m| m.len()).unwrap_or(0)
            };
            (count + 1, bytes + size)
        })
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
/// search dirs cannot be deleted. The directory is fsynced after the unlink, so
/// a deleted profile does not come back after a power loss.
pub fn delete(store_dir: &Path, id: &str) -> Result<bool, String> {
    let Some(path) = profile_path(store_dir, id) else {
        return Err(format!("unsafe profile id: {id:?}"));
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {
            if let Err(e) = crate::atomic_io::fsync_dir(store_dir) {
                log::warn!(
                    "profile store: fsync of '{}' after a delete failed (the delete may \
                     not survive a power loss): {e}",
                    store_dir.display()
                );
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("delete profile '{}': {e}", path.display())),
    }
}

/// Read `{id}.json` from the first search directory holding a readable copy,
/// with the real path it was read from. Directories are tried in order (store
/// first), so a stored profile shadows a same-id preset; a copy that is refused
/// is skipped as if absent.
pub fn read_by_id(search_dirs: &[PathBuf], id: &str) -> Option<(PathBuf, String)> {
    read_by_id_until(search_dirs, id, default_deadline())
        .ok()
        .flatten()
}

/// [`read_by_id`], stopping at `deadline`: `Err` when it passed before the id
/// was found in a directory or every directory was looked in. A user's
/// directory that stopped answering is gone past; root's directories never are
/// (see [`ClaimedDir::strict`]), so a busy store cannot hand an id to a user's
/// same-named file.
pub fn read_by_id_until(
    search_dirs: &[PathBuf],
    id: &str,
    deadline: Instant,
) -> Result<Option<(PathBuf, String)>, OutOfTime> {
    if !is_safe_profile_id(id) {
        return Ok(None);
    }
    let name = format!("{id}.json");
    for dir in search_dirs {
        let claimed = match open_search_dir(dir, deadline) {
            Opened::Dir(claimed) => claimed,
            Opened::Skipped | Opened::Hung => continue,
            Opened::OutOfTime { .. } => return Err(OutOfTime),
        };
        if let Some(content) = read_logged(&claimed.opened, dir, &name) {
            return Ok(Some((normalize_lexically(dir).join(&name), content)));
        }
    }
    Ok(None)
}

/// Fetch a stored/preset profile as a lossless JSON value (preserves any fields
/// the daemon model doesn't know). Resolves as [`read_by_id`].
pub fn get_raw(search_dirs: &[PathBuf], id: &str) -> Option<serde_json::Value> {
    get_raw_until(search_dirs, id, default_deadline())
        .ok()
        .flatten()
}

/// [`get_raw`], stopping at `deadline` as [`read_by_id_until`] does.
pub fn get_raw_until(
    search_dirs: &[PathBuf],
    id: &str,
    deadline: Instant,
) -> Result<Option<serde_json::Value>, OutOfTime> {
    Ok(read_by_id_until(search_dirs, id, deadline)?
        .and_then(|(_, content)| serde_json::from_str(&content).ok()))
}

/// Why a `profile_path` could not be read for activation.
#[derive(Debug, PartialEq, Eq)]
pub enum PathReadError {
    /// The file, or the directory holding it, does not exist.
    NotFound,
    /// The path does not name a file directly inside a search directory.
    OutsideSearchDirs,
    /// The file exists but was refused or could not be read; the detail is for
    /// the log.
    Unreadable(String),
    /// Its directory could not be read before the deadline.
    OutOfTime,
}

/// The search directory and file name `requested` names for activation by
/// path, without touching the filesystem.
///
/// Its directory must be a registered entry: as written (lexically), or as
/// `real_parent`, the directory's real path when the caller resolved it. Only
/// the directory is ever resolved: the file must be named by the request, not
/// reached through a link.
pub fn locate_by_path<'a>(
    search_dirs: &'a [PathBuf],
    requested: &Path,
    real_parent: Option<&Path>,
) -> Result<(&'a Path, String), PathReadError> {
    let name = requested.file_name().and_then(|n| n.to_str());
    let (Some(parent), Some(name), true) = (requested.parent(), name, requested.is_absolute())
    else {
        return Err(PathReadError::OutsideSearchDirs);
    };
    let parent = real_parent.map_or_else(|| normalize_lexically(parent), Path::to_path_buf);
    search_dirs
        .iter()
        .find(|d| normalize_lexically(d) == parent)
        .map(|d| (d.as_path(), name.to_string()))
        .ok_or(PathReadError::OutsideSearchDirs)
}

/// Read `name` from the search directory `dir` for activation, with the real
/// path it was read from. The read goes through `SearchDir`, as any profile
/// read does.
pub fn read_in_dir(
    dir: &Path,
    name: &str,
    deadline: Instant,
) -> Result<(PathBuf, String), PathReadError> {
    let claimed = match open_search_dir(dir, deadline) {
        Opened::Dir(claimed) => claimed,
        Opened::Skipped => {
            return Err(PathReadError::Unreadable(format!(
                "search directory {} could not be opened",
                dir.display()
            )))
        }
        // The file may well be there: retryable, not refused.
        Opened::Hung | Opened::OutOfTime { .. } => return Err(PathReadError::OutOfTime),
    };
    let path = normalize_lexically(dir).join(name);
    match read_entry(&claimed.opened, name) {
        Ok(Some(content)) => Ok((path, content)),
        Ok(None) => Err(PathReadError::NotFound),
        Err(e) => Err(PathReadError::Unreadable(format!(
            "{}: {e}",
            path.display()
        ))),
    }
}

/// [`locate_by_path`] then [`read_in_dir`], resolving the directory here when
/// it is not a registered entry as written. Blocking and unbounded: for the
/// boot path, which runs it on a thread of its own, and for tests.
pub fn read_by_path(
    search_dirs: &[PathBuf],
    requested: &Path,
) -> Result<(PathBuf, String), PathReadError> {
    let located = match locate_by_path(search_dirs, requested, None) {
        Ok(located) => located,
        Err(PathReadError::OutsideSearchDirs) => {
            let Some(parent) = requested.parent().filter(|_| requested.is_absolute()) else {
                return Err(PathReadError::OutsideSearchDirs);
            };
            let real = parent.canonicalize().map_err(|_| PathReadError::NotFound)?;
            locate_by_path(search_dirs, requested, Some(&real))?
        }
        Err(e) => return Err(e),
    };
    read_in_dir(located.0, &located.1, default_deadline())
}

/// List profiles across `search_dirs` (store ∪ presets ∪ …), deduped by id with
/// earlier dirs winning (the store, being first, shadows same-id presets).
/// Refused, unreadable or unparseable files are skipped.
/// Empty when [`list_until`] would fail.
pub fn list(search_dirs: &[PathBuf]) -> Vec<ProfileSummary> {
    list_until(search_dirs, default_deadline()).unwrap_or_default()
}

/// [`list`], stopping at `deadline` (FFA-b): a directory that is slow without
/// having stopped answering must not hold every listing past its budget, nor
/// keep an abandoned walk reading behind it.
///
/// Time running out in a user's directory answers what was found so far
/// (logged): a listing short of entries, never one with an entry in the wrong
/// place, because nothing after the stop is read. Time running out in root's
/// directory — the store or the presets — is `Err`: a listing without them
/// would show a profile as unpublished that is not.
pub fn list_until(
    search_dirs: &[PathBuf],
    deadline: Instant,
) -> Result<Vec<ProfileSummary>, OutOfTime> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<ProfileSummary> = Vec::new();
    let stop = |dir: &Path, strict: bool| {
        if strict {
            return Err(OutOfTime);
        }
        log::warn!(
            "profile listing ran out of time at {}; later search directories not read",
            dir.display()
        );
        Ok(())
    };
    for dir in search_dirs {
        let claimed = match open_search_dir(dir, deadline) {
            Opened::Dir(claimed) => claimed,
            Opened::Skipped | Opened::Hung => continue,
            Opened::OutOfTime { strict } => {
                stop(dir, strict)?;
                break;
            }
        };
        let strict = claimed.strict(dir);
        let Ok(FileNames {
            names,
            total,
            complete,
        }) = claimed
            .opened
            .json_file_names(MAX_PROFILES_LISTED_PER_DIR, deadline)
        else {
            continue;
        };
        if !complete {
            stop(dir, strict)?;
        }
        // FFA-c: bounded per directory, and one log line per directory however
        // many files it holds.
        if total > MAX_PROFILES_LISTED_PER_DIR {
            log::warn!(
                "profile search directory {} holds {total} profile files; listing the first {}",
                dir.display(),
                MAX_PROFILES_LISTED_PER_DIR
            );
        }
        let mut refused = 0usize;
        let mut first_refusal = None;
        let mut out_of_time = !complete;
        for name in names {
            if Instant::now() >= deadline {
                stop(dir, strict)?;
                out_of_time = true;
                break;
            }
            let content = match read_entry(&claimed.opened, &name) {
                Ok(Some(content)) => content,
                Ok(None) => continue,
                Err(e) => {
                    refused += 1;
                    first_refusal.get_or_insert_with(|| format!("{name}: {e}"));
                    continue;
                }
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
        if let Some(first) = first_refusal {
            log::warn!(
                "profile search directory {}: {refused} profile file(s) skipped, first {first}",
                dir.display()
            );
        }
        if out_of_time {
            break;
        }
    }
    Ok(out)
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

    #[cfg(unix)]
    #[test]
    fn save_raw_creates_private_store_dir() {
        // DEC-173: the store dir is created 0o700 (owner-only), matching the
        // 0o600 profile files written into it — an other-readable store dir would
        // leak the set of profile ids (filenames) to local users.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("profiles"); // does not exist yet
        save_raw(&store, "p", &sample("p", "P")).unwrap();
        assert!(store.join("p.json").exists());
        let mode = std::fs::metadata(&store).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
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

    // ── FFA-a / FFA-b: what a search-dir read refuses ──────────────────

    fn mkfifo(path: &Path) {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
    }

    /// Run `f` on its own thread and fail if it has not returned within 5 s.
    /// Before FFA-b a FIFO blocked the open forever, so the wedge releases itself
    /// at the deadline: a writer opens every FIFO named, the blocked reader
    /// returns, and the test fails rather than hangs.
    fn within_deadline<T: Send + 'static>(
        fifos: &[PathBuf],
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(value) => value,
            Err(_) => {
                for fifo in fifos {
                    let _ = std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(fifo);
                }
                panic!("the read blocked on a FIFO");
            }
        }
    }

    #[test]
    fn a_symlinked_profile_is_neither_served_nor_listed() {
        // F-1: `x.json -> <any root-readable JSON>` was returned verbatim by
        // `GET /profiles/x`. The target here is a valid profile, so only the
        // link refusal can keep it out.
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("secret.json");
        std::fs::write(&target, sample("x", "Secret")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("x.json")).unwrap();
        // Positive control: a regular file beside the link is served.
        std::fs::write(dir.path().join("y.json"), sample("y", "Mine")).unwrap();
        let dirs = [dir.path().to_path_buf()];

        assert!(
            get_raw(&dirs, "x").is_none(),
            "the link must not be followed"
        );
        assert_eq!(get_raw(&dirs, "y").unwrap()["name"], "Mine");
        let ids: Vec<String> = list(&dirs).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["y".to_string()]);
    }

    #[test]
    fn a_fifo_profile_is_refused_without_blocking() {
        // F-2: a FIFO named like a profile blocked the reading thread until a
        // writer came, a tokio worker each time `GET /profiles` was called.
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("a.json");
        mkfifo(&fifo);
        std::fs::write(dir.path().join("good.json"), sample("good", "Good")).unwrap();
        let dirs = vec![dir.path().to_path_buf()];

        let listed = {
            let dirs = dirs.clone();
            within_deadline(std::slice::from_ref(&fifo), move || list(&dirs))
        };
        let ids: Vec<String> = listed.into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["good".to_string()]);
        let got = {
            let dirs = dirs.clone();
            within_deadline(std::slice::from_ref(&fifo), move || get_raw(&dirs, "a"))
        };
        assert!(got.is_none());
        let by_path = within_deadline(std::slice::from_ref(&fifo), {
            let fifo = fifo.clone();
            move || read_by_path(&dirs, &fifo)
        });
        assert!(
            matches!(by_path, Err(PathReadError::Unreadable(_))),
            "{by_path:?}"
        );
    }

    #[test]
    fn a_search_dir_swapped_for_a_symlink_is_refused() {
        // The DEC-205 residual: a directory confined at registration, later
        // replaced by a symlink to somewhere its owner could not register.
        let tmp = tempfile::tempdir().unwrap();
        let registered = tmp.path().join("p");
        std::fs::create_dir(&registered).unwrap();
        std::fs::write(registered.join("x.json"), sample("x", "Mine")).unwrap();
        let dirs = [registered.clone()];
        // Precondition: before the swap the entry is served, so the refusal
        // below is the swap's doing and not the fixture's.
        assert_eq!(get_raw(&dirs, "x").unwrap()["name"], "Mine");

        let elsewhere = tmp.path().join("not-registered");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("x.json"), sample("x", "Secret")).unwrap();
        std::fs::rename(&registered, tmp.path().join("p.old")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &registered).unwrap();

        assert!(
            get_raw(&dirs, "x").is_none(),
            "the swapped dir must be refused"
        );
        assert!(list(&dirs).is_empty());
        // Named as registered, it matches the entry as written; the anchor then
        // refuses the swapped directory behind it.
        let by_path = read_by_path(&dirs, &registered.join("x.json"));
        assert!(
            matches!(by_path, Err(PathReadError::Unreadable(_))),
            "{by_path:?}"
        );
    }

    #[test]
    fn a_respelled_search_dir_entry_is_still_read() {
        // Entries stored before FFA-c are raw strings; a spelling that differs
        // only lexically from the real path is the same directory.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("x.json"), sample("x", "Mine")).unwrap();
        let raw = tmp.path().display().to_string();
        for spelling in [
            format!("{raw}/"),
            format!("{raw}/."),
            raw.replacen('/', "//", 1),
        ] {
            let dirs = [PathBuf::from(&spelling)];
            assert_eq!(
                get_raw(&dirs, "x").map(|v| v["name"].clone()),
                Some(serde_json::json!("Mine")),
                "{spelling}"
            );
        }
    }

    #[test]
    fn a_profile_not_owned_by_its_directory_owner_is_refused() {
        // A hard link to another user's file has that user as its owner. Tests
        // cannot chown, so use a directory owned by someone else that we can
        // write to: the sticky, root-owned /tmp.
        let tmp_dir = Path::new("/tmp");
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        let Ok(tmp_meta) = std::fs::metadata(tmp_dir) else {
            return;
        };
        if tmp_meta.uid() == me || tmp_dir.canonicalize().ok().as_deref() != Some(tmp_dir) {
            eprintln!("skipped: needs a /tmp that is its real path and owned by another uid");
            return;
        }
        let file = tempfile::Builder::new()
            .prefix("ffa-owner-")
            .suffix(".json")
            .tempfile_in(tmp_dir)
            .unwrap();
        let id = file
            .path()
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        std::fs::write(file.path(), sample(&id, "Planted")).unwrap();
        assert!(get_raw(&[tmp_dir.to_path_buf()], &id).is_none());
        // Positive control: the same document in a directory we own is served.
        let mine = tempfile::tempdir().unwrap();
        std::fs::write(
            mine.path().join(format!("{id}.json")),
            sample(&id, "Planted"),
        )
        .unwrap();
        assert!(get_raw(&[mine.path().to_path_buf()], &id).is_some());
    }

    #[test]
    fn read_by_path_takes_only_a_file_directly_in_a_search_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let search = tmp.path().join("search");
        std::fs::create_dir_all(search.join("nested")).unwrap();
        std::fs::write(search.join("ok.json"), sample("ok", "OK")).unwrap();
        std::fs::write(search.join("nested").join("deep.json"), sample("d", "D")).unwrap();
        let dirs = [search.clone()];

        let (path, content) = read_by_path(&dirs, &search.join("ok.json")).unwrap();
        assert_eq!(path, search.join("ok.json"));
        assert!(content.contains("\"OK\""));
        assert_eq!(
            read_by_path(&dirs, &search.join("nested").join("deep.json")),
            Err(PathReadError::OutsideSearchDirs)
        );
        assert_eq!(
            read_by_path(&dirs, &search.join("missing.json")),
            Err(PathReadError::NotFound)
        );
        assert_eq!(
            read_by_path(&dirs, &tmp.path().join("gone").join("x.json")),
            Err(PathReadError::NotFound)
        );
        assert_eq!(
            read_by_path(&dirs, Path::new("search/ok.json")),
            Err(PathReadError::OutsideSearchDirs)
        );
    }

    #[test]
    fn a_listing_reads_a_bounded_number_of_names_per_directory() {
        // Security review S4: one user's directory full of files must not make
        // every caller's listing read without end.
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_PROFILES_LISTED_PER_DIR + 10 {
            let id = format!("p{i:04}");
            std::fs::write(dir.path().join(format!("{id}.json")), sample(&id, "P")).unwrap();
        }
        let ids: Vec<String> = list(&[dir.path().to_path_buf()])
            .into_iter()
            .map(|p| p.id)
            .collect();
        let first: Vec<String> = (0..MAX_PROFILES_LISTED_PER_DIR)
            .map(|i| format!("p{i:04}"))
            .collect();
        assert_eq!(
            ids, first,
            "the first names in sorted order, whatever readdir's order"
        );
    }

    #[test]
    fn a_busy_root_directory_is_never_gone_past() {
        // Security review of FFA-b: skipping a busy directory and reading on let
        // anyone who kept the store busy have a same-named file in their own
        // registered directory served or activated in its place.
        let store = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        std::fs::write(store.path().join("x.json"), sample("x", "Stored")).unwrap();
        std::fs::write(user.path().join("x.json"), sample("x", "Planted")).unwrap();
        let dirs = vec![store.path().to_path_buf(), user.path().to_path_buf()];
        let _root = test_hook::treat_as_root_owned(store.path());

        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_rx, entered_tx) = (
            parking_lot::Mutex::new(release_rx),
            parking_lot::Mutex::new(entered_tx),
        );
        let _hook = test_hook::block_opens_of(store.path(), move || {
            let _ = entered_tx.lock().send(());
            let _ = release_rx
                .lock()
                .recv_timeout(std::time::Duration::from_secs(10));
        });
        let holder = {
            let dirs = dirs.clone();
            std::thread::spawn(move || read_by_id(&dirs, "x").map(|(_, content)| content))
        };
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the holder took the store");
        // Old enough that a user's directory would now be skipped.
        std::thread::sleep(PROFILE_DIR_HUNG_AFTER + std::time::Duration::from_millis(100));

        let soon = || Instant::now() + std::time::Duration::from_millis(300);
        assert_eq!(read_by_id_until(&dirs, "x", soon()), Err(OutOfTime));
        assert!(
            list_until(&dirs, soon()).is_err(),
            "a listing that could not read the store must fail, not list without it"
        );

        let _ = release_tx.send(());
        let read = holder.join().unwrap().unwrap();
        assert!(read.contains("Stored"), "{read}");
    }

    #[test]
    fn the_store_refuses_a_write_past_its_byte_cap() {
        // Security review of FFA-b: every listing reads the store, so its total
        // size bounds what one listing costs.
        let dir = tempfile::tempdir().unwrap();
        // Sparse: its length counts, its blocks are not written.
        let big = std::fs::File::create(dir.path().join("big.json")).unwrap();
        big.set_len(MAX_STORE_BYTES - 10).unwrap();
        assert_eq!(
            store(dir.path(), "next", &sample("next", "N"), false),
            Err(StoreError::TooLarge)
        );
        assert!(!dir.path().join("next.json").exists());
        // Replacing the big one with something small frees its bytes.
        assert_eq!(store(dir.path(), "big", &sample("big", "B"), true), Ok(()));
        assert_eq!(
            store(dir.path(), "next", &sample("next", "N"), false),
            Ok(())
        );
    }

    // ── FFA-j: store bounds ────────────────────────────────────────────

    #[test]
    fn store_refuses_a_duplicate_create_and_allows_a_replace() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "p", &sample("p", "One"), false).unwrap();
        assert_eq!(
            store(dir.path(), "p", &sample("p", "Two"), false),
            Err(StoreError::AlreadyExists)
        );
        store(dir.path(), "p", &sample("p", "Two"), true).unwrap();
        assert_eq!(
            get_raw(&[dir.path().to_path_buf()], "p").unwrap()["name"],
            "Two"
        );
    }

    #[test]
    fn store_refuses_a_new_id_once_full_but_still_replaces() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_STORED_PROFILES {
            let id = format!("p{i}");
            store(dir.path(), &id, &sample(&id, "P"), false).unwrap();
        }
        assert_eq!(
            store(dir.path(), "one-more", &sample("one-more", "P"), false),
            Err(StoreError::Full)
        );
        assert_eq!(
            store(dir.path(), "one-more", &sample("one-more", "P"), true),
            Err(StoreError::Full),
            "a PUT of a new id creates one, so it is bounded too"
        );
        assert!(!dir.path().join("one-more.json").exists());
        store(dir.path(), "p0", &sample("p0", "Replaced"), true).unwrap();
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
