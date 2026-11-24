// SPDX-License-Identifier: GPL-3.0-or-later

//! Minisign verification of the manifest bytes.

use crate::manifest::ManifestError;
use minisign_verify::{PublicKey, Signature};

/// Checks `sig` (the text of a `.minisig` file) over `bytes` against `pubkey`, which is either
/// the text of a minisign `.pub` file or its bare base64 key line.
pub fn verify_signature(bytes: &[u8], sig: &str, pubkey: &str) -> Result<(), ManifestError> {
    let key = PublicKey::decode(pubkey)
        .or_else(|_| PublicKey::from_base64(pubkey.trim()))
        .map_err(|_| ManifestError::BadSignature)?;
    let sig = Signature::decode(sig).map_err(|_| ManifestError::BadSignature)?;
    key.verify(bytes, &sig, false)
        .map_err(|_| ManifestError::BadSignature)
}

/// Why a channel's release list was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelSigError {
    /// The signature does not verify over the bytes, or is malformed.
    BadSignature,
    /// The signature verifies, but its trusted comment is not the one `channel`'s list is signed
    /// with.
    WrongChannel { expected: String, found: String },
}

/// The trusted comment the release workflow signs `channel`'s list with.
pub fn channel_trusted_comment(channel: &str) -> String {
    format!("file:manifest.json channel:{channel}")
}

/// `verify_signature`, then requires the signature's trusted comment to be exactly
/// `file:manifest.json channel:<channel>`. The comment is read only after the signature verified:
/// minisign's global signature covers it, so it cannot have been edited since signing.
pub fn verify_channel_signature(
    bytes: &[u8],
    sig: &str,
    pubkey: &str,
    channel: &str,
) -> Result<(), ChannelSigError> {
    verify_signature(bytes, sig, pubkey).map_err(|_| ChannelSigError::BadSignature)?;
    let found = Signature::decode(sig)
        .map_err(|_| ChannelSigError::BadSignature)?
        .trusted_comment()
        .to_string();
    let expected = channel_trusted_comment(channel);
    if found == expected {
        Ok(())
    } else {
        Err(ChannelSigError::WrongChannel { expected, found })
    }
}
