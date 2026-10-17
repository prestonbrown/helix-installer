// SPDX-License-Identifier: GPL-3.0-or-later

//! The signed manifest that carries every piece of printer knowledge the installer acts on.

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
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
    pub restore: String,
    pub fallback: Fallback,
    #[serde(default)]
    pub verified: Option<Verified>,
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
    Range(VersionRange),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainsRepr {
    contains: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CondRepr {
    Text(String),
    Contains(ContainsRepr),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    pub user: String,
    pub password: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Probe {
    Moonraker { moonraker: MoonrakerProbe },
    Ssh { ssh: String, fact: String },
    TcpPort { tcp_port: u16, fact: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitTarget {
    Port(u16),
    Moonraker,
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
    /// Exactly one of `moonraker`, `ssh` (with `fact`) or `tcp_port` (with `fact`); `Step::probe`
    /// returns it as a `Probe`, and `verify_and_parse` rejects any other combination.
    Probe {
        #[serde(default)]
        moonraker: Option<MoonrakerProbe>,
        #[serde(default)]
        ssh: Option<String>,
        #[serde(default)]
        tcp_port: Option<u16>,
        #[serde(default)]
        fact: Option<String>,
        #[serde(default)]
        commits: bool,
    },
    Require {
        fact: String,
        range: VersionRange,
        doc: String,
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
        update: bool,
        #[serde(default)]
        dir: Option<String>,
        #[serde(default)]
        commits: bool,
    },
}

impl Step {
    /// The probe a `Step::Probe` describes, or `None` for any other step or an invalid combination.
    pub fn probe(&self) -> Option<Probe> {
        match self {
            Step::Probe {
                moonraker: Some(m),
                ssh: None,
                tcp_port: None,
                fact: None,
                ..
            } => Some(Probe::Moonraker { moonraker: *m }),
            Step::Probe {
                moonraker: None,
                ssh: Some(ssh),
                tcp_port: None,
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
                fact: Some(fact),
                ..
            } => Some(Probe::TcpPort {
                tcp_port: *port,
                fact: fact.clone(),
            }),
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

    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    for (name, steps) in &manifest.recipes {
        for (i, step) in steps.iter().enumerate() {
            if matches!(step, Step::Probe { .. }) && step.probe().is_none() {
                return Err(ManifestError::Malformed(format!(
                    "recipe {name} step {i}: probe needs exactly one of moonraker, ssh+fact, tcp_port+fact"
                )));
            }
        }
    }
    Ok(manifest)
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

    /// Writes `name` via a temp file in the same directory so a reader never sees a partial file.
    fn replace(&self, name: &str, contents: &[u8]) -> std::io::Result<()> {
        let tmp = self.path(&format!("{name}.tmp"));
        std::fs::write(&tmp, contents)?;
        std::fs::rename(&tmp, self.path(name)).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    /// Returns the cached manifest only if it was fetched under 30 days before `now` and still
    /// verifies.
    pub fn load(&self, now: SystemTime) -> Option<Manifest> {
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
        verify_and_parse(&bytes, &sig, &self.pubkey, &self.app_version).ok()
    }
}
