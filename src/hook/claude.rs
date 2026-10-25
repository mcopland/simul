use serde_json::Value;

use super::{AgentEvent, Meta, Parsed, Screen, bottom_lines, str_field};

pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Notification",
    "Stop",
    "SessionEnd",
];

/// The native installer runs a versioned binary, so tmux reports the version (`2.1.307`) as the command.
pub fn matches_process(pane_current_command: &str) -> bool {
    pane_current_command == "claude" || is_version(pane_current_command)
}

fn is_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

pub fn parse(event: &str, payload: &Value) -> Parsed {
    // Subagents fire tool hooks from the parent's pane, even after its turn has ended. Their
    // permission prompts still show in that pane, so those count.
    let subagent = str_field(payload, "agent_id").is_some_and(|id| !id.is_empty());
    let event = match event {
        "PreToolUse" | "PostToolUse" if subagent => AgentEvent::NoChange,
        // Compaction restarts the session mid-turn; the agent is still busy.
        "SessionStart" if str_field(payload, "source").as_deref() == Some("compact") => {
            AgentEvent::NoChange
        }
        "SessionStart" => AgentEvent::SessionStart,
        "UserPromptSubmit" => AgentEvent::PromptSubmit,
        "PreToolUse" => AgentEvent::PreTool {
            tool: str_field(payload, "tool_name"),
        },
        "PostToolUse" => AgentEvent::PostTool,
        "PermissionRequest" => AgentEvent::PermissionPrompt,
        "Notification" => notification(payload),
        "Stop" => AgentEvent::Stop,
        "SessionEnd" => AgentEvent::SessionEnd,
        _ => AgentEvent::Unrecognized,
    };
    Parsed {
        event,
        meta: Meta {
            session_id: str_field(payload, "session_id"),
            cwd: str_field(payload, "cwd"),
            model: str_field(payload, "model"),
            effort: payload
                .pointer("/effort/level")
                .and_then(Value::as_str)
                .map(str::to_owned),
            transcript: str_field(payload, "transcript_path"),
        },
    }
}

fn notification(payload: &Value) -> AgentEvent {
    match str_field(payload, "notification_type").as_deref() {
        Some("permission_prompt" | "elicitation_dialog") => AgentEvent::PermissionPrompt,
        Some("idle_prompt") => AgentEvent::IdlePrompt,
        Some(_) => AgentEvent::NoChange,
        None => match str_field(payload, "message") {
            Some(m) if m.contains("needs your permission") => AgentEvent::PermissionPrompt,
            Some(m) if m.contains("waiting for your input") => AgentEvent::IdlePrompt,
            _ => AgentEvent::NoChange,
        },
    }
}

const SCRAPE_LINES: usize = 15;

/// Signatures recorded in `fixtures/CALIBRATION.md`.
pub fn classify_screen(screen: &str) -> Screen {
    let lines = bottom_lines(screen, SCRAPE_LINES);
    let has = |needle: &str| lines.iter().any(|l| l.contains(needle));
    let dialog =
        (has("Do you want to") && has("Esc to cancel")) || lines.iter().any(|l| is_selector(l));
    // Dialogs replace the input box, so with it on screen a "selector" is echoed prompt text.
    if dialog && !has_input_box(&lines) {
        Screen::Blocked
    } else if lines.iter().any(|l| is_spinner(l)) {
        Screen::Working
    } else if lines.iter().any(|l| l.trim_start().starts_with('\u{276f}')) {
        Screen::Idle
    } else {
        Screen::Unknown
    }
}

