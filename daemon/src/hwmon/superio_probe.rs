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
//! - **Read before write, then clean enter/exit.** The DEVID read happens with
//!   **no unlock at all**; a vendor unlock is written only when that read
//!   returns `0xffff`, exactly as `it87_find` does. This ordering is the whole
//!   safety property — see [`probe_base`]. Until 2026-09-05 this bullet claimed
//!   the sequences were "byte-for-byte per the kernel drivers (verified against
//!   `it87.c`)". The unlock **bytes** did match; the **decision whether to
//!   unlock** did not, and only the decision protects the hardware. We read only
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

/// Did nothing answer at this base? `0xffff` is the usual undecoded-read value
/// and `0x0000` the other one chipsets produce; [`is_valid_family_devid`] has
/// always treated both as "no chip", and this must agree with it or the two
/// predicates disagree about what an empty base looks like. **This is the only
/// condition that licenses an unlock write** — see [`probe_base`].
fn no_response(devid: u16) -> bool {
    !is_valid_family_devid(devid)
}

/// What a DEVID that has actually been read means, wherever it was read from.
/// Shared by the no-enter read and the post-unlock reads so the two cannot
/// drift — they did, once, and it put a destructive write back on the ITE leg.
enum DevidVerdict {
    /// The eSPI→LPC bridge. Report it; never write to this base again.
    Bridge,
    /// A nameable ITE chip.
    IteChip,
    /// Nothing is listening. The one case where an unlock is licensed.
    NoResponse,
    /// Something answered that we cannot name. Do not write — a base that
    /// already responds is exactly where an unlock is unnecessary and risky.
    UnknownResponder,
}

fn classify_devid(devid: u16) -> DevidVerdict {
    if devid == IT8883_BRIDGE_DEVID {
        DevidVerdict::Bridge
    } else if is_ite_devid(devid) {
        DevidVerdict::IteChip
    } else if no_response(devid) {
        DevidVerdict::NoResponse
    } else {
        DevidVerdict::UnknownResponder
    }
}

/// Build the bridge report. `chip_name: None` deliberately: an ITE-family
/// response that is emphatically not a chip. The raw DEVID is the diagnostic
/// value — it is what tells an operator the bridge is latched.
fn bridge_report(base: u16, devid: u16) -> ProbedChip {
    log::warn!(
        "Super-I/O base {base:#06x} answered DEVID {devid:#06x}: an ITE eSPI→LPC \
         bridge is in configuration mode and is masking the Super-I/O behind it. \
         Not probing this base further — the config-mode unlock write is what \
         causes this. Recovery needs a full power cut (a reboot does not clear \
         it); see the Hardware Troubleshooting guide."
    );
    ProbedChip {
        base,
        vendor: SuperIoVendor::Ite,
        devid,
        chip_name: None,
    }
}

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

/// The IT8883 eSPI→LPC bridge's signature.
///
/// It is **not** a sensor chip and never becomes one. It answers in place of the
/// Super-I/O behind it while it is in configuration mode, and what puts it there
/// is precisely an unlock write to its base — the write this module performed
/// before reading until 2026-09-05.
///
/// That cost is **measured, not theoretical**: on an X870E AORUS MASTER, loading
/// a driver that unlocks 0x4E hid the secondary IT87952E (3 fan headers, 3
/// thermistor temps) until a full power cut. Naming it `it8883` reported
/// hardware that does not exist, on the one code path able to create the state
/// it was reporting.
pub(crate) const IT8883_BRIDGE_DEVID: u16 = 0x8883;

/// ITE DEVIDs are `0x86xx`–`0x88xx` (the chip number after "IT", e.g. IT8688E =
/// 0x8688). The bridge signature is explicitly **excluded** — it is a bus
/// bridge, not a chip we can name. See [`IT8883_BRIDGE_DEVID`].
fn is_ite_devid(devid: u16) -> bool {
    devid != IT8883_BRIDGE_DEVID && matches!(devid >> 8, 0x86..=0x88)
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

/// RAII guard that issues the matching Super-I/O config-mode *exit* on drop, so
/// a probe that dies mid-sequence (canonically the DEVID read erroring right
/// after the unlock) can never leave the chip in unlock/config mode. Mirrors the
/// `CalibrationGuard` idiom in `api/handlers/openfan.rs`.
///
/// Best-effort by necessity: `drop` cannot return a `Result`, so a failed exit
/// write is logged at `debug` and swallowed — the caller still sees the original
/// probe error (the read failure), which is the meaningful one to surface.
struct SioExitGuard<'a> {
    r: &'a dyn SuperIoPortReader,
    base: u16,
    vendor: SuperIoVendor,
}

