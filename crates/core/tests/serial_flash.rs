// SPDX-License-Identifier: GPL-3.0-or-later

//! The K-Touch recipes in `manifest/staging.json`, run end to end against a fake board on a fake
//! USB port: what is written, what is never written, and what the user is told when it goes
//! wrong.

#[path = "support/ktouch.rs"]
mod ktouch;

use helix_installer_core::facts::Facts;
use helix_installer_core::identify::{identify, Identified, StartState};
use helix_installer_core::manifest::{Channel, Manifest};
use helix_installer_core::run_state::RunState;
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::steps::helix_install::{releases_manifest_url, GITHUB_RELEASE_API};
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSerial, FakeSsh,
};
use helix_installer_core::transport::{ChipInfo, Env, Host};
use ktouch::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

const PORT: &str = "/dev/ttyUSB0";
const MAC: &str = "dc:da:0c:11:22:33";
const NVS: u32 = 0x9000;
const CFG: u32 = 0xfe0000;
const DOC: &str = "https://helixscreen.org/1.1/guide/install-esp32/";

fn staging() -> Manifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../manifest/staging.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn s3(mac: &str) -> ChipInfo {
    ChipInfo {
        chip: "esp32s3".into(),
        flash_bytes: 16 * 1024 * 1024,
        mac: Some(mac.into()),
    }
}

fn usb_facts() -> Facts {
    Facts::from([("usb".to_string(), json!("1a86:7522"))])
}

struct Rig {
    serial: FakeSerial,
    fetch: Arc<FakeFetch>,
    cache: tempfile::TempDir,
}

impl Rig {
    /// A K-Touch on `PORT` running BTT's own firmware, with its WiFi and settings in flash, and
    /// HelixScreen `v1.1.0-beta.10` as the newest beta.
    fn new() -> Rig {
        let rig = Rig {
            serial: FakeSerial::new(),
            fetch: Arc::new(FakeFetch::new()),
            cache: tempfile::tempdir().unwrap(),
        };
        rig.plug(PORT, s3(MAC));
        rig.publish("v1.1.0-beta.10", &release_zip("v1.1.0-beta.10"), None);
        rig
    }

    /// The board runs HelixScreen `version` from its second app slot, otadata selecting it, with
    /// `older` left in the first.
    fn running_second_slot(&self, version: &str, older: &str) {
        self.serial.preload(PORT, 0x8000, partition_table());
        self.serial.preload(
            PORT,
            0xf000,
            otadata(Some(ota_entry(1, CRC_SEQ_1)), Some(ota_entry(2, CRC_SEQ_2))),
        );
        self.serial
            .preload(PORT, APP_OFFSET, app_image("helixscreen_esp32", older));
        self.serial
            .preload(PORT, 0x6a0000, app_image("helixscreen_esp32", version));
    }

    fn plug(&self, port: &str, chip: ChipInfo) {
        self.serial.plug(port, 0x1a86, 0x7522, chip);
        self.serial.preload(port, NVS, b"wifi credentials".to_vec());
        self.serial.preload(port, CFG, b"settings.json".to_vec());
        self.serial
            .preload(port, APP_OFFSET, app_image("ktouch_stock", "2.1.0"));
    }

    /// The board already runs HelixScreen `version`.
    fn running(&self, version: &str) {
        self.serial
            .preload(PORT, APP_OFFSET, app_image("helixscreen_esp32", version));
    }

