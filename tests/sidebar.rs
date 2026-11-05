mod common;

use std::collections::HashSet;
use std::time::Duration;

use common::{Harness, eventually, git_repo, poll};
use simul::sidebar::model;

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

/// The pane's directory as its sidebar row shows it outside git.
fn place_of(h: &Harness, target: &str) -> String {
    let path = h.tmux(&[
        "display-message",
        "-p",
        "-t",
        target,
        "#{pane_current_path}",
    ]);
    model::short_path(&path, &std::env::var("HOME").unwrap_or_default())
}

/// The line under the first line that reads `above`, trimmed.
fn line_under(screen: &str, above: &str) -> Option<String> {
    let mut lines = screen.lines();
    lines.find(|l| l.trim_end() == above)?;
    lines.next().map(|l| l.trim().to_owned())
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
        screen.contains("\n 1:agents\n   \u{2298} claude")
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
fn pane_rows_show_git_status_or_a_short_path() {
    let mut h = Harness::new();
    let repo = h.dir.path().join("repo");
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    // Without `-c`, tmux starts the pane in this test process's directory, not the session's.
    let dir = h.dir.path().to_str().unwrap().to_owned();
    let plain = h.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "t:",
        "-c",
        &dir,
        "sh",
    ]);
    h.tmux(&["rename-window", "-t", &plain, "plain"]);
    h.start_daemon();
    toggle(&h);
    let (pane, _, _) = sidebars(&h).remove(0);
    let row = || line_under(&h.tmux(&["capture-pane", "-p", "-t", &pane]), " 1:plain");
    let want = place_of(&h, &plain);
    eventually("directory listed", || row().as_ref() == Some(&want));
    h.tmux(&["send-keys", "-t", &plain, "cd repo/sub", "Enter"]);
    eventually("cd shown without a tmux event", || {
        row().is_some_and(|r| r.ends_with("/r/sub"))
    });
    git_repo(&repo, "trunk");
    // The status is re-read every 5 s.
    let slow = Duration::from_secs(10);
    assert!(
        poll(slow, || row().as_deref() == Some("trunk")),
        "{:?}",
        row()
    );
    std::fs::write(repo.join("new.txt"), "x").unwrap();
    assert!(
        poll(slow, || row().as_deref() == Some("trunk ?")),
        "{:?}",
        row()
    );
}

#[test]
fn lists_windows_without_agents() {
    let mut h = Harness::new();
    let cwd = h.dir.path().to_str().unwrap().to_owned();
    h.tmux(&["new-session", "-d", "-c", &cwd, "-s", "u", "sh"]);
    h.tmux(&["rename-window", "-t", "u:0", "plain"]);
    h.start_daemon();
    toggle(&h);
    let (pane, _, _) = sidebars(&h).remove(0);
    let mut screen = String::new();
    eventually("agent-less session and window listed", || {
        screen = h.tmux(&["capture-pane", "-p", "-t", &pane]);
        screen.contains(&format!("\nu\n 0:plain\n     {}", place_of(&h, "u:0")))
    });
    assert!(
        screen.starts_with("t\n 0:"),
        "own session's window too: {screen}"
    );
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
    let row = screen
        .lines()
        .skip_while(|l| !l.contains("mine"))
        .nth(1)
        .unwrap();
    assert!(
        style_of(row, "claude").is_some_and(|s| s.bold),
        "the agent under its window header is selected: {row:?}"
    );
    let symbol = style_of(row, "\u{25d0}").unwrap();
    assert!(
        symbol.fg.is_some() && !symbol.muted(),
        "selected symbol keeps its state color: {row:?}"
    );
    let rest = style_of(&screen, "other").unwrap();
    assert!(
        !rest.bold && rest.muted(),
        "other windows are muted: {screen:?}"
    );
    let plain = header(&screen, &place_of(&h, "t:0")).unwrap();
    assert!(
        !plain.bold && plain.muted(),
        "plain panes are muted: {screen:?}"
    );
}

/// The `@simul-collapsed` option of session `session`, empty when unset.
fn collapsed_option(h: &Harness, session: &str) -> String {
    h.tmux(&[
        "show-options",
        "-qv",
        "-t",
        &format!("={session}:"),
        "@simul-collapsed",
    ])
}

/// The sidebar in the first window of `session`.
fn sidebar_in(h: &Harness, session: &str) -> String {
    let window = h.tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={session}:"),
        "#{window_id}",
    ]);
    sidebars(h)
        .into_iter()
        .find(|s| s.1 == window)
        .unwrap_or_else(|| panic!("no sidebar in {session}"))
        .0
}

