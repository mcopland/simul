use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::hook::{claude, kiro};
use crate::paths::env_var;
use crate::tmux::shell_quote;

/// Trailing shell comment that identifies hooks simul owns, so reinstalling replaces them.
pub const MARKER: &str = "#simul-managed";

/// Where `install-hooks kiro --project` writes, relative to the current directory.
pub const KIRO_PROJECT_FILE: &str = ".kiro/hooks/simul.json";

/// Keeps project installs out of git; a bare `.kiro/hooks/...` would only match at the repo root.
pub const KIRO_EXCLUDE_PATTERN: &str = "**/.kiro/hooks/simul.json";

pub fn run(agent: &str, project: bool, settings: Option<PathBuf>) -> Result<()> {
    match agent {
        "claude" => run_claude(project, settings),
        "kiro" => run_kiro(project, settings),
        _ => bail!("install-hooks: unknown agent {agent} (expected claude or kiro)"),
    }
}

/// `simul uninstall-hooks`: removes what `run` installed for the same arguments.
pub fn run_uninstall(agent: &str, project: bool, settings: Option<PathBuf>) -> Result<()> {
    match agent {
        "claude" => {
            let path = claude_settings_path(project, settings)?;
            match uninstall(&path)? {
                0 => println!("no simul hooks in {}", path.display()),
                n => println!("removed {n} Claude hooks from {}", path.display()),
            }
        }
        "kiro" => {
            let path = kiro_path(project, settings)?;
            if uninstall_kiro(&path)? {
                println!("removed {}", path.display());
            } else {
                println!("no simul hook file at {}", path.display());
            }
            if project {
                let cwd =
                    std::env::current_dir().context("failed to read the current directory")?;
                match unexclude_project_hooks(&cwd)? {
                    Unexclude::Removed(exclude) => {
                        println!("removed {KIRO_EXCLUDE_PATTERN} from {}", exclude.display());
                    }
                    Unexclude::Kept(others) => println!(
                        "kept {KIRO_EXCLUDE_PATTERN} in git's exclude file for {}",
                        others.join(", ")
                    ),
                    Unexclude::NotARepo | Unexclude::Absent => {}
                }
            }
        }
        _ => bail!("uninstall-hooks: unknown agent {agent} (expected claude or kiro)"),
    }
    Ok(())
}

fn claude_settings_path(project: bool, settings: Option<PathBuf>) -> Result<PathBuf> {
    match (settings, project) {
        (Some(p), _) => Ok(p),
        // Local, not shared: the hook embeds this machine's absolute binary path.
        (None, true) => Ok(PathBuf::from(".claude/settings.local.json")),
        (None, false) => env_var("HOME")
            .map(|h| PathBuf::from(h).join(".claude/settings.json"))
            .context("cannot locate ~/.claude/settings.json: $HOME unset"),
    }
}

fn kiro_path(project: bool, file: Option<PathBuf>) -> Result<PathBuf> {
    match (file, project) {
        (Some(p), _) => Ok(p),
        // Local, not shared: the hook embeds this machine's absolute binary path.
        (None, true) => Ok(PathBuf::from(KIRO_PROJECT_FILE)),
        (None, false) => kiro_hook_file(env_var)
            .context("cannot locate ~/.kiro/hooks: $KIRO_HOME and $HOME unset"),
    }
}

fn run_kiro(project: bool, file: Option<PathBuf>) -> Result<()> {
    let path = kiro_path(project, file)?;
    let exe = std::env::current_exe().context("failed to locate the simul binary")?;
    let written = install_kiro(&path, &exe)?;
    println!(
        "installed {} Kiro hooks into {}",
        kiro::EVENTS.len(),
        written.display()
    );
    if project {
        exclude_project_hooks().with_context(|| {
            format!("hooks are installed, but {KIRO_PROJECT_FILE} is not excluded from git")
        })?;
    }
    Ok(())
}

