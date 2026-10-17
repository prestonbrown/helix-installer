// SPDX-License-Identifier: GPL-3.0-or-later

//! `moonraker_gcode` and `moonraker_update`: one request each to the run's Moonraker.

use super::{StepCtx, StepOutcome};

pub async fn gcode(ctx: &mut StepCtx<'_>, script: &str) -> StepOutcome {
    match ctx.env.moonraker.gcode(ctx.host, script).await {
        Ok(()) => StepOutcome::Done,
        Err(e) => StepOutcome::failed(format!("Moonraker did not run {script}: {e}")),
    }
}

pub async fn update(ctx: &mut StepCtx<'_>, name: &str) -> StepOutcome {
    match ctx.env.moonraker.update_upgrade(ctx.host, name).await {
        Ok(()) => StepOutcome::Done,
        Err(e) => StepOutcome::failed(format!("Moonraker could not update {name}: {e}")),
    }
}
