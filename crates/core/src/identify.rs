// SPDX-License-Identifier: GPL-3.0-or-later

//! Maps the facts gathered about a printer to a recipe, or refuses.

use crate::facts::Facts;
use crate::manifest::{Cond, Manifest, Printer, RestoreSel};
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
pub(crate) fn cond_matches(cond: &Cond, fact: Option<&Value>) -> bool {
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
        Cond::Prefix { prefix } => fact
            .as_str()
            .is_some_and(|s| s.starts_with(prefix.as_str())),
        Cond::Not { not } => match fact {
            Value::Array(items) => !items.iter().any(|i| i.as_str() == Some(not)),
            other => !cond_matches(&Cond::Exact(not.clone()), Some(other)),
        },
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

/// Whether `f` still describes the manifest's printer `printer_id`, by the same rules
/// `identify` matched it with. An id the manifest does not list matches nothing.
pub fn answers_as(m: &Manifest, printer_id: &str, f: &Facts) -> bool {
    m.printers
        .iter()
        .find(|p| p.id == printer_id)
        .is_some_and(|p| printer_matches(p, f))
}

/// Whether some identify rule of `printer_id` can still match `f` once the facts it lacks are
/// known: no fact `f` holds contradicts the rule. Facts gathered a few at a time can rule a
/// printer out before they are enough to identify it.
pub fn could_answer_as(m: &Manifest, printer_id: &str, f: &Facts) -> bool {
    m.printers
        .iter()
        .find(|p| p.id == printer_id)
        .is_some_and(|p| {
            p.identify.iter().any(|rule| {
                !rule.is_empty()
                    && rule
                        .iter()
                        .all(|(k, c)| f.get(k).is_none_or(|v| cond_matches(c, Some(v))))
            })
        })
}

/// What identification makes of the facts for the manifest's printer `printer_id`: a recipe,
/// or its refusal. `None` when the manifest does not list the printer.
pub fn choose_for(m: &Manifest, printer_id: &str, f: &Facts) -> Option<Identified> {
    let printer = m.printers.iter().find(|p| p.id == printer_id)?;
    Some(choose_recipe(printer, f))
}

/// A printer with no matching choice always refuses; `fallback.refuse` is not consulted.
fn refuse(printer: &Printer) -> Identified {
    Identified::Refuse {
        printer_id: Some(printer.id.clone()),
        doc: printer.fallback.doc.clone(),
        reason: format!("no install recipe for this {} setup", printer.name),
    }
}

/// The printers the facts match, a last-resort printer only when nothing else does.
fn matching<'a>(m: &'a Manifest, f: &Facts) -> Vec<&'a Printer> {
    let all: Vec<&Printer> = m
        .printers
        .iter()
        .filter(|p| printer_matches(p, f))
        .collect();
    if all.iter().any(|p| !p.last_resort) {
        all.into_iter().filter(|p| !p.last_resort).collect()
    } else {
        all
    }
}

/// The first refused printer the facts match: it wins over every other match.
fn refused_match<'a>(m: &'a Manifest, f: &Facts) -> Option<&'a Printer> {
    m.printers
        .iter()
        .find(|p| p.refused && printer_matches(p, f))
}

fn turned_away(p: &Printer) -> Identified {
    Identified::Refuse {
        printer_id: Some(p.id.clone()),
        doc: p.fallback.doc.clone(),
        reason: format!("this installer cannot put HelixScreen on a {}", p.name),
    }
}

pub fn identify(m: &Manifest, f: &Facts) -> Identified {
    if let Some(p) = refused_match(m, f) {
        return turned_away(p);
    }
    let matched = matching(m, f);
    let printer = match matched.as_slice() {
        [] => return not_recognised(),
        [one] => *one,
        many => {
            return Identified::Ambiguous {
                candidates: many.iter().map(|p| p.id.clone()).collect(),
            }
        }
    };

    choose_recipe(printer, f)
}

/// The restore recipe for the manifest's printer `printer_id` given the facts: its single
/// recipe, or the first choice whose `when` the facts (with `state`) match. `None` when the
/// printer is unknown, refused or no choice matches.
pub fn restore_for(m: &Manifest, printer_id: &str, f: &Facts) -> Option<String> {
    let printer = m.printers.iter().find(|p| p.id == printer_id)?;
    match printer.restore.as_ref()? {
        RestoreSel::Recipe(r) => Some(r.clone()),
        RestoreSel::ByFacts(choices) => {
            let facts = with_state(f);
            choices
                .iter()
                .find(|c| !c.when.is_empty() && all_match(&c.when, &facts))
                .map(|c| c.recipe.clone())
        }
    }
}

fn with_state(f: &Facts) -> Facts {
    let mut facts = f.clone();
    facts.insert("state".to_string(), Value::String(start_state(f).as_fact()));
    facts
}

/// Picks the recipe for `printer` from the facts' starting state, or refuses.
fn choose_recipe(printer: &Printer, f: &Facts) -> Identified {
    if printer.refused {
        return turned_away(printer);
    }
    let state = start_state(f);
    let facts = with_state(f);
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

fn not_recognised() -> Identified {
    Identified::Refuse {
        printer_id: None,
        doc: GENERIC_REFUSE_DOC.to_string(),
        reason: "printer not recognised".to_string(),
    }
}

/// Identifies the printer as the model the user picked, which is how a printer that answers
/// nothing (stock firmware) or answers like several models is named. The pick only chooses
/// among the models the facts are consistent with: a printer that answered anything at all
/// and matches no model is not recognised, and one that matches other models but not the
/// picked one refuses. The user's word never overrides what the printer says.
pub fn identify_as(m: &Manifest, printer_id: &str, f: &Facts) -> Identified {
    let Some(printer) = m.printers.iter().find(|p| p.id == printer_id) else {
        return not_recognised();
    };
    if let Some(p) = refused_match(m, f) {
        return turned_away(p);
    }
    let matched = matching(m, f);
    let matched_any = !matched.is_empty();
    if !f.is_empty() && !matched_any {
        return not_recognised();
    }
    if matched_any && !matched.iter().any(|p| p.id == printer.id) {
        return Identified::Refuse {
            printer_id: Some(printer.id.clone()),
            doc: printer.fallback.doc.clone(),
            reason: format!(
                "this printer answers as a different model, not a {}",
                printer.name
            ),
        };
    }
    choose_recipe(printer, f)
}
