// SPDX-License-Identifier: GPL-3.0-or-later

//! `manifest-lint <manifest.json> <asset_root> [--allow-unverified | --signing]`: checks an
//! unsigned manifest; `--signing` also refuses a recipe still awaiting its hardware run.
//! `manifest-lint --digests <manifest.json>`: prints each recipe's id and the behavior digest a
//! verified run records for it.
//! `manifest-lint --verify <manifest.json> --sig <file> --pubkey <file or key>`: checks a
//! signature the way the app does, so a signature the app would refuse never gets published.

use helix_installer_core::lint::{behavior_sha256, lint_for_signing, lint_with};
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

fn read_manifest(path: &str) -> Result<Manifest, ExitCode> {
    let bytes = std::fs::read(path).map_err(|e| {
        eprintln!("{path}: {e}");
        ExitCode::from(2)
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        eprintln!("{path}: {e}");
        ExitCode::from(2)
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--verify") {
        return verify_mode(&args);
    }
    if let Some(path) = flag_value(&args, "--digests") {
        return match read_manifest(path) {
            Ok(m) => {
                for (id, steps) in &m.recipes {
                    println!("{id} {}", behavior_sha256(steps));
                }
                ExitCode::SUCCESS
            }
            Err(c) => c,
        };
    }
    let mut allow_unverified = false;
    let mut signing = false;
    let mut paths = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--allow-unverified" => allow_unverified = true,
            "--signing" => signing = true,
            _ => paths.push(arg),
        }
    }
    let ([manifest_path, asset_root], false) = (paths.as_slice(), allow_unverified && signing)
    else {
        eprintln!(
            "usage: manifest-lint <manifest.json> <asset_root> [--allow-unverified | --signing]"
        );
        return ExitCode::from(2);
    };
    let manifest = match read_manifest(manifest_path) {
        Ok(m) => m,
        Err(c) => return c,
    };
    let root = Path::new(asset_root);
    let errors = if signing {
        lint_for_signing(&manifest, root)
    } else {
        lint_with(&manifest, root, allow_unverified)
    };
    for e in &errors {
        println!("{e}");
    }
    if errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