    /// Makes `tag` the newest beta, its K-Touch zip on GitHub with `digest` (else the zip's own).
    /// The stable list stays at 1.0.5, a release before the K-Touch builds.
    fn publish(&self, tag: &str, zip: &[u8], digest: Option<&str>) {
        let stable = json!({ "version": "1.0.5", "tag": "v1.0.5", "assets": {} });
        self.fetch.add(
            &releases_manifest_url(Channel::Stable),
            stable.to_string().into_bytes(),
        );
        let release = json!({ "version": tag.trim_start_matches('v'), "tag": tag, "assets": {} });
        self.fetch.add(
            &releases_manifest_url(Channel::Beta),
            release.to_string().into_bytes(),
        );
        let name = format!("helixscreen-esp32-ktouch-{tag}.zip");
        let url =
            format!("https://github.com/prestonbrown/helixscreen/releases/download/{tag}/{name}");
        let digest = digest.map_or_else(|| sha(zip), str::to_string);
        let gh = json!({ "tag_name": tag, "assets": [
            { "name": "install.sh", "browser_download_url": "https://example.invalid/install.sh",
              "digest": format!("sha256:{}", sha(b"x")) },
            { "name": name, "browser_download_url": url, "digest": format!("sha256:{digest}") },
        ]});
        self.fetch.add(
            &format!("{GITHUB_RELEASE_API}/{tag}"),
            gh.to_string().into_bytes(),
        );
        self.fetch.add(&url, zip.to_vec());
    }

    fn env(&self) -> Env {
        Env {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh: Arc::new(FakeSsh::new()),
            drives: Arc::new(FakeDrives::new()),
            fetch: self.fetch.clone(),
            clock: Arc::new(FakeClock::new()),
            net: Arc::new(FakeNet::new()),
            serial: Arc::new(self.serial.clone()),
        }
    }

    fn runner(&self) -> Runner {
        let Identified::Recipe { recipe_id, .. } = identify(&staging(), &usb_facts()) else {
            panic!("the K-Touch did not identify");
        };
        self.runner_for(&recipe_id)
    }

    fn runner_for(&self, recipe_id: &str) -> Runner {
        let m = staging();
        let state = RunState::new("run", "ktouch", recipe_id, Host::new(PORT), usb_facts());
        Runner::new(self.env(), Arc::new(m), state).with_cache_dir(self.cache.path().to_path_buf())
    }

    fn written_offsets(&self) -> Vec<u32> {
        let mut o: Vec<u32> = self.serial.writes().iter().map(|w| w.1).collect();
        o.sort();
        o
    }
}

struct Trace {
    prompts: Vec<Prompt>,
    end: Event,
}

/// Advances until the run ends or fails, answering every screen with Next.
async fn drive(r: &mut Runner) -> Trace {
    let mut prompts = Vec::new();
    let mut input = None;
    for _ in 0..500 {
        match r.advance(input.take()).await {
            Event::Progress { .. } => {}
            Event::NeedInput(p) => {
                match &p {
                    Prompt::Info { .. } | Prompt::UserAction { .. } | Prompt::Confirm { .. } => {
                        input = Some(Input::Next)
                    }
                    other => panic!("unexpected prompt {other:?}"),
                }
                prompts.push(p);
            }
            end => return Trace { prompts, end },
        }
    }
    panic!("the run never ended");
}

