// SPDX-License-Identifier: GPL-3.0-or-later

//! The commands the UI calls, and the state they share.
//!
//! `AppCore` holds everything a command needs and works without Tauri, so the command layer
//! runs end to end against the core fakes in `tests/commands.rs`; the `ipc` module only adapts
//! it to `#[tauri::command]`.
//!
//! Progress is pulled, not pushed: `advance` returns one `Event` per call. A long step reports
//! `Event::Progress` and returns, and the UI calls `advance` again with no input to continue it,
//! so the window stays responsive and no event exists outside a call's answer.
//!
//! Each run's `Runner` sits behind its own `tokio::sync::Mutex` in a map keyed by run id, so a
//! slow step on one run never holds up a command about another. The answers a user types,
//! passwords included, travel as `Input` straight into the runner; nothing here prints,
//! logs or stores one.

use async_trait::async_trait;
use helix_installer_core::app_log::AppLog;
use helix_installer_core::bundle::support_bundle_with_app_log;
use helix_installer_core::discovery::{self, Candidate};
use helix_installer_core::facts::Facts;
use helix_installer_core::identify::{identify, identify_as, start_state, Identified, StartState};
use helix_installer_core::manifest::{
    verify_and_parse, Channel, Manifest, ManifestCache, SignedManifest, Verified,
};
use helix_installer_core::record::record;
use helix_installer_core::run_state::{DoneKind, RunState, RunStatus};
use helix_installer_core::runner::{Event, Input, Runner};
use helix_installer_core::transport::Env;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::Mutex;

/// How long `discover` listens for printers.
pub const DISCOVERY_TIME: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// No signed printer list verified, fetched or cached.
    NoPrinterList,
    /// The candidate key is not one `discover` or `add_manual_host` returned.
    UnknownPrinter,
    UnknownRun,
    NotAnAddress,
    /// The printer cannot be named from what it answers; the user must pick its model.
    ChooseModel,
    /// Identification refused this printer; `doc` is the guide to follow instead.
    Unsupported,
    /// The run's files on this computer could not be read or written.
    Storage,
    /// The run has changed the printer, so it can only be continued, never dismissed.
    RunCommitted,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandError {
    pub kind: ErrorKind,
    pub message: String,
    pub doc: Option<String>,
}

impl CommandError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> CommandError {
        CommandError {
            kind,
            message: message.into(),
            doc: None,
        }
    }
}

fn storage(e: io::Error) -> CommandError {
    CommandError::new(ErrorKind::Storage, e.to_string())
}

/// Finds printers on the network. The app uses `LanDiscovery`; tests give a fixed list.
#[async_trait]
pub trait Discovery: Send + Sync {
    async fn discover(&self, env: &Env) -> Vec<Candidate>;
}

/// mDNS, a /24 sweep and FlashForge UDP, for `DISCOVERY_TIME`.
pub struct LanDiscovery;

#[async_trait]
impl Discovery for LanDiscovery {
    async fn discover(&self, env: &Env) -> Vec<Candidate> {
        discovery::discover(env, DISCOVERY_TIME).await
    }
}

