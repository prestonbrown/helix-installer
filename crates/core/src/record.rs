// SPDX-License-Identifier: GPL-3.0-or-later

//! Record mode: turns what a real printer answered into a probe fixture that can be committed
//! to the repository, so a printer can be added to the identify tests without keeping its
//! addresses or serial.

use crate::bundle::{mask_addresses, SERIAL};
use crate::discovery::Candidate;
use crate::facts::Facts;
use serde_json::Value;

/// Facts that identify the printer model rather than the unit, kept as found.
fn kept_verbatim(key: &str) -> bool {
    key == "model" || key == "cpu_arch" || key.ends_with("_version")
}

/// The candidate's facts plus the extra `probes` results (fact name, value), as one compact
/// JSON object in the layout of `tests/fixtures/probes/*.json`. Addresses and the serial are
/// masked as in the support bundle; `model`, `cpu_arch` and `*_version` facts are not.
pub fn record(candidate: &Candidate, probes: &[(String, Value)]) -> String {
    let mut facts: Facts = candidate.facts.clone();
    facts.extend(probes.iter().cloned());
    let serial = facts
        .get("serial")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let masked: Facts = facts
        .into_iter()
        .map(|(key, value)| {
            let value = if kept_verbatim(&key) {
                value
            } else if key == "serial" {
                Value::String(SERIAL.to_string())
            } else {
                mask_value(value, serial.as_deref())
            };
            (key, value)
        })
        .collect();
    serde_json::to_string(&masked).unwrap_or_default()
}

fn mask_value(value: Value, serial: Option<&str>) -> Value {
    match value {
        Value::String(s) => {
            let s = serial.map_or(s.clone(), |n| s.replace(n, SERIAL));
            Value::String(mask_addresses(&s))
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| mask_value(v, serial)).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, mask_value(v, serial)))
                .collect(),
        ),
        other => other,
    }
}
