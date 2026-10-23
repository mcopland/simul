use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use super::state::{PaneRecord, State, Transition};
use crate::config::NotifyConfig;
use crate::log;
use crate::tmux::Tmux;

/// Decides which transitions notify: entering blocked or done, while unseen, not repeated.
#[derive(Debug, Default)]
pub struct Gate {
    last: HashMap<(String, State), u64>,
}

impl Gate {
    pub fn admit(&mut self, t: &Transition, viewed: bool, now: u64, dedupe_ms: u64) -> bool {
        if !matches!(t.to, State::Blocked | State::Done) || viewed {
            return false;
        }
        let key = (t.pane.clone(), t.to);
        if self
            .last
            .get(&key)
            .is_some_and(|&at| now.saturating_sub(at) < dedupe_ms)
        {
            return false;
        }
        self.last.insert(key, now);
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: String,
}

pub fn message(t: &Transition, rec: Option<&PaneRecord>) -> Message {
    let agent = t.agent.label();
    let title = match t.to {
        State::Blocked => format!("{agent} needs approval"),
        _ => format!("{agent} finished"),
    };
    let mut body = rec.map_or_else(|| t.pane.clone(), crate::cli::ls::location);
    if body == "-" {
        body.clone_from(&t.pane);
    }
    if let Some(branch) = rec.and_then(|r| r.branch.as_deref()) {
        body.push_str(&format!(" ({branch})"));
    }
    if let (State::Blocked, Some(tool)) = (t.to, rec.and_then(|r| r.pending_tool.as_deref())) {
        body.push_str(&format!(" wants to run {tool}"));
    }
    Message { title, body }
}

/// AppleScript string literal.
pub fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// PowerShell single-quoted string literal.
pub fn powershell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// tmux expands formats in display-message, so a literal `#` must be doubled.
pub fn tmux_literal(s: &str) -> String {
    s.replace('#', "##")
}

/// Always shows a tmux status message; desktop notifications and the bell are opt-in.
pub async fn deliver(tmux: Tmux, msg: Message, cfg: NotifyConfig) {
    if cfg.desktop
        && let Err(e) = desktop(&msg).await
    {
        log!("desktop notification failed: {e:#}");
    }
    if let Err(e) = alert_clients(&tmux, &msg, cfg.bell).await {
        log!("tmux notification failed: {e:#}");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Wsl,
    Linux,
    Other,
}

fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        if release.to_lowercase().contains("microsoft") {
            Platform::Wsl
        } else {
            Platform::Linux
        }
    } else {
        Platform::Other
    }
}

pub async fn desktop(msg: &Message) -> Result<()> {
    match platform() {
        Platform::MacOs => {
            let script = format!(
                "display notification {} with title {}",
                applescript_quote(&msg.body),
                applescript_quote(&msg.title)
            );
            run("osascript", &["-e", &script]).await
        }
        Platform::Wsl => {
            let script = format!(
                "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
                 $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
                 $x = $t.GetElementsByTagName('text'); \
                 $x.Item(0).AppendChild($t.CreateTextNode({})) > $null; \
                 $x.Item(1).AppendChild($t.CreateTextNode({})) > $null; \
                 [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($t))",
                powershell_quote(&msg.title),
                powershell_quote(&msg.body)
            );
            run(
                "powershell.exe",
                &["-NoProfile", "-NonInteractive", "-Command", &script],
            )
            .await
        }
        Platform::Linux => linux_desktop(msg).await,
        Platform::Other => Ok(()),
    }
}

#[cfg(target_os = "linux")]
async fn linux_desktop(msg: &Message) -> Result<()> {
    let msg = msg.clone();
    tokio::task::spawn_blocking(move || {
        notify_rust::Notification::new()
            .summary(&msg.title)
            .body(&msg.body)
            .show()
            .map(|_| ())
            .context("failed to send D-Bus notification")
    })
    .await
    .context("notification task panicked")?
}

