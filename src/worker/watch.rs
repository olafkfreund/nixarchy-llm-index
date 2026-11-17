use std::collections::HashMap;
use std::error::Error;
use std::io::{self, BufRead};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::gitignore::Gitignore;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::scan::{Walker, skip_contents};
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
type Pending = HashMap<PathBuf, (Instant, String, bool)>;

#[cfg(test)]
type WatchHook = Option<Box<dyn FnMut(&Path)>>;

#[cfg(test)]
thread_local! {
    static WATCH_HOOK: std::cell::RefCell<WatchHook> = const { std::cell::RefCell::new(None) };
}

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
    pending: &mut Pending,
    subtree: Option<&Path>,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    // Files already inside a new directory before its watch existed (`mkdir d && echo x > d/f`).
    let mut found = Vec::new();
    let mut changed = Vec::new();
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
        if (reason.is_some() && !matches!(reason, Some("worktree" | "cache")))
            || watched.contains_key(rel)
        {
            return Ok(());
        }
        let Some(fd) = fd else {
            if subtree.is_some() {
                found.push(rel.to_path_buf());
            }
            return Ok(());
        };
        #[cfg(test)]
        WATCH_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(rel);
            }
        });
        let root = &ready.config.roots[ready.root.ok_or("missing root")?].path;
        let rules = loader.load_rules(fd, &root.join(rel));
        let pinned = rustix::io::dup(fd)?;
        let proc_path = format!("/proc/self/fd/{}/.", pinned.as_raw_fd());
        let wd = match inotify.watches().add(&proc_path, MASK) {
            Ok(wd) => wd,
            Err(error)
                if !rel.as_os_str().is_empty()
                    && matches!(
                        rustix::io::Errno::from_io_error(&error),
                        Some(
                            rustix::io::Errno::NOENT
                                | rustix::io::Errno::NOTDIR
                                | rustix::io::Errno::ACCESS
                                | rustix::io::Errno::LOOP
                                | rustix::io::Errno::XDEV
                        )
                    ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        drop(pinned);
        by_wd.insert(wd.clone(), rel.to_path_buf());
        watched.insert(rel.to_path_buf(), (wd, rules));
        output.entry(0)?;
        if !rel.as_os_str().is_empty() {
            let now = skip_contents(fd);
            if now != reason {
                changed.push((rel.to_path_buf(), now.is_some()));
            }
        }
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
    for (rel, skipped) in changed {
        pending.insert(rel.clone(), (Instant::now(), "MODIFY".into(), true));
        if skipped {
            let mut removed = Vec::new();
            remove_watch(&rel, watched, by_wd, &mut removed, true);
            for wd in removed {
                let _ = inotify.watches().remove(wd);
            }
        } else {
            found.extend(add_watches(
                ready,
                output,
                inotify,
                watched,
                by_wd,
                pending,
                Some(&rel),
            )?);
        }
    }
    Ok(found)
}

fn remove_watch(
    path: &Path,
    watched: &mut Watches,
    by_wd: &mut HashMap<WatchDescriptor, PathBuf>,
    removed: &mut Vec<WatchDescriptor>,
    keep_top: bool,
) {
    for rel in watched
        .keys()
        .filter(|rel| rel.starts_with(path) && (!keep_top || rel.as_path() != path))
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
    let mut pending = Pending::new();
    add_watches(
        ready,
        output,
        &mut inotify,
        &mut watched,
        &mut by_wd,
        &mut pending,
        None,
    )?;
    output.send(&Message::WatchReady)?;
    let mut buffer = [0; 8192];
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
            process_events(
                ready,
                output,
                &mut inotify,
                &mut watched,
                &mut by_wd,
                &mut pending,
                &mut buffer,
            )?;
        }
        for message in due_events(ready, &mut pending)? {
            output.send(&message)?;
        }
    }
}

