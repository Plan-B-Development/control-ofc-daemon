//! Opt-in active Super-I/O port probe (DEC-203). **The only code in the daemon
//! that touches an x86 I/O port.**
//!
//! It reads the Super-I/O configuration ports (0x2E / 0x4E) to identify a
//! motherboard chip whose kernel driver is not loaded — something passive
//! detection cannot do for a chip that never reached hwmon, kmsg, or the DMI
//! board table. It exists so a user with an unbound Super-I/O chip can be told
//! which driver to load.
//!
//! ## Non-negotiable safety invariants (all enforced here or by the caller)
//! - **Off by default.** `[detection] allow_port_probe` gates it, and it needs
//!   `CAP_SYS_RAWIO` (granted only via an opt-in systemd drop-in).
//!   [`port_probe_available`] reports exactly why it is or isn't usable.
//! - **Refuse the race.** A userspace port access bypasses the kernel's
//!   `request_muxed_region`, so we ONLY probe a base the caller has confirmed is
//!   unclaimed (no bound driver, no ACPI/ioport reservation). On such a base
//!   nothing else owns the chip, bounding the blast radius.
//! - **Clean enter/exit every time**, byte-for-byte per the kernel drivers
//!   (verified against `it87.c` and `nct6775-platform.c`, 2026-07). We read only
//!   the fixed DEVID registers and **never** write a configuration value,
//!   `force_id`, or the hardware-monitor block. A vendor whose unlock did not
//!   take stays locked and ignores our writes.
//! - **One-shot.** Driven by a deliberate `POST`; never in a loop, never at
//!   startup.
//! - **Safe Rust.** `/dev/port` via positioned `FileExt` I/O — no `unsafe`, no
//!   inline asm. Kernel lockdown (Secure Boot) blocks the open; we degrade.

use std::io;
use std::os::unix::fs::FileExt;

use crate::hwmon::superio::SuperIoVendor;

/// The two canonical Super-I/O configuration base ports. The index port is the
/// base, the data port is `base + 1`. No other ports are ever touched.
pub const SIO_BASES: [u16; 2] = [0x2e, 0x4e];

/// Read/write access to individual x86 I/O ports, injected so the probe logic is
/// exercised against a fake — no real hardware, no `CAP_SYS_RAWIO` — in tests.
pub trait SuperIoPortReader {
    fn read_port(&self, port: u16) -> io::Result<u8>;
    fn write_port(&self, port: u16, value: u8) -> io::Result<()>;
}

/// Production backend: `/dev/port` positioned reads/writes (byte offset = port
/// number, per `drivers/char/mem.c`). Safe Rust. Opening it requires
/// `CAP_SYS_RAWIO` and a non-locked-down kernel.
pub struct DevPortReader {
    file: std::fs::File,
}

impl DevPortReader {
    pub fn open() -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/port")?;
        Ok(Self { file })
    }
}

impl SuperIoPortReader for DevPortReader {
    fn read_port(&self, port: u16) -> io::Result<u8> {
        let mut b = [0u8; 1];
        self.file.read_at(&mut b, u64::from(port))?;
        Ok(b[0])
    }

    fn write_port(&self, port: u16, value: u8) -> io::Result<()> {
        self.file.write_at(&[value], u64::from(port))?;
        Ok(())
    }
}

/// A chip identified by an active port probe of one base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedChip {
    /// The config base it responded at (0x2e or 0x4e).
    pub base: u16,
    pub vendor: SuperIoVendor,
    /// The raw 16-bit DEVID read from register 0x20/0x21.
    pub devid: u16,
    /// A specific hwmon chip name when derivable (ITE: `it{devid}`); `None` for
    /// families we only identify at vendor level (Nuvoton/Winbond).
    pub chip_name: Option<String>,
}

/// Read one config register: write its index to the base port, read the value
/// from the data port (`base + 1`). Mirrors the kernel `superio_inb`.
fn sio_inb(r: &dyn SuperIoPortReader, base: u16, reg: u8) -> io::Result<u8> {
    r.write_port(base, reg)?;
    r.read_port(base + 1)
}

