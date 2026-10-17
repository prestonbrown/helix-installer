// SPDX-License-Identifier: GPL-3.0-or-later

//! SSH to printers over russh. Everything runs on exec channels because BusyBox targets
//! (Creality K1, FlashForge AD5M) ship no sftp-server, and every command is written for
//! BusyBox ash as well as Debian's dash/bash.
//!
//! Host keys are trusted on first use and pinned in a small JSON file; a printer whose key
//! changes is refused until the user says it was reflashed (`forget_host`).

use super::*;
use crate::fs_atomic::write_atomic;
use crate::manifest::Login;
use russh::client::{self, KeyboardInteractiveAuthResponse};
use russh::keys::{HashAlg, PublicKeyOrCertificate};
use russh::{cipher, kex, mac, Channel, ChannelMsg, MethodKind, Preferred};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// How long the TCP connect, SSH handshake and login may each take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for the short housekeeping commands (`wc`, `tail`, probes, launching a job).
/// sudo sleeps about two seconds after a wrong password, so this stays generous.
const SHORT_TIMEOUT: Duration = Duration::from_secs(30);
/// Keyboard-interactive servers may send several rounds of prompts; past this many the
/// login is treated as failed instead of looping forever.
const MAX_KBD_ROUNDS: usize = 8;
const EXIT_MARKER: &str = "HELIX-EXIT ";
const PID_MARKER: &str = "HELIX-PID ";

/// Modern algorithms first, then the older ones a 2015-2019 dropbear or OpenSSH on a
/// printer may be limited to (sha1 Diffie-Hellman, NIST ECDH, CBC ciphers, hmac-sha1).
/// The `ssh-rsa` host key algorithm is already in russh's default key list.
const KEX_ORDER: &[kex::Name] = &[
    kex::MLKEM768X25519_SHA256,
    kex::CURVE25519,
    kex::CURVE25519_PRE_RFC_8731,
    kex::DH_GEX_SHA256,
    kex::DH_G18_SHA512,
    kex::DH_G17_SHA512,
    kex::DH_G16_SHA512,
    kex::DH_G15_SHA512,
    kex::DH_G14_SHA256,
    kex::ECDH_SHA2_NISTP256,
    kex::ECDH_SHA2_NISTP384,
    kex::ECDH_SHA2_NISTP521,
    kex::DH_G14_SHA1,
    kex::DH_GEX_SHA1,
    kex::DH_G1_SHA1,
    kex::EXTENSION_SUPPORT_AS_CLIENT,
    kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
];

const CIPHER_ORDER: &[cipher::Name] = &[
    cipher::CHACHA20_POLY1305,
    cipher::AES_256_GCM,
    cipher::AES_128_GCM,
    cipher::AES_256_CTR,
    cipher::AES_192_CTR,
    cipher::AES_128_CTR,
    cipher::AES_256_CBC,
    cipher::AES_192_CBC,
    cipher::AES_128_CBC,
];

const MAC_ORDER: &[mac::Name] = &[
    mac::HMAC_SHA512_ETM,
    mac::HMAC_SHA256_ETM,
    mac::HMAC_SHA512,
    mac::HMAC_SHA256,
    mac::HMAC_SHA1_ETM,
    mac::HMAC_SHA1,
];

/// Quotes `s` as one POSIX shell word: single quotes, with each embedded `'` written as
/// `'\''`. Every path or argument placed into a remote command goes through here.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn io_err(e: impl std::fmt::Display) -> TransportError {
    TransportError::Io(e.to_string())
}

fn russh_err(e: russh::Error) -> TransportError {
    match e {
        russh::Error::IO(e) => TransportError::Io(e.to_string()),
        e => TransportError::Protocol(e.to_string()),
    }
}

/// Pinned host key fingerprints, keyed like OpenSSH's known_hosts: the bare address for
/// port 22, `[addr]:port` otherwise.
#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownHostsFile {
    #[serde(default)]
    hosts: BTreeMap<String, String>,
}

