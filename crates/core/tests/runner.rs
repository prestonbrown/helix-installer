// SPDX-License-Identifier: GPL-3.0-or-later

use async_trait::async_trait;
use helix_installer_core::facts::Facts;
use helix_installer_core::manifest::{Login, Manifest, Step};
use helix_installer_core::run_state::{recipe_sha256, RunState, RunStatus};
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::steps::{StepCtx, StepHandler, StepOutcome};
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
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
    net: Arc<FakeNet>,
}

impl Fakes {
    fn new() -> Self {
        Fakes {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh: FakeSsh::new(),
            fetch: Arc::new(FakeFetch::new()),
            clock: Arc::new(FakeClock::new()),
            net: Arc::new(FakeNet::new()),
        }
    }
    fn env(&self) -> Env {
        Env {
            moonraker: self.moonraker.clone(),
            ssh: Arc::new(self.ssh.clone()),
            drives: Arc::new(FakeDrives::new()),
            fetch: self.fetch.clone(),
            clock: self.clock.clone(),
            net: self.net.clone(),
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
        Event::Refused {
            doc,
            reason,
            cancelled,
        } => {
            assert!(!cancelled, "a refusal the recipe made is not a cancel");
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
                cancelled: false,
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
        Event::Refused { doc, cancelled, .. } => {
            assert_eq!(doc, PRINTER_DOC);
            assert!(cancelled);
        }
        e => panic!("expected Refused, got {e:?}"),
    }
    assert!(matches!(
        r.state().status,
        RunStatus::Refused {
            cancelled: true,
            ..
        }
    ));
    // The saved run still says it was cancelled when it is opened again.
    assert!(matches!(
        r.advance(None).await,
        Event::Refused {
            cancelled: true,
            ..
        }
    ));
}

#[test]
fn a_saved_refusal_without_the_cancelled_flag_is_not_a_cancel() {
    let s: RunStatus =
        serde_json::from_value(json!({ "status": "refused", "doc": "d", "reason": "r" })).unwrap();
    assert_eq!(
        s,
        RunStatus::Refused {
            doc: "d".into(),
            reason: "r".into(),
            cancelled: false,
        }
    );
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

/// A step handler that never finishes, standing in for the app dying mid-step.
struct Hang;

#[async_trait]
impl StepHandler for Hang {
    async fn run(&self, _: &mut StepCtx<'_>, _: &Step, _: Option<Input>) -> StepOutcome {
        std::future::pending().await
    }
}

/// Fails its first `fail_first` runs, then succeeds; counts every run.
struct Flaky {
    fail_first: usize,
    runs: std::sync::Mutex<usize>,
}

impl Flaky {
    fn new(fail_first: usize) -> Arc<Flaky> {
        Arc::new(Flaky {
            fail_first,
            runs: std::sync::Mutex::new(0),
        })
    }
    fn runs(&self) -> usize {
        *self.runs.lock().unwrap()
    }
}

#[async_trait]
impl StepHandler for Flaky {
    async fn run(&self, _: &mut StepCtx<'_>, _: &Step, _: Option<Input>) -> StepOutcome {
        let mut n = self.runs.lock().unwrap();
        *n += 1;
        if *n <= self.fail_first {
            StepOutcome::failed("printer said no")
        } else {
            StepOutcome::Done
        }
    }
}

fn is_rerun_question(ev: &Event) -> bool {
    matches!(ev, Event::NeedInput(Prompt::Ask { question, choices })
        if question.contains("may already have run")
            && choices.iter().map(|c| c.value.as_str()).collect::<Vec<_>>() == ["retry", "stop"])
}

#[tokio::test]
async fn a_crash_inside_a_committing_step_resumes_as_committed() {
    let f = Fakes::new();
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(json!([
        { "type": "moonraker_gcode", "script": "FLASH", "commits": true }
    ]));
    let mut r = Runner::new(f.env(), m.clone(), state("crash", json!({})))
        .with_handler("moonraker_gcode", Arc::new(Hang))
        .with_autosave(dir.path().to_path_buf());
    let timed_out = tokio::time::timeout(Duration::from_millis(100), r.advance(None)).await;
    assert!(timed_out.is_err());
    drop(r);

    let saved = Runner::load(dir.path(), "crash").unwrap();
    assert!(saved.committed);
    assert_eq!(saved.started_step, Some(0));
    let mut r = Runner::new(f.env(), m, saved).with_autosave(dir.path().to_path_buf());
    let ev = r.advance(None).await;
    assert!(is_rerun_question(&ev), "{ev:?}");
    assert!(matches!(
        r.advance(Some(Input::Cancel)).await,
        Event::Failed {
            committed: true,
            ..
        }
    ));
    let ids: Vec<_> = Runner::list_unfinished(dir.path())
        .into_iter()
        .map(|s| s.run_id)
        .collect();
    assert_eq!(ids, vec!["crash".to_string()]);
    assert!(f.moonraker.gcodes().is_empty());
}

#[tokio::test]
async fn a_committing_user_action_commits_on_confirm_even_if_verify_fails() {
    let f = Fakes::new();
    let m = manifest(json!([{
        "type": "user_action", "text": "Flash", "commits": true,
        "verify": { "moonraker": "printer_info" }
    }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    r.advance(None).await;
    assert!(!r.state().committed);
    assert!(matches!(
        r.advance(Some(Input::Next)).await,
        Event::Failed {
            committed: true,
            ..
        }
    ));
}

#[tokio::test]
async fn retrying_a_non_idempotent_step_asks_first_and_retry_runs_it() {
    let f = Fakes::new();
    let flaky = Flaky::new(1);
    let m = manifest(json!([{ "type": "moonraker_gcode", "script": "X" }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})))
        .with_handler("moonraker_gcode", flaky.clone());
    assert!(matches!(r.advance(None).await, Event::Failed { .. }));
    let ev = r.advance(None).await;
    assert!(is_rerun_question(&ev), "{ev:?}");
    assert_eq!(flaky.runs(), 1);
    // Any other answer asks again.
    assert!(is_rerun_question(&r.advance(Some(Input::Next)).await));
    assert!(matches!(
        r.advance(Some(Input::Choice("retry".into()))).await,
        Event::Done
    ));
    assert_eq!(flaky.runs(), 2);
    assert_eq!(r.state().started_step, None);
}

#[tokio::test]
async fn retrying_a_non_idempotent_step_can_stop_instead() {
    let f = Fakes::new();
    let flaky = Flaky::new(1);
    let m = manifest(json!([
        { "type": "info", "text": "go", "commits": true },
        { "type": "ssh_run", "command": "x", "timeout_s": 5 }
    ]));
    let mut r =
        Runner::new(f.env(), m, state("run1", json!({}))).with_handler("ssh_run", flaky.clone());
    r.advance(None).await;
    assert!(matches!(
        r.advance(Some(Input::Next)).await,
        Event::Failed { .. }
    ));
    assert!(is_rerun_question(&r.advance(None).await));
    assert!(matches!(
        r.advance(Some(Input::Choice("stop".into()))).await,
        Event::Failed {
            committed: true,
            step: 1,
            ..
        }
    ));
    assert_eq!(flaky.runs(), 1);
}

#[tokio::test]
async fn an_idempotent_step_is_not_questioned_on_retry() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "probe", "moonraker": "printer_info" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(r.advance(None).await, Event::Failed { .. }));
    f.moonraker.set_printer_info(ADDR, json!({}));
    assert!(matches!(r.advance(None).await, Event::Done));
}

async fn resume_with_changed_manifest(commit: bool, change_recipe: bool) -> Event {
    let f = Fakes::new();
    let dir = tempfile::tempdir().unwrap();
    let steps = json!([
        { "type": "info", "text": "one", "commits": commit },
        { "type": "info", "text": "two" }
    ]);
    let m = manifest(steps);
    let mut r = Runner::new(f.env(), m.clone(), state("run1", json!({})));
    r.advance(None).await;
    r.advance(Some(Input::Next)).await;
    r.save(dir.path()).unwrap();
    let changed = if change_recipe {
        manifest(json!([
            { "type": "info", "text": "one", "commits": commit },
            { "type": "info", "text": "two, reworded" }
        ]))
    } else {
        let mut other = (*m).clone();
        other.sha256 = "0".repeat(64);
        Arc::new(other)
    };
    let mut r = Runner::new(f.env(), changed, Runner::load(dir.path(), "run1").unwrap());
    r.advance(Some(Input::Next)).await
}

#[tokio::test]
async fn a_changed_recipe_or_manifest_refuses_a_resume_before_commit() {
    for change_recipe in [true, false] {
        match resume_with_changed_manifest(false, change_recipe).await {
            Event::Refused { doc, reason, .. } => {
                assert_eq!(doc, PRINTER_DOC);
                assert!(reason.contains("start again"), "{reason}");
            }
            e => panic!("expected Refused, got {e:?}"),
        }
    }
}

#[tokio::test]
async fn a_changed_recipe_or_manifest_fails_a_resume_after_commit() {
    for change_recipe in [true, false] {
        match resume_with_changed_manifest(true, change_recipe).await {
            Event::Failed {
                committed, help, ..
            } => {
                assert!(committed);
                assert!(help.unwrap_or_default().contains("recovery guide"));
            }
            e => panic!("expected Failed, got {e:?}"),
        }
    }
}

#[tokio::test]
async fn a_step_index_past_the_end_is_never_done() {
    let f = Fakes::new();
    let m = manifest(json!([{ "type": "info", "text": "one" }]));
    let mut s = state("run1", json!({}));
    s.manifest_sha256 = Some(m.sha256.clone());
    s.recipe_sha256 = Some(recipe_sha256(&m.recipes["r"]));
    s.step_index = 5;
    let mut r = Runner::new(f.env(), m, s);
    assert!(matches!(r.advance(None).await, Event::Failed { .. }));
}

#[tokio::test]
async fn an_unpinned_state_part_way_through_is_not_trusted() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "info", "text": "one" },
        { "type": "info", "text": "two" }
    ]));
    let mut s = state("run1", json!({}));
    s.step_index = 1;
    let mut r = Runner::new(f.env(), m, s);
    assert!(matches!(r.advance(None).await, Event::Refused { .. }));
}

