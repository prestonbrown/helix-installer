// SPDX-License-Identifier: GPL-3.0-or-later

use async_trait::async_trait;
use helix_installer_core::bundle::support_bundle;
use helix_installer_core::facts::Facts;
use helix_installer_core::manifest::{Channel, Login, Manifest, Secret, Step};
use helix_installer_core::run_state::{DoneKind, RunState};
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::steps::helix_install::{
    new_install_channel, releases_manifest_url, GITHUB_RELEASE_API, HELIX_INSTALL_SILENCE,
    INSTALLED_VERSION_PROBE,
};
use helix_installer_core::steps::progress::{parse_progress_line, ProgressEvent, StepStatus};
use helix_installer_core::steps::{StepCtx, StepHandler, StepOutcome};
use helix_installer_core::transport::fake::FakeSerial;
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
};
use helix_installer_core::transport::{Clock, Env, ExecOutput, Host, Ssh};
use helix_installer_core::version::ReleaseVersion;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const ADDR: &str = "192.168.1.50";
const PASSWORD: &str = "s3cret-pw";
const SUDO_PW: &str = "typed-sudo-pw";
const TAG: &str = "v1.0.5";
const TARBALL: &[u8] = b"helixscreen tarball bytes";
const INSTALL_SH: &[u8] =
    b"#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --progress) P=1 ;;\n  esac\n  shift\ndone\necho install\n";

// ---------------------------------------------------------------- progress parser

fn step(n: u32, total: u32, status: StepStatus, title: &str, detail: &str) -> ProgressEvent {
    ProgressEvent::Step {
        n,
        total,
        status,
        title: title.into(),
        detail: detail.into(),
    }
}

#[test]
fn every_progress_line_form_parses() {
    let cases = [
        (
            "HELIX-PROGRESS\tplatform\tpi32",
            ProgressEvent::Platform("pi32".into()),
        ),
        (
            "HELIX-PROGRESS\tstep\t1\t7\tstart\tChecking the printer\t",
            step(1, 7, StepStatus::Start, "Checking the printer", ""),
        ),
        (
            "HELIX-PROGRESS\tstep\t1\t7\tok\tChecking the printer\t512 MB free",
            step(1, 7, StepStatus::Ok, "Checking the printer", "512 MB free"),
        ),
        (
            "HELIX-PROGRESS\tstep\t2\t7\tskip\tStopping the old UI\t",
            step(2, 7, StepStatus::Skip, "Stopping the old UI", ""),
        ),
        (
            "HELIX-PROGRESS\tstep\t3\t7\tfail\tExtracting\tno space left",
            step(3, 7, StepStatus::Fail, "Extracting", "no space left"),
        ),
        (
            "HELIX-PROGRESS\tresult\tok",
            ProgressEvent::Result {
                ok: true,
                reason: None,
            },
        ),
        (
            "HELIX-PROGRESS\tresult\tfail\tno space left on /data",
            ProgressEvent::Result {
                ok: false,
                reason: Some("no space left on /data".into()),
            },
        ),
        // A log written through a terminal or by a CRLF shell keeps the carriage return.
        (
            "HELIX-PROGRESS\tresult\tfail\texit 3\r",
            ProgressEvent::Result {
                ok: false,
                reason: Some("exit 3".into()),
            },
        ),
    ];
    for (line, want) in cases {
        assert_eq!(parse_progress_line(line), Some(want), "{line:?}");
    }
}

#[test]
fn garbage_and_the_bare_word_are_not_progress() {
    let garbage = [
        "",
        "HELIX-PROGRESS",
        "HELIX-PROGRESS\t",
        // install.sh's own usage text names the word without a tab after it.
        "  --progress     Also print one machine-readable HELIX-PROGRESS line per event",
        "HELIX-PROGRESS platform pi",
        " HELIX-PROGRESS\tplatform\tpi",
        "XHELIX-PROGRESS\tplatform\tpi",
        "HELIX-PROGRESSplatform\tpi",
        "HELIX-PROGRESS\tplatform",
        "HELIX-PROGRESS\tplatform\t",
        "HELIX-PROGRESS\tplatform\tpi\textra",
        "HELIX-PROGRESS\tstep\t1\t7\tstart\tTitle",
        "HELIX-PROGRESS\tstep\t1\t7\tstart\tTitle\t\textra",
        "HELIX-PROGRESS\tstep\tone\t7\tstart\tTitle\t",
        "HELIX-PROGRESS\tstep\t1\t-7\tstart\tTitle\t",
        "HELIX-PROGRESS\tstep\t1\t7\tdone\tTitle\t",
        "HELIX-PROGRESS\tresult",
        "HELIX-PROGRESS\tresult\tmaybe",
        "HELIX-PROGRESS\tresult\tok\tbut also",
        "HELIX-PROGRESS\tresult\tfail\twhy\tmore",
        "HELIX-PROGRESS\tbanana\tpi",
        "[3/7] Extracting ok",
    ];
    for line in garbage {
        assert_eq!(parse_progress_line(line), None, "{line:?}");
    }
}

// ---------------------------------------------------------------- fixtures

struct Fakes {
    ssh: FakeSsh,
    fetch: Arc<FakeFetch>,
    clock: Arc<FakeClock>,
    cache: tempfile::TempDir,
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn tarball_url(platform: &str) -> String {
    format!("https://releases.helixscreen.org/releases/{TAG}/helixscreen-{platform}-{TAG}.tar.gz")
}

const INSTALL_SH_URL: &str =
    "https://github.com/prestonbrown/helixscreen/releases/download/v1.0.5/install.sh";

impl Fakes {
    /// A releases manifest with `platforms`, each serving `TARBALL`, and a GitHub release
    /// whose `install.sh` asset carries `digest`.
    fn new(platforms: &[&str], install_sh_asset: Value) -> Self {
        let fetch = Arc::new(FakeFetch::new());
        let mut assets = serde_json::Map::new();
        for p in platforms {
            assets.insert(
                p.to_string(),
                json!({ "url": tarball_url(p), "sha256": sha(TARBALL), "size": TARBALL.len() }),
            );
            fetch.add(&tarball_url(p), TARBALL.to_vec());
        }
        let release = json!({
            "version": "1.0.5",
            "tag": TAG,
            "notes": "notes",
            "published_at": "2026-10-01T00:00:00Z",
            "assets": assets,
        });
        for channel in [Channel::Stable, Channel::Beta] {
            fetch.add(
                &releases_manifest_url(channel),
                release.to_string().into_bytes(),
            );
        }
        let gh = json!({
            "tag_name": TAG,
            "assets": [
                { "name": "helixscreen-pi-v1.0.5.tar.gz", "browser_download_url": "https://example.invalid/x", "digest": "sha256:00" },
                install_sh_asset,
            ]
        });
        fetch.add(
            &format!("{GITHUB_RELEASE_API}/{TAG}"),
            gh.to_string().into_bytes(),
        );
        fetch.add(INSTALL_SH_URL, INSTALL_SH.to_vec());
        let ssh = FakeSsh::new();
        ssh.accept(ADDR, "root", PASSWORD);
        ssh.accept(ADDR, "pi", PASSWORD);
        ssh.accept(ADDR, "mks", PASSWORD);
        Fakes {
            ssh,
            fetch,
            clock: Arc::new(FakeClock::new()),
            cache: tempfile::tempdir().unwrap(),
        }
    }

    fn standard(platforms: &[&str]) -> Self {
        Fakes::new(platforms, good_install_sh())
    }

    fn env(&self) -> Env {
        Env {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh: Arc::new(self.ssh.clone()),
            drives: Arc::new(FakeDrives::new()),
            fetch: self.fetch.clone(),
            clock: self.clock.clone(),
            net: Arc::new(FakeNet::new()),
            serial: Arc::new(FakeSerial::new()),
        }
    }

    /// A runner for `steps`, logged in as `user` by a stand-in `ssh_login` first step.
    fn runner(&self, user: &str, steps: Value, facts: Value) -> Runner {
        self.runner_with_password(user, PASSWORD, steps, facts)
    }

    fn runner_with_password(
        &self,
        user: &str,
        password: &str,
        steps: Value,
        facts: Value,
    ) -> Runner {
        let mut all = vec![json!({ "type": "ssh_login", "logins": [] })];
        all.extend(steps.as_array().unwrap().iter().cloned());
        let facts: Facts = serde_json::from_value(facts).unwrap();
        let state = RunState::new("run1", "k1", "r", Host::new(ADDR), facts);
        Runner::new(self.env(), manifest(Value::Array(all)), state)
            .with_handler(
                "ssh_login",
                Arc::new(TestLogin(user.into(), password.into())),
            )
            .with_cache_dir(self.cache.path().to_path_buf())
    }

