// SPDX-License-Identifier: GPL-3.0-or-later

//! `manifest-lint <manifest.json> <asset_root> [--allow-unverified]`: checks an unsigned manifest.
//! `manifest-lint --verify <manifest.json> --sig <file> --pubkey <file or key>`: checks a
//! signature the way the app does, so a signature the app would refuse never gets published.

use helix_installer_core::lint::lint_with;
use helix_installer_core::manifest::Manifest;
use helix_installer_core::manifest_sig::verify_signature;
use std::path::Path;
use std::process::ExitCode;

/// Value following `flag` in `args`, if any.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).map(String::as_str)
}

fn verify_mode(args: &[String]) -> ExitCode {
    let (Some(manifest_path), Some(sig_path), Some(key)) = (
        flag_value(args, "--verify"),
        flag_value(args, "--sig"),
        flag_value(args, "--pubkey"),
    ) else {
        eprintln!(
            "usage: manifest-lint --verify <manifest.json> --sig <file> --pubkey <file or key>"
        );
        return ExitCode::from(2);
    };
    let read = |path: &str| {
        std::fs::read(path).map_err(|e| {
            eprintln!("{path}: {e}");
            ExitCode::from(2)
        })
    };
    let (bytes, sig) = match (read(manifest_path), read(sig_path)) {
        (Ok(b), Ok(s)) => (b, String::from_utf8_lossy(&s).into_owned()),
        (Err(c), _) | (_, Err(c)) => return c,
    };
    let key = if Path::new(key).is_file() {
        std::fs::read_to_string(key).unwrap_or_default()
    } else {
        key.to_string()
    };
    match verify_signature(&bytes, &sig, &key) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{manifest_path}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--verify") {
        return verify_mode(&args);
    }
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