/// Read the 16-bit DEVID at 0x20 (high) / 0x21 (low). Mirrors `superio_inw`.
fn sio_devid(r: &dyn SuperIoPortReader, base: u16) -> io::Result<u16> {
    let hi = sio_inb(r, base, 0x20)?;
    let lo = sio_inb(r, base, 0x21)?;
    Ok((u16::from(hi) << 8) | u16::from(lo))
}

// ── ITE (it87.c) ────────────────────────────────────────────────────

fn ite_enter(r: &dyn SuperIoPortReader, base: u16) -> io::Result<()> {
    r.write_port(base, 0x87)?;
    r.write_port(base, 0x01)?;
    r.write_port(base, 0x55)?;
    r.write_port(base, if base == 0x4e { 0xaa } else { 0x55 })?;
    Ok(())
}

fn ite_exit(r: &dyn SuperIoPortReader, base: u16) -> io::Result<()> {
    // CR02 = 0x02 (software-exit config mode).
    r.write_port(base, 0x02)?;
    r.write_port(base + 1, 0x02)?;
    Ok(())
}

/// ITE DEVIDs are `0x86xx`–`0x88xx` (the chip number after "IT", e.g. IT8688E =
/// 0x8688). `0x88xx` covers the config-mode `0x8883` reported by the STEALTH ICE
/// secondary before recovery.
fn is_ite_devid(devid: u16) -> bool {
    matches!(devid >> 8, 0x86..=0x88)
}

// ── Nuvoton / Winbond 0x87,0x87 family (nct6775-platform.c) ──────────

fn nuvoton_enter(r: &dyn SuperIoPortReader, base: u16) -> io::Result<()> {
    r.write_port(base, 0x87)?;
    r.write_port(base, 0x87)?;
    Ok(())
}

fn nuvoton_exit(r: &dyn SuperIoPortReader, base: u16) -> io::Result<()> {
    r.write_port(base, 0xaa)?;
    r.write_port(base, 0x02)?;
    r.write_port(base + 1, 0x02)?;
    Ok(())
}

/// A chip that responded to the `0x87,0x87` unlock reports a real DEVID; an
/// empty/absent base reads `0xffff` (all-ones) or `0x0000`. We identify this
/// family at vendor level only (no reliable in-repo Nuvoton/Winbond DEVID
/// table), reporting the raw DEVID for the user's reference.
fn is_valid_family_devid(devid: u16) -> bool {
    devid != 0xffff && devid != 0x0000
}

/// Probe one already-confirmed-unclaimed base. Tries ITE first (short-circuits
/// on a match so the wrong vendor's exit never runs on an identified chip), then
/// the Nuvoton/Winbond family. Returns `Ok(None)` when nothing plausible
/// responds. Each attempt does a clean enter/exit even on a miss.
fn probe_base(r: &dyn SuperIoPortReader, base: u16) -> io::Result<Option<ProbedChip>> {
    // ── ITE ──
    ite_enter(r, base)?;
    let devid = sio_devid(r, base)?;
    if is_ite_devid(devid) {
        ite_exit(r, base)?;
        return Ok(Some(ProbedChip {
            base,
            vendor: SuperIoVendor::Ite,
            devid,
            chip_name: Some(format!("it{devid:04x}")),
        }));
    }
    ite_exit(r, base)?;

    // ── Nuvoton / Winbond family ──
    nuvoton_enter(r, base)?;
    let devid = sio_devid(r, base)?;
    let hit = if is_valid_family_devid(devid) {
        Some(ProbedChip {
            base,
            vendor: SuperIoVendor::Nuvoton,
            devid,
            chip_name: None,
        })
    } else {
        None
    };
    nuvoton_exit(r, base)?;
    Ok(hit)
}

/// Probe each supplied base (the caller MUST pass only unclaimed bases). A probe
/// error on one base is logged and skipped, not fatal.
pub fn probe_ports(r: &dyn SuperIoPortReader, bases: &[u16]) -> Vec<ProbedChip> {
    let mut out = Vec::new();
    for &base in bases {
        match probe_base(r, base) {
            Ok(Some(chip)) => out.push(chip),
            Ok(None) => {}
            Err(e) => log::warn!("superio port probe: base 0x{base:04x} read failed: {e}"),
        }
    }
    out
}

