// SPDX-License-Identifier: GPL-3.0-or-later

//! `helix_install`: downloads the current HelixScreen release and its `install.sh` on the PC,
//! verifies both, uploads them to the printer and runs `install.sh --progress` there, following
//! its `HELIX-PROGRESS` lines to the result. The uninstall mode uploads and runs only
//! `install.sh --uninstall`.

use super::download::fetch_artifact;
use super::job::{self, Followed};
use super::progress::{parse_progress_line, ProgressEvent, StepStatus};
use super::release::{self, safe_name, ReleaseManifest};
use super::sudo::{self, SudoPlan};
use super::{StepCtx, StepOutcome};
use crate::identify::GENERIC_REFUSE_DOC;
use crate::manifest::{Artifact, Channel, PlatformSel, Step};
use crate::runner::Input;
use crate::transport::ssh_russh::sh_quote;
use crate::transport::{RemoteJob, SshSession, TransportError};
use crate::version::ReleaseVersion;
use std::path::PathBuf;
use std::time::Duration;

pub use super::release::{releases_manifest_url, GITHUB_RELEASE_API};

/// How long install.sh may print nothing before the install counts as hung. Extracting the
/// release onto a MIPS printer's slow flash takes minutes without a line of output.
pub const HELIX_INSTALL_SILENCE: Duration = Duration::from_secs(15 * 60);
const DEFAULT_DIR: &str = "/tmp";
/// Prints the installed HelixScreen's version, one tagged line per install. A running
/// `helix-screen` is read in its own tree only (`release_info.json`, which every install by
/// install.sh writes, else the binary's `--version` line, `helix-screen 1.1.0-beta.9 (9ed38c74d5)`)
/// as `running <version>`, and nothing else is read. With nothing running, every place install.sh
/// installs to is read, the Forge-X payload root and the root its record names included, each
/// as `found <version>`: an old standalone tree can sit beside the current payload.
pub const INSTALLED_VERSION_PROBE: &str = r#"v() { if [ -f "$1/release_info.json" ]; then sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1/release_info.json" | head -n 1; elif [ -x "$1/bin/helix-screen" ]; then "$1/bin/helix-screen" --version 2>/dev/null | head -n 1; fi; }; p=$(pidof helix-screen 2>/dev/null | cut -d' ' -f1); e=; [ -n "$p" ] && e=$(readlink "/proc/$p/exe" 2>/dev/null); if [ -n "$e" ]; then r=$(v "${e%/bin/*}"); [ -z "$r" ] && r=$("$e" --version 2>/dev/null | head -n 1); [ -n "$r" ] && { echo "running $r"; exit 0; }; fi; for f in /opt/config/mod_data/helixscreen_payload_root /usr/data/config/mod_data/helixscreen_payload_root; do [ -f "$f" ] && set -- "$@" "$(head -n 1 "$f")"; done; for d in "$@" /usr/data/helixscreen /mnt/UDISK/helixscreen /opt/helixscreen /opt/config/mod_data/helixscreen /usr/data/config/mod_data/helixscreen /srv/helixscreen /root/printer_software/helixscreen /root/helixscreen /home/*/helixscreen; do r=$(v "$d"); [ -n "$r" ] && echo "found $r"; done; true"#;
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// The fact holding the installed HelixScreen's version (`v1.1.0-beta.9`).
const VERSION_FACT: &str = "helix_version";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hashing a 70 MB release on a K1's MIPS core takes seconds; a slow one is still cheaper than
/// sending it again.
const REMOTE_HASH_TIMEOUT: Duration = Duration::from_secs(120);
/// Fact holding the remote paths this run uploaded, so a later step or a reopened run can still
/// remove them.
const UPLOADED_FACT: &str = "helix_install_uploaded";
const TOO_OLD: &str = "This HelixScreen release is older than this installer supports. Choose a newer release or update the installer.";
const TITLE: &str = "Installing HelixScreen";
const UNINSTALL_TITLE: &str = "Removing HelixScreen";
const DRY_RUN_TITLE: &str = "Checking the printer with the HelixScreen installer";