#[tokio::test]
async fn wait_for_ssh_reads_the_banner_without_logging_in() {
    let f = Fakes::new();
    f.net.answer(ADDR, 22, Some("SSH-2.0-dropbear_2022.83"));
    let m = manifest(json!([
        { "type": "wait_for", "target": "ssh", "timeout_s": 10, "message": "w" }
    ]));
    let mut r = Runner::new(f.env(), m.clone(), state("run1", json!({})));
    assert!(matches!(settle(&mut r, None).await, Event::Done));

    let f = Fakes::new();
    f.net.answer(ADDR, 22, Some("HTTP/1.1 400 Bad Request"));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(settle(&mut r, None).await, Event::Failed { .. }));
}

#[tokio::test]
async fn wait_for_port_needs_only_a_connection() {
    let m = manifest(json!([
        { "type": "wait_for", "target": { "port": 8080 }, "timeout_s": 10, "message": "w" }
    ]));
    let f = Fakes::new();
    f.net.answer(ADDR, 8080, None);
    let mut r = Runner::new(f.env(), m.clone(), state("run1", json!({})));
    assert!(matches!(settle(&mut r, None).await, Event::Done));

    let f = Fakes::new();
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    assert!(matches!(settle(&mut r, None).await, Event::Failed { .. }));
}

