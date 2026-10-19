// SPDX-License-Identifier: GPL-3.0-or-later

//! SSH key logins over the real russh transport: the OS agent, then the default key files
//! (a passphrase-protected one unlocked with the passphrase the user typed), then the password.

mod ssh_server;

use helix_installer_core::manifest::{Login, Secret};
use helix_installer_core::transport::ssh_russh::{rsa_signature_hash, KeySources, SshRussh};
use helix_installer_core::transport::*;
use russh::keys::{HashAlg, PublicKey};
use ssh_server::{ServerCfg, TestServer};
use std::path::{Path, PathBuf};

const PASSPHRASE: &str = "correct horse battery";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ssh_keys")
        .join(name)
}

fn public(name: &str) -> PublicKey {
    let text = std::fs::read_to_string(fixture(&format!("{name}.pub"))).unwrap();
    PublicKey::from_openssh(text.trim()).unwrap()
}

fn host(port: u16) -> Host {
    Host {
        addr: "127.0.0.1".into(),
        moonraker_port: 7125,
        ssh_port: port,
    }
}

/// A `~/.ssh` holding the fixture `keys`, each under the default name it is given.
struct KeyDir {
    dir: tempfile::TempDir,
}

impl KeyDir {
    fn new(keys: &[(&str, &str)]) -> KeyDir {
        let dir = tempfile::tempdir().unwrap();
        for (fixture_name, default_name) in keys {
            std::fs::copy(fixture(fixture_name), dir.path().join(default_name)).unwrap();
        }
        KeyDir { dir }
    }

    fn sources(&self) -> KeySources {
        KeySources {
            agent: None,
            key_files: KeySources::default_files(self.dir.path()),
        }
    }
}

struct Client {
    ssh: SshRussh,
    _dir: tempfile::TempDir,
}

fn client(keys: KeySources) -> Client {
    let dir = tempfile::tempdir().unwrap();
    Client {
        ssh: SshRussh::new(dir.path().join("known_hosts.json")).with_keys(keys),
        _dir: dir,
    }
}

fn key_login(user: &str) -> Login {
    Login::new(user, "")
}

fn pi_server(keys: Vec<PublicKey>) -> ServerCfg {
    ServerCfg {
        user: "pi".into(),
        password: "raspberry".into(),
        authorized_keys: keys,
        ..ServerCfg::default()
    }
}

#[tokio::test]
async fn an_unencrypted_default_key_file_logs_in_without_a_password() {
    let srv = TestServer::start(pi_server(vec![public("ed25519")])).await;
    let keys = KeyDir::new(&[("ed25519", "id_ed25519")]);
    let c = client(keys.sources());
    let mut s = c
        .ssh
        .connect(&host(srv.port), &key_login("pi"))
        .await
        .unwrap();
    assert_eq!(
        s.exec("rm -f /x", None, std::time::Duration::from_secs(5))
            .await
            .unwrap()
            .exit,
        0
    );
    assert_eq!(srv.records().auth_methods, vec!["publickey".to_string()]);
}

#[tokio::test]
async fn each_default_key_type_logs_in() {
    for (name, default_name) in [("ecdsa", "id_ecdsa"), ("rsa", "id_rsa")] {
        let srv = TestServer::start(pi_server(vec![public(name)])).await;
        let keys = KeyDir::new(&[(name, default_name)]);
        let c = client(keys.sources());
        c.ssh
            .connect(&host(srv.port), &key_login("pi"))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            srv.records().auth_methods,
            vec!["publickey".to_string()],
            "{name}"
        );
    }
}

#[tokio::test]
async fn an_rsa_key_logs_in_to_a_server_that_names_no_signature_algorithms() {
    // legacy_only drops the ext-info kex marker, so the server sends no server-sig-algs, as
    // dropbear 2019.78 on a K1 does. The test server takes either RSA signature, so the SHA-1
    // choice itself is pinned by `rsa_signs_with_sha1_unless_the_server_names_a_sha2_algorithm`.
    let srv = TestServer::start(ServerCfg {
        legacy_only: true,
        ..pi_server(vec![public("rsa")])
    })
    .await;
    let keys = KeyDir::new(&[("rsa", "id_rsa")]);
    let c = client(keys.sources());
    c.ssh
        .connect(&host(srv.port), &key_login("pi"))
        .await
        .unwrap();
    assert_eq!(srv.records().auth_methods, vec!["publickey".to_string()]);
}

#[test]
fn rsa_signs_with_sha1_unless_the_server_names_a_sha2_algorithm() {
    assert_eq!(
        rsa_signature_hash(None),
        None,
        "no server-sig-algs: ssh-rsa"
    );
    assert_eq!(rsa_signature_hash(Some(None)), None, "only ssh-rsa listed");
    assert_eq!(
        rsa_signature_hash(Some(Some(HashAlg::Sha256))),
        Some(HashAlg::Sha256)
    );
    assert_eq!(
        rsa_signature_hash(Some(Some(HashAlg::Sha512))),
        Some(HashAlg::Sha512)
    );
}

