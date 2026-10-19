// SPDX-License-Identifier: GPL-3.0-or-later

mod ssh_server;

use helix_installer_core::manifest::Login;
use helix_installer_core::transport::ssh_russh::{sh_quote, SshRussh};
use helix_installer_core::transport::*;
use ssh_server::{Job, JobStep, ServerCfg, TestServer};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

const T: Duration = Duration::from_secs(10);

fn login(user: &str, pw: &str) -> Login {
    Login::new(user, pw)
}

fn host(port: u16) -> Host {
    Host {
        addr: "127.0.0.1".into(),
        moonraker_port: 7125,
        ssh_port: port,
    }
}

struct Client {
    ssh: SshRussh,
    _dir: tempfile::TempDir,
}

impl Client {
    fn with_start_timeout(d: Duration) -> Client {
        let dir = tempfile::tempdir().unwrap();
        Client {
            ssh: SshRussh::new(dir.path().join("known_hosts.json")).with_start_timeout(d),
            _dir: dir,
        }
    }
}

/// The first line of every job log in the test server (its first job's pid is 4242).
const STARTED: &str = "HELIX-STARTED 4242\n";

fn client() -> Client {
    let dir = tempfile::tempdir().unwrap();
    Client {
        ssh: SshRussh::new(dir.path().join("known_hosts.json")),
        _dir: dir,
    }
}

async fn connect(c: &Client, srv: &TestServer) -> Box<dyn SshSession> {
    let cfg = ServerCfg::default();
    c.ssh
        .connect(&host(srv.port), &login(&cfg.user, &cfg.password))
        .await
        .unwrap()
}

