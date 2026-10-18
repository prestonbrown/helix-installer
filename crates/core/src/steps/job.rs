// SPDX-License-Identifier: GPL-3.0-or-later

//! Following a detached job on the printer: its log read line by line across step runs, and
//! the moment it exits, dies or goes silent.

use super::{StepCtx, StepOutcome};
use crate::transport::{RemoteJob, TransportError};
use std::time::{Duration, SystemTime};

const POLL: Duration = Duration::from_secs(2);
/// Output lines quoted in a failure.
const TAIL_LINES: usize = 10;

/// How far the current step has read its job's log. Held in memory only: a reopened app reads
/// the log again from the start, which is how it sees lines written before it closed.
pub(crate) struct JobCursor {
    log_path: String,
    offset: u64,
    /// Text after the last newline, not yet a complete line.
    partial: String,
    lines: Vec<String>,
    /// When the log last grew, or when the current call began following.
    last_growth: SystemTime,
}

impl JobCursor {
    /// Takes in new log text; returns the index of the first line it completed. `ended` means
    /// no more text will come, so an unterminated last line is complete.
    fn take(&mut self, text: &str, next_offset: u64, ended: bool, now: SystemTime) -> usize {
        let first_new = self.lines.len();
        if !text.is_empty() {
            self.last_growth = now;
        }
        self.offset = next_offset;
        self.partial.push_str(text);
        while let Some(nl) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=nl).collect();
            self.lines
                .push(line.trim_end_matches(['\n', '\r']).to_string());
        }
        if ended && !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.lines.push(line.trim_end_matches('\r').to_string());
        }
        first_new
    }
}

pub(crate) enum Followed {
    /// A new line the caller asked to hear about arrived; the job is still running.
    Event,
    Exited(i32),
    /// The job's process is gone and it never wrote an exit status.
    Died,
    /// The log did not grow for the whole silence limit.
    Silent,
}

/// Reads `job`'s log until a new line matches `is_event`, the job ends, or the log stays
/// unchanged for `silence`, counted from this call: a retry after a silence timeout gets a
/// whole window again.
pub(crate) async fn follow(
    ctx: &mut StepCtx<'_>,
    job: &RemoteJob,
    silence: Duration,
    is_event: fn(&str) -> bool,
) -> Result<Followed, TransportError> {
    let now = ctx.env.clock.now();
    if ctx
        .session
        .job_cursor
        .as_ref()
        .is_none_or(|c| c.log_path != job.log_path)
    {
        ctx.session.job_cursor = Some(JobCursor {
            log_path: job.log_path.clone(),
            offset: 0,
            partial: String::new(),
            lines: Vec::new(),
            last_growth: now,
        });
    }
    if let Some(cursor) = ctx.session.job_cursor.as_mut() {
        cursor.last_growth = now;
    }
    loop {
        let offset = ctx.session.job_cursor.as_ref().map_or(0, |c| c.offset);
        let chunk = ctx.ssh().await?.read_job(job, offset).await?;
        let now = ctx.env.clock.now();
        let ended = chunk.exit.is_some() || chunk.alive == Some(false);
        let Some(cursor) = ctx.session.job_cursor.as_mut() else {
            return Err(TransportError::Protocol("job cursor lost".into()));
        };
        let first_new = cursor.take(&chunk.text, chunk.next_offset, ended, now);
        if let Some(code) = chunk.exit {
            return Ok(Followed::Exited(code));
        }
        if chunk.alive == Some(false) {
            return Ok(Followed::Died);
        }
        if cursor.lines[first_new..].iter().any(|l| is_event(l)) {
            return Ok(Followed::Event);
        }
        let quiet = now.duration_since(cursor.last_growth).unwrap_or_default();
        if quiet >= silence {
            return Ok(Followed::Silent);
        }
        ctx.env.clock.sleep(POLL.min(silence - quiet)).await;
    }
}

/// Every complete line the job has written so far.
pub(crate) fn lines<'a>(ctx: &'a StepCtx<'_>) -> &'a [String] {
    ctx.session
        .job_cursor
        .as_ref()
        .map_or(&[], |c| c.lines.as_slice())
}

/// The last output lines that `keep` accepts, for a failure message: empty, or a colon and
/// one line each.
pub(crate) fn tail(ctx: &StepCtx<'_>, keep: fn(&str) -> bool) -> String {
    let kept: Vec<&str> = lines(ctx)
        .iter()
        .map(String::as_str)
        .filter(|l| keep(l) && !l.trim().is_empty())
        .collect();
    let last = &kept[kept.len().saturating_sub(TAIL_LINES)..];
    if last.is_empty() {
        String::new()
    } else {
        format!(":\n{}", last.join("\n"))
    }
}

/// Drops the job once it has ended, so a retry starts it again.
pub(crate) fn forget(ctx: &mut StepCtx<'_>) {
    *ctx.job = None;
    ctx.session.job_cursor = None;
}

/// The connection failed while the job runs on. The job stays recorded so a retry follows it
/// instead of starting a second copy.
pub(crate) fn lost(ctx: &mut StepCtx<'_>, what: &str, e: TransportError) -> StepOutcome {
    ctx.drop_ssh();
    StepOutcome::Failed {
        reason: format!("lost contact with the printer while {what} runs: {e}"),
        help: Some("It keeps running on the printer; retry to follow it.".into()),
    }
}
