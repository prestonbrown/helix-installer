// SPDX-License-Identifier: GPL-3.0-or-later

//! `require`: a version fact must fall in a range, else the printer is refused.

use super::{StepCtx, StepOutcome};
use crate::version::{FwVersion, VersionRange};
use serde_json::Value;

/// A missing or unparseable version refuses: an unknown version is never assumed to fit.
pub fn run(ctx: &StepCtx<'_>, fact: &str, range: &VersionRange, doc: &str) -> StepOutcome {
    let have = ctx.facts.get(fact).and_then(Value::as_str);
    if have
        .and_then(FwVersion::parse)
        .is_some_and(|v| range.matches(&v))
    {
        return StepOutcome::Done;
    }
    StepOutcome::Refused {
        doc: doc.to_string(),
        reason: format!(
            "{fact} is {}, and this install needs {}",
            have.unwrap_or("unknown"),
            range.as_str()
        ),
    }
}
