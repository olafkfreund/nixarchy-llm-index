//! Which paths a `validate` worker checks for a root, and how a result is recorded.
use crate::config::Config;

/// Per root: its own path plus every exclusion under it.
pub fn paths(config: &Config) -> Vec<Vec<String>> {
    config
        .roots
        .iter()
        .map(|r| {
            std::iter::once(&r.path)
                .chain(
                    config
                        .exclude
                        .paths
                        .iter()
                        .filter(|p| p.starts_with(&r.path)),
                )
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_its_exclusions() {
        let c: Config = toml::from_str(
            "state_dir=\"/s\"\n[[root]]\nname=\"a\"\npath=\"/a\"\n[[root]]\nname=\"b\"\npath=\"/b\"\n[exclude]\npaths=[\"/a/x\"]\n",
        )
        .unwrap();
        assert_eq!(paths(&c), [vec!["/a", "/a/x"], vec!["/b"]]);
    }
}
