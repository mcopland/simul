# simul

A tmux plugin that tracks the state of coding agents (Claude Code and Kiro CLI), shows them in a sidebar, and notifies you when one is blocked or done.

Status: early development. See `fixtures/CALIBRATION.md` for verified agent behavior.

## Requirements

- tmux 3.3 or newer, with `set -g focus-events on` (without it, simul cannot tell whether you are looking at a pane)
- macOS, Linux, or WSL
- `curl`, `tar`, and `sha256sum` or `shasum`, to download and verify the prebuilt binary (macOS and Linux on x86_64 or arm64; WSL uses the Linux build)

## Install

With TPM, add this to `tmux.conf` and press `prefix + I`:

```tmux
set -g @plugin 'mcopland/simul'
```

When tmux loads the plugin, it downloads the release binary matching the plugin's `VERSION` into `~/.tmux/plugins/simul/bin/simul` and checks its sha256. Then it starts the daemon. Link the binary onto your PATH:

```sh
ln -s ~/.tmux/plugins/simul/bin/simul ~/.local/bin/simul  # or any directory on your PATH
```

Then install the agent hooks:

```sh
simul install-hooks claude            # ~/.claude/settings.json
simul install-hooks claude --project  # ./.claude/settings.local.json
simul install-hooks kiro              # ~/.kiro/hooks/simul.json ($KIRO_HOME/hooks if set)
simul install-hooks kiro --project    # ./.kiro/hooks/simul.json, added to .git/info/exclude
```

Hooks record the path you ran `simul` from, so run it through a path that won't move, such as the symlink above. Plugin updates replace the binary at the same path. Start new agent sessions to pick up the hooks.

For Claude, the merge keeps your existing hooks. If the settings file is a symlink, simul writes through it and leaves the link in place. Each run replaces simul's previous entries, which are tagged `#simul-managed`, and saves a `.simul-bak` copy first.

For Kiro, simul owns the whole `simul.json` hook file and rewrites it on each run. It uses the standalone hook files of Kiro CLI's V3 engine; the V2 engine reads hooks only from agent profiles, which simul does not edit. Kiro support is built from Kiro's public docs and has not yet been checked against a live Kiro session; see `fixtures/CALIBRATION.md`.

Check the setup with `simul doctor`. After a TPM update (`prefix + U`), reload tmux (`tmux source ~/.tmux.conf`) to download the new binary, then run `pkill -f 'simul daemon'`. After a rebuild, just run the `pkill`. The next hook event or tmux reload starts the new binary, and toggling the sidebar off and on restarts it too.

### Build from source

Without TPM, or on a platform with no prebuilt binary, build with a Rust toolchain:

```sh
cargo build --release
ln -s "$PWD/target/release/simul" ~/.cargo/bin/simul  # or any directory on your PATH
```

Then add `run-shell /path/to/simul/simul.tmux` to `tmux.conf`. When `target/release/simul` exists, the plugin uses it and skips the download, and rebuilds update the symlink in place.

## Use

| Command             | What it does                                                             |
| ------------------- | ------------------------------------------------------------------------ |
| `prefix + a`        | Toggles the sidebar in every window. Set `@simul-key` to change the key. |
| `simul ls [--json]` | Lists tracked agents.                                                    |
| `simul doctor`      | Checks tmux, the daemon, hooks, and notifications.                       |

Sidebar keys:

| Key            | Action                 |
| -------------- | ---------------------- |
| `j` / `k`      | Move                   |
| Enter or click | Jump to the agent      |
| `/`            | Filter by text         |
| `f`            | Cycle the state filter |

Each agent row shows a state symbol, `window:window_name`, and time in the current state, with its branch on the line below. While the sidebar isn't focused, it highlights the agent in its own window, or nothing if that window has none. Like tmux's current window, the highlighted agent and the sidebar's own session are bold and everything else is dimmed; state symbols keep their colors. Focusing the sidebar keeps that row, or selects the first row if none was highlighted.

| Symbol | State     | Meaning                                |
| ------ | --------- | -------------------------------------- |
| `◐`    | `working` | The agent is running                   |
| `⊘`    | `blocked` | The agent is waiting for your approval |
| `●`    | `done`    | Finished, and you haven't looked yet   |
| `○`    | `idle`    | Waiting for input                      |
| `∅`    | `unknown` | Not yet classified                     |

When an agent becomes `blocked` or `done` in a pane you are not looking at, simul shows a message in the tmux status line. Desktop notifications and the terminal bell are off by default; turn them on under `[notify]`.

### tmux config that counts panes

The sidebar is a pane, so `#{window_panes}` includes it. Sidebar panes carry the pane option `@simul-sidebar`, so this format counts only the other panes, one `x` each:

```tmux
#{P:#{?#{@simul-sidebar},,x}}
```

For example, to confirm before closing the last real pane:

```tmux
bind x if -F '#{==:#{P:#{?#{@simul-sidebar},,x}},x}' \
    'confirm-before -p "Close #W? (y/n)" kill-pane' 'kill-pane'
```

After opening a sidebar, simul re-runs `window-layout-changed` hooks, so hooks that use this check see the sidebar's tag.

## Config

`~/.config/simul/config.toml`, all keys optional:

```toml
blocked_threshold_ms = 1500  # quiet time before a busy pane's screen is checked

[sidebar]
width = 30

[notify]
enabled = true   # tmux status-line messages
dedupe_secs = 10
desktop = false  # macOS, Linux D-Bus, or WSL toast notifications
bell = false     # ring the terminal bell

[kiro]
approval_patterns = []  # extra text that marks Kiro's approval prompt on screen
```

Kiro has no hook for approval prompts, so simul reads the bottom of a busy Kiro pane. It recognizes the classic `Allow this action? ... [y/n/t]:` prompt. If your Kiro shows a different approval dialog, add a line of text unique to it to `approval_patterns`. Pick text that disappears once you answer: a pattern still in the bottom 15 lines keeps the pane `blocked`.

The daemon log is next to its socket: `$XDG_RUNTIME_DIR/simul/<hash>.log`, or `/tmp/simul-$UID/<hash>.log`.

## License

MIT, see `LICENSE`.