#[tokio::test]
async fn tcp_port_probe_records_whether_the_port_answers() {
    let m = manifest(json!([
        { "type": "probe", "tcp_port": 4408, "fact": "fluidd_open" }
    ]));
    for open in [true, false] {
        let f = Fakes::new();
        if open {
            f.net.answer(ADDR, 4408, None);
        }
        let mut r = Runner::new(f.env(), m.clone(), state("run1", json!({})));
        assert!(matches!(r.advance(None).await, Event::Done));
        assert_eq!(r.state().facts.get("fluidd_open"), Some(&json!(open)));
    }
}

fn helix_wait() -> Arc<Manifest> {
    manifest(json!([
        { "type": "wait_for", "target": "helix_service", "timeout_s": 10, "message": "w",
          "help": "Power-cycle the printer" }
    ]))
}

#[tokio::test]
async fn wait_for_helix_service_reads_moonraker_service_state() {
    let f = Fakes::new();
    f.moonraker.set_system_info(
        ADDR,
        json!({ "system_info": { "service_state": {
            "helixscreen": { "active_state": "active", "sub_state": "running" }
        }}}),
    );
    let mut r = Runner::new(f.env(), helix_wait(), state("run1", json!({})));
    assert!(matches!(settle(&mut r, None).await, Event::Done));
}

