// SPDX-License-Identifier: GPL-3.0-or-later

use helix_installer_core::facts::Facts;
use helix_installer_core::manifest::{Login, Manifest, Secret};
use helix_installer_core::run_state::{recipe_sha256, RunState};
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::transport::fake::FakeSerial;
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
};
use helix_installer_core::transport::{Env, ExecOutput, Host, KeyStatus};
use serde_json::{json, Value};
use std::sync::Arc;

const ADDR: &str = "192.168.1.50";
const DOC: &str = "https://helixscreen.org/guide/install-k1/";
const ROOT_PW: &str = "root-default-pw";
const USER_PW: &str = "mine-pw-1234";

struct Fakes {
    ssh: FakeSsh,
}

impl Fakes {
    fn new() -> Self {
        let ssh = FakeSsh::new();
        ssh.on_command("id -u", out("0\n"));
        Fakes { ssh }
    }
    fn env(&self) -> Env {
        Env {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh: Arc::new(self.ssh.clone()),
            drives: Arc::new(FakeDrives::new()),
            fetch: Arc::new(FakeFetch::new()),
            clock: Arc::new(FakeClock::new()),
            net: Arc::new(FakeNet::new()),
            serial: Arc::new(FakeSerial::new()),
        }
    }
}

fn out(stdout: &str) -> ExecOutput {
    ExecOutput {
        stdout: stdout.into(),
        stderr: String::new(),
        exit: 0,
    }
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
            "fallback": { "refuse": true, "doc": DOC }
        }],
        "artifacts": {},
        "recipes": { "r": steps }
    }))
    .unwrap();
    Arc::new(m)
}

fn state() -> RunState {
    RunState::new("run1", "k1", "r", Host::new(ADDR), Facts::new())
}

fn login_step(logins: Value, ask: bool) -> Value {
    json!({ "type": "ssh_login", "logins": logins, "ask_if_failed": ask })
}

fn runner(f: &Fakes, steps: Value) -> Runner {
    Runner::new(f.env(), manifest(steps), state())
}

fn login(user: &str, password: &str) -> Input {
    Input::Login(Login::new(user, password))
}

fn key_login(user: &str, passphrase: Option<&str>) -> Input {
    let mut l = Login::new(user, "");
    l.passphrase = passphrase.map(Secret::new);
    Input::Login(l)
}

#[tokio::test]
async fn the_first_default_that_works_wins() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", ROOT_PW);
    let mut r = runner(
        &f,
        json!([login_step(
            json!([{ "user": "root", "password": ROOT_PW }, { "user": "pi", "password": "x" }]),
            false
        )]),
    );
    assert!(matches!(r.advance(None).await, Event::Done));
    assert_eq!(r.state().facts["ssh_user"], json!("root"));
    assert_eq!(r.state().facts["needs_sudo"], json!(false));
}

#[tokio::test]
async fn the_second_default_wins_when_the_first_is_refused() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "pi", "raspberry");
    let mut r = runner(
        &f,
        json!([login_step(
            json!([{ "user": "root", "password": ROOT_PW }, { "user": "pi", "password": "raspberry" }]),
            false
        )]),
    );
    assert!(matches!(r.advance(None).await, Event::Done));
    assert_eq!(r.state().facts["ssh_user"], json!("pi"));
}

#[tokio::test]
async fn a_non_root_user_needs_sudo() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "pi", "raspberry");
    f.ssh.on_command("id -u", out("1000\n"));
    let mut r = runner(
        &f,
        json!([login_step(
            json!([{ "user": "pi", "password": "raspberry" }]),
            false
        )]),
    );
    assert!(matches!(r.advance(None).await, Event::Done));
    assert_eq!(r.state().facts["needs_sudo"], json!(true));
}

#[tokio::test]
async fn every_default_refused_asks_when_the_recipe_allows_it() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", "something-else");
    let mut r = runner(
        &f,
        json!([login_step(
            json!([{ "user": "root", "password": ROOT_PW }]),
            true
        )]),
    );
    match r.advance(None).await {
        Event::NeedInput(Prompt::Login { user_hint, .. }) => {
            assert_eq!(user_hint.as_deref(), Some("root"))
        }
        e => panic!("expected a login prompt, got {e:?}"),
    }
    f.ssh.accept(ADDR, "root", USER_PW);
    assert!(matches!(
        r.advance(Some(login("root", USER_PW))).await,
        Event::Done
    ));
    assert_eq!(r.state().facts["ssh_user"], json!("root"));
}

#[tokio::test]
async fn every_default_refused_fails_when_the_recipe_does_not_ask() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", "something-else");
    let mut r = runner(
        &f,
        json!([login_step(
            json!([{ "user": "root", "password": ROOT_PW }]),
            false
        )]),
    );
    assert!(matches!(r.advance(None).await, Event::Failed { .. }));
}