    fn now(&self) -> SystemTime {
        self.clock.now()
    }
}

fn good_install_sh() -> Value {
    json!({ "name": "install.sh", "browser_download_url": INSTALL_SH_URL, "digest": format!("sha256:{}", sha(INSTALL_SH)) })
}

fn manifest(steps: Value) -> Arc<Manifest> {
    let m: Manifest = serde_json::from_value(json!({
        "manifest_version": 1,
        "min_app_version": "0.1.0",
        "printers": [{
            "id": "k1",
            "name": "Creality K1",
            "identify": [{ "model": "K1" }],
            "recipes": [{ "when": { "state": "stock" }, "recipe": "r" }],
            "restore": "r",
            "fallback": { "refuse": true, "doc": "https://helixscreen.org/guide/install-k1/" }
        }],
        "artifacts": {},
        "recipes": { "r": steps }
    }))
    .unwrap();
    Arc::new(m)
}

struct TestLogin(String, String);

#[async_trait]
impl StepHandler for TestLogin {
    async fn run(&self, ctx: &mut StepCtx<'_>, _step: &Step, _input: Option<Input>) -> StepOutcome {
        ctx.set_login(Login::new(self.0.clone(), self.1.clone()));
        StepOutcome::Done
    }
}

/// Advances past every `Progress` event, collecting their titles, and returns the first other
/// event.
async fn settle(r: &mut Runner) -> (Event, Vec<String>) {
    let mut titles = Vec::new();
    for _ in 0..200 {
        match r.advance(None).await {
            Event::Progress { title, .. } => titles.push(title),
            other => return (other, titles),
        }
    }
    panic!("runner kept reporting progress");
}

fn failure(ev: &Event) -> &str {
    match ev {
        Event::Failed { reason, .. } => reason,
        e => panic!("expected Failed, got {e:?}"),
    }
}

fn pl(fields: &[&str]) -> String {
    format!("HELIX-PROGRESS\t{}\n", fields.join("\t"))
}

fn install_step(platform: Value) -> Value {
    json!([{ "type": "helix_install", "platform": platform }])
}

fn spawned(f: &Fakes) -> Vec<(String, Option<Vec<u8>>)> {
    f.ssh
        .calls()
        .into_iter()
        .filter(|(c, _)| c.starts_with("sh ") && c.contains("install.sh"))
        .collect()
}

// ---------------------------------------------------------------- helix_install

#[tokio::test]
async fn installs_the_platform_for_the_cpu_and_reports_its_steps() {
    let f = Fakes::standard(&["pi", "pi32"]);
    let log = [
        "human output on stderr\n".to_string(),
        pl(&["platform", "pi"]),
        pl(&["step", "1", "2", "start", "Extracting", ""]),
        pl(&["step", "1", "2", "ok", "Extracting", "done"]),
        pl(&["step", "2", "2", "start", "Starting HelixScreen", ""]),
        pl(&["step", "2", "2", "ok", "Starting HelixScreen", ""]),
        pl(&["result", "ok"]),
    ];
    let mut chunks: Vec<(&str, Option<i32>)> = log.iter().map(|l| (l.as_str(), None)).collect();
    chunks.push(("", Some(0)));
    f.ssh.script_job(chunks);
    let mut r = f.runner(
        "root",
        install_step(json!({ "from_arch": { "aarch64": "pi", "armv7l": "pi32" } })),
        json!({ "cpu_arch": "aarch64" }),
    );

    let (ev, titles) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert!(titles.iter().any(|t| t == "Extracting"), "{titles:?}");
    assert!(
        titles.iter().any(|t| t == "Starting HelixScreen"),
        "{titles:?}"
    );

    let uploads = f.ssh.uploads();
    assert!(uploads.contains(&("/tmp/install.sh".into(), INSTALL_SH.to_vec())));
    assert!(uploads.contains(&("/tmp/helixscreen-pi-v1.0.5.tar.gz".into(), TARBALL.to_vec())));
    assert_eq!(
        spawned(&f),
        vec![(
            "sh '/tmp/install.sh' --local '/tmp/helixscreen-pi-v1.0.5.tar.gz' --progress".into(),
            None
        )]
    );
    assert!(f.fetch.requests().contains(&tarball_url("pi")));
    assert!(!f.fetch.requests().contains(&tarball_url("pi32")));
    assert_eq!(r.state().job, None, "a finished job is forgotten");
}

#[tokio::test]
async fn a_fixed_platform_installs_into_the_recipe_dir_as_an_update() {
    let f = Fakes::standard(&["ad5m"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "ad5m"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let steps =
        json!([{ "type": "helix_install", "platform": "ad5m", "update": true, "dir": "/data" }]);
    let mut r = f.runner("root", steps, json!({ "helix_version": "v1.0.4" }));

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let paths: Vec<String> = f.ssh.uploads().into_iter().map(|(p, _)| p).collect();
    assert!(paths.contains(&"/data/install.sh".into()), "{paths:?}");
    assert!(paths.contains(&"/data/helixscreen-ad5m-v1.0.5.tar.gz".into()));
    assert_eq!(
        spawned(&f)[0].0,
        "sh '/data/install.sh' --local '/data/helixscreen-ad5m-v1.0.5.tar.gz' --progress --update"
    );
}

#[tokio::test]
async fn uninstall_uploads_only_install_sh_and_runs_it_with_uninstall() {
    let f = Fakes::standard(&["k1"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "k1"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let steps = json!([{ "type": "helix_install", "platform": "k1", "uninstall": true, "dir": "/usr/data" }]);
    let mut r = f.runner("root", steps, json!({}));

    let (ev, titles) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert!(
        titles.iter().any(|t| t.contains("remove HelixScreen")),
        "{titles:?}"
    );
    let paths: Vec<String> = f.ssh.uploads().into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, vec!["/usr/data/install.sh".to_string()]);
    assert!(!f.fetch.requests().contains(&tarball_url("k1")));
    assert_eq!(
        spawned(&f)[0].0,
        "sh '/usr/data/install.sh' --uninstall --progress"
    );
    assert!(
        f.ssh
            .commands()
            .iter()
            .any(|c| c == "rm -f '/usr/data/install.sh'"),
        "{:?}",
        f.ssh.commands()
    );
}

#[tokio::test]
async fn a_platform_the_printer_does_not_need_stops_the_installer_at_once() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.job_pid(4242);
    f.ssh.on_command(
        "kill -TERM -- -4242 2>/dev/null || kill -TERM 4242",
        ExecOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit: 0,
        },
    );
    f.ssh.script_job(vec![
        (&pl(&["platform", "snapmaker-u1"]), None),
        (&pl(&["step", "1", "5", "start", "Extracting", ""]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    let reason = failure(&ev);
    assert!(
        reason.contains("snapmaker-u1") && reason.contains("pi"),
        "{reason}"
    );
    let cmds = f.ssh.commands();
    let kill = cmds.iter().position(|c| c.contains("kill -TERM -- -4242"));
    let rm = cmds.iter().position(|c| c.starts_with("rm -f "));
    assert!(kill.is_some() && rm.is_some() && kill < rm, "{cmds:?}");
    assert_eq!(r.state().job, None);
}

#[tokio::test]
async fn a_wrong_platform_from_a_job_run_through_sudo_is_killed_through_sudo() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.job_pid(77);
    f.ssh.set_fallback(|cmd| ExecOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit: if cmd == "sudo -n true" { 1 } else { 0 },
    });
    f.ssh
        .script_job(vec![(&pl(&["platform", "pi32"]), None), ("", None)]);
    let mut r = f.runner("pi", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("pi32"));
    let kill = f
        .ssh
        .calls()
        .into_iter()
        .find(|(c, _)| c.contains("kill -TERM -- -77"))
        .expect("a kill");
    assert!(kill.0.starts_with("sudo -S -p '' sh -c "), "{kill:?}");
    assert_eq!(kill.1.as_deref(), Some(format!("{PASSWORD}\n").as_bytes()));
}

#[tokio::test]
async fn a_dry_run_uploads_runs_dry_run_and_removes_its_files() {
    let f = Fakes::standard(&["k1"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "k1"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let steps = json!([{ "type": "helix_install", "platform": "k1", "update": true, "dry_run": true, "dir": "/usr/data" }]);
    let mut r = f.runner("root", steps, json!({ "helix_version": "v1.0.4" }));

    let (ev, titles) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert!(titles.iter().any(|t| t.contains("Checking")), "{titles:?}");
    assert_eq!(
        spawned(&f)[0].0,
        "sh '/usr/data/install.sh' --local '/usr/data/helixscreen-k1-v1.0.5.tar.gz' --progress --update --dry-run"
    );
    assert!(
        f.ssh
            .commands()
            .iter()
            .any(|c| c.starts_with("rm -f ") && c.contains("helixscreen-k1")),
        "{:?}",
        f.ssh.commands()
    );
}

#[tokio::test]
async fn a_dry_run_that_fails_fails_the_step_with_the_installers_reason() {
    let f = Fakes::standard(&["k1"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "k1"]), None),
        (
            &pl(&["result", "fail", "not enough space on /usr/data"]),
            Some(1),
        ),
    ]);
    let steps = json!([{ "type": "helix_install", "platform": "k1", "dry_run": true }]);
    let mut r = f.runner("root", steps, json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("not enough space"), "{ev:?}");
}

#[tokio::test]
async fn an_installer_reporting_another_platform_fails() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi32"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    let reason = failure(&ev);
    assert!(reason.contains("pi32") && reason.contains("pi"), "{reason}");
}

#[tokio::test]
async fn an_ok_result_without_a_platform_line_fails() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![(&pl(&["result", "ok"]), Some(0))]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("platform"), "{ev:?}");
}

#[tokio::test]
async fn a_failed_result_surfaces_the_installers_reason() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (
            &pl(&[
                "step",
                "1",
                "3",
                "fail",
                "Extracting",
                "no space left on /tmp",
            ]),
            None,
        ),
        (&pl(&["result", "fail", "no space left on /tmp"]), Some(1)),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("no space left on /tmp"), "{ev:?}");
    assert_eq!(r.state().job, None);
}

