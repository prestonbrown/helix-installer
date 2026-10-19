// SPDX-License-Identifier: GPL-3.0-or-later

//! Scriptable in-memory transports. Each fake is cheap to clone-share via
//! `Arc` and records what it was asked so tests can assert on it afterwards.

use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct MoonrakerState {
    printer_info: HashMap<String, Value>,
    system_info: HashMap<String, Value>,
    update_status: HashMap<String, Value>,
    objects_list: HashMap<String, Value>,
    gcodes: Vec<(String, String)>,
    upgrades: Vec<String>,
    /// Answers `printer_info` gives in turn, the last one repeated; `None` is unreachable.
    printer_info_script: HashMap<String, std::collections::VecDeque<Option<Value>>>,
    printer_info_calls: HashMap<String, u32>,
    /// `printer_info` calls left that find the host restarting (unreachable), per address.
    down_for: HashMap<String, u32>,
    /// A G-code script that restarts the printer, and for how many `printer_info` calls.
    restart_gcode: Option<(String, u32)>,
}

/// Canned JSON per host address; a host with nothing registered is unreachable.
#[derive(Default)]
pub struct FakeMoonraker {
    state: Mutex<MoonrakerState>,
}

impl FakeMoonraker {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_printer_info(&self, addr: &str, v: Value) {
        lock(&self.state).printer_info.insert(addr.into(), v);
    }
    /// `printer_info` answers these in turn, repeating the last (`None`: unreachable). Wins
    /// over `set_printer_info`.
    pub fn script_printer_info(&self, addr: &str, answers: Vec<Option<Value>>) {
        lock(&self.state)
            .printer_info_script
            .insert(addr.into(), answers.into());
    }
    /// The printer restarts: its next `calls` `printer_info` calls find it unreachable.
    pub fn restart(&self, addr: &str, calls: u32) {
        lock(&self.state).down_for.insert(addr.into(), calls);
    }
    /// Sending `script` restarts the printer it went to, as `restart` does.
    pub fn restart_on_gcode(&self, script: &str, calls: u32) {
        lock(&self.state).restart_gcode = Some((script.into(), calls));
    }
    pub fn printer_info_calls(&self, addr: &str) -> u32 {
        lock(&self.state)
            .printer_info_calls
            .get(addr)
            .copied()
            .unwrap_or(0)
    }
    pub fn set_system_info(&self, addr: &str, v: Value) {
        lock(&self.state).system_info.insert(addr.into(), v);
    }
    pub fn set_update_status(&self, addr: &str, v: Value) {
        lock(&self.state).update_status.insert(addr.into(), v);
    }
    pub fn set_objects_list(&self, addr: &str, v: Value) {
        lock(&self.state).objects_list.insert(addr.into(), v);
    }
    /// (host addr, script) for every `gcode` call, in order.
    pub fn gcodes(&self) -> Vec<(String, String)> {
        lock(&self.state).gcodes.clone()
    }
    pub fn upgrades(&self) -> Vec<String> {
        lock(&self.state).upgrades.clone()
    }
}

