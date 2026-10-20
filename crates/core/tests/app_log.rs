// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::app_log::{timestamp, AppLog, KEEP_LOGS, MAX_LOG_BYTES};
use helix_installer_core::bundle::support_bundle_with_app_log;
use helix_installer_core::facts::Facts;
use helix_installer_core::run_state::RunState;
use helix_installer_core::transport::Host;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PASSWORD: &str = "hunter2-pw";

/// 2026-10-20T15:28:28.581Z
fn at(ms_after: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(1_792_510_108_581 + ms_after)
}

fn logs_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".log"))
        .collect();
    names.sort();
    names
}

#[test]
fn a_timestamp_is_utc_to_the_millisecond() {
    assert_eq!(timestamp(at(0)), "2026-10-20T15:28:28.581Z");
    assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    // A leap day.
    assert_eq!(
        timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
        "2000-02-29T00:00:00.000Z"
    );
}

#[test]
fn every_line_carries_its_timestamp_and_no_password() {
    let dir = tempfile::tempdir().unwrap();
    let log = AppLog::open(dir.path(), at(0)).unwrap();
    log.write(at(5), "signed in", &[]);
    log.write(
        at(10),
        &format!("the installer said: bad login {PASSWORD}\nsecond line"),
        &[PASSWORD],
    );
    let text = log.contents();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "2026-10-20T15:28:28.586Z signed in",
            "2026-10-20T15:28:28.591Z the installer said: bad login <secret>",
            "2026-10-20T15:28:28.591Z second line",
        ]
    );
    assert!(log.path().starts_with(dir.path()));
}

#[test]
fn each_launch_starts_a_new_file_and_only_the_newest_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not a log").unwrap();
    for i in 0..(KEEP_LOGS as u64 + 2) {
        let log = AppLog::open(dir.path(), at(i * 1000)).unwrap();
        log.write(at(i * 1000), &format!("launch {i}"), &[]);
    }
    let names = logs_in(dir.path());
    assert_eq!(names.len(), KEEP_LOGS, "{names:?}");
    let newest = std::fs::read_to_string(dir.path().join(names.last().unwrap())).unwrap();
    assert!(
        newest.ends_with(&format!("launch {}\n", KEEP_LOGS + 1)),
        "{newest}"
    );
    let oldest = std::fs::read_to_string(dir.path().join(&names[0])).unwrap();
    assert!(oldest.ends_with("launch 2\n"), "{oldest}");
    assert!(
        dir.path().join("notes.txt").exists(),
        "other files are left alone"
    );
}

#[test]
fn two_launches_in_the_same_millisecond_get_their_own_files() {
    let dir = tempfile::tempdir().unwrap();
    let a = AppLog::open(dir.path(), at(0)).unwrap();
    let b = AppLog::open(dir.path(), at(0)).unwrap();
    a.write(at(1), "first", &[]);
    b.write(at(1), "second", &[]);
    assert_ne!(a.path(), b.path());
    assert!(a.contents().contains("first") && !a.contents().contains("second"));
}

#[test]
fn a_full_log_stops_growing_and_says_so_once() {
    let dir = tempfile::tempdir().unwrap();
    let log = AppLog::open(dir.path(), at(0)).unwrap();
    let line = "x".repeat(1000);
    for _ in 0..(MAX_LOG_BYTES / 1000 + 50) {
        log.write(at(1), &line, &[]);
    }
    let size = std::fs::metadata(log.path()).unwrap().len();
    assert!(size <= MAX_LOG_BYTES + 200, "{size}");
    let text = log.contents();
    assert_eq!(
        text.matches("log is full").count(),
        1,
        "{}",
        &text[text.len() - 300..]
    );
    assert!(text
        .trim_end()
        .ends_with("log is full; later lines are not written"));
}

#[test]
fn the_support_bundle_carries_the_app_log_redacted_and_masked() {
    let state = RunState::new("run1", "k1", "r", Host::new("192.168.1.50"), Facts::new());
    let app_log = format!(
        "2026-10-20T15:28:28.586Z run1: started at 192.168.1.50\n2026-10-20T15:28:28.590Z run1: echo {PASSWORD}\n"
    );
    let plain = support_bundle_with_app_log(&state, &[], &app_log, &[PASSWORD], false);
    let (_, section) = plain
        .split_once("installer log:\n")
        .expect("the bundle has an installer log section");
    assert!(
        section.contains("  2026-10-20T15:28:28.586Z run1: started at 192.168.1.50"),
        "{plain}"
    );
    assert!(!plain.contains(PASSWORD), "{plain}");
    let masked = support_bundle_with_app_log(&state, &[], &app_log, &[PASSWORD], true);
    assert!(!masked.contains("192.168.1.50"), "{masked}");
    assert!(
        masked.contains("2026-10-20T15:28:28.586Z"),
        "times are not addresses: {masked}"
    );
}

#[test]
fn of_two_launches_in_one_millisecond_the_later_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "installer-0000000000003.log",
        "installer-0000000000003-1.log",
        "installer-0000000000004.log",
        "installer-0000000000005.log",
        "installer-0000000000006.log",
    ] {
        std::fs::write(dir.path().join(name), name).unwrap();
    }
    AppLog::open(dir.path(), UNIX_EPOCH + Duration::from_millis(7)).unwrap();
    let names = logs_in(dir.path());
    assert_eq!(names.len(), KEEP_LOGS, "{names:?}");
    assert!(
        names.contains(&"installer-0000000000003-1.log".to_string()),
        "the second launch of ms 3 is newer: {names:?}"
    );
    assert!(
        !names.contains(&"installer-0000000000003.log".to_string()),
        "{names:?}"
    );
}

#[test]
fn a_masked_bundle_keeps_only_its_own_runs_log_lines_with_the_host_name_masked() {
    let facts: Facts =
        serde_json::from_value(serde_json::json!({ "klipper_hostname": "voronv0" })).unwrap();
    let state = RunState::new("run-1", "k1", "r", Host::new("192.168.1.50"), facts);
    let app_log = "\
2026-10-20T15:28:28.586Z discovery found 2 printers
2026-10-20T15:28:28.586Z   192.168.1.51 Ready K1 (host name other-box, printer k1, HelixScreen -)
2026-10-20T15:28:28.590Z run-2: started for SN-OTHER at 192.168.1.51 (printer k1, recipe r)
2026-10-20T15:28:28.591Z run-1: started for 192.168.1.50 at 192.168.1.50 (printer k1, recipe r)
2026-10-20T15:28:28.592Z run-1: step 1: connected to voronv0 (voronv0.local), not voronv0x
";
    let masked = support_bundle_with_app_log(&state, &[], app_log, &[], true);
    let (_, section) = masked.split_once("installer log:\n").unwrap();
    assert!(!masked.contains("SN-OTHER"), "{masked}");
    assert!(!masked.contains("other-box"), "{masked}");
    assert!(!section.contains("run-2"), "{section}");
    assert!(section.contains("run-1: started"), "{section}");
    assert!(
        section.contains("connected to <host> (<host>.local), not voronv0x"),
        "{section}"
    );
    assert!(
        !masked.contains("\"voronv0\""),
        "the facts section masks it too: {masked}"
    );
    let plain = support_bundle_with_app_log(&state, &[], app_log, &[], false);
    assert!(
        plain.contains("run-2: started for SN-OTHER"),
        "unmasked keeps the launch: {plain}"
    );
}
