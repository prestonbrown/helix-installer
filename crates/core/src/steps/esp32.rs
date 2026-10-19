// SPDX-License-Identifier: GPL-3.0-or-later

//! What the installer reads from an ESP32 panel and from the HelixScreen build for it: the app
//! descriptor ESP-IDF puts at the start of every app image, the release zip's `flash_args` and
//! parts, the partition table, and the version a booting app prints. Also the `serial` probe.

use super::{StepCtx, StepOutcome};
use crate::facts::Facts;
use crate::transport::{ChipInfo, Serial, TransportError};
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

/// Where the app the bootloader starts lives: the `ota_0` partition.
pub const APP_OFFSET: u32 = 0x20000;
/// The image header, the first segment header and the `esp_app_desc_t` after them.
pub const APP_DESC_LEN: u32 = 0x120;
/// The project name HelixScreen's firmware builds under.
pub const HELIX_PROJECT: &str = "helixscreen_esp32";
/// What an ESP-IDF app's boot log puts before its version.
pub const BOOT_VERSION_MARKER: &str = "App version:";
const APP_DESC_MAGIC: u32 = 0xABCD_5432;
/// Where in an app image the descriptor starts, and its version and project name fields.
const DESC_AT: usize = 0x20;
const VERSION_AT: usize = DESC_AT + 0x10;
const PROJECT_AT: usize = DESC_AT + 0x30;
const NAME_LEN: usize = 32;
/// Where `flash_args` puts the partition table.
const PARTITION_TABLE_OFFSET: u32 = 0x8000;
const MIB: u64 = 1024 * 1024;

/// The project and version an app image says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDesc {
    pub project: String,
    pub version: String,
}

/// One file `flash_args` writes, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashPart {
    pub offset: u32,
    /// Its path inside the build, as `flash_args` names it.
    pub name: String,
    pub data: Vec<u8>,
}

/// A build as its `flash_args` describes it: the flash settings it was built for and the parts
/// to write, lowest offset first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashImage {
    pub flash_mode: String,
    pub flash_freq: String,
    pub flash_size: String,
    pub parts: Vec<FlashPart>,
}

impl FlashImage {
    /// The app this build installs: the first part with an app descriptor.
    pub fn app(&self) -> Option<AppDesc> {
        self.parts.iter().find_map(|p| app_desc(&p.data))
    }

    /// Bytes written in all.
    pub fn total_bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.data.len() as u64).sum()
    }
}

/// A NUL-terminated text field.
fn c_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// The `esp_app_desc_t` at the start of an app image; `None` when `image` holds none (erased
/// flash, another kind of image, or too few bytes).
pub fn app_desc(image: &[u8]) -> Option<AppDesc> {
    let magic = image.get(DESC_AT..DESC_AT + 4)?;
    if u32::from_le_bytes(magic.try_into().ok()?) != APP_DESC_MAGIC {
        return None;
    }
    let version = c_str(image.get(VERSION_AT..VERSION_AT + NAME_LEN)?);
    let project = c_str(image.get(PROJECT_AT..PROJECT_AT + NAME_LEN)?);
    Some(AppDesc { project, version })
}

/// A path inside the build: relative, and never leaving it.
fn plain_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains(':')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

fn parse_offset(text: &str) -> Option<u32> {
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    u32::from_str_radix(hex, 16).ok()
}

