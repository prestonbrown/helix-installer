// SPDX-License-Identifier: GPL-3.0-or-later

//! An in-process SSH server whose "shell" understands exactly the command shapes the
//! transport sends (upload, size check, setsid probe, detached launch, sudo, `tail -c`)
//! plus a table of canned commands. Files live under a temp dir standing in for `/`, so
//! uploads and job logs are real bytes on disk. It records every command line and every
//! byte fed to sudo so tests can prove where a secret went.

#![allow(dead_code)]

use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Response, Session};
use russh::{Channel, ChannelId, ChannelMsg, MethodKind, MethodSet, Preferred};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
pub enum JobStep {
    Write(String),
    Bytes(Vec<u8>),
    /// Blocks the job until the test calls `notify_one` on it.
    Wait(Arc<Notify>),
}

#[derive(Clone)]
pub struct Job {
    pub steps: Vec<JobStep>,
    pub exit: i32,
}

#[derive(Clone)]
pub struct ServerCfg {
    pub user: String,
    pub password: String,
    /// Offer only keyboard-interactive, like some dropbear builds.
    pub kbd_interactive_only: bool,
    pub host_key_seed: u8,
    pub has_setsid: bool,
    pub sudo_password: String,
    /// sudo refuses without a tty (`Defaults requiretty`).
    pub sudo_requiretty: bool,
    /// Drops the last byte of every upload, so a size check has something to catch.
    pub truncate_uploads: bool,
    /// Negotiate only 2015-era algorithms: dh group14-sha1, aes128-cbc, hmac-sha1.
    pub legacy_only: bool,
}

impl Default for ServerCfg {
    fn default() -> Self {
        ServerCfg {
            user: "root".into(),
            password: "creality_2023".into(),
            kbd_interactive_only: false,
            host_key_seed: 1,
            has_setsid: true,
            sudo_password: "makerbase".into(),
            sudo_requiretty: false,
            truncate_uploads: false,
            legacy_only: false,
        }
    }
}

#[derive(Default)]
pub struct Records {
    /// Every exec command line, in order, including the inner command of `sudo sh -c`.
    pub commands: Vec<String>,
    /// Every exec command line that ran with a PTY.
    pub pty_commands: Vec<String>,
    /// Lines sudo read from stdin.
    pub sudo_stdin: Vec<String>,
    /// The detach tool (`setsid` / `nohup`) of each launch.
    pub detach_tools: Vec<String>,
    /// Commands started as detached jobs, and whether sudo started them.
    pub launched: Vec<(String, bool)>,
    /// Methods that authenticated a user: "password" or "keyboard-interactive".
    pub auth_methods: Vec<String>,
}

pub struct ServerState {
    pub cfg: Mutex<ServerCfg>,
    pub root: tempfile::TempDir,
    pub records: Mutex<Records>,
    pub canned: Mutex<HashMap<String, (String, String, u32)>>,
    pub jobs: Mutex<HashMap<String, Job>>,
    sessions: Mutex<Vec<server::Handle>>,
}

pub struct TestServer {
    pub port: u16,
    pub state: Arc<ServerState>,
    accept: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl TestServer {
    pub async fn start(cfg: ServerCfg) -> TestServer {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("tmp")).unwrap();
        let state = Arc::new(ServerState {
            cfg: Mutex::new(cfg),
            root,
            records: Mutex::new(Records::default()),
            canned: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            sessions: Mutex::new(Vec::new()),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let st = state.clone();
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let config = Arc::new(server_config(&lock(&st.cfg)));
                let handler = Handler { state: st.clone() };
                tokio::spawn(async move {
                    if let Ok(running) = server::run_stream(config, stream, handler).await {
                        let _ = running.await;
                    }
                });
            }
        });
        TestServer {
            port,
            state,
            accept,
        }
    }

    pub fn set_cfg(&self, f: impl FnOnce(&mut ServerCfg)) {
        f(&mut lock(&self.state.cfg));
    }

    pub fn records(&self) -> std::sync::MutexGuard<'_, Records> {
        lock(&self.state.records)
    }

    /// Answers `cmd` with this stdout, stderr and exit status.
    pub fn canned(&self, cmd: &str, stdout: &str, stderr: &str, exit: u32) {
        lock(&self.state.canned).insert(cmd.into(), (stdout.into(), stderr.into(), exit));
    }

    pub fn job(&self, cmd: &str, job: Job) {
        lock(&self.state.jobs).insert(cmd.into(), job);
    }

    /// The real file behind a remote absolute path.
    pub fn path(&self, remote: &str) -> PathBuf {
        local_path(&self.state, remote)
    }

    /// Cuts every live connection from the server side, like a laptop losing Wi-Fi.
    pub async fn drop_connections(&self) {
        let sessions: Vec<_> = lock(&self.state.sessions).drain(..).collect();
        for s in sessions {
            let _ = s
                .disconnect(russh::Disconnect::ByApplication, "gone".into(), "en".into())
                .await;
        }
    }
}