/// What `install.sh` is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Install,
    Update,
    Uninstall,
}

/// What install.sh is asked to do, and whether only as a `--dry-run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub mode: Mode,
    pub dry_run: bool,
}

impl Run {
    fn title(self) -> &'static str {
        match (self.dry_run, self.mode) {
            (true, _) => DRY_RUN_TITLE,
            (false, Mode::Uninstall) => UNINSTALL_TITLE,
            (false, Mode::Install | Mode::Update) => TITLE,
        }
    }
}

/// `channel` is the recipe's pin, if any. An update with no pin, and no run-wide override,
/// follows the installed version: a prerelease reads the beta channel, else stable.
pub async fn run(
    ctx: &mut StepCtx<'_>,
    platform: &PlatformSel,
    channel: Option<Channel>,
    run: Run,
    dir: Option<&str>,
    input: Option<Input>,
) -> StepOutcome {
    if let Some(ask) = ctx.need_login(input).await {
        return ask;
    }
    let platform = match resolve_platform(ctx, platform) {
        Ok(p) => p,
        Err(stop) => return stop,
    };
    if let Some(running) = ctx.job.clone() {
        return follow(ctx, &running, &platform, run).await;
    }
    if ctx.calls == 0 {
        let verb = match (run.dry_run, run.mode) {
            (true, _) => "check the printer for",
            (false, Mode::Uninstall) => "remove",
            (false, _) => "install",
        };
        return progress(
            format!("Preparing to {verb} HelixScreen ({platform})"),
            None,
        );
    }
    // The run-wide override (testing, beta testers) wins over the recipe.
    let pinned = ctx.channel_override.or(channel);
    // An unpinned update of a prerelease takes the newest of beta and stable, so a beta tester
    // reaches the stable release once it is out; a stable install only ever reads stable.
    let (channels, installed) = match run.mode {
        Mode::Update => match installed_version(ctx).await {
            Ok(v) => (release::update_channels(pinned, &v), Some(v)),
            Err(stop) => return stop,
        },
        Mode::Install | Mode::Uninstall => (vec![pinned.unwrap_or_default()], None),
    };
    let target = Target {
        platform: &platform,
        channels: &channels,
        installed: installed.as_ref(),
    };
    match launch(ctx, target, run, dir.unwrap_or(DEFAULT_DIR)).await {
        Ok(started) => {
            *ctx.job = Some(started);
            progress(run.title().into(), None)
        }
        // Waiting on the user keeps a dry run's files for the launch that follows; any other
        // stop leaves nothing behind.
        Err(stop @ StepOutcome::NeedInput(_)) => stop,
        Err(stop) => {
            remove_uploaded(ctx).await;
            stop
        }
    }
}

fn progress(title: String, fraction: Option<f32>) -> StepOutcome {
    StepOutcome::Progress {
        title,
        detail: None,
        fraction,
    }
}

/// The recipe's fixed platform, or the one it maps this printer's `cpu_arch` fact to.
fn resolve_platform(ctx: &StepCtx<'_>, sel: &PlatformSel) -> Result<String, StepOutcome> {
    match sel {
        PlatformSel::Fixed(p) => Ok(p.clone()),
        PlatformSel::FromArch { from_arch } => {
            let arch = ctx.facts.get("cpu_arch").and_then(|v| v.as_str());
            match arch.and_then(|a| from_arch.get(a)) {
                Some(p) => Ok(p.clone()),
                None => Err(StepOutcome::Refused {
                    doc: GENERIC_REFUSE_DOC.into(),
                    reason: format!(
                        "there is no HelixScreen build for this printer's CPU ({})",
                        arch.unwrap_or("unknown")
                    ),
                }),
            }
        }
    }
}

