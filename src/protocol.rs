use serde::{Deserialize, Serialize};

use crate::daemon::state::PaneRecord;
use crate::hook::Envelope;

/// One newline-delimited JSON request on the daemon socket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Fire-and-forget; the daemon sends no reply.
    Hook(Envelope),
    List,
    /// Replies with a `Status` now and another after every change.
    Subscribe,
    Ping,
    Refresh,
    /// Shows or hides the sidebar in every window; replies with `Toggled`.
    Toggle,
    /// Reasserts sidebar widths after a client resize.
    Resized,
    /// Folds or unfolds a session in every sidebar; replies with `Ack`.
    Collapse {
        session: String,
        collapsed: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub now_ms: u64,
    pub sidebar: bool,
    pub panes: Vec<PaneRecord>,
    /// Every tmux window, agent or not; absent from daemons before 0.2.
    #[serde(default)]
    pub windows: Vec<WindowInfo>,
    /// Sessions folded in the sidebar, by name; absent from daemons before 0.5.
    #[serde(default)]
    pub collapsed: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: String,
    pub session: String,
    pub index: u32,
    pub name: String,
    /// Non-sidebar panes in tmux's order; absent from daemons before 0.2.
    #[serde(default)]
    pub panes: Vec<PaneSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: String,
    pub command: String,
    /// Whether this is the active pane of its window.
    pub active: bool,
    #[serde(default)]
    pub cwd: String,
    /// The repo state at `cwd`; `None` outside git or before the daemon has looked.
    #[serde(default)]
    pub git: Option<GitStatus>,
}

/// What `git status` reports, as counts of entries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    /// `None` on a detached HEAD.
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub conflicted: u32,
    pub staged: u32,
    pub deleted: u32,
    pub modified: u32,
    pub untracked: u32,
    pub stashed: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toggled {
    pub sidebar: bool,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_without_windows_parses() {
        let s: Status =
            serde_json::from_value(json!({"now_ms": 1, "sidebar": false, "panes": []})).unwrap();
        assert!(s.windows.is_empty());
        assert!(s.collapsed.is_empty());
        let w: WindowInfo =
            serde_json::from_value(json!({"id": "@1", "session": "s", "index": 1, "name": "n"}))
                .unwrap();
        assert!(w.panes.is_empty());
    }

    #[test]
    fn wire_format_is_tagged() {
        let hook = Request::Hook(Envelope {
            agent: "claude".into(),
            event: "Stop".into(),
            pane: Some("%1".into()),
            tmux: None,
            payload: json!({}),
        });
        let v = serde_json::to_value(&hook).unwrap();
        assert_eq!(v["type"], "hook");
        assert_eq!(v["event"], "Stop");
        assert_eq!(
            serde_json::to_value(Request::List).unwrap(),
            json!({"type": "list"})
        );
        let back: Request = serde_json::from_value(v).unwrap();
        assert_eq!(back, hook);
        assert_eq!(
            serde_json::to_value(Request::Collapse {
                session: "me".into(),
                collapsed: true
            })
            .unwrap(),
            json!({"type": "collapse", "session": "me", "collapsed": true})
        );
    }
}
