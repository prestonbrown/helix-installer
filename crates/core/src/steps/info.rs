// SPDX-License-Identifier: GPL-3.0-or-later

//! `info` and `user_action`: show the user something and wait for Next.

use super::{probe, StepCtx, StepOutcome};
use crate::manifest::Probe;
use crate::runner::{Input, Prompt};

pub fn info(text: &str, image: &Option<String>, input: Option<Input>) -> StepOutcome {
    match input {
        Some(Input::Next) => StepOutcome::Done,
        _ => StepOutcome::NeedInput(Prompt::Info {
            text: text.to_string(),
            image: image.clone(),
        }),
    }
}

/// Done once the user confirms and, when the step has one, the `verify` probe answers. A
/// `tcp_port` verify needs the port open: the action is what was meant to open it.
pub async fn user_action(
    ctx: &mut StepCtx<'_>,
    text: &str,
    image: &Option<String>,
    verify: Option<&Probe>,
    input: Option<Input>,
) -> StepOutcome {
    if !matches!(input, Some(Input::Next)) {
        return StepOutcome::NeedInput(Prompt::UserAction {
            text: text.to_string(),
            image: image.clone(),
        });
    }
    match verify {
        None => StepOutcome::Done,
        Some(p) => match probe::run(ctx, p).await {
            StepOutcome::Failed { reason, help } => StepOutcome::Failed {
                reason: format!("the printer did not respond as expected: {reason}"),
                help,
            },
            StepOutcome::Done => match p {
                Probe::TcpPort { tcp_port, fact } if ctx.facts.get(fact) != Some(&true.into()) => {
                    StepOutcome::failed(format!(
                        "the printer did not respond as expected: port {tcp_port} is not open"
                    ))
                }
                _ => StepOutcome::Done,
            },
            other => other,
        },
    }
}
