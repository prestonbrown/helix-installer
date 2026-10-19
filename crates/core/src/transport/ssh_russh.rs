// SPDX-License-Identifier: GPL-3.0-or-later

//! SSH to printers over russh. Everything runs on exec channels because BusyBox targets
//! (Creality K1, FlashForge AD5M) ship no sftp-server, and every command is written for
//! BusyBox ash as well as Debian's dash/bash.
//!
//! Host keys are trusted on first use and pinned in a small JSON file; a printer whose key
//! changes is refused until the user says it was reflashed (`forget_host`).
//!
//! A login tries this computer's SSH keys first, the OS agent's then the default key files, and
//! only then the password.

use super::*;
use crate::fs_atomic::write_atomic;
use crate::manifest::Login;
use russh::client::{self, AuthResult, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::{
    decode_secret_key, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey,
    PublicKeyOrCertificate,
};
use russh::{cipher, kex, mac, Channel, ChannelMsg, ChannelReadHalf, MethodKind, Preferred};
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
/// Keys offered before the password. A server that counts refusals hangs up after a few
/// (OpenSSH's MaxAuthTries is 6); the password then goes over a fresh connection.
const MAX_KEY_OFFERS: usize = 6;
/// The key files tried, in order, in the user's `.ssh` directory.
const KEY_FILE_NAMES: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];
/// How long reaching the agent may take; a Windows pipe that stays busy is given up on.
const AGENT_TIMEOUT: Duration = Duration::from_secs(3);
const EXIT_MARKER: &str = "HELIX-EXIT ";
const STARTED_MARKER: &str = "HELIX-STARTED ";
/// How long a launched job has to write its `HELIX-STARTED` line, and how often to look.
const START_TIMEOUT: Duration = Duration::from_secs(5);
const START_POLL: Duration = Duration::from_millis(100);

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

/// Appended to the inherited PATH for every remote command: a non-login exec on a BusyBox
/// printer (FlashForge AD5M) gets a PATH without /sbin, where `reboot` and friends live.
/// Appending finds those without shadowing the account's own tools (/usr/local/bin, Entware's
/// /opt/bin on a K1).
pub const SBIN_PATH: &str = "PATH=\"$PATH:/usr/sbin:/sbin:/usr/bin:/bin\"; export PATH; ";

/// `cmd` run with `SBIN_PATH` set. Exported, so a detached job's shell inherits it.
pub fn with_sbin_path(cmd: &str) -> String {
    format!("{SBIN_PATH}{cmd}")
}

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

/// Where this computer's SSH keys are: the agent's socket (on Windows, its named pipe) and the
/// key files to try, in order.
#[derive(Debug, Clone, Default)]
pub struct KeySources {
    pub agent: Option<PathBuf>,
    pub key_files: Vec<PathBuf>,
}

impl KeySources {
    /// `id_ed25519`, `id_ecdsa` and `id_rsa` in `dir`, the order OpenSSH tries them in.
    pub fn default_files(dir: &Path) -> Vec<PathBuf> {
        KEY_FILE_NAMES.iter().map(|n| dir.join(n)).collect()
    }

    /// The agent the user's session names and the default key files in their `~/.ssh`.
    pub fn from_env() -> KeySources {
        KeySources {
            agent: default_agent(),
            key_files: home_dir()
                .map(|h| Self::default_files(&h.join(".ssh")))
                .unwrap_or_default(),
        }
    }
}

