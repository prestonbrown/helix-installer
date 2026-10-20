// SPDX-License-Identifier: GPL-3.0-or-later

//! Walks a recipe one step at a time, pausing whenever the user must act, and enforces the
//! point of no return: before it a stop is a refusal (nothing changed), after it every stop is a
//! committed failure the UI must turn into recovery.

use crate::facts::Facts;
use crate::identify::{self, Identified, StartState, GENERIC_REFUSE_DOC};
use crate::manifest::{verify_and_parse, Channel, Choice, Login, Manifest, Secret, Step};
use crate::run_state::{self, recipe_sha256, LogEntry, RunState, RunStatus, IDENTITY_KEYS};
use crate::steps::{self, Session, StepCtx, StepHandler, StepOutcome};
use crate::transport::{Drive, Env};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "prompt", rename_all = "snake_case")]
pub enum Prompt {
    Info {
        text: String,
        image: Option<String>,
    },
    UserAction {
        text: String,
        image: Option<String>,
    },
    /// The installer is about to do what `text` says; `button` (answered with `Input::Next`) lets
    /// it go ahead.
    Confirm {
        text: String,
        button: String,
    },
    Ask {
        question: String,
        choices: Vec<Choice>,
    },
    /// `password_hint` says where this printer shows its password, when the recipe knows.
    /// `has_ssh_key` says this computer has an SSH key or agent to sign in with instead, and
    /// `key_locked` that a key file needs its passphrase typed before it can be used.
    Login {
        user_hint: Option<String>,
        password_hint: Option<String>,
        has_ssh_key: bool,
        key_locked: bool,
    },
    /// sudo on the printer needs `user`'s password, which a key login does not carry.
    /// `refused` says sudo turned down the one typed last.
    SudoPassword {
        user: String,
        refused: bool,
    },
    /// `message` says why `drives` is empty, when it is.
    ChooseDrive {
        drives: Vec<Drive>,
        needed_bytes: u64,
        message: Option<String>,
    },
    Ambiguous {
        candidates: Vec<String>,
    },
}

/// What the user answered. Deserialize-only: a `Login` carries a password, so an input is never
/// written anywhere.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "input", content = "value", rename_all = "snake_case")]
pub enum Input {
    Next,
    Choice(String),
    Login(Login),
    /// Answers `Prompt::SudoPassword`.
    SudoPassword(Secret),
    Drive(String),
    Cancel,
}

/// `step` is the 0-based index into the recipe; `total` is its length.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    NeedInput(Prompt),
    Progress {
        step: usize,
        total: usize,
        title: String,
        detail: Option<String>,
        fraction: Option<f32>,
    },
    Done,
    Refused {
        doc: String,
        reason: String,
        /// The user stopped the run before it changed the printer; the UI says so rather
        /// than showing a refusal.
        cancelled: bool,
    },
    Failed {
        committed: bool,
        step: usize,
        reason: String,
        help: Option<String>,
    },
}

pub struct Runner {
    env: Env,
    manifest: Arc<Manifest>,
    state: RunState,
    handlers: HashMap<String, Arc<dyn StepHandler>>,
    session: Session,
    cache_dir: Option<PathBuf>,
    channel_override: Option<Channel>,
    autosave: Option<PathBuf>,
    /// Times the current step has run since it became current.
    calls: u32,
    /// Why the run's own manifest copy could not be verified on resume; the run cannot go on.
    manifest_error: Option<String>,
    /// The step the probes before it were last re-run for, so a printer they do not identify
    /// stops the run instead of being probed again and again.
    replayed_for: Option<usize>,
    /// The step reporting progress and the largest fraction it has reported, so its bar never
    /// moves backwards: install.sh reuses a step number after a skip.
    progress_floor: Option<(usize, f32)>,
}

impl Runner {
    /// A run loaded from disk trusts none of its `fresh` facts: the address may answer for
    /// another printer by now.
    pub fn new(env: Env, manifest: Arc<Manifest>, mut state: RunState) -> Runner {
        state.fresh.clear();
        Runner {
            env,
            manifest,
            state,
            handlers: HashMap::new(),
            session: Session::default(),
            cache_dir: None,
            channel_override: None,
            autosave: None,
            calls: 0,
            manifest_error: None,
            replayed_for: None,
            progress_floor: None,
        }
    }

