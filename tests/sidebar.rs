mod common;

use std::collections::HashSet;

use common::{Harness, eventually};

/// (pane, window, width) of every tagged sidebar.
fn sidebars(h: &Harness) -> Vec<(String, String, u32)> {
    h.tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id} #{window_id} #{pane_width} #{@simul-sidebar}",
    ])
    .lines()
    .filter_map(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        (f.get(3) == Some(&"1")).then(|| (f[0].into(), f[1].into(), f[2].parse().unwrap()))
    })
    .collect()
}

fn windows(h: &Harness) -> HashSet<String> {
    h.tmux(&["list-windows", "-a", "-F", "#{window_id}"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn toggle(h: &Harness) {
    let out = h.simul().arg("toggle").output().unwrap();
    assert!(
        out.status.success(),
        "toggle: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn toggle_adds_and_removes_sidebars_in_every_window() {
    let mut h = Harness::new();
    h.new_pane("sleep 600");
    h.start_daemon();
    toggle(&h);
    let side = sidebars(&h);
    assert_eq!(side.len(), 2, "{side:?}");
    assert_eq!(
        side.iter().map(|s| s.1.clone()).collect::<HashSet<_>>(),
        windows(&h)
    );
    assert!(side.iter().all(|s| s.2 == 30), "{side:?}");
    eventually("sidebar process running", || {
        h.tmux(&[
            "display-message",
            "-p",
            "-t",
            &side[0].0,
            "#{pane_current_command}",
        ]) == "simul"
    });
    toggle(&h);
    eventually("sidebars removed", || sidebars(&h).is_empty());
}

#[test]
fn new_window_gets_a_sidebar_while_on() {
    let mut h = Harness::new();
    h.start_daemon();
    toggle(&h);
    h.new_pane("sleep 600");
    eventually("sidebar in new window", || sidebars(&h).len() == 2);
}

#[test]
fn closing_the_last_other_pane_closes_the_window() {
    let mut h = Harness::new();
    let pane = h.new_pane("sleep 600");
    h.start_daemon();
    toggle(&h);
    let window = h.tmux(&["display-message", "-p", "-t", &pane, "#{window_id}"]);
    h.tmux(&["kill-pane", "-t", &pane]);
    eventually("window closed", || !windows(&h).contains(&window));
    assert_eq!(sidebars(&h).len(), 1, "the other window keeps its sidebar");
}

#[test]
fn resize_reasserts_width() {
    let mut h = Harness::with_config("[sidebar]\nwidth = 30\n");
    h.start_daemon();
    toggle(&h);
    let (pane, _, width) = sidebars(&h).remove(0);
    assert_eq!(width, 30);
    h.tmux(&["resize-pane", "-t", &pane, "-x", "70"]);
    let out = h.simul().args(["internal", "resized"]).output().unwrap();
    assert!(out.status.success());
    eventually("width restored", || sidebars(&h)[0].2 == 30);
}

#[test]
fn sidebar_renders_agents() {
    let mut h = Harness::new();
    let agent = h.new_pane("sleep 600");
    h.start_daemon();
    h.hook_fixture(&agent, "Notification", "Notification-permission_prompt");
    toggle(&h);
    let side = sidebars(&h)
        .into_iter()
        .find(|s| s.1 != h.tmux(&["display-message", "-p", "-t", &agent, "#{window_id}"]))
        .unwrap();
    // Renaming after the sidebar opens checks that window renames reach it.
    h.tmux(&["rename-window", "-t", &agent, "agents"]);
    let mut screen = String::new();
    eventually("agent row rendered", || {
        screen = h.tmux(&["capture-pane", "-p", "-t", &side.0]);
        screen.contains(" \u{2298} 1:agents")
    });
    assert!(screen.starts_with('t'), "session header first: {screen}");
    assert!(screen.contains("f:all"), "{screen}");
}

#[test]
fn toggle_after_daemon_restart_hides_existing_sidebars() {
    let mut h = Harness::new();
    h.start_daemon();
    toggle(&h);
    assert_eq!(sidebars(&h).len(), 1);
    h.stop_daemon();
    h.start_daemon();
    toggle(&h);
    eventually("sidebars removed", || sidebars(&h).is_empty());
}

#[test]
fn windows_in_every_session_get_sidebars() {
    let mut h = Harness::new();
    h.tmux(&["new-session", "-d", "-s", "u", "sh"]);
    h.start_daemon();
    toggle(&h);
    assert_eq!(sidebars(&h).len(), 2);
    for (session, want) in [("t:", 3), ("u:", 4)] {
        h.tmux(&["new-window", "-d", "-t", session, "sleep 600"]);
        eventually(&format!("sidebar in new window of {session}"), || {
            sidebars(&h).len() == want
        });
    }
}

#[test]
fn lone_sidebar_in_unattached_session_is_closed() {
    let mut h = Harness::new();
    let pane = h.new_pane("sleep 600");
    h.tmux(&["new-session", "-d", "-s", "u", "sh"]);
    h.start_daemon();
    toggle(&h);
    let window = h.tmux(&["display-message", "-p", "-t", &pane, "#{window_id}"]);
    h.tmux(&["kill-pane", "-t", &pane]);
    eventually("window closed", || !windows(&h).contains(&window));
}

#[test]
fn closing_sidebars_does_not_log_errors() {
    let mut h = Harness::new();
    h.new_pane("sleep 600");
    h.start_daemon();
    toggle(&h);
    let side = sidebars(&h);
    eventually("sidebars subscribed", || {
        side.iter().all(|s| {
            h.tmux(&[
                "display-message",
                "-p",
                "-t",
                &s.0,
                "#{pane_current_command}",
            ]) == "simul"
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    toggle(&h);
    eventually("sidebars removed", || sidebars(&h).is_empty());
    // A status push after the sidebars are gone hits their closed sockets.
    h.hook_fixture(&h.first_pane(), "UserPromptSubmit", "UserPromptSubmit");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let log = std::fs::read_to_string(h.simul_sock.with_extension("log")).unwrap();
    assert!(!log.contains("Broken pipe"), "{log}");
}

#[test]
fn each_sidebar_highlights_its_own_windows_agent() {
    let mut h = Harness::new();
    let agent = h.new_pane("sleep 600");
    h.tmux(&["rename-window", "-t", &agent, "agents"]);
    h.start_daemon();
    h.hook_fixture(&agent, "UserPromptSubmit", "UserPromptSubmit");
    toggle(&h);
    let agent_window = h.tmux(&["display-message", "-p", "-t", &agent, "#{window_id}"]);
    for (pane, window, _) in sidebars(&h) {
        let want = window == agent_window;
        eventually(&format!("sidebar {pane} highlight = {want}"), || {
            let screen = h.tmux(&["capture-pane", "-e", "-p", "-t", &pane]);
            style_of(&screen, "1:agents").is_some_and(|s| s.bold == want)
        });
    }
}

#[test]
fn selected_row_is_bold_without_a_block() {
    let mut h = Harness::new();
    let mine = h.new_pane("sleep 600");
    let other = h.new_pane("sleep 600");
    h.tmux(&["rename-window", "-t", &mine, "mine"]);
    h.tmux(&["rename-window", "-t", &other, "other"]);
    h.start_daemon();
    h.hook_fixture(&mine, "UserPromptSubmit", "UserPromptSubmit");
    h.hook_fixture(&other, "UserPromptSubmit", "UserPromptSubmit");
    toggle(&h);
    let mine_window = h.tmux(&["display-message", "-p", "-t", &mine, "#{window_id}"]);
    let (pane, _, _) = sidebars(&h)
        .into_iter()
        .find(|s| s.1 == mine_window)
        .unwrap();
    let mut screen = String::new();
    eventually("selected row bold", || {
        screen = h.tmux(&["capture-pane", "-e", "-p", "-t", &pane]);
        style_of(&screen, "other").is_some() && style_of(&screen, "mine").is_some_and(|s| s.bold)
    });
    assert!(!screen.contains("\x1b[7m"), "no reverse video: {screen:?}");
    let label = style_of(&screen, "mine").unwrap();
    assert_eq!(label.fg, None, "selected label in default fg: {screen:?}");
    let row = screen.lines().find(|l| l.contains("mine")).unwrap();
    let symbol = style_of(row, "\u{25d0}").unwrap();
    assert!(
        symbol.fg.is_some() && !symbol.muted(),
        "selected symbol keeps its state color: {row:?}"
    );
    let rest = style_of(&screen, "other").unwrap();
    assert!(
        !rest.bold && rest.muted(),
        "other agents are muted: {screen:?}"
    );
}

#[test]
fn only_the_current_session_header_is_bold() {
    let mut h = Harness::new();
    let agent = h.new_pane("sleep 600");
    h.tmux(&["new-session", "-d", "-s", "u", "sleep 600"]);
    h.start_daemon();
    h.hook_fixture(&agent, "UserPromptSubmit", "UserPromptSubmit");
    let other = h.tmux(&["display-message", "-p", "-t", "u:", "#{pane_id}"]);
    h.hook_fixture(&other, "UserPromptSubmit", "UserPromptSubmit");
    toggle(&h);
    let (pane, _, _) = sidebars(&h)
        .into_iter()
        .find(|s| s.1 == h.tmux(&["display-message", "-p", "-t", &agent, "#{window_id}"]))
        .unwrap();
    let mut screen = String::new();
    eventually("both sessions listed", || {
        screen = h.tmux(&["capture-pane", "-e", "-p", "-t", &pane]);
        header(&screen, "u").is_some()
    });
    let own = header(&screen, "t").unwrap();
    assert!(own.bold && !own.muted(), "own session bold: {screen:?}");
    let rest = header(&screen, "u").unwrap();
    assert!(
        !rest.bold && rest.muted(),
        "other session muted: {screen:?}"
    );
}

/// Text attributes of a run of cells in `capture-pane -e` output.
#[derive(Clone, Debug, Default)]
struct Sgr {
    bold: bool,
    /// The foreground SGR parameters, e.g. "31" or "38;5;8"; `None` is the default.
    fg: Option<String>,
}

impl Sgr {
    fn muted(&self) -> bool {
        matches!(self.fg.as_deref(), Some("90" | "38;5;8"))
    }

    fn apply(&mut self, params: &str) {
        let mut it = params.split(';');
        while let Some(p) = it.next() {
            match p {
                "" | "0" => *self = Sgr::default(),
                "1" => self.bold = true,
                "22" => self.bold = false,
                "39" => self.fg = None,
                "38" => {
                    let fg: Vec<&str> = match it.next() {
                        Some("5") => vec!["38", "5", it.next().unwrap_or_default()],
                        Some("2") => ["38", "2"].into_iter().chain(it.by_ref().take(3)).collect(),
                        _ => continue,
                    };
                    self.fg = Some(fg.join(";"));
                }
                _ if matches!(p.parse::<u8>(), Ok(30..=37 | 90..=97)) => {
                    self.fg = Some(p.to_owned());
                }
                _ => {}
            }
        }
    }
}

/// Each line's runs of visible text with the attributes they are drawn in.
fn runs(line: &str) -> Vec<(String, Sgr)> {
    let mut out = Vec::new();
    let mut sgr = Sgr::default();
    let mut rest = line;
    while let Some(start) = rest.find("\x1b[") {
        out.push((rest[..start].to_owned(), sgr.clone()));
        let after = &rest[start + 2..];
        let Some(end) = after.find('m') else { break };
        sgr.apply(&after[..end]);
        rest = &after[end + 1..];
    }
    out.push((rest.to_owned(), sgr));
    out
}

/// Attributes of the first run of text containing `needle`.
fn style_of(screen: &str, needle: &str) -> Option<Sgr> {
    screen
        .lines()
        .flat_map(runs)
        .find_map(|(text, sgr)| text.contains(needle).then_some(sgr))
}

/// Attributes of the header line for `session`, if present.
fn header(screen: &str, session: &str) -> Option<Sgr> {
    screen.lines().find_map(|line| {
        let runs = runs(line);
        let visible: String = runs.iter().map(|r| r.0.as_str()).collect();
        if visible.trim() != session {
            return None;
        }
        runs.into_iter()
            .find_map(|(text, sgr)| (!text.trim().is_empty()).then_some(sgr))
    })
}

#[test]
fn layout_hooks_see_the_sidebar_tag() {
    let mut h = Harness::new();
    // A user hook that hides pane borders when only one non-sidebar pane is left.
    h.tmux(&[
        "set-hook",
        "-g",
        "window-layout-changed",
        "if -F \"#{==:#{P:#{?#{@simul-sidebar},,x}},x}\" \"setw pane-border-status off\" \"setw pane-border-status top\"",
    ]);
    h.start_daemon();
    toggle(&h);
    assert_eq!(sidebars(&h).len(), 1);
    eventually("borders off with one real pane", || {
        h.tmux(&["show-options", "-wv", "-t", "t:0", "pane-border-status"]) == "off"
    });
}
