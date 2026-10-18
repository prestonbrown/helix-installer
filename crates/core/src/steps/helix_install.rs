// SPDX-License-Identifier: GPL-3.0-or-later

//! `helix_install`: downloads the current HelixScreen release and its `install.sh` on the PC,
//! verifies both, uploads them to the printer and runs `install.sh --progress` there, following
//! its `HELIX-PROGRESS` lines to the result.

use super::download::fetch_artifact;
use super::job::{self, Followed};
use super::progress::{parse_progress_line, ProgressEvent, StepStatus};
use super::{StepCtx, StepOutcome};
use crate::identify::GENERIC_REFUSE_DOC;
use crate::manifest::{Artifact, Channel, PlatformSel};
use crate::runner::Input;
use crate::transport::ssh_russh::sh_quote;
use crate::transport::{Env, RemoteJob};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

const RELEASES_BASE: &str = "https://releases.helixscreen.org";

/// The release list of `channel`: `assets.<platform>` = `{url, sha256, size}` plus the `tag`.
pub fn releases_manifest_url(channel: Channel) -> String {
    format!("{RELEASES_BASE}/{}/manifest.json", channel.as_str())
}

/// `install.sh` is published only as a GitHub release asset; `/<tag>` follows.
pub const GITHUB_RELEASE_API: &str =
    "https://api.github.com/repos/prestonbrown/helixscreen/releases/tags";
/// How long install.sh may print nothing before the install counts as hung. Extracting the
/// release onto a MIPS printer's slow flash takes minutes without a line of output.
pub const HELIX_INSTALL_SILENCE: Duration = Duration::from_secs(15 * 60);
const DEFAULT_DIR: &str = "/tmp";
const SUDO_DOC: &str = "https://helixscreen.org/docs/install";
const SUDO_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Fact holding the remote paths this step uploaded, so a reopened run can still remove them.
const UPLOADED_FACT: &str = "helix_install_uploaded";
const TOO_OLD: &str = "This HelixScreen release is older than this installer supports. Choose a newer release or update the installer.";
const TITLE: &str = "Installing HelixScreen";

