use std::collections::HashMap;
use std::error::Error;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use super::scan::Walker;
use super::{Output, sandbox::Ready};
use crate::proto::Message;

const MASK: WatchMask = WatchMask::ONLYDIR
    .union(WatchMask::DONT_FOLLOW)
    .union(WatchMask::CREATE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MODIFY)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO);

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
    watched: &mut HashMap<WatchDescriptor, (PathBuf, OwnedFd)>,
) -> Result<(), Box<dyn Error>> {
    let mut walker = Walker::new(ready)?;
    walker.walk(|rel, _, fd, reason| {
        if reason.is_some() {
            return Ok(());
        }
        let Some(fd) = fd else {
            return Ok(());
        };
        if watched.values().any(|(path, _)| path == rel) {
            return Ok(());
        }
        let pinned = rustix::io::dup(fd)?;
        let proc_path = format!("/proc/self/fd/{}/.", pinned.as_raw_fd());
        let wd = inotify.watches().add(&proc_path, MASK)?;
        watched.insert(wd, (rel.to_path_buf(), pinned));
        output.entry(0)?;
        Ok(())
    })
}

fn watch_inner(ready: &mut Ready, output: &Output) -> Result<(), Box<dyn Error>> {
    let mut inotify = Inotify::init()?;
    let mut watched = HashMap::new();
    add_watches(ready, output, &mut inotify, &mut watched)?;
    let mut buffer = [0; 8192];
    let mut pending = HashMap::<PathBuf, (Instant, String)>::new();
    loop {
        match inotify.read_events(&mut buffer) {
            Ok(events) => {
                let mut rewalk = false;
                for event in events {
                    if event.mask.contains(EventMask::Q_OVERFLOW) {
                        return Err("IN_Q_OVERFLOW".into());
                    }
                    let Some((dir, _)) = watched.get(&event.wd) else {
                        continue;
                    };
                    let Some(name) = event.name else {
                        continue;
                    };
                    let path = dir.join(name);
                    if !Walker::new(ready)?
                        .allowed_event(&path, event.mask.contains(EventMask::ISDIR))
                    {
                        continue;
                    }
                    if event.mask.contains(EventMask::ISDIR)
                        && (event.mask.contains(EventMask::CREATE)
                            || event.mask.contains(EventMask::MOVED_TO))
                    {
                        ready.mountinfo.seek(SeekFrom::Start(0))?;
                        let mut data = String::new();
                        ready.mountinfo.read_to_string(&mut data)?;
                        rewalk = true;
                    }
                    pending.insert(path, (Instant::now(), format!("{:?}", event.mask)));
                }
                if rewalk {
                    add_watches(ready, output, &mut inotify, &mut watched)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
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
        std::thread::sleep(Duration::from_millis(50));
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
