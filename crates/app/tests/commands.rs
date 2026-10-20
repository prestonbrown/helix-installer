// SPDX-License-Identifier: GPL-3.0-or-later

use async_trait::async_trait;
use helix_installer_app_lib::commands::{
    AppConfig, AppCore, CandidateStatus, CommandError, Connection, Discovery, ErrorKind,
};
use helix_installer_core::app_log::AppLog;
use helix_installer_core::discovery::Candidate;
use helix_installer_core::facts::Facts;
use helix_installer_core::manifest::Login;
use helix_installer_core::runner::{Event, Input, Prompt};
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSerial, FakeSsh,
};
use helix_installer_core::transport::{Env, ExecOutput, Host};
use serde_json::{json, Value};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ADDR: &str = "192.168.1.50";
const OTHER: &str = "192.168.1.51";
const PASSWORD: &str = "typed-s3cret";
const K1_DOC: &str = "https://helixscreen.org/guide/install-k1/";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/fixtures")
        .join(name)
}

fn pubkey() -> String {
    std::fs::read_to_string(fixture("test_key.pub")).unwrap()
}

fn sign(bytes: &[u8]) -> String {
    let sk_text = std::fs::read_to_string(fixture("test_key.sec")).unwrap();
    let sk = minisign::SecretKeyBox::from_string(&sk_text)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, Cursor::new(bytes), None, None)
        .unwrap()
        .into_string()
}

fn manifest_json() -> Value {
    json!({
        "manifest_version": 1,
        "min_app_version": "0.1.0",
        "printers": [
            {
                "id": "k1",
                "name": "Creality K1",
                "photo": "img/k1.jpg",
                "identify": [{ "klipper_hostname": "k1-printer" }, { "model": "shared" }],
                "recipes": [
                    { "when": { "state": "modded:moonraker" }, "recipe": "install" },
                    { "when": { "state": "stock" }, "recipe": "stock" }
                ],
                "restore": "install",
                "fallback": { "refuse": true, "doc": K1_DOC },
                "verified": { "by": "helixscreen", "date": "2026-10-01", "firmware": "1.3.3.46" }
            },
            {
                "id": "k2",
                "name": "Creality K2",
                "identify": [{ "model": "shared" }],
                "recipes": [{ "when": { "state": "stock" }, "recipe": "stock" }],
                "restore": "stock",
                "fallback": { "refuse": true, "doc": "https://helixscreen.org/guide/install-k2/" }
            },
            {
                "id": "pi",
                "name": "Raspberry Pi",
                "identify": [{ "klipper_hostname": "pi-box" }],
                "recipes": [{ "when": { "state": "modded:moonraker" }, "recipe": "shell" }],
                "restore": "shell",
                "fallback": { "refuse": true, "doc": "https://helixscreen.org/guide/install-generic/" }
            }
        ],
        "artifacts": {},
        "recipes": {
            "install": [
                { "type": "info", "text": "Welcome", "image": "img/welcome.png" },
                { "type": "ask", "fact": "screen", "question": "What does the screen show?",
                  "choices": [{ "value": "home", "label": "Home", "image": "img/home.png" },
                              { "value": "error", "label": "An error" }] },
                { "type": "ssh_login", "logins": [{ "user": "root", "password": "wrong-default" }],
                  "ask_if_failed": true },
                { "type": "moonraker_gcode", "script": "DISPLAY_OFF HELIX=1", "commits": true }
            ],
            "shell": [
                { "type": "ssh_login", "logins": [{ "user": "root", "password": PASSWORD }],
                  "ask_if_failed": true },
                { "type": "ssh_run", "command": "sh /tmp/setup.sh", "timeout_s": 60, "commits": true }
            ],
            "stock": [
                { "type": "info", "text": "Changing the printer", "commits": true },
                { "type": "require", "fact": "stock_version", "range": ">=2",
                  "doc": "https://helixscreen.org/guide/supported-printers/" }
            ]
        }
    })
}