#[tokio::test]
async fn the_support_bundle_carries_the_installers_output_after_a_failure() {
    let f = Fakes::standard(&["pi"]);
    let mut lines = vec![
        pl(&["platform", "pi"]),
        "tar: /usr/data/helixscreen: Cannot write: No space left on device\n".to_string(),
        format!("a broken script echoed {PASSWORD}\n"),
    ];
    // More lines than a failure message quotes.
    lines.extend((1..=12).map(|n| format!("unpacking file {n}\n")));
    lines.push(pl(&["result", "fail", "no space left"]));
    let last = lines.len() - 1;
    let chunks = lines
        .iter()
        .enumerate()
        .map(|(i, l)| (l.as_str(), (i == last).then_some(1)))
        .collect();
    f.ssh.script_job(chunks);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    failure(&ev);
    let logs = r.job_logs();
    let secrets = r.secrets();
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    let bundle = support_bundle(r.state(), &logs, &secrets, false);
    let (_, printer_side) = bundle
        .split_once("printer-side job log:\n")
        .expect("the bundle has a job log section");
    for line in [
        "tar: /usr/data/helixscreen: Cannot write: No space left on device",
        "unpacking file 1\n",
        "unpacking file 12",
        "HELIX-PROGRESS\tresult\tfail\tno space left",
    ] {
        assert!(
            printer_side.contains(line),
            "{line:?} missing from {printer_side}"
        );
    }
    assert!(!bundle.contains(PASSWORD), "{bundle}");
}

#[tokio::test]
async fn progress_lines_split_across_reads_still_parse() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![
        ("HELIX-PROGRESS\tplat", None),
        ("form\tpi\nHELIX-PROGRESS\tresult", None),
        ("\tok", None),
        ("", Some(0)),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
}

#[tokio::test]
async fn a_job_that_goes_silent_times_out_instead_of_succeeding() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![(&pl(&["platform", "pi"]), None)]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let start = f.now();
    let (ev, _) = settle(&mut r).await;
    let reason = failure(&ev);
    assert!(reason.contains("nothing"), "{reason}");
    let waited = f.now().duration_since(start).unwrap();
    assert!(waited >= HELIX_INSTALL_SILENCE, "gave up after {waited:?}");
    assert!(
        r.state().job.is_some(),
        "a job still running is kept, so a retry follows it instead of starting a second install"
    );
}

#[tokio::test]
async fn a_job_that_dies_without_a_result_fails() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_dying_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["step", "1", "3", "start", "Extracting", ""]), None),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let start = f.now();
    let (ev, _) = settle(&mut r).await;
    assert_eq!(
        failure(&ev),
        "the installer stopped without reporting a result"
    );
    assert!(f.now().duration_since(start).unwrap() < HELIX_INSTALL_SILENCE);
    assert_eq!(r.state().job, None);
}

#[tokio::test]
async fn a_job_that_exits_without_a_result_fails() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        ("sh: syntax error\n", Some(2)),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    let reason = failure(&ev);
    assert!(reason.contains("without reporting a result"), "{reason}");
    assert!(reason.contains("syntax error"), "{reason}");
}

fn out_ok() -> ExecOutput {
    ExecOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit: 0,
    }
}

fn sudo_needs_a_password(f: &Fakes) {
    f.ssh.on_command(
        "sudo -n true",
        ExecOutput {
            stdout: String::new(),
            stderr: "sudo: a password is required".into(),
            exit: 1,
        },
    );
}

#[tokio::test]
async fn a_non_root_login_without_passwordless_sudo_runs_the_installer_through_sudo() {
    let f = Fakes::standard(&["pi"]);
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts(PASSWORD);
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let mut r = f.runner("mks", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let s = spawned(&f);
    assert_eq!(s.len(), 1);
    assert_eq!(
        s[0].1.as_deref(),
        Some(PASSWORD.as_bytes()),
        "the password goes to sudo's stdin"
    );
    assert!(f.ssh.commands().iter().all(|c| !c.contains(PASSWORD)));
    let others: Vec<_> = f
        .ssh
        .calls()
        .into_iter()
        .filter(|(c, _)| !(c.starts_with("sh ") && c.contains("install.sh")))
        .collect();
    // Only the removal of the root-owned job log goes through sudo as well.
    assert!(
        others
            .iter()
            .all(|(c, stdin)| stdin.is_none() || c == "sudo -S -p '' rm -f '/tmp/fake-job-1.log'"),
        "{others:?}"
    );
    let saved = serde_json::to_string(r.state()).unwrap();
    assert!(!saved.contains(PASSWORD), "{saved}");
}

#[tokio::test]
async fn sudo_refusing_the_login_password_fails_without_naming_it() {
    let f = Fakes::standard(&["pi"]);
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts("another-password");
    f.ssh.script_job(vec![(&pl(&["result", "ok"]), Some(0))]);
    let mut r = f.runner("mks", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    match &ev {
        Event::Failed { reason, help, .. } => {
            assert!(reason.contains("sudo"), "{reason}");
            assert!(reason.contains("mks"), "{reason}");
            assert!(!reason.contains(PASSWORD), "{reason}");
            assert!(!help.as_deref().unwrap_or("").contains(PASSWORD));
        }
        e => panic!("expected Failed, got {e:?}"),
    }
    assert!(
        !rm_commands(&f).is_empty(),
        "the uploaded files are removed"
    );
    let saved = serde_json::to_string(r.state()).unwrap();
    assert!(!saved.contains(PASSWORD), "{saved}");
}

#[tokio::test]
async fn a_key_login_without_passwordless_sudo_asks_for_the_sudo_password_before_uploading() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.accept(ADDR, "mks", "");
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts(SUDO_PW);
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let mut r = f.runner_with_password("mks", "", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    match &ev {
        Event::NeedInput(Prompt::SudoPassword { user, refused }) => {
            assert_eq!(user, "mks");
            assert!(!refused);
        }
        e => panic!("expected a sudo password prompt, got {e:?}"),
    }
    assert!(f.ssh.uploads().is_empty());
    assert!(spawned(&f).is_empty());

    let ev = r
        .advance(Some(Input::SudoPassword(Secret::new(SUDO_PW))))
        .await;
    let ev = match ev {
        Event::Progress { .. } => settle(&mut r).await.0,
        e => e,
    };
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let s = spawned(&f);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].1.as_deref(), Some(SUDO_PW.as_bytes()));
    assert!(f.ssh.commands().iter().all(|c| !c.contains(SUDO_PW)));
    let saved = serde_json::to_string(r.state()).unwrap();
    assert!(!saved.contains(SUDO_PW), "{saved}");
    let logged = format!("{:?}", r.state().log);
    assert!(!logged.contains(SUDO_PW), "{logged}");
}

#[tokio::test]
async fn a_typed_sudo_password_sudo_refuses_is_asked_for_again_before_uploading() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.accept(ADDR, "mks", "");
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts(SUDO_PW);
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let mut r = f.runner_with_password("mks", "", install_step(json!("pi")), json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(
        matches!(ev, Event::NeedInput(Prompt::SudoPassword { .. })),
        "{ev:?}"
    );

    let ev = r
        .advance(Some(Input::SudoPassword(Secret::new("not-it"))))
        .await;
    match &ev {
        Event::NeedInput(Prompt::SudoPassword { refused, .. }) => assert!(*refused),
        e => panic!("expected the prompt again, got {e:?}"),
    }
    assert!(
        f.ssh.uploads().is_empty(),
        "nothing uploads on a wrong password"
    );

    let ev = r
        .advance(Some(Input::SudoPassword(Secret::new(SUDO_PW))))
        .await;
    let ev = match ev {
        Event::Progress { .. } => settle(&mut r).await.0,
        e => e,
    };
    assert!(matches!(ev, Event::Done), "{ev:?}");
}

#[tokio::test]
async fn the_typed_sudo_password_is_held_as_a_secret_for_the_support_bundle() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.accept(ADDR, "mks", "");
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts(SUDO_PW);
    let mut r = f.runner_with_password("mks", "", install_step(json!("pi")), json!({}));
    let _ = settle(&mut r).await;
    let _ = r
        .advance(Some(Input::SudoPassword(Secret::new(SUDO_PW))))
        .await;
    assert!(
        r.secrets().contains(&SUDO_PW.to_string()),
        "{:?}",
        r.secrets().len()
    );
    let shown = format!("{:?}", Input::SudoPassword(Secret::new(SUDO_PW)));
    assert!(!shown.contains(SUDO_PW), "{shown}");
}

#[tokio::test]
async fn a_non_root_login_with_passwordless_sudo_installs_without_sending_the_password() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.on_command(
        "sudo -n true",
        ExecOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit: 0,
        },
    );
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let mut r = f.runner("pi", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let s = spawned(&f);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].1, None, "the password never reaches the install job");
    assert!(f.ssh.stdins().iter().all(|b| b != PASSWORD.as_bytes()));
}

async fn fails_before_upload(f: Fakes, platform: Value, facts: Value) -> String {
    let mut r = f.runner("root", install_step(platform), facts);
    let (ev, _) = settle(&mut r).await;
    assert!(f.ssh.uploads().is_empty(), "{:?}", f.ssh.uploads());
    assert!(spawned(&f).is_empty());
    match ev {
        Event::Failed { reason, .. } | Event::Refused { reason, .. } => reason,
        e => panic!("expected a stop, got {e:?}"),
    }
}

#[tokio::test]
async fn an_install_sh_whose_digest_does_not_match_is_not_uploaded() {
    let bad = json!({ "name": "install.sh", "browser_download_url": INSTALL_SH_URL, "digest": format!("sha256:{}", sha(b"other")) });
    let reason = fails_before_upload(Fakes::new(&["pi"], bad), json!("pi"), json!({})).await;
    assert!(reason.contains("install.sh"), "{reason}");
    assert!(reason.contains("hash"), "{reason}");
}

#[tokio::test]
async fn an_install_sh_asset_without_a_digest_fails() {
    let bare = json!({ "name": "install.sh", "browser_download_url": INSTALL_SH_URL });
    let reason = fails_before_upload(Fakes::new(&["pi"], bare), json!("pi"), json!({})).await;
    assert!(reason.contains("digest"), "{reason}");
}

