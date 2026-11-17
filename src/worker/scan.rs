use std::error::Error;
use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rustix::fs::{
    AtFlags, CWD, Dir, Mode, OFlags, ResolveFlags, Statx, StatxFlags, fstat, openat, openat2, statx,
};
use rustix::ioctl::{Getter, ioctl};

use super::{Output, sandbox::Ready};
use crate::config::Tier;
use crate::deny;
use crate::proto::{Message, MountIdentity, Share, ValidationResult};

pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "vendor",
    "third_party",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".direnv",
];
pub(crate) const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "flake.lock",
    "poetry.lock",
    "Pipfile.lock",
    "composer.lock",
    "Gemfile.lock",
    "go.sum",
    "uv.lock",
];
pub(crate) const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);

fn recoverable(error: rustix::io::Errno) -> bool {
    matches!(error, rustix::io::Errno::ACCESS | rustix::io::Errno::NOENT)
}

pub(crate) fn errno_reason(error: rustix::io::Errno) -> String {
    match error {
        rustix::io::Errno::ACCESS => "EACCES".into(),
        rustix::io::Errno::NOENT => "ENOENT".into(),
        rustix::io::Errno::IO => "EIO".into(),
        rustix::io::Errno::STALE => "ESTALE".into(),
        _ => format!("{error:?}"),
    }
}

#[derive(Clone)]
struct Mount {
    id: u64,
    point: PathBuf,
    root: String,
    fs: String,
    source: String,
}

fn unescape(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn mounts(data: &str) -> Vec<Mount> {
    data.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let fields: Vec<_> = left.split(' ').collect();
            let tail: Vec<_> = right.split(' ').collect();
            if fields.len() < 5 || tail.len() < 2 {
                return None;
            }
            Some(Mount {
                id: fields[0].parse().ok()?,
                point: unescape(fields[4]).into(),
                root: unescape(fields[3]),
                fs: unescape(tail[0]),
                source: unescape(tail[1]),
            })
        })
        .collect()
}

fn mount_state(
    mountinfo: &mut impl ReadSeek,
    root: &Path,
) -> Result<Option<MountIdentity>, Box<dyn Error>> {
    mountinfo.seek(SeekFrom::Start(0))?;
    let mut data = String::new();
    mountinfo.read_to_string(&mut data)?;
    let mounts = mounts(&data);
    let best = mounts
        .iter()
        .enumerate()
        .filter(|(_, m)| root.starts_with(&m.point))
        .max_by_key(|(n, m)| (m.point.components().count(), *n));
    let Some((_, mount)) = best else {
        return Ok(None);
    };
    let mut below: Vec<_> = mounts
        .iter()
        .filter(|m| m.point != root && m.point.starts_with(root))
        .map(|m| m.point.to_string_lossy().into_owned())
        .collect();
    below.sort();
    Ok(Some(MountIdentity {
        share: Share {
            fstype: mount.fs.clone(),
            source: mount.source.clone(),
            mount_root: mount.root.clone(),
            mount_point: mount.point.to_string_lossy().into_owned(),
        },
        mount_id: mount.id,
        submounts: below,
        fs_uuid: None,
    }))
}

pub fn mount_identity(
    mountinfo: &mut impl ReadSeek,
    root: &Path,
) -> Result<Option<MountIdentity>, Box<dyn Error>> {
    mount_state(mountinfo, root)
}

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

pub fn root_mount_id(fd: &OwnedFd) -> Result<u64, Box<dyn Error>> {
    Ok(statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?.stx_mnt_id)
}

pub fn fs_uuid(fd: &impl AsFd) -> Option<String> {
    // SAFETY: FS_IOC_GETFSUUID writes a length byte and up to 16 UUID bytes.
    let buf = unsafe { ioctl(fd, Getter::<0x80111500, [u8; 17]>::new()) }.ok()?;
    decode_fsuuid(&buf)
}

fn decode_fsuuid(buf: &[u8; 17]) -> Option<String> {
    let len = usize::from(buf[0]);
    if !(1..=16).contains(&len) {
        return None;
    }
    Some(buf[1..=len].iter().map(|b| format!("{b:02x}")).collect())
}

