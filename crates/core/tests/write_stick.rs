// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::cache::cached_path;
use helix_installer_core::facts::Facts;
use helix_installer_core::manifest::Manifest;
use helix_installer_core::run_state::RunState;
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::transport::drives_sys::{is_system, SysDrives};
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
};
use helix_installer_core::transport::{Drive, Drives, Env, Host};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FW: &[u8] = b"stock firmware image bytes";
const EXTRA: &[u8] = b"second file";
const FW_URL: &str = "https://example.test/fw.bin";
const EXTRA_URL: &str = "https://example.test/extra.cfg";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn drive(id: &str, fs: &str, free: u64, removable: bool, system: bool) -> Drive {
    Drive {
        id: id.into(),
        label: id.to_uppercase(),
        mount: PathBuf::from(format!("/media/{id}")),
        fs: fs.into(),
        size_bytes: 8_000_000_000,
        free_bytes: free,
        removable,
        system,
    }
}

fn stick(id: &str, fs: &str) -> Drive {
    drive(id, fs, 1_000_000_000, true, false)
}

struct Rig {
    drives: Arc<FakeDrives>,
    fetch: Arc<FakeFetch>,
    cache: tempfile::TempDir,
}

impl Rig {
    fn new() -> Rig {
        let fetch = Arc::new(FakeFetch::new());
        fetch.add(FW_URL, FW.to_vec());
        fetch.add(EXTRA_URL, EXTRA.to_vec());
        Rig {
            drives: Arc::new(FakeDrives::new()),
            fetch,
            cache: tempfile::tempdir().unwrap(),
        }
    }

    fn env(&self) -> Env {
        Env {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh: Arc::new(FakeSsh::new()),
            drives: self.drives.clone(),
            fetch: self.fetch.clone(),
            clock: Arc::new(FakeClock::new()),
            net: Arc::new(FakeNet::new()),
        }
    }

    /// A recipe that downloads both artifacts and then runs `write_stick` as given.
    fn runner(&self, write_stick: Value) -> Runner {
        let m: Manifest = serde_json::from_value(json!({
            "manifest_version": 1,
            "min_app_version": "0.1.0",
            "printers": [{
                "id": "p",
                "name": "Printer",
                "identify": [{ "model": "P" }],
                "recipes": [{ "when": { "state": "stock" }, "recipe": "r" }],
                "restore": "r",
                "fallback": { "refuse": true, "doc": "https://helixscreen.org/docs/install-p" }
            }],
            "artifacts": {
                "fw": { "url": FW_URL, "sha256": sha(FW), "filename": "firmware.bin" },
                "extra": { "url": EXTRA_URL, "sha256": sha(EXTRA) }
            },
            "recipes": { "r": [
                { "type": "download", "artifact": "fw" },
                { "type": "download", "artifact": "extra" },
                write_stick
            ] }
        }))
        .unwrap();
        let state = RunState::new("run1", "p", "r", Host::new("192.168.1.50"), Facts::new());
        Runner::new(self.env(), Arc::new(m), state).with_cache_dir(self.cache.path().to_path_buf())
    }

    fn file(&self, id: &str, name: &str) -> Option<Vec<u8>> {
        self.drives.file(id, name)
    }
}

/// Advances past every `Progress` event, answering with `input` first.
async fn settle(r: &mut Runner, input: Option<Input>) -> Event {
    let mut ev = r.advance(input).await;
    while let Event::Progress { .. } = ev {
        ev = r.advance(None).await;
    }
    ev
}

fn both() -> Value {
    json!({ "type": "write_stick", "artifacts": ["fw", "extra"] })
}

fn offered(ev: &Event) -> (Vec<String>, u64) {
    match ev {
        Event::NeedInput(Prompt::ChooseDrive {
            drives,
            needed_bytes,
        }) => (drives.iter().map(|d| d.id.clone()).collect(), *needed_bytes),
        e => panic!("expected ChooseDrive, got {e:?}"),
    }
}

fn help_of(ev: &Event) -> String {
    match ev {
        Event::Failed { help, .. } => help.clone().unwrap_or_default(),
        e => panic!("expected Failed, got {e:?}"),
    }
}

const TOTAL: u64 = (FW.len() + EXTRA.len()) as u64;

#[tokio::test]
async fn only_removable_non_system_drives_are_offered() {
    let rig = Rig::new();
    rig.drives
        .add(drive("internal", "vfat", 1 << 40, false, false));
    rig.drives
        .add(drive("bootstick", "vfat", 1 << 30, true, true));
    rig.drives
        .add(drive("fixedsys", "vfat", 1 << 30, false, true));
    rig.drives.add(stick("good", "vfat"));
    let mut r = rig.runner(both());
    let ev = settle(&mut r, None).await;
    assert_eq!(offered(&ev), (vec!["good".to_string()], TOTAL));
}

