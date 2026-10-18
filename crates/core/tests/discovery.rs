// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::discovery::sweep::{order_ifaces, subnet_targets, sweep, Iface};
use helix_installer_core::discovery::{
    manual_host, merge, parse_flashforge_reply, probe_host, Candidate,
};
use helix_installer_core::facts::Facts;
use helix_installer_core::manifest::MoonrakerProbe;
use helix_installer_core::steps::probe::moonraker_facts;
use helix_installer_core::transport::fake::{FakeMoonraker, FakeNet};
use helix_installer_core::transport::Host;
use serde_json::{json, Value};
use std::net::Ipv4Addr;

/// A 276-byte modern FlashForge discovery reply laid out as documented in
/// GhostTypes/ff-5mp-api-py (`flashforge/discovery/discovery.py`). Synthetic, not a capture.
fn flashforge_reply(name: &str, product_id: u16, serial: &str) -> Vec<u8> {
    let mut b = vec![0u8; 276];
    b[..name.len()].copy_from_slice(name.as_bytes());
    b[0x84..0x86].copy_from_slice(&8899u16.to_be_bytes());
    b[0x86..0x88].copy_from_slice(&0x2B71u16.to_be_bytes());
    b[0x88..0x8A].copy_from_slice(&product_id.to_be_bytes());
    b[0x8C..0x8E].copy_from_slice(&0x5A02u16.to_be_bytes());
    b[0x8E..0x90].copy_from_slice(&8898u16.to_be_bytes());
    b[0x92..0x92 + serial.len()].copy_from_slice(serial.as_bytes());
    b
}

fn facts(pairs: &[(&str, Value)]) -> Facts {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn flashforge_reply_gives_model_and_serial() {
    let f = parse_flashforge_reply(&flashforge_reply("AD5X", 0x0026, "SNMOMC9900001")).unwrap();
    assert_eq!(f["flashforge_udp.model"], json!("AD5X"));
    assert_eq!(f["flashforge_udp.serial"], json!("SNMOMC9900001"));
    assert_eq!(f["serial"], json!("SNMOMC9900001"));
    assert_eq!(f["model"], json!("AD5X"));
}

#[test]
fn flashforge_model_falls_back_to_the_printer_name() {
    let f = parse_flashforge_reply(&flashforge_reply("Adventurer 5M Pro", 0, "SN1")).unwrap();
    assert_eq!(f["flashforge_udp.model"], json!("Adventurer5MPro"));
}

#[test]
fn a_truncated_flashforge_reply_is_none() {
    let full = flashforge_reply("AD5X", 0x0026, "SN1");
    assert!(parse_flashforge_reply(&full[..200]).is_none());
    assert!(parse_flashforge_reply(&[]).is_none());
}

#[test]
fn a_flashforge_reply_without_a_serial_is_none() {
    assert!(parse_flashforge_reply(&flashforge_reply("AD5X", 0x0026, "")).is_none());
}

#[test]
fn one_printer_seen_twice_on_the_same_ip_is_one_candidate() {
    let a = Candidate::new(
        Host::new("192.168.1.50"),
        facts(&[("has_moonraker", json!(true))]),
    );
    let b = Candidate::new(
        Host::new("192.168.1.50"),
        facts(&[("klipper_hostname", json!("voron"))]),
    );
    let merged = merge(vec![a, b]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].facts["has_moonraker"], json!(true));
    assert_eq!(merged[0].facts["klipper_hostname"], json!("voron"));
}

#[test]
fn one_serial_on_two_ips_is_one_candidate() {
    let a = Candidate::new(
        Host::new("192.168.1.50"),
        facts(&[("serial", json!("SN1")), ("has_moonraker", json!(true))]),
    );
    let b = Candidate::new(
        Host::new("192.168.1.77"),
        facts(&[("serial", json!("SN1"))]),
    );
    let merged = merge(vec![a, b]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].key, "SN1");
    assert_eq!(merged[0].host.addr, "192.168.1.50");
}

#[test]
fn a_serial_only_reply_joins_the_candidate_on_the_same_ip() {
    let mr = Candidate::new(
        Host::new("192.168.1.50"),
        facts(&[("has_moonraker", json!(true))]),
    );
    let ff = Candidate::new(
        Host::new("192.168.1.50"),
        facts(&[("serial", json!("SN1"))]),
    );
    let merged = merge(vec![mr, ff]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].key, "SN1");
    assert_eq!(merged[0].facts["has_moonraker"], json!(true));
}

#[test]
fn different_printers_stay_separate() {
    let a = Candidate::new(Host::new("192.168.1.50"), Facts::new());
    let b = Candidate::new(Host::new("192.168.1.51"), Facts::new());
    assert_eq!(merge(vec![a, b]).len(), 2);
}

#[test]
fn the_key_is_serial_then_mac_then_address() {
    let by_serial = Candidate::new(
        Host::new("10.0.0.2"),
        facts(&[("serial", json!("S")), ("mac", json!("aa:bb"))]),
    );
    let by_mac = Candidate::new(Host::new("10.0.0.2"), facts(&[("mac", json!("aa:bb"))]));
    let by_addr = Candidate::new(Host::new("10.0.0.2"), Facts::new());
    assert_eq!(by_serial.key, "S");
    assert_eq!(by_mac.key, "aa:bb");
    assert_eq!(by_addr.key, "10.0.0.2");
}