pub struct AppConfig {
    /// Runs, the download cache, the cached printer list and SSH host keys live under here.
    pub data_dir: PathBuf,
    pub env: Env,
    pub discovery: Arc<dyn Discovery>,
    /// The minisign public key the printer list must be signed with.
    pub pubkey: String,
    /// Where to fetch the signed printer list: an `http(s)://` URL or a file path, with its
    /// signature beside it as `<location>.minisig`. `None` uses only the cached copy.
    pub manifest_location: Option<String>,
    /// Replaces the release channel of every `helix_install` step; `None` keeps the recipes'.
    pub channel_override: Option<Channel>,
    pub app_version: String,
    /// The app's own log; `None` writes nothing.
    pub log: Option<Arc<AppLog>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub version: String,
    /// What relative image paths in the printer list are relative to, when it came from a URL.
    pub image_base: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrinterView {
    pub id: String,
    pub name: String,
    pub photo: Option<String>,
    pub verified: Option<Verified>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// Identified with a recipe: `start_run` with no model starts it.
    Ready,
    /// It answers like more than one model; `choices` lists them.
    Choose,
    /// It answered, and no recipe covers it; `doc` is the guide.
    Unsupported,
    /// Nothing answered at the address; the user must pick the model.
    NoAnswer,
}

/// How the installer reaches a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Network,
    /// A panel plugged into this computer; `address` is its serial port.
    Usb,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateView {
    pub key: String,
    pub address: String,
    pub connection: Connection,
    pub name: String,
    pub photo: Option<String>,
    pub printer_id: Option<String>,
    pub status: CandidateStatus,
    /// False when nothing at the address answered the installer's questions.
    pub answered: bool,
    /// Set when HelixScreen is already installed: its version, empty when unknown.
    pub helix_version: Option<String>,
    /// The name the printer's own system goes by (Moonraker's `hostname`), as it reports it.
    pub host_name: Option<String>,
    pub choices: Vec<PrinterView>,
    pub doc: Option<String>,
    pub reason: Option<String>,
}

/// Where a run stands, sent with every event.
#[derive(Debug, Clone, Serialize)]
pub struct RunInfo {
    pub run_id: String,
    pub printer_name: String,
    pub address: String,
    /// 0-based index of the current step.
    pub step: usize,
    pub total_steps: usize,
    /// The current step's manifest `type`, `None` past the last step.
    pub step_kind: Option<String>,
    pub committed: bool,
    /// The printer's install guide.
    pub guide: String,
    /// Why a run that ended early is done, for the finish screen.
    pub done_note: Option<String>,
    /// What kind of ending `done_note` describes.
    pub done_kind: Option<DoneKind>,
    /// The name the printer's own system goes by, as on its picker card.
    pub host_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdvanceView {
    pub event: Event,
    pub run: RunInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub run_id: String,
    pub printer_name: String,
    pub address: String,
    pub step: usize,
    pub total_steps: Option<usize>,
    pub committed: bool,
    /// Why it stopped, when it failed.
    pub reason: Option<String>,
}

type SharedRunner = Arc<Mutex<Runner>>;

#[derive(Default)]
struct RunLogged {
    /// Entries of the run's own log already copied.
    entries: usize,
    line: String,
}

pub struct AppCore {
    cfg: AppConfig,
    manifest: Mutex<Option<Arc<SignedManifest>>>,
    candidates: std::sync::Mutex<Vec<Candidate>>,
    runs: Mutex<HashMap<String, SharedRunner>>,
    /// What each run has written to the app log so far.
    logged: std::sync::Mutex<HashMap<String, RunLogged>>,
}

fn printer_view(p: &helix_installer_core::manifest::Printer) -> PrinterView {
    PrinterView {
        id: p.id.clone(),
        name: p.name.clone(),
        photo: p.photo.clone(),
        verified: p.verified.clone(),
    }
}

fn printer_name(m: &Manifest, printer_id: &str) -> String {
    m.printers
        .iter()
        .find(|p| p.id == printer_id)
        .map_or_else(|| printer_id.to_string(), |p| p.name.clone())
}

/// The longest name shown or logged for a printer: a DNS label's length.
const MAX_NAME_LEN: usize = 63;

/// A name a device reported about itself, made safe to show and log: control characters (a
/// newline would forge app-log lines) are dropped and it is cut to `MAX_NAME_LEN` characters.
/// Nothing printable left is no name.
fn clean_name(raw: &str) -> Option<String> {
    let kept: String = raw.chars().filter(|c| !c.is_control()).collect();
    let name: String = kept.trim().chars().take(MAX_NAME_LEN).collect();
    let name = name.trim_end().to_string();
    (!name.is_empty()).then_some(name)
}

/// What the printer calls itself, for one the manifest does not name.
fn reported_name(facts: &Facts) -> Option<String> {
    ["model", "flashforge_udp.model", "klipper_hostname"]
        .iter()
        .find_map(|k| facts.get(*k).and_then(Value::as_str).and_then(clean_name))
}

/// One picker card for the log: where, what it was taken for, and what it runs.
fn describe(v: &CandidateView) -> String {
    format!(
        "{} {:?} {} (host name {}, printer {}, HelixScreen {})",
        v.address,
        v.status,
        v.name,
        v.host_name.as_deref().unwrap_or("-"),
        v.printer_id.as_deref().unwrap_or("-"),
        v.helix_version.as_deref().unwrap_or("-"),
    )
}

/// An event for the log. Prompts are named, never the answers typed into them.
fn describe_event(event: &Event) -> String {
    match event {
        Event::Progress {
            title,
            detail,
            fraction,
            ..
        } => {
            let mut s = format!("progress: {title}");
            if let Some(d) = detail {
                s.push_str(&format!(" ({d})"));
            }
            if let Some(f) = fraction {
                s.push_str(&format!(" {:.0}%", f * 100.0));
            }
            s
        }
        Event::NeedInput(prompt) => {
            let kind = serde_json::to_value(prompt)
                .ok()
                .and_then(|v| v.get("prompt").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "input".into());
            format!("waiting for the user: {kind}")
        }
        Event::Done => "done".into(),
        Event::Refused {
            cancelled: true, ..
        } => "stopped by the user".into(),
        Event::Refused { reason, doc, .. } => format!("refused: {reason} ({doc})"),
        Event::Failed { reason, .. } => format!("failed: {reason}"),
    }
}

/// The name the printer's own system goes by (Moonraker's `hostname`).
fn host_name(facts: &Facts) -> Option<String> {
    facts
        .get("klipper_hostname")
        .and_then(Value::as_str)
        .and_then(clean_name)
}

fn is_url(location: &str) -> bool {
    location.starts_with("https://") || location.starts_with("http://")
}

fn run_info(r: &Runner) -> RunInfo {
    let state = r.state();
    let m = r.manifest();
    let steps = m.recipes.get(&state.recipe_id);
    RunInfo {
        run_id: state.run_id.clone(),
        printer_name: printer_name(m, &state.printer_id),
        address: state.host.addr.clone(),
        step: state.step_index,
        total_steps: steps.map_or(0, Vec::len),
        step_kind: steps
            .and_then(|s| s.get(state.step_index))
            .map(|s| s.type_name().to_string()),
        committed: state.committed,
        guide: r.guide(),
        done_note: state.done_note.clone(),
        done_kind: state.done_kind,
        host_name: host_name(&state.facts),
    }
}

fn summary(state: &RunState, m: Option<&Manifest>) -> RunSummary {
    RunSummary {
        run_id: state.run_id.clone(),
        printer_name: m.map_or_else(
            || state.printer_id.clone(),
            |m| printer_name(m, &state.printer_id),
        ),
        address: state.host.addr.clone(),
        step: state.step_index,
        total_steps: m.and_then(|m| m.recipes.get(&state.recipe_id).map(Vec::len)),
        committed: state.committed,
        reason: match &state.status {
            RunStatus::Failed { reason, .. } => Some(reason.clone()),
            _ => None,
        },
    }
}

impl AppCore {
    pub fn new(cfg: AppConfig) -> AppCore {
        AppCore {
            cfg,
            manifest: Mutex::new(None),
            candidates: std::sync::Mutex::new(Vec::new()),
            runs: Mutex::new(HashMap::new()),
            logged: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Writes `text` to the app log with `secrets` removed.
    pub fn log(&self, text: &str, secrets: &[&str]) {
        if let Some(log) = &self.cfg.log {
            log.write(self.cfg.env.clock.now(), text, secrets);
        }
    }

    /// Logs the run's new engine log entries, then its event unless the event's line repeats
    /// the last one: a step polled many times logs once per change.
    fn log_run(&self, r: &Runner, event: &Event) {
        let state = r.state();
        let info = run_info(r);
        let secrets = r.secrets();
        let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let mut logged = self.logged.lock().unwrap_or_else(|e| e.into_inner());
        let seen = logged.entry(state.run_id.clone()).or_default();
        for e in state.log.iter().skip(seen.entries) {
            self.log(
                &format!("{}: step {}: {}", state.run_id, e.step + 1, e.text),
                &secrets,
            );
        }
        seen.entries = state.log.len();
        let at = match &info.step_kind {
            Some(kind) => format!("step {}/{} {kind}", info.step + 1, info.total_steps),
            None => "end".into(),
        };
        let line = format!(
            "{}: {at}{}: {}",
            state.run_id,
            if info.committed { " (committed)" } else { "" },
            describe_event(event)
        );
        if seen.line != line {
            self.log(&line, &secrets);
            seen.line = line;
        }
        if matches!(event, Event::Done | Event::Refused { .. }) {
            logged.remove(&state.run_id);
        }
    }

    fn runs_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("runs")
    }

    fn cache_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("downloads")
    }