/// Reads a K-Touch release zip: its `flash_args` (in the zip's top folder) and each part it
/// lists. A merged factory image is never a part: writing one would wipe the panel's WiFi and
/// settings.
pub fn read_release_zip(bytes: &[u8]) -> Result<FlashImage, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("not a readable zip: {e}"))?;
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let args_name = names
        .iter()
        .filter(|n| n.rsplit('/').next() == Some("flash_args"))
        .min_by_key(|n| n.matches('/').count())
        .cloned()
        .ok_or("the build has no flash_args")?;
    let base = args_name
        .strip_suffix("flash_args")
        .unwrap_or_default()
        .to_string();
    let mut read = |name: &str| -> Result<Vec<u8>, String> {
        let mut f = zip
            .by_name(name)
            .map_err(|_| format!("the build has no {}", &name[base.len()..]))?;
        let mut out = Vec::new();
        f.read_to_end(&mut out)
            .map_err(|e| format!("could not read {name}: {e}"))?;
        Ok(out)
    };
    let args = String::from_utf8(read(&args_name)?).map_err(|_| "flash_args is not text")?;
    let mut lines = args.lines().map(str::trim).filter(|l| !l.is_empty());
    let options = lines.next().ok_or("flash_args is empty")?;
    let mut opts = options.split_whitespace();
    let (mut mode, mut freq, mut size) = (None, None, None);
    while let Some(key) = opts.next() {
        let slot = match key {
            "--flash_mode" => &mut mode,
            "--flash_freq" => &mut freq,
            "--flash_size" => &mut size,
            other => {
                return Err(format!(
                    "flash_args has an option this installer does not know: {other}"
                ))
            }
        };
        *slot = opts.next().map(str::to_string);
    }
    let (Some(flash_mode), Some(flash_freq), Some(flash_size)) = (mode, freq, size) else {
        return Err("flash_args does not name the flash mode, frequency and size".into());
    };
    let mut parts = Vec::new();
    for line in lines {
        let (offset, path) = line
            .split_once(char::is_whitespace)
            .map(|(o, p)| (o, p.trim()))
            .ok_or_else(|| format!("flash_args line {line:?} is not an offset and a file"))?;
        let offset = parse_offset(offset)
            .ok_or_else(|| format!("flash_args line {line:?} has no readable offset"))?;
        if !plain_path(path) {
            return Err(format!("flash_args names a file outside the build: {path}"));
        }
        if path
            .rsplit('/')
            .next()
            .is_some_and(|n| n.contains("factory"))
        {
            return Err(format!("flash_args names the factory image {path}, which would erase the panel's WiFi and settings"));
        }
        let data = read(&format!("{base}{path}"))?;
        parts.push(FlashPart {
            offset,
            name: path.to_string(),
            data,
        });
    }
    if parts.is_empty() {
        return Err("flash_args lists nothing to write".into());
    }
    parts.sort_by_key(|p| p.offset);
    Ok(FlashImage {
        flash_mode,
        flash_freq,
        flash_size,
        parts,
    })
}

/// One partition-table entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub label: String,
    pub offset: u32,
    pub size: u32,
}

/// The entries of an ESP-IDF partition table, up to its end marker.
pub fn partitions(table: &[u8]) -> Vec<Partition> {
    table
        .chunks_exact(32)
        .take_while(|e| e[..2] == [0xaa, 0x50])
        .map(|e| Partition {
            offset: u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            size: u32::from_le_bytes([e[8], e[9], e[10], e[11]]),
            label: c_str(&e[12..28]),
        })
        .collect()
}

/// `16MB` as bytes.
fn size_bytes(text: &str) -> Option<u64> {
    text.strip_suffix("MB")?
        .parse::<u64>()
        .ok()
        .map(|n| n * MIB)
}

/// The bootloader image header's flash mode byte and its size (high) and frequency (low)
/// nibbles, as ESP32-S3 images encode them.
fn header_bytes(mode: &str, size: &str, freq: &str) -> Option<(u8, u8)> {
    let mode = match mode {
        "qio" => 0,
        "qout" => 1,
        "dio" => 2,
        "dout" => 3,
        _ => return None,
    };
    let size = match size {
        "1MB" => 0,
        "2MB" => 1,
        "4MB" => 2,
        "8MB" => 3,
        "16MB" => 4,
        "32MB" => 5,
        "64MB" => 6,
        "128MB" => 7,
        _ => return None,
    };
    let freq = match freq {
        "40m" => 0x0,
        "26m" => 0x1,
        "20m" => 0x2,
        "80m" => 0xf,
        _ => return None,
    };
    Some((mode, size << 4 | freq))
}

