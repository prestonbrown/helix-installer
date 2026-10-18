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
use helix_installer_core::bundle::support_bundle;
use helix_installer_core::discovery::{self, Candidate};
use helix_installer_core::facts::Facts;
use helix_installer_core::identify::{identify, identify_as, start_state, Identified, StartState};
use helix_installer_core::manifest::{
    verify_and_parse, Channel, Manifest, ManifestCache, SignedManifest, Verified,
};
use helix_installer_core::record::record;
use helix_installer_core::run_state::{RunState, RunStatus};
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

#[derive(Debug, Clone, Serialize)]
pub struct CandidateView {
    pub key: String,
    pub address: String,
    pub name: String,
    pub photo: Option<String>,
    pub printer_id: Option<String>,
    pub status: CandidateStatus,
    /// False when nothing at the address answered the installer's questions.
    pub answered: bool,
    /// Set when HelixScreen is already installed: its version, empty when unknown.
    pub helix_version: Option<String>,
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
    /// Why a run that ended early is done, for the finish screen ("HelixScreen is up to date").
    pub done_note: Option<String>,
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

pub struct AppCore {
    cfg: AppConfig,
    manifest: Mutex<Option<Arc<SignedManifest>>>,
    candidates: std::sync::Mutex<Vec<Candidate>>,
    runs: Mutex<HashMap<String, SharedRunner>>,
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

/// What the printer calls itself, for one the manifest does not name.
fn reported_name(facts: &Facts) -> Option<String> {
    ["model", "flashforge_udp.model", "klipper_hostname"]
        .iter()
        .find_map(|k| facts.get(*k).and_then(Value::as_str))
        .map(str::to_string)
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
        let loaded = Arc::new(self.load_manifest().await?);
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
            name: reported_name(&c.facts).unwrap_or_else(|| c.host.addr.clone()),
            photo: None,
            printer_id: None,
            status: CandidateStatus::NoAnswer,
            answered,
            helix_version: None,
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
        let views = found.iter().map(|c| self.view(&m.manifest, c)).collect();
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
        let runner = self.runner(run_id).await?;
        let mut r = runner.lock().await;
        let event = r.advance(input).await;
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
        match r.advance(Some(Input::Cancel)).await {
            Event::Failed { reason, .. } => Err(CommandError::new(ErrorKind::Storage, reason)),
            _ => Ok(()),
        }
    }

    /// The run's support report, with what the printer printed while this app followed the
    /// run. Passwords held in memory are always stripped; `mask` also hides addresses and the
    /// serial.
    pub async fn export_bundle(&self, run_id: &str, mask: bool) -> Result<String, CommandError> {
        let held = self.runs.lock().await.get(run_id).cloned();
        if let Some(r) = held {
            let r = r.lock().await;
            let secrets = r.secrets();
            let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
            return Ok(support_bundle(r.state(), &r.job_logs(), &secrets, mask));
        }
        let state = Runner::load(&self.runs_dir(), run_id).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => {
                CommandError::new(ErrorKind::UnknownRun, format!("no run {run_id:?}"))
            }
            _ => storage(e),
        })?;
        Ok(support_bundle(&state, &[], &[], mask))
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
