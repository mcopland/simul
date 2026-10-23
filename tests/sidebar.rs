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
    assert!(side.iter().all(|s| s.2 == 40), "{side:?}");
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
        screen.contains(" ! 1:agents")
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
    h.start_daemon();
    h.hook_fixture(&agent, "UserPromptSubmit", "UserPromptSubmit");
    toggle(&h);
    let agent_window = h.tmux(&["display-message", "-p", "-t", &agent, "#{window_id}"]);
    let reversed = |pane: &str| {
        h.tmux(&["capture-pane", "-e", "-p", "-t", pane])
            .contains("\x1b[7m")
    };
    for (pane, window, _) in sidebars(&h) {
        let want = window == agent_window;
        eventually(&format!("sidebar {pane} highlight = {want}"), || {
            reversed(&pane) == want
        });
    }
}
