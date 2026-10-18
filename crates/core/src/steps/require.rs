// SPDX-License-Identifier: GPL-3.0-or-later

//! `require`: a fact must fall in a version range or equal a value, else the printer is refused.

use super::{StepCtx, StepOutcome};
use crate::identify::cond_matches;
use crate::manifest::Cond;
use crate::version::VersionRange;
use serde_json::Value;

/// A missing, unparseable or unequal fact refuses: an unknown value is never assumed to fit.
/// `equals` compares as identify rules do, so `true` matches a yes/no probe's boolean.
pub fn run(
    ctx: &StepCtx<'_>,
    fact: &str,
    range: Option<&VersionRange>,
    equals: Option<&str>,
    reason: Option<&str>,
    doc: &str,
) -> StepOutcome {
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
    StepOutcome::Refused {
        doc: doc.to_string(),
        reason: reason.map_or_else(
            || format!("{fact} is {shown}, and this install needs {needs}"),
            str::to_string,
        ),
    }
}
