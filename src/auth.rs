use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::{Config, Tier};
use crate::deny;

pub const PUBLIC_CHECKING: &str = "index configuration is being checked";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootState {
    Ok,
    ConfigError,
}

pub type RootStates = HashMap<String, RootState>;

/// BLAKE3 hashes of validated path strings (the `validated` table): `ok` passed the
/// symlink check, `errors` failed it (ELOOP). Only paths in the current config matter.
#[derive(Default)]
pub struct Validated {
    pub ok: HashSet<blake3::Hash>,
    pub errors: HashSet<blake3::Hash>,
}

pub fn path_hash(path: &Path) -> Option<blake3::Hash> {
    path.to_str().map(|value| blake3::hash(value.as_bytes()))
}

impl Validated {
    fn state(&self, path: &Path) -> PathCheck {
        match path_hash(path) {
            Some(h) if self.errors.contains(&h) => PathCheck::Error,
            Some(h) if self.ok.contains(&h) => PathCheck::Ok,
            _ => PathCheck::Pending,
        }
    }
}

#[derive(Eq, PartialEq)]
enum PathCheck {
    Ok,
    Pending,
    Error,
}

/// Call once per public tool call, before any query; the error deliberately contains no
/// path. Only roots and exclusions in the current config are checked, so removing a broken
/// or pending path, or changing a tier to private, never waits for the indexer.
pub fn public_gate(
    config: &Config,
    states: &RootStates,
    validated: &Validated,
) -> Result<(), &'static str> {
    let paths = config
        .roots
        .iter()
        .map(|root| root.path.as_path())
        .chain(config.exclude.paths.iter().map(|path| path.as_path()));
    if config
        .roots
        .iter()
        .any(|root| states.get(&root.name) == Some(&RootState::ConfigError))
    {
        return Err(PUBLIC_CHECKING);
    }
    for path in paths {
        if validated.state(path) != PathCheck::Ok {
            return Err(PUBLIC_CHECKING);
        }
    }
    Ok(())
}

