// SPDX-License-Identifier: GPL-3.0-or-later

use async_trait::async_trait;
use helix_installer_core::bundle::support_bundle;
use helix_installer_core::facts::Facts;
use helix_installer_core::manifest::{Channel, Login, Manifest, Step};
use helix_installer_core::run_state::RunState;
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::steps::helix_install::{
    releases_manifest_url, GITHUB_RELEASE_API, HELIX_INSTALL_SILENCE,
};
use helix_installer_core::steps::progress::{parse_progress_line, ProgressEvent, StepStatus};
use helix_installer_core::steps::{StepCtx, StepHandler, StepOutcome};
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
};
use helix_installer_core::transport::{Clock, Env, ExecOutput, Host, Ssh};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const ADDR: &str = "192.168.1.50";
const PASSWORD: &str = "s3cret-pw";
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
            "fallback": { "refuse": true, "doc": "https://helixscreen.org/docs/install-k1" }
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
        ctx.set_login(Login {
            user: self.0.clone(),
            password: self.1.clone(),
        });
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
    let mut r = f.runner("root", steps, json!({}));

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
    let mut r = f.runner("root", steps, json!({}));

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
    assert!(
        others.iter().all(|(_, stdin)| stdin.is_none()),
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
async fn a_non_root_login_with_no_password_and_no_passwordless_sudo_stops_before_uploading() {
    let f = Fakes::standard(&["pi"]);
    f.ssh.accept(ADDR, "mks", "");
    sudo_needs_a_password(&f);
    let mut r = f.runner_with_password("mks", "", install_step(json!("pi")), json!({}));

    let (ev, _) = settle(&mut r).await;
    match &ev {
        Event::Failed { reason, help, .. } => {
            assert!(reason.contains("passwordless sudo"), "{reason}");
            assert!(
                help.as_deref()
                    .unwrap_or("")
                    .contains("https://helixscreen.org/docs/install"),
                "{help:?}"
            );
        }
        e => panic!("expected Failed, got {e:?}"),
    }
    assert!(f.ssh.uploads().is_empty());
    assert!(spawned(&f).is_empty());
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
    let login = || Login {
        user: "root".into(),
        password: PASSWORD.into(),
    };
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
async fn ssh_run_for_a_login_with_no_password_runs_as_the_login_user() {
    let f = Fakes::standard(&[]);
    f.ssh.accept(ADDR, "mks", "");
    sudo_needs_a_password(&f);
    f.ssh.script_job(vec![("", Some(0))]);
    let mut r = f.runner_with_password("mks", "", ssh_run("ls", 60), json!({}));

    let (ev, _) = settle(&mut r).await;
    assert!(matches!(ev, Event::Done), "{ev:?}");
    assert_eq!(job_calls(&f, "ls"), vec![("ls".to_string(), None)]);
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
fn the_channel_defaults_to_stable_and_names_its_manifest() {
    let step: Step = serde_json::from_value(channel_step(None)[0].clone()).unwrap();
    assert!(matches!(
        step,
        Step::HelixInstall {
            channel: Channel::Stable,
            ..
        }
    ));
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
async fn a_recipe_without_a_channel_reads_the_stable_list() {
    let f = Fakes::standard(&["pi"]);
    let r = f.runner("root", channel_step(None), json!({}));
    install_with(&f, r).await;
    assert_eq!(
        release_list_requests(&f),
        vec![releases_manifest_url(Channel::Stable)]
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

fn rm_commands(f: &Fakes) -> Vec<String> {
    f.ssh
        .commands()
        .into_iter()
        .filter(|c| c.starts_with("rm "))
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
