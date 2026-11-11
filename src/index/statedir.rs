//! The state directory: private, local, and not inside a synced tree.
use std::error::Error;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
# This file is a cache directory tag created by llm-index.\n";

/// statfs magic numbers of network and FUSE filesystems.
const NETWORK_MAGIC: &[u64] = &[
    0x6969,     // NFS
    0x517B,     // SMB
    0xFF534D42, // CIFS
    0xFE534D42, // SMB2
    0x65735546, // FUSE
    0x01021997, // 9p
    0x73757245, // Coda
    0x5346414F, // AFS
    0x00C36400, // Ceph
    0x187,      // autofs
];

fn sync_marker(name: &str) -> bool {
    matches!(
        name,
        ".stfolder" | ".sync" | ".dropbox" | ".nextcloudsync.log"
    ) || (name.starts_with(".sync_") && name.ends_with(".db"))
}

/// No ancestor of `dir` may hold a sync-client marker.
pub fn check_parents(dir: &Path) -> Result<(), Box<dyn Error>> {
    for parent in dir.ancestors().skip(1) {
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        for e in entries.flatten() {
            if sync_marker(&e.file_name().to_string_lossy()) {
                return Err(format!(
                    "state dir {} is inside a synced folder ({} has {:?})",
                    dir.display(),
                    parent.display(),
                    e.file_name()
                )
                .into());
            }
        }
    }
    Ok(())
}

pub fn prepare(dir: &Path) -> Result<(), Box<dyn Error>> {
    check_parents(dir)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let magic = rustix::fs::statfs(dir)?.f_type as u64;
    if NETWORK_MAGIC.contains(&magic) {
        return Err(format!(
            "state dir {} is on a network or FUSE filesystem",
            dir.display()
        )
        .into());
    }
    let tag = dir.join("CACHEDIR.TAG");
    if !tag.exists() {
        fs::write(&tag, CACHEDIR_TAG)?;
        fs::set_permissions(&tag, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_private_dir_with_cache_tag() {
        let base = crate::db::tests::tmp("statedir");
        let dir = base.join("a/state");
        prepare(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            fs::read_to_string(dir.join("CACHEDIR.TAG"))
                .unwrap()
                .starts_with("Signature: 8a477f597d28d172789f06886806bc55")
        );
    }

    #[test]
    fn refuses_a_synced_parent() {
        for marker in [
            ".stfolder",
            ".sync",
            ".dropbox",
            ".nextcloudsync.log",
            ".sync_x1.db",
        ] {
            let base = crate::db::tests::tmp("synced");
            fs::write(base.join(marker), "").unwrap();
            let err = prepare(&base.join("deep/state")).unwrap_err().to_string();
            assert!(err.contains("synced folder"), "{marker}: {err}");
            assert!(
                !base.join("deep").exists(),
                "nothing created before the check"
            );
        }
    }
}