/// Writes the signed manifest into `dir` and returns its path.
fn write_manifest(dir: &Path, tamper: bool) -> String {
    let bytes = serde_json::to_vec(&manifest_json()).unwrap();
    let sig = sign(&bytes);
    let mut on_disk = bytes.clone();
    if tamper {
        on_disk[10] ^= 0x01;
    }
    let path = dir.join("manifest.json");
    std::fs::write(&path, on_disk).unwrap();
    std::fs::write(dir.join("manifest.json.minisig"), sig).unwrap();
    path.to_string_lossy().into_owned()
}

fn facts(v: Value) -> Facts {
    serde_json::from_value(v).unwrap()
}

struct Fixed(Vec<Candidate>);

#[async_trait]
impl Discovery for Fixed {
    async fn discover(&self, _env: &Env) -> Vec<Candidate> {
        self.0.clone()
    }
}

fn found() -> Vec<Candidate> {
    vec![
        Candidate::new(
            Host::new(ADDR),
            facts(
                json!({ "has_moonraker": true, "klipper_hostname": "k1-printer", "serial": "SN42" }),
            ),
        ),
        Candidate::new(Host::new(OTHER), facts(json!({ "model": "shared" }))),
        Candidate::new(
            Host::new("192.168.1.52"),
            facts(json!({ "model": "toaster" })),
        ),
    ]
}

struct Fakes {
    moonraker: Arc<FakeMoonraker>,
    ssh: FakeSsh,
}

impl Fakes {
    fn new() -> Self {
        Fakes {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh: FakeSsh::new(),
        }
    }
    fn env(&self) -> Env {
        Env {
            moonraker: self.moonraker.clone(),
            ssh: Arc::new(self.ssh.clone()),
            drives: Arc::new(FakeDrives::new()),
            fetch: Arc::new(FakeFetch::new()),
            clock: Arc::new(FakeClock::new()),
            net: Arc::new(FakeNet::new()),
            serial: Arc::new(FakeSerial::new()),
        }
    }
}

fn core(f: &Fakes, data: &Path, manifest: Option<String>) -> AppCore {
    AppCore::new(config(f, data, manifest))
}

fn config(f: &Fakes, data: &Path, manifest: Option<String>) -> AppConfig {
    AppConfig {
        data_dir: data.to_path_buf(),
        env: f.env(),
        discovery: Arc::new(Fixed(found())),
        pubkey: pubkey(),
        manifest_location: manifest,
        channel_override: None,
        app_version: "0.1.0".into(),
        log: None,
    }
}

fn core_with_log(f: &Fakes, data: &Path, manifest: Option<String>, log: Arc<AppLog>) -> AppCore {
    AppCore::new(AppConfig {
        log: Some(log),
        ..config(f, data, manifest)
    })
}

fn kind<T: std::fmt::Debug>(r: Result<T, CommandError>) -> ErrorKind {
    r.expect_err("expected an error").kind
}

/// Advances past every `Progress` event.
async fn settle(c: &AppCore, run: &str, input: Option<Input>) -> Event {
    let mut ev = c.advance(run, input).await.unwrap().event;
    for _ in 0..100 {
        match ev {
            Event::Progress { .. } => ev = c.advance(run, None).await.unwrap().event,
            other => return other,
        }
    }
    panic!("the run kept reporting progress");
}

#[tokio::test]
async fn without_a_verified_manifest_every_printer_command_says_so() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), None);
    assert_eq!(kind(c.printers().await), ErrorKind::NoPrinterList);
    assert_eq!(kind(c.discover().await), ErrorKind::NoPrinterList);
    assert_eq!(
        kind(c.add_manual_host(ADDR).await),
        ErrorKind::NoPrinterList
    );

    let src = tempfile::tempdir().unwrap();
    let tampered = core(&f, data.path(), Some(write_manifest(src.path(), true)));
    let err = tampered
        .printers()
        .await
        .expect_err("a tampered manifest loaded");
    assert_eq!(err.kind, ErrorKind::NoPrinterList);
    assert!(err.message.contains("signature"), "{}", err.message);
}

#[tokio::test]
async fn a_verified_manifest_is_cached_for_the_next_launch() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), Some(write_manifest(src.path(), false)));
    let printers = c.printers().await.unwrap();
    assert_eq!(
        printers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["k1", "k2", "pi"]
    );
    assert_eq!(printers[0].photo.as_deref(), Some("img/k1.jpg"));
    assert_eq!(printers[0].verified.as_ref().unwrap().firmware, "1.3.3.46");

    let offline = core(&f, data.path(), None);
    assert_eq!(offline.printers().await.unwrap().len(), 3);
}