    /// Starts a run in `dir`: verifies the manifest, keeps a copy of its bytes and signature
    /// beside the run's state so a later resume runs against the same recipe even after a newer
    /// manifest is published, and saves the state after every step.
    pub fn start(
        env: Env,
        dir: &Path,
        manifest_bytes: &[u8],
        sig: &str,
        pubkey: &str,
        app_version: &str,
        state: RunState,
    ) -> io::Result<Runner> {
        let manifest = verify_and_parse(manifest_bytes, sig, pubkey, app_version)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        run_state::save_manifest(dir, &state.run_id, manifest_bytes, sig)?;
        let runner = Runner::new(env, Arc::new(manifest), state).with_autosave(dir.to_path_buf());
        runner.save(dir)?;
        Ok(runner)
    }

    /// Reopens a run saved in `dir` against its own manifest copy, re-verified. A copy that no
    /// longer verifies is reported by the first `advance`, as a refusal before the point of no
    /// return and a committed failure after it.
    pub fn resume(
        env: Env,
        dir: &Path,
        run_id: &str,
        pubkey: &str,
        app_version: &str,
    ) -> io::Result<Runner> {
        let state = run_state::load(dir, run_id)?;
        let (manifest, error) = match run_state::load_manifest(dir, run_id, pubkey, app_version) {
            Ok(m) => (m, None),
            Err(e) => (Manifest::default(), Some(e)),
        };
        let mut runner =
            Runner::new(env, Arc::new(manifest), state).with_autosave(dir.to_path_buf());
        runner.manifest_error = error;
        Ok(runner)
    }

    /// Runs every step of `step_type` (its manifest `type` tag) through `handler`.
    pub fn with_handler(mut self, step_type: &str, handler: Arc<dyn StepHandler>) -> Runner {
        self.handlers.insert(step_type.to_string(), handler);
        self
    }

    /// Where `download` keeps verified artifacts.
    pub fn with_cache_dir(mut self, dir: PathBuf) -> Runner {
        self.cache_dir = Some(dir);
        self
    }

    /// Makes every `helix_install` step read `channel`'s release list, whatever its recipe says.
    pub fn with_channel_override(mut self, channel: Channel) -> Runner {
        self.channel_override = Some(channel);
        self
    }

    /// Saves the state to `dir` after every completed step and before every event, so closing
    /// the app at any moment loses at most the step in flight.
    pub fn with_autosave(mut self, dir: PathBuf) -> Runner {
        self.autosave = Some(dir);
        self
    }

    pub fn state(&self) -> &RunState {
        &self.state
    }

    /// The passwords this run holds in memory, for `bundle::support_bundle` to strip.
    pub fn secrets(&self) -> Vec<String> {
        self.session.secrets()
    }

    /// What the printer-side jobs this runner followed have printed, held in memory only. A
    /// reopened app has only what it has read since.
    pub fn job_logs(&self) -> Vec<String> {
        self.session.job_logs()
    }

    /// The manifest the run is pinned to: its own verified copy, not the newest one.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The printer's install guide from the run's own manifest, else the generic one.
    pub fn guide(&self) -> String {
        self.printer_doc()
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        run_state::save(&self.state, dir)
    }

    pub fn load(dir: &Path, run_id: &str) -> io::Result<RunState> {
        run_state::load(dir, run_id)
    }

    pub fn list_unfinished(dir: &Path) -> Vec<RunState> {
        run_state::list_unfinished(dir)
    }