struct KnownHosts {
    path: PathBuf,
    /// Serialises read-modify-write of the file between concurrent connects.
    lock: Mutex<()>,
}

impl KnownHosts {
    fn load(&self) -> Result<KnownHostsFile> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io_err(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KnownHostsFile::default()),
            Err(e) => Err(io_err(format!("{}: {e}", self.path.display()))),
        }
    }

    fn save(&self, file: &KnownHostsFile) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err)?;
        }
        let bytes = serde_json::to_vec_pretty(file).map_err(io_err)?;
        write_atomic(&self.path, &bytes).map_err(io_err)
    }

    /// True when `fingerprint` matches the pinned key or nothing was pinned yet (it is then
    /// pinned); false when a different key is pinned.
    fn check_or_pin(&self, entry: &str, fingerprint: &str) -> Result<bool> {
        let _guard = lock(&self.lock);
        let mut file = self.load()?;
        match file.hosts.get(entry) {
            Some(pinned) => Ok(pinned == fingerprint),
            None => {
                file.hosts
                    .insert(entry.to_string(), fingerprint.to_string());
                self.save(&file)?;
                Ok(true)
            }
        }
    }

    fn forget(&self, addr: &str) -> Result<()> {
        let _guard = lock(&self.lock);
        let mut file = self.load()?;
        let bracketed = format!("[{addr}]:");
        let before = file.hosts.len();
        file.hosts
            .retain(|k, _| k != addr && !k.starts_with(&bracketed));
        if file.hosts.len() != before {
            self.save(&file)?;
        }
        Ok(())
    }
}

fn known_hosts_entry(host: &Host) -> String {
    if host.ssh_port == 22 {
        host.addr.clone()
    } else {
        format!("[{}]:{}", host.addr, host.ssh_port)
    }
}

/// The russh-backed `Ssh`. One `connect` is one login attempt with one credential; which
/// credentials to try, and in what order, is the caller's policy.
pub struct SshRussh {
    known: Arc<KnownHosts>,
    config: Arc<client::Config>,
    connect_timeout: Duration,
}

impl SshRussh {
    /// `known_hosts_path` is the JSON file that pins each printer's host key.
    pub fn new(known_hosts_path: impl Into<PathBuf>) -> Self {
        let config = client::Config {
            preferred: Preferred {
                kex: Cow::Borrowed(KEX_ORDER),
                cipher: Cow::Borrowed(CIPHER_ORDER),
                mac: Cow::Borrowed(MAC_ORDER),
                ..Preferred::DEFAULT
            },
            // A laptop that sleeps or leaves Wi-Fi turns into an error within a minute
            // instead of a session that hangs forever.
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
            ..Default::default()
        };
        SshRussh {
            known: Arc::new(KnownHosts {
                path: known_hosts_path.into(),
                lock: Mutex::new(()),
            }),
            config: Arc::new(config),
            connect_timeout: CONNECT_TIMEOUT,
        }
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Drops the pinned key for `addr` on every port, so the next connect trusts whatever
    /// key the printer now presents. For a printer the user has reflashed.
    pub fn forget_host(&self, addr: &str) -> Result<()> {
        self.known.forget(addr)
    }

    async fn handshake(&self, host: &Host) -> Result<client::Handle<HostKeyCheck>> {
        let stream = tokio::net::TcpStream::connect((host.addr.as_str(), host.ssh_port))
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::TimedOut => TransportError::Timeout,
                _ => TransportError::Unreachable,
            })?;
        let _ = stream.set_nodelay(true);
        let verdict = Arc::new(Mutex::new(None));
        let handler = HostKeyCheck {
            known: self.known.clone(),
            entry: known_hosts_entry(host),
            addr: host.addr.clone(),
            verdict: verdict.clone(),
        };
        client::connect_stream(self.config.clone(), stream, handler)
            .await
            .map_err(|e| lock(&verdict).take().unwrap_or_else(|| russh_err(e)))
    }
}

