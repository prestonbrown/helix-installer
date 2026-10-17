// SPDX-License-Identifier: GPL-3.0-or-later

//! The only path by which firmware and release files reach the user's PC:
//! every file is hash-verified before it is visible in the cache.

use super::{StepCtx, StepOutcome};
use crate::cache::{cached_path, hash_file, is_sha256_hex, part_path};
use crate::manifest::Artifact;
use crate::transport::{Env, TransportError};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DownloadError {
    #[error("download failed: {0}")]
    Network(TransportError),
    #[error("downloaded file does not match its expected hash (expected {expected}, got {got})")]
    HashMismatch { expected: String, got: String },
    #[error("file error: {0}")]
    Io(String),
}

fn io(e: std::io::Error) -> DownloadError {
    DownloadError::Io(e.to_string())
}

/// Returns the path of the verified file `<cache_dir>/<sha256>`, downloading
/// it first unless an intact copy is already cached. The expected hash comes
/// from the caller's artifact; this function trusts no other source.
pub async fn fetch_artifact(
    env: &Env,
    a: &Artifact,
    cache_dir: &Path,
    progress: &(dyn Fn(u64, Option<u64>) + Sync),
) -> Result<PathBuf, DownloadError> {
    if !is_sha256_hex(&a.sha256) {
        return Err(DownloadError::Io(format!(
            "expected hash is not 64 lowercase hex digits: {}",
            a.sha256
        )));
    }
    std::fs::create_dir_all(cache_dir).map_err(io)?;
    let final_path = cached_path(cache_dir, &a.sha256);

    if final_path.exists() {
        if hash_file(&final_path).map_err(io)? == a.sha256 {
            return Ok(final_path);
        }
        std::fs::remove_file(&final_path).map_err(io)?;
    }

    let part = part_path(cache_dir, &a.sha256);
    if let Err(e) = env.fetch.get(&a.url, &part, progress).await {
        let _ = std::fs::remove_file(&part);
        return Err(DownloadError::Network(e));
    }
    let got = match hash_file(&part) {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(io(e));
        }
    };
    if got != a.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(DownloadError::HashMismatch {
            expected: a.sha256.clone(),
            got,
        });
    }
    std::fs::rename(&part, &final_path).map_err(io)?;
    Ok(final_path)
}

/// The `download` step. Its first run only reports what is being fetched, so the UI shows it
/// while the download runs.
pub async fn run(ctx: &mut StepCtx<'_>, name: &str) -> StepOutcome {
    let Some(a) = ctx.manifest.artifacts.get(name) else {
        return StepOutcome::failed(format!("artifact {name} is not in the manifest"));
    };
    if ctx.calls == 0 {
        return StepOutcome::Progress {
            title: format!("Downloading {}", a.filename.as_deref().unwrap_or(name)),
            detail: None,
            fraction: None,
        };
    }
    let Some(dir) = ctx.cache_dir else {
        return StepOutcome::failed("no download cache directory is set");
    };
    match fetch_artifact(ctx.env, a, dir, &|_, _| {}).await {
        Ok(_) => StepOutcome::Done,
        Err(e) => StepOutcome::failed(format!("{name}: {e}")),
    }
}
