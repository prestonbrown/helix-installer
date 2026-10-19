// SPDX-License-Identifier: GPL-3.0-or-later

//! `wait_for`: poll until a service on the printer answers, or time out with the step's help.

use super::{probe, StepCtx, StepOutcome};
use crate::manifest::{Choice, WaitTarget};
use crate::runner::{Input, Prompt};
use crate::transport::{Env, Host};
use std::time::Duration;

const POLL: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const PIDOF_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a `klipper_restart` wait gives Moonraker to stop answering before it decides the
/// printer did not restart.
pub const RESTART_START_WINDOW: Duration = Duration::from_secs(120);

enum Poll {
    Up,
    NotYet,
    /// Nothing answered: the printer may still be rebooting.
    Silent,
    /// No source available can say whether the target is up; only the user can.
    CannotTell,
}

/// The first run only reports the step's message, so the UI shows it (and "do not power off")
/// for the whole wait; the next run polls until the target answers or the timeout passes.
pub async fn run(
    ctx: &mut StepCtx<'_>,
    target: WaitTarget,
    timeout_s: u64,
    message: &str,
    help: &str,
    input: Option<Input>,
) -> StepOutcome {
    let help = (!help.is_empty()).then(|| help.to_string());
    if let (WaitTarget::HelixService, Some(Input::Choice(answer))) = (target, &input) {
        return match answer.as_str() {
            "yes" => StepOutcome::Done,
            "no" => StepOutcome::Failed {
                reason: "HelixScreen is not showing on the printer's screen".into(),
                help,
            },
            _ => ask_if_showing(),
        };
    }
    if ctx.calls == 0 {
        if target == WaitTarget::KlipperRestart {
            note_restart_wait(ctx);
        }
        return StepOutcome::Progress {
            title: message.to_string(),
            detail: None,
            fraction: None,
        };
    }
    let timeout = Duration::from_secs(timeout_s);
    let start = ctx.env.clock.now();
    if target == WaitTarget::KlipperRestart {
        let since = restart_wait_since(ctx);
        let window = RESTART_START_WINDOW.min(timeout);
        // A printer the user restarted by hand before a retry is already back: its boot time
        // says so without another window.
        let restarted = booted_since(ctx.env, ctx.host, since).await
            || wait_until_gone(ctx.env, ctx.host, window).await
            || booted_since(ctx.env, ctx.host, since).await;
        if !restarted {
            return StepOutcome::Failed {
                reason: format!(
                    "the printer did not restart: Moonraker kept answering for {} seconds",
                    window.as_secs()
                ),
                help: Some(RESTART_BY_HAND.into()),
            };
        }
    }
    loop {
        let silent = match poll(ctx, target).await {
            Poll::Up => {
                ctx.facts.remove(RESTART_SINCE_FACT);
                return StepOutcome::Done;
            }
            Poll::NotYet => false,
            Poll::Silent => true,
            Poll::CannotTell => return ask_if_showing(),
        };
        let waited = ctx
            .env
            .clock
            .now()
            .duration_since(start)
            .unwrap_or_default();
        if waited >= timeout {
            // With nothing on the printer able to answer, only the user can say.
            if silent {
                return ask_if_showing();
            }
            return StepOutcome::Failed {
                reason: format!("{} did not answer within {timeout_s} seconds", name(target)),
                help,
            };
        }
        ctx.env.clock.sleep(POLL.min(timeout - waited)).await;
    }
}

/// Fact holding the step whose restart is awaited and when the wait began (unix seconds), kept
/// across retries and a reopened app: a boot after it is a restart.
const RESTART_SINCE_FACT: &str = "restart_wait_since";
const RESTART_BY_HAND: &str = "Restart the printer, then press Try this step again.";

/// Records when the wait for this step's restart began, unless an earlier attempt already did.
fn note_restart_wait(ctx: &mut StepCtx<'_>) {
    let step = ctx.step;
    let recorded = ctx
        .facts
        .get(RESTART_SINCE_FACT)
        .and_then(|v| v.get("step"))
        .and_then(|v| v.as_u64());
    if recorded == Some(step as u64) {
        return;
    }
    let now = unix_secs(ctx.env.clock.now());
    ctx.facts.insert(
        RESTART_SINCE_FACT.into(),
        serde_json::json!({ "step": step, "since": now }),
    );
}

fn restart_wait_since(ctx: &mut StepCtx<'_>) -> f64 {
    let step = ctx.step as u64;
    let entry = ctx.facts.get(RESTART_SINCE_FACT);
    match entry {
        Some(v) if v.get("step").and_then(|s| s.as_u64()) == Some(step) => {
            v.get("since").and_then(|s| s.as_f64()).unwrap_or(f64::MAX)
        }
        // A run reopened mid-wait from before the fact existed: from now.
        _ => {
            note_restart_wait(ctx);
            unix_secs(ctx.env.clock.now())
        }
    }
}

