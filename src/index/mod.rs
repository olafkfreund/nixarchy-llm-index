//! The `index` service: coordinator that schedules sandboxed workers and never touches a root.
//! Single-threaded (one `poll(2)` loop), so the 16-thread cap holds by construction.
pub mod sched;
pub mod shares;
pub mod watchers;

use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
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
use crate::proto::{Chunk, Job, Message, MountIdentity};
use sched::{Allow, Params, RootSpec, Sched};
use shares::{WorkerId, derive};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Scan,
    Extract,
    Watch,
}

#[allow(clippy::large_enum_variant)] // one short-lived value per message
pub enum Event {
    Msg(Message),
    /// The process was reaped by `waitpid`.
    Exit,
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
    pub identity: Option<MountIdentity>,
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
    /// Stored (size, mtime_ns) of a file, if indexed.
    fn known(&self, root: &str, relpath: &str) -> Option<(u64, i64)>;
    fn scan_done(&mut self, root: &str, scan: ScanResult);
    fn extracted(&mut self, root: &str, file: ExtractedFile);
    fn failed(&mut self, root: &str, relpath: &str, reason: &str);
    fn root_state(&mut self, root: &str, state: &str, reason: &str);
}

/// Process control; the scheduler decides, this executes.
pub trait Spawner {
    fn spawn(&mut self, role: Role, root: &str) -> io::Result<WorkerId>;
    fn send(&mut self, id: WorkerId, job: &Job) -> io::Result<()>;
    /// SIGKILL; the process still counts until an `Event::Exit` reports its reaping.
    fn kill(&mut self, id: WorkerId);
}

/// Stand-in sink until the store of step 9 exists: logs state changes, keeps nothing.
pub struct LogSink;

impl Sink for LogSink {
    fn indexed_files(&self, _: &str) -> usize {
        0
    }
    fn known(&self, _: &str, _: &str) -> Option<(u64, i64)> {
        None
    }
    fn scan_done(&mut self, root: &str, scan: ScanResult) {
        eprintln!(
            "scan {root}: {} entries, complete={}",
            scan.entries.len(),
            scan.complete
        );
    }
    fn extracted(&mut self, _: &str, _: ExtractedFile) {}
    fn failed(&mut self, root: &str, relpath: &str, reason: &str) {
        eprintln!("extract failed {root}/{relpath}: {reason}");
    }
    fn root_state(&mut self, root: &str, state: &str, reason: &str) {
        eprintln!("root {root}: {state} {reason}");
    }
}

struct Proc {
    child: Child,
    out: ChildStdout,
    buf: Vec<u8>,
}

pub struct ProcSpawner {
    exe: PathBuf,
    config: PathBuf,
    next: WorkerId,
    procs: HashMap<WorkerId, Proc>,
}

const MAX_LINE: usize = 256 << 20;

impl ProcSpawner {
    pub fn new(config: &Path) -> io::Result<Self> {
        Ok(Self {
            exe: std::env::current_exe()?,
            config: config.into(),
            next: 1,
            procs: HashMap::new(),
        })
    }

    fn read_messages(p: &mut Proc) -> Vec<Message> {
        let mut tmp = [0u8; 65536];
        loop {
            match p.out.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => p.buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let mut out = Vec::new();
        while let Some(end) = p.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = p.buf.drain(..=end).collect();
            match serde_json::from_slice(&line) {
                Ok(m) => out.push(m),
                Err(e) => eprintln!("bad worker message: {e}"),
            }
        }
        if p.buf.len() > MAX_LINE {
            p.buf.clear();
            let _ = p.child.kill();
        }
        out
    }

    /// Wait up to `timeout` for worker output or a mountinfo change; reap exited workers.
    pub fn poll(
        &mut self,
        mountinfo: &File,
        timeout: Duration,
    ) -> io::Result<(bool, Vec<(WorkerId, Event)>)> {
        let ids: Vec<WorkerId> = self.procs.keys().copied().collect();
        let mut fds = vec![PollFd::new(mountinfo, PollFlags::PRI | PollFlags::ERR)];
        fds.extend(
            ids.iter()
                .map(|id| PollFd::new(&self.procs[id].out, PollFlags::IN)),
        );
        let ts = Timespec {
            tv_sec: timeout.as_secs() as i64,
            tv_nsec: timeout.subsec_nanos() as i64,
        };
        match poll(&mut fds, Some(&ts)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
        let changed = fds[0].revents().intersects(PollFlags::PRI | PollFlags::ERR);
        let ready: Vec<bool> = fds[1..].iter().map(|f| !f.revents().is_empty()).collect();
        let mut events = Vec::new();
        for (id, ready) in ids.into_iter().zip(ready) {
            let p = self.procs.get_mut(&id).expect("listed");
            if ready {
                events.extend(
                    Self::read_messages(p)
                        .into_iter()
                        .map(|m| (id, Event::Msg(m))),
                );
            }
            if matches!(p.child.try_wait(), Ok(Some(_))) {
                events.extend(
                    Self::read_messages(p)
                        .into_iter()
                        .map(|m| (id, Event::Msg(m))),
                );
                events.push((id, Event::Exit));
                self.procs.remove(&id);
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
        let id = self.next;
        self.next += 1;
        self.procs.insert(
            id,
            Proc {
                child,
                out,
                buf: Vec::new(),
            },
        );
        Ok(id)
    }

    fn send(&mut self, id: WorkerId, job: &Job) -> io::Result<()> {
        let p = self.procs.get_mut(&id).ok_or(io::ErrorKind::NotFound)?;
        let stdin = p.child.stdin.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        let mut line = serde_json::to_vec(job).map_err(io::Error::other)?;
        line.push(b'\n');
        stdin.write_all(&line)
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

fn make_state_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
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
    let config = config::load(config_path)?;
    let (params, specs) = specs_of(&config)?;
    make_state_dir(&config.state_dir)?;
    restrict_coordinator(&config.state_dir)?;
    let mut mountinfo = File::open("/proc/self/mountinfo")?;
    let spawner = ProcSpawner::new(config_path)?;
    let mut sched = Sched::new(
        params,
        specs,
        &mut mountinfo,
        spawner,
        LogSink,
        allow(config::load(config_path)?),
    );
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
                    sched.reload(allow(new), now);
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
    make_state_dir(&config.state_dir)?;
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
