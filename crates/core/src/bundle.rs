// SPDX-License-Identifier: GPL-3.0-or-later

//! The plain-text report a user can send us when an install goes wrong. Passwords are removed
//! always; with `mask` the addresses that identify the user's network and the printer's serial
//! go too.

use crate::run_state::RunState;
use serde_json::Value;

const SECRET: &str = "<secret>";
const IP: &str = "<ip>";
const MAC: &str = "<mac>";
const SERIAL: &str = "<serial>";

/// `secrets` are the passwords seen in the run (the run state never holds them). Each is
/// removed wherever it appears; an empty one is ignored. `mask` additionally hides IPv4 and
/// IPv6 addresses, MAC addresses and the `serial` fact, which also vanishes from any log
/// line that repeats it.
pub fn support_bundle(
    state: &RunState,
    job_logs: &[String],
    secrets: &[&str],
    mask: bool,
) -> String {
    let serial = serial_of(state);
    let mut text = render(state, job_logs, mask);
    let mut secrets: Vec<&str> = secrets.iter().copied().filter(|s| !s.is_empty()).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in secrets {
        text = text.replace(s, SECRET);
    }
    if mask {
        if let Some(serial) = serial.filter(|s| !s.is_empty()) {
            text = text.replace(&serial, SERIAL);
        }
        text = rewrite_runs(&text, is_mac_char, mask_mac);
        text = rewrite_runs(&text, is_v6_char, mask_ipv6);
        text = rewrite_runs(&text, is_v4_char, mask_ipv4);
    }
    text
}

fn serial_of(state: &RunState) -> Option<String> {
    state.facts.get("serial").map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn render(state: &RunState, job_logs: &[String], mask: bool) -> String {
    let mut out = String::new();
    out.push_str("HelixScreen installer support bundle\n");
    out.push_str(&format!("run: {}\n", state.run_id));
    out.push_str(&format!("printer: {}\n", state.printer_id));
    out.push_str(&format!("recipe: {}\n", state.recipe_id));
    out.push_str(&format!("host: {}\n", state.host.addr));
    out.push_str(&format!("status: {:?}\n", state.status));
    out.push_str(&format!(
        "step: {} committed: {}\n",
        state.step_index, state.committed
    ));
    out.push_str("facts:\n");
    for (key, value) in &state.facts {
        if mask && key == "serial" {
            out.push_str(&format!("  {key}: {SERIAL}\n"));
        } else {
            out.push_str(&format!("  {key}: {value}\n"));
        }
    }
    out.push_str("log:\n");
    for e in &state.log {
        out.push_str(&format!("  [{}] step {}: {}\n", e.at_unix, e.step, e.text));
    }
    out.push_str("printer-side job log:\n");
    for line in job_logs {
        out.push_str(&format!("  {line}\n"));
    }
    out
}

/// Replaces every maximal run of `in_run` characters that `mask` accepts. A run `mask` rejects
/// is kept as it was.
fn rewrite_runs(text: &str, in_run: fn(char) -> bool, mask: fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if !run.is_empty() {
            out.push_str(&mask(run).unwrap_or_else(|| run.clone()));
            run.clear();
        }
    };
    for c in text.chars() {
        if in_run(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn is_mac_char(c: char) -> bool {
    c.is_ascii_hexdigit() || c == ':' || c == '-'
}

fn is_v6_char(c: char) -> bool {
    c.is_ascii_hexdigit() || c == ':'
}

fn is_v4_char(c: char) -> bool {
    c.is_ascii_digit() || c == '.'
}

/// Six hex pairs joined by one separator kind.
fn mask_mac(run: &str) -> Option<String> {
    let sep = if run.contains(':') { ':' } else { '-' };
    let groups: Vec<&str> = run.split(sep).collect();
    let pair = |g: &&str| g.len() == 2 && g.chars().all(|c| c.is_ascii_hexdigit());
    (groups.len() == 6 && groups.iter().all(pair)).then(|| MAC.to_string())
}

fn mask_ipv6(run: &str) -> Option<String> {
    if is_ipv6(run) {
        return Some(IP.to_string());
    }
    // A sentence-ending colon belongs to the prose, not the address.
    let trimmed = run.strip_suffix(':')?;
    is_ipv6(trimmed).then(|| format!("{IP}:"))
}

fn is_ipv6(s: &str) -> bool {
    if s.matches(':').count() < 2 || s.contains(":::") {
        return false;
    }
    let compressed = s.matches("::").count();
    if compressed > 1 {
        return false;
    }
    let groups: Vec<&str> = s.split(':').filter(|g| !g.is_empty()).collect();
    if groups.iter().any(|g| g.len() > 4) {
        return false;
    }
    if compressed == 1 {
        groups.len() <= 7
    } else {
        groups.len() == 8 && !s.starts_with(':') && !s.ends_with(':')
    }
}

/// Four dotted decimal groups of at most 255. Trailing dots are sentence punctuation.
fn mask_ipv4(run: &str) -> Option<String> {
    let body = run.trim_end_matches('.');
    let dots = run.len() - body.len();
    let octet = |g: &str| !g.is_empty() && g.len() <= 3 && g.parse::<u8>().is_ok();
    let parts: Vec<&str> = body.split('.').collect();
    (parts.len() == 4 && parts.iter().all(|p| octet(p)))
        .then(|| format!("{IP}{}", ".".repeat(dots)))
}