    /// Runs steps from the current one until one needs the user, reports progress, or the run
    /// ends. `input` answers the prompt the previous call returned. After `Failed`, the next
    /// call retries the failed step.
    pub async fn advance(&mut self, mut input: Option<Input>) -> Event {
        match &self.state.status {
            RunStatus::Done => return Event::Done,
            RunStatus::Refused {
                doc,
                reason,
                cancelled,
            } => {
                return Event::Refused {
                    doc: doc.clone(),
                    reason: reason.clone(),
                    cancelled: *cancelled,
                }
            }
            RunStatus::Running | RunStatus::Failed { .. } => {}
        }
        if let Some(e) = self.manifest_error.clone() {
            self.log(
                self.state.step_index,
                format!("the run's manifest copy did not verify: {e}"),
            );
            return self.manifest_changed();
        }
        let manifest = self.manifest.clone();
        let Some(steps) = manifest.recipes.get(&self.state.recipe_id) else {
            let reason = format!("recipe {} is not in the manifest", self.state.recipe_id);
            return self.fail(reason, None);
        };
        if !self.pinned_to(steps) {
            return self.manifest_changed();
        }
        if matches!(input, Some(Input::Cancel)) {
            // A write stopped part way leaves the panel unbootable and its port held open, so a
            // running one is followed to its end instead.
            if !self.session.flash_running() {
                self.session.release_flash().await;
                return self.cancel();
            }
            self.log(
                self.state.step_index,
                "cancel ignored: the panel is being written and must finish".into(),
            );
            input = None;
        }
        // A retry may come much later, after the address has moved to another printer.
        if matches!(self.state.status, RunStatus::Failed { .. }) {
            self.state.fresh.clear();
            self.replayed_for = None;
            self.progress_floor = None;
        }
        self.state.status = RunStatus::Running;

        loop {
            let index = self.state.step_index;
            let Some(step) = steps.get(index) else {
                if index > steps.len() {
                    let reason = format!("the saved run is at step {index} of {}", steps.len());
                    return self.fail(reason, None);
                }
                self.state.status = RunStatus::Done;
                self.log(index, "recipe complete".into());
                self.end_of_run(&manifest).await;
                return self.persist(Event::Done);
            };

            // Re-identifying: only the probes run on the way back to the step that needs them.
            if let Some(until) = self.state.replay_until {
                if index >= until {
                    self.state.replay_until = None;
                } else if !matches!(step, Step::Probe { .. }) {
                    self.state.step_index += 1;
                    continue;
                }
            }

            // A recorded job is followed rather than run again, so there is nothing to ask.
            if self.calls == 0
                && self.state.started_step == Some(index)
                && !step.is_idempotent()
                && self.state.job.is_none()
            {
                match input.take() {
                    Some(Input::Choice(c)) if c == RERUN => {}
                    Some(Input::Choice(c)) if c == STOP => {
                        let reason = "stopped: this step may already have run on the printer";
                        return self.fail(reason.into(), None);
                    }
                    _ => return self.persist(Event::NeedInput(rerun_prompt())),
                }
            }

            // The committing step acts on the identification made before it; every later step
            // that acts needs the printer identified again (`RunState::fresh`).
            // When it has not been, the probes since the last commit or wait (idempotent) run
            // again once, then this step is reconsidered.
            if self.state.committed
                && step.acts_on_printer()
                && !step.commits()
                && !identify::answers_as(&manifest, &self.state.printer_id, &self.state.fresh)
            {
                match probes_since_boundary(steps, index) {
                    Some(start) if self.replayed_for != Some(index) => {
                        self.replayed_for = Some(index);
                        self.state.replay_until = Some(index);
                        self.state.fresh.clear();
                        self.state.step_index = start;
                        self.calls = 0;
                        self.log(
                            index,
                            "probing the printer again before acting on it".into(),
                        );
                        continue;
                    }
                    _ => return self.not_reidentified(&manifest),
                }
            }

            // Recorded on disk before the step acts, so a crash inside it resumes as committed.
            let mut changed = false;
            if step.commits() && !self.state.committed && commits_on(step, input.as_ref()) {
                self.state.committed = true;
                self.state.fresh.clear();
                self.log(index, "passed the point of no return".into());
                changed = true;
            }
            if self.state.started_step != Some(index) {
                self.state.started_step = Some(index);
                changed = true;
            }
            if changed {
                if let Err(e) = self.autosave() {
                    return self.fail(format!("could not save install progress: {e}"), None);
                }
            }

            let outcome = self.run_step(&manifest, step, input.take()).await;
            self.calls += 1;
            if std::mem::take(&mut self.session.commit_requested) && !self.state.committed {
                self.state.committed = true;
                self.state.fresh.clear();
                self.log(index, "passed the point of no return".into());
                if let Err(e) = self.autosave() {
                    // Nothing was changed yet: the prepared flash lets the panel go unwritten.
                    self.session.release_flash().await;
                    return self.fail(format!("could not save install progress: {e}"), None);
                }
            }
            match outcome {
                StepOutcome::Done => {
                    if step.commits() && self.state.identity.is_none() {
                        self.record_identity(index);
                    }
                    // The printer may have restarted while the step waited: what was probed
                    // before it may describe another machine now answering at the address.
                    if self.state.committed && matches!(step, Step::WaitFor { .. }) {
                        self.state.fresh.clear();
                    }
                    self.log(index, format!("{}: done", step.type_name()));
                    self.state.step_index += 1;
                    self.state.started_step = None;
                    self.calls = 0;
                    if let Err(e) = self.autosave() {
                        let reason = format!("could not save install progress: {e}");
                        return self.fail(reason, None);
                    }
                    if matches!(step, Step::Probe { .. }) && !self.state.committed {
                        if let Some(ev) = self.switch_recipe(&manifest, &steps[..=index]) {
                            return ev;
                        }
                    }
                }
                StepOutcome::NeedInput(prompt) => return self.persist(Event::NeedInput(prompt)),
                StepOutcome::Progress {
                    title,
                    detail,
                    fraction,
                } => {
                    let fraction = self.never_backwards(index, fraction);
                    return self.persist(Event::Progress {
                        step: index,
                        total: steps.len(),
                        title,
                        detail,
                        fraction,
                    });
                }
                StepOutcome::Refused { doc, reason } if !self.state.committed => {
                    return self.refuse(doc, reason)
                }
                StepOutcome::Refused { doc, reason } => return self.fail(reason, Some(doc)),
                StepOutcome::Failed { reason, help } => return self.fail(reason, help),
                StepOutcome::Complete { note, kind } => {
                    self.log(index, format!("{}{note}", kind.label()));
                    self.state.done_note = Some(note);
                    self.state.done_kind = Some(kind);
                    self.state.status = RunStatus::Done;
                    self.state.step_index = steps.len();
                    self.state.started_step = None;
                    self.calls = 0;
                    self.end_of_run(&manifest).await;
                    return self.persist(Event::Done);
                }
            }
        }
    }

