// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::facts::Facts;
use helix_installer_core::identify::{identify, start_state, Identified, StartState};
use helix_installer_core::manifest::Manifest;
use std::path::PathBuf;

fn dir(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(sub)
}

fn manifest() -> Manifest {
    let bytes = std::fs::read(dir("identify_manifest.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn probe(name: &str) -> Facts {
    let bytes = std::fs::read(dir("probes").join(format!("{name}.json"))).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn run(name: &str) -> Identified {
    identify(&manifest(), &probe(name))
}

fn recipe(printer: &str, state: StartState, recipe_id: &str) -> Identified {
    Identified::Recipe {
        printer_id: printer.to_string(),
        state,
        recipe_id: recipe_id.to_string(),
    }
}

fn modded() -> StartState {
    StartState::Modded("moonraker".into())
}

#[test]
fn start_state_precedence() {
    assert_eq!(start_state(&probe("empty")), StartState::Stock);
    assert_eq!(start_state(&probe("k1_simple_af")), modded());
    assert_eq!(
        start_state(&probe("helix_k2")),
        StartState::Helix("1.0.3".into())
    );
    assert_eq!(StartState::Stock.as_fact(), "stock");
    assert_eq!(modded().as_fact(), "modded:moonraker");
    assert_eq!(StartState::Helix("1.0.3".into()).as_fact(), "helix");
}

#[test]
fn k1_rooted_stock_without_moonraker_uses_user_model() {
    assert_eq!(
        run("k1_rooted_stock"),
        recipe("k1", StartState::Stock, "k1_stock")
    );
}

#[test]
fn user_model_with_no_network_facts_picks_the_stock_recipe() {
    let mut f = Facts::new();
    f.insert("user_model".into(), "k1".into());
    assert_eq!(
        identify(&manifest(), &f),
        recipe("k1", StartState::Stock, "k1_stock")
    );
}

#[test]
fn k1_simple_af() {
    assert_eq!(run("k1_simple_af"), recipe("k1", modded(), "k1_simple_af"));
}

#[test]
fn k2_q2_max4_stock() {
    assert_eq!(run("k2_stock"), recipe("k2", StartState::Stock, "k2_stock"));
    assert_eq!(run("q2_stock"), recipe("q2", StartState::Stock, "q2_stock"));
    assert_eq!(
        run("max4_stock"),
        recipe("max4", StartState::Stock, "max4_stock")
    );
}

#[test]
fn mainsailos_pi5() {
    assert_eq!(run("mainsailos_pi5"), recipe("pi5", modded(), "pi_install"));
}

#[test]
fn ad5x_stock_is_refused_with_the_printer_doc() {
    assert_eq!(
        run("ad5x_stock"),
        Identified::Refuse {
            printer_id: Some("ad5x".into()),
            doc: "https://helixscreen.org/docs/ad5x".into(),
            reason: run_reason("ad5x_stock"),
        }
    );
}

fn run_reason(name: &str) -> String {
    match run(name) {
        Identified::Refuse { reason, .. } => {
            assert!(!reason.is_empty());
            reason
        }
        other => panic!("expected Refuse, got {other:?}"),
    }
}

#[test]
fn ad5x_with_zmod() {
    assert_eq!(run("ad5x_zmod"), recipe("ad5x", modded(), "ad5x_zmod"));
}

#[test]
fn ad5m_stock() {
    assert_eq!(
        run("ad5m_stock"),
        recipe("ad5m", StartState::Stock, "ad5m_stock")
    );
}

#[test]
fn installed_helix_gets_the_upgrade_recipe() {
    assert_eq!(
        run("helix_k2"),
        recipe("k2", StartState::Helix("1.0.3".into()), "helix_upgrade")
    );
}

#[test]
fn fact_matching_two_printers_is_ambiguous() {
    assert_eq!(
        run("two_printers"),
        Identified::Ambiguous {
            candidates: vec!["k1".into(), "k2".into()]
        }
    );
}

#[test]
fn unparseable_version_fact_refuses() {
    assert!(matches!(
        run("q2_unknown_version"),
        Identified::Refuse { printer_id: Some(ref p), .. } if p == "q2"
    ));
}

#[test]
fn missing_version_fact_refuses() {
    let mut f = probe("q2_stock");
    f.remove("stock_version");
    assert!(matches!(
        identify(&manifest(), &f),
        Identified::Refuse {
            printer_id: Some(_),
            ..
        }
    ));
}

#[test]
fn nothing_recognised_gets_the_generic_refusal() {
    assert_eq!(
        run("empty"),
        Identified::Refuse {
            printer_id: None,
            doc: "https://helixscreen.org/docs/install".into(),
            reason: "printer not recognised".into(),
        }
    );
}

#[test]
fn contains_matches_only_list_facts() {
    // pi5 needs services to contain "mainsail"; a plain string that happens to equal or
    // include it is not a list.
    let mut f = probe("mainsailos_pi5");
    f.insert("services".into(), "mainsail".into());
    assert!(matches!(
        identify(&manifest(), &f),
        Identified::Refuse {
            printer_id: None,
            ..
        }
    ));
    f.insert(
        "services".into(),
        serde_json::json!(["moonraker", "mainsail"]),
    );
    assert!(matches!(
        identify(&manifest(), &f),
        Identified::Recipe { .. }
    ));
}

#[test]
fn exact_does_not_match_a_list_fact() {
    assert!(matches!(
        run("model_is_list"),
        Identified::Refuse {
            printer_id: None,
            ..
        }
    ));
}

#[test]
fn bool_and_number_facts_compare_by_json_text() {
    let mut m = manifest();
    m.printers[0].identify = vec![[(
        "slots".to_string(),
        helix_installer_core::manifest::Cond::Exact("4".into()),
    )]
    .into_iter()
    .collect()];
    let mut f = Facts::new();
    f.insert("slots".into(), serde_json::json!(4));
    assert!(matches!(
        identify(&m, &f),
        Identified::Recipe { ref printer_id, .. } if printer_id == "k1"
    ));
}
