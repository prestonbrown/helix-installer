// SPDX-License-Identifier: GPL-3.0-or-later

use async_trait::async_trait;
use helix_installer_core::facts::Facts;
use helix_installer_core::manifest::{Login, Manifest, Step};
use helix_installer_core::run_state::{RunState, RunStatus};
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::steps::{StepCtx, StepHandler, StepOutcome};
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeSsh,
};
use helix_installer_core::transport::{Clock, Env, ExecOutput, Host};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

const ADDR: &str = "192.168.1.50";
const PRINTER_DOC: &str = "https://helixscreen.org/docs/install-k1";
const PASSWORD: &str = "s3cret-pw";

struct Fakes {
    moonraker: Arc<FakeMoonraker>,
    ssh: FakeSsh,
    fetch: Arc<FakeFetch>,
    clock: Arc<FakeClock>,
}

impl Fakes {
    fn new() -> Self {
        Fakes {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh: FakeSsh::new(),
            fetch: Arc::new(FakeFetch::new()),
            clock: Arc::new(FakeClock::new()),
        }
    }
    fn env(&self) -> Env {
        Env {
            moonraker: self.moonraker.clone(),
            ssh: Arc::new(self.ssh.clone()),
            drives: Arc::new(FakeDrives::new()),
            fetch: self.fetch.clone(),
            clock: self.clock.clone(),
        }
    }
}

fn manifest_with(steps: Value, artifacts: Value) -> Arc<Manifest> {
    let m: Manifest = serde_json::from_value(json!({
        "manifest_version": 1,
        "min_app_version": "0.1.0",
        "printers": [{
            "id": "k1",
            "name": "Creality K1",
            "identify": [{ "model": "K1" }],
            "recipes": [{ "when": { "state": "stock" }, "recipe": "r" }],
            "restore": "r",
            "fallback": { "refuse": true, "doc": PRINTER_DOC }
        }],
        "artifacts": artifacts,
        "recipes": { "r": steps }
    }))
    .unwrap();
    Arc::new(m)
}

fn manifest(steps: Value) -> Arc<Manifest> {
    manifest_with(steps, json!({}))
}

fn state(run_id: &str, facts: Value) -> RunState {
    let facts: Facts = serde_json::from_value(facts).unwrap();
    RunState::new(run_id, "k1", "r", Host::new(ADDR), facts)
}

/// Advances past every `Progress` event, returning the first event that needs the user or ends
/// the run.
async fn settle(r: &mut Runner, input: Option<Input>) -> Event {
    let mut ev = r.advance(input).await;
    for _ in 0..100 {
        match ev {
            Event::Progress { .. } => ev = r.advance(None).await,
            other => return other,
        }
    }
    panic!("runner kept reporting progress");
}

fn out(stdout: &str) -> ExecOutput {
    ExecOutput {
        stdout: stdout.into(),
        stderr: String::new(),
        exit: 0,
    }
}

/// Stands in for the real `ssh_login` step: holds a fixed login in the runner's memory.
struct TestLogin;

#[async_trait]
impl StepHandler for TestLogin {
    async fn run(&self, ctx: &mut StepCtx<'_>, _step: &Step, _input: Option<Input>) -> StepOutcome {
        ctx.set_login(Login {
            user: "root".into(),
            password: PASSWORD.into(),
        });
        ctx.facts.insert("ssh_user".into(), json!("root"));
        StepOutcome::Done
    }
}

