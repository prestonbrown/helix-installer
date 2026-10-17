// SPDX-License-Identifier: GPL-3.0-or-later

//! Firmware versions and the range expressions recipes use to accept them.

use std::cmp::Ordering;
use std::fmt;

/// A dotted numeric firmware version. Missing trailing components count as zero.
#[derive(Debug, Clone)]
pub struct FwVersion(Vec<u32>);

impl FwVersion {
    /// Parses dotted numerics, ignoring one leading `v`/`V` and anything from the first `-`.
    pub fn parse(s: &str) -> Option<FwVersion> {
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let core = s.split('-').next().unwrap_or("");
        parse_components(core).map(FwVersion)
    }

    fn component(&self, i: usize) -> u32 {
        self.0.get(i).copied().unwrap_or(0)
    }
}

fn parse_components(s: &str) -> Option<Vec<u32>> {
    s.split('.')
        .map(|c| {
            if !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) {
                c.parse::<u32>().ok()
            } else {
                None
            }
        })
        .collect()
}

impl Ord for FwVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.0.len().max(other.0.len());
        (0..n)
            .map(|i| self.component(i).cmp(&other.component(i)))
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for FwVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for FwVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for FwVersion {}

impl fmt::Display for FwVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u32::to_string).collect();
        f.write_str(&parts.join("."))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeError(String);

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid version range: {}", self.0)
    }
}

impl std::error::Error for RangeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Le,
    Gt,
    Lt,
    Eq,
}

#[derive(Debug, Clone)]
enum Comparator {
    Cmp(Op, FwVersion),
    /// `5.1.x`: every version whose leading components equal the prefix.
    Prefix(Vec<u32>),
}

impl Comparator {
    fn parse(token: &str) -> Result<Comparator, RangeError> {
        let err = || RangeError(format!("bad comparator {token:?}"));
        let (op, rest) = [
            (">=", Op::Ge),
            ("<=", Op::Le),
            (">", Op::Gt),
            ("<", Op::Lt),
            ("=", Op::Eq),
        ]
        .iter()
        .find_map(|(p, op)| token.strip_prefix(p).map(|r| (Some(*op), r)))
        .unwrap_or((None, token));

        let bare = rest.strip_prefix(['v', 'V']).unwrap_or(rest);
        if let Some(prefix) = bare.strip_suffix(".x").or_else(|| bare.strip_suffix(".X")) {
            if op.is_some_and(|o| o != Op::Eq) {
                return Err(err());
            }
            let prefix = prefix.split('-').next().unwrap_or("");
            return parse_components(prefix)
                .map(Comparator::Prefix)
                .ok_or_else(err);
        }
        let version = FwVersion::parse(rest).ok_or_else(err)?;
        Ok(Comparator::Cmp(op.unwrap_or(Op::Eq), version))
    }

    fn matches(&self, v: &FwVersion) -> bool {
        match self {
            Comparator::Cmp(op, bound) => {
                let ord = v.cmp(bound);
                match op {
                    Op::Ge => ord.is_ge(),
                    Op::Le => ord.is_le(),
                    Op::Gt => ord.is_gt(),
                    Op::Lt => ord.is_lt(),
                    Op::Eq => ord.is_eq(),
                }
            }
            Comparator::Prefix(prefix) => {
                prefix.iter().enumerate().all(|(i, p)| v.component(i) == *p)
            }
        }
    }
}

/// Alternatives separated by `||`; each alternative is space-separated comparators that must all hold.
#[derive(Debug, Clone)]
pub struct VersionRange {
    alternatives: Vec<Vec<Comparator>>,
    source: String,
}

impl VersionRange {
    pub fn parse(s: &str) -> Result<VersionRange, RangeError> {
        let alternatives = s
            .split("||")
            .map(|alt| {
                let comparators = alt
                    .split_whitespace()
                    .map(Comparator::parse)
                    .collect::<Result<Vec<_>, _>>()?;
                if comparators.is_empty() {
                    return Err(RangeError(format!("empty alternative in {s:?}")));
                }
                Ok(comparators)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(VersionRange {
            alternatives,
            source: s.to_string(),
        })
    }

    /// The expression this range was parsed from.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Versions at and around every bound: enough to tell whether two ranges can accept the same
    /// version, since membership only changes at a bound.
    pub fn sample_versions(&self) -> Vec<FwVersion> {
        let mut out = Vec::new();
        for c in self.alternatives.iter().flatten() {
            let base = match c {
                Comparator::Cmp(_, v) => v.0.clone(),
                Comparator::Prefix(p) => p.clone(),
            };
            let mut bumped = base.clone();
            if let Some(last) = bumped.last_mut() {
                *last = last.saturating_add(1);
            }
            let mut lowered = base.clone();
            if let Some(last) = lowered.last_mut() {
                *last = last.saturating_sub(1);
            }
            let mut deeper = base.clone();
            deeper.push(1);
            out.extend([base, bumped, lowered, deeper].map(FwVersion));
        }
        out
    }

    pub fn matches(&self, v: &FwVersion) -> bool {
        self.alternatives
            .iter()
            .any(|alt| alt.iter().all(|c| c.matches(v)))
    }
}

impl serde::Serialize for VersionRange {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.source)
    }
}

impl<'de> serde::Deserialize<'de> for VersionRange {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        VersionRange::parse(&s).map_err(serde::de::Error::custom)
    }
}
