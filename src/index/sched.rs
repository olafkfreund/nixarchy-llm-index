//! Scheduler: pool, per-share rule, timeouts, abandonment, solo retry, watcher lifecycle.
//! Pure state machine: time, process control and storage are injected, so it never does I/O.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use super::shares::{Derived, ShareBook, Time, Verdict, WorkerId, derive, is_network, verdict};
use super::watchers::{DEGRADED_POLL, WatchState};
use super::{Entry, Event, ExtractedFile, Role, ScanResult, Sink, Spawner};
use crate::config::Kind;
use crate::proto::{Job, Message, MountIdentity, Share};
use crate::worker::scan::ReadSeek;

pub const PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
pub const EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(600);
pub const RETRY: Duration = Duration::from_secs(60);
pub const TOUCH_DELAY: Duration = Duration::from_secs(5);
/// Time a retiring worker gets to exit after `Shutdown` before SIGKILL.
pub const GRACE: Duration = Duration::from_secs(5);
/// An extract worker serves at most this many files / this long while others wait.
const MAX_FILES: u32 = 64;
const MAX_RUN: Duration = Duration::from_secs(60);

/// Current-config check `(root name, relpath) -> may be extracted`, applied right before dispatch.
pub type Allow = Box<dyn Fn(&str, &str) -> bool>;

#[derive(Debug, PartialEq)]
pub struct RootSpec {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub interval: Duration,
    pub share: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Params {
    pub workers: usize,
    pub backoff_max: Duration,
}

struct ExtractJob {
    rel: String,
    size: u64,
    mtime: i64,
    started: Time,
    /// Another worker was live at some point while this one ran.
    overlap: bool,
}

#[derive(Default)]
struct ScanBuf {
    entries: Vec<Entry>,
    skipped: Vec<(String, String)>,
}

struct Worker {
    root: usize,
    role: Role,
    abandoned: bool,
    /// Started under an older config: Shutdown sent, output ignored, still counts until reaped.
    retired: bool,
    /// Sent a byte/entry Progress during its job; silence from one that never did means stuck I/O.
    progressed: bool,
    /// Shutdown was sent at this time; SIGKILL after `GRACE` if still not reaped.
    closing: Option<Time>,
    served: u32,
    network: bool,
    solo: bool,
    started: Time,
    last_seen: Time,
    job: Option<ExtractJob>,
    scan: Option<ScanBuf>,
}

struct RootRt {
    spec: RootSpec,
    derived: Derived,
    expected: Option<Share>,
    state: String,
    scan_due: Time,
    rescan: bool,
    /// No new jobs for this root before this time (root error, identity mismatch).
    hold: Time,
    queue: VecDeque<(String, u64, i64)>,
    queued: HashSet<String>,
    failed: HashMap<String, (u64, i64)>,
    retried: HashMap<String, (u64, i64)>,
    busy: Option<(WorkerId, Role)>,
    watch: WatchState,
}

pub struct Sched<S, K> {
    pub spawner: S,
    pub sink: K,
    params: Params,
    roots: Vec<RootRt>,
    workers: HashMap<WorkerId, Worker>,
    shares: ShareBook,
    solo: VecDeque<(usize, String, u64, i64)>,
    solo_running: Option<WorkerId>,
    cursor: usize,
    allow: Allow,
}

impl<S: Spawner, K: Sink> Sched<S, K> {
    pub fn new(
        params: Params,
        specs: Vec<RootSpec>,
        mountinfo: &mut impl ReadSeek,
        spawner: S,
        sink: K,
        allow: Allow,
    ) -> Self {
        let roots = specs
            .into_iter()
            .map(|spec| RootRt {
                derived: derive(mountinfo, &spec.path),
                expected: sink.expected_identity(&spec.name),
                spec,
                state: String::new(),
                scan_due: Time::ZERO,
                rescan: false,
                hold: Time::ZERO,
                queue: VecDeque::new(),
                queued: HashSet::new(),
                failed: HashMap::new(),
                retried: HashMap::new(),
                busy: None,
                watch: WatchState::default(),
            })
            .collect();
        Self {
            spawner,
            sink,
            params,
            roots,
            workers: HashMap::new(),
            shares: ShareBook::default(),
            solo: VecDeque::new(),
            solo_running: None,
            cursor: 0,
            allow,
        }
    }

    fn network(&self, i: usize) -> bool {
        match self.roots[i].spec.kind {
            Kind::Network => true,
            Kind::Local => false,
            // A root recorded on a network share stays network when the share is unmounted
            // and its path falls through to a local mount.
            Kind::Auto => {
                is_network(&self.roots[i].derived.share.fstype)
                    || self.roots[i]
                        .expected
                        .as_ref()
                        .is_some_and(|e| is_network(&e.fstype))
            }
        }
    }

    fn pool_live(&self) -> usize {
        self.workers
            .values()
            .filter(|w| w.role != Role::Watch && !w.abandoned)
            .count()
    }

    fn set_state(&mut self, i: usize, state: &str, reason: &str) {
        let r = &mut self.roots[i];
        if r.state != state {
            r.state = state.into();
            self.sink.root_state(&r.spec.name, state, reason);
        }
    }

    /// Re-derive identities from a changed mountinfo; a live worker is charged against old and new.
    pub fn rederive(&mut self, mountinfo: &mut impl ReadSeek, now: Time) {
        for i in 0..self.roots.len() {
            let d = derive(mountinfo, &self.roots[i].spec.path);
            if d == self.roots[i].derived {
                continue;
            }
            for (id, w) in &self.workers {
                if w.root == i
                    && w.role != Role::Watch
                    && (w.network || is_network(&d.share.fstype))
                {
                    self.shares.charge(&d.share, *id);
                }
            }
            self.roots[i].derived = d;
            self.request_scan(i, now, Duration::ZERO);
        }
    }