#[tokio::test]
async fn discover_names_each_printer_and_what_can_be_done_with_it() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), Some(write_manifest(src.path(), false)));
    let found = c.discover().await.unwrap();
    assert_eq!(found.len(), 3);

    assert_eq!(found[0].key, "SN42");
    assert_eq!(found[0].address, ADDR);
    assert_eq!(found[0].name, "Creality K1");
    assert_eq!(found[0].status, CandidateStatus::Ready);
    assert_eq!(found[0].printer_id.as_deref(), Some("k1"));
    assert_eq!(found[0].host_name.as_deref(), Some("k1-printer"));
    assert_eq!(found[1].host_name, None);

    assert_eq!(found[1].status, CandidateStatus::Choose);
    assert_eq!(
        found[1]
            .choices
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        ["k1", "k2"]
    );

    assert_eq!(found[2].status, CandidateStatus::Unsupported);
    assert_eq!(found[2].name, "toaster");
    assert!(found[2].doc.is_some());
}

#[tokio::test]
async fn start_run_refuses_what_it_cannot_start() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), Some(write_manifest(src.path(), false)));
    assert_eq!(
        kind(c.start_run("nobody", None).await),
        ErrorKind::UnknownPrinter
    );
    c.discover().await.unwrap();
    assert_eq!(kind(c.start_run(OTHER, None).await), ErrorKind::ChooseModel);
    let err = c.start_run("192.168.1.52", None).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(err.doc.is_some());
    // The printer answers as a K1, so picking a K2 for it is refused.
    let err = c.start_run("SN42", Some("k2")).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert_eq!(
        kind(c.advance("no-such-run", None).await),
        ErrorKind::UnknownRun
    );
    assert_eq!(
        kind(c.export_bundle("no-such-run", true).await),
        ErrorKind::UnknownRun
    );
}

#[tokio::test]
async fn a_typed_address_is_probed_and_can_be_started_as_a_picked_model() {
    let f = Fakes::new();
    f.moonraker.set_printer_info(
        "10.0.0.9",
        json!({ "hostname": "k1-printer", "state": "ready" }),
    );
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), Some(write_manifest(src.path(), false)));

    assert_eq!(
        kind(c.add_manual_host("printer.local!").await),
        ErrorKind::NotAnAddress
    );

    let answering = c.add_manual_host(" 10.0.0.9 ").await.unwrap();
    assert!(answering.answered);
    assert_eq!(answering.status, CandidateStatus::Ready);
    assert_eq!(answering.name, "Creality K1");

    let silent = c.add_manual_host("10.0.0.10").await.unwrap();
    assert!(!silent.answered);
    assert_eq!(silent.status, CandidateStatus::NoAnswer);
    assert_eq!(
        kind(c.start_run(&silent.key, None).await),
        ErrorKind::ChooseModel
    );
    let run = c.start_run(&silent.key, Some("k1")).await.unwrap();
    let view = c.advance(&run, None).await.unwrap();
    assert_eq!(view.run.printer_name, "Creality K1");
    assert!(matches!(view.event, Event::NeedInput(Prompt::Info { .. })));
}