/// Checks the server's key against the pin; a refusal records why, so `connect` can
/// report "host key changed" instead of russh's generic error.
struct HostKeyCheck {
    known: Arc<KnownHosts>,
    entry: String,
    addr: String,
    verdict: Arc<Mutex<Option<TransportError>>>,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let refusal = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
                match self.known.check_or_pin(&self.entry, &fingerprint) {
                    Ok(true) => return Ok(true),
                    Ok(false) => {
                        TransportError::Protocol(format!("host key changed for {}", self.addr))
                    }
                    Err(e) => e,
                }
            }
            PublicKeyOrCertificate::Certificate(_) => TransportError::Protocol(format!(
                "{} presented a host certificate instead of a key",
                self.addr
            )),
        };
        *lock(&self.verdict) = Some(refusal);
        Ok(false)
    }
}

/// Password first; keyboard-interactive when the server offers it instead (some dropbear
/// builds offer nothing else), answering every prompt with the password.
async fn authenticate(handle: &mut client::Handle<HostKeyCheck>, login: &Login) -> Result<()> {
    let first = handle
        .authenticate_password(login.user.clone(), login.password.clone())
        .await
        .map_err(russh_err)?;
    match first {
        client::AuthResult::Success => return Ok(()),
        client::AuthResult::Failure {
            remaining_methods, ..
        } if !remaining_methods.contains(&MethodKind::KeyboardInteractive) => {
            return Err(TransportError::Auth)
        }
        client::AuthResult::Failure { .. } => {}
    }
    let mut reply = handle
        .authenticate_keyboard_interactive_start(login.user.clone(), None::<String>)
        .await
        .map_err(russh_err)?;
    for _ in 0..MAX_KBD_ROUNDS {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Err(TransportError::Auth),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = vec![login.password.clone(); prompts.len()];
                reply = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(russh_err)?;
            }
        }
    }
    Err(TransportError::Auth)
}

#[async_trait]
impl Ssh for SshRussh {
    async fn connect(&self, host: &Host, login: &Login) -> Result<Box<dyn SshSession>> {
        let mut handle = tokio::time::timeout(self.connect_timeout, self.handshake(host))
            .await
            .map_err(|_| TransportError::Timeout)??;
        tokio::time::timeout(self.connect_timeout, authenticate(&mut handle, login))
            .await
            .map_err(|_| TransportError::Timeout)??;
        Ok(Box::new(RusshSession { handle }))
    }
}

struct RawOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: i32,
}

impl RawOutput {
    fn into_exec(self) -> ExecOutput {
        ExecOutput {
            stdout: String::from_utf8_lossy(&self.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&self.stderr).into_owned(),
            exit: self.exit,
        }
    }
}

/// Reads a channel to its end. A channel that closes without an exit status means the
/// connection died under the command.
async fn collect(channel: &mut Channel<client::Msg>) -> Result<RawOutput> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => {
                exit = Some(i32::try_from(exit_status).unwrap_or(i32::MAX))
            }
            ChannelMsg::ExitSignal { .. } => exit = Some(-1),
            ChannelMsg::Failure => {
                return Err(TransportError::Protocol(
                    "the printer refused to run the command".into(),
                ))
            }
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    let exit = exit.ok_or_else(|| {
        TransportError::Protocol("connection closed before the command finished".into())
    })?;
    Ok(RawOutput {
        stdout,
        stderr,
        exit,
    })
}

static JOB_SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique per launch, so two runs never share a log.
fn job_log_path() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = JOB_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("/tmp/helix-installer-{nanos:x}-{seq}.log")
}

