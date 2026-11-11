//! Share identity, the adoption rule and the one-live-worker-per-share book.
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use crate::proto::Share;
use crate::worker::scan::{ReadSeek, mount_identity};

/// Monotonic time since the coordinator started; tests drive it by hand.
pub type Time = Duration;
pub type WorkerId = u64;

const FIRST_BACKOFF: Duration = Duration::from_secs(60);

pub fn is_network(fstype: &str) -> bool {
    matches!(
        fstype,
        "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "autofs" | "fuse"
    ) || fstype.starts_with("fuse.")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Derived {
    pub share: Share,
    pub mount_id: u64,
}

/// String match of `path` against mountinfo; never touches `path` itself.
pub fn derive(mountinfo: &mut impl ReadSeek, path: &Path) -> Derived {
    match mount_identity(mountinfo, path) {
        Ok(Some(i)) => Derived {
            share: i.share,
            mount_id: i.mount_id,
        },
        _ => Derived {
            share: Share {
                fstype: "unknown".into(),
                source: String::new(),
                mount_root: "/".into(),
                mount_point: path.to_string_lossy().into_owned(),
            },
            mount_id: 0,
        },
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Verdict {
    Ok,
    /// Record this identity as the expected one (first scan, or a deeper mount over an empty root).
    Adopt,
    /// autofs: scan once to trigger the real mount; the result is not accepted.
    Probe,
    Mismatch,
}

pub fn verdict(
    expected: Option<&Share>,
    configured: Option<&str>,
    current: &Share,
    indexed_files: usize,
) -> Verdict {
    if current.fstype == "autofs" {
        return Verdict::Probe;
    }
    let configured_ok = configured.is_none_or(|s| s == current.source);
    match expected {
        Some(e) if e == current => Verdict::Ok,
        None if configured_ok => Verdict::Adopt,
        Some(e)
            if indexed_files == 0
                && configured_ok
                && Path::new(&current.mount_point).starts_with(&e.mount_point)
                && current.mount_point != e.mount_point =>
        {
            Verdict::Adopt
        }
        _ => Verdict::Mismatch,
    }
}

#[derive(Default)]
struct Entry {
    live: HashSet<WorkerId>,
    backoff: Duration,
    not_before: Time,
}

#[derive(Default)]
pub struct ShareBook {
    map: HashMap<Share, Entry>,
}

impl ShareBook {
    /// No live (unreaped) worker and backoff expired.
    pub fn free(&self, share: &Share, now: Time) -> bool {
        self.map
            .get(share)
            .is_none_or(|e| e.live.is_empty() && now >= e.not_before)
    }
    pub fn charge(&mut self, share: &Share, id: WorkerId) {
        self.map.entry(share.clone()).or_default().live.insert(id);
    }
    /// Every share this worker is charged against (old and new across a re-derive).
    pub fn charged(&self, id: WorkerId) -> Vec<Share> {
        let shares = self.map.iter();
        shares
            .filter(|(_, e)| e.live.contains(&id))
            .map(|(s, _)| s.clone())
            .collect()
    }
    pub fn release(&mut self, id: WorkerId) {
        for e in self.map.values_mut() {
            e.live.remove(&id);
        }
    }
    pub fn offline(&mut self, share: &Share, now: Time, max: Duration) {
        let e = self.map.entry(share.clone()).or_default();
        e.backoff = if e.backoff.is_zero() {
            FIRST_BACKOFF.min(max)
        } else {
            (e.backoff * 2).min(max)
        };
        e.not_before = now + e.backoff;
    }
    pub fn healthy(&mut self, share: &Share) {
        if let Some(e) = self.map.get_mut(share) {
            e.backoff = Duration::ZERO;
            e.not_before = Duration::ZERO;
        }
    }
    #[cfg(test)]
    pub fn max_live(&self) -> usize {
        self.map.values().map(|e| e.live.len()).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(point: &str, source: &str) -> Share {
        Share {
            fstype: "nfs".into(),
            source: source.into(),
            mount_root: "/".into(),
            mount_point: point.into(),
        }
    }

    #[test]
    fn adoption_rules() {
        let a = share("/mnt/nas", "nas:/a");
        let deeper = share("/mnt/nas/x", "nas:/b");
        assert_eq!(verdict(None, None, &a, 0), Verdict::Adopt);
        assert_eq!(verdict(Some(&a), None, &a, 5), Verdict::Ok);
        assert_eq!(verdict(Some(&a), None, &deeper, 0), Verdict::Adopt);
        assert_eq!(verdict(Some(&a), None, &deeper, 1), Verdict::Mismatch);
        assert_eq!(
            verdict(Some(&a), None, &share("/mnt/other", "x"), 0),
            Verdict::Mismatch
        );
        assert_eq!(verdict(None, Some("nas:/zzz"), &a, 0), Verdict::Mismatch);
        let mut auto = a.clone();
        auto.fstype = "autofs".into();
        assert_eq!(verdict(None, Some("nas:/a"), &auto, 0), Verdict::Probe);
    }

    #[test]
    fn backoff_doubles_to_max_and_resets() {
        let s = share("/m", "s");
        let mut book = ShareBook::default();
        let max = Duration::from_secs(180);
        book.offline(&s, Duration::ZERO, max);
        assert!(!book.free(&s, Duration::from_secs(59)));
        assert!(book.free(&s, Duration::from_secs(60)));
        book.offline(&s, Duration::from_secs(60), max);
        assert!(book.free(&s, Duration::from_secs(180)));
        book.offline(&s, Duration::from_secs(180), max);
        book.offline(&s, Duration::from_secs(400), max);
        assert!(!book.free(&s, Duration::from_secs(579)));
        assert!(book.free(&s, Duration::from_secs(580)));
        book.healthy(&s);
        book.charge(&s, 1);
        assert!(!book.free(&s, Duration::from_secs(10_000)));
        book.release(1);
        assert!(book.free(&s, Duration::ZERO));
    }
}
