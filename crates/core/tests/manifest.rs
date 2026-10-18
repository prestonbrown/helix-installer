// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::manifest::{
    verify_and_parse, Cond, Manifest, ManifestCache, ManifestError, PlatformSel, Probe, Step,
    WaitTarget,
};
use helix_installer_core::version::FwVersion;
use std::io::Cursor;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAY: u64 = 24 * 60 * 60;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn pubkey() -> String {
    std::fs::read_to_string(fixture("test_key.pub")).unwrap()
}

fn sign(bytes: &[u8]) -> String {
    let sk_text = std::fs::read_to_string(fixture("test_key.sec")).unwrap();
    let sk = minisign::SecretKeyBox::from_string(&sk_text)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, Cursor::new(bytes), None, None)
        .unwrap()
        .into_string()
}

fn manifest_bytes() -> Vec<u8> {
    std::fs::read(fixture("manifest_min.json")).unwrap()
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn with_min_version(v: &str) -> Vec<u8> {
    String::from_utf8(manifest_bytes())
        .unwrap()
        .replace(
            "\"min_app_version\": \"0.1.0\"",
            &format!("\"min_app_version\": \"{v}\""),
        )
        .into_bytes()
}

#[test]
fn good_signature_parses() {
    let bytes = manifest_bytes();
    let m = verify_and_parse(&bytes, &sign(&bytes), &pubkey(), "0.1.0").unwrap();
    assert_eq!(m.manifest_version, 1);
    assert_eq!(m.printers[0].id, "ad5m");
    assert_eq!(m.recipes["ad5m_forge_x"].len(), 9);
    assert!(m.printers[0].verified.is_some());
}

#[test]
fn flipped_byte_is_bad_signature() {
    let mut bytes = manifest_bytes();
    let sig = sign(&bytes);
    bytes[10] ^= 0x01;
    assert_eq!(
        verify_and_parse(&bytes, &sig, &pubkey(), "0.1.0").unwrap_err(),
        ManifestError::BadSignature
    );
}

#[test]
fn garbage_signature_is_bad_signature() {
    let bytes = manifest_bytes();
    assert_eq!(
        verify_and_parse(&bytes, "not a signature", &pubkey(), "0.1.0").unwrap_err(),
        ManifestError::BadSignature
    );
}

#[test]
fn newer_min_app_version_is_too_new() {
    let bytes = with_min_version("0.2.0");
    let err = verify_and_parse(&bytes, &sign(&bytes), &pubkey(), "0.1.9").unwrap_err();
    assert_eq!(
        err,
        ManifestError::TooNew {
            min_app_version: "0.2.0".into()
        }
    );
    let ok = verify_and_parse(&bytes, &sign(&bytes), &pubkey(), "0.2.0");
    assert!(ok.is_ok());
}

#[test]
fn too_new_wins_over_unknown_step_types() {
    let text = String::from_utf8(with_min_version("9.0.0"))
        .unwrap()
        .replace("\"type\": \"info\"", "\"type\": \"from_the_future\"");
    let bytes = text.into_bytes();
    let err = verify_and_parse(&bytes, &sign(&bytes), &pubkey(), "0.1.0").unwrap_err();
    assert!(matches!(err, ManifestError::TooNew { .. }));
}

#[test]
fn unknown_step_type_is_malformed() {
    let text = String::from_utf8(manifest_bytes())
        .unwrap()
        .replace("\"type\": \"info\"", "\"type\": \"reformat_everything\"");
    let bytes = text.into_bytes();
    let err = verify_and_parse(&bytes, &sign(&bytes), &pubkey(), "0.1.0").unwrap_err();
    assert!(matches!(err, ManifestError::Malformed(_)), "{err:?}");
}

#[test]
fn bad_version_range_is_malformed() {
    let text = String::from_utf8(manifest_bytes())
        .unwrap()
        .replace(">=1.0 <2.0", "banana");
    let bytes = text.into_bytes();
    let err = verify_and_parse(&bytes, &sign(&bytes), &pubkey(), "0.1.0").unwrap_err();
    assert!(matches!(err, ManifestError::Malformed(_)), "{err:?}");
}

fn parsed() -> Manifest {
    let bytes = manifest_bytes();
    verify_and_parse(&bytes, &sign(&bytes), &pubkey(), "0.1.0").unwrap()
}

#[test]
fn cond_type_follows_the_key() {
    let m = parsed();
    let rule = &m.printers[0].identify[0];
    assert!(matches!(&rule["ssh_banner"], Cond::Exact(s) if s == "Forge-X"));
    match &rule["fw_version"] {
        Cond::Range(r) => {
            assert!(r.matches(&FwVersion::parse("1.5").unwrap()));
            assert!(!r.matches(&FwVersion::parse("2.0").unwrap()));
        }
        other => panic!("expected Range, got {other:?}"),
    }
    assert!(matches!(
        &m.printers[0].identify[1]["plugins"],
        Cond::Contains { contains } if contains == "klipper"
    ));
    assert!(matches!(
        &m.printers[0].recipes[0].when["fw_version"],
        Cond::Range(_)
    ));
}

#[test]
fn steps_deserialize_into_their_variants() {
    let m = parsed();
    let steps = &m.recipes["ad5m_forge_x"];
    assert!(matches!(steps[1].probe(), Some(Probe::Moonraker { .. })));
    assert!(matches!(steps[2].probe(), Some(Probe::Ssh { fact, .. }) if fact == "arch"));
    assert!(matches!(
        &steps[5],
        Step::WaitFor {
            target: WaitTarget::Port(22),
            ..
        }
    ));
    assert!(matches!(&steps[7], Step::SshRun { commits: true, .. }));
    assert!(matches!(
        &steps[8],
        Step::HelixInstall {
            platform: PlatformSel::FromArch { .. },
            ..
        }
    ));
    assert!(matches!(&steps[0], Step::Info { commits: false, .. }));
}

#[test]
fn manifest_round_trips_through_json() {
    let m = parsed();
    let again: Manifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
    assert_eq!(again.printers[0].identify.len(), 2);
    assert!(matches!(
        &again.printers[0].identify[0]["fw_version"],
        Cond::Range(_)
    ));
    assert!(matches!(
        &again.printers[0].identify[1]["plugins"],
        Cond::Contains { contains } if contains == "klipper"
    ));
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn cache(dir: &std::path::Path) -> ManifestCache {
    ManifestCache::new(dir.to_path_buf(), &pubkey(), "0.1.0")
}

#[test]
fn cache_honours_the_30_day_limit() {
    let td = tempdir();
    let dir = td.path().to_path_buf();
    let bytes = manifest_bytes();
    let c = cache(&dir);
    c.store(&bytes, &sign(&bytes), at(1_000_000)).unwrap();
    assert!(c.load(at(1_000_000 + 29 * DAY)).is_some());
    assert!(c.load(at(1_000_000 + 30 * DAY + 1)).is_none());
    assert!(c.load(at(1_000_000 + 30 * DAY)).is_none());
}

#[test]
fn cache_rejects_a_time_in_the_future() {
    let td = tempdir();
    let dir = td.path().to_path_buf();
    let bytes = manifest_bytes();
    let c = cache(&dir);
    c.store(&bytes, &sign(&bytes), at(2_000_000)).unwrap();
    assert!(c.load(at(1_000_000)).is_none());
}

#[test]
fn cache_reverifies_the_signature() {
    let td = tempdir();
    let dir = td.path().to_path_buf();
    let bytes = manifest_bytes();
    let c = cache(&dir);
    c.store(&bytes, &sign(&bytes), at(1_000_000)).unwrap();
    let mut tampered = bytes.clone();
    tampered[10] ^= 0x01;
    std::fs::write(dir.join("manifest.json"), tampered).unwrap();
    assert!(c.load(at(1_000_000 + DAY)).is_none());
}

#[test]
fn empty_cache_loads_nothing() {
    assert!(cache(tempdir().path()).load(at(1)).is_none());
}

fn malformed_after(from: &str, to: &str) {
    let text = String::from_utf8(manifest_bytes()).unwrap();
    assert!(text.contains(from), "fixture lacks {from:?}");
    let bytes = text.replacen(from, to, 1).into_bytes();
    let err = verify_and_parse(&bytes, &sign(&bytes), &pubkey(), "0.1.0").unwrap_err();
    assert!(
        matches!(err, ManifestError::Malformed(_)),
        "{to:?}: {err:?}"
    );
}

#[test]
fn unknown_fields_are_malformed() {
    malformed_after("\"id\": \"ad5m\",", "\"id\": \"ad5m\", \"surprise\": 1,");
    malformed_after(
        "\"type\": \"info\",",
        "\"type\": \"info\", \"surprise\": 1,",
    );
    malformed_after("\"sha256\"", "\"extra\": 1, \"sha256\"");
    malformed_after(
        "\"contains\": \"klipper\"",
        "\"contains\": \"klipper\", \"extra\": 1",
    );
    malformed_after("\"by\": ", "\"extra\": 1, \"by\": ");
}

#[test]
fn misspelled_commits_is_malformed() {
    malformed_after("\"commits\": true", "\"commit\": true");
    malformed_after(
        "\"moonraker\": \"printer_info\"",
        "\"moonraker\": \"printer_info\", \"commit\": true",
    );
    malformed_after("\"fact\": \"arch\"", "\"fact\": \"arch\", \"extra\": 1");
}

#[test]
fn unknown_field_in_platform_selector_is_malformed() {
    malformed_after(
        "\"from_arch\": { \"armv7l\": \"pi32\" }",
        "\"from_arch\": { \"armv7l\": \"pi32\" }, \"x\": 1",
    );
}

#[test]
fn probe_with_two_shapes_is_malformed() {
    malformed_after(
        "\"moonraker\": \"printer_info\"",
        "\"moonraker\": \"printer_info\", \"tcp_port\": 22",
    );
    malformed_after(
        "\"ssh\": \"uname -m\", \"fact\": \"arch\"",
        "\"ssh\": \"uname -m\"",
    );
}

#[test]
fn failed_store_keeps_the_previous_cache() {
    let td = tempdir();
    let dir = td.path().to_path_buf();
    let bytes = manifest_bytes();
    let c = cache(&dir);
    c.store(&bytes, &sign(&bytes), at(1_000_000)).unwrap();

    let newer = with_min_version("0.1.0")
        .into_iter()
        .chain(*b"\n")
        .collect::<Vec<u8>>();
    std::fs::remove_file(dir.join("manifest.fetched_at")).unwrap();
    std::fs::create_dir(dir.join("manifest.fetched_at")).unwrap();
    assert!(c.store(&newer, &sign(&newer), at(1_000_100)).is_err());
    std::fs::remove_dir(dir.join("manifest.fetched_at")).unwrap();
    std::fs::write(dir.join("manifest.fetched_at"), "1000000").unwrap();
    // The signature file and manifest are replaced as a pair; the old pair would also verify.
    assert!(c.load(at(1_000_000 + DAY)).is_some());
}

/// Regenerates the committed test keypair: `cargo test -p helix_installer_core --test manifest -- --ignored regenerate`.
#[test]
#[ignore]
fn regenerate_test_keypair() {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    std::fs::write(
        fixture("test_key.pub"),
        kp.pk.to_box().unwrap().into_string(),
    )
    .unwrap();
    std::fs::write(
        fixture("test_key.sec"),
        kp.sk.to_box(None).unwrap().into_string(),
    )
    .unwrap();
}

#[test]
fn cache_hands_back_the_signed_bytes_it_verified() {
    let td = tempdir();
    let dir = td.path().to_path_buf();
    let bytes = manifest_bytes();
    let sig = sign(&bytes);
    let c = cache(&dir);
    c.store(&bytes, &sig, at(1_000_000)).unwrap();
    let signed = c.load_signed(at(1_000_000 + DAY)).unwrap();
    assert_eq!(signed.bytes, bytes);
    assert_eq!(signed.sig, sig);
    assert_eq!(signed.manifest.min_app_version, "0.1.0");
    assert!(c.load_signed(at(1_000_000 + 31 * DAY)).is_none());
}