/// The shell line that starts `cmd` in its own session with its output in `log`, then
/// prints the child's pid. `cmd` runs in a subshell on its own lines, so an `exit` inside
/// it or a trailing `# comment` still lets the exit marker line run.
fn launcher(cmd: &str, log: &str, detach: &str) -> String {
    let script = format!("(\n{cmd}\n)\necho {EXIT_MARKER}$?");
    format!(
        "{detach} sh -c {} > {} 2>&1 < /dev/null & echo {PID_MARKER}$!",
        sh_quote(&script),
        sh_quote(log)
    )
}

fn launched_pid(stdout: &[u8]) -> Option<u32> {
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix(PID_MARKER)?.trim().parse().ok())
}

fn sudo_wants_tty(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("must have a tty") || t.contains("no tty present")
}

fn sudo_rejected_password(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("incorrect password") || t.contains("sorry, try again")
}

/// Splits new log bytes into the text to show, how many bytes were consumed, and the exit
/// status once the marker line has arrived. The marker line is consumed but not shown. A
/// trailing fragment that could still grow into the marker, or a UTF-8 sequence cut off by
/// the read, is left for the next read.
fn parse_job_log(bytes: &[u8]) -> (String, u64, Option<i32>) {
    let mut line_start = 0;
    while let Some(nl) = bytes[line_start..].iter().position(|&b| b == b'\n') {
        let line_end = line_start + nl;
        let line = String::from_utf8_lossy(&bytes[line_start..line_end]);
        if let Some(code) = line
            .trim_end_matches('\r')
            .strip_prefix(EXIT_MARKER)
            .and_then(|c| c.trim().parse::<i32>().ok())
        {
            let text = String::from_utf8_lossy(&bytes[..line_start]).into_owned();
            return (text, (line_end + 1) as u64, Some(code));
        }
        line_start = line_end + 1;
    }
    let tail = &bytes[line_start..];
    let mut keep = if !tail.is_empty()
        && (EXIT_MARKER.as_bytes().starts_with(tail) || tail.starts_with(EXIT_MARKER.as_bytes()))
    {
        line_start
    } else {
        bytes.len()
    };
    if let Err(e) = std::str::from_utf8(&bytes[..keep]) {
        if e.error_len().is_none() {
            keep = e.valid_up_to();
        }
    }
    let text = String::from_utf8_lossy(&bytes[..keep]).into_owned();
    (text, keep as u64, None)
}

struct RusshSession {
    handle: client::Handle<HostKeyCheck>,
}

impl RusshSession {
    async fn run(
        &mut self,
        cmd: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
        pty: bool,
    ) -> Result<RawOutput> {
        tokio::time::timeout(timeout, self.run_untimed(cmd, stdin, pty))
            .await
            .map_err(|_| TransportError::Timeout)?
    }

    async fn run_untimed(
        &mut self,
        cmd: &str,
        stdin: Option<&[u8]>,
        pty: bool,
    ) -> Result<RawOutput> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(russh_err)?;
        if pty {
            channel
                .request_pty(false, "dumb", 80, 24, 0, 0, &[])
                .await
                .map_err(russh_err)?;
        }
        channel.exec(true, cmd).await.map_err(russh_err)?;
        if let Some(bytes) = stdin {
            channel.data(bytes).await.map_err(russh_err)?;
        }
        channel.eof().await.map_err(russh_err)?;
        collect(&mut channel).await
    }

    async fn launch_with_sudo(&mut self, launcher: &str, password: &str) -> Result<RawOutput> {
        let line = format!("sudo -S -p '' sh -c {}", sh_quote(launcher));
        let stdin = format!("{password}\n");
        let first = self
            .run(&line, Some(stdin.as_bytes()), SHORT_TIMEOUT, false)
            .await?;
        if launched_pid(&first.stdout).is_none() {
            let said = String::from_utf8_lossy(&first.stderr);
            if sudo_wants_tty(&said) || sudo_wants_tty(&String::from_utf8_lossy(&first.stdout)) {
                return self
                    .run(&line, Some(stdin.as_bytes()), SHORT_TIMEOUT, true)
                    .await;
            }
        }
        Ok(first)
    }
}