#[async_trait]
impl Moonraker for FakeMoonraker {
    async fn printer_info(&self, host: &Host) -> Result<Value> {
        let mut st = lock(&self.state);
        *st.printer_info_calls.entry(host.addr.clone()).or_default() += 1;
        if let Some(n) = st.down_for.get_mut(&host.addr).filter(|n| **n > 0) {
            *n -= 1;
            return Err(TransportError::Unreachable);
        }
        if let Some(q) = st.printer_info_script.get_mut(&host.addr) {
            let answer = if q.len() > 1 {
                q.pop_front().flatten()
            } else {
                q.front().cloned().flatten()
            };
            return answer.ok_or(TransportError::Unreachable);
        }
        st.printer_info
            .get(&host.addr)
            .cloned()
            .ok_or(TransportError::Unreachable)
    }
    async fn system_info(&self, host: &Host) -> Result<Value> {
        lock(&self.state)
            .system_info
            .get(&host.addr)
            .cloned()
            .ok_or(TransportError::Unreachable)
    }
    async fn update_status(&self, host: &Host) -> Result<Value> {
        lock(&self.state)
            .update_status
            .get(&host.addr)
            .cloned()
            .ok_or(TransportError::Unreachable)
    }
    async fn objects_list(&self, host: &Host) -> Result<Value> {
        lock(&self.state)
            .objects_list
            .get(&host.addr)
            .cloned()
            .ok_or(TransportError::Unreachable)
    }
    async fn gcode(&self, host: &Host, script: &str) -> Result<()> {
        let mut st = lock(&self.state);
        st.gcodes.push((host.addr.clone(), script.into()));
        if let Some((_, calls)) = st.restart_gcode.clone().filter(|(s, _)| s == script) {
            st.down_for.insert(host.addr.clone(), calls);
        }
        Ok(())
    }
    async fn update_upgrade(&self, _host: &Host, name: &str) -> Result<()> {
        lock(&self.state).upgrades.push(name.into());
        Ok(())
    }
}

struct JobScript {
    chunks: Vec<(String, Option<i32>)>,
    revealed: usize,
    /// The job's process is gone once every chunk is shown, without having exited.
    dies: bool,
}

type Fallback = Box<dyn Fn(&str) -> ExecOutput + Send + Sync>;
type SpawnHook = std::sync::Arc<dyn Fn(&str) + Send + Sync>;
/// A job's (text, exit) chunks and whether it dies once they are shown.
type QueuedJob = (Vec<(String, Option<i32>)>, bool);

#[derive(Default)]
struct SshState {
    accepted: Vec<(String, String, String)>,
    /// (addr, user, passphrase the key needs) for logins this computer's key opens.
    keys: Vec<(String, String, Option<String>)>,
    key_status: KeyStatus,
    table: HashMap<String, ExecOutput>,
    fallback: Option<Fallback>,
    on_spawn: Option<SpawnHook>,
    calls: Vec<(String, Option<Vec<u8>>)>,
    uploads: Vec<(String, Vec<u8>)>,
    queued_jobs: Vec<QueuedJob>,
    jobs: HashMap<String, JobScript>,
    next_job: u32,
    drop_next_session_after: Option<u32>,
    connects: u32,
    sudo_password: Option<String>,
    job_pid: Option<u32>,
}

/// Shared by every session it hands out, so a reconnect sees the same job logs.
#[derive(Clone, Default)]
pub struct FakeSsh {
    state: std::sync::Arc<Mutex<SshState>>,
}

