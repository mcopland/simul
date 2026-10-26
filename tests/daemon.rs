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
fn kiro_hook_events_drive_ls() {
    let mut h = Harness::new();
    h.start_daemon();
    let pane = h.first_pane();
    let kiro = |event: &str| h.agent_hook_fixture("kiro", &pane, event, event);

    kiro("SessionStart");
    h.wait_state(&pane, Some("idle"));
    kiro("UserPromptSubmit");
    h.wait_state(&pane, Some("working"));
    kiro("PreToolUse");
    kiro("PostToolUse");
    kiro("Stop");
    h.wait_state(&pane, Some("done"));

    let rec = h
        .ls()
        .into_iter()
        .find(|r| r["pane"] == pane.as_str())
        .unwrap();
    assert_eq!(rec["agent"], "kiro");
    assert_eq!(rec["session_id"], "sess_01jf3k9q7w2m5n8p4r6t0v2x4z");
    assert_eq!(rec["cwd"], "/home/user/src/scratch");

    kiro("SessionEnd");
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

#[test]
fn recovered_panes_get_their_branch() {
    let mut h = Harness::new();
    let repo = h.dir.path().join("repo");
    common::git_repo(&repo, "feat/scan");
    let pane = h.fake_claude_pane_in("done", &repo);
    h.start_daemon();
    let mut branch = None;
    let ok = common::poll(std::time::Duration::from_secs(5), || {
        branch = h
            .ls()
            .into_iter()
            .find(|r| r["pane"] == pane.as_str())
            .and_then(|r| r["branch"].as_str().map(str::to_owned));
        branch.is_some()
    });
    assert!(ok, "no branch for recovered pane");
    assert_eq!(branch.as_deref(), Some("feat/scan"));
}

#[test]
fn model_and_effort_come_from_the_transcript_after_each_turn() {
    let mut h = Harness::new();
    h.start_daemon();
    let pane = h.first_pane();
    let transcript = h.dir.path().join("t.jsonl");
    let reply = |model: &str, effort: &str| {
        format!(
            "{{\"type\":\"assistant\",\"isSidechain\":false,\"effort\":\"{effort}\",\"message\":{{\"model\":\"{model}\"}}}}\n"
        )
    };
    std::fs::write(&transcript, reply("claude-opus-5-5", "high")).unwrap();
    let payload = |event: &str| {
        serde_json::json!({
            "hook_event_name": event, "session_id": "s", "cwd": "/w",
            "transcript_path": transcript, "effort": {"level": "high"},
        })
        .to_string()
    };
    h.hook(&pane, "SessionStart", &payload("SessionStart"));
    h.hook(&pane, "Stop", &payload("Stop"));
    let model = || {
        h.ls()
            .into_iter()
            .find(|r| r["pane"] == pane.as_str())
            .map(|r| {
                (
                    r["model"].as_str().map(str::to_owned),
                    r["effort"].as_str().map(str::to_owned),
                )
            })
    };
    eventually("opus from transcript", || {
        model() == Some((Some("claude-opus-5-5".into()), Some("high".into())))
    });
    // A /model switch shows up in the transcript; the next Stop picks it up.
    let mut text = std::fs::read_to_string(&transcript).unwrap();
    text.push_str(&reply("claude-sonnet-5-5", "low"));
    std::fs::write(&transcript, text).unwrap();
    h.hook(&pane, "Stop", &payload("Stop"));
    eventually("sonnet after switch", || {
        model() == Some((Some("claude-sonnet-5-5".into()), Some("low".into())))
    });
    let out = h.simul().arg("ls").output().unwrap();
    let table = String::from_utf8(out.stdout).unwrap();
    assert!(
        table.contains("Sonnet 5.5") && table.contains(" low "),
        "{table}"
    );
}

/// Writes the live status file Claude Code keeps for a session, owned by this test process.
fn write_session(h: &Harness, pane: &str, status: &str) {
    let dir = h.dir.path().join("claude-config/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let window = h.tmux(&["display-message", "-p", "-t", pane, "#{window_id}"]);
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let body = format!(
        r#"{{"pid":{pid},"tmux":"t:{window}.{pane}","status":"{status}","statusUpdatedAt":{now}}}"#
    );
    // Rename so the daemon never reads a half-written file.
    let tmp = dir.join(format!("{pid}.json.tmp"));
    std::fs::write(&tmp, body).unwrap();
    std::fs::rename(&tmp, dir.join(format!("{pid}.json"))).unwrap();
}

#[test]
fn live_session_file_drives_state_without_hooks() {
    let mut h = Harness::new();
    let pane = h.fake_claude_pane("done");
    write_session(&h, &pane, "busy");
    h.start_daemon();
    h.wait_state(&pane, Some("working"));
    write_session(&h, &pane, "waiting");
    h.wait_state(&pane, Some("blocked"));
    // Esc fires no hook; only the session file says the turn is over.
    h.hook_fixture(&pane, "UserPromptSubmit", "UserPromptSubmit");
    h.wait_state(&pane, Some("working"));
    write_session(&h, &pane, "idle");
    h.wait_state(&pane, Some("idle"));
}