#[tokio::test]
async fn the_sweep_finds_open_ports_on_the_subnet_and_skips_itself() {
    let net = FakeNet::new();
    net.answer("192.168.7.20", 7125, None);
    net.answer("192.168.7.99", 7125, None);
    net.answer("192.168.7.5", 7125, None);
    net.answer("192.168.8.20", 7125, None);
    let own = Iface {
        ip: Ipv4Addr::new(192, 168, 7, 5),
        mask: Ipv4Addr::new(255, 255, 255, 0),
    };
    let mut found = sweep(&net, &[own], std::time::Duration::from_secs(5)).await;
    found.sort_by(|a, b| a.addr.cmp(&b.addr));
    let addrs: Vec<_> = found.iter().map(|h| h.addr.as_str()).collect();
    assert_eq!(addrs, ["192.168.7.20", "192.168.7.99"]);
}

#[tokio::test]
async fn moonraker_without_update_manager_still_has_moonraker() {
    let mr = FakeMoonraker::new();
    mr.set_printer_info("10.0.0.9", json!({"hostname": "voron", "state": "ready"}));
    mr.set_system_info(
        "10.0.0.9",
        json!({"system_info": {
            "cpu_info": {"processor": "aarch64"},
            "distribution": {"name": "Debian GNU/Linux 12"},
            "available_services": ["klipper", "helixscreen"]
        }}),
    );
    mr.set_objects_list("10.0.0.9", json!({"objects": ["extruder", "heater_bed"]}));
    let f = probe_host(&mr, &Host::new("10.0.0.9")).await.unwrap();
    assert_eq!(f["has_moonraker"], json!(true));
    assert_eq!(f["klipper_hostname"], json!("voron"));
    assert_eq!(f["cpu_arch"], json!("aarch64"));
    assert_eq!(f["os_distribution"], json!("Debian GNU/Linux 12"));
    assert_eq!(f["services"], json!(["klipper", "helixscreen"]));
    assert_eq!(f["klipper_objects"], json!(["extruder", "heater_bed"]));
    assert_eq!(f["helix_installed"], json!(true));
    assert!(!f.contains_key("helix_version"));
    assert!(!f.contains_key("serial"));
}

#[tokio::test]
async fn update_status_supplies_the_helix_version() {
    let mr = FakeMoonraker::new();
    mr.set_printer_info("10.0.0.9", json!({"hostname": "voron"}));
    mr.set_update_status(
        "10.0.0.9",
        json!({"version_info": {"helixscreen": {"version": "1.0.3"}}}),
    );
    let f = probe_host(&mr, &Host::new("10.0.0.9")).await.unwrap();
    assert_eq!(f["helix_installed"], json!(true));
    assert_eq!(f["helix_version"], json!("1.0.3"));
}

#[tokio::test]
async fn a_host_that_answers_nothing_is_not_a_moonraker() {
    let mr = FakeMoonraker::new();
    assert!(probe_host(&mr, &Host::new("10.0.0.9")).await.is_none());
}

#[test]
fn objects_list_facts_are_the_object_names() {
    let f = moonraker_facts(
        MoonrakerProbe::ObjectsList,
        &json!({"objects": ["extruder", 3, "fan"]}),
    );
    assert_eq!(f["klipper_objects"], json!(["extruder", "fan"]));
}

#[test]
fn manual_host_accepts_an_ip_and_rejects_text() {
    assert_eq!(manual_host(" 192.168.1.9 ").unwrap().addr, "192.168.1.9");
    assert!(manual_host("not an ip").is_err());
    assert!(manual_host("").is_err());
}

fn iface(ip: [u8; 4], prefix: u32) -> Iface {
    Iface {
        ip: Ipv4Addr::from(ip),
        mask: Ipv4Addr::from(u32::MAX << (32 - prefix)),
    }
}

#[test]
fn subnet_targets_stay_within_one_slash_24() {
    let t24 = subnet_targets(iface([192, 168, 1, 5], 24));
    assert_eq!(t24.len(), 253);
    assert!(!t24.contains(&Ipv4Addr::new(192, 168, 1, 5)));
    assert_eq!(t24[0], Ipv4Addr::new(192, 168, 1, 1));

    let t16 = subnet_targets(iface([10, 1, 7, 9], 16));
    assert_eq!(t16.len(), 253);
    assert!(t16.iter().all(|a| a.octets()[..3] == [10, 1, 7]));

    let t28 = subnet_targets(iface([192, 168, 1, 70], 28));
    assert_eq!(t28.len(), 13);
    assert_eq!(t28[0], Ipv4Addr::new(192, 168, 1, 65));
    assert_eq!(*t28.last().unwrap(), Ipv4Addr::new(192, 168, 1, 78));

    assert!(subnet_targets(iface([192, 168, 1, 70], 32)).is_empty());
}

