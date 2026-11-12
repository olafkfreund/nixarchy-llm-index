//! The `index` service: coordinator that schedules sandboxed workers and never touches a root.
//! Single-threaded (one `poll(2)` loop), so the 16-thread cap holds by construction.
pub mod sched;
pub mod shares;
pub mod statedir;
pub mod store;
pub mod validate;
pub mod watchers;

use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, LandlockStatus, PathBeneath, PathFd,
    Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_setfl};

use crate::config::{self, Config};
use crate::proto::{Chunk, Job, Message, Share, ValidationResult};
use crate::worker;
use sched::{Allow, Params, RootSpec, Sched};
use shares::{WorkerId, derive};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Scan,
    Extract,
    Watch,
    /// Resolves the root and exclusion paths of one root without following symlinks.
    Validate,
}

#[allow(clippy::large_enum_variant)] // one short-lived value per message
pub enum Event {
    Msg(Message),
    /// The process was reaped by `waitpid`.
    Exit,
    /// Its stdin queue has not drained within the no-progress timeout.
    WriteStalled,
}

pub struct Entry {
    pub relpath: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub is_dir: bool,
}

pub struct ScanResult {
    /// Complete with equal identities before and after: deletions are allowed.
    pub complete: bool,
    pub entries: Vec<Entry>,
    pub skipped: Vec<(String, String)>,
}

pub struct ExtractedFile {
    pub relpath: String,
    pub title: String,
    pub r#type: String,
    pub blake3: String,
    pub chunks: Vec<Chunk>,
    pub size: u64,
    pub mtime_ns: i64,
}

/// What the coordinator needs from storage; step 9 implements it over the databases.
pub trait Sink {
    fn indexed_files(&self, root: &str) -> usize;
    /// Share identity recorded for the root (survives restarts); `None` before the first scan.
    fn expected_identity(&self, root: &str) -> Option<Share>;
    fn set_expected_identity(&mut self, root: &str, share: &Share);
    /// Stored (size, mtime_ns) of a file, if indexed.
    fn known(&self, root: &str, relpath: &str) -> Option<(u64, i64)>;
    fn scan_done(&mut self, root: &str, scan: ScanResult);
    fn extracted(&mut self, root: &str, file: ExtractedFile);
    fn failed(&mut self, root: &str, relpath: &str, size: u64, mtime_ns: i64, reason: &str);
    /// Result of a `validate` worker for one root or exclusion path.
    fn validated(&mut self, path: &str, result: ValidationResult);
    fn root_state(&mut self, root: &str, state: &str, reason: &str);
}

/// Process control; the scheduler decides, this executes.
pub trait Spawner {
    fn spawn(&mut self, role: Role, root: &str) -> io::Result<WorkerId>;
    fn send(&mut self, id: WorkerId, job: &Job) -> io::Result<()>;
    /// SIGKILL; the process still counts until an `Event::Exit` reports its reaping.
    fn kill(&mut self, id: WorkerId);
}

struct Proc {
    child: Child,
    out: ChildStdout,
    buf: Vec<u8>,
    /// Stdout reached EOF: no longer polled, the process is reaped via `try_wait`.
    eof: bool,
    /// Bytes for the worker's stdin that did not fit the pipe yet.
    outq: Vec<u8>,
    blocked_since: Option<Instant>,
    stalled_reported: bool,
}

pub struct ProcSpawner {
    exe: PathBuf,
    config: PathBuf,
    next: WorkerId,
    procs: HashMap<WorkerId, Proc>,
}

const MAX_LINE: usize = 256 << 20;
const READ_CHUNK: usize = 64 << 10;
/// A worker whose stdin queue has not drained for this long is reported as stuck.
const WRITE_STALL: Duration = Duration::from_secs(30);