fn exclude_project_hooks() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the current directory")?;
    match git_exclude_file(&cwd)? {
        Some(exclude) => {
            if add_exclude(&exclude, KIRO_EXCLUDE_PATTERN)? {
                println!("added {KIRO_EXCLUDE_PATTERN} to {}", exclude.display());
            }
        }
        None => println!("not in a git repo; {KIRO_PROJECT_FILE} is not excluded from anything"),
    }
    Ok(())
}

/// What `unexclude_project_hooks` did with the exclude line.
#[derive(Debug)]
pub enum Unexclude {
    NotARepo,
    /// The line was not there.
    Absent,
    /// Removed from this exclude file.
    Removed(PathBuf),
    /// Left in place for these other project hook files, relative to the repo root.
    Kept(Vec<String>),
}

/// Drops the exclude line for project Kiro hooks from the repo containing `dir`, unless other
/// directories in that repo still hold one: the line covers the whole repo.
pub fn unexclude_project_hooks(dir: &Path) -> Result<Unexclude> {
    let Some(exclude) = git_exclude_file(dir)? else {
        return Ok(Unexclude::NotARepo);
    };
    let others = ignored_project_hook_files(dir)?;
    if !others.is_empty() {
        return Ok(Unexclude::Kept(others));
    }
    Ok(if remove_exclude(&exclude, KIRO_EXCLUDE_PATTERN)? {
        Unexclude::Removed(exclude)
    } else {
        Unexclude::Absent
    })
}

/// Project Kiro hook files anywhere in the repo containing `dir` that git ignores.
fn ignored_project_hook_files(dir: &Path) -> Result<Vec<String>> {
    let pathspec = format!(":(top,glob){KIRO_EXCLUDE_PATTERN}");
    let out = std::process::Command::new("git")
        .args(["ls-files", "--others", "--ignored", "--exclude-standard"])
        .args(["--full-name", "--", &pathspec])
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run git ls-files in {}", dir.display()))?;
    if !out.status.success() {
        bail!(
            "git ls-files failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

fn run_claude(project: bool, settings: Option<PathBuf>) -> Result<()> {
    let path = claude_settings_path(project, settings)?;
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
    crate::kiro_sessions::kiro_home(env).map(|home| home.join("hooks/simul.json"))
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
                // The unit is undocumented. 500 is safe either way: as ms it still outlasts the
                // shim's 200 ms budget, and as seconds the shim exits long before it.
                "timeout": 500,
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
        .filter(|h| h.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|h| {
            let trigger = h.get("trigger").and_then(Value::as_str)?;
            let command = h.pointer("/action/command").and_then(Value::as_str)?;
            command
                .ends_with(MARKER)
                .then(|| (trigger.to_owned(), command.to_owned()))
        })
        .collect()
}

/// Writes simul's Kiro hook file, replacing an earlier one but never a file with other hooks.
/// Fails unless the Kiro hook file at `path` holds only simul's hooks; `verb` names the refused
/// action in the error.
fn check_kiro_file_is_ours(path: &Path, verb: &str) -> Result<()> {
    let old = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let old: Value = serde_json::from_str(&old).with_context(|| {
        format!(
            "refusing to {verb} {}: not a JSON hook file",
            path.display()
        )
    })?;
    let hooks = old.get("hooks").and_then(Value::as_array);
    let total = hooks.map_or(0, Vec::len);
    let ours = hooks
        .into_iter()
        .flatten()
        .filter(|h| {
            h.pointer("/action/command")
                .and_then(Value::as_str)
                .is_some_and(|c| c.ends_with(MARKER))
        })
        .count();
    if ours < total {
        bail!(
            "refusing to {verb} {}: it holds hooks simul does not manage",
            path.display()
        );
    }
    Ok(())
}

pub fn install_kiro(path: &Path, exe: &Path) -> Result<PathBuf> {
    if exists(path)? {
        check_kiro_file_is_ours(path, "replace")?;
    }
    let mut text =
        serde_json::to_string_pretty(&kiro_hooks(exe)).context("failed to encode Kiro hooks")?;
    text.push('\n');
    replace_file(path, &text, false)
}

/// The exclude file of the git repo containing `dir`; `None` outside a repo.
pub fn git_exclude_file(dir: &Path) -> Result<Option<PathBuf>> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run git in {}", dir.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        bail!(
            "git rev-parse --git-path info/exclude failed in {}: {}",
            dir.display(),
            stderr.trim()
        );
    }
    let path = String::from_utf8(out.stdout)
        .with_context(|| format!("git printed a non-UTF-8 exclude path in {}", dir.display()))?;
    // Relative to `dir` unless the repo is elsewhere (worktrees, $GIT_DIR).
    Ok(Some(dir.join(path.trim_end_matches('\n'))))
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

/// Drops lines equal to `entry` from an exclude file; true if any were removed.
pub fn remove_exclude(file: &Path, entry: &str) -> Result<bool> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", file.display())),
    };
    let kept: Vec<&str> = text.lines().filter(|l| l.trim() != entry).collect();
    if kept.len() == text.lines().count() {
        return Ok(false);
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(file, out).with_context(|| format!("failed to write {}", file.display()))?;
    Ok(true)
}

