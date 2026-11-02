//! Reads the live status Kiro CLI's V3 engine keeps for each session in
//! `sessions/<hash>/<session id>/`: a `.lock` with the owning pid, the `messages.jsonl` event
//! log, and `session.json`. See `fixtures/CALIBRATION.md`. Anything unreadable means "no live
//! status" for that pane, so the screen scrape takes over.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::claude_sessions::{Live, LiveStatus, Sessions};

/// How much of the end of `messages.jsonl` is read; a turn's deciding events are near the end.
const TAIL_BYTES: u64 = 256 * 1024;

/// Event types that decide the status, quoted as they appear in a log line. Checked before
/// parsing, so the bulk of the log (messages, tool output) is never parsed.
const DECIDING: [&str; 4] = [
    r#""turn_start""#,
    r#""turn_end""#,
    r#""pending_interaction""#,
    r#""interaction_resolved""#,
];

/// A Kiro pane with the session id its hooks reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub pane: String,
    /// The tmux session the pane is in.
    pub session: String,
    pub session_id: String,
}

#[derive(Deserialize)]
struct Lock {
    pid: i32,
}

#[derive(Deserialize)]
struct SessionFile {
    status: Option<String>,
}

#[derive(Deserialize)]
struct LogLine {
    timestamp: String,
    payload: Payload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    #[serde(rename = "type")]
    kind: String,
    stop_reason: Option<String>,
    tool_call_id: Option<String>,
}

/// `$KIRO_HOME`, else `$HOME/.kiro`.
pub fn kiro_home(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("KIRO_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".kiro")))
}

/// `$KIRO_HOME/sessions`, else `$HOME/.kiro/sessions`.
pub fn sessions_dir(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    kiro_home(env).map(|d| d.join("sessions"))
}

/// Live status of each pane whose session is open in a process `alive` accepts.
pub fn read(dir: &Path, panes: &[Pane], alive: impl Fn(i32) -> bool) -> Result<Sessions> {
    if panes.is_empty() {
        return Ok(Sessions::default());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Sessions::default()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to list Kiro sessions in {}", dir.display()));
        }
    };
    let mut hashes = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to list Kiro sessions in {}", dir.display()))?;
        hashes.push(entry.path());
    }
    let mut out = Sessions::default();
    for p in panes {
        // The id becomes a path component.
        if !valid_id(&p.session_id) {
            continue;
        }
        let Some(session) = hashes
            .iter()
            .map(|h| h.join(&p.session_id))
            .find(|d| d.is_dir())
        else {
            continue;
        };
        match read_session(&session, &alive) {
            Ok(Some((status, updated_ms))) => out.live.push(Live {
                pane: p.pane.clone(),
                session: p.session.clone(),
                status,
                updated_ms,
            }),
            Ok(None) => {}
            Err(e) => out.skipped.push(format!(
                "failed to read Kiro session {}: {e}",
                session.display()
            )),
        }
    }
    Ok(out)
}

/// `sess_` followed only by ASCII letters, digits, `-`, and `_`.
fn valid_id(id: &str) -> bool {
    id.strip_prefix("sess_")
        .is_some_and(|rest| !rest.is_empty())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `Ok(None)` for a session that is closed, stale, torn mid-write, or not yet decided.
fn read_session(dir: &Path, alive: impl Fn(i32) -> bool) -> io::Result<Option<(LiveStatus, u64)>> {
    let Some(lock) = read_optional(&dir.join(".lock"))? else {
        return Ok(None);
    };
    match serde_json::from_str::<Lock>(&lock) {
        Ok(lock) if alive(lock.pid) => {}
        _ => return Ok(None),
    }
    let log = match read_tail(&dir.join("messages.jsonl"), TAIL_BYTES) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some((status, at)) = status_from_log(&log) else {
        return Ok(None);
    };
    if status != LiveStatus::Idle {
        return Ok(Some((status, at)));
    }
    // A question the agent asked outlives the turn that asked it.
    let Some(text) = read_optional(&dir.join("session.json"))? else {
        return Ok(None);
    };
    let Ok(file) = serde_json::from_str::<SessionFile>(&text) else {
        return Ok(None);
    };
    let status = match file.status.as_deref() {
        Some("waiting_on_user") => LiveStatus::Asking,
        _ => LiveStatus::Idle,
    };
    Ok(Some((status, at)))
}

/// The file's text, or `None` if it does not exist.
fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// About the last `max` bytes of the file, starting at a line boundary.
fn read_tail(path: &Path, max: u64) -> io::Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max);
    // One byte early, so a cut that lands on a line boundary keeps the line after it.
    let from = start.saturating_sub(1);
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.take(len - from).read_to_end(&mut buf)?;
    let body = if start == 0 {
        &buf[..]
    } else {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => &buf[i + 1..],
            None => &[],
        }
    };
    Ok(String::from_utf8_lossy(body).into_owned())
}

