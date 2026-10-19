// SPDX-License-Identifier: GPL-3.0-or-later

//! Install steps, and the context and outcome every step shares with the runner.

pub mod ask;
pub mod download;
pub mod esp32;
pub mod helix_install;
pub mod info;
mod job;
pub mod moonraker;
pub mod probe;
pub mod progress;
pub mod release;
pub mod require;
pub mod serial_flash;
pub mod ssh_login;
pub mod ssh_run;
mod sudo;
pub mod wait_for;
pub mod write_stick;

use crate::facts::Facts;
use crate::identify::{self, GENERIC_REFUSE_DOC};
use crate::manifest::{Channel, Login, Manifest, Step};
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
    /// Nothing is left to do: the run ends done here, skipping its remaining steps, and `note`
    /// says why on the finish screen.
    Complete {
        note: String,
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

/// Logins the user may type before an SSH step gives up.
pub(crate) const USER_LOGIN_TRIES: u32 = 3;

/// What lives only in the runner's memory: never saved, never logged.
#[derive(Default)]
pub struct Session {
    login: Option<Login>,
    ssh: Option<Box<dyn SshSession>>,
    job_cursor: Option<job::JobCursor>,
    /// Every line of the jobs this runner followed to their end, for the support bundle.
    job_log: Vec<String>,
    stick: Option<write_stick::Stage>,
    /// A flash ready or running in the background, and the step it belongs to.
    flash: Option<serial_flash::Task>,
    /// The USB port the user last let the installer open, in this app session only.
    confirmed_port: Option<String>,
    /// The current step asked to pass the point of no return before it changes anything; the
    /// runner records it on disk when the step returns.
    pub(crate) commit_requested: bool,
    /// Logins the user typed that the printer refused since the last success.
    login_tries: u32,
    /// The recipe's default logins were already tried during this step run.
    defaults_tried: bool,
    /// sudo accepted the sudo password the user typed for the held login.
    sudo_checked: bool,
    /// Typed sudo passwords sudo refused since the last one it took.
    sudo_tries: u32,
    /// Typed sudo passwords sudo refused: no longer held, still stripped from a support bundle.
    refused_secrets: Vec<String>,
}

impl Session {
    /// Called when a step becomes current or is retried.
    pub(crate) fn begin_step(&mut self) {
        self.defaults_tried = false;
    }

    /// What the printer-side jobs printed: the ended jobs' lines, then the current job's.
    pub(crate) fn job_logs(&self) -> Vec<String> {
        let current = self.job_cursor.iter().flat_map(|c| c.lines());
        self.job_log.iter().chain(current).cloned().collect()
    }

    /// Stops following the current job, keeping its lines for `job_logs`.
    pub(crate) fn retire_job_cursor(&mut self) {
        if let Some(c) = self.job_cursor.take() {
            self.job_log.extend(c.into_lines());
        }
    }

    /// Lets go of a flash that has not started writing (see `serial_flash::Task::release`); a
    /// running one is kept.
    pub(crate) async fn release_flash(&mut self) {
        match self.flash.take() {
            Some(t) if t.running() => self.flash = Some(t),
            Some(t) => t.release().await,
            None => {}
        }
    }

    /// A flash is erasing or writing the panel in the background.
    pub(crate) fn flash_running(&self) -> bool {
        self.flash.as_ref().is_some_and(serial_flash::Task::running)
    }

    /// The password, passphrase and sudo password of the login held now, and the sudo passwords
    /// sudo refused, for stripping from a support bundle.
    pub(crate) fn secrets(&self) -> Vec<String> {
        self.login
            .iter()
            .flat_map(Login::secrets)
            .chain(self.refused_secrets.iter().cloned())
            .collect()
    }
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
    /// After the commit, the probe results re-identification reads (`RunState::fresh`).
    pub(crate) fresh: &'a mut Facts,
    /// A detached job on the printer, saved with the run so a reopened app can follow it.
    pub job: &'a mut Option<RemoteJob>,
    pub cache_dir: Option<&'a Path>,
    /// Replaces the release channel of every `helix_install` step in the run.
    pub channel_override: Option<Channel>,
    /// Times this step has already run since it became current; 0 on its first run and again
    /// after a failure or a resume.
    pub calls: u32,
    pub committed: bool,
    /// The run's step index, for log entries.
    pub step: usize,
    pub printer_id: &'a str,
    /// The steps of the run's recipe.
    pub recipe: &'a [Step],
    pub(crate) identity: Option<&'a Facts>,
    pub(crate) log: &'a mut Vec<LogEntry>,
    pub(crate) session: &'a mut Session,
}

impl StepCtx<'_> {
    /// Marks the point of no return for a step that decides itself when it starts changing the
    /// printer: the runner records it when the step returns, before the step is run again.
    pub(crate) fn commit_before_acting(&mut self) {
        self.session.commit_requested = true;
    }

    pub fn login(&self) -> Option<&Login> {
        self.session.login.as_ref()
    }

    /// Holds `login` for later SSH steps (and sudo) in memory only. Closes any open session,
    /// which belonged to the previous login.
    pub fn set_login(&mut self, login: Login) {
        self.session.login = Some(login);
        self.session.ssh = None;
        self.session.sudo_checked = false;
    }

    /// Connects with `login` and, only if the printer accepts it, holds it and the session.
    pub(crate) async fn try_login(&mut self, login: Login) -> Result<(), TransportError> {
        let ssh = self.env.ssh.connect(self.host, &login).await?;
        self.session.login = Some(login);
        self.session.ssh = Some(ssh);
        self.session.sudo_checked = false;
        Ok(())
    }

    /// Holds a login for an SSH step: the one the user just gave (checked against the printer,
    /// three tries in all), else the recipe's own `ssh_login` defaults tried quietly (a reopened
    /// run has lost the login it had in memory), else a prompt. A typed sudo password is held
    /// with the login. `None` means a login is held and the step may go on.
    pub(crate) async fn need_login(&mut self, input: Option<Input>) -> Option<StepOutcome> {
        let input = match input {
            Some(Input::SudoPassword(pw)) => {
                if let Some(l) = self.session.login.as_mut() {
                    l.sudo_password = Some(pw);
                    self.session.sudo_checked = false;
                }
                Some(Input::Next)
            }
            other => other,
        };
        match input {
            Some(Input::Login(login)) => {
                self.session.login_tries += 1;
                match self.try_login(login).await {
                    Ok(()) => {
                        self.session.login_tries = 0;
                        return None;
                    }
                    Err(TransportError::Auth) => return Some(self.refused_login()),
                    Err(e) => return Some(self.login_error(e)),
                }
            }
            None => self.session.login_tries = 0,
            Some(_) => {}
        }
        if self.login().is_some() {
            return None;
        }
        if let Err(e) = self.try_default_logins().await {
            return Some(self.login_error(e));
        }
        match self.login() {
            Some(_) => None,
            None => Some(StepOutcome::NeedInput(self.login_prompt(None))),
        }
    }

    /// The printer refused a typed login: ask again, or give up after `USER_LOGIN_TRIES`.
    pub(crate) fn refused_login(&mut self) -> StepOutcome {
        if self.session.login_tries < USER_LOGIN_TRIES {
            return StepOutcome::NeedInput(self.login_prompt(None));
        }
        self.session.login_tries = 0;
        StepOutcome::Failed {
            reason: "the printer refused that login three times".into(),
            help: self.login_help(),
        }
    }

    pub(crate) fn login_error(&mut self, e: TransportError) -> StepOutcome {
        self.session.login_tries = 0;
        StepOutcome::failed(format!("could not log in over SSH: {e}"))
    }

    /// The printer's install guide, where the recipe tells the user how to find its password.
    pub(crate) fn login_help(&self) -> Option<String> {
        let printer = self
            .manifest
            .printers
            .iter()
            .find(|p| p.id == self.printer_id)?;
        Some(format!(
            "Look up this printer's SSH password in its install guide: {}",
            printer.fallback.doc
        ))
    }

    /// Holds the first of the recipe's `ssh_login` defaults the printer accepts. Refused
    /// defaults are not an error; an unreachable host is. Tried once per step run, so a stale
    /// default is not retried against the printer on every poll.
    pub(crate) async fn try_default_logins(&mut self) -> Result<(), TransportError> {
        if std::mem::replace(&mut self.session.defaults_tried, true) {
            return Ok(());
        }
        for login in self.default_logins() {
            match self.try_login(login).await {
                Ok(()) => return Ok(()),
                Err(TransportError::Auth) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The logins of the recipe's first `ssh_login` step.
    pub(crate) fn default_logins(&self) -> Vec<Login> {
        self.recipe
            .iter()
            .find_map(|s| match s {
                Step::SshLogin { logins, .. } => Some(logins.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Asks for a login, suggesting the user that last worked, else `fallback_user`, with the
    /// password hint of the recipe's first `ssh_login` step.
    pub(crate) fn login_prompt(&self, fallback_user: Option<&str>) -> Prompt {
        let user_hint = self
            .facts
            .get("ssh_user")
            .and_then(|v| v.as_str())
            .or(fallback_user)
            .map(str::to_string);
        let password_hint = self.recipe.iter().find_map(|s| match s {
            Step::SshLogin { password_hint, .. } => password_hint.clone(),
            _ => None,
        });
        let keys = self.env.ssh.key_status();
        Prompt::Login {
            user_hint,
            password_hint,
            has_ssh_key: keys.available,
            key_locked: keys.locked,
        }
    }

    /// The open SSH session to the run's host, connecting with the held login if none is open.
    pub async fn ssh(&mut self) -> Result<&mut (dyn SshSession + 'static), TransportError> {
        if self.session.login.is_none() {
            self.try_default_logins().await?;
        }
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

    /// The run's printer as the manifest names it.
    pub(crate) fn printer_name(&self) -> &str {
        self.manifest
            .printers
            .iter()
            .find(|p| p.id == self.printer_id)
            .map_or(self.printer_id, |p| p.name.as_str())
    }

    /// The run's printer's install guide, else the generic one.
    pub(crate) fn printer_doc(&self) -> String {
        self.manifest
            .printers
            .iter()
            .find(|p| p.id == self.printer_id)
            .map_or_else(
                || GENERIC_REFUSE_DOC.to_string(),
                |p| p.fallback.doc.clone(),
            )
    }

    /// Merges probe results into the facts. After the point of no return they are also what
    /// re-identifies the printer, and the run stops instead when they rule out the run's printer
    /// by its identify rules (even before they are enough to confirm it), or a model or serial
    /// differs from the one recorded at the commit.
    pub fn record_probe(&mut self, found: Facts) -> Result<(), StepOutcome> {
        if self.committed {
            let mut fresh = self.fresh.clone();
            fresh.extend(found.clone());
            if !identify::could_answer_as(self.manifest, self.printer_id, &fresh) {
                return Err(StepOutcome::failed(format!(
                    "this is not the printer the install started on: it no longer answers as a {}",
                    self.printer_name()
                )));
            }
        }
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
        if self.committed {
            self.fresh.extend(found.clone());
        }
        self.facts.extend(found);
        Ok(())
    }
}

/// Removes what a run kept on the printer for a later step once the run is done: a dry run's
/// uploads the install did not consume, and the logs of jobs that failed along the way. Runs
/// that never kept anything do not connect.
pub(crate) async fn end_of_run(ctx: &mut StepCtx<'_>) {
    helix_install::remove_uploaded(ctx).await;
    job::remove_kept_logs(ctx).await;
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
            Some(crate::manifest::Probe::Serial { serial }) => {
                probe::serial(ctx, serial, input).await
            }
            Some(p) => {
                if matches!(p, crate::manifest::Probe::Ssh { .. }) {
                    if let Some(ask) = ctx.need_login(input).await {
                        return ask;
                    }
                }
                probe::run(ctx, &p).await
            }
            None => StepOutcome::failed(
                "probe needs exactly one of moonraker, ssh+fact, tcp_port+fact, serial",
            ),
        },
        Step::Require {
            fact,
            range,
            equals,
            reason,
            doc,
            ..
        } => require::run(
            ctx,
            fact,
            range.as_ref(),
            equals.as_deref(),
            reason.as_deref(),
            doc,
        ),
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
            channel,
            update,
            uninstall,
            dry_run,
            dir,
            ..
        } => {
            let mode = match (*uninstall, *update) {
                (true, _) => helix_install::Mode::Uninstall,
                (false, true) => helix_install::Mode::Update,
                (false, false) => helix_install::Mode::Install,
            };
            let run = helix_install::Run {
                mode,
                dry_run: *dry_run,
            };
            helix_install::run(ctx, platform, *channel, run, dir.as_deref(), input).await
        }
        Step::WriteStick {
            artifacts, rename, ..
        } => write_stick::run(ctx, artifacts, rename, input).await,
        Step::SshLogin {
            logins,
            ask_if_failed,
            ..
        } => ssh_login::run(ctx, logins, *ask_if_failed, input).await,
        Step::SerialFlash {
            asset,
            chip,
            flash_mb,
            keep,
            channel,
            update,
            ..
        } => {
            let spec = serial_flash::Spec {
                asset,
                chip,
                flash_mb: *flash_mb,
                keep,
                channel: *channel,
                update: *update,
            };
            serial_flash::run(ctx, &spec, input).await
        }
    }
}
