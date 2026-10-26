use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::hook::{claude, kiro};
use crate::paths::env_var;
use crate::tmux::shell_quote;

/// Trailing shell comment that identifies hooks simul owns, so reinstalling replaces them.
pub const MARKER: &str = "#simul-managed";

/// Keeps a project install out of git; unanchored so it matches below the repo root too.
pub const KIRO_EXCLUDE: &str = ".kiro/hooks/simul.json";

pub fn run(agent: &str, project: bool, settings: Option<PathBuf>) -> Result<()> {
    match agent {
        "claude" => run_claude(project, settings),
        "kiro" => run_kiro(project, settings),
        _ => bail!("install-hooks: unknown agent {agent} (expected claude or kiro)"),
    }
}

fn run_kiro(project: bool, file: Option<PathBuf>) -> Result<()> {
    let path = match (file, project) {
        (Some(p), _) => p,
        // Local, not shared: the hook embeds this machine's absolute binary path.
        (None, true) => PathBuf::from(KIRO_EXCLUDE),
        (None, false) => kiro_hook_file(env_var)
            .context("cannot locate ~/.kiro/hooks: $KIRO_HOME and $HOME unset")?,
    };
    let exe = std::env::current_exe().context("failed to locate the simul binary")?;
    let written = install_kiro(&path, &exe)?;
    println!(
        "installed {} Kiro hooks into {}",
        kiro::EVENTS.len(),
        written.display()
    );
    if project {
        match git_exclude_file(Path::new("."))? {
            Some(exclude) => {
                if add_exclude(&exclude, KIRO_EXCLUDE)? {
                    println!("added {KIRO_EXCLUDE} to {}", exclude.display());
                }
            }
            None => println!("not in a git repo; {KIRO_EXCLUDE} is not excluded from anything"),
        }
    }
    Ok(())
}

fn run_claude(project: bool, settings: Option<PathBuf>) -> Result<()> {
    let path = match (settings, project) {
        (Some(p), _) => p,
        // Local, not shared: the hook embeds this machine's absolute binary path.
        (None, true) => PathBuf::from(".claude/settings.local.json"),
        (None, false) => env_var("HOME")
            .map(|h| PathBuf::from(h).join(".claude/settings.json"))
            .context("cannot locate ~/.claude/settings.json: $HOME unset")?,
    };
    let exe = std::env::current_exe().context("failed to locate the simul binary")?;
    let written = install(&path, &exe)?;
    println!(
        "installed {} Claude hooks into {}",
        claude::EVENTS.len(),
        written.display()
    );
    Ok(())
}

pub fn hook_command(exe: &Path, agent: &str, event: &str) -> String {
    format!(
        "{} hook {agent} {event} {MARKER}",
        shell_quote(&exe.to_string_lossy())
    )
}

/// Removes simul's hooks, dropping matcher groups and events that become empty.
pub fn strip_managed(settings: &mut Value) {
    let Some(events) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for groups in events.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                hooks.retain(|h| !is_managed(h));
            }
        }
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|h| !h.is_empty())
        });
    }
    events.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
}

fn is_managed(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.ends_with(MARKER))
}

/// Returns `settings` with exactly one simul hook per Claude event, other hooks untouched.
pub fn merge(mut settings: Value, exe: &Path) -> Result<Value> {
    let Some(root) = settings.as_object_mut() else {
        bail!("settings file is not a JSON object");
    };
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        bail!("settings \"hooks\" is not a JSON object");
    }
    strip_managed(&mut settings);
    let hooks = settings["hooks"]
        .as_object_mut()
        .context("settings \"hooks\" vanished during merge")?;
    for event in claude::EVENTS {
        let group = json!({"hooks": [{
            "type": "command",
            "command": hook_command(exe, "claude", event),
            "timeout": 1,
        }]});
        match hooks.entry(*event).or_insert_with(|| json!([])) {
            Value::Array(groups) => groups.push(group),
            _ => bail!("settings \"hooks.{event}\" is not a JSON array"),
        }
    }
    Ok(settings)
}

