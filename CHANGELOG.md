# Changelog

All notable changes to simul are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- The daemon rereads a folder's git status after an agent tool call only when the tool can change files: Claude's Edit, Write, MultiEdit, NotebookEdit, and Bash, and Kiro's fs_write and execute_bash. Reads, searches, and MCP tools no longer start `git status`. Turn ends and the one-minute refresh still catch changes made any other way.

## [1.1.0] - 2026-11-23

### Added

- `[panes] git_refresh_secs` sets how often the daemon rereads git status when nothing else asks for it. The default is 60 seconds.

### Changed

- The daemon no longer starts a tmux process every second while the sidebar is shown. tmux reports pane changes to the daemon's control client, and the daemon reads tmux only after a change. A hidden sidebar starts no tmux process until something changes.
- The daemon rereads a folder's git status when an agent session starts or stops, after an agent tool call (at most every 5 seconds), and when a command in a plain pane ends. It no longer runs `git status` every 5 seconds. Idle CPU fell from about 2.1% to 0.5% of a core in the 13-window bench.

### Fixed

- The daemon's control client can no longer start a tmux server. Before, a reattach after the server stopped could start a new server, and tmux-continuum restored into it.
- An in-place restart after a plugin update no longer leaves a zombie tmux process.
- Sidebars show a window's new index after `move-window -r` or another renumber.
- The daemon forgets notification records for closed panes.

## [1.0.0] - 2026-11-16

### Added

- Plain panes show how a command ended: `✓` for success and `✗` for failure, where the shell reports its commands with OSC 133 shell integration. Such a command shows when it starts and ends, at once, even if it ran for less than a second. The README explains how to turn on Ghostty's zsh integration in tmux panes. Panes without it still show `■`.

### Changed

- simul requires tmux 3.8 or newer. The plugin shows a message and does nothing on an older tmux, and `simul doctor` fails its tmux check.
- A pane switch, a client resize, or a layout change no longer starts a process. simul's tmux hooks send the daemon a message through its control client, where each started `sh` and `simul internal` before.

## [0.11.6] - 2026-11-16

### Changed

- Sidebars no longer wake on each tool call an agent makes. The daemon sends a status only when a sidebar would draw it differently, and a tool call changes nothing a sidebar draws.
- The daemon starts fewer processes while the sidebar is shown. One tmux call reads panes, clients, and `focus-events`, where two or three ran before. A pane outside any git repo no longer runs `git status` every 5 seconds. A layout change in the daemon's own session takes one look at tmux, not two.
- A click on a sidebar row jumps with one tmux call instead of three.
- The daemon reads a Kiro session log again only when the log or `session.json` changed, not every second. It no longer reads Claude Code's session files while no Claude pane is open.

## [0.11.5] - 2026-11-15

### Fixed

- After tmux-resurrect restores a session, each window gets its sidebar back instead of an empty shell pane where the sidebar was. Sidebars now carry the pane title `simul-sidebar`, which resurrect restores, and the daemon restarts a restored pane with that title as a sidebar and turns the sidebar on. A shell that sets its own pane title at startup can still leave the empty pane.

## [0.11.4] - 2026-11-15

### Changed

- Sidebars use less CPU. Each one redraws when the daemon reports a change, on a click, or when an age it shows ticks over, instead of four times a second, and ages tick only in windows on screen. The daemon sends a status only when it changed, not every second, and a sidebar learns its window from that status instead of asking tmux every 5 seconds.

## [0.11.3] - 2026-11-14

### Fixed

- Switching panes no longer opens a `'...' returned 1` message after the daemon stops. `pkill -f 'simul daemon'` now removes the daemon's tmux hooks, and a hook left by a crashed daemon or a deleted binary stays silent.
- On WSL with `notify.desktop = true`, a branch name holding a curly single quote (U+2018, U+2019, U+201A, or U+201B) can no longer end the PowerShell string in a notification and run commands.
- The Claude settings backup, `settings.json.simul-bak`, gets the settings file's mode and owner, so a private settings file no longer leaves a backup others can read. A backup that an earlier version left readable loses the permissions the settings file lacks the next time `install-hooks claude` runs. The backup is also replaced whole, like the settings file.
- A plugin reload with a config that fails to parse keeps the running daemon instead of stopping it and starting none. The daemon log now records why a daemon failed to start.
- Two daemons that start at the same moment for one tmux server no longer both run. A lock file beside the socket lets only one take over a stale socket.
- With `CLAUDE_CONFIG_DIR` set, `install-hooks claude`, `uninstall-hooks claude`, and `doctor` use `$CLAUDE_CONFIG_DIR/settings.json`, the file Claude Code reads. They used `~/.claude/settings.json` before.
- `install-hooks kiro --project` and `uninstall-hooks kiro --project` outside a git repo print their notice under a translated git too, instead of failing.
- A failed toggle shows simul's message, which names the daemon socket and `simul doctor`, not only `'...' toggle returned 1`.
- A daemon out of file descriptors or memory waits 100 ms between attempts to accept a connection, instead of filling its log.

## [0.11.2] - 2026-11-13

### Fixed

- `uninstall-hooks kiro` on a symlinked hook file takes simul's hooks out of the file it points to. It removed only the link before, which left the hooks for a dotfiles tool to link back.
- Two `install-hooks` runs at the same time no longer overwrite each other's temporary file.
- Edits to `.git/info/exclude` replace the whole file at once, so an interrupted write can't truncate it.
- Files that `install-hooks` and `uninstall-hooks` write reach the disk before they replace the old ones, are never readable by others while being written, and keep the original's owner. A new settings file is created readable by you only.

## [0.11.1] - 2026-11-13

### Fixed

- A new pane no longer shows `■` before anything ran in it. simul could catch tmux, or the default shell tmux starts the command with, before the pane's own program, so it now waits until a new pane is a second old before judging it.

## [0.11.0] - 2026-11-12

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

[Unreleased]: https://github.com/mcopland/simul/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/mcopland/simul/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/mcopland/simul/compare/v0.11.6...v1.0.0
[0.11.6]: https://github.com/mcopland/simul/compare/v0.11.5...v0.11.6
[0.11.5]: https://github.com/mcopland/simul/compare/v0.11.4...v0.11.5
[0.11.4]: https://github.com/mcopland/simul/compare/v0.11.3...v0.11.4
[0.11.3]: https://github.com/mcopland/simul/compare/v0.11.2...v0.11.3
[0.11.2]: https://github.com/mcopland/simul/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/mcopland/simul/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/mcopland/simul/compare/v0.10.0...v0.11.0
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
