//! Reads the live status Claude Code keeps for each running session in `sessions/<pid>.json`.
//! The file is undocumented, so anything unexpected in it means "no live status" for that pane.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveStatus {
    Busy,
    Waiting,
    Idle,
}

/// One pane's status as Claude Code last wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    pub pane: String,
    pub session: String,
    pub status: LiveStatus,
    /// Unix ms of the last status change.
    pub updated_ms: u64,
}

/// What one read of the sessions dir found.
#[derive(Debug, Default)]
pub struct Sessions {
    pub live: Vec<Live>,
    /// Session files that could not be read, one message each.
    pub skipped: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    pid: Option<i32>,
    /// `<session>:<window id>.<pane id>`, e.g. `dev:@54.%475`.
    tmux: Option<String>,
    status: Option<String>,
    status_updated_at: Option<u64>,
    job_id: Option<String>,
    /// Set on the pane's client when the conversation runs in a background session.
    parked_job_id: Option<String>,
}

/// `$CLAUDE_CONFIG_DIR/sessions`, else `$HOME/.claude/sessions`.
pub fn sessions_dir(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".claude")))
        .map(|d| d.join("sessions"))
}

/// Live status of every pane with a session file whose process `alive` accepts.
pub fn read(dir: &Path, alive: impl Fn(i32) -> bool) -> Result<Sessions> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Sessions::default()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to list Claude sessions in {}", dir.display()));
        }
    };
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("failed to list Claude sessions in {}", dir.display()))?
            .path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            // Claude removes the file when it exits, possibly between listing and reading.
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                skipped.push(format!(
                    "failed to read Claude session {}: {e}",
                    path.display()
                ));
                continue;
            }
        };
        // A read that races Claude rewriting the file fails to parse; the next tick rereads it.
        if let Ok(f) = serde_json::from_str::<SessionFile>(&text) {
            files.push(f);
        }
    }
    let jobs: HashMap<&str, &SessionFile> = files
        .iter()
        .filter_map(|f| Some((f.job_id.as_deref()?, f)))
        .collect();
    let alive = |f: &SessionFile| f.pid.is_some_and(&alive);
    let live = files
        .iter()
        .filter(|f| alive(f))
        .filter_map(|f| {
            let (session, pane) = parse_tmux(f.tmux.as_deref()?)?;
            // A parked client's own status is frozen; without its live bg session there is none.
            let source = match f.parked_job_id.as_deref() {
                Some(job) => jobs.get(job).copied().filter(|bg| alive(bg))?,
                None => f,
            };
            Some(Live {
                pane: pane.to_owned(),
                session: session.to_owned(),
                status: parse_status(source.status.as_deref()?)?,
                updated_ms: source.status_updated_at.unwrap_or_default(),
            })
        })
        .collect();
    Ok(Sessions { live, skipped })
}

fn parse_status(s: &str) -> Option<LiveStatus> {
    match s {
        "busy" => Some(LiveStatus::Busy),
        "waiting" => Some(LiveStatus::Waiting),
        "idle" => Some(LiveStatus::Idle),
        _ => None,
    }
}

/// `dev:@54.%475` -> (`dev`, `%475`). tmux forbids `:` and `.` in session names.
fn parse_tmux(s: &str) -> Option<(&str, &str)> {
    let (session, rest) = s.split_once(':')?;
    let (window, pane) = rest.split_once('.')?;
    (!session.is_empty() && window.starts_with('@') && pane.starts_with('%'))
        .then_some((session, pane))
}

/// Whether a process with this pid exists.
pub fn pid_alive(pid: i32) -> bool {
    // kill(0) and negative pids address process groups, not one process.
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the pid exists and may be signalled; nothing is sent.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/claude/sessions")
    }

    fn live(pane: &str, status: LiveStatus, updated_ms: u64) -> Live {
        Live {
            pane: pane.into(),
            session: "main".into(),
            status,
            updated_ms,
        }
    }

    #[test]
    fn reads_recorded_session_files() {
        let mut got = read(&fixtures(), |pid| pid != 104 && pid != 109)
            .unwrap()
            .live;
        got.sort_by(|a, b| a.pane.cmp(&b.pane));
        assert_eq!(
            got,
            vec![
                live("%3", LiveStatus::Busy, 1792954216290),
                // The pane's client is parked; the background session holds the status.
                live("%5", LiveStatus::Waiting, 1792897864864),
            ],
            "dead pids, unknown statuses, torn files, bg sessions, and parked panes whose bg \
             session is gone or dead are skipped"
        );
    }

    #[test]
    fn one_unreadable_file_does_not_hide_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("8.json")).unwrap();
        std::fs::write(
            dir.path().join("7.json"),
            r#"{"pid":7,"tmux":"s:@1.%2","status":"busy"}"#,
        )
        .unwrap();
        let got = read(dir.path(), |_| true).unwrap();
        assert_eq!(got.live.len(), 1);
        assert_eq!(got.skipped.len(), 1);
        assert!(got.skipped[0].contains("8.json"), "{:?}", got.skipped);
    }

    #[test]
    fn missing_dir_has_no_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let got = read(&dir.path().join("nope"), |_| true).unwrap();
        assert!(got.live.is_empty() && got.skipped.is_empty());
    }

    #[test]
    fn ignores_other_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("7.abc.key"), "x").unwrap();
        std::fs::write(
            dir.path().join("7.json"),
            r#"{"pid":7,"tmux":"s:@1.%2","status":"idle"}"#,
        )
        .unwrap();
        let got = read(dir.path(), |_| true).unwrap();
        assert!(got.skipped.is_empty());
        assert_eq!(
            got.live,
            vec![Live {
                pane: "%2".into(),
                session: "s".into(),
                status: LiveStatus::Idle,
                updated_ms: 0,
            }]
        );
    }

    #[test]
    fn parses_tmux_field() {
        assert_eq!(parse_tmux("dev:@54.%475"), Some(("dev", "%475")));
        assert_eq!(parse_tmux("dev:@54"), None);
        assert_eq!(parse_tmux(":@1.%2"), None);
        assert_eq!(parse_tmux("dev:1.2"), None);
    }

    #[test]
    fn sessions_dir_prefers_claude_config_dir() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            sessions_dir(env(&[("CLAUDE_CONFIG_DIR", "/c"), ("HOME", "/h")])),
            Some(PathBuf::from("/c/sessions"))
        );
        assert_eq!(
            sessions_dir(env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.claude/sessions"))
        );
        assert_eq!(sessions_dir(env(&[])), None);
    }

    #[test]
    fn pid_alive_checks_the_process() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
        assert!(!pid_alive(i32::MAX));
    }
}