#[tokio::test]
async fn info_require_wait_for_reaches_done() {
    let f = Fakes::new();
    f.moonraker
        .set_printer_info(ADDR, json!({ "state": "ready" }));
    let m = manifest(json!([
        { "type": "info", "text": "Welcome" },
        { "type": "require", "fact": "fw_version", "range": ">=1.3.3.5", "doc": "https://doc/fw" },
        { "type": "wait_for", "target": "moonraker", "timeout_s": 60, "message": "Waiting" }
    ]));
    let mut r = Runner::new(
        f.env(),
        m,
        state("run1", json!({ "fw_version": "1.3.3.46" })),
    );

    match r.advance(None).await {
        Event::NeedInput(Prompt::Info { text, .. }) => assert_eq!(text, "Welcome"),
        e => panic!("expected the info prompt, got {e:?}"),
    }
    assert!(matches!(
        settle(&mut r, Some(Input::Next)).await,
        Event::Done
    ));
    assert_eq!(r.state().step_index, 3);
    assert_eq!(r.state().status, RunStatus::Done);
    assert!(matches!(r.advance(None).await, Event::Done));
}

#[tokio::test]
async fn info_waits_for_next_and_reprompts_on_other_input() {
    let f = Fakes::new();
    let m = manifest(json!([{ "type": "info", "text": "Read this" }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Info { .. })
    ));
    assert!(matches!(
        r.advance(Some(Input::Choice("x".into()))).await,
        Event::NeedInput(Prompt::Info { .. })
    ));
    assert_eq!(r.state().step_index, 0);
    assert!(matches!(r.advance(Some(Input::Next)).await, Event::Done));
}

#[tokio::test]
async fn failing_require_before_commit_refuses_with_doc() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "require", "fact": "fw_version", "range": ">=1.3.3.5", "doc": "https://doc/fw" },
        { "type": "info", "text": "never reached" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({ "fw_version": "1.2.0" })));
    match r.advance(None).await {
        Event::Refused { doc, reason } => {
            assert_eq!(doc, "https://doc/fw");
            assert!(reason.contains("1.2.0"), "{reason}");
        }
        e => panic!("expected Refused, got {e:?}"),
    }
    assert!(!r.state().committed);
    assert!(matches!(r.state().status, RunStatus::Refused { .. }));
    // A refused run stays refused.
    assert!(matches!(r.advance(None).await, Event::Refused { .. }));
}

#[tokio::test]
async fn require_refuses_a_missing_or_unparseable_fact() {
    for facts in [json!({}), json!({ "fw_version": "unknown" })] {
        let f = Fakes::new();
        let m = manifest(json!([
            { "type": "require", "fact": "fw_version", "range": ">=1.0", "doc": "https://doc/fw" }
        ]));
        let mut r = Runner::new(f.env(), m, state("run1", facts));
        assert!(matches!(r.advance(None).await, Event::Refused { .. }));
    }
}

#[tokio::test]
async fn refusal_after_commit_is_a_failure() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "info", "text": "Flash it", "commits": true },
        { "type": "require", "fact": "fw_version", "range": ">=2", "doc": "https://doc/fw" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({ "fw_version": "1.0" })));
    r.advance(None).await;
    match r.advance(Some(Input::Next)).await {
        Event::Failed {
            committed, help, ..
        } => {
            assert!(committed);
            assert_eq!(help.as_deref(), Some("https://doc/fw"));
        }
        e => panic!("expected Failed, got {e:?}"),
    }
}

#[tokio::test]
async fn wait_for_timeout_after_commit_fails_with_help() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "info", "text": "Flash it", "commits": true },
        { "type": "wait_for", "target": "moonraker", "timeout_s": 10, "message": "Waiting",
          "help": "What does the printer screen show?" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    let start = f.clock.now();
    r.advance(None).await;
    match settle(&mut r, Some(Input::Next)).await {
        Event::Failed {
            committed,
            step,
            help,
            ..
        } => {
            assert!(committed);
            assert_eq!(step, 1);
            assert_eq!(help.as_deref(), Some("What does the printer screen show?"));
        }
        e => panic!("expected Failed, got {e:?}"),
    }
    let waited = f.clock.now().duration_since(start).unwrap();
    assert!(
        waited >= Duration::from_secs(10) && waited <= Duration::from_secs(12),
        "waited {waited:?}"
    );
    assert!(matches!(r.state().status, RunStatus::Failed { .. }));
}

