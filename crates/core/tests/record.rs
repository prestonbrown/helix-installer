// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::discovery::Candidate;
use helix_installer_core::facts::Facts;
use helix_installer_core::identify::identify;
use helix_installer_core::manifest::Manifest;
use helix_installer_core::record::record;
use helix_installer_core::transport::Host;
use serde_json::{json, Value};
use std::path::PathBuf;

fn manifest() -> Manifest {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/identify_manifest.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn candidate(facts: Value) -> Candidate {
    let facts: Facts = serde_json::from_value(facts).unwrap();
    Candidate::new(Host::new("192.168.1.50"), facts)
}

fn probes() -> Vec<(String, Value)> {
    vec![
        ("klipper_hostname".to_string(), json!("k1.local")),
        (
            "net_dump".to_string(),
            json!([
                "eth0 192.168.1.50 aa:bb:cc:dd:ee:ff",
                "fe80::1ff:fe23:4567:890a"
            ]),
        ),
    ]
}

fn sample() -> Candidate {
    candidate(json!({
        "model": "k2",
        "stock_version": "1.1.5.8",
        "cpu_arch": "mips",
        "serial": "SN0012345678",
        "mac": "aa:bb:cc:dd:ee:ff",
        "ip": "192.168.1.50",
        "note": "serial SN0012345678 at 10.0.0.9",
    }))
}

#[test]
fn a_recording_hides_addresses_and_serial_but_keeps_model_versions_and_arch() {
    let out = record(&sample(), &probes());
    for leaked in [
        "192.168.1.50",
        "10.0.0.9",
        "aa:bb:cc:dd:ee:ff",
        "fe80::1ff",
        "SN0012345678",
    ] {
        assert!(!out.contains(leaked), "{leaked} leaked: {out}");
    }
    let facts: Facts = serde_json::from_str(&out).unwrap();
    assert_eq!(facts["model"], json!("k2"));
    assert_eq!(facts["stock_version"], json!("1.1.5.8"));
    assert_eq!(facts["cpu_arch"], json!("mips"));
    assert_eq!(facts["serial"], json!("<serial>"));
    assert_eq!(facts["klipper_hostname"], json!("k1.local"));
}

#[test]
fn probe_results_join_the_facts_in_the_fixture_layout() {
    let out = record(&sample(), &probes());
    assert!(!out.contains('\n'), "fixtures are one compact object");
    let facts: Facts = serde_json::from_str(&out).unwrap();
    assert!(facts["net_dump"].is_array());
}

#[test]
fn a_recorded_fixture_gets_the_same_verdict_as_the_candidate() {
    let m = manifest();
    let c = sample();
    let recorded: Facts = serde_json::from_str(&record(&c, &[])).unwrap();
    assert_eq!(identify(&m, &recorded), identify(&m, &c.facts));
}
