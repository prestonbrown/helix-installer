// SPDX-License-Identifier: GPL-3.0-or-later

//! Finds printers on the LAN and turns what they say into facts. Identification itself
//! lives in `identify`; nothing here decides what a printer is.

pub mod flashforge;
pub mod mdns;
pub mod sweep;
pub mod usb;

use crate::facts::Facts;
use crate::manifest::MoonrakerProbe;
use crate::steps::probe::moonraker_facts;
use crate::transport::{Env, Host, Moonraker, Result, TransportError};
use serde_json::Value;
use std::net::IpAddr;
use std::time::Duration;

pub use flashforge::parse_flashforge_reply;

/// A printer found on the network. `key` is its serial, else its MAC, else its address.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub host: Host,
    pub facts: Facts,
    pub key: String,
}

fn key_of(host: &Host, facts: &Facts) -> String {
    ["serial", "mac"]
        .iter()
        .find_map(|k| facts.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| endpoint(host))
}

/// The address, with the port when it is not Moonraker's usual one.
fn endpoint(host: &Host) -> String {
    if host.moonraker_port == sweep::MOONRAKER_PORT {
        host.addr.clone()
    } else {
        format!("{}:{}", host.addr, host.moonraker_port)
    }
}

fn runs_moonraker(c: &Candidate) -> bool {
    c.facts.contains_key("has_moonraker")
}

fn endpoint_pair(h: &Host) -> (&str, u16) {
    (h.addr.as_str(), h.moonraker_port)
}

impl Candidate {
    pub fn new(host: Host, facts: Facts) -> Self {
        let key = key_of(&host, &facts);
        Candidate { host, facts, key }
    }

    /// Two Moonraker instances are one printer only at the same address and port; a reply
    /// that never reached Moonraker (FlashForge UDP) joins by shared serial or address.
    fn is_same_printer(&self, other: &Candidate) -> bool {
        if runs_moonraker(self) && runs_moonraker(other) {
            return endpoint_pair(&self.host) == endpoint_pair(&other.host);
        }
        self.key == other.key || self.host.addr == other.host.addr
    }

    fn absorb(&mut self, other: Candidate) {
        let take_host = match (runs_moonraker(self), runs_moonraker(&other)) {
            (false, true) => true,
            (true, false) => false,
            _ => endpoint_pair(&other.host) < endpoint_pair(&self.host),
        };
        if take_host {
            self.host = other.host;
        }
        for (k, v) in other.facts {
            self.facts.entry(k).or_insert(v);
        }
        self.key = key_of(&self.host, &self.facts);
    }
}

/// Folds candidates that are the same printer (see `is_same_printer`). The surviving host
/// is the one running Moonraker, ties broken by the lowest (address, port).
pub fn merge(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for c in candidates {
        match out.iter().position(|o| o.is_same_printer(&c)) {
            Some(i) => out[i].absorb(c),
            None => out.push(c),
        }
        // A fold can make two earlier entries the same printer (a serial learned late).
        let mut i = 0;
        while i < out.len() {
            match (i + 1..out.len()).find(|&j| out[i].is_same_printer(&out[j])) {
                Some(j) => {
                    let other = out.remove(j);
                    out[i].absorb(other);
                }
                None => i += 1,
            }
        }
    }
    out
}

/// A host the user typed by IP.
pub fn manual_host(ip: &str) -> Result<Host> {
    let ip = ip.trim();
    ip.parse::<IpAddr>()
        .map(|a| Host::new(a.to_string()))
        .map_err(|_| TransportError::Protocol(format!("not an IP address: {ip:?}")))
}

/// Asks Moonraker what it can see. `None` when it answers none of the questions.
pub async fn probe_host(mr: &dyn Moonraker, host: &Host) -> Option<Facts> {
    let (info, system, update, objects) = tokio::join!(
        mr.printer_info(host),
        mr.system_info(host),
        mr.update_status(host),
        mr.objects_list(host),
    );
    let answers = [
        (MoonrakerProbe::PrinterInfo, info),
        (MoonrakerProbe::SystemInfo, system),
        (MoonrakerProbe::UpdateStatus, update),
        (MoonrakerProbe::ObjectsList, objects),
    ];
    let mut facts: Option<Facts> = None;
    for (kind, answer) in answers {
        // A missing endpoint (no update_manager, say) leaves its facts out.
        if let Ok(v) = answer {
            let f = facts.get_or_insert_with(Facts::new);
            for (k, val) in moonraker_facts(kind, &v) {
                f.insert(k, val);
            }
        }
    }
    facts
}

async fn candidate_for(env: &Env, host: Host) -> Option<Candidate> {
    probe_host(env.moonraker.as_ref(), &host)
        .await
        .map(|f| Candidate::new(host, f))
}

/// Looks for printers for `timeout`: mDNS, a sweep of the local /24s and FlashForge UDP run
/// together, and what they find is merged by `Candidate::key`; panels on USB follow.
pub async fn discover(env: &Env, timeout: Duration) -> Vec<Candidate> {
    let own = sweep::local_private_ifaces();
    let (announced, swept, flashforge) = tokio::join!(
        mdns::browse(timeout),
        sweep::sweep(env.net.as_ref(), &own, timeout),
        flashforge::scan(timeout),
    );
    let mut hosts: Vec<Host> = Vec::new();
    for h in announced.into_iter().chain(swept) {
        if !hosts.iter().any(|x| endpoint_pair(x) == endpoint_pair(&h)) {
            hosts.push(h);
        }
    }
    let probed = futures_util::future::join_all(hosts.into_iter().map(|h| candidate_for(env, h)))
        .await
        .into_iter()
        .flatten();
    let udp = flashforge
        .into_iter()
        .map(|(host, facts)| Candidate::new(host, facts));
    let mut found = merge(probed.chain(udp).collect());
    found.extend(usb::scan(env.serial.as_ref()));
    found
}
