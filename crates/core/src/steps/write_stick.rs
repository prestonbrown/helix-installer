// SPDX-License-Identifier: GPL-3.0-or-later

//! `write_stick`: copies verified artifacts onto a FAT32 USB stick the user picks, then ejects
//! it. Only removable, non-system drives are offered; nothing on a drive is formatted,
//! partitioned or deleted; every file is hashed again as it reads back from the stick.

use super::{StepCtx, StepOutcome};
use crate::cache::{cached_path, hash_file};
use crate::runner::{Input, Prompt};
use crate::transport::drives_sys::is_plain_file_name;
use crate::transport::Drive;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How far the step at `step` got on its previous run, held in memory only: a reopened app
/// asks for the stick again and rewrites it.
pub(crate) enum Stage {
    Chosen { step: usize, drive: String },
    EjectWarned { step: usize },
}

/// What the platforms call FAT32: Linux `vfat`, macOS `msdos`, Windows `FAT32`.
const FAT32_NAMES: [&str; 3] = ["vfat", "msdos", "fat32"];

pub const FAT32_HELP: &str = "The printer reads only FAT32 sticks. Copy off anything you want \
to keep, reformat as FAT32 with your computer's disk tool (this erases the stick), or use another \
stick, then try again.";

pub fn is_fat32(fs: &str) -> bool {
    FAT32_NAMES.iter().any(|f| fs.eq_ignore_ascii_case(f))
}

struct StickFile {
    artifact: String,
    name: String,
    src: PathBuf,
    sha256: String,
    size: u64,
}

pub async fn run(
    ctx: &mut StepCtx<'_>,
    artifacts: &[String],
    rename: &BTreeMap<String, String>,
    input: Option<Input>,
) -> StepOutcome {
    let stage = ctx.session.stick.take();
    let files = match resolve(ctx, artifacts, rename) {
        Ok(f) => f,
        Err(o) => return o,
    };
    let needed: u64 = files.iter().map(|f| f.size).sum();
    let step = ctx.step;
    match stage {
        Some(Stage::EjectWarned { step: s }) if s == step => {
            if matches!(input, Some(Input::Next)) {
                return StepOutcome::Done;
            }
            ctx.session.stick = Some(Stage::EjectWarned { step });
            return eject_warning();
        }
        Some(Stage::Chosen { step: s, drive }) if s == step => {
            return write(ctx, &files, needed, &drive).await
        }
        _ => {}
    }
    let offered = match offered_drives(ctx) {
        Ok(d) => d,
        Err(o) => return o,
    };
    if let Some(Input::Drive(id)) = input {
        if let Some(d) = offered.iter().find(|d| d.id == id) {
            if let Err(o) = check_fits(d, needed) {
                return o;
            }
            let title = format!("Writing to {}", d.label);
            ctx.session.stick = Some(Stage::Chosen { step, drive: id });
            return StepOutcome::Progress {
                title,
                detail: None,
                fraction: None,
            };
        }
    }
    StepOutcome::NeedInput(Prompt::ChooseDrive {
        drives: offered,
        needed_bytes: needed,
    })
}

/// Each artifact's downloaded file and the name it gets on the stick: its `rename` entry, else
/// its manifest filename, else its id.
fn resolve(
    ctx: &StepCtx<'_>,
    artifacts: &[String],
    rename: &BTreeMap<String, String>,
) -> Result<Vec<StickFile>, StepOutcome> {
    if artifacts.is_empty() {
        return Err(StepOutcome::failed("write_stick lists no artifacts"));
    }
    let Some(dir) = ctx.cache_dir else {
        return Err(StepOutcome::failed("no download cache directory is set"));
    };
    let mut files: Vec<StickFile> = Vec::new();
    for id in artifacts {
        let Some(a) = ctx.manifest.artifacts.get(id) else {
            return Err(StepOutcome::failed(format!(
                "artifact {id} is not in the manifest"
            )));
        };
        let name = rename.get(id).or(a.filename.as_ref()).unwrap_or(id).clone();
        if !is_plain_file_name(&name) {
            return Err(StepOutcome::failed(format!(
                "{id}: {name:?} is not a plain file name"
            )));
        }
        if files.iter().any(|f| f.name.eq_ignore_ascii_case(&name)) {
            return Err(StepOutcome::failed(format!(
                "two artifacts would both be written as {name}"
            )));
        }
        let src = cached_path(dir, &a.sha256);
        let Ok(meta) = std::fs::metadata(&src) else {
            return Err(StepOutcome::failed(format!(
                "{id} has not been downloaded; a download step must come first"
            )));
        };
        files.push(StickFile {
            artifact: id.clone(),
            name,
            src,
            sha256: a.sha256.clone(),
            size: meta.len(),
        });
    }
    Ok(files)
}