/// Whether `img` may be written to a board with `flash_bytes` of flash: built for that size,
/// every part inside it with none overlapping another or any partition labelled in `keep`, and
/// a bootloader whose header carries the flash settings `flash_args` names.
pub fn check_image(img: &FlashImage, flash_bytes: u64, keep: &[String]) -> Result<(), String> {
    let built_for = size_bytes(&img.flash_size)
        .ok_or_else(|| format!("the build names an unknown flash size {}", img.flash_size))?;
    if built_for != flash_bytes {
        return Err(format!(
            "the build is for {} of flash and this board has {} MB",
            img.flash_size,
            flash_bytes / MIB
        ));
    }
    let span = |p: &FlashPart| (p.offset as u64, p.offset as u64 + p.data.len() as u64);
    for p in &img.parts {
        if span(p).1 > flash_bytes {
            return Err(format!("{} does not fit in the board's flash", p.name));
        }
    }
    for w in img.parts.windows(2) {
        if span(&w[0]).1 > span(&w[1]).0 {
            return Err(format!("{} and {} overlap", w[0].name, w[1].name));
        }
    }
    let table = img
        .parts
        .iter()
        .find(|p| p.offset == PARTITION_TABLE_OFFSET)
        .map(|p| partitions(&p.data))
        .unwrap_or_default();
    for label in keep {
        let Some(kept) = table.iter().find(|t| t.label == *label) else {
            return Err(format!(
                "the build's partition table has no {label} partition"
            ));
        };
        let (a, b) = (kept.offset as u64, kept.offset as u64 + kept.size as u64);
        if let Some(p) = img.parts.iter().find(|p| span(p).0 < b && a < span(p).1) {
            return Err(format!("{} would overwrite the {label} partition", p.name));
        }
    }
    if let Some(boot) = img.parts.iter().find(|p| p.offset == 0) {
        let want = header_bytes(&img.flash_mode, &img.flash_size, &img.flash_freq)
            .ok_or("flash_args names flash settings this installer does not know")?;
        let got = (boot.data.get(2).copied(), boot.data.get(3).copied());
        if boot.data.first() != Some(&0xe9) || got != (Some(want.0), Some(want.1)) {
            return Err(format!(
                "the bootloader {} was built for other flash settings than flash_args names",
                boot.name
            ));
        }
    }
    Ok(())
}

/// The version an ESP-IDF app printed as it booted, from its `App version:` line.
pub fn boot_version(log: &str) -> Option<String> {
    log.lines().find_map(|l| {
        let (_, rest) = l.split_once(BOOT_VERSION_MARKER)?;
        rest.split_whitespace().next().map(str::to_string)
    })
}

/// What the `serial` probe learns from the chip and the first bytes of its app partition: the
/// chip, its flash in MiB, its MAC as the board's `serial`, the app's project name, and, when
/// that app is HelixScreen, that it is installed and its version.
pub fn probe_facts(chip: &ChipInfo, app: &[u8]) -> Facts {
    let mut f = Facts::from([
        ("esp_chip".to_string(), json!(chip.chip)),
        ("flash_mb".to_string(), json!(chip.flash_bytes / MIB)),
    ]);
    if let Some(mac) = &chip.mac {
        f.insert("serial".into(), json!(mac));
    }
    if let Some(desc) = app_desc(app) {
        if desc.project == HELIX_PROJECT {
            f.insert("helix_installed".into(), Value::Bool(true));
            f.insert("helix_version".into(), json!(desc.version));
        }
        f.insert("esp_app".into(), json!(desc.project));
    }
    f
}

