// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::facts::Facts;
use helix_installer_core::identify::{
    identify, identify_as, start_state, Identified, StartState, GENERIC_REFUSE_DOC,
};
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

#[test]
fn empty_when_never_matches() {
    let mut m = manifest();
    m.printers[0].recipes[0].when.clear();
    m.printers[0].recipes.truncate(1);
    match identify(&m, &probe("k1_rooted_stock")) {
        Identified::Refuse {
            printer_id: Some(p),
            doc,
            reason,
        } => {
            assert_eq!(p, "k1");
            assert_eq!(doc, "https://helixscreen.org/docs/k1");
            assert!(reason.contains("Creality K1"), "{reason}");
        }
        other => panic!("expected Refuse, got {other:?}"),
    }
}

#[test]
fn helix_installed_must_be_a_json_true() {
    let mut f = probe("helix_k2");
    f.insert("helix_installed".into(), "true".into());
    assert_eq!(start_state(&f), modded());
}

#[test]
fn helix_without_version_still_reaches_the_helix_recipe() {
    let mut f = probe("helix_k2");
    f.remove("helix_version");
    assert_eq!(start_state(&f), StartState::Helix(String::new()));
    assert_eq!(
        identify(&manifest(), &f),
        recipe("k2", StartState::Helix(String::new()), "helix_upgrade")
    );
}

#[test]
fn a_picked_model_with_no_facts_gets_its_stock_recipe() {
    assert_eq!(
        identify_as(&manifest(), "k1", &probe("empty")),
        recipe("k1", StartState::Stock, "k1_stock")
    );
}

#[test]
fn a_picked_model_settles_an_ambiguous_printer() {
    assert_eq!(
        identify_as(&manifest(), "k1", &probe("two_printers")),
        recipe("k1", StartState::Stock, "k1_stock")
    );
}

#[test]
fn a_picked_model_the_facts_contradict_is_refused() {
    match identify_as(&manifest(), "k1", &probe("k2_stock")) {
        Identified::Refuse {
            printer_id: Some(p),
            doc,
            reason,
        } => {
            assert_eq!(p, "k1");
            assert_eq!(doc, "https://helixscreen.org/docs/k1");
            assert!(reason.contains("Creality K1"), "{reason}");
        }
        other => panic!("expected Refuse, got {other:?}"),
    }
}

#[test]
fn a_picked_model_missing_from_the_manifest_is_refused() {
    assert_eq!(
        identify_as(&manifest(), "nope", &probe("empty")),
        Identified::Refuse {
            printer_id: None,
            doc: GENERIC_REFUSE_DOC.to_string(),
            reason: "printer not recognised".to_string(),
        }
    );
}

#[test]
fn a_picked_model_still_needs_a_matching_recipe() {
    let mut f = probe("k2_stock");
    f.insert("stock_version".into(), "1.0".into());
    assert!(matches!(
        identify_as(&manifest(), "k2", &f),
        Identified::Refuse { printer_id: Some(ref p), .. } if p == "k2"
    ));
}

fn not_recognised() -> Identified {
    Identified::Refuse {
        printer_id: None,
        doc: GENERIC_REFUSE_DOC.to_string(),
        reason: "printer not recognised".to_string(),
    }
}

#[test]
fn a_picked_model_never_names_a_printer_whose_answers_match_no_model() {
    // A FlashForge reply whose product id the installer does not know: a serial, no model.
    let f: Facts =
        serde_json::from_str(r#"{"flashforge_udp.serial":"SNAD5M0001","serial":"SNAD5M0001"}"#)
            .unwrap();
    // Facts that would pick an AD5X recipe once the model is assumed still refuse.
    let mut helix = f.clone();
    helix.insert("helix_installed".into(), true.into());
    assert_eq!(identify_as(&manifest(), "ad5x", &helix), not_recognised());
    assert_eq!(identify_as(&manifest(), "ad5x", &f), not_recognised());
}

#[test]
fn a_picked_model_never_names_an_unknown_moonraker_host() {
    let f: Facts = serde_json::from_str(
        r#"{"has_moonraker":true,"klipper_hostname":"voron24","cpu_arch":"aarch64"}"#,
    )
    .unwrap();
    assert_eq!(identify_as(&manifest(), "pi5", &f), not_recognised());
    assert_eq!(identify_as(&manifest(), "k1", &f), not_recognised());
}

fn one_printer(rule: serde_json::Value) -> Manifest {
    serde_json::from_value(serde_json::json!({
        "manifest_version": 1, "min_app_version": "0.1.0", "artifacts": {}, "recipes": {},
        "printers": [{
            "id": "p", "name": "P", "identify": [rule],
            "recipes": [{"when": {"state": "modded:moonraker"}, "recipe": "r"}],
            "restore": "r", "fallback": {"refuse": true, "doc": "https://d"}
        }]
    }))
    .unwrap()
}

fn matches_p(m: &Manifest, facts: serde_json::Value) -> bool {
    let f: Facts = serde_json::from_value(facts).unwrap();
    matches!(identify(m, &f), Identified::Recipe { ref printer_id, .. } if printer_id == "p")
}

#[test]
fn prefix_matches_the_start_of_a_text_fact_only() {
    let m = one_printer(serde_json::json!({"os_distribution": {"prefix": "Debian GNU/Linux"}}));
    let mr =
        |os: serde_json::Value| serde_json::json!({"has_moonraker": true, "os_distribution": os});
    assert!(matches_p(&m, mr("Debian GNU/Linux 13 (trixie)".into())));
    assert!(matches_p(&m, mr("Debian GNU/Linux".into())));
    assert!(!matches_p(&m, mr("Raspbian Debian GNU/Linux".into())));
    assert!(!matches_p(&m, mr("debian gnu/linux 13".into())));
    assert!(!matches_p(
        &m,
        mr(serde_json::json!(["Debian GNU/Linux 13"]))
    ));
    assert!(!matches_p(&m, serde_json::json!({"has_moonraker": true})));
}

#[test]
fn not_needs_the_fact_and_rejects_the_value_or_list_element() {
    let m = one_printer(serde_json::json!({"klipper_hostname": {"not": "linaro-alip"}}));
    let host =
        |h: serde_json::Value| serde_json::json!({"has_moonraker": true, "klipper_hostname": h});
    assert!(matches_p(&m, host("mainsailos".into())));
    assert!(!matches_p(&m, host("linaro-alip".into())));
    assert!(
        !matches_p(&m, serde_json::json!({"has_moonraker": true})),
        "a missing fact never matches"
    );
    let m = one_printer(serde_json::json!({"klipper_objects": {"not": "led chamber_light"}}));
    let objs =
        |o: serde_json::Value| serde_json::json!({"has_moonraker": true, "klipper_objects": o});
    assert!(matches_p(&m, objs(serde_json::json!(["mcu", "probe"]))));
    assert!(!matches_p(
        &m,
        objs(serde_json::json!(["mcu", "led chamber_light"]))
    ));
}

#[test]
fn prefix_and_not_round_trip_through_serde() {
    let rule = serde_json::json!({"a": {"prefix": "x"}, "b": {"not": "y"}});
    let m = one_printer(rule.clone());
    let back = serde_json::to_value(&m.printers[0].identify[0]).unwrap();
    assert_eq!(back, rule);
}