impl FakeSsh {
    pub fn new() -> Self {
        Self::default()
    }
    /// Registers a login the host accepts; hosts with no registration are unreachable.
    pub fn accept(&self, addr: &str, user: &str, password: &str) {
        lock(&self.state)
            .accepted
            .push((addr.into(), user.into(), password.into()));
    }
    /// The host accepts this computer's key for `user`, unlocked with `passphrase` if given.
    pub fn accept_key(&self, addr: &str, user: &str, passphrase: Option<&str>) {
        lock(&self.state)
            .keys
            .push((addr.into(), user.into(), passphrase.map(Into::into)));
    }
    pub fn set_key_status(&self, status: KeyStatus) {
        lock(&self.state).key_status = status;
    }
    pub fn on_command(&self, cmd: &str, out: ExecOutput) {
        lock(&self.state).table.insert(cmd.into(), out);
    }
    /// Answers any command missing from the table; without one it exits 127.
    pub fn set_fallback(&self, f: impl Fn(&str) -> ExecOutput + Send + Sync + 'static) {
        lock(&self.state).fallback = Some(Box::new(f));
    }
    /// Called with the command of every `spawn_detached`, after it is recorded: how a test
    /// makes a job act on other fakes (a reboot taking Moonraker down).
    pub fn on_spawn(&self, f: impl Fn(&str) + Send + Sync + 'static) {
        lock(&self.state).on_spawn = Some(std::sync::Arc::new(f));
    }
    /// Queues the log for the next `spawn_detached`: (text, exit) chunks,
    /// one revealed per `read_job` call; `Some` exit marks the job finished.
    pub fn script_job(&self, chunks: Vec<(&str, Option<i32>)>) {
        self.queue_job(chunks, false);
    }
    /// Like `script_job`, but once every chunk is shown the job reports `alive: Some(false)`
    /// with no exit status: its process died without finishing.
    pub fn script_dying_job(&self, chunks: Vec<(&str, Option<i32>)>) {
        self.queue_job(chunks, true);
    }
    fn queue_job(&self, chunks: Vec<(&str, Option<i32>)>, dies: bool) {
        let chunks = chunks
            .into_iter()
            .map(|(t, e)| (t.to_string(), e))
            .collect();
        lock(&self.state).queued_jobs.push((chunks, dies));
    }
    /// `spawn_detached` with a sudo password other than `password` fails with `Auth`, and an
    /// `exec` of `sudo -S` fed another one on stdin exits 1, as sudo does after a wrong
    /// password. Without this any password is taken.
    pub fn sudo_accepts(&self, password: &str) {
        lock(&self.state).sudo_password = Some(password.into());
    }
    /// Jobs started from now on report `pid` (by default the pid is unknown).
    pub fn job_pid(&self, pid: u32) {
        lock(&self.state).job_pid = Some(pid);
    }
    /// The next session to connect fails every call from its n-th (1-based)
    /// onward with `Unreachable`; later sessions are unaffected.
    pub fn drop_after(&self, n: u32) {
        lock(&self.state).drop_next_session_after = Some(n);
    }
    /// Every command passed to `exec` or `spawn_detached`.
    pub fn commands(&self) -> Vec<String> {
        lock(&self.state)
            .calls
            .iter()
            .map(|(c, _)| c.clone())
            .collect()
    }
    /// How many times `connect` was called, accepted or not.
    pub fn connect_count(&self) -> u32 {
        lock(&self.state).connects
    }
    /// Every stdin payload, kept apart from the command lines.
    pub fn stdins(&self) -> Vec<Vec<u8>> {
        lock(&self.state)
            .calls
            .iter()
            .filter_map(|(_, s)| s.clone())
            .collect()
    }
    /// (command, stdin) per `exec`/`spawn_detached`, so a test can prove
    /// which command received which stdin.
    pub fn calls(&self) -> Vec<(String, Option<Vec<u8>>)> {
        lock(&self.state).calls.clone()
    }
    /// (remote path, bytes) for every upload.
    pub fn uploads(&self) -> Vec<(String, Vec<u8>)> {
        lock(&self.state).uploads.clone()
    }
}

#[async_trait]
impl Ssh for FakeSsh {
    async fn connect(&self, host: &Host, login: &Login) -> Result<Box<dyn SshSession>> {
        let mut st = lock(&self.state);
        st.connects += 1;
        if !st.accepted.iter().any(|(a, _, _)| *a == host.addr)
            && !st.keys.iter().any(|(a, _, _)| *a == host.addr)
        {
            return Err(TransportError::Unreachable);
        }
        let by_password = st
            .accepted
            .iter()
            .any(|(a, u, p)| *a == host.addr && *u == login.user && *p == login.password);
        let given = login.passphrase.as_ref().map(|p| p.expose());
        let by_key = st.keys.iter().any(|(a, u, needs)| {
            *a == host.addr && *u == login.user && needs.as_deref().is_none_or(|n| given == Some(n))
        });
        if !by_password && !by_key {
            return Err(TransportError::Auth);
        }
        let drop_at = st.drop_next_session_after.take();
        Ok(Box::new(FakeSshSession {
            state: self.state.clone(),
            calls: 0,
            drop_at,
        }))
    }

    fn key_status(&self) -> KeyStatus {
        lock(&self.state).key_status
    }
}

