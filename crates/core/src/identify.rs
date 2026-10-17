// SPDX-License-Identifier: GPL-3.0-or-later

//! Maps the facts gathered about a printer to a recipe, or refuses.

use crate::facts::Facts;
use crate::manifest::{Cond, Manifest, Printer};
use crate::version::FwVersion;
use serde_json::Value;
use std::collections::BTreeMap;

/// The install guide linked when no printer-specific one applies.
pub const GENERIC_REFUSE_DOC: &str = "https://helixscreen.org/docs/install";

/// What is on the printer before the installer touches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartState {
    Stock,
    /// A Moonraker host without HelixScreen; the payload names the mod family.
    Modded(String),
    /// HelixScreen is installed; the payload is its version (empty when unknown).
    Helix(String),
}

impl StartState {
    /// The value recipes match against the `state` fact.
    pub fn as_fact(&self) -> String {
        match self {
            StartState::Stock => "stock".to_string(),
            StartState::Modded(m) => format!("modded:{m}"),
            StartState::Helix(_) => "helix".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identified {
    Recipe {
        printer_id: String,
        state: StartState,
        recipe_id: String,
    },
    /// More than one printer matches; the UI asks which.
    Ambiguous { candidates: Vec<String> },
    Refuse {
        printer_id: Option<String>,
        doc: String,
        reason: String,
    },
}

fn is_true(f: &Facts, key: &str) -> bool {
    f.get(key) == Some(&Value::Bool(true))
}

pub fn start_state(f: &Facts) -> StartState {
    if is_true(f, "helix_installed") {
        let version = f
            .get("helix_version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        StartState::Helix(version.to_string())
    } else if is_true(f, "has_moonraker") {
        StartState::Modded("moonraker".to_string())
    } else {
        StartState::Stock
    }
}

/// A missing fact never matches.
fn cond_matches(cond: &Cond, fact: Option<&Value>) -> bool {
    let Some(fact) = fact else { return false };
    match cond {
        Cond::Exact(want) => match fact {
            Value::String(s) => s == want,
            Value::Bool(b) => *want == if *b { "true" } else { "false" },
            Value::Number(n) => n.to_string() == want.as_str(),
            _ => false,
        },
        Cond::Contains { contains } => fact
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i.as_str() == Some(contains))),
        Cond::Range(range) => fact
            .as_str()
            .and_then(FwVersion::parse)
            .is_some_and(|v| range.matches(&v)),
    }
}

fn all_match(conds: &BTreeMap<String, Cond>, f: &Facts) -> bool {
    conds.iter().all(|(k, c)| cond_matches(c, f.get(k)))
}

/// An empty rule would match every printer, so it matches none.
fn printer_matches(p: &Printer, f: &Facts) -> bool {
    p.identify
        .iter()
        .any(|rule| !rule.is_empty() && all_match(rule, f))
}

/// A printer with no matching choice always refuses; `fallback.refuse` is not consulted.
fn refuse(printer: &Printer) -> Identified {
    Identified::Refuse {
        printer_id: Some(printer.id.clone()),
        doc: printer.fallback.doc.clone(),
        reason: format!("no install recipe for this {} setup", printer.name),
    }
}

pub fn identify(m: &Manifest, f: &Facts) -> Identified {
    let matched: Vec<&Printer> = m
        .printers
        .iter()
        .filter(|p| printer_matches(p, f))
        .collect();
    let printer = match matched.as_slice() {
        [] => {
            return Identified::Refuse {
                printer_id: None,
                doc: GENERIC_REFUSE_DOC.to_string(),
                reason: "printer not recognised".to_string(),
            }
        }
        [one] => *one,
        many => {
            return Identified::Ambiguous {
                candidates: many.iter().map(|p| p.id.clone()).collect(),
            }
        }
    };

    let state = start_state(f);
    let mut facts = f.clone();
    facts.insert("state".to_string(), Value::String(state.as_fact()));
    match printer
        .recipes
        .iter()
        .find(|c| !c.when.is_empty() && all_match(&c.when, &facts))
    {
        Some(choice) => Identified::Recipe {
            printer_id: printer.id.clone(),
            state,
            recipe_id: choice.recipe.clone(),
        },
        None => refuse(printer),
    }
}