fn process_events(
    ready: &Ready,
    output: &Output,
    inotify: &mut Inotify,
    watched: &mut Watches,
    by_wd: &mut HashMap<WatchDescriptor, PathBuf>,
    pending: &mut Pending,
    buffer: &mut [u8],
) -> Result<(), Box<dyn Error>> {
    const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".llmindexignore"];
    let events = inotify.read_events(buffer)?;
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
            remove_watch(&dir, watched, by_wd, &mut removed, false);
            continue;
        }
        let Some(name) = event.name else {
            continue;
        };
        let path = dir.join(name);
        let is_dir = event.mask.contains(EventMask::ISDIR);
        if name == ".git" {
            if !dir.as_os_str().is_empty() {
                let mut walker = Walker::new(ready)?;
                if walker.under_skipped(&dir) {
                    remove_watch(&dir, watched, by_wd, &mut removed, true);
                } else {
                    additions.push(dir.clone());
                }
                pending.insert(dir, (Instant::now(), format!("{:?}", event.mask), true));
            }
            continue;
        }
        // A changed ignore file invalidates the cached rules of its directory subtree.
        if !is_dir && IGNORE_FILES.iter().any(|f| name == *f) && watched.contains_key(&dir) {
            remove_watch(&dir, watched, by_wd, &mut removed, false);
            additions.push(dir.clone());
        }
        if is_dir
            && event
                .mask
                .intersects(EventMask::DELETE | EventMask::MOVED_FROM)
        {
            remove_watch(&path, watched, by_wd, &mut removed, false);
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
        pending.insert(path, (Instant::now(), format!("{:?}", event.mask), false));
    }
    for wd in removed {
        let _ = inotify.watches().remove(wd);
    }
    for path in additions {
        for file in add_watches(ready, output, inotify, watched, by_wd, pending, Some(&path))? {
            pending.insert(file, (Instant::now(), "CREATE".into(), false));
        }
    }
    Ok(())
}

fn due_events(ready: &Ready, pending: &mut Pending) -> Result<Vec<Message>, Box<dyn Error>> {
    let now = Instant::now();
    let mut messages = Vec::new();
    let mut walker = Walker::new(ready)?;
    for path in pending
        .iter()
        .filter(|(_, (last, _, _))| due(*last, now))
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>()
    {
        let (_, kind, marker) = pending.remove(&path).ok_or("pending event disappeared")?;
        if !marker && walker.under_skipped(&path) {
            continue;
        }
        let (size, mtime_ns) = if marker {
            (None, None)
        } else {
            stat_regular(ready, &path)
        };
        messages.push(Message::WatchEvent {
            relpath: path.to_string_lossy().into_owned(),
            kind,
            size,
            mtime_ns,
        });
    }
    Ok(messages)
}

