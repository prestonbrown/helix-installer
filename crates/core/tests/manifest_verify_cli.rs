// SPDX-License-Identifier: GPL-3.0-or-later

//! The `manifest-lint --verify` mode CI uses to prove a signature is one the app accepts.

use helix_installer_core::keys::{manifest_location, DEFAULT_MANIFEST_URL};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
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

fn verify(manifest: &Path, sig: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_manifest-lint"))
        .arg("--verify")
        .arg(manifest)
        .arg("--sig")
        .arg(sig)
        .arg("--pubkey")
        .arg(fixture("test_key.pub"))
        .output()
        .unwrap()
}

#[test]
fn verify_accepts_a_good_signature_and_rejects_a_tampered_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("manifest.json");
    let sig = dir.path().join("manifest.json.minisig");
    let bytes = std::fs::read(fixture("manifest_min.json")).unwrap();
    std::fs::write(&manifest, &bytes).unwrap();
    std::fs::write(&sig, sign(&bytes)).unwrap();
    assert!(verify(&manifest, &sig).status.success());

    std::fs::write(&manifest, [bytes.as_slice(), b" "].concat()).unwrap();
    assert!(!verify(&manifest, &sig).status.success());
}

#[test]
fn verify_rejects_a_legacy_signature() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("manifest.json");
    let sig = dir.path().join("manifest.json.minisig");
    let bytes = std::fs::read(fixture("manifest_min.json")).unwrap();
    std::fs::write(&manifest, &bytes).unwrap();
    // A legacy ("Ed") signature carries algorithm bytes "Ed" where a prehashed one has "ED".
    let prehashed = sign(&bytes);
    let mut lines: Vec<String> = prehashed.lines().map(String::from).collect();
    let decoded = minisign_b64_decode(&lines[1]);
    assert_eq!(&decoded[..2], b"ED");
    let mut legacy = decoded.clone();
    legacy[..2].copy_from_slice(b"Ed");
    lines[1] = minisign_b64_encode(&legacy);
    std::fs::write(&sig, lines.join("\n") + "\n").unwrap();
    assert!(!verify(&manifest, &sig).status.success());
}

fn minisign_b64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = B64.iter().position(|b| *b == c).unwrap() as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

fn minisign_b64_encode(b: &[u8]) -> String {
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |a, (i, x)| a | (*x as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        for _ in chunk.len()..3 {
            out.push('=');
        }
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[test]
fn manifest_location_defaults_to_the_published_list_and_honours_an_override() {
    assert_eq!(
        DEFAULT_MANIFEST_URL,
        "https://releases.helixscreen.org/installer/manifest.json"
    );
    assert_eq!(manifest_location(None), DEFAULT_MANIFEST_URL);
    assert_eq!(manifest_location(Some(String::new())), DEFAULT_MANIFEST_URL);
    assert_eq!(manifest_location(Some("/tmp/m.json".into())), "/tmp/m.json");
}