    fn manifest_cache(&self) -> ManifestCache {
        ManifestCache::new(
            self.cfg.data_dir.join("printer-list"),
            &self.cfg.pubkey,
            &self.cfg.app_version,
        )
    }

    pub fn app_info(&self) -> AppInfo {
        let image_base = self
            .cfg
            .manifest_location
            .as_deref()
            .filter(|l| is_url(l))
            .and_then(|l| l.rfind('/').map(|i| l[..=i].to_string()));
        AppInfo {
            version: self.cfg.app_version.clone(),
            image_base,
        }
    }

    /// The verified printer list, loaded on first use and kept. A failed load is tried again
    /// on the next call, so the UI's "try again" works once the network is back.
    async fn manifest(&self) -> Result<Arc<SignedManifest>, CommandError> {
        let mut held = self.manifest.lock().await;
        if let Some(m) = held.as_ref() {
            return Ok(m.clone());
        }
        let loaded = match self.load_manifest().await {
            Ok(m) => Arc::new(m),
            Err(e) => {
                self.log(&format!("no printer list: {}", e.message), &[]);
                return Err(e);
            }
        };
        self.log(
            &format!(
                "printer list loaded: {} printers",
                loaded.manifest.printers.len()
            ),
            &[],
        );
        *held = Some(loaded.clone());
        Ok(loaded)
    }

