// SPDX-License-Identifier: GPL-3.0-or-later

//! The recipes in `manifest/staging.json` run end to end against the fakes, starting from facts
//! recorded on real printers (`tests/fixtures/fleet`).

use helix_installer_core::facts::Facts;
use helix_installer_core::identify::{
    identify, identify_as, restore_for, Identified, StartState, GENERIC_REFUSE_DOC,
};
use helix_installer_core::lint::lint_with;
use helix_installer_core::manifest::{Channel, Login, Manifest, Step, WaitTarget};
use helix_installer_core::run_state::RunState;
use helix_installer_core::runner::{Event, Input, Prompt, Runner};
use helix_installer_core::steps::helix_install::{
    releases_manifest_url, GITHUB_RELEASE_API, INSTALLED_VERSION_PROBE,
};
use helix_installer_core::transport::fake::FakeSerial;
use helix_installer_core::transport::fake::{
    FakeClock, FakeDrives, FakeFetch, FakeMoonraker, FakeNet, FakeSsh,
};
use helix_installer_core::transport::{Env, ExecOutput, Host};
use helix_installer_core::version::FwVersion;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

const ADDR: &str = "192.168.1.50";
const TARBALL: &[u8] = b"helixscreen tarball bytes";
const INSTALL_SH: &[u8] = b"#!/bin/sh\ncase \"$1\" in\n  --progress) ;;\nesac\n";

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
        let ssh = FakeSsh::new();
        ssh.set_fallback(|cmd| match cmd {
            "id -u" => out("0\n", 0),
            "pidof helix-screen" => out("1234\n", 0),
            _ => out("", 127),
        });
        let rig = Rig {
            moonraker: Arc::new(FakeMoonraker::new()),
            ssh,
            fetch: Arc::new(FakeFetch::new()),
            drives: Arc::new(FakeDrives::new()),
            net: Arc::new(FakeNet::new()),
            clock: Arc::new(FakeClock::new()),
            cache: tempfile::tempdir().unwrap(),
        };
        for channel in [Channel::Stable, Channel::Beta] {
            rig.publish(channel, "1.1.0");
        }
        // ZMOD restarts the printer to load its plugin list.
        rig.moonraker.restart_on_gcode("ENABLE_EXTRA_PLUGINS", 3);
        rig
    }

    /// Makes `version` the newest release on `channel`, with a build for every platform and
    /// its `install.sh` on GitHub.
    fn publish(&self, channel: Channel, version: &str) {
        self.publish_with(
            channel,
            version,
            INSTALL_SH,
            &["pi", "pi32", "k1", "k2", "ad5m", "ad5x"],
        );
    }

    /// As `publish`, with `script` as the release's `install.sh` and builds for `platforms` only.
    fn publish_with(&self, channel: Channel, version: &str, script: &[u8], platforms: &[&str]) {
        let tag = format!("v{version}");
        let mut assets = serde_json::Map::new();
        for &p in platforms {
            let url = format!(
                "https://releases.helixscreen.org/releases/{tag}/helixscreen-{p}-{tag}.tar.gz"
            );
            assets.insert(
                p.into(),
                json!({ "url": url, "sha256": sha(TARBALL), "size": TARBALL.len() }),
            );
            self.fetch.add(&url, TARBALL.to_vec());
        }
        let release = json!({ "version": version, "tag": tag, "assets": assets });
        self.fetch.add(
            &releases_manifest_url(channel),
            release.to_string().into_bytes(),
        );
        let script_url = format!(
            "https://github.com/prestonbrown/helixscreen/releases/download/{tag}/install.sh"
        );
        let gh = json!({ "tag_name": tag, "assets": [{
            "name": "install.sh", "browser_download_url": script_url,
            "digest": format!("sha256:{}", sha(script)),
        }]});
        self.fetch.add(
            &format!("{GITHUB_RELEASE_API}/{tag}"),
            gh.to_string().into_bytes(),
        );
        self.fetch.add(&script_url, script.to_vec());
    }

    fn env(&self) -> Env {
        Env {
            moonraker: self.moonraker.clone(),
            ssh: Arc::new(self.ssh.clone()),
            drives: self.drives.clone(),
            fetch: self.fetch.clone(),
            clock: self.clock.clone(),
            net: self.net.clone(),
            serial: Arc::new(FakeSerial::new()),
        }
    }

    /// The installer on the printer reports `platform` and succeeds.
    /// The installer on the printer reports `platform` and succeeds: once for the dry run
    /// that comes before an install, once for the install (an uninstall uses the first).
    fn installer_succeeds(&self, platform: &str) {
        for _ in 0..2 {
            self.ssh.script_job(vec![
                (&pl(&["platform", platform]), None),
                (&pl(&["result", "ok"]), Some(0)),
            ]);
        }
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
        self.installer_runs()
            .into_iter()
            .filter(|(c, _)| !c.contains("--dry-run"))
            .collect()
    }

    /// Every `install.sh` started, dry runs included, in order.
    fn installer_runs(&self) -> Vec<(String, Option<Vec<u8>>)> {
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
                Prompt::Info { text, .. }
                | Prompt::UserAction { text, .. }
                | Prompt::Confirm { text, .. } => text.clone(),
                Prompt::Ask { question, .. } => question.clone(),
                Prompt::Login { user_hint, .. } => format!("login {user_hint:?}"),
                Prompt::ChooseDrive { .. } => "choose drive".into(),
                Prompt::Ambiguous { .. } => "ambiguous".into(),
                Prompt::SudoPassword { user, .. } => format!("sudo password {user}"),
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
    Input::Login(Login::new(user, password))
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
    for (host, recipe_id) in [
        ("pi3_mainsailos", "generic_update"),
        ("cb1_voron", "generic_update_board"),
    ] {
        assert_eq!(
            identify(&m, &fleet(host)),
            recipe(
                "generic_klipper",
                StartState::Helix("v1.0.0".into()),
                recipe_id
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
        "pidof helix-screen" => out("1234\n", 0),
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
            "sh '/tmp/install.sh' --local '/tmp/helixscreen-pi-v1.1.0.tar.gz' --progress".into(),
            Some(b"raspberry".to_vec())
        )],
        "a login without passwordless sudo runs the installer through sudo -S"
    );
}