pub fn open_dir(parent: &OwnedFd, name: &CStr) -> rustix::io::Result<OwnedFd> {
    openat2(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        RESOLVE,
    )
}

fn read_ignore(parent: &OwnedFd, name: &str) -> Option<String> {
    if !statx(parent, name, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::TYPE)
        .is_ok_and(|st| st.stx_mode & 0o170000 == 0o100000)
    {
        return None;
    }
    let fd = match openat2(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
        RESOLVE,
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP | rustix::io::Errno::XDEV) => {
            return None;
        }
        Err(error) => {
            eprintln!("ignore {name}: {error}");
            return None;
        }
    };
    let meta = fstat(&fd).ok()?;
    if meta.st_mode & 0o170000 != 0o100000 || meta.st_size > 1024 * 1024 {
        eprintln!("ignore {name}: not a regular file under 1 MiB");
        return None;
    }
    let mut text = String::new();
    if File::from(fd)
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .is_err()
        || text.len() > 1024 * 1024
    {
        eprintln!("ignore {name}: unreadable or too large");
        return None;
    }
    Some(text)
}

fn linked_worktree(dir: &OwnedFd) -> bool {
    let Some(text) = read_ignore(dir, ".git") else {
        return false;
    };
    let Some(path) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
    else {
        return false;
    };
    let p = Path::new(path.trim());
    p.parent().and_then(Path::file_name) == Some("worktrees".as_ref())
        && p.parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|g| g.as_bytes().ends_with(b".git"))
}

pub(crate) fn skip_contents(dir: &OwnedFd) -> Option<&'static str> {
    if linked_worktree(dir) {
        Some("worktree")
    } else if statx(
        dir,
        "CACHEDIR.TAG",
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::TYPE,
    )
    .is_ok_and(|tag| tag.stx_mode & 0o170000 == 0o100000)
    {
        Some("cache")
    } else {
        None
    }
}

pub fn global_ignore() -> Gitignore {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    let mut builder = GitignoreBuilder::new("/");
    if let Some(path) = base.map(|p| p.join("git/ignore")) {
        match openat(
            CWD,
            &path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                if let Ok(meta) = fstat(&fd) {
                    if meta.st_mode & 0o170000 == 0o100000 && meta.st_size <= 1024 * 1024 {
                        let mut text = String::new();
                        if File::from(fd)
                            .take(1024 * 1024 + 1)
                            .read_to_string(&mut text)
                            .is_ok()
                            && text.len() <= 1024 * 1024
                        {
                            for line in text.lines() {
                                if let Err(e) = builder.add_line(Some(path.clone()), line) {
                                    eprintln!("global gitignore: {e}");
                                }
                            }
                        } else {
                            eprintln!("global gitignore: unreadable or too large");
                        }
                    } else {
                        eprintln!("global gitignore: not a regular file under 1 MiB");
                    }
                }
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(e) => eprintln!("global gitignore: {e}"),
        }
    }
    builder.build().unwrap_or_else(|e| {
        eprintln!("global gitignore: {e}");
        Gitignore::empty()
    })
}