fn server_config(cfg: &ServerCfg) -> server::Config {
    let mut seed = [0u8; 32];
    seed[0] = cfg.host_key_seed;
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    let mut methods = MethodSet::empty();
    if cfg.kbd_interactive_only {
        methods.push(MethodKind::KeyboardInteractive);
    } else {
        methods.push(MethodKind::Password);
        methods.push(MethodKind::KeyboardInteractive);
    }
    let preferred = if cfg.legacy_only {
        Preferred {
            kex: Cow::Borrowed(&[
                russh::kex::DH_G14_SHA1,
                russh::kex::EXTENSION_SUPPORT_AS_SERVER,
            ]),
            cipher: Cow::Borrowed(&[russh::cipher::AES_128_CBC]),
            mac: Cow::Borrowed(&[russh::mac::HMAC_SHA1]),
            ..Preferred::default()
        }
    } else {
        Preferred::default()
    };
    server::Config {
        keys: vec![key],
        methods,
        preferred,
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    }
}

fn local_path(state: &ServerState, remote: &str) -> PathBuf {
    state.root.path().join(remote.trim_start_matches('/'))
}

struct Handler {
    state: Arc<ServerState>,
}

impl server::Handler for Handler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let cfg = lock(&self.state.cfg).clone();
        if !cfg.kbd_interactive_only && user == cfg.user && password == cfg.password {
            lock(&self.state.records)
                .auth_methods
                .push("password".into());
            return Ok(Auth::Accept);
        }
        Ok(Auth::reject())
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let cfg = lock(&self.state.cfg).clone();
        let Some(mut response) = response else {
            return Ok(Auth::Partial {
                name: Cow::Borrowed(""),
                instructions: Cow::Borrowed(""),
                prompts: Cow::Owned(vec![(Cow::Borrowed("Password: "), false)]),
            });
        };
        let answer = response.next();
        if user == cfg.user && answer.as_deref() == Some(cfg.password.as_bytes()) {
            lock(&self.state.records)
                .auth_methods
                .push("keyboard-interactive".into());
            return Ok(Auth::Accept);
        }
        Ok(Auth::reject())
    }

    async fn auth_succeeded(&mut self, session: &mut Session) -> Result<(), Self::Error> {
        lock(&self.state.sessions).push(session.handle());
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        tokio::spawn(run_channel(channel, self.state.clone()));
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }
}

/// Collects the command and all of stdin, runs it, and sends the result back.
async fn run_channel(mut channel: Channel<Msg>, state: Arc<ServerState>) {
    let mut pty = false;
    let mut cmd = None;
    let mut stdin = Vec::new();
    loop {
        match channel.wait().await {
            Some(ChannelMsg::RequestPty { .. }) => pty = true,
            Some(ChannelMsg::Exec { command, .. }) => {
                cmd = Some(String::from_utf8_lossy(&command).into_owned())
            }
            Some(ChannelMsg::Data { data }) => stdin.extend_from_slice(&data),
            Some(ChannelMsg::Eof) => break,
            Some(_) => {}
            None => return,
        }
    }
    let Some(cmd) = cmd else { return };
    {
        let mut r = lock(&state.records);
        r.commands.push(cmd.clone());
        if pty {
            r.pty_commands.push(cmd.clone());
        }
    }
    let (mut out, err, code) = run_command(&state, &cmd, &stdin, pty, false);
    // A PTY merges stderr into stdout and turns newlines into CRLF, as a real one does.
    if pty {
        out.extend_from_slice(err.as_bytes());
        out = String::from_utf8_lossy(&out)
            .replace('\n', "\r\n")
            .into_bytes();
    } else if !err.is_empty() {
        let _ = channel.extended_data(1, err.as_bytes()).await;
    }
    if !out.is_empty() {
        let _ = channel.data(&out[..]).await;
    }
    let _ = channel.exit_status(code).await;
    let _ = channel.eof().await;
    let _ = channel.close().await;
}