/// Return the tier permitted by the current configuration, using strings only. This is the
/// pure per-row rule shared by both servers and the indexer's purge; the public server must
/// also pass `public_gate` once per call. It never looks at validation state.
pub fn visible(
    abspath: &str,
    config: &Config,
    states: &RootStates,
    _validated: &Validated,
) -> Option<Tier> {
    let path = Path::new(abspath);
    if !crate::config::normal_absolute(path) {
        return None;
    }
    let root = config
        .roots
        .iter()
        .filter(|root| path.starts_with(&root.path))
        .max_by_key(|root| root.path.components().count())?;
    if states.get(&root.name) == Some(&RootState::ConfigError)
        || config
            .exclude
            .paths
            .iter()
            .any(|exclude| path.starts_with(exclude))
    {
        return None;
    }
    let lifts = if root.tier == Tier::Private {
        &config.exclude.lift_builtin[..]
    } else {
        &[]
    };
    if deny::denied(abspath, &config.state_dir, lifts) {
        None
    } else {
        Some(root.tier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_config() -> Config {
        toml::from_str(
            r#"
            state_dir = "/home/x/.local/state/llm-index"
            [[root]]
            name = "home"
            path = "/home/x"
            tier = "public"
            [[root]]
            name = "secret"
            path = "/home/x/private"
            tier = "private"
        "#,
        )
        .unwrap()
    }

    fn validated(config: &Config) -> Validated {
        let mut result = Validated::default();
        for root in &config.roots {
            result.ok.insert(path_hash(&root.path).unwrap());
        }
        result
    }

    #[test]
    fn tiers_and_changes() {
        let mut config = fixture_config();
        let validated = validated(&config);
        let states = RootStates::new();
        assert_eq!(
            visible("/home/x/private/a", &config, &states, &validated),
            Some(Tier::Private)
        );
        config.roots.remove(1);
        assert_eq!(
            visible("/home/x/private/a", &config, &states, &validated),
            Some(Tier::Public)
        );
        config.roots.clear();
        assert_eq!(
            visible("/home/x/private/a", &config, &states, &validated),
            None
        );
        assert!(public_gate(&config, &states, &validated).is_ok());
        let mut stale_states = RootStates::new();
        stale_states.insert("secret".into(), RootState::ConfigError);
        assert!(public_gate(&config, &stale_states, &validated).is_ok());
        config.roots = fixture_config().roots;
        config.roots[0].path = "/other".into();
        assert_eq!(visible("/home/x/a", &config, &states, &validated), None);
        assert_eq!(
            visible("/home/x/../other", &config, &states, &validated),
            None
        );
    }

    #[test]
    fn gate_and_lifts() {
        let mut config = fixture_config();
        let validated = validated(&config);
        let mut states = RootStates::new();
        config.exclude.paths.push("/home/x/new-secret".into());
        assert_eq!(
            public_gate(&config, &states, &validated),
            Err(PUBLIC_CHECKING)
        );
        assert!(!PUBLIC_CHECKING.contains('/'));
        // visible() is the pure per-row rule; the gate is separate, so the private server and
        // the purge are unaffected by a pending public validation
        assert_eq!(
            visible("/home/x/a", &config, &states, &validated),
            Some(Tier::Public)
        );
        assert_eq!(
            visible("/home/x/private/a", &config, &states, &validated),
            Some(Tier::Private)
        );
        config.exclude.paths.clear();
        states.insert("secret".into(), RootState::ConfigError);
        assert_eq!(
            public_gate(&config, &states, &validated),
            Err(PUBLIC_CHECKING)
        );
        states.clear();
        config.exclude.lift_builtin.push(".env*".into());
        assert_eq!(
            visible("/home/x/private/.env", &config, &states, &validated),
            Some(Tier::Private)
        );
        assert_eq!(visible("/home/x/.env", &config, &states, &validated), None);
        config.roots[0].path = "/home/x/.ssh".into();
        let validated = self::validated(&config);
        assert!(public_gate(&config, &states, &validated).is_ok());
        assert_eq!(
            visible("/home/x/.ssh/a", &config, &states, &validated),
            None
        );
    }

    #[test]
    fn gate_follows_current_config_only() {
        let mut config = fixture_config();
        let mut validated = validated(&config);
        let states = RootStates::new();
        // a newly added root (any tier) closes public until validated
        config.roots.push(
            toml::from_str("name = \"child\"\npath = \"/home/x/child\"\ntier = \"private\"")
                .unwrap(),
        );
        assert_eq!(
            public_gate(&config, &states, &validated),
            Err(PUBLIC_CHECKING)
        );
        config.roots.pop();
        // public -> private needs no validation
        config.roots[0].tier = Tier::Private;
        assert!(public_gate(&config, &states, &validated).is_ok());
        assert_eq!(
            visible("/home/x/a", &config, &states, &validated),
            Some(Tier::Private)
        );
        // a path that failed validation closes public; removing it reopens at once
        config.exclude.paths.push("/home/x/bad-link".into());
        validated
            .errors
            .insert(path_hash(Path::new("/home/x/bad-link")).unwrap());
        assert_eq!(
            public_gate(&config, &states, &validated),
            Err(PUBLIC_CHECKING)
        );
        config.exclude.paths.clear();
        assert!(public_gate(&config, &states, &validated).is_ok());
    }

    #[test]
    fn sandboxed_string_logic() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "auth::tests::sandbox_child", "--nocapture"])
            .env("LLM_INDEX_SANDBOX_CHILD", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn sandbox_child() {
        if std::env::var_os("LLM_INDEX_SANDBOX_CHILD").is_none() {
            return;
        }
        use landlock::{AccessFs, Ruleset, RulesetAttr, RulesetStatus};
        let status = Ruleset::default()
            .handle_access(AccessFs::from_read(landlock::ABI::V6))
            .unwrap()
            .create()
            .unwrap()
            .restrict_self()
            .unwrap();
        assert_eq!(status.ruleset, RulesetStatus::FullyEnforced);
        assert!(std::fs::read_to_string("/etc/hosts").is_err());
        let config = fixture_config();
        let validated = validated(&config);
        assert_eq!(
            visible("/home/x/private/a", &config, &RootStates::new(), &validated),
            Some(Tier::Private)
        );
        assert_eq!(
            visible("/home/x/.ssh/key", &config, &RootStates::new(), &validated),
            None
        );
    }
}
