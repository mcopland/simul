use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::install_hooks;
use crate::client;
use crate::daemon::notify::{self, Message};
use crate::hook::{claude, kiro};
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
    checks.push(check_kiro_hooks_file(
        install_hooks::kiro_hook_file(env_var).as_deref(),
        Path::new(install_hooks::KIRO_PROJECT_FILE),
        on_path("kiro-cli", env_var("PATH").as_deref()),
    ));
    checks.push(test_notification());
    if let (true, Ok(sock)) = (daemon_up, &socket) {
        checks.push(match client::request::<Status>(sock, &Request::List) {
            Ok(status) => pass("agents", format!("{} tracked", status.panes.len())),
            Err(e) => fail("agents", format!("{e:#}")),
        });
    }
    checks
}

fn test_notification() -> Check {
    match crate::config::load() {
        Ok(cfg) if !cfg.notify.desktop => {
            return pass("notification", "desktop notifications off (notify.desktop)");
        }
        Ok(_) => {}
        Err(e) => return fail("notification", format!("{e:#}")),
    }
    let msg = Message {
        title: "simul".into(),
        body: "test notification from simul doctor".into(),
    };
    let sent = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(notify::desktop(&msg)));
    match sent {
        Ok(()) => pass("notification", "sent; check that it appeared"),
        Err(e) => fail("notification", format!("{e:#}")),
    }
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
    check_hook_files("claude hooks", "claude", paths, check_claude_hooks)
}

/// Passes on the first file whose hooks check out; otherwise lists every file's problem.
fn check_hook_files<'a>(
    name: &'static str,
    agent: &str,
    paths: impl IntoIterator<Item = &'a PathBuf>,
    check: fn(&Value) -> std::result::Result<(), String>,
) -> Check {
    let mut problems = Vec::new();
    for path in paths.into_iter().filter(|p| p.exists()) {
        let parsed = std::fs::read_to_string(path)
            .map_err(anyhow::Error::from)
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(anyhow::Error::from));
        match parsed {
            Ok(v) => match check(&v) {
                Ok(()) => return pass(name, format!("installed in {}", path.display())),
                Err(why) => problems.push(format!("{}: {why}", path.display())),
            },
            Err(e) => problems.push(format!("{}: {e}", path.display())),
        }
    }
    if problems.is_empty() {
        problems.push(format!("not installed; run `simul install-hooks {agent}`"));
    }
    fail(name, problems.join("; "))
}

/// Every Claude event has a simul hook whose binary exists.
pub fn check_claude_hooks(settings: &Value) -> std::result::Result<(), String> {
    check_installed(&install_hooks::managed(settings), claude::EVENTS, "claude")
}

fn check_installed(
    installed: &[(String, String)],
    events: &[&str],
    agent: &str,
) -> std::result::Result<(), String> {
    if installed.is_empty() {
        return Err(format!("no simul hooks; run `simul install-hooks {agent}`"));
    }
    let missing: Vec<&str> = events
        .iter()
        .copied()
        .filter(|e| !installed.iter().any(|(ev, _)| ev == e))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing {}; rerun `simul install-hooks {agent}`",
            missing.join(", ")
        ));
    }
    for (_, cmd) in installed {
        match first_word(cmd) {
            Some(exe) if Path::new(&exe).exists() => {}
            Some(exe) => return Err(format!("hook binary {exe} does not exist")),
            None => return Err(format!("cannot parse hook command: {cmd}")),
        }
    }
    Ok(())
}

/// Every Kiro trigger has a simul hook whose binary exists.
pub fn check_kiro_hooks(file: &Value) -> std::result::Result<(), String> {
    check_installed(&install_hooks::kiro_managed(file), kiro::EVENTS, "kiro")
}

