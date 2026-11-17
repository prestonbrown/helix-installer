// SPDX-License-Identifier: GPL-3.0-or-later

//! The key the published printer list is signed with, and where the list is published.

/// The public half of the production manifest key (the bare base64 line of its minisign `.pub`
/// file). Changing it means every installed app stops trusting the published list, so it is
/// permanent (`docs/SIGNING.md`). A debug build can be given another key in
/// `HELIX_INSTALLER_PUBKEY`.
pub const MANIFEST_PUBKEY: &str = "RWRxS2qC5t0QSNK4KA0dM6m9eHEejJyZrfK5tqqUL9U5+T8C8eXrHkyY";

/// Where the signed printer list is fetched from; the signature is at `<url>.minisig`.
pub const DEFAULT_MANIFEST_URL: &str = "https://releases.helixscreen.org/installer/manifest.json";

/// The printer list location to use: `override_` when set and non-empty, else the published one.
pub fn manifest_location(override_: Option<String>) -> String {
    override_
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_MANIFEST_URL.to_string())
}

/// The public half of the key release builds are signed with for the in-app updater (the text
/// printed by `npx tauri signer generate`). Installed copies accept only updates signed with its
/// private half, so it is permanent (`docs/SIGNING.md`); blank switches update checks off.
pub const UPDATER_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEQ4QUUzMzg4NkRFMjgyNzgKUldSNGd1SnRpRE91MkkxRkJhbU9yV2xreDFNNVBOcEQ4Wk1qMkhLc1IyOExNOGlHRUlBd2gzNVEK";

/// Where the app looks for a newer release: a Tauri updater manifest.
pub const UPDATE_ENDPOINT: &str = "https://releases.helixscreen.org/installer/app/latest.json";

/// `key` trimmed, or `None` when it is blank, meaning updates are switched off.
pub fn updater_key(key: &str) -> Option<&str> {
    optional_key(key)
}

/// The bare base64 line of HelixScreen's release key (minisign `.pub`, key id A91F86E5AB692257),
/// the key HelixScreen's release workflow signs each channel's release list
/// (`<channel>/manifest.json`, signature at `<channel>/manifest.json.minisig`) with
/// (`docs/SIGNING.md`, "HelixScreen release lists"). Set, every release list must carry a
/// prehashed signature that verifies against it before any hash in it is trusted. Blank, release
/// lists are read unsigned.
pub const RELEASE_PUBKEY: &str = "RWRXImmr5YYfqd5aaV48WN+50k5e83NgKuEtxECC6agLUBkXG7t7AH9S";

/// The key release lists must be signed with; `None` while `RELEASE_PUBKEY` is blank.
pub fn release_key() -> Option<&'static str> {
    optional_key(RELEASE_PUBKEY)
}

/// `key` trimmed, or `None` when it is blank.
fn optional_key(key: &str) -> Option<&str> {
    let key = key.trim();
    (!key.is_empty()).then_some(key)
}