#[tokio::test]
async fn choosing_a_drive_that_was_not_offered_asks_again_and_writes_nothing() {
    let rig = Rig::new();
    rig.drives
        .add(drive("internal", "vfat", 1 << 40, false, false));
    rig.drives
        .add(drive("bootstick", "vfat", 1 << 30, true, true));
    rig.drives.add(stick("good", "vfat"));
    let mut r = rig.runner(both());
    settle(&mut r, None).await;
    for id in ["internal", "bootstick", "nonexistent"] {
        let ev = settle(&mut r, Some(Input::Drive(id.into()))).await;
        assert_eq!(offered(&ev).0, vec!["good".to_string()], "{id}");
        assert_eq!(rig.file(id, "firmware.bin"), None, "{id}");
    }
    assert!(rig.drives.ejected().is_empty());
}

#[tokio::test]
async fn exfat_and_ntfs_sticks_refuse_with_the_reformat_help() {
    for fs in ["exfat", "exFAT", "ntfs", "NTFS", "FAT", "ext4"] {
        let rig = Rig::new();
        rig.drives.add(stick("s", fs));
        let mut r = rig.runner(both());
        settle(&mut r, None).await;
        let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
        let help = help_of(&ev);
        assert!(help.contains("reformat as FAT32"), "{fs}: {help}");
        assert_eq!(rig.file("s", "firmware.bin"), None, "{fs}");
    }
}

#[tokio::test]
async fn every_fat32_spelling_is_accepted() {
    for fs in ["vfat", "msdos", "FAT32", "fat32"] {
        let rig = Rig::new();
        rig.drives.add(stick("s", fs));
        let mut r = rig.runner(both());
        settle(&mut r, None).await;
        let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
        assert!(matches!(ev, Event::Done), "{fs}: {ev:?}");
        assert_eq!(rig.file("s", "firmware.bin").as_deref(), Some(FW), "{fs}");
    }
}

#[tokio::test]
async fn a_stick_one_byte_too_small_for_all_artifacts_refuses_before_copying() {
    let rig = Rig::new();
    rig.drives.add(drive("s", "vfat", TOTAL - 1, true, false));
    let mut r = rig.runner(both());
    settle(&mut r, None).await;
    let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
    assert!(help_of(&ev).contains("space"), "{ev:?}");
    // The first artifact alone would fit; nothing may be copied when all of them do not.
    assert_eq!(rig.file("s", "firmware.bin"), None);
    assert_eq!(rig.file("s", "extra"), None);
}

#[tokio::test]
async fn a_stick_with_exactly_enough_space_is_written() {
    let rig = Rig::new();
    rig.drives.add(drive("s", "vfat", TOTAL, true, false));
    let mut r = rig.runner(both());
    settle(&mut r, None).await;
    let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
}

#[tokio::test]
async fn files_are_written_under_their_names_and_the_stick_is_ejected() {
    let rig = Rig::new();
    rig.drives.add(stick("s", "vfat"));
    let mut r = rig.runner(both());
    settle(&mut r, None).await;
    let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(rig.file("s", "firmware.bin").as_deref(), Some(FW));
    // An artifact without a filename is written under its id.
    assert_eq!(rig.file("s", "extra").as_deref(), Some(EXTRA));
    assert_eq!(rig.drives.ejected(), vec!["s".to_string()]);
}

#[tokio::test]
async fn a_copy_that_does_not_hash_to_the_manifest_value_fails_and_is_not_ejected() {
    let rig = Rig::new();
    rig.drives.add(stick("s", "vfat"));
    rig.drives.corrupt_copies();
    let mut r = rig.runner(both());
    settle(&mut r, None).await;
    let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
    match &ev {
        Event::Failed { reason, help, .. } => {
            assert!(reason.contains("does not match"), "{reason}");
            assert!(
                help.as_deref().unwrap_or("").contains("firmware.bin"),
                "{help:?}"
            );
        }
        e => panic!("expected Failed, got {e:?}"),
    }
    assert!(rig.drives.ejected().is_empty());
}

#[tokio::test]
async fn the_pro_rename_applies() {
    let rig = Rig::new();
    rig.drives.add(stick("s", "vfat"));
    let mut r = rig.runner(json!({
        "type": "write_stick",
        "artifacts": ["fw", "extra"],
        "rename": { "fw": "Pro_firmware.bin" }
    }));
    settle(&mut r, None).await;
    let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(rig.file("s", "Pro_firmware.bin").as_deref(), Some(FW));
    assert_eq!(rig.file("s", "firmware.bin"), None);
    assert_eq!(rig.file("s", "extra").as_deref(), Some(EXTRA));
}

