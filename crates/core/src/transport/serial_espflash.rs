// SPDX-License-Identifier: GPL-3.0-or-later

//! `Serial` over this computer's serial ports, talking to an ESP32's ROM bootloader through
//! espflash and its flasher stub. Everything espflash does blocks, so each call runs on tokio's
//! blocking pool with the flasher moved there and back.

use super::{ChipInfo, FlashSession, Result, Serial, TransportError, UsbPort};
use async_trait::async_trait;
use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::Flasher;
use espflash::image_format::Segment;
use espflash::target::ProgressCallbacks;
use serialport::{SerialPort, SerialPortType, UsbPortInfo};
use std::borrow::Cow;
use std::time::{Duration, Instant};

/// The ROM bootloader's own rate, and what the panel's console runs at.
const CONNECT_BAUD: u32 = 115_200;
/// Writes run faster. The K-Touch's CH340 drops bytes when reading at 921600, so reads stay at
/// `CONNECT_BAUD` and writes use the rate HelixScreen's own K-Touch guide gives.
const WRITE_BAUD: u32 = 460_800;

pub struct EspflashSerial;

/// The serial ports the OS lists now, USB ones only.
fn usb_ports() -> Result<Vec<UsbPort>> {
    let ports = serialport::available_ports().map_err(|e| TransportError::Io(e.to_string()))?;
    Ok(ports
        .into_iter()
        .filter_map(|p| match p.port_type {
            SerialPortType::UsbPort(u) => Some(UsbPort {
                name: p.port_name,
                vid: u.vid,
                pid: u.pid,
            }),
            _ => None,
        })
        .collect())
}

fn listed(port: &str) -> bool {
    usb_ports().is_ok_and(|ports| ports.iter().any(|p| p.name == port))
}

/// The group that owns the device node, which a user must belong to to open it.
#[cfg(unix)]
fn device_group(port: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    let Ok(gid) = std::fs::metadata(port).map(|m| m.gid()) else {
        return String::new();
    };
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    groups
        .lines()
        .find_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let id = f.nth(1)?.parse::<u32>().ok()?;
            (id == gid).then(|| name.to_string())
        })
        .unwrap_or_default()
}

#[cfg(not(unix))]
fn device_group(_port: &str) -> String {
    String::new()
}

/// Why `port` would not open. A port the OS still lists but will not open is held by another
/// program, or (Linux) belongs to a group this user is not in; Windows reports both cases as a
/// missing device.
fn open_error(port: &str, e: serialport::Error) -> TransportError {
    match e.kind() {
        serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied) => {
            TransportError::PermissionDenied(device_group(port))
        }
        serialport::ErrorKind::NoDevice
        | serialport::ErrorKind::Io(std::io::ErrorKind::NotFound)
            if listed(port) =>
        {
            TransportError::PermissionDenied(String::new())
        }
        serialport::ErrorKind::NoDevice
        | serialport::ErrorKind::Io(std::io::ErrorKind::NotFound) => TransportError::Disconnected,
        _ => TransportError::Io(e.to_string()),
    }
}

/// An espflash failure, as the step reports it: a port that is no longer listed was unplugged,
/// whatever the error said.
fn flash_error(port: &str, e: espflash::Error) -> TransportError {
    if !listed(port) {
        return TransportError::Disconnected;
    }
    let text = e.to_string();
    if text.contains("Timeout") || text.contains("timed out") {
        TransportError::Timeout
    } else {
        TransportError::Protocol(text)
    }
}

fn joined<T>(r: std::result::Result<T, tokio::task::JoinError>) -> Result<T> {
    r.map_err(|e| TransportError::Io(format!("the serial worker stopped: {e}")))
}

#[async_trait]
impl Serial for EspflashSerial {
    fn usb_ports(&self) -> Result<Vec<UsbPort>> {
        usb_ports()
    }

    async fn connect(&self, port: &str) -> Result<Box<dyn FlashSession>> {
        let name = port.to_string();
        let session = tokio::task::spawn_blocking(move || -> Result<EspflashSession> {
            let info = serialport::available_ports()
                .ok()
                .and_then(|ports| ports.into_iter().find(|p| p.port_name == name))
                .and_then(|p| match p.port_type {
                    SerialPortType::UsbPort(u) => Some(u),
                    _ => None,
                })
                .unwrap_or(UsbPortInfo {
                    vid: 0,
                    pid: 0,
                    serial_number: None,
                    manufacturer: None,
                    product: None,
                });
            let serial = serialport::new(&name, CONNECT_BAUD)
                .flow_control(serialport::FlowControl::None)
                .open_native()
                .map_err(|e| open_error(&name, e))?;
            let connection = Connection::new(
                serial,
                info,
                ResetAfterOperation::HardReset,
                ResetBeforeOperation::DefaultReset,
                CONNECT_BAUD,
            );
            let mut flasher = Flasher::connect(connection, true, false, false, None, None)
                .map_err(|e| flash_error(&name, e))?;
            let device = flasher.device_info().map_err(|e| flash_error(&name, e))?;
            let chip = ChipInfo {
                chip: device.chip.to_string(),
                flash_bytes: device.flash_size.size() as u64,
                mac: device.mac_address,
            };
            Ok(EspflashSession {
                port: name,
                chip,
                flasher: Some(flasher),
                fast: false,
            })
        })
        .await;
        Ok(Box::new(joined(session)??))
    }
}

