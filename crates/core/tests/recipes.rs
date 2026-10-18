// SPDX-License-Identifier: GPL-3.0-or-later

//! The recipes in `manifest/staging.json` run end to end against the fakes, starting from facts
//! recorded on real printers (`tests/fixtures/fleet`).

use helix_installer_core::facts::Facts;
use helix_installer_core::identify::{identify, identify_as, Identified, StartState};
use helix_installer_core::lint::lint_with;
use helix_installer_core::manifest::{Channel, Login, Manifest};
use helix_installer_core::run_state::RunState;
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::steps::helix_install::{releases_manifest_url, GITHUB_RELEASE_API};
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
};
use helix_installer_core::transport::{Env, ExecOutput, Host};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

const ADDR: &str = "192.168.1.50";
const TAG: &str = "v1.0.5";
const TARBALL: &[u8] = b"helixscreen tarball bytes";
const INSTALL_SH: &[u8] = b"#!/bin/sh\ncase \"$1\" in\n  --progress) ;;\nesac\n";
const INSTALL_SH_URL: &str =
    "https://github.com/prestonbrown/helixscreen/releases/download/v1.0.5/install.sh";

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn staging() -> Manifest {
    let bytes = std::fs::read(repo().join("manifest/staging.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn fleet(name: &str) -> Facts {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fleet")
        .join(format!("{name}.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn out(stdout: &str, exit: i32) -> ExecOutput {
    ExecOutput {
        stdout: stdout.into(),
        stderr: String::new(),
        exit,
    }
}

fn pl(fields: &[&str]) -> String {
    format!("HELIX-PROGRESS\t{}\n", fields.join("\t"))
}

fn recipe(printer: &str, state: StartState, recipe_id: &str) -> Identified {
    Identified::Recipe {
        printer_id: printer.into(),
        state,
        recipe_id: recipe_id.into(),
    }
}

fn modded() -> StartState {
    StartState::Modded("moonraker".into())
}

/// Every fake a recipe can touch, with a HelixScreen release published for each platform.
struct Rig {
    moonraker: Arc<FakeMoonraker>,
    ssh: FakeSsh,
    fetch: Arc<FakeFetch>,
    drives: Arc<FakeDrives>,
    net: Arc<FakeNet>,
    clock: Arc<FakeClock>,
    cache: tempfile::TempDir,
}

impl Rig {
    fn new() -> Rig {
        let fetch = Arc::new(FakeFetch::new());
        let mut assets = serde_json::Map::new();
        for p in ["pi", "pi32", "k1", "k2", "ad5m", "ad5x"] {
            let url = format!(
                "https://releases.helixscreen.org/releases/{TAG}/helixscreen-{p}-{TAG}.tar.gz"
            );
            assets.insert(
                p.into(),
                json!({ "url": url, "sha256": sha(TARBALL), "size": TARBALL.len() }),
            );
            fetch.add(&url, TARBALL.to_vec());
        }
        let release = json!({ "version": "1.0.5", "tag": TAG, "assets": assets });
        for channel in [Channel::Stable, Channel::Beta] {
            fetch.add(
                &releases_manifest_url(channel),
                release.to_string().into_bytes(),
            );
        }
        let gh = json!({ "tag_name": TAG, "assets": [{
            "name": "install.sh", "browser_download_url": INSTALL_SH_URL,
            "digest": format!("sha256:{}", sha(INSTALL_SH)),
        }]});
        fetch.add(
            &format!("{GITHUB_RELEASE_API}/{TAG}"),
            gh.to_string().into_bytes(),
        );
        fetch.add(INSTALL_SH_URL, INSTALL_SH.to_vec());
        let ssh = FakeSsh::new();
        ssh.set_fallback(|cmd| match cmd {
            "id -u" => out("0\n", 0),
            _ => out("", 127),
        });
        Rig {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh,
            fetch,
            drives: Arc::new(FakeDrives::new()),
            net: Arc::new(FakeNet::new()),
            clock: Arc::new(FakeClock::new()),
            cache: tempfile::tempdir().unwrap(),
        }
    }

    fn env(&self) -> Env {
        Env {
            moonraker: self.moonraker.clone(),
            ssh: Arc::new(self.ssh.clone()),
            drives: self.drives.clone(),
            fetch: self.fetch.clone(),
            clock: self.clock.clone(),
            net: self.net.clone(),
        }
    }

    /// The installer on the printer reports `platform` and succeeds.
    fn installer_succeeds(&self, platform: &str) {
        self.ssh.script_job(vec![
            (&pl(&["platform", platform]), None),
            (&pl(&["result", "ok"]), Some(0)),
        ]);
    }

    fn runner(&self, m: &Manifest, picked: &Identified, facts: Facts) -> Runner {
        let Identified::Recipe {
            printer_id,
            recipe_id,
            ..
        } = picked
        else {
            panic!("expected a recipe, got {picked:?}");
        };
        let state = RunState::new("run", printer_id, recipe_id, Host::new(ADDR), facts);
        Runner::new(self.env(), Arc::new(m.clone()), state)
            .with_cache_dir(self.cache.path().to_path_buf())
    }

    /// The `sh … install.sh …` commands the installer was started with.
    fn installs(&self) -> Vec<(String, Option<Vec<u8>>)> {
        self.ssh
            .calls()
            .into_iter()
            .filter(|(c, _)| c.starts_with("sh ") && c.contains("install.sh"))
            .collect()
    }
}

/// What a run did, in order: every prompt it showed and how it ended.
struct Trace {
    prompts: Vec<Prompt>,
    end: Event,
}

impl Trace {
    fn texts(&self) -> Vec<String> {
        self.prompts
            .iter()
            .map(|p| match p {
                Prompt::Info { text, .. } | Prompt::UserAction { text, .. } => text.clone(),
                Prompt::Ask { question, .. } => question.clone(),
                Prompt::Login { user_hint } => format!("login {user_hint:?}"),
                Prompt::ChooseDrive { .. } => "choose drive".into(),
                Prompt::Ambiguous { .. } => "ambiguous".into(),
            })
            .collect()
    }

    fn done(&self) -> bool {
        matches!(self.end, Event::Done)
    }
}

/// Runs to the end, answering each prompt with `answer` (which sees the prompt and how many
/// came before it).
async fn drive(r: &mut Runner, mut answer: impl FnMut(&Prompt, usize) -> Input) -> Trace {
    let mut prompts = Vec::new();
    let mut input = None;
    for _ in 0..500 {
        match r.advance(input.take()).await {
            Event::Progress { .. } => {}
            Event::NeedInput(p) => {
                input = Some(answer(&p, prompts.len()));
                prompts.push(p);
            }
            end => return Trace { prompts, end },
        }
    }
    panic!("the run never ended");
}

/// Next for every screen; never answers a question or a login.
fn next_only(p: &Prompt, _: usize) -> Input {
    match p {
        Prompt::Info { .. } | Prompt::UserAction { .. } => Input::Next,
        other => panic!("unexpected prompt {other:?}"),
    }
}

fn login(user: &str, password: &str) -> Input {
    Input::Login(Login {
        user: user.into(),
        password: password.into(),
    })
}

// ---------------------------------------------------------------- the staging manifest

#[test]
fn staging_passes_the_lint_with_unverified_printers() {
    let m = staging();
    let errs = lint_with(&m, &repo().join("manifest/img"), true);
    assert_eq!(errs, vec![], "{errs:?}");
    assert!(
        m.printers.iter().all(|p| p.verified.is_none()),
        "a printer with a hardware run belongs in manifest.json"
    );
}

#[test]
fn staging_is_what_the_manifest_workflow_lints() {
    let wf = std::fs::read_to_string(repo().join(".github/workflows/manifest.yml")).unwrap();
    assert!(
        wf.contains("manifest-lint manifest/staging.json manifest/img --allow-unverified"),
        "{wf}"
    );
    assert!(
        !wf.contains("cp manifest/staging.json") && !wf.contains("staging.json \"$dest"),
        "staging is never published"
    );
}

// ---------------------------------------------------------------- generic Klipper hosts

#[test]
fn the_voron_and_the_pi_are_generic_hosts_with_helix_installed() {
    let m = staging();
    for host in ["pi3_mainsailos", "cb1_voron"] {
        assert_eq!(
            identify(&m, &fleet(host)),
            recipe(
                "generic_klipper",
                StartState::Helix("v1.0.0".into()),
                "generic_update"
            ),
            "{host}"
        );
    }
}

#[test]
fn a_generic_host_without_helix_gets_the_install_recipe() {
    let mut f = fleet("pi3_mainsailos");
    f.remove("helix_installed");
    f.remove("helix_version");
    assert_eq!(
        identify(&staging(), &f),
        recipe("generic_klipper", modded(), "generic_install")
    );
}

#[test]
fn a_generic_host_on_a_cpu_without_a_build_is_refused() {
    let mut f = fleet("cb1_voron");
    f.insert("cpu_arch".into(), "x86_64".into());
    assert!(matches!(
        identify(&staging(), &f),
        Identified::Refuse { printer_id: Some(ref p), .. } if p == "generic_klipper"
    ));
}

#[tokio::test]
async fn generic_install_asks_for_the_login_and_installs_for_the_cpu() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "pi", "raspberry");
    rig.ssh.set_fallback(|cmd| match cmd {
        "id -u" => out("1000\n", 0),
        "sudo -n true" => out("", 1),
        _ => out("", 127),
    });
    rig.installer_succeeds("pi");
    let m = staging();
    let mut f = fleet("pi3_mainsailos");
    f.remove("helix_installed");
    let picked = identify(&m, &f);
    let mut r = rig.runner(&m, &picked, f);

    let trace = drive(&mut r, |p, _| match p {
        Prompt::Login { .. } => login("pi", "raspberry"),
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        rig.installs(),
        vec![(
            "sh '/tmp/install.sh' --local '/tmp/helixscreen-pi-v1.0.5.tar.gz' --progress".into(),
            Some(b"raspberry".to_vec())
        )],
        "a login without passwordless sudo runs the installer through sudo -S"
    );
}

#[tokio::test]
async fn generic_update_on_the_voron_passes_update() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "biqu", "biqu");
    rig.ssh.set_fallback(|cmd| match cmd {
        "id -u" => out("1000\n", 0),
        "sudo -n true" => out("", 0),
        _ => out("", 127),
    });
    rig.installer_succeeds("pi");
    let m = staging();
    let picked = identify(&m, &fleet("cb1_voron"));
    let mut r = rig.runner(&m, &picked, fleet("cb1_voron"));

    let trace = drive(&mut r, |p, _| match p {
        Prompt::Login { .. } => login("biqu", "biqu"),
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    assert!(trace.done(), "{:?}", trace.end);
    let installs = rig.installs();
    assert_eq!(installs.len(), 1);
    assert!(
        installs[0].0.ends_with("--progress --update"),
        "{installs:?}"
    );
    assert_eq!(
        installs[0].1, None,
        "passwordless sudo needs no password fed"
    );
}

#[tokio::test]
async fn generic_install_stops_after_three_wrong_logins_without_changing_anything() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "pi", "raspberry");
    let m = staging();
    let mut f = fleet("pi3_mainsailos");
    f.remove("helix_installed");
    let picked = identify(&m, &f);
    let mut r = rig.runner(&m, &picked, f);

    let trace = drive(&mut r, |_, _| login("pi", "wrong")).await;
    assert!(
        matches!(
            trace.end,
            Event::Failed {
                committed: false,
                ..
            }
        ),
        "{:?}",
        trace.end
    );
    assert!(rig.installs().is_empty());
}