impl Proc {
    fn parse(&mut self) -> Vec<Message> {
        let mut out = Vec::new();
        while let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            match serde_json::from_slice(&line) {
                Ok(m) => out.push(m),
                Err(e) => eprintln!("bad worker message: {e}"),
            }
        }
        if self.buf.len() > MAX_LINE {
            self.buf.clear();
            let _ = self.child.kill();
        }
        out
    }

    /// At most one 64 KiB read, so a chatty worker cannot starve the loop.
    fn read_some(&mut self) -> Vec<Message> {
        let mut tmp = [0u8; READ_CHUNK];
        match self.out.read(&mut tmp) {
            Ok(0) => self.eof = true,
            Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            Err(_) => {}
        }
        self.parse()
    }

    /// After the process exited: whatever is left in the pipe is finite.
    fn drain(&mut self) -> Vec<Message> {
        let mut tmp = [0u8; READ_CHUNK];
        loop {
            match self.out.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            }
        }
        self.parse()
    }

    fn flush(&mut self) {
        if let Some(stdin) = self.child.stdin.as_mut() {
            while !self.outq.is_empty() {
                match stdin.write(&self.outq) {
                    Ok(0) => break,
                    Ok(n) => {
                        self.outq.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        self.outq.clear(); // worker is gone; it will be reaped
                    }
                }
            }
        }
        if self.outq.is_empty() {
            self.blocked_since = None;
        } else if self.blocked_since.is_none() {
            self.blocked_since = Some(Instant::now());
        }
    }
}

impl ProcSpawner {
    pub fn new(config: &Path) -> io::Result<Self> {
        Self::with_exe(std::env::current_exe()?, config)
    }

    pub fn with_exe(exe: PathBuf, config: &Path) -> io::Result<Self> {
        Ok(Self {
            exe,
            config: config.into(),
            next: 1,
            procs: HashMap::new(),
        })
    }

    /// Wait up to `timeout` for worker output, writable stdin or a mountinfo change; reap
    /// exited workers.
    pub fn poll(
        &mut self,
        mountinfo: &File,
        timeout: Duration,
    ) -> io::Result<(bool, Vec<(WorkerId, Event)>)> {
        let ids: Vec<WorkerId> = self.procs.keys().copied().collect();
        let mut fds = vec![PollFd::new(mountinfo, PollFlags::PRI | PollFlags::ERR)];
        let mut which = Vec::new(); // (worker, is_stdin) per fd after the first
        for id in &ids {
            let p = &self.procs[id];
            if !p.eof {
                fds.push(PollFd::new(&p.out, PollFlags::IN));
                which.push((*id, false));
            }
            if !p.outq.is_empty()
                && let Some(stdin) = &p.child.stdin
            {
                fds.push(PollFd::new(stdin, PollFlags::OUT));
                which.push((*id, true));
            }
        }
        let ts = Timespec {
            tv_sec: timeout.as_secs() as i64,
            tv_nsec: timeout.subsec_nanos() as i64,
        };
        match poll(&mut fds, Some(&ts)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
        let changed = fds[0].revents().intersects(PollFlags::PRI | PollFlags::ERR);
        let ready: Vec<_> = which
            .iter()
            .zip(&fds[1..])
            .filter(|(_, f)| !f.revents().is_empty())
            .map(|(w, _)| *w)
            .collect();
        drop(fds);
        let mut events = Vec::new();
        let msgs = |id, v: Vec<Message>| v.into_iter().map(move |m| (id, Event::Msg(m)));
        for (id, is_stdin) in ready {
            let p = self.procs.get_mut(&id).expect("listed");
            if is_stdin {
                p.flush();
            } else {
                events.extend(msgs(id, p.read_some()));
            }
        }
        for id in ids {
            let p = self.procs.get_mut(&id).expect("listed");
            if matches!(p.child.try_wait(), Ok(Some(_))) {
                events.extend(msgs(id, p.drain()));
                events.push((id, Event::Exit));
                self.procs.remove(&id);
            } else if !p.stalled_reported
                && p.blocked_since.is_some_and(|t| t.elapsed() >= WRITE_STALL)
            {
                p.stalled_reported = true;
                events.push((id, Event::WriteStalled));
            }
        }
        Ok((changed, events))
    }
}

impl Spawner for ProcSpawner {
    fn spawn(&mut self, role: Role, root: &str) -> io::Result<WorkerId> {
        let role = match role {
            Role::Scan => "scan",
            Role::Extract => "extract",
            Role::Watch => "watch",
            Role::Validate => "validate",
        };
        let mut child = Command::new(&self.exe)
            .arg("--config")
            .arg(&self.config)
            .args(["worker", "--role", role, "--root", root])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let out = child.stdout.take().expect("piped");
        fcntl_setfl(&out, OFlags::NONBLOCK)?;
        fcntl_setfl(child.stdin.as_ref().expect("piped"), OFlags::NONBLOCK)?;
        let id = self.next;
        self.next += 1;
        self.procs.insert(
            id,
            Proc {
                child,
                out,
                buf: Vec::new(),
                eof: false,
                outq: Vec::new(),
                blocked_since: None,
                stalled_reported: false,
            },
        );
        Ok(id)
    }

