// SPDX-License-Identifier: GPL-3.0-or-later

//! The app's own log: one file per launch in the platform log directory, every line stamped
//! with UTC time to the millisecond, passwords stripped as the support bundle strips them.
//! Only the newest `KEEP_LOGS` files are kept and each stops at `MAX_LOG_BYTES`, so the logs
//! never take more than about 5 MB.

use crate::bundle::redact;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Launches whose logs are kept, this one included.
pub const KEEP_LOGS: usize = 5;
/// Where one launch's log stops growing.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;
const PREFIX: &str = "installer-";
const SUFFIX: &str = ".log";
const FULL: &str = "log is full; later lines are not written";

pub struct AppLog {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    file: File,
    written: u64,
    full: bool,
}

impl AppLog {
    /// Starts this launch's log in `dir`, then removes all but the newest `KEEP_LOGS` logs.
    pub fn open(dir: &Path, now: SystemTime) -> io::Result<AppLog> {
        std::fs::create_dir_all(dir)?;
        let ms = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let mut n = 0u32;
        let (path, file) = loop {
            let name = match n {
                0 => format!("{PREFIX}{ms:013}{SUFFIX}"),
                n => format!("{PREFIX}{ms:013}-{n}{SUFFIX}"),
            };
            let path = dir.join(name);
            match OpenOptions::new().append(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(e),
            }
        };
        prune(dir, &path);
        Ok(AppLog {
            path,
            inner: Mutex::new(Inner {
                file,
                written: 0,
                full: false,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `text`, one stamped line per line of it, with every one of `secrets` removed.
    /// A write that fails is dropped: the log must never stop the install it describes.
    pub fn write(&self, now: SystemTime, text: &str, secrets: &[&str]) {
        let stamp = timestamp(now);
        let text = redact(text, secrets);
        let mut out = String::new();
        for line in text.lines() {
            out.push_str(&format!("{stamp} {line}\n"));
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.full {
            return;
        }
        if inner.written + out.len() as u64 > MAX_LOG_BYTES {
            inner.full = true;
            out = format!("{stamp} {FULL}\n");
        }
        if inner.file.write_all(out.as_bytes()).is_ok() {
            inner.written += out.len() as u64;
        }
    }

    /// What this launch has logged so far.
    pub fn contents(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }
}

/// Removes the oldest logs past `KEEP_LOGS`, never `current`. Names sort by launch time.
fn prune(dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
        })
        .collect();
    logs.sort();
    let excess = logs.len().saturating_sub(KEEP_LOGS);
    for old in logs.iter().take(excess).filter(|p| p.as_path() != current) {
        let _ = std::fs::remove_file(old);
    }
}

/// `2026-10-20T15:28:28.581Z`.
pub fn timestamp(t: SystemTime) -> String {
    let ms = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
