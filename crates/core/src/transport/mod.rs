// SPDX-License-Identifier: GPL-3.0-or-later

//! The four interfaces every install step talks to the outside world through,
//! plus the clock. Recipes depend only on these traits so they run end to end
//! against the scriptable fakes in `fake`.

#[cfg(any(test, feature = "fakes"))]
pub mod fake;

use crate::manifest::Login;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("host unreachable")]
    Unreachable,
    #[error("timed out")]
    Timeout,
    #[error("authentication failed")]
    Auth,
    #[error("HTTP status {0}")]
    Http(u16),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("I/O error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, TransportError>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Host {
    pub addr: String,
    pub moonraker_port: u16,
    pub ssh_port: u16,
}

impl Host {
    pub fn new(addr: impl Into<String>) -> Self {
        Host {
            addr: addr.into(),
            moonraker_port: 7125,
            ssh_port: 22,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
}

/// A command running detached on the printer, identified by the log it writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteJob {
    pub log_path: String,
}

/// New log text from `from_offset`; `exit` is set once the job has finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobChunk {
    pub text: String,
    pub next_offset: u64,
    pub exit: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    pub id: String,
    pub label: String,
    pub mount: PathBuf,
    pub fs: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub removable: bool,
    pub system: bool,
}

#[async_trait]
pub trait Moonraker: Send + Sync {
    async fn printer_info(&self, host: &Host) -> Result<Value>;
    async fn system_info(&self, host: &Host) -> Result<Value>;
    async fn update_status(&self, host: &Host) -> Result<Value>;
    async fn gcode(&self, host: &Host, script: &str) -> Result<()>;
    async fn update_upgrade(&self, host: &Host, name: &str) -> Result<()>;
}

#[async_trait]
pub trait Ssh: Send + Sync {
    async fn connect(&self, host: &Host, login: &Login) -> Result<Box<dyn SshSession>>;
}

#[async_trait]
pub trait SshSession: Send {
    /// `stdin` is the only channel for secrets; never put one in `cmd`.
    async fn exec(
        &mut self,
        cmd: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput>;
    /// Streams the file with `cat > remote` on an exec channel (no sftp).
    async fn upload(&mut self, local: &Path, remote: &str) -> Result<()>;
    /// Starts `cmd` detached so it survives the session; `sudo_password`
    /// is fed on stdin.
    async fn spawn_detached(&mut self, cmd: &str, sudo_password: Option<&str>)
        -> Result<RemoteJob>;
    async fn read_job(&mut self, job: &RemoteJob, from_offset: u64) -> Result<JobChunk>;
}

#[async_trait]
pub trait Drives: Send + Sync {
    /// Every drive with its `removable` and `system` flags, unfiltered;
    /// callers decide which to offer.
    fn list_drives(&self) -> Result<Vec<Drive>>;
    async fn copy_to(&self, drive: &Drive, src: &Path, name: &str) -> Result<()>;
    async fn eject(&self, drive: &Drive) -> Result<()>;
}

#[async_trait]
pub trait Fetch: Send + Sync {
    /// `progress` receives (bytes so far, total when known).
    async fn get(
        &self,
        url: &str,
        dest: &Path,
        progress: &(dyn Fn(u64, Option<u64>) + Sync),
    ) -> Result<()>;
    async fn get_text(&self, url: &str) -> Result<String>;
}

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
    async fn sleep(&self, d: Duration);
}

#[derive(Clone)]
pub struct Env {
    pub moonraker: Arc<dyn Moonraker>,
    pub ssh: Arc<dyn Ssh>,
    pub drives: Arc<dyn Drives>,
    pub fetch: Arc<dyn Fetch>,
    pub clock: Arc<dyn Clock>,
}