#[tokio::test]
async fn a_host_that_does_not_answer_is_not_a_wrong_password() {
    let f = Fakes::new();
    let mut r = runner(
        &f,
        json!([login_step(
            json!([{ "user": "root", "password": ROOT_PW }]),
            true
        )]),
    );
    match r.advance(None).await {
        Event::Failed { reason, .. } => assert!(reason.contains("unreachable"), "{reason}"),
        e => panic!("expected Failed, got {e:?}"),
    }
}

#[tokio::test]
async fn three_wrong_user_logins_stop_with_the_printers_help() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", "something-else");
    let mut r = runner(&f, json!([login_step(json!([]), true)]));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    for _ in 0..2 {
        assert!(matches!(
            r.advance(Some(login("root", "wrong"))).await,
            Event::NeedInput(Prompt::Login { .. })
        ));
    }
    match r.advance(Some(login("root", "wrong"))).await {
        Event::Failed { help, .. } => assert!(help.unwrap_or_default().contains(DOC)),
        e => panic!("expected Failed after three tries, got {e:?}"),
    }
}

#[tokio::test]
async fn the_password_stays_out_of_state_log_and_events() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", USER_PW);
    let mut r = runner(&f, json!([login_step(json!([]), true)]));
    let first = r.advance(None).await;
    let last = r.advance(Some(login("root", USER_PW))).await;
    assert!(matches!(last, Event::Done));
    let seen = format!(
        "{first:?} {last:?} {}",
        serde_json::to_string(r.state()).unwrap()
    );
    assert!(!seen.contains(USER_PW), "{seen}");
}

/// A run reopened after the app closed: the earlier steps are done and no login is in memory.
fn resumed(f: &Fakes, steps: Value) -> Runner {
    let m = manifest(steps);
    let mut s = state();
    s.manifest_sha256 = Some(m.sha256.clone());
    s.recipe_sha256 = Some(recipe_sha256(&m.recipes["r"]));
    s.step_index = 1;
    Runner::new(f.env(), m, s)
}

fn probe_after_login(ask: bool) -> Value {
    json!([
        login_step(json!([{ "user": "root", "password": ROOT_PW }]), ask),
        { "type": "probe", "ssh": "cat /etc/model", "fact": "model" }
    ])
}

#[tokio::test]
async fn a_resumed_run_logs_in_again_quietly_for_a_probe() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", ROOT_PW);
    f.ssh.on_command("cat /etc/model", out("K1\n"));
    let mut r = resumed(&f, probe_after_login(true));
    assert!(matches!(r.advance(None).await, Event::Done));
    assert_eq!(r.state().facts["model"], json!("K1"));
}

#[tokio::test]
async fn a_resumed_run_asks_when_the_defaults_no_longer_work() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", USER_PW);
    f.ssh.on_command("cat /etc/model", out("K1\n"));
    let mut r = resumed(&f, probe_after_login(true));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    assert!(matches!(
        r.advance(Some(login("root", USER_PW))).await,
        Event::Done
    ));
    assert_eq!(r.state().facts["model"], json!("K1"));
}

#[tokio::test]
async fn a_wrong_typed_login_is_asked_again_by_any_ssh_step() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", USER_PW);
    f.ssh.on_command("cat /etc/model", out("K1\n"));
    let mut r = resumed(&f, probe_after_login(true));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    assert!(matches!(
        r.advance(Some(login("root", "wrong"))).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    assert!(matches!(
        r.advance(Some(login("root", USER_PW))).await,
        Event::Done
    ));
    assert_eq!(r.state().facts["model"], json!("K1"));
}

#[tokio::test]
async fn three_wrong_typed_logins_fail_any_ssh_step_with_the_recipe_help() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", USER_PW);
    let mut r = resumed(&f, probe_after_login(true));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    for _ in 0..2 {
        assert!(matches!(
            r.advance(Some(login("root", "wrong"))).await,
            Event::NeedInput(Prompt::Login { .. })
        ));
    }
    match r.advance(Some(login("root", "wrong"))).await {
        Event::Failed { help, .. } => assert!(help.unwrap_or_default().contains(DOC)),
        e => panic!("expected Failed, got {e:?}"),
    }
}

#[tokio::test]
async fn an_unreachable_host_fails_a_resumed_step_instead_of_asking() {
    let f = Fakes::new();
    let mut r = resumed(&f, probe_after_login(true));
    match r.advance(None).await {
        Event::Failed { reason, .. } => assert!(reason.contains("unreachable"), "{reason}"),
        e => panic!("expected Failed, got {e:?}"),
    }
}

#[tokio::test]
async fn wait_for_tries_the_stale_defaults_once_not_on_every_poll() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", USER_PW);
    let mut r = resumed(
        &f,
        json!([
            login_step(json!([{ "user": "root", "password": ROOT_PW }]), true),
            { "type": "wait_for", "target": "helix_service", "timeout_s": 30, "message": "w" }
        ]),
    );
    let mut ev = r.advance(None).await;
    while matches!(ev, Event::Progress { .. }) {
        ev = r.advance(None).await;
    }
    assert!(matches!(ev, Event::NeedInput(Prompt::Ask { .. })), "{ev:?}");
    assert_eq!(f.ssh.connect_count(), 1);
}

