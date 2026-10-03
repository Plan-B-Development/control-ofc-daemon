//! Canonical hwmon chip names — the it87 v2.0 board suffix (DEC-442, `BRD-a`).
//!
//! it87 v2.0 (frankcrawford/it87 PR #132, merged 2026-09-09) names every ITE chip
//! on a Gigabyte board after the board's SIV word whenever it can read one:
//! `devm_kasprintf(dev, GFP_KERNEL, "%s_%08x", name, siv)` in `it87_probe`, so
//! `it8696` publishes as `it8696_a008090a`. Every stable id this daemon builds
//! embeds the chip name, so without this module a driver rebuild renames every
//! header, sensor, fan and voltage id on those boards — and orphans every pump
//! role, profile member and preferred sensor saved against the old spelling.
//!
//! **The suffix is stripped once, where the name is read**, so every id, table
//! and compare downstream sees `it8696` exactly as it did before the rename. The
//! sysfs spelling is kept beside it on the header descriptor (`sysfs_chip_name`)
//! for the one consumer that needs it: a client matching `/etc/sensors.d` blocks,
//! which upstream now writes against the suffixed name.
//!
//! **Pattern only, never the SIV file** (Q2-a). The rule is upstream's own:
//! `install-sensorsd.sh` parses these names with `^it[0-9][0-9]*_([0-9A-Fa-f]{8})$`.
//! Requiring the suffix to equal `/sys/class/gigabyte/id/gigabyte_siv` would make
//! the ids flip back to the suffixed spelling whenever that file cannot be read,
//! which is the failure this module exists to prevent. A name that starts like an
//! ITE chip and carries an underscore but does not fit is left alone and logged
//! once, so a future change to the format is visible rather than silently
//! splitting ids again.
//!
//! [`canonical_hwmon_id`] applies the same rule to a stored id. It exists for the
//! read-side clean-up of state a daemon before this change saved under the
//! suffixed spelling (header roles, cooling devices, preferred sensors, profiles,
//! and the two boot-pruned stores) — see DEC-442.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::error::HwmonError;
use crate::hwmon::util::read_sysfs_string;

/// Length of the it87 v2.0 board suffix: `%08x`, always eight hex digits.
const SUFFIX_HEX_DIGITS: usize = 8;

/// A chip name as the daemon uses it (`canonical`) and as sysfs published it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipName {
    /// The name every id, table and compare uses — `it8696`.
    pub canonical: String,
    /// The `name` attribute exactly as read (trimmed) — `it8696_a008090a`.
    /// Equal to `canonical` everywhere except an it87 v2.0 Gigabyte chip.
    pub sysfs: String,
}

/// Strip the it87 v2.0 board suffix from a chip name; any other name is
/// returned unchanged.
///
/// Matches upstream's own parser exactly: `^it[0-9]+_[0-9A-Fa-f]{8}$`.
pub fn canonical_chip_name(raw: &str) -> &str {
    match split_it87_suffix(raw) {
        Some(base) => base,
        None => {
            if looks_like_unrecognised_ite_suffix(raw) {
                warn_once_unrecognised(raw);
            }
            raw
        }
    }
}

/// The base name when `raw` is exactly `it<digits>_<8 hex digits>`.
fn split_it87_suffix(raw: &str) -> Option<&str> {
    let (base, suffix) = raw.split_once('_')?;
    let digits = base.strip_prefix("it")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if suffix.len() != SUFFIX_HEX_DIGITS || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(base)
}

/// `it<digit>…_…` that [`split_it87_suffix`] rejected — the shape a future
/// change to upstream's format would take.
fn looks_like_unrecognised_ite_suffix(raw: &str) -> bool {
    raw.strip_prefix("it")
        .and_then(|rest| rest.bytes().next())
        .is_some_and(|b| b.is_ascii_digit())
        && raw.contains('_')
}

fn warn_once_unrecognised(raw: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let first = seen
        .lock()
        .map(|mut s| s.insert(raw.to_string()))
        .unwrap_or(false);
    if first {
        log::warn!(
            "hwmon chip '{raw}' looks like an ITE chip with a suffix, but not the it87 v2.0 \
             board suffix (<chip>_<8 hex digits>); using the name as published, so its ids \
             will not match ones saved under a different spelling (DEC-442)"
        );
    }
}

