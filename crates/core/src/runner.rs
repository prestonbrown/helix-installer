// SPDX-License-Identifier: GPL-3.0-or-later

//! Walks a recipe one step at a time, pausing whenever the user must act, and enforces the
//! point of no return: before it a stop is a refusal (nothing changed), after it every stop is a
//! committed failure the UI must turn into recovery.

use crate::identify::GENERIC_REFUSE_DOC;
use crate::manifest::{Choice, Login, Manifest, Step};
use crate::run_state::{self, LogEntry, RunState, RunStatus, IDENTITY_KEYS};
use crate::steps::{self, Session, StepCtx, StepHandler, StepOutcome};
use crate::transport::{Drive, Env};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
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
    ChooseDrive {
        drives: Vec<Drive>,
        needed_bytes: u64,
    },
    Ambiguous {
        candidates: Vec<String>,
    },
}

/// What the user answered. Deserialize-only: a `Login` carries a password, so an input is never
/// written anywhere.
#[derive(Clone, Deserialize)]
#[serde(tag = "input", content = "value", rename_all = "snake_case")]
pub enum Input {
    Next,
    Choice(String),
    Login(Login),
    Drive(String),
    Cancel,
}

impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Input::Next => f.write_str("Next"),
            Input::Choice(c) => f.debug_tuple("Choice").field(c).finish(),
            Input::Login(l) => write!(f, "Login({:?}, <password>)", l.user),
            Input::Drive(d) => f.debug_tuple("Drive").field(d).finish(),
            Input::Cancel => f.write_str("Cancel"),
        }
    }
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
        }
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
        let manifest = self.manifest.clone();
        let Some(steps) = manifest.recipes.get(&self.state.recipe_id) else {
            let reason = format!("recipe {} is not in the manifest", self.state.recipe_id);
            return self.fail(reason, None);
        };
        if matches!(input, Some(Input::Cancel)) {
            return self.cancel();
        }
        self.state.status = RunStatus::Running;

        let mut input = input;
        loop {
            let index = self.state.step_index;
            let Some(step) = steps.get(index) else {
                self.state.status = RunStatus::Done;
                self.log(index, "recipe complete".into());
                return self.persist(Event::Done);
            };
            let outcome = self.run_step(&manifest, step, input.take()).await;
            self.calls += 1;
            // A prompt alone changes nothing, and a refusal says nothing changed; any other
            // outcome means the committing step has started acting on the printer.
            if step.commits()
                && !self.state.committed
                && !matches!(
                    outcome,
                    StepOutcome::NeedInput(_) | StepOutcome::Refused { .. }
                )
            {
                self.state.committed = true;
                self.log(index, "passed the point of no return".into());
            }
            match outcome {
                StepOutcome::Done => {
                    if step.commits() && self.state.identity.is_none() {
                        self.state.identity = Some(
                            IDENTITY_KEYS
                                .iter()
                                .filter_map(|k| {
                                    self.state.facts.get(*k).map(|v| (k.to_string(), v.clone()))
                                })
                                .collect(),
                        );
                    }
                    self.log(index, format!("{}: done", step.type_name()));
                    self.state.step_index += 1;
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
        self.state.status = RunStatus::Failed {
            reason: reason.clone(),
            help: None,
        };
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

pub(crate) fn unix_secs(env: &Env) -> u64 {
    env.clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
