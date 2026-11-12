# Changelog

All notable changes to simul are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `simul uninstall-hooks <claude|kiro> [--project]` removes only simul's hooks, and for Kiro `--project` its `.git/info/exclude` line.
- `simul doctor` checks tmux's `mouse` option and that the `simul` on PATH matches the daemon's version.
- `simul doctor` and the daemon log name config keys simul does not know, such as a misspelled one.

### Fixed

- Changing `@simul-key` and reloading unbinds the previous key, unless you rebound it since.
- A plugin path that holds a single quote no longer breaks the daemon, toggle, or installer commands.
- `install-hooks claude` keeps the original settings in `.simul-bak` instead of replacing them with an already-modified copy on the next run.
- A daemon that starts with no sidebar open restores a status line that an earlier daemon hid.
- The daemon log moves to `<hash>.log.1` at 1 MiB instead of growing without limit.
- The binary installer downloads only over https.

## [0.10.0] - 2026-11-11

### Changed

- BREAKING: the sidebar is click-only and needs `set -g mouse on`. It never keeps focus, and its keys (`j`/`k`, Enter, Space, `/`, `f`) are gone; click the `view:` footer to switch views.
- Panes and windows where nothing runs are dimmed, and the pane you are in is bold.

## [0.9.0] - 2026-11-10

### Changed

- Only inactive sessions, windows, and panes are dimmed.

## [0.8.1] - 2026-11-09

### Fixed

- Process checks while the sidebar is hidden run at the configured interval.
- A failed self-update is retried with backoff, and a daemon installs the version its checkout names at startup.

## [0.8.0] - 2026-11-07

### Added

- Panes without an agent show whether a command is running (`▶`), finished unseen (`■`), or an interactive program is open (`▷`).

### Changed

- Pane processes are checked every 10 seconds while the sidebar is hidden (`hidden_check_secs`).

### Fixed

- Scripts are told from prompts by the pane's shell, not `ps`.

## [0.7.2] - 2026-11-06

### Fixed

- The sidebar's own session stays bright like its window.

## [0.7.1] - 2026-11-06

### Fixed

- Sessions with numbers sort in numeric order.

## [0.7.0] - 2026-11-05

### Added

- Each agent shows where it is beside its state symbol.

### Changed

- The sidebar keeps a quarter of its window's width.

### Fixed

- Unselected sessions are dimmed so the selected one stands out.

## [0.6.0] - 2026-11-02

### Added

- The daemon updates itself in place when the plugin is updated.

## [0.5.0] - 2026-11-02

### Added

- Sessions can be collapsed in the sidebar.

## [0.4.0] - 2026-11-02

### Added

- `f` switches the sidebar between the `all` and `status` views.

## [0.3.0] - 2026-11-01

### Added

- Kiro V3 session files drive live status, with a new `asking` state.

## [0.2.0] - 2026-10-31

### Added

- The sidebar lists every session, window, and pane, with git status.
- tmux's first status line is hidden while the sidebar is shown.
- A `failed` state for turns that end in an API error.

### Fixed

- `install-hooks` refuses to write through a dangling symlink.
- Claude's `elicitation_url_dialog` counts as a permission prompt.

## [0.1.1] - 2026-10-27

### Removed

- The sidebar's work directory line.

## [0.1.0] - 2026-10-25

### Added

- Daemon, Claude hook shim, state machine, and `simul ls`.
- Sidebar with state symbols, window names, and branches, toggled from tmux, with blocked and done notifications.
- `simul install-hooks` and `simul doctor` for Claude and Kiro.
- Kiro hook events, with blocked inferred from its approval prompt.
- `simul.tmux` entry point that downloads the prebuilt binary matching `VERSION`.
- Model and effort for each agent in `simul ls`.

[Unreleased]: https://github.com/mcopland/simul/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/mcopland/simul/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/mcopland/simul/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/mcopland/simul/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/mcopland/simul/compare/v0.7.2...v0.8.0
[0.7.2]: https://github.com/mcopland/simul/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/mcopland/simul/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/mcopland/simul/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/mcopland/simul/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/mcopland/simul/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/mcopland/simul/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/mcopland/simul/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/mcopland/simul/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/mcopland/simul/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/mcopland/simul/releases/tag/v0.1.0
