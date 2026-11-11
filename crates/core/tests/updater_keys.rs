// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::keys::{updater_key, UPDATE_ENDPOINT};

#[test]
fn an_empty_or_blank_updater_key_disables_updates() {
    assert_eq!(updater_key(""), None);
    assert_eq!(updater_key("  \n\t"), None);
}

#[test]
fn a_set_updater_key_is_used_trimmed() {
    assert_eq!(updater_key(" dW50cnVzdGVk\n"), Some("dW50cnVzdGVk"));
}

#[test]
fn updates_are_fetched_from_the_release_bucket() {
    assert_eq!(
        UPDATE_ENDPOINT,
        "https://releases.helixscreen.org/installer/app/latest.json"
    );
}

#[test]
fn a_set_release_key_is_a_minisign_public_key() {
    // Blank until the release key exists (docs/SIGNING.md); once pasted in it must decode, or
    // every release list would be refused.
    if let Some(key) = helix_installer_core::keys::release_key() {
        assert!(
            minisign_verify::PublicKey::from_base64(key).is_ok(),
            "RELEASE_PUBKEY is not the base64 line of a minisign .pub file: {key}"
        );
    }
}