struct FakeSshSession {
    state: std::sync::Arc<Mutex<SshState>>,
    calls: u32,
    drop_at: Option<u32>,
}

impl FakeSshSession {
    fn tick(&mut self) -> Result<()> {
        self.calls += 1;
        match self.drop_at {
            Some(n) if self.calls >= n => Err(TransportError::Unreachable),
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl SshSession for FakeSshSession {
    async fn exec(
        &mut self,
        cmd: &str,
        stdin: Option<&[u8]>,
        _timeout: Duration,
    ) -> Result<ExecOutput> {
        self.tick()?;
        let mut st = lock(&self.state);
        st.calls.push((cmd.into(), stdin.map(|b| b.to_vec())));
        // `sudo -S` reads the password from stdin: one `sudo_accepts` does not take is refused.
        if let Some(inner) = cmd.strip_prefix("sudo -S -p '' ") {
            let typed = stdin
                .map(|b| {
                    String::from_utf8_lossy(b)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            if st
                .sudo_password
                .as_deref()
                .is_some_and(|want| want != typed)
            {
                return Ok(ExecOutput {
                    stdout: String::new(),
                    stderr: "Sorry, try again.\nsudo: 1 incorrect password attempt\n".into(),
                    exit: 1,
                });
            }
            if inner == "true" {
                return Ok(ExecOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit: 0,
                });
            }
        }
        if let Some(o) = st.table.get(cmd) {
            return Ok(o.clone());
        }
        if let Some(f) = &st.fallback {
            return Ok(f(cmd));
        }
        Ok(ExecOutput {
            stdout: String::new(),
            stderr: format!("{cmd}: command not found"),
            exit: 127,
        })
    }

    async fn upload(&mut self, local: &Path, remote: &str) -> Result<()> {
        self.tick()?;
        let bytes = std::fs::read(local).map_err(|e| TransportError::Io(e.to_string()))?;
        lock(&self.state).uploads.push((remote.into(), bytes));
        Ok(())
    }

    async fn spawn_detached(
        &mut self,
        cmd: &str,
        sudo_password: Option<&str>,
    ) -> Result<RemoteJob> {
        self.tick()?;
        let mut st = lock(&self.state);
        st.calls
            .push((cmd.into(), sudo_password.map(|p| p.as_bytes().to_vec())));
        if let (Some(given), Some(want)) = (sudo_password, st.sudo_password.as_deref()) {
            if given != want {
                return Err(TransportError::Auth);
            }
        }
        if st.queued_jobs.is_empty() {
            return Err(TransportError::Protocol(format!("unscripted job: {cmd}")));
        }
        let (chunks, dies) = st.queued_jobs.remove(0);
        if let Some(hook) = st.on_spawn.clone() {
            drop(st);
            hook(cmd);
            st = lock(&self.state);
        }
        st.next_job += 1;
        let log_path = format!("/tmp/fake-job-{}.log", st.next_job);
        st.jobs.insert(
            log_path.clone(),
            JobScript {
                chunks,
                revealed: 0,
                dies,
            },
        );
        Ok(RemoteJob {
            log_path,
            pid: st.job_pid,
        })
    }

    async fn read_job(&mut self, job: &RemoteJob, from_offset: u64) -> Result<JobChunk> {
        self.tick()?;
        let mut st = lock(&self.state);
        let script = st
            .jobs
            .get_mut(&job.log_path)
            .ok_or_else(|| TransportError::Io(format!("no such log {}", job.log_path)))?;
        if script.revealed < script.chunks.len() {
            script.revealed += 1;
        }
        let shown = &script.chunks[..script.revealed];
        let full: String = shown.iter().map(|(t, _)| t.as_str()).collect();
        let text = full
            .get(from_offset as usize..)
            .ok_or_else(|| TransportError::Protocol(format!("bad log offset {from_offset}")))?
            .to_string();
        let ended = script.revealed == script.chunks.len();
        let exit = if ended {
            script.chunks.last().and_then(|(_, e)| *e)
        } else {
            None
        };
        let alive = (script.dies && exit.is_none()).then_some(!ended);
        Ok(JobChunk {
            text,
            next_offset: full.len() as u64,
            exit,
            alive,
        })
    }
}

#[derive(Default)]
struct DrivesState {
    drives: Vec<Drive>,
    files: HashMap<(String, String), Vec<u8>>,
    ejected: Vec<String>,
    corrupt_copies: bool,
    fail_eject: bool,
    system_unknown: bool,
}

#[derive(Default)]
pub struct FakeDrives {
    state: Mutex<DrivesState>,
}

impl FakeDrives {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&self, d: Drive) {
        lock(&self.state).drives.push(d);
    }
    /// Bytes written to `name` on drive `id` by `copy_to`.
    pub fn file(&self, id: &str, name: &str) -> Option<Vec<u8>> {
        lock(&self.state)
            .files
            .get(&(id.to_string(), name.to_string()))
            .cloned()
    }
    pub fn ejected(&self) -> Vec<String> {
        lock(&self.state).ejected.clone()
    }
    /// Unplugs drive `id`; files written to it stay readable through `file`.
    pub fn remove(&self, id: &str) {
        lock(&self.state).drives.retain(|d| d.id != id);
    }
    /// `system_drives_known` answers false, as when the OS root or home cannot be found.
    pub fn system_unknown(&self) {
        lock(&self.state).system_unknown = true;
    }
    /// Every later copy lands with its last byte changed, as a failing stick would leave it.
    pub fn corrupt_copies(&self) {
        lock(&self.state).corrupt_copies = true;
    }
    /// Every later `eject` fails, as for a stick another program holds open.
    pub fn fail_eject(&self) {
        lock(&self.state).fail_eject = true;
    }
    fn known(&self, d: &Drive) -> Result<()> {
        if lock(&self.state).drives.iter().any(|x| x.id == d.id) {
            Ok(())
        } else {
            Err(TransportError::Io(format!("unknown drive {}", d.id)))
        }
    }
}

#[async_trait]
impl Drives for FakeDrives {
    fn list_drives(&self) -> Result<Vec<Drive>> {
        Ok(lock(&self.state).drives.to_vec())
    }
    fn system_drives_known(&self) -> bool {
        !lock(&self.state).system_unknown
    }
    async fn copy_to(&self, drive: &Drive, src: &Path, name: &str) -> Result<()> {
        self.known(drive)?;
        let mut bytes = std::fs::read(src).map_err(|e| TransportError::Io(e.to_string()))?;
        let mut st = lock(&self.state);
        if st.corrupt_copies {
            if let Some(b) = bytes.last_mut() {
                *b ^= 0xff;
            }
        }
        st.files.insert((drive.id.clone(), name.into()), bytes);
        Ok(())
    }
    async fn sha256_of(&self, drive: &Drive, name: &str) -> Result<String> {
        self.known(drive)?;
        self.file(&drive.id, name)
            .map(|b| crate::cache::sha256_hex(&b))
            .ok_or_else(|| TransportError::Io(format!("no file {name} on {}", drive.id)))
    }
    async fn eject(&self, drive: &Drive) -> Result<()> {
        self.known(drive)?;
        let mut st = lock(&self.state);
        if st.fail_eject {
            return Err(TransportError::Io("the drive is busy".into()));
        }
        st.ejected.push(drive.id.clone());
        Ok(())
    }
}

#[derive(Default)]
pub struct FakeFetch {
    bodies: Mutex<HashMap<String, Vec<u8>>>,
    requests: Mutex<Vec<String>>,
}

impl FakeFetch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&self, url: &str, bytes: Vec<u8>) {
        lock(&self.bodies).insert(url.into(), bytes);
    }
    /// Every URL requested, in order, including misses.
    pub fn requests(&self) -> Vec<String> {
        lock(&self.requests).clone()
    }
    fn body(&self, url: &str) -> Result<Vec<u8>> {
        lock(&self.requests).push(url.into());
        lock(&self.bodies)
            .get(url)
            .cloned()
            .ok_or(TransportError::Http(404))
    }
}

#[async_trait]
impl Fetch for FakeFetch {
    async fn get(
        &self,
        url: &str,
        dest: &Path,
        progress: &(dyn Fn(u64, Option<u64>) + Sync),
    ) -> Result<()> {
        let bytes = self.body(url)?;
        std::fs::write(dest, &bytes).map_err(|e| TransportError::Io(e.to_string()))?;
        let n = bytes.len() as u64;
        progress(n, Some(n));
        Ok(())
    }
    async fn get_text(&self, url: &str) -> Result<String> {
        String::from_utf8(self.body(url)?).map_err(|e| TransportError::Protocol(e.to_string()))
    }
}

/// Sleeping advances the clock and returns at once.
pub struct FakeClock {
    now: Mutex<SystemTime>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
    }
    pub fn at(t: SystemTime) -> Self {
        FakeClock { now: Mutex::new(t) }
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        *lock(&self.now)
    }
    async fn sleep(&self, d: Duration) {
        *lock(&self.now) += d;
    }
}

