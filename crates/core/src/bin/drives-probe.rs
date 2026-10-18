// SPDX-License-Identifier: GPL-3.0-or-later

//! `drives-probe`: prints every drive the installer sees, with the flags that decide whether it
//! is offered for writing. Read-only: it never writes, mounts or ejects anything.

use helix_installer_core::steps::write_stick::is_fat32;
use helix_installer_core::transport::drives_sys::{system_anchors, SysDrives};
use helix_installer_core::transport::Drives;
use std::process::ExitCode;

fn main() -> ExitCode {
    match system_anchors() {
        Some(a) => {
            for p in a {
                println!("system anchor: {}", p.display());
            }
        }
        None => println!("system anchors unknown: every drive counts as system"),
    }
    let drives = match SysDrives::new().list_drives() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("could not list drives: {e}");
            return ExitCode::FAILURE;
        }
    };
    for d in drives {
        let offered = d.removable && !d.system;
        println!(
            "{}\n  label={:?} mount={} fs={} fat32={} size={} free={} removable={} system={} offered={}",
            d.id,
            d.label,
            d.mount.display(),
            d.fs,
            is_fat32(&d.fs),
            d.size_bytes,
            d.free_bytes,
            d.removable,
            d.system,
            offered
        );
    }
    ExitCode::SUCCESS
}