#[tokio::test]
async fn a_passphrase_protected_key_needs_the_passphrase() {
    let srv = TestServer::start(pi_server(vec![public("ed25519_encrypted")])).await;
    let keys = KeyDir::new(&[("ed25519_encrypted", "id_ed25519")]);
    let c = client(keys.sources());
    let h = host(srv.port);

    let locked = c.ssh.connect(&h, &key_login("pi")).await.err();
    assert_eq!(locked, Some(TransportError::Auth), "no passphrase given");

    let mut wrong = key_login("pi");
    wrong.passphrase = Some(Secret::new("not it"));
    assert_eq!(
        c.ssh.connect(&h, &wrong).await.err(),
        Some(TransportError::Auth)
    );

    let mut right = key_login("pi");
    right.passphrase = Some(Secret::new(PASSPHRASE));
    c.ssh.connect(&h, &right).await.unwrap();
    assert_eq!(srv.records().auth_methods, vec!["publickey".to_string()]);
}

#[tokio::test]
async fn the_key_is_tried_before_the_password_and_the_password_still_works() {
    // The printer does not know the key: the login falls through to its password.
    let srv = TestServer::start(pi_server(vec![])).await;
    srv.set_cfg(|c| c.offer_publickey = true);
    let keys = KeyDir::new(&[("ed25519", "id_ed25519")]);
    let c = client(keys.sources());
    c.ssh
        .connect(&host(srv.port), &Login::new("pi", "raspberry"))
        .await
        .unwrap();
    let r = srv.records();
    let first_key = r.auth_attempts.iter().position(|m| m == "publickey");
    let first_password = r.auth_attempts.iter().position(|m| m == "password");
    assert!(
        matches!((first_key, first_password), (Some(k), Some(p)) if k < p),
        "attempts: {:?}",
        r.auth_attempts
    );
    assert_eq!(r.auth_methods, vec!["password".to_string()]);
}

#[tokio::test]
async fn a_key_the_printer_accepts_wins_over_the_password() {
    let srv = TestServer::start(pi_server(vec![public("ed25519")])).await;
    let keys = KeyDir::new(&[("ed25519", "id_ed25519")]);
    let c = client(keys.sources());
    c.ssh
        .connect(&host(srv.port), &Login::new("pi", "raspberry"))
        .await
        .unwrap();
    let r = srv.records();
    assert_eq!(r.auth_methods, vec!["publickey".to_string()]);
    assert!(!r.auth_attempts.contains(&"password".to_string()));
}

#[tokio::test]
async fn a_server_that_hangs_up_after_refused_keys_still_takes_the_password() {
    // Three unknown keys against a server that allows two refusals: it disconnects during
    // the key attempts, and the password goes over a fresh connection.
    let srv = TestServer::start(ServerCfg {
        offer_publickey: true,
        max_auth_attempts: Some(2),
        ..pi_server(vec![])
    })
    .await;
    let keys = KeyDir::new(&[
        ("ed25519", "id_ed25519"),
        ("ecdsa", "id_ecdsa"),
        ("rsa", "id_rsa"),
    ]);
    let c = client(keys.sources());
    c.ssh
        .connect(&host(srv.port), &Login::new("pi", "raspberry"))
        .await
        .unwrap();
    assert_eq!(srv.records().auth_methods, vec!["password".to_string()]);
}

#[tokio::test]
async fn a_key_login_with_no_working_key_is_refused() {
    let srv = TestServer::start(pi_server(vec![public("ecdsa")])).await;
    let keys = KeyDir::new(&[("ed25519", "id_ed25519")]);
    let c = client(keys.sources());
    let got = c.ssh.connect(&host(srv.port), &key_login("pi")).await.err();
    assert_eq!(got, Some(TransportError::Auth));
    assert!(!srv
        .records()
        .auth_attempts
        .contains(&"password".to_string()));
}

#[tokio::test]
async fn a_server_without_publickey_is_never_offered_a_key() {
    let srv = TestServer::start(pi_server(vec![])).await;
    let keys = KeyDir::new(&[("ed25519", "id_ed25519")]);
    let c = client(keys.sources());
    c.ssh
        .connect(&host(srv.port), &Login::new("pi", "raspberry"))
        .await
        .unwrap();
    assert!(!srv
        .records()
        .auth_attempts
        .contains(&"publickey".to_string()));
}

#[test]
fn key_status_reports_keys_and_whether_one_is_locked() {
    let none = client(KeySources {
        agent: None,
        key_files: KeySources::default_files(tempfile::tempdir().unwrap().path()),
    });
    assert_eq!(none.ssh.key_status(), KeyStatus::default());

    let plain = KeyDir::new(&[("ed25519", "id_ed25519")]);
    let c = client(plain.sources());
    assert_eq!(
        c.ssh.key_status(),
        KeyStatus {
            available: true,
            locked: false
        }
    );

    let locked = KeyDir::new(&[("ed25519_encrypted", "id_ed25519"), ("rsa", "id_rsa")]);
    let c = client(locked.sources());
    assert_eq!(
        c.ssh.key_status(),
        KeyStatus {
            available: true,
            locked: true
        }
    );
}