#[tokio::test]
async fn generic_restore_runs_the_uninstaller() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "pi", "raspberry");
    rig.installer_succeeds("pi");
    let m = staging();
    let restore = &m
        .printers
        .iter()
        .find(|p| p.id == "generic_klipper")
        .unwrap()
        .restore;
    let picked = recipe("generic_klipper", StartState::Helix(String::new()), restore);
    let mut r = rig.runner(&m, &picked, fleet("pi3_mainsailos"));

    let trace = drive(&mut r, |p, _| match p {
        Prompt::Login { .. } => login("pi", "raspberry"),
        Prompt::Info { .. } => Input::Next,
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        rig.installs()[0].0,
        "sh '/tmp/install.sh' --uninstall --progress"
    );
}

// ---------------------------------------------------------------- QIDI

#[test]
fn the_qidi_with_a_box_identifies_as_the_new_qidi_range() {
    assert_eq!(
        identify(&staging(), &fleet("qidi_box")),
        recipe("qidi", modded(), "qidi_install")
    );
}

#[tokio::test]
async fn qidi_install_warns_about_studio_then_falls_back_to_mks_with_sudo() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "mks", "makerbase");
    rig.ssh.set_fallback(|cmd| match cmd {
        "id -u" => out("1000\n", 0),
        "sudo -n true" => out("", 1),
        _ => out("", 127),
    });
    rig.installer_succeeds("pi");
    let m = staging();
    let picked = identify(&m, &fleet("qidi_box"));
    let mut r = rig.runner(&m, &picked, fleet("qidi_box"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    let texts = trace.texts();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("QIDI Studio") && texts[0].contains("cloud"),
        "{texts:?}"
    );
    assert_eq!(
        rig.installs()[0].1.as_deref(),
        Some(&b"makerbase"[..]),
        "mks needs sudo with its password"
    );
}