#[tokio::test]
async fn a_name_that_is_not_a_plain_file_name_is_refused() {
    for name in [
        "../evil.bin",
        "dir/fw.bin",
        "dir\\fw.bin",
        "C:fw.bin",
        "..",
        "",
    ] {
        let rig = Rig::new();
        rig.drives.add(stick("s", "vfat"));
        let mut r = rig.runner(json!({
            "type": "write_stick",
            "artifacts": ["fw"],
            "rename": { "fw": name }
        }));
        let ev = settle(&mut r, None).await;
        assert!(matches!(ev, Event::Failed { .. }), "{name:?}: {ev:?}");
        assert_eq!(rig.file("s", name), None, "{name:?}");
    }
}

#[tokio::test]
async fn a_cached_file_changed_since_its_download_is_never_written() {
    let rig = Rig::new();
    rig.drives.add(stick("s", "vfat"));
    let mut r = rig.runner(both());
    offered(&settle(&mut r, None).await);
    std::fs::write(cached_path(rig.cache.path(), &sha(FW)), b"tampered").unwrap();
    let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
    assert!(matches!(ev, Event::Failed { .. }), "{ev:?}");
    assert_eq!(rig.file("s", "firmware.bin"), None);
    assert_eq!(rig.file("s", "extra"), None);
}

#[tokio::test]
async fn an_eject_failure_after_a_verified_copy_is_a_warning_not_a_failure() {
    let rig = Rig::new();
    rig.drives.add(stick("s", "vfat"));
    rig.drives.fail_eject();
    let mut r = rig.runner(both());
    settle(&mut r, None).await;
    let ev = settle(&mut r, Some(Input::Drive("s".into()))).await;
    match &ev {
        Event::NeedInput(Prompt::Info { text, .. }) => {
            assert!(text.contains("eject it yourself"), "{text}")
        }
        e => panic!("expected the eject warning, got {e:?}"),
    }
    assert_eq!(rig.file("s", "firmware.bin").as_deref(), Some(FW));
    let ev = settle(&mut r, Some(Input::Next)).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
}

// ---------------------------------------------------------------- the real drive backend

#[test]
fn a_mount_that_is_or_contains_an_anchor_is_system() {
    let anchors = [
        PathBuf::from("/"),
        PathBuf::from("/home/me"),
        PathBuf::from("/opt/app"),
    ];
    let sys = |m: &str| is_system(Path::new(m), Some(&anchors));
    assert!(sys("/"));
    assert!(sys("/home"));
    assert!(sys("/home/me"));
    assert!(sys("/opt/app"));
    assert!(!sys("/media/me/STICK"));
    assert!(!sys("/home/me2"));
    assert!(!sys("/home/me/STICK"));
}

#[test]
fn unknown_anchors_make_every_drive_system() {
    assert!(is_system(Path::new("/media/me/STICK"), None));
}

#[tokio::test]
async fn the_real_copy_writes_through_a_part_file_and_hashes_what_landed() {
    let root = tempfile::tempdir().unwrap();
    let mount = root.path().join("stick");
    std::fs::create_dir(&mount).unwrap();
    let src = root.path().join("fw");
    std::fs::write(&src, FW).unwrap();
    let d = Drive {
        mount: mount.clone(),
        ..stick("s", "vfat")
    };
    let sys = SysDrives::new();
    sys.copy_to(&d, &src, "firmware.bin").await.unwrap();
    assert_eq!(std::fs::read(mount.join("firmware.bin")).unwrap(), FW);
    assert!(!mount.join("firmware.bin.part").exists());
    assert_eq!(sys.sha256_of(&d, "firmware.bin").await.unwrap(), sha(FW));

    // A failed copy leaves neither the file nor its .part behind.
    let missing = root.path().join("missing");
    assert!(sys.copy_to(&d, &missing, "other.bin").await.is_err());
    assert!(!mount.join("other.bin").exists());
    assert!(!mount.join("other.bin.part").exists());
    // A rename that fails after the .part is complete removes the .part too.
    std::fs::create_dir_all(mount.join("busy.bin").join("x")).unwrap();
    assert!(sys.copy_to(&d, &src, "busy.bin").await.is_err());
    assert!(!mount.join("busy.bin.part").exists());
    assert!(sys.copy_to(&d, &src, "../escape.bin").await.is_err());
    assert!(!root.path().join("escape.bin").exists());
}

#[test]
fn the_drive_holding_this_test_binary_is_never_offered() {
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    for d in SysDrives::new().list_drives().unwrap() {
        let holds_exe = d
            .mount
            .canonicalize()
            .map(|m| exe.starts_with(&m))
            .unwrap_or(false);
        if holds_exe {
            assert!(d.system, "{d:?}");
        }
    }
}
