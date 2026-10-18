// SPDX-License-Identifier: GPL-3.0-or-later

//! `ssh_login`: finds a login the printer accepts and holds it in the runner's memory.

use super::{StepCtx, StepOutcome};
use crate::manifest::Login;
use crate::runner::Input;
use crate::transport::TransportError;
use std::time::Duration;

/// Logins the user may type before the step gives up.
const USER_TRIES: u32 = 3;
const ID_TIMEOUT: Duration = Duration::from_secs(15);

/// Tries each of the recipe's `logins` once, in order. When all are refused the user is asked
/// (if the recipe allows) and gets `USER_TRIES` attempts. Records `ssh_user` and `needs_sudo`.
pub async fn run(
    ctx: &mut StepCtx<'_>,
    logins: &[Login],
    ask_if_failed: bool,
    input: Option<Input>,
) -> StepOutcome {
    if let Some(Input::Login(login)) = input {
        ctx.session.login_tries += 1;
        return match ctx.try_login(login).await {
            Ok(()) => finish(ctx).await,
            Err(TransportError::Auth) if ctx.session.login_tries < USER_TRIES => {
                StepOutcome::NeedInput(ctx.login_prompt(None))
            }
            Err(TransportError::Auth) => {
                ctx.session.login_tries = 0;
                StepOutcome::Failed {
                    reason: "the printer refused that login three times".into(),
                    help: help(ctx),
                }
            }
            Err(e) => unreachable_failure(ctx, e),
        };
    }
    for login in logins {
        match ctx.try_login(login.clone()).await {
            Ok(()) => return finish(ctx).await,
            Err(TransportError::Auth) => {}
            Err(e) => return unreachable_failure(ctx, e),
        }
    }
    if ask_if_failed {
        let first = logins.first().map(|l| l.user.as_str());
        return StepOutcome::NeedInput(ctx.login_prompt(first));
    }
    StepOutcome::Failed {
        reason: "the printer refused every known login".into(),
        help: help(ctx),
    }
}

/// Records who the held login is and whether its commands need sudo.
async fn finish(ctx: &mut StepCtx<'_>) -> StepOutcome {
    ctx.session.login_tries = 0;
    let Some(user) = ctx.login().map(|l| l.user.clone()) else {
        return StepOutcome::failed("no SSH login is held");
    };
    let uid = match ctx.ssh().await {
        Ok(s) => s.exec("id -u", None, ID_TIMEOUT).await,
        Err(e) => Err(e),
    };
    match uid {
        Ok(out) if out.exit == 0 => {
            ctx.facts.insert("ssh_user".into(), user.into());
            ctx.facts
                .insert("needs_sudo".into(), (out.stdout.trim() != "0").into());
            StepOutcome::Done
        }
        Ok(out) => StepOutcome::failed(format!(
            "could not tell which user the login is: id -u exited with {}",
            out.exit
        )),
        Err(e) => {
            ctx.drop_ssh();
            StepOutcome::failed(format!("could not tell which user the login is: {e}"))
        }
    }
}

fn unreachable_failure(ctx: &mut StepCtx<'_>, e: TransportError) -> StepOutcome {
    ctx.session.login_tries = 0;
    StepOutcome::failed(format!("could not log in over SSH: {e}"))
}

/// The printer's install guide, where the recipe tells the user how to find its password.
fn help(ctx: &StepCtx<'_>) -> Option<String> {
    let printer = ctx
        .manifest
        .printers
        .iter()
        .find(|p| p.id == ctx.printer_id)?;
    Some(format!(
        "Look up this printer's SSH password in its install guide: {}",
        printer.fallback.doc
    ))
}
