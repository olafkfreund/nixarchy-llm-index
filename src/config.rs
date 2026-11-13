use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    // A parse error quotes the offending source line: escape unsafe characters in it.
    let config: Config = toml::from_str(&fs::read_to_string(path)?)
        .map_err(|e| crate::sanitize::escape_unsafe(&e.to_string()))?;
    config.validate()?;
    Ok(config)
}

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("invalid duration: {}", crate::sanitize::escape_unsafe(s));
    let (i, unit) = s.char_indices().last().ok_or_else(bad)?;
    let n: u64 = s[..i].parse().map_err(|_| bad())?;
    let mult: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return Err(bad()),
    };
    match n.checked_mul(mult) {
        Some(secs) if secs > 0 => Ok(Duration::from_secs(secs)),
        _ => Err(bad()),
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), Box<dyn Error>> {
        // First: later messages interpolate paths, which must not carry control characters.
        let unsafe_path = |p: &Path| p.to_str().is_none_or(crate::sanitize::unsafe_path);
        if unsafe_path(&self.state_dir)
            || self.roots.iter().any(|r| unsafe_path(&r.path))
            || self.exclude.paths.iter().any(|p| unsafe_path(p))
        {
            return Err("path contains control or invisible characters".into());
        }
        if !normal_absolute(&self.state_dir) {
            return Err("state_dir must be an absolute, normalized path".into());
        }
        if self.limits.workers < 2 {
            return Err("workers must be at least 2".into());
        }
        if self.limits.max_file_mb == 0 {
            return Err("max_file_mb must be at least 1".into());
        }
        parse_duration(&self.limits.backoff_max)?;
        let mut names = HashSet::new();
        let mut paths = HashSet::new();
        for root in &self.roots {
            if root.name.is_empty() || !names.insert(&root.name) {
                return Err("root names must be nonempty and unique".into());
            }
            parse_duration(&root.interval)?;
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
    fn durations_and_limits_are_validated() {
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        for bad in ["5µ", "0s", "", "s", "99999999999999999d", "1\u{202e}"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert!(
            !parse_duration("1\u{202e}")
                .unwrap_err()
                .contains('\u{202e}')
        );
        let base = |extra: &str| -> Config {
            toml::from_str(&format!(
                "state_dir = \"/state\"\n{extra}[[root]]\nname = \"a\"\npath = \"/home/x\"\n"
            ))
            .unwrap()
        };
        assert!(base("").validate().is_ok());
        assert!(base("[limits]\nmax_file_mb = 0\n").validate().is_err());
        assert!(base("[limits]\nbackoff_max = \"0s\"\n").validate().is_err());
        let c: Config = toml::from_str(
            "state_dir = \"/state\"\n[[root]]\nname = \"a\"\npath = \"/home/x\"\ninterval = \"5µ\"\n",
        )
        .unwrap();
        assert!(c.validate().is_err());
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

    #[test]
    fn parse_errors_do_not_echo_unsafe_characters() {
        let p = std::env::temp_dir().join(format!("llm-index-badtoml-{}.toml", std::process::id()));
        fs::write(&p, "state_dir = \"/s\"\nbad\u{202e}key = = 1\n").unwrap();
        let e = load(&p).unwrap_err().to_string();
        fs::remove_file(&p).unwrap();
        assert!(!e.contains('\u{202e}'), "{e}");
        assert!(e.contains("\\u{202e}"), "{e}");
    }

    #[test]
    fn unsafe_paths_are_refused_first_without_naming_them() {
        let msg = "path contains control or invisible characters";
        let base = |extra: &str| -> Config {
            toml::from_str(&format!(
                "state_dir = \"/state\"\n[[root]]\nname = \"a\"\npath = \"/home/x\"\n{extra}"
            ))
            .unwrap()
        };
        let cases = [
            "[[root]]\nname = \"b\"\npath = \"/home/y\u{202e}\"\n",
            "[exclude]\npaths = [\"/home/x/a\u{202e}\"]\n",
            // duplicate unsafe roots: the unsafe message wins and the path is not echoed
            "[[root]]\nname = \"b\"\npath = \"/home/x\u{202e}\"\n[[root]]\nname = \"c\"\npath = \"/home/x\u{202e}\"\n",
        ];
        for extra in cases {
            let e = base(extra).validate().unwrap_err().to_string();
            assert_eq!(e, msg);
        }
        let mut c = base("");
        c.state_dir = "/sta\u{202e}te".into();
        assert_eq!(c.validate().unwrap_err().to_string(), msg);
        assert!(base("").validate().is_ok());
    }
}
