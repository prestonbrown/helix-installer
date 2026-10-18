// SPDX-License-Identifier: GPL-3.0-or-later

//! How a detached job on the printer gets root for a login that is not root.

use super::{StepCtx, StepOutcome};
use std::time::Duration;

pub(crate) const SUDO_DOC: &str = "https://helixscreen.org/docs/install";
const SUDO_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) enum SudoPlan {
    /// Root, or a login with passwordless sudo: the job runs as the login and calls sudo itself.
    AsLoginUser,
    /// sudo needs a password, which a detached job cannot type: the job runs as root through
    /// `sudo -S`, fed the held login's password on stdin.
    WithPassword(String),
    /// sudo needs a password and the login has none to give it.
    Unavailable { user: String },
}

/// Decides with `sudo -n true` on the printer how a job started for the held login gets root.
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
        Ok(_) if !login.password.is_empty() => Ok(SudoPlan::WithPassword(login.password)),
        Ok(_) => Ok(SudoPlan::Unavailable { user: login.user }),
        Err(e) => {
            ctx.drop_ssh();
            Err(StepOutcome::failed(format!(
                "could not check sudo on the printer: {e}"
            )))
        }
    }
}

/// sudo turned down the password the printer accepted for the login.
pub(crate) fn refused(ctx: &StepCtx<'_>) -> StepOutcome {
    let user = ctx.login().map(|l| l.user.as_str()).unwrap_or_default();
    StepOutcome::Failed {
        reason: format!(
            "sudo on the printer refused the password of this printer's account ({user})"
        ),
        help: Some(format!(
            "Allow {user} to run sudo with its login password, or without one, then retry: {SUDO_DOC}"
        )),
    }
}
