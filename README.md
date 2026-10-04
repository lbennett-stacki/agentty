# agentically

A tmux dashboard that lists every live Claude Code session on this machine and shows what each one is actually doing. Press Enter on a row to jump into the real session. agentically only reads Claude Code's files. It never writes them, installs no hooks, and does not wrap or re-implement Claude Code.

```
 agentically  5 live · 1 needs you · 2 parked                                       ? help
 webapp ──────────────────────────────────────────────────────────────────────────────── 3
›▲ Fix token refresh race                          feat/auth-race #4812  412k   3m
     permission prompt · Bash: Run migration against staging
 ◐ Rewrite onboarding docs                         docs/onboarding #4790  91k  18m
     waiting on 2 agents, 1 monitor
     ├─ ● pstack:poteto-agent  Audit README links     opus  Grep: broken anchors  6m
     ├─ ◉ monitor  Watch CI for PR #4790                                          14m
     └─ ✓ 4 finished
 ● Splice PR 4790 into a stack                     splice/4790          288k  42s
     Edit: stack_plan.rs
 agentically ─────────────────────────────────────────────────────────────────────────── 1
 ○ Dashboard design                                personal · main      277k   5m
     Recap: sketched the module map
```

## Why it exists

`claude agents` marks a session idle or done when its turn ends, even if the session is still waiting on a background shell, a Monitor, or background subagents. Claude Code's own registry (`~/.claude*/sessions/<pid>.json`) already records the difference. Its `shell` status means "idle except for a background Bash or Monitor", and `busy` covers "turn ended, background agents still running". agentically shows that difference and names what each session is waiting on, using the session transcript and, for bg jobs, `jobs/<id>/state.json`.

## Install

Build agentically with Rust 1.88 or later:

```sh
cargo install --path .
```

Then add two key bindings to your tmux config:

```tmux
bind a display-popup -E -w 90% -h 85% -T ' agents ' 'agentically --popup'
bind A run-shell 'tmux select-window -t "=agents" 2>/dev/null || tmux new-window -n agents agentically'
```

`prefix a` opens agentically as a popup that closes after you jump to a session. `prefix A` opens or selects a persistent `agents` window.

## Statuses

| glyph | status | meaning |
|---|---|---|
| `▲` | needs you | The registry says `waiting`: a permission prompt, a question, or a dialog. For a bg job, it can also mean the job reported a blocking question. |
| `●` | working | A turn is streaming. |
| `◐` | parked | No turn is streaming, but the session is waiting on its own background agents, shells, monitors, workflows, or a scheduled wakeup. The second line counts them. `1+` means a launch happened before the part of the transcript agentically read. |
| `?` | asked a question | Idle, and the last reply ends in a question. This is a heuristic, so it sorts first among idle rows but never ranks as needs you. |
| `○` | idle | Nothing in flight. |
| `·` | exited | A bg job that ended in the last 24 hours. |

A row's title is your label if you set one, otherwise Claude Code's `/rename` title, otherwise the session name or the AI title. The second line shows what the session is doing now: the latest tool call and its description, what it is parked on, or the latest recap.

Rows group by your own groups first, then by team, then by git repository (worktrees fold into their main repository).

## Keys

| key | action |
|---|---|
| `j` `k`, arrows | move |
| `g` `G` | go to top or bottom |
| `l`, `→`, space | expand the subagent tree |
| `h`, `←` | collapse |
| Enter | open the session: switch to its tmux pane, or open a new window that runs `claude attach <job>` for a bg job |
| tab, `p` | show the detail pane (last prompt, last reply, open tasks, pending work, ids) |
| `r` | set a label (an empty label clears it) |
| `m` | move to a group. Type a new name to create a group, or `-` to return the row to its automatic group. |
| `n` | start a new `claude` in a new tmux window, with the selected row's directory, config dir, and group |
| `/` | filter |
| `?` | help and diagnostics |
| `q`, Esc | quit |

## Context and cost

Context shows as a token count, such as `277k`, because the transcript does not record the model's context window. Cost is shown only for exited sessions, from Claude Code's own exit record. agentically has no price table, so it never estimates cost.

To get context as a percentage and live cost, have your statusline script save its input for agentically. Add this line after the script reads stdin into `$input`:

```sh
d="$HOME/.cache/agentically/statusline"; mkdir -p "$d"; sid=$(printf '%s' "$input" | jq -r '.session_id // empty'); [ -n "$sid" ] && printf '%s' "$input" >"$d/$sid.json.tmp" && mv -f "$d/$sid.json.tmp" "$d/$sid.json"
```

Claude Code runs the statusline only while a session is on screen, so these values can lag for sessions you are not looking at. agentically ignores the cached values when the transcript has newer usage.

## What it reads

agentically scans `~/.claude` and every `~/.claude-*` directory that has a `sessions/` directory, plus `$CLAUDE_CONFIG_DIR`. In each one it reads:

- `sessions/<pid>.json`, to find live sessions and their status. It checks each pid against `ps` so that stale files and reused pids are ignored.
- `projects/*/<session>.jsonl` and `subagents/`, to get titles, the current tool call, background work, branch, pull requests, and context tokens. It reads only the last 256 KB to 4 MB of each transcript, then only the bytes appended after that.
- `jobs/<id>/state.json` for bg jobs. agentically does not read the `providerEnv` field, which holds credentials.
- `tasks/<session>/`, `teams/*/config.json`, and the optional statusline cache.

It shows Bash background work by its description, never by its command line. It removes token-shaped strings from every label before display.

agentically writes only `~/.config/agentically/state.toml`, which holds your labels and groups.

## Check what it sees

`agentically --dump` prints one frame as plain text, followed by any diagnostics, and exits. A diagnostic means a file did not parse as expected, usually after a Claude Code release changed a format. The affected column is blank instead of wrong.

The design and its trade-offs are in [docs/DESIGN.md](docs/DESIGN.md).
