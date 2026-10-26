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

### Live session file

Recorded 2026-10-25, Claude Code 2.1.304 to 2.1.310. Claude Code keeps `~/.claude/sessions/<pid>.json` (under `$CLAUDE_CONFIG_DIR` if set) for each running session and removes it on exit. The file is undocumented; [tmux-agent-sidebar](https://github.com/OlgierdOp/tmux-agent-sidebar) uses it the same way.

| Field | Meaning |
| --- | --- |
| `status` | `busy`, `waiting`, or `idle`. It updates after Esc, a denied prompt, or an interrupt, where no hook fires. |
| `statusUpdatedAt` | Unix ms of the last status change. |
| `tmux` | `<session>:<window id>.<pane id>`, e.g. `dev:@54.%475`. Absent outside tmux and on background sessions. |
| `kind` | `interactive` or `bg`. |
| `parkedJobId` / `jobId` | A pane whose conversation runs in a background session carries `parkedJobId`. The `bg` file with the same `jobId` holds the live status. |

The daemon reads these files every tick. A status counts only when it changed and is newer than the pane's last hook. Panes with a file are not screen-scraped. Not yet observed on screen: that `waiting` covers permission prompts and questions.

Subagent hooks carry `agent_id` and fire with the parent's `$TMUX_PANE`, even after the parent's `Stop`. They are ignored for state.

## Kiro CLI

**Unverified.** Kiro is not installed on the home machine, and work fixtures must not enter this public repo. Support was built on 2026-10-25 from public sources. Every `fixtures/kiro/` file is **synthetic**, hand-written to match these sources:

- Kiro docs: [CLI V3 hooks migration](https://kiro.dev/docs/cli/v3/hooks-migration/), [2.x reference](https://kiro.dev/docs/cli/2x-reference/)
- Captured payloads and field reports: [stablyai/orca#31577](https://github.com/stablyai/orca/issues/31577), [manaflow-ai/cmux#23685](https://github.com/manaflow-ai/cmux/pull/23685), [yangzichao/JustSessions#185](https://github.com/yangzichao/JustSessions/issues/185)
- Approval prompt text and keys: [kirodotdev/KiroCrew#23454](https://github.com/kirodotdev/KiroCrew/pull/23454), [OrchestratorInc/agent-orchestrator#6601](https://github.com/OrchestratorInc/agent-orchestrator/pull/6601)

### What the sources say

| Question | Reported answer |
| --- | --- |
| Hook location | V3 engine: standalone `~/.kiro/hooks/*.json` (`$KIRO_HOME/hooks`) and `.kiro/hooks/*.json`, run together. V2 engine (the default in kiro-cli 2.x): only `hooks` inside an agent profile, so the built-in default agent runs none. |
| File schema | `{"version": "v1", "hooks": [{name, trigger, matcher?, action: {type: "command", command}, timeout, enabled}]}` |
| Triggers | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, and `SessionEnd` (fires on `/quit`). V2 names: `agentSpawn`, `userPromptSubmit`, `preToolUse`, `postToolUse`, `stop`. No permission or notification trigger. |
| Payload | `session_id`, `hook_event_name`, `cwd`; plus `prompt`, or `tool_name`, `tool_input`, `tool_response`. `Stop` has nothing more. |
| `$TMUX_PANE` reaches hooks | Yes. The hook's parent is not the `kiro-cli` process under V3. |
| `pane_current_command` | `kiro-cli` (it runs `kiro-cli-chat` and a TUI under it). |
| Hook stdout and exit codes | `SessionStart` and `UserPromptSubmit` stdout enters the model's context. `Stop` exiting 1 makes Kiro continue. The shim prints nothing and exits 0. |
| Interrupts | Esc or Ctrl-C fires no `Stop`. |
| Approval prompt | Classic: `Allow this action? Use 't' to trust (always allow) this tool for the session. [y/n/t]:`, answered at the `>` prompt below it. `y` approves, `n` denies, `t` trusts the tool for the session. A newer TUI may show a Yes/Trust/No bar instead. |

### Known gaps

- **Esc leaves a pane `working`.** Neither a hook nor the screen corrects it until the next turn ends. Kiro writes `~/.local/share/kiro-cli/run/turn-markers/<pid>-<ms>.json` while a turn runs, and removes it at the end. That could play the role Claude's session files play, once the pid can be mapped to a pane.
- **Only `blocked` is scraped.** Kiro's idle and working screens are unrecorded, so the classifier returns `unknown` for them, and recovered Kiro panes stay `unknown` until their first hook.
- **A denied approval** keeps the pane `blocked` until Kiro's next hook.

### To check at work

Record results here, sanitized, without work fixtures:

- [ ] `kiro-cli --version`, and whether it runs the V3 engine by default or needs `--v3`
- [ ] Kiro loads `~/.kiro/hooks/simul.json` with no validation error, including the `SessionEnd` trigger
- [ ] the `timeout` unit (simul writes `500`, which outlasts the shim's 200 ms budget whether it means ms or seconds)
- [ ] hook commands run through a shell (the quoted path and the `#simul-managed` comment depend on it)
- [ ] `$TMUX_PANE` in hooks, and `pane_current_command` for a running session
- [ ] the approval screen (classic `[y/n/t]` or a TUI dialog); add a pattern to `[kiro] approval_patterns` if it's the dialog
- [ ] the approve, deny, and interrupt keys, for Phase 4

## Files

- `claude/hooks/<Event>.json`: one payload per event, plus both Notification variants.
- `claude/session.jsonl`: the full ordered session as shim envelopes. Sequence: start, approve a Write, deny a Write with `3`, deny with `Escape`, interrupt a reply, two Bash runs, idle notification, `/exit`.
- `claude/screens/*.txt`: `capture-pane -p` output for each signature above. `echoed-list-prompt.txt` is a submitted numbered list, which must not read as a selector.
- `claude/sessions/*.json`: sanitized live session files, including a parked pane and its background session.
- `kiro/hooks/<Event>.json`, `kiro/session.jsonl`, `kiro/screens/*.txt`: synthetic, see above. `session.jsonl` includes an Esc after `PreToolUse` that sends no hook. `screens/answered.txt` is a prompt already answered with `y`; `prose-mentions-prompt.txt` quotes the prompt in prose. Neither may read as blocked.
