use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::env_var;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// How long a busy pane may go without a hook event before its screen is checked.
    pub blocked_threshold_ms: u64,
    pub sidebar: SidebarConfig,
    pub notify: NotifyConfig,
    pub kiro: KiroConfig,
    pub panes: PanesConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SidebarConfig {
    /// Hide the session and windows from tmux's first status line, keeping status-right,
    /// while the sidebar is shown.
    pub hide_status: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub dedupe_secs: u64,
    /// macOS, D-Bus, or Windows toast notifications, on top of the tmux status message.
    pub desktop: bool,
    /// Ring the terminal bell, which many terminals turn into a system alert.
    pub bell: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct KiroConfig {
    /// Extra text that marks Kiro's approval prompt, matched in the bottom lines of its pane.
    /// It should vanish once answered, or the pane stays blocked.
    pub approval_patterns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct PanesConfig {
    /// Commands that mark a pane interactive rather than running, matched by name.
    pub interactive: Vec<String>,
    /// How often panes' processes are checked while the sidebar is hidden; it checks every
    /// second while shown. Each check costs CPU, so the default trades a command that runs
    /// and ends unseen between checks for idle cost.
    pub hidden_check_secs: u64,
}

impl Default for PanesConfig {
    fn default() -> PanesConfig {
        let names = [
            "vi", "vim", "nvim", "hx", "helix", "emacs", "nano", "micro", "kak", "less", "more",
            "man", "ssh", "mosh", "htop", "btop", "top", "watch", "lazygit", "tig",
        ];
        PanesConfig {
            interactive: names.into_iter().map(str::to_owned).collect(),
            hidden_check_secs: 10,
        }
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            blocked_threshold_ms: 1500,
            sidebar: SidebarConfig::default(),
            notify: NotifyConfig::default(),
            kiro: KiroConfig::default(),
            panes: PanesConfig::default(),
        }
    }
}

impl Default for SidebarConfig {
    fn default() -> SidebarConfig {
        SidebarConfig { hide_status: true }
    }
}

impl Default for NotifyConfig {
    fn default() -> NotifyConfig {
        NotifyConfig {
            enabled: true,
            dedupe_secs: 10,
            desktop: false,
            bell: false,
        }
    }
}

pub fn default_path() -> Option<PathBuf> {
    env_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env_var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|dir| dir.join("simul/config.toml"))
}

/// Keys simul once read and now ignores, so an old config file raises no warning.
const RETIRED: &[&str] = &["sidebar.width"];

/// Loads the config file; a missing file means defaults.
pub fn load_from(path: &Path) -> Result<Config> {
    load_checked_from(path).map(|(config, _)| config)
}

/// Loads the config file, also returning the dotted names of keys simul doesn't know, which
/// serde skips without a word: usually typos.
pub fn load_checked_from(path: &Path) -> Result<(Config, Vec<String>)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Config::default(), Vec::new()));
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read config: {}", path.display()));
        }
    };
    let config: Config =
        toml::from_str(&text).with_context(|| format!("invalid config: {}", path.display()))?;
    let given: toml::Table =
        toml::from_str(&text).with_context(|| format!("invalid config: {}", path.display()))?;
    let known =
        toml::Table::try_from(Config::default()).context("failed to list the known config keys")?;
    let mut unknown = Vec::new();
    unknown_keys(&given, &known, "", &mut unknown);
    unknown.sort();
    Ok((config, unknown))
}

/// Collects keys of `given` missing from `known`, recursing into tables both have.
fn unknown_keys(given: &toml::Table, known: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in given {
        let name = format!("{prefix}{key}");
        match (known.get(key), value) {
            (None, _) if RETIRED.contains(&name.as_str()) => {}
            (None, _) => out.push(name),
            (Some(toml::Value::Table(known)), toml::Value::Table(given)) => {
                unknown_keys(given, known, &format!("{name}."), out);
            }
            (Some(_), _) => {}
        }
    }
}

pub fn load() -> Result<Config> {
    load_checked().map(|(config, _)| config)
}

/// `load`, plus the unknown keys `load_checked_from` reports.
pub fn load_checked() -> Result<(Config, Vec<String>)> {
    match default_path() {
        Some(path) => load_checked_from(&path),
        None => Ok((Config::default(), Vec::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_from(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
        assert!(cfg.sidebar.hide_status);
        assert_eq!(cfg.notify.dedupe_secs, 10);
        assert!(!cfg.notify.desktop, "system notifications are opt-in");
        assert!(!cfg.notify.bell, "the terminal bell is opt-in");
        assert_eq!(cfg.blocked_threshold_ms, 1500);
        assert!(cfg.kiro.approval_patterns.is_empty());
        for name in ["hx", "nvim", "less", "ssh"] {
            assert!(cfg.panes.interactive.iter().any(|n| n == name), "{name}");
        }
    }

    #[test]
    fn interactive_list_replaces_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[panes]\ninteractive = [\"k9s\"]\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.panes.interactive, vec!["k9s".to_string()]);
        assert_eq!(
            cfg.panes.hidden_check_secs, 10,
            "the rest of the table keeps defaults"
        );
        std::fs::write(&path, "[panes]\nhidden_check_secs = 3\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.panes.hidden_check_secs, 3);
        assert!(cfg.panes.interactive.iter().any(|n| n == "hx"));
    }

    #[test]
    fn kiro_approval_patterns_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[kiro]\napproval_patterns = [\"Yes  Trust  No\"]\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(
            cfg.kiro.approval_patterns,
            vec!["Yes  Trust  No".to_string()]
        );
        assert_eq!(cfg.sidebar, SidebarConfig::default());
    }

    #[test]
    fn partial_file_overrides_only_given_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[sidebar]\nhide_status = false\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert!(!cfg.sidebar.hide_status);
        assert!(cfg.notify.enabled);
        assert_eq!(cfg.blocked_threshold_ms, 1500);
    }

    #[test]
    fn retired_sidebar_width_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[sidebar]\nwidth = 30\nhide_status = false\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert!(
            !cfg.sidebar.hide_status,
            "the rest of the table still applies"
        );
    }

    #[test]
    fn unknown_keys_are_reported_with_their_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "blocked_treshold_ms = 1\n[sidebar]\nhide_stat = false\n[notfy]\nbell = true\n",
        )
        .unwrap();
        let (cfg, unknown) = load_checked_from(&path).unwrap();
        assert_eq!(cfg, Config::default(), "unknown keys change nothing");
        assert_eq!(
            unknown,
            ["blocked_treshold_ms", "notfy", "sidebar.hide_stat"]
        );
    }

    #[test]
    fn known_and_retired_keys_are_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let text = "blocked_threshold_ms = 1\n\
                    [sidebar]\nhide_status = false\nwidth = 30\n\
                    [notify]\nenabled = true\ndedupe_secs = 1\ndesktop = true\nbell = true\n\
                    [kiro]\napproval_patterns = [\"x\"]\n\
                    [panes]\ninteractive = [\"k9s\"]\nhidden_check_secs = 3\n";
        std::fs::write(&path, text).unwrap();
        let (_, unknown) = load_checked_from(&path).unwrap();
        assert!(unknown.is_empty(), "{unknown:?}");
        let (_, unknown) = load_checked_from(&dir.path().join("none.toml")).unwrap();
        assert!(unknown.is_empty());
    }

    #[test]
    fn bad_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[sidebar]\nhide_status = \"wide\"\n").unwrap();
        let err = format!("{:#}", load_from(&path).unwrap_err());
        assert!(err.contains("config.toml"), "{err}");
    }
}