/// Commands of simul-managed hooks, keyed by event.
pub fn managed(settings: &Value) -> Vec<(String, String)> {
    let Some(events) = settings.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    events
        .iter()
        .flat_map(|(event, groups)| {
            groups
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|g| g.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter(|h| is_managed(h))
                .filter_map(|h| h.get("command").and_then(Value::as_str))
                .map(move |c| (event.clone(), c.to_owned()))
        })
        .collect()
}

/// Kiro's global hook file: `$KIRO_HOME/hooks/simul.json`, else `~/.kiro/hooks/simul.json`.
pub fn kiro_hook_file(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let home = env("KIRO_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".kiro")))?;
    Some(home.join("hooks/simul.json"))
}

/// The whole simul-owned Kiro hook file, one command hook per V3 trigger.
pub fn kiro_hooks(exe: &Path) -> Value {
    let hooks: Vec<Value> = kiro::EVENTS
        .iter()
        .map(|event| {
            json!({
                "name": format!("simul-{event}"),
                "trigger": event,
                "action": {"type": "command", "command": hook_command(exe, "kiro", event)},
                "timeout": 1,
                "enabled": true,
            })
        })
        .collect();
    json!({"version": "v1", "hooks": hooks})
}

/// `(trigger, command)` of simul-managed hooks in a Kiro hook file.
pub fn kiro_managed(file: &Value) -> Vec<(String, String)> {
    file.get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| {
            let trigger = h.get("trigger").and_then(Value::as_str)?;
            let command = h.pointer("/action/command").and_then(Value::as_str)?;
            command
                .ends_with(MARKER)
                .then(|| (trigger.to_owned(), command.to_owned()))
        })
        .collect()
}

/// Writes simul's Kiro hook file, replacing any earlier one.
pub fn install_kiro(path: &Path, exe: &Path) -> Result<PathBuf> {
    let mut text =
        serde_json::to_string_pretty(&kiro_hooks(exe)).context("failed to encode Kiro hooks")?;
    text.push('\n');
    replace_file(path, &text, false)
}

/// The exclude file of the git repo containing `dir`; `None` outside a repo.
pub fn git_exclude_file(dir: &Path) -> Result<Option<PathBuf>> {
    let out = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ])
        .current_dir(dir)
        .stderr(std::process::Stdio::null())
        .output()
        .with_context(|| format!("failed to run git in {}", dir.display()))?;
    if !out.status.success() {
        return Ok(None);
    }
    let path = String::from_utf8(out.stdout)
        .with_context(|| format!("git printed a non-UTF-8 exclude path in {}", dir.display()))?;
    Ok(Some(PathBuf::from(path.trim_end_matches('\n'))))
}

/// Appends `entry` to an exclude file unless a line already equals it; true if it was added.
pub fn add_exclude(file: &Path, entry: &str) -> Result<bool> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", file.display())),
    };
    if text.lines().any(|l| l.trim() == entry) {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let sep = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    std::fs::write(file, format!("{text}{sep}{entry}\n"))
        .with_context(|| format!("failed to write {}", file.display()))?;
    Ok(true)
}

/// Merges hooks into the settings file, writing through a symlink rather than replacing it.
pub fn install(path: &Path, exe: &Path) -> Result<PathBuf> {
    let settings = if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("invalid JSON in {}", path.display()))?
    } else {
        json!({})
    };
    let merged =
        merge(settings, exe).with_context(|| format!("cannot merge into {}", path.display()))?;
    let mut text = serde_json::to_string_pretty(&merged).context("failed to encode settings")?;
    text.push('\n');
    replace_file(path, &text, true)
}