/// A numbered option under the cursor (`\u{276f} 1. Yes`): approvals, plan exit, questions.
fn is_selector(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("\u{276f} ")
        .and_then(|rest| rest.split_once(". "))
        .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The prompt line directly under a `\u{2500}` rule, which may carry a session name.
fn has_input_box(lines: &[&str]) -> bool {
    lines.windows(2).any(|w| {
        w[0].trim_start().starts_with('\u{2500}')
            && w[1].trim_start().starts_with('\u{276f}')
            && !is_selector(w[1])
    })
}

/// Matches `<glyph> <Verb>\u{2026} (<n>s ...`.
fn is_spinner(line: &str) -> bool {
    line.split_once("\u{2026} (")
        .is_some_and(|(_, rest)| rest.starts_with(|c: char| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn fixture(rel: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/claude")
            .join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn hook(name: &str) -> Value {
        serde_json::from_str(&fixture(&format!("hooks/{name}.json"))).unwrap()
    }

    #[test]
    fn parses_every_recorded_event() {
        let cases = [
            ("SessionStart", "SessionStart", AgentEvent::SessionStart),
            (
                "UserPromptSubmit",
                "UserPromptSubmit",
                AgentEvent::PromptSubmit,
            ),
            (
                "PreToolUse",
                "PreToolUse",
                AgentEvent::PreTool {
                    tool: Some("Write".into()),
                },
            ),
            ("PostToolUse", "PostToolUse", AgentEvent::PostTool),
            (
                "Notification",
                "Notification-permission_prompt",
                AgentEvent::PermissionPrompt,
            ),
            (
                "Notification",
                "Notification-idle_prompt",
                AgentEvent::IdlePrompt,
            ),
            ("Stop", "Stop", AgentEvent::Stop),
            ("SessionEnd", "SessionEnd", AgentEvent::SessionEnd),
        ];
        for (event, file, want) in cases {
            let parsed = parse(event, &hook(file));
            assert_eq!(parsed.event, want, "{file}");
            assert_eq!(parsed.meta.cwd.as_deref(), Some("/home/user/src/scratch"));
            assert!(parsed.meta.session_id.is_some(), "{file}");
        }
    }

    #[test]
    fn reads_model_effort_and_transcript() {
        let start = parse("SessionStart", &hook("SessionStart")).meta;
        assert_eq!(start.model.as_deref(), Some("claude-opus-5-5"));
        assert!(
            start
                .transcript
                .as_deref()
                .is_some_and(|t| t.ends_with(".jsonl"))
        );
        let stop = parse("Stop", &super::super::slim_payload(&hook("Stop"))).meta;
        assert_eq!(stop.effort.as_deref(), Some("high"));
        assert_eq!(stop.model, None);
        assert_eq!(parse("Stop", &json!({"effort": "high"})).meta.effort, None);
    }

    #[test]
    fn parses_after_slimming() {
        let slim = super::super::slim_payload(&hook("PreToolUse"));
        assert_eq!(
            parse("PreToolUse", &slim).event,
            AgentEvent::PreTool {
                tool: Some("Write".into())
            }
        );
    }

    #[test]
    fn permission_falls_back_to_message() {
        let p = json!({"message": "Claude needs your permission to use Bash"});
        assert_eq!(
            parse("Notification", &p).event,
            AgentEvent::PermissionPrompt
        );
        let p = json!({"message": "something else"});
        assert_eq!(parse("Notification", &p).event, AgentEvent::NoChange);
    }

    #[test]
    fn subagent_events_leave_the_pane_alone() {
        let mut p = hook("PreToolUse");
        p["agent_id"] = json!("a1b2");
        p["agent_type"] = json!("Explore");
        let parsed = parse("PreToolUse", &super::super::slim_payload(&p));
        assert_eq!(parsed.event, AgentEvent::NoChange);
        assert_eq!(parsed.meta.cwd.as_deref(), Some("/home/user/src/scratch"));
        // A subagent's permission prompt still shows in the parent pane and needs the user.
        assert_eq!(
            parse("PermissionRequest", &p).event,
            AgentEvent::PermissionPrompt
        );
        let mut n = hook("Notification-permission_prompt");
        n["agent_id"] = json!("a1b2");
        assert_eq!(
            parse("Notification", &n).event,
            AgentEvent::PermissionPrompt
        );
        p["agent_id"] = json!("");
        assert!(matches!(
            parse("PreToolUse", &p).event,
            AgentEvent::PreTool { .. }
        ));
    }

    #[test]
    fn permission_request_and_questions_block() {
        assert!(EVENTS.contains(&"PermissionRequest"));
        let p = json!({"tool_name": "Bash"});
        assert_eq!(
            parse("PermissionRequest", &p).event,
            AgentEvent::PermissionPrompt
        );
        let p = json!({"notification_type": "elicitation_dialog"});
        assert_eq!(
            parse("Notification", &p).event,
            AgentEvent::PermissionPrompt
        );
    }

    #[test]
    fn compaction_session_start_keeps_state() {
        let p = json!({"source": "compact", "session_id": "s"});
        assert_eq!(parse("SessionStart", &p).event, AgentEvent::NoChange);
        let p = json!({"source": "clear"});
        assert_eq!(parse("SessionStart", &p).event, AgentEvent::SessionStart);
    }

    #[test]
    fn numbered_selector_is_blocked() {
        let plan_exit = "Ready to code?\n\n Would you like to proceed?\n \u{276f} 1. Yes, and auto-accept edits\n   2. Yes\n   3. No, keep planning\n";
        assert_eq!(classify_screen(plan_exit), Screen::Blocked);
        let idle_with_text = "\u{276f} hello\n";
        assert_eq!(classify_screen(idle_with_text), Screen::Idle);
    }

    #[test]
    fn echoed_list_prompt_while_thinking_is_working() {
        let screen = fixture("screens/echoed-list-prompt.txt").replace(
            "probe\n\n",
            "probe\n\n\u{2722} Pondering\u{2026} (2s \u{b7} thinking with high effort)\n",
        );
        assert!(screen.contains("Pondering"));
        assert_eq!(classify_screen(&screen), Screen::Working);
    }

    #[test]
    fn unknown_input_degrades() {
        assert_eq!(parse("Bogus", &json!({})).event, AgentEvent::Unrecognized);
        assert_eq!(parse("Stop", &Value::Null).event, AgentEvent::Stop);
        assert_eq!(parse("Stop", &Value::Null).meta, Meta::default());
    }

    #[test]
    fn classifies_recorded_screens() {
        let cases = [
            ("approval", Screen::Blocked),
            ("working", Screen::Working),
            ("working-tool", Screen::Working),
            ("done", Screen::Idle),
            ("idle", Screen::Idle),
            ("trust", Screen::Idle),
            // A submitted numbered list echoes as `\u{276f} 1. ...`, which is not a selector.
            ("echoed-list-prompt", Screen::Idle),
        ];
        for (name, want) in cases {
            let screen = fixture(&format!("screens/{name}.txt"));
            assert_eq!(classify_screen(&screen), want, "{name}");
        }
        assert_eq!(classify_screen("$ ls\nfoo\n"), Screen::Unknown);
    }

    #[test]
    fn matches_claude_process_names() {
        assert!(matches_process("claude"));
        assert!(matches_process("2.1.307"));
        assert!(!matches_process("zsh"));
        assert!(!matches_process("2.1"));
        assert!(!matches_process("node"));
    }
}