#[tokio::test]
async fn generic_update_on_the_voron_passes_update_and_stays_on_stable() {
    let rig = Rig::new();
    // The Voron runs a stable release: a newer beta is not for it.
    rig.publish(Channel::Beta, "1.1.0-beta.10");
    rig.ssh.accept(ADDR, "biqu", "biqu");
    rig.ssh.set_fallback(|cmd| match cmd {
        "pidof helix-screen" => out("1234\n", 0),
        "id -u" => out("1000\n", 0),
        "sudo -n true" => out("", 0),
        c if c == board_probe() => out("yes\n", 0),
        _ => out("", 127),
    });
    rig.installer_succeeds("pi");
    let m = staging();
    let picked = identify(&m, &fleet("cb1_voron"));
    assert_eq!(
        picked,
        recipe(
            "generic_klipper",
            StartState::Helix("v1.0.0".into()),
            "generic_update_board"
        ),
        "Moonraker names no board on the CB1, so the board is checked over SSH"
    );
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
    assert!(
        installs[0].0.contains("helixscreen-pi-v1.1.0.tar.gz"),
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
    let restore = restore_for(&m, "generic_klipper", &fleet("pi3_mainsailos")).unwrap();
    let picked = recipe(
        "generic_klipper",
        StartState::Helix(String::new()),
        &restore,
    );
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
        "pidof helix-screen" => out("1234\n", 0),
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
        "pidof helix-screen" => out("1234\n", 0),
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
        Prompt::Login { user_hint, .. } => {
            assert_eq!(user_hint.as_deref(), Some("qidi"));
            login("root", "secret")
        }
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    assert!(trace.done(), "{:?}", trace.end);
}

// ---------------------------------------------------------------- Creality K2

const S99_PROBE: &str = "test -e /etc/init.d/S99helixscreen && echo yes || echo no";

/// A K2 whose root login is `password`, SSH open, and `S99helixscreen` present or not.
fn k2_rig(password: &'static str, helix_there: bool) -> Rig {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "root", password);
    rig.net.answer(ADDR, 22, Some("SSH-2.0-dropbear"));
    rig.ssh.on_command(
        S99_PROBE,
        out(if helix_there { "yes\n" } else { "no\n" }, 0),
    );
    rig.installer_succeeds("k2");
    rig
}

#[test]
fn the_k2_plus_identifies_as_a_k2_on_stock_moonraker() {
    assert_eq!(
        identify(&staging(), &fleet("k2_plus")),
        recipe("k2", modded(), "k2_install")
    );
}

#[tokio::test]
async fn k2_install_has_root_turned_on_then_installs_with_the_fixed_password() {
    let rig = k2_rig("creality_2024", false);
    let m = staging();
    let picked = identify(&m, &fleet("k2_plus"));
    let mut r = rig.runner(&m, &picked, fleet("k2_plus"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    match &trace.prompts[..] {
        [Prompt::UserAction { text, image }] => {
            assert!(text.contains("Root account information"), "{text}");
            assert_eq!(image.as_deref(), Some("k2/root-account.png"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        rig.installs()[0].0,
        "sh '/mnt/UDISK/install.sh' --local '/mnt/UDISK/helixscreen-k2-v1.1.0.tar.gz' --progress"
    );
}

#[tokio::test]
async fn k2_with_ssh_still_closed_fails_the_root_step_and_retries_it() {
    let rig = k2_rig("creality_2024", false);
    let net_closed = Arc::new(FakeNet::new());
    let mut env = rig.env();
    env.net = net_closed.clone();
    let m = staging();
    let picked = identify(&m, &fleet("k2_plus"));
    let Identified::Recipe { recipe_id, .. } = &picked else {
        unreachable!()
    };
    let state = RunState::new("run", "k2", recipe_id, Host::new(ADDR), fleet("k2_plus"));
    let mut r = Runner::new(env, Arc::new(m), state).with_cache_dir(rig.cache.path().into());

    let ev = r.advance(None).await;
    assert!(
        matches!(ev, Event::NeedInput(Prompt::UserAction { .. })),
        "{ev:?}"
    );
    let ev = r.advance(Some(Input::Next)).await;
    match ev {
        Event::Failed {
            committed: false,
            step: 0,
            ..
        } => {}
        other => panic!("expected a retryable failure at the root step, got {other:?}"),
    }
    net_closed.answer(ADDR, 22, Some("SSH-2.0-dropbear"));
    let ev = r.advance(None).await;
    assert!(
        matches!(ev, Event::NeedInput(Prompt::UserAction { .. })),
        "{ev:?}"
    );
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
}

#[tokio::test]
async fn k2_with_a_per_unit_password_asks_for_the_one_on_the_root_screen() {
    let rig = k2_rig("unit-pw-123", false);
    let m = staging();
    let picked = identify(&m, &fleet("k2_plus"));
    let mut r = rig.runner(&m, &picked, fleet("k2_plus"));

    let mut hint = None;
    let trace = drive(&mut r, |p, _| match p {
        Prompt::UserAction { .. } => Input::Next,
        Prompt::Login {
            user_hint,
            password_hint,
            ..
        } => {
            assert_eq!(user_hint.as_deref(), Some("root"));
            hint = password_hint.clone();
            login("root", "unit-pw-123")
        }
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        hint.as_deref(),
        Some("the password shown on the printer's root screen")
    );
}

#[tokio::test]
async fn a_k2_that_already_runs_helixscreen_switches_to_the_update() {
    let rig = k2_rig("creality_2024", true);
    let m = staging();
    let picked = identify(&m, &fleet("k2_plus"));
    let mut r = rig.runner(&m, &picked, fleet("k2_plus"));

    rig.ssh
        .on_command(INSTALLED_VERSION_PROBE, out("v1.0.4\n", 0));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(r.state().recipe_id, "k2_update");
    assert_eq!(trace.prompts.len(), 1, "the root step is not asked twice");
    assert!(rig.installs()[0].0.ends_with("--progress --update"));
}

#[tokio::test]
async fn a_k2_whose_installed_version_cannot_be_read_is_not_updated() {
    let rig = k2_rig("creality_2024", true);
    let m = staging();
    let picked = identify(&m, &fleet("k2_plus"));
    let mut r = rig.runner(&m, &picked, fleet("k2_plus"));

    let trace = drive(&mut r, next_only).await;
    match &trace.end {
        Event::Refused { reason, doc, .. } => {
            assert!(reason.contains("which HelixScreen version"), "{reason}");
            assert!(reason.contains("Reinstall"), "{reason}");
            assert_eq!(doc, "https://helixscreen.org/guide/install-k2/");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(rig.installer_runs().is_empty());
    assert!(rig.ssh.uploads().is_empty());
}

#[tokio::test]
async fn k2_restore_runs_the_uninstaller() {
    let rig = k2_rig("creality_2024", true);
    let m = staging();
    let picked = recipe("k2", StartState::Helix(String::new()), "k2_restore");
    let mut r = rig.runner(&m, &picked, fleet("k2_plus"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        rig.installs()[0].0,
        "sh '/mnt/UDISK/install.sh' --uninstall --progress"
    );
}

// ---------------------------------------------------------------- Creality K1

const SIMPLE_AF: &str = "git config --global http.sslVerify false; if [ -d /usr/data/pellcorp/.git ]; then git -C /usr/data/pellcorp remote get-url origin | grep -q 'github.com/pellcorp/creality' || { echo 'FATAL: /usr/data/pellcorp is not a copy of pellcorp/creality; factory reset the printer and start again'; exit 1; }; git -C /usr/data/pellcorp pull || { echo 'FATAL: could not update /usr/data/pellcorp (git pull failed); check the printer can reach github.com, then retry'; exit 1; }; elif [ -e /usr/data/pellcorp ]; then echo 'FATAL: /usr/data/pellcorp is in the way; factory reset the printer and start again'; exit 1; else git clone https://github.com/pellcorp/creality.git /usr/data/pellcorp; fi && sh /usr/data/pellcorp/k1/installer.sh --install loadcells";
const K1_MODEL_PROBE: &str = r#"case "$(/usr/bin/get_sn_mac.sh model 2>/dev/null)" in CR-K1|K1C|"CR-K1 Max") echo yes;; *) echo no;; esac"#;
const OTA_PROBE: &str = r"sed -n 's/^ota_version=\(6\.\)\{0,1\}//p' /etc/ota_info";
const LEFTOVERS_PROBE: &str = r#"if [ -d /usr/data/helper-script ] || [ -f /usr/data/fluidd.sh ] || [ -f /usr/data/mainsail.sh ] || { [ -d /opt ] && [ ! -L /opt ] && [ -n "$(ls -1 /opt)" ]; }; then echo yes; else echo no; fi"#;

struct K1 {
    firmware: &'static str,
    helix_there: bool,
    leftovers: bool,
    is_k1: bool,
}

const STOCK_K1: K1 = K1 {
    firmware: "1.3.3.46",
    helix_there: false,
    leftovers: false,
    is_k1: true,
};

/// A K1 on root/creality_2023 whose Moonraker, once it answers, lists the fleet K1C's objects.
fn k1_rig(k1: K1) -> Rig {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "root", "creality_2023");
    rig.net.answer(ADDR, 22, Some("SSH-2.0-dropbear"));
    let yes_no = |b: bool| out(if b { "yes\n" } else { "no\n" }, 0);
    rig.ssh.on_command("uname -m", out("mips\n", 0));
    rig.ssh.on_command(S99_PROBE, yes_no(k1.helix_there));
    rig.ssh
        .on_command(OTA_PROBE, out(&format!("{}\n", k1.firmware), 0));
    rig.ssh.on_command(LEFTOVERS_PROBE, yes_no(k1.leftovers));
    rig.ssh.on_command(K1_MODEL_PROBE, yes_no(k1.is_k1));
    let objects = fleet("k1c_helper_script")["klipper_objects"].clone();
    rig.moonraker
        .set_printer_info(ADDR, json!({ "state": "ready", "hostname": "K1C-0000" }));
    rig.moonraker
        .set_objects_list(ADDR, json!({ "objects": objects }));
    // The power cycle Simple AF asks for: Moonraker is gone for a while once it has run.
    let moonraker = rig.moonraker.clone();
    rig.ssh.on_spawn(move |cmd| {
        if cmd == SIMPLE_AF {
            moonraker.restart(ADDR, 3);
        }
    });
    rig
}

/// Answers a stock K1 run: the root option is there, Next everywhere else.
fn stock_k1_answers(p: &Prompt, _: usize) -> Input {
    match p {
        Prompt::Ask { .. } => Input::Choice("yes".into()),
        _ => next_only(p, 0),
    }
}

fn ran(rig: &Rig, cmd: &str) -> bool {
    rig.ssh.commands().iter().any(|c| c == cmd)
}

#[test]
fn the_k1c_with_helper_script_and_helixscreen_identifies_for_the_update() {
    assert_eq!(
        identify(&staging(), &fleet("k1c_helper_script")),
        recipe("k1", StartState::Helix("v1.1.0-beta.9".into()), "k1_update")
    );
}

/// The fleet K1C (HelixScreen v1.1.0-beta.9) with stable 1.0.5 and `beta` published.
fn k1c_update_rig(beta: &str) -> Rig {
    let rig = k1_rig(K1 {
        helix_there: true,
        leftovers: true,
        ..STOCK_K1
    });
    rig.installer_succeeds("k1");
    rig.publish(Channel::Stable, "1.0.5");
    rig.publish(Channel::Beta, beta);
    rig
}

#[tokio::test]
async fn the_k1c_on_a_beta_updates_to_the_next_beta_over_ssh_without_simple_af() {
    let rig = k1c_update_rig("1.1.0-beta.10");
    let m = staging();
    let picked = identify(&m, &fleet("k1c_helper_script"));
    let mut r = rig.runner(&m, &picked, fleet("k1c_helper_script"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(trace.prompts.is_empty(), "{:?}", trace.texts());
    assert!(!ran(&rig, SIMPLE_AF));
    assert_eq!(
        rig.installs()[0].0,
        "sh '/usr/data/install.sh' --local '/usr/data/helixscreen-k1-v1.1.0-beta.10.tar.gz' --progress --update"
    );
}

#[tokio::test]
async fn the_k1c_on_the_newest_beta_is_up_to_date_and_never_gets_the_older_stable() {
    let rig = k1c_update_rig("1.1.0-beta.9");
    let m = staging();
    let picked = identify(&m, &fleet("k1c_helper_script"));
    let mut r = rig.runner(&m, &picked, fleet("k1c_helper_script"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(
        rig.installer_runs().is_empty(),
        "{:?}",
        rig.installer_runs()
    );
    assert!(rig.ssh.uploads().is_empty());
    assert!(!r.state().committed, "nothing on the printer changed");
    let note = r.state().done_note.clone().unwrap_or_default();
    assert_eq!(
        r.state().done_kind,
        Some(helix_installer_core::run_state::DoneKind::UpToDate)
    );
    assert!(note.contains("1.1.0-beta.9"), "{note:?}");
}

#[tokio::test]
async fn the_k1c_on_a_beta_takes_the_stable_release_once_it_is_newer_than_every_beta() {
    let rig = k1c_update_rig("1.1.0-beta.9");
    rig.publish(Channel::Stable, "1.1.0");
    let m = staging();
    let picked = identify(&m, &fleet("k1c_helper_script"));
    let mut r = rig.runner(&m, &picked, fleet("k1c_helper_script"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        rig.installs()[0].0,
        "sh '/usr/data/install.sh' --local '/usr/data/helixscreen-k1-v1.1.0.tar.gz' --progress --update"
    );
}

#[tokio::test]
async fn an_install_without_release_info_is_read_from_its_binarys_version_line() {
    let rig = k2_rig("creality_2024", true);
    rig.ssh.on_command(
        INSTALLED_VERSION_PROBE,
        out("helix-screen 1.0.4 (9ed38c74d5)\n", 0),
    );
    let m = staging();
    let picked = identify(&m, &fleet("k2_plus"));
    let mut r = rig.runner(&m, &picked, fleet("k2_plus"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(rig.installs()[0].0.ends_with("--progress --update"));
}

#[tokio::test]
async fn a_recipe_pinned_to_stable_still_never_installs_below_the_installed_beta() {
    let rig = k1c_update_rig("1.2.0-beta.1");
    rig.publish(Channel::Stable, "1.1.0");
    rig.ssh
        .on_command(INSTALLED_VERSION_PROBE, out("v1.2.0-beta.1\n", 0));
    let mut m = staging();
    for step in m.recipes.get_mut("k1_update").unwrap() {
        if let helix_installer_core::manifest::Step::HelixInstall { channel, .. } = step {
            *channel = Some(Channel::Stable);
        }
    }
    let picked = identify(&m, &fleet("k1c_helper_script"));
    let mut r = rig.runner(&m, &picked, fleet("k1c_helper_script"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(
        rig.installer_runs().is_empty(),
        "{:?}",
        rig.installer_runs()
    );
}

#[tokio::test]
async fn a_stock_k1_picked_from_the_grid_gets_simple_af_then_helixscreen() {
    let rig = k1_rig(STOCK_K1);
    rig.ssh.script_job(vec![("Simple AF installed\n", Some(0))]);
    rig.installer_succeeds("k1");
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    assert_eq!(picked, recipe("k1", StartState::Stock, "k1_stock"));
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, stock_k1_answers).await;
    assert!(trace.done(), "{:?}", trace.end);
    let texts = trace.texts();
    assert_eq!(texts.len(), 5, "{texts:?}");
    assert!(texts[0].contains("tap Cancel"), "{texts:?}");
    assert!(texts[1].contains("Root account information"), "{texts:?}");
    assert!(texts[2].contains("factory reset"), "{texts:?}");
    assert!(texts[3].contains("Root account information"), "{texts:?}");
    assert!(texts[4].contains("wait 10 seconds"), "{texts:?}");
    assert!(ran(&rig, SIMPLE_AF), "{:?}", rig.ssh.commands());
    assert!(r.state().committed);
    assert!(
        rig.installs()[0]
            .0
            .starts_with("sh '/usr/data/install.sh' --local"),
        "{:?}",
        rig.installs()
    );
}

/// An `install.sh` from before `--progress`, as stable 1.0.5 ships it.
const INSTALL_SH_WITHOUT_PROGRESS: &[u8] = b"#!/bin/sh\ncase \"$1\" in\n  --local) ;;\nesac\n";

/// A stock K1 whose stable release is 1.0.5 (an `install.sh` without `--progress`) and whose
/// beta is 1.1.0-beta.10.
fn stock_k1_with_an_old_stable() -> Rig {
    let rig = k1_rig(STOCK_K1);
    rig.publish_with(
        Channel::Stable,
        "1.0.5",
        INSTALL_SH_WITHOUT_PROGRESS,
        &["k1"],
    );
    rig.publish(Channel::Beta, "1.1.0-beta.10");
    rig
}

#[tokio::test]
async fn a_stock_k1_installs_the_beta_while_stable_is_older_than_1_1() {
    let rig = stock_k1_with_an_old_stable();
    rig.ssh.script_job(vec![("Simple AF installed\n", Some(0))]);
    rig.installer_succeeds("k1");
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, stock_k1_answers).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(
        rig.installs()[0]
            .0
            .contains("helixscreen-k1-v1.1.0-beta.10.tar.gz"),
        "{:?}",
        rig.installs()
    );
    // The check, the dry run and the install read one release: GitHub is asked about it once
    // (its unauthenticated API allows 60 requests an hour).
    let github = rig
        .fetch
        .requests()
        .into_iter()
        .filter(|u| u.starts_with(GITHUB_RELEASE_API))
        .count();
    assert_eq!(github, 1);
}

#[tokio::test]
async fn a_stock_k1_whose_release_cannot_be_installed_is_refused_before_simple_af() {
    // The beta has no K1 build: the release check must stop the run before the commit.
    let rig = k1_rig(STOCK_K1);
    rig.publish_with(
        Channel::Stable,
        "1.0.5",
        INSTALL_SH_WITHOUT_PROGRESS,
        &["k1"],
    );
    rig.publish_with(Channel::Beta, "1.1.0-beta.10", INSTALL_SH, &["pi"]);
    rig.ssh.script_job(vec![("Simple AF installed\n", Some(0))]);
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, stock_k1_answers).await;
    assert!(
        matches!(
            trace.end,
            Event::Failed {
                committed: false,
                ..
            } | Event::Refused { .. }
        ),
        "{:?}",
        trace.end
    );
    assert!(!r.state().committed);
    assert!(!ran(&rig, SIMPLE_AF), "{:?}", rig.ssh.commands());
}

#[tokio::test]
async fn simple_af_is_followed_by_a_power_cycle_and_a_wait_that_sees_the_reboot() {
    let m = staging();
    let steps = &m.recipes["k1_stock"];
    let simple_af = steps
        .iter()
        .position(|s| matches!(s, Step::SshRun { command, .. } if command == SIMPLE_AF))
        .unwrap();
    match &steps[simple_af + 1] {
        Step::UserAction { text, .. } => {
            assert!(text.contains("off"), "{text}");
            assert!(text.contains("10 seconds"), "{text}");
            assert!(text.contains("I've done this"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(
            steps[simple_af + 2],
            Step::WaitFor {
                target: WaitTarget::KlipperRestart,
                ..
            }
        ),
        "{:?}",
        steps[simple_af + 2]
    );
}

#[tokio::test]
async fn a_k1_without_the_root_option_is_refused_before_anything_runs() {
    let rig = k1_rig(STOCK_K1);
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, |p, _| match p {
        Prompt::Ask { choices, .. } => {
            assert!(choices.iter().any(|c| c.value == "no"));
            Input::Choice("no".into())
        }
        Prompt::Info { .. } => Input::Next,
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    match &trace.end {
        Event::Refused { reason, .. } => assert!(reason.contains("2025"), "{reason}"),
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(rig.ssh.commands().is_empty());
}

#[tokio::test]
async fn a_k1_on_old_firmware_is_refused_before_simple_af() {
    let rig = k1_rig(K1 {
        firmware: "1.3.2.20",
        ..STOCK_K1
    });
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, stock_k1_answers).await;
    match &trace.end {
        Event::Refused { reason, .. } => assert!(reason.contains("1.3.3.5"), "{reason}"),
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(!ran(&rig, SIMPLE_AF));
    assert!(rig.installs().is_empty());
}

#[tokio::test]
async fn a_k1_with_helper_script_leftovers_is_refused_before_simple_af() {
    let rig = k1_rig(K1 {
        leftovers: true,
        ..STOCK_K1
    });
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, stock_k1_answers).await;
    match &trace.end {
        Event::Refused { reason, .. } => {
            assert!(reason.contains("Helper Script"), "{reason}");
            assert!(reason.contains("factory reset"), "{reason}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(!ran(&rig, SIMPLE_AF));
}

#[tokio::test]
async fn a_grid_picked_k1_that_already_runs_helixscreen_updates_instead_of_simple_af() {
    let rig = k1_rig(K1 {
        helix_there: true,
        leftovers: true,
        ..STOCK_K1
    });
    rig.installer_succeeds("k1");
    rig.ssh
        .on_command(INSTALLED_VERSION_PROBE, out("v1.0.4\n", 0));
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, stock_k1_answers).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(r.state().recipe_id, "k1_update");
    assert!(!ran(&rig, SIMPLE_AF));
    assert!(rig.installs()[0].0.ends_with("--progress --update"));
}

#[tokio::test]
async fn a_printer_that_is_no_k1_after_simple_af_stops_the_run() {
    let rig = k1_rig(STOCK_K1);
    rig.ssh.script_job(vec![("done\n", Some(0))]);
    rig.installer_succeeds("k1");
    rig.moonraker.set_objects_list(
        ADDR,
        json!({ "objects": fleet("ad5m_forgex")["klipper_objects"].clone() }),
    );
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, stock_k1_answers).await;
    match &trace.end {
        Event::Failed {
            committed: true,
            reason,
            ..
        } => assert!(reason.contains("not the printer"), "{reason}"),
        other => panic!("expected a committed failure, got {other:?}"),
    }
    assert!(rig.installs().is_empty());
}

#[tokio::test]
async fn a_k1_with_moonraker_and_no_helixscreen_installs_without_simple_af() {
    let rig = k1_rig(K1 {
        leftovers: true,
        ..STOCK_K1
    });
    rig.installer_succeeds("k1");
    let mut f = fleet("k1c_helper_script");
    f.remove("helix_installed");
    f.remove("helix_version");
    f.insert(
        "services".into(),
        json!(["klipper", "moonraker", "klipper_mcu"]),
    );
    let m = staging();
    let picked = identify(&m, &f);
    assert_eq!(picked, recipe("k1", modded(), "k1_modded"));
    let mut r = rig.runner(&m, &picked, f);

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(!ran(&rig, SIMPLE_AF));
    assert!(rig.installs()[0].0.ends_with("--progress"));
}

#[tokio::test]
async fn k1_restore_says_how_far_it_goes_then_uninstalls() {
    let rig = k1_rig(K1 {
        helix_there: true,
        ..STOCK_K1
    });
    rig.installer_succeeds("k1");
    let m = staging();
    let picked = recipe("k1", StartState::Helix(String::new()), "k1_restore");
    let mut r = rig.runner(&m, &picked, fleet("k1c_helper_script"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    let texts = trace.texts();
    assert!(
        texts[0].contains("GrumpyScreen") && texts[0].contains("factory reset"),
        "{texts:?}"
    );
    assert_eq!(
        rig.installs()[0].0,
        "sh '/usr/data/install.sh' --uninstall --progress"
    );
}

// ---------------------------------------------------------------- FlashForge AD5X and AD5M

const FIRMWARE: &[u8] = b"mod firmware image bytes";

fn stick_drive() -> helix_installer_core::transport::Drive {
    helix_installer_core::transport::Drive {
        id: "usb1".into(),
        label: "STICK".into(),
        mount: PathBuf::from("/media/stick"),
        fs: "vfat".into(),
        size_bytes: 8_000_000_000,
        free_bytes: 7_000_000_000,
        removable: true,
        system: false,
    }
}

/// The staging manifest with `artifact` pointing at test bytes, served by the rig at its own URL
/// (the real file is far too large for a test), and a FAT32 stick plugged in.
fn with_test_firmware(rig: &Rig, artifact: &str) -> Manifest {
    let mut m = staging();
    let a = m.artifacts.get_mut(artifact).unwrap();
    a.sha256 = sha(FIRMWARE);
    rig.fetch.add(&a.url, FIRMWARE.to_vec());
    rig.drives.add(stick_drive());
    m
}

/// Objects a ZMOD AD5X lists. Not recorded from a printer: our AD5X was unplugged.
fn zmod_ad5x_objects() -> serde_json::Value {
    json!({ "objects": ["webhooks", "configfile", "mcu", "mcu eboard", "zmod", "zmod_ifs", "zmod_color", "extruder"] })
}

fn moonraker_answers(rig: &Rig, objects: serde_json::Value) {
    moonraker_answers_on(rig, objects, "mips");
}

/// Moonraker answers with `objects` on a `cpu` board (the AD5M's is armv7l).
fn moonraker_answers_on(rig: &Rig, objects: serde_json::Value, cpu: &str) {
    rig.moonraker
        .set_printer_info(ADDR, json!({ "state": "ready", "hostname": "printer" }));
    rig.moonraker.set_objects_list(ADDR, objects);
    rig.moonraker.set_system_info(
        ADDR,
        json!({ "system_info": { "cpu_info": { "processor": cpu }, "available_services": [] } }),
    );
}

/// Answers a FlashForge flash: the version `version`, the stick, Next on every screen, and the
/// printer's Moonraker comes up (listing `objects`) once the user says the stick is in.
fn flash_answers<'a>(
    rig: &'a Rig,
    version: &'a str,
    objects: serde_json::Value,
) -> impl FnMut(&Prompt, usize) -> Input + 'a {
    move |p, _| match p {
        Prompt::Ask { question, .. } if question.contains("version") => {
            Input::Choice(version.into())
        }
        Prompt::Ask { question, .. } if question.contains("HelixScreen showing") => {
            Input::Choice("yes".into())
        }
        Prompt::ChooseDrive { drives, .. } => Input::Drive(drives[0].id.clone()),
        Prompt::UserAction { text, .. } if text.contains("USB stick in") => {
            moonraker_answers(rig, objects.clone());
            Input::Next
        }
        Prompt::Info { .. } | Prompt::UserAction { .. } => Input::Next,
        other => panic!("unexpected prompt {other:?}"),
    }
}

#[test]
fn ad5x_zmod_artifact_is_the_upstream_release_file() {
    let m = staging();
    let a = &m.artifacts["zmod_1.7.4_ad5x"];
    assert_eq!(
        a.url,
        "https://github.com/ghzserg/zmod/releases/download/1.7.4/AD5X-zmod-1.7.4.tgz"
    );
    assert_eq!(
        a.sha256,
        "6f230debd47343a3bec1284e009ea341f34f8d2cdd96b896588349eef4e7f565"
    );
}

#[tokio::test]
async fn a_stock_ad5x_picked_from_the_grid_gets_zmod_then_helixscreen() {
    let rig = Rig::new();
    let m = with_test_firmware(&rig, "zmod_1.7.4_ad5x");
    let picked = identify_as(&m, "ad5x", &Facts::new());
    assert_eq!(picked, recipe("ad5x", StartState::Stock, "ad5x_zmod"));
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, flash_answers(&rig, "1.1.7", zmod_ad5x_objects())).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        rig.drives.file("usb1", "AD5X-zmod-1.7.4.tgz").as_deref(),
        Some(FIRMWARE)
    );
    assert_eq!(
        rig.moonraker.gcodes(),
        vec![
            (ADDR.to_string(), "ENABLE_EXTRA_PLUGINS".to_string()),
            (ADDR.to_string(), "DISPLAY_OFF HELIX=1".to_string())
        ],
        "ZMOD's HelixScreen update_manager entry comes with its extra plugins"
    );
    assert_eq!(rig.moonraker.upgrades(), vec!["helixscreen".to_string()]);
    let texts = trace.texts();
    assert!(
        texts.iter().any(|t| t.contains("Remove the USB stick")),
        "{texts:?}"
    );
    assert!(r.state().committed);
}

#[tokio::test]
async fn an_ad5x_outside_the_zmod_window_is_refused_before_the_stick() {
    for version in ["0"] {
        let rig = Rig::new();
        let m = with_test_firmware(&rig, "zmod_1.7.4_ad5x");
        let picked = identify_as(&m, "ad5x", &Facts::new());
        let mut r = rig.runner(&m, &picked, Facts::new());

        let trace = drive(&mut r, flash_answers(&rig, version, zmod_ad5x_objects())).await;
        assert!(
            matches!(trace.end, Event::Refused { .. }),
            "{version}: {:?}",
            trace.end
        );
        assert_eq!(rig.drives.file("usb1", "AD5X-zmod-1.7.4.tgz"), None);
    }
}

/// The version each FlashForge stock recipe asks for, and the range its `require` accepts.
fn asked_versions(recipe_id: &str) -> (Vec<String>, helix_installer_core::version::VersionRange) {
    let m = staging();
    let steps = &m.recipes[recipe_id];
    let choices = steps
        .iter()
        .find_map(|s| match s {
            helix_installer_core::manifest::Step::Ask { choices, .. } => {
                Some(choices.iter().map(|c| c.value.clone()).collect())
            }
            _ => None,
        })
        .unwrap();
    let range = steps
        .iter()
        .find_map(|s| match s {
            helix_installer_core::manifest::Step::Require {
                range, warn: false, ..
            } => range.clone(),
            _ => None,
        })
        .unwrap();
    (choices, range)
}

#[test]
fn the_ad5x_window_is_zmods_list_and_nothing_between() {
    use helix_installer_core::version::FwVersion;
    let (choices, range) = asked_versions("ad5x_zmod");
    let ok = |v: &str| range.matches(&FwVersion::parse(v).unwrap());
    for v in [
        "1.1.7", "1.1.9", "1.2.0", "1.2.3", "3.0.3", "3.0.9", "3.1.0",
    ] {
        assert!(ok(v), "{v}");
        assert!(choices.iter().any(|c| c == v), "{v} is offered");
    }
    for v in [
        "1.1.6", "1.1.8", "1.2.4", "3.0.4", "3.1.1", "3.1.4", "3.1.5", "0",
    ] {
        assert!(!ok(v), "{v}");
    }
}

#[test]
fn the_ad5m_window_is_forgexs_own() {
    use helix_installer_core::version::FwVersion;
    for recipe_id in ["ad5m_forgex", "ad5m_pro_forgex"] {
        let (choices, range) = asked_versions(recipe_id);
        let ok = |v: &str| range.matches(&FwVersion::parse(v).unwrap());
        for v in [
            "2.6.5", "2.7.8", "3.1.3", "3.1.4", "3.1.5", "3.1.6", "3.2.3", "5.0.3", "5.1.7",
        ] {
            assert!(ok(v), "{v}");
        }
        for v in ["2.6.4", "5.2", "5.2.1", "6.0"] {
            assert!(!ok(v), "{v}");
        }
        assert!(
            choices.iter().all(|c| FwVersion::parse(c).is_some()),
            "{choices:?}"
        );
    }
}

#[tokio::test]
async fn an_ad5x_that_answers_as_something_else_after_the_flash_stops() {
    let rig = Rig::new();
    let m = with_test_firmware(&rig, "zmod_1.7.4_ad5x");
    let picked = identify_as(&m, "ad5x", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let other = json!({ "objects": fleet("k1c_helper_script")["klipper_objects"].clone() });
    let trace = drive(&mut r, flash_answers(&rig, "1.2.3", other)).await;
    match &trace.end {
        Event::Failed {
            committed: true,
            reason,
            ..
        } => assert!(
            reason.contains("not the printer") || reason.contains("cannot confirm"),
            "{reason}"
        ),
        other => panic!("expected a committed failure, got {other:?}"),
    }
    assert!(rig.moonraker.gcodes().is_empty());
}

/// Objects a ZMOD AD5M lists: ZMOD's own, but not the AD5X's IFS.
fn zmod_ad5m_objects() -> serde_json::Value {
    json!({ "objects": ["webhooks", "configfile", "mcu", "mcu eboard", "zmod", "extruder"] })
}

#[tokio::test]
async fn an_ad5x_found_by_udp_whose_address_answers_as_another_zmod_printer_after_the_flash_stops()
{
    // After the reboot DHCP hands the AD5X's address to an AD5M on ZMOD. Its object list carries
    // nothing of the AD5X's, and the UDP model from before the flash must not stand in for it.
    let rig = Rig::new();
    let m = with_test_firmware(&rig, "zmod_1.7.4_ad5x");
    let f: Facts = serde_json::from_value(
        json!({ "flashforge_udp.model": "AD5X", "model": "AD5X", "serial": "SERIAL" }),
    )
    .unwrap();
    let picked = identify(&m, &f);
    assert_eq!(picked, recipe("ad5x", StartState::Stock, "ad5x_zmod"));
    let mut r = rig.runner(&m, &picked, f);

    let trace = drive(&mut r, flash_answers(&rig, "1.2.3", zmod_ad5m_objects())).await;
    match &trace.end {
        Event::Failed {
            committed: true,
            reason,
            ..
        } => assert!(reason.contains("FlashForge Adventurer 5X"), "{reason}"),
        other => panic!("expected a committed failure, got {other:?}"),
    }
    assert!(rig.moonraker.gcodes().is_empty());
    assert!(rig.moonraker.upgrades().is_empty());
}

#[tokio::test]
async fn an_ad5x_found_by_its_udp_reply_gets_the_zmod_recipe() {
    let f: Facts = serde_json::from_value(
        json!({ "flashforge_udp.model": "AD5X", "model": "AD5X", "serial": "SERIAL" }),
    )
    .unwrap();
    assert_eq!(
        identify(&staging(), &f),
        recipe("ad5x", StartState::Stock, "ad5x_zmod")
    );
}

#[tokio::test]
async fn an_ad5x_with_zmod_only_switches_the_screen() {
    let rig = Rig::new();
    moonraker_answers(&rig, zmod_ad5x_objects());
    let f: Facts = serde_json::from_value(json!({
        "has_moonraker": true, "cpu_arch": "mips",
        "klipper_objects": zmod_ad5x_objects()["objects"].clone(),
    }))
    .unwrap();
    let m = staging();
    let picked = identify(&m, &f);
    assert_eq!(picked, recipe("ad5x", modded(), "ad5x_enable_helix"));
    let mut r = rig.runner(&m, &picked, f);

    let trace = drive(&mut r, flash_answers(&rig, "", json!(null))).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        rig.moonraker.gcodes(),
        vec![
            (ADDR.to_string(), "ENABLE_EXTRA_PLUGINS".to_string()),
            (ADDR.to_string(), "DISPLAY_OFF HELIX=1".to_string())
        ],
        "ZMOD's HelixScreen update_manager entry comes with its extra plugins"
    );
    assert_eq!(rig.moonraker.upgrades(), vec!["helixscreen".to_string()]);
}

#[tokio::test]
async fn an_ad5x_with_helixscreen_updates_through_zmods_update_manager() {
    let rig = Rig::new();
    let f: Facts = serde_json::from_value(json!({
        "has_moonraker": true, "helix_installed": true, "helix_version": "v1.0.4",
        "klipper_objects": zmod_ad5x_objects()["objects"].clone(),
    }))
    .unwrap();
    // ZMOD's Moonraker reports HelixScreen's service once the update has restarted it.
    rig.moonraker.set_system_info(
        ADDR,
        json!({ "system_info": { "service_state": { "helixscreen": { "active_state": "active" } } } }),
    );
    let m = staging();
    let picked = identify(&m, &f);
    let mut r = rig.runner(&m, &picked, f);
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(rig.moonraker.upgrades(), vec!["helixscreen".to_string()]);
}

#[tokio::test]
async fn ad5x_restore_switches_back_to_the_stock_screen() {
    let rig = Rig::new();
    let m = staging();
    moonraker_answers(&rig, zmod_ad5x_objects());
    let picked = recipe("ad5x", StartState::Helix(String::new()), "ad5x_restore");
    let mut r = rig.runner(&m, &picked, Facts::new());
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(
        trace.texts()[0].contains("REMOVE_ZMOD"),
        "{:?}",
        trace.texts()
    );
    assert_eq!(
        rig.moonraker.gcodes(),
        vec![(ADDR.to_string(), "DISPLAY_ON".to_string())]
    );
}

#[test]
fn forgex_artifact_is_the_upstream_release_file() {
    let m = staging();
    let a = &m.artifacts["forgex_1.4.1"];
    assert_eq!(
        a.url,
        "https://github.com/DrA1ex/ff5m/releases/download/1.4.1/Adventurer5M-ForgeX-1.4.1.tgz"
    );
    assert_eq!(
        a.sha256,
        "ba9c0745a84f299b52342cadd3ca4146177ecc552ac9ee27157e0ef39aef43b4"
    );
}

fn forgex_objects() -> serde_json::Value {
    json!({ "objects": fleet("ad5m_forgex")["klipper_objects"].clone() })
}

const INIT_SCRIPTS_PROBE: &str = "for f in /etc/init.d/S80helixscreen /etc/init.d/S90helixscreen /etc/init.d/S99helixscreen /etc/init.d/helixscreen /opt/config/mod_data/helixscreen_payload_root; do [ -e \"$f\" ] && { echo yes; exit 0; }; done; echo no";

/// A Forge-X AD5M on root/root after the flash, without HelixScreen. Its installer succeeds,
/// then the restart that starts HelixScreen is launched.
fn forgex_rig() -> Rig {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "root", "root");
    rig.ssh.on_command(INIT_SCRIPTS_PROBE, out("no\n", 0));
    rig.installer_succeeds("ad5m");
    rig.ssh.script_job(vec![("", Some(0))]);
    moonraker_answers_on(&rig, forgex_objects(), "armv7l");
    // The reboot job takes Moonraker down for a while, as a real restart does.
    let moonraker = rig.moonraker.clone();
    rig.ssh.on_spawn(move |cmd| {
        if cmd.contains("/sbin/reboot") {
            moonraker.restart(ADDR, 3);
        }
    });
    rig
}

#[tokio::test]
async fn a_forgex_printer_that_does_not_restart_fails_at_the_restart() {
    let rig = forgex_rig();
    rig.ssh.on_spawn(|_| {});
    let m = staging();
    let picked = identify_as(&m, "ad5m", &fleet("ad5m_forgex"));
    let mut r = rig.runner(&m, &picked, fleet("ad5m_forgex"));
    let trace = drive(&mut r, next_only).await;
    match &trace.end {
        Event::Failed { reason, .. } => assert!(reason.contains("did not restart"), "{reason}"),
        e => panic!("expected the restart to fail, got {e:?}"),
    }
}

#[test]
fn the_forgex_reboot_names_its_binary_and_fails_when_it_is_missing() {
    let m = staging();
    for id in [
        "ad5m_forgex",
        "ad5m_pro_forgex",
        "ad5m_forgex_install",
        "ad5m_forgex_update",
    ] {
        let cmd = m.recipes[id]
            .iter()
            .find_map(|s| match s {
                Step::SshRun { command, .. } if command.contains("reboot") => Some(command.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{id} has no reboot"));
        assert!(cmd.contains("[ -x /sbin/reboot ]"), "{id}: {cmd}");
        assert!(
            !cmd.replace("/sbin/reboot", "").contains("reboot"),
            "{id}: a bare reboot depends on PATH: {cmd}"
        );
    }
}

/// The detached commands that were not `install.sh`.
fn other_jobs(rig: &Rig) -> Vec<String> {
    let installs: Vec<String> = rig.installer_runs().into_iter().map(|(c, _)| c).collect();
    rig.ssh
        .commands()
        .into_iter()
        .filter(|c| c.contains("reboot") && !installs.contains(c))
        .collect()
}

#[tokio::test]
async fn a_forgex_install_restarts_the_printer_and_ends_only_once_helixscreen_runs() {
    for (recipe_id, installed) in [
        ("ad5m_forgex_install", "no\n"),
        ("ad5m_forgex_update", "yes\n"),
    ] {
        let rig = forgex_rig();
        rig.ssh.on_command(INIT_SCRIPTS_PROBE, out(installed, 0));
        rig.ssh
            .on_command(INSTALLED_VERSION_PROBE, out("v1.0.4\n", 0));
        let m = staging();
        let picked = identify_as(&m, "ad5m", &fleet("ad5m_forgex"));
        let mut r = rig.runner(&m, &picked, fleet("ad5m_forgex"));
        let trace = drive(&mut r, next_only).await;
        assert!(trace.done(), "{recipe_id}: {:?}", trace.end);
        assert_eq!(r.state().recipe_id, recipe_id);

        let cmds = rig.ssh.commands();
        let install = cmds
            .iter()
            .rposition(|c| {
                c.starts_with("sh ") && c.contains("install.sh") && !c.contains("--dry-run")
            })
            .unwrap();
        let reboot = cmds
            .iter()
            .position(|c| c.contains("reboot"))
            .expect("a restart");
        let seen = cmds
            .iter()
            .rposition(|c| c == "pidof helix-screen")
            .expect("a check");
        assert!(install < reboot && reboot < seen, "{recipe_id}: {cmds:?}");
        assert_eq!(other_jobs(&rig).len(), 1, "{:?}", other_jobs(&rig));
    }
}

#[tokio::test]
async fn a_forgex_install_whose_helixscreen_never_starts_is_not_done() {
    let rig = forgex_rig();
    rig.ssh.on_command("pidof helix-screen", out("", 1));
    let m = staging();
    let picked = identify_as(&m, "ad5m", &fleet("ad5m_forgex"));
    let mut r = rig.runner(&m, &picked, fleet("ad5m_forgex"));
    let trace = drive(&mut r, next_only).await;
    match &trace.end {
        Event::Failed { reason, .. } => assert!(reason.contains("HelixScreen"), "{reason}"),
        e => panic!("expected the run to fail, got {e:?}"),
    }
}

#[test]
fn every_install_and_update_recipe_ends_by_seeing_helixscreen_run() {
    let m = staging();
    let restores: Vec<&str> = m
        .printers
        .iter()
        .filter_map(|p| p.restore.as_ref())
        .flat_map(|r| r.recipes())
        .collect();
    for (id, steps) in &m.recipes {
        if restores.contains(&id.as_str()) {
            continue;
        }
        // A panel flashed over USB is seen running by its serial_flash step, which reads the
        // version it boots; an info step may follow it.
        let flashed = steps
            .iter()
            .rev()
            .find(|s| !matches!(s, Step::Info { .. }))
            .is_some_and(|s| matches!(s, Step::SerialFlash { .. }));
        assert!(
            flashed
                || matches!(
                    steps.last(),
                    Some(Step::WaitFor {
                        target: WaitTarget::HelixService,
                        ..
                    })
                ),
            "{id} does not end by waiting for HelixScreen"
        );
    }
}

#[tokio::test]
async fn a_stock_ad5m_gets_forgex_under_its_own_name_then_helixscreen_in_data() {
    for (printer, stick_name) in [
        ("ad5m", "Adventurer5M-ForgeX-1.4.1.tgz"),
        ("ad5m_pro", "Adventurer5MPro-ForgeX-1.4.1.tgz"),
    ] {
        let rig = forgex_rig();
        let m = with_test_firmware(&rig, "forgex_1.4.1");
        let picked = identify_as(&m, printer, &Facts::new());
        let mut r = rig.runner(&m, &picked, Facts::new());

        let trace = drive(&mut r, flash_answers(&rig, "2.6.5", forgex_objects())).await;
        assert!(trace.done(), "{printer}: {:?}", trace.end);
        assert_eq!(
            rig.drives.file("usb1", stick_name).as_deref(),
            Some(FIRMWARE),
            "{printer}"
        );
        assert_eq!(
            rig.installs()[0].0,
            "sh '/data/install.sh' --local '/data/helixscreen-ad5m-v1.1.0.tar.gz' --progress"
        );
    }
}

#[tokio::test]
async fn an_ad5m_on_a_firmware_forgex_cannot_take_is_refused_before_the_stick() {
    for version in ["2.6.4", "5.2"] {
        let rig = forgex_rig();
        let m = with_test_firmware(&rig, "forgex_1.4.1");
        let picked = identify_as(&m, "ad5m", &Facts::new());
        let mut r = rig.runner(&m, &picked, Facts::new());

        let trace = drive(&mut r, flash_answers(&rig, version, forgex_objects())).await;
        assert!(
            matches!(trace.end, Event::Refused { .. }),
            "{version}: {:?}",
            trace.end
        );
        assert!(rig
            .drives
            .file("usb1", "Adventurer5M-ForgeX-1.4.1.tgz")
            .is_none());
    }
}

const FACTORY_IMAGE_DOC: &str =
    "https://github.com/DrA1ex/ff5m/blob/main/docs/UNINSTALL.md#flashing-factory-firmware";

/// A stock AD5M on firmware 3.1.5, answered with `at_warning` at the config-files warning.
async fn forgex_on_3_1_5(printer: &str, at_warning: Input) -> (Rig, Trace, Vec<Prompt>) {
    let rig = forgex_rig();
    let m = with_test_firmware(&rig, "forgex_1.4.1");
    let picked = identify_as(&m, printer, &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());
    let mut warnings = Vec::new();
    let mut answer = flash_answers(&rig, "3.1.5", forgex_objects());
    let trace = drive(&mut r, |p, n| match p {
        Prompt::Confirm { .. } => {
            warnings.push(p.clone());
            at_warning.clone()
        }
        other => answer(other, n),
    })
    .await;
    drop(answer);
    (rig, trace, warnings)
}

#[tokio::test]
async fn an_ad5m_on_3_1_5_is_warned_about_its_config_files_and_may_go_on() {
    for (printer, stick_name) in [
        ("ad5m", "Adventurer5M-ForgeX-1.4.1.tgz"),
        ("ad5m_pro", "Adventurer5MPro-ForgeX-1.4.1.tgz"),
    ] {
        let (rig, trace, warnings) = forgex_on_3_1_5(printer, Input::Next).await;
        assert!(trace.done(), "{printer}: {:?}", trace.end);
        match &warnings[..] {
            [Prompt::Confirm {
                text, doc, warn, ..
            }] => {
                assert!(*warn, "{printer}: a require warning reads as a caution");
                assert!(text.contains("Factory"), "{text}");
                assert!(text.contains("prints normally"), "{text}");
                assert_eq!(doc.as_deref(), Some(FACTORY_IMAGE_DOC));
            }
            other => panic!("{printer}: one warning expected, got {other:?}"),
        }
        assert_eq!(
            rig.drives.file("usb1", stick_name).as_deref(),
            Some(FIRMWARE),
            "{printer}"
        );
    }
}

#[tokio::test]
async fn an_ad5m_on_other_supported_firmware_gets_no_warning() {
    for version in ["2.6.5", "3.1.6"] {
        let rig = forgex_rig();
        let m = with_test_firmware(&rig, "forgex_1.4.1");
        let picked = identify_as(&m, "ad5m", &Facts::new());
        let mut r = rig.runner(&m, &picked, Facts::new());
        // flash_answers panics on any prompt it does not expect, a warning included.
        let trace = drive(&mut r, flash_answers(&rig, version, forgex_objects())).await;
        assert!(trace.done(), "{version}: {:?}", trace.end);
    }
}

#[tokio::test]
async fn stopping_at_the_3_1_5_warning_writes_nothing() {
    let (rig, trace, warnings) = forgex_on_3_1_5("ad5m", Input::Cancel).await;
    assert_eq!(warnings.len(), 1);
    assert!(
        matches!(
            trace.end,
            Event::Refused {
                cancelled: true,
                ..
            }
        ),
        "{:?}",
        trace.end
    );
    assert!(rig
        .drives
        .file("usb1", "Adventurer5M-ForgeX-1.4.1.tgz")
        .is_none());
}

#[tokio::test]
async fn the_forgex_ad5m_picked_as_a_pro_installs_over_ssh() {
    let rig = forgex_rig();
    let m = staging();
    let picked = identify_as(&m, "ad5m_pro", &fleet("ad5m_forgex"));
    assert_eq!(picked, recipe("ad5m_pro", modded(), "ad5m_forgex_install"));
    let mut r = rig.runner(&m, &picked, fleet("ad5m_forgex"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(rig.installs()[0]
        .0
        .starts_with("sh '/data/install.sh' --local"));
}

#[tokio::test]
async fn a_forgex_ad5m_with_helixscreen_switches_to_the_update() {
    let rig = forgex_rig();
    rig.ssh.on_command(INIT_SCRIPTS_PROBE, out("yes\n", 0));
    rig.ssh
        .on_command(INSTALLED_VERSION_PROBE, out("v1.0.4\n", 0));
    let m = staging();
    let picked = identify_as(&m, "ad5m", &fleet("ad5m_forgex"));
    let mut r = rig.runner(&m, &picked, fleet("ad5m_forgex"));

    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(r.state().recipe_id, "ad5m_forgex_update");
    assert!(rig.installs()[0].0.ends_with("--progress --update"));
}

#[tokio::test]
async fn a_zmod_ad5m_takes_the_macro_step() {
    let rig = Rig::new();
    let f: Facts = serde_json::from_value(json!({
        "has_moonraker": true, "cpu_arch": "armv7l",
        "klipper_objects": ["mcu", "mcu eboard", "zmod", "zmod_color", "fan_generic fanM106"],
    }))
    .unwrap();
    moonraker_answers_on(
        &rig,
        json!({ "objects": f["klipper_objects"].clone() }),
        "armv7l",
    );
    let m = staging();
    let picked = identify_as(&m, "ad5m", &f);
    assert_eq!(picked, recipe("ad5m", modded(), "ad5m_zmod_enable"));
    let mut r = rig.runner(&m, &picked, f);

    let trace = drive(&mut r, flash_answers(&rig, "", json!(null))).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        rig.moonraker.gcodes(),
        vec![
            (ADDR.to_string(), "ENABLE_EXTRA_PLUGINS".to_string()),
            (ADDR.to_string(), "DISPLAY_OFF HELIX=1".to_string())
        ],
        "ZMOD's HelixScreen update_manager entry comes with its extra plugins"
    );
    assert_eq!(rig.moonraker.upgrades(), vec!["helixscreen".to_string()]);
}

#[tokio::test]
async fn ad5m_restore_uninstalls_and_points_at_forgex_for_fully_stock() {
    let rig = forgex_rig();
    let m = staging();
    let picked = recipe("ad5m", StartState::Helix(String::new()), "ad5m_restore");
    let mut r = rig.runner(&m, &picked, fleet("ad5m_forgex"));
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(
        trace.texts()[0].contains("uninstall image"),
        "{:?}",
        trace.texts()
    );
    assert_eq!(
        rig.installs()[0].0,
        "sh '/data/install.sh' --uninstall --progress"
    );
}

// ---------------------------------------------------------------- dry runs

/// Every recipe that installs HelixScreen asks install.sh for a dry run of the same install
/// first, and that dry run comes before the commit unless the recipe committed earlier.
#[test]
fn every_install_is_preceded_by_its_dry_run() {
    use helix_installer_core::manifest::Step;
    let m = staging();
    let mut checked = 0;
    for (id, steps) in &m.recipes {
        for (i, s) in steps.iter().enumerate() {
            let Step::HelixInstall {
                platform,
                update,
                uninstall: false,
                dry_run: false,
                check: false,
                dir,
                ..
            } = s
            else {
                continue;
            };
            let Some(Step::HelixInstall {
                platform: p,
                update: u,
                dry_run: true,
                dir: d,
                commits: false,
                ..
            }) = i.checked_sub(1).map(|j| &steps[j])
            else {
                panic!("{id}: step {i} installs without a dry run right before it");
            };
            assert_eq!(
                (serde_json::to_value(p).unwrap(), u, d),
                (serde_json::to_value(platform).unwrap(), update, dir),
                "{id}"
            );
            let commit = steps.iter().position(|s| s.commits()).unwrap();
            assert!(commit <= i, "{id}");
            checked += 1;
        }
    }
    assert!(checked >= 10, "{checked}");
}

#[tokio::test]
async fn a_dry_run_naming_another_platform_stops_the_install_before_the_commit() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "pi", "raspberry");
    rig.ssh.script_job(vec![
        (&pl(&["platform", "snapmaker-u1"]), None),
        (&pl(&["result", "ok"]), Some(0)),
    ]);
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
    match &trace.end {
        Event::Failed {
            committed: false,
            reason,
            ..
        } => assert!(reason.contains("snapmaker-u1"), "{reason}"),
        other => panic!("expected an uncommitted failure, got {other:?}"),
    }
    let runs = rig.installer_runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(
        runs[0].0,
        "sh '/tmp/install.sh' --local '/tmp/helixscreen-pi-v1.1.0.tar.gz' --progress --dry-run"
    );
}

#[tokio::test]
async fn a_zmod_printer_whose_extra_plugins_are_on_skips_their_restart() {
    for (printer, objects) in [
        ("ad5x", zmod_ad5x_objects()["objects"].clone()),
        ("ad5m", json!(["mcu", "mcu eboard", "zmod", "zmod_color"])),
    ] {
        let rig = Rig::new();
        let cpu = if printer == "ad5x" { "mips" } else { "armv7l" };
        moonraker_answers_on(&rig, json!({ "objects": objects.clone() }), cpu);
        // ZMOD seeds HelixScreen's update_manager entry at v0.0.1 when the extra plugins go on;
        // nothing has been installed through it yet.
        let f: Facts = serde_json::from_value(json!({
            "has_moonraker": true, "cpu_arch": if printer == "ad5x" { "mips" } else { "armv7l" },
            "helix_installed": true, "helix_version": "v0.0.1", "klipper_objects": objects,
        }))
        .unwrap();
        let m = staging();
        let picked = identify_as(&m, printer, &f);
        let Identified::Recipe { recipe_id, .. } = &picked else {
            panic!("{printer}: {picked:?}")
        };
        assert!(
            recipe_id.ends_with("_switch_screen"),
            "{printer}: {recipe_id}"
        );
        let mut r = rig.runner(&m, &picked, f);
        let trace = drive(&mut r, flash_answers(&rig, "", json!(null))).await;
        assert!(trace.done(), "{printer}: {:?}", trace.end);
        assert_eq!(
            rig.moonraker.gcodes(),
            vec![(ADDR.to_string(), "DISPLAY_OFF HELIX=1".to_string())],
            "{printer}"
        );
        assert_eq!(rig.moonraker.upgrades(), vec!["helixscreen".to_string()]);
    }
}

#[test]
fn the_zmod_restart_is_waited_for_not_confirmed_by_the_user() {
    use helix_installer_core::manifest::{Step, WaitTarget};
    let m = staging();
    for id in ["ad5x_zmod", "ad5x_enable_helix", "ad5m_zmod_enable"] {
        let steps = &m.recipes[id];
        let at = steps
            .iter()
            .position(|s| matches!(s, Step::MoonrakerGcode { script, .. } if script == "ENABLE_EXTRA_PLUGINS"))
            .unwrap();
        assert!(
            matches!(
                steps[at + 1],
                Step::WaitFor {
                    target: WaitTarget::KlipperRestart,
                    ..
                }
            ),
            "{id}: {:?}",
            steps[at + 1]
        );
    }
}

// ---------------------------------------------------------------- refused printers

fn facts(v: serde_json::Value) -> Facts {
    serde_json::from_value(v).unwrap()
}

fn refused_as(f: &Facts) -> Option<(String, String)> {
    match identify(&staging(), f) {
        Identified::Refuse {
            printer_id: Some(p),
            doc,
            ..
        } => Some((p, doc)),
        _ => None,
    }
}

#[test]
fn a_host_whose_moonraker_gives_no_board_fact_is_refused_with_the_manual_install_guide() {
    for host in ["pi3_mainsailos", "cb1_voron"] {
        let mut f = fleet(host);
        f.remove("cpu_model");
        assert_eq!(
            identify(&staging(), &f),
            Identified::Refuse {
                printer_id: None,
                doc: GENERIC_REFUSE_DOC.into(),
                reason: "printer not recognised".into(),
            },
            "{host}"
        );
    }
}

/// The `ssh` command of the first probe whose fact is `board_ok`.
fn board_probe() -> String {
    staging().recipes["generic_install_board"]
        .iter()
        .find_map(|s| match s {
            helix_installer_core::manifest::Step::Probe {
                ssh: Some(cmd),
                fact: Some(fact),
                ..
            } if fact == "board_ok" => Some(cmd.clone()),
            _ => None,
        })
        .unwrap()
}

/// Runs the board probe with a real `sh`, reading `armbian` and `dt_model` in place of
/// /etc/armbian-release and /proc/device-tree/model.
#[cfg(target_os = "linux")]
fn run_board_probe(armbian: Option<&str>, dt_model: Option<&[u8]>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let rel = dir.path().join("armbian-release");
    let model = dir.path().join("model");
    if let Some(a) = armbian {
        std::fs::write(&rel, a).unwrap();
    }
    if let Some(m) = dt_model {
        std::fs::write(&model, m).unwrap();
    }
    let cmd = board_probe()
        .replace("/etc/armbian-release", rel.to_str().unwrap())
        .replace("/proc/device-tree/model", model.to_str().unwrap());
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

// Runs the probe in the local `sh`: it is written for the printer's Linux userland (GNU or
// BusyBox `timeout`, POSIX paths), which macOS and Windows runners do not have.
#[cfg(target_os = "linux")]
#[test]
fn the_board_probe_accepts_known_sbcs_and_nothing_else() {
    let armbian = |board: &str| format!("BOARD={board}\nBOARD_NAME=\"x\"\nBOARDFAMILY=sun50iw9\n");
    for board in [
        "bigtreetech-cb1",
        "bigtreetech-cb2",
        "orangepi5",
        "orangepizero2",
        "rock-5b",
        "radxa-zero3",
    ] {
        assert_eq!(
            run_board_probe(Some(&armbian(board)), None),
            "yes",
            "{board}"
        );
    }
    for board in ["mkspi", "qidi", "sovol-sv08", ""] {
        assert_eq!(
            run_board_probe(Some(&armbian(board)), None),
            "no",
            "{board:?}"
        );
    }
    assert_eq!(
        run_board_probe(None, Some(b"Raspberry Pi 4 Model B Rev 1.4\0")),
        "yes"
    );
    assert_eq!(run_board_probe(None, Some(b"Rockchip RK3308 EVB\0")), "no");
    assert_eq!(
        run_board_probe(None, None),
        "no",
        "a host that names no board"
    );
}

#[tokio::test]
async fn a_cb1_voron_whose_board_is_not_whitelisted_is_refused_before_any_change() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "biqu", "biqu");
    rig.ssh.set_fallback(|cmd| match cmd {
        "pidof helix-screen" => out("1234\n", 0),
        "id -u" => out("1000\n", 0),
        c if c == board_probe() => out("no\n", 0),
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
    match &trace.end {
        Event::Refused { doc, .. } => assert_eq!(doc, GENERIC_REFUSE_DOC),
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(!r.state().committed);
    assert!(rig.installs().is_empty());
}

#[tokio::test]
async fn the_pi3_installs_without_the_ssh_board_check() {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "pi", "raspberry");
    rig.installer_succeeds("pi");
    let m = staging();
    let picked = identify(&m, &fleet("pi3_mainsailos"));
    assert_eq!(
        picked,
        recipe(
            "generic_klipper",
            StartState::Helix("v1.0.0".into()),
            "generic_update"
        )
    );
    let mut r = rig.runner(&m, &picked, fleet("pi3_mainsailos"));
    let trace = drive(&mut r, |p, _| match p {
        Prompt::Login { .. } => login("pi", "raspberry"),
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(!rig.ssh.commands().contains(&board_probe()));
}

#[test]
fn a_specific_printer_with_no_board_model_is_never_generic() {
    let m = staging();
    for (name, id) in [
        ("qidi_box", "qidi"),
        ("k2_plus", "k2"),
        ("k1c_helper_script", "k1"),
    ] {
        assert!(
            matches!(identify(&m, &fleet(name)), Identified::Recipe { ref printer_id, .. } if printer_id == id),
            "{name}: {:?}",
            identify(&m, &fleet(name))
        );
        assert!(
            matches!(
                identify_as(&m, "generic_klipper", &fleet(name)),
                Identified::Refuse { .. }
            ),
            "{name} picked as generic"
        );
    }
}

#[test]
fn an_unknown_board_on_debian_is_refused_not_given_the_pi_build() {
    let mut f = fleet("pi3_mainsailos");
    f.insert("cpu_model".into(), "Rockchip RK3562 EVB".into());
    assert!(matches!(
        identify(&staging(), &f),
        Identified::Refuse {
            printer_id: None,
            ..
        }
    ));
}

/// Synthetic facts for the printers the spec refuses. Each also carries a board model the
/// generic rule accepts, so the refusal must win over it.
#[test]
fn the_printers_the_spec_refuses_are_refused_with_their_own_guide() {
    let base = |objects: serde_json::Value, hostname: &str| {
        facts(json!({
            "has_moonraker": true, "cpu_arch": "aarch64", "cpu_model": "BigTreeTech CB1",
            "os_distribution": "Debian GNU/Linux 12 (bookworm)", "klipper_hostname": hostname,
            "klipper_objects": objects,
        }))
    };
    let cases = [
        (
            "snapmaker_u1",
            base(json!(["machine_state_manager", "extruder"]), "u1"),
        ),
        (
            "snapmaker_u1",
            base(json!(["homing_precise_corexy"]), "lava"),
        ),
        (
            "qidi_tjc",
            base(
                json!([
                    "temperature_sensor Chamber_Thermal_Protection_Sensor",
                    "heater_generic chamber"
                ]),
                "mkspi",
            ),
        ),
        (
            "sovol",
            base(json!(["probe_pressure", "quad_gantry_level"]), "SV08"),
        ),
        (
            "elegoo_cc1",
            base(json!(["gcode_macro _COSMOS_SETTINGS"]), "cosmos"),
        ),
        (
            "flashforge_creator5",
            base(json!(["ff_toolchange", "zmod"]), "creator5"),
        ),
        (
            "anycubic_kobra",
            base(json!(["filament_hub", "extruder"]), "kobra"),
        ),
        ("elegoo_neptune4", base(json!(["extruder"]), "neptune4")),
    ];
    let m = staging();
    for (want, f) in cases {
        let (got, doc) = refused_as(&f).unwrap_or_else(|| panic!("{want}: {:?}", identify(&m, &f)));
        assert_eq!(got, want);
        let p = m.printers.iter().find(|p| p.id == want).unwrap();
        assert_eq!(doc, p.fallback.doc);
        assert!(p.refused && p.recipes.is_empty(), "{want}");
    }
    let creator: Facts =
        facts(json!({ "flashforge_udp.model": "Creator5Pro", "model": "Creator5Pro" }));
    assert_eq!(
        refused_as(&creator).map(|r| r.0).as_deref(),
        Some("flashforge_creator5")
    );
}

#[test]
fn no_fleet_printer_is_caught_by_a_refusal() {
    let m = staging();
    let refused: Vec<&str> = m
        .printers
        .iter()
        .filter(|p| p.refused)
        .map(|p| p.id.as_str())
        .collect();
    for name in ["qidi_box", "k2_plus", "k1c_helper_script", "ad5m_forgex"] {
        if let Identified::Refuse {
            printer_id: Some(p),
            ..
        } = identify(&m, &fleet(name))
        {
            assert!(!refused.contains(&p.as_str()), "{name} refused as {p}");
        }
    }
    for name in ["pi3_mainsailos", "cb1_voron"] {
        assert!(
            matches!(identify(&m, &fleet(name)), Identified::Recipe { .. }),
            "{name}"
        );
    }
}

#[test]
fn a_qidi_plus_5_is_a_qidi_by_its_chamber_sensor() {
    let plus5 = facts(json!({
        "has_moonraker": true, "cpu_arch": "aarch64", "klipper_hostname": "qidi-plus5",
        "os_distribution": "Debian GNU/Linux 11 (bullseye)",
        "klipper_objects": ["temperature_sensor Chamber_Thermal_Protection_Sensor", "extruder"],
    }));
    assert_eq!(
        identify(&staging(), &plus5),
        recipe("qidi", modded(), "qidi_install")
    );
}

#[tokio::test]
async fn a_creality_ke_on_helper_script_is_refused_before_anything_changes() {
    let rig = k1_rig(K1 {
        is_k1: false,
        ..STOCK_K1
    });
    rig.installer_succeeds("k1");
    let mut f = fleet("k1c_helper_script");
    f.remove("helix_installed");
    f.remove("helix_version");
    f.insert("services".into(), json!(["klipper", "moonraker"]));
    let m = staging();
    let picked = identify(&m, &f);
    let mut r = rig.runner(&m, &picked, f);

    let trace = drive(&mut r, next_only).await;
    match &trace.end {
        Event::Refused { reason, .. } => assert!(reason.contains("K1C"), "{reason}"),
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(rig.installs().is_empty());
}

#[tokio::test]
async fn a_grid_picked_stock_creality_that_is_no_k1_is_refused_before_simple_af() {
    let rig = k1_rig(K1 {
        is_k1: false,
        ..STOCK_K1
    });
    let m = staging();
    let picked = identify_as(&m, "k1", &Facts::new());
    let mut r = rig.runner(&m, &picked, Facts::new());

    let trace = drive(&mut r, stock_k1_answers).await;
    assert!(
        matches!(trace.end, Event::Refused { .. }),
        "{:?}",
        trace.end
    );
    assert!(!ran(&rig, SIMPLE_AF));
}

#[test]
fn simple_af_reuses_its_own_checkout_and_refuses_anything_else() {
    let m = staging();
    let cmd = m.recipes["k1_stock"]
        .iter()
        .find_map(|s| match s {
            helix_installer_core::manifest::Step::SshRun { command, .. } => Some(command.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(cmd, SIMPLE_AF);
}

#[test]
fn an_ad5m_restores_by_the_mod_it_runs() {
    let m = staging();
    let zmod = facts(json!({ "cpu_arch": "armv7l", "klipper_objects": ["zmod", "mcu eboard"] }));
    for printer in ["ad5m", "ad5m_pro"] {
        assert_eq!(
            restore_for(&m, printer, &fleet("ad5m_forgex")).as_deref(),
            Some("ad5m_restore")
        );
        assert_eq!(
            restore_for(&m, printer, &zmod).as_deref(),
            Some("ad5m_zmod_restore")
        );
        assert_eq!(restore_for(&m, printer, &Facts::new()), None);
    }
}

#[tokio::test]
async fn a_zmod_ad5m_restore_switches_the_screen_back() {
    let rig = Rig::new();
    let m = staging();
    moonraker_answers(&rig, json!({ "objects": [] }));
    let picked = recipe(
        "ad5m",
        StartState::Helix(String::new()),
        "ad5m_zmod_restore",
    );
    let mut r = rig.runner(&m, &picked, Facts::new());
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(
        rig.moonraker.gcodes(),
        vec![(ADDR.to_string(), "DISPLAY_ON".to_string())]
    );
    assert!(rig.ssh.commands().is_empty());
}

// ---------------------------------------------------------------- the fleet

/// Each recorded printer identifies as exactly the printer it is.
#[test]
fn every_fleet_printer_identifies_as_itself_and_nothing_else() {
    let m = staging();
    let expect: &[(&str, Option<&str>)] = &[
        ("qidi_box", Some("qidi")),
        ("k2_plus", Some("k2")),
        ("k1c_helper_script", Some("k1")),
    ];
    for host in ["pi3_mainsailos", "cb1_voron"] {
        assert!(
            matches!(identify(&m, &fleet(host)), Identified::Recipe { ref printer_id, .. } if printer_id == "generic_klipper"),
            "{host}"
        );
    }
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
    // Moonraker cannot tell a 5M from a 5M Pro, so the user is asked; both get one recipe.
    assert_eq!(
        identify(&m, &fleet("ad5m_forgex")),
        Identified::Ambiguous {
            candidates: vec!["ad5m".into(), "ad5m_pro".into()]
        }
    );
}

#[test]
fn a_picked_generic_host_cannot_be_a_qidi() {
    assert!(matches!(
        identify_as(&staging(), "generic_klipper", &fleet("qidi_box")),
        Identified::Refuse { printer_id: Some(ref p), .. } if p == "generic_klipper"
    ));
}

/// Forge-X officially runs only on the AD5M and 5M Pro; this AD5X runs it with a port of
/// ZMOD's IFS support, and answers with Forge-X's macros on a MIPS board.
#[test]
fn an_ad5x_running_forgex_is_refused_and_never_offered_an_ad5m_recipe() {
    let m = staging();
    let f = fleet("ad5x_forgex");
    let refused = Identified::Refuse {
        printer_id: Some("ad5x_forgex".into()),
        doc: "https://helixscreen.org/guide/install-ad5x/".into(),
        reason: "this installer cannot put HelixScreen on a FlashForge Adventurer 5X with Forge-X"
            .into(),
    };
    assert_eq!(identify(&m, &f), refused);
    for picked in ["ad5m", "ad5m_pro", "ad5x", "ad5x_forgex"] {
        assert_eq!(identify_as(&m, picked, &f), refused, "{picked}");
    }
}

#[test]
fn the_forgex_ad5m_still_installs_as_an_ad5m() {
    let m = staging();
    let f = fleet("ad5m_forgex");
    assert_eq!(
        identify_as(&m, "ad5m", &f),
        recipe("ad5m", modded(), "ad5m_forgex_install")
    );
}

#[test]
fn a_zmod_ad5x_keeps_its_route() {
    let m = staging();
    let mut f = fleet("ad5x_forgex");
    f.insert("helix_installed".into(), json!(false));
    f.insert("os_distribution".into(), json!("Buildroot 2020.02.1"));
    f.insert(
        "klipper_objects".into(),
        zmod_ad5x_objects()["objects"].clone(),
    );
    assert_eq!(
        identify(&m, &f),
        recipe("ad5x", modded(), "ad5x_enable_helix")
    );
}

/// The AD5X's facts without `cpu_arch` (its system_info answer lost) and without the objects
/// named in `drop`.
fn ad5x_forgex_without(drop: &[&str]) -> Facts {
    let mut f = fleet("ad5x_forgex");
    f.remove("cpu_arch");
    let objects: Vec<serde_json::Value> = f["klipper_objects"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| !drop.contains(&o.as_str().unwrap()))
        .cloned()
        .collect();
    f.insert("klipper_objects".into(), json!(objects));
    f
}

const AD5X_IFS_OBJECTS: [&str; 3] = ["ifs", "ifs_materials", "ifs_toolhead_sensor toolhead"];

fn not_told_apart() -> Identified {
    Identified::Refuse {
        printer_id: Some("ad5x_forgex".into()),
        doc: "https://helixscreen.org/guide/install-ad5x/".into(),
        reason: "the printer did not report enough to tell it apart from a FlashForge Adventurer 5X with Forge-X, which this installer cannot put HelixScreen on".into(),
    }
}

#[test]
fn a_forgex_printer_that_does_not_report_its_cpu_is_refused_not_offered_an_ad5m_recipe() {
    let m = staging();
    let f = ad5x_forgex_without(&AD5X_IFS_OBJECTS);
    assert_eq!(identify(&m, &f), not_told_apart());
    for picked in ["ad5m", "ad5m_pro"] {
        assert_eq!(identify_as(&m, picked, &f), not_told_apart(), "{picked}");
    }
    // The same holds for a real AD5M whose cpu went unreported: it cannot be told apart either.
    let mut ad5m = fleet("ad5m_forgex");
    ad5m.remove("cpu_arch");
    assert_eq!(identify_as(&m, "ad5m", &ad5m), not_told_apart());
}

#[test]
fn forgex_with_the_ad5x_ifs_objects_is_refused_whatever_else_it_reports() {
    // The FlashForge reply names an AD5M, which rules out the cpu-and-model rule.
    let m = staging();
    let mut f = fleet("ad5x_forgex");
    f.remove("cpu_arch");
    f.insert("flashforge_udp.model".into(), json!("Adventurer5M"));
    assert!(
        matches!(identify_as(&m, "ad5m", &f), Identified::Refuse { printer_id: Some(ref p), ref reason, .. } if p == "ad5x_forgex" && reason.starts_with("this installer cannot")),
        "{:?}",
        identify_as(&m, "ad5m", &f)
    );
}

#[test]
fn a_zmod_ad5x_with_the_ifs_module_is_not_refused() {
    let m = staging();
    let mut f = fleet("ad5x_forgex");
    f.insert("helix_installed".into(), json!(false));
    f.insert("os_distribution".into(), json!("Buildroot 2020.02.1"));
    let mut objects = zmod_ad5x_objects()["objects"].as_array().unwrap().clone();
    objects.extend(AD5X_IFS_OBJECTS.iter().map(|o| json!(o)));
    f.insert("klipper_objects".into(), json!(objects));
    assert_eq!(
        identify(&m, &f),
        recipe("ad5x", modded(), "ad5x_enable_helix")
    );
    f.remove("cpu_arch");
    assert_eq!(
        identify(&m, &f),
        recipe("ad5x", modded(), "ad5x_enable_helix")
    );
}

#[test]
fn an_ad5m_whose_flashforge_reply_names_it_is_not_refused_without_its_cpu() {
    let m = staging();
    let mut f = fleet("ad5m_forgex");
    f.remove("cpu_arch");
    f.insert("flashforge_udp.model".into(), json!("Adventurer5M"));
    assert_eq!(
        identify_as(&m, "ad5m", &f),
        recipe("ad5m", modded(), "ad5m_forgex_install")
    );
}

fn not_recognised() -> Identified {
    Identified::Refuse {
        printer_id: None,
        doc: GENERIC_REFUSE_DOC.into(),
        reason: "printer not recognised".into(),
    }
}

#[test]
fn a_k1_whose_object_list_is_lost_is_not_called_an_ad5x() {
    let mut f = fleet("k1c_helper_script");
    f.remove("klipper_objects");
    assert!(
        matches!(identify(&staging(), &f), Identified::Refuse { printer_id: Some(ref p), .. } if p == "generic_klipper"),
        "{:?}",
        identify(&staging(), &f)
    );
}

#[test]
fn facts_that_name_nothing_a_refused_rule_needs_refuse_nothing() {
    let m = staging();
    assert_eq!(identify(&m, &Facts::new()), not_recognised());
    let mut f = Facts::new();
    f.insert("has_moonraker".into(), json!(true));
    assert_eq!(identify(&m, &f), not_recognised());
}

#[test]
fn a_forgex_ad5x_that_answers_flashforge_udp_is_still_refused() {
    let m = staging();
    let mut f = fleet("ad5x_forgex");
    f.insert("flashforge_udp.model".into(), json!("AD5X"));
    f.insert("model".into(), json!("AD5X"));
    assert!(matches!(
        identify(&m, &f),
        Identified::Refuse { printer_id: Some(ref p), .. } if p == "ad5x_forgex"
    ));
}

// ---------------------------------------------------------------- stock K1 texts and probes

#[cfg(unix)]
fn k1_stock_ota_probe() -> String {
    staging().recipes["k1_stock"]
        .iter()
        .find_map(|s| match s {
            Step::Probe {
                ssh: Some(cmd),
                fact: Some(fact),
                ..
            } if fact == "k1_firmware_version" => Some(cmd.clone()),
            _ => None,
        })
        .expect("k1_stock reads the firmware version")
}

#[cfg(unix)]
fn busybox() -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| std::path::Path::new(d).join("busybox"))
        .find(|p| p.is_file())
}

/// The K1's firmware version probe run against an `/etc/ota_info` holding `ota_info`, by the
/// host's `sh` and `sed`, or with `busybox` by BusyBox's, as the K1 runs it.
#[cfg(unix)]
fn firmware_version_from(ota_info: &str, busybox: Option<&std::path::Path>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ota_info");
    std::fs::write(&file, ota_info).unwrap();
    let probe = k1_stock_ota_probe();
    assert!(probe.contains("/etc/ota_info"), "{probe}");
    let probe = probe.replace("/etc/ota_info", &file.display().to_string());
    let out = match busybox {
        None => std::process::Command::new("sh")
            .args(["-c", &probe])
            .output()
            .unwrap(),
        Some(bb) => {
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            std::os::unix::fs::symlink(bb, bin.join("sed")).unwrap();
            let path = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            std::process::Command::new(bb)
                .args(["sh", "-c", &probe])
                .env("PATH", path)
                .output()
                .unwrap()
        }
    };
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(unix)]
#[test]
fn the_k1_firmware_version_is_read_with_or_without_the_6_prefix() {
    let bb = busybox();
    if bb.is_none() {
        // CI installs it on Linux, so a missing one there is a broken setup, not a skip.
        assert!(
            !(cfg!(target_os = "linux") && std::env::var_os("CI").is_some()),
            "busybox is not installed on the Linux CI runner"
        );
    }
    // A stock K1C on 1.3.3.46 writes it without the prefix; other units write `6.1.3.3.46`.
    for ota_info in [
        "ota_version=1.3.3.46\nota_board_name=K1C\n",
        "ota_version=6.1.3.3.46\nota_board_name=K1C\n",
    ] {
        assert_eq!(
            firmware_version_from(ota_info, None),
            "1.3.3.46\n",
            "{ota_info}"
        );
        if let Some(bb) = &bb {
            assert_eq!(
                firmware_version_from(ota_info, Some(bb)),
                "1.3.3.46\n",
                "busybox: {ota_info}"
            );
        }
    }
}

#[test]
fn signing_lints_the_published_list_for_signing() {
    let wf = std::fs::read_to_string(repo().join(".github/workflows/manifest.yml")).unwrap();
    assert!(
        wf.contains("manifest-lint manifest/manifest.json manifest/img --signing"),
        "the publish job must refuse a recipe awaiting its hardware run"
    );
}

#[test]
fn the_k1_firmware_floor_compares_numerically() {
    let floor = staging().recipes["k1_stock"]
        .iter()
        .find_map(|s| match s {
            Step::Require {
                fact,
                range: Some(r),
                ..
            } if fact == "k1_firmware_version" => Some(r.clone()),
            _ => None,
        })
        .expect("k1_stock has a firmware floor");
    for (v, ok) in [
        ("1.3.3.46", true),
        ("1.3.3.5", true),
        ("1.3.3.4", false),
        ("1.3.10.1", true),
    ] {
        assert_eq!(
            floor.matches(&FwVersion::parse(v).unwrap()),
            ok,
            "{v} against {floor:?}"
        );
    }
}

/// The button under every `user_action` reads "I've done this" (ui/src/routes/Run.svelte).
#[test]
fn user_action_texts_name_the_button_the_screen_shows() {
    for (id, steps) in &staging().recipes {
        for s in steps {
            if let Step::UserAction { text, .. } = s {
                assert!(!text.contains("ress Next"), "{id}: {text}");
            }
        }
    }
}

#[test]
fn the_stock_k1_is_told_to_cancel_creality_s_update_offer_before_settings() {
    let steps = &staging().recipes["k1_stock"];
    let first_settings = steps
        .iter()
        .position(|s| match s {
            Step::Ask { question: t, .. } | Step::UserAction { text: t, .. } => {
                t.contains("Settings")
            }
            _ => false,
        })
        .expect("k1_stock sends the user into Settings");
    let cancel = steps[..first_settings].iter().find_map(|s| match s {
        Step::Info { text, .. } if text.contains("tap Cancel") => Some(text),
        _ => None,
    });
    let cancel = cancel.expect("the update offer is dealt with before Settings");
    assert!(!cancel.contains('\u{2014}'), "{cancel}");
}

// ---------------------------------------------------------------- the published manifest

fn published() -> Manifest {
    let bytes = std::fs::read(repo().join("manifest/manifest.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn refused_with(id: &Identified, printer: &str, doc: &str) {
    match id {
        Identified::Refuse {
            printer_id, doc: d, ..
        } => {
            assert_eq!(printer_id.as_deref(), Some(printer), "{id:?}");
            assert_eq!(d, doc, "{id:?}");
        }
        other => panic!("expected {printer} refused with {doc}, got {other:?}"),
    }
}

#[test]
fn the_published_list_passes_the_strict_lint() {
    let errs = helix_installer_core::lint::lint(&published(), &repo().join("manifest/img"));
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn every_published_printer_that_installs_carries_its_hardware_runs() {
    let m = published();
    for p in m.printers.iter().filter(|p| !p.refused) {
        let v = p.verified.as_ref().unwrap_or_else(|| panic!("{}", p.id));
        assert!(!v.runs.is_empty(), "{}", p.id);
        assert!(!p.recipes.is_empty(), "{}", p.id);
    }
}

#[test]
fn every_fleet_printer_gets_its_verified_route_or_its_refusal_from_the_published_list() {
    let m = published();
    let helix = |v: &str| StartState::Helix(v.into());
    for (host, want) in [
        (
            "k1c_helper_script",
            recipe("k1", helix("v1.1.0-beta.9"), "k1_update"),
        ),
        (
            "pi3_mainsailos",
            recipe("generic_klipper", helix("v1.0.0"), "generic_update"),
        ),
        (
            "cb1_voron",
            recipe("generic_klipper", helix("v1.0.0"), "generic_update_board"),
        ),
        // Moonraker does not always see HelixScreen on the K2, the Forge-X AD5M or a QIDI: the
        // update recipe finds it over SSH, or refuses.
        ("k2_plus", recipe("k2", modded(), "k2_update")),
        (
            "ad5m_forgex",
            recipe("ad5m", modded(), "ad5m_forgex_update"),
        ),
        ("qidi_box", recipe("qidi", modded(), "qidi_update")),
    ] {
        assert_eq!(identify(&m, &fleet(host)), want, "{host}");
    }
    refused_with(
        &identify(&m, &fleet("ad5x_forgex")),
        "ad5x_forgex",
        "https://helixscreen.org/guide/install-ad5x/",
    );
}

#[test]
fn a_stock_or_modded_k1_is_refused_with_the_k1_guide_on_the_published_list() {
    let m = published();
    let doc = "https://helixscreen.org/guide/install-k1/";
    refused_with(&identify_as(&m, "k1", &Facts::new()), "k1", doc);
    let mut f = fleet("k1c_helper_script");
    f.remove("helix_installed");
    f.remove("helix_version");
    refused_with(&identify(&m, &f), "k1", doc);
}

#[test]
fn first_installs_and_zmod_are_refused_with_their_guides_on_the_published_list() {
    let m = published();
    refused_with(
        &identify_as(&m, "ad5m", &Facts::new()),
        "ad5m",
        "https://helixscreen.org/guide/install-ad5m/",
    );
    let mut f = fleet("pi3_mainsailos");
    f.remove("helix_installed");
    f.remove("helix_version");
    refused_with(
        &identify(&m, &f),
        "generic_klipper",
        "https://helixscreen.org/guide/install-generic/",
    );
    // ZMOD declares [zmod_ifs] only in its AD5X configs (ad5x_generic.cfg, ad5x_display_off.cfg),
    // so a ZMOD AD5M lists zmod and no zmod_ifs.
    let zmod_ad5m = Facts::from([
        ("has_moonraker".to_string(), json!(true)),
        ("cpu_arch".to_string(), json!("armv7l")),
        ("klipper_objects".to_string(), json!(["zmod", "mcu"])),
    ]);
    refused_with(
        &identify(&m, &zmod_ad5m),
        "ad5m",
        "https://helixscreen.org/guide/install-ad5m/",
    );
    let zmod_ad5x = Facts::from([("flashforge_udp.model".to_string(), json!("AD5X"))]);
    refused_with(
        &identify(&m, &zmod_ad5x),
        "ad5x",
        "https://helixscreen.org/guide/install-ad5x/",
    );
}

#[tokio::test]
async fn the_published_k1c_update_runs_as_it_did_on_the_printer() {
    let rig = k1c_update_rig("1.1.0-beta.10");
    let m = published();
    let picked = identify(&m, &fleet("k1c_helper_script"));
    let mut r = rig.runner(&m, &picked, fleet("k1c_helper_script"));
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(rig.installs()[0].0.ends_with("--progress --update"));
}

#[tokio::test]
async fn a_published_k2_update_finds_helixscreen_over_ssh_or_refuses_before_any_change() {
    let rig = k2_rig("creality_2024", true);
    rig.ssh
        .on_command(INSTALLED_VERSION_PROBE, out("v1.0.4\n", 0));
    let m = published();
    let picked = identify(&m, &fleet("k2_plus"));
    let mut r = rig.runner(&m, &picked, fleet("k2_plus"));
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(rig.installs()[0].0.ends_with("--progress --update"));
    match &trace.prompts[..] {
        [Prompt::UserAction { image, .. }] => {
            assert_eq!(*image, None, "the root-account picture is not drawn yet")
        }
        other => panic!("{other:?}"),
    }

    let rig = k2_rig("creality_2024", false);
    let mut r = rig.runner(&m, &picked, fleet("k2_plus"));
    let trace = drive(&mut r, next_only).await;
    match &trace.end {
        Event::Refused { reason, doc, .. } => {
            assert!(reason.contains("not installed"), "{reason}");
            assert_eq!(doc, "https://helixscreen.org/guide/install-k2/");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(rig.installer_runs().is_empty());
    assert!(rig.ssh.uploads().is_empty());
    assert!(!r.state().committed);
}

#[tokio::test]
async fn a_published_forgex_update_finds_helixscreen_over_ssh_or_refuses_before_any_change() {
    let m = published();
    let picked = identify(&m, &fleet("ad5m_forgex"));

    let rig = forgex_rig();
    rig.ssh.on_command(INIT_SCRIPTS_PROBE, out("yes\n", 0));
    rig.ssh
        .on_command(INSTALLED_VERSION_PROBE, out("v1.0.4\n", 0));
    let mut r = rig.runner(&m, &picked, fleet("ad5m_forgex"));
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert_eq!(r.state().recipe_id, "ad5m_forgex_update");

    let rig = forgex_rig();
    let mut r = rig.runner(&m, &picked, fleet("ad5m_forgex"));
    let trace = drive(&mut r, next_only).await;
    match &trace.end {
        Event::Refused { reason, doc, .. } => {
            assert!(reason.contains("not installed"), "{reason}");
            assert_eq!(doc, "https://helixscreen.org/guide/install-ad5m/");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(rig.installer_runs().is_empty());
    assert!(!r.state().committed);
}

const QIDI_HELIX_PROBE: &str =
    "test -e /etc/systemd/system/helixscreen.service && echo yes || echo no";

/// The fleet Q2 on mks/makerbase (sudo with its password), HelixScreen there or not.
fn qidi_rig(helix_there: bool) -> Rig {
    let rig = Rig::new();
    rig.ssh.accept(ADDR, "mks", "makerbase");
    rig.ssh.set_fallback(move |cmd| match cmd {
        "pidof helix-screen" => out("1234\n", 0),
        "id -u" => out("1000\n", 0),
        "sudo -n true" => out("", 1),
        QIDI_HELIX_PROBE => out(if helix_there { "yes\n" } else { "no\n" }, 0),
        _ => out("", 127),
    });
    rig.installer_succeeds("pi");
    rig
}

#[tokio::test]
async fn a_published_qidi_update_finds_helixscreen_over_ssh_or_refuses_before_any_change() {
    let m = published();
    let picked = identify(&m, &fleet("qidi_box"));

    let rig = qidi_rig(true);
    rig.ssh
        .on_command(INSTALLED_VERSION_PROBE, out("v1.1.0-beta.9\n", 0));
    let mut r = rig.runner(&m, &picked, fleet("qidi_box"));
    let trace = drive(&mut r, next_only).await;
    assert!(trace.done(), "{:?}", trace.end);
    assert!(rig.installs()[0].0.ends_with("--progress --update"));

    let rig = qidi_rig(false);
    let mut r = rig.runner(&m, &picked, fleet("qidi_box"));
    let trace = drive(&mut r, next_only).await;
    match &trace.end {
        Event::Refused { reason, doc, .. } => {
            assert!(reason.contains("not installed"), "{reason}");
            assert_eq!(doc, "https://helixscreen.org/guide/install-qidi/");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(rig.installer_runs().is_empty());
    assert!(!r.state().committed);
}

#[tokio::test]
async fn a_stable_install_is_refused_before_any_change_until_helixscreen_1_1() {
    let rig = Rig::new();
    rig.publish(Channel::Stable, "1.0.5");
    rig.ssh.accept(ADDR, "pi", "raspberry");
    rig.ssh.set_fallback(|cmd| match cmd {
        "pidof helix-screen" => out("1234\n", 0),
        "id -u" => out("1000\n", 0),
        "sudo -n true" => out("", 1),
        _ => out("", 127),
    });
    rig.ssh
        .on_command(INSTALLED_VERSION_PROBE, out("v1.0.0\n", 0));
    rig.installer_succeeds("pi");
    let m = published();
    let picked = identify(&m, &fleet("pi3_mainsailos"));
    let mut r = rig.runner(&m, &picked, fleet("pi3_mainsailos"));
    let trace = drive(&mut r, |p, _| match p {
        Prompt::Login { .. } => login("pi", "raspberry"),
        other => panic!("unexpected prompt {other:?}"),
    })
    .await;
    match &trace.end {
        Event::Refused { reason, doc, .. } => {
            assert!(reason.contains("HelixScreen 1.1 and newer"), "{reason}");
            assert!(reason.contains("the usual way"), "{reason}");
            assert!(reason.contains("1.0.5"), "{reason}");
            assert_eq!(doc, "https://helixscreen.org/upgrading/");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(
        rig.installer_runs().is_empty(),
        "{:?}",
        rig.installer_runs()
    );
    assert!(rig.ssh.uploads().is_empty());
    assert!(!r.state().committed);
    assert_eq!(r.state().recipe_id, "generic_update");
}
