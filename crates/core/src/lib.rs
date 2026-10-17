// SPDX-License-Identifier: GPL-3.0-or-later

//! Installer logic for HelixScreen. This crate must not depend on Tauri.

pub mod facts;
pub mod identify;
pub mod lint;
pub mod manifest;
pub mod manifest_sig;
pub mod version;

/// Crate version, reported by the app shell.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
