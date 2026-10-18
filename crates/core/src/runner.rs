// SPDX-License-Identifier: GPL-3.0-or-later

//! Walks a recipe one step at a time, pausing whenever the user must act, and enforces the
//! point of no return: before it a stop is a refusal (nothing changed), after it every stop is a
//! committed failure the UI must turn into recovery.

use crate::facts::Facts;
use crate::identify::GENERIC_REFUSE_DOC;
use crate::manifest::{verify_and_parse, Choice, Login, Manifest, Step};
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
    Ask {
        question: String,
        choices: Vec<Choice>,
    },
    Login {
        user_hint: Option<String>,
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
    autosave: Option<PathBuf>,
    /// Times the current step has run since it became current.
    calls: u32,
    /// Why the run's own manifest copy could not be verified on resume; the run cannot go on.
    manifest_error: Option<String>,
}

impl Runner {
    pub fn new(env: Env, manifest: Arc<Manifest>, state: RunState) -> Runner {
        Runner {
            env,
            manifest,
            state,
            handlers: HashMap::new(),
            session: Session::default(),
            cache_dir: None,
            autosave: None,
            calls: 0,
            manifest_error: None,
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

    /// Saves the state to `dir` after every completed step and before every event, so closing
    /// the app at any moment loses at most the step in flight.
    pub fn with_autosave(mut self, dir: PathBuf) -> Runner {
        self.autosave = Some(dir);
        self
    }

    pub fn state(&self) -> &RunState {
        &self.state
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
    pub async fn advance(&mut self, input: Option<Input>) -> Event {
        match &self.state.status {
            RunStatus::Done => return Event::Done,
            RunStatus::Refused { doc, reason } => {
                return Event::Refused {
                    doc: doc.clone(),
                    reason: reason.clone(),
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
            return self.cancel();
        }
        self.state.status = RunStatus::Running;

        let mut input = input;
        loop {
            let index = self.state.step_index;
            let Some(step) = steps.get(index) else {
                if index > steps.len() {
                    let reason = format!("the saved run is at step {index} of {}", steps.len());
                    return self.fail(reason, None);
                }
                self.state.status = RunStatus::Done;
                self.log(index, "recipe complete".into());
                return self.persist(Event::Done);
            };

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

            // Recorded on disk before the step acts, so a crash inside it resumes as committed.
            let mut changed = false;
            if step.commits() && !self.state.committed && commits_on(step, input.as_ref()) {
                self.state.committed = true;
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
            match outcome {
                StepOutcome::Done => {
                    if step.commits() && self.state.identity.is_none() {
                        self.record_identity(index);
                    }
                    self.log(index, format!("{}: done", step.type_name()));
                    self.state.step_index += 1;
                    self.state.started_step = None;
                    self.calls = 0;
                    if let Err(e) = self.autosave() {
                        let reason = format!("could not save install progress: {e}");
                        return self.fail(reason, None);
                    }
                }
                StepOutcome::NeedInput(prompt) => return self.persist(Event::NeedInput(prompt)),
                StepOutcome::Progress {
                    title,
                    detail,
                    fraction,
                } => {
                    return self.persist(Event::Progress {
                        step: index,
                        total: steps.len(),
                        title,
                        detail,
                        fraction,
                    })
                }
                StepOutcome::Refused { doc, reason } if !self.state.committed => {
                    return self.refuse(doc, reason)
                }
                StepOutcome::Refused { doc, reason } => return self.fail(reason, Some(doc)),
                StepOutcome::Failed { reason, help } => return self.fail(reason, help),
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
        let handler = self.handlers.get(step.type_name()).cloned();
        let mut ctx = StepCtx {
            env: &self.env,
            manifest,
            host: &self.state.host,
            facts: &mut self.state.facts,
            job: &mut self.state.job,
            cache_dir: self.cache_dir.as_deref(),
            calls: self.calls,
            committed: self.state.committed,
            identity: self.state.identity.as_ref(),
            log: &mut self.state.log,
            step: self.state.step_index,
            session: &mut self.session,
        };
        match handler {
            Some(h) => h.run(&mut ctx, step, input).await,
            None => steps::run_builtin(&mut ctx, step, input).await,
        }
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
            self.refuse(doc, "cancelled; nothing on the printer was changed".into())
        }
    }

    fn refuse(&mut self, doc: String, reason: String) -> Event {
        self.log(self.state.step_index, format!("refused: {reason}"));
        self.state.status = RunStatus::Refused {
            doc: doc.clone(),
            reason: reason.clone(),
        };
        self.persist(Event::Refused { doc, reason })
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
/// commits on the answer that tells the installer to act; every other step as it starts.
fn commits_on(step: &Step, input: Option<&Input>) -> bool {
    match step {
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
