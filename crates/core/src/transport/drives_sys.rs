// SPDX-License-Identifier: GPL-3.0-or-later

//! The PC's real drives, read through `sysinfo`, and the platform's own eject command.
//!
//! `removable` is `sysinfo`'s flag as each platform reports it: on Linux a device that has a
//! `/dev/disk/by-id/usb-*` link, on macOS a volume that is ejectable, removable or not internal,
//! on Windows a drive whose type is `DRIVE_REMOVABLE`. `system` is decided here and fails
//! closed: a drive is system unless it is known to hold none of the places listed in
//! `system_anchors`.

use super::{Drive, Drives, Result, TransportError};
use crate::fs_atomic::sync_dir;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default, Clone, Copy)]
pub struct SysDrives;

impl SysDrives {
    pub fn new() -> Self {
        SysDrives
    }
}

/// True for a name that puts exactly one file in the drive's root and that FAT32 under every OS
/// can hold: not empty, no path separator, drive prefix, control character or any of
/// `* ? " < > |`, no trailing dot or space, and not a Windows device name (`CON`, `PRN`, `AUX`,
/// `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, with or without an extension, in any case).
pub fn is_plain_file_name(name: &str) -> bool {
    const FORBIDDEN: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    if name.is_empty() || name.ends_with(['.', ' ']) {
        return false;
    }
    if name.chars().any(|c| c < ' ' || FORBIDDEN.contains(&c)) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name);
    let device = ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|d| stem.eq_ignore_ascii_case(d));
    let numbered = stem.len() == 4
        && ["COM", "LPT"]
            .iter()
            .any(|p| stem.get(..3).is_some_and(|s| s.eq_ignore_ascii_case(p)))
        && matches!(stem.as_bytes()[3], b'1'..=b'9');
    !device && !numbered
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

static PART_SEQ: AtomicU64 = AtomicU64::new(0);

/// A part-file name no other file on the stick has: unique per process and per call, and
/// created with `create_new`, so a file the user already has is never truncated or removed.
fn part_name(name: &str) -> String {
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{name}.{}.{seq}.part", std::process::id())
}

/// Copies `src` into a new part file, syncs it to the device, then renames it into place, so
/// the final name never holds a partial file. A failed copy removes the part file, which is
/// only ever the one this call created.
fn copy_through_part(src: &Path, dest: &Path, part: &Path) -> std::io::Result<()> {
    let mut from = std::fs::File::open(src)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    uncached::write_options(&mut opts);
    let mut to = opts.open(part)?;
    let copied = uncached::after_open(&to)
        .and_then(|()| std::io::copy(&mut from, &mut to))
        .and_then(|_| to.sync_all())
        .and_then(|()| uncached::after_sync(&to));
    drop(to);
    if let Err(e) = copied.and_then(|()| std::fs::rename(part, dest)) {
        let _ = std::fs::remove_file(part);
        return Err(e);
    }
    sync_dir(dest);
    Ok(())
}

/// Keeps the readback hash coming from the stick rather than from the OS's copy of what was
/// written, as far as each platform allows.
///
/// - Linux: the written file is synced and its pages dropped (`POSIX_FADV_DONTNEED`), and the
///   readback syncs and drops them again before reading, so it reads the device.
/// - macOS: writes and the readback use `F_NOCACHE`, so neither keeps pages in the cache;
///   pages some other reader cached in between can still serve the readback.
/// - Windows: the copy is written with `FILE_FLAG_WRITE_THROUGH` and read back with
///   `FILE_FLAG_NO_BUFFERING`, which reads the device. A volume whose sector size breaks the
///   aligned reads falls back to a buffered read, which proves only what the filesystem holds.
///
/// None of them can see past a stick's own controller cache.
#[cfg(target_os = "linux")]
mod uncached {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    pub fn write_options(_opts: &mut OpenOptions) {}

    pub fn after_open(_f: &File) -> io::Result<()> {
        Ok(())
    }

    pub fn after_sync(f: &File) -> io::Result<()> {
        // SAFETY: the descriptor is open for the duration of the call.
        match unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) } {
            0 => Ok(()),
            e => Err(io::Error::from_raw_os_error(e)),
        }
    }

    pub fn hash(path: &Path) -> io::Result<String> {
        let mut f = File::open(path)?;
        f.sync_all()?;
        after_sync(&f)?;
        crate::cache::hash_reader(&mut f)
    }
}

#[cfg(target_os = "macos")]
mod uncached {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    pub fn write_options(_opts: &mut OpenOptions) {}

    pub fn after_open(f: &File) -> io::Result<()> {
        // SAFETY: the descriptor is open for the duration of the call.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn after_sync(_f: &File) -> io::Result<()> {
        Ok(())
    }

    pub fn hash(path: &Path) -> io::Result<String> {
        let mut f = File::open(path)?;
        after_open(&f)?;
        crate::cache::hash_reader(&mut f)
    }
}

#[cfg(windows)]
mod uncached {
    use sha2::{Digest, Sha256};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    /// Unbuffered reads must start on, span whole multiples of, and land in a buffer aligned to
    /// the volume's sector size; 4096 is a multiple of every sector size a USB stick reports.
    const ALIGN: usize = 4096;
    const CHUNK: usize = 64 * 1024;

    pub fn write_options(opts: &mut OpenOptions) {
        opts.custom_flags(FILE_FLAG_WRITE_THROUGH);
    }

    pub fn after_open(_f: &File) -> io::Result<()> {
        Ok(())
    }

    pub fn after_sync(_f: &File) -> io::Result<()> {
        Ok(())
    }

    pub fn hash(path: &Path) -> io::Result<String> {
        unbuffered(path).or_else(|_| crate::cache::hash_file(path))
    }

    fn unbuffered(path: &Path) -> io::Result<String> {
        let mut f = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(path)?;
        let mut raw = vec![0u8; CHUNK + ALIGN];
        let start = raw.as_ptr().align_offset(ALIGN);
        let buf = raw
            .get_mut(start..start + CHUNK)
            .ok_or_else(|| io::Error::other("could not align the read buffer"))?;
        let mut hasher = Sha256::new();
        loop {
            let n = f.read(buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(crate::cache::hex(&hasher.finalize()))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod uncached {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    pub fn write_options(_opts: &mut OpenOptions) {}

    pub fn after_open(_f: &File) -> io::Result<()> {
        Ok(())
    }

    pub fn after_sync(_f: &File) -> io::Result<()> {
        Ok(())
    }

    pub fn hash(path: &Path) -> io::Result<String> {
        crate::cache::hash_file(path)
    }
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

    fn system_drives_known(&self) -> bool {
        system_anchors().is_some()
    }

    async fn copy_to(&self, drive: &Drive, src: &Path, name: &str) -> Result<()> {
        if !is_plain_file_name(name) {
            return Err(TransportError::Io(format!(
                "not a plain file name: {name:?}"
            )));
        }
        let dest = drive.mount.join(name);
        let part = drive.mount.join(part_name(name));
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
        blocking(move || uncached::hash(&path).map_err(io)).await
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
