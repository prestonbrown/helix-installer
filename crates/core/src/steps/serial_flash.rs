// SPDX-License-Identifier: GPL-3.0-or-later

//! `serial_flash`: writes the HelixScreen release's build to an ESP32 panel on USB. It picks the
//! release as `helix_install` does (same channels, an update never older), downloads the build's
//! zip checked against GitHub's digest for it, then opens the panel's port once: it checks the
//! chip, its flash and that it is the board the run started on, writes only the parts the build's
//! `flash_args` lists, checks each by the MD5 the chip reads back, and resets the panel to read the
//! version it boots. It never erases the chip and never writes the merged factory image, so the
//! panel's WiFi and settings survive.

use super::download::fetch_artifact;
use super::esp32::{self, FlashPart, BOOT_VERSION_MARKER};
use super::release;
use super::{StepCtx, StepOutcome};
use crate::manifest::Channel;
use crate::runner::Input;
use crate::transport::{ChipInfo, FlashSession, TransportError};
use crate::version::ReleaseVersion;
use md5::Digest;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// How long one advance waits on a running flash before it reports progress.
const POLL: Duration = Duration::from_millis(500);
/// How long the panel's console is read after the reset for the version it boots.
const BOOT_LISTEN: Duration = Duration::from_secs(20);
const MIB: u64 = 1024 * 1024;

const MID_FLASH_HELP: &str = "The K-Touch may not start until it is written again, and its \
screen may stay blank. This cannot damage it: its chip always accepts a new write over USB. Plug \
it back in, check the cable carries data, then choose Try this step again to write it again.";

/// The step's manifest fields.
pub struct Spec<'a> {
    pub asset: &'a str,
    pub chip: &'a str,
    pub flash_mb: u32,
    pub keep: &'a [String],
    pub channel: Option<Channel>,
    pub update: bool,
}

/// A flash checked and ready, or running in the background. Held in memory only: a closed app
/// closes the port, and the step then writes the panel again from the start.
pub(crate) struct Task {
    step: usize,
    update: bool,
    /// The chip is erased before the writes.
    erase: bool,
    /// The version being written, as its app names it (`v1.1.0-beta.10`), and parsed.
    version: String,
    offered: ReleaseVersion,
    total: u64,
    written: Arc<AtomicU64>,
    job: Job,
}

impl Task {
    pub(crate) fn running(&self) -> bool {
        matches!(self.job, Job::Running(_))
    }

    /// Lets go of a flash that has not started writing, restarting the panel out of its stub.
    pub(crate) async fn release(self) {
        if let Job::Ready(Some((session, _))) = self.job {
            restart_unwritten(session).await;
        }
    }
}

enum Job {
    /// Every check passed on the open port; nothing is written until the run has recorded its
    /// point of no return.
    Ready(Option<(Box<dyn FlashSession>, Vec<FlashPart>)>),
    Running(JoinHandle<Result<String, Stop>>),
}

/// Why a flash stopped before the panel booted the new build.
enum Stop {
    /// Writing `part`, or reading it back, failed.
    Write { part: String, error: TransportError },
    /// `part` was written and the chip read back other bytes.
    Verify { part: String },
    /// Erasing the chip before the writes failed.
    Erase(TransportError),
    /// Everything was written and checked; the reset after it failed.
    Reset(TransportError),
}

fn progress(title: String, detail: Option<String>, fraction: Option<f32>) -> StepOutcome {
    StepOutcome::Progress {
        title,
        detail,
        fraction,
    }
}

pub async fn run(ctx: &mut StepCtx<'_>, spec: &Spec<'_>, input: Option<Input>) -> StepOutcome {
    if let Some(mut task) = ctx.session.flash.take() {
        if task.step == ctx.step {
            let ready = match &mut task.job {
                Job::Running(_) => return follow(ctx, task).await,
                Job::Ready(ready) => ready.take(),
            };
            // The runner records the commit on disk between the checks and the first change;
            // without it (the save failed) the prepared flash is dropped unwritten.
            match (ready, ctx.committed) {
                (Some((session, parts)), true) => {
                    let handle =
                        tokio::spawn(write_all(session, task.erase, parts, task.written.clone()));
                    task.job = Job::Running(handle);
                    let title = writing_title(ctx, &task.version);
                    ctx.session.flash = Some(task);
                    return progress(title, None, Some(0.0));
                }
                (Some((session, _)), false) => restart_unwritten(session).await,
                (None, _) => {}
            }
        } else {
            task.release().await;
        }
    }
    if ctx.calls == 0 {
        let title = format!(
            "Preparing to write HelixScreen to the {}",
            ctx.printer_name()
        );
        return progress(title, None, None);
    }
    match start(ctx, spec, input).await {
        Ok(task) => {
            let title = writing_title(ctx, &task.version);
            ctx.session.flash = Some(task);
            progress(title, None, Some(0.0))
        }
        Err(stop) => stop,
    }
}

