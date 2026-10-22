// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::bundle::support_bundle;
use helix_installer_core::facts::Facts;
use helix_installer_core::run_state::{LogEntry, RunState};
use helix_installer_core::transport::Host;
use serde_json::json;

const PASSWORD: &str = "hunter2-pw";
const SERIAL: &str = "SN0012345678";

fn state() -> RunState {
    let facts: Facts = serde_json::from_value(json!({
        "model": "K1",
        "serial": SERIAL,
        "ssh_user": "root",
        "lan_ip": "192.168.1.77",
    }))
    .unwrap();
    let mut s = RunState::new("run1", "k1", "r", Host::new("192.168.1.50"), facts);
    s.log.push(LogEntry {
        at_unix: 100,
        step: 2,
        text: "connected to 192.168.1.50:7125 mac aa:bb:cc:dd:ee:ff".into(),
    });
    s.log.push(LogEntry {
        at_unix: 101,
        step: 3,
        text: "link-local fe80::1ff:fe23:4567:890a and ::1 seen".into(),
    });
    s
}

fn logs() -> Vec<String> {
    vec![
        format!("login ok with {PASSWORD} for SN {SERIAL}"),
        "eth0 AA-BB-CC-DD-EE-01 up".to_string(),
    ]
}

#[test]
fn a_secret_is_stripped_wherever_and_however_often_it_appears() {
    let mut s = state();
    s.log.push(LogEntry {
        at_unix: 102,
        step: 3,
        text: format!("prefix{PASSWORD}suffix and again {PASSWORD}"),
    });
    for mask in [true, false] {
        let out = support_bundle(&s, &logs(), &[PASSWORD], mask);
        assert!(!out.contains(PASSWORD), "mask={mask}: {out}");
        assert!(out.contains("prefix"), "the rest of the line stays");
        assert!(out.contains("suffix and again"));
    }
}

#[test]
fn an_empty_secret_does_not_wipe_the_bundle() {
    let out = support_bundle(&state(), &logs(), &["", PASSWORD], false);
    assert!(out.contains("recipe") && out.contains("connected to"));
    assert!(out.contains("192.168.1.50"));
    assert!(!out.contains(PASSWORD));
}

#[test]
fn masking_hides_ips_macs_and_the_serial() {
    let out = support_bundle(&state(), &logs(), &[PASSWORD], true);
    for leaked in [
        "192.168.1.50",
        "192.168.1.77",
        "aa:bb:cc:dd:ee:ff",
        "AA-BB-CC-DD-EE-01",
        "fe80::1ff:fe23:4567:890a",
        SERIAL,
    ] {
        assert!(!out.contains(leaked), "{leaked} leaked: {out}");
    }
    assert!(out.contains(":7125"), "a port is not an address");
    assert!(out.contains("model") && out.contains("K1"));
}

#[test]
fn unmasked_keeps_addresses_and_serial() {
    let out = support_bundle(&state(), &logs(), &[PASSWORD], false);
    for kept in [
        "192.168.1.50",
        "aa:bb:cc:dd:ee:ff",
        "fe80::1ff:fe23:4567:890a",
        SERIAL,
    ] {
        assert!(out.contains(kept), "{kept} missing: {out}");
    }
}

#[test]
fn masking_leaves_versions_and_times_alone() {
    let mut s = state();
    s.log.push(LogEntry {
        at_unix: 5,
        step: 0,
        text: "version 1.2.3.4.5 and 0.1.0 at 12:34:56".into(),
    });
    let out = support_bundle(&s, &[], &[], true);
    assert!(
        out.contains("version 1.2.3.4.5 and 0.1.0 at 12:34:56"),
        "{out}"
    );
}

#[test]
fn a_mac_after_a_label_is_still_masked() {
    let mut s = state();
    s.log.push(LogEntry {
        at_unix: 6,
        step: 0,
        text: "mac:aa:bb:cc:dd:ee:11 and eth0:aa:bb:cc:dd:ee:22 and wlan-AA-BB-CC-DD-EE-33".into(),
    });
    let out = support_bundle(&s, &[], &[], true);
    for leaked in ["ee:11", "ee:22", "EE-33"] {
        assert!(!out.contains(leaked), "{leaked} leaked: {out}");
    }
    assert!(
        out.contains("eth0:<mac>") && out.contains("wlan-<mac>"),
        "{out}"
    );
}

#[test]
fn a_v4_mapped_v6_address_is_masked_whole() {
    let mut s = state();
    s.log.push(LogEntry {
        at_unix: 7,
        step: 0,
        text: "peer ::ffff:10.20.30.40 ok".into(),
    });
    let out = support_bundle(&s, &[], &[], true);
    assert!(out.contains("peer <ip> ok"), "{out}");
}

#[test]
fn the_longest_secret_is_stripped_first() {
    let mut s = state();
    s.log.push(LogEntry {
        at_unix: 8,
        step: 0,
        text: "pw=hunter2-extra-tail".into(),
    });
    let out = support_bundle(&s, &[], &["hunter2", "hunter2-extra"], false);
    assert!(out.contains("pw=<secret>-tail"), "{out}");
    assert!(!out.contains("extra"), "{out}");
}

#[test]
fn print_files_still_set_aside_head_the_bundle() {
    let mut s = state();
    let line = "print files set aside and not yet put back: 3 in /g/.helix-kept";
    assert!(!support_bundle(&s, &logs(), &[], true).contains("set aside"));
    s.facts.insert("kept_gcodes".into(), json!(3));
    s.facts.insert("kept_gcodes_dir".into(), json!("/g"));
    assert!(support_bundle(&s, &logs(), &[], true).contains(line));
    s.facts.insert("kept_gcodes_back".into(), json!(true));
    assert!(!support_bundle(&s, &logs(), &[], true).contains(line));
}
