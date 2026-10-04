//! The OpenFan controller's USB identity, read from sysfs (DEC-481).
//!
//! **Read-only, and nothing here opens a device.** Every fact is an attribute
//! the kernel cached when the device enumerated, so a firmware update can watch
//! the board leave, appear as a bootloader and come back without asserting DTR
//! on anything (the `OFN-b` rule).
//!
//! Identity is the pair the update relies on, never vendor and product ids on
//! their own: `2e8a:000a` is the Pico SDK's generic CDC id, shared by any RP2040
//! board using its USB serial. The OpenFan firmware's USB serial number is its
//! flash chip's unique id, so it tells two boards apart; the RP2040 bootloader's
//! serial is the same on every board, so while the board is in its bootloader
//! the physical USB port (the device's sysfs name, `8-8`) is what identifies it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The production sysfs mount. Tests pass a fixture tree instead.
pub const SYSFS_ROOT: &str = "/sys";

/// Raspberry Pi's USB vendor id, used by every RP2040.
pub const RP2040_VENDOR_ID: &str = "2e8a";

/// The RP2040 boot ROM's USB product id (the `RPI-RP2` drive).
pub const RP2040_BOOTLOADER_PRODUCT_ID: &str = "0003";

/// The largest `descriptors` file read; a real one is a few hundred bytes.
const MAX_DESCRIPTORS_BYTES: u64 = 64 * 1024;

/// One USB device as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDevice {
    /// The device's sysfs name, which is its physical port path (`8-8`, `1-4.2`).
    pub port: String,
    pub vendor_id: String,
    pub product_id: String,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub bcd_device: Option<String>,
    /// The first configuration descriptor as the kernel cached it, lower-case
    /// hex. A firmware build's descriptor is compiled into its image, so this is
    /// evidence of which build is running (never proof of the exact one).
    pub config_descriptor_hex: Option<String>,
}

impl UsbDevice {
    /// Whether this is an RP2040 in its USB bootloader.
    pub fn is_rp2040_bootloader(&self) -> bool {
        self.vendor_id == RP2040_VENDOR_ID && self.product_id == RP2040_BOOTLOADER_PRODUCT_ID
    }
}

/// The USB device behind a serial port, and which of its interfaces the port is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtyIdentity {
    pub device: UsbDevice,
    /// `bInterfaceNumber` of the interface the tty belongs to.
    pub interface_number: u8,
    /// The tty's kernel name (`ttyACM1`).
    pub tty: String,
}

/// Resolve the USB device behind a serial device path — `/dev/ttyACM1`, or a
/// `/dev/serial/by-id/` link to one. `None` for a port that is not a USB
/// interface (a `ttyS*` UART) or whose attributes cannot be read.
pub fn tty_identity(sys: &Path, dev_path: &str) -> Option<TtyIdentity> {
    let tty = tty_name(dev_path)?;
    let interface = fs::canonicalize(sys.join("class/tty").join(&tty).join("device")).ok()?;
    let interface_number = read_interface_number(&interface)?;
    let device = read_device(interface.parent()?)?;
    Some(TtyIdentity {
        device,
        interface_number,
        tty,
    })
}

/// The device enumerated at `port`, if one is there now.
pub fn device_at(sys: &Path, port: &str) -> Option<UsbDevice> {
    read_device(&device_dir(sys, port)?)
}

/// Every USB device (not interface) sysfs lists.
pub fn usb_devices(sys: &Path) -> Vec<UsbDevice> {
    let Ok(entries) = fs::read_dir(sys.join("bus/usb/devices")) else {
        return Vec::new();
    };
    let mut devices: Vec<UsbDevice> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            // `8-8:1.0` is an interface of `8-8`, not a device.
            if name.contains(':') {
                return None;
            }
            read_device(&fs::canonicalize(e.path()).ok()?)
        })
        .collect();
    devices.sort_by(|a, b| a.port.cmp(&b.port));
    devices
}

/// Every RP2040 currently in its USB bootloader.
pub fn bootloaders(sys: &Path) -> Vec<UsbDevice> {
    usb_devices(sys)
        .into_iter()
        .filter(UsbDevice::is_rp2040_bootloader)
        .collect()
}

/// The device path of the tty on interface `interface_number` of the device at
/// `port` — the one interface a firmware update reopens, never another.
pub fn tty_for(sys: &Path, port: &str, interface_number: u8) -> Option<String> {
    let dir = device_dir(sys, port)?;
    let prefix = format!("{port}:");
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name().into_string().ok()?;
        if !name.starts_with(&prefix) {
            continue;
        }
        if read_interface_number(&entry.path()) != Some(interface_number) {
            continue;
        }
        let ttys = fs::read_dir(entry.path().join("tty")).ok()?;
        return ttys
            .flatten()
            .filter_map(|t| t.file_name().into_string().ok())
            .find(|t| t.starts_with("tty"))
            .map(|t| format!("/dev/{t}"));
    }
    None
}

