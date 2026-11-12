# simul

A tmux plugin that tracks the state of coding agents (Claude Code and Kiro CLI), shows them in a sidebar, and notifies you when one is blocked, asking, done, or failed.

Status: early development. See `fixtures/CALIBRATION.md` for verified agent behavior.

## Requirements

- tmux 3.3 or newer, with `set -g focus-events on` (without it, simul cannot tell whether you are looking at a pane)
- `set -g mouse on` in tmux, since the sidebar is driven by clicks
- macOS, Linux, or WSL
- `curl`, `tar`, and `sha256sum` or `shasum`, to download and verify the prebuilt binary (macOS and Linux on x86_64 or arm64; WSL uses the Linux build)

## Install

With TPM, add this to `tmux.conf` and press `prefix + I`:

```tmux
set -g @plugin 'mcopland/simul'
```

When tmux loads the plugin, it downloads the release binary matching the plugin's `VERSION` into `bin/simul` in the plugin directory and checks its sha256. Then it starts the daemon. TPM keeps plugins in `~/.tmux/plugins`, or in `~/.config/tmux/plugins` when your config is `~/.config/tmux/tmux.conf`. Link the binary onto your PATH:

```sh
ln -s ~/.tmux/plugins/simul/bin/simul ~/.local/bin/simul  # or any directory on your PATH
ln -s ~/.config/tmux/plugins/simul/bin/simul ~/.local/bin/simul  # with ~/.config/tmux
```

Then install the agent hooks:

```sh
simul install-hooks claude            # ~/.claude/settings.json
simul install-hooks claude --project  # ./.claude/settings.local.json
simul install-hooks kiro              # ~/.kiro/hooks/simul.json ($KIRO_HOME/hooks if set)
simul install-hooks kiro --project    # ./.kiro/hooks/simul.json, added to .git/info/exclude
```

Hooks record the path you ran `simul` from, so run it through a path that won't move, such as the symlink above. Plugin updates replace the binary at the same path. Start new agent sessions to pick up the hooks.

For Claude, the merge keeps your existing hooks. If the settings file is a symlink, simul writes through it and leaves the link in place. Each run replaces simul's previous entries, which are tagged `#simul-managed`. The first run saves the file as it was in a `.simul-bak` copy, and later runs keep that copy.

For Kiro, simul owns the whole `simul.json` hook file and rewrites it on each run. It uses the standalone hook files of Kiro CLI's V3 engine; the V2 engine reads hooks only from agent profiles, which simul does not edit. Kiro's hooks were built from its public docs and have not yet been checked against a live Kiro session. Its session files were observed with `kiro-cli 2.34.2 --v3`. See `fixtures/CALIBRATION.md`.

Check the setup with `simul doctor`.

### Update

Update with TPM (`prefix + U`); nothing else is needed. The daemon watches the plugin's `VERSION` file, and the OS tells it when the update changes it, so it doesn't poll. It then downloads and verifies the new release, restarts itself on it, and restarts each sidebar in place. Agent states carry over from what tmux and the agents report, as after any restart. Loading the plugin, when tmux starts or you reload its config, also replaces a running daemon of another version or binary.

Daemons from 0.5.0 and earlier don't do this, so the first update past 0.5.0 needs one manual restart: run `pkill -f 'simul daemon'` and reload your tmux config.

To try a local build, point the plugin at it and reload your tmux config; the daemon and sidebars switch to it. After each rebuild, reload again:

```tmux
set -g @simul-bin ~/dev/simul/target/release/simul
```

Remove the line (or `set -gu @simul-bin`) and reload again to go back to the release.

### Build from source

Without TPM, or on a platform with no prebuilt binary, build with a Rust toolchain:

```sh
cargo build --release
ln -s "$PWD/target/release/simul" ~/.cargo/bin/simul  # or any directory on your PATH
```

Then add `run-shell /path/to/simul/simul.tmux` to `tmux.conf`. When `target/release/simul` exists, the plugin uses it and skips the download, and rebuilds update the symlink in place. A build from source doesn't update itself; after a rebuild, reload your tmux config.

### Uninstall

Remove the hooks first, while `simul` is still installed. Only entries tagged `#simul-managed` are removed:

```sh
simul uninstall-hooks claude            # ~/.claude/settings.json
simul uninstall-hooks claude --project  # ./.claude/settings.local.json, in each project you installed into
simul uninstall-hooks kiro              # deletes ~/.kiro/hooks/simul.json
simul uninstall-hooks kiro --project    # deletes ./.kiro/hooks/simul.json and its .git/info/exclude line
```

Toggle the sidebar off so tmux gets its status line back. Then delete the `set -g @plugin 'mcopland/simul'` line from `tmux.conf` and press `prefix + alt + u`, which makes TPM delete the plugin directory. Without TPM, delete the `run-shell` line and the checkout instead. Finally, stop the daemon and remove what is left:

```sh
pkill -f 'simul daemon'
rm ~/.local/bin/simul                   # the PATH symlink you made at install
rm -r "$XDG_RUNTIME_DIR/simul"          # daemon socket and log, if XDG_RUNTIME_DIR is set
rm -r "/tmp/simul-$(id -u)"             # otherwise
rm -r ~/.config/simul                   # config, if you made one
```

The Claude settings backup, `settings.json.simul-bak`, stays next to the settings file; delete it once you no longer need it. The toggle key stays bound until tmux restarts, or until you unbind it with `tmux unbind-key a` (or your `@simul-key`).

## Use

| Command             | What it does                                                             |
| ------------------- | ------------------------------------------------------------------------ |
| `prefix + a`        | Toggles the sidebar in every window. Set `@simul-key` to change the key. |
| `simul ls [--json]` | Lists tracked agents.                                                    |
| `simul doctor`      | Checks tmux, the daemon, hooks, and notifications.                       |