/// The installed HelixScreen's version. The running `helix-screen`'s own version decides when one
/// runs: Moonraker's `helix_version` can lag it. With nothing running, the highest of every
/// install found on the printer and Moonraker's answer counts, never the first found, so a stale
/// copy beside the current install cannot cause a downgrade. An update that cannot tell is
/// refused, since it could otherwise install an older release over a newer one.
async fn installed_version(ctx: &mut StepCtx<'_>) -> Result<ReleaseVersion, StepOutcome> {
    let read = match ctx.ssh().await {
        Ok(s) => {
            s.exec(INSTALLED_VERSION_PROBE, None, VERSION_PROBE_TIMEOUT)
                .await
        }
        Err(e) => Err(e),
    };
    let text = match read {
        Ok(out) if out.exit == 0 => out.stdout,
        Ok(_) => String::new(),
        Err(e) => {
            ctx.drop_ssh();
            return Err(StepOutcome::failed(format!(
                "could not read the installed HelixScreen version: {e}"
            )));
        }
    };
    let reported = ctx
        .facts
        .get(VERSION_FACT)
        .and_then(|v| v.as_str())
        .and_then(|t| ReleaseVersion::parse(t).map(|v| (t.to_string(), v)));
    let chosen = match read_versions(&text) {
        (Some(running), _) => Some(running),
        (None, found) => found
            .into_iter()
            .chain(reported)
            .max_by(|a, b| a.1.cmp(&b.1)),
    };
    match chosen {
        Some((token, v)) => {
            ctx.facts
                .insert(VERSION_FACT.into(), serde_json::json!(token));
            Ok(v)
        }
        None => Err(StepOutcome::Refused {
            doc: ctx.printer_doc(),
            reason: "the installer cannot tell which HelixScreen version is on this printer, \
                     so it will not update it: an update never installs an older version. \
                     Reinstall HelixScreen following this printer's install guide"
                .into(),
        }),
    }
}

/// A version as the printer printed it, and parsed.
type Tagged = (String, ReleaseVersion);

/// The probe's `running` version, and every `found` one (an untagged line counts as found).
/// Each line holds a bare version or a `helix-screen <version> (<hash>)` line.
fn read_versions(text: &str) -> (Option<Tagged>, Vec<Tagged>) {
    let version_in = |line: &str| {
        line.split_whitespace()
            .find_map(|t| ReleaseVersion::parse(t).map(|v| (t.to_string(), v)))
    };
    let mut found = Vec::new();
    for line in text.lines() {
        match line.trim().strip_prefix("running ") {
            Some(rest) => {
                if let Some(v) = version_in(rest) {
                    return (Some(v), Vec::new());
                }
            }
            None => found.extend(version_in(line.trim().trim_start_matches("found "))),
        }
    }
    (None, found)
}

/// What a launch installs: the platform's build from the newest release of `channels` (the
/// first wins a tie), and for an update only when that release is newer than `installed`.
#[derive(Clone, Copy)]
struct Target<'a> {
    platform: &'a str,
    channels: &'a [Channel],
    installed: Option<&'a ReleaseVersion>,
}