#[tokio::test]
async fn wait_for_helix_service_falls_back_to_pidof_over_a_held_login() {
    let f = Fakes::new();
    f.moonraker
        .set_system_info(ADDR, json!({ "system_info": { "service_state": {} } }));
    f.ssh.accept(ADDR, "root", PASSWORD);
    f.ssh.on_command("pidof helix-screen", out("1234\n"));
    let m = manifest(json!([
        { "type": "ssh_login", "logins": [{ "user": "root", "password": PASSWORD }] },
        { "type": "wait_for", "target": "helix_service", "timeout_s": 10, "message": "w" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})))
        .with_handler("ssh_login", Arc::new(TestLogin));
    assert!(matches!(settle(&mut r, None).await, Event::Done));
    assert!(f.ssh.commands().contains(&"pidof helix-screen".to_string()));
}

#[tokio::test]
async fn wait_for_helix_service_asks_the_user_when_nothing_can_tell() {
    for (answer, done) in [("yes", true), ("no", false)] {
        let f = Fakes::new();
        f.moonraker
            .set_system_info(ADDR, json!({ "system_info": { "service_state": {} } }));
        let mut r = Runner::new(f.env(), helix_wait(), state("run1", json!({})));
        match settle(&mut r, None).await {
            Event::NeedInput(Prompt::Ask { question, choices }) => {
                assert!(question.contains("Is HelixScreen showing"), "{question}");
                let values: Vec<_> = choices.iter().map(|c| c.value.as_str()).collect();
                assert_eq!(values, ["yes", "no"]);
            }
            e => panic!("expected Ask, got {e:?}"),
        }
        let ev = r.advance(Some(Input::Choice(answer.into()))).await;
        if done {
            assert!(matches!(ev, Event::Done), "{ev:?}");
        } else {
            match ev {
                Event::Failed { help, .. } => {
                    assert_eq!(help.as_deref(), Some("Power-cycle the printer"))
                }
                e => panic!("expected Failed, got {e:?}"),
            }
        }
    }
}

#[test]
fn a_login_never_prints_its_password() {
    let l = Login {
        user: "root".into(),
        password: PASSWORD.into(),
    };
    let shown = format!("{l:?} {:?}", Input::Login(l.clone()));
    assert!(shown.contains("root"));
    assert!(!shown.contains(PASSWORD), "{shown}");
}

/// Refuses after replacing the autosave directory with a file, so the save that records the
/// refusal fails.
struct RefuseAndBreakSaves(std::path::PathBuf);

#[async_trait]
impl StepHandler for RefuseAndBreakSaves {
    async fn run(&self, _: &mut StepCtx<'_>, _: &Step, _: Option<Input>) -> StepOutcome {
        std::fs::remove_dir_all(&self.0).unwrap();
        std::fs::write(&self.0, "not a directory").unwrap();
        StepOutcome::Refused {
            doc: "https://doc/fw".into(),
            reason: "too old".into(),
        }
    }
}