    /// Fetches and verifies the list from `manifest_location`, caching it; otherwise falls
    /// back to the cached copy, which is used only while it verifies and is under 30 days old.
    async fn load_manifest(&self) -> Result<SignedManifest, CommandError> {
        let cache = self.manifest_cache();
        let mut problem = None;
        if let Some(location) = &self.cfg.manifest_location {
            match self.fetch_signed(location).await {
                Ok((bytes, sig)) => {
                    match verify_and_parse(&bytes, &sig, &self.cfg.pubkey, &self.cfg.app_version) {
                        Ok(manifest) => {
                            // A list that cannot be cached still serves this session.
                            let _ = cache.store(&bytes, &sig, self.cfg.env.clock.now());
                            return Ok(SignedManifest {
                                bytes,
                                sig,
                                manifest,
                            });
                        }
                        Err(e) => problem = Some(format!("{location}: {e}")),
                    }
                }
                Err(e) => problem = Some(e),
            }
        }
        if let Some(cached) = cache.load_signed(self.cfg.env.clock.now()) {
            return Ok(cached);
        }
        Err(CommandError::new(
            ErrorKind::NoPrinterList,
            problem.unwrap_or_else(|| {
                "no printer list location is set and no recent verified copy is saved".into()
            }),
        ))
    }

    async fn fetch_signed(&self, location: &str) -> Result<(Vec<u8>, String), String> {
        let sig_location = format!("{location}.minisig");
        if is_url(location) {
            let fetch = &self.cfg.env.fetch;
            let bytes = fetch
                .get_text(location)
                .await
                .map_err(|e| format!("{location}: {e}"))?;
            let sig = fetch
                .get_text(&sig_location)
                .await
                .map_err(|e| format!("{sig_location}: {e}"))?;
            return Ok((bytes.into_bytes(), sig));
        }
        let bytes = tokio::fs::read(location)
            .await
            .map_err(|e| format!("{location}: {e}"))?;
        let sig = tokio::fs::read_to_string(&sig_location)
            .await
            .map_err(|e| format!("{sig_location}: {e}"))?;
        Ok((bytes, sig))
    }

    pub async fn printers(&self) -> Result<Vec<PrinterView>, CommandError> {
        let m = self.manifest().await?;
        Ok(m.manifest.printers.iter().map(printer_view).collect())
    }

