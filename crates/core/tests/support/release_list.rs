// SPDX-License-Identifier: GPL-3.0-or-later

//! HelixScreen release lists published the way the release workflow publishes them: each
//! channel's `manifest.json` with a prehashed minisign signature beside it, made with a test
//! release key (`fixtures/release_test_key.*`, never the real one).

#![allow(dead_code)]

use helix_installer_core::manifest::Channel;
use helix_installer_core::steps::helix_install::releases_manifest_url;
use helix_installer_core::transport::fake::FakeFetch;
use std::io::Cursor;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The public half of the test release key, as a runner is given it.
pub fn release_pubkey() -> Option<String> {
    Some(std::fs::read_to_string(fixture("release_test_key.pub")).unwrap())
}

/// A prehashed minisign signature over `bytes` with the secret key in `fixtures/<key_file>`.
pub fn sign_with(key_file: &str, bytes: &[u8]) -> String {
    let text = std::fs::read_to_string(fixture(key_file)).unwrap();
    let sk = minisign::SecretKeyBox::from_string(&text)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, Cursor::new(bytes), None, None)
        .unwrap()
        .into_string()
}

/// The URL a release list's signature is published at.
pub fn signature_url(channel: Channel) -> String {
    format!("{}.minisig", releases_manifest_url(channel))
}

/// Serves `bytes` as `channel`'s release list, signed with the test release key.
pub fn publish_release_list(fetch: &FakeFetch, channel: Channel, bytes: Vec<u8>) {
    fetch.add(
        &signature_url(channel),
        sign_with("release_test_key.sec", &bytes).into_bytes(),
    );
    fetch.add(&releases_manifest_url(channel), bytes);
}

/// Writes a new test release key pair to `fixtures/release_test_key.*`; run through the ignored
/// `regenerate_release_test_keypair` test.
pub fn regenerate_release_test_keypair() {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    std::fs::write(
        fixture("release_test_key.pub"),
        kp.pk.to_box().unwrap().into_string(),
    )
    .unwrap();
    std::fs::write(
        fixture("release_test_key.sec"),
        kp.sk.to_box(None).unwrap().into_string(),
    )
    .unwrap();
}