pub struct Walker<'a> {
    ready: &'a Ready,
    root: &'a Path,
    pub(crate) local: Vec<Gitignore>,
    pub skipped: Vec<(PathBuf, String)>,
    pub incomplete: bool,
}
impl<'a> Walker<'a> {
    pub fn new(ready: &'a Ready) -> Result<Self, Box<dyn Error>> {
        let root = &ready.config.roots[ready.root.ok_or("missing root")?].path;
        Ok(Self {
            ready,
            root,
            local: Vec::new(),
            skipped: Vec::new(),
            incomplete: false,
        })
    }
    pub(crate) fn reason(&self, path: &Path, is_dir: bool) -> Option<&'static str> {
        let i = self.ready.root?;
        let root = &self.ready.config.roots[i];
        let name = path.file_name().unwrap_or(OsStr::new(""));
        if crate::sanitize::unsafe_name(name) {
            return Some("unsafe_name");
        }
        if self
            .ready
            .config
            .roots
            .iter()
            .enumerate()
            .any(|(n, r)| n != i && r.path.starts_with(&root.path) && path.starts_with(&r.path))
        {
            return Some("nested_root");
        }
        if self
            .ready
            .config
            .exclude
            .paths
            .iter()
            .any(|p| path.starts_with(p))
        {
            return Some("excluded");
        }
        let lifts = if root.tier == Tier::Private {
            self.ready.config.exclude.lift_builtin.as_slice()
        } else {
            &[]
        };
        if deny::denied(&path.to_string_lossy(), &self.ready.config.state_dir, lifts) {
            return Some("denied");
        }
        for matcher in self
            .local
            .iter()
            .rev()
            .chain(std::iter::once(&self.ready.global_ignore))
        {
            let result = matcher.matched_path_or_any_parents(path, is_dir);
            if result.is_ignore() {
                return Some("ignored");
            }
            if result.is_whitelist() {
                break;
            }
        }
        if is_dir
            && (SKIP_DIRS.iter().any(|n| OsStr::new(n) == name)
                || name.to_string_lossy().starts_with("result"))
        {
            return Some("directory");
        }
        if !is_dir && LOCKFILES.iter().any(|n| OsStr::new(n) == name) {
            return Some("lockfile");
        }
        None
    }
    pub(crate) fn load_rules(&self, fd: &OwnedFd, path: &Path) -> Gitignore {
        let mut builder = GitignoreBuilder::new(path);
        for name in [".gitignore", ".ignore", ".llmindexignore"] {
            if let Some(text) = read_ignore(fd, name) {
                for line in text.lines() {
                    if let Err(e) = builder.add_line(Some(path.join(name)), line) {
                        eprintln!("{}: {e}", path.join(name).display());
                    }
                }
            }
        }
        if let Ok(git) = openat2(
            fd,
            ".git",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            RESOLVE,
        ) && let Some(text) = read_ignore(&git, "info/exclude")
        {
            for line in text.lines() {
                if let Err(e) = builder.add_line(Some(path.join(".git/info/exclude")), line) {
                    eprintln!("git exclude: {e}");
                }
            }
        }
        builder.build().unwrap_or_else(|e| {
            eprintln!("ignore rules: {e}");
            Gitignore::empty()
        })
    }
    pub fn walk<F>(&mut self, mut visit: F) -> Result<(), Box<dyn Error>>
    where
        F: FnMut(&Path, &Statx, Option<&OwnedFd>, Option<&str>) -> Result<(), Box<dyn Error>>,
    {
        let fd = self.ready.root_fd.as_ref().ok_or("missing root fd")?;
        if let Some(reason) = self.reason(self.root, true) {
            return Err(format!("root skipped: {reason}").into());
        }
        let st = statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS)?;
        visit(Path::new(""), &st, Some(fd), None)?;
        self.descend(fd, Path::new(""), &mut visit)
    }
    pub fn walk_subtree<F>(&mut self, rel: &Path, mut visit: F) -> Result<(), Box<dyn Error>>
    where
        F: FnMut(&Path, &Statx, Option<&OwnedFd>, Option<&str>) -> Result<(), Box<dyn Error>>,
    {
        let mut fd = rustix::io::dup(self.ready.root_fd.as_ref().ok_or("missing root fd")?)?;
        let mut opened_rel = PathBuf::new();
        for part in rel.components() {
            fd = match open_dir(&fd, &CString::new(part.as_os_str().as_bytes())?) {
                Ok(fd) => fd,
                Err(error)
                    if !rel.as_os_str().is_empty()
                        && matches!(
                            error,
                            rustix::io::Errno::NOENT
                                | rustix::io::Errno::NOTDIR
                                | rustix::io::Errno::ACCESS
                                | rustix::io::Errno::LOOP
                                | rustix::io::Errno::XDEV
                        ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            opened_rel.push(part.as_os_str());
            if let Some(reason) = skip_contents(&fd) {
                let st = statx(&fd, "", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS)?;
                visit(&opened_rel, &st, Some(&fd), Some(reason))?;
                return Ok(());
            }
        }
        let st = statx(&fd, "", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS)?;
        visit(rel, &st, Some(&fd), None)?;
        self.descend(&fd, rel, &mut visit)
    }
    fn skip_error(&mut self, rel: &Path, error: rustix::io::Errno) {
        self.incomplete |= !recoverable(error);
        self.skipped.push((rel.to_path_buf(), errno_reason(error)));
    }
    fn descend<F>(&mut self, fd: &OwnedFd, rel: &Path, visit: &mut F) -> Result<(), Box<dyn Error>>
    where
        F: FnMut(&Path, &Statx, Option<&OwnedFd>, Option<&str>) -> Result<(), Box<dyn Error>>,
    {
        let path = self.root.join(rel);
        self.local.push(self.load_rules(fd, &path));
        let dir = match Dir::read_from(fd) {
            Ok(dir) => dir,
            Err(error) if rel != Path::new("") => {
                self.skip_error(rel, error);
                self.local.pop();
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        for entry in dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if rel != Path::new("") => {
                    self.skip_error(rel, error);
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let part = OsStr::from_bytes(name.to_bytes());
            let child_rel = rel.join(part);
            let child_path = self.root.join(&child_rel);
            let st = match statx(
                fd,
                name,
                AtFlags::SYMLINK_NOFOLLOW,
                StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
            ) {
                Ok(st) => st,
                Err(error) => {
                    self.skip_error(&child_rel, error);
                    continue;
                }
            };
            let is_dir = st.stx_mode & 0o170000 == 0o040000;
            let reason = if st.stx_mode & 0o170000 == 0o120000 {
                Some("symlink")
            } else if !is_dir && st.stx_mode & 0o170000 != 0o100000 {
                Some("special")
            } else {
                self.reason(&child_path, is_dir)
            };
            if reason.is_some() {
                visit(&child_rel, &st, None, reason)?;
                continue;
            }
            if is_dir {
                match open_dir(fd, name) {
                    Ok(child) => {
                        let identity = (|| -> rustix::io::Result<(u64, u64, u64)> {
                            Ok((
                                statx(&child, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?
                                    .stx_mnt_id,
                                statx(fd, part, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::INO)?
                                    .stx_ino,
                                statx(&child, "", AtFlags::EMPTY_PATH, StatxFlags::INO)?.stx_ino,
                            ))
                        })();
                        let (mount_id, parent_ino, child_ino) = match identity {
                            Ok(identity) => identity,
                            Err(error) => {
                                self.skip_error(&child_rel, error);
                                continue;
                            }
                        };
                        if mount_id
                            != root_mount_id(self.ready.root_fd.as_ref().ok_or("missing root fd")?)?
                        {
                            visit(&child_rel, &st, None, Some("mount"))?;
                        } else if parent_ino == st.stx_ino {
                            if child_ino != st.stx_ino {
                                visit(&child_rel, &st, None, Some("symlink"))?;
                                continue;
                            }
                            if let Some(reason) = skip_contents(&child) {
                                visit(&child_rel, &st, Some(&child), Some(reason))?;
                                continue;
                            }
                            visit(&child_rel, &st, Some(&child), None)?;
                            self.descend(&child, &child_rel, visit)?;
                        } else {
                            visit(&child_rel, &st, None, Some("symlink"))?;
                        }
                    }
                    Err(rustix::io::Errno::XDEV) => visit(&child_rel, &st, None, Some("mount"))?,
                    Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                        visit(&child_rel, &st, None, Some("symlink"))?
                    }
                    Err(error) => self.skip_error(&child_rel, error),
                }
            } else {
                visit(&child_rel, &st, None, None)?;
            }
        }
        self.local.pop();
        Ok(())
    }
    pub fn allowed_dir(&self, path: &Path) -> bool {
        self.reason(path, true).is_none()
    }
    pub fn allowed_event(&mut self, rel: &Path, is_dir: bool) -> bool {
        self.local.clear();
        let Some(root_fd) = self.ready.root_fd.as_ref() else {
            return false;
        };
        let mut opened: Option<OwnedFd> = None;
        let mut path = self.root.to_path_buf();
        self.local.push(self.load_rules(root_fd, &path));
        let parts: Vec<_> = rel.components().collect();
        for (n, part) in parts.iter().enumerate() {
            path.push(part.as_os_str());
            let directory = n + 1 < parts.len() || is_dir;
            if self.reason(&path, directory).is_some() {
                return false;
            }
            if n + 1 < parts.len() {
                let Ok(name) = CString::new(part.as_os_str().as_bytes()) else {
                    return false;
                };
                let fd = opened.as_ref().unwrap_or(root_fd);
                let Ok(child) = open_dir(fd, &name) else {
                    return false;
                };
                self.local.push(self.load_rules(&child, &path));
                opened = Some(child);
            }
        }
        true
    }
}

/// An identity is reported only when it is the mount the root fd lives on.
fn on_fd_mount(identity: Option<MountIdentity>, fd_id: Option<u64>) -> Option<MountIdentity> {
    identity.filter(|m| Some(m.mount_id) == fd_id)
}

pub fn scan(ready: &mut Ready, output: &Output) -> Result<(), Box<dyn Error>> {
    let i = ready.root.ok_or("missing root")?;
    let root = &ready.config.roots[i];
    let before = match mount_state(&mut ready.mountinfo, &root.path) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("scan root mount: {error}");
            return output.send(&Message::ScanDone {
                complete: false,
                identity_before: None,
                identity_after: None,
            });
        }
    };
    // Not tested: statx on a local directory fd does not fail without kernel or FUSE fault
    // injection; NFS/FUSE can give ESTALE/EIO, which is why this branch exists.
    let fd_id = match root_mount_id(ready.root_fd.as_ref().ok_or("missing root fd")?) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("scan root mount id: {error}");
            return output.send(&Message::ScanDone {
                complete: false,
                identity_before: None,
                identity_after: None,
            });
        }
    };
    // An identity that is not the one the root fd lives on is unknown, not a mismatch.
    let before = on_fd_mount(before, Some(fd_id)).map(|mut identity| {
        identity.fs_uuid = fs_uuid(ready.root_fd.as_ref().expect("root fd checked above"));
        identity
    });
    let mut complete = before.is_some();
    let max_size = ready.config.limits.max_file_mb.saturating_mul(1024 * 1024);
    let mut walker = Walker::new(ready)?;
    if let Err(error) = walker.walk(|rel, st, _, reason| {
        if rel.as_os_str().is_empty() {
            return Ok(());
        }
        let relpath = rel.to_string_lossy().into_owned();
        let is_dir = st.stx_mode & 0o170000 == 0o040000;
        if let Some(reason) =
            reason.or_else(|| (!is_dir && st.stx_size > max_size).then_some("too_large"))
        {
            output.send(&Message::Skipped {
                relpath,
                reason: reason.into(),
            })?;
        } else {
            output.send(&Message::Entry {
                relpath,
                size: st.stx_size,
                mtime_ns: super::mtime_ns(st.stx_mtime.tv_sec, i64::from(st.stx_mtime.tv_nsec)),
                is_dir,
            })?;
            output.entry(st.stx_size)?;
        }
        Ok(())
    }) {
        eprintln!("scan: {error}");
        complete = false;
    }
    for (rel, reason) in walker.skipped {
        output.send(&Message::Skipped {
            relpath: rel.to_string_lossy().into_owned(),
            reason,
        })?;
    }
    complete &= !walker.incomplete;
    let after = match mount_state(&mut ready.mountinfo, &root.path) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("scan root mount: {error}");
            complete = false;
            None
        }
    };
    let after_id = match root_mount_id(ready.root_fd.as_ref().ok_or("missing root fd")?) {
        Ok(id) => Some(id),
        Err(error) => {
            eprintln!("scan root mount id: {error}");
            complete = false;
            None
        }
    };
    let after = on_fd_mount(after, after_id).map(|mut identity| {
        identity.fs_uuid = fs_uuid(ready.root_fd.as_ref().expect("root fd checked above"));
        identity
    });
    complete &= before == after && after.is_some();
    output.send(&Message::ScanDone {
        complete,
        identity_before: before,
        identity_after: after,
    })
}