fn failure(e: &Event) -> (bool, String, String) {
    match e {
        Event::Failed {
            committed,
            reason,
            help,
            ..
        } => (*committed, reason.clone(), help.clone().unwrap_or_default()),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_k_touch_on_usb_identifies_for_a_first_install() {
    match identify(&staging(), &usb_facts()) {
        Identified::Recipe {
            printer_id,
            state,
            recipe_id,
        } => {
            assert_eq!(printer_id, "ktouch");
            assert_eq!(state, StartState::Stock);
            assert_eq!(recipe_id, "ktouch_install");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn another_usb_serial_device_is_not_a_k_touch() {
    let f = Facts::from([("usb".to_string(), json!("1a86:7523"))]);
    assert!(matches!(
        identify(&staging(), &f),
        Identified::Refuse { .. }
    ));
}

#[tokio::test]
async fn a_first_install_writes_only_the_flash_args_parts_then_points_at_the_setup_screen() {
    let rig = Rig::new();
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(rig.written_offsets(), PART_OFFSETS.to_vec());
    // Over BTT's firmware the whole chip is erased first, so its WiFi and settings are gone;
    // the new app reading back below proves the erase came before the writes.
    assert_eq!(rig.serial.erases(), 1);
    assert_eq!(rig.serial.read_back(PORT, NVS, 16), vec![0xff; 16]);
    assert_eq!(rig.serial.read_back(PORT, CFG, 13), vec![0xff; 13]);
    // The user was told before anything was opened, then before the erase.
    match t.prompts.first() {
        Some(Prompt::Confirm { text, button, .. }) => {
            assert!(text.contains(PORT), "{text}");
            assert_eq!(button, "Continue");
        }
        other => panic!("expected the port to be confirmed first, got {other:?}"),
    }
    match t.prompts.get(1) {
        Some(Prompt::Info { text, .. }) => {
            assert!(text.contains("removes"), "{text}");
            assert!(text.contains("WiFi"), "{text}");
        }
        other => panic!("expected the erase warning, got {other:?}"),
    }
    // The factory image is in the zip and never written: no write is its size.
    assert!(rig.serial.writes().iter().all(|w| w.2 != FACTORY_LEN));
    assert_eq!(
        rig.serial.read_back(PORT, APP_OFFSET, 0x1000),
        app_image("helixscreen_esp32", "v1.1.0-beta.10")
    );
    // One open to read the board, one to flash it: never a polling loop.
    assert_eq!(rig.serial.connects(), vec![PORT, PORT]);
    assert_eq!(rig.serial.resets(), 2);
    match t.prompts.last() {
        Some(Prompt::Info { text, .. }) => assert!(text.contains("Set Up WiFi"), "{text}"),
        other => panic!("expected the setup screen, got {other:?}"),
    }
    assert_eq!(r.state().facts["helix_version"], json!("v1.1.0-beta.10"));
}

#[tokio::test]
async fn an_older_helixscreen_switches_to_the_update_and_ends_on_the_new_version() {
    let rig = Rig::new();
    rig.running("v1.1.0-beta.9");
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(r.state().recipe_id, "ktouch_update");
    assert!(
        matches!(t.prompts.as_slice(), [Prompt::Confirm { .. }]),
        "{:?}",
        t.prompts
    );
    assert_eq!(rig.written_offsets(), PART_OFFSETS.to_vec());
    // An update never erases: WiFi and settings survive.
    assert_eq!(rig.serial.erases(), 0);
    assert_eq!(rig.serial.read_back(PORT, NVS, 16), b"wifi credentials");
    assert_eq!(rig.serial.read_back(PORT, CFG, 13), b"settings.json");
    assert_eq!(r.state().facts["helix_version"], json!("v1.1.0-beta.10"));
    assert_eq!(r.state().done_note, None);
}

#[tokio::test]
async fn an_up_to_date_panel_is_not_written() {
    let rig = Rig::new();
    rig.running("v1.1.0-beta.10");
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert!(rig.serial.writes().is_empty());
    assert_eq!(rig.serial.connects().len(), 1);
    assert!(!r.state().committed, "nothing was written");
    assert_eq!(
        r.state().done_kind,
        Some(helix_installer_core::run_state::DoneKind::UpToDate)
    );
}

#[tokio::test]
async fn a_newer_helixscreen_is_never_downgraded() {
    let rig = Rig::new();
    rig.running("v1.1.0");
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert!(rig.serial.writes().is_empty());
    assert!(!r.state().committed, "nothing was written");
    let note = r.state().done_note.clone().unwrap_or_default();
    assert!(
        note.contains("a newer build of HelixScreen is installed"),
        "{note}"
    );
}

#[tokio::test]
async fn a_board_that_is_not_an_esp32_s3_is_refused_before_anything_is_written() {
    let rig = Rig::new();
    rig.plug(
        PORT,
        ChipInfo {
            chip: "esp32".into(),
            flash_bytes: 16 * 1024 * 1024,
            mac: Some(MAC.into()),
        },
    );
    let mut r = rig.runner();
    match drive(&mut r).await.end {
        Event::Refused { doc, reason, .. } => {
            assert_eq!(doc, DOC);
            assert!(reason.contains("ESP32-S3"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn a_board_with_less_flash_is_refused() {
    let rig = Rig::new();
    rig.plug(
        PORT,
        ChipInfo {
            chip: "esp32s3".into(),
            flash_bytes: 8 * 1024 * 1024,
            mac: Some(MAC.into()),
        },
    );
    let mut r = rig.runner();
    assert!(matches!(drive(&mut r).await.end, Event::Refused { .. }));
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn a_device_that_never_answers_as_a_bootloader_fails_without_a_write() {
    let rig = Rig::new();
    rig.serial.unplug(PORT);
    rig.serial.plug_other(PORT, 0x1a86, 0x7522);
    let mut r = rig.runner();
    let (committed, reason, help) = failure(&drive(&mut r).await.end);
    assert!(!committed);
    assert!(reason.contains("did not answer"), "{reason}");
    assert!(help.contains("plug it back in"), "{help}");
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn a_port_this_user_may_not_open_says_how_to_get_access() {
    let rig = Rig::new();
    rig.serial.deny(PORT, "uucp");
    let mut r = rig.runner();
    let (committed, reason, help) = failure(&drive(&mut r).await.end);
    assert!(!committed);
    assert!(reason.contains(PORT), "{reason}");
    assert!(help.contains("unplug"), "{help}");
    if cfg!(target_os = "linux") {
        assert!(help.contains("sudo usermod -aG uucp $USER"), "{help}");
    }
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn a_panel_unplugged_mid_flash_says_what_state_it_is_in_and_is_not_retried_blindly() {
    let rig = Rig::new();
    rig.serial.vanish_on_write(3);
    let mut r = rig.runner();
    let (committed, reason, help) = failure(&drive(&mut r).await.end);
    assert!(committed);
    assert!(reason.contains("while it was being written"), "{reason}");
    assert!(help.contains("blank until a flash completes"), "{help}");
    assert!(help.contains("Try this step again"), "{help}");
    let opened = rig.serial.connects().len();
    assert_eq!(opened, 2);

    // Plugged back in, it comes back under another name; Try again finds it by its USB id, asks
    // before opening that port, and checks it is the same board before writing it again.
    rig.plug("/dev/ttyUSB1", s3(MAC));
    let mut ev = r.advance(None).await;
    while matches!(ev, Event::Progress { .. }) {
        ev = r.advance(None).await;
    }
    match &ev {
        Event::NeedInput(Prompt::Confirm { text, .. }) => {
            assert!(text.contains("/dev/ttyUSB1"), "{text}")
        }
        other => panic!("expected the new port to be confirmed, got {other:?}"),
    }
    assert_eq!(rig.serial.connects().len(), 2);
    r.advance(Some(Input::Next)).await;
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(
        rig.serial.connects().last().map(String::as_str),
        Some("/dev/ttyUSB1")
    );
    assert_eq!(
        rig.serial.read_back("/dev/ttyUSB1", APP_OFFSET, 0x1000),
        app_image("helixscreen_esp32", "v1.1.0-beta.10")
    );
}

#[tokio::test]
async fn a_different_board_plugged_in_after_a_failure_is_not_written() {
    let rig = Rig::new();
    rig.serial.vanish_on_write(1);
    let mut r = rig.runner();
    failure(&drive(&mut r).await.end);
    rig.plug("/dev/ttyUSB1", s3("dc:da:0c:99:99:99"));
    let before = rig.serial.writes().len();
    let (committed, reason, _) = failure(&drive(&mut r).await.end);
    assert!(committed);
    assert!(
        reason.contains("not the BTT K-Touch the install started on"),
        "{reason}"
    );
    assert_eq!(rig.serial.writes().len(), before);
}

#[tokio::test]
async fn a_part_that_does_not_read_back_as_written_fails_the_flash() {
    let rig = Rig::new();
    rig.serial.corrupt_writes();
    let mut r = rig.runner();
    let (committed, reason, help) = failure(&drive(&mut r).await.end);
    assert!(committed);
    assert!(reason.contains("did not read back"), "{reason}");
    assert!(!help.is_empty());
    assert_eq!(rig.serial.writes().len(), 1);
    // The port is closed with a reset, not left holding the chip in its bootloader.
    assert_eq!(rig.serial.open_sessions(), 0);
    assert_eq!(rig.serial.abandoned(), 0);
}

#[tokio::test]
async fn an_update_whose_version_cannot_be_read_back_says_to_check_the_screen() {
    let rig = Rig::new();
    rig.running("v1.1.0-beta.9");
    rig.serial.silent_boot();
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(rig.written_offsets(), PART_OFFSETS.to_vec());
    let note = r.state().done_note.clone().unwrap_or_default();
    assert!(note.contains("restarted"), "{note}");
    assert_eq!(
        r.state().done_kind,
        Some(helix_installer_core::run_state::DoneKind::Note)
    );
    assert!(note.contains("check its screen"), "{note}");
}

#[tokio::test]
async fn a_panel_that_boots_another_version_fails() {
    // An update (no erase) whose build puts the app in the other slot: the panel boots the old one.
    let rig = Rig::new();
    rig.running("v1.1.0-beta.9");
    rig.publish(
        "v1.1.0-beta.10",
        &zip_with(
            "v1.1.0-beta.10",
            "v1.1.0-beta.10",
            FLASH_ARGS.replace("0x20000", "0x6a0000").as_str(),
        ),
        None,
    );
    let mut r = rig.runner();
    let (committed, reason, _) = failure(&drive(&mut r).await.end);
    assert!(committed);
    assert!(reason.contains("v1.1.0-beta.9"), "{reason}");
}

#[tokio::test]
async fn the_flash_reports_its_progress_while_it_writes() {
    let rig = Rig::new();
    let gate = rig.serial.hold_writes();
    let mut r = rig.runner();
    let mut fractions = Vec::new();
    let mut input = None;
    let end = loop {
        match r.advance(input.take()).await {
            Event::Progress { fraction, .. } => {
                if let Some(f) = fraction {
                    fractions.push(f);
                }
                gate.notify_one();
            }
            Event::NeedInput(_) => input = Some(Input::Next),
            end => break end,
        }
    };
    assert!(matches!(end, Event::Done), "{end:?}");
    assert!(
        fractions.iter().any(|f| *f > 0.0 && *f < 1.0),
        "{fractions:?}"
    );
    assert!(fractions.windows(2).all(|w| w[0] <= w[1]), "{fractions:?}");
}

#[tokio::test]
async fn a_zip_whose_app_is_not_its_release_is_not_written() {
    let rig = Rig::new();
    rig.publish(
        "v1.1.0-beta.10",
        &zip_with("v1.1.0-beta.10", "v1.1.0-beta.8", FLASH_ARGS),
        None,
    );
    let mut r = rig.runner();
    let (_, reason, _) = failure(&drive(&mut r).await.end);
    assert!(reason.contains("v1.1.0-beta.8"), "{reason}");
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn a_zip_that_does_not_match_its_github_digest_is_not_written() {
    let rig = Rig::new();
    rig.publish(
        "v1.1.0-beta.10",
        &release_zip("v1.1.0-beta.10"),
        Some(&"0".repeat(64)),
    );
    let mut r = rig.runner();
    let (committed, reason, _) = failure(&drive(&mut r).await.end);
    assert!(!committed, "nothing was written");
    assert!(reason.contains("hash"), "{reason}");
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn no_port_is_opened_until_the_user_confirms_it_and_stopping_there_resets_nothing() {
    let rig = Rig::new();
    let mut r = rig.runner();
    match r.advance(None).await {
        Event::NeedInput(Prompt::Confirm { text, .. }) => {
            assert!(text.contains(PORT), "{text}");
            assert!(text.contains("K-Touch"), "{text}");
        }
        other => panic!("expected a confirmation, got {other:?}"),
    }
    assert!(rig.serial.connects().is_empty());
    match r.advance(Some(Input::Cancel)).await {
        Event::Refused { cancelled, .. } => assert!(cancelled),
        other => panic!("{other:?}"),
    }
    assert!(rig.serial.connects().is_empty());
    assert_eq!(rig.serial.resets(), 0);
}

#[tokio::test]
async fn a_board_with_no_readable_app_is_erased_before_the_install() {
    let rig = Rig::new();
    rig.serial.unplug(PORT);
    rig.serial.plug(PORT, 0x1a86, 0x7522, s3(MAC));
    rig.serial.preload(PORT, NVS, b"wifi credentials".to_vec());
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(r.state().recipe_id, "ktouch_install");
    assert_eq!(rig.serial.erases(), 1);
    assert_eq!(rig.serial.read_back(PORT, NVS, 16), vec![0xff; 16]);
}

#[tokio::test]
async fn writing_helixscreen_again_over_helixscreen_never_erases() {
    let rig = Rig::new();
    rig.running("v1.1.0-beta.10");
    let mut r = rig.runner_for("ktouch_reflash");
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(rig.written_offsets(), PART_OFFSETS.to_vec());
    assert_eq!(rig.serial.erases(), 0);
    assert_eq!(rig.serial.read_back(PORT, NVS, 16), b"wifi credentials");
    assert_eq!(rig.serial.read_back(PORT, CFG, 13), b"settings.json");
}

#[tokio::test]
async fn a_panel_unplugged_while_it_is_erased_says_so() {
    let rig = Rig::new();
    rig.serial.vanish_on_erase();
    let mut r = rig.runner();
    let (committed, reason, help) = failure(&drive(&mut r).await.end);
    assert!(committed);
    assert!(reason.contains("erased"), "{reason}");
    assert!(help.contains("blank until a flash completes"), "{help}");
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn a_development_build_newer_than_the_release_is_left_alone() {
    // What a real K-Touch built from main after the beta.10 tag reports.
    let rig = Rig::new();
    rig.running("v1.1.0-beta.10-64-ga2416a6b7a");
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(r.state().recipe_id, "ktouch_update");
    assert!(rig.serial.writes().is_empty());
    assert_eq!(rig.serial.erases(), 0);
    assert_eq!(rig.serial.connects().len(), 1);
    assert!(!r.state().committed, "nothing was written");
    let note = r.state().done_note.clone().unwrap_or_default();
    assert!(
        note.contains("a newer build of HelixScreen is installed"),
        "{note}"
    );
    assert!(note.contains("1.1.0-beta.10-64-ga2416a6b7a"), "{note}");
}

#[tokio::test]
async fn a_development_build_older_than_the_release_is_updated() {
    let rig = Rig::new();
    rig.running("v1.1.0-beta.9-12-g0123abcd");
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(rig.written_offsets(), PART_OFFSETS.to_vec());
}

#[tokio::test]
async fn a_board_swapped_before_the_flash_is_refused_before_the_point_of_no_return() {
    let rig = Rig::new();
    let mut r = rig.runner();
    // Through the probe and the checks, up to the flash step's first call.
    let mut input = None;
    loop {
        match r.advance(input.take()).await {
            Event::NeedInput(_) => input = Some(Input::Next),
            Event::Progress { .. } if r.state().step_index == 4 => break,
            Event::Progress { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    rig.serial.unplug(PORT);
    rig.plug(
        PORT,
        ChipInfo {
            chip: "esp32".into(),
            flash_bytes: 16 * 1024 * 1024,
            mac: Some(MAC.into()),
        },
    );
    match drive(&mut r).await.end {
        Event::Refused { reason, .. } => {
            assert!(reason.contains("nothing was written"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!r.state().committed);
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn the_point_of_no_return_is_recorded_before_the_first_erase() {
    let rig = Rig::new();
    let gate = rig.serial.hold_writes();
    let mut r = rig.runner();
    let mut input = None;
    loop {
        let ev = r.advance(input.take()).await;
        let touched = rig.serial.erases() > 0 || !rig.serial.writes().is_empty();
        if touched {
            assert!(
                r.state().committed,
                "the panel was changed before the commit"
            );
        }
        match ev {
            Event::NeedInput(_) => input = Some(Input::Next),
            Event::Progress { .. } => gate.notify_one(),
            Event::Done => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(r.state().committed);
    assert_eq!(rig.serial.erases(), 1);
}

#[tokio::test]
async fn a_first_install_stopped_after_the_erase_says_the_panel_is_blank_and_try_again_finishes_it()
{
    let rig = Rig::new();
    // A flasher that resets the chip after a write leaves the stub: the next check times out.
    rig.serial.reboot_after_next_write();
    let mut r = rig.runner();
    let (committed, reason, help) = failure(&drive(&mut r).await.end);
    assert!(committed);
    assert!(reason.contains("blank"), "{reason}");
    assert!(help.contains("blank until"), "{help}");
    assert!(help.contains("Try this step again"), "{help}");
    assert_eq!(rig.serial.erases(), 1);
    assert_eq!(rig.serial.read_back(PORT, APP_OFFSET, 4), vec![0xff; 4]);

    // Try this step again writes the panel from the start, erase included, and finishes.
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(
        rig.serial.read_back(PORT, APP_OFFSET, 0x1000),
        app_image("helixscreen_esp32", "v1.1.0-beta.10")
    );
}

#[tokio::test]
async fn the_version_is_read_from_the_app_slot_the_panel_boots() {
    let rig = Rig::new();
    rig.running_second_slot("v1.1.0", "v1.1.0-beta.9");
    let mut r = rig.runner();
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert!(
        rig.serial.writes().is_empty(),
        "an older release was written over 1.1.0"
    );
    assert_eq!(r.state().facts["helix_version"], json!("v1.1.0"));
}

#[tokio::test]
async fn a_reopened_run_names_the_port_again_before_opening_it() {
    let rig = Rig::new();
    let mut r = rig.runner();
    let mut input = None;
    loop {
        match r.advance(input.take()).await {
            Event::NeedInput(_) => input = Some(Input::Next),
            Event::Progress { .. } if r.state().step_index == 4 => break,
            Event::Progress { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    let opened = rig.serial.connects().len();
    // The app is closed and opened again: a new runner from the saved state.
    let mut reopened = Runner::new(rig.env(), Arc::new(staging()), r.state().clone())
        .with_cache_dir(rig.cache.path().to_path_buf());
    let mut ev = reopened.advance(None).await;
    while matches!(ev, Event::Progress { .. }) {
        ev = reopened.advance(None).await;
    }
    match &ev {
        Event::NeedInput(Prompt::Confirm { text, .. }) => assert!(text.contains(PORT), "{text}"),
        other => panic!("expected the port to be confirmed again, got {other:?}"),
    }
    assert_eq!(rig.serial.connects().len(), opened);
    reopened.advance(Some(Input::Next)).await;
    assert!(matches!(drive(&mut reopened).await.end, Event::Done));
}

#[tokio::test]
async fn a_cancel_while_the_panel_is_being_written_lets_the_write_finish() {
    let rig = Rig::new();
    let gate = rig.serial.hold_writes();
    let mut r = rig.runner();
    let mut input = None;
    let mut cancelled = false;
    let end = loop {
        let ev = r.advance(input.take()).await;
        match ev {
            Event::NeedInput(_) => input = Some(Input::Next),
            Event::Progress { .. } => {
                if !cancelled && !rig.serial.writes().is_empty() {
                    cancelled = true;
                    input = Some(Input::Cancel);
                } else {
                    gate.notify_one();
                }
            }
            end => break end,
        }
    };
    assert!(cancelled);
    assert!(matches!(end, Event::Done), "{end:?}");
    assert_eq!(rig.written_offsets(), PART_OFFSETS.to_vec());
}

/// Advances to where the flash step holds the checked panel open in its flasher stub, the run
/// past its point of no return, before the first erase or write.
async fn to_ready(r: &mut Runner, rig: &Rig) {
    let mut input = None;
    loop {
        match r.advance(input.take()).await {
            Event::NeedInput(_) => input = Some(Input::Next),
            Event::Progress { .. } if rig.serial.open_sessions() == 1 => return,
            Event::Progress { .. } => {}
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn a_cancel_before_the_first_write_restarts_the_panel() {
    let rig = Rig::new();
    let mut r = rig.runner();
    to_ready(&mut r, &rig).await;
    assert!(rig.serial.writes().is_empty());
    assert_eq!(rig.serial.erases(), 0);
    r.advance(Some(Input::Cancel)).await;
    assert_eq!(rig.serial.open_sessions(), 0, "the port was left open");
    assert_eq!(
        rig.serial.abandoned(),
        0,
        "the panel was left in its bootloader"
    );
    assert!(rig.serial.writes().is_empty());
}

#[tokio::test]
async fn a_failed_save_of_the_point_of_no_return_restarts_the_panel_unwritten() {
    let rig = Rig::new();
    let runs = tempfile::tempdir().unwrap();
    let dir = runs.path().join("runs");
    let mut r = rig.runner().with_autosave(dir.clone());
    // Up to the flash step's first call, with saves working.
    let mut input = None;
    loop {
        match r.advance(input.take()).await {
            Event::NeedInput(_) => input = Some(Input::Next),
            Event::Progress { .. } if r.state().step_index == 4 => break,
            Event::Progress { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    // The disk refuses the next save: where the run directory was is now a file.
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();
    let (_, reason, _) = failure(&r.advance(None).await);
    assert!(reason.contains("could not save"), "{reason}");
    assert!(rig.serial.writes().is_empty());
    assert_eq!(rig.serial.erases(), 0);
    assert_eq!(rig.serial.open_sessions(), 0, "the port was left open");
    assert_eq!(
        rig.serial.abandoned(),
        0,
        "the panel was left in its bootloader"
    );
}

fn published() -> Manifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../manifest/manifest.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn published_runner(rig: &Rig) -> Runner {
    let m = published();
    let Identified::Recipe { recipe_id, .. } = identify(&m, &usb_facts()) else {
        panic!("the K-Touch did not identify on the published list");
    };
    let state = RunState::new("run", "ktouch", &recipe_id, Host::new(PORT), usb_facts());
    Runner::new(rig.env(), Arc::new(m), state).with_cache_dir(rig.cache.path().to_path_buf())
}

#[tokio::test]
async fn the_published_list_updates_a_k_touch_that_runs_helixscreen() {
    let rig = Rig::new();
    rig.running("v1.1.0-beta.9");
    let mut r = published_runner(&rig);
    let t = drive(&mut r).await;
    assert!(matches!(t.end, Event::Done), "{:?}", t.end);
    assert_eq!(r.state().recipe_id, "ktouch_update");
    assert_eq!(rig.serial.erases(), 0);
    assert_eq!(rig.written_offsets(), PART_OFFSETS.to_vec());
}

#[tokio::test]
async fn the_published_list_refuses_a_k_touch_on_btt_firmware_without_writing() {
    let rig = Rig::new();
    let mut r = published_runner(&rig);
    let t = drive(&mut r).await;
    match &t.end {
        Event::Refused { reason, doc, .. } => {
            assert!(reason.contains("not installed"), "{reason}");
            assert_eq!(doc, DOC);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(rig.serial.writes().is_empty());
    assert_eq!(rig.serial.erases(), 0);
    assert!(!r.state().committed);
}