async fn launch(
    ctx: &mut StepCtx<'_>,
    target: Target<'_>,
    run: Run,
    dir: &str,
) -> Result<RemoteJob, StepOutcome> {
    let platform = target.platform;
    let mode = run.mode;
    let (release, offered) = release::newest(ctx.env, target.channels).await?;
    if let Some(installed) = target.installed {
        if offered <= *installed {
            return Err(StepOutcome::Complete {
                note: release::up_to_date_note(installed, target.channels, &offered),
            });
        }
    }
    let sudo_password = sudo_password(ctx).await?;
    let Some(cache) = ctx.cache_dir.map(PathBuf::from) else {
        return Err(StepOutcome::failed("no download cache directory is set"));
    };
    // Uninstalling needs only the installer, so a release without this platform's build still
    // serves.
    let tarball = match mode {
        Mode::Uninstall => None,
        Mode::Install | Mode::Update => Some(release_tarball(&release, platform)?),
    };
    let script = release::github_asset(ctx.env, &release.tag, "install.sh").await?;

    let tarball_local = match &tarball {
        Some(t) => {
            let name = t.filename.clone().unwrap_or_default();
            let local = fetch_artifact(ctx.env, t, &cache, &|_, _| {})
                .await
                .map_err(|e| StepOutcome::failed(format!("{name}: {e}")))?;
            Some((local, name))
        }
        None => None,
    };
    let script_local = fetch_artifact(ctx.env, &script, &cache, &|_, _| {})
        .await
        .map_err(|e| StepOutcome::failed(format!("install.sh: {e}")))?;

    let dir = dir.trim_end_matches('/');
    let script_remote = format!("{dir}/install.sh");
    let tarball_remote = tarball_local
        .as_ref()
        .map(|(_, name)| format!("{dir}/{name}"));
    let command = match &tarball_remote {
        Some(remote) => format!(
            "sh {} --local {} --progress{}{}",
            sh_quote(&script_remote),
            sh_quote(remote),
            if mode == Mode::Update {
                " --update"
            } else {
                ""
            },
            // install.sh's --dry-run exits at its confirm point, before the first change.
            if run.dry_run { " --dry-run" } else { "" }
        ),
        None => format!("sh {} --uninstall --progress", sh_quote(&script_remote)),
    };
    // install.sh only understands --progress in newer releases; an older one prints its usage
    // and exits, after the upload has already filled the printer's /tmp.
    let script_text = std::fs::read(&script_local)
        .map_err(|e| StepOutcome::failed(format!("install.sh: {e}")))?;
    if !handles_progress(&String::from_utf8_lossy(&script_text)) {
        return Err(StepOutcome::failed(TOO_OLD));
    }
    // Files an earlier step of this run (the dry run) uploaded are sent again only if they no
    // longer read back as the verified ones.
    let earlier = uploaded_files(ctx);
    let mut sends: Vec<(PathBuf, String, String)> = Vec::new();
    if let (Some((local, _)), Some(remote), Some(t)) = (&tarball_local, &tarball_remote, &tarball) {
        sends.push((local.clone(), remote.clone(), t.sha256.clone()));
    }
    sends.push((
        script_local.clone(),
        script_remote.clone(),
        script.sha256.clone(),
    ));
    let mut uploaded = earlier.clone();
    for (_, remote, _) in &sends {
        if !uploaded.contains(remote) {
            uploaded.push(remote.clone());
        }
    }
    ctx.facts
        .insert(UPLOADED_FACT.into(), serde_json::json!(uploaded));
    let sent = async {
        let ssh = ctx.ssh().await?;
        for (local, remote, sha256) in &sends {
            if earlier.contains(remote) && remote_matches(ssh, local, remote, sha256).await {
                continue;
            }
            ssh.upload(local, remote).await?;
        }
        ssh.spawn_detached(&command, sudo_password.as_deref()).await
    }
    .await;
    match sent {
        Ok(job) => Ok(job),
        Err(e) => {
            ctx.drop_ssh();
            remove_uploaded(ctx).await;
            Err(match e {
                TransportError::Auth => sudo::refused(ctx),
                e => StepOutcome::failed(format!(
                    "could not start the installer on the printer: {e}"
                )),
            })
        }
    }
}

/// The release's build for `platform`, named for upload.
fn release_tarball(release: &ReleaseManifest, platform: &str) -> Result<Artifact, StepOutcome> {
    let Some(asset) = release.assets.get(platform) else {
        return Err(StepOutcome::failed(format!(
            "HelixScreen {} has no build for platform {platform}",
            release.tag
        )));
    };
    let name = asset.url.rsplit('/').next().unwrap_or_default().to_string();
    if !safe_name(&name) {
        return Err(StepOutcome::failed(format!(
            "the release list names an unusable file: {}",
            asset.url
        )));
    }
    Ok(Artifact {
        url: asset.url.clone(),
        sha256: asset.sha256.clone(),
        filename: Some(name),
    })
}