pub fn validate(paths: Vec<String>, output: &Output) -> Result<(), Box<dyn Error>> {
    for path in paths {
        output.send(&Message::Validated {
            result: validate_path(Path::new(&path)),
            path,
        })?;
    }
    Ok(())
}

pub fn validate_path(path: &Path) -> ValidationResult {
    let mut current = path;
    loop {
        match openat2(
            CWD,
            current,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => {
                if fstat(&fd).is_ok_and(|st| st.st_mode & 0o170000 == 0o120000) {
                    return ValidationResult::Eloop;
                }
                return ValidationResult::Ok;
            }
            Err(rustix::io::Errno::LOOP) => return ValidationResult::Eloop,
            Err(rustix::io::Errno::NOENT) => match current.parent() {
                Some(parent) if parent != current => current = parent,
                _ => return ValidationResult::Pending,
            },
            Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::ACCESS) => {
                return ValidationResult::Ok;
            }
            Err(_) => return ValidationResult::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn test_ready(base: &Path) -> Ready {
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = base.join("config.toml");
        std::fs::write(
            &cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"r\"\npath = \"{}\"\n",
                base.join("state").display(),
                root.display()
            ),
        )
        .unwrap();
        let config = crate::config::load(&cfg).unwrap();
        let root_fd = openat2(
            CWD,
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .unwrap();
        Ready {
            root: Some(0),
            root_fd: Some(root_fd),
            mountinfo: File::open("/proc/self/mountinfo").unwrap(),
            global_ignore: Gitignore::empty(),
            oom_score_adj: String::new(),
            config,
        }
    }

    #[test]
    fn linked_worktree_markers_skip_only_worktrees() {
        let base =
            std::env::temp_dir().join(format!("llm-index-worktree-markers-{}", std::process::id()));
        let root = base.join("root");
        for (name, marker) in [
            ("standard", "gitdir: /x/repo/.git/worktrees/a"),
            ("bare", "gitdir: /x/repo.git/worktrees/a"),
            ("submodule", "gitdir: ../.git/modules/foo"),
            ("named_worktrees", "gitdir: ../.git/modules/worktrees/foo"),
        ] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(".git"), marker).unwrap();
            std::fs::write(dir.join("file.md"), name).unwrap();
        }
        std::fs::create_dir_all(root.join("git_folder/.git")).unwrap();
        std::fs::write(root.join("git_folder/file.md"), "content").unwrap();
        std::fs::write(base.join("git_target"), "gitdir: /x/repo/.git/worktrees/a").unwrap();
        std::fs::create_dir_all(root.join("git_symlink")).unwrap();
        std::os::unix::fs::symlink(base.join("git_target"), root.join("git_symlink/.git")).unwrap();
        std::fs::write(root.join("git_symlink/file.md"), "content").unwrap();

        let ready = test_ready(&base);
        let mut seen = Vec::new();
        Walker::new(&ready)
            .unwrap()
            .walk(|rel, _, _, reason| {
                seen.push((rel.to_path_buf(), reason.map(str::to_owned)));
                Ok(())
            })
            .unwrap();
        for name in ["standard", "bare"] {
            assert!(seen.contains(&(PathBuf::from(name), Some("worktree".into()))));
            assert!(
                !seen
                    .iter()
                    .any(|(rel, _)| rel == &Path::new(name).join("file.md"))
            );
        }
        for name in ["submodule", "named_worktrees", "git_folder", "git_symlink"] {
            assert!(seen.contains(&(PathBuf::from(name), None)));
            assert!(seen.contains(&(Path::new(name).join("file.md"), None)));
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn worktree_root_is_walked() {
        let base =
            std::env::temp_dir().join(format!("llm-index-worktree-root-{}", std::process::id()));
        let ready = test_ready(&base);
        std::fs::write(base.join("root/.git"), "gitdir: /x/repo/.git/worktrees/a").unwrap();
        std::fs::write(base.join("root/file.md"), "content").unwrap();
        let mut seen = Vec::new();
        Walker::new(&ready)
            .unwrap()
            .walk(|rel, _, _, reason| {
                seen.push((rel.to_path_buf(), reason.map(str::to_owned)));
                Ok(())
            })
            .unwrap();
        assert!(seen.contains(&(PathBuf::from("file.md"), None)));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn worktree_subtree_visits_only_skipped_folder() {
        let base =
            std::env::temp_dir().join(format!("llm-index-worktree-subtree-{}", std::process::id()));
        let ready = test_ready(&base);
        std::fs::create_dir_all(base.join("root/wt/nested")).unwrap();
        std::fs::write(
            base.join("root/wt/.git"),
            "gitdir: /x/repo/.git/worktrees/a",
        )
        .unwrap();
        std::fs::write(base.join("root/wt/nested/file.md"), "content").unwrap();
        for rel in ["wt", "wt/nested"] {
            let mut seen = Vec::new();
            Walker::new(&ready)
                .unwrap()
                .walk_subtree(Path::new(rel), |path, _, fd, reason| {
                    seen.push((path.to_path_buf(), fd.is_some(), reason.map(str::to_owned)));
                    Ok(())
                })
                .unwrap();
            assert_eq!(seen, [(PathBuf::from("wt"), true, Some("worktree".into()))]);
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn decode_fsuuid_bounds_and_hex() {
        let mut buf = [0_u8; 17];
        assert_eq!(decode_fsuuid(&buf), None);
        buf[0] = 1;
        buf[1] = 0xab;
        assert_eq!(decode_fsuuid(&buf).as_deref(), Some("ab"));
        buf[0] = 16;
        buf[1..].copy_from_slice(&[
            0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x33, 0x33, 0x44, 0x44, 0x55, 0x55, 0x55, 0x55,
            0x55, 0x55,
        ]);
        assert_eq!(
            decode_fsuuid(&buf).as_deref(),
            Some("11111111222233334444555555555555")
        );
        buf[0] = 17;
        assert_eq!(decode_fsuuid(&buf), None);
    }

    #[test]
    fn fs_uuid_on_tempdir_does_not_panic() {
        let path = std::env::temp_dir().join(format!("llm-index-fsuuid-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let dir = File::open(&path).unwrap();
        if let Some(uuid) = fs_uuid(&dir) {
            assert!((2..=32).contains(&uuid.len()));
            assert!(uuid.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn identity_off_the_root_fd_mount_is_none() {
        let data = "1 0 0:1 / / rw - ext4 /dev/root rw\n2 1 0:2 / /mnt/nas rw - nfs old rw";
        let id = mount_identity(&mut Cursor::new(data), Path::new("/mnt/nas"))
            .unwrap()
            .unwrap();
        assert_eq!(on_fd_mount(Some(id.clone()), Some(2)), Some(id.clone()));
        assert_eq!(on_fd_mount(Some(id.clone()), Some(1)), None);
        assert_eq!(on_fd_mount(Some(id), None), None);
    }

    #[test]
    fn foreign_mount_identity_is_unknown() {
        let Some(base) = std::env::var_os("LLM_INDEX_FOREIGN_BASE").map(PathBuf::from) else {
            let base =
                std::env::temp_dir().join(format!("llm-index-foreign-{}", std::process::id()));
            std::fs::create_dir_all(base.join("root")).unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "worker::scan::tests::foreign_mount_identity_is_unknown",
                    "--nocapture",
                ])
                .env("LLM_INDEX_FOREIGN_BASE", &base)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            std::fs::remove_dir_all(&base).unwrap();
            assert!(out.status.success(), "{stderr}");
            let done: serde_json::Value = serde_json::from_str(
                stdout
                    .lines()
                    .find(|line| line.starts_with("{\"message\":\"ScanDone\""))
                    .expect("ScanDone message"),
            )
            .unwrap();
            assert_eq!(done["complete"], false);
            assert!(done["identity_before"].is_null());
            assert!(done["identity_after"].is_null());
            return;
        };
        let cfg = base.join("config.toml");
        std::fs::write(
            &cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"r\"\npath = \"{}\"\n",
                base.join("state").display(),
                base.join("root").display()
            ),
        )
        .unwrap();
        let config = crate::config::load(&cfg).unwrap();
        let root_fd = openat2(
            CWD,
            &config.roots[0].path,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .unwrap();
        // A mount id that is never the root fd's: the line parses, but off the fd's mount.
        let fake_id = root_mount_id(&root_fd).unwrap() + 1;
        let info = base.join("mountinfo");
        std::fs::write(
            &info,
            format!(
                "{fake_id} 0 0:1 / {} rw - ext4 /dev/fake rw\n",
                config.roots[0].path.display()
            ),
        )
        .unwrap();
        let mut mountinfo = File::open(&info).unwrap();
        assert!(
            mount_state(&mut mountinfo, &config.roots[0].path)
                .unwrap()
                .is_some()
        );
        let mut ready = Ready {
            root: Some(0),
            root_fd: Some(root_fd),
            mountinfo,
            global_ignore: Gitignore::empty(),
            oom_score_adj: String::new(),
            config,
        };
        println!(); // libtest may have written "test ... " without a newline
        scan(&mut ready, &Output::new()).unwrap();
    }

    #[test]
    fn mountinfo_identity() {
        let data = "1 0 0:1 / / rw - ext4 /dev/root rw\n2 1 0:2 / /mnt/nas rw - nfs old rw\n3 1 0:3 / /mnt/nas rw - nfs new rw\n4 3 0:4 / /mnt/nas/x rw - tmpfs tmpfs rw\n5 1 0:5 / /mnt/nas2 rw - tmpfs tmpfs rw\n6 1 0:6 /a\\040b /space\\040name rw - fuse.x\\040y  rw";
        let mut cursor = Cursor::new(data);
        let identity = mount_identity(&mut cursor, Path::new("/mnt/nas"))
            .unwrap()
            .unwrap();
        assert_eq!(identity.share.fstype, "nfs");
        assert_eq!(identity.share.source, "new");
        assert_eq!(identity.share.mount_root, "/");
        assert_eq!(identity.share.mount_point, "/mnt/nas");
        assert_eq!(identity.mount_id, 3);
        assert_eq!(identity.submounts, ["/mnt/nas/x"]);
        let escaped = mount_identity(&mut cursor, Path::new("/space name"))
            .unwrap()
            .unwrap();
        assert_eq!(escaped.share.fstype, "fuse.x y");
        assert_eq!(escaped.share.source, "");
        assert_eq!(escaped.share.mount_root, "/a b");
        assert_eq!(escaped.share.mount_point, "/space name");
        assert_eq!(escaped.mount_id, 6);
    }

    #[test]
    fn swapped_directory_is_not_followed() {
        let base = std::env::temp_dir().join(format!("llm-index-swap-{}", std::process::id()));
        std::fs::create_dir_all(base.join("root/child")).unwrap();
        let root = openat2(
            CWD,
            base.join("root"),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .unwrap();
        std::fs::rename(base.join("root/child"), base.join("moved")).unwrap();
        std::os::unix::fs::symlink(&base, base.join("root/child")).unwrap();
        let name = c"child";
        assert!(open_dir(&root, name).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn unsafe_names_are_skipped_first() {
        let base = std::env::temp_dir().join(format!("llm-index-unsafe-{}", std::process::id()));
        std::fs::create_dir_all(base.join("root")).unwrap();
        let cfg = base.join("config.toml");
        std::fs::write(
            &cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"r\"\npath = \"{}\"\n",
                base.join("state").display(),
                base.join("root").display()
            ),
        )
        .unwrap();
        let config = crate::config::load(&cfg).unwrap();
        let root_fd = openat2(
            CWD,
            &config.roots[0].path,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .unwrap();
        let ready = Ready {
            root: Some(0),
            root_fd: Some(root_fd),
            mountinfo: File::open("/proc/self/mountinfo").unwrap(),
            global_ignore: Gitignore::empty(),
            oom_score_adj: String::new(),
            config,
        };
        let walker = Walker::new(&ready).unwrap();
        let root = &ready.config.roots[0].path;
        for is_dir in [false, true] {
            assert_eq!(
                walker.reason(&root.join("a\u{202e}b"), is_dir),
                Some("unsafe_name")
            );
            assert_eq!(
                walker.reason(&root.join("a\nb"), is_dir),
                Some("unsafe_name")
            );
            assert_eq!(walker.reason(&root.join("notes"), is_dir), None);
        }
        std::fs::remove_dir_all(base).unwrap();
    }
}
