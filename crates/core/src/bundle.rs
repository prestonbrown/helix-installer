// SPDX-License-Identifier: GPL-3.0-or-later

//! The plain-text report a user can send us when an install goes wrong. Passwords are removed
//! always; with `mask` the addresses that identify the user's network and the printer's serial
//! go too.

use crate::run_state::RunState;
use serde_json::Value;

const SECRET: &str = "<secret>";
const IP: &str = "<ip>";
const MAC: &str = "<mac>";
pub(crate) const SERIAL: &str = "<serial>";
const HOST: &str = "<host>";

/// `secrets` are the passwords seen in the run (the run state never holds them). Each is
/// removed wherever it appears; an empty one is ignored. `mask` additionally hides IPv4 and
/// IPv6 addresses, MAC addresses and the `serial` fact, which also vanishes from any log
/// line that repeats it. Host names (`k1.local`) are not masked.
pub fn support_bundle(
    state: &RunState,
    job_logs: &[String],
    secrets: &[&str],
    mask: bool,
) -> String {
    support_bundle_with_app_log(state, job_logs, "", secrets, mask)
}

/// `support_bundle` with the app's own log (`app_log::AppLog`) after the printer's, redacted
/// the same way. The log covers the whole launch, so a masked bundle keeps only this run's
/// lines (other printers and runs stay out) and also hides this printer's host name.
pub fn support_bundle_with_app_log(
    state: &RunState,
    job_logs: &[String],
    app_log: &str,
    secrets: &[&str],
    mask: bool,
) -> String {
    let serial = serial_of(state);
    let mut text = render(state, job_logs, mask);
    if !app_log.is_empty() {
        text.push_str("installer log:\n");
        let own = format!(" {}: ", state.run_id);
        for line in app_log.lines().filter(|l| !mask || l.contains(&own)) {
            text.push_str(&format!("  {line}\n"));
        }
    }
    let mut text = redact(&text, secrets);
    if mask {
        if let Some(host) = state.facts.get("klipper_hostname").and_then(Value::as_str) {
            text = mask_word(&text, host, HOST);
        }
        if let Some(serial) = serial.filter(|s| !s.is_empty()) {
            text = text.replace(&serial, SERIAL);
        }
        text = mask_addresses(&text);
    }
    text
}

/// `text` with each of `secrets` replaced, longest first so one that contains another goes
/// whole. An empty secret is ignored.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut secrets: Vec<&str> = secrets.iter().copied().filter(|s| !s.is_empty()).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut text = text.to_string();
    for s in secrets {
        text = text.replace(s, SECRET);
    }
    text
}

/// `text` with every whole-word `word` replaced: a match glued to a letter, digit, `-` or `_`
/// is part of a longer name and stays.
fn mask_word(text: &str, word: &str, with: &str) -> String {
    let word = word.trim();
    if word.is_empty() {
        return text.to_string();
    }
    let in_name = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_');
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (i, _) in text.match_indices(word) {
        if i < copied {
            continue;
        }
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        if !in_name(before) && !in_name(after) {
            out.push_str(&text[copied..i]);
            out.push_str(with);
            copied = i + word.len();
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Hides MAC, IPv6 and IPv4 addresses (in that order, so a MAC is not read as IPv6).
pub(crate) fn mask_addresses(text: &str) -> String {
    let text = rewrite_runs(text, is_mac_char, mask_mac);
    let text = rewrite_runs(&text, is_v6_char, mask_ipv6);
    rewrite_runs(&text, is_v4_char, mask_ipv4)
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
    if let Some((n, path)) = crate::steps::keep_gcodes::set_aside_pending(&state.facts) {
        out.push_str(&format!(
            "print files set aside and not yet put back: {n} in {path}\n"
        ));
    }
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
    c.is_ascii_hexdigit() || c == ':' || c == '.'
}

fn is_v4_char(c: char) -> bool {
    c.is_ascii_digit() || c == '.'
}

/// Every window of six hex pairs joined by one separator kind, wherever it sits in the run,
/// so a label glued on in front (`mac:aa:...`) does not hide it.
fn mask_mac(run: &str) -> Option<String> {
    let sep = if run.contains(':') { ':' } else { '-' };
    let groups: Vec<&str> = run.split(sep).collect();
    let pair = |g: &str| g.len() == 2 && g.chars().all(|c| c.is_ascii_hexdigit());
    let mut covered = vec![false; groups.len()];
    for start in 0..groups.len().saturating_sub(5) {
        if groups[start..start + 6].iter().all(|g| pair(g)) {
            covered[start..start + 6].fill(true);
        }
    }
    if !covered.contains(&true) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for (i, g) in groups.iter().enumerate() {
        if !covered[i] {
            parts.push(g);
        } else if i == 0 || !covered[i - 1] {
            parts.push(MAC);
        }
    }
    Some(parts.join(&sep.to_string()))
}

fn mask_ipv6(run: &str) -> Option<String> {
    // Trailing dots and a colon are sentence punctuation, not part of the address.
    let body = run.trim_end_matches('.');
    let tail = &run[body.len()..];
    if is_ipv6(body) || is_v4_mapped(body) {
        return Some(format!("{IP}{tail}"));
    }
    let trimmed = body.strip_suffix(':')?;
    (is_ipv6(trimmed) || is_v4_mapped(trimmed)).then(|| format!("{IP}:{tail}"))
}

/// `::ffff:10.0.0.1`: an IPv6 prefix followed by a dotted IPv4 tail.
fn is_v4_mapped(s: &str) -> bool {
    let Some((head, v4)) = s.rsplit_once(':') else {
        return false;
    };
    mask_ipv4(v4).is_some() && is_ipv6(&format!("{head}:0:0"))
}

fn is_ipv6(s: &str) -> bool {
    if s.contains('.') || s.matches(':').count() < 2 || s.contains(":::") {
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