#[tokio::test]
async fn a_release_without_install_sh_fails() {
    let other = json!({ "name": "README.md", "browser_download_url": "https://example.invalid/r", "digest": "sha256:00" });
    let reason = fails_before_upload(Fakes::new(&["pi"], other), json!("pi"), json!({})).await;
    assert!(reason.contains("has no install.sh"), "{reason}");
}

#[tokio::test]
async fn a_tarball_that_does_not_match_the_release_manifest_is_not_uploaded() {
    let f = Fakes::standard(&["pi"]);
    f.fetch.add(&tarball_url("pi"), b"tampered".to_vec());
    let reason = fails_before_upload(f, json!("pi"), json!({})).await;
    assert!(reason.contains("hash"), "{reason}");
}

#[tokio::test]
async fn a_platform_missing_from_the_release_fails_clearly() {
    let reason = fails_before_upload(Fakes::standard(&["pi"]), json!("k2"), json!({})).await;
    assert!(reason.contains("k2"), "{reason}");
}

#[tokio::test]
async fn a_cpu_the_recipe_does_not_map_stops() {
    let reason = fails_before_upload(
        Fakes::standard(&["pi"]),
        json!({ "from_arch": { "aarch64": "pi" } }),
        json!({ "cpu_arch": "mips" }),
    )
    .await;
    assert!(reason.contains("mips"), "{reason}");
}

#[tokio::test]
async fn a_reopened_run_asks_for_the_login_then_follows_its_recorded_job() {
    let f = Fakes::standard(&["pi"]);
    // The job the earlier app session launched; the fake hands out logs only through
    // spawn_detached.
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["step", "1", "1", "start", "Extracting", ""]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let login = || Login::new("root", PASSWORD);
    let job = {
        let mut s = Ssh::connect(&f.ssh, &Host::new(ADDR), &login())
            .await
            .unwrap();
        s.spawn_detached("earlier launch", None).await.unwrap()
    };
    // The run as the reopened app loads it: at the install step, which started and recorded
    // its job, with no login in memory.
    let facts: Facts = serde_json::from_value(json!({ "ssh_user": "root" })).unwrap();
    let mut state = RunState::new("run1", "k1", "r", Host::new(ADDR), facts);
    state.started_step = Some(0);
    state.job = Some(job);
    let mut r = Runner::new(f.env(), manifest(install_step(json!("pi"))), state)
        .with_cache_dir(f.cache.path().to_path_buf());

    match r.advance(None).await {
        Event::NeedInput(Prompt::Login { user_hint, .. }) => {
            assert_eq!(user_hint.as_deref(), Some("root"))
        }
        e => panic!("expected a login prompt, got {e:?}"),
    }
    let mut ev = r.advance(Some(Input::Login(login()))).await;
    let mut titles = Vec::new();
    while let Event::Progress { title, .. } = &ev {
        titles.push(title.clone());
        ev = r.advance(None).await;
    }
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert!(titles.iter().any(|t| t == "Extracting"), "{titles:?}");
    assert!(f.ssh.uploads().is_empty(), "nothing re-uploaded");
    assert!(spawned(&f).is_empty(), "nothing re-run");
    assert!(f.fetch.requests().is_empty(), "nothing re-downloaded");
}

// ---------------------------------------------------------------- ssh_run

fn ssh_run(command: &str, timeout_s: u64) -> Value {
    json!([{ "type": "ssh_run", "command": command, "timeout_s": timeout_s }])
}

#[tokio::test]
async fn ssh_run_succeeds_on_exit_zero() {
    let f = Fakes::standard(&[]);
    f.ssh.script_job(vec![("syncing\n", None), ("", Some(0))]);
    let mut r = f.runner("root", ssh_run("sync && echo done", 60), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let job_calls: Vec<_> = f
        .ssh
        .calls()
        .into_iter()
        .filter(|(c, _)| c == "sync && echo done")
        .collect();
    assert_eq!(job_calls, vec![("sync && echo done".to_string(), None)]);
    assert_eq!(r.state().job, None);
}

fn job_calls(f: &Fakes, command: &str) -> Vec<(String, Option<Vec<u8>>)> {
    f.ssh
        .calls()
        .into_iter()
        .filter(|(c, _)| c == command)
        .collect()
}

#[tokio::test]
async fn ssh_run_for_a_login_without_passwordless_sudo_runs_through_sudo() {
    let f = Fakes::standard(&[]);
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts(PASSWORD);
    f.ssh.script_job(vec![("", Some(0))]);
    let mut r = f.runner(
        "mks",
        ssh_run("systemctl stop KlipperScreen", 60),
        json!({}),
    );

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(
        job_calls(&f, "systemctl stop KlipperScreen"),
        vec![(
            "systemctl stop KlipperScreen".to_string(),
            Some(PASSWORD.as_bytes().to_vec())
        )]
    );
    assert!(f.ssh.commands().iter().all(|c| !c.contains(PASSWORD)));
}

#[tokio::test]
async fn ssh_run_with_passwordless_sudo_runs_as_the_login_user() {
    let f = Fakes::standard(&[]);
    f.ssh.on_command("sudo -n true", out_ok());
    f.ssh.script_job(vec![("", Some(0))]);
    let mut r = f.runner(
        "mks",
        ssh_run("sudo systemctl stop KlipperScreen", 60),
        json!({}),
    );

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(
        job_calls(&f, "sudo systemctl stop KlipperScreen"),
        vec![("sudo systemctl stop KlipperScreen".to_string(), None)]
    );
}

#[tokio::test]
async fn ssh_run_whose_sudo_refuses_the_password_fails_naming_sudo() {
    let f = Fakes::standard(&[]);
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts("another-password");
    f.ssh.script_job(vec![("", Some(0))]);
    let mut r = f.runner(
        "mks",
        ssh_run("systemctl stop KlipperScreen", 60),
        json!({}),
    );

    let (ev, _) = settle(&mut r).await;
    let reason = failure(&ev);
    assert!(reason.contains("sudo"), "{reason}");
    assert!(!reason.contains(PASSWORD), "{reason}");
}

#[tokio::test]
async fn ssh_run_for_a_key_login_without_passwordless_sudo_asks_for_the_sudo_password() {
    let f = Fakes::standard(&[]);
    f.ssh.accept(ADDR, "mks", "");
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts(SUDO_PW);
    f.ssh.script_job(vec![("", Some(0))]);
    let mut r = f.runner_with_password("mks", "", ssh_run("ls", 60), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(
        matches!(ev, Event::NeedInput(Prompt::SudoPassword { .. })),
        "{ev:?}"
    );
    assert!(job_calls(&f, "ls").is_empty());
    let ev = r
        .advance(Some(Input::SudoPassword(Secret::new(SUDO_PW))))
        .await;
    let ev = match ev {
        Event::Progress { .. } => settle(&mut r).await.0,
        e => e,
    };
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(
        job_calls(&f, "ls"),
        vec![("ls".to_string(), Some(SUDO_PW.as_bytes().to_vec()))]
    );
}

#[tokio::test]
async fn ssh_run_failure_shows_the_last_lines_of_output() {
    let f = Fakes::standard(&[]);
    let mut text = String::new();
    for i in 0..40 {
        text.push_str(&format!("line {i}\n"));
    }
    text.push_str("cp: cannot create /opt/x: Read-only file system\n");
    f.ssh.script_job(vec![(&text, None), ("", Some(1))]);
    let mut r = f.runner("root", ssh_run("cp a /opt/x", 60), json!({}));

    let (ev, _) = settle(&mut r).await;
    let reason = failure(&ev);
    assert!(reason.contains("exit 1"), "{reason}");
    assert!(reason.contains("Read-only file system"), "{reason}");
    assert!(
        !reason.contains("line 0\n") && !reason.contains("line 0 "),
        "only the tail: {reason}"
    );
}

#[tokio::test]
async fn ssh_run_times_out_after_its_timeout_of_silence() {
    let f = Fakes::standard(&[]);
    f.ssh.script_job(vec![("started\n", None)]);
    let mut r = f.runner("root", ssh_run("sleep 9999", 30), json!({}));

    let start = f.now();
    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("30 seconds"), "{ev:?}");
    assert!(f.now().duration_since(start).unwrap() >= Duration::from_secs(30));
}

#[tokio::test]
async fn ssh_run_whose_process_dies_fails() {
    let f = Fakes::standard(&[]);
    f.ssh.script_dying_job(vec![("half\n", None)]);
    let mut r = f.runner("root", ssh_run("thing", 60), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("stopped"), "{ev:?}");
}

#[tokio::test]
async fn a_retry_after_a_silence_timeout_waits_a_whole_window_again() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![(&pl(&["platform", "pi"]), None)]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("nothing"), "{ev:?}");

    let retried = f.now();
    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("nothing"), "{ev:?}");
    let waited = f.now().duration_since(retried).unwrap();
    assert!(
        waited >= HELIX_INSTALL_SILENCE,
        "retry gave up after {waited:?}"
    );
}

#[tokio::test]
async fn ssh_run_retried_after_silence_finishes_when_the_job_speaks_again() {
    let f = Fakes::standard(&[]);
    // Reads 1-3 time out a 4 s window; the retry's first read is still quiet.
    f.ssh.script_job(vec![
        ("started\n", None),
        ("", None),
        ("", None),
        ("", None),
        ("done\n", None),
        ("", Some(0)),
    ]);
    let mut r = f.runner("root", ssh_run("slow thing", 4), json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("4 seconds"), "{ev:?}");

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(r.state().job, None);
}

