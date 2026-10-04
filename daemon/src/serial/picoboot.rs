//! PICOBOOT: writing an RP2040's flash through its USB bootloader (DEC-483).
//!
//! An RP2040 in its bootloader shows two USB interfaces: the `RPI-RP2` drive
//! (mass storage, the kernel's) and PICOBOOT, a vendor interface with one bulk
//! endpoint each way (RP2040 datasheet §2.8.5). The daemon claims PICOBOOT
//! alone; the drive is never touched.
//!
//! **A command** is 32 bytes on bulk OUT: magic, token, id, the size of its
//! arguments, the length of its data phase, then the arguments. READ and WRITE
//! carry a data phase (IN for READ); every command then ends with an empty
//! acknowledgement the opposite way to its data — IN when it had none. A
//! refused command stalls the endpoints: the command-status request says why,
//! and an interface reset clears it.
//!
//! **A write**, as the update's run drives it: [`Client::reset`]; exclusive
//! access, so the drive turns read-only and nothing else writes the flash
//! meanwhile; leave XIP; read the flash chip's unique id — which the OpenFan
//! firmware shows as its USB serial — with the helper picotool runs for the
//! same purpose ([`Client::flash_id`]); erase each 4 KiB sector the image
//! touches and program its pages ([`write_sector`]); read every page back
//! ([`verify_sector`]); reboot.
//!
//! Everything here blocks: callers run it on the blocking pool. Nothing here
//! opens a device but the RP2040 bootloader on the port it is given
//! ([`open_bootloader`]); [`usb_access`] opens nothing.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::serial::uf2;

/// The first word of every command.
pub const MAGIC: u32 = 0x431F_D10B;

/// Command ids (`bCmdId`).
pub const CMD_EXCLUSIVE_ACCESS: u8 = 0x01;
pub const CMD_REBOOT: u8 = 0x02;
pub const CMD_FLASH_ERASE: u8 = 0x03;
pub const CMD_READ: u8 = 0x84;
pub const CMD_WRITE: u8 = 0x05;
pub const CMD_EXIT_XIP: u8 = 0x06;
pub const CMD_EXEC: u8 = 0x08;
/// Bit 7 of a command id: its data phase is IN.
const DIR_IN: u8 = 0x80;

/// Vendor requests to the PICOBOOT interface.
pub const IF_RESET: u8 = 0x41;
pub const IF_CMD_STATUS: u8 = 0x42;
const STATUS_LEN: u16 = 16;

/// `EXCLUSIVE_ACCESS`: let the drive write again, or not.
pub const NOT_EXCLUSIVE: u8 = 0;
pub const EXCLUSIVE: u8 = 1;

/// How long a command may take to send — and the acknowledgement of one that
/// carried data — and how long a data phase, or the acknowledgement of a
/// command with none (an erase finishes before it), may take: picotool's limits.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
pub const DATA_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);

/// The reboot after a write: run the flash image (`pc` 0), stack at the top
/// of SRAM, half a second after the acknowledgement — picotool's `load -x`.
const REBOOT_PC: u32 = 0;
const REBOOT_SP: u32 = 0x2004_2000;
const REBOOT_DELAY_MS: u32 = 500;

/// RP2040 USB ids.
pub const RP2040_VENDOR_ID: u16 = 0x2e8a;
pub const RP2040_BOOTLOADER_PRODUCT_ID: u16 = 0x0003;

/// Where the flash-ID helper is loaded and run: XIP SRAM, free while XIP is off.
pub const FLASH_ID_CODE_ADDR: u32 = 0x1500_0000;
/// Where the helper leaves the id: in its receive buffer (offset 28), after the
/// byte clocked in with the command and the chip's four dummy bytes.
pub const FLASH_ID_RESULT_ADDR: u32 = FLASH_ID_CODE_ADDR + 28 + 1 + 4;

/// picotool's flash-ID helper, `picoboot_flash_id/flash_id.bin` in picotool
/// 2.3.1 (`2041936`), BSD-3-Clause, © Raspberry Pi (Trading) Ltd. — see
/// `NOTICE.md`. Its source is `flash_id.c` beside it, its disassembly is in
/// `picoboot_connection.c`: it sends the flash chip `4B` (read unique id) with
/// four dummy bytes and clocks eight bytes back into its own buffer. Thumb code
/// for the RP2040's Cortex-M0+; run by the boot ROM's EXEC, never by the host.
/// A test pins its SHA-256 to picotool's file.
pub const FLASH_ID_HELPER: [u8; 152] = [
    0x02, 0xa0, 0x06, 0xa1, 0x00, 0x4a, 0x11, 0xe0, 0x0d, 0x00, 0x00, 0x00, 0x4b, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x23, 0xf0, 0xb5,
    0x17, 0x4e, 0x9b, 0x00, 0x34, 0x68, 0x63, 0x40, 0xc0, 0x24, 0xa4, 0x00, 0x23, 0x40, 0x15, 0x4c,
    0x23, 0x60, 0xc0, 0x24, 0x13, 0x00, 0x64, 0x05, 0x17, 0x00, 0x1f, 0x43, 0x06, 0xd1, 0xc0, 0x23,
    0x32, 0x68, 0x9b, 0x00, 0x93, 0x43, 0x0f, 0x4a, 0x13, 0x60, 0xf0, 0xbd, 0x08, 0x25, 0xa7, 0x6a,
    0x3d, 0x40, 0xac, 0x46, 0x02, 0x25, 0x2f, 0x42, 0x08, 0xd0, 0x00, 0x2a, 0x06, 0xd0, 0x9f, 0x1a,
    0x0d, 0x2f, 0x03, 0xd8, 0x07, 0x78, 0x01, 0x3a, 0x27, 0x66, 0x01, 0x30, 0x65, 0x46, 0x00, 0x2d,
    0xe2, 0xd0, 0x00, 0x2b, 0xe0, 0xd0, 0x27, 0x6e, 0x01, 0x3b, 0x0f, 0x70, 0x01, 0x31, 0xdb, 0xe7,
    0x0c, 0x80, 0x01, 0x40, 0x0c, 0x90, 0x01, 0x40,
];

/// What went wrong on the USB side of one exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The endpoint stalled: the bootloader refused the command.
    Stall,
    /// No answer in time.
    Timeout,
    /// The device left.
    Gone,
    Other(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stall => f.write_str("the bootloader stalled"),
            Self::Timeout => f.write_str("no answer in time"),
            Self::Gone => f.write_str("the device left USB"),
            Self::Other(e) => f.write_str(e),
        }
    }
}