#[test]
fn default_files_are_ed25519_then_ecdsa_then_rsa() {
    let d = Path::new("/home/u/.ssh");
    assert_eq!(
        KeySources::default_files(d),
        vec![d.join("id_ed25519"), d.join("id_ecdsa"), d.join("id_rsa")]
    );
}

#[cfg(unix)]
mod agent {
    use super::*;
    use futures_util::stream;
    use russh::keys::agent::client::AgentClient;
    use russh::keys::PrivateKey;

    /// An ssh-agent on a Unix socket in `dir`, holding the fixture key `name`.
    async fn start_agent(dir: &Path, name: &str) -> PathBuf {
        let sock = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let incoming = Box::pin(stream::unfold(listener, |l| async move {
            let next = l.accept().await.map(|(s, _)| s);
            Some((next, l))
        }));
        tokio::spawn(russh::keys::agent::server::serve(incoming, ()));
        let key = PrivateKey::read_openssh_file(fixture(name)).unwrap();
        let mut c = AgentClient::connect_uds(&sock).await.unwrap();
        c.add_identity(&key, &[]).await.unwrap();
        sock
    }

    #[tokio::test]
    async fn the_agent_logs_in_with_a_key_it_holds() {
        let srv = TestServer::start(pi_server(vec![public("ed25519")])).await;
        let dir = tempfile::tempdir().unwrap();
        let sock = start_agent(dir.path(), "ed25519").await;
        let c = client(KeySources {
            agent: Some(sock),
            key_files: Vec::new(),
        });
        c.ssh
            .connect(&host(srv.port), &key_login("pi"))
            .await
            .unwrap();
        assert_eq!(srv.records().auth_methods, vec!["publickey".to_string()]);
    }

    /// OpenSSH's own `ssh-agent` on a socket in `dir`, holding the fixture key `name`; `None`
    /// when this machine has no OpenSSH client tools. The child is killed on drop.
    struct OpenSshAgent {
        sock: PathBuf,
        child: std::process::Child,
    }

    impl Drop for OpenSshAgent {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn openssh_agent(dir: &Path, name: &str) -> Option<OpenSshAgent> {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};
        let sock = dir.join("openssh-agent.sock");
        let child = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let agent = OpenSshAgent { sock, child };
        for _ in 0..100 {
            if agent.sock.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // ssh-add refuses a key file others can read.
        let key = dir.join(name);
        std::fs::copy(fixture(name), &key).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let added = Command::new("ssh-add")
            .arg(&key)
            .env("SSH_AUTH_SOCK", &agent.sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        added.success().then_some(agent)
    }

    #[tokio::test]
    async fn an_rsa_key_in_the_agent_logs_in_with_and_without_server_signature_algorithms() {
        for legacy_only in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let Some(agent) = openssh_agent(dir.path(), "rsa") else {
                eprintln!("skipped: no OpenSSH ssh-agent/ssh-add on this machine");
                return;
            };
            let srv = TestServer::start(ServerCfg {
                legacy_only,
                ..pi_server(vec![public("rsa")])
            })
            .await;
            let c = client(KeySources {
                agent: Some(agent.sock.clone()),
                key_files: Vec::new(),
            });
            c.ssh
                .connect(&host(srv.port), &key_login("pi"))
                .await
                .unwrap_or_else(|e| panic!("legacy_only {legacy_only}: {e}"));
            assert_eq!(srv.records().auth_methods, vec!["publickey".to_string()]);
        }
    }

    #[tokio::test]
    async fn the_agent_comes_before_the_key_files() {
        // The agent holds the key the printer accepts; the key file is one it does not, so a
        // login that tried the file first would still succeed, but only after a refusal.
        let srv = TestServer::start(pi_server(vec![public("ed25519")])).await;
        let dir = tempfile::tempdir().unwrap();
        let sock = start_agent(dir.path(), "ed25519").await;
        let keys = KeyDir::new(&[("ecdsa", "id_ecdsa")]);
        let c = client(KeySources {
            agent: Some(sock),
            key_files: KeySources::default_files(keys.dir.path()),
        });
        c.ssh
            .connect(&host(srv.port), &key_login("pi"))
            .await
            .unwrap();
        let r = srv.records();
        assert_eq!(r.auth_methods, vec!["publickey".to_string()]);
        // One probe and its signed request, both for the agent's key: no refusal first.
        assert!(r.auth_attempts.iter().all(|m| m == "publickey"));
        assert!(
            r.auth_attempts.len() <= 2,
            "attempts: {:?}",
            r.auth_attempts
        );
    }

    #[tokio::test]
    async fn an_agent_that_is_not_running_is_skipped() {
        let srv = TestServer::start(pi_server(vec![public("ed25519")])).await;
        let keys = KeyDir::new(&[("ed25519", "id_ed25519")]);
        let c = client(KeySources {
            agent: Some(keys.dir.path().join("no-agent.sock")),
            key_files: KeySources::default_files(keys.dir.path()),
        });
        c.ssh
            .connect(&host(srv.port), &key_login("pi"))
            .await
            .unwrap();
    }
}
