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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub now_ms: u64,
    pub panes: Vec<PaneRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
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
    }
}