/// The transfers PICOBOOT needs from a claimed interface. The production one is
/// [`UsbLink`]; tests drive a simulated boot ROM.
pub trait Link: Send {
    /// Send `data` on the bulk OUT endpoint.
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<(), LinkError>;
    /// One transfer on the bulk IN endpoint, of at most `max` bytes: it ends
    /// at the device's first short packet.
    fn bulk_in(&mut self, max: usize, timeout: Duration) -> Result<Vec<u8>, LinkError>;
    /// A vendor request to the interface, device to host.
    fn control_in(
        &mut self,
        request: u8,
        len: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, LinkError>;
    /// A vendor request to the interface, host to device, without data.
    fn control_out(&mut self, request: u8, timeout: Duration) -> Result<(), LinkError>;
    /// Clear the halt on each bulk endpoint that has one.
    fn clear_halts(&mut self) -> Result<(), LinkError>;
}

/// Why a PICOBOOT step failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PicobootError {
    /// This process may not open USB devices: the opt-in
    /// `openfan-firmware-write` drop-in is not installed.
    NoAccess(String),
    /// No RP2040 bootloader on that USB port, or it left.
    NotFound(String),
    /// A transfer failed or timed out.
    Transfer(String),
    /// The bootloader refused a command; its status code, when it could be read.
    Refused {
        command: &'static str,
        status: Option<u32>,
    },
    /// An answer the protocol does not allow.
    Protocol(String),
}

impl fmt::Display for PicobootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAccess(e) => write!(f, "the daemon may not open USB devices ({e})"),
            Self::NotFound(e) | Self::Transfer(e) | Self::Protocol(e) => f.write_str(e),
            Self::Refused {
                command,
                status: Some(code),
            } => write!(
                f,
                "the bootloader refused {command}: {}",
                status_text(*code)
            ),
            Self::Refused {
                command,
                status: None,
            } => write!(f, "the bootloader refused {command}"),
        }
    }
}

/// A command status code in words (`enum picoboot_status`, RP2040 boot ROM).
pub fn status_text(code: u32) -> &'static str {
    match code {
        0 => "ok",
        1 => "unknown command",
        2 => "invalid command length",
        3 => "invalid transfer length",
        4 => "invalid address",
        5 => "bad alignment",
        6 => "interleaved write",
        7 => "rebooting",
        8 => "unknown error",
        _ => "an unrecognised status",
    }
}

/// The command-status request's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandStatus {
    pub token: u32,
    pub code: u32,
    pub command: u8,
    pub in_progress: bool,
}

/// A command's data phase.
enum Data<'a> {
    None,
    Out(&'a [u8]),
    In(u32),
}

/// Which part of an exchange failed.
struct Failed {
    phase: &'static str,
    error: LinkError,
}

/// PICOBOOT on one claimed interface.
pub struct Client {
    link: Box<dyn Link>,
    token: u32,
}

/// Opens the PICOBOOT interface of the bootloader on a USB port (`8-8`).
pub type Opener = Arc<dyn Fn(&str) -> Result<Client, PicobootError> + Send + Sync>;