#[tokio::test]
async fn a_full_run_through_the_commands_reaches_done_and_keeps_the_password_out() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", PASSWORD);
    f.ssh.on_command(
        "id -u",
        ExecOutput {
            stdout: "0\n".into(),
            stderr: String::new(),
            exit: 0,
        },
    );
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), Some(write_manifest(src.path(), false)));
    c.discover().await.unwrap();
    let run = c.start_run("SN42", None).await.unwrap();

    let view = c.advance(&run, None).await.unwrap();
    assert_eq!(view.run.step, 0);
    assert_eq!(view.run.total_steps, 4);
    assert_eq!(view.run.step_kind.as_deref(), Some("info"));
    assert!(!view.run.committed);
    assert_eq!(view.run.guide, K1_DOC);
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(wire["event"]["event"], "need_input");
    assert_eq!(wire["event"]["prompt"], "info");
    assert_eq!(wire["event"]["image"], "img/welcome.png");

    match settle(&c, &run, Some(Input::Next)).await {
        Event::NeedInput(Prompt::Ask { choices, .. }) => assert_eq!(choices.len(), 2),
        other => panic!("expected the question, got {other:?}"),
    }
    let login = |password: &str| Input::Login(Login::new("root", password));
    assert!(matches!(
        settle(&c, &run, Some(Input::Choice("home".into()))).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    assert!(matches!(
        settle(&c, &run, Some(login("not-it"))).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    assert!(matches!(
        settle(&c, &run, Some(login(PASSWORD))).await,
        Event::Done
    ));
    assert_eq!(
        f.moonraker.gcodes(),
        vec![(ADDR.to_string(), "DISPLAY_OFF HELIX=1".to_string())]
    );

    let bundle = c.export_bundle(&run, false).await.unwrap();
    assert!(
        bundle.contains("DISPLAY_OFF") || bundle.contains("moonraker_gcode"),
        "{bundle}"
    );
    assert!(!bundle.contains(PASSWORD), "{bundle}");
    assert!(bundle.contains(ADDR), "{bundle}");
    let masked = c.export_bundle(&run, true).await.unwrap();
    assert!(!masked.contains(ADDR), "{masked}");
    assert!(!masked.contains("SN42"), "{masked}");
    assert!(c.unfinished_runs().await.is_empty());
}

#[tokio::test]
async fn a_committed_failure_is_listed_and_resumes_in_a_new_app() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let manifest = write_manifest(src.path(), false);
    let c = core(&f, data.path(), Some(manifest.clone()));
    let silent = c.add_manual_host("10.0.0.10").await.unwrap();
    let run = c.start_run(&silent.key, Some("k1")).await.unwrap();
    c.advance(&run, None).await.unwrap();
    let view = c.advance(&run, Some(Input::Next)).await.unwrap();
    match &view.event {
        Event::Failed {
            committed: true,
            step: 1,
            help: Some(help),
            ..
        } => assert_eq!(help, "https://helixscreen.org/guide/supported-printers/"),
        other => panic!("expected a committed failure, got {other:?}"),
    }
    assert!(view.run.committed);
    assert_eq!(view.run.step_kind.as_deref(), Some("require"));

    let listed = c.unfinished_runs().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].run_id, run);
    assert_eq!(listed[0].printer_name, "Creality K1");
    assert_eq!(listed[0].step, 1);
    assert_eq!(listed[0].total_steps, Some(2));
    assert!(listed[0].committed);
    assert!(listed[0].reason.is_some());

    let reopened = core(&f, data.path(), Some(manifest));
    assert!(matches!(
        reopened.advance(&run, None).await.unwrap().event,
        Event::Failed {
            committed: true,
            ..
        }
    ));
    let bundle = reopened.export_bundle(&run, true).await.unwrap();
    assert!(bundle.contains("passed the point of no return"), "{bundle}");
}

#[tokio::test]
async fn record_mode_masks_the_serial_and_unknown_keys_are_errors() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), Some(write_manifest(src.path(), false)));
    c.discover().await.unwrap();
    let json = c.record_printer("SN42").await.unwrap();
    assert!(!json.contains("SN42"), "{json}");
    assert!(json.contains("k1-printer"), "{json}");
    assert_eq!(
        kind(c.record_printer("nobody").await),
        ErrorKind::UnknownPrinter
    );
}

#[test]
fn errors_cross_to_the_ui_as_a_kind_and_a_message() {
    let e = CommandError::new(ErrorKind::UnknownRun, "gone");
    assert_eq!(
        serde_json::to_value(&e).unwrap(),
        json!({ "kind": "unknown_run", "message": "gone", "doc": null })
    );
}

#[test]
fn pictures_are_relative_to_the_img_directory_beside_the_printer_list() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let remote = core(
        &f,
        data.path(),
        Some("https://helixscreen.org/installer/v1/manifest.json".into()),
    );
    assert_eq!(
        remote.app_info().image_base.as_deref(),
        Some("https://helixscreen.org/installer/v1/img/")
    );
    assert_eq!(remote.app_info().version, "0.1.0");
    let local = core(&f, data.path(), Some("/tmp/manifest.json".into()));
    assert_eq!(local.app_info().image_base, None);
}

