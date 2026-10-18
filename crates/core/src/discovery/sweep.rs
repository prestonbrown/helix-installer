// SPDX-License-Identifier: GPL-3.0-or-later

//! A /24 sweep for hosts listening on Moonraker's port, through the injectable `Net`.

use crate::transport::{Host, Net};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::Instant;

pub const MOONRAKER_PORT: u16 = 7125;
const CONCURRENT_CONNECTS: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// Most subnets one sweep covers.
const MAX_SUBNETS: usize = 4;

/// A local interface address with its netmask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Iface {
    pub ip: Ipv4Addr,
    pub mask: Ipv4Addr,
}

/// Private, non-loopback IPv4 interfaces in sweep order (see `order_ifaces`).
pub fn local_private_ifaces() -> Vec<Iface> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let named: Vec<(String, Iface)> = ifaces
        .iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(ref v4) if v4.ip.is_private() => Some((
                i.name.clone(),
                Iface {
                    ip: v4.ip,
                    mask: v4.netmask,
                },
            )),
            _ => None,
        })
        .collect();
    order_ifaces(&named, default_route_ip())
}

/// The local address the OS would send internet traffic from. Connecting a UDP socket only
/// picks a route; nothing is sent. `None` when there is no default route.
fn default_route_ip() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(1, 1, 1, 1), 53)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Container and VM bridges: printers are never behind them, and Docker's sorts first.
fn is_virtual_bridge(name: &str) -> bool {
    name == "docker0"
        || name.starts_with("br-")
        || name.starts_with("veth")
        || name.starts_with("virbr")
}

/// Orders the interfaces for the sweep, which covers only the first few. The interface
/// holding `default_ip` (the default route's) comes first and is always kept; the rest follow
/// by address, without virtual bridges and, once the default route is known, without any
/// /16 or wider, which are rarely a printer's LAN. With no default route only the bridges go.
pub fn order_ifaces(named: &[(String, Iface)], default_ip: Option<Ipv4Addr>) -> Vec<Iface> {
    let is_default = |i: &Iface| Some(i.ip) == default_ip;
    let mut rest: Vec<Iface> = named
        .iter()
        .filter(|(name, i)| !is_default(i) && !is_virtual_bridge(name))
        .filter(|(_, i)| default_ip.is_none() || u32::from(i.mask).leading_ones() > 16)
        .map(|(_, i)| *i)
        .collect();
    rest.sort_by_key(|i| (i.ip, i.mask));
    let mut out: Vec<Iface> = named.iter().map(|(_, i)| *i).filter(is_default).collect();
    out.extend(rest);
    out.dedup();
    out
}

/// The other hosts on the interface's subnet, never more than one /24's worth: a shorter
/// prefix sweeps the interface's own /24, and a /31 or /32 has no one else to find.
pub fn subnet_targets(iface: Iface) -> Vec<Ipv4Addr> {
    let prefix = u32::from(iface.mask).leading_ones().min(32);
    let (first, last) = if prefix >= 24 {
        let mask = u32::from(iface.mask);
        let net = u32::from(iface.ip) & mask;
        (net.saturating_add(1), (net | !mask).saturating_sub(1))
    } else {
        let net = u32::from(iface.ip) & 0xFFFF_FF00;
        (net + 1, net + 254)
    };
    (first..=last)
        .map(Ipv4Addr::from)
        .filter(|t| *t != iface.ip)
        .collect()
}

/// Every other host on the interfaces' subnets (at most 4 of them) that accepts a connection
/// on port 7125, among those answering within `window`.
pub async fn sweep(net: &dyn Net, ifaces: &[Iface], window: Duration) -> Vec<Host> {
    let permits = Semaphore::new(CONCURRENT_CONNECTS);
    let mut targets: Vec<Ipv4Addr> = ifaces
        .iter()
        .take(MAX_SUBNETS)
        .flat_map(|i| subnet_targets(*i))
        .filter(|t| !ifaces.iter().any(|i| i.ip == *t))
        .collect();
    targets.sort();
    targets.dedup();

    let mut probes: FuturesUnordered<_> = targets
        .into_iter()
        .map(|t| {
            let permits = &permits;
            async move {
                let _slot = permits.acquire().await.ok()?;
                let host = Host::new(t.to_string());
                net.tcp_banner(&host, MOONRAKER_PORT, CONNECT_TIMEOUT)
                    .await
                    .ok()
                    .map(|_| host)
            }
        })
        .collect();
    let deadline = Instant::now() + window;
    let mut found = Vec::new();
    while let Ok(Some(done)) = tokio::time::timeout_at(deadline, probes.next()).await {
        found.extend(done);
    }
    found.sort_by(|a, b| a.addr.cmp(&b.addr));
    found
}
