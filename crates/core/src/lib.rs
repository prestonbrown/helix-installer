// SPDX-License-Identifier: GPL-3.0-or-later

//! Installer logic for HelixScreen. This crate must not depend on Tauri.

pub mod app_log;
pub mod bundle;
pub mod cache;
pub mod discovery;
pub mod facts;
mod fs_atomic;
pub mod identify;
pub mod keys;
pub mod lint;
pub mod manifest;
pub mod manifest_sig;
pub mod record;
pub mod run_state;
pub mod runner;
pub mod steps;
pub mod transport;
pub mod version;

/// Crate version, reported by the app shell.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
