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

#[derive(Default)]
pub struct Validated {
    pub paths: HashSet<blake3::Hash>,
    pub config_error: bool,
}

impl Validated {
    pub fn contains(&self, path: &Path) -> bool {
        path.to_str()
            .is_some_and(|value| self.paths.contains(&blake3::hash(value.as_bytes())))
    }
}

/// Call before every public tool; the error deliberately contains no path.
pub fn public_gate(
    config: &Config,
    states: &RootStates,
    validated: &Validated,
) -> Result<(), &'static str> {
    if validated.config_error
        || config
            .roots
            .iter()
            .any(|root| states.get(&root.name) == Some(&RootState::ConfigError))
        || config
            .roots
            .iter()
            .any(|root| !validated.contains(&root.path))
        || config
            .exclude
            .paths
            .iter()
            .any(|path| !validated.contains(path))
    {
        return Err(PUBLIC_CHECKING);
    }
    Ok(())
}

/// Return the tier permitted by the current configuration, using strings only.
pub fn visible(
    abspath: &str,
    config: &Config,
    states: &RootStates,
    validated: &Validated,
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
        || (root.tier == Tier::Public && public_gate(config, states, validated).is_err())
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
            result
                .paths
                .insert(blake3::hash(root.path.to_str().unwrap().as_bytes()));
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
        assert!(!PUBLIC_CHECKING.contains("/home"));
        assert_eq!(visible("/home/x/a", &config, &states, &validated), None);
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
        assert_eq!(
            visible("/home/x/.ssh/a", &config, &states, &validated),
            None
        );
    }

    #[test]
    fn sandboxed_string_logic() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "auth::tests::sandbox_child", "--nocapture"])
            .env("LLM_INDEX_SANDBOX_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
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