fn offered_drives(ctx: &StepCtx<'_>) -> Result<Vec<Drive>, StepOutcome> {
    let drives = ctx
        .env
        .drives
        .list_drives()
        .map_err(|e| StepOutcome::failed(format!("could not list drives: {e}")))?;
    Ok(drives
        .into_iter()
        .filter(|d| d.removable && !d.system)
        .collect())
}

fn check_fits(d: &Drive, needed: u64) -> Result<(), StepOutcome> {
    if !is_fat32(&d.fs) {
        return Err(StepOutcome::Failed {
            reason: format!("{} is formatted as {}, not FAT32", d.label, d.fs),
            help: Some(FAT32_HELP.into()),
        });
    }
    if needed > d.free_bytes {
        return Err(StepOutcome::Failed {
            reason: format!(
                "{} has {} bytes free and the files need {needed}",
                d.label, d.free_bytes
            ),
            help: Some("Free up space on the stick or use a larger one, then try again.".into()),
        });
    }
    Ok(())
}

/// Checks the chosen stick again, as it may have changed since it was picked, and every cached
/// file against its manifest hash, all before the first byte is copied.
async fn write(
    ctx: &mut StepCtx<'_>,
    files: &[StickFile],
    needed: u64,
    drive_id: &str,
) -> StepOutcome {
    let offered = match offered_drives(ctx) {
        Ok(d) => d,
        Err(o) => return o,
    };
    let Some(drive) = offered.into_iter().find(|d| d.id == drive_id) else {
        return StepOutcome::Failed {
            reason: "the chosen stick is no longer available".into(),
            help: Some("Plug the stick back in and choose it again.".into()),
        };
    };
    if let Err(o) = check_fits(&drive, needed) {
        return o;
    }
    for f in files {
        match hash_file(&f.src) {
            Ok(h) if h == f.sha256 => {}
            Ok(_) => {
                return StepOutcome::failed(format!(
                    "the downloaded {} no longer matches its verified hash",
                    f.artifact
                ))
            }
            Err(e) => return StepOutcome::failed(format!("could not read {}: {e}", f.artifact)),
        }
    }
    let label = drive.label.clone();
    for f in files {
        if let Err(e) = ctx.env.drives.copy_to(&drive, &f.src, &f.name).await {
            return StepOutcome::Failed {
                reason: format!("could not write {} to {label}: {e}", f.name),
                help: Some(
                    "Check the stick is still plugged in and not write-protected, then try again."
                        .into(),
                ),
            };
        }
        let faulty = || {
            StepOutcome::Failed {
            reason: format!("{} on {label} does not match the verified download", f.name),
            help: Some(format!(
                "The stick may be faulty. Delete {} from it, do not use it on the printer, and try another stick.",
                f.name
            )),
        }
        };
        match ctx.env.drives.sha256_of(&drive, &f.name).await {
            Ok(h) if h == f.sha256 => ctx.log(format!("wrote and verified {} on {label}", f.name)),
            Ok(_) => return faulty(),
            Err(e) => {
                ctx.log(format!("could not read back {} on {label}: {e}", f.name));
                return faulty();
            }
        }
    }
    match ctx.env.drives.eject(&drive).await {
        Ok(()) => StepOutcome::Done,
        Err(e) => {
            ctx.log(format!("could not eject {label}: {e}"));
            ctx.session.stick = Some(Stage::EjectWarned { step: ctx.step });
            eject_warning()
        }
    }
}

fn eject_warning() -> StepOutcome {
    StepOutcome::NeedInput(Prompt::Info {
        text: "The files are on the stick and verified, but the installer could not eject it: \
eject it yourself before unplugging it."
            .into(),
        image: None,
    })
}