#[tokio::test]
async fn the_support_bundle_carries_what_the_printer_printed() {
    let f = Fakes::new();
    f.ssh.accept("10.0.0.9", "root", PASSWORD);
    f.ssh.on_command(
        "id -u",
        ExecOutput {
            stdout: "0\n".into(),
            stderr: String::new(),
            exit: 0,
        },
    );
    f.moonraker.set_printer_info(
        "10.0.0.9",
        json!({ "hostname": "pi-box", "state": "ready" }),
    );
    let echoed = format!("setup: logging in with {PASSWORD}\n");
    f.ssh.script_job(vec![
        ("setup: unpacking the screen\n", None),
        (&echoed, None),
        ("setup: no space left on /home/pi\n", Some(1)),
    ]);
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), Some(write_manifest(src.path(), false)));
    let host = c.add_manual_host("10.0.0.9").await.unwrap();
    let run = c.start_run(&host.key, None).await.unwrap();
    let ev = settle(&c, &run, None).await;
    assert!(
        matches!(
            ev,
            Event::Failed {
                committed: true,
                ..
            }
        ),
        "{ev:?}"
    );
    let bundle = c.export_bundle(&run, false).await.unwrap();
    let (_, printer_side) = bundle
        .split_once("printer-side job log:\n")
        .expect("the bundle has a job log section");
    assert!(
        printer_side.contains("setup: unpacking the screen"),
        "{bundle}"
    );
    assert!(
        printer_side.contains("setup: no space left on /home/pi"),
        "{bundle}"
    );
    assert!(!bundle.contains(PASSWORD), "{bundle}");
}

/// A K1 run started on a printer that answers nothing, stopped at its first prompt: nothing
/// on the printer has changed.
async fn run_before_commit(c: &AppCore) -> String {
    let silent = c.add_manual_host("10.0.0.10").await.unwrap();
    let run = c.start_run(&silent.key, Some("k1")).await.unwrap();
    c.advance(&run, None).await.unwrap();
    run
}

#[tokio::test]
async fn a_run_that_changed_nothing_can_be_dismissed_for_good() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let manifest = write_manifest(src.path(), false);
    let c = core(&f, data.path(), Some(manifest.clone()));
    let run = run_before_commit(&c).await;
    assert_eq!(c.unfinished_runs().await.len(), 1);

    c.dismiss_run(&run).await.unwrap();
    assert!(c.unfinished_runs().await.is_empty());
    assert!(matches!(
        c.advance(&run, None).await.unwrap().event,
        Event::Refused {
            cancelled: true,
            ..
        }
    ));
    let reopened = core(&f, data.path(), Some(manifest));
    assert!(reopened.unfinished_runs().await.is_empty());
    assert_eq!(
        kind(c.dismiss_run("no-such-run").await),
        ErrorKind::UnknownRun
    );
}

#[tokio::test]
async fn a_run_that_changed_the_printer_cannot_be_dismissed() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let manifest = write_manifest(src.path(), false);
    let c = core(&f, data.path(), Some(manifest.clone()));
    let run = run_before_commit(&c).await;
    c.advance(&run, Some(Input::Next)).await.unwrap();

    assert_eq!(kind(c.dismiss_run(&run).await), ErrorKind::RunCommitted);
    // Not even from an app that has not opened the run yet.
    let reopened = core(&f, data.path(), Some(manifest));
    assert_eq!(
        kind(reopened.dismiss_run(&run).await),
        ErrorKind::RunCommitted
    );
    let listed = reopened.unfinished_runs().await;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].committed);
}

