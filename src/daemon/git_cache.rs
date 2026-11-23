//! Git status by working directory, as last read for the panes the daemon tracks or shows.

use super::{Cmd, Daemon};
use crate::log;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::protocol::GitStatus;

/// The least time between two reads of a directory that an event, rather than the fallback,
/// asks for; an agent's tool calls can come many times a second.
const GIT_MIN_GAP: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub struct GitCache {
    /// Status by directory, with when it was read; `None` outside a repo.
    statuses: HashMap<String, (Instant, Option<GitStatus>)>,
    /// Directories being read.
    inflight: HashSet<String>,
}

impl GitCache {
    pub fn get(&self, cwd: &str) -> Option<&GitStatus> {
        self.statuses.get(cwd).and_then(|(_, s)| s.as_ref())
    }

    /// Whether `cwd` has been read at all, repo or not.
    pub fn knows(&self, cwd: &str) -> bool {
        self.statuses.contains_key(cwd)
    }

    /// Forgets directories no agent or shown pane is in, and returns the ones to read: agents'
    /// directories never read, and shown panes' directories missing or older than `every`.
    /// A directory can appear twice; `start` drops the repeat.
    pub fn stale(
        &mut self,
        agents: &HashSet<String>,
        panes: &HashSet<String>,
        now: Instant,
        every: Duration,
    ) -> Vec<String> {
        self.statuses
            .retain(|cwd, _| agents.contains(cwd) || panes.contains(cwd));
        agents
            .iter()
            .filter(|cwd| !self.statuses.contains_key(*cwd))
            .chain(panes.iter().filter(|cwd| {
                self.statuses
                    .get(*cwd)
                    .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= every)
            }))
            .cloned()
            .collect()
    }

    /// Whether an event may read `cwd` again: it was never read, or `GIT_MIN_GAP` ago.
    pub fn nudge(&self, cwd: &str, now: Instant) -> bool {
        self.statuses
            .get(cwd)
            .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= GIT_MIN_GAP)
    }

    /// Marks `cwd` as being read; false if it is empty or already being read.
    pub fn start(&mut self, cwd: &str) -> bool {
        !cwd.is_empty() && self.inflight.insert(cwd.to_owned())
    }

    /// Records a finished read; true if the status differs from the last one known.
    pub fn finish(&mut self, cwd: String, status: Option<GitStatus>, now: Instant) -> bool {
        self.inflight.remove(&cwd);
        let changed = self
            .statuses
            .get(&cwd)
            .is_none_or(|(_, old)| *old != status);
        self.statuses.insert(cwd, (now, status));
        changed
    }
}

impl Daemon {
    /// Looks up agents' directories once, since panes found by the startup scan send no hook
    /// until used, and while the sidebar shows them, every pane's directory once it is
    /// `git_refresh_secs` old. Events read a directory sooner; see `fetch_git`'s callers.
    pub(super) fn refresh_git(&mut self) {
        let agents: HashSet<String> = self.store.cwds().map(str::to_owned).collect();
        let panes: HashSet<String> = if self.sidebar.on {
            self.windows
                .iter()
                .flat_map(|w| &w.panes)
                .map(|p| p.cwd.clone())
                .collect()
        } else {
            HashSet::new()
        };
        let every = Duration::from_secs(self.opts.config.panes.git_refresh_secs);
        for cwd in self.git.stale(&agents, &panes, Instant::now(), every) {
            self.fetch_git(cwd);
        }
    }

    pub(super) fn fetch_git(&mut self, cwd: String) {
        if !self.git.start(&cwd) {
            return;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let status = match crate::git::status(&cwd).await {
                Ok(s) => s,
                Err(e) => {
                    log!("{e:#}");
                    None
                }
            };
            // The daemon loop only stops at shutdown, when the status no longer matters.
            let _ = tx.send(Cmd::Git { cwd, status }).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(dirs: &[&str]) -> HashSet<String> {
        dirs.iter().map(|d| (*d).to_owned()).collect()
    }

    fn on(branch: &str) -> Option<GitStatus> {
        Some(GitStatus {
            branch: Some(branch.to_owned()),
            ..GitStatus::default()
        })
    }

    #[test]
    fn unread_directories_are_stale() {
        let mut c = GitCache::default();
        let mut stale = c.stale(&set(&["/a"]), &set(&["/b"]), Instant::now(), Duration::ZERO);
        stale.sort();
        assert_eq!(stale, ["/a", "/b"]);
    }

    #[test]
    fn agent_directories_are_read_once_but_shown_ones_refresh() {
        let mut c = GitCache::default();
        let t0 = Instant::now();
        c.finish("/a".into(), on("main"), t0);
        c.finish("/b".into(), on("main"), t0);
        let every = Duration::from_secs(60);
        let later = t0 + every;
        assert!(c.stale(&set(&["/a"]), &set(&["/b"]), t0, every).is_empty());
        assert_eq!(c.stale(&set(&["/a"]), &set(&["/b"]), later, every), ["/b"]);
    }

    #[test]
    fn shown_directories_wait_out_the_fallback() {
        let mut c = GitCache::default();
        let t0 = Instant::now();
        c.finish("/b".into(), on("main"), t0);
        let every = Duration::from_secs(60);
        let shown = set(&["/b"]);
        assert!(
            c.stale(&set(&[]), &shown, t0 + Duration::from_secs(6), every)
                .is_empty()
        );
        assert_eq!(c.stale(&set(&[]), &shown, t0 + every, every), ["/b"]);
    }

    #[test]
    fn nudges_are_rate_limited() {
        let mut c = GitCache::default();
        let t0 = Instant::now();
        assert!(c.nudge("/a", t0), "never read");
        c.finish("/a".into(), on("main"), t0);
        assert!(!c.nudge("/a", t0 + Duration::from_secs(4)));
        assert!(c.nudge("/a", t0 + GIT_MIN_GAP));
    }

    #[test]
    fn directories_nothing_uses_are_forgotten() {
        let mut c = GitCache::default();
        let t0 = Instant::now();
        c.finish("/gone".into(), on("main"), t0);
        c.stale(&set(&[]), &set(&[]), t0, Duration::ZERO);
        assert!(!c.knows("/gone"));
        assert_eq!(c.get("/gone"), None);
    }

    #[test]
    fn a_directory_is_read_once_at_a_time() {
        let mut c = GitCache::default();
        assert!(!c.start(""), "no directory to read");
        assert!(c.start("/a"));
        assert!(!c.start("/a"), "already being read");
        c.finish("/a".into(), None, Instant::now());
        assert!(c.start("/a"), "read again once finished");
    }

    #[test]
    fn finish_reports_only_changes() {
        let mut c = GitCache::default();
        let t0 = Instant::now();
        assert!(c.finish("/a".into(), on("main"), t0), "first read");
        assert!(!c.finish("/a".into(), on("main"), t0), "same status");
        assert!(c.finish("/a".into(), on("dev"), t0), "new branch");
        assert!(c.finish("/a".into(), None, t0), "no longer a repo");
        assert!(c.knows("/a"), "a known non-repo is still known");
        assert_eq!(c.get("/a"), None);
    }
}
