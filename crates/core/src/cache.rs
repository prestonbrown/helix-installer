// SPDX-License-Identifier: GPL-3.0-or-later

//! Content-addressed download cache: a file's name is its sha256, so a name
//! can never refer to different bytes.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static PART_SEQ: AtomicU64 = AtomicU64::new(0);

/// True for the 64 lowercase hex digits a manifest sha256 must be; also what
/// makes the value safe to use as a file name.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn cached_path(cache_dir: &Path, sha256: &str) -> PathBuf {
    cache_dir.join(sha256)
}

/// A part-file path no other writer uses: unique per process and per call, so two runs
/// fetching one artifact never write, truncate or rename each other's partial bytes.
pub fn part_path(cache_dir: &Path, sha256: &str) -> PathBuf {
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    cache_dir.join(format!("{sha256}.{}.{seq}.part", std::process::id()))
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex sha256 of the file, hashed in chunks so large images never
/// sit in memory whole.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    hash_reader(&mut std::fs::File::open(path)?)
}

/// `hash_file` on a blocking thread, so hashing a large image never stalls the async runtime.
pub async fn hash_file_blocking(path: &Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || hash_file(&path))
        .await
        .map_err(std::io::Error::other)?
}

/// Lowercase hex sha256 of everything `r` yields, read in 64 KiB chunks.
pub fn hash_reader(r: &mut impl Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}
