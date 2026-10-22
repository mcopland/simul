use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::hook::claude;
use crate::paths::env_var;
use crate::tmux::shell_quote;

/// Trailing shell comment that identifies hooks simul owns, so reinstalling replaces them.
pub const MARKER: &str = "#simul-managed";

pub fn run(agent: &str, project: bool, settings: Option<PathBuf>) -> Result<()> {
    if agent != "claude" {
        bail!("install-hooks: {agent} is not supported yet (only claude)");
    }
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

pub fn hook_command(exe: &Path, event: &str) -> String {
    format!(
        "{} hook claude {event} {MARKER}",
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
            "command": hook_command(exe, event),
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

/// Merges hooks into the settings file, writing through a symlink rather than replacing it.
pub fn install(path: &Path, exe: &Path) -> Result<PathBuf> {
    let (real, settings) = if path.exists() {
        let real = path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        let text = std::fs::read_to_string(&real)
            .with_context(|| format!("failed to read {}", real.display()))?;
        let settings: Value = serde_json::from_str(&text)
            .with_context(|| format!("invalid JSON in {}", real.display()))?;
        let backup = real.with_file_name(format!(
            "{}.simul-bak",
            real.file_name()
                .map_or_else(Default::default, |n| n.to_string_lossy())
        ));
        std::fs::write(&backup, &text)
            .with_context(|| format!("failed to write backup {}", backup.display()))?;
        (real, settings)
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        (path.to_path_buf(), json!({}))
    };
    let merged =
        merge(settings, exe).with_context(|| format!("cannot merge into {}", real.display()))?;
    let mut text = serde_json::to_string_pretty(&merged).context("failed to encode settings")?;
    text.push('\n');
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
            hook_command(Path::new("/a b/simul"), "Stop"),
            "'/a b/simul' hook claude Stop #simul-managed"
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