#[cfg(not(target_os = "linux"))]
async fn linux_desktop(_msg: &Message) -> Result<()> {
    Ok(())
}

async fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Shows the message on every attached terminal client, ringing its bell if asked.
async fn alert_clients(tmux: &Tmux, msg: &Message, bell: bool) -> Result<()> {
    let clients = tmux
        .run_async(&["list-clients", "-F", "#{client_tty}\t#{client_flags}"])
        .await?;
    let text = tmux_literal(&format!("{}: {}", msg.title, msg.body));
    for line in clients.lines() {
        let Some((tty, flags)) = line.split_once('\t') else {
            continue;
        };
        if tty.is_empty() || flags.split(',').any(|f| f == "control-mode") {
            continue;
        }
        // One unreachable client must not keep the alert from the others.
        if let Err(e) = tmux
            .run_async(&["display-message", "-c", tty, "-d", "4000", &text])
            .await
        {
            log!("{e:#}");
        }
        if bell && let Err(e) = ring_bell(tty) {
            log!("{e:#}");
        }
    }
    Ok(())
}

fn ring_bell(tty: &str) -> Result<()> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .open(tty)
        .and_then(|mut f| f.write_all(b"\x07"))
        .with_context(|| format!("failed to ring bell on {tty}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::Agent;

    fn t(pane: &str, to: State) -> Transition {
        Transition {
            pane: pane.into(),
            agent: Agent::Claude,
            from: Some(State::Working),
            to,
        }
    }

    fn rec(branch: Option<&str>) -> PaneRecord {
        serde_json::from_value(serde_json::json!({
            "pane": "%1", "agent": "claude", "state": "blocked", "since_ms": 0,
            "last_event_ms": 0, "session_id": null, "cwd": "/w/repo",
            "branch": branch, "pending_tool": "Bash", "session": "dev", "window": 2,
        }))
        .unwrap()
    }

    #[test]
    fn message_names_agent_place_and_branch() {
        let m = message(&t("%1", State::Blocked), Some(&rec(Some("main"))));
        assert_eq!(m.title, "Claude needs approval");
        assert_eq!(m.body, "dev:2 (main) wants to run Bash");
        let m = message(&t("%1", State::Done), Some(&rec(None)));
        assert_eq!(m.title, "Claude finished");
        assert_eq!(m.body, "dev:2");
        let m = message(&t("%7", State::Done), None);
        assert_eq!(m.body, "%7");
    }

    #[test]
    fn quoting() {
        assert_eq!(applescript_quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(powershell_quote("it's"), "'it''s'");
        assert_eq!(tmux_literal("#{x} #1"), "##{x} ##1");
    }

    #[test]
    fn only_blocked_and_done_notify() {
        let mut g = Gate::default();
        assert!(g.admit(&t("%1", State::Blocked), false, 0, 10_000));
        assert!(g.admit(&t("%2", State::Done), false, 0, 10_000));
        for s in [State::Working, State::Idle, State::Unknown] {
            assert!(!g.admit(&t("%3", s), false, 0, 10_000), "{s:?}");
        }
    }

    #[test]
    fn viewed_panes_are_skipped() {
        let mut g = Gate::default();
        assert!(!g.admit(&t("%1", State::Blocked), true, 0, 10_000));
        assert!(
            g.admit(&t("%1", State::Blocked), false, 1, 10_000),
            "skip is not recorded"
        );
    }

    #[test]
    fn repeats_within_window_are_skipped() {
        let mut g = Gate::default();
        assert!(g.admit(&t("%1", State::Blocked), false, 0, 10_000));
        assert!(!g.admit(&t("%1", State::Blocked), false, 9_999, 10_000));
        assert!(g.admit(&t("%1", State::Done), false, 9_999, 10_000));
        assert!(g.admit(&t("%2", State::Blocked), false, 9_999, 10_000));
        assert!(g.admit(&t("%1", State::Blocked), false, 10_000, 10_000));
    }
}
