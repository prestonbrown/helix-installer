// SPDX-License-Identifier: GPL-3.0-or-later

//! Facts gathered during identification, keyed by name.

use std::collections::BTreeMap;

pub type Facts = BTreeMap<String, serde_json::Value>;
