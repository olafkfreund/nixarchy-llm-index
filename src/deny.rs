use std::path::Path;

pub const BUILTIN: &[&str] = &[
    ".env*",
    "*.pem",
    "*.key",
    "id_rsa*",
    "id_dsa*",
    "id_ecdsa*",
    "id_ed25519*",
    "*.p12",
    "*.pfx",
    "*.kdbx",
    "*.age",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".docker/config.json",
    ".cargo/credentials*",
    ".ssh/",
    ".gnupg/",
    ".password-store/",
    ".aws/",
    ".azure/",
    ".kube/",
    ".config/gcloud/",
    ".config/gh/",
    ".config/sops/",
    ".local/share/keyrings/",
    ".claude/",
    ".codex/",
    ".gemini/",
    ".antigravity/",
    ".config/Antigravity/",
    ".mozilla/",
    ".thunderbird/",
    ".config/google-chrome/",
    ".config/chromium/",
    ".config/BraveSoftware/",
    ".config/vivaldi/",
    "/run/",
    "/proc/",
    "/sys/",
];

/// Match built-in patterns against an absolute path without touching the filesystem.
pub fn denied(abspath: &str, state_dir: &Path, lifts: &[String]) -> bool {
    if !crate::config::normal_absolute(Path::new(abspath)) {
        return true;
    }
    if Path::new(&abspath.to_lowercase())
        .starts_with(Path::new(&state_dir.to_string_lossy().to_lowercase()))
    {
        return true;
    }
    BUILTIN.iter().any(|pattern| {
        !lifts.iter().any(|lift| lift.eq_ignore_ascii_case(pattern)) && matches(abspath, pattern)
    })
}

fn matches(abspath: &str, pattern: &str) -> bool {
    let path = abspath.to_lowercase();
    let pattern = pattern.to_lowercase();
    let anchored = pattern.starts_with('/');
    let directory = pattern.ends_with('/');
    let parts: Vec<_> = pattern.trim_matches('/').split('/').collect();
    let path_parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    if parts.len() > path_parts.len() {
        return false;
    }
    let starts = if anchored {
        0..1
    } else {
        0..path_parts.len() - parts.len() + 1
    };
    // A match on any run of components denies that file or directory and everything below
    // it (gitignore semantics); a trailing `/` additionally requires the match to be a
    // directory, i.e. not the last component.
    starts.into_iter().any(|start| {
        (!directory || start + parts.len() < path_parts.len())
            && parts
                .iter()
                .zip(&path_parts[start..])
                .all(|(pat, part)| glob_component(pat, part))
    })
}

fn glob_component(pattern: &str, value: &str) -> bool {
    let pieces: Vec<_> = pattern.split('*').collect();
    if pieces.len() == 1 {
        return pattern == value;
    }
    let mut remaining = value;
    if !remaining.starts_with(pieces[0]) {
        return false;
    }
    remaining = &remaining[pieces[0].len()..];
    for piece in &pieces[1..pieces.len() - 1] {
        let Some(pos) = remaining.find(piece) else {
            return false;
        };
        remaining = &remaining[pos + piece.len()..];
    }
    remaining.ends_with(pieces[pieces.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_patterns() {
        let state = Path::new("/home/x/.local/state/llm-index");
        let denied = |path| denied(path, state, &[]);
        assert!(denied("/home/x/.config/gcloud/creds"));
        assert!(denied("/home/x/.CONFIG/GCLOUD/x"));
        assert!(!denied("/home/x/id_map.rs"));
        assert!(denied("/home/x/id_ed25519"));
        assert!(denied("/home/x/.ssh/sub/file"));
        assert!(denied("/run/user/1000/agenix/x"));
        assert!(denied("/run/x"));
        assert!(!denied("/home/x/run/x"));
        assert!(denied("/home/x/.local/state/llm-index/data"));
        assert!(denied("/home/x//.ssh/key"));
        // patterns without a trailing slash also deny directories they name, and their contents
        assert!(denied("/home/x/proj/.envs/.production/.django"));
        assert!(denied("/home/x/id_rsa.d/work"));
        assert!(denied("/home/x/.cargo/credentials.d/token"));
        assert!(!denied("/home/x/envs/file"));
    }
}
