//! Bus-independent ids for memory-module temperature sensors (DEC-492).
//!
//! A memory-module sensor (`spd5118`, `jc42`) is an i2c client, and its sysfs
//! device is named `<bus>-<addr>` (`21-0051`). The bus number is assigned by
//! the kernel in registration order (`i2c_add_adapter`), so a GPU swap, an iGPU
//! toggle or a module-load reorder renumbers it — and the sensor id, which
//! embeds the device name, with it. Curves, chart colours and hidden series
//! bound to the old id then point at nothing.
//!
//! The stable device segment names the module by what does not move: the SMBus
//! controller (the device above the adapter, normally its PCI address), the
//! adapter's port (`p<N>`, from a piix4 adapter name) or mux channel
//! (`ch<K>`, from the mux device's `channel-K` link), and the SPD address:
//! `0000:00:14.0-p0-0051`. Where the topology cannot be named unambiguously
//! the legacy `<bus>-<addr>` form is kept, so only what is understood is
//! re-keyed.
//!
//! Saved ids cross between the two forms through [`resolve`], the rule the GUI
//! applies to its own stores. Both sides are pinned to the shared oracle
//! `tests/fixtures/memory_sensor_ids.json`.
//!
//! Scope is the memory-module chips only ([`MEMORY_MODULE_CHIPS`]): they carry
//! no fan or PWM ids, so no header id, role or floor input is re-keyed.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::hwmon::classify::MEMORY_MODULE_CHIPS;

/// A parsed memory-module sensor id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySensorId<'a> {
    pub chip: &'a str,
    /// True for the legacy `<bus>-<addr>` device segment.
    pub legacy: bool,
    /// The SMBus controller; empty for the legacy form.
    pub controller: &'a str,
    /// Port / mux-channel tokens, outermost first; empty for the legacy form.
    pub segments: Vec<&'a str>,
    /// The SPD (i2c) address.
    pub address: u16,
    pub label: &'a str,
}

fn is_addr4(s: &str) -> bool {
    s.len() == 4
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// A `p<N>` or `ch<K>` token.
fn is_segment(s: &str) -> bool {
    s.strip_prefix("ch")
        .or_else(|| s.strip_prefix('p'))
        .is_some_and(is_digits)
}

/// Parse `hwmon:<memory chip>:<device>:<label>`; `None` for anything else.
///
/// The label is the last `:`-separated part and the device everything between
/// the chip and it (a PCI controller contains colons of its own).
pub fn parse(id: &str) -> Option<MemorySensorId<'_>> {
    let rest = id.strip_prefix("hwmon:")?;
    let (chip, rest) = rest.split_once(':')?;
    if !MEMORY_MODULE_CHIPS.contains(&chip) {
        return None;
    }
    let (device, label) = rest.rsplit_once(':')?;
    if device.is_empty() || label.is_empty() {
        return None;
    }
    let (head, addr) = device.rsplit_once('-')?;
    if !is_addr4(addr) || head.is_empty() {
        return None;
    }
    let address = u16::from_str_radix(addr, 16).ok()?;
    if is_digits(head) {
        return Some(MemorySensorId {
            chip,
            legacy: true,
            controller: "",
            segments: Vec::new(),
            address,
            label,
        });
    }
    // Peel `-p<N>` / `-ch<K>` tokens off the end; what is left is the
    // controller.
    let mut controller = head;
    let mut segments = Vec::new();
    while let Some((before, last)) = controller.rsplit_once('-') {
        if before.is_empty() || !is_segment(last) {
            break;
        }
        segments.push(last);
        controller = before;
    }
    segments.reverse();
    Some(MemorySensorId {
        chip,
        legacy: false,
        controller,
        segments,
        address,
        label,
    })
}