#[tokio::test]
async fn wait_for_announces_its_message_before_waiting() {
    let f = Fakes::new();
    f.moonraker.set_printer_info(ADDR, json!({}));
    let m = manifest(json!([
        { "type": "wait_for", "target": "moonraker", "timeout_s": 10, "message": "Do not power off" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    match r.advance(None).await {
        Event::Progress {
            step, total, title, ..
        } => {
            assert_eq!((step, total), (0, 1));
            assert_eq!(title, "Do not power off");
        }
        e => panic!("expected Progress, got {e:?}"),
    }
    assert!(matches!(r.advance(None).await, Event::Done));
}

#[tokio::test]
async fn a_failed_step_is_retried_by_the_next_advance() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "wait_for", "target": "moonraker", "timeout_s": 4, "message": "Waiting" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(
        settle(&mut r, None).await,
        Event::Failed {
            committed: false,
            ..
        }
    ));
    f.moonraker.set_printer_info(ADDR, json!({}));
    assert!(matches!(settle(&mut r, None).await, Event::Done));
}

#[tokio::test]
async fn saved_state_resumes_in_a_new_runner_and_lists_as_unfinished() {
    let f = Fakes::new();
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(json!([
        { "type": "info", "text": "one" },
        { "type": "require", "fact": "fw_version", "range": ">=1", "doc": "https://doc/fw" },
        { "type": "info", "text": "three" }
    ]));
    let mut r = Runner::new(
        f.env(),
        m.clone(),
        state("run-42", json!({ "fw_version": "1.5" })),
    );
    r.advance(None).await;
    assert!(matches!(
        r.advance(Some(Input::Next)).await,
        Event::NeedInput(Prompt::Info { .. })
    ));
    assert_eq!(r.state().step_index, 2);
    r.save(dir.path()).unwrap();
    drop(r);

    let unfinished = Runner::list_unfinished(dir.path());
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].run_id, "run-42");

    let loaded = Runner::load(dir.path(), "run-42").unwrap();
    assert_eq!(loaded.step_index, 2);
    assert_eq!(loaded.host, Host::new(ADDR));
    let mut r = Runner::new(f.env(), m, loaded);
    match r.advance(None).await {
        Event::NeedInput(Prompt::Info { text, .. }) => assert_eq!(text, "three"),
        e => panic!("expected step 3's prompt, got {e:?}"),
    }
    assert!(matches!(r.advance(Some(Input::Next)).await, Event::Done));
    r.save(dir.path()).unwrap();
    assert!(Runner::list_unfinished(dir.path()).is_empty());
}

#[tokio::test]
async fn autosave_records_every_step_without_an_explicit_save() {
    let f = Fakes::new();
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(json!([
        { "type": "info", "text": "one" },
        { "type": "require", "fact": "fw_version", "range": ">=1", "doc": "https://doc/fw" },
        { "type": "info", "text": "three" }
    ]));
    let mut r = Runner::new(f.env(), m, state("auto", json!({ "fw_version": "1.5" })))
        .with_autosave(dir.path().to_path_buf());
    r.advance(None).await;
    r.advance(Some(Input::Next)).await;
    assert_eq!(Runner::load(dir.path(), "auto").unwrap().step_index, 2);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["auto.json".to_string()]);
}

#[tokio::test]
async fn list_unfinished_skips_done_and_refused_runs() {
    let f = Fakes::new();
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(json!([{ "type": "info", "text": "x" }]));
    let statuses = [
        ("a-running", RunStatus::Running),
        ("b-done", RunStatus::Done),
        (
            "c-refused",
            RunStatus::Refused {
                doc: "d".into(),
                reason: "r".into(),
            },
        ),
        (
            "d-failed",
            RunStatus::Failed {
                reason: "r".into(),
                help: None,
            },
        ),
    ];
    for (id, status) in statuses {
        let mut s = state(id, json!({}));
        s.status = status;
        Runner::new(f.env(), m.clone(), s).save(dir.path()).unwrap();
    }
    std::fs::write(dir.path().join("junk.json"), "not json").unwrap();
    let ids: Vec<String> = Runner::list_unfinished(dir.path())
        .into_iter()
        .map(|s| s.run_id)
        .collect();
    assert_eq!(ids, vec!["a-running".to_string(), "d-failed".to_string()]);
}

