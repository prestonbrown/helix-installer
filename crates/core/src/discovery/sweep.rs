// SPDX-License-Identifier: GPL-3.0-or-later

//! A /24 sweep for hosts listening on Moonraker's port, through the injectable `Net`.

use crate::transport::{Host, Net};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::sync::Semaphore;

pub const MOONRAKER_PORT: u16 = 7125;
const CONCURRENT_CONNECTS: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// Private, non-loopback IPv4 addresses of the local interfaces.
pub fn local_private_ipv4s() -> Vec<Ipv4Addr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut ips: Vec<Ipv4Addr> = ifaces
        .iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) if v4.is_private() => Some(v4),
            _ => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// Every other address in each interface's /24 that accepts a connection on port 7125.
pub async fn sweep(net: &dyn Net, own: &[Ipv4Addr]) -> Vec<Host> {
    let permits = Semaphore::new(CONCURRENT_CONNECTS);
    let mut targets: Vec<Ipv4Addr> = own
        .iter()
        .flat_map(|ip| {
            let [a, b, c, _] = ip.octets();
            (1..=254u8).map(move |d| Ipv4Addr::new(a, b, c, d))
        })
        .filter(|t| !own.contains(t))
        .collect();
    targets.sort();
    targets.dedup();

    let probes = targets.into_iter().map(|t| {
        let permits = &permits;
        async move {
            let _slot = permits.acquire().await.ok()?;
            let host = Host::new(t.to_string());
            net.tcp_banner(&host, MOONRAKER_PORT, CONNECT_TIMEOUT)
                .await
                .ok()
                .map(|_| host)
        }
    });
    futures_util::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}