/// The session's status from its event log, newest event first, with the time of the newest
/// deciding event: the status changed then, even when an older event says what it is.
fn status_from_log(text: &str) -> Option<(LiveStatus, u64)> {
    let mut resolved = HashSet::new();
    let mut newest = None;
    for line in text.lines().rev() {
        if !DECIDING.iter().any(|t| line.contains(t)) {
            continue;
        }
        // The last line may be half written; the next read sees it whole.
        let Ok(line) = serde_json::from_str::<LogLine>(line) else {
            continue;
        };
        let Some(at) = parse_utc_ms(&line.timestamp) else {
            continue;
        };
        let p = line.payload;
        let status = match p.kind.as_str() {
            "interaction_resolved" => {
                resolved.extend(p.tool_call_id);
                None
            }
            "pending_interaction" => {
                let answered = p.tool_call_id.is_some_and(|id| resolved.contains(&id));
                (!answered).then_some(LiveStatus::Waiting)
            }
            "turn_end" if p.stop_reason.as_deref() == Some("error") => Some(LiveStatus::Failed),
            "turn_end" => Some(LiveStatus::Idle),
            "turn_start" => Some(LiveStatus::Busy),
            // Another type whose line merely mentions a deciding one.
            _ => continue,
        };
        let newest = *newest.get_or_insert(at);
        if let Some(status) = status {
            return Some((status, newest));
        }
    }
    None
}

