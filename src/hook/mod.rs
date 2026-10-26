pub mod claude;
pub mod kiro;
pub mod shim;

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

    pub fn matches_process(self, pane_current_command: &str) -> bool {
        match self {
            Agent::Claude => claude::matches_process(pane_current_command),
            Agent::Kiro => kiro::matches_process(pane_current_command),
        }
    }

    pub fn parse(self, event: &str, payload: &Value) -> Parsed {
        match self {
            Agent::Claude => claude::parse(event, payload),
            Agent::Kiro => kiro::parse(event, payload),
        }
    }

    pub fn classify_screen(self, screen: &str) -> Screen {
        match self {
            Agent::Claude => claude::classify_screen(screen),
            Agent::Kiro => kiro::classify_screen(screen),
        }
    }
}

/// Agent-neutral event vocabulary; each agent's parser maps its hook names onto it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    SessionStart,
    PromptSubmit,
    PreTool {
        tool: Option<String>,
    },
    PostTool,
    PermissionPrompt,
    /// The agent reports it is waiting for input (corrects a stale `working`).
    IdlePrompt,
    Stop,
    SessionEnd,
    NoChange,
    Unrecognized,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// Model id as the agent reports it, e.g. `claude-opus-5-5` or `auto`.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub transcript: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub event: AgentEvent,
    pub meta: Meta,
}

/// What a screen-scrape of the bottom of an agent pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Blocked,
    Working,
    Idle,
    Unknown,
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

/// Largest JSON size of a top-level field the shim forwards.
const SLIM_MAX: usize = 512;

/// Keeps small top-level fields so large tool inputs, outputs, and prompts never cross the socket.
pub fn slim_payload(payload: &Value) -> Value {
    match payload {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| v.to_string().len() <= SLIM_MAX)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        _ => Value::Null,
    }
}

pub(crate) fn str_field(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The bottom `n` lines of a captured screen, ignoring trailing blank lines.
pub fn bottom_lines(screen: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = screen.lines().collect();
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |i| i + 1);
    lines[end.saturating_sub(n)..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slim_payload_keeps_small_values_and_drops_large_ones() {
        let big = "y".repeat(2000);
        let p = json!({
            "a": "x", "n": 1, "effort": {"level": "high"},
            "tool_input": {"content": big}, "prompt": big,
        });
        assert_eq!(
            slim_payload(&p),
            json!({"a": "x", "n": 1, "effort": {"level": "high"}})
        );
        assert_eq!(slim_payload(&json!("str")), Value::Null);
    }

    #[test]
    fn bottom_lines_skips_trailing_blanks() {
        let s = "a\nb\nc\n\n\n";
        assert_eq!(bottom_lines(s, 2), vec!["b", "c"]);
        assert_eq!(bottom_lines(s, 10), vec!["a", "b", "c"]);
        assert!(bottom_lines("\n\n", 3).is_empty());
    }
}
