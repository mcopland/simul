use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Kiro,
}

impl Agent {
    pub fn from_name(name: &str) -> Option<Agent> {
        match name {
            "claude" => Some(Agent::Claude),
            "kiro" => Some(Agent::Kiro),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Kiro => "kiro",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Kiro => "Kiro",
        }
    }

    pub fn tag(self) -> char {
        match self {
            Agent::Claude => 'C',
            Agent::Kiro => 'K',
        }
    }

    pub fn all() -> &'static [Agent] {
        &[Agent::Claude, Agent::Kiro]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Working,
    Blocked,
    /// The agent asked a question; lasts until the next prompt.
    Asking,
    Done,
    /// The turn ended in an error; lasts until the next prompt.
    Failed,
    Idle,
    /// Also any state a newer daemon reports that this build doesn't know.
    #[serde(other)]
    Unknown,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Working => "working",
            State::Blocked => "blocked",
            State::Asking => "asking",
            State::Done => "done",
            State::Failed => "failed",
            State::Idle => "idle",
            State::Unknown => "unknown",
        }
    }
}

/// One tracked agent pane, as the daemon reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneRecord {
    pub pane: String,
    pub agent: Agent,
    pub state: State,
    /// Unix ms of the last state change.
    pub since_ms: u64,
    /// Unix ms of the last hook event; 0 if none yet (recovered panes).
    pub last_event_ms: u64,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub pending_tool: Option<String>,
    /// Why the last turn failed, while the pane is `failed`.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Transcript file, read after each turn for the current model.
    #[serde(default)]
    pub transcript: Option<String>,
    pub session: Option<String>,
    pub window: Option<u32>,
    #[serde(default)]
    pub window_id: Option<String>,
    #[serde(default)]
    pub window_name: Option<String>,
    /// Whether this is the active pane of its window.
    #[serde(default)]
    pub active: bool,
}

impl PaneRecord {
    /// A pane first seen at `now`, with nothing known about it yet.
    pub fn new(pane: &str, agent: Agent, state: State, now: u64) -> PaneRecord {
        PaneRecord {
            pane: pane.to_owned(),
            agent,
            state,
            since_ms: now,
            last_event_ms: 0,
            session_id: None,
            cwd: None,
            branch: None,
            pending_tool: None,
            error: None,
            model: None,
            effort: None,
            transcript: None,
            session: None,
            window: None,
            window_id: None,
            window_name: None,
            active: false,
        }
    }
}

/// What the shim sends to the daemon for one hook invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub agent: String,
    pub event: String,
    pub pane: Option<String>,
    pub tmux: Option<String>,
    pub payload: Value,
}

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
    /// Reasserts sidebar widths after a client resize or a layout change.
    Resized,
    /// Folds or unfolds a session in every sidebar; replies with `Ack`.
    Collapse {
        session: String,
        collapsed: bool,
    },
    /// Replies with the daemon's `Identity`, so a newer build can tell it needs replacing.
    Identify,
    /// Replies with `Ack`, then the daemon exits so another can take its socket.
    Shutdown,
}

/// Which build a process runs: its version, its binary's resolved path, and that file's
/// modification time when the process started, so a rebuild in place counts as another build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub version: String,
    pub exe: String,
    /// Seconds since the epoch; 0 if unknown.
    pub modified: u64,
}

impl Identity {
    /// This build, running from `exe`. Call it at startup: later, the file may be a newer build.
    pub fn of(exe: &std::path::Path) -> Identity {
        let modified = std::fs::metadata(exe)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        Identity {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            exe: exe.to_string_lossy().into_owned(),
            modified,
        }
    }

    /// As stored in a sidebar pane's `@simul-sidebar-exe` option.
    pub fn tag(&self) -> String {
        format!("{} {} {}", self.version, self.modified, self.exe)
    }
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
    /// What the pane's foreground process is doing, unless it is an agent or an idle shell.
    #[serde(default)]
    pub run: Option<Run>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    /// An editor, pager, remote shell, or monitor: a program you work in, not one you wait on.
    Interactive,
    /// The process ended while you weren't looking; lasts until you view the pane.
    Finished,
    /// Any state a newer daemon reports that this build doesn't know.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub state: RunState,
    /// Unix ms the pane entered `state`.
    pub since_ms: u64,
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
        let p: PaneSummary =
            serde_json::from_value(json!({"id": "%1", "command": "zsh", "active": true})).unwrap();
        assert_eq!(p.run, None);
    }

    #[test]
    fn run_states_read_forward_compatibly() {
        let r: Run = serde_json::from_value(json!({"state": "finished", "since_ms": 5})).unwrap();
        assert_eq!(r.state, RunState::Finished);
        let r: Run = serde_json::from_value(json!({"state": "paused", "since_ms": 5})).unwrap();
        assert_eq!(r.state, RunState::Unknown);
    }

    /// Asserts `json` survives a parse into `T` and back byte for byte.
    fn round_trips<T: Serialize + serde::de::DeserializeOwned>(json: &str) {
        let parsed: T = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    // Sidebars and `ls` of one build read daemons of another, so the bytes on the socket must
    // not shift when types move between modules.
    #[test]
    fn status_wire_format_is_pinned() {
        round_trips::<Status>(concat!(
            r#"{"now_ms":1700000000000,"sidebar":true,"panes":["#,
            r#"{"pane":"%1","agent":"claude","state":"failed","since_ms":10,"last_event_ms":20,"#,
            r#""session_id":"abc","cwd":"/w","branch":"main","pending_tool":"Bash","#,
            r#""error":"rate_limit","model":"claude-opus-5-5","effort":"high","#,
            r#""transcript":"/t.jsonl","session":"me","window":2,"window_id":"@2","#,
            r#""window_name":"code","active":true},"#,
            r#"{"pane":"%2","agent":"kiro","state":"unknown","since_ms":0,"last_event_ms":0,"#,
            r#""session_id":null,"cwd":null,"branch":null,"pending_tool":null,"error":null,"#,
            r#""model":null,"effort":null,"transcript":null,"session":null,"window":null,"#,
            r#""window_id":null,"window_name":null,"active":false}],"#,
            r#""windows":[{"id":"@2","session":"me","index":2,"name":"code","panes":["#,
            r#"{"id":"%3","command":"cargo","active":true,"cwd":"/w","#,
            r#""git":{"branch":"main","ahead":1,"behind":2,"conflicted":3,"staged":4,"#,
            r#""deleted":5,"modified":6,"untracked":7,"stashed":8},"#,
            r#""run":{"state":"running","since_ms":30}},"#,
            r#"{"id":"%4","command":"zsh","active":false,"cwd":"/x","#,
            r#""git":{"branch":null,"ahead":0,"behind":0,"conflicted":0,"staged":0,"#,
            r#""deleted":0,"modified":0,"untracked":0,"stashed":0},"run":null}]}],"#,
            r#""collapsed":["other"]}"#,
        ));
    }

    #[test]
    fn hook_wire_format_is_pinned() {
        round_trips::<Request>(concat!(
            r#"{"type":"hook","agent":"claude","event":"Stop","pane":"%1","#,
            r#""tmux":"/tmp/t,1,0","payload":{"session_id":"abc","cwd":"/w"}}"#,
        ));
        round_trips::<Request>(
            r#"{"type":"hook","agent":"kiro","event":"Stop","pane":null,"tmux":null,"payload":null}"#,
        );
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