/// Whether `install.sh` has a case arm for `--progress`.
fn handles_progress(script: &str) -> bool {
    script
        .lines()
        .any(|l| l.contains("--progress)") || l.contains("--progress|"))
}

/// The remote paths this run uploaded and has not removed yet.
fn uploaded_files(ctx: &StepCtx<'_>) -> Vec<String> {
    ctx.facts
        .get(UPLOADED_FACT)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// True when `remote` on the printer has `local`'s size and the sha256 it was verified against,
/// by `sha256sum` or BusyBox's. Anything else, a printer without either tool included, is false.
async fn remote_matches(
    ssh: &mut (dyn SshSession + 'static),
    local: &std::path::Path,
    remote: &str,
    sha256: &str,
) -> bool {
    let Ok(size) = std::fs::metadata(local).map(|m| m.len()) else {
        return false;
    };
    let p = sh_quote(remote);
    let cmd = format!("wc -c < {p} && {{ sha256sum {p} 2>/dev/null || busybox sha256sum {p} ; }}");
    let Ok(out) = ssh.exec(&cmd, None, REMOTE_HASH_TIMEOUT).await else {
        return false;
    };
    let mut lines = out.stdout.lines();
    let got_size = lines.next().and_then(|l| l.trim().parse::<u64>().ok());
    let got_sha = lines
        .next()
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_ascii_lowercase);
    out.exit == 0 && got_size == Some(size) && got_sha.as_deref() == Some(sha256)
}

/// Removes the exact files this run uploaded. A failure is logged, never a step failure.
pub(crate) async fn remove_uploaded(ctx: &mut StepCtx<'_>) {
    let Some(files) = ctx.facts.remove(UPLOADED_FACT) else {
        return;
    };
    let paths: Vec<String> = files
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(sh_quote).collect())
        .unwrap_or_default();
    if paths.is_empty() {
        return;
    }
    let command = format!("rm -f {}", paths.join(" "));
    let ran = match ctx.ssh().await {
        Ok(s) => s.exec(&command, None, CLEANUP_TIMEOUT).await,
        Err(e) => Err(e),
    };
    match ran {
        Ok(out) if out.exit == 0 => {}
        Ok(out) => ctx.log(format!(
            "could not remove the uploaded installer files (exit {})",
            out.exit
        )),
        Err(e) => {
            ctx.drop_ssh();
            ctx.log(format!(
                "could not remove the uploaded installer files: {e}"
            ));
        }
    }
}

/// install.sh calls sudo itself, which cannot prompt from a detached job, so a login without
/// passwordless sudo runs the whole installer through sudo with the held password, or the one
/// the user typed for sudo after a key login. Run as
/// root, install.sh still installs for the Klipper service user: it finds that user from the
/// klipper service, the process table and /home, never from who started it.
async fn sudo_password(ctx: &mut StepCtx<'_>) -> Result<Option<String>, StepOutcome> {
    match sudo::plan(ctx).await? {
        SudoPlan::AsLoginUser => Ok(None),
        SudoPlan::WithPassword(pw) => Ok(Some(pw)),
    }
}

/// Kills the installer that named `found`, then removes the uploads; a job that could not be
/// killed keeps its files, since it may still be reading them.
async fn stop_wrong_platform(
    ctx: &mut StepCtx<'_>,
    running: &RemoteJob,
    found: &str,
    platform: &str,
) -> StepOutcome {
    let sudo_password = match sudo::plan(ctx).await {
        Ok(SudoPlan::WithPassword(pw)) => Some(pw),
        Ok(_) => None,
        Err(stop) => return stop,
    };
    let killed = job::kill(ctx, running, sudo_password.as_deref()).await;
    job::ended(ctx, running, false).await;
    let reason = format!("the installer found platform {found}, but this printer needs {platform}");
    match killed {
        Ok(()) => {
            remove_uploaded(ctx).await;
            StepOutcome::failed(format!("{reason}; it was stopped before installing"))
        }
        Err(why) => StepOutcome::failed(format!(
            "{reason}, and it could not be stopped ({why}); it may still be running on the printer"
        )),
    }
}

