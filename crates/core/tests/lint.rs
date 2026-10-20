// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::lint::{lint, lint_with, LintError};
use helix_installer_core::manifest::Manifest;
use serde_json::{json, Value};
use std::process::Command;

fn base() -> Value {
    json!({
        "manifest_version": 1,
        "min_app_version": "0.1.0",
        "artifacts": {
            "fw": {"url": "https://example.org/fw.bin", "sha256": "a".repeat(64)}
        },
        "printers": [{
            "id": "p1", "name": "Printer One", "photo": "p1.png",
            "identify": [{"model": "p1"}],
            "recipes": [{"when": {"state": "stock"}, "recipe": "install"}],
            "restore": "restore",
            "fallback": {"refuse": true, "doc": "https://helixscreen.org/guide/supported-printers/"},
            "verified": {"by": "me", "date": "2026-10-01", "firmware": "1.0"}
        }],
        "recipes": {
            "install": [
                {"type": "info", "text": "hi", "image": "step.png"},
                {"type": "download", "artifact": "fw"},
                {"type": "helix_install", "platform": "armv7", "commits": true},
                {"type": "wait_for", "target": "helix_service", "timeout_s": 300, "message": "m"}
            ],
            "restore": [{"type": "ssh_run", "command": "true", "timeout_s": 5, "commits": true}]
        }
    })
}

fn assets() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    for f in ["p1.png", "step.png"] {
        std::fs::write(d.path().join(f), b"x").unwrap();
    }
    d
}

fn run(v: Value, allow_unverified: bool) -> Vec<LintError> {
    let m: Manifest = serde_json::from_value(v).unwrap();
    lint_with(&m, assets().path(), allow_unverified)
}

fn only(v: Value, needle: &str) {
    let errs = run(v, false);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].message.contains(needle), "{errs:?}");
}

#[test]
fn valid_manifest_is_clean() {
    let m: Manifest = serde_json::from_value(base()).unwrap();
    assert_eq!(lint(&m, assets().path()), vec![]);
}

#[test]
fn bad_sha_and_url() {
    let mut v = base();
    v["artifacts"]["fw"]["sha256"] = json!("abc");
    only(v, "lowercase hex");
    let mut v = base();
    v["artifacts"]["fw"]["sha256"] = json!("A".repeat(64));
    only(v, "lowercase hex");
    let mut v = base();
    v["artifacts"]["fw"]["sha256"] = json!("g".repeat(64));
    only(v, "lowercase hex");
    let mut v = base();
    v["artifacts"]["fw"]["url"] = json!("http://example.org/fw.bin");
    only(v, "https");
}

#[test]
fn fallback_must_refuse_with_doc() {
    let mut v = base();
    v["printers"][0]["fallback"]["refuse"] = json!(false);
    only(v, "refuse");
    let mut v = base();
    v["printers"][0]["fallback"]["doc"] = json!("");
    only(v, "doc");
}

#[test]
fn unknown_restore_and_recipe() {
    let mut v = base();
    v["printers"][0]["restore"] = json!("nope");
    v["recipes"]["restore"] = json!([
        {"type": "helix_install", "platform": "armv7", "commits": true},
        helix_running()
    ]);
    only(v, "restore");
    let mut v = base();
    v["printers"][0]["recipes"][0]["recipe"] = json!("nope");
    only(v, "unknown recipe");
}

#[test]
fn unknown_artifact() {
    let mut v = base();
    v["recipes"]["install"][1]["artifact"] = json!("nope");
    only(v, "unknown artifact");
}

#[test]
fn missing_images() {
    let mut v = base();
    v["printers"][0]["photo"] = json!("missing.png");
    only(v, "missing.png");
    let mut v = base();
    v["recipes"]["install"][0]["image"] = json!("../etc/passwd");
    only(v, "relative");
}