/// Scripted TCP answers per (host, port); anything unscripted is unreachable.
#[derive(Default)]
pub struct FakeNet {
    answers: Mutex<HashMap<(String, u16), Option<String>>>,
}

impl FakeNet {
    pub fn new() -> Self {
        Self::default()
    }
    /// The port accepts connections and sends `banner` as its first line (`None`: silent).
    pub fn answer(&self, addr: &str, port: u16, banner: Option<&str>) {
        lock(&self.answers).insert((addr.into(), port), banner.map(str::to_string));
    }
}

#[async_trait]
impl Net for FakeNet {
    async fn tcp_banner(
        &self,
        host: &Host,
        port: u16,
        _timeout: Duration,
    ) -> Result<Option<String>> {
        lock(&self.answers)
            .get(&(host.addr.clone(), port))
            .cloned()
            .ok_or(TransportError::Unreachable)
    }
}

struct Board {
    chip: ChipInfo,
    /// Writes in order; a later one wins where they overlap. Unwritten flash reads 0xFF.
    flash: Vec<(u32, Vec<u8>)>,
}

impl Board {
    fn read(&self, offset: u32, len: u32) -> Vec<u8> {
        let (start, end) = (offset as u64, offset as u64 + len as u64);
        let mut out = vec![0xFF; len as usize];
        for (at, bytes) in &self.flash {
            let (a, b) = (*at as u64, *at as u64 + bytes.len() as u64);
            let (lo, hi) = (start.max(a), end.min(b));
            if lo < hi {
                out[(lo - start) as usize..(hi - start) as usize]
                    .copy_from_slice(&bytes[(lo - a) as usize..(hi - a) as usize]);
            }
        }
        out
    }
}