    /// True when the run belongs to this manifest and recipe. A run that has not started is
    /// pinned to them here; one part-way through without a pin cannot be trusted.
    fn pinned_to(&mut self, steps: &[Step]) -> bool {
        let manifest = &self.manifest.sha256;
        let recipe = recipe_sha256(steps);
        match (&self.state.manifest_sha256, &self.state.recipe_sha256) {
            (Some(m), Some(r)) => m == manifest && *r == recipe,
            (None, None) if self.state.step_index == 0 && !self.state.committed => {
                self.state.manifest_sha256 = Some(manifest.clone());
                self.state.recipe_sha256 = Some(recipe);
                true
            }
            _ => false,
        }
    }

    fn manifest_changed(&mut self) -> Event {
        let doc = self.printer_doc();
        if self.state.committed {
            let help = format!(
                "The installer's printer data changed mid-install; follow the recovery guide: {doc}"
            );
            self.fail(
                "the installer's printer data changed since this install started".into(),
                Some(help),
            )
        } else {
            self.refuse(
                doc,
                "the installer's printer data changed; start again".into(),
            )
        }
    }

    /// Before the point of no return, facts a probe added can change which recipe the printer
    /// needs (an install that finds HelixScreen already there becomes an update). The run then
    /// moves to that recipe, skipping its leading steps that equal the steps already `done`,
    /// and the switch is reported as progress; facts that leave the printer no recipe refuse
    /// the run. Only a run on one of the printer's own install or update recipes switches, so a
    /// restore stays a restore. After the commit the recipe never changes.
    fn switch_recipe(&mut self, manifest: &Manifest, done: &[Step]) -> Option<Event> {
        let printer = manifest
            .printers
            .iter()
            .find(|p| p.id == self.state.printer_id)?;
        if !printer
            .recipes
            .iter()
            .any(|c| c.recipe == self.state.recipe_id)
        {
            return None;
        }
        let (state, to) = match identify::choose_for(manifest, &printer.id, &self.state.facts)? {
            Identified::Recipe {
                state, recipe_id, ..
            } => (state, recipe_id),
            Identified::Refuse { doc, reason, .. } => return Some(self.refuse(doc, reason)),
            Identified::Ambiguous { .. } => return None,
        };
        if to == self.state.recipe_id {
            return None;
        }
        let steps = manifest.recipes.get(&to)?;
        let resume = done
            .iter()
            .zip(steps)
            .take_while(|(a, b)| same_step(a, b))
            .count();
        let from = std::mem::replace(&mut self.state.recipe_id, to.clone());
        self.state.recipe_sha256 = Some(recipe_sha256(steps));
        self.state.step_index = resume;
        let title = match state {
            StartState::Helix(_) => "HelixScreen is already installed; switching to update",
            _ => {
                "This printer is set up differently than it first looked; switching how to install"
            }
        };
        self.log(resume, format!("{title} (recipe {from} -> {to})"));
        Some(self.persist(Event::Progress {
            step: resume,
            total: steps.len(),
            title: title.into(),
            detail: None,
            fraction: None,
        }))
    }