fn le(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

impl Client {
    pub fn new(link: Box<dyn Link>) -> Self {
        Self { link, token: 0 }
    }

    /// Clear any halt, then reset the interface: a command in progress is
    /// aborted and exclusive access dropped. picotool's first step, and its
    /// way out of a refused command.
    pub fn reset(&mut self) -> Result<(), PicobootError> {
        let transfer = |what: &str, e: LinkError| PicobootError::Transfer(format!("{what}: {e}"));
        self.link
            .clear_halts()
            .map_err(|e| transfer("clearing a stalled endpoint", e))?;
        self.link
            .control_out(IF_RESET, CONTROL_TIMEOUT)
            .map_err(|e| transfer("resetting the PICOBOOT interface", e))
    }

    /// Whether the drive may write meanwhile: [`EXCLUSIVE`] or [`NOT_EXCLUSIVE`].
    pub fn exclusive_access(&mut self, mode: u8) -> Result<(), PicobootError> {
        self.command(
            CMD_EXCLUSIVE_ACCESS,
            "EXCLUSIVE_ACCESS",
            &[mode],
            1,
            Data::None,
        )
        .map(drop)
    }

    /// Leave execute-in-place, so the flash takes commands.
    pub fn exit_xip(&mut self) -> Result<(), PicobootError> {
        self.command(CMD_EXIT_XIP, "EXIT_XIP", &[], 0, Data::None)
            .map(drop)
    }

    /// Erase `len` bytes from `addr`, both whole sectors.
    pub fn erase(&mut self, addr: u32, len: u32) -> Result<(), PicobootError> {
        let args = [le(addr), le(len)].concat();
        self.command(CMD_FLASH_ERASE, "FLASH_ERASE", &args, 8, Data::None)
            .map(drop)
    }

    /// Write `data` at `addr`: flash (whole pages, already erased) or RAM.
    pub fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), PicobootError> {
        let args = [le(addr), le(data.len() as u32)].concat();
        self.command(CMD_WRITE, "WRITE", &args, 8, Data::Out(data))
            .map(drop)
    }

    /// Read `len` bytes from `addr`.
    pub fn read(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, PicobootError> {
        let args = [le(addr), le(len)].concat();
        self.command(CMD_READ, "READ", &args, 8, Data::In(len))
    }

    /// Run the code at `addr` in RAM, and come back when it returns.
    pub fn exec(&mut self, addr: u32) -> Result<(), PicobootError> {
        self.command(CMD_EXEC, "EXEC", &le(addr), 4, Data::None)
            .map(drop)
    }

    /// Restart into the flash image, half a second after the acknowledgement.
    pub fn reboot(&mut self) -> Result<(), PicobootError> {
        let args = [le(REBOOT_PC), le(REBOOT_SP), le(REBOOT_DELAY_MS)].concat();
        self.command(CMD_REBOOT, "REBOOT", &args, 12, Data::None)
            .map(drop)
    }

    /// The flash chip's 64-bit unique id, as picotool reads it for `--ser`:
    /// load [`FLASH_ID_HELPER`], run it, read its result. Needs XIP off —
    /// call [`Self::exit_xip`] first.
    pub fn flash_id(&mut self) -> Result<[u8; 8], PicobootError> {
        self.write(FLASH_ID_CODE_ADDR, &FLASH_ID_HELPER)?;
        self.exec(FLASH_ID_CODE_ADDR)?;
        let raw = self.read(FLASH_ID_RESULT_ADDR, 8)?;
        raw.try_into()
            .map_err(|_| PicobootError::Protocol("the flash id read back short".into()))
    }

    /// The last command's status.
    pub fn status(&mut self) -> Result<CommandStatus, PicobootError> {
        let raw = self
            .link
            .control_in(IF_CMD_STATUS, STATUS_LEN, CONTROL_TIMEOUT)
            .map_err(|e| PicobootError::Transfer(format!("reading the command status: {e}")))?;
        if raw.len() != STATUS_LEN as usize {
            return Err(PicobootError::Protocol(format!(
                "the command status is {} bytes, not {STATUS_LEN}",
                raw.len()
            )));
        }
        let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        Ok(CommandStatus {
            token: word(0),
            code: word(4),
            command: raw[8],
            in_progress: raw[9] != 0,
        })
    }

    fn command(
        &mut self,
        id: u8,
        name: &'static str,
        args: &[u8],
        arg_size: u8,
        data: Data,
    ) -> Result<Vec<u8>, PicobootError> {
        self.token = self.token.wrapping_add(1);
        let transfer_len = match data {
            Data::None => 0,
            Data::Out(d) => d.len() as u32,
            Data::In(n) => n,
        };
        let mut cmd = [0u8; 32];
        cmd[0..4].copy_from_slice(&le(MAGIC));
        cmd[4..8].copy_from_slice(&le(self.token));
        cmd[8] = id;
        cmd[9] = arg_size;
        cmd[12..16].copy_from_slice(&le(transfer_len));
        cmd[16..16 + args.len()].copy_from_slice(args);
        match self.exchange(&cmd, id, data, transfer_len) {
            Ok(received) => Ok(received),
            Err(Failed {
                error: LinkError::Stall,
                ..
            }) => Err(PicobootError::Refused {
                command: name,
                status: self.status().ok().map(|s| s.code),
            }),
            Err(Failed { phase, error }) => {
                Err(PicobootError::Transfer(format!("{name}: {phase}: {error}")))
            }
        }
    }

    fn exchange(
        &mut self,
        cmd: &[u8; 32],
        id: u8,
        data: Data,
        transfer_len: u32,
    ) -> Result<Vec<u8>, Failed> {
        let at = |phase: &'static str| move |error| Failed { phase, error };
        self.link
            .bulk_out(cmd, COMMAND_TIMEOUT)
            .map_err(at("sending the command"))?;
        let mut received = Vec::new();
        match data {
            Data::None => {}
            Data::Out(d) => self
                .link
                .bulk_out(d, DATA_TIMEOUT)
                .map_err(at("sending its data"))?,
            Data::In(n) => {
                received = self
                    .link
                    .bulk_in(n as usize, DATA_TIMEOUT)
                    .map_err(at("receiving its data"))?;
                if received.len() != n as usize {
                    return Err(Failed {
                        phase: "receiving its data",
                        error: LinkError::Other(format!(
                            "{} bytes came back, not {n}",
                            received.len()
                        )),
                    });
                }
            }
        }
        // The acknowledgement goes the other way to the data. A command with
        // no data does its work first, so it gets the longer limit.
        let ack_timeout = if transfer_len == 0 {
            DATA_TIMEOUT
        } else {
            COMMAND_TIMEOUT
        };
        if id & DIR_IN != 0 {
            self.link
                .bulk_out(&[0], ack_timeout)
                .map_err(at("acknowledging"))?;
        } else {
            let ack = self
                .link
                .bulk_in(1, ack_timeout)
                .map_err(at("waiting for the acknowledgement"))?;
            if !ack.is_empty() {
                return Err(Failed {
                    phase: "waiting for the acknowledgement",
                    error: LinkError::Other(format!("{} bytes instead of none", ack.len())),
                });
            }
        }
        Ok(received)
    }
}

/// Erase `sector`, then program each of its runs.
pub fn write_sector(client: &mut Client, sector: &uf2::Sector) -> Result<(), PicobootError> {
    client.erase(sector.base, uf2::SECTOR_SIZE)?;
    for run in &sector.runs {
        client.write(run.addr, &run.data)?;
    }
    Ok(())
}

/// Read `sector`'s runs back: `None` when every byte is the image's, else the
/// first address that is not.
pub fn verify_sector(
    client: &mut Client,
    sector: &uf2::Sector,
) -> Result<Option<u32>, PicobootError> {
    for run in &sector.runs {
        let got = client.read(run.addr, run.data.len() as u32)?;
        if let Some(i) = got.iter().zip(&run.data).position(|(a, b)| a != b) {
            return Ok(Some(run.addr + i as u32));
        }
    }
    Ok(None)
}

/// A flash id as the OpenFan firmware shows it for its USB serial
/// (`usbd_serial_init`: `%02X` for each byte, in order).
pub fn serial_of(id: &[u8; 8]) -> String {
    id.iter().map(|b| format!("{b:02X}")).collect()
}

/// Whether `id` is the flash chip of the board whose normal-mode USB serial is
/// `serial`: the identity check before anything is erased.
pub fn id_matches_serial(id: &[u8; 8], serial: &str) -> bool {
    serial.eq_ignore_ascii_case(&serial_of(id))
}

// ── The production link: usbfs through nusb ──────────────────────────

/// The PICOBOOT interface of an RP2040 bootloader, through usbfs. Holds the
/// interface claim until dropped.
pub struct UsbLink {
    interface: nusb::Interface,
    out_ep: nusb::Endpoint<nusb::transfer::Bulk, nusb::transfer::Out>,
    in_ep: nusb::Endpoint<nusb::transfer::Bulk, nusb::transfer::In>,
    out_addr: u8,
    in_addr: u8,
    in_packet: usize,
    number: u8,
    _device: nusb::Device,
}

fn link_error(e: nusb::transfer::TransferError) -> LinkError {
    use nusb::transfer::TransferError as T;
    match e {
        T::Stall => LinkError::Stall,
        // `transfer_blocking` cancels a transfer that ran out of time.
        T::Cancelled => LinkError::Timeout,
        T::Disconnected => LinkError::Gone,
        other => LinkError::Other(other.to_string()),
    }
}

fn open_error(port: &str, e: nusb::Error) -> PicobootError {
    match e.kind() {
        nusb::ErrorKind::PermissionDenied => PicobootError::NoAccess(e.to_string()),
        nusb::ErrorKind::Disconnected | nusb::ErrorKind::NotFound => {
            PicobootError::NotFound(format!("the bootloader on USB port {port} left ({e})"))
        }
        nusb::ErrorKind::Busy => PicobootError::Transfer(format!(
            "the bootloader's PICOBOOT interface is in use by another program ({e})"
        )),
        _ => PicobootError::Transfer(format!("opening the bootloader on USB port {port}: {e}")),
    }
}

/// Open the PICOBOOT interface of the RP2040 bootloader at USB `port` (the
/// sysfs device name, `8-8`) — a `2e8a:0003` device on that port, and nothing
/// else. The bootloader's own USB serial is the same on every board, so the
/// port is what names it; [`Client::flash_id`] then says whose flash it is.
pub fn open_bootloader(port: &str) -> Result<Client, PicobootError> {
    use nusb::descriptors::TransferType;
    use nusb::transfer::{Bulk, Direction, In, Out};
    use nusb::MaybeFuture;

    let listed = nusb::list_devices()
        .wait()
        .map_err(|e| PicobootError::Transfer(format!("listing USB devices: {e}")))?;
    let info = listed
        .into_iter()
        .find(|d| d.sysfs_path().file_name().and_then(|n| n.to_str()) == Some(port))
        .ok_or_else(|| PicobootError::NotFound(format!("no USB device on port {port}")))?;
    if (info.vendor_id(), info.product_id()) != (RP2040_VENDOR_ID, RP2040_BOOTLOADER_PRODUCT_ID) {
        return Err(PicobootError::NotFound(format!(
            "the device on USB port {port} is not an RP2040 bootloader"
        )));
    }
    let number = info
        .interfaces()
        .find(|i| i.class() == 0xff)
        .map(|i| i.interface_number())
        .ok_or_else(|| {
            PicobootError::Protocol(format!(
                "the bootloader on USB port {port} shows no PICOBOOT interface"
            ))
        })?;
    let device = info.open().wait().map_err(|e| open_error(port, e))?;
    let descriptor = device.device_descriptor();
    if (descriptor.vendor_id(), descriptor.product_id())
        != (RP2040_VENDOR_ID, RP2040_BOOTLOADER_PRODUCT_ID)
    {
        return Err(PicobootError::NotFound(format!(
            "the device on USB port {port} changed while it was opened"
        )));
    }
    let interface = device
        .claim_interface(number)
        .wait()
        .map_err(|e| open_error(port, e))?;
    let (out_addr, in_addr, in_packet) = {
        let alt = interface.descriptor().ok_or_else(|| {
            PicobootError::Protocol("the PICOBOOT interface has no descriptor".into())
        })?;
        let bulk = |dir: Direction| {
            alt.endpoints()
                .find(|e| e.transfer_type() == TransferType::Bulk && e.direction() == dir)
        };
        let (Some(out), Some(inn)) = (bulk(Direction::Out), bulk(Direction::In)) else {
            return Err(PicobootError::Protocol(
                "the PICOBOOT interface lacks a bulk endpoint".into(),
            ));
        };
        (out.address(), inn.address(), inn.max_packet_size().max(1))
    };
    let endpoint_error =
        |e: nusb::Error| PicobootError::Protocol(format!("opening a PICOBOOT endpoint: {e}"));
    let out_ep = interface
        .endpoint::<Bulk, Out>(out_addr)
        .map_err(endpoint_error)?;
    let in_ep = interface
        .endpoint::<Bulk, In>(in_addr)
        .map_err(endpoint_error)?;
    Ok(Client::new(Box::new(UsbLink {
        interface,
        out_ep,
        in_ep,
        out_addr,
        in_addr,
        in_packet,
        number,
        _device: device,
    })))
}

/// The production [`Opener`].
pub fn usb_opener() -> Opener {
    Arc::new(open_bootloader)
}

impl Link for UsbLink {
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<(), LinkError> {
        let mut buf = nusb::transfer::Buffer::new(data.len());
        buf.extend_from_slice(data);
        let done = self.out_ep.transfer_blocking(buf, timeout);
        done.status.map_err(link_error)?;
        if done.actual_len != data.len() {
            return Err(LinkError::Other(format!(
                "{} of {} bytes went out",
                done.actual_len,
                data.len()
            )));
        }
        Ok(())
    }

    fn bulk_in(&mut self, max: usize, timeout: Duration) -> Result<Vec<u8>, LinkError> {
        // usbfs takes an IN transfer in whole packets; the device ends it short.
        let want = max.div_ceil(self.in_packet).max(1) * self.in_packet;
        let done = self
            .in_ep
            .transfer_blocking(nusb::transfer::Buffer::new(want), timeout);
        done.status.map_err(link_error)?;
        Ok(done.buffer.into_vec())
    }

    fn control_in(
        &mut self,
        request: u8,
        len: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, LinkError> {
        use nusb::transfer::{ControlIn, ControlType, Recipient};
        use nusb::MaybeFuture;
        self.interface
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Interface,
                    request,
                    value: 0,
                    index: u16::from(self.number),
                    length: len,
                },
                timeout,
            )
            .wait()
            .map_err(link_error)
    }

    fn control_out(&mut self, request: u8, timeout: Duration) -> Result<(), LinkError> {
        use nusb::transfer::{ControlOut, ControlType, Recipient};
        use nusb::MaybeFuture;
        self.interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Interface,
                    request,
                    value: 0,
                    index: u16::from(self.number),
                    data: &[],
                },
                timeout,
            )
            .wait()
            .map_err(link_error)
    }

    fn clear_halts(&mut self) -> Result<(), LinkError> {
        use nusb::transfer::{ControlIn, ControlType, Recipient};
        use nusb::MaybeFuture;
        // GET_STATUS on each endpoint, and CLEAR_FEATURE only where halted —
        // picotool's order.
        for addr in [self.in_addr, self.out_addr] {
            let status = self
                .interface
                .control_in(
                    ControlIn {
                        control_type: ControlType::Standard,
                        recipient: Recipient::Endpoint,
                        request: 0x00,
                        value: 0,
                        index: u16::from(addr),
                        length: 2,
                    },
                    CONTROL_TIMEOUT,
                )
                .wait()
                .map_err(link_error)?;
            if status.first().is_some_and(|b| b & 1 == 1) {
                let cleared = if addr == self.in_addr {
                    self.in_ep.clear_halt().wait()
                } else {
                    self.out_ep.clear_halt().wait()
                };
                cleared.map_err(|e| LinkError::Other(e.to_string()))?;
            }
        }
        Ok(())
    }
}