#[tokio::test]
async fn save_rejects_a_run_id_that_is_not_a_plain_name() {
    let f = Fakes::new();
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(json!([]));
    for id in ["../escape", "a/b", ""] {
        let r = Runner::new(f.env(), m.clone(), state(id, json!({})));
        assert!(r.save(dir.path()).is_err(), "{id:?} was accepted");
    }
}

#[tokio::test]
async fn the_ssh_password_never_reaches_saved_state() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", PASSWORD);
    f.ssh.on_command("cat /etc/model", out("K1\n"));
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(json!([
        { "type": "ssh_login", "logins": [{ "user": "root", "password": PASSWORD }] },
        { "type": "probe", "ssh": "cat /etc/model", "fact": "model" },
        { "type": "info", "text": "pause" }
    ]));
    let mut r = Runner::new(f.env(), m, state("pw", json!({})))
        .with_handler("ssh_login", Arc::new(TestLogin));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Info { .. })
    ));
    // The probe could only have logged in with the password the runner holds in memory.
    assert_eq!(r.state().facts.get("model"), Some(&json!("K1")));
    r.save(dir.path()).unwrap();
    let saved = std::fs::read_to_string(dir.path().join("pw.json")).unwrap();
    assert!(!saved.to_lowercase().contains("password"), "{saved}");
    assert!(!saved.contains(PASSWORD), "{saved}");
}

#[tokio::test]
async fn an_unregistered_step_type_fails_as_unsupported() {
    let f = Fakes::new();
    let m = manifest(json!([{ "type": "ssh_run", "command": "true", "timeout_s": 5 }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    match r.advance(None).await {
        Event::Failed { reason, .. } => assert_eq!(reason, "unsupported step ssh_run"),
        e => panic!("expected Failed, got {e:?}"),
    }
}

async fn model_check(after_commit: &str) -> Event {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", PASSWORD);
    f.ssh.on_command("cat /etc/model", out("K1\n"));
    let m = manifest(json!([
        { "type": "ssh_login", "logins": [{ "user": "root", "password": PASSWORD }] },
        { "type": "probe", "ssh": "cat /etc/model", "fact": "model" },
        { "type": "info", "text": "Flash it", "commits": true },
        { "type": "probe", "ssh": "cat /etc/model", "fact": "model" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})))
        .with_handler("ssh_login", Arc::new(TestLogin));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Info { .. })
    ));
    f.ssh.on_command("cat /etc/model", out(after_commit));
    r.advance(Some(Input::Next)).await
}

#[tokio::test]
async fn a_model_change_after_commit_stops_the_recipe() {
    match model_check("K2\n").await {
        Event::Failed {
            committed, reason, ..
        } => {
            assert!(committed);
            assert!(
                reason.contains("this is not the printer the install started on"),
                "{reason}"
            );
        }
        e => panic!("expected Failed, got {e:?}"),
    }
    assert!(matches!(model_check("K1\n").await, Event::Done));
}