/// Checks the global and project Kiro hook files; skipped when Kiro CLI isn't installed.
fn check_kiro_hooks_file(global: Option<&Path>, project: &Path, cli_installed: bool) -> Check {
    let paths: Vec<PathBuf> = global
        .into_iter()
        .chain([project])
        .map(Path::to_path_buf)
        .collect();
    // ~/.kiro alone proves nothing: Kiro IDE creates it too, and only the CLI runs these hooks.
    if !cli_installed && !paths.iter().any(|p| p.exists()) {
        return pass("kiro hooks", "skipped: kiro-cli not on PATH");
    }
    check_hook_files("kiro hooks", "kiro", &paths, check_kiro_hooks)
}

/// Whether an executable `name` is in one of the directories of a `$PATH` value.
fn on_path(name: &str, path: Option<&str>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.into_iter()
        .flat_map(|p| p.split(':'))
        .filter(|dir| !dir.is_empty())
        .any(|dir| {
            std::fs::metadata(Path::new(dir).join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
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

    #[test]
    fn kiro_hooks_check() {
        let exe = std::env::current_exe().unwrap();
        let good = install_hooks::kiro_hooks(&exe);
        assert_eq!(check_kiro_hooks(&good), Ok(()));
        let gone = install_hooks::kiro_hooks(Path::new("/nonexistent/simul"));
        assert!(
            check_kiro_hooks(&gone)
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(
            check_kiro_hooks(&json!({}))
                .unwrap_err()
                .contains("install-hooks kiro")
        );
        let mut partial = good.clone();
        partial["hooks"]
            .as_array_mut()
            .unwrap()
            .retain(|h| h["trigger"] != "Stop");
        assert!(check_kiro_hooks(&partial).unwrap_err().contains("Stop"));
    }

    #[test]
    fn kiro_hooks_file_is_skipped_until_kiro_cli_exists() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join(".kiro/hooks/simul.json");
        let project = dir.path().join("repo/.kiro/hooks/simul.json");
        // Kiro IDE also creates ~/.kiro; only the CLI runs these hooks.
        std::fs::create_dir(dir.path().join(".kiro")).unwrap();
        let check = check_kiro_hooks_file(Some(&global), &project, false);
        assert!(check.ok && check.detail.contains("skipped"), "{check:?}");
        assert!(check_kiro_hooks_file(None, &project, false).ok);

        let check = check_kiro_hooks_file(Some(&global), &project, true);
        assert!(
            !check.ok && check.detail.contains("install-hooks kiro"),
            "{check:?}"
        );

        let exe = std::env::current_exe().unwrap();
        install_hooks::install_kiro(&project, &exe).unwrap();
        let check = check_kiro_hooks_file(Some(&global), &project, false);
        assert!(check.ok && check.detail.contains("repo/.kiro"), "{check:?}");

        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(&global, "{ not json").unwrap();
        let check = check_kiro_hooks_file(Some(&global), &project, true);
        assert!(check.ok, "one good file is enough: {check:?}");
        std::fs::remove_file(&project).unwrap();
        let check = check_kiro_hooks_file(Some(&global), &project, false);
        assert!(
            !check.ok && check.detail.contains("simul.json"),
            "{check:?}"
        );
    }

    #[test]
    fn on_path_finds_executables_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("kiro-cli");
        std::fs::write(&bin, "").unwrap();
        let path = format!("/nonexistent:{}", dir.path().display());
        assert!(!on_path("kiro-cli", Some(&path)), "not executable");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(on_path("kiro-cli", Some(&path)));
        assert!(!on_path("kiro-cli", Some("/nonexistent")));
        assert!(!on_path("kiro-cli", None));
    }

    #[test]
    fn disabled_kiro_hook_is_reported_missing() {
        let exe = std::env::current_exe().unwrap();
        let mut file = install_hooks::kiro_hooks(&exe);
        for h in file["hooks"].as_array_mut().unwrap() {
            if h["trigger"] == "Stop" {
                h["enabled"] = json!(false);
            }
        }
        assert!(check_kiro_hooks(&file).unwrap_err().contains("Stop"));
    }
}
