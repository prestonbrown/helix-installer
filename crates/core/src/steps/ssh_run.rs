// SPDX-License-Identifier: GPL-3.0-or-later

//! `ssh_run`: runs the recipe's command detached on the printer and follows it to its exit.

use super::job::{self, Followed};
use super::sudo::{self, SudoPlan};
use super::{StepCtx, StepOutcome};
use crate::runner::Input;
use crate::transport::TransportError;
use std::time::Duration;

/// `command` comes from the signed manifest only. `timeout_s` bounds how long the command may
/// print nothing, not how long it may run.
pub async fn run(
    ctx: &mut StepCtx<'_>,
    command: &str,
    timeout_s: u64,
    input: Option<Input>,
) -> StepOutcome {
    if let Some(ask) = ctx.need_login(input).await {
        return ask;
    }
    let Some(running) = ctx.job.clone() else {
        if ctx.calls == 0 {
            return StepOutcome::Progress {
                title: "Running a command on the printer".into(),
                detail: None,
                fraction: None,
            };
        }
        // A login without a password to give sudo runs the command as itself, as a root
        // login would; the command then fails on its own if it needed root.
        let sudo_password = match sudo::plan(ctx).await {
            Ok(SudoPlan::WithPassword(pw)) => Some(pw),
            Ok(SudoPlan::AsLoginUser | SudoPlan::Unavailable { .. }) => None,
            Err(stop) => return stop,
        };
        let started = match ctx.ssh().await {
            Ok(s) => s.spawn_detached(command, sudo_password.as_deref()).await,
            Err(e) => Err(e),
        };
        return match started {
            Ok(j) => {
                *ctx.job = Some(j);
                StepOutcome::Progress {
                    title: "Running a command on the printer".into(),
                    detail: None,
                    fraction: None,
                }
            }
            Err(TransportError::Auth) => {
                ctx.drop_ssh();
                sudo::refused(ctx)
            }
            Err(e) => {
                ctx.drop_ssh();
                StepOutcome::failed(format!("the command did not start on the printer: {e}"))
            }
        };
    };
    let silence = Duration::from_secs(timeout_s);
    let followed = match job::follow(ctx, &running, silence, |_| false).await {
        Ok(f) => f,
        Err(e) => return job::lost(ctx, "the command", e),
    };
    let tail = job::tail(ctx, |_| true);
    match followed {
        Followed::Exited(0) => {
            job::forget(ctx);
            StepOutcome::Done
        }
        Followed::Exited(code) => {
            job::forget(ctx);
            StepOutcome::failed(format!("the command failed (exit {code}){tail}"))
        }
        Followed::Died => {
            job::forget(ctx);
            StepOutcome::failed(format!("the command stopped without finishing{tail}"))
        }
        Followed::Silent => StepOutcome::failed(format!(
            "the command printed nothing for {timeout_s} seconds{tail}"
        )),
        Followed::Event => StepOutcome::Progress {
            title: "Running a command on the printer".into(),
            detail: None,
            fraction: None,
        },
    }
}