#[test]
fn terminal_step_rule() {
    let mut v = base();
    v["recipes"]["install"] = json!([{"type": "info", "text": "x"}]);
    only(v, "helix_service");
    // SshRun is only a legal ending for a restore recipe.
    let mut v = base();
    v["recipes"]["install"] =
        json!([{"type": "ssh_run", "command": "true", "timeout_s": 5, "commits": true}]);
    only(v, "helix_service");
    let mut v = base();
    v["recipes"]["install"] =
        json!([{"type": "wait_for", "target": "moonraker", "timeout_s": 5, "message": "m"}]);
    only(v, "helix_service");
    let mut v = base();
    v["recipes"]["install"] =
        json!([{"type": "wait_for", "target": "helix_service", "timeout_s": 5, "message": "m"}]);
    assert_eq!(run(v, false), vec![]);
}

#[test]
fn at_most_one_commit_step() {
    let mut v = base();
    v["recipes"]["install"][1]["commits"] = json!(true);
    let errs = run(v, false);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("more than one step has commits")),
        "{errs:?}"
    );
}

#[test]
fn empty_when_is_an_error() {
    let mut v = base();
    v["printers"][0]["recipes"][0]["when"] = json!({});
    only(v, "empty when");
}

#[test]
fn overlapping_ranges_are_flagged() {
    let mut v = base();
    v["printers"][0]["recipes"] = json!([
        {"when": {"fw_version": ">=1.0 <=2.0"}, "recipe": "install"},
        {"when": {"fw_version": ">=1.5"}, "recipe": "restore"}
    ]);
    only(v, "same facts");
}

#[test]
fn disjoint_choices_pass() {
    let mut v = base();
    v["printers"][0]["recipes"] = json!([
        {"when": {"fw_version": "<1.5"}, "recipe": "install"},
        {"when": {"fw_version": ">=1.5"}, "recipe": "restore"}
    ]);
    assert_eq!(run(v, false), vec![]);
    let mut v = base();
    v["printers"][0]["recipes"] = json!([
        {"when": {"state": "stock"}, "recipe": "install"},
        {"when": {"state": "helix"}, "recipe": "restore"}
    ]);
    assert_eq!(run(v, false), vec![]);
}

#[test]
fn unverified_printer_needs_the_flag() {
    let mut v = base();
    v["printers"][0].as_object_mut().unwrap().remove("verified");
    only(v.clone(), "verified");
    assert_eq!(run(v, true), vec![]);
}

#[test]
fn restore_recipe_may_end_in_gcode() {
    let mut v = base();
    v["recipes"]["restore"] =
        json!([{"type": "moonraker_gcode", "script": "FIRMWARE_RESTART", "commits": true}]);
    assert_eq!(run(v, false), vec![]);
}

#[test]
fn binary_prints_errors_and_exits_nonzero() {
    let dir = assets();
    let mut v = base();
    v["artifacts"]["fw"]["sha256"] = json!("abc");
    let path = dir.path().join("m.json");
    std::fs::write(&path, v.to_string()).unwrap();
    let bin = env!("CARGO_BIN_EXE_manifest-lint");
    let out = Command::new(bin)
        .arg(&path)
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("fw: "), "{stdout}");

    std::fs::write(&path, base().to_string()).unwrap();
    let out = Command::new(bin)
        .arg(&path)
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
}

fn install_with(step: Value) -> Value {
    let mut v = base();
    v["recipes"]["install"] = json!([step, {"type": "helix_install", "platform": "armv7", "commits": true}, helix_running()]);
    v
}

fn helix_running() -> Value {
    json!({"type": "wait_for", "target": "helix_service", "timeout_s": 300, "message": "m"})
}

#[test]
fn write_stick_artifact_refs() {
    only(
        install_with(json!({"type": "write_stick", "artifacts": ["nope"]})),
        "unknown artifact",
    );
    only(
        install_with(
            json!({"type": "write_stick", "artifacts": ["fw"], "rename": {"nope": "x.bin"}}),
        ),
        "unknown artifact",
    );
    assert_eq!(
        run(
            install_with(
                json!({"type": "write_stick", "artifacts": ["fw"], "rename": {"fw": "x.bin"}})
            ),
            false
        ),
        vec![]
    );
}

