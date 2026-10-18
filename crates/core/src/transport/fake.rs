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
        lock(&self.state)
            .gcodes
            .push((host.addr.clone(), script.into()));
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
/// A job's (text, exit) chunks and whether it dies once they are shown.
type QueuedJob = (Vec<(String, Option<i32>)>, bool);

#[derive(Default)]
struct SshState {
    accepted: Vec<(String, String, String)>,
    table: HashMap<String, ExecOutput>,
    fallback: Option<Fallback>,
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
    pub fn on_command(&self, cmd: &str, out: ExecOutput) {
        lock(&self.state).table.insert(cmd.into(), out);
    }
    /// Answers any command missing from the table; without one it exits 127.
    pub fn set_fallback(&self, f: impl Fn(&str) -> ExecOutput + Send + Sync + 'static) {
        lock(&self.state).fallback = Some(Box::new(f));
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
    /// `spawn_detached` with a sudo password other than `password` fails with `Auth`, as
    /// sudo does after a wrong password. Without this any password is taken.
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
        if !st.accepted.iter().any(|(a, _, _)| *a == host.addr) {
            return Err(TransportError::Unreachable);
        }
        if !st
            .accepted
            .iter()
            .any(|(a, u, p)| *a == host.addr && *u == login.user && *p == login.password)
        {
            return Err(TransportError::Auth);
        }
        let drop_at = st.drop_next_session_after.take();
        Ok(Box::new(FakeSshSession {
            state: self.state.clone(),
            calls: 0,
            drop_at,
        }))
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
