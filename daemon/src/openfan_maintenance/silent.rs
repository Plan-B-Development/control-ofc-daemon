//! A silent OpenFAN board (DEC-484): on USB with the board's identity, running
//! firmware that does not answer Control-OFC — the 2023 FW_01 build, which
//! floods its serial line, a firmware with another command set, or one that
//! has hung.
//!
//! **Known by evidence, never by USB ids alone.** A board is silent only when
//! an adoption probe opened its serial device and the identity handshake got no
//! answer ([`crate::serial::adoption::first_openfan_port`]), and that device is
//! still the node it was. A board nothing has asked is not silent, however its
//! descriptors read. Nothing here opens a device: the evidence is read from the
//! shared state and the identity from sysfs.

use std::path::Path;

use crate::health::state::SilentBoardEntry;
use crate::serial::adoption::NodeId;
use crate::serial::usb_identity::{self as usb, UsbDevice};

/// An OpenFAN board a probe opened and that did not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilentBoard {
    /// The board as sysfs shows it; its serial is always present
    /// ([`UsbDevice::is_openfan_board`]).
    pub usb: UsbDevice,
    /// The serial interface an update signals on: the lowest-numbered one
    /// with a tty — interface 0, the protocol's own, on every known build.
    pub interface_number: u8,
    /// That interface's device path.
    pub tty: String,
}

impl SilentBoard {
    pub fn usb_serial(&self) -> &str {
        self.usb.serial.as_deref().unwrap_or_default()
    }

    pub fn usb_port(&self) -> &str {
        &self.usb.port
    }

    /// What `/status` publishes.
    pub fn entry(&self) -> SilentBoardEntry {
        SilentBoardEntry {
            usb_serial: self.usb_serial().to_string(),
            usb_port: self.usb_port().to_string(),
        }
    }
}

/// Every OpenFAN board whose serial device a probe opened without an answer,
/// as sysfs shows it now, by USB port.
///
/// `unanswered` is the probe evidence and `observe` reads a path's node now
/// (production: [`crate::serial::adoption::node_id`]). An entry whose path no
/// longer names the node it named is skipped: a device that left, or came back
/// as a new node, proves nothing about what is there now. Whether a controller
/// answers meanwhile is the caller's question, not this one's.
pub fn silent_boards(
    sys: &Path,
    unanswered: &[(String, NodeId)],
    observe: impl Fn(&str) -> Option<NodeId>,
) -> Vec<SilentBoard> {
    let mut boards: Vec<SilentBoard> = Vec::new();
    for (path, node) in unanswered {
        if observe(path) != Some(*node) {
            continue;
        }
        let Some(identity) = usb::tty_identity(sys, path) else {
            continue;
        };
        if !identity.device.is_openfan_board()
            || boards.iter().any(|b| b.usb.port == identity.device.port)
        {
            continue;
        }
        let Some((interface_number, tty)) =
            usb::ttys_of(sys, &identity.device.port).into_iter().next()
        else {
            continue;
        };
        boards.push(SilentBoard {
            usb: identity.device,
            interface_number,
            tty,
        });
    }
    boards.sort_by(|a, b| a.usb.port.cmp(&b.usb.port));
    boards
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::usb_identity::fixture::{descriptors, Sysfs};

    const SERIAL: &str = "DE615CB14721492C";

    fn node(ino: u64) -> NodeId {
        NodeId { dev: 5, ino }
    }

    /// The nodes the fixture's ttys are, as `stat` would say.
    fn observe(path: &str) -> Option<NodeId> {
        match path {
            "/dev/ttyACM1" => Some(node(1)),
            "/dev/ttyACM2" => Some(node(2)),
            "/dev/ttyACM3" => Some(node(3)),
            "/dev/ttyACM5" => Some(node(5)),
            _ => None,
        }
    }

    fn evidence(paths: &[(&str, u64)]) -> Vec<(String, NodeId)> {
        paths
            .iter()
            .map(|(p, i)| (p.to_string(), node(*i)))
            .collect()
    }

    /// The board on `8-8` (`ttyACM1` on interface 0, `ttyACM2` on 2), another
    /// Pico SDK project on `3-2` (`ttyACM3`), and an Arduino on `1-4`
    /// (`ttyACM5`).
    fn bus() -> Sysfs {
        let sys = Sysfs::new();
        sys.add_openfan("8-8", SERIAL, 0x80, "ttyACM1", "ttyACM2");
        sys.add_device(
            "3-2",
            "2e8a",
            "000a",
            "Raspberry Pi",
            "Pico",
            "E6614103E7452D2F",
            &descriptors(2),
        );
        sys.add_tty("3-2", 0, "ttyACM3");
        sys.add_device(
            "1-4",
            "2341",
            "0043",
            "Arduino",
            "Uno",
            "7573530393",
            &descriptors(3),
        );
        sys.add_tty("1-4", 0, "ttyACM5");
        sys
    }

    #[test]
    fn a_board_that_did_not_answer_is_silent_and_signalled_on_its_first_interface() {
        let sys = bus();
        // The probe opened the board's second interface: the board is still
        // signalled on its first.
        let found = silent_boards(sys.root(), &evidence(&[("/dev/ttyACM2", 2)]), observe);
        assert_eq!(found.len(), 1);
        let b = &found[0];
        assert_eq!((b.usb_serial(), b.usb_port()), (SERIAL, "8-8"));
        assert_eq!((b.interface_number, b.tty.as_str()), (0, "/dev/ttyACM1"));
        assert_eq!(
            b.entry(),
            SilentBoardEntry {
                usb_serial: SERIAL.into(),
                usb_port: "8-8".into()
            }
        );
        // Both interfaces unanswered: still one board.
        let both = evidence(&[("/dev/ttyACM1", 1), ("/dev/ttyACM2", 2)]);
        assert_eq!(silent_boards(sys.root(), &both, observe).len(), 1);
    }

    #[test]
    fn a_board_nothing_asked_is_not_silent() {
        let sys = bus();
        assert!(
            silent_boards(sys.root(), &[], observe).is_empty(),
            "USB ids alone are no evidence"
        );
        // Strangers that did not answer are not OpenFAN boards.
        let strangers = evidence(&[("/dev/ttyACM3", 3), ("/dev/ttyACM5", 5)]);
        assert!(silent_boards(sys.root(), &strangers, observe).is_empty());
    }

    #[test]
    fn evidence_about_a_node_that_has_since_changed_proves_nothing() {
        let sys = bus();
        // The probe saw another node on that path: the board re-enumerated
        // since, and nothing has asked the device there now.
        let stale = evidence(&[("/dev/ttyACM1", 9)]);
        assert!(silent_boards(sys.root(), &stale, observe).is_empty());
        // The board left: the path names nothing.
        let fresh = evidence(&[("/dev/ttyACM1", 1)]);
        assert_eq!(silent_boards(sys.root(), &fresh, observe).len(), 1);
        sys.remove_device("8-8");
        assert!(silent_boards(sys.root(), &fresh, |_| Some(node(1))).is_empty());
    }

    #[test]
    fn a_board_in_its_bootloader_is_not_silent() {
        let sys = bus();
        sys.remove_device("8-8");
        sys.add_bootloader("8-8", "sdb");
        let evidence = evidence(&[("/dev/ttyACM1", 1)]);
        assert!(silent_boards(sys.root(), &evidence, |_| Some(node(1))).is_empty());
    }
}