    fn send(&mut self, id: WorkerId, job: &Job) -> io::Result<()> {
        let p = self.procs.get_mut(&id).ok_or(io::ErrorKind::NotFound)?;
        serde_json::to_writer(&mut p.outq, job).map_err(io::Error::other)?;
        p.outq.push(b'\n');
        p.flush();
        Ok(())
    }

    fn kill(&mut self, id: WorkerId) {
        if let Some(p) = self.procs.get_mut(&id) {
            let _ = p.child.kill();
        }
    }
}

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("invalid duration: {s}");
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = n.parse().map_err(|_| bad())?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(bad()),
    };
    if n == 0 {
        return Err(bad());
    }
    Ok(Duration::from_secs(n * mult))
}

/// SQLite's temp files (large deletes, VACUUM) must land where Landlock lets us write.
fn sqlite_tmpdir(state_dir: &Path) {
    // SAFETY: called before any thread, worker or SQLite connection exists.
    unsafe { std::env::set_var("SQLITE_TMPDIR", state_dir) };
}

/// Write-only Landlock: the coordinator may write beneath the state dir and /proc, nothing else.
/// Read access is left alone (the coordinator reads config, mountinfo and its databases).
pub fn restrict_coordinator(state_dir: &Path) -> Result<(), Box<dyn Error>> {
    let abi = ABI::V6;
    let write = AccessFs::from_write(abi);
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(write)?
        .handle_access(AccessNet::from_all(abi))?
        .create()?
        .add_rule(PathBeneath::new(PathFd::new(state_dir)?, write))?
        .add_rule(PathBeneath::new(PathFd::new("/proc")?, write))?
        .restrict_self()?;
    if status.ruleset == RulesetStatus::NotEnforced
        || !matches!(status.landlock, LandlockStatus::Available { .. })
    {
        return Err("Landlock is unavailable: refusing to start".into());
    }
    Ok(())
}

fn specs_of(config: &Config) -> Result<(Params, Vec<RootSpec>), Box<dyn Error>> {
    let params = Params {
        workers: config.limits.workers,
        backoff_max: parse_duration(&config.limits.backoff_max)?,
    };
    let specs = config
        .roots
        .iter()
        .map(|r| {
            Ok(RootSpec {
                name: r.name.clone(),
                path: r.path.clone(),
                kind: r.kind,
                interval: parse_duration(&r.interval)?,
                share: r.share.clone(),
            })
        })
        .collect::<Result<_, String>>()?;
    Ok((params, specs))
}

/// Extract policy of one config generation: auth::visible (exclusions, denylist with
/// private-only lifts) and the file must belong to its own root, not a nested one.
fn allow(config: Config) -> Allow {
    Box::new(move |root, rel| {
        let Some(r) = config.roots.iter().find(|r| r.name == root) else {
            return false;
        };
        let path = r.path.join(rel);
        let Some(abs) = path.to_str() else {
            return false;
        };
        let owner = config
            .roots
            .iter()
            .filter(|o| path.starts_with(&o.path))
            .max_by_key(|o| o.path.components().count());
        owner.is_some_and(|o| o.name == root)
            && crate::auth::visible(abs, &config, &Default::default(), &Default::default())
                .is_some()
    })
}

fn stamp(path: &Path) -> Option<(u64, i64, i64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path).ok()?;
    Some((m.ino(), m.mtime(), m.mtime_nsec(), m.len()))
}

