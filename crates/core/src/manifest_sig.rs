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
