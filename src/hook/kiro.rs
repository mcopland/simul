use serde_json::Value;

use super::{AgentEvent, Meta, Parsed, Screen, bottom_lines, str_field};

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

const SCRAPE_LINES: usize = 15;

/// Non-blank lines checked for the approval prompt: the prompt line, the input line, and one spare.
const PROMPT_WINDOW: usize = 3;

/// Only the approval prompt is recognized: Kiro's idle and working screens are not recorded
/// yet, so without the prompt the hooks decide. See `fixtures/CALIBRATION.md`.
pub fn classify_screen(screen: &str, approval_patterns: &[String]) -> Screen {
    let lines = bottom_lines(screen, SCRAPE_LINES);
    let configured = approval_patterns
        .iter()
        .filter(|p| !p.is_empty())
        .any(|p| lines.iter().any(|l| l.contains(p.as_str())));
    let newest_first: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(PROMPT_WINDOW)
        .copied()
        .collect();
    // An answer typed at the prompt (`> y`) means the tool is already running.
    let waiting = newest_first
        .iter()
        .position(|l| is_approval(l))
        .is_some_and(|i| newest_first[..i].iter().all(|l| is_bare_prompt(l)));
    if configured || waiting {
        Screen::Blocked
    } else {
        Screen::NoPrompt
    }
}

/// The classic prompt: `Allow this action? Use 't' to trust ... [y/n/t]:`.
fn is_approval(line: &str) -> bool {
    let line = line.trim();
    line.contains("Allow this action?") && line.ends_with("[y/n/t]:")
}

/// An empty input prompt: `>`, `!>` when tools are trusted, or `[agent] >`.
fn is_bare_prompt(line: &str) -> bool {
    let line = line.trim();
    let rest = match line.strip_prefix('[') {
        Some(tagged) => match tagged.split_once(']') {
            Some((agent, rest)) if !agent.is_empty() && !agent.contains(' ') => rest.trim_start(),
            _ => return false,
        },
        None => line,
    };
    matches!(rest, ">" | "!>")
}

#[cfg(test)]
mod tests {
    use super::super::slim_payload;
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn fixture(rel: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/kiro")
            .join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn hook(name: &str) -> Value {
        serde_json::from_str(&fixture(&format!("hooks/{name}.json"))).unwrap()
    }

    #[test]
    fn classifies_fixture_screens() {
        let cases = [
            ("approval", Screen::Blocked),
            // The user typed `y`; the tool is running now.
            ("answered", Screen::NoPrompt),
            ("prose-mentions-prompt", Screen::NoPrompt),
        ];
        for (name, want) in cases {
            let screen = fixture(&format!("screens/{name}.txt"));
            assert_eq!(classify_screen(&screen, &[]), want, "{name}");
        }
        assert_eq!(classify_screen("", &[]), Screen::NoPrompt);
    }

    #[test]
    fn approval_with_trusted_prompt_or_agent_name_is_blocked() {
        let line = "Allow this action? Use 't' to trust (always allow) the 'search' tool for the session. [y/n/t]:";
        for prompt in ["!>", "[dev] >", ""] {
            let screen = format!("output\n\n{line}\n\n{prompt}\n");
            assert_eq!(classify_screen(&screen, &[]), Screen::Blocked, "{prompt:?}");
        }
    }

    #[test]
    fn output_after_the_prompt_is_not_an_input_line() {
        let line = "Allow this action? Use 't' to trust (always allow) this tool for the session. [y/n/t]:";
        for after in ["</div>", "a -> b >", "> y", "[dev] > yes"] {
            let screen = format!("{line}\n{after}\n");
            assert_eq!(classify_screen(&screen, &[]), Screen::NoPrompt, "{after:?}");
        }
    }

    #[test]
    fn approval_scrolled_away_is_not_blocked() {
        let screen = format!(
            "{}\n\n> \nmore\nlines\nof output\n",
            fixture("screens/approval.txt").trim_end()
        );
        assert_eq!(classify_screen(&screen, &[]), Screen::NoPrompt);
    }

    #[test]
    fn configured_patterns_mark_blocked() {
        let screen = "Run shell command?\n  Yes   Trust   No\n";
        assert_eq!(classify_screen(screen, &[]), Screen::NoPrompt);
        let pats = ["Yes   Trust   No".to_string()];
        assert_eq!(classify_screen(screen, &pats), Screen::Blocked);
        let old = format!("  Yes   Trust   No\n{}", "line\n".repeat(20));
        assert_eq!(
            classify_screen(&old, &pats),
            Screen::NoPrompt,
            "only the bottom lines count"
        );
        assert_eq!(
            classify_screen(screen, &[String::new()]),
            Screen::NoPrompt,
            "empty pattern matches nothing"
        );
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