    /// The config changed: swap the policy, retire every worker started under the old one
    /// (Shutdown now, SIGKILL after `GRACE`) and dispatch nothing until they are reaped.
    pub fn reload(&mut self, allow: Allow, now: Time) {
        self.allow = allow;
        let ids: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, w)| !w.abandoned && !w.retired)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let w = self.workers.get_mut(&id).expect("listed");
            w.retired = true;
            let i = w.root;
            if matches!(self.roots[i].busy, Some((b, _)) if b == id) {
                self.roots[i].busy = None;
            }
            match w.role {
                Role::Extract => {
                    if let Some(j) = w.job.take() {
                        self.roots[i].queued.insert(j.rel.clone());
                        self.roots[i].queue.push_front((j.rel, j.size, j.mtime));
                    }
                }
                Role::Scan => {
                    w.scan = None;
                    self.roots[i].scan_due = now;
                }
                Role::Watch => {
                    let r = &mut self.roots[i].watch;
                    r.ready = false;
                    r.degraded = false;
                    r.retry_at = now;
                }
            }
            self.shutdown(id, now);
        }
    }

    fn shutdown(&mut self, id: WorkerId, now: Time) {
        if let Some(w) = self.workers.get_mut(&id) {
            w.closing.get_or_insert(now);
        }
        let _ = self.spawner.send(id, &Job::Shutdown);
    }

    /// No new work on a root while one of its workers is abandoned and not yet reaped.
    fn root_blocked(&self, i: usize) -> bool {
        self.workers.values().any(|w| w.root == i && w.abandoned)
    }

    /// Pop the next queued file that the current config still allows; drop the rest.
    fn next_allowed(&mut self, i: usize) -> Option<(String, u64, i64)> {
        loop {
            let r = &mut self.roots[i];
            let (rel, size, mtime) = r.queue.pop_front()?;
            r.queued.remove(&rel);
            let (name, cur) = (&r.spec.name, (size, mtime));
            if (self.allow)(name, &rel)
                && self.sink.known(name, &rel) != Some(cur)
                && r.failed.get(&rel) != Some(&cur)
            {
                return Some((rel, size, mtime));
            }
        }
    }

    fn has_allowed(&mut self, i: usize) -> bool {
        match self.next_allowed(i) {
            Some((rel, size, mtime)) => {
                let r = &mut self.roots[i];
                r.queued.insert(rel.clone());
                r.queue.push_front((rel, size, mtime));
                true
            }
            None => false,
        }
    }

    fn request_scan(&mut self, i: usize, now: Time, delay: Duration) {
        let r = &mut self.roots[i];
        if matches!(r.busy, Some((_, Role::Scan))) {
            r.rescan = true;
        } else {
            r.scan_due = r.scan_due.min(now + delay);
        }
    }

    fn scan_finished(&mut self, i: usize, now: Time, next: Duration) {
        let r = &mut self.roots[i];
        let d = if r.rescan {
            TOUCH_DELAY.min(next)
        } else {
            next
        };
        r.rescan = false;
        r.scan_due = now + d;
    }

    fn interval_for(&self, i: usize) -> Duration {
        if self.network(i) {
            self.roots[i].spec.interval
        } else {
            self.roots[i].watch.local_interval()
        }
    }

    pub fn handle(&mut self, id: WorkerId, ev: Event, now: Time) {
        match ev {
            Event::Exit => self.reap(id, now),
            Event::WriteStalled => self.abandon(id, "stdin blocked", true, now),
            Event::Msg(m) => self.message(id, m, now),
        }
    }

    fn message(&mut self, id: WorkerId, m: Message, now: Time) {
        let Some(w) = self.workers.get_mut(&id) else {
            return;
        };
        if w.abandoned || w.retired {
            return;
        }
        w.last_seen = now;
        if matches!(m, Message::Progress { .. }) {
            w.progressed = true;
        }
        let (i, role) = (w.root, w.role);
        match m {
            Message::Entry {
                relpath,
                size,
                mtime_ns,
                is_dir,
            } => {
                if let Some(b) = &mut w.scan {
                    b.entries.push(Entry {
                        relpath,
                        size,
                        mtime_ns,
                        is_dir,
                    });
                }
            }
            Message::Skipped { relpath, reason } => {
                if let Some(b) = &mut w.scan {
                    b.skipped.push((relpath, reason));
                }
            }
            Message::ScanDone {
                complete,
                identity_before,
                identity_after,
            } => self.scan_done(id, complete, identity_before, identity_after, now),
            Message::Extracted {
                relpath,
                title,
                r#type,
                blake3,
                chunks,
            } => {
                if let Some(job) = take_job(w, &relpath) {
                    let file = ExtractedFile {
                        relpath,
                        title,
                        r#type,
                        blake3,
                        chunks,
                        size: job.size,
                        mtime_ns: job.mtime,
                    };
                    self.sink.extracted(&self.roots[i].spec.name, file);
                    self.send_next(id, now);
                }
            }
            Message::Failed { relpath, reason } => {
                if let Some(job) = take_job(w, &relpath) {
                    self.fail(i, &job, &reason);
                    self.send_next(id, now);
                }
            }
            Message::RootError { state, reason } if role != Role::Watch => {
                w.scan = None;
                let requeue = w.job.take();
                if let Some(j) = requeue {
                    self.roots[i].queued.insert(j.rel.clone());
                    self.roots[i].queue.push_front((j.rel, j.size, j.mtime));
                }
                let r = &mut self.roots[i];
                r.hold = now + RETRY;
                r.scan_due = now + RETRY;
                self.set_state(i, &state, &reason);
                if state == "offline" && self.network(i) {
                    self.share_offline(id, now);
                }
                self.shutdown(id, now);
            }
            Message::WatchReady if role == Role::Watch => {
                self.roots[i].watch.ready = true;
                self.roots[i].watch.degraded = false;
                if self.roots[i].state == "degraded" {
                    self.set_state(i, "ok", "");
                }
            }
            Message::WatchEvent {
                relpath,
                size,
                mtime_ns,
                ..
            } if role == Role::Watch => match (size, mtime_ns) {
                (Some(size), Some(mtime)) => self.queue_one(i, relpath, size, mtime),
                // gone or not a regular file: a deletion is only seen by a scan
                _ => self.request_scan(i, now, TOUCH_DELAY),
            },
            Message::WatchDegraded { reason } if role == Role::Watch => {
                self.mark_degraded(i, &reason, now);
            }
            _ => {}
        }
    }

    fn mark_degraded(&mut self, i: usize, reason: &str, now: Time) {
        self.roots[i].watch.degrade(now);
        self.request_scan(i, now, Duration::ZERO);
        if self.roots[i].state == "ok" || self.roots[i].state.is_empty() {
            self.set_state(i, "degraded", reason);
        }
    }

    fn fail(&mut self, i: usize, job: &ExtractJob, reason: &str) {
        let r = &mut self.roots[i];
        r.failed.insert(job.rel.clone(), (job.size, job.mtime));
        self.sink.failed(&r.spec.name, &job.rel, reason);
    }

    /// Feed the extract worker its next file, or let it exit.
    fn send_next(&mut self, id: WorkerId, now: Time) {
        let Some(w) = self.workers.get(&id) else {
            return;
        };
        let i = w.root;
        let capped = w.served >= MAX_FILES || now.saturating_sub(w.started) >= MAX_RUN;
        let stop = w.solo
            || !self.solo.is_empty()
            || (capped && self.others_waiting(i, now))
            || !self.identity_allows(i, now);
        let next = if stop { None } else { self.next_allowed(i) };
        let Some((rel, size, mtime)) = next else {
            self.shutdown(id, now);
            return;
        };
        let w = self.workers.get_mut(&id).expect("checked");
        w.served += 1;
        w.progressed = false;
        w.last_seen = now;
        w.job = Some(ExtractJob {
            rel: rel.clone(),
            size,
            mtime,
            started: now,
            overlap: false,
        });
        let _ = self.spawner.send(
            id,
            &Job::Extract {
                relpath: rel,
                expected_size: size,
                expected_mtime_ns: mtime,
            },
        );
        self.mark_overlap(id);
    }

    fn others_waiting(&self, i: usize, now: Time) -> bool {
        self.roots
            .iter()
            .enumerate()
            .any(|(j, r)| j != i && r.busy.is_none() && (!r.queue.is_empty() || r.scan_due <= now))
    }

    fn mark_overlap(&mut self, id: WorkerId) {
        let others = self
            .workers
            .iter()
            .any(|(k, w)| *k != id && w.role != Role::Watch && !w.abandoned);
        if others && let Some(j) = self.workers.get_mut(&id).and_then(|w| w.job.as_mut()) {
            j.overlap = true;
        }
    }

    fn identity_allows(&mut self, i: usize, now: Time) -> bool {
        let r = &self.roots[i];
        let indexed = self.sink.indexed_files(&r.spec.name);
        let v = verdict(
            r.expected.as_ref(),
            r.spec.share.as_deref(),
            &r.derived.share,
            indexed,
        );
        if v == Verdict::Mismatch {
            let r = &mut self.roots[i];
            r.queue.clear();
            r.queued.clear();
            r.hold = now + RETRY;
            self.solo.retain(|s| s.0 != i);
            self.set_state(i, "offline", "identity mismatch");
        }
        v != Verdict::Mismatch
    }

    fn scan_done(
        &mut self,
        id: WorkerId,
        complete: bool,
        before: Option<MountIdentity>,
        after: Option<MountIdentity>,
        now: Time,
    ) {
        let Some(w) = self.workers.get_mut(&id) else {
            return;
        };
        let Some(buf) = w.scan.take() else {
            return;
        };
        let i = w.root;
        self.shutdown(id, now);
        let name = self.roots[i].spec.name.clone();
        let changed = matches!((&before, &after), (Some(b), Some(a)) if b != a);
        let v = before.as_ref().map(|b| {
            let r = &self.roots[i];
            verdict(
                r.expected.as_ref(),
                r.spec.share.as_deref(),
                &b.share,
                self.sink.indexed_files(&name),
            )
        });
        if changed || v == Some(Verdict::Mismatch) {
            // result discarded entirely
            self.set_state(i, "offline", "identity mismatch");
            self.scan_finished(i, now, RETRY);
            return;
        }
        if v == Some(Verdict::Probe) {
            self.scan_finished(i, now, RETRY);
            return;
        }
        let accepted = complete && before.is_some() && before == after;
        if accepted && v == Some(Verdict::Adopt) {
            self.roots[i].expected = before.as_ref().map(|b| b.share.clone());
            if let Some(b) = &before {
                self.sink.set_expected_identity(&name, &b.share);
            }
        }
        self.queue_extracts(i, &buf.entries);
        self.sink.scan_done(
            &name,
            ScanResult {
                complete: accepted,
                identity: before,
                entries: buf.entries,
                skipped: buf.skipped,
            },
        );
        if accepted {
            let share = self.roots[i].derived.share.clone();
            self.shares.healthy(&share);
            if self.roots[i].watch.degraded {
                self.set_state(i, "degraded", "watch degraded");
            } else {
                self.set_state(i, "ok", "");
            }
        }
        let next = if accepted {
            self.interval_for(i)
        } else {
            RETRY
        };
        self.scan_finished(i, now, next);
    }

    fn queue_extracts(&mut self, i: usize, entries: &[Entry]) {
        for e in entries.iter().filter(|e| !e.is_dir) {
            self.queue_one(i, e.relpath.clone(), e.size, e.mtime_ns);
        }
    }

    fn queue_one(&mut self, i: usize, rel: String, size: u64, mtime: i64) {
        let r = &mut self.roots[i];
        let cur = (size, mtime);
        if self.sink.known(&r.spec.name, &rel) == Some(cur) || r.failed.get(&rel) == Some(&cur) {
            return;
        }
        if r.queued.insert(rel.clone()) {
            r.queue.push_back((rel, size, mtime));
        } else if let Some(q) = r.queue.iter_mut().find(|q| q.0 == rel) {
            *q = (rel, size, mtime);
        }
    }

    fn reap(&mut self, id: WorkerId, now: Time) {
        let Some(w) = self.workers.remove(&id) else {
            return;
        };
        self.shares.release(id);
        if self.solo_running == Some(id) {
            self.solo_running = None;
        }
        let i = w.root;
        if matches!(self.roots[i].busy, Some((b, _)) if b == id) {
            self.roots[i].busy = None;
        }
        match w.role {
            Role::Watch => {
                self.roots[i].watch.worker = None;
                if !self.roots[i].watch.degraded && !w.abandoned && !w.retired {
                    self.mark_degraded(i, "watcher exited", now);
                }
            }
            Role::Scan => {
                if w.scan.is_some() {
                    self.scan_finished(i, now, RETRY);
                }
            }
            Role::Extract => {
                if let Some(job) = w.job {
                    self.extract_died(i, job);
                }
            }
        }
    }

    /// Died or was OOM-killed mid-extract: one solo retry if it had company, else failed.
    fn extract_died(&mut self, i: usize, job: ExtractJob) {
        let cur = (job.size, job.mtime);
        if job.overlap && self.roots[i].retried.get(&job.rel) != Some(&cur) {
            self.roots[i].retried.insert(job.rel.clone(), cur);
            self.solo.push_back((i, job.rel, job.size, job.mtime));
        } else {
            self.fail(i, &job, "worker died");
        }
    }

    fn share_offline(&mut self, id: WorkerId, now: Time) {
        for s in self.shares.charged(id) {
            self.shares.offline(&s, now, self.params.backoff_max);
        }
    }

    /// `silent`: no message for `PROGRESS_TIMEOUT` (as opposed to a deadline hit while progressing).
    fn abandon(&mut self, id: WorkerId, why: &str, silent: bool, now: Time) {
        self.spawner.kill(id);
        let Some(w) = self.workers.get_mut(&id) else {
            return;
        };
        if w.abandoned {
            return;
        }
        w.abandoned = true;
        let (i, role) = (w.root, w.role);
        let (network, progressed) = (w.network, w.progressed);
        let job = w.job.take();
        let was_scanning = w.scan.take().is_some();
        if self.solo_running == Some(id) {
            self.solo_running = None;
        }
        if matches!(self.roots[i].busy, Some((b, _)) if b == id) {
            self.roots[i].busy = None;
        }
        if role == Role::Watch {
            self.mark_degraded(i, why, now);
            return;
        }
        // Only a network share that stopped answering goes offline; a slow or crashing
        // extract (it made progress) only fails its file, and local roots never go offline.
        let stuck = network && silent && (role == Role::Scan || !progressed);
        if let Some(job) = job {
            if stuck {
                let r = &mut self.roots[i];
                r.queued.insert(job.rel.clone());
                r.queue.push_front((job.rel, job.size, job.mtime));
            } else {
                self.fail(i, &job, why);
            }
        } else if was_scanning {
            self.scan_finished(i, now, RETRY);
        }
        if stuck {
            let shares = self.shares.charged(id);
            self.share_offline(id, now);
            for j in 0..self.roots.len() {
                if shares.contains(&self.roots[j].derived.share) {
                    self.set_state(j, "offline", why);
                }
            }
        }
    }

    pub fn tick(&mut self, now: Time) {
        let stalled: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, w)| !w.abandoned && w.closing.is_none())
            .filter_map(|(id, w)| {
                let silent = now.saturating_sub(w.last_seen) >= PROGRESS_TIMEOUT;
                match w.role {
                    Role::Scan if w.scan.is_some() && silent => Some((*id, "no progress", true)),
                    Role::Scan
                        if w.scan.is_some() && now.saturating_sub(w.started) >= SCAN_TIMEOUT =>
                    {
                        Some((*id, "scan timeout", false))
                    }
                    Role::Extract if w.job.is_some() && silent => {
                        Some((*id, "extract timeout", true))
                    }
                    Role::Extract
                        if w.job
                            .as_ref()
                            .is_some_and(|j| now.saturating_sub(j.started) >= EXTRACT_TIMEOUT) =>
                    {
                        Some((*id, "extract timeout", false))
                    }
                    Role::Watch if silent && !self.roots[w.root].watch.ready => {
                        Some((*id, "watch setup stalled", true))
                    }
                    _ => None,
                }
            })
            .collect();
        for (id, why, silent) in stalled {
            self.abandon(id, why, silent, now);
        }
        // Shutdown sent but not exited within the grace: SIGKILL (still counts until reaped).
        let overdue: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, w)| !w.abandoned && w.closing.is_some_and(|t| now >= t + GRACE))
            .map(|(id, w)| (*id, w.root))
            .collect();
        for (id, i) in overdue {
            self.spawner.kill(id);
            self.workers.get_mut(&id).expect("listed").abandoned = true;
            if matches!(self.roots[i].busy, Some((b, _)) if b == id) {
                self.roots[i].busy = None;
            }
            if self.solo_running == Some(id) {
                self.solo_running = None;
            }
        }
        for i in 0..self.roots.len() {
            if !self.network(i)
                && self.roots[i].watch.should_spawn(now)
                && !self.root_blocked(i)
                && self.identity_allows(i, now)
            {
                self.start_watcher(i, now);
            }
        }
        self.dispatch(now);
    }

    fn start_watcher(&mut self, i: usize, now: Time) {
        let name = self.roots[i].spec.name.clone();
        match self.spawner.spawn(Role::Watch, &name) {
            Ok(id) => {
                let _ = self.spawner.send(id, &Job::Watch);
                self.workers
                    .insert(id, new_worker(i, Role::Watch, false, now));
                self.roots[i].watch.worker = Some(id);
                self.roots[i].watch.ready = false;
            }
            Err(e) => {
                eprintln!("watcher spawn {name}: {e}");
                self.roots[i].watch.retry_at = now + DEGRADED_POLL;
            }
        }
    }

    fn dispatch(&mut self, now: Time) {
        if self.solo_running.is_some() || self.workers.values().any(|w| w.retired && !w.abandoned) {
            return;
        }
        while let Some((i, rel, size, mtime)) = self.solo.front().cloned() {
            let (name, cur) = (&self.roots[i].spec.name, (size, mtime));
            if (self.allow)(name, &rel)
                && self.sink.known(name, &rel) != Some(cur)
                && self.roots[i].failed.get(&rel) != Some(&cur)
            {
                break;
            }
            self.solo.pop_front();
        }
        if let Some((i, ..)) = self.solo.front().cloned() {
            let share = self.roots[i].derived.share.clone();
            let held = now < self.roots[i].hold
                || self.root_blocked(i)
                || (self.network(i) && !self.shares.free(&share, now));
            if !held && self.identity_allows(i, now) {
                if self.pool_live() == 0 {
                    let (_, rel, size, mtime) = self.solo.pop_front().expect("front");
                    self.start(i, Role::Extract, Some((rel, size, mtime)), now);
                }
                return; // drain: nothing else starts while a solo retry waits for the pool
            }
        }
        let n = self.roots.len();
        for k in 0..n {
            self.try_root((self.cursor + k) % n, now);
        }
        if n > 0 {
            self.cursor = (self.cursor + 1) % n;
        }
    }

    fn try_root(&mut self, i: usize, now: Time) {
        let r = &self.roots[i];
        if r.busy.is_some() || now < r.hold || self.root_blocked(i) {
            return;
        }
        let role = if self.has_allowed(i) {
            Role::Extract
        } else if self.roots[i].scan_due <= now {
            Role::Scan
        } else {
            return;
        };
        if !self.identity_allows(i, now) {
            return;
        }
        if self.network(i) && !self.shares.free(&self.roots[i].derived.share, now) {
            return;
        }
        let pool = self.params.workers;
        if self.pool_live() >= pool {
            return;
        }
        if self.network(i) {
            let net = self
                .workers
                .values()
                .filter(|w| w.network && !w.abandoned && w.role != Role::Watch)
                .count();
            if net >= pool - 1 {
                return;
            }
        }
        self.start(i, role, None, now);
    }

    fn start(&mut self, i: usize, role: Role, solo: Option<(String, u64, i64)>, now: Time) {
        let name = self.roots[i].spec.name.clone();
        let id = match self.spawner.spawn(role, &name) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("spawn {name}: {e}");
                self.roots[i].hold = now + RETRY;
                if let Some((rel, size, mtime)) = solo {
                    self.solo.push_front((i, rel, size, mtime));
                }
                return;
            }
        };
        let share = self.roots[i].derived.share.clone();
        if self.network(i) {
            self.shares.charge(&share, id);
        }
        let overlap = self.pool_live() > 0;
        for w in self.workers.values_mut() {
            if let Some(j) = &mut w.job {
                j.overlap = true;
            }
        }
        let mut w = new_worker(i, role, self.network(i), now);
        self.roots[i].busy = Some((id, role));
        if role == Role::Scan {
            w.scan = Some(ScanBuf::default());
            let _ = self.spawner.send(id, &Job::Scan);
            self.workers.insert(id, w);
            return;
        }
        let item = solo.clone().or_else(|| self.next_allowed(i));
        w.solo = solo.is_some();
        w.served = 1;
        if w.solo {
            self.solo_running = Some(id);
        }
        let (rel, size, mtime) = item.expect("extract start needs a file");
        w.job = Some(ExtractJob {
            rel: rel.clone(),
            size,
            mtime,
            started: now,
            overlap,
        });
        let _ = self.spawner.send(
            id,
            &Job::Extract {
                relpath: rel,
                expected_size: size,
                expected_mtime_ns: mtime,
            },
        );
        self.workers.insert(id, w);
    }

    #[cfg(test)]
    pub fn max_live_per_share(&self) -> usize {
        self.shares.max_live()
    }
}