#[tokio::test]
async fn a_k_touch_on_usb_is_listed_as_connected_by_usb() {
    let mut m = manifest_json();
    m["printers"].as_array_mut().unwrap().push(json!({
        "id": "ktouch",
        "name": "BTT K-Touch",
        "identify": [{ "usb": "1a86:7522" }],
        "recipes": [{ "when": { "state": "stock" }, "recipe": "stock" }],
        "restore": "stock",
        "fallback": { "refuse": true, "doc": "https://helixscreen.org/1.1/guide/install-esp32/" }
    }));
    let src = tempfile::tempdir().unwrap();
    let bytes = serde_json::to_vec(&m).unwrap();
    let path = src.path().join("manifest.json");
    std::fs::write(&path, &bytes).unwrap();
    std::fs::write(src.path().join("manifest.json.minisig"), sign(&bytes)).unwrap();
    let mut usb = found();
    usb.push(Candidate::new(
        Host::new("/dev/ttyUSB0"),
        facts(json!({ "usb": "1a86:7522" })),
    ));
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let c = AppCore::new(AppConfig {
        data_dir: data.path().to_path_buf(),
        env: f.env(),
        discovery: Arc::new(Fixed(usb)),
        pubkey: pubkey(),
        manifest_location: Some(path.to_string_lossy().into_owned()),
        channel_override: None,
        app_version: "0.1.0".into(),
        log: None,
    });
    let views = c.discover().await.unwrap();
    let panel = views.iter().find(|v| v.address == "/dev/ttyUSB0").unwrap();
    assert_eq!(panel.connection, Connection::Usb);
    assert_eq!(panel.name, "BTT K-Touch");
    assert_eq!(panel.status, CandidateStatus::Ready);
    assert_eq!(
        serde_json::to_value(panel).unwrap()["connection"],
        json!("usb")
    );
    assert!(views
        .iter()
        .filter(|v| v.address != "/dev/ttyUSB0")
        .all(|v| v.connection == Connection::Network));
}

#[tokio::test]
async fn the_app_log_follows_a_run_with_timestamps_and_never_the_password() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", PASSWORD);
    f.ssh.on_command(
        "id -u",
        ExecOutput {
            stdout: "0\n".into(),
            stderr: String::new(),
            exit: 0,
        },
    );
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let log = Arc::new(AppLog::open(logs.path(), std::time::SystemTime::now()).unwrap());
    let c = core_with_log(
        &f,
        data.path(),
        Some(write_manifest(src.path(), false)),
        log.clone(),
    );
    c.discover().await.unwrap();
    let run = c.start_run("SN42", None).await.unwrap();
    settle(&c, &run, None).await;
    settle(&c, &run, Some(Input::Next)).await;
    settle(&c, &run, Some(Input::Choice("home".into()))).await;
    let login = |password: &str| Input::Login(Login::new("root", password));
    settle(&c, &run, Some(login("not-it"))).await;
    assert!(matches!(
        settle(&c, &run, Some(login(PASSWORD))).await,
        Event::Done
    ));

    let text = log.contents();
    assert!(!text.contains(PASSWORD), "{text}");
    assert!(!text.contains("not-it"), "{text}");
    for line in text.lines() {
        let (stamp, _) = line.split_once(' ').unwrap_or_default();
        assert!(
            stamp.len() == 24 && stamp.ends_with('Z') && stamp.as_bytes()[10] == b'T',
            "an untimed line: {line:?}"
        );
    }
    assert!(text.contains(ADDR), "discovery is logged: {text}");
    assert!(text.contains(&run), "the run is logged: {text}");
    assert!(
        text.contains("moonraker_gcode"),
        "its steps are logged: {text}"
    );
    assert!(text.lines().last().unwrap().contains("done"), "{text}");

    let bundle = c.export_bundle(&run, true).await.unwrap();
    let (_, section) = bundle
        .split_once("installer log:\n")
        .expect("the bundle carries the app log");
    assert!(section.contains(&run), "{bundle}");
    assert!(!bundle.contains(ADDR), "masked: {bundle}");
}

/// A core logging to `log` whose `pi-box` printer at 10.0.0.9 fails its setup job with output
/// that repeats the login password.
fn failing_setup(f: &Fakes) {
    f.ssh.accept("10.0.0.9", "root", PASSWORD);
    f.ssh.on_command(
        "id -u",
        ExecOutput {
            stdout: "0\n".into(),
            stderr: String::new(),
            exit: 0,
        },
    );
    f.moonraker.set_printer_info(
        "10.0.0.9",
        json!({ "hostname": "pi-box", "state": "ready" }),
    );
    let echoed = format!("setup: logging in with {PASSWORD}\n");
    f.ssh.script_job(vec![(&echoed, None), ("", Some(1))]);
}