    fn view(&self, m: &Manifest, c: &Candidate) -> CandidateView {
        let answered = !c.facts.is_empty();
        let printer = |id: &str| m.printers.iter().find(|p| p.id == id);
        let mut view = CandidateView {
            key: c.key.clone(),
            address: c.host.addr.clone(),
            connection: if c.facts.contains_key("usb") {
                Connection::Usb
            } else {
                Connection::Network
            },
            name: reported_name(&c.facts).unwrap_or_else(|| c.host.addr.clone()),
            photo: None,
            printer_id: None,
            status: CandidateStatus::NoAnswer,
            answered,
            helix_version: None,
            host_name: host_name(&c.facts),
            choices: Vec::new(),
            doc: None,
            reason: None,
        };
        match identify(m, &c.facts) {
            Identified::Recipe { printer_id, .. } => {
                if let Some(p) = printer(&printer_id) {
                    view.name = p.name.clone();
                    view.photo = p.photo.clone();
                }
                view.printer_id = Some(printer_id);
                view.status = CandidateStatus::Ready;
                if let StartState::Helix(v) = start_state(&c.facts) {
                    view.helix_version = Some(v);
                }
            }
            Identified::Ambiguous { candidates } => {
                view.status = CandidateStatus::Choose;
                view.choices = candidates
                    .iter()
                    .filter_map(|id| printer(id))
                    .map(printer_view)
                    .collect();
            }
            Identified::Refuse { .. } if !answered => {}
            Identified::Refuse {
                printer_id,
                doc,
                reason,
            } => {
                if let Some(p) = printer_id.as_deref().and_then(printer) {
                    view.name = p.name.clone();
                    view.photo = p.photo.clone();
                }
                view.printer_id = printer_id;
                view.status = CandidateStatus::Unsupported;
                view.doc = Some(doc);
                view.reason = Some(reason);
            }
        }
        view
    }

    fn candidate(&self, key: &str) -> Result<Candidate, CommandError> {
        let held = self.candidates.lock().unwrap_or_else(|e| e.into_inner());
        held.iter().find(|c| c.key == key).cloned().ok_or_else(|| {
            CommandError::new(ErrorKind::UnknownPrinter, format!("no printer {key:?}"))
        })
    }

    pub async fn discover(&self) -> Result<Vec<CandidateView>, CommandError> {
        let m = self.manifest().await?;
        let found = self.cfg.discovery.discover(&self.cfg.env).await;
        let views: Vec<CandidateView> = found.iter().map(|c| self.view(&m.manifest, c)).collect();
        self.log(&format!("discovery found {} printers", views.len()), &[]);
        for v in &views {
            self.log(&format!("  {}", describe(v)), &[]);
        }
        *self.candidates.lock().unwrap_or_else(|e| e.into_inner()) = found;
        Ok(views)
    }

    /// Asks the address the user typed what it is. An address where nothing answers is still
    /// returned (status `NoAnswer`): a stock printer runs nothing the installer can ask.
    pub async fn add_manual_host(&self, ip: &str) -> Result<CandidateView, CommandError> {
        let m = self.manifest().await?;
        let host = discovery::manual_host(ip)
            .map_err(|e| CommandError::new(ErrorKind::NotAnAddress, e.to_string()))?;
        let facts = discovery::probe_host(self.cfg.env.moonraker.as_ref(), &host)
            .await
            .unwrap_or_default();
        let candidate = Candidate::new(host, facts);
        let view = self.view(&m.manifest, &candidate);
        self.log(&format!("address typed in: {}", describe(&view)), &[]);
        let mut held = self.candidates.lock().unwrap_or_else(|e| e.into_inner());
        held.retain(|c| c.key != candidate.key);
        held.push(candidate);
        Ok(view)
    }

    /// A run id that names no saved run: the time plus a counter, so ids sort by start.
    fn new_run_id(&self) -> String {
        let ms = self
            .cfg
            .env
            .clock
            .now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let dir = self.runs_dir();
        (0u32..)
            .map(|n| format!("run-{ms}-{n}"))
            .find(|id| !dir.join(format!("{id}.json")).exists())
            .unwrap_or_default()
    }