fn new_worker(root: usize, role: Role, network: bool, now: Time) -> Worker {
    Worker {
        root,
        role,
        abandoned: false,
        retired: false,
        progressed: false,
        closing: None,
        served: 0,
        network,
        solo: false,
        started: now,
        last_seen: now,
        job: None,
        scan: None,
    }
}

fn take_job(w: &mut Worker, rel: &str) -> Option<ExtractJob> {
    if w.job.as_ref().is_some_and(|j| j.rel == rel) {
        w.job.take()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::process::Command;

    #[derive(Default)]
    struct Fake {
        spawned: Vec<(WorkerId, Role, String)>,
        sent: Vec<(WorkerId, String)>,
        killed: Vec<WorkerId>,
    }
    impl Spawner for Fake {
        fn spawn(&mut self, role: Role, root: &str) -> std::io::Result<WorkerId> {
            let id = self.spawned.len() as WorkerId + 1;
            self.spawned.push((id, role, root.into()));
            Ok(id)
        }
        fn send(&mut self, id: WorkerId, job: &Job) -> std::io::Result<()> {
            self.sent.push((id, serde_json::to_string(job).unwrap()));
            Ok(())
        }
        fn kill(&mut self, id: WorkerId) {
            self.killed.push(id);
        }
    }

    #[derive(Default)]
    struct Mem {
        indexed: usize,
        expected: HashMap<String, Share>,
        scans: Vec<(String, bool)>,
        failed: Vec<String>,
        states: RefCell<Vec<(String, String, String)>>,
    }
    impl Sink for Mem {
        fn expected_identity(&self, root: &str) -> Option<Share> {
            self.expected.get(root).cloned()
        }
        fn set_expected_identity(&mut self, root: &str, share: &Share) {
            self.expected.insert(root.into(), share.clone());
        }
        fn indexed_files(&self, _: &str) -> usize {
            self.indexed
        }
        fn known(&self, _: &str, _: &str) -> Option<(u64, i64)> {
            None
        }
        fn scan_done(&mut self, root: &str, scan: ScanResult) {
            self.scans.push((root.into(), scan.complete));
        }
        fn extracted(&mut self, _: &str, _: ExtractedFile) {}
        fn failed(&mut self, _: &str, relpath: &str, _: &str) {
            self.failed.push(relpath.into());
        }
        fn root_state(&mut self, root: &str, state: &str, reason: &str) {
            self.states
                .borrow_mut()
                .push((root.into(), state.into(), reason.into()));
        }
    }

    type T = Sched<Fake, Mem>;

    fn secs(n: u64) -> Time {
        Duration::from_secs(n)
    }
    fn mi(lines: &[&str]) -> Cursor<String> {
        Cursor::new(lines.join("\n"))
    }
    fn nfs(id: u32, point: &str, src: &str) -> String {
        format!("{id} 1 0:{id} / {point} rw - nfs {src} rw")
    }
    fn ext4(id: u32, point: &str) -> String {
        format!("{id} 1 8:{id} / {point} rw - ext4 /dev/d{id} rw")
    }
    fn spec(name: &str, path: &str) -> RootSpec {
        RootSpec {
            name: name.into(),
            path: path.into(),
            kind: Kind::Auto,
            interval: Duration::from_secs(900),
            share: None,
        }
    }
    fn sched(workers: usize, roots: &[(&str, &str)], mount: &[String]) -> T {
        let lines: Vec<&str> = mount.iter().map(String::as_str).collect();
        Sched::new(
            Params {
                workers,
                backoff_max: secs(3600),
            },
            roots.iter().map(|(n, p)| spec(n, p)).collect(),
            &mut mi(&lines),
            Fake::default(),
            Mem::default(),
            Box::new(|_, _| true),
        )
    }
    fn ident(point: &str, src: &str, fstype: &str, id: u64) -> MountIdentity {
        MountIdentity {
            share: Share {
                fstype: fstype.into(),
                source: src.into(),
                mount_root: "/".into(),
                mount_point: point.into(),
            },
            mount_id: id,
            submounts: vec![],
        }
    }
    fn scans(s: &T) -> Vec<(WorkerId, String)> {
        s.spawner
            .spawned
            .iter()
            .filter(|(_, r, _)| *r == Role::Scan)
            .map(|(i, _, n)| (*i, n.clone()))
            .collect()
    }
    fn roles(s: &T, role: Role) -> usize {
        s.spawner
            .spawned
            .iter()
            .filter(|(_, r, _)| *r == role)
            .count()
    }
    fn finish_scan(
        s: &mut T,
        id: WorkerId,
        who: MountIdentity,
        files: &[(&str, u64, i64)],
        now: Time,
    ) {
        for (rel, size, mtime) in files {
            s.handle(
                id,
                Event::Msg(Message::Entry {
                    relpath: (*rel).into(),
                    size: *size,
                    mtime_ns: *mtime,
                    is_dir: false,
                }),
                now,
            );
        }
        s.handle(
            id,
            Event::Msg(Message::ScanDone {
                complete: true,
                identity_before: Some(who.clone()),
                identity_after: Some(who),
            }),
            now,
        );
    }

    /// A finished scan is told to shut down and then exits, as a real worker does.
    fn done_scan(
        s: &mut T,
        id: WorkerId,
        who: MountIdentity,
        files: &[(&str, u64, i64)],
        now: Time,
    ) {
        finish_scan(s, id, who, files, now);
        assert!(
            s.spawner
                .sent
                .iter()
                .any(|(w, j)| *w == id && j.contains("Shutdown"))
        );
        s.handle(id, Event::Exit, now);
    }

    #[test]
    fn never_two_live_workers_per_share_across_outages_and_autofs() {
        let mut s = sched(
            2,
            &[("a", "/mnt/a")],
            &[ext4(1, "/"), nfs(2, "/mnt/a", "nas:/a")],
        );
        // three outage cycles on a: hang, abandon, reap, wait out the doubling backoff
        let mut t = secs(0);
        let mut killed_at = secs(0);
        for cycle in 0..3u32 {
            let before = scans(&s).iter().filter(|(_, n)| n == "a").count();
            loop {
                s.tick(t);
                assert!(s.max_live_per_share() <= 1);
                if scans(&s).iter().filter(|(_, n)| n == "a").count() > before {
                    break;
                }
                t += secs(5);
                assert!(t < secs(10_000));
            }
            if cycle > 0 {
                assert!(t >= killed_at + secs(60 * 2u64.pow(cycle - 1)));
            }
            let id = scans(&s).last().unwrap().0;
            t += secs(30);
            s.tick(t);
            assert_eq!(s.spawner.killed.last(), Some(&id));
            killed_at = t;
            t += secs(5);
            s.tick(t); // abandoned but unreaped: nothing new on the share
            assert_eq!(scans(&s).last().unwrap().0, id);
            assert!(s.max_live_per_share() <= 1);
            s.handle(id, Event::Exit, t);
        }
    }

    #[test]
    fn autofs_rederive_charges_old_and_new_identity() {
        let mut s = sched(
            2,
            &[("x", "/mnt/x/one"), ("y", "/mnt/x/two")],
            &[
                ext4(1, "/"),
                "3 1 0:3 / /mnt/x rw - autofs /etc/auto.x rw".into(),
            ],
        );
        let mut t = secs(0);
        s.tick(t);
        let x = scans(&s)[0].clone();
        assert_eq!(scans(&s).len(), 1);
        let nfs_line = nfs(4, "/mnt/x", "nas:/x");
        s.rederive(
            &mut mi(&[
                &ext4(1, "/"),
                "3 1 0:3 / /mnt/x rw - autofs /etc/auto.x rw",
                &nfs_line,
            ]),
            t,
        );
        s.tick(t);
        assert_eq!(scans(&s).len(), 1);
        assert!(s.max_live_per_share() <= 1);
        done_scan(&mut s, x.0, ident("/mnt/x", "nas:/x", "nfs", 4), &[], t);
        t += secs(1);
        s.tick(t);
        assert_eq!(scans(&s).len(), 2);
        assert!(s.max_live_per_share() <= 1);
    }

    #[test]
    fn sibling_roots_on_a_dead_share_get_one_probe() {
        let mut s = sched(
            2,
            &[("one", "/mnt/a/one"), ("two", "/mnt/a/two")],
            &[ext4(1, "/"), nfs(2, "/mnt/a", "nas:/a")],
        );
        s.tick(secs(0));
        assert_eq!(scans(&s).len(), 1);
        let id = scans(&s)[0].0;
        s.handle(
            id,
            Event::Msg(Message::RootError {
                state: "offline".into(),
                reason: "EIO".into(),
            }),
            secs(1),
        );
        s.handle(id, Event::Exit, secs(1));
        s.tick(secs(59));
        assert_eq!(scans(&s).len(), 1);
        s.tick(secs(61));
        assert_eq!(scans(&s).len(), 2);
        assert!(s.max_live_per_share() <= 1);
    }

    #[test]
    fn local_keeps_a_slot_and_healthy_share_is_scanned() {
        let mut s = sched(
            2,
            &[
                ("a", "/mnt/a"),
                ("b", "/mnt/b"),
                ("c", "/mnt/c"),
                ("l", "/home/l"),
            ],
            &[
                ext4(1, "/"),
                nfs(2, "/mnt/a", "nas:/a"),
                nfs(3, "/mnt/b", "nas:/b"),
                nfs(4, "/mnt/c", "nas:/c"),
                ext4(5, "/home"),
            ],
        );
        s.tick(secs(0));
        let w = s
            .spawner
            .spawned
            .iter()
            .find(|x| x.1 == Role::Watch)
            .unwrap()
            .0;
        s.handle(w, Event::Msg(Message::WatchReady), secs(0));
        // network jobs use at most workers-1 = 1 slot; the local root takes the other
        let names: Vec<_> = scans(&s).into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, ["a", "l"]);
        let l = scans(&s)[1].0;
        done_scan(
            &mut s,
            l,
            ident("/home", "/dev/d5", "ext4", 5),
            &[],
            secs(1),
        );
        s.tick(secs(30));
        let a = scans(&s)[0].0;
        assert!(s.spawner.killed.contains(&a));
        assert_eq!(scans(&s).last().unwrap().1, "b");
        s.tick(secs(60));
        assert!(s.spawner.killed.contains(&scans(&s)[2].0));
        // a and b hang unreaped; the healthy third share is still scanned
        assert_eq!(scans(&s).last().unwrap().1, "c");
        // and a local job still gets a slot
        s.rederive(
            &mut mi(&[
                &ext4(1, "/"),
                &nfs(2, "/mnt/a", "nas:/a"),
                &nfs(3, "/mnt/b", "nas:/b"),
                &nfs(4, "/mnt/c", "nas:/c"),
                "6 1 8:5 / /home rw - ext4 /dev/d5 rw",
            ]),
            secs(61),
        );
        // c is still live when the local root starts
        let c = scans(&s).iter().find(|(_, n)| n == "c").unwrap().0;
        assert!(!s.spawner.killed.contains(&c));
        s.tick(secs(61));
        assert_eq!(scans(&s).last().unwrap().1, "l");
        assert_eq!(s.pool_live(), 2);
    }

    fn mem_sched(sink: Mem, kind: Kind, mount: &[String]) -> T {
        let lines: Vec<&str> = mount.iter().map(String::as_str).collect();
        let mut spec = spec("r", "/mnt/nas");
        spec.kind = kind;
        Sched::new(
            Params {
                workers: 2,
                backoff_max: secs(3600),
            },
            vec![spec],
            &mut mi(&lines),
            Fake::default(),
            sink,
            Box::new(|_, _| true),
        )
    }

    #[test]
    fn restart_with_indexed_files_and_unmounted_share_never_scans() {
        let mut sink = Mem {
            indexed: 10_000,
            ..Mem::default()
        };
        sink.expected
            .insert("r".into(), ident("/mnt/nas", "nas:/a", "nfs", 2).share);
        // share not mounted: /mnt/nas falls through to the local root fs
        let mut s = mem_sched(sink, Kind::Auto, &[ext4(1, "/")]);
        for t in [0, 70, 200, 4000] {
            s.tick(secs(t));
        }
        assert!(s.spawner.spawned.is_empty()); // no scan, no watcher: nothing to delete with
        assert!(s.sink.scans.is_empty());
        let states = s.sink.states.borrow();
        assert_eq!(states.last().unwrap().1, "offline");
        assert_eq!(states.last().unwrap().2, "identity mismatch");
    }

    #[test]
    fn unrecorded_identity_with_indexed_files_is_a_mismatch() {
        let sink = Mem {
            indexed: 3,
            ..Mem::default()
        };
        let mut s = mem_sched(
            sink,
            Kind::Auto,
            &[ext4(1, "/"), nfs(2, "/mnt/nas", "nas:/a")],
        );
        s.tick(secs(0));
        assert!(scans(&s).is_empty());
    }

    #[test]
    fn expected_identity_is_persisted_on_first_scan() {
        let mut s = mem_sched(Mem::default(), Kind::Auto, &[nfs(2, "/mnt/nas", "nas:/a")]);
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        done_scan(
            &mut s,
            id,
            ident("/mnt/nas", "nas:/a", "nfs", 2),
            &[],
            secs(1),
        );
        assert_eq!(s.sink.expected["r"].source, "nas:/a");
    }

    #[test]
    fn repeated_hangs_on_a_local_root_leave_one_unreaped_worker() {
        let mut s = sched(2, &[("l", "/home/l")], &[ext4(1, "/")]);
        for t in (0..2000).step_by(10) {
            s.tick(secs(t));
        }
        // one scan and one watcher hung and unreaped: nothing else ever starts on the root
        assert_eq!(roles(&s, Role::Scan), 1);
        assert_eq!(roles(&s, Role::Watch), 1);
        assert_eq!(roles(&s, Role::Extract), 0);
    }

    #[test]
    fn finished_scan_is_not_later_declared_hung() {
        let mut s = sched(
            2,
            &[("a", "/mnt/a")],
            &[ext4(1, "/"), nfs(2, "/mnt/a", "nas:/a")],
        );
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        // the worker is slow to exit after ScanDone (no Exit event)
        finish_scan(
            &mut s,
            id,
            ident("/mnt/a", "nas:/a", "nfs", 2),
            &[],
            secs(1),
        );
        assert!(
            s.spawner
                .sent
                .iter()
                .any(|(w, j)| *w == id && j.contains("Shutdown"))
        );
        s.tick(secs(33));
        let states = s.sink.states.borrow();
        assert_eq!(states.last().unwrap().1, "ok");
        assert!(!states.iter().any(|x| x.1 == "offline"));
        drop(states);
        let share = s.roots[0].derived.share.clone();
        s.handle(id, Event::Exit, secs(34));
        assert!(s.shares.free(&share, secs(34)));
    }

    #[test]
    fn progress_resets_per_job_so_a_later_silent_file_takes_the_share_offline() {
        let mut s = sched(
            2,
            &[("a", "/mnt/a")],
            &[ext4(1, "/"), nfs(2, "/mnt/a", "nas:/a")],
        );
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        done_scan(
            &mut s,
            id,
            ident("/mnt/a", "nas:/a", "nfs", 2),
            &[("f", 1, 1), ("g", 1, 1)],
            secs(1),
        );
        s.tick(secs(1));
        let e = s.spawner.spawned.last().unwrap().0;
        s.handle(
            e,
            Event::Msg(Message::Progress {
                entries: None,
                bytes: Some(5),
            }),
            secs(2),
        );
        s.handle(
            e,
            Event::Msg(Message::Extracted {
                relpath: "f".into(),
                title: "t".into(),
                r#type: "text".into(),
                blake3: "h".into(),
                chunks: vec![],
            }),
            secs(3),
        );
        assert!(extracts(&s)[1].1.contains("\"g\""));
        s.tick(secs(40)); // g never reports anything: stuck in I/O
        let share = s.roots[0].derived.share.clone();
        s.handle(e, Event::Exit, secs(41));
        assert!(!s.shares.free(&share, secs(42)));
        assert!(s.sink.failed.is_empty()); // g is retried later, not failed
        assert!(s.roots[0].queue.iter().any(|q| q.0 == "g"));
    }

    #[test]
    fn scan_deadline_while_progressing_does_not_take_the_share_offline() {
        let mut s = sched(
            2,
            &[("a", "/mnt/a")],
            &[ext4(1, "/"), nfs(2, "/mnt/a", "nas:/a")],
        );
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        for t in (20..=600).step_by(20) {
            s.handle(
                id,
                Event::Msg(Message::Progress {
                    entries: Some(t),
                    bytes: None,
                }),
                secs(t),
            );
            s.tick(secs(t));
        }
        assert!(s.spawner.killed.contains(&id));
        s.handle(id, Event::Exit, secs(601));
        let share = s.roots[0].derived.share.clone();
        assert!(s.shares.free(&share, secs(601)));
        assert!(!s.sink.states.borrow().iter().any(|x| x.1 == "offline"));
    }

    #[test]
    fn empty_root_adopts_deeper_mount_indexed_root_mismatches() {
        for indexed in [0, 5] {
            let mut s = sched(
                2,
                &[("r", "/mnt/a/sub/docs")],
                &[ext4(1, "/"), nfs(2, "/mnt/a", "nas:/a")],
            );
            s.tick(secs(0));
            let id = scans(&s)[0].0;
            done_scan(
                &mut s,
                id,
                ident("/mnt/a", "nas:/a", "nfs", 2),
                &[],
                secs(1),
            );
            assert_eq!(s.sink.scans, [("r".to_string(), true)]);
            s.sink.indexed = indexed;
            s.rederive(
                &mut mi(&[
                    &ext4(1, "/"),
                    &nfs(2, "/mnt/a", "nas:/a"),
                    &nfs(3, "/mnt/a/sub", "nas:/b"),
                ]),
                secs(2),
            );
            s.tick(secs(3));
            if indexed == 0 {
                assert_eq!(scans(&s).len(), 2);
                let id = scans(&s)[1].0;
                done_scan(
                    &mut s,
                    id,
                    ident("/mnt/a/sub", "nas:/b", "nfs", 3),
                    &[],
                    secs(4),
                );
                assert_eq!(s.sink.scans.len(), 2);
                assert!(s.sink.scans[1].1);
            } else {
                assert_eq!(scans(&s).len(), 1);
                let states = s.sink.states.borrow();
                assert_eq!(states.last().unwrap().1, "offline");
                assert_eq!(states.last().unwrap().2, "identity mismatch");
            }
        }
    }

    #[test]
    fn mismatched_scan_result_is_discarded() {
        let mut s = sched(
            2,
            &[("r", "/mnt/a")],
            &[ext4(1, "/"), nfs(2, "/mnt/a", "nas:/a")],
        );
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        done_scan(
            &mut s,
            id,
            ident("/mnt/a", "nas:/a", "nfs", 2),
            &[],
            secs(1),
        );
        s.tick(secs(1000));
        let id = scans(&s)[1].0;
        done_scan(
            &mut s,
            id,
            ident("/mnt/a", "other:/z", "nfs", 2),
            &[],
            secs(1001),
        );
        assert_eq!(s.sink.scans.len(), 1);
    }

    #[test]
    fn oom_killed_worker_gets_one_solo_retry_then_fails() {
        let mut s = sched(
            2,
            &[("a", "/mnt/la"), ("b", "/mnt/lb")],
            &[ext4(1, "/"), ext4(2, "/mnt/la"), ext4(3, "/mnt/lb")],
        );
        s.tick(secs(0));
        watchers_ready(&mut s, secs(0));
        let (a, b) = (scans(&s)[0].0, scans(&s)[1].0);
        done_scan(
            &mut s,
            a,
            ident("/mnt/la", "/dev/d2", "ext4", 2),
            &[("f", 10, 5)],
            secs(1),
        );
        s.tick(secs(1)); // b still scanning: a's extract has company
        let e1 = s.spawner.spawned.last().unwrap().0;
        assert_eq!(s.spawner.spawned.last().unwrap().1, Role::Extract);
        s.handle(e1, Event::Exit, secs(2)); // OOM-killed: no Failed message
        assert!(s.sink.failed.is_empty());
        s.tick(secs(2)); // b still live: the pool drains, nothing new starts
        let spawned = s.spawner.spawned.len();
        s.tick(secs(3));
        assert_eq!(s.spawner.spawned.len(), spawned);
        done_scan(
            &mut s,
            b,
            ident("/mnt/lb", "/dev/d3", "ext4", 3),
            &[],
            secs(4),
        );
        s.tick(secs(4));
        let solo = s.spawner.spawned.last().unwrap().clone();
        assert_eq!(solo.1, Role::Extract);
        assert_ne!(solo.0, e1);
        s.tick(secs(5)); // the pool is the solo worker's alone
        assert_eq!(s.pool_live(), 1);
        s.handle(solo.0, Event::Exit, secs(6));
        assert_eq!(s.sink.failed, ["f"]);
        // not retried on an unchanged rescan, retried when mtime changes
        let w = watch_id(&s);
        let touch = |s: &mut T, t: u64| {
            let gone = Message::WatchEvent {
                relpath: "x".into(),
                kind: "x".into(),
                size: None,
                mtime_ns: None,
            };
            s.handle(w, Event::Msg(gone), secs(t));
            s.tick(secs(t + 6));
        };
        touch(&mut s, 200);
        let a2 = *scans(&s)
            .iter()
            .filter(|(_, n)| n == "a")
            .map(|(i, _)| i)
            .next_back()
            .unwrap();
        done_scan(
            &mut s,
            a2,
            ident("/mnt/la", "/dev/d2", "ext4", 2),
            &[("f", 10, 5)],
            secs(207),
        );
        s.tick(secs(207));
        assert_eq!(roles(&s, Role::Extract), 2);
        touch(&mut s, 300);
        let a3 = *scans(&s)
            .iter()
            .filter(|(_, n)| n == "a")
            .map(|(i, _)| i)
            .next_back()
            .unwrap();
        done_scan(
            &mut s,
            a3,
            ident("/mnt/la", "/dev/d2", "ext4", 2),
            &[("f", 10, 6)],
            secs(307),
        );
        s.tick(secs(307));
        assert_eq!(roles(&s, Role::Extract), 3);
    }

    #[test]
    fn watcher_setup_stall_degrades_and_polls() {
        let mut s = sched(2, &[("l", "/home/l")], &[ext4(1, "/")]);
        s.tick(secs(0));
        let w = s
            .spawner
            .spawned
            .iter()
            .find(|x| x.1 == Role::Watch)
            .unwrap()
            .0;
        s.tick(secs(30));
        assert!(s.spawner.killed.contains(&w));
        assert_eq!(roles(&s, Role::Watch), 1); // not respawned before reaped
        s.tick(secs(100));
        assert_eq!(roles(&s, Role::Watch), 1);
        s.handle(w, Event::Exit, secs(100));
        s.handle(scans(&s)[0].0, Event::Exit, secs(100)); // the silent scan is abandoned too
        s.tick(secs(100));
        assert_eq!(roles(&s, Role::Watch), 1); // retry only after the poll interval
        let rescan = scans(&s).last().unwrap().0;
        s.handle(
            rescan,
            Event::Msg(Message::Progress {
                entries: Some(1),
                bytes: None,
            }),
            secs(140),
        );
        s.tick(secs(150));
        assert_eq!(roles(&s, Role::Watch), 2);
    }

    fn extracts(s: &T) -> Vec<(WorkerId, String)> {
        s.spawner
            .sent
            .iter()
            .filter(|(_, j)| j.contains("Extract"))
            .cloned()
            .collect()
    }

    #[test]
    fn config_change_retires_workers_and_rechecks_queued_extracts() {
        let mut s = sched(2, &[("a", "/mnt/la")], &[ext4(1, "/"), ext4(2, "/mnt/la")]);
        s.tick(secs(0));
        watchers_ready(&mut s, secs(0));
        let id = scans(&s)[0].0;
        done_scan(
            &mut s,
            id,
            ident("/mnt/la", "/dev/d2", "ext4", 2),
            &[("f", 10, 5), ("g", 10, 5)],
            secs(1),
        );
        s.tick(secs(1));
        let old = s.spawner.spawned.last().unwrap().0;
        assert_eq!(extracts(&s).len(), 1);
        assert!(extracts(&s)[0].1.contains("\"f\""));
        // config now excludes g; a result from the old worker must not be used
        s.reload(Box::new(|_, rel| rel != "g"), secs(2));
        s.handle(
            old,
            Event::Msg(Message::Failed {
                relpath: "f".into(),
                reason: "x".into(),
            }),
            secs(2),
        );
        assert!(s.sink.failed.is_empty());
        assert!(s.spawner.sent.last().unwrap().1.contains("Shutdown"));
        let spawned = s.spawner.spawned.len();
        s.tick(secs(5)); // old worker still live: nothing dispatched
        assert_eq!(s.spawner.spawned.len(), spawned);
        assert_eq!(extracts(&s).len(), 1);
        s.tick(secs(12)); // grace over: SIGKILL, abandoned, share still held until reaped
        assert!(s.spawner.killed.contains(&old));
        s.handle(old, Event::Exit, secs(13));
        s.handle(watch_id(&s), Event::Exit, secs(13));
        s.tick(secs(13));
        let sent = extracts(&s);
        assert_eq!(sent.len(), 2);
        assert_ne!(sent[1].0, old);
        assert!(sent[1].1.contains("\"f\""));
        assert!(sent.iter().all(|(_, j)| !j.contains("\"g\"")));
    }

    #[test]
    fn excluded_file_is_never_sent_to_any_worker() {
        let mut s = sched(2, &[("a", "/mnt/la")], &[ext4(1, "/"), ext4(2, "/mnt/la")]);
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        done_scan(
            &mut s,
            id,
            ident("/mnt/la", "/dev/d2", "ext4", 2),
            &[("g", 10, 5)],
            secs(1),
        );
        s.reload(Box::new(|_, _| false), secs(1));
        s.tick(secs(2));
        assert!(extracts(&s).is_empty());
        assert_eq!(roles(&s, Role::Extract), 0);
    }

    #[test]
    fn aborting_worker_fails_the_file_without_taking_the_share_offline() {
        let mut s = sched(2, &[("a", "/mnt/la")], &[ext4(1, "/"), ext4(2, "/mnt/la")]);
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        done_scan(
            &mut s,
            id,
            ident("/mnt/la", "/dev/d2", "ext4", 2),
            &[("f", 10, 5)],
            secs(1),
        );
        s.tick(secs(1));
        let e = s.spawner.spawned.last().unwrap().0;
        s.handle(e, Event::Exit, secs(2)); // SIGABRT: EOF and exit, no reply
        assert_eq!(s.sink.failed, ["f"]);
        let share = s.roots[0].derived.share.clone();
        assert!(s.shares.free(&share, secs(2)));
        assert!(s.solo.is_empty());
    }

    /// Healthy local watchers: every spawned watcher reports its initial watches in place.
    fn watchers_ready(s: &mut T, now: Time) {
        let ids: Vec<_> = s
            .spawner
            .spawned
            .iter()
            .filter(|x| x.1 == Role::Watch)
            .map(|x| x.0)
            .collect();
        for id in ids {
            s.handle(id, Event::Msg(Message::WatchReady), now);
        }
    }

    fn watch_id(s: &T) -> WorkerId {
        s.spawner
            .spawned
            .iter()
            .find(|x| x.1 == Role::Watch)
            .unwrap()
            .0
    }

    #[test]
    fn watch_event_queues_extract_directly_or_requests_rescan() {
        let mut s = sched(2, &[("a", "/mnt/la")], &[ext4(1, "/"), ext4(2, "/mnt/la")]);
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        done_scan(
            &mut s,
            id,
            ident("/mnt/la", "/dev/d2", "ext4", 2),
            &[],
            secs(1),
        );
        let w = watch_id(&s);
        s.handle(w, Event::Msg(Message::WatchReady), secs(1));
        let ev = |size, m| {
            Event::Msg(Message::WatchEvent {
                relpath: "n.md".into(),
                kind: "x".into(),
                size,
                mtime_ns: m,
            })
        };
        s.handle(w, ev(Some(3), Some(9)), secs(2));
        s.tick(secs(2));
        assert_eq!(extracts(&s).len(), 1);
        assert!(extracts(&s)[0].1.contains("n.md"));
        assert_eq!(scans(&s).len(), 1); // no rescan for a present file
        let e = extracts(&s)[0].0;
        s.handle(
            e,
            Event::Msg(Message::Failed {
                relpath: "n.md".into(),
                reason: "x".into(),
            }),
            secs(3),
        );
        s.handle(e, Event::Exit, secs(3));
        s.handle(w, ev(None, None), secs(3));
        s.tick(secs(8));
        assert_eq!(scans(&s).len(), 2);
    }

    #[test]
    fn local_roots_on_one_filesystem_run_in_parallel() {
        let mut s = sched(2, &[("a", "/home/a"), ("b", "/home/b")], &[ext4(1, "/")]);
        s.tick(secs(0));
        assert_eq!(scans(&s).len(), 2);
    }

    #[test]
    fn extract_timeout_fails_the_file_but_only_silent_network_extracts_take_the_share_offline() {
        for (net, progress, offline) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let (root, mounts) = if net {
                ("/mnt/a", vec![ext4(1, "/"), nfs(2, "/mnt/a", "nas:/a")])
            } else {
                ("/mnt/a", vec![ext4(1, "/"), ext4(2, "/mnt/a")])
            };
            let mut s = sched(2, &[("a", root)], &mounts);
            s.tick(secs(0));
            let id = scans(&s)[0].0;
            let who = if net {
                ident("/mnt/a", "nas:/a", "nfs", 2)
            } else {
                ident("/mnt/a", "/dev/d2", "ext4", 2)
            };
            done_scan(&mut s, id, who, &[("f", 1, 1)], secs(1));
            s.tick(secs(1));
            let e = s.spawner.spawned.last().unwrap().0;
            if progress {
                s.handle(
                    e,
                    Event::Msg(Message::Progress {
                        entries: None,
                        bytes: Some(5),
                    }),
                    secs(2),
                );
            }
            s.tick(secs(40));
            // a stuck network extract goes back in the queue; otherwise only the file fails
            assert_eq!(s.sink.failed.is_empty(), offline);
            let share = s.roots[0].derived.share.clone();
            s.handle(e, Event::Exit, secs(41));
            assert_eq!(!s.shares.free(&share, secs(42)), offline);
        }
    }

    /// Runs in a child process under a Landlock ruleset that cannot read the fixture roots.
    #[test]
    fn coordinator_never_opens_a_root_child() {
        let Some(base) = std::env::var_os("LLM_INDEX_NOROOT_BASE") else {
            return;
        };
        use landlock::*;
        let abi = ABI::V6;
        let mut rules = Ruleset::default()
            .handle_access(AccessFs::from_read(abi))
            .unwrap()
            .create()
            .unwrap();
        for p in ["/nix/store", "/proc"] {
            rules = rules
                .add_rule(PathBeneath::new(
                    PathFd::new(p).unwrap(),
                    AccessFs::from_read(abi),
                ))
                .unwrap();
        }
        let status = rules.restrict_self().unwrap();
        assert_eq!(status.ruleset, RulesetStatus::FullyEnforced);
        let root = std::path::Path::new(&base).join("root");
        assert!(std::fs::File::open(&root).is_err());
        let mut mount = std::fs::File::open("/proc/self/mountinfo").unwrap();
        let mut s = Sched::new(
            Params {
                workers: 2,
                backoff_max: secs(3600),
            },
            vec![spec("r", root.to_str().unwrap())],
            &mut mount,
            Fake::default(),
            Mem::default(),
            Box::new(|_, _| true),
        );
        s.tick(secs(0));
        let id = scans(&s)[0].0;
        let who = derive_for(&mut mount, &root);
        done_scan(&mut s, id, who, &[("f", 1, 1)], secs(1));
        s.rederive(&mut mount, secs(2));
        s.tick(secs(3));
        assert_eq!(s.sink.scans, [("r".to_string(), true)]);
    }

    fn derive_for(mount: &mut std::fs::File, root: &std::path::Path) -> MountIdentity {
        crate::worker::scan::mount_identity(mount, root)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn coordinator_never_opens_a_root() {
        let base = std::env::temp_dir().join(format!("llm-index-noroot-{}", std::process::id()));
        std::fs::create_dir_all(base.join("root")).unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "index::sched::tests::coordinator_never_opens_a_root_child",
                "--nocapture",
            ])
            .env("LLM_INDEX_NOROOT_BASE", &base)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&base).unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && text.contains("1 passed"),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