#[tokio::test]
async fn a_save_error_does_not_turn_a_refusal_into_a_failure() {
    let f = Fakes::new();
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let m = manifest(json!([
        { "type": "require", "fact": "fw_version", "range": ">=2", "doc": "https://doc/fw" }
    ]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({ "fw_version": "1" })))
        .with_handler("require", Arc::new(RefuseAndBreakSaves(runs.clone())))
        .with_autosave(runs);
    let ev = r.advance(None).await;
    assert!(
        matches!(ev, Event::Failed { ref reason, .. } if reason.contains("save")),
        "{ev:?}"
    );
    assert!(matches!(r.state().status, RunStatus::Refused { .. }));
}

#[tokio::test]
async fn committing_without_a_model_or_serial_is_logged() {
    let f = Fakes::new();
    let m = manifest(json!([{ "type": "info", "text": "go", "commits": true }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    r.advance(None).await;
    r.advance(Some(Input::Next)).await;
    assert!(
        r.state()
            .log
            .iter()
            .any(|e| e.text.contains("cannot be re-identified")),
        "{:?}",
        r.state().log
    );
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
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
    minisign::sign(None, &sk, std::io::Cursor::new(bytes), None, None)
        .unwrap()
        .into_string()
}

const APP: &str = "0.1.0";

fn signed_manifest(second_text: &str, commit: bool) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "manifest_version": 1,
        "min_app_version": APP,
        "printers": [{
            "id": "k1",
            "name": "Creality K1",
            "identify": [{ "model": "K1" }],
            "recipes": [{ "when": { "state": "stock" }, "recipe": "r" }],
            "restore": "r",
            "fallback": { "refuse": true, "doc": PRINTER_DOC }
        }],
        "artifacts": {},
        "recipes": { "r": [
            { "type": "info", "text": "one", "commits": commit },
            { "type": "info", "text": second_text }
        ]}
    }))
    .unwrap()
}

/// Starts a run on manifest A and leaves it saved at its second step.
async fn started_run(f: &Fakes, dir: &std::path::Path, commit: bool) {
    let a = signed_manifest("two", commit);
    let mut r = Runner::start(
        f.env(),
        dir,
        &a,
        &sign(&a),
        &pubkey(),
        APP,
        state("run1", json!({})),
    )
    .unwrap();
    r.advance(None).await;
    assert!(matches!(
        r.advance(Some(Input::Next)).await,
        Event::NeedInput(Prompt::Info { .. })
    ));
}

#[tokio::test]
async fn a_resumed_run_uses_its_own_manifest_copy_not_the_newest() {
    let f = Fakes::new();
    let dir = tempfile::tempdir().unwrap();
    started_run(&f, dir.path(), true).await;
    assert!(dir.path().join("run1.manifest.json").exists());
    assert!(dir.path().join("run1.manifest.json.minisig").exists());

    // A newer manifest is published to the shared cache.
    let cache_dir = tempfile::tempdir().unwrap();
    let b = signed_manifest("two, reworded", true);
    helix_installer_core::manifest::ManifestCache::new(
        cache_dir.path().to_path_buf(),
        &pubkey(),
        APP,
    )
    .store(&b, &sign(&b), std::time::SystemTime::now())
    .unwrap();

    let ids: Vec<_> = Runner::list_unfinished(dir.path())
        .into_iter()
        .map(|s| s.run_id)
        .collect();
    assert_eq!(ids, vec!["run1".to_string()]);

    let mut r = Runner::resume(f.env(), dir.path(), "run1", &pubkey(), APP).unwrap();
    match r.advance(None).await {
        Event::NeedInput(Prompt::Info { text, .. }) => assert_eq!(text, "two"),
        e => panic!("expected step two, got {e:?}"),
    }
    assert!(matches!(r.advance(Some(Input::Next)).await, Event::Done));
}

enum Tamper {
    Corrupt,
    SwapForAnotherSigned,
}