/// Atomically replaces the file behind `path` (following symlinks) with `text`, optionally keeping
/// the old content as `<name>.simul-bak`. Returns the file actually written.
fn replace_file(path: &Path, text: &str, backup: bool) -> Result<PathBuf> {
    let real = if path.exists() {
        let real = path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        if backup {
            write_backup(&real)?;
        }
        real
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        path.to_path_buf()
    };
    let tmp = real.with_extension("simul-tmp");
    std::fs::write(&tmp, text).with_context(|| format!("failed to write {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(&real) {
        std::fs::set_permissions(&tmp, meta.permissions())
            .with_context(|| format!("failed to copy permissions to {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &real)
        .with_context(|| format!("failed to replace {}", real.display()))?;
    Ok(real)
}

fn write_backup(real: &Path) -> Result<()> {
    let old = std::fs::read(real).with_context(|| format!("failed to read {}", real.display()))?;
    let backup = real.with_file_name(format!(
        "{}.simul-bak",
        real.file_name()
            .map_or_else(Default::default, |n| n.to_string_lossy())
    ));
    std::fs::write(&backup, old)
        .with_context(|| format!("failed to write backup {}", backup.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/simul/bin/simul";

    fn user_settings() -> Value {
        json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "/u/check.sh"}]}
                ],
                "PostToolUse": [
                    {"matcher": "Edit", "hooks": [{"type": "command", "command": "/u/fmt.sh"}]}
                ]
            },
            "permissions": {"allow": ["Bash(ls)"]}
        })
    }

    #[test]
    fn command_is_quoted_and_marked() {
        assert_eq!(
            hook_command(Path::new("/a b/simul"), "claude", "Stop"),
            "'/a b/simul' hook claude Stop #simul-managed"
        );
        assert_eq!(
            hook_command(Path::new("/s"), "kiro", "Stop"),
            "'/s' hook kiro Stop #simul-managed"
        );
    }

    #[test]
    fn merge_adds_every_event_and_keeps_user_hooks() {
        let out = merge(user_settings(), Path::new(EXE)).unwrap();
        for event in claude::EVENTS {
            let groups = out["hooks"][event].as_array().unwrap();
            let ours: Vec<&Value> = groups
                .iter()
                .filter(|g| g["hooks"][0]["command"].as_str().unwrap().ends_with(MARKER))
                .collect();
            assert_eq!(ours.len(), 1, "{event}");
            assert_eq!(ours[0]["hooks"][0]["timeout"], 1);
            assert!(ours[0].get("matcher").is_none());
        }
        assert_eq!(
            out["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/u/check.sh"
        );
        assert_eq!(out["hooks"]["PostToolUse"][0]["matcher"], "Edit");
        assert_eq!(out["model"], "opus");
        assert_eq!(out["permissions"], user_settings()["permissions"]);
    }

    #[test]
    fn merge_is_idempotent_and_replaces_old_paths() {
        let once = merge(user_settings(), Path::new("/old/simul")).unwrap();
        let twice = merge(once, Path::new(EXE)).unwrap();
        assert_eq!(twice, merge(user_settings(), Path::new(EXE)).unwrap());
        assert!(!twice.to_string().contains("/old/simul"));
    }

    #[test]
    fn strip_drops_emptied_groups_and_events() {
        let mut v = merge(json!({}), Path::new(EXE)).unwrap();
        strip_managed(&mut v);
        assert_eq!(v, json!({"hooks": {}}));
        let mut mixed = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "/u/a.sh"},
            {"type": "command", "command": "x hook claude Stop #simul-managed"}
        ]}]}});
        strip_managed(&mut mixed);
        assert_eq!(
            mixed,
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/u/a.sh"}]}]}})
        );
    }

    #[test]
    fn merge_rejects_non_object_hooks() {
        let err = merge(json!({"hooks": []}), Path::new(EXE)).unwrap_err();
        assert!(err.to_string().contains("hooks"), "{err}");
        assert!(merge(json!([]), Path::new(EXE)).is_err());
    }

    #[test]
    fn managed_lists_installed_commands() {
        let v = merge(user_settings(), Path::new(EXE)).unwrap();
        let m = managed(&v);
        assert_eq!(m.len(), claude::EVENTS.len());
        assert!(
            m.iter()
                .all(|(_, c)| c.starts_with("'/opt/simul/bin/simul'"))
        );
        assert!(managed(&user_settings()).is_empty());
    }

    #[test]
    fn install_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.local.json");
        install(&path, Path::new(EXE)).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(managed(&v).len(), claude::EVENTS.len());
    }

    #[test]
    fn install_writes_through_symlink_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-settings.json");
        let link = dir.path().join("settings.json");
        std::fs::write(
            &real,
            serde_json::to_string_pretty(&user_settings()).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let written = install(&link, Path::new(EXE)).unwrap();
        assert_eq!(written, real.canonicalize().unwrap());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(managed(&v).len(), claude::EVENTS.len());
        let backup = dir.path().join("dotfiles-settings.json.simul-bak");
        let b: Value = serde_json::from_str(&std::fs::read_to_string(backup).unwrap()).unwrap();
        assert_eq!(b, user_settings());
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn kiro_file_has_one_hook_per_trigger() {
        let file = kiro_hooks(Path::new(EXE));
        assert_eq!(file["version"], "v1");
        let hooks = file["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), kiro::EVENTS.len());
        for (hook, event) in hooks.iter().zip(kiro::EVENTS) {
            assert_eq!(hook["name"], format!("simul-{event}"));
            assert_eq!(hook["trigger"], *event);
            assert_eq!(hook["action"]["type"], "command");
            assert_eq!(
                hook["action"]["command"],
                hook_command(Path::new(EXE), "kiro", event)
            );
            assert_eq!(hook["timeout"], 1);
            assert_eq!(hook["enabled"], true);
            assert!(hook.get("matcher").is_none(), "every tool must match");
        }
    }

    #[test]
    fn kiro_managed_lists_only_simul_hooks() {
        let m = kiro_managed(&kiro_hooks(Path::new(EXE)));
        assert_eq!(m.len(), kiro::EVENTS.len());
        assert_eq!(m[0].0, "SessionStart");
        let other = json!({"version": "v1", "hooks": [
            {"trigger": "Stop", "action": {"type": "command", "command": "/u/x.sh"}}
        ]});
        assert!(kiro_managed(&other).is_empty());
        assert!(kiro_managed(&json!([])).is_empty());
    }

    #[test]
    fn kiro_hook_file_prefers_kiro_home() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            kiro_hook_file(env(&[("KIRO_HOME", "/k"), ("HOME", "/h")])),
            Some(PathBuf::from("/k/hooks/simul.json"))
        );
        assert_eq!(
            kiro_hook_file(env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.kiro/hooks/simul.json"))
        );
        assert_eq!(kiro_hook_file(env(&[])), None);
    }

    #[test]
    fn install_kiro_creates_dirs_and_replaces_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".kiro/hooks/simul.json");
        install_kiro(&path, Path::new("/old/simul")).unwrap();
        assert_eq!(read_json(&path), kiro_hooks(Path::new("/old/simul")));
        install_kiro(&path, Path::new(EXE)).unwrap();
        assert_eq!(read_json(&path), kiro_hooks(Path::new(EXE)));
        // simul owns the whole file, so a backup would only litter the hooks dir.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["simul.json"]);
    }

    #[test]
    fn install_kiro_writes_through_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-simul.json");
        std::fs::write(&real, "{}").unwrap();
        let link = dir.path().join("simul.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let written = install_kiro(&link, Path::new(EXE)).unwrap();
        assert_eq!(written, real.canonicalize().unwrap());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read_json(&real), kiro_hooks(Path::new(EXE)));
    }

    #[test]
    fn add_exclude_appends_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("info/exclude");
        assert!(
            add_exclude(&file, KIRO_EXCLUDE).unwrap(),
            "creates the file"
        );
        assert!(!add_exclude(&file, KIRO_EXCLUDE).unwrap());
        std::fs::write(&file, "# ignore\n*.log").unwrap();
        assert!(add_exclude(&file, KIRO_EXCLUDE).unwrap());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("# ignore\n*.log\n{KIRO_EXCLUDE}\n")
        );
    }

    #[test]
    fn git_exclude_file_finds_the_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_exclude_file(dir.path()).unwrap(), None);
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(ok.success());
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let found = git_exclude_file(&sub).unwrap().unwrap();
        assert_eq!(
            found.canonicalize().unwrap_or(found.clone()),
            dir.path().canonicalize().unwrap().join(".git/info/exclude")
        );
    }

    #[test]
    fn install_leaves_invalid_json_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = format!("{:#}", install(&path, Path::new(EXE)).unwrap_err());
        assert!(err.contains("settings.json"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
