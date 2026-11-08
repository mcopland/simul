//! The agents' own live session files: Claude Code's `sessions/<pid>.json` and Kiro's session
//! logs, which report states no hook fires for.

use anyhow::Result;

use super::Daemon;
use crate::protocol::now_ms;
use crate::{claude_sessions, kiro_sessions, log};

use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct LiveFiles {
    /// Where Claude Code keeps live session status; `None` without `$HOME`.
    pub claude_dir: Option<PathBuf>,
    /// Where Kiro CLI keeps its session files; `None` without `$KIRO_HOME` or `$HOME`.
    pub kiro_dir: Option<PathBuf>,
    /// Last failure reading either directory, so a persistent one is logged once.
    error: Option<String>,
}

impl LiveFiles {
    pub fn new(claude_dir: Option<PathBuf>, kiro_dir: Option<PathBuf>) -> LiveFiles {
        LiveFiles {
            claude_dir,
            kiro_dir,
            error: None,
        }
    }

    /// Records the latest read's failure, if any, and returns it when it is new and worth
    /// logging.
    pub fn report(&mut self, error: Option<String>) -> Option<String> {
        let new = if error.is_some() && error != self.error {
            error.clone()
        } else {
            None
        };
        self.error = error;
        new
    }
}

impl Daemon {
    /// Applies Claude's and Kiro's own session status; it covers Esc, denials, interrupts, and
    /// Kiro's approvals, which fire no hook.
    pub(super) async fn read_live(&mut self) {
        let claude_dir = self.live.claude_dir.clone();
        let kiro_dir = self.live.kiro_dir.clone();
        if claude_dir.is_none() && kiro_dir.is_none() {
            return;
        }
        let kiro_panes = self.store.kiro_panes();
        let read = tokio::task::spawn_blocking(move || {
            let mut found = claude_sessions::Sessions::default();
            let mut merge = |read: Result<claude_sessions::Sessions>| match read {
                Ok(s) => {
                    found.live.extend(s.live);
                    found.skipped.extend(s.skipped);
                }
                Err(e) => found.skipped.push(format!("{e:#}")),
            };
            if let Some(dir) = claude_dir {
                merge(claude_sessions::read(&dir, claude_sessions::pid_alive));
            }
            if let Some(dir) = kiro_dir {
                merge(kiro_sessions::read(
                    &dir,
                    &kiro_panes,
                    claude_sessions::pid_alive,
                ));
            }
            found
        })
        .await;
        // Panes a failed source covered lose their live status, so the screen scrape takes over.
        let (live, error) = match read {
            Ok(found) => {
                let error = (!found.skipped.is_empty()).then(|| found.skipped.join("; "));
                (found.live, error)
            }
            Err(e) => (Vec::new(), Some(format!("session reader panicked: {e}"))),
        };
        if let Some(error) = self.live.report(error) {
            log!("{error}");
        }
        let changed = self.store.apply_live(&live, now_ms());
        if !changed.is_empty() {
            self.on_transitions(changed, "session");
            self.publish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_persistent_failure_is_reported_once() {
        let mut l = LiveFiles::default();
        assert_eq!(l.report(None), None);
        assert_eq!(l.report(Some("bad".into())).as_deref(), Some("bad"));
        assert_eq!(l.report(Some("bad".into())), None, "same failure again");
        assert_eq!(l.report(Some("worse".into())).as_deref(), Some("worse"));
        assert_eq!(l.report(None), None, "recovery is not a failure");
        assert_eq!(
            l.report(Some("bad".into())).as_deref(),
            Some("bad"),
            "a failure after recovery is new"
        );
    }
}
