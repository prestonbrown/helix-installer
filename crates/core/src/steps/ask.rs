// SPDX-License-Identifier: GPL-3.0-or-later

//! `ask`: a question with fixed answers; the chosen value becomes a fact.

use super::{StepCtx, StepOutcome};
use crate::manifest::Choice;
use crate::runner::{Input, Prompt};
use serde_json::Value;

/// An answer that is not one of `choices` asks again.
pub fn run(
    ctx: &mut StepCtx<'_>,
    fact: &str,
    question: &str,
    choices: &[Choice],
    input: Option<Input>,
) -> StepOutcome {
    match input {
        Some(Input::Choice(v)) if choices.iter().any(|c| c.value == v) => {
            ctx.facts.insert(fact.to_string(), Value::String(v));
            StepOutcome::Done
        }
        _ => StepOutcome::NeedInput(Prompt::Ask {
            question: question.to_string(),
            choices: choices.to_vec(),
        }),
    }
}
