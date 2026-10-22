mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{Harness, eventually};

#[test]
fn hook_events_drive_ls() {
    let mut h = Harness::new();
    h.start_daemon();
    let pane = h.first_pane();

    h.hook_fixture(&pane, "SessionStart", "SessionStart");
    h.wait_state(&pane, Some("idle"));
    h.hook_fixture(&pane, "UserPromptSubmit", "UserPromptSubmit");
    h.wait_state(&pane, Some("working"));
    h.hook_fixture(&pane, "Notification", "Notification-permission_prompt");
    h.wait_state(&pane, Some("blocked"));
    h.hook_fixture(&pane, "PostToolUse", "PostToolUse");
    h.wait_state(&pane, Some("working"));
    h.hook_fixture(&pane, "Stop", "Stop");
    h.wait_state(&pane, Some("done"));

    let rec = h
        .ls()
        .into_iter()
        .find(|r| r["pane"] == pane.as_str())
        .unwrap();
    assert_eq!(rec["session"], "t");
    assert_eq!(rec["window"], 0);
    assert_eq!(rec["agent"], "claude");
    assert_eq!(rec["cwd"], "/home/user/src/scratch");

    h.hook_fixture(&pane, "SessionEnd", "SessionEnd");
    h.wait_state(&pane, None);
}

#[test]
fn hook_is_silent_and_exits_zero_without_daemon() {
    let h = Harness::new();
    let run = |args: &[&str]| {
        let out = h
            .simul()
            .args(args)
            .env("TMUX", "/nonexistent/tmux.sock,0,0")
            .env("SIMUL_SOCKET", h.dir.path().join("none.sock"))
            .env("TMUX_PANE", "%0")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {:?}", out.status);
        assert!(out.stdout.is_empty(), "{args:?} wrote stdout");
    };
    run(&["hook", "claude", "Stop"]);
    run(&["hook"]);
    run(&["hook", "claude", "Stop", "--bogus"]);
    run(&["hook", "kiro", "Stop"]);
}

#[test]
fn hook_gives_up_within_budget_when_stdin_stays_open() {
    let mut h = Harness::new();
    h.start_daemon();
    let start = Instant::now();
    let mut child = h
        .simul()
        .args(["hook", "claude", "Stop"])
        .env("TMUX_PANE", h.first_pane())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
    assert!(
        start.elapsed() < Duration::from_millis(1000),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn hook_starts_daemon_on_demand() {
    let h = Harness::new();
    let pane = h.first_pane();
    h.hook_fixture(&pane, "SessionStart", "SessionStart");
    eventually("daemon autostart", || {
        std::os::unix::net::UnixStream::connect(&h.simul_sock).is_ok()
    });
    // The first event may be lost while the daemon starts; the next one registers the pane.
    h.hook_fixture(&pane, "UserPromptSubmit", "UserPromptSubmit");
    h.wait_state(&pane, Some("working"));
    h.tmux(&["kill-server"]);
    eventually("daemon exits with tmux", || {
        std::os::unix::net::UnixStream::connect(&h.simul_sock).is_err()
    });
}

#[test]
fn killed_pane_is_dropped() {
    let mut h = Harness::new();
    h.start_daemon();
    let pane = h.new_pane("sleep 600");
    h.hook_fixture(&pane, "UserPromptSubmit", "UserPromptSubmit");
    h.wait_state(&pane, Some("working"));
    h.tmux(&["kill-pane", "-t", &pane]);
    h.wait_state(&pane, None);
}

#[test]
fn recovery_scan_classifies_running_agents() {
    let mut h = Harness::new();
    let blocked = h.fake_claude_pane("approval");
    let idle = h.fake_claude_pane("done");
    h.start_daemon();
    h.wait_state(&blocked, Some("blocked"));
    h.wait_state(&idle, Some("idle"));
    assert_eq!(h.ls().len(), 2, "plain shells must not be recovered");
}

#[test]
fn screen_scrape_clears_stuck_working() {
    let mut h = Harness::new();
    h.start_daemon();
    let pane = h.fake_claude_pane("done");
    h.hook_fixture(&pane, "UserPromptSubmit", "UserPromptSubmit");
    // Denied or interrupted turns send no further hook; the screen shows the prompt again.
    h.wait_state(&pane, Some("idle"));
}

#[test]
fn screen_scrape_detects_approval_before_notification() {
    let mut h = Harness::new();
    h.start_daemon();
    let pane = h.fake_claude_pane("approval");
    h.hook_fixture(&pane, "PreToolUse", "PreToolUse");
    h.wait_state(&pane, Some("blocked"));
}

#[test]
fn ls_table_lists_agents() {
    let mut h = Harness::new();
    h.start_daemon();
    let pane = h.first_pane();
    h.hook_fixture(&pane, "Stop", "Stop");
    h.wait_state(&pane, Some("done"));
    let out = h.simul().arg("ls").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("PANE"), "{text}");
    assert!(
        text.contains(&pane) && text.contains("done") && text.contains("t:0"),
        "{text}"
    );
}

#[test]
fn ls_without_daemon_explains() {
    let h = Harness::new();
    let out = h.simul().arg("ls").output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("simul daemon") && err.contains("s.sock"),
        "{err}"
    );
}