    /// Stops a committed run whose printer has not been identified again since the commit or
    /// its last restart, before the step sends it anything.
    fn not_reidentified(&mut self, manifest: &Manifest) -> Event {
        let name = manifest
            .printers
            .iter()
            .find(|p| p.id == self.state.printer_id)
            .map_or(self.state.printer_id.clone(), |p| p.name.clone());
        let reason = format!(
            "the installer cannot confirm this is still the {name} the install started on: \
             nothing it reported since the point of no return or its last restart identifies it, \
             so nothing more is sent to it"
        );
        let doc = self.printer_doc();
        self.fail(reason, Some(doc))
    }

    /// Captures the facts that name the printer, for re-identification after the commit.
    fn record_identity(&mut self, index: usize) {
        let identity: Facts = IDENTITY_KEYS
            .iter()
            .filter_map(|k| self.state.facts.get(*k).map(|v| (k.to_string(), v.clone())))
            .collect();
        if identity.is_empty() {
            self.log(
                index,
                "no model or serial is known at the point of no return; the printer cannot be re-identified".into(),
            );
        }
        self.state.identity = Some(identity);
    }

    async fn run_step(
        &mut self,
        manifest: &Manifest,
        step: &Step,
        input: Option<Input>,
    ) -> StepOutcome {
        if self.calls == 0 {
            self.session.begin_step();
        }
        let handler = self.handlers.get(step.type_name()).cloned();
        let mut ctx = self.step_ctx(manifest);
        match handler {
            Some(h) => h.run(&mut ctx, step, input).await,
            None => steps::run_builtin(&mut ctx, step, input).await,
        }
    }

    fn step_ctx<'a>(&'a mut self, manifest: &'a Manifest) -> StepCtx<'a> {
        StepCtx {
            env: &self.env,
            manifest,
            host: &self.state.host,
            facts: &mut self.state.facts,
            fresh: &mut self.state.fresh,
            job: &mut self.state.job,
            cache_dir: self.cache_dir.as_deref(),
            channel_override: self.channel_override,
            calls: self.calls,
            committed: self.state.committed,
            identity: self.state.identity.as_ref(),
            log: &mut self.state.log,
            step: self.state.step_index,
            printer_id: &self.state.printer_id,
            recipe: manifest
                .recipes
                .get(&self.state.recipe_id)
                .map_or(&[], Vec::as_slice),
            session: &mut self.session,
        }
    }

    /// Removes what the run left on the printer for a later step (see `steps::end_of_run`).
    async fn end_of_run(&mut self, manifest: &Manifest) {
        let mut ctx = self.step_ctx(manifest);
        steps::end_of_run(&mut ctx).await;
    }

    /// `fraction`, raised to the most `step` has reported. A step that has reported a fraction
    /// keeps showing one.
    fn never_backwards(&mut self, step: usize, fraction: Option<f32>) -> Option<f32> {
        let floor = self
            .progress_floor
            .filter(|(s, _)| *s == step)
            .map(|(_, f)| f);
        let shown = match (fraction, floor) {
            (Some(f), Some(m)) => Some(f.max(m)),
            (None, m) => m,
            (f, None) => f,
        };
        if let Some(f) = shown {
            self.progress_floor = Some((step, f));
        }
        shown
    }

