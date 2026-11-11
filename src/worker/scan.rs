use std::error::Error;
use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rustix::fs::{
    AtFlags, CWD, Dir, Mode, OFlags, ResolveFlags, Statx, StatxFlags, fstat, openat, openat2, statx,
};

use super::{Output, sandbox::Ready};
use crate::config::Tier;
use crate::deny;
use crate::proto::{Message, MountIdentity, Share, ValidationResult};

const SKIP_DIRS: &[&str] = &[
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
const LOCKFILES: &[&str] = &[
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
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);

fn recoverable(error: rustix::io::Errno) -> bool {
    matches!(error, rustix::io::Errno::ACCESS | rustix::io::Errno::NOENT)
}

fn errno_reason(error: rustix::io::Errno) -> String {
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
        for part in rel.components() {
            fd = open_dir(&fd, &CString::new(part.as_os_str().as_bytes())?)?;
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
                            if statx(
                                &child,
                                "CACHEDIR.TAG",
                                AtFlags::SYMLINK_NOFOLLOW,
                                StatxFlags::TYPE,
                            )
                            .is_ok_and(|tag| tag.stx_mode & 0o170000 == 0o100000)
                            {
                                visit(&child_rel, &st, None, Some("cache"))?;
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
    let fd_id = match root_mount_id(ready.root_fd.as_ref().ok_or("missing root fd")?) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("scan root mount id: {error}");
            return output.send(&Message::ScanDone {
                complete: false,
                identity_before: before,
                identity_after: None,
            });
        }
    };
    let mut complete = before.as_ref().is_some_and(|m| m.mount_id == fd_id);
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
                mtime_ns: st.stx_mtime.tv_sec * 1_000_000_000 + i64::from(st.stx_mtime.tv_nsec),
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
    complete &= before == after && after.as_ref().is_some_and(|m| Some(m.mount_id) == after_id);
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
}
