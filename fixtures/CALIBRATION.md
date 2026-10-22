# Phase 0 calibration results

Recorded 2026-10-21 on macOS (home machine), tmux 3.8, Claude Code 2.1.307, using a scratch tmux server and `claude --settings <recorder>`. Paths are sanitized (`$HOME` becomes `/home/user`, the scratch repo becomes `/home/user/src/scratch`).

## Claude Code

| Question | Result |
| --- | --- |
| `$TMUX_PANE` reaches hooks | **Verified.** All hooks saw `TMUX_PANE=%0` and `TMUX=<socket>,<pid>,<session>`. The hook's `$PPID` is the pane's `pane_pid` (the claude process). No process-tree fallback needed. |
| `pane_current_command` | The **version string** (`2.1.307`), because the binary is a versioned file. `ps -o comm` on `pane_pid` shows `claude`. |
| Permission detection | `Notification` with `notification_type: "permission_prompt"`. `idle_prompt` (about 60 s idle) carries `message: "Claude is waiting for your input"`. |
| Permission notification delay | About **6 s** after the approval prompt renders. Answering faster than that sends no Notification at all. |
| Approve key | `1` (single key, no Enter). |
| Deny key | `3` (single key). `Escape` also rejects. |
| Interrupt key | `Escape`. |
| Deny / interrupt hooks | **Neither fires `Stop` or `PostToolUse`.** The last event stays `PreToolUse` or `UserPromptSubmit`, so a hook-only state machine sticks at `working`. |

### Screen signatures (bottom of pane)

- **Approval:** a `Do you want to ...?` line followed by numbered options (`❯ 1. Yes` ... `3. No`) and `Esc to cancel · Tab to amend`.
- **Working:** a spinner line, `<glyph> <Verb>… (<n>s · ...)`, above the input box. It is present both while thinking (`working.txt`) and while a tool runs (`working-tool.txt`). There is no "esc to interrupt" hint in this version.
- **Finished turn:** `✻ <Verb> for <n>s · done <time>` and no spinner line.
- **Folder trust:** `Enter to confirm · Esc to cancel` with no `Do you want to` line. It appears before `SessionStart` fires.

### Consequence for the design

Because deny and interrupt emit no hook, the daemon screen-scrapes Claude panes the same way the handoff planned for Kiro. While a pane has been `working` or `blocked` with no hook event for `blocked_threshold_ms`, it scrapes once per second:

- the approval signature gives `blocked`, which also covers the 6 s Notification delay
- the spinner gives `working`
- neither gives `idle`

## Kiro CLI

Deferred, unverified. Kiro is not installed on the home machine, and work fixtures must not enter this public repo.

## Files

- `claude/hooks/<Event>.json`: one payload per event, plus both Notification variants.
- `claude/session.jsonl`: the full ordered session as shim envelopes. Sequence: start, approve a Write, deny a Write with `3`, deny with `Escape`, interrupt a reply, two Bash runs, idle notification, `/exit`.
- `claude/screens/*.txt`: `capture-pane -p` output for each signature above.
