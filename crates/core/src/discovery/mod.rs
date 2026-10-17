// SPDX-License-Identifier: GPL-3.0-or-later

//! Finds printers on the LAN and turns what they say into facts. Identification itself
//! lives in `identify`; nothing here decides what a printer is.

pub mod flashforge;
pub mod mdns;
pub mod sweep;

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
        .unwrap_or(&host.addr)
        .to_string()
}

impl Candidate {
    pub fn new(host: Host, facts: Facts) -> Self {
        let key = key_of(&host, &facts);
        Candidate { host, facts, key }
    }

    fn absorb(&mut self, other: Candidate) {
        // The address that runs Moonraker is the one the install talks to.
        if !self.facts.contains_key("has_moonraker") && other.facts.contains_key("has_moonraker") {
            self.host = other.host;
        }
        for (k, v) in other.facts {
            self.facts.entry(k).or_insert(v);
        }
        self.key = key_of(&self.host, &self.facts);
    }
}

/// Folds candidates that are the same printer: equal key, or the same address.
pub fn merge(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for c in candidates {
        let same = |o: &Candidate| o.key == c.key || o.host.addr == c.host.addr;
        match out.iter().position(same) {
            Some(i) => out[i].absorb(c),
            None => out.push(c),
        }
        // A merge can make two earlier entries the same printer (a serial learned late).
        let mut i = 0;
        while i < out.len() {
            let dup = (i + 1..out.len())
                .find(|&j| out[j].key == out[i].key || out[j].host.addr == out[i].host.addr);
            match dup {
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
/// together, and what they find is merged by `Candidate::key`.
pub async fn discover(env: &Env, timeout: Duration) -> Vec<Candidate> {
    let own = sweep::local_private_ipv4s();
    let (announced, swept, flashforge) = tokio::join!(
        mdns::browse(timeout),
        sweep::sweep(env.net.as_ref(), &own),
        flashforge::scan(timeout),
    );
    let mut hosts: Vec<Host> = Vec::new();
    for h in announced.into_iter().chain(swept) {
        if !hosts.iter().any(|x| x.addr == h.addr) {
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
    merge(probed.chain(udp).collect())
}