    /// Starts installing on the candidate. `printer_id` is the model the user picked, needed
    /// when the printer answers like several models or not at all.
    pub async fn start_run(
        &self,
        candidate_key: &str,
        printer_id: Option<&str>,
    ) -> Result<String, CommandError> {
        let m = self.manifest().await?;
        let c = self.candidate(candidate_key)?;
        let identified = match printer_id {
            Some(id) => identify_as(&m.manifest, id, &c.facts),
            None => identify(&m.manifest, &c.facts),
        };
        let (printer_id, recipe_id) = match identified {
            Identified::Recipe {
                printer_id,
                recipe_id,
                ..
            } => (printer_id, recipe_id),
            Identified::Ambiguous { .. } => {
                return Err(CommandError::new(
                    ErrorKind::ChooseModel,
                    "this printer answers like more than one model",
                ))
            }
            Identified::Refuse { .. } if printer_id.is_none() && c.facts.is_empty() => {
                return Err(CommandError::new(
                    ErrorKind::ChooseModel,
                    "nothing at this address answered",
                ))
            }
            Identified::Refuse { doc, reason, .. } => {
                return Err(CommandError {
                    kind: ErrorKind::Unsupported,
                    message: reason,
                    doc: Some(doc),
                })
            }
        };
        let run_id = self.new_run_id();
        let state = RunState::new(&run_id, printer_id, recipe_id, c.host, c.facts);
        let runner = Runner::start(
            self.cfg.env.clone(),
            &self.runs_dir(),
            &m.bytes,
            &m.sig,
            &self.cfg.pubkey,
            &self.cfg.app_version,
            state,
        )
        .map_err(storage)?
        .with_cache_dir(self.cache_dir());
        let runner = self.with_channel(runner);
        self.log(
            &format!(
                "{run_id}: started for {} at {} (printer {}, recipe {})",
                c.key,
                runner.state().host.addr,
                runner.state().printer_id,
                runner.state().recipe_id
            ),
            &[],
        );
        self.runs
            .lock()
            .await
            .insert(run_id.clone(), Arc::new(Mutex::new(runner)));
        Ok(run_id)
    }

    fn with_channel(&self, runner: Runner) -> Runner {
        match self.cfg.channel_override {
            Some(channel) => runner.with_channel_override(channel),
            None => runner,
        }
    }

    /// The run's runner, reopened from disk when this app has not touched it yet.
    async fn runner(&self, run_id: &str) -> Result<SharedRunner, CommandError> {
        let mut runs = self.runs.lock().await;
        if let Some(r) = runs.get(run_id) {
            return Ok(r.clone());
        }
        let runner = Runner::resume(
            self.cfg.env.clone(),
            &self.runs_dir(),
            run_id,
            &self.cfg.pubkey,
            &self.cfg.app_version,
        )
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => {
                CommandError::new(ErrorKind::UnknownRun, format!("no run {run_id:?}"))
            }
            _ => storage(e),
        })?
        .with_cache_dir(self.cache_dir());
        let runner = self.with_channel(runner);
        let shared = Arc::new(Mutex::new(runner));
        runs.insert(run_id.to_string(), shared.clone());
        Ok(shared)
    }

    /// Runs the install until it needs the user, reports progress, or ends. `input` answers the
    /// prompt the previous call returned; after a failure, `None` retries the failed step.
    pub async fn advance(
        &self,
        run_id: &str,
        input: Option<Input>,
    ) -> Result<AdvanceView, CommandError> {
        let runner = match self.runner(run_id).await {
            Ok(r) => r,
            Err(e) => {
                self.log(&format!("{run_id}: {}", e.message), &[]);
                return Err(e);
            }
        };
        let mut r = runner.lock().await;
        let event = r.advance(input).await;
        self.log_run(&r, &event);
        Ok(AdvanceView {
            event,
            run: run_info(&r),
        })
    }

    /// Runs saved on this computer that neither finished nor were refused.
    pub async fn unfinished_runs(&self) -> Vec<RunSummary> {
        let current = self.manifest().await.ok();
        let runs = self.runs.lock().await.clone();
        let mut out = Vec::new();
        for state in Runner::list_unfinished(&self.runs_dir()) {
            let s = match runs.get(&state.run_id) {
                Some(r) => {
                    let r = r.lock().await;
                    summary(r.state(), Some(r.manifest()))
                }
                None => summary(&state, current.as_deref().map(|m| &m.manifest)),
            };
            out.push(s);
        }
        out
    }