#[test]
fn space_folds_a_session_in_every_sidebar_and_it_persists() {
    let mut h = Harness::new();
    h.tmux(&["new-session", "-d", "-s", "u", "sleep 600"]);
    h.start_daemon();
    toggle(&h);
    eventually("a sidebar in each session", || sidebars(&h).len() == 2);
    let folded = "u \u{25b8}";
    let shows = |h: &Harness, session: &str, line: &str| {
        let screen = h.tmux(&["capture-pane", "-p", "-t", &sidebar_in(h, session)]);
        screen.lines().any(|l| l.trim_end() == line)
    };
    eventually("u listed", || shows(&h, "t", "u"));
    // Unfocused, u's sidebar has its own window's pane selected, so Space folds u.
    h.tmux(&["send-keys", "-t", &sidebar_in(&h, "u"), "Space"]);
    if !poll(Duration::from_secs(5), || shows(&h, "t", folded)) {
        let screen = |s| h.tmux(&["capture-pane", "-p", "-t", &sidebar_in(&h, s)]);
        panic!(
            "u never folded in t's sidebar\n-- t --\n{}\n-- u --\n{}",
            screen("t"),
            screen("u")
        );
    }
    assert!(shows(&h, "u", folded), "and in u's own");
    assert_eq!(collapsed_option(&h, "u"), "1");
    assert_eq!(collapsed_option(&h, "t"), "");

    h.stop_daemon();
    h.start_daemon();
    toggle(&h);
    eventually("sidebars removed", || sidebars(&h).is_empty());
    toggle(&h);
    eventually("sidebars back", || sidebars(&h).len() == 2);
    eventually("still folded after a daemon restart", || {
        shows(&h, "t", folded)
    });

    // Folded away, u's sidebar selects u's header, so Space unfolds it.
    h.tmux(&["send-keys", "-t", &sidebar_in(&h, "u"), "Space"]);
    eventually("u unfolded", || shows(&h, "t", "u"));
    assert_eq!(collapsed_option(&h, "u"), "");
}

#[test]
fn session_headers_are_dim_gold_and_the_selected_one_bright() {
    const GOLD: &str = "38;2;246;193;119";
    const DIM_GOLD: &str = "38;2;147;115;71";
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
    for session in ["t", "u"] {
        let sgr = header(&screen, session).unwrap();
        assert!(
            !sgr.bold && sgr.fg.as_deref() == Some(DIM_GOLD),
            "unselected session {session} dim gold, not bold: {screen:?}"
        );
    }

    // Folded away, u's sidebar selects u's header.
    let side = sidebar_in(&h, "u");
    h.tmux(&["send-keys", "-t", &side, "Space"]);
    let folded = "u \u{25b8}";
    eventually("u folded and selected", || {
        screen = h.tmux(&["capture-pane", "-e", "-p", "-t", &side]);
        style_of(&screen, folded).is_some()
    });
    let selected = style_of(&screen, folded).unwrap();
    assert!(
        selected.bold && selected.fg.as_deref() == Some(GOLD),
        "selected session bright gold and bold, like status-left: {screen:?}"
    );
    let rest = header(&screen, "t").unwrap();
    assert!(
        !rest.bold && rest.fg.as_deref() == Some(DIM_GOLD),
        "unselected session dim gold: {screen:?}"
    );
    assert!(!screen.contains("\x1b[7m"), "no reverse video: {screen:?}");
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

/// Attributes of the line whose text is just `session` (or another word), if present.
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

fn status_format(h: &Harness) -> String {
    h.tmux(&["show-options", "-gv", "status-format[0]"])
}

#[test]
fn toggle_cuts_first_status_line_to_status_right() {
    let mut h = Harness::new();
    h.tmux(&["set-option", "-g", "status-format[0]", "#S custom"]);
    h.tmux(&["set-option", "-g", "status-left", "LEFT"]);
    h.tmux(&["set-option", "-g", "status-right", "RIGHT"]);
    h.tmux(&["rename-window", "-t", "t", "WINDOW"]);
    let second = h.tmux(&["show-options", "-gv", "status-format[1]"]);
    h.start_daemon();
    toggle(&h);
    let shown = h.tmux(&["display-message", "-p", "-t", "t", "#{E:status-format[0]}"]);
    assert!(shown.contains("RIGHT"), "{shown}");
    assert!(
        !shown.contains("LEFT") && !shown.contains("WINDOW"),
        "{shown}"
    );
    assert_eq!(h.tmux(&["show-options", "-gv", "status-format[1]"]), second);
    toggle(&h);
    assert_eq!(status_format(&h), "#S custom");
    assert_eq!(h.tmux(&["show-options", "-gq", "@simul-status-format"]), "");
}

#[test]
fn status_line_restored_after_daemon_restart() {
    let mut h = Harness::new();
    h.tmux(&["set-option", "-g", "status-format[0]", "#S custom"]);
    h.start_daemon();
    toggle(&h);
    assert!(!status_format(&h).contains("#S"), "{}", status_format(&h));
    h.stop_daemon();
    h.start_daemon();
    toggle(&h);
    assert_eq!(status_format(&h), "#S custom");
}

#[test]
fn hide_status_false_leaves_status_line() {
    let mut h = Harness::with_config("[sidebar]\nhide_status = false\n");
    h.tmux(&["set-option", "-g", "status-format[0]", "#S custom"]);
    h.start_daemon();
    toggle(&h);
    assert_eq!(sidebars(&h).len(), 1);
    assert_eq!(status_format(&h), "#S custom");
}