async fn resume_tampered(commit: bool, how: Tamper) -> Event {
    let f = Fakes::new();
    let dir = tempfile::tempdir().unwrap();
    started_run(&f, dir.path(), commit).await;
    let copy = dir.path().join("run1.manifest.json");
    match how {
        Tamper::Corrupt => {
            let mut bytes = std::fs::read(&copy).unwrap();
            bytes[10] ^= 1;
            std::fs::write(&copy, bytes).unwrap();
        }
        Tamper::SwapForAnotherSigned => {
            let b = signed_manifest("two, reworded", commit);
            std::fs::write(&copy, &b).unwrap();
            std::fs::write(dir.path().join("run1.manifest.json.minisig"), sign(&b)).unwrap();
        }
    }
    let mut r = Runner::resume(f.env(), dir.path(), "run1", &pubkey(), APP).unwrap();
    r.advance(Some(Input::Next)).await
}

#[tokio::test]
async fn a_tampered_manifest_copy_refuses_before_commit() {
    for how in [Tamper::Corrupt, Tamper::SwapForAnotherSigned] {
        assert!(matches!(
            resume_tampered(false, how).await,
            Event::Refused { .. }
        ));
    }
}

#[tokio::test]
async fn a_tampered_manifest_copy_fails_committed_after_commit() {
    for how in [Tamper::Corrupt, Tamper::SwapForAnotherSigned] {
        match resume_tampered(true, how).await {
            Event::Failed {
                committed, help, ..
            } => {
                assert!(committed);
                assert!(help.unwrap_or_default().contains("recovery guide"));
            }
            e => panic!("expected Failed, got {e:?}"),
        }
    }
}

#[tokio::test]
async fn wait_for_helix_service_asks_after_a_silent_timeout() {
    let f = Fakes::new();
    let mut r = Runner::new(f.env(), helix_wait(), state("run1", json!({})));
    let start = f.clock.now();
    match settle(&mut r, None).await {
        Event::NeedInput(Prompt::Ask { question, .. }) => {
            assert!(question.contains("Is HelixScreen showing"), "{question}")
        }
        e => panic!("expected Ask, got {e:?}"),
    }
    assert!(f.clock.now().duration_since(start).unwrap() >= Duration::from_secs(10));
    assert!(matches!(
        r.advance(Some(Input::Choice("yes".into()))).await,
        Event::Done
    ));
}

#[tokio::test]
async fn a_committing_ask_commits_only_on_a_listed_choice() {
    let f = Fakes::new();
    let m = manifest(json!([{
        "type": "ask", "fact": "go", "question": "Flash now?", "commits": true,
        "choices": [{ "value": "yes", "label": "Yes" }]
    }]));
    let mut r = Runner::new(f.env(), m, state("run1", json!({})));
    r.advance(None).await;
    assert!(matches!(
        r.advance(Some(Input::Choice("maybe".into()))).await,
        Event::NeedInput(Prompt::Ask { .. })
    ));
    assert!(!r.state().committed);
    assert!(matches!(
        r.advance(Some(Input::Choice("yes".into()))).await,
        Event::Done
    ));
    assert!(r.state().committed);
}

#[tokio::test]
async fn secrets_are_the_login_held_in_memory() {
    let f = Fakes::new();
    let m = manifest(json!([
        { "type": "info", "text": "before" },
        { "type": "ssh_login", "logins": [{ "user": "root", "password": PASSWORD }] },
        { "type": "info", "text": "after" }
    ]));
    let mut r = Runner::new(f.env(), m, state("sec", json!({})))
        .with_handler("ssh_login", Arc::new(TestLogin));
    r.advance(None).await;
    assert!(r.secrets().is_empty());
    assert!(matches!(
        r.advance(Some(Input::Next)).await,
        Event::NeedInput(Prompt::Info { .. })
    ));
    assert_eq!(r.secrets(), vec![PASSWORD.to_string()]);
}

/// The app runs `advance` on a multi-threaded runtime, so its future must be `Send`.
#[test]
fn advance_can_run_on_another_thread() {
    fn assert_send<T: Send>(_: &T) {}
    let f = Fakes::new();
    let mut r = Runner::new(f.env(), manifest(json!([])), state("send", json!({})));
    let fut = r.advance(None);
    assert_send(&fut);
}