#[tokio::test]
async fn a_model_change_before_commit_is_not_checked() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", PASSWORD);
    f.ssh.on_command("cat /etc/model", out("K2\n"));
    let m = manifest(json!([
        { "type": "ssh_login", "logins": [{ "user": "root", "password": PASSWORD }] },
        { "type": "probe", "ssh": "cat /etc/model", "fact": "model" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({ "model": "K1" })))
        .with_handler("ssh_login", Arc::new(TestLogin));
    assert!(matches!(r.advance(None).await, Event::Done));
    assert_eq!(r.state().facts.get("model"), Some(&json!("K2")));
}

#[tokio::test]
async fn a_serial_change_after_a_resume_still_stops_the_recipe() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", PASSWORD);
    f.ssh.on_command("cat /etc/serial", out("AAA\n"));
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(json!([
        { "type": "info", "text": "Flash it", "commits": true },
        { "type": "info", "text": "pause" },
        { "type": "ssh_login", "logins": [{ "user": "root", "password": PASSWORD }] },
        { "type": "probe", "ssh": "cat /etc/serial", "fact": "serial" }
    ]));
    let mut r = Runner::new(
        f.env(),
        m.clone(),
        state("run1", json!({ "serial": "BBB" })),
    );
    r.advance(None).await;
    r.advance(Some(Input::Next)).await;
    r.save(dir.path()).unwrap();
    let mut r = Runner::new(f.env(), m, Runner::load(dir.path(), "run1").unwrap())
        .with_handler("ssh_login", Arc::new(TestLogin));
    match r.advance(Some(Input::Next)).await {
        Event::Failed {
            committed, reason, ..
        } => {
            assert!(committed);
            assert!(reason.contains("not the printer"), "{reason}");
        }
        e => panic!("expected Failed, got {e:?}"),
    }
}

