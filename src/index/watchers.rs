//! Watcher lifecycle for one local root: outside the pool and the share rule.
use std::time::Duration;

use super::shares::{Time, WorkerId};

pub const DEGRADED_POLL: Duration = Duration::from_secs(120);
pub const RECONCILE: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
pub struct WatchState {
    /// Live (unreaped) watcher process; no new one starts until it is reaped.
    pub worker: Option<WorkerId>,
    pub ready: bool,
    pub degraded: bool,
    pub retry_at: Time,
}

impl WatchState {
    pub fn should_spawn(&self, now: Time) -> bool {
        self.worker.is_none() && now >= self.retry_at
    }
    pub fn degrade(&mut self, now: Time) {
        self.ready = false;
        self.degraded = true;
        self.retry_at = now + DEGRADED_POLL;
    }
    /// Scan cadence for a local root: daily while watched, every 2 min while not.
    pub fn local_interval(&self) -> Duration {
        if self.ready { RECONCILE } else { DEGRADED_POLL }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degraded_watcher_waits_for_reap_and_retry() {
        let mut w = WatchState::default();
        assert!(w.should_spawn(Duration::ZERO));
        w.worker = Some(1);
        w.ready = true;
        assert_eq!(w.local_interval(), RECONCILE);
        w.degrade(Duration::from_secs(10));
        assert_eq!(w.local_interval(), DEGRADED_POLL);
        assert!(!w.should_spawn(Duration::from_secs(500)));
        w.worker = None;
        assert!(!w.should_spawn(Duration::from_secs(129)));
        assert!(w.should_spawn(Duration::from_secs(130)));
    }
}
