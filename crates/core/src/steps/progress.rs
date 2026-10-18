// SPDX-License-Identifier: GPL-3.0-or-later

//! The `HELIX-PROGRESS` lines `install.sh --progress` prints on stdout, one per event,
//! tab-separated (docs/devel/INSTALLER.md, "Machine-readable progress", in helixscreen).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Start,
    Ok,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    /// The platform the install uses, sent once after detection.
    Platform(String),
    /// `detail` is empty on `Start` and `Skip`.
    Step {
        n: u32,
        total: u32,
        status: StepStatus,
        title: String,
        detail: String,
    },
    /// The last line of every run.
    Result { ok: bool, reason: Option<String> },
}

/// The line prefix, tab included: install.sh's usage text names the bare word.
const PREFIX: &str = "HELIX-PROGRESS\t";

/// Parses one log line; `None` for anything that is not exactly one of the documented forms.
/// A trailing carriage return is ignored.
pub fn parse_progress_line(line: &str) -> Option<ProgressEvent> {
    let rest = line
        .strip_suffix('\r')
        .unwrap_or(line)
        .strip_prefix(PREFIX)?;
    let fields: Vec<&str> = rest.split('\t').collect();
    match fields.as_slice() {
        ["platform", p] if !p.is_empty() => Some(ProgressEvent::Platform(p.to_string())),
        ["step", n, total, status, title, detail] => Some(ProgressEvent::Step {
            n: n.parse().ok()?,
            total: total.parse().ok()?,
            status: match *status {
                "start" => StepStatus::Start,
                "ok" => StepStatus::Ok,
                "fail" => StepStatus::Fail,
                "skip" => StepStatus::Skip,
                _ => return None,
            },
            title: title.to_string(),
            detail: detail.to_string(),
        }),
        ["result", "ok"] => Some(ProgressEvent::Result {
            ok: true,
            reason: None,
        }),
        ["result", "fail", reason] => Some(ProgressEvent::Result {
            ok: false,
            reason: (!reason.is_empty()).then(|| reason.to_string()),
        }),
        _ => None,
    }
}