/// What to do when the OS will not let this user open `port`. On Linux the port belongs to a
/// group (`dialout` on most systems) the user must join; elsewhere another program holds it.
pub fn permission_help(os: &str, port: &str, group: &str) -> String {
    if os == "linux" {
        let group = if group.is_empty() { "dialout" } else { group };
        return format!(
            "Your user account is not allowed to use {port}. Open a terminal and run:\n\n\
             sudo usermod -aG {group} $USER\n\n\
             then log out and back in (or restart the computer), unplug the K-Touch, plug it \
             back in, and try again."
        );
    }
    format!(
        "{port} is in use by another program, such as a slicer or a serial monitor. Close it, \
         unplug the K-Touch, plug it back in, and try again."
    )
}

/// The port the run's panel is on now: the run's own port while it is still listed, else the
/// one port with the panel's USB id (a replugged panel can come back under another name).
pub(crate) fn find_port(
    serial: &dyn Serial,
    wanted: &str,
    usb: Option<&str>,
) -> Result<String, StepOutcome> {
    let ports = serial
        .usb_ports()
        .map_err(|e| StepOutcome::failed(format!("could not list the USB ports: {e}")))?;
    if ports.iter().any(|p| p.name == wanted) {
        return Ok(wanted.to_string());
    }
    let same: Vec<&str> = ports
        .iter()
        .filter(|p| Some(crate::discovery::usb::usb_id(p.vid, p.pid).as_str()) == usb)
        .map(|p| p.name.as_str())
        .collect();
    match same.as_slice() {
        [one] => Ok(one.to_string()),
        [] => Err(StepOutcome::Failed {
            reason: format!("the K-Touch is not plugged in: {wanted} is gone"),
            help: Some(PLUG_IN.into()),
        }),
        _ => Err(StepOutcome::Failed {
            reason: "more than one K-Touch is plugged in".into(),
            help: Some(
                "Unplug every K-Touch but the one you are installing, then try again.".into(),
            ),
        }),
    }
}

pub(crate) const PLUG_IN: &str =
    "Plug the K-Touch back in with a USB cable that carries data, then try again.";

/// The step outcome for a port that would not open or a bootloader that did not answer, before
/// anything was written.
pub(crate) fn open_failure(e: TransportError, port: &str) -> StepOutcome {
    match e {
        TransportError::PermissionDenied(group) => StepOutcome::Failed {
            reason: format!("this computer would not let the installer open {port}"),
            help: Some(permission_help(std::env::consts::OS, port, &group)),
        },
        TransportError::Disconnected => StepOutcome::Failed {
            reason: format!("the K-Touch disconnected from {port}; nothing on it was changed"),
            help: Some(PLUG_IN.into()),
        },
        e => StepOutcome::Failed {
            reason: format!("the device on {port} did not answer as an ESP32 bootloader ({e})"),
            help: Some(
                "Unplug the K-Touch, plug it back in and try again. If it keeps happening, try \
                 another USB cable or port."
                    .into(),
            ),
        },
    }
}

/// The `serial` probe: one open of the run's port, which reads the chip and the app's
/// descriptor and then restarts the panel into its app.
pub(crate) async fn probe(ctx: &mut StepCtx<'_>) -> Result<Facts, StepOutcome> {
    let usb = ctx
        .facts
        .get("usb")
        .and_then(Value::as_str)
        .map(str::to_string);
    let port = find_port(ctx.env.serial.as_ref(), &ctx.host.addr, usb.as_deref())?;
    let mut session = ctx
        .env
        .serial
        .connect(&port)
        .await
        .map_err(|e| open_failure(e, &port))?;
    let chip = session.chip().clone();
    let app = session.read(APP_OFFSET, APP_DESC_LEN).await;
    // The panel stays in its bootloader until reset, whatever the read gave.
    let reset = session.reset("", Duration::ZERO).await;
    let app = app.map_err(|e| open_failure(e, &port))?;
    if let Err(e) = reset {
        ctx.log(format!("could not restart the panel after reading it: {e}"));
    }
    Ok(probe_facts(&chip, &app))
}