/// Merges hooks into the settings file, writing through a symlink rather than replacing it.
pub fn install(path: &Path, exe: &Path) -> Result<PathBuf> {
    let settings = if exists(path)? {
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

/// Removes simul's hooks from a Claude settings file, and the `hooks` table if that empties it.
/// Returns how many were removed; a missing file, or one without them, is left untouched.
pub fn uninstall(path: &Path) -> Result<usize> {
    if !exists(path)? {
        return Ok(0);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut settings: Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    let removed = managed(&settings).len();
    if removed == 0 {
        return Ok(0);
    }
    strip_managed(&mut settings);
    if let Some(root) = settings.as_object_mut()
        && root
            .get("hooks")
            .and_then(Value::as_object)
            .is_some_and(serde_json::Map::is_empty)
    {
        root.remove("hooks");
    }
    let mut text = serde_json::to_string_pretty(&settings).context("failed to encode settings")?;
    text.push('\n');
    replace_file(path, &text, true)?;
    Ok(removed)
}

/// Deletes simul's Kiro hook file, refusing one that holds other hooks. A symlink is removed
/// itself, leaving the file it points to. Returns false if there was no file.
pub fn uninstall_kiro(path: &Path) -> Result<bool> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(false);
    }
    if path.exists() {
        check_kiro_file_is_ours(path, "remove")?;
    }
    std::fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(true)
}

/// Whether `path` exists, following symlinks. A symlink whose target is missing is an error:
/// treating it as absent would replace the link, and the settings behind it, with a new file.
fn exists(path: &Path) -> Result<bool> {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if is_link && !path.exists() {
        let target = std::fs::read_link(path)
            .with_context(|| format!("failed to read link {}", path.display()))?;
        bail!(
            "{} links to {}, which does not exist; restore it or remove the link",
            path.display(),
            target.display()
        );
    }
    Ok(path.exists())
}

/// Atomically replaces the file behind `path` (following symlinks) with `text`, optionally keeping
/// the old content as `<name>.simul-bak`. Returns the file actually written.
fn replace_file(path: &Path, text: &str, backup: bool) -> Result<PathBuf> {
    let real = if exists(path)? {
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

/// Copies `real` to `<name>.simul-bak`, unless a backup exists and `real` already holds simul's
/// hooks: then the backup is the copy from before simul, and overwriting it would lose it.
fn write_backup(real: &Path) -> Result<()> {
    let old = std::fs::read(real).with_context(|| format!("failed to read {}", real.display()))?;
    let backup = real.with_file_name(format!(
        "{}.simul-bak",
        real.file_name()
            .map_or_else(Default::default, |n| n.to_string_lossy())
    ));
    let has_ours = serde_json::from_slice::<Value>(&old).is_ok_and(|v| !managed(&v).is_empty());
    if has_ours && backup.exists() {
        return Ok(());
    }
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

    fn backup_of(path: &Path) -> Value {
        let mut name = path.file_name().unwrap().to_owned();
        name.push(".simul-bak");
        read_json(&path.with_file_name(name))
    }

    #[test]
    fn reinstalling_keeps_the_original_in_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, user_settings().to_string()).unwrap();
        install(&path, Path::new("/old/simul")).unwrap();
        install(&path, Path::new(EXE)).unwrap();
        assert_eq!(backup_of(&path), user_settings());
    }

    #[test]
    fn a_file_without_simul_hooks_refreshes_the_backup() {
        // The user removed simul's hooks and changed the file since; that is the new original.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, user_settings().to_string()).unwrap();
        install(&path, Path::new(EXE)).unwrap();
        let edited = json!({"model": "sonnet"});
        std::fs::write(&path, edited.to_string()).unwrap();
        install(&path, Path::new(EXE)).unwrap();
        assert_eq!(backup_of(&path), edited);
    }

    #[test]
    fn install_refuses_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(dir.path().join("gone.json"), &link).unwrap();
        let err = format!("{:#}", install(&link, Path::new(EXE)).unwrap_err());
        assert!(
            err.contains("gone.json") && err.contains("does not exist"),
            "{err}"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is left alone"
        );
        let kiro = dir.path().join("simul.json");
        std::os::unix::fs::symlink(dir.path().join("gone-kiro.json"), &kiro).unwrap();
        assert!(install_kiro(&kiro, Path::new(EXE)).is_err());
        assert!(!dir.path().join("gone.json").exists());
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
            assert_eq!(hook["timeout"], 500);
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
        let mut off = kiro_hooks(Path::new(EXE));
        off["hooks"][4]["enabled"] = json!(false);
        let m = kiro_managed(&off);
        assert_eq!(m.len(), kiro::EVENTS.len() - 1);
        assert!(m.iter().all(|(t, _)| t != "Stop"), "{m:?}");
    }

    #[test]
    fn install_kiro_refuses_files_with_other_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.json");
        let mine = r#"{"version": "v1", "hooks": [{"name": "fmt", "trigger": "Stop", "action": {"type": "command", "command": "/u/fmt.sh"}}]}"#;
        for text in [mine, "{ not json"] {
            std::fs::write(&path, text).unwrap();
            let err = format!("{:#}", install_kiro(&path, Path::new(EXE)).unwrap_err());
            assert!(err.contains("mine.json"), "{err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
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
    fn uninstall_removes_only_simul_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, user_settings().to_string()).unwrap();
        install(&path, Path::new(EXE)).unwrap();
        assert_eq!(uninstall(&path).unwrap(), claude::EVENTS.len());
        assert_eq!(read_json(&path), user_settings());
        assert_eq!(uninstall(&path).unwrap(), 0, "nothing left to remove");
        assert_eq!(read_json(&path), user_settings());
    }

    #[test]
    fn uninstall_drops_the_hooks_table_it_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"model": "opus"}"#).unwrap();
        install(&path, Path::new(EXE)).unwrap();
        uninstall(&path).unwrap();
        assert_eq!(read_json(&path), json!({"model": "opus"}));
    }

    #[test]
    fn uninstall_leaves_missing_and_invalid_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(uninstall(&path).unwrap(), 0);
        assert!(!path.exists(), "no file is created");
        std::fs::write(&path, "{ not json").unwrap();
        let err = format!("{:#}", uninstall(&path).unwrap_err());
        assert!(err.contains("settings.json"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn uninstall_kiro_removes_only_a_simul_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("simul.json");
        assert!(!uninstall_kiro(&path).unwrap(), "nothing to remove");
        install_kiro(&path, Path::new(EXE)).unwrap();
        assert!(uninstall_kiro(&path).unwrap());
        assert!(!path.exists());

        let mine = r#"{"version": "v1", "hooks": [{"name": "fmt", "trigger": "Stop", "action": {"type": "command", "command": "/u/fmt.sh"}}]}"#;
        std::fs::write(&path, mine).unwrap();
        let err = format!("{:#}", uninstall_kiro(&path).unwrap_err());
        assert!(err.contains("simul.json"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), mine);
    }

    #[test]
    fn remove_exclude_drops_only_that_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("info/exclude");
        assert!(
            !remove_exclude(&file, KIRO_EXCLUDE_PATTERN).unwrap(),
            "no file"
        );
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, format!("# ignore\n{KIRO_EXCLUDE_PATTERN}\n*.log\n")).unwrap();
        assert!(remove_exclude(&file, KIRO_EXCLUDE_PATTERN).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# ignore\n*.log\n");
        assert!(!remove_exclude(&file, KIRO_EXCLUDE_PATTERN).unwrap());
    }

    #[test]
    fn add_exclude_appends_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("info/exclude");
        assert!(
            add_exclude(&file, KIRO_PROJECT_FILE).unwrap(),
            "creates the file"
        );
        assert!(!add_exclude(&file, KIRO_PROJECT_FILE).unwrap());
        std::fs::write(&file, "# ignore\n*.log").unwrap();
        assert!(add_exclude(&file, KIRO_PROJECT_FILE).unwrap());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("# ignore\n*.log\n{KIRO_PROJECT_FILE}\n")
        );
    }

    #[test]
    fn exclude_pattern_ignores_project_files_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q"]));
        let exclude = git_exclude_file(dir.path()).unwrap().unwrap();
        add_exclude(&exclude, KIRO_EXCLUDE_PATTERN).unwrap();
        for rel in [KIRO_PROJECT_FILE, "sub/dir/.kiro/hooks/simul.json"] {
            assert!(git(&["check-ignore", "-q", rel]), "{rel} not ignored");
        }
        assert!(!git(&["check-ignore", "-q", ".kiro/hooks/team.json"]));
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
        assert!(found.is_absolute(), "{}", found.display());
        assert_eq!(
            found.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap().join(".git/info/exclude")
        );
        let err = git_exclude_file(&dir.path().join("missing")).unwrap_err();
        assert!(format!("{err:#}").contains("missing"), "{err:#}");
    }

    #[test]
    fn unexclude_keeps_the_line_while_other_project_files_need_it() {
        let dir = tempfile::tempdir().unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(ok.success());
        let exclude = git_exclude_file(dir.path()).unwrap().unwrap();
        add_exclude(&exclude, KIRO_EXCLUDE_PATTERN).unwrap();
        let (a, b) = (dir.path().join("pkg/a"), dir.path().join("pkg/b"));
        for pkg in [&a, &b] {
            install_kiro(&pkg.join(KIRO_PROJECT_FILE), Path::new(EXE)).unwrap();
        }

        assert!(uninstall_kiro(&a.join(KIRO_PROJECT_FILE)).unwrap());
        match unexclude_project_hooks(&a).unwrap() {
            Unexclude::Kept(others) => {
                assert_eq!(others, ["pkg/b/.kiro/hooks/simul.json"])
            }
            other => panic!("expected the line kept, got {other:?}"),
        }
        let text = std::fs::read_to_string(&exclude).unwrap();
        assert!(text.contains(KIRO_EXCLUDE_PATTERN), "{text}");

        assert!(uninstall_kiro(&b.join(KIRO_PROJECT_FILE)).unwrap());
        assert!(matches!(
            unexclude_project_hooks(&b).unwrap(),
            Unexclude::Removed(_)
        ));
        let text = std::fs::read_to_string(&exclude).unwrap();
        assert!(!text.contains(KIRO_EXCLUDE_PATTERN), "{text}");
        assert!(matches!(
            unexclude_project_hooks(&b).unwrap(),
            Unexclude::Absent
        ));

        let outside = tempfile::tempdir().unwrap();
        assert!(matches!(
            unexclude_project_hooks(outside.path()).unwrap(),
            Unexclude::NotARepo
        ));
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