The sidebar is for the mouse. It never keeps focus: tmux hands focus straight back to the pane you were in.

| Mouse                    | Action                                      |
| ------------------------ | ------------------------------------------- |
| Click a window or pane   | Jump to it                                  |
| Click a session          | Collapse or expand the session              |
| Click the `view:` footer | Switch between the `all` and `status` views |
| Wheel                    | Scroll                                      |

The sidebar takes a quarter of its window's width and returns to it after you drag its border or resize the terminal. It lists every session, every `window_index:window_name` in it, and every pane in each window, whether or not an agent runs there. An agent pane is one line: a state symbol, where it is, and time in the current state (the agent name stands in when where it is is unknown); any other pane shows only where it is. Where a pane is reads like a starship prompt: in a git repo, the branch (or `HEAD`) followed by status markers, rechecked every 5 seconds while the sidebar is shown; elsewhere, its directory with each parent cut to one letter, from home when under it (`~/dev/simul/examples` is `/d/s/examples`, and home is `~`). That is the `all` view; the footer switches to the `status` view, which lists only agents, without session or window headers, ordered `blocked`, `asking`, `failed`, `done`, `working`, `idle`, then `unknown`, and within a state, the one in it longest first. Under each agent it shows the session and window. Rows come in three tones. A pane where something runs (any agent, or a process running, open, or newly finished) is drawn at full color, and so are its window and session; a pane at a shell prompt, a window where nothing runs, and a session where nothing runs are dimmed. The pane you are in, the active pane of the sidebar's own window, is bold, with its window and session. Session names are in the bright gold of tmux's status-left, or a dim gold when nothing runs there; state symbols keep their colors.

A collapsed session shrinks to its header, `▸`, and a count of its agents in each state, in the `status` view's order, then of its finished and running panes (`me ▸ ⊘1 ?1 ●2 ■1 ▶2`), so a blocked or asking agent or an unseen finish stays visible. Collapsing applies to the sidebar in every window. simul remembers it in the tmux session option `@simul-collapsed`, so it lasts through toggling the sidebar and restarting the daemon, and goes away with the session. Only the `all` view folds sessions.

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

A pane without an agent gets a symbol while its foreground process isn't a shell at its prompt. Running and finished panes also show the time in that state.

| Symbol | Meaning                                                           |
| ------ | ----------------------------------------------------------------- |
| `▶`    | A command is running, such as a build, a script, or a server      |
| `■`    | It finished while you weren't looking, and you haven't looked yet |
| `▷`    | An interactive program is open, such as an editor, pager, or ssh  |

`■` clears when you look at the pane, like `done`; a command that ends while you watch it leaves nothing. simul checks once a second while the sidebar is shown, and every 10 seconds while it is hidden (`hidden_check_secs`), to keep its idle cost low; a command that starts and ends between two checks may not show, and a time can be off by up to the interval. It can't see exit codes, so `■` means ended, not succeeded. A script counts as running when its interpreter differs from the pane's shell, so `./build.sh` (a bash script) shows in a zsh pane, but a bash script in a bash pane reads as idle, as does any shell-named interpreter in a pane started as `sh`. For the same reason, a shell started from another shell, such as `bash` typed at a `zsh` prompt, counts as running. If your shell's startup file `exec`s another shell, set tmux's `default-shell` (or `default-command`) to that shell, or its panes always show `▶`.

Git status markers follow the branch in this order:

| Marker      | Meaning                                     |
| ----------- | ------------------------------------------- |
| `!`         | Merge conflicts                             |
| `⌃` `⌄` `⇕` | Ahead of, behind, or diverged from upstream |
| `+`         | Staged changes                              |
| `-`         | Deleted files                               |
| `*`         | Modified files                              |
| `?`         | Untracked files                             |
| `≡`         | Stashed changes                             |

When an agent becomes `blocked`, `asking`, `done`, or `failed` in a pane you are not looking at, simul shows a message in the tmux status line; a pane without an agent shows only its sidebar symbol. Desktop notifications and the terminal bell are off by default; turn them on under `[notify]`.

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
hide_status = true  # hide the session and windows from tmux's status line while the sidebar is shown

[notify]
enabled = true   # tmux status-line messages
dedupe_secs = 10
desktop = false  # macOS, Linux D-Bus, or WSL toast notifications
bell = false     # ring the terminal bell

[kiro]
approval_patterns = []  # extra text that marks Kiro's approval prompt on screen

[panes]
hidden_check_secs = 10  # how often panes' processes are checked while the sidebar is hidden
# commands shown as ▷, not ▶; a list here replaces the default
interactive = ["vi", "vim", "nvim", "hx", "helix", "emacs", "nano", "micro", "kak", "less", "more", "man", "ssh", "mosh", "htop", "btop", "top", "watch", "lazygit", "tig"]
```

Kiro has no hook for approval prompts, so simul reads them from Kiro's session log (`$KIRO_HOME/sessions`, else `~/.kiro/sessions`). The same log shows when a turn ends, is interrupted, fails, or asks you a question. When the log can't be read, simul falls back to reading the bottom of a busy Kiro pane. That scrape recognizes the classic `Allow this action? ... [y/n/t]:` prompt. If your Kiro shows a different approval dialog, add a line of text unique to it to `approval_patterns`. Pick text that disappears once you answer: a pattern still in the bottom 15 lines keeps the pane `blocked`.

The daemon log is next to its socket: `$XDG_RUNTIME_DIR/simul/<hash>.log`, or `/tmp/simul-$UID/<hash>.log`.

## License

MIT, see `LICENSE`.
