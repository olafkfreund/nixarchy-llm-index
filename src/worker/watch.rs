use std::collections::HashMap;
use std::error::Error;
use std::io::{self, BufRead};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::gitignore::Gitignore;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::scan::Walker;
use super::{Output, sandbox::Ready};
use crate::proto::{Job, Message};

const MASK: WatchMask = WatchMask::ONLYDIR
    .union(WatchMask::DONT_FOLLOW)
    .union(WatchMask::CREATE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MODIFY)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF);

type Watches = HashMap<PathBuf, (WatchDescriptor, Gitignore)>;

pub fn degraded(error: &io::Error) -> Message {
    Message::WatchDegraded {
        reason: if error.raw_os_error() == Some(28) {
            "ENOSPC".into()
        } else {
            error.to_string()
        },
    }
}

pub fn due(last: Instant, now: Instant) -> bool {
    now.duration_since(last) >= Duration::from_secs(2)
}

fn add_watches(
    ready: &Ready,
    output: &Output,
    inotify: &mut Inotify,
    watched: &mut Watches,
    by_wd: &mut HashMap<WatchDescriptor, PathBuf>,
    subtree: Option<&Path>,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    // Files already inside a new directory before its watch existed (`mkdir d && echo x > d/f`).
    let mut found = Vec::new();
    let mut walker = Walker::new(ready)?;
    let loader = Walker::new(ready)?;
    if let Some(rel) = subtree {
        for ancestor in rel
            .ancestors()
            .skip(1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            if let Some((_, rules)) = watched.get(ancestor) {
                walker.local.push(rules.clone());
            }
        }
    }
    let mut add = |rel: &Path,
                   fd: Option<&std::os::fd::OwnedFd>,
                   reason: Option<&str>|
     -> Result<(), Box<dyn Error>> {
        if reason.is_some() || watched.contains_key(rel) {
            return Ok(());
        }
        let Some(fd) = fd else {
            if subtree.is_some() {
                found.push(rel.to_path_buf());
            }
            return Ok(());
        };
        let root = &ready.config.roots[ready.root.ok_or("missing root")?].path;
        let rules = loader.load_rules(fd, &root.join(rel));
        let pinned = rustix::io::dup(fd)?;
        let proc_path = format!("/proc/self/fd/{}/.", pinned.as_raw_fd());
        let wd = inotify.watches().add(&proc_path, MASK)?;
        drop(pinned);
        by_wd.insert(wd.clone(), rel.to_path_buf());
        watched.insert(rel.to_path_buf(), (wd, rules));
        output.entry(0)?;
        Ok(())
    };
    let result = if let Some(rel) = subtree {
        walker.walk_subtree(rel, |rel, _, fd, reason| add(rel, fd, reason))
    } else {
        walker.walk(|rel, _, fd, reason| add(rel, fd, reason))
    };
    result?;
    // Unreadable or vanished directories are tolerated like in the scanner; only I/O errors
    // that mean the filesystem itself is failing degrade the watcher.
    if let Some((rel, reason)) = walker
        .skipped
        .iter()
        .find(|(_, reason)| reason == "EIO" || reason == "ESTALE")
    {
        return Err(format!("watch walk {}: {reason}", rel.display()).into());
    }
    Ok(found)
}

fn remove_watch(
    path: &Path,
    watched: &mut Watches,
    by_wd: &mut HashMap<WatchDescriptor, PathBuf>,
    removed: &mut Vec<WatchDescriptor>,
) {
    for rel in watched
        .keys()
        .filter(|rel| rel.starts_with(path))
        .cloned()
        .collect::<Vec<_>>()
    {
        if let Some((wd, _)) = watched.remove(&rel) {
            by_wd.remove(&wd);
            removed.push(wd);
        }
    }
}

