// SPDX-License-Identifier: GPL-3.0-or-later

//! Whole-file replacement that a crash or power loss cannot leave half-written.

use std::io::{self, Write};
use std::path::Path;

/// Writes `bytes` to a temp file beside `path`, syncs it, then renames it over `path`, so a
/// reader sees either the old file or the new one, never part of either.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(path);
    Ok(())
}

/// Makes the rename itself durable. Best effort: the data is already safe in the file.
#[cfg(unix)]
fn sync_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) {}
