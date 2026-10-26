# agentty

A tmux dashboard that lists every live Claude Code session on this machine and shows what each one is actually doing. Press Enter on a row to jump into the real session. agentty only reads Claude Code's files. It never writes them, installs no hooks, and does not wrap or re-implement Claude Code.

```
 agentty  5 live · 1 needs you · 2 parked                                       ? help
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
 agentty ─────────────────────────────────────────────────────────────────────────── 1
 ○ Dashboard design                                personal · main      277k   5m
     Recap: sketched the module map
```

## Why it exists

`claude agents` marks a session idle or done when its turn ends, even if the session is still waiting on a background shell, a Monitor, or background subagents. Claude Code's own registry (`~/.claude*/sessions/<pid>.json`) already records the difference. Its `shell` status means "idle except for a background Bash or Monitor", and `busy` covers "turn ended, background agents still running". agentty shows that difference and names what each session is waiting on, using the session transcript and, for bg jobs, `jobs/<id>/state.json`.

## Install

Build agentty with Rust 1.88 or later:

```sh
cargo install --path .
```

Then add two key bindings to your tmux config:

```tmux
bind a display-popup -E -d '#{pane_current_path}' -w 90% -h 85% -T ' agents ' 'agentty --popup'
bind A run-shell 'tmux select-window -t "=agents" 2>/dev/null || tmux new-window -n agents agentty'
```

`prefix a` opens agentty as a popup that closes after you jump to a session. `prefix A` opens or selects a persistent `agents` window.

## Statuses

| glyph | status | meaning |
|---|---|---|
| `▲` | needs you | The registry says `waiting`: a permission prompt, a question, or a dialog. For a bg job, it can also mean the job reported a blocking question. |
| `●` | working | A turn is streaming. |
| `◐` | parked | No turn is streaming, but the session is waiting on its own background agents, shells, monitors, workflows, or a scheduled wakeup. The second line counts them. `1+` means a launch happened before the part of the transcript agentty read. |
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
| Enter | open the session: switch to its tmux pane, or, for a bg job, run `claude attach <job>` right here in agentty's place. Ctrl+Z detaches and brings agentty back (← opens Claude Code's own agent view instead). Attaching works outside tmux too. |
| tab, `p` | show the detail pane (last prompt, last reply, open tasks, pending work, ids) |
| `r` | set a label (an empty label clears it) |
| `m` | move to a group. Type a new name to create a group, or `-` to return the row to its automatic group. |
| `n` | start a new background `claude` (`claude --bg`) in the selected row's directory and group, and attach to it here. Ctrl+Z comes back. In a workspace it starts on the workspace account with the most usage left, and tab picks another. |
| `w` | show the next workspace, then all sessions |
| `/` | filter |
| `?` | help and diagnostics |
| `q`, Esc | quit |

## Workspaces and accounts

If you use several Claude accounts, each with its own config dir, a workspace limits agentty to one directory tree and its accounts. Define them in `~/.config/agentty/config.toml`:

```toml
[accounts]
jnj = "~/.claude"
personal = "~/.claude-personal"
wukong = "~/.claude-wukong"

[workspaces.jnj]
root = "~/workspace/jnj"
accounts = ["jnj"]

[workspaces.blade]
root = "~/workspace/blade"
accounts = ["personal", "wukong"]
```

agentty opens on the workspace whose `root` holds the directory you start it in, and shows only sessions whose working directory is under that root. `--workspace <name>` picks one, `--all` shows everything, and `w` switches while it runs. Without this file, agentty shows every session.

The header shows each workspace account's 5-hour and 7-day usage, such as `personal 5h 12% 7d 52%  wukong 7d ↺`. `↺` means the window has reset since agentty last saw it. A dim age such as `(1d)` means the numbers are that old. `n` starts new sessions on the account with the most room: one with no full window, then the least 7-day use, then the least 5-hour use. That account's name is bold.

Usage comes from two places. Claude Code passes it to your statusline script, so the statusline tee below keeps it current while any session of that account is on screen. Claude Code also caches it in the account's `.claude.json` whenever you open `/usage`. agentty uses whichever is newer.

## Context and cost

Context shows as a token count, such as `277k`, because the transcript does not record the model's context window. Cost is shown only for exited sessions, from Claude Code's own exit record. agentty has no price table, so it never estimates cost.

To get context as a percentage and live cost, have your statusline script save its input for agentty. Add this line after the script reads stdin into `$input`:

```sh
d="$HOME/.cache/agentty/statusline"; mkdir -p "$d"; sid=$(printf '%s' "$input" | jq -r '.session_id // empty'); [ -n "$sid" ] && printf '%s' "$input" >"$d/$sid.json.tmp" && mv -f "$d/$sid.json.tmp" "$d/$sid.json"
```

Claude Code runs the statusline only while a session is on screen, so these values can lag for sessions you are not looking at. agentty ignores the cached values when the transcript has newer usage.

## What it reads

agentty scans `~/.claude` and every `~/.claude-*` directory that has a `sessions/` directory, plus `$CLAUDE_CONFIG_DIR`. In each one it reads:

- `sessions/<pid>.json`, to find live sessions and their status. It checks each pid against `ps` so that stale files and reused pids are ignored.
- `projects/*/<session>.jsonl` and `subagents/`, to get titles, the current tool call, background work, branch, pull requests, and context tokens. It reads only the last 256 KB to 4 MB of each transcript, then only the bytes appended after that.
- `jobs/<id>/state.json` for bg jobs. agentty does not read the `providerEnv` field, which holds credentials.
- `tasks/<session>/`, `teams/*/config.json`, and the optional statusline cache.
- The usage cache (`cachedUsageUtilization`) in each account's `.claude.json` (`~/.claude.json` for `~/.claude`). It reads no other field of that file.

It shows Bash background work by its description, never by its command line. It removes token-shaped strings from every label before display.

agentty writes only `~/.config/agentty/state.toml`, which holds your labels and groups. It reads `~/.config/agentty/config.toml` but never writes it.

## Check what it sees

`agentty --dump` prints one frame as plain text, followed by any diagnostics, and exits. A diagnostic means a file did not parse as expected, usually after a Claude Code release changed a format. The affected column is blank instead of wrong.

The design and its trade-offs are in [docs/DESIGN.md](docs/DESIGN.md).
