// SPDX-License-Identifier: GPL-3.0-or-later

//! `wait_for`: poll until a service on the printer answers, or time out with the step's help.

use super::{probe, StepCtx, StepOutcome};
use crate::manifest::{Login, WaitTarget};
use crate::transport::TransportError;
use std::time::Duration;

const POLL: Duration = Duration::from_secs(2);

/// The first run only reports the step's message, so the UI shows it (and "do not power off")
/// for the whole wait; the next run polls until the target answers or the timeout passes.
pub async fn run(
    ctx: &mut StepCtx<'_>,
    target: WaitTarget,
    timeout_s: u64,
    message: &str,
    help: &str,
) -> StepOutcome {
    if ctx.calls == 0 {
        return StepOutcome::Progress {
            title: message.to_string(),
            detail: None,
            fraction: None,
        };
    }
    let timeout = Duration::from_secs(timeout_s);
    let start = ctx.env.clock.now();
    loop {
        match is_up(ctx, target).await {
            Ok(true) => return StepOutcome::Done,
            Ok(false) => {}
            Err(reason) => return StepOutcome::failed(reason),
        }
        let waited = ctx
            .env
            .clock
            .now()
            .duration_since(start)
            .unwrap_or_default();
        if waited >= timeout {
            return StepOutcome::Failed {
                reason: format!("{} did not answer within {timeout_s} seconds", name(target)),
                help: (!help.is_empty()).then(|| help.to_string()),
            };
        }
        ctx.env.clock.sleep(POLL.min(timeout - waited)).await;
    }
}

fn name(target: WaitTarget) -> String {
    match target {
        WaitTarget::Port(p) => format!("port {p}"),
        WaitTarget::Moonraker => "Moonraker".into(),
        WaitTarget::Ssh => "SSH".into(),
        WaitTarget::HelixService => "HelixScreen".into(),
    }
}

/// Login used only to see whether sshd answers when the run holds none: a rejected login
/// proves the server is up as well as an accepted one.
fn probe_login() -> Login {
    Login {
        user: "helix-installer-probe".into(),
        password: String::new(),
    }
}

async fn is_up(ctx: &mut StepCtx<'_>, target: WaitTarget) -> Result<bool, String> {
    let (env, host) = (ctx.env, ctx.host);
    match target {
        WaitTarget::Moonraker => Ok(env.moonraker.printer_info(host).await.is_ok()),
        WaitTarget::HelixService => Ok(env
            .moonraker
            .system_info(host)
            .await
            .is_ok_and(|v| probe::helix_service_active(&v))),
        WaitTarget::Ssh => {
            let login = ctx.login().cloned().unwrap_or_else(probe_login);
            Ok(matches!(
                env.ssh.connect(host, &login).await,
                Ok(_) | Err(TransportError::Auth)
            ))
        }
        WaitTarget::Port(p) => Err(format!("waiting on port {p} is not supported")),
    }
}
