// SPDX-License-Identifier: GPL-3.0-or-later

//! The interfaces every install step talks to the outside world through,
//! plus the clock. Recipes depend only on these traits so they run end to end
//! against the scriptable fakes in `fake`.

pub mod drives_sys;
#[cfg(any(test, feature = "fakes"))]
pub mod fake;
pub mod fetch_http;
pub mod moonraker_http;
pub mod net_tcp;
pub mod serial_espflash;
pub mod ssh_russh;

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
    /// The OS refused this user the device; the payload is the group that owns it, empty when
    /// unknown.
    #[error("permission denied")]
    PermissionDenied(String),
    /// The device went away: unplugged, or its USB connection dropped.
    #[error("the device disconnected")]
    Disconnected,
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
    /// The job's shell, from the line it writes first; `None` when it is unknown.
    #[serde(default)]
    pub pid: Option<u32>,
}

/// New log text from `from_offset`; `exit` is set once the job has finished. `alive` says
/// whether the job's process still exists, when that was checked: a job that is not alive
/// and has no `exit` died without finishing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobChunk {
    pub text: String,
    pub next_offset: u64,
    pub exit: Option<i32>,
    pub alive: Option<bool>,
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
    /// Moonraker's `/printer/objects/list` answer: `{"objects": [..]}`.
    async fn objects_list(&self, host: &Host) -> Result<Value>;
    async fn gcode(&self, host: &Host, script: &str) -> Result<()>;
    async fn update_upgrade(&self, host: &Host, name: &str) -> Result<()>;
}

/// The SSH keys this computer could sign in with, as far as can be told without a printer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct KeyStatus {
    /// An agent is running, or a default key file exists.
    pub available: bool,
    /// A default key file is passphrase-protected.
    pub locked: bool,
}

#[async_trait]
pub trait Ssh: Send + Sync {
    /// Tries this computer's SSH keys before `login`'s password; a login with an empty password
    /// uses the keys alone.
    async fn connect(&self, host: &Host, login: &Login) -> Result<Box<dyn SshSession>>;
    fn key_status(&self) -> KeyStatus {
        KeyStatus::default()
    }
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
    /// False when the drives holding the OS, the user's home or this app could not be
    /// determined, so `list_drives` marks every drive `system`.
    fn system_drives_known(&self) -> bool;
    /// Writes `src` to `name` in the drive's root; `name` is never a path.
    async fn copy_to(&self, drive: &Drive, src: &Path, name: &str) -> Result<()>;
    /// Lowercase hex sha256 of the file `name` as it now reads back from the drive.
    async fn sha256_of(&self, drive: &Drive, name: &str) -> Result<String>;
    async fn eject(&self, drive: &Drive) -> Result<()>;
}

/// A USB serial port as the OS lists it, read without opening it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbPort {
    /// What the port is opened by: `/dev/ttyUSB0`, `/dev/cu.usbserial-110`, `COM3`.
    pub name: String,
    pub vid: u16,
    pub pid: u16,
}

/// What an ESP32's ROM bootloader reports about its chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipInfo {
    /// The chip as espflash names it, lowercase: `esp32s3`.
    pub chip: String,
    pub flash_bytes: u64,
    /// The factory MAC address, `aa:bb:cc:dd:ee:ff`, when the chip gives it.
    pub mac: Option<String>,
}

#[async_trait]
pub trait Serial: Send + Sync {
    /// The USB serial ports present now. Lists them without opening any: opening an ESP32
    /// board's port resets it.
    fn usb_ports(&self) -> Result<Vec<UsbPort>>;
    /// Opens `port`, which resets the board into its ROM bootloader, and connects to it.
    async fn connect(&self, port: &str) -> Result<Box<dyn FlashSession>>;
}

/// One open connection to an ESP32's bootloader. Every open of the port resets the board, so a
/// step opens one session and does all its work through it.
#[async_trait]
pub trait FlashSession: Send {
    fn chip(&self) -> &ChipInfo;
    async fn read(&mut self, offset: u32, len: u32) -> Result<Vec<u8>>;
    /// Writes `data` at `offset` (erasing only the sectors it covers); `progress` gets the bytes
    /// of `data` written so far.
    async fn write(
        &mut self,
        offset: u32,
        data: &[u8],
        progress: &(dyn Fn(u64) + Sync),
    ) -> Result<()>;
    /// Erases the whole flash chip.
    async fn erase_all(&mut self) -> Result<()>;
    /// The MD5 of `len` bytes at `offset` as the chip reads them back from its flash.
    async fn md5(&mut self, offset: u32, len: u32) -> Result<[u8; 16]>;
    /// Hard-resets the board so it boots its app, then reads its console until a line holding
    /// `until` arrives or `listen` passes, and closes the port. Returns what it read.
    async fn reset(self: Box<Self>, until: &str, listen: Duration) -> Result<String>;
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
pub trait Net: Send + Sync {
    /// Connects to `port` and returns the first line the server sends within `timeout`:
    /// `Ok(None)` when it connected but stayed silent, `Unreachable` when the connection was
    /// refused or never answered.
    async fn tcp_banner(&self, host: &Host, port: u16, timeout: Duration)
        -> Result<Option<String>>;
}

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
    async fn sleep(&self, d: Duration);
}

/// The wall clock and tokio's timer.
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await
    }
}

#[derive(Clone)]
pub struct Env {
    pub moonraker: Arc<dyn Moonraker>,
    pub ssh: Arc<dyn Ssh>,
    pub drives: Arc<dyn Drives>,
    pub fetch: Arc<dyn Fetch>,
    pub clock: Arc<dyn Clock>,
    pub net: Arc<dyn Net>,
    pub serial: Arc<dyn Serial>,
}