#[derive(Default)]
struct SerialState {
    ports: Vec<UsbPort>,
    boards: HashMap<String, Board>,
    denied: HashMap<String, String>,
    connects: Vec<String>,
    writes: Vec<(String, u32, usize)>,
    resets: u32,
    erases: u32,
    vanish_on_erase: bool,
    vanish_on_write: Option<u32>,
    corrupt_writes: bool,
    silent_boot: bool,
    hold: Option<std::sync::Arc<tokio::sync::Notify>>,
}

/// USB serial ports with scriptable ESP32 boards behind them. Shared by every session it hands
/// out, so what one session writes the next one reads.
#[derive(Clone, Default)]
pub struct FakeSerial {
    state: std::sync::Arc<Mutex<SerialState>>,
}

/// The line an ESP-IDF app prints at boot naming its version.
pub const FAKE_BOOT_VERSION_LINE: &str = "I (312) app_init: App version:      ";

impl FakeSerial {
    pub fn new() -> Self {
        Self::default()
    }
    /// A board with `chip` behind a port with this USB id.
    pub fn plug(&self, port: &str, vid: u16, pid: u16, chip: ChipInfo) {
        let mut st = lock(&self.state);
        st.ports.retain(|p| p.name != port);
        st.ports.push(UsbPort {
            name: port.into(),
            vid,
            pid,
        });
        st.boards.insert(
            port.into(),
            Board {
                chip,
                flash: Vec::new(),
            },
        );
    }
    /// A port whose device never answers as an ESP32 bootloader.
    pub fn plug_other(&self, port: &str, vid: u16, pid: u16) {
        let mut st = lock(&self.state);
        st.ports.push(UsbPort {
            name: port.into(),
            vid,
            pid,
        });
    }
    pub fn unplug(&self, port: &str) {
        let mut st = lock(&self.state);
        st.ports.retain(|p| p.name != port);
        st.boards.remove(port);
    }
    /// Puts `bytes` in the board's flash at `offset`, as if flashed before the test.
    pub fn preload(&self, port: &str, offset: u32, bytes: Vec<u8>) {
        if let Some(b) = lock(&self.state).boards.get_mut(port) {
            b.flash.push((offset, bytes));
        }
    }
    pub fn read_back(&self, port: &str, offset: u32, len: u32) -> Vec<u8> {
        lock(&self.state)
            .boards
            .get(port)
            .map(|b| b.read(offset, len))
            .unwrap_or_default()
    }
    /// Opening `port` is refused as the OS refuses a user outside the device's `group`.
    pub fn deny(&self, port: &str, group: &str) {
        lock(&self.state).denied.insert(port.into(), group.into());
    }
    /// The `n`-th write (1-based) from now writes half its bytes, then the board is unplugged.
    pub fn vanish_on_write(&self, n: u32) {
        lock(&self.state).vanish_on_write = Some(n);
    }
    /// Every write lands with its first byte flipped, so it never reads back as sent.
    pub fn corrupt_writes(&self) {
        lock(&self.state).corrupt_writes = true;
    }
    /// The board prints nothing after a reset.
    pub fn silent_boot(&self) {
        lock(&self.state).silent_boot = true;
    }
    /// From now on every write reports half its bytes, then waits until the returned `Notify`
    /// is notified once for it.
    pub fn hold_writes(&self) -> std::sync::Arc<tokio::sync::Notify> {
        let n = std::sync::Arc::new(tokio::sync::Notify::new());
        lock(&self.state).hold = Some(n.clone());
        n
    }
    /// Every port `connect` was asked to open, in order, opened or not.
    pub fn connects(&self) -> Vec<String> {
        lock(&self.state).connects.clone()
    }
    /// (port, offset, length) of every write that completed or was cut off.
    pub fn writes(&self) -> Vec<(String, u32, usize)> {
        lock(&self.state).writes.clone()
    }
    pub fn resets(&self) -> u32 {
        lock(&self.state).resets
    }
    /// How many times a whole chip was erased.
    pub fn erases(&self) -> u32 {
        lock(&self.state).erases
    }
    /// The next whole-chip erase unplugs the board part way through.
    pub fn vanish_on_erase(&self) {
        lock(&self.state).vanish_on_erase = true;
    }
}