#[test]
fn step_images_must_exist() {
    only(
        install_with(json!({"type": "user_action", "text": "t", "image": "gone.png"})),
        "gone.png",
    );
    only(
        install_with(json!({"type": "info", "text": "t", "image": "gone.png"})),
        "gone.png",
    );
    only(
        install_with(json!({"type": "ask", "fact": "f", "question": "q",
            "choices": [{"value": "a", "label": "A", "image": "gone.png"}]})),
        "gone.png",
    );
    only(
        install_with(json!({"type": "info", "text": "t", "image": "/etc/passwd"})),
        "relative",
    );
}

#[test]
fn empty_recipe_is_an_error() {
    let mut v = base();
    v["recipes"]["install"] = json!([]);
    only(v, "no steps");
}

#[test]
fn an_install_or_update_must_end_by_seeing_helixscreen_run() {
    for last in [
        json!({"type": "helix_install", "platform": "armv7", "commits": true}),
        json!({"type": "moonraker_update", "name": "helixscreen", "commits": true}),
        json!({"type": "require", "fact": "fw_version", "range": ">=1.0", "doc": "https://x"}),
        json!({"type": "wait_for", "target": "klipper_restart", "timeout_s": 5, "message": "m"}),
    ] {
        let mut v = base();
        v["recipes"]["install"] = json!([last.clone()]);
        let errs = run(v, false);
        assert_eq!(errs.len(), 1, "{last}: {errs:?}");
        assert!(
            errs[0].message.contains("HelixScreen") && errs[0].message.contains("helix_service"),
            "{errs:?}"
        );
    }
    let mut v = base();
    v["recipes"]["install"] = json!([
        {"type": "moonraker_update", "name": "helixscreen", "commits": true},
        helix_running()
    ]);
    assert_eq!(run(v, false), vec![]);
}

#[test]
fn a_restore_may_end_with_the_uninstall_itself() {
    let mut v = base();
    v["recipes"]["restore"] = json!([
        {"type": "helix_install", "platform": "armv7", "uninstall": true, "commits": true}
    ]);
    assert_eq!(run(v, false), vec![]);
}

#[test]
fn malformed_probe_step() {
    only(
        install_with(json!({"type": "probe", "ssh": "uname"})),
        "probe needs exactly one",
    );
}

#[test]
fn contains_conditions_on_one_key_overlap() {
    let mut v = base();
    v["printers"][0]["recipes"] = json!([
        {"when": {"modules": {"contains": "a"}}, "recipe": "install"},
        {"when": {"modules": {"contains": "b"}}, "recipe": "restore"}
    ]);
    only(v, "same facts");
}

#[test]
fn prefix_and_not_conditions_overlap_only_when_one_fact_can_satisfy_both() {
    let pair = |a: Value, b: Value| {
        let mut v = base();
        v["printers"][0]["recipes"] = json!([
            {"when": {"os": a}, "recipe": "install"},
            {"when": {"os": b}, "recipe": "restore"}
        ]);
        run(v, false)
    };
    let p = |s: &str| json!({"prefix": s});
    let n = |s: &str| json!({"not": s});
    let overlap = |errs: Vec<LintError>| errs.iter().any(|e| e.message.contains("same facts"));
    assert!(overlap(pair(p("Debian"), json!("Debian 13"))));
    assert!(!overlap(pair(p("Debian"), json!("Armbian 26"))));
    assert!(overlap(pair(p("Debian"), p("Debian GNU"))));
    assert!(!overlap(pair(p("Debian"), p("Armbian"))));
    assert!(overlap(pair(n("x"), json!("y"))));
    assert!(!overlap(pair(n("x"), json!("x"))));
    assert!(overlap(pair(n("x"), p("Debian"))));
    assert!(
        overlap(pair(n("Debian"), p("Debian"))),
        "Debian 13 satisfies both"
    );
}

#[test]
fn a_require_without_range_or_equals_is_flagged() {
    let v = install(json!([{"type": "require", "fact": "f", "doc": "https://x"}]));
    let errs = run(v, false);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("exactly one of range and equals")),
        "{errs:?}"
    );
}