#[async_trait]
impl SshSession for RusshSession {
    async fn exec(
        &mut self,
        cmd: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput> {
        Ok(self.run(cmd, stdin, timeout, false).await?.into_exec())
    }

    async fn upload(&mut self, local: &Path, remote: &str) -> Result<()> {
        let want = tokio::fs::metadata(local).await.map_err(io_err)?.len();
        let file = tokio::fs::File::open(local).await.map_err(io_err)?;
        let part = format!("{remote}.part");
        let cmd = format!(
            "cat > {} && mv {} {}",
            sh_quote(&part),
            sh_quote(&part),
            sh_quote(remote)
        );
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(russh_err)?;
        channel.exec(true, cmd).await.map_err(russh_err)?;
        let sent = channel.data(file).await;
        let _ = channel.eof().await;
        let out = collect(&mut channel).await?;
        if out.exit != 0 {
            return Err(TransportError::Protocol(format!(
                "upload to {remote} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        sent.map_err(russh_err)?;

        let size = self
            .run(
                &format!("wc -c < {}", sh_quote(remote)),
                None,
                SHORT_TIMEOUT,
                false,
            )
            .await?;
        let got = String::from_utf8_lossy(&size.stdout).trim().parse::<u64>();
        match got {
            Ok(got) if size.exit == 0 && got == want => Ok(()),
            Ok(got) => Err(TransportError::Protocol(format!(
                "{remote} has {got} bytes on the printer, expected {want}"
            ))),
            Err(_) => Err(TransportError::Protocol(format!(
                "could not read the size of {remote}: {}",
                String::from_utf8_lossy(&size.stderr).trim()
            ))),
        }
    }

    async fn spawn_detached(
        &mut self,
        cmd: &str,
        sudo_password: Option<&str>,
    ) -> Result<RemoteJob> {
        let probe = self
            .run("command -v setsid", None, SHORT_TIMEOUT, false)
            .await?;
        let detach = if probe.exit == 0 && !probe.stdout.trim_ascii().is_empty() {
            "setsid"
        } else {
            "nohup"
        };
        let log = job_log_path();
        let line = launcher(cmd, &log, detach);
        let out = match sudo_password {
            None => self.run(&line, None, SHORT_TIMEOUT, false).await?,
            Some(pw) => self.launch_with_sudo(&line, pw).await?,
        };
        if launched_pid(&out.stdout).is_some() {
            return Ok(RemoteJob { log_path: log });
        }
        let mut said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        // A PTY echoes typed input until sudo turns echo off, so the password can come
        // back in the output; it must not reach an error message.
        if let Some(pw) = sudo_password.filter(|p| !p.is_empty()) {
            said = said.replace(pw, "<redacted>");
            if sudo_rejected_password(&said) {
                return Err(TransportError::Auth);
            }
        }
        Err(TransportError::Protocol(format!(
            "the job did not start (exit {}): {}",
            out.exit,
            said.trim()
        )))
    }

    async fn read_job(&mut self, job: &RemoteJob, from_offset: u64) -> Result<JobChunk> {
        let cmd = format!(
            "tail -c +{} {}",
            from_offset.saturating_add(1),
            sh_quote(&job.log_path)
        );
        let out = self.run(&cmd, None, SHORT_TIMEOUT, false).await?;
        if out.exit != 0 {
            // The shell creates the log a moment after the launch returns.
            if from_offset == 0 {
                return Ok(JobChunk {
                    text: String::new(),
                    next_offset: 0,
                    exit: None,
                });
            }
            return Err(TransportError::Protocol(format!(
                "cannot read {}: {}",
                job.log_path,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let (text, consumed, exit) = parse_job_log(&out.stdout);
        Ok(JobChunk {
            text,
            next_offset: from_offset + consumed,
            exit,
        })
    }
}