/// The whole-disk block devices (`sdb`) that belong to the device at `port` —
/// the `RPI-RP2` drive of a board in its bootloader.
pub fn block_devices_of(sys: &Path, port: &str) -> Vec<String> {
    let Some(dir) = device_dir(sys, port) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(sys.join("block")) else {
        return Vec::new();
    };
    let mut disks: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let target = fs::canonicalize(e.path()).ok()?;
            target
                .starts_with(&dir)
                .then(|| e.file_name().into_string().ok())
                .flatten()
        })
        .collect();
    disks.sort();
    disks
}

/// Every whole-disk block device's name, sorted. Cheap: it changes whenever a
/// disk comes or goes, so a watch can re-read the drives only then.
pub fn block_device_names(sys: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(sys.join("block")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// A sysfs USB device name: `usb8`, `8-8`, `1-4.2`. Anything else — a path
/// separator, `..` — is refused before it reaches a path join.
fn valid_port_name(port: &str) -> bool {
    !port.is_empty()
        && port.len() <= 32
        && !port.contains("..")
        && port
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

fn device_dir(sys: &Path, port: &str) -> Option<PathBuf> {
    if !valid_port_name(port) {
        return None;
    }
    fs::canonicalize(sys.join("bus/usb/devices").join(port)).ok()
}

/// The kernel name of the tty a device path names, following a by-id link.
fn tty_name(dev_path: &str) -> Option<String> {
    let path = Path::new(dev_path);
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = resolved.file_name()?.to_str()?.to_string();
    (name.starts_with("tty") && name.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(name)
}

fn read_interface_number(interface: &Path) -> Option<u8> {
    u8::from_str_radix(&read_attr(interface, "bInterfaceNumber")?, 16).ok()
}

fn read_device(dir: &Path) -> Option<UsbDevice> {
    Some(UsbDevice {
        port: dir.file_name()?.to_str()?.to_string(),
        vendor_id: read_attr(dir, "idVendor")?,
        product_id: read_attr(dir, "idProduct")?,
        manufacturer: read_attr(dir, "manufacturer"),
        product: read_attr(dir, "product"),
        serial: read_attr(dir, "serial"),
        bcd_device: read_attr(dir, "bcdDevice"),
        config_descriptor_hex: read_config_descriptor(dir),
    })
}

/// A short text attribute, trimmed; `None` when absent, unreadable or empty.
fn read_attr(dir: &Path, name: &str) -> Option<String> {
    let text = crate::atomic_io::read_to_string_with_cap(&dir.join(name), 4096).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The first configuration descriptor in `descriptors`: the 18-byte device
/// descriptor comes first, then each configuration as the device returned it,
/// whose own `wTotalLength` says where it ends.
fn read_config_descriptor(dir: &Path) -> Option<String> {
    use std::io::Read;
    let file = fs::File::open(dir.join("descriptors")).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_DESCRIPTORS_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    config_descriptor_hex(&bytes)
}

/// [`read_config_descriptor`]'s parse, for tests.
fn config_descriptor_hex(descriptors: &[u8]) -> Option<String> {
    let config = descriptors.get(18..)?;
    if config.len() < 4 || config[1] != 0x02 {
        return None;
    }
    let total = u16::from_le_bytes([config[2], config[3]]) as usize;
    let config = config.get(..total)?;
    Some(config.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A fake sysfs tree with the links the real one has.
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    pub struct Sysfs {
        pub dir: tempfile::TempDir,
    }

    /// Descriptors with a 9-byte config descriptor whose last byte is `tag`.
    pub fn descriptors(tag: u8) -> Vec<u8> {
        let mut d = vec![0x12, 0x01];
        d.resize(18, 0);
        d.extend_from_slice(&[0x09, 0x02, 0x09, 0x00, 0x01, 0x01, 0x00, 0x80, tag]);
        d
    }

    impl Sysfs {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            for d in ["bus/usb/devices", "class/tty", "block", "devices/usb8"] {
                fs::create_dir_all(dir.path().join(d)).unwrap();
            }
            Self { dir }
        }

        pub fn root(&self) -> &Path {
            self.dir.path()
        }

        fn device_dir(&self, port: &str) -> PathBuf {
            self.root().join("devices/usb8").join(port)
        }

        /// Plug in a device at `port`.
        #[allow(clippy::too_many_arguments)]
        pub fn add_device(
            &self,
            port: &str,
            vid: &str,
            pid: &str,
            manufacturer: &str,
            product: &str,
            serial: &str,
            descriptors: &[u8],
        ) {
            let dir = self.device_dir(port);
            fs::create_dir_all(&dir).unwrap();
            for (name, value) in [
                ("idVendor", vid),
                ("idProduct", pid),
                ("manufacturer", manufacturer),
                ("product", product),
                ("serial", serial),
                ("bcdDevice", "0100"),
            ] {
                fs::write(dir.join(name), format!("{value}\n")).unwrap();
            }
            fs::write(dir.join("descriptors"), descriptors).unwrap();
            let link = self.root().join("bus/usb/devices").join(port);
            let _ = fs::remove_file(&link);
            symlink(&dir, link).unwrap();
        }

        /// Give the device at `port` a tty on interface `ifnum`.
        pub fn add_tty(&self, port: &str, ifnum: u8, tty: &str) {
            let iface = self.device_dir(port).join(format!("{port}:1.{ifnum}"));
            fs::create_dir_all(iface.join("tty").join(tty)).unwrap();
            fs::write(iface.join("bInterfaceNumber"), format!("{ifnum:02x}\n")).unwrap();
            let class = self.root().join("class/tty").join(tty);
            fs::create_dir_all(&class).unwrap();
            let link = class.join("device");
            let _ = fs::remove_file(&link);
            symlink(&iface, link).unwrap();
        }

        /// Give the device at `port` a whole-disk block device.
        pub fn add_disk(&self, port: &str, disk: &str) {
            let block = self
                .device_dir(port)
                .join(format!("{port}:1.0/host9/target9:0:0/9:0:0:0/block"))
                .join(disk);
            fs::create_dir_all(&block).unwrap();
            let link = self.root().join("block").join(disk);
            let _ = fs::remove_file(&link);
            symlink(&block, link).unwrap();
        }

        /// Unplug the device at `port`, links and all.
        pub fn remove_device(&self, port: &str) {
            let _ = fs::remove_dir_all(self.device_dir(port));
            let _ = fs::remove_file(self.root().join("bus/usb/devices").join(port));
            for dir in ["class/tty", "block"] {
                for e in fs::read_dir(self.root().join(dir)).unwrap().flatten() {
                    let dangling = if dir == "class/tty" {
                        !e.path().join("device").exists()
                    } else {
                        !e.path().exists()
                    };
                    if dangling {
                        let _ = fs::remove_dir_all(e.path());
                        let _ = fs::remove_file(e.path());
                    }
                }
            }
        }

        /// The board as it runs its firmware: two CDC interfaces.
        pub fn add_openfan(&self, port: &str, serial: &str, tag: u8, tty0: &str, tty2: &str) {
            self.add_device(
                port,
                "2e8a",
                "000a",
                "Karanovic Research",
                "OpenFan",
                serial,
                &descriptors(tag),
            );
            self.add_tty(port, 0, tty0);
            self.add_tty(port, 2, tty2);
        }

        /// The board in its bootloader before the kernel has attached its drive:
        /// on real hardware the disk follows the device by about a second.
        pub fn add_bootloader_without_drive(&self, port: &str) {
            self.add_device(
                port,
                "2e8a",
                "0003",
                "Raspberry Pi",
                "RP2 Boot",
                "E0C9125B0D9B",
                &descriptors(0xb0),
            );
        }

        /// The board in its bootloader, with its drive.
        pub fn add_bootloader(&self, port: &str, disk: &str) {
            self.add_bootloader_without_drive(port);
            self.add_disk(port, disk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{descriptors, Sysfs};
    use super::*;

    /// The live machine's shape: a Steam Controller on `ttyACM0`, the OpenFan
    /// on port `8-8` with `ttyACM1` (interface 0) and `ttyACM2` (interface 2).
    fn desk() -> Sysfs {
        let sys = Sysfs::new();
        sys.add_device(
            "1-3",
            "28de",
            "1304",
            "Valve",
            "Steam Controller",
            "X",
            &descriptors(1),
        );
        sys.add_tty("1-3", 0, "ttyACM0");
        sys.add_openfan("8-8", "DE615CB14721492C", 0x80, "ttyACM1", "ttyACM2");
        sys
    }

    #[test]
    fn the_adopted_tty_resolves_to_its_board_and_interface() {
        let sys = desk();
        let id = tty_identity(sys.root(), "/dev/ttyACM1").expect("the OpenFan tty");
        assert_eq!(id.device.port, "8-8");
        assert_eq!(id.device.serial.as_deref(), Some("DE615CB14721492C"));
        assert_eq!(id.device.product.as_deref(), Some("OpenFan"));
        assert_eq!(id.interface_number, 0);
        assert_eq!(id.tty, "ttyACM1");
        assert_eq!(
            id.device.config_descriptor_hex.as_deref(),
            Some("090209000101008080"),
            "the config descriptor, cut at its own wTotalLength"
        );
        // The other interface is the same board, a different interface.
        assert_eq!(
            tty_identity(sys.root(), "/dev/ttyACM2").map(|i| i.interface_number),
            Some(2)
        );
    }

    #[test]
    fn the_steam_controller_is_not_mistaken_for_the_board() {
        let sys = desk();
        let id = tty_identity(sys.root(), "/dev/ttyACM0").expect("a USB tty");
        assert_eq!(id.device.vendor_id, "28de");
        assert_ne!(id.device.port, "8-8");
    }

    #[test]
    fn the_board_is_reopened_only_on_the_interface_it_was_using() {
        let sys = desk();
        assert_eq!(
            tty_for(sys.root(), "8-8", 0).as_deref(),
            Some("/dev/ttyACM1")
        );
        assert_eq!(
            tty_for(sys.root(), "8-8", 2).as_deref(),
            Some("/dev/ttyACM2")
        );
        assert_eq!(tty_for(sys.root(), "8-8", 1), None, "no such interface");
        assert_eq!(tty_for(sys.root(), "1-3", 2), None, "another device's port");
    }

    #[test]
    fn a_second_pico_board_differs_only_by_serial_and_port() {
        let sys = desk();
        sys.add_openfan("3-1", "AAAAAAAAAAAAAAAA", 0x80, "ttyACM3", "ttyACM4");
        let other = tty_identity(sys.root(), "/dev/ttyACM3").unwrap();
        assert_eq!(other.device.vendor_id, "2e8a");
        assert_eq!(other.device.product_id, "000a");
        assert_ne!(
            other.device.serial.as_deref(),
            Some("DE615CB14721492C"),
            "vendor and product ids alone prove nothing"
        );
    }

    #[test]
    fn a_bootloader_is_found_with_its_drive_and_told_apart_by_port() {
        let sys = desk();
        assert!(bootloaders(sys.root()).is_empty());
        sys.remove_device("8-8");
        sys.add_bootloader("8-8", "sdb");
        sys.add_bootloader("5-2", "sdc");
        let found = bootloaders(sys.root());
        assert_eq!(
            found.iter().map(|d| d.port.as_str()).collect::<Vec<_>>(),
            ["5-2", "8-8"]
        );
        assert!(device_at(sys.root(), "8-8").unwrap().is_rp2040_bootloader());
        assert_eq!(block_devices_of(sys.root(), "8-8"), ["sdb"]);
        assert_eq!(block_devices_of(sys.root(), "5-2"), ["sdc"]);
        assert_eq!(block_device_names(sys.root()), ["sdb", "sdc"]);
        assert_eq!(
            tty_identity(sys.root(), "/dev/ttyACM1"),
            None,
            "the board's tty went with it"
        );
    }

    #[test]
    fn a_bootloader_without_its_drive_reports_none() {
        let sys = Sysfs::new();
        sys.add_device(
            "8-8",
            "2e8a",
            "0003",
            "Raspberry Pi",
            "RP2 Boot",
            "E0C9125B0D9B",
            &descriptors(0xb0),
        );
        assert!(block_devices_of(sys.root(), "8-8").is_empty());
    }

    #[test]
    fn a_port_name_cannot_walk_out_of_sysfs() {
        let sys = desk();
        for bad in ["../8-8", "8-8/..", "", "a/b", "8-8:1.0"] {
            assert_eq!(device_at(sys.root(), bad), None, "{bad:?}");
            assert_eq!(tty_for(sys.root(), bad, 0), None, "{bad:?}");
        }
        assert!(device_at(sys.root(), "8-8").is_some());
    }

    #[test]
    fn a_short_or_foreign_descriptor_blob_yields_no_config() {
        assert_eq!(config_descriptor_hex(&[0x12, 0x01]), None);
        let mut not_config = descriptors(1);
        not_config[19] = 0x04;
        assert_eq!(config_descriptor_hex(&not_config), None);
        let mut truncated = descriptors(1);
        truncated[20] = 0xff;
        assert_eq!(
            config_descriptor_hex(&truncated),
            None,
            "wTotalLength past the end"
        );
    }
}
