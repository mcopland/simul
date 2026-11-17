use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context, Result, bail};

use crate::protocol::{PaneSummary, WindowInfo};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: String,
    pub session: String,
    pub window: u32,
    pub window_id: String,
    pub current_command: String,
    /// What started the pane: its start command, else tmux's `default-command`, else its
    /// session's `default-shell`. tmux quotes a start command that has spaces.
    pub launched: String,
    pub path: String,
    pub is_sidebar: bool,
    pub window_panes: u32,
    /// Whether this is the active pane of its window.
    pub active: bool,
    /// Whether the pane's session carries `@simul-collapsed`, folding it in the sidebar.
    pub session_collapsed: bool,
    /// Columns: the pane's, and its window's.
    pub width: u16,
    pub window_width: u16,
    /// Whether a pane in this window is zoomed.
    pub window_zoomed: bool,
    /// Titled `SIDEBAR_TITLE` but untagged: a sidebar as a session restore such as
    /// tmux-resurrect brings it back, which keeps pane titles but not pane options.
    pub restored_sidebar: bool,
    pub window_name: String,
    /// What the pane's shell reported of its commands with OSC 133; `None` until it reports
    /// one, as a shell without that integration never does.
    pub shell_command: Option<ShellCommand>,
}

/// A pane's current or last command, as its shell marked it with OSC 133.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShellCommand {
    pub running: bool,
    /// The last finished command's exit status, if the shell sent one.
    pub status: Option<i32>,
    /// Unix seconds the current or last command started.
    pub started: u64,
    /// Unix seconds the last command ended; `None` until one has.
    pub ended: Option<u64>,
}

impl ShellCommand {
    /// From `pane_command_running`, `_status`, `_start_time`, and `_end_time`.
    fn parse(running: &str, status: &str, started: &str, ended: &str) -> Option<ShellCommand> {
        Some(ShellCommand {
            running: running == "1",
            status: status.parse().ok(),
            started: started.parse().ok()?,
            ended: ended.parse().ok(),
        })
    }
}

/// A point-in-time view of the tmux server used to reconcile tracked panes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub panes: Vec<PaneInfo>,
    /// Panes the user is looking at right now.
    pub viewed: HashSet<String>,
    /// Windows on some terminal: the current window of a session a client shows, whether
    /// or not that terminal has focus. With no client attached, the current window of every
    /// session, where one would land on attaching.
    pub shown_windows: HashSet<String>,
}

/// The pane title every sidebar carries, so one brought back by a session restore can be found.
/// `PANES_FORMAT` repeats it.
pub const SIDEBAR_TITLE: &str = "simul-sidebar";

pub const PANES_FORMAT: &str = "#{pane_id}\t#{session_name}\t#{window_index}\t#{window_id}\t#{pane_active}\t#{window_active}\t#{pane_current_command}\t#{pane_current_path}\t#{@simul-sidebar}\t#{window_panes}\t#{@simul-collapsed}\t#{pane_width}\t#{window_width}\t#{window_zoomed_flag}\t#{==:#{pane_title},simul-sidebar}\t#{?pane_start_command,#{pane_start_command},#{?default-command,#{default-command},#{default-shell}}}\t#{pane_command_running}\t#{pane_command_status}\t#{pane_command_start_time}\t#{pane_command_end_time}\t#{window_name}";
/// Client lines follow the `focus-events` line in `Tmux::snapshot`'s output; the prefix
/// checks that each one is a client line.
pub const CLIENTS_FORMAT: &str = "client\t#{client_session}\t#{client_flags}\t#{client_tty}";
const CLIENT_PREFIX: &str = "client\t";
const FOCUS_EVENTS_PREFIX: &str = "focus-events ";

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

    /// Panes, clients, and `focus-events`, from one tmux process.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        let args = chain([
            vec!["list-panes", "-a", "-F", PANES_FORMAT],
            vec!["show-options", "-g", "focus-events"],
            vec!["list-clients", "-F", CLIENTS_FORMAT],
        ]);
        let out = self.run_async(&args).await?;
        let (panes, clients, focus_events) = split_snapshot(&out);
        Ok(parse_snapshot(&panes, &clients, focus_events))
    }
}

/// Joins tmux commands into one invocation's arguments, separated by `;`.
pub fn chain<'a>(cmds: impl IntoIterator<Item = Vec<&'a str>>) -> Vec<&'a str> {
    let mut args = Vec::new();
    for cmd in cmds {
        if !args.is_empty() {
            args.push(";");
        }
        args.extend(cmd);
    }
    args
}