fn unix_secs(t: std::time::SystemTime) -> f64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// True when Moonraker answers and its host's uptime puts the last boot after `since`. False
/// when it cannot tell.
async fn booted_since(env: &Env, host: &Host, since: f64) -> bool {
    let Ok(stats) = env.moonraker.proc_stats(host).await else {
        return false;
    };
    let Some(uptime) = stats.get("system_uptime").and_then(|u| u.as_f64()) else {
        return false;
    };
    unix_secs(env.clock.now()) - uptime > since
}

/// Polls until Moonraker stops answering, so the Moonraker still running from before a restart
/// is never taken for the one after it. False when it kept answering for all of `window`.
/// Takes the `Env`, not the `StepCtx`: a shared borrow of the context held across an await
/// makes the runner's future not `Send`.
async fn wait_until_gone(env: &Env, host: &Host, window: Duration) -> bool {
    let start = env.clock.now();
    while env.moonraker.printer_info(host).await.is_ok() {
        let waited = env.clock.now().duration_since(start).unwrap_or_default();
        if waited >= window {
            return false;
        }
        env.clock.sleep(POLL.min(window - waited)).await;
    }
    true
}

fn ask_if_showing() -> StepOutcome {
    let choice = |value: &str, label: &str| Choice {
        value: value.into(),
        label: label.into(),
        image: None,
    };
    StepOutcome::NeedInput(Prompt::Ask {
        question: "Is HelixScreen showing on your printer's screen?".into(),
        choices: vec![choice("yes", "Yes"), choice("no", "No")],
    })
}

fn name(target: WaitTarget) -> String {
    match target {
        WaitTarget::Port(p) => format!("port {p}"),
        WaitTarget::Moonraker => "Moonraker".into(),
        WaitTarget::KlipperReady | WaitTarget::KlipperRestart => "Klipper".into(),
        WaitTarget::Ssh => "SSH".into(),
        WaitTarget::HelixService => "HelixScreen".into(),
    }
}

/// Never logs in: SSH is up when the port sends an SSH banner.
async fn poll(ctx: &mut StepCtx<'_>, target: WaitTarget) -> Poll {
    let (env, host) = (ctx.env, ctx.host);
    let up = |b: bool| if b { Poll::Up } else { Poll::NotYet };
    match target {
        WaitTarget::Moonraker => up(env.moonraker.printer_info(host).await.is_ok()),
        WaitTarget::KlipperReady | WaitTarget::KlipperRestart => {
            match env.moonraker.printer_info(host).await {
                Ok(info) => up(info.get("state").and_then(|s| s.as_str()) == Some("ready")),
                Err(_) => Poll::NotYet,
            }
        }
        WaitTarget::Port(p) => up(env.net.tcp_banner(host, p, CONNECT_TIMEOUT).await.is_ok()),
        WaitTarget::Ssh => up(matches!(
            env.net.tcp_banner(host, host.ssh_port, CONNECT_TIMEOUT).await,
            Ok(Some(banner)) if banner.starts_with("SSH-")
        )),
        WaitTarget::HelixService => helix_service(ctx).await,
    }
}

/// Moonraker's service state where it has one; otherwise `pidof` over the held SSH login.
/// A printer whose Moonraker answers without listing the service, with no login held, can
/// only be judged by the user; one whose Moonraker does not answer may still be rebooting.
async fn helix_service(ctx: &mut StepCtx<'_>) -> Poll {
    let info = ctx.env.moonraker.system_info(ctx.host).await;
    let moonraker_answered = info.is_ok();
    match info.ok().as_ref().and_then(probe::helix_service_active) {
        Some(true) => return Poll::Up,
        Some(false) => return Poll::NotYet,
        None => {}
    }
    if ctx.login().is_none() {
        // An unreachable printer is reported by the poll itself.
        let _ = ctx.try_default_logins().await;
    }
    if ctx.login().is_none() {
        return if moonraker_answered {
            Poll::CannotTell
        } else {
            Poll::Silent
        };
    }
    let ran = match ctx.ssh().await {
        Ok(s) => s.exec("pidof helix-screen", None, PIDOF_TIMEOUT).await,
        Err(e) => Err(e),
    };
    match ran {
        Ok(out) if out.exit == 0 => Poll::Up,
        Ok(_) => Poll::NotYet,
        Err(_) => {
            ctx.drop_ssh();
            Poll::NotYet
        }
    }
}