pub fn run(config_path: &Path) -> Result<(), Box<dyn Error>> {
    let config_path = &std::path::absolute(config_path)?;
    let config = config::load(config_path)?;
    let (params, specs) = specs_of(&config)?;
    statedir::prepare(&config.state_dir)?;
    sqlite_tmpdir(&config.state_dir);
    restrict_coordinator(&config.state_dir)?;
    let mut mountinfo = File::open("/proc/self/mountinfo")?;
    // The config decides what is excluded: it must not sit on a share that can hang or lie.
    if shares::is_network(&derive(&mut mountinfo, config_path).share.fstype) {
        return Err("config.toml is on a network or FUSE filesystem: refusing to start".into());
    }
    std::env::set_current_dir("/")?;
    let spawner = ProcSpawner::new(config_path)?;
    let mut sched = Sched::new(
        params,
        specs,
        &mut mountinfo,
        spawner,
        store::DbSink::open(&config.state_dir, config_path)?,
        allow(config::load(config_path)?),
    );
    sched.set_validate_paths(validate::paths(&config));
    let (running_params, running_specs) = specs_of(&config)?;
    let mut seen = stamp(config_path);
    let start = Instant::now();
    loop {
        let now_stamp = stamp(config_path);
        if now_stamp != seen {
            seen = now_stamp;
            let now = start.elapsed();
            match config::load(config_path) {
                // fail closed: a missing or invalid config denies every queued extract
                Err(e) => {
                    eprintln!("config unusable, extraction paused: {e}");
                    sched.reload(Box::new(|_, _| false), now);
                }
                Ok(new) => {
                    let (p, sp) = specs_of(&new)?;
                    if p != running_params || sp != running_specs {
                        return Err("roots or limits changed: restarting to apply".into());
                    }
                    let paths = validate::paths(&new);
                    if let Err(e) = sched.sink.reload(config_path) {
                        eprintln!("store reload: {e}");
                    }
                    sched.reload(allow(new), now);
                    sched.set_validate_paths(paths);
                }
            }
        }
        let (changed, events) = sched.spawner.poll(&mountinfo, Duration::from_secs(1))?;
        let now = start.elapsed();
        if changed {
            sched.rederive(&mut mountinfo, now);
        }
        for (id, ev) in events {
            sched.handle(id, ev, now);
        }
        sched.tick(now);
    }
}

/// Apply the coordinator ruleset, prove it holds, and prove workers are sandboxed on this host.
pub fn self_test(config_path: &Path) -> Result<(), Box<dyn Error>> {
    let config = config::load(config_path)?;
    statedir::prepare(&config.state_dir)?;
    let dir = config
        .state_dir
        .join(format!("selftest-{}", std::process::id()));
    let result = self_test_in(&config, &dir);
    let _ = fs::remove_dir_all(&dir);
    result
}

fn self_test_in(config: &Config, dir: &Path) -> Result<(), Box<dyn Error>> {
    let root = dir.join("root");
    fs::create_dir_all(&root)?;
    fs::write(root.join("fixture.txt"), "fixture")?;
    let fixture = dir.join("config.toml");
    fs::write(
        &fixture,
        format!(
            "state_dir = {:?}\n[[root]]\nname = \"fx\"\npath = {:?}\n",
            dir.display().to_string(),
            root.display().to_string()
        ),
    )?;
    sqlite_tmpdir(&config.state_dir);
    restrict_coordinator(&config.state_dir)?;
    let outside = std::env::temp_dir().join(format!("llm-index-selftest-{}", std::process::id()));
    match File::create(&outside) {
        Ok(_) => {
            let _ = fs::remove_file(&outside);
            return Err("coordinator can write outside the state dir".into());
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
        Err(e) => return Err(format!("unexpected error writing outside state dir: {e}").into()),
    }
    if worker::sandbox::tcp_restriction_supported() {
        match std::net::TcpStream::connect("127.0.0.1:1") {
            Err(e) if worker::tcp_denied(&e) => {}
            other => return Err(format!("coordinator can open TCP: {other:?}").into()),
        }
    }
    for role in ["selftest-scan", "selftest-extract"] {
        let out = Command::new(std::env::current_exe()?)
            .arg("--config")
            .arg(&fixture)
            .args(["worker", "--role", role, "--root", "fx"])
            .output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() || !stdout.contains("oom_score_adj=1000") {
            return Err(format!(
                "{role} failed: {stdout} {}",
                String::from_utf8_lossy(&out.stderr)
            )
            .into());
        }
    }
    let mut mountinfo = File::open("/proc/self/mountinfo")?;
    if derive(&mut mountinfo, &root).share.fstype == "unknown" {
        return Err("fixture root not found in mountinfo".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("15m"), Ok(Duration::from_secs(900)));
        assert_eq!(parse_duration("60m"), Ok(Duration::from_secs(3600)));
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("15").is_err());
        assert!(parse_duration("").is_err());
    }
}