/// Open `/dev/port` for probing, mapping the OS error to a plain-English reason
/// on failure. Shared by the availability check (which opens + drops) and the
/// probe path (which keeps the reader), so the probe opens `/dev/port` exactly
/// once (no TOCTOU double-open).
pub fn port_probe_open() -> Result<DevPortReader, String> {
    DevPortReader::open().map_err(|e| match e.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => {
            "/dev/port could not be opened — the daemon lacks CAP_SYS_RAWIO, the device is not \
             allowed by the unit, or the kernel is locked down (Secure Boot). Install the opt-in \
             superio-port-probe systemd drop-in."
                .to_string()
        }
        Some(libc::ENOENT) | Some(libc::ENXIO) => {
            "/dev/port is not present on this system".to_string()
        }
        _ => format!("/dev/port could not be opened: {e}"),
    })
}

/// Whether the active probe can run, and a plain-English reason when it cannot.
///
/// Off by default: requires `allow_port_probe` AND an openable `/dev/port`
/// (which itself requires `CAP_SYS_RAWIO`, the device-cgroup allowance, and a
/// kernel that is not locked down — all provided only by the opt-in drop-in).
pub fn port_probe_available(allow_port_probe: bool) -> (bool, String) {
    if !allow_port_probe {
        return (
            false,
            "disabled — set [detection] allow_port_probe = true in \
             /etc/control-ofc/daemon.toml to enable it"
                .to_string(),
        );
    }
    match port_probe_open() {
        Ok(_) => (true, "available".to_string()),
        Err(reason) => (false, reason),
    }
}

