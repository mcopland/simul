use anyhow::{Context, Result};

use crate::client;
use crate::daemon::state::PaneRecord;
use crate::paths;
use crate::protocol::{Request, Status};

pub fn run(json: bool) -> Result<()> {
    let status: Status = client::request(&paths::client_socket()?, &Request::List)?;
    if json {
        let out = serde_json::to_string_pretty(&status.panes).context("failed to encode panes")?;
        println!("{out}");
    } else {
        print!("{}", render(&status));
    }
    Ok(())
}

/// Compact age: `12s`, `3m`, `2h`, `4d`.
pub fn fmt_age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

pub fn location(rec: &PaneRecord) -> String {
    match (&rec.session, rec.window) {
        (Some(s), Some(w)) => format!("{s}:{w}"),
        _ => "-".into(),
    }
}

pub fn render(status: &Status) -> String {
    if status.panes.is_empty() {
        return "no agents\n".into();
    }
    let header = [
        "PANE",
        "STATE",
        "AGENT",
        "SESSION:WIN",
        "BRANCH",
        "AGE",
        "CWD",
    ]
    .map(String::from);
    let rows: Vec<[String; 7]> = status
        .panes
        .iter()
        .map(|r| {
            [
                r.pane.clone(),
                r.state.name().to_owned(),
                r.agent.tag().to_string(),
                location(r),
                r.branch.clone().unwrap_or_else(|| "-".into()),
                fmt_age(status.now_ms.saturating_sub(r.since_ms)),
                r.cwd.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let mut widths = header.clone().map(|h| h.len());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let cells: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::State;

    #[test]
    fn ages() {
        assert_eq!(fmt_age(0), "0s");
        assert_eq!(fmt_age(59_999), "59s");
        assert_eq!(fmt_age(60_000), "1m");
        assert_eq!(fmt_age(3_599_000), "59m");
        assert_eq!(fmt_age(7_200_000), "2h");
        assert_eq!(fmt_age(90_000_000), "1d");
    }

    fn rec(pane: &str, state: State) -> PaneRecord {
        let json = serde_json::json!({
            "pane": pane, "agent": "claude", "state": state, "since_ms": 1_000,
            "last_event_ms": 1_000, "session_id": null, "cwd": "/w/repo",
            "branch": "main", "pending_tool": null, "session": "dev", "window": 3,
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn renders_aligned_table() {
        let status = Status {
            now_ms: 181_000,
            panes: vec![rec("%1", State::Blocked), rec("%12", State::Done)],
        };
        let out = render(&status);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("PANE "), "{out}");
        assert_eq!(
            lines[1].split_whitespace().collect::<Vec<_>>(),
            ["%1", "blocked", "C", "dev:3", "main", "3m", "/w/repo"]
        );
        let col = |l: &str| l.find("dev:3");
        assert_eq!(col(lines[1]), col(lines[2]));
    }

    #[test]
    fn empty_status_says_so() {
        let out = render(&Status {
            now_ms: 0,
            panes: vec![],
        });
        assert_eq!(out, "no agents\n");
    }
}