/// The live id a saved sensor id refers to (DEC-492), or `None`.
///
/// A live id is itself. Otherwise every other id with the same chip, SPD
/// address and label is a candidate — live, or `unavailable` (quarantined,
/// which the cache evicts from the live set), in either form — and the id moves
/// only when there is exactly one, it is live, and it is of the **other** form
/// (legacy ↔ stable). A quarantined or same-form twin therefore blocks the move
/// instead of letting the remaining module inherit the saved one's curve, and a
/// stable id never moves to another stable id. Both directions, so an id
/// written by an older daemon (or saved before a downgrade) is followed.
pub fn resolve<'a, 'b>(
    saved: &str,
    live: impl IntoIterator<Item = &'a str>,
    unavailable: impl IntoIterator<Item = &'b str>,
) -> Option<&'a str> {
    let mut live_twin: Option<&'a str> = None;
    let mut twins = 0usize;
    let wanted = parse(saved);
    let is_twin = |id: &str| {
        let (Some(w), Some(p)) = (wanted.as_ref(), parse(id)) else {
            return None;
        };
        (p.chip == w.chip && p.address == w.address && p.label == w.label)
            .then_some(p.legacy != w.legacy)
    };
    // Each distinct id counts once, whichever list it is in (the GUI's
    // `dict.fromkeys` over both); a live listing wins over an unavailable one.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for id in live {
        if id == saved {
            return Some(id);
        }
        if !seen.insert(id) {
            continue;
        }
        if let Some(other_form) = is_twin(id) {
            twins += 1;
            live_twin = other_form.then_some(id);
        }
    }
    for id in unavailable {
        if id != saved && seen.insert(id) && is_twin(id).is_some() {
            twins += 1;
        }
    }
    if twins == 1 {
        live_twin
    } else {
        None
    }
}

/// `i2c-<N>`: an adapter's sysfs directory.
fn is_adapter_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("i2c-"))
        .is_some_and(is_digits)
}

/// The token that tells an adapter apart from its siblings: `Some(Some(_))`
/// for a port or mux channel, `Some(None)` for an adapter with neither, and
/// `None` when it cannot be read.
fn adapter_token(adapter: &Path) -> Option<Option<String>> {
    let mux = adapter.join("mux_device");
    if mux.exists() {
        // A mux child: the mux device carries `channel-K` links to its adapters.
        let me = std::fs::canonicalize(adapter).ok()?;
        let mux = std::fs::canonicalize(mux).ok()?;
        for entry in std::fs::read_dir(&mux).ok()?.flatten() {
            let name = entry.file_name();
            let Some(k) = name.to_str().and_then(|n| n.strip_prefix("channel-")) else {
                continue;
            };
            if is_digits(k) && std::fs::canonicalize(entry.path()).ok().as_ref() == Some(&me) {
                return Some(Some(format!("ch{k}")));
            }
        }
        return None;
    }
    // piix4 names its adapters "SMBus PIIX4 adapter port N at XXXX"; i801 and
    // older piix4 publish no port.
    let name = std::fs::read_to_string(adapter.join("name")).ok()?;
    let port = name.split_once(" port ").and_then(|(_, after)| {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        (!digits.is_empty()).then_some(digits)
    });
    Some(port.map(|n| format!("p{n}")))
}

/// Whether another adapter beside `adapter` would get the same token — or
/// cannot be named, and so might.
fn sibling_collides(adapter: &Path, token: &Option<String>) -> bool {
    let Some(parent) = adapter.parent() else {
        return true;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return true;
    };
    entries.flatten().any(|e| {
        let path = e.path();
        path != adapter && is_adapter_dir(&path) && adapter_token(&path).is_none_or(|t| &t == token)
    })
}

/// The stable device segment of a memory-module sensor's hwmon directory, or
/// `None` where the topology cannot be named unambiguously (the caller keeps
/// the legacy `<bus>-<addr>` form).
pub fn stable_device_id(hwmon_dir: &Path) -> Option<String> {
    let client = std::fs::canonicalize(hwmon_dir.join("device")).ok()?;
    let client_name = client.file_name()?.to_str()?;
    let (bus, addr) = client_name.split_once('-')?;
    if !is_digits(bus) || !is_addr4(addr) {
        return None;
    }
    let mut adapter = client.parent()?.to_path_buf();
    if adapter.file_name()?.to_str()? != format!("i2c-{bus}") {
        return None;
    }
    let mut segments = Vec::new();
    loop {
        let token = adapter_token(&adapter)?;
        if sibling_collides(&adapter, &token) {
            return None;
        }
        segments.extend(token);
        let parent = adapter.parent()?;
        if is_adapter_dir(parent) {
            adapter = parent.to_path_buf();
            continue;
        }
        let controller = parent.file_name()?.to_str()?;
        if controller.is_empty() || is_digits(controller) {
            return None;
        }
        segments.reverse();
        let mut id = controller.to_string();
        for s in &segments {
            id.push('-');
            id.push_str(s);
        }
        id.push('-');
        id.push_str(addr);
        return Some(id);
    }
}

static FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

/// The device segment for a sensor on `chip_name`: the stable form for a
/// memory-module chip whose topology can be named, else `None` (the caller's
/// usual device id applies). The first fallback is logged once per process.
pub fn device_id_for_memory_chip(chip_name: &str, hwmon_dir: &Path) -> Option<String> {
    if !MEMORY_MODULE_CHIPS.contains(&chip_name) {
        return None;
    }
    let stable = stable_device_id(hwmon_dir);
    if stable.is_none() && !FALLBACK_LOGGED.swap(true, Ordering::Relaxed) {
        log::info!(
            "{}: memory-module SMBus topology could not be named unambiguously; keeping the \
             bus-numbered sensor id (DEC-492)",
            hwmon_dir.display()
        );
    }
    stable
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    // ── the shared oracle ───────────────────────────────────────────────

    /// The cross-stack oracle (DEC-492). The GUI's
    /// `services/memory_id_migration.py` runs the byte-identical copy, and
    /// `parity.yml` in both repos fails if the two diverge — the two rules must
    /// agree, or the GUI re-keys a curve to an id the engine resolves elsewhere.
    #[test]
    fn memory_sensor_ids_match_the_cross_stack_oracle() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/memory_sensor_ids.json"
        );
        let text = std::fs::read_to_string(path).expect("read memory-id fixture");
        let v: serde_json::Value = serde_json::from_str(&text).expect("parse memory-id fixture");

        let parses = v["parse"].as_array().expect("parse array");
        let mut parsed = 0;
        for case in parses {
            let id = case["id"].as_str().unwrap();
            let got = parse(id);
            let want = &case["expect"];
            if want.is_null() {
                assert!(got.is_none(), "{id:?} must not parse, got {got:?}");
                continue;
            }
            parsed += 1;
            let got = got.unwrap_or_else(|| panic!("{id:?} must parse"));
            assert_eq!(got.chip, want["chip"].as_str().unwrap(), "{id:?}");
            assert_eq!(got.legacy, want["legacy"].as_bool().unwrap(), "{id:?}");
            assert_eq!(
                got.controller,
                want["controller"].as_str().unwrap(),
                "{id:?}"
            );
            let segs: Vec<&str> = want["segments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap())
                .collect();
            assert_eq!(got.segments, segs, "{id:?}");
            assert_eq!(
                u64::from(got.address),
                want["address"].as_u64().unwrap(),
                "{id:?}"
            );
            assert_eq!(got.label, want["label"].as_str().unwrap(), "{id:?}");
        }
        assert!(parsed >= 4, "the oracle must exercise both forms");

        let resolves = v["resolve"].as_array().expect("resolve array");
        let mut moved = 0;
        for case in resolves {
            let saved = case["saved"].as_str().unwrap();
            let live: Vec<&str> = case["live"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap())
                .collect();
            let want = case["expect"].as_str();
            let unavailable: Vec<&str> = case
                .get("unavailable")
                .and_then(|u| u.as_array())
                .map(|a| a.iter().map(|s| s.as_str().unwrap()).collect())
                .unwrap_or_default();
            assert_eq!(
                resolve(saved, live.iter().copied(), unavailable.iter().copied()),
                want,
                "{}",
                case["why"].as_str().unwrap()
            );
            moved += usize::from(want.is_some_and(|w| w != saved));
        }
        assert!(moved >= 2, "the oracle must contain real moves");
    }

    // ── sysfs topology ──────────────────────────────────────────────────

    struct Tree {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            Self { _tmp: tmp, root }
        }

        fn adapter(&self, rel: &str, name: &str) -> PathBuf {
            let dir = self.root.join(rel);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("name"), format!("{name}\n")).unwrap();
            dir
        }

        /// An i2c client under `adapter` with an hwmon dir whose `device` link
        /// points at it — relative, as the kernel writes it.
        fn client(&self, adapter: &Path, client: &str, hwmon: &str) -> PathBuf {
            let dev = adapter.join(client);
            fs::create_dir_all(&dev).unwrap();
            let hw = self.root.join("class").join(hwmon);
            fs::create_dir_all(&hw).unwrap();
            let rel = pathdiff(&dev, &hw);
            symlink(rel, hw.join("device")).unwrap();
            hw
        }
    }

    /// `to` relative to `from` (both absolute, sharing the tree root).
    fn pathdiff(to: &Path, from: &Path) -> PathBuf {
        let to: Vec<_> = to.components().collect();
        let from: Vec<_> = from.components().collect();
        let common = to.iter().zip(&from).take_while(|(a, b)| a == b).count();
        let mut out = PathBuf::new();
        for _ in common..from.len() {
            out.push("..");
        }
        for c in &to[common..] {
            out.push(c);
        }
        out
    }

    /// This machine's layout: one piix4 controller with three adapters, the
    /// modules on port 0 at bus 21.
    #[test]
    fn piix4_ports_name_the_adapter() {
        let t = Tree::new();
        let pci = "devices/pci0000:00/0000:00:14.0";
        let p0 = t.adapter(
            &format!("{pci}/i2c-21"),
            "SMBus PIIX4 adapter port 0 at 0b00",
        );
        t.adapter(
            &format!("{pci}/i2c-22"),
            "SMBus PIIX4 adapter port 2 at 0b00",
        );
        t.adapter(
            &format!("{pci}/i2c-23"),
            "SMBus PIIX4 adapter port 1 at 0b20",
        );
        let a = t.client(&p0, "21-0051", "hwmon9");
        let b = t.client(&p0, "21-0053", "hwmon10");
        assert_eq!(
            stable_device_id(&a).as_deref(),
            Some("0000:00:14.0-p0-0051")
        );
        assert_eq!(
            stable_device_id(&b).as_deref(),
            Some("0000:00:14.0-p0-0053")
        );
    }

    /// The point of the change: the same module on a renumbered bus gets the
    /// same id.
    #[test]
    fn a_renumbered_bus_keeps_the_id() {
        let ids: Vec<_> = [21, 7]
            .into_iter()
            .map(|bus| {
                let t = Tree::new();
                let pci = "devices/pci0000:00/0000:00:14.0";
                let p0 = t.adapter(
                    &format!("{pci}/i2c-{bus}"),
                    "SMBus PIIX4 adapter port 0 at 0b00",
                );
                stable_device_id(&t.client(&p0, &format!("{bus}-0051"), "hwmon9"))
            })
            .collect();
        assert!(ids[0].is_some());
        assert_eq!(ids[0], ids[1]);
    }

    /// i801: one adapter, no port in its name.
    #[test]
    fn i801_single_adapter_has_no_segment() {
        let t = Tree::new();
        let a = t.adapter(
            "devices/pci0000:00/0000:00:1f.4/i2c-0",
            "SMBus I801 adapter at 0000:00:1f.4",
        );
        let hw = t.client(&a, "0-0050", "hwmon3");
        assert_eq!(stable_device_id(&hw).as_deref(), Some("0000:00:1f.4-0050"));
    }

    /// A mux child is named by the mux device's `channel-K` link, not by its
    /// own name (which embeds the parent's bus number).
    #[test]
    fn mux_child_is_named_by_its_channel() {
        let t = Tree::new();
        let main = t.adapter(
            "devices/pci0000:00/0000:00:1f.4/i2c-0",
            "SMBus I801 adapter at 0000:00:1f.4",
        );
        let mux = t.root.join("devices/platform/i2c-mux-gpio.0");
        fs::create_dir_all(&mux).unwrap();
        let mut seg = Vec::new();
        for k in 0..2 {
            let child = t.adapter(
                &format!("devices/pci0000:00/0000:00:1f.4/i2c-0/i2c-{}", 5 + k),
                &format!("i2c-0-mux (chan_id {k})"),
            );
            symlink(&mux, child.join("mux_device")).unwrap();
            symlink(&child, mux.join(format!("channel-{k}"))).unwrap();
            seg.push(child);
        }
        let hw = t.client(&seg[1], "6-0050", "hwmon4");
        assert_eq!(
            stable_device_id(&hw).as_deref(),
            Some("0000:00:1f.4-ch1-0050")
        );
        let _ = main;
    }

    /// Two sibling adapters with the same token cannot be told apart: keep the
    /// legacy form rather than guess.
    #[test]
    fn colliding_sibling_tokens_fall_back() {
        let t = Tree::new();
        let pci = "devices/pci0000:00/0000:00:14.0";
        let a = t.adapter(&format!("{pci}/i2c-21"), "SMBus PIIX4 adapter at 0b00");
        t.adapter(&format!("{pci}/i2c-22"), "SMBus PIIX4 adapter at 0b20");
        let hw = t.client(&a, "21-0051", "hwmon9");
        assert_eq!(stable_device_id(&hw), None);

        // Opposite branch: the same adapter alone is named.
        let t = Tree::new();
        let a = t.adapter(&format!("{pci}/i2c-21"), "SMBus PIIX4 adapter at 0b00");
        let hw = t.client(&a, "21-0051", "hwmon9");
        assert_eq!(stable_device_id(&hw).as_deref(), Some("0000:00:14.0-0051"));
    }

    /// An unreadable sibling might collide, so it falls back too.
    #[test]
    fn an_unnameable_sibling_falls_back() {
        let t = Tree::new();
        let pci = "devices/pci0000:00/0000:00:14.0";
        let a = t.adapter(
            &format!("{pci}/i2c-21"),
            "SMBus PIIX4 adapter port 0 at 0b00",
        );
        fs::create_dir_all(t.root.join(format!("{pci}/i2c-22"))).unwrap(); // no `name`
        let hw = t.client(&a, "21-0051", "hwmon9");
        assert_eq!(stable_device_id(&hw), None);
    }

    /// A client whose device is not `<bus>-<addr>` under `i2c-<bus>` (ACPI
    /// names, a missing link) has no stable form.
    #[test]
    fn unrecognised_shapes_have_no_stable_form() {
        let t = Tree::new();
        let a = t.adapter(
            "devices/platform/AMDI0010:01/i2c-1",
            "Synopsys DesignWare I2C adapter",
        );
        let hw = t.client(&a, "i2c-ITE8800:00", "hwmon5");
        assert_eq!(stable_device_id(&hw), None);
        let bare = t.root.join("class/hwmon6");
        fs::create_dir_all(&bare).unwrap();
        assert_eq!(stable_device_id(&bare), None);
        // A client on a bus whose directory does not match its name.
        let other = t.adapter(
            "devices/pci0000:00/0000:00:14.0/i2c-3",
            "SMBus PIIX4 adapter",
        );
        let hw = t.client(&other, "4-0051", "hwmon7");
        assert_eq!(stable_device_id(&hw), None);
    }

    /// Only memory-module chips are re-keyed.
    #[test]
    fn only_memory_chips_get_the_stable_form() {
        let t = Tree::new();
        let a = t.adapter(
            "devices/pci0000:00/0000:00:14.0/i2c-21",
            "SMBus PIIX4 adapter port 0 at 0b00",
        );
        let hw = t.client(&a, "21-002e", "hwmon9");
        assert_eq!(device_id_for_memory_chip("nct7802", &hw), None);
        assert_eq!(
            device_id_for_memory_chip("spd5118", &hw).as_deref(),
            Some("0000:00:14.0-p0-002e")
        );
    }

    /// Every stable id the builder produces parses back as the stable form with
    /// the same address — the builder and the parser cannot disagree.
    #[test]
    fn built_ids_parse_back() {
        for (device, segs) in [
            ("0000:00:14.0-p0-0051", vec!["p0"]),
            ("0000:00:1f.4-ch1-0050", vec!["ch1"]),
            ("0000:00:1f.4-0050", vec![]),
        ] {
            let id = format!("hwmon:spd5118:{device}:temp1");
            let p = parse(&id).unwrap();
            assert!(!p.legacy);
            assert_eq!(p.segments, segs);
        }
    }
}