/// Polls the job from `offset` until it reports an exit, returning all text and the code.
async fn read_to_exit(
    s: &mut Box<dyn SshSession>,
    job: &RemoteJob,
    mut offset: u64,
) -> (String, i32, u64) {
    let mut text = String::new();
    for _ in 0..200 {
        let chunk = s.read_job(job, offset).await.unwrap();
        assert!(chunk.next_offset >= offset, "offset went backwards");
        offset = chunk.next_offset;
        text.push_str(&chunk.text);
        if let Some(code) = chunk.exit {
            return (text, code, offset);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("job never exited; text so far: {text:?}");
}

#[test]
fn sh_quote_wraps_in_single_quotes_and_escapes_them() {
    assert_eq!(sh_quote("plain"), "'plain'");
    assert_eq!(sh_quote(""), "''");
    assert_eq!(sh_quote("a b"), "'a b'");
    assert_eq!(sh_quote("it's"), "'it'\\''s'");
    assert_eq!(sh_quote("$HOME `x` \"q\""), "'$HOME `x` \"q\"'");
}

#[cfg(unix)]
#[test]
fn sh_quote_survives_a_real_shell_unchanged() {
    for s in [
        "with space",
        "it's",
        "$HOME and $(id) and `id`",
        "back\\slash",
        "'''",
        "new\nline",
        "*?[a]",
    ] {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", sh_quote(s)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), s);
    }
}

#[tokio::test]
async fn exec_returns_stdout_stderr_and_exit_and_feeds_stdin() {
    let srv = TestServer::start(ServerCfg::default()).await;
    srv.canned("uname -m", "mips\n", "warn\n", 3);
    let c = client();
    let mut s = connect(&c, &srv).await;
    let out = s.exec("uname -m", None, T).await.unwrap();
    assert_eq!(
        out,
        ExecOutput {
            stdout: "mips\n".into(),
            stderr: "warn\n".into(),
            exit: 3
        }
    );
    // A missing command is an exit status, not a transport error.
    let out = s.exec("frobnicate", None, T).await.unwrap();
    assert_eq!(out.exit, 127);
}

#[tokio::test]
async fn upload_of_5mb_round_trips_byte_exact_through_cat() {
    let srv = TestServer::start(ServerCfg::default()).await;
    std::fs::create_dir_all(srv.path("/usr/data/helix dir")).unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let local = local_dir.path().join("helixscreen.tar.gz");
    let mut x: u32 = 0x1234_5678;
    let data: Vec<u8> = (0..5 * 1024 * 1024 + 7)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    std::fs::write(&local, &data).unwrap();

    let c = client();
    let mut s = connect(&c, &srv).await;
    let remote = "/usr/data/helix dir/it's.tar.gz";
    s.upload(&local, remote).await.unwrap();

    let got = std::fs::read(srv.path(remote)).unwrap();
    assert_eq!(got.len(), data.len());
    assert!(got == data, "uploaded bytes differ");
    assert!(!srv.path(&format!("{remote}.part")).exists());
    let cmds = srv.records().commands.clone();
    assert!(
        cmds.iter().any(|c| c.starts_with("cat > ")),
        "upload must use cat: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("wc -c < ")),
        "upload must check the size: {cmds:?}"
    );
}

#[tokio::test]
async fn upload_reports_a_short_remote_file() {
    let srv = TestServer::start(ServerCfg {
        truncate_uploads: true,
        ..Default::default()
    })
    .await;
    let local_dir = tempfile::tempdir().unwrap();
    let local = local_dir.path().join("f.bin");
    std::fs::write(&local, b"0123456789").unwrap();
    let c = client();
    let mut s = connect(&c, &srv).await;
    let err = s.upload(&local, "/tmp/f.bin").await.unwrap_err();
    assert!(matches!(err, TransportError::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn upload_into_a_missing_directory_fails_promptly_while_still_sending() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let local_dir = tempfile::tempdir().unwrap();
    let local = local_dir.path().join("f.bin");
    // Larger than the SSH window, so a sender that never reads would wait forever.
    std::fs::write(&local, vec![7u8; 8 * 1024 * 1024]).unwrap();
    let c = client();
    let mut s = connect(&c, &srv).await;
    let res = tokio::time::timeout(Duration::from_secs(5), s.upload(&local, "/nowhere/f.bin"))
        .await
        .expect("upload must not stall when cat fails early");
    assert!(matches!(res, Err(TransportError::Protocol(_))), "{res:?}");
}

#[tokio::test]
async fn a_failed_upload_removes_its_part_file() {
    let srv = TestServer::start(ServerCfg {
        fail_mv: true,
        ..Default::default()
    })
    .await;
    let local_dir = tempfile::tempdir().unwrap();
    let local = local_dir.path().join("f.bin");
    std::fs::write(&local, b"abc").unwrap();
    let c = client();
    let mut s = connect(&c, &srv).await;
    assert!(s.upload(&local, "/tmp/f.bin").await.is_err());
    assert!(!srv.path("/tmp/f.bin.part").exists());
    assert!(!srv.path("/tmp/f.bin").exists());
}

#[tokio::test]
async fn a_job_survives_the_connection_dropping_and_a_new_session_reads_its_exit() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let gate = Arc::new(Notify::new());
    srv.job(
        "sh /tmp/install.sh --update",
        Job {
            steps: vec![
                JobStep::Write("unpacking\n".into()),
                JobStep::Wait(gate.clone()),
                JobStep::Write("restarting klipper\n".into()),
            ],
            exit: 0,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s
        .spawn_detached("sh /tmp/install.sh --update", None)
        .await
        .unwrap();
    assert!(
        job.log_path.starts_with("/tmp/helix-installer-") && job.log_path.ends_with(".log"),
        "{}",
        job.log_path
    );
    assert_eq!(
        srv.records().detach_tools,
        vec![("setsid".to_string(), false)]
    );
    assert!(job.pid.is_some(), "the started line carries the pid");

    // Wait for the first line, so the drop happens with the job mid-flight.
    let mut offset = 0;
    let mut first = String::new();
    while !first.contains("unpacking") {
        let chunk = s.read_job(&job, offset).await.unwrap();
        assert_eq!(chunk.exit, None);
        offset = chunk.next_offset;
        first.push_str(&chunk.text);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    srv.drop_connections().await;
    let stale = tokio::time::timeout(T, s.read_job(&job, offset)).await;
    assert!(
        matches!(stale, Ok(Err(_))),
        "a dead session must fail, not hang"
    );
    drop(s);
    gate.notify_one();

    let mut s2 = connect(&c, &srv).await;
    let (text, code, end) = read_to_exit(&mut s2, &job, 0).await;
    assert_eq!(code, 0);
    assert_eq!(text, "unpacking\nrestarting klipper\n");
    let log_len = std::fs::metadata(srv.path(&job.log_path)).unwrap().len();
    assert_eq!(end, log_len, "next_offset covers the whole log");
}

#[tokio::test]
async fn read_job_resumes_from_an_offset_and_reports_nonzero_exit() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let gate = Arc::new(Notify::new());
    srv.job(
        "false",
        Job {
            steps: vec![
                JobStep::Write("one\n".into()),
                JobStep::Wait(gate.clone()),
                JobStep::Write("two\n".into()),
            ],
            exit: 3,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s.spawn_detached("false", None).await.unwrap();
    let mut offset = 0;
    while offset == 0 {
        let chunk = s.read_job(&job, 0).await.unwrap();
        if !chunk.text.is_empty() {
            // The newline may be the one printed before the exit marker, so it waits.
            assert_eq!(chunk.text, "one");
            assert_eq!(chunk.next_offset, STARTED.len() as u64 + 3);
            assert_eq!(chunk.alive, Some(true));
            offset = chunk.next_offset;
        }
    }
    gate.notify_one();
    let (text, code, _) = read_to_exit(&mut s, &job, offset).await;
    assert_eq!((text.as_str(), code), ("\ntwo\n", 3));
}

#[tokio::test]
async fn a_marker_or_character_split_across_reads_is_held_back() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let gate1 = Arc::new(Notify::new());
    let gate2 = Arc::new(Notify::new());
    // "café\n" with the é cut in half, then a marker line cut mid-word.
    srv.job(
        "split",
        Job {
            steps: vec![
                JobStep::Bytes(b"caf\xc3".to_vec()),
                JobStep::Wait(gate1.clone()),
                JobStep::Bytes(b"\xa9\nHELIX-EX".to_vec()),
                JobStep::Wait(gate2.clone()),
                JobStep::Write("IT 7\n".into()),
            ],
            exit: 0,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s.spawn_detached("split", None).await.unwrap();
    let mut chunk = s.read_job(&job, 0).await.unwrap();
    for _ in 0..200 {
        if !chunk.text.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        chunk = s.read_job(&job, 0).await.unwrap();
    }
    assert_eq!(
        (chunk.text.as_str(), chunk.next_offset),
        ("caf", STARTED.len() as u64 + 3)
    );

    gate1.notify_one();
    let mut offset = chunk.next_offset;
    let mut text = String::new();
    for _ in 0..200 {
        if !text.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let c = s.read_job(&job, offset).await.unwrap();
        assert_eq!(c.exit, None);
        offset = c.next_offset;
        text.push_str(&c.text);
    }
    // The newline and the marker-shaped fragment after it both wait for more bytes.
    assert_eq!(text, "\u{e9}");
    let c = s.read_job(&job, offset).await.unwrap();
    assert_eq!(
        (c.text.as_str(), c.next_offset, c.exit, c.alive),
        ("", offset, None, Some(true))
    );

    gate2.notify_one();
    let (rest, code, _) = read_to_exit(&mut s, &job, offset).await;
    assert_eq!((rest.as_str(), code), ("", 7));
}

#[tokio::test]
async fn without_setsid_the_job_detaches_with_nohup() {
    let srv = TestServer::start(ServerCfg {
        has_setsid: false,
        ..Default::default()
    })
    .await;
    srv.job(
        "echo hi",
        Job {
            steps: vec![JobStep::Write("hi\n".into())],
            exit: 0,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s.spawn_detached("echo hi", None).await.unwrap();
    let (text, code, _) = read_to_exit(&mut s, &job, 0).await;
    assert_eq!((text.as_str(), code), ("hi\n", 0));
    assert_eq!(
        srv.records().detach_tools,
        vec![("nohup".to_string(), false)]
    );
}

#[tokio::test]
async fn without_setsid_or_nohup_the_job_backgrounds_with_hup_ignored() {
    let srv = TestServer::start(ServerCfg {
        has_setsid: false,
        has_nohup: false,
        ..Default::default()
    })
    .await;
    srv.job(
        "echo hi",
        Job {
            steps: vec![JobStep::Write("hi".into())],
            exit: 0,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s.spawn_detached("echo hi", None).await.unwrap();
    let (text, code, _) = read_to_exit(&mut s, &job, 0).await;
    // Output without a trailing newline comes back exactly; the marker's own leading
    // newline is not part of it.
    assert_eq!((text.as_str(), code), ("hi", 0));
    assert_eq!(srv.records().detach_tools, vec![("sh".to_string(), true)]);
}

#[tokio::test]
async fn a_launch_that_leaves_no_log_is_an_error() {
    let srv = TestServer::start(ServerCfg {
        broken_launch: Some(String::new()),
        ..Default::default()
    })
    .await;
    let c = Client::with_start_timeout(Duration::from_millis(400));
    let mut s = connect(&c, &srv).await;
    let start = std::time::Instant::now();
    let err = s.spawn_detached("echo hi", None).await.unwrap_err();
    assert_eq!(
        err,
        TransportError::Protocol("job did not start: no log".into())
    );
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_launch_whose_log_never_shows_the_started_line_reports_the_log() {
    let srv = TestServer::start(ServerCfg {
        broken_launch: Some("sh: 1: Syntax error: end of file unexpected\nmore\n".into()),
        ..Default::default()
    })
    .await;
    let c = Client::with_start_timeout(Duration::from_millis(400));
    let mut s = connect(&c, &srv).await;
    let err = s.spawn_detached("echo hi", None).await.unwrap_err();
    assert_eq!(
        err,
        TransportError::Protocol(
            "job did not start: sh: 1: Syntax error: end of file unexpected".into()
        )
    );
}

#[tokio::test]
async fn a_job_that_dies_without_a_marker_reads_as_not_alive() {
    let srv = TestServer::start(ServerCfg::default()).await;
    srv.job(
        "oom",
        Job {
            steps: vec![JobStep::Write("working\n".into()), JobStep::Die],
            exit: 0,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s.spawn_detached("oom", None).await.unwrap();
    let mut last = None;
    for _ in 0..200 {
        let chunk = s.read_job(&job, 0).await.unwrap();
        if chunk.alive == Some(false) {
            last = Some(chunk);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let chunk = last.expect("the dead job must read as not alive");
    assert_eq!(chunk.exit, None);
    assert_eq!(chunk.text, "working");
}

#[tokio::test]
async fn a_log_that_vanishes_after_the_start_is_an_error() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let gate = Arc::new(Notify::new());
    srv.job(
        "long",
        Job {
            steps: vec![JobStep::Wait(gate.clone())],
            exit: 0,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s.spawn_detached("long", None).await.unwrap();
    std::fs::remove_file(srv.path(&job.log_path)).unwrap();
    assert!(s.read_job(&job, 0).await.is_err());
    gate.notify_one();
}

#[tokio::test]
async fn a_job_command_with_quotes_and_an_early_exit_still_reports_its_status() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let cmd = "echo 'it'\"'\"'s' > /tmp/x; exit 4 # trailing comment";
    srv.job(
        cmd,
        Job {
            steps: vec![],
            exit: 4,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s.spawn_detached(cmd, None).await.unwrap();
    let (_, code, _) = read_to_exit(&mut s, &job, 0).await;
    assert_eq!(code, 4);
    assert_eq!(srv.records().launched, vec![(cmd.to_string(), false)]);
}

#[tokio::test]
async fn sudo_password_goes_through_stdin_never_the_command_line() {
    let pw = "s3cr$t 'pw'";
    let srv = TestServer::start(ServerCfg {
        sudo_password: pw.into(),
        ..Default::default()
    })
    .await;
    srv.job(
        "/tmp/install.sh",
        Job {
            steps: vec![JobStep::Write("ok\n".into())],
            exit: 0,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s.spawn_detached("/tmp/install.sh", Some(pw)).await.unwrap();
    let (_, code, _) = read_to_exit(&mut s, &job, 0).await;
    assert_eq!(code, 0);

    let r = srv.records();
    assert_eq!(r.sudo_stdin, vec![pw.to_string()]);
    assert_eq!(r.launched, vec![("/tmp/install.sh".to_string(), true)]);
    for cmd in &r.commands {
        assert!(!cmd.contains(pw), "password leaked into {cmd:?}");
        assert!(!cmd.contains("s3cr"), "password leaked into {cmd:?}");
    }
    assert!(r.pty_commands.is_empty(), "no PTY unless sudo demands one");
}

#[tokio::test]
async fn sudo_that_demands_a_tty_is_retried_once_with_a_pty() {
    let srv = TestServer::start(ServerCfg {
        sudo_requiretty: true,
        ..Default::default()
    })
    .await;
    srv.job(
        "/tmp/install.sh",
        Job {
            steps: vec![],
            exit: 0,
        },
    );
    let c = client();
    let mut s = connect(&c, &srv).await;
    let job = s
        .spawn_detached("/tmp/install.sh", Some("makerbase"))
        .await
        .unwrap();
    let (_, code, _) = read_to_exit(&mut s, &job, 0).await;
    assert_eq!(code, 0);
    let r = srv.records();
    let sudo_runs: Vec<_> = r
        .commands
        .iter()
        .filter(|c| c.starts_with("sudo "))
        .collect();
    assert_eq!(sudo_runs.len(), 2, "one plain try, one PTY retry");
    assert_eq!(r.pty_commands.len(), 1);
    assert!(r.pty_commands[0].starts_with("sudo "));
    assert_eq!(r.sudo_stdin, vec!["makerbase".to_string()]);
    for cmd in &r.commands {
        assert!(!cmd.contains("makerbase"), "password leaked into {cmd:?}");
    }
}

#[tokio::test]
async fn a_wrong_sudo_password_is_an_auth_error() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let c = client();
    let mut s = connect(&c, &srv).await;
    let err = s
        .spawn_detached("/tmp/install.sh", Some("wrong"))
        .await
        .unwrap_err();
    assert_eq!(err, TransportError::Auth);
    assert!(srv.records().launched.is_empty());
}

#[tokio::test]
async fn a_wrong_login_password_is_an_auth_error() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let c = client();
    let err = c
        .ssh
        .connect(&host(srv.port), &login("root", "nope"))
        .await
        .err()
        .unwrap();
    assert_eq!(err, TransportError::Auth);
}

#[tokio::test]
async fn keyboard_interactive_only_servers_accept_the_password() {
    let srv = TestServer::start(ServerCfg {
        kbd_interactive_only: true,
        ..Default::default()
    })
    .await;
    let c = client();
    let mut s = connect(&c, &srv).await;
    srv.canned("true", "", "", 0);
    assert_eq!(s.exec("true", None, T).await.unwrap().exit, 0);
    assert_eq!(
        srv.records().auth_methods,
        vec!["keyboard-interactive".to_string()]
    );
    let err = c
        .ssh
        .connect(&host(srv.port), &login("root", "nope"))
        .await
        .err()
        .unwrap();
    assert_eq!(err, TransportError::Auth);
}

#[tokio::test]
async fn legacy_only_servers_still_negotiate() {
    let srv = TestServer::start(ServerCfg {
        legacy_only: true,
        ..Default::default()
    })
    .await;
    srv.canned("true", "", "", 0);
    let c = client();
    let mut s = connect(&c, &srv).await;
    assert_eq!(s.exec("true", None, T).await.unwrap().exit, 0);
}

#[tokio::test]
async fn host_keys_are_trusted_on_first_use_and_a_changed_key_is_refused_until_forgotten() {
    let srv = TestServer::start(ServerCfg::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("state").join("known_hosts.json");
    let cfg = ServerCfg::default();
    let lg = login(&cfg.user, &cfg.password);

    let ssh = SshRussh::new(&kh);
    ssh.connect(&host(srv.port), &lg).await.unwrap();
    assert!(kh.exists(), "first use records the key");
    ssh.connect(&host(srv.port), &lg).await.unwrap();

    srv.set_cfg(|c| c.host_key_seed = 2);
    let err = ssh.connect(&host(srv.port), &lg).await.err().unwrap();
    assert_eq!(
        err,
        TransportError::Protocol("host key changed for 127.0.0.1".into())
    );
    // The pin is on disk, not just in memory.
    let fresh = SshRussh::new(&kh);
    assert_eq!(
        fresh.connect(&host(srv.port), &lg).await.err().unwrap(),
        TransportError::Protocol("host key changed for 127.0.0.1".into())
    );

    fresh.forget_host("127.0.0.1").unwrap();
    fresh.connect(&host(srv.port), &lg).await.unwrap();
    // The new key is now the pinned one.
    ssh.connect(&host(srv.port), &lg).await.unwrap();
}

#[tokio::test]
async fn a_refused_port_is_unreachable() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let c = client();
    let err = c
        .ssh
        .connect(&host(port), &login("root", "x"))
        .await
        .err()
        .unwrap();
    assert_eq!(err, TransportError::Unreachable);
}

#[tokio::test]
async fn a_server_that_never_speaks_times_out() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let _hold = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let ssh =
        SshRussh::new(dir.path().join("kh.json")).with_connect_timeout(Duration::from_millis(300));
    let start = std::time::Instant::now();
    let err = ssh
        .connect(&host(port), &login("root", "x"))
        .await
        .err()
        .unwrap();
    assert_eq!(err, TransportError::Timeout);
    assert!(start.elapsed() < Duration::from_secs(5));
}

/// Hardware check against a real printer:
/// `HELIX_SSH_TEST_HOST=192.168.1.50 HELIX_SSH_TEST_USER=root HELIX_SSH_TEST_PASS=... \
///  cargo test -p helix_installer_core --test ssh -- --ignored real_host`
/// Set `HELIX_SSH_TEST_SUDO=1` to run the job through sudo with the same password.
#[tokio::test]
#[ignore]
async fn real_host_exec_upload_and_detached_job() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let addr = env("HELIX_SSH_TEST_HOST");
    let lg = login(&env("HELIX_SSH_TEST_USER"), &env("HELIX_SSH_TEST_PASS"));
    let sudo = std::env::var("HELIX_SSH_TEST_SUDO").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().unwrap();
    let ssh = SshRussh::new(dir.path().join("known_hosts.json"));
    let mut s = ssh.connect(&Host::new(addr.clone()), &lg).await.unwrap();

    let out = s.exec("echo hello", None, T).await.unwrap();
    assert_eq!((out.stdout.trim(), out.exit), ("hello", 0));

    let local = dir.path().join("probe.bin");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&local, &data).unwrap();
    s.upload(&local, "/tmp/helix installer probe.bin")
        .await
        .unwrap();
    s.exec("rm -f '/tmp/helix installer probe.bin'", None, T)
        .await
        .unwrap();

    let pw = sudo.then_some(lg.password.as_str());
    let job = s
        .spawn_detached("echo started; sleep 2; id -u", pw)
        .await
        .unwrap();
    drop(s);
    let mut s = ssh.connect(&Host::new(addr), &lg).await.unwrap();
    let (text, code, _) = read_to_exit(&mut s, &job, 0).await;
    assert_eq!(code, 0, "{text}");
    assert!(text.starts_with("started"), "{text}");
    if sudo {
        assert!(text.contains("\n0\n"), "sudo job runs as root: {text}");
    }
    s.exec(&format!("rm -f {}", sh_quote(&job.log_path)), None, T)
        .await
        .unwrap();
}
