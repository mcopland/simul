# simul

A tmux plugin that tracks the state of coding agents (Claude Code and Kiro CLI), shows them in a sidebar, and notifies you when one is blocked, asking, done, or failed.

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

For Kiro, simul owns the whole `simul.json` hook file and rewrites it on each run. It uses the standalone hook files of Kiro CLI's V3 engine; the V2 engine reads hooks only from agent profiles, which simul does not edit. Kiro's hooks were built from its public docs and have not yet been checked against a live Kiro session. Its session files were observed with `kiro-cli 2.34.2 --v3`. See `fixtures/CALIBRATION.md`.

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

| Key            | Action                                      |
| -------------- | ------------------------------------------- |
| `j` / `k`      | Move                                        |
| Enter or click | Jump to the window or pane                  |
| `/`            | Filter by text                              |
| `f`            | Switch between the `all` and `status` views |

The sidebar lists every session, every `window_index:window_name` in it, and every pane in each window, whether or not an agent runs there. An agent pane shows a state symbol, the agent name, and time in the current state, with where it is on the line below; any other pane shows only where it is. Where a pane is reads like a starship prompt: in a git repo, the branch (or `HEAD`) followed by status markers, rechecked every 5 seconds while the sidebar is shown; elsewhere, its directory with each parent cut to one letter, from home when under it (`~/dev/simul/examples` is `/d/s/examples`, and home is `~`). The text filter matches session and window names, where panes are, pane commands, and agent details. That is the `all` view; `f` switches to the `status` view, which lists only agents, without session or window headers, ordered `blocked`, `asking`, `failed`, `done`, `working`, `idle`, then `unknown`, and within a state, the one in it longest first. Under each agent it shows the session and window, then where the agent is. While the sidebar isn't focused, it highlights the active pane of its own window. Session names are gold like tmux's status-left; the sidebar's own window and the highlighted row are bold, and everything else is dimmed; state symbols keep their colors. Focusing the sidebar keeps that row, or selects the first row if none was highlighted.

The sidebar repeats tmux's session name and window list, so while it is shown simul cuts the first status line (`status-format[0]`) down to `status-right` and restores it when you toggle the sidebar off. Further status lines, such as `status-format[1]`, keep running. Set `hide_status = false` under `[sidebar]` to keep the line as is.

| Symbol | State     | Meaning                                            |
| ------ | --------- | -------------------------------------------------- |
| `◐`    | `working` | The agent is running                               |
| `⊘`    | `blocked` | The agent is waiting for your approval             |
| `?`    | `asking`  | The agent asked you a question (Kiro only)         |
| `●`    | `done`    | Finished, and you haven't looked yet               |
| `✗`    | `failed`  | The turn ended with an error, such as a rate limit |
| `○`    | `idle`    | Waiting for input                                  |
| `∅`    | `unknown` | Not yet classified                                 |

`asking` and `failed` stay until your next prompt, even after you look at the pane.

Git status markers follow the branch in this order:

| Marker      | Meaning                                       |
| ----------- | --------------------------------------------- |
| `!`         | Merge conflicts                               |
| `⌃` `⌄` `⇕` | Ahead of, behind, or diverged from upstream   |
| `+`         | Staged changes                                |
| `-`         | Deleted files                                 |
| `*`         | Modified files                                |
| `?`         | Untracked files                               |
| `≡`         | Stashed changes                               |

When an agent becomes `blocked`, `asking`, `done`, or `failed` in a pane you are not looking at, simul shows a message in the tmux status line. Desktop notifications and the terminal bell are off by default; turn them on under `[notify]`.

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
hide_status = true  # hide the session and windows from tmux's status line while the sidebar is shown

[notify]
enabled = true   # tmux status-line messages
dedupe_secs = 10
desktop = false  # macOS, Linux D-Bus, or WSL toast notifications
bell = false     # ring the terminal bell

[kiro]
approval_patterns = []  # extra text that marks Kiro's approval prompt on screen
```

Kiro has no hook for approval prompts, so simul reads them from Kiro's session log (`$KIRO_HOME/sessions`, else `~/.kiro/sessions`). The same log shows when a turn ends, is interrupted, fails, or asks you a question. When the log can't be read, simul falls back to reading the bottom of a busy Kiro pane. That scrape recognizes the classic `Allow this action? ... [y/n/t]:` prompt. If your Kiro shows a different approval dialog, add a line of text unique to it to `approval_patterns`. Pick text that disappears once you answer: a pattern still in the bottom 15 lines keeps the pane `blocked`.

The daemon log is next to its socket: `$XDG_RUNTIME_DIR/simul/<hash>.log`, or `/tmp/simul-$UID/<hash>.log`.

## License

MIT, see `LICENSE`.