#[test]
fn a_refused_printer_needs_no_recipes_restore_or_verified_record() {
    let mut v = base();
    v["printers"].as_array_mut().unwrap().push(json!({
        "id": "u1", "name": "Snapmaker U1", "refused": true,
        "identify": [{"klipper_objects": {"contains": "machine_state_manager"}}],
        "recipes": [], "fallback": {"refuse": true, "doc": "https://helixscreen.org/guide/install-u1/"}
    }));
    assert_eq!(run(v.clone(), false), vec![]);
    v["printers"][1]["recipes"] = json!([{"when": {"state": "stock"}, "recipe": "install"}]);
    assert!(run(v, false)
        .iter()
        .any(|e| e.message.contains("refused printer")));
}

#[test]
fn restore_choices_must_name_known_recipes() {
    let mut v = base();
    v["printers"][0]["restore"] = json!([
        {"when": {"os": "a"}, "recipe": "restore"},
        {"when": {"os": "b"}, "recipe": "missing"}
    ]);
    let errs = run(v, false);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].message.contains("missing"), "{errs:?}");
}

#[test]
fn a_dry_run_install_changes_nothing_so_the_commit_may_follow_it() {
    let v = install(json!([
        {"type": "helix_install", "platform": "pi", "dry_run": true},
        {"type": "helix_install", "platform": "pi", "commits": true}
    ]));
    assert_eq!(run(v, false), vec![]);
}

#[test]
fn binary_exits_2_on_unparseable_json() {
    let dir = assets();
    let path = dir.path().join("m.json");
    std::fs::write(&path, "not json").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manifest-lint"))
        .arg(&path)
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!out.stderr.is_empty());
}

/// An install recipe of `steps`, ending as every install must, by seeing HelixScreen run.
fn install(steps: Value) -> Value {
    let mut v = base();
    let mut steps = steps.as_array().unwrap().clone();
    steps.push(helix_running());
    v["recipes"]["install"] = Value::Array(steps);
    v
}

const HELIX: &str = r#"{"type": "helix_install", "platform": "armv7"}"#;

fn step(json: &str) -> Value {
    serde_json::from_str(json).unwrap()
}

fn committed(json: &str) -> Value {
    let mut s = step(json);
    s["commits"] = json!(true);
    s
}

#[test]
fn a_recipe_that_changes_the_printer_needs_a_commit() {
    only(install(json!([step(HELIX)])), "no step has commits: true");
    for mutating in [
        r#"{"type": "ssh_run", "command": "true", "timeout_s": 5}"#,
        r#"{"type": "moonraker_gcode", "script": "SAVE_CONFIG"}"#,
        r#"{"type": "moonraker_update", "name": "helixscreen"}"#,
    ] {
        only(
            install(json!([step(mutating), committed(HELIX)])),
            "after the first step that changes the printer",
        );
    }
}

#[test]
fn a_restore_recipe_needs_a_commit_too() {
    let mut v = base();
    v["recipes"]["restore"] = json!([{"type": "ssh_run", "command": "true", "timeout_s": 5}]);
    only(v, "no step has commits: true");
}