/// Read an hwmon device's `name` attribute and canonicalise it.
pub fn read_chip_name(hwmon_dir: &Path) -> Result<ChipName, HwmonError> {
    let sysfs = read_sysfs_string(&hwmon_dir.join("name"))?
        .trim()
        .to_string();
    let canonical = canonical_chip_name(&sysfs).to_string();
    Ok(ChipName { canonical, sysfs })
}

/// Canonicalise the chip segment of a stable hwmon id
/// (`hwmon:<chip>:<device>:…`). Any other id — OpenFan, GPU, or an hwmon id
/// whose chip carries no suffix — is returned borrowed and unchanged.
pub fn canonical_hwmon_id(id: &str) -> Cow<'_, str> {
    let mut parts = id.splitn(3, ':');
    let (Some("hwmon"), Some(chip), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return Cow::Borrowed(id);
    };
    match split_it87_suffix(chip) {
        Some(base) => Cow::Owned(format!("hwmon:{base}:{rest}")),
        None => Cow::Borrowed(id),
    }
}

/// Whether `id` is an hwmon id whose chip segment carries the it87 v2.0 suffix.
pub fn is_suffixed_hwmon_id(id: &str) -> bool {
    matches!(canonical_hwmon_id(id), Cow::Owned(_))
}

/// Re-key a store of per-header records by canonical hwmon id (DEC-442).
///
/// For the daemon-owned stores keyed by header id (`control_paths.json`,
/// `pwm_baselines.json`), applied where they load — before the boot prune, which
/// would otherwise delete every record a pre-DEC-442 daemon saved under the it87
/// v2.0 suffixed spelling. `fix` receives the canonical id and the record, so
/// the record's own copy of its id (and any ids it carries) can follow. Where
/// both spellings of one header hold a record, the suffixed one wins: it can
/// only have been written after the rebuild.
pub fn canonicalize_keyed<V>(
    records: BTreeMap<String, V>,
    mut fix: impl FnMut(&str, &mut V),
) -> BTreeMap<String, V> {
    let mut out: BTreeMap<String, (V, bool)> = BTreeMap::new();
    for (key, mut record) in records {
        let suffixed = is_suffixed_hwmon_id(&key);
        let canonical = canonical_hwmon_id(&key).into_owned();
        fix(&canonical, &mut record);
        let replaces = match out.get(&canonical) {
            None => true,
            Some((_, kept_suffixed)) => {
                let replaces = suffixed && !kept_suffixed;
                log::warn!(
                    "Two records for header '{canonical}' under different chip spellings; \
                     keeping the {} one (DEC-442)",
                    if replaces || *kept_suffixed {
                        "suffixed"
                    } else {
                        "earlier"
                    }
                );
                replaces
            }
        };
        if replaces {
            out.insert(canonical, (record, suffixed));
        }
    }
    out.into_iter().map(|(k, (v, _))| (k, v)).collect()
}

/// The chip segment of a stable hwmon id (`hwmon:<chip>:<device>:…`), or
/// `None` for any other id.
pub fn hwmon_id_chip(id: &str) -> Option<&str> {
    let mut parts = id.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("hwmon"), Some(chip), Some(_)) => Some(chip),
        _ => None,
    }
}