fn writing_title(ctx: &StepCtx<'_>, version: &str) -> String {
    format!(
        "Writing HelixScreen {version} to the {}",
        ctx.printer_name()
    )
}

/// The HelixScreen version the panel runs, from the `serial` probe. An update that cannot tell
/// is refused: it could otherwise write an older release over a newer one.
fn installed_version(ctx: &StepCtx<'_>) -> Result<ReleaseVersion, StepOutcome> {
    ctx.facts
        .get("helix_version")
        .and_then(Value::as_str)
        .and_then(ReleaseVersion::parse)
        .ok_or_else(|| StepOutcome::Refused {
            doc: ctx.printer_doc(),
            reason: format!(
                "the installer cannot tell which HelixScreen version the {} runs, so it will not \
                 update it: an update never installs an older version",
                ctx.printer_name()
            ),
        })
}

/// Everything up to the first write: the release, its verified build, and the panel checked
/// over the one open of its port the write then goes through.
async fn start(
    ctx: &mut StepCtx<'_>,
    spec: &Spec<'_>,
    input: Option<Input>,
) -> Result<Task, StepOutcome> {
    let pinned = ctx.channel_override.or(spec.channel);
    let (channels, installed) = if spec.update {
        let v = installed_version(ctx)?;
        (release::update_channels(pinned, &v), Some(v))
    } else {
        (vec![pinned.unwrap_or_default()], None)
    };
    let (rel, offered) = release::newest(ctx.env, &channels).await?;
    if let Some(installed) = &installed {
        if offered <= *installed {
            return Err(StepOutcome::Complete {
                note: release::up_to_date_note(installed, &channels, &offered),
            });
        }
    }
    let name = spec.asset.replace("{tag}", &rel.tag);
    if !release::safe_name(&name) {
        return Err(StepOutcome::failed(format!(
            "the release names an unusable file: {name}"
        )));
    }
    let artifact = release::github_asset(ctx.env, &rel.tag, &name).await?;
    let Some(cache) = ctx.cache_dir.map(PathBuf::from) else {
        return Err(StepOutcome::failed("no download cache directory is set"));
    };
    let local = fetch_artifact(ctx.env, &artifact, &cache, &|_, _| {})
        .await
        .map_err(|e| StepOutcome::failed(format!("{name}: {e}")))?;
    let bytes = tokio::fs::read(&local)
        .await
        .map_err(|e| StepOutcome::failed(format!("could not read {name}: {e}")))?;
    let image = esp32::read_release_zip(&bytes).map_err(|e| {
        StepOutcome::failed(format!(
            "{name} is not a build this installer can write: {e}"
        ))
    })?;
    let version = match image.app() {
        Some(app) if ReleaseVersion::parse(&app.version).as_ref() == Some(&offered) => app.version,
        Some(app) => {
            return Err(StepOutcome::failed(format!(
                "{name} holds HelixScreen {}, not the release {} it is published in",
                app.version, rel.tag
            )))
        }
        None => return Err(StepOutcome::failed(format!("{name} holds no app to write"))),
    };

    let port = esp32::run_port(ctx)?;
    // Every open is of a port the user confirmed in this app session: one a replugged panel came
    // back under, or any port after the app was reopened, may now hold another device.
    if ctx.session.confirmed_port.as_deref() != Some(port.as_str()) {
        if !matches!(input, Some(Input::Next)) {
            return Err(esp32::confirm_open(ctx, &port));
        }
        ctx.session.confirmed_port = Some(port.clone());
    }
    let session = ctx
        .env
        .serial
        .connect(&port)
        .await
        .map_err(|e| esp32::open_failure(e, &port))?;
    let chip = session.chip().clone();
    let problem = board_problem(ctx, spec, &chip, &port).or_else(|| {
        esp32::check_image(&image, chip.flash_bytes, spec.keep)
            .err()
            .map(|e| StepOutcome::failed(format!("{name} cannot be written to this board: {e}")))
    });
    if let Some(stop) = problem {
        // Nothing was written: restart the panel into the app it already has.
        let _ = session.reset("", Duration::ZERO).await;
        return Err(stop);
    }
    // A panel that did not run HelixScreen when the run probed it is erased first, so nothing of
    // its old firmware or settings is left for HelixScreen to trip over. The run's probe decides,
    // not the chip as it is now: a retry after a half-written flash must not erase a panel whose
    // WiFi and settings are HelixScreen's own. An update never erases.
    let erase = !spec.update && ctx.facts.get("helix_installed") != Some(&json!(true));
    ctx.log(format!(
        "{} HelixScreen {version} to the {} on {port}",
        if erase {
            "erasing, then writing"
        } else {
            "writing"
        },
        ctx.printer_name()
    ));
    let total = image.total_bytes();
    // The point of no return is the first erase or write, not this step's start: everything
    // above refused or failed with the panel untouched.
    ctx.commit_before_acting();
    Ok(Task {
        step: ctx.step,
        update: spec.update,
        erase,
        version,
        offered,
        total,
        written: Arc::new(AtomicU64::new(0)),
        job: Job::Ready(Some((session, image.parts))),
    })
}

