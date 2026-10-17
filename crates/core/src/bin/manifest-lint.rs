// SPDX-License-Identifier: GPL-3.0-or-later

//! `manifest-lint <manifest.json> <asset_root> [--allow-unverified]`: checks an unsigned manifest.

use helix_installer_core::lint::lint_with;
use helix_installer_core::manifest::Manifest;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut allow_unverified = false;
    let mut paths = Vec::new();
    for arg in std::env::args().skip(1) {
        if arg == "--allow-unverified" {
            allow_unverified = true;
        } else {
            paths.push(arg);
        }
    }
    let [manifest_path, asset_root] = paths.as_slice() else {
        eprintln!("usage: manifest-lint <manifest.json> <asset_root> [--allow-unverified]");
        return ExitCode::from(2);
    };
    let bytes = match std::fs::read(manifest_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{manifest_path}: {e}");
            return ExitCode::from(2);
        }
    };
    let manifest: Manifest = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{manifest_path}: {e}");
            return ExitCode::from(2);
        }
    };
    let errors = lint_with(&manifest, Path::new(asset_root), allow_unverified);
    for e in &errors {
        println!("{e}");
    }
    if errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