/// Prune a store of per-header records to the headers discovery can see
/// (`PTR-af`). Returns how many records went, so an unchanged store costs no
/// disk write at boot.
///
/// A record goes only when discovery saw its CHIP and not its header. A chip
/// discovery did not see at all — its driver not yet loaded, mid DKMS rebuild,
/// failed to bind this once — keeps every record, so an empty discovery prunes
/// nothing. A record whose id is not an hwmon id can never match a header and
/// goes whenever discovery found any. The cost is that records from a removed
/// board stay until the store's own capacity evicts them; they are keyed by the
/// full header id, so nothing ever matches them to a live header.
pub fn prune_to_live_chips<V>(
    records: &mut BTreeMap<String, V>,
    live_header_ids: &[String],
) -> usize {
    if live_header_ids.is_empty() {
        return 0;
    }
    let live_chips: HashSet<&str> = live_header_ids
        .iter()
        .filter_map(|id| hwmon_id_chip(id))
        .collect();
    let before = records.len();
    records.retain(|header_id, _| {
        live_header_ids.iter().any(|live| live == header_id)
            || hwmon_id_chip(header_id).is_some_and(|chip| !live_chips.contains(chip))
    });
    before - records.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cross-stack oracle (DEC-442). The GUI's
    /// `knowledge/chip_name.py` runs the byte-identical copy in
    /// `tests/fixtures/chip_name_canonical.json`, and `parity.yml` in both repos
    /// fails if the two copies diverge — the two canonicalisers must agree, or
    /// the GUI's settings clean-up keys a fan name to an id the daemon never
    /// publishes.
    #[test]
    fn canonical_chip_name_matches_the_cross_stack_oracle() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/chip_name_canonical.json"
        );
        let text = std::fs::read_to_string(path).expect("read chip-name fixture");
        let v: serde_json::Value = serde_json::from_str(&text).expect("parse chip-name fixture");

        let names = v["chip_names"].as_array().expect("chip_names array");
        assert!(!names.is_empty(), "an empty oracle asserts nothing");
        let mut stripped = 0;
        for case in names {
            let raw = case["raw"].as_str().unwrap();
            let want = case["canonical"].as_str().unwrap();
            assert_eq!(canonical_chip_name(raw), want, "chip name {raw:?}");
            stripped += usize::from(raw != want);
        }
        // Presence before absence: the oracle must exercise the strip itself,
        // not only the names it leaves alone.
        assert!(stripped >= 2, "the oracle must contain suffixed names");

        let ids = v["ids"].as_array().expect("ids array");
        assert!(!ids.is_empty(), "an empty oracle asserts nothing");
        for case in ids {
            let raw = case["raw"].as_str().unwrap();
            let want = case["canonical"].as_str().unwrap();
            assert_eq!(canonical_hwmon_id(raw), want, "id {raw:?}");
            assert_eq!(is_suffixed_hwmon_id(raw), raw != want, "id {raw:?}");
        }
    }

    #[test]
    fn read_chip_name_keeps_the_sysfs_spelling_beside_the_canonical_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("name"), "it8696_a008090a\n").unwrap();
        let name = read_chip_name(dir.path()).unwrap();
        assert_eq!(name.canonical, "it8696");
        assert_eq!(name.sysfs, "it8696_a008090a");

        std::fs::write(dir.path().join("name"), "nct6799\n").unwrap();
        let name = read_chip_name(dir.path()).unwrap();
        assert_eq!(name.canonical, "nct6799");
        assert_eq!(name.sysfs, "nct6799");
    }

    /// Build an it87-style hwmon device: two labelled PWM headers with tachs,
    /// a monitor-only tach, a temperature and a voltage rail, with the platform
    /// device symlinked so the id's device segment is the real `it87.2624`.
    fn it87_device(root: &Path, name: &str) {
        let platform = root.join("devices/platform/it87.2624");
        std::fs::create_dir_all(&platform).unwrap();
        let d = root.join("hwmon3");
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(&platform, d.join("device")).unwrap();
        std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
        for (n, label) in [(1, "CPU_FAN"), (5, "SYS_FAN5_PUMP")] {
            std::fs::write(d.join(format!("pwm{n}")), "128\n").unwrap();
            std::fs::write(d.join(format!("pwm{n}_enable")), "1\n").unwrap();
            std::fs::write(d.join(format!("fan{n}_input")), "900\n").unwrap();
            std::fs::write(d.join(format!("fan{n}_label")), format!("{label}\n")).unwrap();
        }
        std::fs::write(d.join("fan7_input"), "700\n").unwrap();
        std::fs::write(d.join("temp1_input"), "41000\n").unwrap();
        std::fs::write(d.join("in0_input"), "1100\n").unwrap();
    }

    /// Every id discovery builds, from all four `name` read sites.
    fn discovered_ids(root: &Path) -> Vec<String> {
        let hwmon = root.to_path_buf();
        let mut ids: Vec<String> = Vec::new();
        ids.extend(
            crate::hwmon::pwm_discovery::discover_pwm_headers(&hwmon)
                .unwrap()
                .into_iter()
                .map(|h| h.id),
        );
        ids.extend(
            crate::hwmon::discovery::discover_sensors(&hwmon)
                .unwrap()
                .into_iter()
                .map(|s| s.id),
        );
        ids.extend(
            crate::hwmon::inventory::discover_monitor_only_fans(&hwmon)
                .unwrap()
                .into_iter()
                .map(|f| f.id),
        );
        ids.extend(
            crate::hwmon::voltages::discover_voltages(&hwmon)
                .unwrap()
                .into_iter()
                .map(|v| v.id),
        );
        ids.sort();
        ids
    }

    /// [SAFETY] `BRD-a`, at the four call sites. A chip published as
    /// `it8696_a008090a` must produce exactly the ids the same chip produced as
    /// `it8696` — header, sensor, monitor-only fan and voltage alike — because
    /// every saved pump role, profile member and preferred sensor is keyed by
    /// them.
    #[test]
    fn a_suffixed_chip_discovers_exactly_the_ids_of_the_bare_chip() {
        let bare = tempfile::tempdir().unwrap();
        let suffixed = tempfile::tempdir().unwrap();
        it87_device(bare.path(), "it8696");
        it87_device(suffixed.path(), "it8696_a008090a");

        let want = discovered_ids(bare.path());
        // Presence first: every kind of id is really there, on the real device.
        assert_eq!(want.len(), 2 + 1 + 1 + 1, "{want:?}");
        assert!(
            want.iter()
                .all(|id| id.starts_with("hwmon:it8696:it87.2624:")),
            "{want:?}"
        );
        assert_eq!(discovered_ids(suffixed.path()), want);

        // The sysfs spelling rides beside it, for /etc/sensors.d matching only.
        let headers = crate::hwmon::pwm_discovery::discover_pwm_headers(suffixed.path()).unwrap();
        assert!(headers.iter().all(|h| h.chip_name == "it8696"));
        assert!(headers
            .iter()
            .all(|h| h.sysfs_chip_name() == "it8696_a008090a"));
        let headers = crate::hwmon::pwm_discovery::discover_pwm_headers(bare.path()).unwrap();
        assert!(headers.iter().all(|h| h.sysfs_chip_name() == "it8696"));
    }

    #[test]
    fn a_canonical_id_is_borrowed_not_rebuilt() {
        let id = "hwmon:it8696:it87.2624:pwm5:SYS_FAN5_PUMP";
        assert!(matches!(canonical_hwmon_id(id), Cow::Borrowed(_)));
    }

    fn store(ids: &[&str]) -> BTreeMap<String, ()> {
        ids.iter().map(|id| (id.to_string(), ())).collect()
    }

    const NCT_PUMP: &str = "hwmon:nct6798:nct6775.656:pwm2:AIO_PUMP";
    const NCT_GONE: &str = "hwmon:nct6798:nct6775.656:pwm7:OLD";
    const IT87_CPU: &str = "hwmon:it8688:it87.2624:pwm1:CPU_FAN";

    #[test]
    fn the_chip_is_the_second_segment_of_an_hwmon_id_only() {
        assert_eq!(hwmon_id_chip(NCT_PUMP), Some("nct6798"));
        // A device id with its own colons does not move the chip segment.
        assert_eq!(
            hwmon_id_chip("hwmon:arctic_fan:0003:3904:F001.0001:pwm1:pwm1"),
            Some("arctic_fan")
        );
        assert_eq!(hwmon_id_chip("openfan:ch00"), None);
        assert_eq!(hwmon_id_chip("hwmon:nct6798"), None);
    }

    /// `PTR-af`: a boot before the Super-I/O driver loads must not erase the
    /// stores. Pre-fix, every record went.
    #[test]
    fn an_empty_discovery_prunes_nothing() {
        let mut records = store(&[NCT_PUMP, IT87_CPU, "not-an-hwmon-id"]);
        assert_eq!(prune_to_live_chips(&mut records, &[]), 0);
        assert_eq!(records, store(&[NCT_PUMP, IT87_CPU, "not-an-hwmon-id"]));
    }

    /// A chip discovery did not see keeps its records even while another chip
    /// is live — one driver loading late must not cost the other its history.
    #[test]
    fn only_a_chip_discovery_saw_is_pruned() {
        let mut records = store(&[NCT_PUMP, NCT_GONE, IT87_CPU]);
        let dropped = prune_to_live_chips(&mut records, &[NCT_PUMP.to_string()]);
        assert_eq!(dropped, 1);
        assert_eq!(records, store(&[NCT_PUMP, IT87_CPU]));
        // Unchanged hardware costs nothing on the next boot.
        assert_eq!(
            prune_to_live_chips(&mut records, &[NCT_PUMP.to_string()]),
            0
        );
    }

    #[test]
    fn a_non_hwmon_id_goes_once_discovery_found_anything() {
        let mut records = store(&[NCT_PUMP, "not-an-hwmon-id"]);
        assert_eq!(
            prune_to_live_chips(&mut records, &[NCT_PUMP.to_string()]),
            1
        );
        assert_eq!(records, store(&[NCT_PUMP]));
    }
}
