use std::error::Error;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use ignore::WalkBuilder;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, ResolveFlags, StatxFlags, fstat, openat2, statx};

use super::{Output, sandbox::Ready};
use crate::config::Tier;
use crate::deny;
use crate::proto::Message;

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

pub fn mount_identity(
    mountinfo: &mut impl ReadSeek,
    root: &Path,
) -> Result<Option<String>, Box<dyn Error>> {
    mountinfo.seek(SeekFrom::Start(0))?;
    let mut data = String::new();
    mountinfo.read_to_string(&mut data)?;
    let mut best: Option<(&str, String)> = None;
    for line in data.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<_> = left.split_whitespace().collect();
        let tail: Vec<_> = right.split_whitespace().collect();
        if fields.len() < 5 || tail.len() < 2 {
            continue;
        }
        let mount_point = fields[4];
        if root.starts_with(mount_point)
            && best
                .as_ref()
                .is_none_or(|(p, _)| p.len() < mount_point.len())
        {
            best = Some((
                mount_point,
                format!("{}|{}|{}|{}", tail[0], tail[1], fields[3], mount_point),
            ));
        }
    }
    Ok(best.map(|(_, identity)| identity))
}

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

pub fn scan(ready: &mut Ready, output: &Output) -> Result<(), Box<dyn Error>> {
    let i = ready.root.ok_or("missing root")?;
    let root = &ready.config.roots[i];
    let before = mount_identity(&mut ready.mountinfo, &root.path)?;
    let device_before = fstat(ready.root_fd.as_ref().ok_or("missing root fd")?)?.st_dev;
    let mut complete = before.is_some();
    let mut walk = WalkBuilder::new(&root.path);
    walk.follow_links(false)
        .same_file_system(true)
        .git_global(false)
        .add_custom_ignore_filename(".llmindexignore");
    let root_path = root.path.clone();
    let nested: Vec<_> = ready
        .config
        .roots
        .iter()
        .enumerate()
        .filter(|(n, _)| *n != i)
        .map(|(_, r)| r.path.clone())
        .collect();
    let excluded = ready.config.exclude.paths.clone();
    let state_dir = ready.config.state_dir.clone();
    let lifts = if root.tier == Tier::Private {
        ready.config.exclude.lift_builtin.clone()
    } else {
        vec![]
    };
    let global = ready.global_ignore.clone();
    let out = output.clone();
    walk.filter_entry(move |entry| {
        if !entry.file_type().is_some_and(|t| t.is_dir()) || entry.path() == root_path {
            return true;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy();
        let skip = nested.iter().any(|other| path.starts_with(other))
            || excluded.iter().any(|p| path.starts_with(p))
            || deny::denied(&path.to_string_lossy(), &state_dir, &lifts)
            || global
                .matched_path_or_any_parents(path.strip_prefix(&root_path).unwrap_or(path), true)
                .is_ignore()
            || SKIP_DIRS.contains(&name.as_ref())
            || name.starts_with("result")
            || path.join("CACHEDIR.TAG").exists();
        if skip {
            let relpath = path
                .strip_prefix(&root_path)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            let _ = out.send(&Message::Skipped {
                relpath,
                reason: "directory".into(),
            });
        }
        !skip
    });
    for entry in walk.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let path = entry.path();
        if path == root.path {
            continue;
        }
        let relpath = path
            .strip_prefix(&root.path)?
            .to_string_lossy()
            .into_owned();
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let reason = if entry.path_is_symlink() {
            Some("symlink")
        } else if ready
            .config
            .roots
            .iter()
            .enumerate()
            .filter(|(n, _)| *n != i)
            .map(|(_, r)| &r.path)
            .any(|other| path.starts_with(other))
        {
            Some("nested_root")
        } else if ready
            .config
            .exclude
            .paths
            .iter()
            .any(|p| path.starts_with(p))
        {
            Some("excluded")
        } else if deny::denied(
            &path.to_string_lossy(),
            &ready.config.state_dir,
            if root.tier == Tier::Private {
                &ready.config.exclude.lift_builtin
            } else {
                &[]
            },
        ) {
            Some("denied")
        } else if ready
            .global_ignore
            .matched_path_or_any_parents(path.strip_prefix(&root.path).unwrap_or(path), is_dir)
            .is_ignore()
        {
            Some("ignored")
        } else if SKIP_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
            || (is_dir && entry.file_name().to_string_lossy().starts_with("result"))
        {
            Some("directory")
        } else if is_dir && path.join("CACHEDIR.TAG").exists() {
            Some("cache")
        } else if !is_dir && LOCKFILES.contains(&entry.file_name().to_string_lossy().as_ref()) {
            Some("lockfile")
        } else {
            None
        };
        if let Some(reason) = reason {
            output.send(&Message::Skipped {
                relpath,
                reason: reason.into(),
            })?;
            continue;
        }
        let st = match statx(
            CWD,
            path,
            AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::SIZE | StatxFlags::MTIME,
        ) {
            Ok(st) => st,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        if !is_dir && st.stx_size > ready.config.limits.max_file_mb * 1024 * 1024 {
            output.send(&Message::Skipped {
                relpath,
                reason: "too_large".into(),
            })?;
            continue;
        }
        let mtime_ns = st.stx_mtime.tv_sec * 1_000_000_000 + i64::from(st.stx_mtime.tv_nsec);
        output.send(&Message::Entry {
            relpath,
            size: st.stx_size,
            mtime_ns,
            is_dir,
        })?;
        output.entry();
    }
    let after = mount_identity(&mut ready.mountinfo, &root.path)?;
    complete &= before == after
        && device_before == fstat(ready.root_fd.as_ref().ok_or("missing root fd")?)?.st_dev;
    output.send(&Message::ScanDone {
        complete,
        identity_before: before,
        identity_after: after,
    })
}

pub fn validate(paths: Vec<String>, output: &Output) -> Result<(), Box<dyn Error>> {
    for path in paths {
        let result = validate_path(Path::new(&path));
        output.send(&Message::Validated {
            path,
            result: result.into(),
        })?;
    }
    Ok(())
}

pub fn validate_path(path: &Path) -> &'static str {
    let mut current = path;
    loop {
        match openat2(
            CWD,
            current,
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(_) => return "ok",
            Err(rustix::io::Errno::LOOP) => return "eloop",
            Err(rustix::io::Errno::NOENT) => match current.parent() {
                Some(parent) if parent != current => current = parent,
                _ => return "pending",
            },
            Err(_) => return "pending",
        }
    }
}