struct EspflashSession {
    port: String,
    chip: ChipInfo,
    /// Moved to the blocking pool for each call and back; `None` only while a call runs or after
    /// one panicked.
    flasher: Option<Flasher>,
    /// Running at `WRITE_BAUD`.
    fast: bool,
}

impl EspflashSession {
    /// Runs `f` on the flasher on the blocking pool.
    async fn with<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut Flasher, &str) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let mut flasher = self
            .flasher
            .take()
            .ok_or_else(|| TransportError::Io("the serial connection was lost".into()))?;
        let port = self.port.clone();
        let (flasher, out) = joined(
            tokio::task::spawn_blocking(move || {
                let out = f(&mut flasher, &port);
                (flasher, out)
            })
            .await,
        )?;
        self.flasher = Some(flasher);
        out
    }
}

/// Hands espflash's write progress to the async side, as bytes of the part: espflash counts
/// the chunks it has sent of the `chunks` it split the part into.
struct Progress {
    tx: tokio::sync::mpsc::UnboundedSender<u64>,
    len: u64,
    chunks: u64,
}

impl ProgressCallbacks for Progress {
    fn init(&mut self, _addr: u32, total: usize) {
        self.chunks = total.max(1) as u64;
    }
    fn update(&mut self, current: usize) {
        let _ = self
            .tx
            .send(self.len * (current as u64).min(self.chunks) / self.chunks);
    }
    fn verifying(&mut self) {}
    fn finish(&mut self, _skipped: bool) {}
}

#[async_trait]
impl FlashSession for EspflashSession {
    fn chip(&self) -> &ChipInfo {
        &self.chip
    }

    async fn read(&mut self, offset: u32, len: u32) -> Result<Vec<u8>> {
        self.with(move |f, port| {
            let dir = tempfile_dir()?;
            let path = dir.join("read.bin");
            let read = f.read_flash(offset, len, 0x1000, 1, path.clone());
            let out = read
                .map_err(|e| flash_error(port, e))
                .and_then(|()| std::fs::read(&path).map_err(|e| TransportError::Io(e.to_string())));
            let _ = std::fs::remove_dir_all(&dir);
            out
        })
        .await
    }

    async fn write(
        &mut self,
        offset: u32,
        data: &[u8],
        progress: &(dyn Fn(u64) + Sync),
    ) -> Result<()> {
        let fast = std::mem::replace(&mut self.fast, true);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let data = data.to_vec();
        let mut flasher = self
            .flasher
            .take()
            .ok_or_else(|| TransportError::Io("the serial connection was lost".into()))?;
        let port = self.port.clone();
        let job = tokio::task::spawn_blocking(move || {
            let mut cb = Progress {
                tx,
                len: data.len() as u64,
                chunks: 1,
            };
            let out = (|| {
                if !fast {
                    flasher.change_baud(WRITE_BAUD)?;
                }
                let segment = Segment {
                    addr: offset,
                    data: Cow::Borrowed(&data),
                };
                flasher.write_bins_to_flash(&[segment], &mut cb)
            })()
            .map_err(|e| flash_error(&port, e));
            (flasher, out)
        });
        while let Some(n) = rx.recv().await {
            progress(n);
        }
        let (flasher, out) = joined(job.await)?;
        self.flasher = Some(flasher);
        out
    }

    async fn erase_all(&mut self) -> Result<()> {
        self.with(|f, port| f.erase_flash().map_err(|e| flash_error(port, e)))
            .await
    }

    async fn md5(&mut self, offset: u32, len: u32) -> Result<[u8; 16]> {
        self.with(move |f, port| {
            f.checksum_md5(offset, len)
                .map(u128::to_be_bytes)
                .map_err(|e| flash_error(port, e))
        })
        .await
    }

    async fn reset(mut self: Box<Self>, until: &str, listen: Duration) -> Result<String> {
        let mut flasher = self
            .flasher
            .take()
            .ok_or_else(|| TransportError::Io("the serial connection was lost".into()))?;
        let port = self.port.clone();
        let until = until.to_string();
        joined(
            tokio::task::spawn_blocking(move || {
                flasher
                    .connection()
                    .reset()
                    .map_err(|e| flash_error(&port, e))?;
                if listen.is_zero() {
                    return Ok(String::new());
                }
                let mut serial = flasher.into_connection().into_serial();
                // The app's console runs at the ROM's rate, whatever the writes ran at.
                let _ = serial.set_baud_rate(CONNECT_BAUD);
                listen_console(&mut serial, &until, listen, &port)
            })
            .await,
        )?
    }
}

/// Reads the console until a line holding `until` has arrived or `listen` has passed.
fn listen_console(
    serial: &mut dyn SerialPort,
    until: &str,
    listen: Duration,
    port: &str,
) -> Result<String> {
    let deadline = Instant::now() + listen;
    let _ = serial.set_timeout(Duration::from_millis(200));
    let mut text = String::new();
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        match serial.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                if text.find(until).is_some_and(|i| text[i..].contains('\n')) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) if !listed(port) => return Err(TransportError::Disconnected),
            Err(e) => return Err(TransportError::Io(e.to_string())),
        }
    }
    Ok(text)
}

/// A private directory for one flash read, which espflash writes to a file.
fn tempfile_dir() -> Result<std::path::PathBuf> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("helix-installer-read-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| TransportError::Io(e.to_string()))?;
    Ok(dir)
}