#[tokio::test]
async fn qidi_install_tries_qidi_first() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "qidi", "qiditech");
    rig.ssh.accept(ADDR, "mks", "makerbase");
    rig.ssh.set_fallback(|cmd| match cmd {
        "id -u" => out("1000\n", 0),
        "sudo -n true" => out("", 0),
        _ => out("", 127),
    });
    rig.installer_succeeds("pi");
    let m = staging();
    let picked = identify(&m, &fleet("qidi_box"));
    let mut r = rig.runner(&m, &picked, fleet("qidi_box"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(r.state().facts["ssh_user"], json!("qidi"));
}

#[tokio::test]
async fn a_qidi_refusing_both_logins_asks_for_one() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "root", "secret");
    rig.installer_succeeds("pi");
    let m = staging();
    let picked = identify(&m, &fleet("qidi_box"));
    let mut r = rig.runner(&m, &picked, fleet("qidi_box"));

    let trace = drive(&mut r, |p, _| match p {
        Prompt::Info { .. } => Input::Next,
        Prompt::Login { user_hint } => {
            assert_eq!(user_hint.as_deref(), Some("qidi"));
            login("root", "secret")
        }
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    assert!(trace.done(), "{:?}", trace.end);
}

// ---------------------------------------------------------------- the fleet

/// Each recorded printer identifies as exactly the printer it is.
#[test]
fn every_fleet_printer_identifies_as_itself_and_nothing_else() {
    let m = staging();
    let expect: &[(&str, Option<&str>)] = &[
        ("pi3_mainsailos", Some("generic_klipper")),
        ("cb1_voron", Some("generic_klipper")),
        ("qidi_box", Some("qidi")),
        ("k2_plus", None),
        ("k1c_helper_script", None),
        ("ad5m_forgex", None),
    ];
    for (name, want) in expect {
        let got = identify(&m, &fleet(name));
        match (want, &got) {
            (Some(id), Identified::Recipe { printer_id, .. }) => assert_eq!(printer_id, id),
            (
                None,
                Identified::Refuse {
                    printer_id: None, ..
                },
            ) => {}
            _ => panic!("{name}: {got:?}"),
        }
    }
}

#[test]
fn a_picked_generic_host_cannot_be_a_qidi() {
    assert!(matches!(
        identify_as(&staging(), "generic_klipper", &fleet("qidi_box")),
        Identified::Refuse { printer_id: Some(ref p), .. } if p == "generic_klipper"
    ));
}