/// Why the board on `port` must not be written: another chip or flash size than the recipe's,
/// or another board than the one the run identified.
fn board_problem(
    ctx: &StepCtx<'_>,
    spec: &Spec<'_>,
    chip: &ChipInfo,
    port: &str,
) -> Option<StepOutcome> {
    let refuse = |reason: String| StepOutcome::Refused {
        doc: ctx.printer_doc(),
        reason,
    };
    if chip.chip != spec.chip {
        return Some(refuse(format!(
            "the board on {port} is an {}, not the {} a {} has; nothing was written",
            chip.chip,
            spec.chip,
            ctx.printer_name()
        )));
    }
    let want = spec.flash_mb as u64 * MIB;
    if chip.flash_bytes != want {
        return Some(refuse(format!(
            "the board on {port} has {} MB of flash, not the {} MB a {} has; nothing was written",
            chip.flash_bytes / MIB,
            spec.flash_mb,
            ctx.printer_name()
        )));
    }
    let started_on = ctx.facts.get("serial").and_then(Value::as_str);
    if let Some(started_on) = started_on {
        if chip.mac.as_deref() != Some(started_on) {
            return Some(StepOutcome::Failed {
                reason: format!(
                    "the board on {port} is not the {} the install started on; nothing was written",
                    ctx.printer_name()
                ),
                help: Some(format!(
                    "Plug in the {} the install started on, unplug any other, then choose Try this step again.",
                    ctx.printer_name()
                )),
            });
        }
    }
    None
}

/// Erases the chip when `erase` says to, writes each part and checks it by the MD5 the chip reads
/// back, then resets the panel and returns what it printed as it booted.
async fn write_all(
    mut s: Box<dyn FlashSession>,
    erase: bool,
    parts: Vec<FlashPart>,
    written: Arc<AtomicU64>,
) -> Result<String, Stop> {
    match write_parts(s.as_mut(), erase, &parts, &written).await {
        Ok(()) => s
            .reset(BOOT_VERSION_MARKER, BOOT_LISTEN)
            .await
            .map_err(Stop::Reset),
        Err(stop) => {
            // Out of the stub whatever it now holds: the bootloader shows nothing on the screen.
            let _ = s.reset("", Duration::ZERO).await;
            Err(stop)
        }
    }
}

async fn write_parts(
    s: &mut dyn FlashSession,
    erase: bool,
    parts: &[FlashPart],
    written: &AtomicU64,
) -> Result<(), Stop> {
    if erase {
        s.erase_all().await.map_err(Stop::Erase)?;
    }
    let mut done = 0u64;
    for p in parts {
        let base = done;
        let report = |n: u64| written.store(base + n, Ordering::Relaxed);
        let stopped = |error| Stop::Write {
            part: p.name.clone(),
            error,
        };
        s.write(p.offset, &p.data, &report).await.map_err(stopped)?;
        done += p.data.len() as u64;
        written.store(done, Ordering::Relaxed);
        let want: [u8; 16] = md5::Md5::digest(&p.data).into();
        let len = p.data.len() as u32;
        if s.md5(p.offset, len).await.map_err(stopped)? != want {
            return Err(Stop::Verify {
                part: p.name.clone(),
            });
        }
    }
    Ok(())
}

/// Closes a session that wrote nothing, restarting the panel into the app it already has.
async fn restart_unwritten(session: Box<dyn FlashSession>) {
    let _ = session.reset("", Duration::ZERO).await;
}

fn megabytes(n: u64) -> String {
    format!("{:.1}", n as f64 / 1_000_000.0)
}

