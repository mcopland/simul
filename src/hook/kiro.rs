use serde_json::Value;

use super::{AgentEvent, Meta, Parsed, Screen, str_field};

/// Kiro CLI V3 triggers, as installed into its standalone hook file.
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SessionEnd",
];

/// `kiro-cli` is the pane's process; `kiro-cli-chat` is what runs when started directly.
pub fn matches_process(pane_current_command: &str) -> bool {
    matches!(pane_current_command, "kiro-cli" | "kiro-cli-chat")
}

/// Accepts the V2 engine's camelCase names too, so a hook from an agent profile still counts.
pub fn parse(event: &str, payload: &Value) -> Parsed {
    let event = match event {
        "SessionStart" | "agentSpawn" => AgentEvent::SessionStart,
        "UserPromptSubmit" | "userPromptSubmit" => AgentEvent::PromptSubmit,
        "PreToolUse" | "preToolUse" => AgentEvent::PreTool {
            tool: str_field(payload, "tool_name"),
        },
        "PostToolUse" | "postToolUse" => AgentEvent::PostTool,
        "Stop" | "stop" => AgentEvent::Stop,
        "SessionEnd" => AgentEvent::SessionEnd,
        _ => AgentEvent::Unrecognized,
    };
    Parsed {
        event,
        meta: Meta {
            session_id: str_field(payload, "session_id"),
            cwd: str_field(payload, "cwd"),
            ..Meta::default()
        },
    }
}

pub fn classify_screen(_screen: &str) -> Screen {
    Screen::Unknown
}

#[cfg(test)]
mod tests {
    use super::super::slim_payload;
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn hook(name: &str) -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/kiro/hooks")
            .join(format!("{name}.json"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn parses_every_fixture_event() {
        let cases = [
            ("SessionStart", AgentEvent::SessionStart),
            ("UserPromptSubmit", AgentEvent::PromptSubmit),
            (
                "PreToolUse",
                AgentEvent::PreTool {
                    tool: Some("fs_write".into()),
                },
            ),
            ("PostToolUse", AgentEvent::PostTool),
            ("Stop", AgentEvent::Stop),
            ("SessionEnd", AgentEvent::SessionEnd),
        ];
        assert_eq!(cases.len(), EVENTS.len());
        for (event, want) in cases {
            let parsed = parse(event, &slim_payload(&hook(event)));
            assert_eq!(parsed.event, want, "{event}");
            assert_eq!(
                parsed.meta.cwd.as_deref(),
                Some("/home/user/src/scratch"),
                "{event}"
            );
            assert_eq!(
                parsed.meta.session_id.as_deref(),
                Some("sess_01jf3k9q7w2m5n8p4r6t0v2x4z"),
                "{event}"
            );
            assert_eq!(parsed.meta.transcript, None);
        }
    }

    #[test]
    fn accepts_v2_event_names() {
        let cases = [
            ("agentSpawn", AgentEvent::SessionStart),
            ("userPromptSubmit", AgentEvent::PromptSubmit),
            (
                "preToolUse",
                AgentEvent::PreTool {
                    tool: Some("execute_bash".into()),
                },
            ),
            ("postToolUse", AgentEvent::PostTool),
            ("stop", AgentEvent::Stop),
        ];
        for (event, want) in cases {
            let p = json!({"tool_name": "execute_bash"});
            assert_eq!(parse(event, &p).event, want, "{event}");
        }
    }

    #[test]
    fn unknown_input_degrades() {
        assert_eq!(parse("Bogus", &json!({})).event, AgentEvent::Unrecognized);
        assert_eq!(parse("Stop", &Value::Null).event, AgentEvent::Stop);
        assert_eq!(parse("Stop", &Value::Null).meta, Meta::default());
    }

    #[test]
    fn matches_kiro_process_names() {
        assert!(matches_process("kiro-cli"));
        assert!(matches_process("kiro-cli-chat"));
        assert!(!matches_process("kiro"));
        assert!(!matches_process("claude"));
        assert!(!matches_process("zsh"));
    }
}
