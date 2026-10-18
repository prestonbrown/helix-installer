// SPDX-License-Identifier: GPL-3.0-or-later

//! `ssh_login`: finds a login the printer accepts and holds it in the runner's memory.

use super::{StepCtx, StepOutcome};
use crate::manifest::Login;
use crate::runner::Input;
use crate::transport::TransportError;
use std::time::Duration;

const ID_TIMEOUT: Duration = Duration::from_secs(15);

/// Tries each of the recipe's `logins` once, in order. When all are refused the user is asked
/// (if the recipe allows) and gets `USER_LOGIN_TRIES` attempts. Records `ssh_user` and `needs_sudo`.
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
            Err(TransportError::Auth) => ctx.refused_login(),
            Err(e) => ctx.login_error(e),
        };
    }
    if input.is_none() {
        ctx.session.login_tries = 0;
    }
    for login in logins {
        match ctx.try_login(login.clone()).await {
            Ok(()) => return finish(ctx).await,
            Err(TransportError::Auth) => {}
            Err(e) => return ctx.login_error(e),
        }
    }
    if ask_if_failed {
        let first = logins.first().map(|l| l.user.as_str());
        return StepOutcome::NeedInput(ctx.login_prompt(first));
    }
    StepOutcome::Failed {
        reason: "the printer refused every known login".into(),
        help: ctx.login_help(),
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
        Ok(out) if out.exit == 0 => match out.stdout.trim().parse::<u32>() {
            Ok(uid) => {
                ctx.facts.insert("ssh_user".into(), user.into());
                ctx.facts.insert("needs_sudo".into(), (uid != 0).into());
                StepOutcome::Done
            }
            Err(_) => StepOutcome::failed(format!(
                "could not tell which user the login is: id -u printed {:?}",
                out.stdout.trim()
            )),
        },
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