#[cfg(unix)]
fn default_agent() -> Option<PathBuf> {
    std::env::var_os("SSH_AUTH_SOCK")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The Windows OpenSSH agent service listens on this pipe.
#[cfg(windows)]
fn default_agent() -> Option<PathBuf> {
    Some(PathBuf::from(r"\\.\pipe\openssh-ssh-agent"))
}

#[cfg(not(any(unix, windows)))]
fn default_agent() -> Option<PathBuf> {
    None
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

#[cfg(unix)]
async fn open_agent(path: &Path) -> Option<Agent> {
    let client = AgentClient::connect_uds(path).await.ok()?;
    Some(client.dynamic())
}

#[cfg(windows)]
async fn open_agent(path: &Path) -> Option<Agent> {
    let client = AgentClient::connect_named_pipe(path).await.ok()?;
    Some(client.dynamic())
}

#[cfg(not(any(unix, windows)))]
async fn open_agent(_path: &Path) -> Option<Agent> {
    None
}

/// The hash an RSA signature uses, from the server's `server-sig-algs` (`None` when it sent
/// none). A server that names no SHA-2 algorithm, such as dropbear 2019.78 on a K1, gets plain
/// `ssh-rsa` (SHA-1).
pub fn rsa_signature_hash(server_sig_algs: Option<Option<HashAlg>>) -> Option<HashAlg> {
    server_sig_algs.flatten()
}

/// The private key in `path`: as stored, or decrypted with `passphrase` when it is
/// passphrase-protected. `None` for a missing, unreadable or still-locked file.
fn load_key(path: &Path, passphrase: Option<&str>) -> Option<PrivateKey> {
    let text = std::fs::read_to_string(path).ok()?;
    match decode_secret_key(&text, None) {
        Ok(key) => Some(key),
        Err(russh::keys::Error::KeyIsEncrypted) => decode_secret_key(&text, Some(passphrase?)).ok(),
        Err(_) => None,
    }
}

/// How the key attempts of one connection ended.
enum KeyAuth {
    Accepted,
    Refused,
    /// The server hung up during them, as one that counts refusals does.
    Disconnected,
}

/// The RSA hash for this connection, asked of the server once and only if an RSA key is offered.
struct RsaHash(Option<Option<Option<HashAlg>>>);

impl RsaHash {
    async fn for_key(
        &mut self,
        handle: &client::Handle<HostKeyCheck>,
        key: &PublicKey,
    ) -> Option<HashAlg> {
        if !key.algorithm().is_rsa() {
            return None;
        }
        if self.0.is_none() {
            self.0 = Some(handle.best_supported_rsa_hash().await.ok().flatten());
        }
        rsa_signature_hash(self.0.flatten())
    }
}

/// Offers the agent's keys, then `files`' keys it has not already offered, at most
/// `MAX_KEY_OFFERS` in all. A server that does not take publickey is never offered one.
async fn authenticate_with_keys(
    handle: &mut client::Handle<HostKeyCheck>,
    user: &str,
    agent: Option<&Path>,
    files: &[PrivateKey],
) -> KeyAuth {
    let methods = match handle.authenticate_none(user).await {
        Ok(AuthResult::Success) => return KeyAuth::Accepted,
        Ok(AuthResult::Failure {
            remaining_methods, ..
        }) => remaining_methods,
        Err(_) => return KeyAuth::Disconnected,
    };
    if !methods.contains(&MethodKind::PublicKey) {
        return KeyAuth::Refused;
    }
    let mut rsa = RsaHash(None);
    let mut offered: Vec<PublicKey> = Vec::new();
    let agent = match agent {
        Some(path) => tokio::time::timeout(AGENT_TIMEOUT, open_agent(path))
            .await
            .ok()
            .flatten(),
        None => None,
    };
    if let Some(mut agent) = agent {
        let identities = agent.request_identities().await.unwrap_or_default();
        for identity in identities {
            let AgentIdentity::PublicKey { key, .. } = identity else {
                continue;
            };
            if offered.len() >= MAX_KEY_OFFERS {
                break;
            }
            offered.push(key.clone());
            let hash = rsa.for_key(handle, &key).await;
            let result = handle
                .authenticate_publickey_with(user, key, hash, &mut agent)
                .await;
            if let Ok(AuthResult::Success) = result {
                return KeyAuth::Accepted;
            }
            // An agent that cannot sign one key may still sign the next.
            if handle.is_closed() {
                return KeyAuth::Disconnected;
            }
        }
    }
    for key in files {
        let public = key.public_key();
        if offered.iter().any(|k| k.key_data() == public.key_data()) {
            continue;
        }
        if offered.len() >= MAX_KEY_OFFERS {
            break;
        }
        offered.push(public.clone());
        let hash = rsa.for_key(handle, public).await;
        let signed = PrivateKeyWithHashAlg::new(Arc::new(key.clone()), hash);
        match handle.authenticate_publickey(user, signed).await {
            Ok(AuthResult::Success) => return KeyAuth::Accepted,
            Ok(_) if !handle.is_closed() => {}
            _ => return KeyAuth::Disconnected,
        }
    }
    KeyAuth::Refused
}

/// The russh-backed `Ssh`. One `connect` is one login: this computer's keys, then the login's
/// password. Which logins to try, and in what order, is the caller's policy.
pub struct SshRussh {
    known: Arc<KnownHosts>,
    config: Arc<client::Config>,
    connect_timeout: Duration,
    start_timeout: Duration,
    keys: KeySources,
}

impl SshRussh {
    /// Where to find this computer's SSH keys; `new` reads them from the environment.
    pub fn with_keys(mut self, keys: KeySources) -> Self {
        self.keys = keys;
        self
    }

    /// `known_hosts_path` is the JSON file that pins each printer's host key. Keys come from
    /// the user's agent and `~/.ssh` (`KeySources::from_env`).
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
            start_timeout: START_TIMEOUT,
            keys: KeySources::from_env(),
        }
    }

    /// How long `spawn_detached` waits for a job to confirm it started.
    pub fn with_start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = timeout;
        self
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

/// The password; keyboard-interactive when the server offers it instead (some dropbear builds
/// offer nothing else), answering every prompt with the password.
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
        // Decrypting a passphrase-protected key runs a deliberately slow KDF: off the runtime,
        // and before the login's clock starts.
        let paths = self.keys.key_files.clone();
        let passphrase = login.passphrase.as_ref().map(|p| p.expose().to_string());
        let files = tokio::task::spawn_blocking(move || {
            paths
                .iter()
                .filter_map(|p| load_key(p, passphrase.as_deref()))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        let mut handle = self.timed_handshake(host).await?;
        let keyed = tokio::time::timeout(
            self.connect_timeout,
            authenticate_with_keys(&mut handle, &login.user, self.keys.agent.as_deref(), &files),
        )
        .await
        .map_err(|_| TransportError::Timeout)?;
        match keyed {
            KeyAuth::Accepted => return Ok(self.session(handle)),
            KeyAuth::Refused if login.password.is_empty() => return Err(TransportError::Auth),
            KeyAuth::Refused => {}
            KeyAuth::Disconnected if login.password.is_empty() => {
                return Err(TransportError::Protocol(
                    "the printer closed the connection while this computer's SSH keys were tried"
                        .into(),
                ))
            }
            KeyAuth::Disconnected => handle = self.timed_handshake(host).await?,
        }
        tokio::time::timeout(self.connect_timeout, authenticate(&mut handle, login))
            .await
            .map_err(|_| TransportError::Timeout)??;
        Ok(self.session(handle))
    }

    fn key_status(&self) -> KeyStatus {
        let agent = self
            .keys
            .agent
            .as_deref()
            .is_some_and(|a| std::fs::metadata(a).is_ok());
        let mut status = KeyStatus {
            available: agent,
            locked: false,
        };
        for path in &self.keys.key_files {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            status.available = true;
            if matches!(
                decode_secret_key(&text, None),
                Err(russh::keys::Error::KeyIsEncrypted)
            ) {
                status.locked = true;
            }
        }
        status
    }
}

impl SshRussh {
    async fn timed_handshake(&self, host: &Host) -> Result<client::Handle<HostKeyCheck>> {
        tokio::time::timeout(self.connect_timeout, self.handshake(host))
            .await
            .map_err(|_| TransportError::Timeout)?
    }

    fn session(&self, handle: client::Handle<HostKeyCheck>) -> Box<dyn SshSession> {
        Box::new(RusshSession {
            handle,
            start_timeout: self.start_timeout,
        })
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
async fn collect(channel: &mut ChannelReadHalf) -> Result<RawOutput> {
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

/// Sends `stdin` (then EOF) while reading the command's output, so a command that fails
/// before reading its input ends the exchange instead of leaving the sender waiting on a
/// window that will never open again.
async fn exchange<R>(
    channel: Channel<client::Msg>,
    stdin: Option<R>,
) -> (Result<RawOutput>, Option<Result<()>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let (mut read, write) = channel.split();
    let send = async {
        if let Some(input) = stdin {
            write.data(input).await.map_err(russh_err)?;
        }
        write.eof().await.map_err(russh_err)
    };
    let recv = collect(&mut read);
    tokio::pin!(send);
    tokio::pin!(recv);
    let mut sent = None;
    loop {
        tokio::select! {
            r = &mut send, if sent.is_none() => sent = Some(r),
            out = &mut recv => return (out, sent),
        }
    }
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

/// How a job is cut loose from the SSH session, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detach {
    /// New session: no terminal or session teardown can signal it.
    Setsid,
    /// Ignores SIGHUP from the session ending.
    Nohup,
    /// A plain background job whose script ignores SIGHUP itself.
    Background,
}

/// The shell line that starts `cmd` detached with its output in `log`. The script's first
/// act is the `HELIX-STARTED <pid>` line that confirms the launch. `cmd` runs in a
/// subshell on its own lines, so an `exit` inside it or a trailing `# comment` still lets
/// the exit marker run; the marker starts with a newline so it begins a line even when
/// `cmd`'s output does not end with one.
fn launcher(cmd: &str, log: &str, detach: Detach) -> String {
    let trap = if detach == Detach::Background {
        "trap '' HUP\n"
    } else {
        ""
    };
    let script = format!(
        "{trap}echo {STARTED_MARKER}$$\n(\n{cmd}\n)\nprintf '\\n{EXIT_MARKER}%s\\n' \"$?\""
    );
    let tool = match detach {
        Detach::Setsid => "setsid ",
        Detach::Nohup => "nohup ",
        Detach::Background => "",
    };
    format!(
        "{tool}sh -c {} > {} 2>&1 < /dev/null &",
        sh_quote(&script),
        sh_quote(log)
    )
}

/// The pid on a complete `HELIX-STARTED <pid>` line.
fn started_pid(line: &[u8]) -> Option<u32> {
    std::str::from_utf8(line)
        .ok()?
        .trim_end_matches('\r')
        .strip_prefix(STARTED_MARKER)?
        .trim()
        .parse()
        .ok()
}

/// True when `partial` (an unfinished line) may yet turn out to be `marker`'s line.
fn could_become(partial: &[u8], marker: &str) -> bool {
    !partial.is_empty()
        && (marker.as_bytes().starts_with(partial) || partial.starts_with(marker.as_bytes()))
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
/// status once the marker line has arrived.
///
/// At the start of the log the `HELIX-STARTED` line is consumed unseen. The exit marker
/// line is consumed with the one newline printed just before it. Left for the next read,
/// because more bytes decide what they are: a trailing newline (it may be that marker's
/// newline), an unfinished line that could become a marker, and a UTF-8 sequence cut off
/// by the read.
fn parse_job_log(bytes: &[u8], at_start: bool) -> (String, u64, Option<i32>) {
    let mut pos = 0;
    if at_start {
        match bytes.iter().position(|&b| b == b'\n') {
            Some(nl) if started_pid(&bytes[..nl]).is_some() => pos = nl + 1,
            None if could_become(bytes, STARTED_MARKER) => return (String::new(), 0, None),
            _ => {}
        }
    }
    let body = &bytes[pos..];
    let mut line_start = 0;
    while let Some(nl) = body[line_start..].iter().position(|&b| b == b'\n') {
        let line_end = line_start + nl;
        let line = String::from_utf8_lossy(&body[line_start..line_end]);
        if let Some(code) = line
            .trim_end_matches('\r')
            .strip_prefix(EXIT_MARKER)
            .and_then(|c| c.trim().parse::<i32>().ok())
        {
            let before = &body[..line_start];
            let before = before.strip_suffix(b"\n").unwrap_or(before);
            let text = String::from_utf8_lossy(before).into_owned();
            return (text, (pos + line_end + 1) as u64, Some(code));
        }
        line_start = line_end + 1;
    }
    let mut keep = if could_become(&body[line_start..], EXIT_MARKER) {
        line_start
    } else {
        body.len()
    };
    if keep > 0 && body[keep - 1] == b'\n' {
        keep -= 1;
    }
    if let Err(e) = std::str::from_utf8(&body[..keep]) {
        if e.error_len().is_none() {
            keep = e.valid_up_to();
        }
    }
    let text = String::from_utf8_lossy(&body[..keep]).into_owned();
    (text, (pos + keep) as u64, None)
}

struct RusshSession {
    handle: client::Handle<HostKeyCheck>,
    start_timeout: Duration,
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
        let channel = self
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
        channel
            .exec(true, with_sbin_path(cmd))
            .await
            .map_err(russh_err)?;
        let (out, sent) = exchange(channel, stdin).await;
        let out = out?;
        if let Some(Err(e)) = sent {
            if out.exit == 0 {
                return Err(e);
            }
        }
        Ok(out)
    }

    async fn has_tool(&mut self, tool: &str) -> Result<bool> {
        let out = self
            .run(&format!("command -v {tool}"), None, SHORT_TIMEOUT, false)
            .await?;
        Ok(out.exit == 0 && !out.stdout.trim_ascii().is_empty())
    }

    async fn detach_tool(&mut self) -> Result<Detach> {
        if self.has_tool("setsid").await? {
            Ok(Detach::Setsid)
        } else if self.has_tool("nohup").await? {
            Ok(Detach::Nohup)
        } else {
            Ok(Detach::Background)
        }
    }

    async fn launch_with_sudo(&mut self, launcher: &str, password: &str) -> Result<RawOutput> {
        // sudo resets PATH to its secure_path, so the line it runs sets it again.
        let line = format!(
            "sudo -S -p '' sh -c {}",
            sh_quote(&with_sbin_path(launcher))
        );
        let stdin = format!("{password}\n");
        let first = self
            .run(&line, Some(stdin.as_bytes()), SHORT_TIMEOUT, false)
            .await?;
        if first.exit != 0 {
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&first.stdout),
                String::from_utf8_lossy(&first.stderr)
            );
            if sudo_wants_tty(&said) {
                return self
                    .run(&line, Some(stdin.as_bytes()), SHORT_TIMEOUT, true)
                    .await;
            }
        }
        Ok(first)
    }

    async fn read_log(&mut self, log: &str, from_offset: u64) -> Result<RawOutput> {
        let cmd = format!(
            "tail -c +{} {}",
            from_offset.saturating_add(1),
            sh_quote(log)
        );
        self.run(&cmd, None, SHORT_TIMEOUT, false).await
    }

    /// Waits for the job's `HELIX-STARTED` line and returns its pid.
    async fn wait_started(&mut self, log: &str) -> Result<u32> {
        let deadline = tokio::time::Instant::now() + self.start_timeout;
        let mut first_line = None;
        loop {
            let out = self.read_log(log, 0).await?;
            if out.exit == 0 {
                let line = out.stdout.split(|&b| b == b'\n').next().unwrap_or(&[]);
                let complete = out.stdout.contains(&b'\n');
                if complete {
                    if let Some(pid) = started_pid(line) {
                        return Ok(pid);
                    }
                }
                let text = String::from_utf8_lossy(line).trim().to_string();
                if !text.is_empty() {
                    first_line = Some(text);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(START_POLL).await;
        }
        Err(TransportError::Protocol(format!(
            "job did not start: {}",
            first_line.as_deref().unwrap_or("no log")
        )))
    }

    async fn is_alive(&mut self, pid: u32) -> Result<bool> {
        // `kill -0` alone reads a root job as dead to a non-root user (EPERM), so a
        // /proc entry also counts.
        let cmd = format!("kill -0 {pid} 2>/dev/null || [ -d /proc/{pid} ]");
        Ok(self.run(&cmd, None, SHORT_TIMEOUT, false).await?.exit == 0)
    }

    async fn read_chunk(&mut self, job: &RemoteJob, from_offset: u64) -> Result<JobChunk> {
        let out = self.read_log(&job.log_path, from_offset).await?;
        if out.exit != 0 {
            return Err(TransportError::Protocol(format!(
                "cannot read {}: {}",
                job.log_path,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let (text, consumed, exit) = parse_job_log(&out.stdout, from_offset == 0);
        Ok(JobChunk {
            text,
            next_offset: from_offset + consumed,
            exit,
            alive: None,
        })
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
            "cat > {p} && mv {p} {r} || {{ rm -f {p} ; false ; }}",
            p = sh_quote(&part),
            r = sh_quote(remote)
        );
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(russh_err)?;
        channel
            .exec(true, with_sbin_path(&cmd))
            .await
            .map_err(russh_err)?;
        let (out, sent) = exchange(channel, Some(file)).await;
        let failure = match out {
            Ok(out) if out.exit != 0 => Some(TransportError::Protocol(format!(
                "upload to {remote} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
            Ok(_) => match sent {
                Some(Err(e)) => Some(e),
                _ => None,
            },
            Err(e) => Some(e),
        };
        if let Some(e) = failure {
            // The command's own cleanup cannot run if the transfer broke off mid-stream.
            let _ = self
                .run(
                    &format!("rm -f {}", sh_quote(&part)),
                    None,
                    SHORT_TIMEOUT,
                    false,
                )
                .await;
            return Err(e);
        }

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
        let detach = self.detach_tool().await?;
        let log = job_log_path();
        let line = launcher(cmd, &log, detach);
        let out = match sudo_password {
            None => self.run(&line, None, SHORT_TIMEOUT, false).await?,
            Some(pw) => self.launch_with_sudo(&line, pw).await?,
        };
        if out.exit != 0 {
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
            return Err(TransportError::Protocol(format!(
                "the job did not start (exit {}): {}",
                out.exit,
                said.trim()
            )));
        }
        let pid = self.wait_started(&log).await?;
        Ok(RemoteJob {
            log_path: log,
            pid: Some(pid),
        })
    }

    async fn read_job(&mut self, job: &RemoteJob, from_offset: u64) -> Result<JobChunk> {
        let chunk = self.read_chunk(job, from_offset).await?;
        let Some(pid) = job.pid.filter(|_| chunk.exit.is_none()) else {
            return Ok(chunk);
        };
        if self.is_alive(pid).await? {
            return Ok(JobChunk {
                alive: Some(true),
                ..chunk
            });
        }
        // The job may have written its marker and exited between the two reads.
        let chunk = self.read_chunk(job, from_offset).await?;
        Ok(JobChunk {
            alive: Some(false),
            ..chunk
        })
    }
}