#[tokio::test]
async fn a_fresh_start_gets_three_new_tries() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", USER_PW);
    let mut r = runner(&f, json!([login_step(json!([]), true)]));
    r.advance(None).await;
    r.advance(Some(login("root", "wrong"))).await;
    r.advance(Some(login("root", "wrong"))).await;
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    assert!(matches!(
        r.advance(Some(login("root", "wrong"))).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
}

#[tokio::test]
async fn an_unreadable_uid_is_a_failure_not_non_root() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", ROOT_PW);
    f.ssh.on_command("id -u", out("uid=0(root)\n"));
    let mut r = runner(
        &f,
        json!([login_step(
            json!([{ "user": "root", "password": ROOT_PW }]),
            false
        )]),
    );
    assert!(matches!(r.advance(None).await, Event::Failed { .. }));
    assert!(!r.state().facts.contains_key("needs_sudo"));
}

// ---------------------------------------------------------------- SSH keys

#[tokio::test]
async fn the_login_prompt_says_whether_this_computer_has_an_ssh_key() {
    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", "something-else");
    let mut r = runner(&f, json!([login_step(json!([]), true)]));
    match r.advance(None).await {
        Event::NeedInput(Prompt::Login {
            has_ssh_key,
            key_locked,
            ..
        }) => assert!(!has_ssh_key && !key_locked),
        e => panic!("expected a login prompt, got {e:?}"),
    }

    let f = Fakes::new();
    f.ssh.accept(ADDR, "root", "something-else");
    f.ssh.set_key_status(KeyStatus {
        available: true,
        locked: true,
    });
    let mut r = runner(&f, json!([login_step(json!([]), true)]));
    match r.advance(None).await {
        Event::NeedInput(Prompt::Login {
            has_ssh_key,
            key_locked,
            ..
        }) => assert!(has_ssh_key && key_locked),
        e => panic!("expected a login prompt, got {e:?}"),
    }
}

#[tokio::test]
async fn a_login_with_only_a_user_name_signs_in_with_the_computers_key() {
    let f = Fakes::new();
    f.ssh.accept_key(ADDR, "pi", None);
    f.ssh.on_command("id -u", out("1000\n"));
    let mut r = runner(&f, json!([login_step(json!([]), true)]));
    assert!(matches!(
        r.advance(None).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    assert!(matches!(
        r.advance(Some(key_login("pi", None))).await,
        Event::Done
    ));
    assert_eq!(r.state().facts["ssh_user"], json!("pi"));
    assert_eq!(r.state().facts["needs_sudo"], json!(true));
}

#[tokio::test]
async fn a_passphrase_protected_key_signs_in_once_the_passphrase_is_given() {
    let f = Fakes::new();
    f.ssh.accept_key(ADDR, "pi", Some("open sesame"));
    let mut r = runner(&f, json!([login_step(json!([]), true)]));
    let _ = r.advance(None).await;
    assert!(matches!(
        r.advance(Some(key_login("pi", None))).await,
        Event::NeedInput(Prompt::Login { .. })
    ));
    assert!(matches!(
        r.advance(Some(key_login("pi", Some("open sesame")))).await,
        Event::Done
    ));
    assert_eq!(r.secrets(), vec!["open sesame".to_string()]);
    let saved = serde_json::to_string(r.state()).unwrap();
    assert!(!saved.contains("open sesame"), "{saved}");
}

#[test]
fn a_login_never_prints_or_saves_its_passphrase_or_sudo_password() {
    let mut l = Login::new("pi", "pw-visible-nowhere");
    l.passphrase = Some(Secret::new("phrase-visible-nowhere"));
    l.sudo_password = Some(Secret::new("sudo-visible-nowhere"));
    let shown = format!("{l:?} {:?}", Input::Login(l.clone()));
    assert!(!shown.contains("visible-nowhere"), "{shown}");
    let saved = serde_json::to_string(&l).unwrap();
    assert!(!saved.contains("phrase-visible-nowhere"), "{saved}");
    assert!(!saved.contains("sudo-visible-nowhere"), "{saved}");
}

#[test]
fn a_typed_login_reads_its_passphrase_from_the_ui() {
    let input: Input = serde_json::from_value(json!({
        "input": "login",
        "value": { "user": "pi", "password": "", "passphrase": "pp" }
    }))
    .unwrap();
    match input {
        Input::Login(l) => {
            assert_eq!(l.user, "pi");
            assert_eq!(l.passphrase.as_ref().map(Secret::expose), Some("pp"));
        }
        i => panic!("{i:?}"),
    }
    let sudo: Input =
        serde_json::from_value(json!({ "input": "sudo_password", "value": "sp" })).unwrap();
    assert!(matches!(sudo, Input::SudoPassword(s) if s.expose() == "sp"));
}
