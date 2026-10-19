// SPDX-License-Identifier: GPL-3.0-or-later

//! Panels plugged into this computer by USB, found by the USB id of their serial bridge. Ports
//! are only listed here, never opened: opening an ESP32 panel's port resets it.

use super::Candidate;
use crate::facts::Facts;
use crate::transport::{Host, Serial, UsbPort};
use serde_json::json;

/// The serial bridges of the panels the installer flashes: the BTT K-Touch's CH340. Nothing else
/// on USB is ever offered.
pub const OFFERED: [(u16, u16); 1] = [(0x1a86, 0x7522)];

/// A USB id as the `usb` fact holds it: `1a86:7522`.
pub fn usb_id(vid: u16, pid: u16) -> String {
    format!("{vid:04x}:{pid:04x}")
}

/// macOS lists every serial device twice, as `/dev/tty.<name>` and `/dev/cu.<name>`; the `cu`
/// one is the port to open.
fn shadowed(name: &str, ports: &[UsbPort]) -> bool {
    name.strip_prefix("/dev/tty.").is_some_and(|rest| {
        let cu = format!("/dev/cu.{rest}");
        ports.iter().any(|p| p.name == cu)
    })
}

/// A candidate for each offered panel in `ports`, its host the port and its facts the `usb` id.
pub fn candidates(ports: &[UsbPort]) -> Vec<Candidate> {
    ports
        .iter()
        .filter(|p| OFFERED.contains(&(p.vid, p.pid)))
        .filter(|p| !shadowed(&p.name, ports))
        .map(|p| {
            let facts = Facts::from([("usb".to_string(), json!(usb_id(p.vid, p.pid)))]);
            Candidate::new(Host::new(p.name.clone()), facts)
        })
        .collect()
}

/// The offered panels plugged in now; none when the ports cannot be listed.
pub fn scan(serial: &dyn Serial) -> Vec<Candidate> {
    serial
        .usb_ports()
        .map(|ports| candidates(&ports))
        .unwrap_or_default()
}