#[tokio::test]
async fn cancel_before_commit_refuses_with_the_printer_guide() {
    let f = Fakes::new();
    let m = manifest(json!([{ "type": "info", "text": "x" }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    r.advance(None).await;
    match r.advance(Some(Input::Cancel)).await {
        Event::Refused { doc, .. } => assert_eq!(doc, PRINTER_DOC),
        e => panic!("expected Refused, got {e:?}"),
    }
}

#[tokio::test]
async fn cancel_after_commit_is_a_committed_failure() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "info", "text": "x", "commits": true },
        { "type": "info", "text": "y" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    r.advance(None).await;
    r.advance(Some(Input::Next)).await;
    assert!(matches!(
        r.advance(Some(Input::Cancel)).await,
        Event::Failed {
            committed: true,
            step: 1,
            ..
        }
    ));
}

#[tokio::test]
async fn a_prompt_alone_does_not_commit() {
    let f = Fakes::new();
    let m = manifest(json!([{ "type": "user_action", "text": "Flash", "commits": true }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::UserAction { .. })
    ));
    assert!(!r.state().committed);
    assert!(matches!(r.advance(Some(Input::Next)).await, Event::Done));
    assert!(r.state().committed);
}

#[tokio::test]
async fn ask_sets_the_fact_and_rejects_unknown_choices() {
    let f = Fakes::new();
    let m = manifest(json!([{
        "type": "ask", "fact": "is_pro", "question": "Pro?",
        "choices": [{ "value": "yes", "label": "Yes" }, { "value": "no", "label": "No" }]
    }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    match r.advance(None).await {
        Event::NeedInput(Prompt::Ask { question, choices }) => {
            assert_eq!(question, "Pro?");
            assert_eq!(choices.len(), 2);
        }
        e => panic!("expected Ask, got {e:?}"),
    }
    assert!(matches!(
        r.advance(Some(Input::Choice("maybe".into()))).await,
        Event::NeedInput(Prompt::Ask { .. })
    ));
    assert!(matches!(
        r.advance(Some(Input::Choice("yes".into()))).await,
        Event::Done
    ));
    assert_eq!(r.state().facts.get("is_pro"), Some(&json!("yes")));
}

#[tokio::test]
async fn user_action_verify_failure_fails_then_reprompts() {
    let f = Fakes::new();
    let m = manifest(json!([{
        "type": "user_action", "text": "Enable root",
        "verify": { "moonraker": "printer_info" }
    }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    r.advance(None).await;
    assert!(matches!(
        r.advance(Some(Input::Next)).await,
        Event::Failed {
            committed: false,
            ..
        }
    ));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::UserAction { .. })
    ));
    f.moonraker
        .set_printer_info(ADDR, json!({ "hostname": "k1" }));
    assert!(matches!(r.advance(Some(Input::Next)).await, Event::Done));
    assert_eq!(r.state().facts.get("has_moonraker"), Some(&json!(true)));
}

#[tokio::test]
async fn moonraker_probe_records_system_facts() {
    let f = Fakes::new();
    f.moonraker.set_system_info(
        ADDR,
        json!({ "system_info": {
            "cpu_info": { "processor": "aarch64" },
            "distribution": { "name": "Debian GNU/Linux 12" },
            "available_services": ["klipper", "moonraker"]
        }}),
    );
    let m = manifest(json!([{ "type": "probe", "moonraker": "system_info" }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(r.advance(None).await, Event::Done));
    let facts = &r.state().facts;
    assert_eq!(facts.get("cpu_arch"), Some(&json!("aarch64")));
    assert_eq!(
        facts.get("os_distribution"),
        Some(&json!("Debian GNU/Linux 12"))
    );
    assert_eq!(
        facts.get("services"),
        Some(&json!(["klipper", "moonraker"]))
    );
}

#[tokio::test]
async fn an_unreachable_probe_fails() {
    let f = Fakes::new();
    let m = manifest(json!([{ "type": "probe", "moonraker": "printer_info" }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(r.advance(None).await, Event::Failed { .. }));
}

#[tokio::test]
async fn moonraker_steps_send_gcode_and_upgrades_to_the_pinned_host() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "moonraker_gcode", "script": "DISPLAY_OFF HELIX=1" },
        { "type": "moonraker_update", "name": "helixscreen" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(r.advance(None).await, Event::Done));
    assert_eq!(
        f.moonraker.gcodes(),
        vec![(ADDR.to_string(), "DISPLAY_OFF HELIX=1".to_string())]
    );
    assert_eq!(f.moonraker.upgrades(), vec!["helixscreen".to_string()]);
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn download_fetches_into_the_cache_and_fails_on_a_bad_hash() {
    let body = b"forge-x image".to_vec();
    for (sha, ok) in [(hex(&body), true), (hex(b"other"), false)] {
        let f = Fakes::new();
        f.fetch.add("https://example.test/fx.tgz", body.clone());
        let cache = tempfile::tempdir().unwrap();
        let m = manifest_with(
            json!([{ "type": "download", "artifact": "fx" }]),
            json!({ "fx": { "url": "https://example.test/fx.tgz", "sha256": sha } }),
        );
        let mut r = Runner::new(f.env(), m, state("run1", json!({})))
            .with_cache_dir(cache.path().to_path_buf());
        let ev = settle(&mut r, None).await;
        if ok {
            assert!(matches!(ev, Event::Done), "{ev:?}");
            assert!(cache.path().join(&sha).exists());
        } else {
            assert!(matches!(ev, Event::Failed { .. }), "{ev:?}");
        }
    }
}

#[tokio::test]
async fn download_of_an_artifact_missing_from_the_manifest_fails() {
    let f = Fakes::new();
    let cache = tempfile::tempdir().unwrap();
    let m = manifest(json!([{ "type": "download", "artifact": "nope" }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})))
        .with_cache_dir(cache.path().to_path_buf());
    assert!(matches!(settle(&mut r, None).await, Event::Failed { .. }));
}

#[tokio::test]
async fn events_serialize_with_a_kind_tag() {
    let ev = Event::NeedInput(Prompt::Info {
        text: "hi".into(),
        image: None,
    });
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["event"], "need_input");
    assert_eq!(v["prompt"], "info");
    assert_eq!(v["text"], "hi");
    let input: Input =
        serde_json::from_value(json!({ "input": "choice", "value": "yes" })).unwrap();
    assert!(matches!(input, Input::Choice(c) if c == "yes"));
}