#[tokio::test]
async fn a_recipe_dir_with_a_space_and_a_quote_is_shell_quoted() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let steps = json!([{ "type": "helix_install", "platform": "pi", "dir": "/data/my dir's/" }]);
    let mut r = f.runner("root", steps, json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let paths: Vec<String> = f.ssh.uploads().into_iter().map(|(p, _)| p).collect();
    assert!(
        paths.contains(&"/data/my dir's/install.sh".into()),
        "{paths:?}"
    );
    assert!(paths.contains(&"/data/my dir's/helixscreen-pi-v1.0.5.tar.gz".into()));
    assert_eq!(
        spawned(&f)[0].0,
        r"sh '/data/my dir'\''s/install.sh' --local '/data/my dir'\''s/helixscreen-pi-v1.0.5.tar.gz' --progress"
    );
}

// ---------------------------------------------------------------- release channel

fn channel_step(channel: Option<&str>) -> Value {
    let mut step = json!({ "type": "helix_install", "platform": "pi" });
    if let Some(c) = channel {
        step["channel"] = json!(c);
    }
    json!([step])
}

fn release_list_requests(f: &Fakes) -> Vec<String> {
    f.fetch
        .requests()
        .into_iter()
        .filter(|u| u.ends_with("/manifest.json"))
        .collect()
}

async fn install_with(f: &Fakes, mut r: Runner) {
    f.ssh.script_job(vec![
        (pl(&["platform", "pi"]).as_str(), None),
        (pl(&["result", "ok"]).as_str(), None),
        ("", Some(0)),
    ]);
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
}

#[test]
fn an_unpinned_channel_is_left_to_the_run_and_each_channel_names_its_manifest() {
    let step: Step = serde_json::from_value(channel_step(None)[0].clone()).unwrap();
    assert!(matches!(step, Step::HelixInstall { channel: None, .. }));
    assert_eq!(
        releases_manifest_url(Channel::Stable),
        "https://releases.helixscreen.org/stable/manifest.json"
    );
    assert_eq!(
        releases_manifest_url(Channel::Beta),
        "https://releases.helixscreen.org/beta/manifest.json"
    );
}

#[test]
fn an_unknown_channel_fails_manifest_parsing() {
    let bad = serde_json::from_value::<Step>(json!({
        "type": "helix_install", "platform": "pi", "channel": "nightly"
    }));
    assert!(bad.is_err());
    assert_eq!(Channel::parse("beta"), Some(Channel::Beta));
    assert_eq!(Channel::parse("stable"), Some(Channel::Stable));
    assert_eq!(Channel::parse("Beta"), None);
    assert_eq!(Channel::parse(""), None);
}

#[tokio::test]
async fn a_new_install_without_a_channel_reads_the_beta_while_stable_is_older_than_1_1_0() {
    // The stable list holds 1.0.5.
    let f = Fakes::standard(&["pi"]);
    let r = f.runner("root", channel_step(None), json!({}));
    install_with(&f, r).await;
    assert_eq!(
        release_list_requests(&f),
        vec![
            releases_manifest_url(Channel::Stable),
            releases_manifest_url(Channel::Beta)
        ]
    );
}

#[tokio::test]
async fn a_beta_recipe_reads_the_beta_list() {
    let f = Fakes::standard(&["pi"]);
    let r = f.runner("root", channel_step(Some("beta")), json!({}));
    install_with(&f, r).await;
    assert_eq!(
        release_list_requests(&f),
        vec![releases_manifest_url(Channel::Beta)]
    );
}

#[tokio::test]
async fn the_override_wins_over_the_recipes_channel() {
    let f = Fakes::standard(&["pi"]);
    let r = f
        .runner("root", channel_step(None), json!({}))
        .with_channel_override(Channel::Beta);
    install_with(&f, r).await;
    assert_eq!(
        release_list_requests(&f),
        vec![releases_manifest_url(Channel::Beta)]
    );

    let f = Fakes::standard(&["pi"]);
    let r = f
        .runner("root", channel_step(Some("beta")), json!({}))
        .with_channel_override(Channel::Stable);
    install_with(&f, r).await;
    assert_eq!(
        release_list_requests(&f),
        vec![releases_manifest_url(Channel::Stable)]
    );
}

// ---------------------------------------------------------------- capability and cleanup

const OLD_INSTALL_SH: &[u8] =
    b"#!/bin/sh\ncase \"$1\" in\n  --update) U=1 ;;\n  --help) usage ;;\nesac\n";

/// The removals of uploaded files; a job log's removal is `log_removals`.
fn rm_commands(f: &Fakes) -> Vec<String> {
    f.ssh
        .commands()
        .into_iter()
        .filter(|c| c.starts_with("rm ") && !c.contains("fake-job-"))
        .collect()
}

fn log_removals(f: &Fakes) -> Vec<String> {
    f.ssh
        .commands()
        .into_iter()
        .filter(|c| c.contains("rm -f ") && c.contains("fake-job-"))
        .collect()
}

const CLEANUP: &str = "rm -f '/tmp/helixscreen-pi-v1.0.5.tar.gz' '/tmp/install.sh'";

#[tokio::test]
async fn an_install_sh_without_progress_support_is_refused_before_any_upload() {
    let old = json!({ "name": "install.sh", "browser_download_url": INSTALL_SH_URL, "digest": format!("sha256:{}", sha(OLD_INSTALL_SH)) });
    let f = Fakes::new(&["pi"], old);
    f.fetch.add(INSTALL_SH_URL, OLD_INSTALL_SH.to_vec());
    let reason = fails_before_upload(f, json!("pi"), json!({})).await;
    assert_eq!(
        reason,
        "This HelixScreen release is older than this installer supports. Choose a newer release or update the installer."
    );
}

#[tokio::test]
async fn a_committing_install_whose_release_fails_its_checks_stops_before_the_commit() {
    let old = json!({ "name": "install.sh", "browser_download_url": INSTALL_SH_URL, "digest": format!("sha256:{}", sha(OLD_INSTALL_SH)) });
    let f = Fakes::new(&["pi"], old);
    f.fetch.add(INSTALL_SH_URL, OLD_INSTALL_SH.to_vec());
    let steps = json!([{ "type": "helix_install", "platform": "pi", "commits": true }]);
    let mut r = f.runner("root", steps, json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(
        matches!(
            ev,
            Event::Failed {
                committed: false,
                ..
            } | Event::Refused { .. }
        ),
        "{ev:?}"
    );
    assert!(!r.state().committed);
}

#[tokio::test]
async fn a_release_check_step_runs_every_check_and_touches_nothing() {
    let f = Fakes::standard(&["pi"]);
    let steps = json!([{ "type": "helix_install", "platform": "pi", "check": true }]);
    let mut r = f.runner("root", steps, json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert!(f.ssh.uploads().is_empty(), "{:?}", f.ssh.uploads());
    assert!(spawned(&f).is_empty());
    assert!(!r.state().committed);

    let old = json!({ "name": "install.sh", "browser_download_url": INSTALL_SH_URL, "digest": format!("sha256:{}", sha(OLD_INSTALL_SH)) });
    let f = Fakes::new(&["pi"], old);
    f.fetch.add(INSTALL_SH_URL, OLD_INSTALL_SH.to_vec());
    let steps = json!([{ "type": "helix_install", "platform": "pi", "check": true }]);
    let mut r = f.runner("root", steps, json!({}));
    let (ev, _) = settle(&mut r).await;
    match ev {
        Event::Failed {
            committed: false,
            reason,
            ..
        } => assert!(
            reason.contains("older than this installer supports"),
            "{reason}"
        ),
        e => panic!("{e:?}"),
    }
}

#[test]
fn a_new_install_takes_the_beta_until_stable_reaches_1_1_0() {
    let v = |s: &str| ReleaseVersion::parse(s).unwrap();
    for (stable, channel) in [
        ("1.0.5", Channel::Beta),
        ("v1.0.9", Channel::Beta),
        ("1.1.0-beta.10", Channel::Beta),
        ("1.1.0", Channel::Stable),
        ("1.2.3", Channel::Stable),
    ] {
        assert_eq!(new_install_channel(&v(stable)), channel, "{stable}");
    }
}

#[tokio::test]
async fn the_uploaded_files_are_removed_after_a_successful_install() {
    let f = Fakes::standard(&["pi"]);
    let r = f.runner("root", install_step(json!("pi")), json!({}));
    install_with(&f, r).await;
    assert_eq!(rm_commands(&f), vec![CLEANUP.to_string()]);
}

#[tokio::test]
async fn the_uploaded_files_are_removed_after_a_failed_install() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![
        (pl(&["result", "fail", "no space"]).as_str(), None),
        ("", Some(1)),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("installer failed"), "{ev:?}");
    assert_eq!(rm_commands(&f), vec![CLEANUP.to_string()]);
}

#[tokio::test]
async fn the_uploaded_files_are_removed_after_an_exit_without_a_result() {
    let f = Fakes::standard(&["pi"]);
    f.ssh
        .script_job(vec![("Usage: install.sh\n", None), ("", Some(1))]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(
        failure(&ev).contains("without reporting a result"),
        "{ev:?}"
    );
    assert_eq!(rm_commands(&f), vec![CLEANUP.to_string()]);
}

#[tokio::test]
async fn a_spawn_failure_removes_what_was_uploaded() {
    let f = Fakes::standard(&["pi"]);
    // No job is scripted, so the fake's spawn_detached fails after both uploads.
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("could not start"), "{ev:?}");
    assert_eq!(f.ssh.uploads().len(), 2);
    assert_eq!(rm_commands(&f), vec![CLEANUP.to_string()]);
}

#[tokio::test]
async fn a_cleanup_that_fails_does_not_fail_the_step() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.on_command(
        CLEANUP,
        ExecOutput {
            stdout: String::new(),
            stderr: "read-only".into(),
            exit: 1,
        },
    );
    let r = f.runner("root", install_step(json!("pi")), json!({}));
    install_with(&f, r).await;
    assert_eq!(rm_commands(&f).len(), 1);
}

