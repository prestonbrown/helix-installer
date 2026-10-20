// SPDX-License-Identifier: GPL-3.0-or-later

//! The signed manifest that carries every piece of printer knowledge the installer acts on.

use crate::cache::sha256_hex;
use crate::fs_atomic::write_atomic;
use crate::manifest_sig::verify_signature;
use crate::version::{FwVersion, VersionRange};
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a cached manifest stays usable offline.
pub const CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    BadSignature,
    TooNew { min_app_version: String },
    Malformed(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::BadSignature => f.write_str("manifest signature is not valid"),
            ManifestError::TooNew { min_app_version } => {
                write!(f, "manifest needs installer {min_app_version} or newer")
            }
            ManifestError::Malformed(m) => write!(f, "manifest is malformed: {m}"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Lowercase hex sha256 of the verified bytes this manifest was parsed from; empty when it
    /// was not parsed by `verify_and_parse`. A run is pinned to it.
    #[serde(skip)]
    pub sha256: String,
    pub manifest_version: u32,
    pub min_app_version: String,
    pub printers: Vec<Printer>,
    pub artifacts: BTreeMap<String, Artifact>,
    pub recipes: BTreeMap<String, Vec<Step>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Printer {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub photo: Option<String>,
    #[serde(with = "rule_list")]
    pub identify: Vec<IdentifyRule>,
    pub recipes: Vec<RecipeChoice>,
    /// Absent only on a refused printer.
    #[serde(default)]
    pub restore: Option<RestoreSel>,
    pub fallback: Fallback,
    #[serde(default)]
    pub verified: Option<Verified>,
    /// A printer the installer knows and turns away: when its identify rules match, the
    /// printer is refused with its `fallback.doc`, whatever else the facts also match.
    #[serde(default)]
    pub refused: bool,
    /// Matches only when no other printer does: the catch-all for hosts that answer too
    /// little to be told apart from a vendor's printer.
    #[serde(default)]
    pub last_resort: bool,
}

/// One restore recipe, or a list chosen by the facts (first match), for a printer whose way
/// back depends on what it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RestoreSel {
    Recipe(String),
    ByFacts(Vec<RecipeChoice>),
}

impl RestoreSel {
    /// Every recipe this can name.
    pub fn recipes(&self) -> Vec<&str> {
        match self {
            RestoreSel::Recipe(r) => vec![r.as_str()],
            RestoreSel::ByFacts(choices) => choices.iter().map(|c| c.recipe.as_str()).collect(),
        }
    }
}

/// All conditions must hold against the gathered facts.
pub type IdentifyRule = BTreeMap<String, Cond>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeChoice {
    #[serde(with = "cond_map")]
    pub when: BTreeMap<String, Cond>,
    pub recipe: String,
}

#[derive(Debug, Clone)]
pub enum Cond {
    Exact(String),
    /// Matches a list fact that has this element.
    Contains {
        contains: String,
    },
    /// Matches a list fact that has every one of these elements.
    ContainsAll {
        contains_all: Vec<String>,
    },
    Range(VersionRange),
    /// Matches a text fact that starts with this text (case-sensitive).
    Prefix {
        prefix: String,
    },
    /// Matches a text fact that differs from this text, or a list fact without this element.
    /// A missing fact still never matches.
    Not {
        not: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainsRepr {
    contains: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainsAllRepr {
    contains_all: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrefixRepr {
    prefix: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotRepr {
    not: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CondRepr {
    Text(String),
    Contains(ContainsRepr),
    ContainsAll(ContainsAllRepr),
    Prefix(PrefixRepr),
    Not(NotRepr),
}

impl Cond {
    fn from_repr<E: de::Error>(key: &str, repr: CondRepr) -> Result<Cond, E> {
        match repr {
            CondRepr::Text(s) if key.ends_with("_version") => VersionRange::parse(&s)
                .map(Cond::Range)
                .map_err(|e| E::custom(format!("{key}: {e}"))),
            CondRepr::Text(s) => Ok(Cond::Exact(s)),
            CondRepr::Contains(c) => Ok(Cond::Contains {
                contains: c.contains,
            }),
            CondRepr::ContainsAll(c) => Ok(Cond::ContainsAll {
                contains_all: c.contains_all,
            }),
            CondRepr::Prefix(p) => Ok(Cond::Prefix { prefix: p.prefix }),
            CondRepr::Not(n) => Ok(Cond::Not { not: n.not }),
        }
    }
}

impl Serialize for Cond {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Cond::Exact(v) => s.serialize_str(v),
            Cond::Range(r) => s.serialize_str(r.as_str()),
            Cond::Contains { contains } => ContainsRepr {
                contains: contains.clone(),
            }
            .serialize(s),
            Cond::ContainsAll { contains_all } => ContainsAllRepr {
                contains_all: contains_all.clone(),
            }
            .serialize(s),
            Cond::Prefix { prefix } => PrefixRepr {
                prefix: prefix.clone(),
            }
            .serialize(s),
            Cond::Not { not } => NotRepr { not: not.clone() }.serialize(s),
        }
    }
}

/// `Cond` is typed by the key it sits under, so conditions are only deserialized as a whole map.
mod cond_map {
    use super::*;

    pub fn serialize<S: Serializer>(m: &BTreeMap<String, Cond>, s: S) -> Result<S::Ok, S::Error> {
        m.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, Cond>, D::Error> {
        let raw = BTreeMap::<String, CondRepr>::deserialize(d)?;
        raw.into_iter()
            .map(|(k, v)| Cond::from_repr(&k, v).map(|c| (k, c)))
            .collect()
    }
}

/// Deserializes a list of identify rules, each typed by key like `RecipeChoice::when`.
mod rule_list {
    use super::*;

    pub fn serialize<S: Serializer>(
        rules: &[BTreeMap<String, Cond>],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        rules.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<BTreeMap<String, Cond>>, D::Error> {
        let raw = Vec::<BTreeMap<String, CondRepr>>::deserialize(d)?;
        raw.into_iter()
            .map(|rule| {
                rule.into_iter()
                    .map(|(k, v)| Cond::from_repr(&k, v).map(|c| (k, c)))
                    .collect()
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fallback {
    pub refuse: bool,
    pub doc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verified {
    pub by: String,
    pub date: String,
    pub firmware: String,
}

/// Something the user typed that must never be shown or kept: it deserializes from a plain
/// string, has no `Serialize`, and its `Debug` prints `<redacted>`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    pub user: String,
    /// Empty when the login relies on this computer's SSH key alone.
    #[serde(default)]
    pub password: String,
    /// Unlocks a passphrase-protected key file on this computer. Typed by the user, held in
    /// memory only, never written out.
    #[serde(default, skip_serializing)]
    pub passphrase: Option<Secret>,
    /// What sudo on the printer is fed when the login has no password of its own to give it.
    /// Typed by the user, held in memory only, never written out.
    #[serde(default, skip_serializing)]
    pub sudo_password: Option<Secret>,
}

impl Login {
    pub fn new(user: impl Into<String>, password: impl Into<String>) -> Login {
        Login {
            user: user.into(),
            password: password.into(),
            ..Login::default()
        }
    }

    /// The secrets this login holds, for stripping from a support bundle.
    pub fn secrets(&self) -> Vec<String> {
        let typed = [&self.passphrase, &self.sudo_password]
            .into_iter()
            .flatten()
            .map(|s| s.expose().to_string());
        std::iter::once(self.password.clone())
            .chain(typed)
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Never prints a password or passphrase, so a logged login cannot leak one.
impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("passphrase", &self.passphrase)
            .field("sudo_password", &self.sudo_password)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub image: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoonrakerProbe {
    PrinterInfo,
    SystemInfo,
    UpdateStatus,
    ObjectsList,
}

/// What a `serial` probe reads from a board on the run's USB port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialProbe {
    /// The ESP32's chip, flash size and MAC from its bootloader, and the app it runs from that
    /// app's descriptor (`steps::esp32::probe_facts`).
    Esp32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Probe {
    Moonraker { moonraker: MoonrakerProbe },
    Ssh { ssh: String, fact: String },
    TcpPort { tcp_port: u16, fact: String },
    Serial { serial: SerialProbe },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitTarget {
    Port(u16),
    Moonraker,
    /// Moonraker answers and reports Klipper `ready`.
    KlipperReady,
    /// The printer restarts: Moonraker stops answering, then comes back with Klipper `ready`.
    /// If it never stops answering within `RESTART_START_WINDOW`, the restart is taken as
    /// already over (a retried step) and only `ready` is waited for.
    KlipperRestart,
    Ssh,
    HelixService,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum PlatformSel {
    Fixed(String),
    FromArch { from_arch: BTreeMap<String, String> },
}

/// One recipe step. `commits` marks the point of no return.
/// Which HelixScreen release list a `helix_install` step reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

impl Channel {
    /// The channel named `value` (`stable` or `beta`, lowercase); anything else is `None`.
    pub fn parse(value: &str) -> Option<Channel> {
        match value {
            "stable" => Some(Channel::Stable),
            "beta" => Some(Channel::Beta),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Info {
        text: String,
        #[serde(default)]
        image: Option<String>,
        #[serde(default)]
        commits: bool,
    },
    UserAction {
        text: String,
        #[serde(default)]
        image: Option<String>,
        #[serde(default)]
        verify: Option<Probe>,
        #[serde(default)]
        commits: bool,
    },
    Ask {
        fact: String,
        question: String,
        choices: Vec<Choice>,
        #[serde(default)]
        commits: bool,
    },
    /// Exactly one of `moonraker`, `ssh` (with `fact`), `tcp_port` (with `fact`) or `serial`;
    /// `Step::probe` returns it as a `Probe`, and `verify_and_parse` rejects any other combination.
    Probe {
        #[serde(default)]
        moonraker: Option<MoonrakerProbe>,
        #[serde(default)]
        ssh: Option<String>,
        #[serde(default)]
        tcp_port: Option<u16>,
        #[serde(default)]
        serial: Option<SerialProbe>,
        #[serde(default)]
        fact: Option<String>,
        #[serde(default)]
        commits: bool,
    },
    /// Exactly one of `range` (a version fact) or `equals` (text, or `true`/`false` for a
    /// yes/no fact); `verify_and_parse` rejects any other combination. `reason`, when set, is
    /// what the refusal says instead of the generated text.
    Require {
        fact: String,
        #[serde(default)]
        range: Option<VersionRange>,
        #[serde(default)]
        equals: Option<String>,
        #[serde(default)]
        reason: Option<String>,
        doc: String,
        /// A fact that does not fit warns instead of refusing: the user reads `reason`, with
        /// `doc`, and continues or stops.
        #[serde(default)]
        warn: bool,
        #[serde(default)]
        commits: bool,
    },
    Download {
        artifact: String,
        #[serde(default)]
        commits: bool,
    },
    WriteStick {
        artifacts: Vec<String>,
        #[serde(default)]
        rename: BTreeMap<String, String>,
        #[serde(default)]
        commits: bool,
    },
    WaitFor {
        target: WaitTarget,
        timeout_s: u64,
        message: String,
        #[serde(default)]
        help: String,
        #[serde(default)]
        commits: bool,
    },
    SshLogin {
        logins: Vec<Login>,
        #[serde(default)]
        ask_if_failed: bool,
        /// Shown when the user is asked for a login: where to find this printer's password.
        #[serde(default)]
        password_hint: Option<String>,
        #[serde(default)]
        commits: bool,
    },
    SshRun {
        command: String,
        timeout_s: u64,
        #[serde(default)]
        commits: bool,
    },
    MoonrakerGcode {
        script: String,
        #[serde(default)]
        commits: bool,
    },
    MoonrakerUpdate {
        name: String,
        #[serde(default)]
        commits: bool,
    },
    HelixInstall {
        platform: PlatformSel,
        #[serde(default)]
        channel: Option<Channel>,
        #[serde(default)]
        update: bool,
        /// Runs `install.sh --uninstall` instead of installing; uploads no release.
        #[serde(default)]
        uninstall: bool,
        /// Runs the same install with `--dry-run`, which changes nothing on the printer and
        /// stops at install.sh's confirm point, to check its platform and result first.
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        dir: Option<String>,
        #[serde(default)]
        commits: bool,
    },
    /// Writes the HelixScreen release's build for an ESP32 panel to the board on the run's USB
    /// port: only the parts the build's `flash_args` lists, each checked by reading it back,
    /// then resets the board and reads the version it boots.
    SerialFlash {
        /// The GitHub release asset holding the build, `{tag}` standing for the release tag.
        asset: String,
        /// The chip the board must report, as espflash names it (`esp32s3`).
        chip: String,
        /// The flash size the board must report, in MiB.
        flash_mb: u32,
        /// Labels of partitions no part may overlap: the board's own WiFi and settings.
        #[serde(default)]
        keep: Vec<String>,
        #[serde(default)]
        channel: Option<Channel>,
        /// Installs only a release newer than the HelixScreen the board runs.
        #[serde(default)]
        update: bool,
        #[serde(default)]
        commits: bool,
    },
}

impl Step {
    /// The `type` tag this step has in the manifest.
    pub fn type_name(&self) -> &'static str {
        match self {
            Step::Info { .. } => "info",
            Step::UserAction { .. } => "user_action",
            Step::Ask { .. } => "ask",
            Step::Probe { .. } => "probe",
            Step::Require { .. } => "require",
            Step::Download { .. } => "download",
            Step::WriteStick { .. } => "write_stick",
            Step::WaitFor { .. } => "wait_for",
            Step::SshLogin { .. } => "ssh_login",
            Step::SshRun { .. } => "ssh_run",
            Step::MoonrakerGcode { .. } => "moonraker_gcode",
            Step::MoonrakerUpdate { .. } => "moonraker_update",
            Step::HelixInstall { .. } => "helix_install",
            Step::SerialFlash { .. } => "serial_flash",
        }
    }

    /// False for steps that act on the printer in a way that is not safe to repeat blindly.
    pub fn is_idempotent(&self) -> bool {
        !matches!(
            self,
            Step::MoonrakerGcode { .. }
                | Step::SshRun { .. }
                | Step::HelixInstall { dry_run: false, .. }
        )
    }

    /// True for steps that send something to the printer itself: commands, installs, G-code and
    /// Moonraker updates. After the point of no return each needs the printer identified again
    /// first, by probes since the commit or the last wait.
    pub fn acts_on_printer(&self) -> bool {
        matches!(
            self,
            Step::SshRun { .. }
                | Step::HelixInstall { .. }
                | Step::MoonrakerGcode { .. }
                | Step::MoonrakerUpdate { .. }
                | Step::SerialFlash { .. }
        )
    }

    /// True for the step that marks the point of no return.
    pub fn commits(&self) -> bool {
        match self {
            Step::Info { commits, .. }
            | Step::UserAction { commits, .. }
            | Step::Ask { commits, .. }
            | Step::Probe { commits, .. }
            | Step::Require { commits, .. }
            | Step::Download { commits, .. }
            | Step::WriteStick { commits, .. }
            | Step::WaitFor { commits, .. }
            | Step::SshLogin { commits, .. }
            | Step::SshRun { commits, .. }
            | Step::MoonrakerGcode { commits, .. }
            | Step::MoonrakerUpdate { commits, .. }
            | Step::HelixInstall { commits, .. }
            | Step::SerialFlash { commits, .. } => *commits,
        }
    }

    /// What is wrong with a step whose fields cannot all be expressed in its JSON type.
    pub fn shape_error(&self) -> Option<&'static str> {
        match self {
            Step::Require { range, equals, .. } if range.is_some() == equals.is_some() => {
                Some("require needs exactly one of range and equals")
            }
            Step::HelixInstall {
                dry_run: true,
                uninstall: true,
                ..
            } => Some("helix_install cannot be a dry run of an uninstall"),
            Step::SerialFlash { chip, flash_mb, .. } if chip.is_empty() || *flash_mb == 0 => {
                Some("serial_flash needs a chip and a flash_mb")
            }
            _ => None,
        }
    }

    /// The probe a `Step::Probe` describes, or `None` for any other step or an invalid combination.
    pub fn probe(&self) -> Option<Probe> {
        match self {
            Step::Probe {
                moonraker: Some(m),
                ssh: None,
                tcp_port: None,
                serial: None,
                fact: None,
                ..
            } => Some(Probe::Moonraker { moonraker: *m }),
            Step::Probe {
                moonraker: None,
                ssh: Some(ssh),
                tcp_port: None,
                serial: None,
                fact: Some(fact),
                ..
            } => Some(Probe::Ssh {
                ssh: ssh.clone(),
                fact: fact.clone(),
            }),
            Step::Probe {
                moonraker: None,
                ssh: None,
                tcp_port: Some(port),
                serial: None,
                fact: Some(fact),
                ..
            } => Some(Probe::TcpPort {
                tcp_port: *port,
                fact: fact.clone(),
            }),
            Step::Probe {
                moonraker: None,
                ssh: None,
                tcp_port: None,
                serial: Some(serial),
                fact: None,
                ..
            } => Some(Probe::Serial { serial: *serial }),
            _ => None,
        }
    }
}

/// Verifies the signature over `bytes`, rejects a manifest that needs a newer app than
/// `app_version`, then parses it. An unknown step type is an error, never skipped.
pub fn verify_and_parse(
    bytes: &[u8],
    sig: &str,
    pubkey: &str,
    app_version: &str,
) -> Result<Manifest, ManifestError> {
    verify_signature(bytes, sig, pubkey)?;

    #[derive(Deserialize)]
    struct Header {
        min_app_version: String,
    }
    let header: Header =
        serde_json::from_slice(bytes).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    let need = FwVersion::parse(&header.min_app_version).ok_or_else(|| {
        ManifestError::Malformed(format!("min_app_version {:?}", header.min_app_version))
    })?;
    let have = FwVersion::parse(app_version)
        .ok_or_else(|| ManifestError::Malformed(format!("app version {app_version:?}")))?;
    if need > have {
        return Err(ManifestError::TooNew {
            min_app_version: header.min_app_version,
        });
    }

    let mut manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    manifest.sha256 = sha256_hex(bytes);
    for (name, steps) in &manifest.recipes {
        for (i, step) in steps.iter().enumerate() {
            if matches!(step, Step::Probe { .. }) && step.probe().is_none() {
                return Err(ManifestError::Malformed(format!(
                    "recipe {name} step {i}: probe needs exactly one of moonraker, ssh+fact, tcp_port+fact, serial"
                )));
            }
            if let Some(problem) = step.shape_error() {
                return Err(ManifestError::Malformed(format!(
                    "recipe {name} step {i}: {problem}"
                )));
            }
            if matches!(
                step,
                Step::HelixInstall {
                    update: true,
                    uninstall: true,
                    ..
                }
            ) {
                return Err(ManifestError::Malformed(format!(
                    "recipe {name} step {i}: helix_install cannot both update and uninstall"
                )));
            }
        }
    }
    Ok(manifest)
}

/// A verified manifest with the bytes and signature it was verified from.
#[derive(Debug, Clone)]
pub struct SignedManifest {
    pub bytes: Vec<u8>,
    pub sig: String,
    pub manifest: Manifest,
}

/// On-disk copy of the last good manifest, usable offline for `CACHE_MAX_AGE`.
pub struct ManifestCache {
    dir: PathBuf,
    pubkey: String,
    app_version: String,
}

impl ManifestCache {
    pub fn new(dir: PathBuf, pubkey: &str, app_version: &str) -> ManifestCache {
        ManifestCache {
            dir,
            pubkey: pubkey.to_string(),
            app_version: app_version.to_string(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Saves the manifest, its signature and the fetch time. The signature is checked again on load.
    pub fn store(&self, bytes: &[u8], sig: &str, now: SystemTime) -> std::io::Result<()> {
        let secs = now
            .duration_since(UNIX_EPOCH)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
            .as_secs();
        std::fs::create_dir_all(&self.dir)?;
        // The sidecar goes last: until it is renamed, the previous cache is still complete.
        self.replace("manifest.json", bytes)?;
        self.replace("manifest.json.minisig", sig.as_bytes())?;
        self.replace("manifest.fetched_at", secs.to_string().as_bytes())
    }

    fn replace(&self, name: &str, contents: &[u8]) -> std::io::Result<()> {
        write_atomic(&self.path(name), contents)
    }

    /// Returns the cached manifest only if it was fetched under 30 days before `now` and still
    /// verifies.
    pub fn load(&self, now: SystemTime) -> Option<Manifest> {
        self.load_signed(now).map(|s| s.manifest)
    }

    /// As `load`, keeping the signed bytes, which a run needs to keep its own copy.
    pub fn load_signed(&self, now: SystemTime) -> Option<SignedManifest> {
        let fetched: u64 = std::fs::read_to_string(self.path("manifest.fetched_at"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let age = now
            .duration_since(UNIX_EPOCH + Duration::from_secs(fetched))
            .ok()?;
        if age >= CACHE_MAX_AGE {
            return None;
        }
        let bytes = std::fs::read(self.path("manifest.json")).ok()?;
        let sig = std::fs::read_to_string(self.path("manifest.json.minisig")).ok()?;
        let manifest = verify_and_parse(&bytes, &sig, &self.pubkey, &self.app_version).ok()?;
        Some(SignedManifest {
            bytes,
            sig,
            manifest,
        })
    }
}
