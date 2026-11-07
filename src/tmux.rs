use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context, Result, bail};

use crate::daemon::state::{PaneInfo, Snapshot};
use crate::protocol::{PaneSummary, WindowInfo};

pub const PANES_FORMAT: &str = "#{pane_id}\t#{session_name}\t#{window_index}\t#{window_id}\t#{pane_active}\t#{window_active}\t#{pane_current_command}\t#{pane_current_path}\t#{@simul-sidebar}\t#{window_panes}\t#{@simul-collapsed}\t#{pane_width}\t#{window_width}\t#{window_zoomed_flag}\t#{?pane_start_command,#{pane_start_command},#{?default-command,#{default-command},#{default-shell}}}\t#{window_name}";
pub const CLIENTS_FORMAT: &str = "#{client_session}\t#{client_flags}\t#{client_tty}";

#[derive(Clone, Debug)]
pub struct Tmux {
    socket: PathBuf,
}

impl Tmux {
    pub fn new(socket: impl Into<PathBuf>) -> Tmux {
        Tmux {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn check(&self, args: &[&str], out: Output) -> Result<String> {
        if !out.status.success() {
            bail!(
                "tmux -S {} {} failed: {}",
                self.socket.display(),
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = std::process::Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn tmux {}", args.join(" ")))?;
        self.check(args, out)
    }

    pub async fn run_async(&self, args: &[&str]) -> Result<String> {
        let out = tokio::process::Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .await
            .with_context(|| format!("failed to spawn tmux {}", args.join(" ")))?;
        self.check(args, out)
    }

    pub async fn snapshot(&self, focus_events: bool) -> Result<Snapshot> {
        let panes = self
            .run_async(&["list-panes", "-a", "-F", PANES_FORMAT])
            .await?;
        let clients = self
            .run_async(&["list-clients", "-F", CLIENTS_FORMAT])
            .await?;
        Ok(parse_snapshot(&panes, &clients, focus_events))
    }
}

/// Builds a snapshot from `list-panes -a` and `list-clients` output.
///
/// A pane is viewed when it is the active pane of the active window of a session shown by a
/// client that has terminal focus. Without `focus-events` tmux cannot know focus, so any
/// attached client counts.
pub fn parse_snapshot(panes: &str, clients: &str, focus_events: bool) -> Snapshot {
    let viewing_sessions: HashSet<&str> = clients
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let session = f.next()?;
            let flags: Vec<&str> = f.next()?.split(',').collect();
            let counts =
                !flags.contains(&"control-mode") && (!focus_events || flags.contains(&"focused"));
            (counts && !session.is_empty()).then_some(session)
        })
        .collect();
    let mut snap = Snapshot::default();
    let mut seen = HashSet::new();
    for line in panes.lines() {
        // The window name comes last so a tab inside it stays part of the name.
        let f: Vec<&str> = line.splitn(16, '\t').collect();
        let [
            id,
            session,
            window,
            window_id,
            pane_active,
            window_active,
            cmd,
            path,
            sidebar,
            window_panes,
            collapsed,
            width,
            window_width,
            zoomed,
            launched,
            window_name,
        ] = f[..]
        else {
            continue;
        };
        let (Ok(window), Ok(window_panes), Ok(width), Ok(window_width)) = (
            window.parse(),
            window_panes.parse(),
            width.parse(),
            window_width.parse(),
        ) else {
            continue;
        };
        if pane_active == "1" && window_active == "1" && viewing_sessions.contains(session) {
            snap.viewed.insert(id.to_owned());
        }
        if !seen.insert(id) {
            continue;
        }
        snap.panes.push(PaneInfo {
            id: id.to_owned(),
            session: session.to_owned(),
            window,
            window_id: window_id.to_owned(),
            current_command: cmd.to_owned(),
            launched: launched.to_owned(),
            path: path.to_owned(),
            is_sidebar: sidebar == "1",
            window_panes,
            active: pane_active == "1",
            session_collapsed: collapsed == "1",
            width,
            window_width,
            window_zoomed: zoomed == "1",
            window_name: window_name.to_owned(),
        });
    }
    snap
}

/// One entry per window with its non-sidebar panes, both in snapshot (tmux) order.
pub fn windows(snap: &Snapshot) -> Vec<WindowInfo> {
    let mut out: Vec<WindowInfo> = Vec::new();
    for p in &snap.panes {
        let i = match out.iter().position(|w| w.id == p.window_id) {
            Some(i) => i,
            None => {
                out.push(WindowInfo {
                    id: p.window_id.clone(),
                    session: p.session.clone(),
                    index: p.window,
                    name: p.window_name.clone(),
                    panes: Vec::new(),
                });
                out.len() - 1
            }
        };
        if !p.is_sidebar {
            out[i].panes.push(PaneSummary {
                id: p.id.clone(),
                command: p.current_command.clone(),
                active: p.active,
                cwd: p.path.clone(),
                git: None,
                run: None,
            });
        }
    }
    out
}

/// Parses `tmux -V` output such as `tmux 3.3a` or `tmux next-3.4`.
pub fn parse_version(output: &str) -> Option<(u32, u32)> {
    let word = output.split_whitespace().nth(1)?;
    let digits = word.trim_start_matches(|c: char| !c.is_ascii_digit());
    let (major, rest) = digits.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Quotes `s` as a tmux double-quoted string, where `$`, `"`, and `\\` are special.
pub fn tmux_double_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANES: &str = "%1\tmain\t1\t@1\t1\t1\t2.1.307\t/w/a\t\t2\t\t120\t200\t0\t\tapi\n\
%2\tmain\t1\t@1\t0\t1\tzsh\t/w/a\t\t2\t\t79\t200\t0\t\tapi\n\
%3\tmain\t2\t@2\t1\t0\tclaude\t/w/b\t\t1\t\t200\t200\t0\t\tweb\n\
%4\twork\t1\t@3\t1\t1\tclaude\t/w/c\t\t2\t1\t150\t200\t1\t\tmy\ttabbed name\n\
%5\twork\t1\t@3\t0\t1\tsimul\t/w/c\t1\t2\t1\t49\t200\t1\t\"exec simul sidebar\"\tmy\ttabbed name\n";

    #[test]
    fn parses_panes() {
        let snap = parse_snapshot(PANES, "", true);
        assert_eq!(snap.panes.len(), 5);
        assert_eq!(
            snap.panes[0],
            PaneInfo {
                id: "%1".into(),
                session: "main".into(),
                window: 1,
                window_id: "@1".into(),
                current_command: "2.1.307".into(),
                launched: String::new(),
                path: "/w/a".into(),
                is_sidebar: false,
                window_panes: 2,
                active: true,
                session_collapsed: false,
                width: 120,
                window_width: 200,
                window_zoomed: false,
                window_name: "api".into(),
            }
        );
        assert!(snap.panes[4].is_sidebar);
        assert_eq!(snap.panes[4].launched, "\"exec simul sidebar\"");
        assert_eq!(snap.panes[4].width, 49);
        assert!(snap.panes[4].window_zoomed);
        assert!(snap.panes[3].session_collapsed, "a session option");
        assert!(!snap.panes[1].active);
        assert_eq!(snap.panes[3].window_name, "my\ttabbed name");
    }

    #[test]
    fn viewed_requires_focused_client_with_focus_events() {
        let clients =
            "main\tattached,focused,UTF-8\t/dev/ttys001\nwork\tattached,UTF-8\t/dev/ttys002\n";
        let snap = parse_snapshot(PANES, clients, true);
        assert_eq!(snap.viewed, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn unfocused_terminal_views_nothing() {
        let clients = "main\tattached,UTF-8\t/dev/ttys001\n";
        assert!(parse_snapshot(PANES, clients, true).viewed.is_empty());
    }

    #[test]
    fn without_focus_events_any_attached_client_counts() {
        let clients = "main\tattached,UTF-8\t/dev/ttys001\nwork\tattached,UTF-8\t/dev/ttys002\n";
        let snap = parse_snapshot(PANES, clients, false);
        assert_eq!(
            snap.viewed,
            HashSet::from(["%1".to_string(), "%4".to_string()])
        );
    }

    #[test]
    fn control_mode_clients_never_view() {
        let clients = "work\tattached,focused,control-mode,no-output\t\n";
        assert!(parse_snapshot(PANES, clients, false).viewed.is_empty());
        assert!(parse_snapshot(PANES, clients, true).viewed.is_empty());
    }

    #[test]
    fn linked_windows_are_listed_once() {
        let panes = "%1\ta\t1\t@1\t1\t0\tclaude\t/w\t\t1\t\t80\t80\t0\t\tx\n%1\tb\t3\t@1\t1\t1\tclaude\t/w\t\t1\t\t80\t80\t0\t\tx\n";
        let snap = parse_snapshot(panes, "b\tattached,focused\t/dev/ttys1\n", true);
        assert_eq!(snap.panes.len(), 1);
        assert_eq!(snap.viewed, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn lists_each_window_once_with_its_panes() {
        let pane = |id: &str, command: &str, active, cwd: &str| PaneSummary {
            id: id.into(),
            command: command.into(),
            active,
            cwd: cwd.into(),
            git: None,
            run: None,
        };
        let w = windows(&parse_snapshot(PANES, "", true));
        assert_eq!(
            w,
            [
                WindowInfo {
                    id: "@1".into(),
                    session: "main".into(),
                    index: 1,
                    name: "api".into(),
                    panes: vec![
                        pane("%1", "2.1.307", true, "/w/a"),
                        pane("%2", "zsh", false, "/w/a"),
                    ],
                },
                WindowInfo {
                    id: "@2".into(),
                    session: "main".into(),
                    index: 2,
                    name: "web".into(),
                    panes: vec![pane("%3", "claude", true, "/w/b")],
                },
                WindowInfo {
                    id: "@3".into(),
                    session: "work".into(),
                    index: 1,
                    name: "my\ttabbed name".into(),
                    panes: vec![pane("%4", "claude", true, "/w/c")],
                },
            ]
        );
    }

    #[test]
    fn ignores_malformed_lines() {
        let snap = parse_snapshot(
            "garbage\n%9\tx\tnotanumber\t@1\t1\t1\tc\t/\t\t1\n",
            "",
            true,
        );
        assert!(snap.panes.is_empty());
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("tmux 3.8\n"), Some((3, 8)));
        assert_eq!(parse_version("tmux 3.3a"), Some((3, 3)));
        assert_eq!(parse_version("tmux next-3.4"), Some((3, 4)));
        assert_eq!(parse_version("tmux master"), None);
    }

    #[test]
    fn quotes_for_tmux() {
        assert_eq!(tmux_double_quote("a 'b'"), "\"a 'b'\"");
        assert_eq!(tmux_double_quote(r#"x"$y\"#), r#""x\"\$y\\""#);
    }

    #[test]
    fn quotes_for_shell() {
        assert_eq!(shell_quote("/a b/simul"), "'/a b/simul'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
