// SPDX-License-Identifier: GPL-3.0-or-later

//! Install steps, and the context and outcome every step shares with the runner.

pub mod ask;
pub mod download;
pub mod helix_install;
pub mod info;
mod job;
pub mod moonraker;
pub mod probe;
pub mod progress;
pub mod require;
pub mod ssh_run;
pub mod wait_for;

use crate::facts::Facts;
use crate::manifest::{Login, Manifest, Step};
use crate::run_state::{LogEntry, IDENTITY_KEYS};
use crate::runner::{unix_secs, Input, Prompt};
use crate::transport::{Env, Host, RemoteJob, SshSession, TransportError};
use async_trait::async_trait;
use std::path::Path;

#[derive(Debug, Clone)]
pub enum StepOutcome {
    Done,
    NeedInput(Prompt),
    /// Not finished: the runner reports this and runs the step again on the next advance.
    Progress {
        title: String,
        detail: Option<String>,
        fraction: Option<f32>,
    },
    /// Nothing was changed and this printer cannot go further. After the point of no return
    /// the runner reports it as a committed failure with `doc` as the help.
    Refused {
        doc: String,
        reason: String,
    },
    Failed {
        reason: String,
        help: Option<String>,
    },
}

impl StepOutcome {
    pub fn failed(reason: impl Into<String>) -> StepOutcome {
        StepOutcome::Failed {
            reason: reason.into(),
            help: None,
        }
    }
}

/// Runs a step type the runner does not implement itself; registered with
/// `Runner::with_handler` under the step's manifest `type` tag.
#[async_trait]
pub trait StepHandler: Send + Sync {
    async fn run(&self, ctx: &mut StepCtx<'_>, step: &Step, input: Option<Input>) -> StepOutcome;
}

/// What lives only in the runner's memory: never saved, never logged.
#[derive(Default)]
pub struct Session {
    login: Option<Login>,
    ssh: Option<Box<dyn SshSession>>,
    job_cursor: Option<job::JobCursor>,
}

/// Everything a step may read or change while it runs.
pub struct StepCtx<'a> {
    pub env: &'a Env,
    pub manifest: &'a Manifest,
    /// The host the run is pinned to.
    pub host: &'a Host,
    /// Probe output must go through `record_probe`, which enforces re-identification; write
    /// here directly only for facts the user supplied or the step itself decided.
    pub facts: &'a mut Facts,
    /// A detached job on the printer, saved with the run so a reopened app can follow it.
    pub job: &'a mut Option<RemoteJob>,
    pub cache_dir: Option<&'a Path>,
    /// Times this step has already run since it became current; 0 on its first run and again
    /// after a failure or a resume.
    pub calls: u32,
    pub committed: bool,
    /// The run's step index, for log entries.
    pub step: usize,
    pub(crate) identity: Option<&'a Facts>,
    pub(crate) log: &'a mut Vec<LogEntry>,
    pub(crate) session: &'a mut Session,
}

impl StepCtx<'_> {
    pub fn login(&self) -> Option<&Login> {
        self.session.login.as_ref()
    }

    /// Holds `login` for later SSH steps (and sudo) in memory only. Closes any open session,
    /// which belonged to the previous login.
    pub fn set_login(&mut self, login: Login) {
        self.session.login = Some(login);
        self.session.ssh = None;
    }

    /// The open SSH session to the run's host, connecting with the held login if none is open.
    pub async fn ssh(&mut self) -> Result<&mut (dyn SshSession + 'static), TransportError> {
        if self.session.ssh.is_none() {
            let login = self
                .session
                .login
                .as_ref()
                .ok_or_else(|| TransportError::Protocol("no SSH login yet".into()))?;
            self.session.ssh = Some(self.env.ssh.connect(self.host, login).await?);
        }
        self.session
            .ssh
            .as_deref_mut()
            .ok_or_else(|| TransportError::Protocol("SSH session closed".into()))
    }

    /// Forgets a session that failed, so the next `ssh()` reconnects.
    pub fn drop_ssh(&mut self) {
        self.session.ssh = None;
    }

    /// Appends to the run log. Never pass a secret.
    pub fn log(&mut self, text: impl Into<String>) {
        let at_unix = unix_secs(self.env);
        self.log.push(LogEntry {
            at_unix,
            step: self.step,
            text: text.into(),
        });
    }

    /// Merges probe results into the facts. After the point of no return, a model or serial
    /// that differs from the one recorded at the commit stops the run instead.
    pub fn record_probe(&mut self, found: Facts) -> Result<(), StepOutcome> {
        if let Some(identity) = self.identity {
            for key in IDENTITY_KEYS {
                if let (Some(was), Some(now)) = (identity.get(key), found.get(key)) {
                    if was != now {
                        return Err(StepOutcome::failed(format!(
                            "this is not the printer the install started on: its {key} was {was}, now {now}"
                        )));
                    }
                }
            }
        }
        self.facts.extend(found);
        Ok(())
    }
}

/// Runs a step the runner implements itself. Step types that need a handler fail until one is
/// registered.
pub(crate) async fn run_builtin(
    ctx: &mut StepCtx<'_>,
    step: &Step,
    input: Option<Input>,
) -> StepOutcome {
    match step {
        Step::Info { text, image, .. } => info::info(text, image, input),
        Step::UserAction {
            text,
            image,
            verify,
            ..
        } => info::user_action(ctx, text, image, verify.as_ref(), input).await,
        Step::Ask {
            fact,
            question,
            choices,
            ..
        } => ask::run(ctx, fact, question, choices, input),
        Step::Probe { .. } => match step.probe() {
            Some(p) => probe::run(ctx, &p).await,
            None => {
                StepOutcome::failed("probe needs exactly one of moonraker, ssh+fact, tcp_port+fact")
            }
        },
        Step::Require {
            fact, range, doc, ..
        } => require::run(ctx, fact, range, doc),
        Step::Download { artifact, .. } => download::run(ctx, artifact).await,
        Step::WaitFor {
            target,
            timeout_s,
            message,
            help,
            ..
        } => wait_for::run(ctx, *target, *timeout_s, message, help, input).await,
        Step::MoonrakerGcode { script, .. } => moonraker::gcode(ctx, script).await,
        Step::MoonrakerUpdate { name, .. } => moonraker::update(ctx, name).await,
        Step::SshRun {
            command, timeout_s, ..
        } => ssh_run::run(ctx, command, *timeout_s, input).await,
        Step::HelixInstall {
            platform,
            update,
            dir,
            ..
        } => helix_install::run(ctx, platform, *update, dir.as_deref(), input).await,
        Step::WriteStick { .. } | Step::SshLogin { .. } => {
            StepOutcome::failed(format!("unsupported step {}", step.type_name()))
        }
    }
}