#[async_trait]
impl Serial for FakeSerial {
    fn usb_ports(&self) -> Result<Vec<UsbPort>> {
        Ok(lock(&self.state).ports.clone())
    }
    async fn connect(&self, port: &str) -> Result<Box<dyn FlashSession>> {
        let mut st = lock(&self.state);
        st.connects.push(port.into());
        if let Some(group) = st.denied.get(port) {
            return Err(TransportError::PermissionDenied(group.clone()));
        }
        if !st.ports.iter().any(|p| p.name == port) {
            return Err(TransportError::Disconnected);
        }
        let Some(board) = st.boards.get(port) else {
            return Err(TransportError::Timeout);
        };
        Ok(Box::new(FakeFlashSession {
            state: self.state.clone(),
            port: port.into(),
            chip: board.chip.clone(),
        }))
    }
}

struct FakeFlashSession {
    state: std::sync::Arc<Mutex<SerialState>>,
    port: String,
    chip: ChipInfo,
}

impl FakeFlashSession {
    fn unplug_here(&self) {
        let mut st = lock(&self.state);
        st.ports.retain(|p| p.name != self.port);
        st.boards.remove(&self.port);
    }

    fn board<T>(&self, f: impl FnOnce(&mut Board) -> T) -> Result<T> {
        lock(&self.state)
            .boards
            .get_mut(&self.port)
            .map(f)
            .ok_or(TransportError::Disconnected)
    }
}

