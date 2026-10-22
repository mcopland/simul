# simul

A tmux plugin that tracks the state of coding agents (Claude Code today, Kiro CLI planned), shows them in a sidebar, and notifies you when one is blocked or done.

Status: early development. See `fixtures/CALIBRATION.md` for verified agent behavior.

## Requirements

- tmux 3.3 or newer, with `set -g focus-events on` (without it, simul cannot tell whether you are looking at a pane)
- macOS, Linux, or WSL
- A Rust toolchain to build (prebuilt releases are planned)

## Install

```sh
cargo build --release
```

With TPM, add `set -g @plugin 'mcopland/simul'` to `tmux.conf`. Without TPM, add `run-shell /path/to/simul/simul.tmux`. Either one starts the daemon when tmux loads.

Then install the agent hooks:

```sh
target/release/simul install-hooks claude            # ~/.claude/settings.json
target/release/simul install-hooks claude --project  # ./.claude/settings.local.json
```

The merge keeps your existing hooks. If the settings file is a symlink, simul writes through it and leaves the link in place. Each run replaces simul's previous entries, which are tagged `#simul-managed`, and saves a `.simul-bak` copy first.

Check the setup with `simul doctor`.

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

States:

| State     | Meaning                                |
| --------- | -------------------------------------- |
| `working` | The agent is running                   |
| `blocked` | The agent is waiting for your approval |
| `done`    | Finished, and you haven't looked yet   |
| `idle`    | Waiting for input                      |
| `unknown` | Not yet classified                     |

Notifications fire when an agent becomes `blocked` or `done` in a pane you are not looking at.

## Config

`~/.config/simul/config.toml`, all keys optional:

```toml
blocked_threshold_ms = 1500  # quiet time before a busy pane's screen is checked

[sidebar]
width = 40

[notify]
enabled = true
dedupe_secs = 10
```

The daemon log is next to its socket: `$XDG_RUNTIME_DIR/simul/<hash>.log`, or `/tmp/simul-$UID/<hash>.log`.

## License

MIT, see `LICENSE`.
