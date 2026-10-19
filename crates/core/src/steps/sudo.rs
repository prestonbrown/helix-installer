// SPDX-License-Identifier: GPL-3.0-or-later

//! How a detached job on the printer gets root for a login that is not root.

use super::{StepCtx, StepOutcome, USER_LOGIN_TRIES};
use crate::runner::Prompt;
use std::time::Duration;

pub(crate) const SUDO_DOC: &str = "https://helixscreen.org/docs/install";
const SUDO_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) enum SudoPlan {
    /// Root, or a login with passwordless sudo: the job runs as the login and calls sudo itself.
    AsLoginUser,
    /// sudo needs a password, which a detached job cannot type: the job runs as root through
    /// `sudo -S`, fed the password on stdin. That is the one the user typed for sudo, else the
    /// held login's own.
    WithPassword(String),
}

/// Decides with `sudo -n true` on the printer how a job started for the held login gets root.
/// A login with no password to give sudo (an SSH key login) asks the user for it; a typed one
/// is checked with sudo before anything is uploaded, and asked for again if sudo refuses it.
pub(crate) async fn plan(ctx: &mut StepCtx<'_>) -> Result<SudoPlan, StepOutcome> {
    let Some(login) = ctx.login().cloned() else {
        return Err(StepOutcome::failed("no SSH login is held"));
    };
    if login.user == "root" {
        return Ok(SudoPlan::AsLoginUser);
    }
    let ran = match ctx.ssh().await {
        Ok(s) => s.exec("sudo -n true", None, SUDO_CHECK_TIMEOUT).await,
        Err(e) => Err(e),
    };
    match ran {
        Ok(out) if out.exit == 0 => Ok(SudoPlan::AsLoginUser),
        Ok(_) => match login.sudo_password {
            Some(typed) => check_typed(ctx, &login.user, typed.expose()).await,
            None if !login.password.is_empty() => Ok(SudoPlan::WithPassword(login.password)),
            None => Err(ask(&login.user, false)),
        },
        Err(e) => {
            ctx.drop_ssh();
            Err(StepOutcome::failed(format!(
                "could not check sudo on the printer: {e}"
            )))
        }
    }
}

fn ask(user: &str, refused: bool) -> StepOutcome {
    StepOutcome::NeedInput(Prompt::SudoPassword {
        user: user.to_string(),
        refused,
    })
}

/// Feeds the typed password to `sudo -S true` once per typed password. A password sudo turns
/// down is forgotten and asked for again, `USER_LOGIN_TRIES` times in all. Any other failure
/// (a sudo that wants a terminal) leaves the verdict to the job's own launch.
async fn check_typed(
    ctx: &mut StepCtx<'_>,
    user: &str,
    typed: &str,
) -> Result<SudoPlan, StepOutcome> {
    if ctx.session.sudo_checked {
        return Ok(SudoPlan::WithPassword(typed.to_string()));
    }
    let stdin = format!("{typed}\n");
    let ran = match ctx.ssh().await {
        Ok(s) => {
            s.exec(
                "sudo -S -p '' true",
                Some(stdin.as_bytes()),
                SUDO_CHECK_TIMEOUT,
            )
            .await
        }
        Err(e) => Err(e),
    };
    match ran {
        Ok(out) if out.exit != 0 && rejected(&format!("{}{}", out.stdout, out.stderr)) => {
            forget_typed(ctx);
            ctx.session.sudo_tries += 1;
            if ctx.session.sudo_tries >= USER_LOGIN_TRIES {
                ctx.session.sudo_tries = 0;
                return Err(refused(ctx));
            }
            Err(ask(user, true))
        }
        Ok(_) => {
            ctx.session.sudo_checked = true;
            ctx.session.sudo_tries = 0;
            Ok(SudoPlan::WithPassword(typed.to_string()))
        }
        Err(e) => {
            ctx.drop_ssh();
            Err(StepOutcome::failed(format!(
                "could not check sudo on the printer: {e}"
            )))
        }
    }
}

fn rejected(said: &str) -> bool {
    let t = said.to_ascii_lowercase();
    t.contains("incorrect password") || t.contains("sorry, try again")
}

fn forget_typed(ctx: &mut StepCtx<'_>) {
    if let Some(l) = ctx.session.login.as_mut() {
        l.sudo_password = None;
    }
    ctx.session.sudo_checked = false;
}

/// sudo turned down the password the job was started with. A typed one is forgotten, so the
/// retry asks for it again.
pub(crate) fn refused(ctx: &mut StepCtx<'_>) -> StepOutcome {
    let typed = ctx
        .login()
        .is_some_and(|l| l.sudo_password.is_some() || l.password.is_empty());
    forget_typed(ctx);
    let user = ctx.login().map(|l| l.user.clone()).unwrap_or_default();
    if typed {
        return StepOutcome::Failed {
            reason: format!("sudo on the printer refused the password typed for {user}"),
            help: Some(format!(
                "Check {user}'s password on the printer, then retry: {SUDO_DOC}"
            )),
        };
    }
    StepOutcome::Failed {
        reason: format!(
            "sudo on the printer refused the password of this printer's account ({user})"
        ),
        help: Some(format!(
            "Allow {user} to run sudo with its login password, or without one, then retry: {SUDO_DOC}"
        )),
    }
}