#[derive(Deserialize)]
struct ReleaseManifest {
    tag: String,
    assets: BTreeMap<String, ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    url: String,
    sha256: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

pub async fn run(
    ctx: &mut StepCtx<'_>,
    platform: &PlatformSel,
    channel: Channel,
    update: bool,
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
        return follow(ctx, &running, &platform).await;
    }
    if ctx.calls == 0 {
        return progress(
            format!("Preparing to install HelixScreen ({platform})"),
            None,
        );
    }
    // The run-wide override (testing, beta testers) wins over the recipe.
    let channel = ctx.channel_override.unwrap_or(channel);
    match launch(ctx, &platform, channel, update, dir.unwrap_or(DEFAULT_DIR)).await {
        Ok(started) => {
            *ctx.job = Some(started);
            progress(TITLE.into(), None)
        }
        Err(stop) => stop,
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

/// A name from a network response that becomes a path on the printer: one plain file name.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

async fn launch(
    ctx: &mut StepCtx<'_>,
    platform: &str,
    channel: Channel,
    update: bool,
    dir: &str,
) -> Result<RemoteJob, StepOutcome> {
    check_sudo(ctx).await?;
    let Some(cache) = ctx.cache_dir.map(PathBuf::from) else {
        return Err(StepOutcome::failed("no download cache directory is set"));
    };
    let list_url = releases_manifest_url(channel);
    let release: ReleaseManifest = fetch_json(ctx.env, &list_url, "the release list").await?;
    let Some(asset) = release.assets.get(platform) else {
        return Err(StepOutcome::failed(format!(
            "HelixScreen {} has no build for platform {platform}",
            release.tag
        )));
    };
    let tarball_name = asset.url.rsplit('/').next().unwrap_or_default().to_string();
    if !safe_name(&tarball_name) {
        return Err(StepOutcome::failed(format!(
            "the release list names an unusable file: {}",
            asset.url
        )));
    }
    let tarball = Artifact {
        url: asset.url.clone(),
        sha256: asset.sha256.clone(),
        filename: Some(tarball_name.clone()),
    };
    let script = install_sh_artifact(ctx.env, &release.tag).await?;

    let tarball_local = fetch_artifact(ctx.env, &tarball, &cache, &|_, _| {})
        .await
        .map_err(|e| StepOutcome::failed(format!("{tarball_name}: {e}")))?;
    let script_local = fetch_artifact(ctx.env, &script, &cache, &|_, _| {})
        .await
        .map_err(|e| StepOutcome::failed(format!("install.sh: {e}")))?;

    let dir = dir.trim_end_matches('/');
    let script_remote = format!("{dir}/install.sh");
    let tarball_remote = format!("{dir}/{tarball_name}");
    let command = format!(
        "sh {} --local {} --progress{}",
        sh_quote(&script_remote),
        sh_quote(&tarball_remote),
        if update { " --update" } else { "" }
    );
    // install.sh only understands --progress in newer releases; an older one prints its usage
    // and exits, after the upload has already filled the printer's /tmp.
    let script_text = std::fs::read(&script_local)
        .map_err(|e| StepOutcome::failed(format!("install.sh: {e}")))?;
    if !handles_progress(&String::from_utf8_lossy(&script_text)) {
        return Err(StepOutcome::failed(TOO_OLD));
    }
    ctx.facts.insert(
        UPLOADED_FACT.into(),
        serde_json::json!([tarball_remote, script_remote]),
    );
    let sent = async {
        let ssh = ctx.ssh().await?;
        ssh.upload(&tarball_local, &tarball_remote).await?;
        ssh.upload(&script_local, &script_remote).await?;
        // install.sh calls sudo itself, so no password goes to the job.
        ssh.spawn_detached(&command, None).await
    }
    .await;
    match sent {
        Ok(job) => Ok(job),
        Err(e) => {
            ctx.drop_ssh();
            remove_uploaded(ctx).await;
            Err(StepOutcome::failed(format!(
                "could not start the installer on the printer: {e}"
            )))
        }
    }
}

/// Whether `install.sh` has a case arm for `--progress`.
fn handles_progress(script: &str) -> bool {
    script
        .lines()
        .any(|l| l.contains("--progress)") || l.contains("--progress|"))
}

/// Removes the exact files this step uploaded. A failure is logged, never a step failure.
async fn remove_uploaded(ctx: &mut StepCtx<'_>) {
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

/// install.sh runs as the login user and calls sudo itself, which cannot prompt from a
/// detached job: a user other than root needs passwordless sudo.
async fn check_sudo(ctx: &mut StepCtx<'_>) -> Result<(), StepOutcome> {
    let user = ctx.login().map(|l| l.user.clone()).unwrap_or_default();
    if user == "root" {
        return Ok(());
    }
    let ran = match ctx.ssh().await {
        Ok(s) => s.exec("sudo -n true", None, SUDO_CHECK_TIMEOUT).await,
        Err(e) => Err(e),
    };
    match ran {
        Ok(out) if out.exit == 0 => Ok(()),
        Ok(_) => Err(StepOutcome::Failed {
            reason: format!(
                "this printer's account ({user}) needs passwordless sudo to install HelixScreen"
            ),
            help: Some(format!(
                "Allow {user} to run sudo without a password, then retry: {SUDO_DOC}"
            )),
        }),
        Err(e) => {
            ctx.drop_ssh();
            Err(StepOutcome::failed(format!(
                "could not check sudo on the printer: {e}"
            )))
        }
    }
}

/// Takes the `Env` rather than the `StepCtx`: a shared borrow of the context held across an
/// await would make the runner's future not `Send`.
async fn fetch_json<T: for<'de> Deserialize<'de>>(
    env: &Env,
    url: &str,
    what: &str,
) -> Result<T, StepOutcome> {
    let text = env
        .fetch
        .get_text(url)
        .await
        .map_err(|e| StepOutcome::failed(format!("could not fetch {what}: {e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| StepOutcome::failed(format!("{what} is unreadable: {e}")))
}

/// `install.sh` from the GitHub release `tag`, checked against the asset's `digest`.
async fn install_sh_artifact(env: &Env, tag: &str) -> Result<Artifact, StepOutcome> {
    if !safe_name(tag) {
        return Err(StepOutcome::failed(format!(
            "the release list names an unusable tag: {tag}"
        )));
    }
    let url = format!("{GITHUB_RELEASE_API}/{tag}");
    let release: GithubRelease = fetch_json(env, &url, "the GitHub release").await?;
    let Some(asset) = release.assets.into_iter().find(|a| a.name == "install.sh") else {
        return Err(StepOutcome::failed(format!(
            "the GitHub release {tag} has no install.sh"
        )));
    };
    let Some(sha256) = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
    else {
        return Err(StepOutcome::failed(format!(
            "install.sh in the GitHub release {tag} has no sha256 digest"
        )));
    };
    Ok(Artifact {
        url: asset.browser_download_url,
        sha256: sha256.to_string(),
        filename: Some("install.sh".into()),
    })
}

fn is_progress(line: &str) -> bool {
    parse_progress_line(line).is_some()
}

/// Success needs `result ok` and the installer's own platform equal to the chosen one.
async fn follow(ctx: &mut StepCtx<'_>, running: &RemoteJob, platform: &str) -> StepOutcome {
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

    if let Some((ok, reason)) = result {
        job::forget(ctx);
        remove_uploaded(ctx).await;
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
                None => progress(TITLE.into(), None),
            }
        }
        Followed::Exited(code) => {
            job::forget(ctx);
            remove_uploaded(ctx).await;
            StepOutcome::failed(format!(
                "the installer exited ({code}) without reporting a result{tail}"
            ))
        }
        Followed::Died => {
            job::forget(ctx);
            remove_uploaded(ctx).await;
            StepOutcome::failed("the installer stopped without reporting a result")
        }
        Followed::Silent => StepOutcome::failed(format!(
            "the installer printed nothing for {} minutes{tail}",
            HELIX_INSTALL_SILENCE.as_secs() / 60
        )),
    }
}