/// (size, mtime_ns) of a regular file, read through fd-relative `openat2` + `statx`.
fn stat_regular(ready: &Ready, rel: &Path) -> (Option<u64>, Option<i64>) {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (Some(root), Some(name)) = (ready.root_fd.as_ref(), rel.file_name()) else {
        return (None, None);
    };
    let st = (|| {
        let mut fd = rustix::io::dup(root).ok()?;
        for part in rel.parent()?.components() {
            fd = super::scan::open_dir(&fd, &CString::new(part.as_os_str().as_bytes()).ok()?)
                .ok()?;
        }
        statx(
            &fd,
            name,
            AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::TYPE | StatxFlags::SIZE | StatxFlags::MTIME,
        )
        .ok()
    })();
    match st {
        Some(st) if st.stx_mode & 0o170000 == 0o100000 => (
            Some(st.stx_size),
            Some(super::mtime_ns(
                st.stx_mtime.tv_sec,
                i64::from(st.stx_mtime.tv_nsec),
            )),
        ),
        _ => (None, None),
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
    use std::fs;

    fn fixture(label: &str) -> (PathBuf, PathBuf, Ready) {
        let base =
            std::env::temp_dir().join(format!("llm-index-watch-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        let cfg = base.join("config.toml");
        fs::write(
            &cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"r\"\npath = \"{}\"\n",
                base.join("state").display(),
                root.display()
            ),
        )
        .unwrap();
        let config = crate::config::load(&cfg).unwrap();
        let ready = Ready {
            root: Some(0),
            root_fd: Some(
                rustix::fs::openat(
                    rustix::fs::CWD,
                    &root,
                    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
                    rustix::fs::Mode::empty(),
                )
                .unwrap(),
            ),
            mountinfo: fs::File::open("/proc/self/mountinfo").unwrap(),
            global_ignore: Gitignore::empty(),
            oom_score_adj: String::new(),
            config,
        };
        (base, root, ready)
    }

    fn flush(ready: &Ready, pending: &mut Pending) -> Vec<Message> {
        for (last, _, _) in pending.values_mut() {
            *last = Instant::now() - Duration::from_secs(3);
        }
        due_events(ready, pending).unwrap()
    }

    fn events(
        ready: &Ready,
        output: &Output,
        inotify: &mut Inotify,
        watched: &mut Watches,
        by_wd: &mut HashMap<WatchDescriptor, PathBuf>,
        pending: &mut Pending,
    ) {
        process_events(
            ready,
            output,
            inotify,
            watched,
            by_wd,
            pending,
            &mut [0; 8192],
        )
        .unwrap();
    }

    #[test]
    fn under_skipped_checks_worktrees_and_submodules() {
        let (base, root, ready) = fixture("under-skipped");
        fs::create_dir_all(root.join("wt/a")).unwrap();
        fs::create_dir_all(root.join("sub/a")).unwrap();
        fs::write(root.join("wt/.git"), "gitdir: /x/repo/.git/worktrees/a\n").unwrap();
        fs::write(root.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        let mut walker = Walker::new(&ready).unwrap();
        assert!(walker.under_skipped(Path::new("wt/a/b.md")));
        assert!(!walker.under_skipped(Path::new("sub/a/b.md")));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn marker_events_change_watches_and_root_marker_is_ignored() {
        let (base, root, ready) = fixture("marker-events");
        fs::create_dir_all(root.join("wt/deep")).unwrap();
        let output = Output::new();
        let mut inotify = Inotify::init().unwrap();
        let mut watched = Watches::new();
        let mut by_wd = HashMap::new();
        let mut pending = Pending::new();
        add_watches(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
            None,
        )
        .unwrap();
        assert!(watched.contains_key(Path::new("wt/deep")));

        fs::write(root.join("wt/.git"), "gitdir: /x/repo/.git/worktrees/a\n").unwrap();
        events(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
        );
        assert!(watched.contains_key(Path::new("wt")));
        assert!(!watched.contains_key(Path::new("wt/deep")));
        let messages = flush(&ready, &mut pending);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0], Message::WatchEvent { relpath, size: None, mtime_ns: None, .. } if relpath == "wt")
        );
        fs::write(root.join("wt/deep/f.md"), "new").unwrap();
        assert!(flush(&ready, &mut pending).is_empty());

        fs::remove_file(root.join("wt/.git")).unwrap();
        events(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
        );
        assert!(watched.contains_key(Path::new("wt/deep")));
        assert!(flush(&ready, &mut pending).iter().any(|message| matches!(message, Message::WatchEvent { relpath, size: None, mtime_ns: None, .. } if relpath == "wt")));
        fs::write(root.join("wt/deep/f.md"), "changed").unwrap();
        events(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
        );
        assert!(flush(&ready, &mut pending).iter().any(|message| matches!(message, Message::WatchEvent { relpath, size: Some(_), .. } if relpath == "wt/deep/f.md")));

        fs::write(root.join(".git"), "gitdir: /x/repo/.git/worktrees/a\n").unwrap();
        events(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
        );
        assert!(flush(&ready, &mut pending).is_empty());
        fs::write(root.join("plain.md"), "plain").unwrap();
        events(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
        );
        assert!(flush(&ready, &mut pending).iter().any(|message| matches!(message, Message::WatchEvent { relpath, size: Some(_), .. } if relpath == "plain.md")));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn initial_worktree_keeps_its_watch_and_restores_children() {
        let (base, root, ready) = fixture("initial-worktree");
        fs::create_dir_all(root.join("wt/deep")).unwrap();
        fs::write(root.join("wt/.git"), "gitdir: /x/repo/.git/worktrees/a\n").unwrap();
        let output = Output::new();
        let mut inotify = Inotify::init().unwrap();
        let mut watched = Watches::new();
        let mut by_wd = HashMap::new();
        let mut pending = Pending::new();
        add_watches(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
            None,
        )
        .unwrap();
        assert_eq!(watched.len(), 2);
        assert!(watched.contains_key(Path::new("wt")));
        fs::remove_file(root.join("wt/.git")).unwrap();
        events(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
        );
        assert!(watched.contains_key(Path::new("wt/deep")));
        assert_eq!(flush(&ready, &mut pending).len(), 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn marker_created_between_walk_and_install_is_rechecked() {
        let (base, root, ready) = fixture("setup-race");
        fs::create_dir_all(root.join("wt/deep")).unwrap();
        let marker = root.join("wt/.git");
        WATCH_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |rel| {
                if rel == Path::new("wt") {
                    fs::write(&marker, "gitdir: /x/repo/.git/worktrees/a\n").unwrap();
                }
            }));
        });
        let output = Output::new();
        let mut inotify = Inotify::init().unwrap();
        let mut watched = Watches::new();
        let mut by_wd = HashMap::new();
        let mut pending = Pending::new();
        add_watches(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
            None,
        )
        .unwrap();
        WATCH_HOOK.with(|hook| *hook.borrow_mut() = None);
        assert_eq!(watched.len(), 2);
        assert!(watched.contains_key(Path::new("wt")));
        assert_eq!(pending.len(), 1);
        assert!(
            matches!(&flush(&ready, &mut pending)[0], Message::WatchEvent { relpath, size: None, mtime_ns: None, .. } if relpath == "wt")
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn vanished_subtree_does_not_degrade_watcher() {
        let base =
            std::env::temp_dir().join(format!("llm-index-watch-race-{}", std::process::id()));
        let root = base.join("root");
        fs::create_dir_all(root.join("gone")).unwrap();
        let cfg = base.join("config.toml");
        fs::write(
            &cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"r\"\npath = \"{}\"\n",
                base.join("state").display(),
                root.display()
            ),
        )
        .unwrap();
        let config = crate::config::load(&cfg).unwrap();
        let ready = Ready {
            root: Some(0),
            root_fd: Some(
                rustix::fs::openat(
                    rustix::fs::CWD,
                    &root,
                    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
                    rustix::fs::Mode::empty(),
                )
                .unwrap(),
            ),
            mountinfo: fs::File::open("/proc/self/mountinfo").unwrap(),
            global_ignore: Gitignore::empty(),
            oom_score_adj: String::new(),
            config,
        };
        let output = Output::new();
        let mut inotify = Inotify::init().unwrap();
        let mut watched = Watches::new();
        let mut by_wd = HashMap::new();
        let mut pending = Pending::new();
        add_watches(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
            None,
        )
        .unwrap();
        fs::remove_dir(root.join("gone")).unwrap();
        add_watches(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
            Some(Path::new("gone")),
        )
        .unwrap();
        fs::create_dir(root.join("live")).unwrap();
        add_watches(
            &ready,
            &output,
            &mut inotify,
            &mut watched,
            &mut by_wd,
            &mut pending,
            Some(Path::new("live")),
        )
        .unwrap();
        assert!(watched.contains_key(Path::new("live")));
        fs::remove_dir_all(base).unwrap();
    }

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