/// A later step of the recipe installs for real.
fn install_follows(ctx: &StepCtx<'_>) -> bool {
    ctx.recipe.iter().skip(ctx.step + 1).any(|s| {
        matches!(
            s,
            Step::HelixInstall {
                dry_run: false,
                uninstall: false,
                ..
            }
        )
    })
}

fn is_progress(line: &str) -> bool {
    parse_progress_line(line).is_some()
}

/// Success needs `result ok` and the installer's own platform equal to the chosen one.
async fn follow(
    ctx: &mut StepCtx<'_>,
    running: &RemoteJob,
    platform: &str,
    run: Run,
) -> StepOutcome {
    let followed = match job::follow(ctx, running, HELIX_INSTALL_SILENCE, is_progress).await {
        Ok(f) => f,
        Err(e) => return job::lost(ctx, "the installer", e),
    };
    let events: Vec<ProgressEvent> = job::lines(ctx)
        .iter()
        .filter_map(|l| parse_progress_line(l))
        .collect();
    let reported = events.iter().find_map(|e| match e {
        ProgressEvent::Platform(p) => Some(p.clone()),
        _ => None,
    });
    let result = events.iter().find_map(|e| match e {
        ProgressEvent::Result { ok, reason } => Some((*ok, reason.clone())),
        _ => None,
    });
    let tail = job::tail(ctx, |l| !is_progress(l));

    // install.sh names its platform before it changes anything, so a wrong one is stopped
    // there rather than left to install a build this printer does not run.
    if let (Some(p), None) = (&reported, &result) {
        if p != platform {
            return stop_wrong_platform(ctx, running, p, platform).await;
        }
    }

    if let Some((ok, reason)) = result {
        job::ended(ctx, running, ok).await;
        // A dry run that passed leaves its files for the install after it, which sends them
        // again only if they changed.
        if !(ok && run.dry_run && install_follows(ctx)) {
            remove_uploaded(ctx).await;
        }
        if !ok {
            let reason = reason.unwrap_or_else(|| "no reason given".into());
            return StepOutcome::failed(format!("the HelixScreen installer failed: {reason}"));
        }
        return match reported {
            Some(p) if p == platform => StepOutcome::Done,
            Some(p) => StepOutcome::failed(format!(
                "the installer installed platform {p}, but this printer needs {platform}"
            )),
            None => StepOutcome::failed("the installer did not report which platform it installed"),
        };
    }
    match followed {
        Followed::Event => {
            let last_step = events.iter().rev().find_map(|e| match e {
                ProgressEvent::Step {
                    n,
                    total,
                    status,
                    title,
                    detail,
                } => Some((*n, *total, *status, title.clone(), detail.clone())),
                _ => None,
            });
            match last_step {
                Some((n, total, status, title, detail)) => {
                    let done = if status == StepStatus::Start {
                        n.saturating_sub(1)
                    } else {
                        n
                    };
                    StepOutcome::Progress {
                        title,
                        detail: (!detail.is_empty()).then_some(detail),
                        fraction: (total > 0).then(|| (done as f32 / total as f32).min(1.0)),
                    }
                }
                None => progress(run.title().into(), None),
            }
        }
        Followed::Exited(code) => {
            job::ended(ctx, running, false).await;
            remove_uploaded(ctx).await;
            StepOutcome::failed(format!(
                "the installer exited ({code}) without reporting a result{tail}"
            ))
        }
        Followed::Died => {
            job::ended(ctx, running, false).await;
            remove_uploaded(ctx).await;
            StepOutcome::failed("the installer stopped without reporting a result")
        }
        Followed::Silent => StepOutcome::failed(format!(
            "the installer printed nothing for {} minutes{tail}",
            HELIX_INSTALL_SILENCE.as_secs() / 60
        )),
    }
}