/// Splits a command line into words, honouring single quotes and backslash escapes outside
/// them. Operators must be separated by spaces, which is how the transport writes them.
fn words(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    cur.push(q);
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' | '\t' | '\n' => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

const JOB_PREFIX: &str = "(\n";
const JOB_SUFFIX: &str = "\n)\necho HELIX-EXIT $?";

fn run_command(
    state: &Arc<ServerState>,
    cmd: &str,
    stdin: &[u8],
    pty: bool,
    as_root: bool,
) -> (Vec<u8>, String, u32) {
    if let Some((o, e, c)) = lock(&state.canned).get(cmd).cloned() {
        return (o.into_bytes(), e, c);
    }
    let cfg = lock(&state.cfg).clone();
    let w = words(cmd);
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    match w.as_slice() {
        ["command", "-v", "setsid"] => {
            if cfg.has_setsid {
                (b"/usr/bin/setsid\n".to_vec(), String::new(), 0)
            } else {
                (Vec::new(), String::new(), 1)
            }
        }
        ["cat", ">", part, "&&", "mv", part2, dest] if part == part2 => {
            let mut bytes = stdin.to_vec();
            if cfg.truncate_uploads {
                bytes.pop();
            }
            let part_path = local_path(state, part);
            if let Err(e) = std::fs::write(&part_path, &bytes) {
                return (Vec::new(), format!("sh: can't create {part}: {e}\n"), 1);
            }
            match std::fs::rename(&part_path, local_path(state, dest)) {
                Ok(()) => (Vec::new(), String::new(), 0),
                Err(e) => (Vec::new(), format!("mv: {e}\n"), 1),
            }
        }
        ["wc", "-c", "<", path] => match std::fs::metadata(local_path(state, path)) {
            Ok(m) => (format!("{}\n", m.len()).into_bytes(), String::new(), 0),
            Err(_) => (
                Vec::new(),
                format!("sh: can't open {path}: no such file\n"),
                1,
            ),
        },
        ["tail", "-c", from, path] if from.starts_with('+') => {
            let Ok(start) = from[1..].parse::<usize>() else {
                return (Vec::new(), "tail: invalid number\n".into(), 1);
            };
            match std::fs::read(local_path(state, path)) {
                Ok(bytes) => {
                    let s = start.saturating_sub(1).min(bytes.len());
                    (bytes[s..].to_vec(), String::new(), 0)
                }
                Err(_) => (
                    Vec::new(),
                    format!("tail: can't open '{path}': No such file or directory\n"),
                    1,
                ),
            }
        }
        ["sudo", "-S", "-p", "", "sh", "-c", inner] => {
            if cfg.sudo_requiretty && !pty {
                return (
                    Vec::new(),
                    "sudo: sorry, you must have a tty to run sudo\n".into(),
                    1,
                );
            }
            let text = String::from_utf8_lossy(stdin);
            let line = text.lines().next().unwrap_or("").trim_end_matches('\r');
            lock(&state.records).sudo_stdin.push(line.to_string());
            if line != cfg.sudo_password {
                return (
                    Vec::new(),
                    "Sorry, try again.\nsudo: 1 incorrect password attempt\n".into(),
                    1,
                );
            }
            lock(&state.records).commands.push(inner.to_string());
            run_command(state, inner, b"", pty, true)
        }
        [tool, "sh", "-c", script, ">", log, "2>&1", "<", "/dev/null", "&", "echo", "HELIX-PID", "$!"]
            if *tool == "setsid" || *tool == "nohup" =>
        {
            if *tool == "setsid" && !cfg.has_setsid {
                return (Vec::new(), "sh: setsid: not found\n".into(), 127);
            }
            let Some(job_cmd) = script
                .strip_prefix(JOB_PREFIX)
                .and_then(|s| s.strip_suffix(JOB_SUFFIX))
            else {
                return (Vec::new(), "unrecognised job script\n".into(), 2);
            };
            {
                let mut r = lock(&state.records);
                r.detach_tools.push(tool.to_string());
                r.launched.push((job_cmd.to_string(), as_root));
            }
            let log_path = local_path(state, log);
            if let Err(e) = std::fs::write(&log_path, b"") {
                return (Vec::new(), format!("sh: can't create {log}: {e}\n"), 1);
            }
            let job = lock(&state.jobs).get(job_cmd).cloned().unwrap_or(Job {
                steps: vec![JobStep::Write(format!("sh: {job_cmd}: not found\n"))],
                exit: 127,
            });
            tokio::spawn(run_job(log_path, job));
            (b"HELIX-PID 4242\n".to_vec(), String::new(), 0)
        }
        _ => (Vec::new(), format!("sh: {cmd}: not found\n"), 127),
    }
}

async fn run_job(log: PathBuf, job: Job) {
    use std::io::Write;
    let append = |b: &[u8]| {
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log) {
            let _ = f.write_all(b);
        }
    };
    for step in job.steps {
        match step {
            JobStep::Write(s) => append(s.as_bytes()),
            JobStep::Bytes(b) => append(&b),
            JobStep::Wait(n) => n.notified().await,
        }
    }
    append(format!("HELIX-EXIT {}\n", job.exit).as_bytes());
}
