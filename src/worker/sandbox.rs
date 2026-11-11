use std::error::Error;
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::path::Path;

use ignore::gitignore::Gitignore;
use landlock::{
    ABI, Access, AccessFs, AccessNet, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus,
};
use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

use crate::config::{self, Config};

pub struct Ready {
    pub config: Config,
    pub root: Option<usize>,
    pub root_fd: Option<OwnedFd>,
    pub mountinfo: File,
    pub global_ignore: Gitignore,
    pub oom_score_adj: String,
}

pub fn start(
    config_path: &Path,
    role: &str,
    root_name: Option<&str>,
) -> Result<Ready, Box<dyn Error>> {
    let mountinfo = File::open("/proc/self/mountinfo")?;
    let config = config::load(config_path)?;
    let (global_ignore, error) = Gitignore::global();
    if let Some(error) = error {
        return Err(error.into());
    }
    fs::write("/proc/self/oom_score_adj", "1000")?;
    let oom_score_adj = fs::read_to_string("/proc/self/oom_score_adj")?;
    let old = getrlimit(Resource::As);
    let limit = 4 * 1024 * 1024 * 1024;
    setrlimit(
        Resource::As,
        Rlimit {
            current: Some(limit.min(old.maximum.unwrap_or(limit))),
            maximum: old.maximum,
        },
    )?;
    let root = if role == "validate" {
        None
    } else {
        Some(
            config
                .roots
                .iter()
                .position(|r| Some(r.name.as_str()) == root_name)
                .ok_or("unknown root")?,
        )
    };
    let root_fd = if let Some(i) = root {
        match openat2(
            CWD,
            &config.roots[i].path,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => Some(fd),
            Err(rustix::io::Errno::LOOP) => {
                return Err("config_error: root path contains a symlink".into());
            }
            Err(error) => return Err(format!("offline: {error}").into()),
        }
    } else {
        None
    };
    restrict(role, root_fd.as_ref())?;
    Ok(Ready {
        config,
        root,
        root_fd,
        mountinfo,
        global_ignore,
        oom_score_adj,
    })
}

fn restrict(role: &str, root_fd: Option<&OwnedFd>) -> Result<(), Box<dyn Error>> {
    let abi = ABI::V6;
    let mut rules = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .handle_access(AccessNet::from_all(abi))?
        .create()?;
    if role == "validate" {
        rules = rules.add_rule(PathBeneath::new(
            PathFd::new("/")?,
            AccessFs::from_read(abi),
        ))?;
    } else {
        rules = rules
            .add_rule(PathBeneath::new(
                root_fd.ok_or("missing root fd")?,
                AccessFs::from_read(abi),
            ))?
            .add_rule(PathBeneath::new(
                PathFd::new("/nix/store")?,
                AccessFs::from_read(abi),
            ))?;
    }
    let status = rules.restrict_self()?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err("Landlock not fully enforced".into());
    }
    Ok(())
}