fn watch_inner(ready: &mut Ready, output: &Output) -> Result<(), Box<dyn Error>> {
    let mut inotify = Inotify::init()?;
    let mut watched = Watches::new();
    let mut by_wd = HashMap::new();
    add_watches(ready, output, &mut inotify, &mut watched, &mut by_wd, None)?;
    const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".llmindexignore"];
    let mut buffer = [0; 8192];
    let mut pending = HashMap::<PathBuf, (Instant, String)>::new();
    let stdin = io::stdin();
    loop {
        let mut fds = [
            PollFd::new(&stdin, PollFlags::IN),
            PollFd::new(&inotify, PollFlags::IN),
        ];
        poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            }),
        )?;
        if fds[0].revents().intersects(PollFlags::IN | PollFlags::HUP) {
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                return Ok(());
            }
            if matches!(serde_json::from_str::<Job>(&line), Ok(Job::Shutdown)) {
                return Ok(());
            }
        }
        if fds[1].revents().contains(PollFlags::IN) {
            let events = inotify.read_events(&mut buffer)?;
            let mut additions = Vec::new();
            let mut removed = Vec::new();
            for event in events {
                if event.mask.contains(EventMask::Q_OVERFLOW) {
                    return Err("IN_Q_OVERFLOW".into());
                }
                let Some(dir) = by_wd.get(&event.wd).cloned() else {
                    continue;
                };
                if event
                    .mask
                    .intersects(EventMask::DELETE_SELF | EventMask::IGNORED | EventMask::MOVE_SELF)
                {
                    if dir.as_os_str().is_empty() {
                        return Err("root watch removed".into());
                    }
                    remove_watch(&dir, &mut watched, &mut by_wd, &mut removed);
                    continue;
                }
                let Some(name) = event.name else {
                    continue;
                };
                let path = dir.join(name);
                let is_dir = event.mask.contains(EventMask::ISDIR);
                // A changed ignore file invalidates the cached rules of its directory subtree.
                if !is_dir && IGNORE_FILES.iter().any(|f| name == *f) && watched.contains_key(&dir)
                {
                    remove_watch(&dir, &mut watched, &mut by_wd, &mut removed);
                    additions.push(dir.clone());
                }
                if is_dir
                    && event
                        .mask
                        .intersects(EventMask::DELETE | EventMask::MOVED_FROM)
                {
                    remove_watch(&path, &mut watched, &mut by_wd, &mut removed);
                }
                let mut walker = Walker::new(ready)?;
                for ancestor in dir.ancestors().collect::<Vec<_>>().into_iter().rev() {
                    if let Some((_, rules)) = watched.get(ancestor) {
                        walker.local.push(rules.clone());
                    }
                }
                let root = &ready.config.roots[ready.root.ok_or("missing root")?].path;
                if walker.reason(&root.join(&path), is_dir).is_some() {
                    continue;
                }
                if is_dir
                    && event
                        .mask
                        .intersects(EventMask::CREATE | EventMask::MOVED_TO)
                {
                    additions.push(path.clone());
                }
                pending.insert(path, (Instant::now(), format!("{:?}", event.mask)));
            }
            for wd in removed {
                let _ = inotify.watches().remove(wd);
            }
            for path in additions {
                for file in add_watches(
                    ready,
                    output,
                    &mut inotify,
                    &mut watched,
                    &mut by_wd,
                    Some(&path),
                )? {
                    pending.insert(file, (Instant::now(), "CREATE".into()));
                }
            }
        }
        let now = Instant::now();
        for path in pending
            .iter()
            .filter(|(_, (last, _))| due(*last, now))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>()
        {
            let (_, kind) = pending.remove(&path).ok_or("pending event disappeared")?;
            output.send(&Message::WatchEvent {
                relpath: path.to_string_lossy().into_owned(),
                kind,
            })?;
        }
    }
}

pub fn watch(ready: &mut Ready, output: &Output) -> Result<(), Box<dyn Error>> {
    if let Err(error) = watch_inner(ready, output) {
        let message = error
            .downcast_ref::<io::Error>()
            .map(degraded)
            .unwrap_or_else(|| Message::WatchDegraded {
                reason: error.to_string(),
            });
        output.send(&message)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_watch_and_add_failure() {
        let dir = std::fs::File::open(std::env::temp_dir()).unwrap();
        let inotify = Inotify::init().unwrap();
        let pinned = format!("/proc/self/fd/{}/.", dir.as_raw_fd());
        assert!(inotify.watches().add(&pinned, MASK).is_ok());
        let error = inotify
            .watches()
            .add("/proc/self/fd/999999999/.", MASK)
            .unwrap_err();
        assert!(matches!(degraded(&error), Message::WatchDegraded { .. }));
    }
}
