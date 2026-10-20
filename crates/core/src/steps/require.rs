// SPDX-License-Identifier: GPL-3.0-or-later

//! `require`: a fact must fall in a version range or equal a value, else the printer is refused,
//! or, for a `warn` require, the user is warned and chooses.

use super::{StepCtx, StepOutcome};
use crate::identify::cond_matches;
use crate::manifest::Cond;
use crate::runner::{Input, Prompt};
use crate::version::VersionRange;
use serde_json::Value;

const CONTINUE: &str = "Continue anyway";

/// One `require` step as the manifest gives it.
pub struct Spec<'a> {
    pub fact: &'a str,
    pub range: Option<&'a VersionRange>,
    pub equals: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub doc: &'a str,
    pub warn: bool,
}

/// A missing, unparseable or unequal fact refuses: an unknown value is never assumed to fit.
/// `equals` compares as identify rules do, so `true` matches a yes/no probe's boolean. A `warn`
/// require shows its reason instead, and `Input::Next` (its button) goes on.
pub fn run(ctx: &StepCtx<'_>, spec: &Spec<'_>, input: Option<Input>) -> StepOutcome {
    let Spec {
        fact,
        range,
        equals,
        reason,
        doc,
        warn,
    } = *spec;
    let have = ctx.facts.get(fact);
    let (ok, needs) = match (range, equals) {
        (Some(r), _) => (
            cond_matches(&Cond::Range(r.clone()), have),
            r.as_str().to_string(),
        ),
        (None, Some(e)) => (cond_matches(&Cond::Exact(e.into()), have), e.to_string()),
        (None, None) => (false, "a value".into()),
    };
    if ok {
        return StepOutcome::Done;
    }
    let shown = match have {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "unknown".into(),
    };
    let reason = reason.map_or_else(
        || format!("{fact} is {shown}, and this install needs {needs}"),
        str::to_string,
    );
    if !warn {
        return StepOutcome::Refused {
            doc: doc.to_string(),
            reason,
        };
    }
    if matches!(input, Some(Input::Next)) {
        return StepOutcome::Done;
    }
    StepOutcome::NeedInput(Prompt::Confirm {
        text: reason,
        button: CONTINUE.into(),
        doc: Some(doc.to_string()),
    })
}