// ---------------------------------------------------------------- job logs on the printer

#[tokio::test]
async fn a_finished_install_removes_its_job_log() {
    let f = Fakes::standard(&["pi"]);
    let r = f.runner("root", install_step(json!("pi")), json!({}));
    install_with(&f, r).await;
    assert_eq!(
        log_removals(&f),
        vec!["rm -f '/tmp/fake-job-1.log'".to_string()]
    );
}

#[tokio::test]
async fn a_failed_install_keeps_its_job_log_until_the_run_ends() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![
        (pl(&["result", "fail", "no space"]).as_str(), None),
        ("", Some(1)),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(failure(&ev).contains("installer failed"), "{ev:?}");
    assert!(log_removals(&f).is_empty(), "{:?}", log_removals(&f));
    assert!(
        r.job_logs().iter().any(|l| l.contains("no space")),
        "the support bundle still has its lines"
    );

    // The retry succeeds, which ends the run: both logs go.
    f.ssh.script_job(vec![
        (pl(&["platform", "pi"]).as_str(), None),
        (pl(&["result", "ok"]).as_str(), Some(0)),
    ]);
    let ev = r.advance(Some(Input::Choice("retry".into()))).await;
    let ev = match ev {
        Event::Progress { .. } => settle(&mut r).await.0,
        e => e,
    };
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let mut removed = log_removals(&f);
    removed.sort();
    assert_eq!(
        removed,
        vec![
            "rm -f '/tmp/fake-job-1.log'".to_string(),
            "rm -f '/tmp/fake-job-2.log'".to_string()
        ]
    );
}

#[tokio::test]
async fn a_job_log_written_through_sudo_is_removed_through_sudo() {
    let f = Fakes::standard(&["pi"]);
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts(PASSWORD);
    let r = f.runner("mks", install_step(json!("pi")), json!({}));
    install_with(&f, r).await;
    let removal = f
        .ssh
        .calls()
        .into_iter()
        .find(|(c, _)| c.contains("fake-job-1.log"))
        .expect("the log is removed");
    assert_eq!(removal.0, "sudo -S -p '' rm -f '/tmp/fake-job-1.log'");
    assert_eq!(
        removal.1.as_deref(),
        Some(format!("{PASSWORD}\n").as_bytes())
    );
}

// ---------------------------------------------------------------- progress

#[tokio::test]
async fn progress_never_moves_backwards_within_a_step() {
    // install.sh reuses a step number after a skip, so its own fractions read 2/6, 3/6, 2/6.
    let f = Fakes::standard(&["pi"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (
            &pl(&["step", "2", "6", "ok", "Installing libraries", ""]),
            None,
        ),
        (&pl(&["step", "3", "6", "start", "Unpacking", ""]), None),
        (&pl(&["step", "2", "6", "start", "Unpacked", ""]), None),
        (
            &pl(&["step", "4", "6", "ok", "Stopping the stock screen", ""]),
            None,
        ),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let mut r = f.runner("root", install_step(json!("pi")), json!({}));
    let mut seen = Vec::new();
    for _ in 0..200 {
        match r.advance(None).await {
            Event::Progress { fraction, .. } => seen.push(fraction),
            Event::Done => break,
            e => panic!("unexpected {e:?}"),
        }
    }
    let fractions: Vec<f32> = seen.iter().flatten().copied().collect();
    assert!(fractions.len() >= 3, "{seen:?}");
    assert!(
        fractions.windows(2).all(|w| w[1] >= w[0]),
        "progress went backwards: {seen:?}"
    );
    assert!(fractions.contains(&(4.0 / 6.0)), "{seen:?}");
    // Once a fraction is known, the bar never goes back to "unknown" in the same step.
    let first_known = seen.iter().position(Option::is_some).unwrap();
    assert!(seen[first_known..].iter().all(Option::is_some), "{seen:?}");
}

#[tokio::test]
async fn a_new_step_starts_its_progress_afresh() {
    let f = Fakes::standard(&["k1"]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "k1"]), None),
        (&pl(&["step", "5", "6", "ok", "Late step", ""]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    f.ssh.script_job(vec![
        (&pl(&["platform", "k1"]), None),
        (&pl(&["step", "1", "6", "ok", "Early step", ""]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let steps = json!([
        { "type": "helix_install", "platform": "k1", "dry_run": true },
        { "type": "helix_install", "platform": "k1" }
    ]);
    let mut r = f.runner("root", steps, json!({}));
    let mut by_step: Vec<(usize, f32)> = Vec::new();
    for _ in 0..200 {
        match r.advance(None).await {
            Event::Progress {
                step,
                fraction: Some(x),
                ..
            } => by_step.push((step, x)),
            Event::Progress { .. } => {}
            Event::Done => break,
            e => panic!("unexpected {e:?}"),
        }
    }
    let second: Vec<f32> = by_step
        .iter()
        .filter(|(s, _)| *s == 2)
        .map(|(_, x)| *x)
        .collect();
    assert_eq!(second.first(), Some(&(1.0 / 6.0)), "{by_step:?}");
}

// ---------------------------------------------------------------- reusing the dry run's upload

/// What `sha256sum` (or busybox's) and `wc -c` print for the files the fakes serve, by path.
fn remote_checks(tarball: &'static [u8], script: &'static [u8]) -> impl Fn(&str) -> ExecOutput {
    move |cmd: &str| {
        let bytes = if cmd.contains("helixscreen-") {
            tarball
        } else if cmd.contains("install.sh") {
            script
        } else {
            return ExecOutput {
                stdout: String::new(),
                stderr: format!("{cmd}: not found"),
                exit: 127,
            };
        };
        if cmd.contains("sha256sum") {
            let path = cmd.split('\'').nth(1).unwrap_or_default();
            ExecOutput {
                stdout: format!("{}\n{}  {path}\n", bytes.len(), sha(bytes)),
                stderr: String::new(),
                exit: 0,
            }
        } else {
            ExecOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit: 0,
            }
        }
    }
}

/// `TARBALL` with its last byte changed: the same size, so only the sha256 tells them apart.
const TARBALL_ONE_BYTE_OFF: &[u8] = b"helixscreen tarball bytez";
const COPYING: &str = "Copying the HelixScreen files to the printer";
const ALREADY: &str = "The HelixScreen files are already on the printer";

fn dry_run_then_install(f: &Fakes) -> Runner {
    for _ in 0..2 {
        f.ssh.script_job(vec![
            (&pl(&["platform", "pi"]), None),
            (&pl(&["result", "ok"]), Some(0)),
        ]);
    }
    let steps = json!([
        { "type": "helix_install", "platform": "pi", "dry_run": true },
        { "type": "helix_install", "platform": "pi" }
    ]);
    f.runner("root", steps, json!({}))
}

#[tokio::test]
async fn the_install_after_a_dry_run_reuses_its_upload_when_the_files_still_match() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.set_fallback(remote_checks(TARBALL, INSTALL_SH));
    let mut r = dry_run_then_install(&f);
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");

    let uploads: Vec<String> = f.ssh.uploads().into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        uploads.len(),
        2,
        "uploaded once, for the dry run: {uploads:?}"
    );
    let s = spawned(&f);
    assert_eq!(s.len(), 2);
    assert!(
        s[1].0.contains("'/tmp/helixscreen-pi-v1.0.5.tar.gz'"),
        "{s:?}"
    );
    // The dry run leaves its files for the install, which removes them once.
    assert_eq!(rm_commands(&f), vec![CLEANUP.to_string()]);
    let cmds = f.ssh.commands();
    let install_spawn = cmds.iter().rposition(|c| c.starts_with("sh ")).unwrap();
    let cleanup = cmds.iter().position(|c| c == CLEANUP).unwrap();
    assert!(cleanup > install_spawn, "{cmds:?}");
}

#[tokio::test]
async fn a_changed_remote_file_is_uploaded_again() {
    let f = Fakes::standard(&["pi"]);
    f.ssh
        .set_fallback(remote_checks(b"something else", INSTALL_SH));
    let mut r = dry_run_then_install(&f);
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let tarballs = f
        .ssh
        .uploads()
        .into_iter()
        .filter(|(p, _)| p.contains("helixscreen-pi"))
        .count();
    assert_eq!(tarballs, 2, "{:?}", f.ssh.uploads());
}

#[tokio::test]
async fn a_printer_without_sha256sum_gets_the_upload_again() {
    let f = Fakes::standard(&["pi"]);
    let mut r = dry_run_then_install(&f);
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(f.ssh.uploads().len(), 4);
}

#[tokio::test]
async fn a_dry_run_with_no_install_after_it_still_removes_its_files() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.set_fallback(remote_checks(TARBALL, INSTALL_SH));
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let steps = json!([{ "type": "helix_install", "platform": "pi", "dry_run": true }]);
    let mut r = f.runner("root", steps, json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(rm_commands(&f), vec![CLEANUP.to_string()]);
}

#[tokio::test]
async fn an_install_that_stops_before_uploading_removes_what_the_dry_run_left() {
    // The release has no pi32 build, so the install step stops before its own upload.
    let f = Fakes::standard(&["pi"]);
    f.ssh.set_fallback(remote_checks(TARBALL, INSTALL_SH));
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let steps = json!([
        { "type": "helix_install", "platform": "pi", "dry_run": true },
        { "type": "helix_install", "platform": "pi32" }
    ]);
    let mut r = f.runner("root", steps, json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(
        failure(&ev).contains("no build for platform pi32"),
        "{ev:?}"
    );
    assert_eq!(rm_commands(&f), vec![CLEANUP.to_string()]);
}

#[tokio::test]
async fn the_progress_says_the_files_are_already_on_the_printer_when_they_are_reused() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.set_fallback(remote_checks(TARBALL, INSTALL_SH));
    let mut r = dry_run_then_install(&f);
    let (ev, titles) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let copying = titles.iter().position(|t| t == COPYING);
    let already = titles.iter().position(|t| t == ALREADY);
    assert!(copying.is_some(), "the dry run copies them: {titles:?}");
    assert!(
        already > copying,
        "the install finds them there: {titles:?}"
    );
    assert_eq!(
        titles.iter().filter(|t| *t == COPYING).count(),
        1,
        "{titles:?}"
    );
}

#[tokio::test]
async fn a_changed_remote_file_is_copied_again_and_not_called_already_there() {
    let f = Fakes::standard(&["pi"]);
    f.ssh
        .set_fallback(remote_checks(TARBALL_ONE_BYTE_OFF, INSTALL_SH));
    let mut r = dry_run_then_install(&f);
    let (ev, titles) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(
        titles.iter().filter(|t| *t == COPYING).count(),
        2,
        "{titles:?}"
    );
    assert!(!titles.iter().any(|t| t == ALREADY), "{titles:?}");
}

#[tokio::test]
async fn the_preparing_line_names_no_platform_token() {
    let f = Fakes::standard(&["pi"]);
    let mut r = dry_run_then_install(&f);
    let (_, titles) = settle(&mut r).await;
    assert!(
        titles.contains(&"Preparing to check the printer for HelixScreen".to_string()),
        "{titles:?}"
    );
    assert!(
        titles.contains(&"Preparing to install HelixScreen".to_string()),
        "{titles:?}"
    );
    assert!(!titles.iter().any(|t| t.contains("(pi)")), "{titles:?}");
}

// ---------------------------------------------------------------- the installed version

#[tokio::test]
async fn the_version_read_on_the_printer_wins_over_moonrakers() {
    // Moonraker still reports v1.0.0, but the printer already runs the newest release.
    let f = Fakes::standard(&["pi"]);
    f.ssh.on_command(
        INSTALLED_VERSION_PROBE,
        ExecOutput {
            stdout: "v1.0.5\n".into(),
            stderr: String::new(),
            exit: 0,
        },
    );
    let steps = json!([{ "type": "helix_install", "platform": "pi", "update": true }]);
    let mut r = f.runner("root", steps, json!({ "helix_version": "v1.0.0" }));
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert!(spawned(&f).is_empty(), "{:?}", spawned(&f));
    assert!(f.ssh.uploads().is_empty());
    let note = r.state().done_note.clone().unwrap_or_default();
    assert!(note.contains("1.0.5 is installed"), "{note:?}");
    assert_eq!(r.state().done_kind, Some(DoneKind::UpToDate));
}

#[tokio::test]
async fn moonrakers_version_serves_when_the_printer_cannot_say() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.on_command(
        INSTALLED_VERSION_PROBE,
        ExecOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit: 1,
        },
    );
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let steps = json!([{ "type": "helix_install", "platform": "pi", "update": true }]);
    let mut r = f.runner("root", steps, json!({ "helix_version": "v1.0.4" }));
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(spawned(&f).len(), 1);
}

fn probe_says(f: &Fakes, stdout: &str) {
    f.ssh.on_command(
        INSTALLED_VERSION_PROBE,
        ExecOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            exit: 0,
        },
    );
}

