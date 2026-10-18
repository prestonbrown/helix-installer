// SPDX-License-Identifier: GPL-3.0-or-later

//! The PC's real drives, read through `sysinfo`, and the platform's own eject command.
//!
//! `removable` is `sysinfo`'s flag as each platform reports it: on Linux a device that has a
//! `/dev/disk/by-id/usb-*` link, on macOS a volume that is ejectable, removable or not internal,
//! on Windows a drive whose type is `DRIVE_REMOVABLE`. `system` is decided here and fails
//! closed: a drive is system unless it is known to hold none of the places listed in
//! `system_anchors`.

use super::{Drive, Drives, Result, TransportError};
use crate::cache::hash_file;
use crate::fs_atomic::sync_dir;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Copy)]
pub struct SysDrives;

impl SysDrives {
    pub fn new() -> Self {
        SysDrives
    }
}

/// True for a name that puts exactly one file in the drive's root: not empty, not `.` or `..`,
/// and free of path separators, drive prefixes and NUL.
pub fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])
}

/// The places whose drive must never be written: the OS root, the user's home and the
/// directory of the running executable, canonicalized. `None` when any of them cannot be
/// determined.
pub fn system_anchors() -> Option<Vec<PathBuf>> {
    let exe = std::env::current_exe().ok()?;
    let anchors = [os_root()?, home_dir()?, exe.parent()?.to_path_buf()];
    anchors.iter().map(|p| p.canonicalize().ok()).collect()
}

/// True when `mount` is one of `anchors` or contains one; with no anchors known, every drive is.
pub fn is_system(mount: &Path, anchors: Option<&[PathBuf]>) -> bool {
    match anchors {
        Some(anchors) => anchors.iter().any(|a| a.starts_with(mount)),
        None => true,
    }
}

#[cfg(windows)]
fn os_root() -> Option<PathBuf> {
    non_empty_var("SystemRoot")
}

#[cfg(not(windows))]
fn os_root() -> Option<PathBuf> {
    Some(PathBuf::from("/"))
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    non_empty_var("USERPROFILE")
}

#[cfg(not(windows))]
fn home_dir() -> Option<PathBuf> {
    non_empty_var("HOME")
}

fn non_empty_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn to_drive(d: &sysinfo::Disk, anchors: Option<&[PathBuf]>) -> Drive {
    let mount = d.mount_point().to_path_buf();
    let system = match mount.canonicalize() {
        Ok(m) => is_system(&m, anchors),
        Err(_) => true,
    };
    Drive {
        id: mount.to_string_lossy().into_owned(),
        label: label(d),
        fs: d.file_system().to_string_lossy().into_owned(),
        size_bytes: d.total_space(),
        free_bytes: d.available_space(),
        removable: d.is_removable(),
        system,
        mount,
    }
}

/// The volume name where the platform gives one; Linux gives the device node instead, so there
/// it is the mount directory's name, which desktop automounters take from the volume label.
fn label(d: &sysinfo::Disk) -> String {
    let name = d.name().to_string_lossy();
    if !name.is_empty() && !name.starts_with("/dev/") {
        return name.into_owned();
    }
    let mount = d.mount_point();
    mount.file_name().map_or_else(
        || mount.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn io(e: std::io::Error) -> TransportError {
    TransportError::Io(e.to_string())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| TransportError::Io(e.to_string()))?
}

/// Copies `src` to `<dest>.part`, syncs it to the device, then renames it into place, so the
/// final name never holds a partial file. A failed copy removes the `.part`.
fn copy_through_part(src: &Path, dest: &Path, part: &Path) -> std::io::Result<()> {
    let copied = std::fs::copy(src, part)
        .and_then(|_| std::fs::OpenOptions::new().write(true).open(part))
        .and_then(|f| f.sync_all());
    if let Err(e) = copied.and_then(|()| std::fs::rename(part, dest)) {
        let _ = std::fs::remove_file(part);
        return Err(e);
    }
    sync_dir(dest);
    Ok(())
}

#[async_trait]
impl Drives for SysDrives {
    fn list_drives(&self) -> Result<Vec<Drive>> {
        let anchors = system_anchors();
        let disks = sysinfo::Disks::new_with_refreshed_list();
        Ok(disks
            .list()
            .iter()
            .map(|d| to_drive(d, anchors.as_deref()))
            .collect())
    }

    async fn copy_to(&self, drive: &Drive, src: &Path, name: &str) -> Result<()> {
        if !is_plain_file_name(name) {
            return Err(TransportError::Io(format!(
                "not a plain file name: {name:?}"
            )));
        }
        let dest = drive.mount.join(name);
        let part = drive.mount.join(format!("{name}.part"));
        let src = src.to_path_buf();
        blocking(move || copy_through_part(&src, &dest, &part).map_err(io)).await
    }

    async fn sha256_of(&self, drive: &Drive, name: &str) -> Result<String> {
        if !is_plain_file_name(name) {
            return Err(TransportError::Io(format!(
                "not a plain file name: {name:?}"
            )));
        }
        let path = drive.mount.join(name);
        blocking(move || hash_file(&path).map_err(io)).await
    }

    async fn eject(&self, drive: &Drive) -> Result<()> {
        let drive = drive.clone();
        blocking(move || eject_blocking(&drive)).await
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| TransportError::Io(format!("{program}: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(TransportError::Io(format!(
            "{program} failed: {}",
            stderr.trim()
        )))
    }
}

#[cfg(target_os = "linux")]
fn eject_blocking(drive: &Drive) -> Result<()> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let device = disks
        .list()
        .iter()
        .find(|d| d.mount_point() == drive.mount)
        .map(|d| d.name().to_os_string())
        .ok_or_else(|| TransportError::Io("the drive is no longer mounted".into()))?;
    let dev = device.as_os_str();
    run("udisksctl", &["unmount".as_ref(), "-b".as_ref(), dev])?;
    // Unmounting has already flushed every file; powering off only lets the stick be pulled
    // without the desktop warning about it.
    let _ = run("udisksctl", &["power-off".as_ref(), "-b".as_ref(), dev]);
    Ok(())
}

#[cfg(target_os = "macos")]
fn eject_blocking(drive: &Drive) -> Result<()> {
    run("diskutil", &["eject".as_ref(), drive.mount.as_os_str()])
}

#[cfg(windows)]
fn eject_blocking(drive: &Drive) -> Result<()> {
    let mount = drive.mount.to_string_lossy();
    let b = mount.as_bytes();
    // Only a bare drive root reaches the PowerShell command line.
    let letter = match b {
        [l, b':', rest @ ..]
            if l.is_ascii_alphabetic() && rest.iter().all(|c| *c == b'\\' || *c == b'/') =>
        {
            *l as char
        }
        _ => {
            return Err(TransportError::Io(format!(
                "not a drive letter root: {mount}"
            )))
        }
    };
    let script = format!(
        "(New-Object -comObject Shell.Application).Namespace(17).ParseName('{letter}:').InvokeVerb('Eject')"
    );
    run(
        "powershell",
        &[
            "-NoProfile".as_ref(),
            "-NonInteractive".as_ref(),
            "-Command".as_ref(),
            script.as_ref(),
        ],
    )?;
    // InvokeVerb reports nothing, so the eject worked only once Windows stops listing the drive.
    for _ in 0..20 {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        if !disks.list().iter().any(|d| d.mount_point() == drive.mount) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err(TransportError::Io("Windows did not eject the drive".into()))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn eject_blocking(_drive: &Drive) -> Result<()> {
    Err(TransportError::Io(
        "ejecting is not supported on this system".into(),
    ))
}
