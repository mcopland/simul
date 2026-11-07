use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths::env_var;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// How long a busy pane may go without a hook event before its screen is checked.
    pub blocked_threshold_ms: u64,
    pub sidebar: SidebarConfig,
    pub notify: NotifyConfig,
    pub kiro: KiroConfig,
    pub panes: PanesConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    /// Hide the session and windows from tmux's first status line, keeping status-right,
    /// while the sidebar is shown.
    pub hide_status: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub dedupe_secs: u64,
    /// macOS, D-Bus, or Windows toast notifications, on top of the tmux status message.
    pub desktop: bool,
    /// Ring the terminal bell, which many terminals turn into a system alert.
    pub bell: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct KiroConfig {
    /// Extra text that marks Kiro's approval prompt, matched in the bottom lines of its pane.
    /// It should vanish once answered, or the pane stays blocked.
    pub approval_patterns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PanesConfig {
    /// Commands that mark a pane interactive rather than running, matched by name.
    pub interactive: Vec<String>,
}

impl Default for PanesConfig {
    fn default() -> PanesConfig {
        let names = [
            "vi", "vim", "nvim", "hx", "helix", "emacs", "nano", "micro", "kak", "less", "more",
            "man", "ssh", "mosh", "htop", "btop", "top", "watch", "lazygit", "tig",
        ];
        PanesConfig {
            interactive: names.into_iter().map(str::to_owned).collect(),
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

/// Loads the config file; a missing file means defaults.
pub fn load_from(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read config: {}", path.display()));
        }
    };
    toml::from_str(&text).with_context(|| format!("invalid config: {}", path.display()))
}

pub fn load() -> Result<Config> {
    match default_path() {
        Some(path) => load_from(&path),
        None => Ok(Config::default()),
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
    fn bad_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[sidebar]\nhide_status = \"wide\"\n").unwrap();
        let err = format!("{:#}", load_from(&path).unwrap_err());
        assert!(err.contains("config.toml"), "{err}");
    }
}