#[test]
fn a_commit_before_the_first_change_is_accepted() {
    let v = install(json!([
        step(r#"{"type": "helix_install", "platform": "armv7", "check": true}"#),
        committed(r#"{"type": "info", "text": "Do not turn the printer off"}"#),
        step(PROBE),
        step(r#"{"type": "ssh_run", "command": "true", "timeout_s": 5}"#),
        step(HELIX)
    ]));
    assert_eq!(run(v, false), vec![]);
}

#[test]
fn a_recipe_that_changes_nothing_needs_no_commit() {
    let v = install(json!([
        {"type": "download", "artifact": "fw"},
        {"type": "require", "fact": "fw_version", "range": ">=1.0", "doc": "https://x"}
    ]));
    assert_eq!(run(v, false), vec![]);
}

const STICK: &str = r#"{"type": "write_stick", "artifacts": ["fw"]}"#;
const FLASH: &str = r#"{"type": "user_action", "text": "Insert the stick and power on"}"#;
const DONE: &str =
    r#"{"type": "wait_for", "target": "helix_service", "timeout_s": 5, "message": "m"}"#;

#[test]
fn flashing_from_a_stick_commits_at_the_flash_not_after() {
    // Writing the stick leaves the printer alone; the user action after it flashes the printer.
    only(
        install(json!([step(STICK), step(FLASH), step(DONE)])),
        "no step has commits: true",
    );
    only(
        install(json!([step(STICK), step(FLASH), committed(DONE)])),
        "after the first step that changes the printer",
    );
    assert_eq!(
        run(
            install(json!([step(STICK), committed(FLASH), step(DONE)])),
            false
        ),
        vec![]
    );
    assert_eq!(
        run(
            install(json!([committed(STICK), step(FLASH), step(DONE)])),
            false
        ),
        vec![]
    );
}

#[test]
fn a_user_action_before_any_stick_changes_nothing() {
    let v = install(json!([
        step(r#"{"type": "user_action", "text": "Turn the printer on"}"#),
        committed(HELIX)
    ]));
    assert_eq!(run(v, false), vec![]);
}

const PROBE: &str = r#"{"type": "probe", "moonraker": "objects_list"}"#;
const WAIT: &str = r#"{"type": "wait_for", "target": "moonraker", "timeout_s": 5, "message": "m"}"#;
const GCODE: &str = r#"{"type": "moonraker_gcode", "script": "X"}"#;

fn reidentify_errors(steps: Value) -> Vec<String> {
    run(install(steps), false)
        .into_iter()
        .map(|e| e.message)
        .filter(|m| m.contains("re-identif"))
        .collect()
}

#[test]
fn a_step_acting_after_the_commit_needs_a_probe_since_the_commit_or_the_last_wait() {
    // The committing step acts on the identification made before it.
    assert_eq!(
        reidentify_errors(json!([committed(GCODE), step(PROBE), step(HELIX)])),
        Vec::<String>::new()
    );
    let errs = reidentify_errors(json!([committed(GCODE), step(GCODE), step(HELIX)]));
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert!(errs[0].contains("step 1"), "{errs:?}");
    // A wait is a restart: what was probed before it no longer counts.
    let errs = reidentify_errors(json!([
        committed(GCODE),
        step(PROBE),
        step(WAIT),
        step(HELIX)
    ]));
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("step 3"), "{errs:?}");
    assert_eq!(
        reidentify_errors(json!([
            committed(GCODE),
            step(WAIT),
            step(PROBE),
            step(HELIX)
        ])),
        Vec::<String>::new()
    );
    // Before the commit nothing is stale.
    assert_eq!(
        reidentify_errors(json!([step(WAIT), committed(HELIX)])),
        Vec::<String>::new()
    );
}

#[test]
fn a_manifest_login_carries_no_typed_secret() {
    let mut v = base();
    v["recipes"]["install"] = json!([
        {"type": "ssh_login", "logins": [{"user": "pi", "password": "x", "sudo_password": "y"}]},
        {"type": "helix_install", "platform": "armv7", "commits": true},
        helix_running()
    ]);
    only(v, "sudo_password");
}

fn flash_step(extra: Value) -> Value {
    let mut s = json!({
        "type": "serial_flash", "asset": "panel-{tag}.zip", "chip": "esp32s3", "flash_mb": 16,
        "keep": ["nvs"], "commits": true
    });
    for (k, v) in extra.as_object().unwrap() {
        s[k] = v.clone();
    }
    s
}

fn with_install(steps: Value) -> Value {
    let mut v = base();
    v["recipes"]["install"] = steps;
    v
}

#[test]
fn a_usb_flash_may_end_with_the_flash_or_an_info_after_it() {
    let probe = json!({"type": "probe", "serial": "esp32"});
    let warn = json!({"type": "info", "text": "this removes what is on it"});
    assert_eq!(
        run(
            with_install(json!([probe, warn, flash_step(json!({}))])),
            false
        ),
        vec![]
    );
    let info = json!({"type": "info", "text": "set it up on its screen"});
    assert_eq!(
        run(
            with_install(json!([probe, warn, flash_step(json!({})), info])),
            false
        ),
        vec![]
    );
    // An update never erases, so it needs no warning.
    assert_eq!(
        run(
            with_install(json!([probe, flash_step(json!({"update": true}))])),
            false
        ),
        vec![]
    );
}

#[test]
fn an_info_with_no_flash_before_it_still_does_not_end_an_install() {
    only(
        with_install(json!([{"type": "info", "text": "done?"}])),
        "wait_for helix_service",
    );
}

#[test]
fn a_usb_flash_changes_the_panel_so_it_needs_the_commit() {
    only(
        with_install(json!([flash_step(
            json!({"commits": false, "update": true})
        )])),
        "no step has commits",
    );
}

#[test]
fn a_usb_install_that_may_erase_the_panel_must_say_so_first() {
    only(
        with_install(json!([{"type": "probe", "serial": "esp32"}, flash_step(json!({}))])),
        "erase",
    );
}

#[test]
fn a_usb_flash_needs_a_chip_and_its_flash_size() {
    only(
        with_install(json!([flash_step(json!({"flash_mb": 0, "update": true}))])),
        "chip and a flash_mb",
    );
}

/// An install whose HelixScreen step comes after another step's point of no return.
fn install_after_commit(before: Value) -> Value {
    let mut v = base();
    let mut steps = vec![];
    if !before.is_null() {
        steps.push(before);
    }
    steps.push(json!({"type": "ssh_run", "command": "true", "timeout_s": 5, "commits": true}));
    steps.push(json!({"type": "probe", "moonraker": "objects_list"}));
    steps.push(json!({"type": "helix_install", "platform": "armv7"}));
    steps.push(helix_running());
    v["recipes"]["install"] = json!(steps);
    v
}

#[test]
fn a_helix_install_after_the_commit_needs_its_release_checked_before_it() {
    only(install_after_commit(Value::Null), "release");
    assert_eq!(
        run(
            install_after_commit(
                json!({"type": "helix_install", "platform": "armv7", "check": true})
            ),
            false
        ),
        vec![]
    );
}

#[test]
fn a_release_check_is_neither_an_update_nor_a_dry_run_nor_a_commit() {
    for extra in [
        json!({"update": true}),
        json!({"dry_run": true}),
        json!({"uninstall": true}),
        json!({"commits": true}),
    ] {
        let mut step = json!({"type": "helix_install", "platform": "armv7", "check": true});
        for (k, v) in extra.as_object().unwrap() {
            step[k] = v.clone();
        }
        let errs = run(install_after_commit(step), false);
        assert!(
            errs.iter().any(|e| e.message.contains("check")),
            "{extra}: {errs:?}"
        );
    }
}

#[test]
fn the_check_before_the_commit_must_be_for_the_install_after_it() {
    // install_after_commit installs armv7, unpinned, as a new install.
    for (check, why) in [
        (
            json!({"type": "helix_install", "platform": "k1", "check": true}),
            "another platform",
        ),
        (
            json!({"type": "helix_install", "platform": "armv7", "channel": "stable", "check": true}),
            "another channel",
        ),
        (
            json!({"type": "helix_install", "platform": "armv7", "update": true, "dry_run": true}),
            "an update's dry run",
        ),
    ] {
        let errs = run(install_after_commit(check), false);
        assert!(
            errs.iter().any(|e| e.message.contains("release")),
            "{why}: {errs:?}"
        );
    }
    // A dry run of the same install covers it as well as a check does.
    assert_eq!(
        run(
            install_after_commit(
                json!({"type": "helix_install", "platform": "armv7", "dry_run": true})
            ),
            false
        ),
        vec![]
    );
}

#[test]
fn every_install_after_the_commit_needs_its_own_check() {
    let mut v =
        install_after_commit(json!({"type": "helix_install", "platform": "armv7", "check": true}));
    let steps = v["recipes"]["install"].as_array_mut().unwrap();
    let last = steps.len() - 1;
    steps.insert(last, json!({"type": "helix_install", "platform": "k1"}));
    let errs = run(v, false);
    assert!(
        errs.iter().any(|e| e.message.contains("release")),
        "{errs:?}"
    );
}