/// Reports the running flash, or what it ended with.
async fn follow(ctx: &mut StepCtx<'_>, mut task: Task) -> StepOutcome {
    let Job::Running(handle) = &mut task.job else {
        return StepOutcome::failed("the flash was never started");
    };
    let ended = match tokio::time::timeout(POLL, handle).await {
        Ok(ended) => ended,
        Err(_) => {
            let written = task.written.load(Ordering::Relaxed).min(task.total);
            let detail = if task.erase && written == 0 {
                "Removing the old firmware and settings".to_string()
            } else {
                format!(
                    "{} of {} MB written",
                    megabytes(written),
                    megabytes(task.total)
                )
            };
            let out = progress(
                writing_title(ctx, &task.version),
                Some(detail),
                Some(written as f32 / task.total.max(1) as f32),
            );
            ctx.session.flash = Some(task);
            return out;
        }
    };
    match ended {
        Ok(Ok(log)) => booted(ctx, &task, &log),
        Ok(Err(stop)) => stopped(ctx, stop, task.erase),
        Err(e) => StepOutcome::Failed {
            reason: format!("the write to the {} stopped: {e}", ctx.printer_name()),
            help: Some(MID_FLASH_HELP.into()),
        },
    }
}

/// The flash ended with the panel reset: done once it boots the version written. An update whose
/// boot printed no version ends with a note to check the screen, since nothing more can be read
/// back; an install goes on to the setup screen, where the user sees it.
fn booted(ctx: &mut StepCtx<'_>, task: &Task, log: &str) -> StepOutcome {
    let name = ctx.printer_name().to_string();
    match esp32::boot_version(log) {
        Some(v) if ReleaseVersion::parse(&v).as_ref() == Some(&task.offered) => {
            ctx.log(format!("the {name} restarted running HelixScreen {v}"));
            ctx.facts.insert("helix_installed".into(), json!(true));
            ctx.facts.insert("helix_version".into(), json!(v));
            StepOutcome::Done
        }
        Some(v) => StepOutcome::Failed {
            reason: format!(
                "the {name} restarted running {v}, not HelixScreen {}",
                task.version
            ),
            help: Some(format!(
                "Unplug the {name}, plug it back in, and choose Try this step again to write it again."
            )),
        },
        None if task.update => StepOutcome::Complete {
            note: format!(
                "HelixScreen {} was written to the {name} and checked, and it restarted, but the \
                 installer could not read its version back: check its screen shows HelixScreen",
                task.version
            ),
        },
        None => {
            ctx.log(format!(
                "the {name} restarted, but printed no version the installer could read"
            ));
            StepOutcome::Done
        }
    }
}

/// What a flash that stopped part way tells the user. A panel erased for a first install is blank
/// from the erase until a write completes, and says so.
fn stopped(ctx: &mut StepCtx<'_>, stop: Stop, erased: bool) -> StepOutcome {
    let name = ctx.printer_name().to_string();
    // Only a reset failure comes after every part was written and checked.
    let blank = erased && !matches!(stop, Stop::Reset(_));
    match stopped_on(&name, stop) {
        StepOutcome::Failed { reason, .. } if blank => StepOutcome::Failed {
            reason: format!(
                "{reason}; the {name} was erased for the install and is blank until a flash \
                 completes"
            ),
            help: Some(format!(
                "The {name} is blank until a flash completes: it shows nothing and will not \
                 start. This cannot damage it: its chip always accepts a new write over USB. Keep \
                 it plugged in (plug it back in if it came loose) and choose Try this step again \
                 to write it."
            )),
        },
        other => other,
    }
}

fn stopped_on(name: &str, stop: Stop) -> StepOutcome {
    match stop {
        Stop::Write {
            error: TransportError::Disconnected,
            ..
        } => StepOutcome::Failed {
            reason: format!("the {name} disconnected while it was being written"),
            help: Some(MID_FLASH_HELP.into()),
        },
        Stop::Write { part, error } => StepOutcome::Failed {
            reason: format!("writing {part} to the {name} failed: {error}"),
            help: Some(MID_FLASH_HELP.into()),
        },
        Stop::Erase(TransportError::Disconnected) => StepOutcome::Failed {
            reason: format!("the {name} disconnected while it was being erased"),
            help: Some(MID_FLASH_HELP.into()),
        },
        Stop::Erase(error) => StepOutcome::Failed {
            reason: format!("erasing the {name} failed: {error}"),
            help: Some(MID_FLASH_HELP.into()),
        },
        Stop::Verify { part } => StepOutcome::Failed {
            reason: format!("{part} did not read back from the {name} as it was written"),
            help: Some(format!(
                "The {name} may not start until it is written again. Choose Try this step again; if \
                 it happens again, try another USB cable or port."
            )),
        },
        Stop::Reset(e) => StepOutcome::Failed {
            reason: format!(
                "HelixScreen was written to the {name} and checked, but the installer could not \
                 restart it: {e}"
            ),
            help: Some(format!(
                "Unplug the {name} and plug it back in to start it, then choose Try this step again."
            )),
        },
    }
}
