// SPDX-License-Identifier: GPL-3.0-or-later

//! The key the published printer list is signed with, and where the list is published.

/// The public half of the production manifest key (the text of its minisign `.pub` file or the
/// bare base64 line). Empty until `docs/SIGNING.md` has been followed, so until then only a key
/// given in `HELIX_INSTALLER_PUBKEY` verifies anything.
pub const MANIFEST_PUBKEY: &str = "";

/// Where the signed printer list is fetched from; the signature is at `<url>.minisig`.
pub const DEFAULT_MANIFEST_URL: &str = "https://releases.helixscreen.org/installer/manifest.json";

/// The printer list location to use: `override_` when set and non-empty, else the published one.
pub fn manifest_location(override_: Option<String>) -> String {
    override_
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_MANIFEST_URL.to_string())
}