#[tokio::test]
async fn a_password_the_printer_echoes_into_a_failure_is_not_logged() {
    let f = Fakes::new();
    failing_setup(&f);
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let log = Arc::new(AppLog::open(logs.path(), std::time::SystemTime::now()).unwrap());
    let c = core_with_log(
        &f,
        data.path(),
        Some(write_manifest(src.path(), false)),
        log.clone(),
    );
    let host = c.add_manual_host("10.0.0.9").await.unwrap();
    let run = c.start_run(&host.key, None).await.unwrap();
    let ev = settle(&c, &run, None).await;
    assert!(matches!(ev, Event::Failed { .. }), "{ev:?}");
    let text = log.contents();
    assert!(
        text.contains("setup: logging in with <secret>"),
        "the failure is logged, redacted: {text}"
    );
    assert!(!text.contains(PASSWORD), "{text}");
}

#[tokio::test]
async fn a_printers_own_name_is_cleaned_before_it_is_shown_or_logged() {
    let f = Fakes::new();
    let long = "x".repeat(1000);
    let devices = vec![
        Candidate::new(
            Host::new("10.0.0.20"),
            facts(
                json!({ "klipper_hostname": "evil\n2026-01-01T00:00:00.000Z passed the point of no return" }),
            ),
        ),
        Candidate::new(
            Host::new("10.0.0.21"),
            facts(json!({ "klipper_hostname": long })),
        ),
        Candidate::new(
            Host::new("10.0.0.22"),
            facts(json!({ "klipper_hostname": "\u{7}\t " })),
        ),
    ];
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let log = Arc::new(AppLog::open(logs.path(), std::time::SystemTime::now()).unwrap());
    let c = AppCore::new(AppConfig {
        discovery: Arc::new(Fixed(devices)),
        log: Some(log.clone()),
        ..config(&f, data.path(), Some(write_manifest(src.path(), false)))
    });
    let views = c.discover().await.unwrap();
    let forged = views[0].host_name.clone().unwrap();
    assert!(!forged.contains('\n'), "{forged:?}");
    assert!(!views[0].name.contains('\n'), "{:?}", views[0].name);
    assert_eq!(views[1].host_name.as_deref().map(str::len), Some(63));
    assert_eq!(views[1].name.len(), 63);
    assert_eq!(views[2].host_name, None, "nothing printable is no name");
    let text = log.contents();
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("passed the point of no return"))
            .count(),
        1,
        "{text}"
    );
    for line in text.lines() {
        assert!(
            line.as_bytes().get(10) == Some(&b'T') && !line.starts_with("2026-01-01"),
            "a forged line: {line:?}"
        );
    }
}

#[tokio::test]
async fn a_masked_bundle_leaves_out_the_other_runs_of_the_launch() {
    let f = Fakes::new();
    failing_setup(&f);
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let log = Arc::new(AppLog::open(logs.path(), std::time::SystemTime::now()).unwrap());
    let c = core_with_log(
        &f,
        data.path(),
        Some(write_manifest(src.path(), false)),
        log.clone(),
    );
    c.discover().await.unwrap();
    let other = c.start_run("SN42", None).await.unwrap();
    settle(&c, &other, None).await;
    let host = c.add_manual_host("10.0.0.9").await.unwrap();
    let run = c.start_run(&host.key, None).await.unwrap();
    settle(&c, &run, None).await;
    assert!(log.contents().contains("SN42"), "the launch log names it");

    let masked = c.export_bundle(&run, true).await.unwrap();
    assert!(!masked.contains("SN42"), "{masked}");
    assert!(!masked.contains(&other), "{masked}");
    assert!(!masked.contains("k1-printer"), "{masked}");
    assert!(
        !masked.contains("pi-box"),
        "its own host name is masked: {masked}"
    );
    let (_, section) = masked.split_once("installer log:\n").unwrap();
    assert!(section.contains(&run), "{masked}");
}

#[tokio::test]
async fn the_run_carries_the_printers_own_name() {
    let f = Fakes::new();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let c = core(&f, data.path(), Some(write_manifest(src.path(), false)));
    c.discover().await.unwrap();
    let run = c.start_run("SN42", None).await.unwrap();
    let view = c.advance(&run, None).await.unwrap();
    assert_eq!(view.run.host_name.as_deref(), Some("k1-printer"));
    assert_eq!(view.run.printer_name, "Creality K1");
}