/// Unix ms from `YYYY-MM-DDTHH:MM:SS[.fff]Z`.
fn parse_utc_ms(s: &str) -> Option<u64> {
    fn num(p: &str) -> Option<u64> {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    }
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let [y, mo, d]: [&str; 3] = date.split('-').collect::<Vec<_>>().try_into().ok()?;
    let [h, mi, sec]: [&str; 3] = hms.split(':').collect::<Vec<_>>().try_into().ok()?;
    let (y, mo, d) = (num(y)?, num(mo)?, num(d)?);
    let (h, mi, sec) = (num(h)?, num(mi)?, num(sec)?);
    if y < 1970
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || h > 23
        || mi > 59
        || sec > 60
    {
        return None;
    }
    let ms = if frac.is_empty() {
        0
    } else {
        num(frac)?;
        num(&format!("{:0<3}", &frac[..frac.len().min(3)]))?
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec;
    Some(secs * 1000 + ms)
}

/// Days since 1970-01-01 of a proleptic Gregorian date on or after it (Hinnant's algorithm).
fn days_from_civil(y: u64, m: u64, d: u64) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-31T23:59:00.000Z
    const T0: u64 = 1_793_491_140_000;

    /// A log line `sec` seconds after `T0`.
    fn ev(sec: u64, payload: &str) -> String {
        format!(r#"{{"timestamp":"2026-10-31T23:59:{sec:02}.000Z","payload":{payload}}}"#)
    }

    fn log(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn turn_start(sec: u64) -> String {
        ev(sec, r#"{"type":"turn_start"}"#)
    }

    fn turn_end(sec: u64, reason: &str) -> String {
        ev(
            sec,
            &format!(r#"{{"type":"turn_end","stopReason":"{reason}"}}"#),
        )
    }

    fn pending(sec: u64, id: &str) -> String {
        ev(
            sec,
            &format!(r#"{{"type":"pending_interaction","toolCallId":"{id}"}}"#),
        )
    }

    fn resolved(sec: u64, id: &str) -> String {
        ev(
            sec,
            &format!(r#"{{"type":"interaction_resolved","toolCallId":"{id}"}}"#),
        )
    }

    #[test]
    fn turn_outcomes() {
        for (reason, want) in [
            ("end_turn", LiveStatus::Idle),
            ("cancelled", LiveStatus::Idle),
            ("error", LiveStatus::Failed),
        ] {
            let text = log(&[
                turn_start(1),
                ev(2, r#"{"type":"assistant_message","text":"hi"}"#),
                turn_end(3, reason),
            ]);
            assert_eq!(status_from_log(&text), Some((want, T0 + 3000)), "{reason}");
        }
        let text = log(&[turn_start(1), turn_end(2, "error"), turn_start(3)]);
        assert_eq!(status_from_log(&text), Some((LiveStatus::Busy, T0 + 3000)));
    }

    #[test]
    fn open_approval_waits_until_resolved() {
        let mut lines = vec![
            turn_start(1),
            pending(2, "tc_a"),
            pending(3, "tc_b"),
            resolved(4, "tc_b"),
        ];
        assert_eq!(
            status_from_log(&log(&lines)),
            Some((LiveStatus::Waiting, T0 + 4000)),
            "tc_a is still open"
        );
        lines.push(resolved(5, "tc_a"));
        assert_eq!(
            status_from_log(&log(&lines)),
            Some((LiveStatus::Busy, T0 + 5000)),
            "busy since the last answer, not since the turn started"
        );
    }

    #[test]
    fn prose_mentioning_an_event_type_is_ignored() {
        let text = log(&[
            turn_start(1),
            ev(
                2,
                r#"{"type":"assistant_message","text":"Kiro writes turn_end, or \"turn_end\" quoted, when done"}"#,
            ),
            ev(3, r#"{"type":"tool_result","output":["turn_end"]}"#),
        ]);
        assert_eq!(status_from_log(&text), Some((LiveStatus::Busy, T0 + 1000)));
    }

    #[test]
    fn half_written_last_line_is_ignored() {
        let mut text = log(&[turn_start(1), turn_end(2, "end_turn")]);
        text.push_str(r#"{"timestamp":"2026-10-31T23:59:03.000Z","payload":{"type":"turn_start""#);
        assert_eq!(status_from_log(&text), Some((LiveStatus::Idle, T0 + 2000)));
        assert_eq!(status_from_log(""), None);
        assert_eq!(
            status_from_log(&log(&[ev(1, r#"{"type":"user_message"}"#)])),
            None
        );
    }

    #[test]
    fn parses_utc_timestamps() {
        assert_eq!(parse_utc_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_utc_ms("2026-10-31T23:59:58.123Z"),
            Some(1_793_491_198_123)
        );
        assert_eq!(
            parse_utc_ms("2024-02-29T12:00:00Z"),
            Some(1_709_208_000_000)
        );
        assert_eq!(
            parse_utc_ms("2024-02-29T12:00:00.5Z"),
            Some(1_709_208_000_500)
        );
        for bad in [
            "",
            "2026-10-31T23:59:58.000",
            "2026-10-31 23:59:58Z",
            "2026-13-01T00:00:00Z",
            "2026-10-31T24:00:00Z",
            "2026-10-31T23:59:58.x1Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_utc_ms(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn tail_cut_drops_the_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("messages.jsonl");
        let text = "first line\nsecond\nthird\n";
        std::fs::write(&path, text).unwrap();
        let len = text.len() as u64;
        assert_eq!(
            read_tail(&path, len - 12).unwrap(),
            "third\n",
            "cut inside `second`"
        );
        assert_eq!(
            read_tail(&path, len - 11).unwrap(),
            "second\nthird\n",
            "cut on a line boundary keeps the next line"
        );
        assert_eq!(read_tail(&path, len).unwrap(), text);
        assert_eq!(read_tail(&path, 1 << 20).unwrap(), text);
    }

    fn pane(pane: &str, id: &str) -> Pane {
        Pane {
            pane: pane.into(),
            session: "main".into(),
            session_id: id.into(),
        }
    }

    /// Writes a session under `root/<hash>/<id>/`; `None` leaves a file out.
    fn write_session(
        root: &Path,
        hash: &str,
        id: &str,
        lock: Option<&str>,
        status: Option<&str>,
    ) -> PathBuf {
        let dir = root.join(hash).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(lock) = lock {
            std::fs::write(dir.join(".lock"), lock).unwrap();
        }
        std::fs::write(
            dir.join("messages.jsonl"),
            log(&[turn_start(1), turn_end(2, "end_turn")]),
        )
        .unwrap();
        if let Some(status) = status {
            std::fs::write(
                dir.join("session.json"),
                format!(r#"{{"id":"{id}","status":"{status}"}}"#),
            )
            .unwrap();
        }
        dir
    }

    fn live(pane: &str, status: LiveStatus) -> Live {
        Live {
            pane: pane.into(),
            session: "main".into(),
            status,
            updated_ms: T0 + 2000,
        }
    }

    #[test]
    fn reads_open_sessions_across_hash_dirs() {
        let root = tempfile::tempdir().unwrap();
        write_session(
            root.path(),
            "h1",
            "sess_a",
            Some(r#"{"pid":7}"#),
            Some("idle"),
        );
        write_session(
            root.path(),
            "h2",
            "sess_b-2",
            Some(r#"{"pid":8}"#),
            Some("waiting_on_user"),
        );
        let panes = [pane("%1", "sess_a"), pane("%2", "sess_b-2")];
        let got = read(root.path(), &panes, |_| true).unwrap();
        assert!(got.skipped.is_empty(), "{:?}", got.skipped);
        assert_eq!(
            got.live,
            vec![live("%1", LiveStatus::Idle), live("%2", LiveStatus::Asking),]
        );
    }

    #[test]
    fn skips_sessions_without_a_usable_status() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        write_session(r, "h", "sess_ok", Some(r#"{"pid":7}"#), Some("idle"));
        write_session(r, "h", "sess_dead", Some(r#"{"pid":9}"#), Some("idle"));
        write_session(r, "h", "sess_torn", Some(r#"{"pid":"#), Some("idle"));
        write_session(r, "h", "sess_closed", None, Some("idle"));
        write_session(r, "h", "sess_nostatus", Some(r#"{"pid":7}"#), None);
        // `h/../sess_x` would resolve to this one.
        write_session(r, "", "sess_x", Some(r#"{"pid":7}"#), Some("idle"));
        let panes = [
            pane("%1", "sess_ok"),
            pane("%2", "sess_dead"),
            pane("%3", "sess_torn"),
            pane("%4", "sess_closed"),
            pane("%5", "sess_missing"),
            pane("%6", "../sess_x"),
            pane("%7", "sess_nostatus"),
            pane("%8", "other"),
            pane("%9", "sess_"),
        ];
        let got = read(r, &panes, |pid| pid != 9).unwrap();
        assert!(got.skipped.is_empty(), "{:?}", got.skipped);
        assert_eq!(got.live, vec![live("%1", LiveStatus::Idle)]);
    }

    #[test]
    fn one_unreadable_session_does_not_hide_the_rest() {
        let root = tempfile::tempdir().unwrap();
        write_session(
            root.path(),
            "h",
            "sess_a",
            Some(r#"{"pid":7}"#),
            Some("idle"),
        );
        let bad = write_session(
            root.path(),
            "h",
            "sess_b",
            Some(r#"{"pid":7}"#),
            Some("idle"),
        );
        std::fs::remove_file(bad.join("messages.jsonl")).unwrap();
        std::fs::create_dir(bad.join("messages.jsonl")).unwrap();
        let panes = [pane("%1", "sess_a"), pane("%2", "sess_b")];
        let got = read(root.path(), &panes, |_| true).unwrap();
        assert_eq!(got.live, vec![live("%1", LiveStatus::Idle)]);
        assert_eq!(got.skipped.len(), 1);
        assert!(got.skipped[0].contains("sess_b"), "{:?}", got.skipped);
    }

    #[test]
    fn missing_dir_has_no_sessions() {
        let root = tempfile::tempdir().unwrap();
        let got = read(&root.path().join("nope"), &[pane("%1", "sess_a")], |_| true).unwrap();
        assert!(got.live.is_empty() && got.skipped.is_empty());
    }

    #[test]
    fn sessions_dir_prefers_kiro_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            sessions_dir(env(&[("KIRO_HOME", "/k"), ("HOME", "/h")])),
            Some(PathBuf::from("/k/sessions"))
        );
        assert_eq!(
            sessions_dir(env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.kiro/sessions"))
        );
        assert_eq!(sessions_dir(env(&[])), None);
    }
}
