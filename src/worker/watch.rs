use std::collections::HashMap;
use std::error::Error;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use inotify::{EventMask, Inotify, WatchMask};

use super::{Output, sandbox::Ready};
use crate::proto::Message;

pub fn skip_submount(path: &Path, root: &Path, mountinfo: &str) -> bool {
    mountinfo
        .lines()
        .filter_map(|line| line.split_once(" - ").map(|(left, _)| left))
        .filter_map(|left| left.split_whitespace().nth(4))
        .any(|mount| {
            let mount = Path::new(mount);
            mount != root && mount.starts_with(root) && path.starts_with(mount)
        })
}

pub fn degraded(error: &std::io::Error) -> Option<Message> {
    if error.raw_os_error() == Some(28) {
        Some(Message::WatchDegraded {
            reason: "ENOSPC".into(),
        })
    } else {
        None
    }
}

pub fn watch(ready: &mut Ready, output: &Output) -> Result<(), Box<dyn Error>> {
    let root = &ready.config.roots[ready.root.ok_or("missing root")?].path;
    ready.mountinfo.seek(SeekFrom::Start(0))?;
    let mut mountinfo = String::new();
    ready.mountinfo.read_to_string(&mut mountinfo)?;
    let mut inotify = Inotify::init()?;
    let mut watched = HashMap::new();
    let mut walk = WalkBuilder::new(root);
    walk.follow_links(false)
        .same_file_system(true)
        .git_global(false);
    let watch_root = root.clone();
    let mounted = mountinfo.clone();
    let nested: Vec<_> = ready
        .config
        .roots
        .iter()
        .filter(|r| r.path != *root)
        .map(|r| r.path.clone())
        .collect();
    walk.filter_entry(move |entry| {
        !skip_submount(entry.path(), &watch_root, &mounted)
            && !nested.iter().any(|path| entry.path().starts_with(path))
    });
    for entry in walk.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_dir())
            || skip_submount(entry.path(), root, &mountinfo)
        {
            continue;
        }
        match inotify.watches().add(
            entry.path(),
            WatchMask::ONLYDIR
                | WatchMask::DONT_FOLLOW
                | WatchMask::CREATE
                | WatchMask::DELETE
                | WatchMask::MODIFY
                | WatchMask::MOVED_FROM
                | WatchMask::MOVED_TO,
        ) {
            Ok(wd) => {
                watched.insert(wd, entry.path().to_path_buf());
                output.entry();
            }
            Err(error) => {
                output.send(&degraded(&error).unwrap_or(Message::WatchDegraded {
                    reason: error.to_string(),
                }))?;
                return Ok(());
            }
        }
    }
    let mut buffer = [0; 8192];
    let mut last = HashMap::<PathBuf, Instant>::new();
    loop {
        let mut new_dirs = Vec::new();
        let events = match inotify.read_events_blocking(&mut buffer) {
            Ok(events) => events,
            Err(error) => {
                output.send(&Message::WatchDegraded {
                    reason: error.to_string(),
                })?;
                return Ok(());
            }
        };
        for event in events {
            if event.mask.contains(EventMask::Q_OVERFLOW) {
                output.send(&Message::WatchDegraded {
                    reason: "IN_Q_OVERFLOW".into(),
                })?;
                return Ok(());
            }
            let Some(dir) = watched.get(&event.wd) else {
                continue;
            };
            let path = dir.join(event.name.unwrap_or_default());
            if event.mask.contains(EventMask::ISDIR)
                && (event.mask.contains(EventMask::CREATE)
                    || event.mask.contains(EventMask::MOVED_TO))
                && !skip_submount(&path, root, &mountinfo)
                && !ready
                    .config
                    .roots
                    .iter()
                    .any(|r| r.path != *root && path.starts_with(&r.path))
            {
                new_dirs.push(path.clone());
            }
            if last
                .get(&path)
                .is_some_and(|when| when.elapsed() < Duration::from_secs(2))
            {
                continue;
            }
            last.insert(path.clone(), Instant::now());
            output.send(&Message::WatchEvent {
                relpath: path.strip_prefix(root)?.to_string_lossy().into_owned(),
                kind: format!("{:?}", event.mask),
            })?;
        }
        for path in new_dirs {
            match inotify.watches().add(
                &path,
                WatchMask::ONLYDIR
                    | WatchMask::DONT_FOLLOW
                    | WatchMask::CREATE
                    | WatchMask::DELETE
                    | WatchMask::MODIFY
                    | WatchMask::MOVED_FROM
                    | WatchMask::MOVED_TO,
            ) {
                Ok(wd) => {
                    watched.insert(wd, path);
                    output.entry();
                }
                Err(error) => {
                    output.send(&degraded(&error).unwrap_or(Message::WatchDegraded {
                        reason: error.to_string(),
                    }))?;
                    return Ok(());
                }
            }
        }
    }
}
