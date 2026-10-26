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
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub width: u16,
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

impl Default for Config {
    fn default() -> Config {
        Config {
            blocked_threshold_ms: 1500,
            sidebar: SidebarConfig::default(),
            notify: NotifyConfig::default(),
            kiro: KiroConfig::default(),
        }
    }
}

impl Default for SidebarConfig {
    fn default() -> SidebarConfig {
        SidebarConfig { width: 30 }
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
        assert_eq!(cfg.sidebar.width, 30);
        assert_eq!(cfg.notify.dedupe_secs, 10);
        assert!(!cfg.notify.desktop, "system notifications are opt-in");
        assert!(!cfg.notify.bell, "the terminal bell is opt-in");
        assert_eq!(cfg.blocked_threshold_ms, 1500);
        assert!(cfg.kiro.approval_patterns.is_empty());
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
        std::fs::write(&path, "[sidebar]\nwidth = 30\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.sidebar.width, 30);
        assert!(cfg.notify.enabled);
    }

    #[test]
    fn bad_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[sidebar]\nwidth = \"wide\"\n").unwrap();
        let err = format!("{:#}", load_from(&path).unwrap_err());
        assert!(err.contains("config.toml"), "{err}");
    }
}
