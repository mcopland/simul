//! Reads Claude Code transcript JSONL files.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// What the last main-thread reply in a transcript ran with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Bytes read from the end of the transcript; the last reply is always near the end.
const TAIL: u64 = 512 * 1024;

pub fn last_turn(path: &Path) -> Result<Option<Turn>> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open transcript {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("failed to stat transcript {}", path.display()))?
        .len();
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("failed to seek transcript {}", path.display()))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)
        .with_context(|| format!("failed to read transcript {}", path.display()))?;
    let text = String::from_utf8_lossy(&tail);
    // A partial first line is expected when reading from the middle; it fails to parse and is skipped.
    Ok(text.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("type")?.as_str()? != "assistant"
            || v.get("isSidechain") == Some(&Value::Bool(true))
        {
            return None;
        }
        let model = v.pointer("/message/model")?.as_str()?;
        // Claude writes `<synthetic>` for locally generated messages such as API errors.
        if model.starts_with('<') {
            return None;
        }
        Some(Turn {
            model: Some(model.to_owned()),
            effort: v.get("effort").and_then(Value::as_str).map(str::to_owned),
        })
    }))
}

/// `claude-opus-5-5` -> `Opus 5.5`; `auto` -> `Auto`; anything unrecognized is kept as is.
pub fn display_model(id: &str) -> String {
    let (base, suffix) = match id.find('[') {
        Some(i) => (&id[..i], Some(&id[i..])),
        None => (id, None),
    };
    let named = match base.strip_prefix("claude-") {
        Some(rest) => {
            let mut parts = rest.split('-');
            let family = capitalize(parts.next().unwrap_or_default());
            let version: Vec<&str> = parts
                .filter(|p| p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit()))
                .collect();
            if family.is_empty() || version.is_empty() {
                return id.to_owned();
            }
            format!("{family} {}", version.join("."))
        }
        None if base == "auto" => "Auto".to_owned(),
        None => return id.to_owned(),
    };
    match suffix {
        Some(s) => format!("{named} {s}"),
        None => named,
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_main_thread_reply_wins() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/claude/transcript.jsonl");
        assert_eq!(
            last_turn(&path).unwrap(),
            Some(Turn {
                model: Some("claude-sonnet-5-5".into()),
                effort: Some("medium".into()),
            })
        );
    }

    #[test]
    fn no_reply_yet_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "{\"type\":\"user\",\"message\":{}}\n").unwrap();
        assert_eq!(last_turn(&path).unwrap(), None);
    }

    #[test]
    fn missing_file_names_the_path() {
        let err = format!(
            "{:#}",
            last_turn(Path::new("/nonexistent/t.jsonl")).unwrap_err()
        );
        assert!(err.contains("/nonexistent/t.jsonl"), "{err}");
    }

    #[test]
    fn reads_only_the_tail_of_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let filler = format!("{{\"type\":\"user\",\"pad\":\"{}\"}}\n", "x".repeat(1000));
        let mut text = filler.repeat(1000);
        text.push_str("{\"type\":\"assistant\",\"isSidechain\":false,\"effort\":\"max\",\"message\":{\"model\":\"claude-opus-5-5\"}}\n");
        std::fs::write(&path, text).unwrap();
        let turn = last_turn(&path).unwrap().unwrap();
        assert_eq!(turn.effort.as_deref(), Some("max"));
    }

    #[test]
    fn model_names() {
        assert_eq!(display_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(display_model("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(display_model("claude-haiku-5-5[1m]"), "Haiku 5.5 [1m]");
        assert_eq!(display_model("claude-fable-5-1"), "Fable 5.1");
        assert_eq!(display_model("auto"), "Auto");
        assert_eq!(display_model("gpt-x"), "gpt-x");
        assert_eq!(display_model(""), "");
    }
}