/// Where usbfs puts the USB device nodes.
pub const USB_DEV_ROOT: &str = "/dev/bus/usb";

/// Whether this process may open USB devices read-write — what the opt-in
/// `openfan-firmware-write` drop-in (`DeviceAllow=char-usb_device rw`) grants.
///
/// Asks `access(2)` about each bus's root hub (`BBB/001`: a root hub is always
/// device 1 on its bus) under `dev_root`. Nothing is opened, so no device is
/// woken or touched. The kernel answers `access` through the same device
/// cgroup check as `open`, and that check refuses every USB node alike, so a
/// root hub answers for the board. `false` when there is no USB node to ask
/// about. A wrong answer costs nothing: an open the cgroup refuses makes the
/// update fall back to the copy by hand.
pub fn usb_access(dev_root: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut hubs: Vec<std::path::PathBuf> = std::fs::read_dir(dev_root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|bus| bus.path().join("001"))
        .collect();
    hubs.sort();
    hubs.iter().any(|hub| {
        let Ok(path) = std::ffi::CString::new(hub.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
        unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
    })
}

#[cfg(test)]
pub(crate) mod fake {
    //! An RP2040 boot ROM's PICOBOOT interface, on the far side of a [`Link`]:
    //! the datasheet's command rules, NOR flash that programs only 1 → 0, the
    //! 16 MiB address window that wraps onto the 4 MiB chip, the XIP SRAM the
    //! flash-ID helper runs from, and the faults a test asks for.
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    /// The flash addresses the ROM takes; past the chip they wrap.
    pub const FLASH_WINDOW: u32 = 0x0100_0000;
    const XIP_SRAM: (u32, u32) = (0x1500_0000, 0x1500_4000);
    const SRAM: (u32, u32) = (0x2000_0000, 0x2004_2000);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Phase {
        Idle,
        DataOut,
        DataIn,
        AckIn,
        AckOut,
    }

    pub struct Rom {
        pub flash: Vec<u8>,
        xip_sram: Vec<u8>,
        sram: Vec<u8>,
        /// The chip's unique id, which the helper reads.
        pub uid: [u8; 8],
        pub exclusive: u8,
        /// Execute-in-place is on, as the ROM leaves it, until EXIT_XIP.
        pub xip: bool,
        halted: bool,
        phase: Phase,
        /// The command whose data or acknowledgement is due.
        pending: Option<(u8, u32, u32, Vec<u8>)>,
        status: (u32, u32, u8),
        /// Every command run, and every reset, in order.
        pub log: Vec<String>,
        pub rebooted: bool,
        /// The `n`th command with this id (1-based) fails with this error
        /// before it reaches the ROM.
        pub fail: Option<(u8, usize, LinkError)>,
        seen: HashMap<u8, usize>,
        /// Every page programmed with its first byte flipped.
        pub corrupt_writes: bool,
        /// The chip restarts when a REBOOT is acknowledged; when `false` it
        /// stays in its bootloader, answering.
        pub restarts: bool,
        /// Run, without the ROM's lock, each time a REBOOT is acknowledged
        /// and the chip restarts.
        pub on_reboot: Option<Box<dyn FnMut() + Send>>,
        /// Run with each command's id and how many of that id have arrived,
        /// before the ROM acts on it — under the ROM's lock.
        pub on_command: Option<Box<dyn FnMut(u8, usize) + Send>>,
    }

    impl Rom {
        pub fn new(uid: [u8; 8]) -> Arc<Mutex<Rom>> {
            Arc::new(Mutex::new(Rom {
                flash: vec![0xff; uf2::FLASH_SIZE as usize],
                xip_sram: vec![0; (XIP_SRAM.1 - XIP_SRAM.0) as usize],
                sram: vec![0; (SRAM.1 - SRAM.0) as usize],
                uid,
                exclusive: NOT_EXCLUSIVE,
                xip: true,
                halted: false,
                phase: Phase::Idle,
                pending: None,
                status: (0, 0, 0),
                log: Vec::new(),
                rebooted: false,
                fail: None,
                seen: HashMap::new(),
                corrupt_writes: false,
                restarts: true,
                on_reboot: None,
                on_command: None,
            }))
        }

        /// A fresh bootloader session on the same flash, as after a reset
        /// into the bootloader.
        pub fn power_on(&mut self) {
            self.exclusive = NOT_EXCLUSIVE;
            self.xip = true;
            self.halted = false;
            self.phase = Phase::Idle;
            self.pending = None;
            self.status = (0, 0, 0);
            self.rebooted = false;
        }

        /// A client talking to `rom`.
        pub fn client(rom: &Arc<Mutex<Rom>>) -> Client {
            Client::new(Box::new(FakeLink(rom.clone())))
        }

        /// The flash bytes at `addr`, as a READ would see them.
        pub fn flash_at(&self, addr: u32, len: usize) -> Vec<u8> {
            (0..len as u32)
                .map(|i| self.flash[Self::flash_index(addr + i).expect("in the window")])
                .collect()
        }

        /// The log entries that start with `prefix`.
        pub fn logged(&self, prefix: &str) -> Vec<String> {
            self.log
                .iter()
                .filter(|l| l.starts_with(prefix))
                .cloned()
                .collect()
        }

        fn flash_index(addr: u32) -> Option<usize> {
            let off = addr.checked_sub(uf2::FLASH_BASE)?;
            (off < FLASH_WINDOW).then_some((off % uf2::FLASH_SIZE) as usize)
        }

        fn ram(&mut self, addr: u32, len: u32) -> Option<&mut [u8]> {
            let end = addr.checked_add(len)?;
            for (range, mem) in [(XIP_SRAM, &mut self.xip_sram), (SRAM, &mut self.sram)] {
                if addr >= range.0 && end <= range.1 {
                    let at = (addr - range.0) as usize;
                    return Some(&mut mem[at..at + len as usize]);
                }
            }
            None
        }

        /// Refuse: the endpoints stall until a reset or a clear.
        fn refuse(&mut self, token: u32, id: u8, code: u32) {
            self.halted = true;
            self.phase = Phase::Idle;
            self.pending = None;
            self.status = (token, code, id);
        }

        fn command(&mut self, raw: &[u8]) -> Result<(), LinkError> {
            let word =
                |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
            if raw.len() != 32 || word(0) != MAGIC {
                self.refuse(0, 0, 1);
                return Ok(());
            }
            let (token, id, size, len) = (word(4), raw[8], raw[9], word(12));
            let n = self.seen.entry(id).or_default();
            *n += 1;
            let nth = *n;
            if let Some(hook) = self.on_command.as_mut() {
                hook(id, nth);
            }
            if let Some((fail_id, fail_nth, err)) = &self.fail {
                if *fail_id == id && *fail_nth == nth {
                    return Err(err.clone());
                }
            }
            let expected = match id {
                CMD_EXCLUSIVE_ACCESS => 1,
                CMD_REBOOT => 12,
                CMD_FLASH_ERASE | CMD_READ | CMD_WRITE => 8,
                CMD_EXIT_XIP => 0,
                CMD_EXEC => 4,
                _ => {
                    self.refuse(token, id, 1);
                    return Ok(());
                }
            };
            if size != expected {
                self.refuse(token, id, 2);
                return Ok(());
            }
            let (addr, span) = (word(16), word(20));
            let wants_data = matches!(id, CMD_READ | CMD_WRITE);
            if (wants_data && len != span) || (!wants_data && len != 0) {
                self.refuse(token, id, 3);
                return Ok(());
            }
            self.status = (token, 0, id);
            match id {
                CMD_EXCLUSIVE_ACCESS => {
                    if raw[16] > 2 {
                        self.refuse(token, id, 8);
                        return Ok(());
                    }
                    self.exclusive = raw[16];
                    self.log.push(format!("EXCLUSIVE_ACCESS {}", raw[16]));
                }
                CMD_EXIT_XIP => {
                    self.xip = false;
                    self.log.push("EXIT_XIP".into());
                }
                CMD_FLASH_ERASE => {
                    let Some(start) = Self::flash_index(addr) else {
                        self.refuse(token, id, 4);
                        return Ok(());
                    };
                    if addr % uf2::SECTOR_SIZE != 0 || span % uf2::SECTOR_SIZE != 0 {
                        self.refuse(token, id, 5);
                        return Ok(());
                    }
                    if self.xip {
                        self.refuse(token, id, 8);
                        return Ok(());
                    }
                    for i in 0..span as usize {
                        let at = (start + i) % uf2::FLASH_SIZE as usize;
                        self.flash[at] = 0xff;
                    }
                    self.log.push(format!("FLASH_ERASE {addr:#010x}+{span:#x}"));
                }
                CMD_WRITE => {
                    if Self::flash_index(addr).is_some() {
                        if addr % 256 != 0 || span % 256 != 0 {
                            self.refuse(token, id, 5);
                            return Ok(());
                        }
                        if self.xip {
                            self.refuse(token, id, 8);
                            return Ok(());
                        }
                    } else if self.ram(addr, span).is_none() {
                        self.refuse(token, id, 4);
                        return Ok(());
                    }
                    self.pending = Some((id, addr, span, Vec::new()));
                    self.phase = Phase::DataOut;
                    return Ok(());
                }
                CMD_READ => {
                    // Flash reads work in or out of XIP, as on the chip.
                    let data = if Self::flash_index(addr).is_some() {
                        (0..span)
                            .map(|i| Self::flash_index(addr + i).map_or(0, |at| self.flash[at]))
                            .collect()
                    } else if let Some(mem) = self.ram(addr, span) {
                        mem.to_vec()
                    } else {
                        self.refuse(token, id, 4);
                        return Ok(());
                    };
                    self.log.push(format!("READ {addr:#010x}+{span:#x}"));
                    self.pending = Some((id, addr, span, data));
                    self.phase = Phase::DataIn;
                    return Ok(());
                }
                CMD_EXEC => {
                    let known = self
                        .ram(addr, FLASH_ID_HELPER.len() as u32)
                        .is_some_and(|code| code == FLASH_ID_HELPER);
                    if known && !self.xip {
                        let uid = self.uid;
                        if let Some(rx) = self.ram(addr + 28, 13) {
                            rx[..5].fill(0xff);
                            rx[5..].copy_from_slice(&uid);
                        }
                    }
                    self.log.push(format!(
                        "EXEC {addr:#010x}{}",
                        if known { " flash-id" } else { "" }
                    ));
                }
                CMD_REBOOT => {
                    self.log.push("REBOOT".into());
                    self.pending = Some((id, 0, 0, Vec::new()));
                }
                _ => unreachable!("refused above"),
            }
            self.phase = Phase::AckIn;
            Ok(())
        }

        fn data_out(&mut self, data: &[u8]) {
            let Some((id, addr, span, mut got)) = self.pending.take() else {
                return;
            };
            got.extend_from_slice(data);
            if got.len() > span as usize {
                self.refuse(self.status.0, id, 3);
                return;
            }
            if got.len() < span as usize {
                self.pending = Some((id, addr, span, got));
                return;
            }
            if Self::flash_index(addr).is_some() {
                for (i, b) in got.iter().enumerate() {
                    let flipped = self.corrupt_writes && i % 256 == 0;
                    let at = Self::flash_index(addr + i as u32).expect("checked above");
                    self.flash[at] &= if flipped { !*b } else { *b };
                }
            } else if let Some(mem) = self.ram(addr, span) {
                mem.copy_from_slice(&got);
            }
            self.log.push(format!("WRITE {addr:#010x}+{span:#x}"));
            self.phase = Phase::AckIn;
        }
    }

    pub struct FakeLink(pub Arc<Mutex<Rom>>);

    impl Link for FakeLink {
        fn bulk_out(&mut self, data: &[u8], _: Duration) -> Result<(), LinkError> {
            let mut rom = self.0.lock();
            if rom.rebooted {
                return Err(LinkError::Gone);
            }
            if rom.halted {
                return Err(LinkError::Stall);
            }
            match rom.phase {
                Phase::Idle => rom.command(data),
                Phase::DataOut => {
                    rom.data_out(data);
                    Ok(())
                }
                Phase::AckOut => {
                    rom.phase = Phase::Idle;
                    rom.pending = None;
                    Ok(())
                }
                // The host sent while the device had something to send.
                Phase::DataIn | Phase::AckIn => {
                    let (token, _, id) = rom.status;
                    rom.refuse(token, id, 8);
                    Err(LinkError::Stall)
                }
            }
        }

        fn bulk_in(&mut self, max: usize, _: Duration) -> Result<Vec<u8>, LinkError> {
            let mut rom = self.0.lock();
            if rom.rebooted {
                return Err(LinkError::Gone);
            }
            if rom.halted {
                return Err(LinkError::Stall);
            }
            match rom.phase {
                Phase::DataIn => {
                    let (id, addr, span, data) = rom.pending.take().expect("data is due");
                    if data.len() > max {
                        rom.pending = Some((id, addr, span, data));
                        return Err(LinkError::Other("babble".into()));
                    }
                    rom.phase = Phase::AckOut;
                    Ok(data)
                }
                Phase::AckIn => {
                    rom.phase = Phase::Idle;
                    let reboot = matches!(rom.pending.take(), Some((CMD_REBOOT, ..)));
                    if reboot && rom.restarts {
                        rom.rebooted = true;
                        let hook = rom.on_reboot.take();
                        drop(rom);
                        if let Some(mut hook) = hook {
                            hook();
                            self.0.lock().on_reboot.get_or_insert(hook);
                        }
                    }
                    Ok(Vec::new())
                }
                // Nothing to send: the host waits out its timeout.
                Phase::Idle | Phase::DataOut | Phase::AckOut => Err(LinkError::Timeout),
            }
        }

        fn control_in(&mut self, request: u8, len: u16, _: Duration) -> Result<Vec<u8>, LinkError> {
            let rom = self.0.lock();
            if rom.rebooted {
                return Err(LinkError::Gone);
            }
            if request != IF_CMD_STATUS || len != STATUS_LEN {
                return Err(LinkError::Stall);
            }
            let (token, code, id) = rom.status;
            let mut out = Vec::with_capacity(16);
            out.extend_from_slice(&token.to_le_bytes());
            out.extend_from_slice(&code.to_le_bytes());
            out.extend_from_slice(&[id, 0, 0, 0, 0, 0, 0, 0]);
            Ok(out)
        }

        fn control_out(&mut self, request: u8, _: Duration) -> Result<(), LinkError> {
            let mut rom = self.0.lock();
            if rom.rebooted {
                return Err(LinkError::Gone);
            }
            if request != IF_RESET {
                return Err(LinkError::Stall);
            }
            rom.halted = false;
            rom.phase = Phase::Idle;
            rom.pending = None;
            rom.exclusive = NOT_EXCLUSIVE;
            rom.status = (0, 0, 0);
            rom.log.push("RESET".into());
            Ok(())
        }

        fn clear_halts(&mut self) -> Result<(), LinkError> {
            let mut rom = self.0.lock();
            if rom.rebooted {
                return Err(LinkError::Gone);
            }
            rom.halted = false;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Rom;
    use super::*;
    use crate::serial::uf2::fixture::image_file;
    use crate::serial::uf2::Image;

    const UID: [u8; 8] = [0xde, 0x61, 0x5c, 0xb1, 0x47, 0x21, 0x49, 0x2c];

    /// picotool 2.3.1's `picoboot_flash_id/flash_id.bin`, by its SHA-256.
    #[test]
    fn the_flash_id_helper_is_picotools_file_byte_for_byte() {
        use sha2::Digest;
        let digest: String = sha2::Sha256::digest(FLASH_ID_HELPER)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            digest,
            "0c598d8a4dc02ede332a65f96aff27a410fd65d8aaa9fa6dc971539c720725b8"
        );
    }

    #[test]
    fn the_flash_id_reads_as_the_firmwares_usb_serial() {
        let rom = Rom::new(UID);
        let mut c = Rom::client(&rom);
        c.reset().unwrap();
        c.exclusive_access(EXCLUSIVE).unwrap();
        c.exit_xip().unwrap();
        let id = c.flash_id().expect("the id");
        assert_eq!(id, UID);
        assert_eq!(
            serial_of(&id),
            "DE615CB14721492C",
            "the serial the board shows in normal mode"
        );
        assert!(id_matches_serial(&id, "DE615CB14721492C"));
        assert!(id_matches_serial(&id, "de615cb14721492c"));
        assert!(!id_matches_serial(&id, "DE615CB14721492D"));
        assert!(
            !id_matches_serial(&id, "E0C9125B0D9B"),
            "the bootloader's own serial"
        );
        assert_eq!(
            rom.lock().logged("EXEC"),
            ["EXEC 0x15000000 flash-id"],
            "the helper ran"
        );
    }

    #[test]
    fn an_image_is_erased_written_and_read_back_sector_by_sector() {
        let rom = Rom::new(UID);
        // Old firmware everywhere the image goes, and past it.
        rom.lock().flash[..3 * uf2::SECTOR_SIZE as usize].fill(0x5a);
        let image = Image::parse(&image_file(20, 3)).unwrap();
        let mut c = Rom::client(&rom);
        c.reset().unwrap();
        c.exclusive_access(EXCLUSIVE).unwrap();
        c.exit_xip().unwrap();
        for sector in image.sectors() {
            write_sector(&mut c, &sector).expect("written");
        }
        for sector in image.sectors() {
            assert_eq!(verify_sector(&mut c, &sector).expect("read"), None);
        }
        c.reboot().unwrap();
        let rom = rom.lock();
        assert_eq!(
            rom.logged("FLASH_ERASE"),
            [
                "FLASH_ERASE 0x10000000+0x1000",
                "FLASH_ERASE 0x10001000+0x1000"
            ],
            "only the two sectors the image touches"
        );
        assert_eq!(rom.flash_at(uf2::FLASH_BASE, 1), [3]);
        assert_eq!(
            rom.flash_at(uf2::FLASH_BASE + 20 * 256, 1),
            [0xff],
            "the rest of a touched sector is erased, as a copy leaves it"
        );
        assert_eq!(
            rom.flash_at(uf2::FLASH_BASE + 2 * uf2::SECTOR_SIZE, 1),
            [0x5a],
            "an untouched sector is left alone"
        );
        assert!(rom.rebooted);
    }

    #[test]
    fn a_read_back_that_differs_is_found() {
        let rom = Rom::new(UID);
        rom.lock().corrupt_writes = true;
        let image = Image::parse(&image_file(2, 1)).unwrap();
        let mut c = Rom::client(&rom);
        c.exit_xip().unwrap();
        let sector = &image.sectors()[0];
        write_sector(&mut c, sector).unwrap();
        assert_eq!(
            verify_sector(&mut c, sector).unwrap(),
            Some(uf2::FLASH_BASE)
        );
    }

    #[test]
    fn writing_over_unerased_flash_reads_back_wrong_as_nor_flash_does() {
        // The guard behind erasing first: programming only clears bits.
        let rom = Rom::new(UID);
        rom.lock().flash[..256].fill(0x0f);
        let mut c = Rom::client(&rom);
        c.exit_xip().unwrap();
        c.write(uf2::FLASH_BASE, &[0xf0; 256]).unwrap();
        assert_eq!(c.read(uf2::FLASH_BASE, 1).unwrap(), [0x00]);
    }

    #[test]
    fn a_refused_command_reports_the_roms_status_and_a_reset_clears_it() {
        let rom = Rom::new(UID);
        let mut c = Rom::client(&rom);
        c.exit_xip().unwrap();
        let err = c.erase(uf2::FLASH_BASE + 0x100, 0x1000).unwrap_err();
        assert_eq!(
            err,
            PicobootError::Refused {
                command: "FLASH_ERASE",
                status: Some(5)
            }
        );
        assert!(err.to_string().contains("bad alignment"), "{err}");
        assert!(
            c.exit_xip().is_err(),
            "stalled until the interface is reset"
        );
        c.reset().unwrap();
        c.exit_xip().expect("after the reset");
    }

    #[test]
    fn flash_commands_need_xip_off_and_a_reset_drops_exclusive_access() {
        let rom = Rom::new(UID);
        let mut c = Rom::client(&rom);
        c.exclusive_access(EXCLUSIVE).unwrap();
        assert_eq!(rom.lock().exclusive, EXCLUSIVE);
        assert!(matches!(
            c.erase(uf2::FLASH_BASE, 0x1000),
            Err(PicobootError::Refused { .. })
        ));
        c.reset().unwrap();
        assert_eq!(rom.lock().exclusive, NOT_EXCLUSIVE);
    }

    #[test]
    fn a_transfer_that_fails_is_a_transfer_error_not_a_refusal() {
        let rom = Rom::new(UID);
        rom.lock().fail = Some((CMD_FLASH_ERASE, 1, LinkError::Timeout));
        let mut c = Rom::client(&rom);
        c.exit_xip().unwrap();
        let err = c.erase(uf2::FLASH_BASE, 0x1000).unwrap_err();
        assert!(
            matches!(&err, PicobootError::Transfer(m) if m.contains("FLASH_ERASE")),
            "{err:?}"
        );
    }

    #[test]
    fn after_the_reboot_the_device_is_gone() {
        let rom = Rom::new(UID);
        let mut c = Rom::client(&rom);
        c.reboot().unwrap();
        assert!(matches!(
            c.exit_xip(),
            Err(PicobootError::Transfer(m)) if m.contains("left USB")
        ));
    }

    #[test]
    fn the_command_block_is_laid_out_as_the_datasheet_says() {
        /// Records what goes out, and acknowledges everything.
        struct Tap(Arc<parking_lot::Mutex<Vec<Vec<u8>>>>);
        impl Link for Tap {
            fn bulk_out(&mut self, d: &[u8], _: Duration) -> Result<(), LinkError> {
                self.0.lock().push(d.to_vec());
                Ok(())
            }
            fn bulk_in(&mut self, _: usize, _: Duration) -> Result<Vec<u8>, LinkError> {
                Ok(Vec::new())
            }
            fn control_in(&mut self, _: u8, _: u16, _: Duration) -> Result<Vec<u8>, LinkError> {
                Err(LinkError::Stall)
            }
            fn control_out(&mut self, _: u8, _: Duration) -> Result<(), LinkError> {
                Ok(())
            }
            fn clear_halts(&mut self) -> Result<(), LinkError> {
                Ok(())
            }
        }
        let sent = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut c = Client::new(Box::new(Tap(sent.clone())));
        c.erase(0x1000_2000, 0x1000).unwrap();
        c.write(0x1000_0100, &[7; 256]).unwrap();
        let sent = sent.lock();
        let erase = &sent[0];
        assert_eq!(erase.len(), 32);
        assert_eq!(erase[0..4], MAGIC.to_le_bytes());
        assert_eq!(erase[4..8], 1u32.to_le_bytes(), "token");
        assert_eq!((erase[8], erase[9]), (CMD_FLASH_ERASE, 8));
        assert_eq!(erase[10..12], [0, 0]);
        assert_eq!(erase[12..16], [0; 4], "no data phase");
        assert_eq!(erase[16..20], 0x1000_2000u32.to_le_bytes());
        assert_eq!(erase[20..24], 0x1000u32.to_le_bytes());
        assert_eq!(erase[24..32], [0; 8]);
        let write = &sent[1];
        assert_eq!(write[4..8], 2u32.to_le_bytes(), "the token moves on");
        assert_eq!(
            write[12..16],
            256u32.to_le_bytes(),
            "the data phase is the write's size"
        );
        assert_eq!(sent[2], vec![7; 256], "then the data");
    }

    #[test]
    fn usb_access_asks_about_root_hubs_only() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!usb_access(dir.path()), "no bus: nothing to ask about");
        assert!(!usb_access(&dir.path().join("absent")));
        let bus = dir.path().join("001");
        std::fs::create_dir(&bus).unwrap();
        std::fs::write(bus.join("002"), b"").unwrap();
        assert!(
            !usb_access(dir.path()),
            "a device that is not a root hub is never asked about"
        );
        std::fs::write(bus.join("001"), b"").unwrap();
        assert!(usb_access(dir.path()), "a root hub it may open read-write");
        // Mode bits stand in for the device cgroup; root ignores them.
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let hub = bus.join("001");
            std::fs::set_permissions(&hub, std::fs::Permissions::from_mode(0o400)).unwrap();
            assert!(!usb_access(dir.path()), "read-only is not enough");
        }
    }
}