impl Drop for SioExitGuard<'_> {
    fn drop(&mut self) {
        let exit = match self.vendor {
            SuperIoVendor::Ite => ite_exit(self.r, self.base),
            SuperIoVendor::Nuvoton => nuvoton_exit(self.r, self.base),
            // The probe arms this guard only for ITE / Nuvoton — the two families
            // with an unlock sequence here. No other vendor is ever put into
            // config mode, so there is nothing to exit.
            _ => return,
        };
        if let Err(e) = exit {
            // A failed exit leaves the chip in config mode until reset — warn so
            // it is visible above the default log level, not just at `debug`.
            log::warn!(
                "Super-I/O {:?} config-mode exit at base {:#06x} failed: {e}",
                self.vendor,
                self.base
            );
        }
    }
}

/// Probe one already-confirmed-unclaimed base.
///
/// **Reads before it writes.** `it87_find` performs its DEVID read via
/// `superio_enter(sioaddr, /*noentry=*/true)` — no unlock — and unlocks only if
/// that returns `0xffff`, because `it8790`/`it8792`/`it87952` carry
/// `FEAT_NOCONF` ("chip conf mode enabled on startup"). This function used to
/// unlock first and read second, which is the exact write that latches an
/// IT8883 eSPI→LPC bridge into configuration mode and hides the Super-I/O
/// behind it. See [`IT8883_BRIDGE_DEVID`] for the measurement.
///
/// Order of operations:
///   1. **no-enter read** — a `FEAT_NOCONF` chip answers here and we never write
///      to its base at all, which is also why a healthy board now reports its
///      true DEVID instead of a bridge signature we created ourselves;
///   2. **the bridge** — reported without writing. Unlocking is what creates
///      that state, so probing it again could only deepen the damage;
///   3. **anything else that answers unlocked** — unrecognised, so we bail
///      without writing, exactly as `it87_find` does rather than forcing a
///      second opinion out of a chip that already gave one;
///   4. **`0xffff` only** — nothing answered, so fall through to the vendor
///      unlock sequences unchanged: ITE first (short-circuiting on a match so
///      the wrong vendor's exit never runs on an identified chip), then the
///      Nuvoton/Winbond family. Each attempt arms a [`SioExitGuard`] right after
///      its unlock, so the matching exit runs on every path out of the block —
///      including an early `?` return when the DEVID read itself errors
///      (DEC-203 config-mode-leak fix).
fn probe_base(r: &dyn SuperIoPortReader, base: u16) -> io::Result<Option<ProbedChip>> {
    // ── 1-3: the no-enter read, before any write reaches this base ──
    let unlocked_devid = sio_devid(r, base)?;
    match classify_devid(unlocked_devid) {
        DevidVerdict::Bridge => return Ok(Some(bridge_report(base, unlocked_devid))),
        DevidVerdict::IteChip => {
            // A FEAT_NOCONF chip, already in config mode at power-on. Identified
            // without a single UNLOCK byte reaching the port — `sio_inb` still
            // writes the register index (0x20/0x21) to select it, which is a
            // read protocol, not an unlock. The tests assert on that distinction.
            return Ok(Some(ProbedChip {
                base,
                vendor: SuperIoVendor::Ite,
                devid: unlocked_devid,
                chip_name: Some(format!("it{unlocked_devid:04x}")),
            }));
        }
        DevidVerdict::UnknownResponder => return Ok(None),
        DevidVerdict::NoResponse => { /* fall through: an unlock is licensed */ }
    }

    // ── 4: nothing answered without an unlock — vendor sequences, unchanged ──
    // ── ITE ──
    {
        ite_enter(r, base)?;
        let _exit = SioExitGuard {
            r,
            base,
            vendor: SuperIoVendor::Ite,
        };
        let devid = sio_devid(r, base)?;
        match classify_devid(devid) {
            // THE regression this arm exists to prevent. Our own ITE unlock can
            // be what latches the bridge, so 0x8883 here is the EXPECTED reading
            // on an affected board — not an edge case. Before this arm existed,
            // `is_ite_devid` matched 0x8883 and returned; once it stopped
            // matching, the value fell through to `nuvoton_enter` and wrote
            // `0x87,0x87` — the exact sequence measured to cause the latch — and
            // the hit was then reported as Nuvoton, recommending the very module
            // the packaged guard exists to block.
            DevidVerdict::Bridge => return Ok(Some(bridge_report(base, devid))),
            DevidVerdict::IteChip => {
                return Ok(Some(ProbedChip {
                    base,
                    vendor: SuperIoVendor::Ite,
                    devid,
                    chip_name: Some(format!("it{devid:04x}")),
                }));
            }
            // Something answered the ITE unlock that is not ITE and not a
            // bridge. A locked Nuvoton chip reads as no-response here, so this
            // is not the Nuvoton case — do not write a second unlock at it.
            DevidVerdict::UnknownResponder => return Ok(None),
            DevidVerdict::NoResponse => { /* fall through to the Nuvoton leg */ }
        }
    }

    // ── Nuvoton / Winbond family ──
    {
        nuvoton_enter(r, base)?;
        let _exit = SioExitGuard {
            r,
            base,
            vendor: SuperIoVendor::Nuvoton,
        };
        let devid = sio_devid(r, base)?;
        if is_valid_family_devid(devid) {
            return Ok(Some(ProbedChip {
                base,
                vendor: SuperIoVendor::Nuvoton,
                devid,
                chip_name: None,
            }));
        }
    }

    Ok(None)
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
    use std::cell::{Cell, RefCell};

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
        /// Models silicon that is already in configuration mode at power-on and
        /// answers with no unlock written: `FEAT_NOCONF` chips
        /// (it8790/it8792/it87952), and the IT8883 bridge once something has
        /// latched it. Data reads succeed immediately.
        answers_without_unlock: bool,
        selected: RefCell<u8>,
        entered: RefCell<Vec<u8>>,
        /// Fault injection: succeed this many reads, then error on every read
        /// after. `None` never fails. Exercises the exit-guard when a DEVID read
        /// dies mid-sequence.
        fail_read_after: Cell<Option<usize>>,
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
        /// A chip already in config mode at startup — it answers its DEVID with
        /// no unlock written at all.
        fn noconf(devid: u16) -> Self {
            let mut f = Self::new(devid, vec![0x87, 0x01, 0x55, 0xaa]);
            f.answers_without_unlock = true;
            f
        }
        /// The IT8883 eSPI→LPC bridge, latched into config mode by somebody
        /// else's unlock write. Answers for the same reason a NOCONF chip does.
        fn bridge() -> Self {
            Self::noconf(IT8883_BRIDGE_DEVID)
        }
        /// A base that reads as empty until an unlock is written, and answers
        /// the BRIDGE signature afterwards — i.e. our own `ite_enter` is what
        /// latched it. This is the real shape of an affected board whose bridge
        /// was not already latched, and the case a post-unlock bridge check
        /// must catch.
        fn bridge_latched_by_our_own_unlock() -> Self {
            Self::new(IT8883_BRIDGE_DEVID, vec![0x87, 0x01, 0x55, 0xaa])
        }
        fn empty() -> Self {
            // No chip: never unlocks, data reads are 0xff.
            Self {
                writes: RefCell::new(Vec::new()),
                devid_hi: 0xff,
                devid_lo: 0xff,
                unlocked_by: None,
                answers_without_unlock: false,
                selected: RefCell::new(0),
                entered: RefCell::new(Vec::new()),
                fail_read_after: Cell::new(None),
            }
        }
        fn new(devid: u16, unlock: Vec<u8>) -> Self {
            Self {
                writes: RefCell::new(Vec::new()),
                devid_hi: (devid >> 8) as u8,
                devid_lo: (devid & 0xff) as u8,
                unlocked_by: Some(unlock),
                answers_without_unlock: false,
                selected: RefCell::new(0),
                entered: RefCell::new(Vec::new()),
                fail_read_after: Cell::new(None),
            }
        }
        fn is_unlocked(&self) -> bool {
            if self.answers_without_unlock {
                return true;
            }
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
        /// Builder: succeed `ok_reads` reads, then error on every read after —
        /// models a DEVID read that dies after the unlock, exercising the
        /// [`SioExitGuard`] exit path.
        fn failing_after(self, ok_reads: usize) -> Self {
            self.fail_read_after.set(Some(ok_reads));
            self
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
            // Optional fault injection: succeed `n` reads, then error — models a
            // DEVID read that dies mid-sequence so the exit-guard path is taken.
            match self.fail_read_after.get() {
                Some(0) => return Err(io::Error::other("injected read failure")),
                Some(n) => self.fail_read_after.set(Some(n - 1)),
                None => {}
            }
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
    fn probe_ite_devid_read_failure_still_exits_config_mode() {
        // DEC-203 leak regression: if the DEVID read errors right after
        // ite_enter, the SioExitGuard must still fire ite_exit. The pre-fix code
        // returned via `?` without exiting, stranding the chip in unlock/config
        // mode where nothing owns it.
        //
        // `failing_after(2)`, not 0: since the read-before-write fix the FIRST
        // two reads are the no-enter DEVID probe, which happens before any
        // unlock and therefore has no config mode to leak. Letting the fault
        // land there would assert nothing about the guard — it would just
        // observe an error path that writes no unlock at all.
        let p = FakePort::ite(0x8688).failing_after(2);
        let err = probe_base(&p, 0x2e).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::Other,
            "the original read error still propagates to the caller"
        );
        let writes = p.writes.borrow();
        assert!(
            writes.windows(2).any(|w| w == [(0x2e, 0x02), (0x2f, 0x02)]),
            "ITE exit (CR02) must run even when the DEVID read fails: {writes:?}"
        );
        assert_eq!(
            writes.last(),
            Some(&(0x2f, 0x02)),
            "base left clean (exited), not mid-config"
        );
    }

    #[test]
    fn probe_nuvoton_devid_read_failure_still_exits_config_mode() {
        // Same leak on the Nuvoton leg: the ITE leg misses (its two DEVID reads
        // read a locked 0xffff), then the Nuvoton DEVID read (the 3rd read)
        // errors — the guard must fire nuvoton_exit on the way out.
        //
        // `failing_after(4)` counts two `sio_devid` calls ahead of the Nuvoton
        // leg, each two port reads (hi @ 0x20 + lo @ 0x21): the no-enter probe
        // added by the read-before-write fix, then the ITE leg. If that count
        // ever changes, bump this — otherwise the fault lands on an earlier leg
        // and silently stops exercising the Nuvoton guard.
        let p = FakePort::nuvoton(0xd428).failing_after(4);
        let err = probe_base(&p, 0x2e).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        let writes = p.writes.borrow();
        assert!(
            writes
                .windows(3)
                .any(|w| w == [(0x2e, 0xaa), (0x2e, 0x02), (0x2f, 0x02)]),
            "Nuvoton exit must run even when its DEVID read fails: {writes:?}"
        );
        assert_eq!(
            writes.last(),
            Some(&(0x2f, 0x02)),
            "base left clean (exited), not mid-config"
        );
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
        assert!(is_ite_devid(0x8695)); // IT87952E, the chip a latched bridge hides
        assert!(
            !is_ite_devid(IT8883_BRIDGE_DEVID),
            "0x8883 is an eSPI→LPC bridge, not a chip. This assertion was \
             inverted until 2026-09-05, which is how the probe came to report a \
             chip named `it8883` that has never existed."
        );
        assert!(!is_ite_devid(0xffff));
        assert!(!is_ite_devid(0xd592)); // Nuvoton
        assert!(!is_ite_devid(0x0000));
    }

    /// The unlock byte shared by BOTH vendor sequences (ITE `0x87,0x01,0x55,..`
    /// and Nuvoton `0x87,0x87`). Its absence from the write log is the precise
    /// safety property: `sio_inb` legitimately writes register indices 0x20/0x21
    /// to the base, so "wrote nothing at all" would be the wrong assertion.
    const UNLOCK_BYTE: u8 = 0x87;

    fn wrote_an_unlock(p: &FakePort) -> bool {
        p.writes.borrow().iter().any(|(_, v)| *v == UNLOCK_BYTE)
    }

    #[test]
    fn noconf_chip_is_identified_without_writing_any_unlock() {
        // it8790/it8792/it87952 carry FEAT_NOCONF — already in config mode at
        // power-on. `it87_find` reads them with `noentry=true` and never writes;
        // so must we, because that write is what latches a bridge.
        let p = FakePort::noconf(0x8695);
        let hits = probe_ports(&p, &[0x4e]);

        assert_eq!(hits.len(), 1, "a NOCONF chip must still be found");
        assert_eq!(hits[0].chip_name.as_deref(), Some("it8695"));
        assert!(
            !wrote_an_unlock(&p),
            "the probe must identify a NOCONF chip with NO unlock written. \
             Writes were: {:?}",
            p.writes.borrow()
        );
    }

    #[test]
    fn latched_bridge_is_reported_as_a_bridge_and_never_unlocked() {
        let p = FakePort::bridge();
        let hits = probe_ports(&p, &[0x4e]);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].devid, IT8883_BRIDGE_DEVID);
        assert_eq!(
            hits[0].chip_name, None,
            "0x8883 must NOT be named as a chip — it is a bus bridge"
        );
        assert!(
            !wrote_an_unlock(&p),
            "unlocking is what latches the bridge in the first place, so the \
             one base already showing that signature is the last place to \
             write one. Writes were: {:?}",
            p.writes.borrow()
        );
    }

    #[test]
    fn bridge_appearing_after_our_own_unlock_never_reaches_the_nuvoton_leg() {
        // Regression for the P1 found in review of DEC-332's first draft.
        // Dropping 0x8883 from `is_ite_devid` stopped the post-unlock ITE read
        // short-circuiting, so the value fell through to `nuvoton_enter` and
        // wrote `0x87,0x87` — the sequence measured to cause the latch — on a
        // path the pre-fix code never wrote it. It was then reported as a
        // Nuvoton chip, recommending `modprobe nct6775`: the exact module the
        // packaged guard exists to block, on the exact board it protects.
        let p = FakePort::bridge_latched_by_our_own_unlock();
        let hits = probe_ports(&p, &[0x4e]);

        assert_eq!(hits.len(), 1, "the bridge must still be reported");
        assert_eq!(hits[0].devid, IT8883_BRIDGE_DEVID);
        assert_eq!(
            hits[0].vendor,
            SuperIoVendor::Ite,
            "an ITE bridge must never be reported as Nuvoton — that is what \
             produces a `modprobe nct6775` recommendation on a board where \
             loading it is destructive"
        );
        assert_eq!(hits[0].chip_name, None);

        // The load-bearing assertion: exactly ONE unlock reached this base (the
        // ITE one), never the Nuvoton follow-up. Counting is what discriminates
        // here — asserting "no 0x87 at all" would fail on the legitimate ITE
        // unlock and prove nothing about the second one.
        let unlock_writes = p
            .writes
            .borrow()
            .iter()
            .filter(|(port, v)| *port == 0x4e && *v == 0x87)
            .count();
        assert_eq!(
            unlock_writes,
            1,
            "expected only the ITE unlock's leading 0x87; a second means the \
             Nuvoton leg ran after the bridge answered. Writes: {:?}",
            p.writes.borrow()
        );
    }

    #[test]
    fn a_base_reading_zero_is_treated_as_empty_not_as_a_responder() {
        // `is_valid_family_devid` has always counted 0x0000 as "no chip". The
        // unlock gate must agree, or a chipset that reads 0x00 on an undecoded
        // port would make the probe skip a genuine locked chip and report
        // nothing — a false negative on the one board the probe exists for.
        assert!(matches!(classify_devid(0x0000), DevidVerdict::NoResponse));
        assert!(matches!(classify_devid(0xffff), DevidVerdict::NoResponse));
        assert!(matches!(
            classify_devid(IT8883_BRIDGE_DEVID),
            DevidVerdict::Bridge
        ));
        assert!(matches!(classify_devid(0x8628), DevidVerdict::IteChip));
        assert!(matches!(
            classify_devid(0xd428),
            DevidVerdict::UnknownResponder
        ));
    }

    #[test]
    fn locked_chip_still_gets_its_unlock() {
        // The complement, without which the two tests above pass against a
        // probe that simply never writes anything and finds nothing.
        let p = FakePort::ite(0x8628);
        let hits = probe_ports(&p, &[0x2e]);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chip_name.as_deref(), Some("it8628"));
        assert!(
            wrote_an_unlock(&p),
            "a LOCKED chip returns 0xffff to the no-enter read, which is the \
             one case that still licenses the unlock sequence"
        );
    }
}