    /// Ends a run that has not changed the printer, marked cancelled, so it is never offered
    /// again. A run past its point of no return is refused: only finishing it, or the
    /// printer's recovery guide, puts the printer right.
    pub async fn dismiss_run(&self, run_id: &str) -> Result<(), CommandError> {
        let runner = self.runner(run_id).await?;
        let mut r = runner.lock().await;
        if r.state().committed {
            return Err(CommandError::new(
                ErrorKind::RunCommitted,
                "this install has already changed the printer",
            ));
        }
        self.log(&format!("{run_id}: dismissed"), &[]);
        self.logged
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
        match r.advance(Some(Input::Cancel)).await {
            Event::Failed { reason, .. } => Err(CommandError::new(ErrorKind::Storage, reason)),
            _ => Ok(()),
        }
    }

    /// The run's support report, with what the printer printed while this app followed the
    /// run. Passwords held in memory are always stripped; `mask` also hides addresses and the
    /// serial.
    pub async fn export_bundle(&self, run_id: &str, mask: bool) -> Result<String, CommandError> {
        let app_log = self
            .cfg
            .log
            .as_ref()
            .map(|l| l.contents())
            .unwrap_or_default();
        let held = self.runs.lock().await.get(run_id).cloned();
        if let Some(r) = held {
            let r = r.lock().await;
            let secrets = r.secrets();
            let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
            return Ok(support_bundle_with_app_log(
                r.state(),
                &r.job_logs(),
                &app_log,
                &secrets,
                mask,
            ));
        }
        let state = Runner::load(&self.runs_dir(), run_id).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => {
                CommandError::new(ErrorKind::UnknownRun, format!("no run {run_id:?}"))
            }
            _ => storage(e),
        })?;
        Ok(support_bundle_with_app_log(
            &state,
            &[],
            &app_log,
            &[],
            mask,
        ))
    }

    /// What the candidate answered, masked, as a fixture for the identify tests.
    pub async fn record_printer(&self, candidate_key: &str) -> Result<String, CommandError> {
        Ok(record(&self.candidate(candidate_key)?, &[]))
    }
}

/// `AppCore` as Tauri commands. Arguments arrive camelCased (`runId`, `candidateKey`).
pub mod ipc {
    use super::*;
    use tauri::State;

    type Core<'a> = State<'a, AppCore>;

    #[tauri::command]
    pub async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
        crate::updater::install(app).await
    }

    #[tauri::command]
    pub fn pending_update(app: tauri::AppHandle) -> Option<String> {
        crate::updater::pending_version(&app)
    }

    #[tauri::command]
    pub fn app_info(core: Core<'_>) -> AppInfo {
        core.app_info()
    }

    #[tauri::command]
    pub async fn printers(core: Core<'_>) -> Result<Vec<PrinterView>, CommandError> {
        core.printers().await
    }

    #[tauri::command]
    pub async fn discover(core: Core<'_>) -> Result<Vec<CandidateView>, CommandError> {
        core.discover().await
    }

    #[tauri::command]
    pub async fn add_manual_host(
        core: Core<'_>,
        ip: String,
    ) -> Result<CandidateView, CommandError> {
        core.add_manual_host(&ip).await
    }

    #[tauri::command]
    pub async fn start_run(
        core: Core<'_>,
        candidate_key: String,
        printer_id: Option<String>,
    ) -> Result<String, CommandError> {
        core.start_run(&candidate_key, printer_id.as_deref()).await
    }

    #[tauri::command]
    pub async fn advance(
        core: Core<'_>,
        run_id: String,
        input: Option<Input>,
    ) -> Result<AdvanceView, CommandError> {
        core.advance(&run_id, input).await
    }

    #[tauri::command]
    pub async fn unfinished_runs(core: Core<'_>) -> Result<Vec<RunSummary>, CommandError> {
        Ok(core.unfinished_runs().await)
    }

    #[tauri::command]
    pub async fn dismiss_run(core: Core<'_>, run_id: String) -> Result<(), CommandError> {
        core.dismiss_run(&run_id).await
    }

    #[tauri::command]
    pub async fn export_bundle(
        core: Core<'_>,
        run_id: String,
        mask: bool,
    ) -> Result<String, CommandError> {
        core.export_bundle(&run_id, mask).await
    }

    #[tauri::command]
    pub async fn record_printer(
        core: Core<'_>,
        candidate_key: String,
    ) -> Result<String, CommandError> {
        core.record_printer(&candidate_key).await
    }
}