/// Splits `Tmux::snapshot`'s output into `list-panes` lines, `list-clients` lines without
/// their prefix, and whether `focus-events` is on. Panes come before the option line and
/// clients after it; going by position, a pane field holding a newline, such as a path,
/// can't pass for a client.
pub fn split_snapshot(out: &str) -> (String, String, bool) {
    let lines: Vec<&str> = out.lines().collect();
    let option = lines
        .iter()
        .rposition(|l| l.starts_with(FOCUS_EVENTS_PREFIX));
    let (pane_lines, rest) = lines.split_at(option.unwrap_or(lines.len()));
    let mut panes = String::new();
    for line in pane_lines {
        panes.push_str(line);
        panes.push('\n');
    }
    let mut clients = String::new();
    let mut focus_events = false;
    if let Some((option, client_lines)) = rest.split_first() {
        focus_events = option[FOCUS_EVENTS_PREFIX.len()..].trim() == "on";
        for client in client_lines
            .iter()
            .filter_map(|l| l.strip_prefix(CLIENT_PREFIX))
        {
            clients.push_str(client);
            clients.push('\n');
        }
    }
    (panes, clients, focus_events)
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
    let showing_sessions: HashSet<&str> = clients
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let session = f.next()?;
            let control = f.next()?.split(',').any(|flag| flag == "control-mode");
            (!control && !session.is_empty()).then_some(session)
        })
        .collect();
    let mut snap = Snapshot::default();
    let mut seen = HashSet::new();
    for line in panes.lines() {
        // The window name comes last so a tab inside it stays part of the name.
        let f: Vec<&str> = line.splitn(21, '\t').collect();
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
            titled,
            launched,
            command_running,
            command_status,
            command_started,
            command_ended,
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
        // Before the check below: a linked window is listed once per session, and may be
        // current in any of them.
        if window_active == "1"
            && (showing_sessions.is_empty() || showing_sessions.contains(session))
        {
            snap.shown_windows.insert(window_id.to_owned());
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
            restored_sidebar: titled == "1" && sidebar != "1",
            window_name: window_name.to_owned(),
            shell_command: ShellCommand::parse(
                command_running,
                command_status,
                command_started,
                command_ended,
            ),
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
                    sidebar: None,
                    shown: snap.shown_windows.contains(&p.window_id),
                });
                out.len() - 1
            }
        };
        if p.is_sidebar {
            out[i].sidebar = Some(p.id.clone());
        } else {
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

    const PANES: &str = "%1\tmain\t1\t@1\t1\t1\t2.1.307\t/w/a\t\t2\t\t120\t200\t0\t0\t\t0\t\t\t\tapi\n\
%2\tmain\t1\t@1\t0\t1\tzsh\t/w/a\t\t2\t\t79\t200\t0\t1\t\t0\t\t\t\tapi\n\
%3\tmain\t2\t@2\t1\t0\tclaude\t/w/b\t\t1\t\t200\t200\t0\t0\t\t0\t\t\t\tweb\n\
%4\twork\t1\t@3\t1\t1\tclaude\t/w/c\t\t2\t1\t150\t200\t1\t0\t\t0\t\t\t\tmy\ttabbed name\n\
%5\twork\t1\t@3\t0\t1\tsimul\t/w/c\t1\t2\t1\t49\t200\t1\t1\t\"exec simul sidebar\"\t0\t\t\t\tmy\ttabbed name\n";

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
                restored_sidebar: false,
                window_name: "api".into(),
                shell_command: None,
            }
        );
        assert!(snap.panes[1].restored_sidebar, "titled, untagged");
        assert!(
            !snap.panes[4].restored_sidebar,
            "tagged sidebars are not restored ones"
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
    fn reads_what_the_shell_reported_with_osc_133() {
        let line = |cmd: &str| {
            format!("%1\tmain\t1\t@1\t1\t1\tzsh\t/w\t\t1\t\t80\t80\t0\t0\t\t{cmd}\tw\n")
        };
        let read = |cmd: &str| parse_snapshot(&line(cmd), "", true).panes[0].shell_command;
        assert_eq!(read("0\t\t\t"), None, "no command reported yet");
        assert_eq!(
            read("1\t\t1700000000\t"),
            Some(ShellCommand {
                running: true,
                status: None,
                started: 1_700_000_000,
                ended: None,
            })
        );
        assert_eq!(
            read("0\t2\t1700000000\t1700000005"),
            Some(ShellCommand {
                running: false,
                status: Some(2),
                started: 1_700_000_000,
                ended: Some(1_700_000_005),
            })
        );
        assert_eq!(
            read("0\t\t1700000000\t1700000005").and_then(|c| c.status),
            None,
            "a shell may end a command without a status"
        );
    }

    #[test]
    fn panes_format_matches_the_sidebar_title() {
        assert!(PANES_FORMAT.contains(&format!("#{{pane_title}},{SIDEBAR_TITLE}}}")));
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

    fn shown(panes: &str, clients: &str) -> HashSet<String> {
        parse_snapshot(panes, clients, true).shown_windows
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn shown_windows_are_current_in_a_session_a_client_shows() {
        assert_eq!(
            shown(PANES, "main\tattached,UTF-8\t/dev/ttys001\n"),
            set(&["@1"]),
            "focus doesn't matter; @2 isn't current and work has no client"
        );
        assert_eq!(
            shown(PANES, "work\tattached,focused,control-mode,no-output\t\n"),
            set(&["@1", "@3"]),
            "a control client alone is no client: every session's current window"
        );
        assert_eq!(shown(PANES, ""), set(&["@1", "@3"]));
    }

    #[test]
    fn linked_window_is_shown_through_any_session() {
        let panes = "%1\ta\t1\t@1\t1\t0\tclaude\t/w\t\t1\t\t80\t80\t0\t0\t\t0\t\t\t\tx\n%1\tb\t3\t@1\t1\t1\tclaude\t/w\t\t1\t\t80\t80\t0\t0\t\t0\t\t\t\tx\n%2\ta\t2\t@2\t1\t1\tsh\t/w\t\t1\t\t80\t80\t0\t0\t\t0\t\t\t\ty\n";
        assert_eq!(shown(panes, "b\tattached\t/dev/ttys1\n"), set(&["@1"]));
    }

    #[test]
    fn linked_windows_are_listed_once() {
        let panes = "%1\ta\t1\t@1\t1\t0\tclaude\t/w\t\t1\t\t80\t80\t0\t0\t\t0\t\t\t\tx\n%1\tb\t3\t@1\t1\t1\tclaude\t/w\t\t1\t\t80\t80\t0\t0\t\t0\t\t\t\tx\n";
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
                    sidebar: None,
                    shown: true,
                },
                WindowInfo {
                    id: "@2".into(),
                    session: "main".into(),
                    index: 2,
                    name: "web".into(),
                    panes: vec![pane("%3", "claude", true, "/w/b")],
                    sidebar: None,
                    shown: false,
                },
                WindowInfo {
                    id: "@3".into(),
                    session: "work".into(),
                    index: 1,
                    name: "my\ttabbed name".into(),
                    panes: vec![pane("%4", "claude", true, "/w/c")],
                    sidebar: Some("%5".into()),
                    shown: true,
                },
            ]
        );
    }

    /// One `list-panes ; show-options ; list-clients` reply, as `Tmux::snapshot` gets it.
    fn chained(focus: &str, clients: &str) -> String {
        format!("{PANES}focus-events {focus}\n{clients}")
    }

    #[test]
    fn splits_a_chained_snapshot() {
        let raw = "client\tmain\tattached,focused,UTF-8\t/dev/ttys001\n\
                   client\twork\tattached,focused,control-mode,no-output\t\n";
        let (panes, clients, focus) = split_snapshot(&chained("on", raw));
        assert_eq!(panes, PANES);
        assert_eq!(
            clients,
            "main\tattached,focused,UTF-8\t/dev/ttys001\n\
             work\tattached,focused,control-mode,no-output\t\n"
        );
        assert!(focus);
        let (_, _, focus) = split_snapshot(&chained("off", raw));
        assert!(!focus);
    }

    #[test]
    fn a_chained_snapshot_may_have_no_clients() {
        let (panes, clients, focus) = split_snapshot(&chained("on", ""));
        assert_eq!(panes, PANES);
        assert_eq!(clients, "");
        assert!(focus);
        assert_eq!(parse_snapshot(&panes, &clients, focus).panes.len(), 5);
    }

    #[test]
    fn a_window_named_like_a_prefix_stays_a_pane() {
        let panes = "%1\tclient\t1\t@1\t1\t1\tsh\t/w\t\t1\t\t80\t80\t0\t0\t\t0\t\t\t\tclient\n\
                     %2\tfocus-events on\t1\t@2\t1\t1\tsh\t/w\t\t1\t\t80\t80\t0\t0\t\t0\t\t\t\tfocus-events on\n";
        let (split, clients, focus) = split_snapshot(&format!(
            "{panes}focus-events off\nclient\tclient\tattached\t/dev/t1\n"
        ));
        assert_eq!(split, panes);
        assert_eq!(clients, "client\tattached\t/dev/t1\n");
        assert!(!focus);
    }

    #[test]
    fn a_pane_field_cannot_pass_for_a_client() {
        // A path holding a newline splits its pane line in two.
        let panes = "%1\tmain\t1\t@1\t1\t1\tsh\t/w/x\nclient\tmain\tattached,focused\t/dev/t9\t\t1\t\t80\t80\t0\t0\t\tw\n";
        let (_, clients, _) = split_snapshot(&chained("on", ""));
        assert_eq!(clients, "");
        let (_, clients, _) = split_snapshot(&format!("{panes}focus-events on\n"));
        assert_eq!(clients, "", "the pane's second half is no client");
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
