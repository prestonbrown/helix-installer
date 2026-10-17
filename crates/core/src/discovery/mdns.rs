// SPDX-License-Identifier: GPL-3.0-or-later

//! Finds Moonraker instances that announce `_moonraker._tcp` over mDNS.

use crate::transport::Host;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::time::Duration;
use tokio::time::Instant;

const SERVICE: &str = "_moonraker._tcp.local.";

/// Every Moonraker that answers within `window`. An mDNS daemon that cannot start (no
/// multicast-capable interface) finds nothing; the sweep covers that network.
pub async fn browse(window: Duration) -> Vec<Host> {
    let Ok(daemon) = ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(events) = daemon.browse(SERVICE) else {
        return Vec::new();
    };
    let deadline = Instant::now() + window;
    let mut found: Vec<Host> = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv_async()).await {
        let ServiceEvent::ServiceResolved(svc) = event else {
            continue;
        };
        for ip in svc.get_addresses_v4() {
            let mut host = Host::new(ip.to_string());
            host.moonraker_port = svc.get_port();
            if !found.iter().any(|h| h.addr == host.addr) {
                found.push(host);
            }
        }
    }
    // The daemon thread stops when dropped; shutdown only hastens it.
    let _ = daemon.shutdown();
    found
}
