// SPDX-License-Identifier: GPL-3.0-or-later

//! FlashForge printers answer a UDP broadcast with a 276-byte reply naming the model and
//! serial. Layout (big-endian) follows GhostTypes/ff-5mp-api-py `flashforge/discovery`.

use crate::facts::Facts;
use crate::transport::Host;
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::Instant;

const REPLY_LEN: usize = 276;
const PRODUCT_ID: std::ops::Range<usize> = 0x88..0x8A;
const SERIAL: std::ops::Range<usize> = 0x92..0x92 + 128;
const NAME: std::ops::Range<usize> = 0..0x84;
const BROADCAST_PORT: u16 = 48899;
const LISTEN_PORT: u16 = 18007;
/// The probe datagram FlashForge firmware answers.
const PROBE: [u8; 20] = [
    0x77, 0x77, 0x77, 0x2E, 0x75, 0x73, 0x72, 0x22, 0x65, 0x36, 0xC0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn c_string(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// The model from the USB product id, falling back to the printer's name.
fn model(product_id: u16, name: &str) -> Option<&'static str> {
    match product_id {
        0x0023 => return Some("Adventurer5M"),
        0x0024 => return Some("Adventurer5MPro"),
        0x0026 => return Some("AD5X"),
        0x0028 => return Some("Creator5"),
        0x0029 => return Some("Creator5Pro"),
        _ => {}
    }
    let upper = name.to_uppercase();
    if upper == "AD5X" {
        Some("AD5X")
    } else if upper.contains("ADVENTURER 5M") || upper.contains("AD5M") {
        Some(if upper.contains("PRO") {
            "Adventurer5MPro"
        } else {
            "Adventurer5M"
        })
    } else {
        None
    }
}

/// Facts from one reply. `None` for a short datagram or one without a serial.
pub fn parse_flashforge_reply(bytes: &[u8]) -> Option<Facts> {
    if bytes.len() < REPLY_LEN {
        return None;
    }
    let serial = c_string(&bytes[SERIAL]);
    if serial.is_empty() {
        return None;
    }
    let name = c_string(&bytes[NAME]);
    let pid = u16::from_be_bytes([bytes[PRODUCT_ID.start], bytes[PRODUCT_ID.start + 1]]);
    let mut f = Facts::new();
    let mut put = |k: &str, v: &str| {
        f.insert(k.to_string(), Value::String(v.to_string()));
    };
    put("flashforge_udp.serial", &serial);
    put("serial", &serial);
    if let Some(m) = model(pid, &name) {
        put("flashforge_udp.model", m);
        put("model", m);
    }
    Some(f)
}

/// Broadcasts the probe and collects replies for `window`.
pub async fn scan(window: Duration) -> Vec<(Host, Facts)> {
    // The firmware replies to the sender's address on the legacy listen port; fall back to
    // an ephemeral port if another program holds it.
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, LISTEN_PORT)).await {
        Ok(s) => s,
        Err(_) => match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        },
    };
    if socket.set_broadcast(true).is_err() {
        return Vec::new();
    }
    let target = SocketAddr::from((Ipv4Addr::BROADCAST, BROADCAST_PORT));
    if socket.send_to(&PROBE, target).await.is_err() {
        return Vec::new();
    }
    let deadline = Instant::now() + window;
    let mut found: Vec<(Host, Facts)> = Vec::new();
    let mut buf = [0u8; 1024];
    while let Ok(Ok((n, from))) =
        tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await
    {
        let Some(facts) = parse_flashforge_reply(&buf[..n]) else {
            continue;
        };
        let addr = from.ip().to_string();
        if !found.iter().any(|(h, _)| h.addr == addr) {
            found.push((Host::new(addr), facts));
        }
    }
    found
}
