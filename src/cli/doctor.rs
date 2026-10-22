use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::install_hooks;
use crate::client;
use crate::hook::claude;
use crate::paths::{self, env_var};
use crate::protocol::{Ack, Request, Status};
use crate::tmux::{self, Tmux};

pub const MIN_TMUX: (u32, u32) = (3, 3);

#[derive(Debug, PartialEq, Eq)]
pub struct Check {
    pub ok: bool,
    pub name: &'static str,
    pub detail: String,
}

fn pass(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        ok: true,
        name,
        detail: detail.into(),
    }
}

fn fail(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        ok: false,
        name,
        detail: detail.into(),
    }
}

pub fn run() -> Result<()> {
    let checks = gather();
    for c in &checks {
        println!(
            "{} {:<13} {}",
            if c.ok { "ok  " } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    let failed = checks.iter().filter(|c| !c.ok).count();
    if failed > 0 {
        anyhow::bail!("{failed} check(s) failed");
    }
    Ok(())
}

fn gather() -> Vec<Check> {
    let mut checks = vec![
        match std::process::Command::new("tmux").arg("-V").output() {
            Ok(out) => check_version(&String::from_utf8_lossy(&out.stdout)),
            Err(e) => fail("tmux", format!("failed to run tmux -V: {e}")),
        },
    ];
    match paths::current_tmux_socket() {
        Some(sock) => checks.push(check_focus_events(&Tmux::new(sock))),
        None => checks.push(fail("focus-events", "not inside tmux ($TMUX unset)")),
    }
    let socket = paths::client_socket();
    let daemon_up = match &socket {
        Ok(sock) => match client::request::<Ack>(sock, &Request::Ping) {
            Ok(_) => {
                checks.push(pass("daemon", format!("listening on {}", sock.display())));
                true
            }
            Err(e) => {
                checks.push(fail("daemon", format!("{e:#}")));
                false
            }
        },
        Err(e) => {
            checks.push(fail("daemon", format!("{e:#}")));
            false
        }
    };
    checks.push(check_claude_hooks_file(&claude_settings_paths()));
    if let (true, Ok(sock)) = (daemon_up, &socket) {
        checks.push(match client::request::<Status>(sock, &Request::List) {
            Ok(status) => pass("agents", format!("{} tracked", status.panes.len())),
            Err(e) => fail("agents", format!("{e:#}")),
        });
    }
    checks
}

pub fn check_version(output: &str) -> Check {
    match tmux::parse_version(output) {
        Some(v) if v >= MIN_TMUX => pass("tmux", output.trim()),
        Some(_) => fail(
            "tmux",
            format!(
                "{} is older than {}.{}",
                output.trim(),
                MIN_TMUX.0,
                MIN_TMUX.1
            ),
        ),
        None => pass("tmux", format!("{} (unreleased build)", output.trim())),
    }
}

fn check_focus_events(tmux: &Tmux) -> Check {
    match tmux.run(&["show-options", "-gv", "focus-events"]) {
        Ok(v) if v.trim() == "on" => pass("focus-events", "on"),
        Ok(v) => fail(
            "focus-events",
            format!(
                "{}: add 'set -g focus-events on' to tmux.conf, or notifications fire while you watch",
                v.trim()
            ),
        ),
        Err(e) => fail("focus-events", format!("{e:#}")),
    }
}

fn claude_settings_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = env_var("HOME")
        .map(|h| PathBuf::from(h).join(".claude/settings.json"))
        .into_iter()
        .collect();
    paths.push(PathBuf::from(".claude/settings.local.json"));
    paths
}

fn check_claude_hooks_file(paths: &[PathBuf]) -> Check {
    let mut problems = Vec::new();
    for path in paths.iter().filter(|p| p.exists()) {
        let parsed = std::fs::read_to_string(path)
            .map_err(anyhow::Error::from)
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(anyhow::Error::from));
        match parsed {
            Ok(v) => match check_claude_hooks(&v) {
                Ok(()) => return pass("claude hooks", format!("installed in {}", path.display())),
                Err(why) => problems.push(format!("{}: {why}", path.display())),
            },
            Err(e) => problems.push(format!("{}: {e}", path.display())),
        }
    }
    if problems.is_empty() {
        problems.push("not installed; run `simul install-hooks claude`".into());
    }
    fail("claude hooks", problems.join("; "))
}

/// Every Claude event has a simul hook whose binary exists.
pub fn check_claude_hooks(settings: &Value) -> std::result::Result<(), String> {
    let installed = install_hooks::managed(settings);
    if installed.is_empty() {
        return Err("no simul hooks; run `simul install-hooks claude`".into());
    }
    let missing: Vec<&str> = claude::EVENTS
        .iter()
        .copied()
        .filter(|e| !installed.iter().any(|(ev, _)| ev == e))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing {}; rerun `simul install-hooks claude`",
            missing.join(", ")
        ));
    }
    for (_, cmd) in &installed {
        match first_word(cmd) {
            Some(exe) if Path::new(&exe).exists() => {}
            Some(exe) => return Err(format!("hook binary {exe} does not exist")),
            None => return Err(format!("cannot parse hook command: {cmd}")),
        }
    }
    Ok(())
}

/// The first word of a command produced by `tmux::shell_quote`.
pub fn first_word(cmd: &str) -> Option<String> {
    let rest = cmd.strip_prefix('\'')?;
    let mut out = String::new();
    let mut parts = rest.split(r"'\''");
    let mut part = parts.next()?;
    loop {
        if let Some(end) = part.find('\'') {
            out.push_str(&part[..end]);
            return Some(out);
        }
        out.push_str(part);
        out.push('\'');
        part = parts.next()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn version_gate() {
        assert!(check_version("tmux 3.8\n").ok);
        assert!(check_version("tmux 3.3a").ok);
        assert!(!check_version("tmux 3.2a").ok);
        assert!(check_version("tmux master").ok);
    }

    #[test]
    fn first_word_unquotes() {
        assert_eq!(
            first_word("'/a b/simul' hook").as_deref(),
            Some("/a b/simul")
        );
        assert_eq!(
            first_word(r"'/it'\''s/simul' hook").as_deref(),
            Some("/it's/simul")
        );
        assert_eq!(first_word("simul hook"), None);
    }

    #[test]
    fn hooks_check() {
        let exe = std::env::current_exe().unwrap();
        let good = install_hooks::merge(json!({}), &exe).unwrap();
        assert_eq!(check_claude_hooks(&good), Ok(()));
        let gone = install_hooks::merge(json!({}), Path::new("/nonexistent/simul")).unwrap();
        assert!(
            check_claude_hooks(&gone)
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(
            check_claude_hooks(&json!({}))
                .unwrap_err()
                .contains("install-hooks")
        );
        let mut partial = good.clone();
        partial["hooks"].as_object_mut().unwrap().remove("Stop");
        assert!(check_claude_hooks(&partial).unwrap_err().contains("Stop"));
    }
}
