// SPDX-License-Identifier: GPL-3.0-or-later

//! The HelixScreen release lists and GitHub's release assets: which release a step installs,
//! and where its files come from with the hash each is checked against.

use super::StepOutcome;
use crate::manifest::{Artifact, Channel};
use crate::transport::Env;
use crate::version::ReleaseVersion;
use serde::Deserialize;
use std::collections::BTreeMap;

const RELEASES_BASE: &str = "https://releases.helixscreen.org";

/// The release list of `channel`: `assets.<platform>` = `{url, sha256, size}` plus the `tag`.
pub fn releases_manifest_url(channel: Channel) -> String {
    format!("{RELEASES_BASE}/{}/manifest.json", channel.as_str())
}

/// Assets the release list does not carry are published only on GitHub; `/<tag>` follows.
pub const GITHUB_RELEASE_API: &str =
    "https://api.github.com/repos/prestonbrown/helixscreen/releases/tags";

#[derive(Deserialize)]
pub(crate) struct ReleaseManifest {
    #[serde(default)]
    version: Option<String>,
    pub(crate) tag: String,
    pub(crate) assets: BTreeMap<String, ReleaseAsset>,
}

impl ReleaseManifest {
    /// The release's version, from `version` or else its tag.
    fn release_version(&self) -> Result<ReleaseVersion, StepOutcome> {
        let text = self.version.as_deref().unwrap_or(&self.tag);
        ReleaseVersion::parse(text).ok_or_else(|| {
            StepOutcome::failed(format!(
                "the release list names an unreadable version: {text}"
            ))
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct ReleaseAsset {
    pub(crate) url: String,
    pub(crate) sha256: String,
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

/// A name from a network response that becomes a file name: one plain name.
pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

/// The channels an update reads: the pin, if any; else, for an installed prerelease, beta and
/// stable (the newest wins, so a beta tester reaches the stable release once it is out); else
/// stable alone.
pub(crate) fn update_channels(pinned: Option<Channel>, installed: &ReleaseVersion) -> Vec<Channel> {
    match pinned {
        Some(c) => vec![c],
        None if installed.is_prerelease() => vec![Channel::Beta, Channel::Stable],
        None => vec![Channel::Stable],
    }
}

/// The first stable release a new install takes. Until it is the stable release, new installs
/// take the beta: the stable releases before it ship an `install.sh` this installer cannot drive.
const FIRST_STABLE_FOR_NEW_INSTALLS: &str = "1.1.0";

/// The channel a new install (or a removal, which has no installed version to follow) reads
/// when nothing pins one, given the newest stable release.
pub fn new_install_channel(stable: &ReleaseVersion) -> Channel {
    match ReleaseVersion::parse(FIRST_STABLE_FOR_NEW_INSTALLS) {
        Some(first) if *stable >= first => Channel::Stable,
        _ => Channel::Beta,
    }
}

/// The channels a new install or a removal reads: the pin, if any, else `new_install_channel`
/// of the stable release list as it is now.
pub(crate) async fn new_install_channels(
    env: &Env,
    pinned: Option<Channel>,
) -> Result<Vec<Channel>, StepOutcome> {
    if let Some(c) = pinned {
        return Ok(vec![c]);
    }
    let (_, stable) = newest(env, &[Channel::Stable]).await?;
    Ok(vec![new_install_channel(&stable)])
}

/// What a run that found nothing newer to install says on its finish screen. A development build
/// can be newer than every release.
pub(crate) fn up_to_date_note(
    installed: &ReleaseVersion,
    channels: &[Channel],
    offered: &ReleaseVersion,
) -> String {
    let channels = channels
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(" or ");
    if installed > offered {
        return format!(
            "a newer build of HelixScreen is installed ({installed}) than the newest {channels} \
             release ({offered})"
        );
    }
    format!("{installed} is installed, and the newest {channels} release is {offered}")
}

/// The newest release of `channels`, the first winning a tie.
pub(crate) async fn newest(
    env: &Env,
    channels: &[Channel],
) -> Result<(ReleaseManifest, ReleaseVersion), StepOutcome> {
    let mut newest: Option<(ReleaseManifest, ReleaseVersion)> = None;
    for &channel in channels {
        let list_url = releases_manifest_url(channel);
        let release: ReleaseManifest = fetch_json(env, &list_url, "the release list").await?;
        let version = release.release_version()?;
        if newest.as_ref().is_none_or(|(_, best)| version > *best) {
            newest = Some((release, version));
        }
    }
    newest.ok_or_else(|| StepOutcome::failed("no release channel to read"))
}

/// Takes the `Env` rather than the `StepCtx`: a shared borrow of the context held across an
/// await would make the runner's future not `Send`.
pub(crate) async fn fetch_json<T: for<'de> Deserialize<'de>>(
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

/// The asset `name` of the GitHub release `tag`, checked against the asset's `digest`.
pub(crate) async fn github_asset(
    env: &Env,
    tag: &str,
    name: &str,
) -> Result<Artifact, StepOutcome> {
    if !safe_name(tag) {
        return Err(StepOutcome::failed(format!(
            "the release list names an unusable tag: {tag}"
        )));
    }
    let url = format!("{GITHUB_RELEASE_API}/{tag}");
    let release: GithubRelease = fetch_json(env, &url, "the GitHub release").await?;
    let Some(asset) = release.assets.into_iter().find(|a| a.name == name) else {
        return Err(StepOutcome::failed(format!(
            "the GitHub release {tag} has no {name}"
        )));
    };
    let Some(sha256) = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
    else {
        return Err(StepOutcome::failed(format!(
            "{name} in the GitHub release {tag} has no sha256 digest"
        )));
    };
    Ok(Artifact {
        url: asset.browser_download_url,
        sha256: sha256.to_string(),
        filename: Some(name.into()),
    })
}