fn update_step() -> Value {
    json!([{ "type": "helix_install", "platform": "pi", "update": true }])
}

#[tokio::test]
async fn with_nothing_running_the_newest_install_found_counts_not_the_first() {
    // An old standalone /opt/helixscreen sits next to the Forge-X payload, which is current.
    let f = Fakes::standard(&["pi"]);
    probe_says(&f, "found v1.0.0\nfound v1.0.5\n");
    let mut r = f.runner("root", update_step(), json!({}));
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert!(spawned(&f).is_empty(), "{:?}", spawned(&f));
    let note = r.state().done_note.clone().unwrap_or_default();
    assert!(note.contains("1.0.5 is installed"), "{note:?}");
    assert_eq!(r.state().done_kind, Some(DoneKind::UpToDate));
}

#[tokio::test]
async fn with_nothing_running_moonrakers_version_counts_when_it_is_the_highest() {
    let f = Fakes::standard(&["pi"]);
    probe_says(&f, "found v1.0.4\n");
    let mut r = f.runner("root", update_step(), json!({ "helix_version": "v1.0.5" }));
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert!(spawned(&f).is_empty(), "{:?}", spawned(&f));
}

#[tokio::test]
async fn the_running_helixscreen_decides_over_every_other_copy_and_moonraker() {
    let f = Fakes::standard(&["pi"]);
    probe_says(&f, "running helix-screen 1.0.4 (9ed38c74d5)\n");
    f.ssh.script_job(vec![
        (&pl(&["platform", "pi"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
    let mut r = f.runner("root", update_step(), json!({ "helix_version": "v1.0.5" }));
    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(spawned(&f).len(), 1, "1.0.4 runs, so 1.0.5 installs");
}

#[test]
fn the_version_probe_reads_the_running_binarys_tree_first_and_knows_the_payload_root() {
    let p = INSTALLED_VERSION_PROBE;
    assert!(p.contains("/opt/config/mod_data/helixscreen "), "{p}");
    assert!(
        p.contains("/opt/config/mod_data/helixscreen_payload_root"),
        "{p}"
    );
    assert!(
        p.contains("/usr/data/config/mod_data/helixscreen_payload_root"),
        "{p}"
    );
    let running = p.find("echo \"running").expect("a running line");
    let scan = p.find("for d in").expect("a scan");
    assert!(running < scan, "{p}");
    // Every copy is reported, not only the first.
    assert!(!p[scan..].contains("exit 0"), "{p}");
}

/// What the version probe printed against a faked printer, and whether it ran the binary.
#[cfg(unix)]
struct ProbeRun {
    stdout: String,
    ran_binary: bool,
    took: Duration,
    /// Every `sleep` the probe started, and whether it still ran once the probe returned.
    sleeps_alive: Vec<bool>,
}

/// Bytes laid out as in a real `helix-screen`'s read-only data: the `--version` handler's format
/// string sits between other NUL-terminated strings, never at the start of a line.
#[cfg(unix)]
const VERSION_HANDLER_BYTES: &str =
    "\0-V\0\0--version\0\0\0helix-screen %s\n\0\0Usage: %s [options]\0";

/// The `helix-screen` a release from 0.9.1 on carries: `--version` prints its version line.
#[cfg(unix)]
fn binary_printing(line: &str) -> String {
    format!("printf '%s' '{line}'\nexit 0\n{VERSION_HANDLER_BYTES}")
}

/// Runs the version probe against a printer faked in a temporary directory: `pidof` finds one
/// `helix-screen`, whose `/proc/<pid>/exe` is `<tree>/bin/helix-screen`, a script with `binary`
/// as its body, and `<tree>/release_info.json` names `release_info_says`.
#[cfg(unix)]
fn run_version_probe_on(binary: &str, release_info_says: &str) -> ProbeRun {
    run_version_probe_with(binary, release_info_says, &[], RUNS_AS_HELIX_SCREEN)
}

/// How the faked printer runs HelixScreen: the program name `pidof` knows it by, and whether
/// `/proc/<pid>/exe` can be read (not for a process with file capabilities, read as another user).
#[cfg(unix)]
#[derive(Clone, Copy)]
struct Running {
    name: &'static str,
    exe_readable: bool,
}

#[cfg(unix)]
const RUNS_AS_HELIX_SCREEN: Running = Running {
    name: "helix-screen",
    exe_readable: true,
};

/// As `run_version_probe_on`, with each of `busybox_applets` run by BusyBox instead of the host
/// and HelixScreen running as `running` says. Any other than the default runs a real process
/// from `<tree>/bin/<name>`, whose `/proc/<pid>/cmdline` the probe reads; the file at that path
/// is then the faked binary.
#[cfg(unix)]
fn run_version_probe_with(
    binary: &str,
    release_info_says: &str,
    busybox_applets: &[&str],
    running: Running,
) -> ProbeRun {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let stubs = dir.path().join("stubs");
    let tree = dir.path().join("helixscreen");
    let ran = dir.path().join("ran");
    let sleeps = dir.path().join("sleeps");
    std::fs::create_dir_all(&stubs).unwrap();
    std::fs::create_dir_all(tree.join("bin")).unwrap();
    let exe = tree.join("bin").join(running.name);
    let script = |path: &std::path::Path, body: String| {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let mut process = None;
    let pid = if running.name == "helix-screen" && running.exe_readable {
        4242
    } else {
        // A link, not a copy: a file this process just wrote can still be open for writing in
        // a child another test is forking, and exec refuses it (ETXTBSY).
        std::os::unix::fs::symlink("/bin/sleep", &exe).unwrap();
        let child = std::process::Command::new(&exe).arg("30").spawn().unwrap();
        let pid = child.id();
        process = Some(child);
        std::fs::remove_file(&exe).unwrap();
        pid
    };
    script(
        &stubs.join("pidof"),
        format!(
            "for n; do [ \"$n\" = '{}' ] && {{ echo {pid}; exit 0; }}; done; exit 1",
            running.name
        ),
    );
    if running.exe_readable {
        script(&stubs.join("readlink"), format!("echo '{}'", exe.display()));
    } else {
        script(
            &stubs.join("readlink"),
            "echo \"readlink: $1: Permission denied\" >&2; exit 1".into(),
        );
    }
    script(
        &stubs.join("sleep"),
        format!("echo $$ >> '{}'\nexec /bin/sleep \"$@\"", sleeps.display()),
    );
    for applet in busybox_applets {
        std::os::unix::fs::symlink(busybox().unwrap(), stubs.join(applet)).unwrap();
    }
    script(&exe, format!(": > '{}'\n{binary}", ran.display()));
    std::fs::write(
        tree.join("release_info.json"),
        format!(r#"{{"project_name":"helixscreen","version":"{release_info_says}"}}"#),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        stubs.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let started = std::time::Instant::now();
    let out = std::process::Command::new("sh")
        .args(["-c", INSTALLED_VERSION_PROBE])
        .env("PATH", path)
        .output()
        .unwrap();
    let took = started.elapsed();
    if let Some(mut child) = process {
        let _ = child.kill();
        let _ = child.wait();
    }
    let sleeps_alive = std::fs::read_to_string(&sleeps)
        .unwrap_or_default()
        .lines()
        .map(|pid| {
            std::process::Command::new("kill")
                .args(["-0", pid])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect();
    ProbeRun {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        ran_binary: ran.exists(),
        took,
        sleeps_alive,
    }
}

#[cfg(unix)]
fn run_version_probe(binary_says: &str, release_info_says: &str) -> String {
    run_version_probe_on(&binary_printing(binary_says), release_info_says).stdout
}

#[cfg(unix)]
#[test]
fn the_running_binarys_own_version_wins_over_a_stale_release_info() {
    // A build deployed by hand over an older install leaves the old release_info.json behind.
    assert_eq!(
        run_version_probe("helix-screen 1.1.0-beta.10 (917215c35c)\n", "v1.0.0"),
        "running helix-screen 1.1.0-beta.10 (917215c35c)\n"
    );
}

#[cfg(unix)]
#[test]
fn release_info_names_the_running_version_when_the_binary_prints_none() {
    assert_eq!(
        run_version_probe("", "v1.1.0-beta.1"),
        "running v1.1.0-beta.1\n"
    );
}

#[cfg(unix)]
#[test]
fn a_line_that_is_not_a_version_does_not_count_as_one() {
    assert_eq!(
        run_version_probe("starting\nhelix-screen 1.1.0 (abc)\n", "v1.1.0-beta.1"),
        "running v1.1.0-beta.1\n"
    );
}

#[cfg(unix)]
#[test]
fn a_binary_older_than_its_version_flag_is_never_run() {
    // Before 0.9.1 `--version` was an unknown argument, and the binary started the UI.
    let run = run_version_probe_on(
        "printf 'helix-screen 0.9.0 (abc)\\n'\nexit 0\n\0-V\0\0Usage: %s [options]\0",
        "v0.9.0",
    );
    assert!(!run.ran_binary, "{}", run.stdout);
    assert_eq!(run.stdout, "running v0.9.0\n");
}

#[cfg(unix)]
#[test]
fn a_binary_that_hangs_is_stopped_and_release_info_answers() {
    let run = run_version_probe_on(
        &format!("exec sleep 60\n{VERSION_HANDLER_BYTES}"),
        "v1.1.0-beta.1",
    );
    assert!(run.ran_binary);
    assert_eq!(run.stdout, "running v1.1.0-beta.1\n");
    assert!(run.took < Duration::from_secs(20), "{:?}", run.took);
}

#[cfg(target_os = "linux")]
#[test]
fn the_egl_build_is_found_running_and_asked_for_its_version() {
    let run = run_version_probe_with(
        &binary_printing("helix-screen 1.1.0-beta.10 (917215c35c)\n"),
        "v1.0.0",
        &[],
        Running {
            name: "helix-screen-egl",
            exe_readable: true,
        },
    );
    assert_eq!(
        run.stdout,
        "running helix-screen 1.1.0-beta.10 (917215c35c)\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_running_binary_whose_exe_link_is_unreadable_is_found_by_its_command_line() {
    // A binary with file capabilities is not dumpable: another user cannot read its exe link.
    for name in ["helix-screen", "helix-screen-egl"] {
        let run = run_version_probe_with(
            &binary_printing("helix-screen 1.1.0-beta.10 (917215c35c)\n"),
            "v1.0.0",
            &[],
            Running {
                name,
                exe_readable: false,
            },
        );
        assert_eq!(
            run.stdout, "running helix-screen 1.1.0-beta.10 (917215c35c)\n",
            "{name}"
        );
    }
}

#[cfg(unix)]
fn busybox() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| std::path::Path::new(d).join("busybox"))
        .find(|p| p.is_file())
}

/// The fleet's printers run BusyBox `grep` and `tr`, whose matching stops at a NUL byte.
#[cfg(unix)]
#[test]
fn busybox_finds_the_version_handler_in_a_binary() {
    if busybox().is_none() {
        // CI installs it on Linux, so a missing one there is a broken setup, not a skip.
        assert!(
            !(cfg!(target_os = "linux") && std::env::var_os("CI").is_some()),
            "busybox is not installed on the Linux CI runner"
        );
        eprintln!("busybox not installed: skipped");
        return;
    }
    let run = run_version_probe_with(
        &binary_printing("helix-screen 1.1.0-beta.10 (917215c35c)\n"),
        "v1.0.0",
        &["grep", "tr"],
        RUNS_AS_HELIX_SCREEN,
    );
    assert_eq!(
        run.stdout,
        "running helix-screen 1.1.0-beta.10 (917215c35c)\n"
    );
}

#[cfg(unix)]
#[test]
fn a_binary_that_exits_before_the_watchdog_leaves_no_watchdog_behind() {
    let run = run_version_probe_on(
        &format!(
            "/bin/sleep 1\n{}",
            binary_printing("helix-screen 1.1.0 (abc)\n")
        ),
        "v1.0.0",
    );
    assert_eq!(run.stdout, "running helix-screen 1.1.0 (abc)\n");
    assert!(!run.sleeps_alive.is_empty(), "the watchdog never slept");
    assert!(!run.sleeps_alive.contains(&true), "{:?}", run.sleeps_alive);
}

#[cfg(unix)]
#[test]
fn a_binary_that_ignores_term_is_killed() {
    let run = run_version_probe_on(
        &format!("trap '' TERM\nwhile :; do sleep 1; done\n{VERSION_HANDLER_BYTES}"),
        "v1.1.0-beta.1",
    );
    assert_eq!(run.stdout, "running v1.1.0-beta.1\n");
    assert!(run.took < Duration::from_secs(20), "{:?}", run.took);
}

#[cfg(unix)]
#[test]
fn the_version_probe_is_valid_sh() {
    let out = std::process::Command::new("sh")
        .args(["-n", "-c", INSTALLED_VERSION_PROBE])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn a_refused_sudo_password_stays_in_the_bundles_redaction_list() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.accept(ADDR, "mks", "");
    sudo_needs_a_password(&f);
    f.ssh.sudo_accepts(SUDO_PW);
    let mut r = f.runner_with_password("mks", "", install_step(json!("pi")), json!({}));
    let _ = settle(&mut r).await;
    let ev = r
        .advance(Some(Input::SudoPassword(Secret::new("typo-pw-77"))))
        .await;
    assert!(
        matches!(
            ev,
            Event::NeedInput(Prompt::SudoPassword { refused: true, .. })
        ),
        "{ev:?}"
    );
    assert!(r.secrets().contains(&"typo-pw-77".to_string()));
}

#[tokio::test]
async fn a_login_sudo_will_not_serve_fails_plainly_instead_of_asking_again() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.accept(ADDR, "mks", "");
    sudo_needs_a_password(&f);
    f.ssh.on_command(
        "sudo -S -p '' true",
        ExecOutput {
            stdout: String::new(),
            stderr: "mks is not in the sudoers file.  This incident will be reported.\n".into(),
            exit: 1,
        },
    );
    let mut r = f.runner_with_password("mks", "", install_step(json!("pi")), json!({}));
    let _ = settle(&mut r).await;
    let ev = r
        .advance(Some(Input::SudoPassword(Secret::new(SUDO_PW))))
        .await;
    match &ev {
        Event::Failed { reason, .. } => {
            assert!(reason.contains("not allowed to use sudo"), "{reason}")
        }
        e => panic!("expected a plain failure, got {e:?}"),
    }
    assert!(f.ssh.uploads().is_empty());
}

#[tokio::test]
async fn a_connection_lost_before_a_file_went_up_never_removes_that_file() {
    // The connection drops at each point of the install in turn; whatever the run then cleans
    // up, it only removes files it uploaded.
    for drop_at in 1..=8 {
        let f = Fakes::standard(&["pi"]);
        f.ssh.drop_after(drop_at);
        let mut r = f.runner("root", install_step(json!("pi")), json!({}));
        let _ = settle(&mut r).await;
        let uploaded: Vec<String> = f.ssh.uploads().into_iter().map(|(p, _)| p).collect();
        for rm in rm_commands(&f) {
            for path in ["/tmp/helixscreen-pi-v1.0.5.tar.gz", "/tmp/install.sh"] {
                assert!(
                    !rm.contains(path) || uploaded.iter().any(|u| u == path),
                    "drop at {drop_at}: {rm} removes {path}, never uploaded ({uploaded:?})"
                );
            }
        }
    }
}