fn named(name: &str, ip: [u8; 4], prefix: u32) -> (String, Iface) {
    (name.to_string(), iface(ip, prefix))
}

/// The interfaces of a Linux desktop running Docker and libvirt, in the order the OS lists them.
fn docker_box() -> Vec<(String, Iface)> {
    vec![
        named("docker0", [172, 17, 0, 1], 16),
        named("br-3f2a9c", [172, 18, 0, 1], 16),
        named("veth12ab", [172, 17, 0, 9], 24),
        named("virbr0", [192, 168, 122, 1], 24),
        named("wlan0", [192, 168, 30, 40], 24),
        named("eth1", [10, 20, 0, 7], 16),
        named("eth0", [192, 168, 1, 50], 24),
    ]
}

#[test]
fn the_default_route_interface_is_swept_first_and_bridges_are_skipped() {
    let order = order_ifaces(&docker_box(), Some(Ipv4Addr::new(192, 168, 30, 40)));
    assert_eq!(
        order,
        vec![iface([192, 168, 30, 40], 24), iface([192, 168, 1, 50], 24)]
    );
}

#[test]
fn a_wide_default_route_interface_is_still_swept() {
    let order = order_ifaces(&docker_box(), Some(Ipv4Addr::new(10, 20, 0, 7)));
    assert_eq!(
        order,
        vec![
            iface([10, 20, 0, 7], 16),
            iface([192, 168, 1, 50], 24),
            iface([192, 168, 30, 40], 24),
        ]
    );
}

#[test]
fn the_default_route_interface_is_kept_whatever_its_name() {
    let order = order_ifaces(&docker_box(), Some(Ipv4Addr::new(192, 168, 122, 1)));
    assert_eq!(order[0], iface([192, 168, 122, 1], 24));
}

#[test]
fn without_a_default_route_only_the_bridge_names_are_skipped() {
    let order = order_ifaces(&docker_box(), None);
    assert_eq!(
        order,
        vec![
            iface([10, 20, 0, 7], 16),
            iface([192, 168, 1, 50], 24),
            iface([192, 168, 30, 40], 24),
        ]
    );
}

#[test]
fn a_default_route_off_every_private_interface_counts_as_known() {
    // A public or CGNAT address on the default route: no LAN interface carries it, so the
    // wide ones are still skipped.
    let order = order_ifaces(&docker_box(), Some(Ipv4Addr::new(100, 64, 3, 2)));
    assert_eq!(
        order,
        vec![iface([192, 168, 1, 50], 24), iface([192, 168, 30, 40], 24)]
    );
}

#[tokio::test]
async fn the_sweep_covers_at_most_four_subnets() {
    let net = FakeNet::new();
    for third in 1..=5u8 {
        net.answer(&format!("10.0.{third}.20"), 7125, None);
    }
    let ifaces: Vec<Iface> = (1..=5u8).map(|t| iface([10, 0, t, 5], 24)).collect();
    let found = sweep(&net, &ifaces, std::time::Duration::from_secs(5)).await;
    assert_eq!(found.len(), 4);
}

fn mr_at(addr: &str, port: u16, name: &str) -> Candidate {
    let mut host = Host::new(addr);
    host.moonraker_port = port;
    Candidate::new(
        host,
        facts(&[
            ("has_moonraker", json!(true)),
            ("klipper_hostname", json!(name)),
        ]),
    )
}

#[test]
fn two_moonraker_ports_on_one_ip_stay_two_candidates() {
    let merged = merge(vec![
        mr_at("10.0.0.9", 7125, "a"),
        mr_at("10.0.0.9", 7126, "b"),
    ]);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].facts["klipper_hostname"], json!("a"));
    assert_eq!(merged[1].facts["klipper_hostname"], json!("b"));
    assert_ne!(merged[0].key, merged[1].key);
}

#[test]
fn a_flashforge_reply_and_one_moonraker_on_an_ip_are_one_candidate() {
    let ff = Candidate::new(Host::new("10.0.0.9"), facts(&[("serial", json!("SN1"))]));
    let merged = merge(vec![ff, mr_at("10.0.0.9", 7126, "a")]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].host.moonraker_port, 7126);
    assert_eq!(merged[0].key, "SN1");
}

#[test]
fn the_surviving_host_does_not_depend_on_order() {
    let udp = |addr: &str| Candidate::new(Host::new(addr), facts(&[("serial", json!("SN1"))]));
    let a = merge(vec![udp("10.0.0.9"), udp("10.0.0.3")]);
    let b = merge(vec![udp("10.0.0.3"), udp("10.0.0.9")]);
    assert_eq!(a[0].host.addr, "10.0.0.3");
    assert_eq!(b[0].host.addr, "10.0.0.3");
    let with_mr = merge(vec![udp("10.0.0.3"), {
        let mut c = mr_at("10.0.0.9", 7125, "x");
        c.facts.insert("serial".into(), json!("SN1"));
        Candidate::new(c.host, c.facts)
    }]);
    assert_eq!(with_mr[0].host.addr, "10.0.0.9");
}