/// True if `/proc/ioports` shows a range covering `base` — i.e. some driver or
/// ACPI OperationRegion currently reserves the config port, so we must NOT probe
/// it. Parses lines like `002e-002f : pnp 00:03`.
pub fn base_claimed(proc_ioports: &str, base: u16) -> bool {
    for line in proc_ioports.lines() {
        let range = line.trim().split(" : ").next().unwrap_or("").trim();
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (
            u16::from_str_radix(start.trim(), 16),
            u16::from_str_radix(end.trim(), 16),
        ) else {
            continue;
        };
        if start <= base && base <= end {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Fake port device: records every write, and answers reads from a map keyed
    /// by (port, current-index). Lets tests assert the exact protocol bytes and
    /// the clean-exit invariant with no hardware.
    struct FakePort {
        writes: RefCell<Vec<(u16, u8)>>,
        /// The value the *data* port returns for a given selected register on a
        /// base, once the correct unlock has been written.
        devid_hi: u8,
        devid_lo: u8,
        /// If false, the chip is "locked" and every data read returns 0xff
        /// (models a chip whose unlock sequence did not match).
        unlocked_by: Option<Vec<u8>>, // the exact enter bytes that unlock it
        selected: RefCell<u8>,
        entered: RefCell<Vec<u8>>,
    }

    impl FakePort {
        fn ite(devid: u16) -> Self {
            Self::ite_at(0x2e, devid)
        }
        fn ite_at(base: u16, devid: u16) -> Self {
            // The 4th ITE enter byte is base-specific: 0xaa at 0x4e, else 0x55.
            let last = if base == 0x4e { 0xaa } else { 0x55 };
            Self::new(devid, vec![0x87, 0x01, 0x55, last])
        }
        /// Unlocks on the base-independent 3-byte ITE prefix, so one fake
        /// responds at BOTH 0x2e and 0x4e (for multi-base tests). The exact 4th
        /// byte is pinned separately by the enter-sequence tests.
        fn ite_any(devid: u16) -> Self {
            Self::new(devid, vec![0x87, 0x01, 0x55])
        }
        fn nuvoton(devid: u16) -> Self {
            Self::new(devid, vec![0x87, 0x87])
        }
        fn empty() -> Self {
            // No chip: never unlocks, data reads are 0xff.
            Self {
                writes: RefCell::new(Vec::new()),
                devid_hi: 0xff,
                devid_lo: 0xff,
                unlocked_by: None,
                selected: RefCell::new(0),
                entered: RefCell::new(Vec::new()),
            }
        }
        fn new(devid: u16, unlock: Vec<u8>) -> Self {
            Self {
                writes: RefCell::new(Vec::new()),
                devid_hi: (devid >> 8) as u8,
                devid_lo: (devid & 0xff) as u8,
                unlocked_by: Some(unlock),
                selected: RefCell::new(0),
                entered: RefCell::new(Vec::new()),
            }
        }
        fn is_unlocked(&self) -> bool {
            match &self.unlocked_by {
                None => false,
                // The chip is unlocked once its exact enter sequence has been
                // written contiguously to the index port at any point.
                Some(seq) => self
                    .entered
                    .borrow()
                    .windows(seq.len())
                    .any(|w| w == seq.as_slice()),
            }
        }
    }

    impl SuperIoPortReader for FakePort {
        fn write_port(&self, port: u16, value: u8) -> io::Result<()> {
            self.writes.borrow_mut().push((port, value));
            // Even ports here are index writes (base); odd are data (base+1).
            if port.is_multiple_of(2) {
                self.entered.borrow_mut().push(value);
                *self.selected.borrow_mut() = value;
            }
            Ok(())
        }
        fn read_port(&self, port: u16) -> io::Result<u8> {
            // A register value must be read from the DATA port (base + 1, odd).
            // A read from the index port (even) is a protocol error and yields
            // nothing — so a regression that read the wrong port fails its test.
            if port.is_multiple_of(2) || !self.is_unlocked() {
                return Ok(0xff);
            }
            Ok(match *self.selected.borrow() {
                0x20 => self.devid_hi,
                0x21 => self.devid_lo,
                _ => 0x00,
            })
        }
    }

    #[test]
    fn ite_enter_sequence_is_exact_for_0x2e_and_0x4e() {
        let p = FakePort::ite(0x8688);
        ite_enter(&p, 0x2e).unwrap();
        assert_eq!(
            p.writes.borrow().as_slice(),
            &[(0x2e, 0x87), (0x2e, 0x01), (0x2e, 0x55), (0x2e, 0x55)]
        );
        let p = FakePort::ite(0x8688);
        ite_enter(&p, 0x4e).unwrap();
        // Same first three bytes; the fourth is 0xaa on the secondary base.
        assert_eq!(
            p.writes.borrow().as_slice(),
            &[(0x4e, 0x87), (0x4e, 0x01), (0x4e, 0x55), (0x4e, 0xaa)]
        );
    }

    #[test]
    fn ite_exit_writes_cr02() {
        let p = FakePort::ite(0x8688);
        ite_exit(&p, 0x2e).unwrap();
        assert_eq!(p.writes.borrow().as_slice(), &[(0x2e, 0x02), (0x2f, 0x02)]);
    }

    #[test]
    fn nuvoton_enter_and_exit_are_exact() {
        let p = FakePort::nuvoton(0xd592);
        nuvoton_enter(&p, 0x2e).unwrap();
        assert_eq!(p.writes.borrow().as_slice(), &[(0x2e, 0x87), (0x2e, 0x87)]);
        let p = FakePort::nuvoton(0xd592);
        nuvoton_exit(&p, 0x2e).unwrap();
        assert_eq!(
            p.writes.borrow().as_slice(),
            &[(0x2e, 0xaa), (0x2e, 0x02), (0x2f, 0x02)]
        );
    }

    #[test]
    fn probe_identifies_ite_chip_and_derives_name() {
        let p = FakePort::ite(0x8688);
        let chip = probe_base(&p, 0x2e).unwrap().unwrap();
        assert_eq!(chip.vendor, SuperIoVendor::Ite);
        assert_eq!(chip.devid, 0x8688);
        assert_eq!(chip.chip_name.as_deref(), Some("it8688"));
        // The base was left in a clean state: the last writes are the ITE exit.
        assert_eq!(p.writes.borrow().last(), Some(&(0x2f, 0x02)));
    }

    #[test]
    fn probe_identifies_nuvoton_family_at_vendor_level() {
        let p = FakePort::nuvoton(0xd592);
        let chip = probe_base(&p, 0x2e).unwrap().unwrap();
        assert_eq!(chip.vendor, SuperIoVendor::Nuvoton);
        assert_eq!(chip.devid, 0xd592);
        assert_eq!(chip.chip_name, None); // vendor-level only
                                          // ITE was tried first (and cleanly exited) before the Nuvoton match.
        let writes = p.writes.borrow();
        assert!(writes.contains(&(0x2e, 0x01)), "ITE enter attempted first");
        assert!(writes.contains(&(0x2e, 0x87)), "Nuvoton enter followed");
        assert_eq!(writes.last(), Some(&(0x2f, 0x02)), "clean Nuvoton exit");
    }

    #[test]
    fn probe_empty_base_returns_none_and_still_exits_cleanly() {
        let p = FakePort::empty();
        assert!(probe_base(&p, 0x2e).unwrap().is_none());
        let writes = p.writes.borrow();
        // BOTH vendor exits must run so no chip is ever left in config mode.
        assert!(
            writes.windows(2).any(|w| w == [(0x2e, 0x02), (0x2f, 0x02)]),
            "ITE exit (CR02) missing: {writes:?}"
        );
        assert!(
            writes
                .windows(3)
                .any(|w| w == [(0x2e, 0xaa), (0x2e, 0x02), (0x2f, 0x02)]),
            "Nuvoton exit missing: {writes:?}"
        );
        // …and the final write is the Nuvoton exit's last byte (the base is left
        // clean, not mid-config).
        assert_eq!(writes.last(), Some(&(0x2f, 0x02)));
    }

    #[test]
    fn probe_identifies_ite_chip_at_secondary_base_0x4e() {
        // Exercises the full probe flow at 0x4E, where the 4th ITE enter byte is
        // 0xaa (not 0x55) and the data port is 0x4f.
        let p = FakePort::ite_at(0x4e, 0x8628);
        let chip = probe_base(&p, 0x4e).unwrap().unwrap();
        assert_eq!(chip.base, 0x4e);
        assert_eq!(chip.chip_name.as_deref(), Some("it8628"));
        let writes = p.writes.borrow();
        assert!(
            writes.contains(&(0x4e, 0xaa)),
            "0x4E uses the 0xaa enter byte"
        );
        assert_eq!(
            writes.last(),
            Some(&(0x4f, 0x02)),
            "clean exit at data port 0x4f"
        );
    }

    #[test]
    fn probe_ports_over_multiple_bases_collects_each_hit() {
        // A fake with an ITE chip responding at BOTH bases → two hits.
        let p = FakePort::ite_any(0x8688);
        let hits = probe_ports(&p, &SIO_BASES);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].base, 0x2e);
        assert_eq!(hits[1].base, 0x4e);
    }

    #[test]
    fn probe_ports_collects_hits() {
        let p = FakePort::ite(0x8628);
        let hits = probe_ports(&p, &[0x2e]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chip_name.as_deref(), Some("it8628"));
    }

    #[test]
    fn availability_is_disabled_when_flag_off() {
        let (ok, reason) = port_probe_available(false);
        assert!(!ok);
        assert!(reason.contains("allow_port_probe"));
    }

    #[test]
    fn base_claimed_detects_reserved_config_ports() {
        let ioports = "0000-001f : dma1\n002e-002f : pnp 00:03\n0290-029f : pnp 00:01\n";
        assert!(base_claimed(ioports, 0x2e), "0x2e is reserved by pnp");
        assert!(base_claimed(ioports, 0x2f));
        assert!(!base_claimed(ioports, 0x4e), "0x4e is free here");
        assert!(!base_claimed("", 0x2e));
    }

    #[test]
    fn is_ite_devid_ranges() {
        assert!(is_ite_devid(0x8688));
        assert!(is_ite_devid(0x8628));
        assert!(is_ite_devid(0x8883));
        assert!(!is_ite_devid(0xffff));
        assert!(!is_ite_devid(0xd592)); // Nuvoton
        assert!(!is_ite_devid(0x0000));
    }
}