#[async_trait]
impl FlashSession for FakeFlashSession {
    fn chip(&self) -> &ChipInfo {
        &self.chip
    }
    async fn read(&mut self, offset: u32, len: u32) -> Result<Vec<u8>> {
        self.board(|b| b.read(offset, len))
    }
    async fn write(
        &mut self,
        offset: u32,
        data: &[u8],
        progress: &(dyn Fn(u64) + Sync),
    ) -> Result<()> {
        self.board(|_| ())?;
        let half = data.len() / 2;
        let (vanish, hold, corrupt) = {
            let mut st = lock(&self.state);
            st.writes.push((self.port.clone(), offset, data.len()));
            let vanish = match st.vanish_on_write.as_mut() {
                Some(n) if *n <= 1 => {
                    st.vanish_on_write = None;
                    true
                }
                Some(n) => {
                    *n -= 1;
                    false
                }
                None => false,
            };
            (vanish, st.hold.clone(), st.corrupt_writes)
        };
        progress(half as u64);
        if vanish {
            self.board(|b| b.flash.push((offset, data[..half].to_vec())))?;
            self.unplug_here();
            return Err(TransportError::Disconnected);
        }
        if let Some(n) = hold {
            n.notified().await;
        }
        let mut bytes = data.to_vec();
        if corrupt {
            if let Some(b) = bytes.first_mut() {
                *b ^= 0xFF;
            }
        }
        self.board(|b| b.flash.push((offset, bytes)))?;
        progress(data.len() as u64);
        Ok(())
    }
    async fn erase_all(&mut self) -> Result<()> {
        let vanish = {
            let mut st = lock(&self.state);
            st.erases += 1;
            std::mem::take(&mut st.vanish_on_erase)
        };
        self.board(|b| b.flash.clear())?;
        if vanish {
            self.unplug_here();
            return Err(TransportError::Disconnected);
        }
        Ok(())
    }
    async fn md5(&mut self, offset: u32, len: u32) -> Result<[u8; 16]> {
        use md5::Digest;
        let bytes = self.board(|b| b.read(offset, len))?;
        Ok(md5::Md5::digest(&bytes).into())
    }
    async fn reset(self: Box<Self>, _until: &str, _listen: Duration) -> Result<String> {
        let app = self.board(|b| b.read(0x20000, 0x120))?;
        let mut st = lock(&self.state);
        st.resets += 1;
        if st.silent_boot {
            return Ok(String::new());
        }
        // The app's own descriptor: magic 0xABCD5432 at 0x20, its version at 0x30.
        let valid = app.get(0x20..0x24) == Some(&[0x32, 0x54, 0xcd, 0xab][..]);
        if !valid {
            return Ok("rst:0x1 (POWERON),boot:0x8 (SPI_FAST_FLASH_BOOT)\ninvalid header\n".into());
        }
        let version: String = app[0x30..0x50]
            .iter()
            .take_while(|b| **b != 0)
            .map(|b| *b as char)
            .collect();
        Ok(format!(
            "rst:0x1 (POWERON),boot:0x8 (SPI_FAST_FLASH_BOOT)\n{FAKE_BOOT_VERSION_LINE}{version}\n"
        ))
    }
}
