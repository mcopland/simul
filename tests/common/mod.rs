#![allow(dead_code)] // Each integration test binary uses a different subset of these helpers.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_simul");

pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(rel)
}

/// An isolated tmux server plus a simul daemon, torn down on drop.
pub struct Harness {
    pub dir: tempfile::TempDir,
    pub tmux_sock: PathBuf,
    pub simul_sock: PathBuf,
    daemon: Option<Child>,
}

/// Unix socket paths are limited to ~104 bytes, and macOS's default temp dir is long.
fn short_tmp() -> PathBuf {
    match std::env::var("TMPDIR") {
        Ok(t) if t.len() < 40 => PathBuf::from(t),
        _ => PathBuf::from("/tmp"),
    }
}

impl Harness {
    pub fn new() -> Harness {
        Harness::with_config("blocked_threshold_ms = 100\n")
    }

    pub fn with_config(config: &str) -> Harness {
        let dir = tempfile::Builder::new()
            .prefix("st")
            .tempdir_in(short_tmp())
            .unwrap();
        // The daemon refuses socket directories other users can read.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir_all(dir.path().join("config/simul")).unwrap();
        // Test daemons see no attached client, so every blocked or done pane would pop a
        // desktop notification on the developer's machine.
        let config = format!("{config}\n[notify]\nenabled = false\n");
        std::fs::write(dir.path().join("config/simul/config.toml"), config).unwrap();
        let h = Harness {
            tmux_sock: dir.path().join("t.sock"),
            simul_sock: dir.path().join("s.sock"),
            dir,
            daemon: None,
        };
        h.tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "t",
            "-x",
            "200",
            "-y",
            "50",
            "sh",
        ]);
        h
    }

    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .arg("-S")
            .arg(&self.tmux_sock)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    pub fn tmux_env(&self) -> String {
        format!("{},0,0", self.tmux_sock.display())
    }

    pub fn simul(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("SIMUL_SOCKET", &self.simul_sock)
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("TMUX", self.tmux_env())
            .env_remove("TMUX_PANE");
        c
    }

    pub fn start_daemon(&mut self) {
        let child = self
            .simul()
            .args(["daemon", "--tmux-socket"])
            .arg(&self.tmux_sock)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        eventually("daemon socket", || {
            UnixStream::connect(&self.simul_sock).is_ok()
        });
    }

    pub fn stop_daemon(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            // Already-exited daemons are fine; the test only needs it gone.
            let _ = d.kill();
            let _ = d.wait();
        }
        let _ = std::fs::remove_file(&self.simul_sock);
    }

    pub fn first_pane(&self) -> String {
        self.tmux(&["display-message", "-p", "-t", "t", "#{pane_id}"])
    }

    pub fn new_pane(&self, command: &str) -> String {
        self.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "t:",
            command,
        ])
    }

    pub fn hook(&self, pane: &str, event: &str, payload: &str) -> Output {
        let mut child = self
            .simul()
            .args(["hook", "claude", event])
            .env("TMUX_PANE", pane)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook exit: {:?}", out.status);
        assert!(out.stdout.is_empty(), "hook wrote stdout: {:?}", out.stdout);
        out
    }

    pub fn hook_fixture(&self, pane: &str, event: &str, file: &str) {
        let payload =
            std::fs::read_to_string(fixture(&format!("claude/hooks/{file}.json"))).unwrap();
        self.hook(pane, event, &payload);
    }

    pub fn ls(&self) -> Vec<Value> {
        let out = self.simul().args(["ls", "--json"]).output().unwrap();
        assert!(
            out.status.success(),
            "ls: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    pub fn state_of(&self, pane: &str) -> Option<String> {
        self.ls()
            .into_iter()
            .find(|r| r["pane"] == pane)
            .map(|r| r["state"].as_str().unwrap().to_owned())
    }

    pub fn wait_state(&self, pane: &str, want: Option<&str>) {
        let mut last = None;
        let ok = poll(Duration::from_secs(5), || {
            last = self.state_of(pane);
            last.as_deref() == want
        });
        assert!(ok, "pane {pane}: wanted {want:?}, last saw {last:?}");
    }

    /// A pane whose foreground process is named `claude`, showing a recorded screen.
    pub fn fake_claude_pane(&self, screen: &str) -> String {
        self.fake_claude_pane_in(screen, self.dir.path())
    }

    /// Like `fake_claude_pane`, with the pane's working directory set to `cwd`.
    pub fn fake_claude_pane_in(&self, screen: &str, cwd: &Path) -> String {
        let fake = self.dir.path().join("claude");
        if !fake.exists() {
            std::fs::copy(fake_agent_bin(), &fake).unwrap();
        }
        let cmd = format!(
            "exec '{}' '{}'",
            fake.display(),
            fixture(&format!("claude/screens/{screen}.txt")).display()
        );
        let pane = self.tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "t:",
            "-c",
            &cwd.to_string_lossy(),
            &cmd,
        ]);
        eventually("fake claude running", || {
            self.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{pane_current_command}",
            ]) == "claude"
        });
        pane
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop_daemon();
        // The server may already be gone if a test killed it.
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.tmux_sock)
            .arg("kill-server")
            .stderr(Stdio::null())
            .status();
    }
}

/// Copies of system binaries are killed on macOS, so the fake agent is built from
/// `examples/fake_agent.rs`.
fn fake_agent_bin() -> PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let status = Command::new(env!("CARGO"))
                .args(["build", "--quiet", "--example", "fake_agent"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .unwrap();
            assert!(status.success(), "failed to build examples/fake_agent");
            Path::new(BIN).parent().unwrap().join("examples/fake_agent")
        })
        .clone()
}

/// A git repo with one commit on `branch`.
pub fn git_repo(dir: &Path, branch: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", branch]);
    git(&["commit", "-q", "--allow-empty", "-m", "chore: init"]);
}

pub fn poll(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    f()
}

pub fn eventually(what: &str, f: impl FnMut() -> bool) {
    assert!(
        poll(Duration::from_secs(5), f),
        "timed out waiting for {what}"
    );
}
