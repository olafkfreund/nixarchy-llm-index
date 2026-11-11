use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub state_dir: PathBuf,
    #[serde(default, rename = "root")]
    pub roots: Vec<Root>,
    #[serde(default)]
    pub exclude: Exclude,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub tier: Tier,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default = "default_interval")]
    pub interval: String,
    pub share: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Public,
    #[default]
    Private,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Auto,
    Local,
    Network,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclude {
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    #[serde(default)]
    pub lift_builtin: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
    #[serde(default = "default_max_text_bytes")]
    pub max_text_bytes: usize,
    #[serde(default = "default_max_cells")]
    pub max_cells: u64,
    #[serde(default = "default_memory_max_mb")]
    pub memory_max_mb: u64,
    #[serde(default = "default_workers")]
    pub workers: usize,
    #[serde(default = "default_backoff_max")]
    pub backoff_max: String,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_mb: default_max_file_mb(),
            max_text_bytes: default_max_text_bytes(),
            max_cells: default_max_cells(),
            memory_max_mb: default_memory_max_mb(),
            workers: default_workers(),
            backoff_max: default_backoff_max(),
        }
    }
}

fn default_interval() -> String {
    "15m".into()
}
fn default_max_file_mb() -> u64 {
    50
}
fn default_max_text_bytes() -> usize {
    32 * 1024 * 1024
}
fn default_max_cells() -> u64 {
    2_000_000
}
fn default_memory_max_mb() -> u64 {
    1024
}
fn default_workers() -> usize {
    2
}
fn default_backoff_max() -> String {
    "60m".into()
}

pub fn default_path() -> Result<PathBuf, Box<dyn Error>> {
    // XDG spec: an empty or relative XDG_CONFIG_HOME is ignored; never resolve against the CWD.
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir.join("llm-index/config.toml"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or("no absolute XDG_CONFIG_HOME or HOME")?;
    Ok(home.join(".config/llm-index/config.toml"))
}

pub fn load(path: &Path) -> Result<Config, Box<dyn Error>> {
    let config: Config = toml::from_str(&fs::read_to_string(path)?)?;
    config.validate()?;
    Ok(config)
}

impl Config {
    pub fn validate(&self) -> Result<(), Box<dyn Error>> {
        if !normal_absolute(&self.state_dir) {
            return Err("state_dir must be an absolute, normalized path".into());
        }
        if self.limits.workers < 2 {
            return Err("workers must be at least 2".into());
        }
        let mut names = HashSet::new();
        let mut paths = HashSet::new();
        for root in &self.roots {
            if root.name.is_empty() || !names.insert(&root.name) {
                return Err("root names must be nonempty and unique".into());
            }
            if !paths.insert(&root.path) {
                return Err(format!("duplicate root path: {}", root.path.display()).into());
            }
            if !normal_absolute(&root.path) {
                return Err(format!("invalid root path: {}", root.path.display()).into());
            }
        }
        for path in &self.exclude.paths {
            if !normal_absolute(path) || !self.roots.iter().any(|root| path.starts_with(&root.path))
            {
                return Err(format!("invalid exclusion: {}", path.display()).into());
            }
        }
        Ok(())
    }
}

pub(crate) fn normal_absolute(path: &Path) -> bool {
    let Some(value) = path.to_str() else {
        return false;
    };
    value.starts_with('/')
        && (value == "/"
            || value[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_corrupt_and_outside_exclusion() {
        assert!(load(Path::new("/surely/nonexistent/llm-index-config.toml")).is_err());
        assert!(toml::from_str::<Config>("not valid toml = [").is_err());
        let corrupt =
            std::env::temp_dir().join(format!("llm-index-corrupt-{}.toml", std::process::id()));
        fs::write(&corrupt, "not valid toml = [").unwrap();
        assert!(load(&corrupt).is_err());
        fs::remove_file(&corrupt).unwrap();
        let config: Config = toml::from_str(
            r#"
            state_dir = "/state"
            [[root]]
            name = "docs"
            path = "/home/x/docs"
            [exclude]
            paths = ["/home/x/other"]
        "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_unsafe_paths_and_workers() {
        let mut config: Config = toml::from_str(
            r#"
            state_dir = "/state"
            [[root]]
            name = "docs"
            path = "/home/x/docs"
            [limits]
            workers = 1
        "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
        config.limits.workers = 2;
        config.roots[0].path = "/home/x/../other".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_root_paths() {
        let config: Config = toml::from_str(
            r#"
            state_dir = "/state"
            [[root]]
            name = "a"
            path = "/home/x/docs"
            tier = "private"
            [[root]]
            name = "b"
            path = "/home/x/docs"
            tier = "public"
        "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
