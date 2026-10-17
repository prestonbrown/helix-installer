// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::version::{FwVersion, VersionRange};

fn v(s: &str) -> FwVersion {
    FwVersion::parse(s).unwrap_or_else(|| panic!("{s} should parse"))
}

fn range(s: &str) -> VersionRange {
    VersionRange::parse(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
}

#[test]
fn ordering_is_numeric_per_component() {
    assert!(v("1.3.3.5") < v("1.3.3.46"));
    assert!(v("3.1.10") > v("3.1.9"));
}

#[test]
fn leading_v_and_suffix_are_ignored() {
    assert_eq!(v("V1.2.3"), v("1.2.3"));
    assert_eq!(v("v1.2.3"), v("1.2.3"));
    assert_eq!(v("5.1.7-beta"), v("5.1.7"));
    assert_eq!(v("1.2.3-"), v("1.2.3"));
}

#[test]
fn shorter_version_pads_with_zeros() {
    assert_eq!(v("2.6.5"), v("2.6.5.0"));
    assert!(range("2.6.5").matches(&v("2.6.5.0")));
    assert!(v("2.6.5") < v("2.6.5.1"));
}

#[test]
fn malformed_versions_are_none() {
    for s in [
        "abc", "", "1..2", "1.2.", ".1", "vv1.2", "1.2.x", "-beta", "1.-2",
    ] {
        assert!(FwVersion::parse(s).is_none(), "{s:?} should not parse");
    }
}

#[test]
fn wildcard_matches_prefix() {
    let r = range("5.1.x");
    assert!(r.matches(&v("5.1.7-beta")));
    assert!(r.matches(&v("5.1.0")));
    assert!(r.matches(&v("5.1")));
    assert!(!r.matches(&v("5.2.0")));
    assert!(!r.matches(&v("5.0.9")));
}

#[test]
fn wildcard_only_as_last_component() {
    assert!(VersionRange::parse("5.x.1").is_err());
    assert!(VersionRange::parse("x").is_err());
}

#[test]
fn alternatives_and_comparators() {
    let r = range(">=1.1.7 <=1.2.3 || 3.0.3 || 3.0.9 || 3.1.0");
    assert!(r.matches(&v("1.2.0")));
    assert!(r.matches(&v("3.0.9")));
    assert!(!r.matches(&v("3.0.4")));
    assert!(!r.matches(&v("3.1.4")));
    assert!(!r.matches(&v("1.1.6")));
    assert!(!r.matches(&v("1.2.4")));
}

#[test]
fn strict_operators() {
    let r = range(">1.0 <2.0");
    assert!(r.matches(&v("1.0.1")));
    assert!(!r.matches(&v("1.0")));
    assert!(!r.matches(&v("2.0")));
    assert!(range("=1.2").matches(&v("1.2.0")));
}

#[test]
fn whitespace_around_alternatives_is_allowed() {
    assert!(range("  1.0  ||  2.0 ").matches(&v("2.0")));
}

#[test]
fn space_after_operator_is_an_error() {
    assert!(VersionRange::parse(">= 1.2").is_err());
}

#[test]
fn malformed_ranges_are_errors() {
    for s in ["", "   ", "||", "1.0 ||", ">=", "abc", ">=1.0 foo", "1..2"] {
        assert!(VersionRange::parse(s).is_err(), "{s:?} should be an error");
    }
}

#[test]
fn inclusive_operators_accept_their_bound() {
    assert!(range(">=1.1.7").matches(&v("1.1.7")));
    assert!(range("<=1.2.3").matches(&v("1.2.3")));
    assert!(!range(">=1.1.7").matches(&v("1.1.6")));
    assert!(!range("<=1.2.3").matches(&v("1.2.4")));
}