    fn printer_doc(&self) -> String {
        self.manifest
            .printers
            .iter()
            .find(|p| p.id == self.state.printer_id)
            .map_or_else(
                || GENERIC_REFUSE_DOC.to_string(),
                |p| p.fallback.doc.clone(),
            )
    }

    fn cancel(&mut self) -> Event {
        let doc = self.printer_doc();
        if self.state.committed {
            self.fail("cancelled after the printer was changed".into(), Some(doc))
        } else {
            self.end_refused(
                doc,
                "cancelled; nothing on the printer was changed".into(),
                true,
            )
        }
    }

    fn refuse(&mut self, doc: String, reason: String) -> Event {
        self.end_refused(doc, reason, false)
    }

    fn end_refused(&mut self, doc: String, reason: String, cancelled: bool) -> Event {
        self.log(self.state.step_index, format!("refused: {reason}"));
        self.state.status = RunStatus::Refused {
            doc: doc.clone(),
            reason: reason.clone(),
            cancelled,
        };
        self.persist(Event::Refused {
            doc,
            reason,
            cancelled,
        })
    }

    fn fail(&mut self, reason: String, help: Option<String>) -> Event {
        let step = self.state.step_index;
        self.log(step, format!("failed: {reason}"));
        self.calls = 0;
        self.state.status = RunStatus::Failed {
            reason: reason.clone(),
            help: help.clone(),
        };
        self.persist(Event::Failed {
            committed: self.state.committed,
            step,
            reason,
            help,
        })
    }

    /// Saves if autosave is on. A save error replaces `event`: the UI must not report progress
    /// that would be lost if the app closed now.
    fn persist(&mut self, event: Event) -> Event {
        let Err(e) = self.autosave() else {
            return event;
        };
        let reason = format!("could not save install progress: {e}");
        if !self.state.status.is_finished() {
            self.state.status = RunStatus::Failed {
                reason: reason.clone(),
                help: None,
            };
        }
        Event::Failed {
            committed: self.state.committed,
            step: self.state.step_index,
            reason,
            help: None,
        }
    }

    fn autosave(&self) -> io::Result<()> {
        match &self.autosave {
            Some(dir) => run_state::save(&self.state, dir),
            None => Ok(()),
        }
    }

    fn log(&mut self, step: usize, text: String) {
        self.state.log.push(LogEntry {
            at_unix: unix_secs(&self.env),
            step,
            text,
        });
    }
}

/// Where re-identifying for the step at `index` starts: just after the last commit or
/// `wait_for` before it, when a probe lies between. `None` when none does.
fn probes_since_boundary(steps: &[Step], index: usize) -> Option<usize> {
    let start = steps[..index]
        .iter()
        .rposition(|s| s.commits() || matches!(s, Step::WaitFor { .. }))
        .map_or(0, |b| b + 1);
    steps[start..index]
        .iter()
        .any(|s| matches!(s, Step::Probe { .. }))
        .then_some(start)
}

const RERUN: &str = "retry";
const STOP: &str = "stop";

fn rerun_prompt() -> Prompt {
    let choice = |value: &str, label: &str| Choice {
        value: value.into(),
        label: label.into(),
        image: None,
    };
    Prompt::Ask {
        question: "This step may already have run on the printer. Run it again?".into(),
        choices: vec![choice(RERUN, "Run it again"), choice(STOP, "Stop")],
    }
}

/// When a committing step passes the point of no return: a step that only asks the user
/// commits on the answer that tells the installer to act; `serial_flash` commits itself just
/// before its first erase or write (`StepCtx::commit_before_acting`); every other step as it
/// starts.
fn commits_on(step: &Step, input: Option<&Input>) -> bool {
    match step {
        Step::SerialFlash { .. } => false,
        Step::Info { .. } | Step::UserAction { .. } => matches!(input, Some(Input::Next)),
        Step::Ask { choices, .. } => {
            matches!(input, Some(Input::Choice(c)) if choices.iter().any(|x| x.value == *c))
        }
        _ => true,
    }
}

pub(crate) fn unix_secs(env: &Env) -> u64 {
    env.clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Two recipe steps that do the same thing: equal type and arguments.
fn same_step(a: &Step, b: &Step) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
