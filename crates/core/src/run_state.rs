// SPDX-License-Identifier: GPL-3.0-or-later

//! What a run has done so far, written to disk after every step so an install outlives the app.
//! Nothing in this tree may hold a secret: the SSH password lives only in the runner's memory.

use crate::cache::sha256_hex;
use crate::facts::Facts;
use crate::fs_atomic::write_atomic;
use crate::manifest::{verify_and_parse, Manifest, Step};
use crate::transport::{Host, RemoteJob};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// Facts that name the physical printer. Once the point of no return is passed, a probe that
/// reports a different value for any of them means the install is talking to another printer.
pub const IDENTITY_KEYS: [&str; 2] = ["model", "serial"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Running,
    Done,
    Refused {
        doc: String,
        reason: String,
    },
    /// Stopped at `step_index`; the next advance retries that step.
    Failed {
        reason: String,
        help: Option<String>,
    },
}

impl RunStatus {
    /// Done and Refused runs have nothing left to resume.
    pub fn is_finished(&self) -> bool {
        matches!(self, RunStatus::Done | RunStatus::Refused { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub at_unix: u64,
    pub step: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub run_id: String,
    pub printer_id: String,
    pub recipe_id: String,
    /// Every step talks to this host and no other.
    pub host: Host,
    /// The step that runs next.
    pub step_index: usize,
    pub facts: Facts,
    /// Set once the recipe has passed its point of no return.
    pub committed: bool,
    /// The `IDENTITY_KEYS` facts as they stood when the committing step completed.
    #[serde(default)]
    pub identity: Option<Facts>,
    /// sha256 of the manifest and of the recipe's canonical JSON the run started with. A run
    /// only continues against the same recipe it was planned with.
    #[serde(default)]
    pub manifest_sha256: Option<String>,
    #[serde(default)]
    pub recipe_sha256: Option<String>,
    /// The step that was dispatched and has not completed: if it is still current when the run
    /// resumes or retries, it may already have acted on the printer.
    #[serde(default)]
    pub started_step: Option<usize>,
    /// A detached job on the printer that the current step is following.
    pub job: Option<RemoteJob>,
    pub log: Vec<LogEntry>,
    #[serde(default)]
    pub status: RunStatus,
}

impl RunState {
    pub fn new(
        run_id: impl Into<String>,
        printer_id: impl Into<String>,
        recipe_id: impl Into<String>,
        host: Host,
        facts: Facts,
    ) -> RunState {
        RunState {
            run_id: run_id.into(),
            printer_id: printer_id.into(),
            recipe_id: recipe_id.into(),
            host,
            step_index: 0,
            facts,
            committed: false,
            identity: None,
            manifest_sha256: None,
            recipe_sha256: None,
            started_step: None,
            job: None,
            log: Vec::new(),
            status: RunStatus::Running,
        }
    }
}

/// sha256 of the recipe's JSON. Serialization is deterministic (declared field order, sorted
/// maps), so the same steps always hash the same.
pub fn recipe_sha256(steps: &[Step]) -> String {
    sha256_hex(&serde_json::to_vec(steps).unwrap_or_default())
}

/// The run id becomes a file name, so it may not name a path.
fn state_path(dir: &Path, run_id: &str) -> io::Result<PathBuf> {
    let plain = !run_id.is_empty()
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !plain {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("run id {run_id:?} is not a plain name"),
        ));
    }
    Ok(dir.join(format!("{run_id}.json")))
}

/// Writes `<dir>/<run_id>.json` atomically, so a crash mid-write leaves the previous state.
pub(crate) fn save(state: &RunState, dir: &Path) -> io::Result<()> {
    let path = state_path(dir, &state.run_id)?;
    let json = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    std::fs::create_dir_all(dir)?;
    write_atomic(&path, &json)
}

/// The run's own copy of the signed manifest it started with:
/// `<dir>/<run_id>.manifest.json` and its `.minisig`.
fn manifest_paths(dir: &Path, run_id: &str) -> io::Result<(PathBuf, PathBuf)> {
    state_path(dir, run_id)?;
    Ok((
        dir.join(format!("{run_id}{MANIFEST_SUFFIX}")),
        dir.join(format!("{run_id}{MANIFEST_SUFFIX}.minisig")),
    ))
}

const MANIFEST_SUFFIX: &str = ".manifest.json";

pub(crate) fn save_manifest(dir: &Path, run_id: &str, bytes: &[u8], sig: &str) -> io::Result<()> {
    let (manifest, minisig) = manifest_paths(dir, run_id)?;
    std::fs::create_dir_all(dir)?;
    write_atomic(&manifest, bytes)?;
    write_atomic(&minisig, sig.as_bytes())
}

/// Re-verifies the run's manifest copy; any problem reading or verifying it is an error.
pub(crate) fn load_manifest(
    dir: &Path,
    run_id: &str,
    pubkey: &str,
    app_version: &str,
) -> Result<Manifest, String> {
    let (manifest, minisig) = manifest_paths(dir, run_id).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let sig =
        std::fs::read_to_string(&minisig).map_err(|e| format!("{}: {e}", minisig.display()))?;
    verify_and_parse(&bytes, &sig, pubkey, app_version).map_err(|e| e.to_string())
}

pub(crate) fn load(dir: &Path, run_id: &str) -> io::Result<RunState> {
    let bytes = std::fs::read(state_path(dir, run_id)?)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Every readable run in `dir` that is not finished, ordered by run id. Unreadable files are
/// skipped: one corrupt run must not hide the others.
pub(crate) fn list_unfinished(dir: &Path) -> Vec<RunState> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut runs: Vec<RunState> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && !p.to_string_lossy().ends_with(MANIFEST_SUFFIX)
        })
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|b| serde_json::from_slice::<RunState>(&b).ok())
        .filter(|s| !s.status.is_finished())
        .collect();
    runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    runs
}
