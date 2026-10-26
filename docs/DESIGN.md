# agentty: design

A read-only, tmux-native dashboard over every live Claude Code session on this Mac. This document explains the shape of the code and why it has that shape. It is the synthesized result of a three-candidate architecture arena (`.work/arch/`, gitignored): candidate X (opus) is the base, with grafts from Y (fable) and Z (sonnet) listed under [Synthesis decision](#synthesis-decision).

## Problem

Show every live Claude Code session in one grouped list with a status that tells the truth: needs you, working, parked on its own background work, idle, or recently exited. Then hand off to real Claude Code (switch to the pane, or `claude attach`). Everything is read-only over files that Claude writes for itself, so the files' properties decide the shape:

- The registry (`sessions/<pid>.json`) is authoritative for liveness and coarse status. Its records are torn on read (no atomic rename) and keyed by a pid whose `sessionId` can change.
- Transcripts are append-only, 1 to 17 MB each, and are the only source that separates Working from Parked.
- Subagent sidecars are flat. Their tree comes from `meta.parentAgentId`, and depth-2 completions land in the parent agent's file.
- bg `state.json` holds live credentials in `providerEnv`. Shell labels and Bash commands can hold inline secrets.
- Several config dirs (accounts) exist, and nothing on disk says which one a cwd belongs to. The user says so in `config.toml`: each workspace is a root directory plus the accounts used under it.
- SPEC binds us to tail reads (never full scans), no pricing or window tables, no writes to Claude files, no hooks, and no PTY.

## What the user sees

```
 agentty  7 live · 1 needs you · 1 parked                                      ? help
 jnj-web ─────────────────────────────────────────────────────────────────────────────── 4
 ▲ auth-race  Fix token refresh race in session middleware   feat/auth-race #4812  412k   3m
     permission prompt · Bash: Run migration against staging
 ● Splice PR 4790 into a reviewable stack                     splice/4790          288k  42s
     Edit: stack_plan.rs
 ◐ docs sweep  Rewrite onboarding docs                  ⇄    docs/onboarding #4790  91k  18m
     waiting on 2 agents, 1 monitor
     ├─ ● pstack:poteto-agent  Audit README links              opus     6m  Grep: broken anchors
     │  ├─ ● Explore  Find dead links in guides                 sonnet   1m  Read: guide.md
     │  └─ ✓ 2 finished
     ├─ ◉ monitor  Watch CI for PR #4790                                14m
     └─ ✓ 4 finished
 ○ fees  Rework fee rounding                                   bg · main             64k   1h
     Fee logic done; client-side & policy next
 agentty ─────────────────────────────────────────────────────────────────────────── 1
 ? Claude agents TUI replacement                              personal · main  277k $3.41  5m
     Want me to wire the statusline tee now?
 finished · 24h ──────────────────────────────────────────────────────────────────────── 1
 · pm-team-logos  Team logos for PM board                     bg · pm-team-logos  $2.17 2h
     done
 j/k move  l expand  ⏎ open  tab detail  r label  m move  n new  / filter  q quit
```

Glyphs: `▲` NeedsYou (red); `●` Working (green); `◐` Parked (yellow); `?` Idle where the last turn ended on a question (a heuristic, so it stays in the Idle rank and only sorts first within it); `○` Idle; `·` Exited (dim). `⇄` marks Remote Control. `personal` is the config-dir tag; the default `~/.claude` has no tag. Context shows `NN%` only when the statusline tee exists, otherwise absolute tokens. Cost shows only from an authoritative source (the statusline tee, or `cost-state` on exit). `1+ shell` appears when the registry says `shell` but the launch predates our tail window.

Keys: `j`/`k`/`↓`/`↑`/`g`/`G` move; `l`/`→`/`space` expand, `h`/`←` collapse; `Enter` open; `tab`/`p` detail; `r` label; `m` move to group (`-` returns to the automatic group); `n` new bg `claude` in the row's cwd, config dir and group, with an optional name, attached in place; `/` filter; `?` help plus diagnostics; `q`/`Esc` quit.

Popup flow (`bind a display-popup -E -w 90% -h 85% -T ' agents ' 'agentty --popup'`): a cold start reads only registries and tail windows within a byte budget. `Enter` on a pane row emits `Effect::Jump`. The shell runs `tmux list-panes -t %N` as the existence check, then `switch-client -t %N`, and feeds back `Outcome::Jumped`. In popup mode `update` answers with `Effect::Quit`, the process exits, and `-E` closes the popup over the target pane. A failed jump shows a flash and keeps the popup open.

## Shape

### Module map

Eleven flat files. Input reaches pixels in at most three hops.

| module | owns | yields |
|---|---|---|
| `model` | the vocabulary: ids, `Status`, `Snapshot`/`Row`/`Tree`, `SafeText`, `Diagnostic`, `Anchor`, `Op`, `Effect`, `Outcome` | types only |
| `registry` | `sessions/<pid>.json`, `ps` output parsing, config-dir discovery | `RegEntry`, `parse_ps` |
| `jobs` | `jobs/<id>/state.json` | `Job`, `FanItem` (no `providerEnv` field exists) |
| `transcript` | main and sidecar JSONL: cursor, window policy, reducer | `Transcript`, `Facts`, `Notice` |
| `subagents` | `subagents/` metas, sidecar cursors, the agent-state rule | `AgentMeta`, `Subagents`, `agent_state -> NodeState` |
| `sidecars` | tasks, statusline tee, team configs, repo roots | `Task`, `Statusline`, `Team`, `repo_root` |
| `world` | every cursor and cache; stat-gated refresh; `Host`; GC | `SessionView`s, `ExitedView`s, `Diagnostic`s |
| `derive` | every product decision: status table, parked counts, title, now-line, tree, auto-group, Enter target | `Snapshot` |
| `user_state` | `~/.config/agentty/state.toml`, `rebind` | `UserState`, `Store`, `Op`s |
| `config` | `~/.config/agentty/config.toml` (user-written, read once at start) | `Config`, `Account`, `Workspace` |
| `ui` | view state, pure `layout -> Vec<Line>`, pure key reducer, renderer | `Vec<Effect>`, frames |
| `main` | the loop, feeder threads, `execute(Effect)`, `TmuxCmd::argv` | nothing |

Data flow: files → parser module (private `Wire*` struct → domain type) → `World` caches → `SessionView` (borrowed refs, defined in `derive`) → `derive::snapshot` (pure) → `Snapshot` → `ui::layout` (pure, applies user state) → `Vec<Line>` → `ui::render`. Keys → `ui::key_of` → `ui::update` (pure) → `Effect` → `main::execute` → `Outcome` → `ui::update`.

`derive` never imports `world`. `World` builds `derive::SessionView`s and hands them to `derive::snapshot` as iterators, so the pure core has no dependency on the IO owner.

### Identity: three keys, one per job

- **`RowKey::Live { dir, pid, proc_start }`** is the row's identity. A sessionId can change under a pid on `/clear`, `/resume` or fork, and a pid can be reused, but this triple cannot change. The cursor and expansion in `App` key on it, so a re-sort never moves the cursor to another session. `RowKey::Exited { dir, job }` covers exited bg jobs, whose dir outlives the process.
- **The transcript path** is the key of a fold (`World.transcripts: HashMap<PathBuf, Tracked>`), resolved from `(dir, sessionId)` by `linkScanPath` for bg, else by a glob. Two live holders of one sessionId share one fold.
- **`UserKey`** keys labels and groups. bg rows use `Job(JobId)`, which survives sessionId changes and process exit, so a label carries into the Exited row. Interactive rows use `Session(SessionId)`, as SPEC asks, and their entry carries an `Anchor { dir, pid, proc_start }`. `user_state::rebind` compares the anchors on disk with the live rows each refresh. When a process now holds a sessionId that has no entry, but some entry carries that process's anchor, it emits `Op::Rekey`, so a label follows `/clear` even when the dashboard was not running at the time.

All ids are newtypes. `ProcStart::new` trims, so an untrimmed `ps` value cannot be compared against `procStart` (gotcha 7). `PaneId::parse` keeps only `%N`, so stale session and window parts cannot be used (gotcha 12).

### Status: one enum, precedence by type

`Status { NeedsYou(Need), Working, Parked(Parked), Idle { asked_question }, Exited(JobOutcome) }`. `rank()` maps each variant to a fieldless `Rank`, whose derived `Ord` is the precedence. `sort_key()` adds one sub-rank so Idle rows that ended on a question sort first within Idle. Rows sort by `(sort_key, since)`. `derive::status` is one documented decision table:

- registry `waiting(w)` → `NeedsYou(need_of(w))`. `WaitingFor::Permission` becomes `Need::Permission { tool }`, where `tool` is the dangling tool_use's name if the transcript shows one. Every other reason becomes `Need::Waiting(w)`.
- registry `busy`: an open turn → `Working`; otherwise `Parked`.
- registry `shell` → `Parked`.
- registry `idle`: a blocked bg job's `needs` → `NeedsYou(JobQuestion)`; a future wakeup → `Parked`; otherwise `Idle { asked_question }`.

The registry decides *whether* a session is parked. The transcript or `fan` decides *on what*. `job.state` never decides liveness (gotcha 4).

A turn closes on `system/turn_duration`, which is authoritative and carries `pendingBackgroundAgentCount`. It also closes on the final assistant block with `stop_reason:"end_turn"` or on `system/stop_hook_summary`, because `sdk-cli` sessions (Remote Control bridges) never write `turn_duration`. A soft close never overwrites a recorded `turn_duration`. A later assistant block re-opens the turn, which covers a Stop hook that asks the model to continue.

`Count { Exact(n), AtLeast(n) }` keeps the design honest about tail reads. `Parked::new` returns `Option` and rejects "parked on nothing", so `derive::parked` applies the registry fallbacks first: `shell` with no visible launch → `shells: AtLeast(1)`, `busy` with a closed turn and no evidence → `agents: AtLeast(1)`. Agents take `max(fold open agents, live tree agents, turn_duration.pendingBackgroundAgentCount)`, because the last of these also counts depth-2 agents.

`RegStatus::Waiting(WaitingFor)` folds the reason into the status, with `WaitingFor::Unspecified` for a `waiting` record that has no `waitingFor` key. Waiting-without-reason and reason-without-waiting are unrepresentable.

### Subagent tree: states that match the grounding one to one

`NodeState { Running, Finished(TaskOutcome), Killed, Orphaned }` with `TaskOutcome { Completed, Failed, Killed, Stopped, AwaitingNotice }`. `transcript::Notice { at, outcome: Option<TaskOutcome> }` carries the timestamp the agent-state rule needs, and `None` is a monitor event (monitor events have no `<status>`, gotcha 21). `subagents::agent_state` is one pure eight-rule function. `Facts.tail_looks_finished` feeds rule 4 (a text block with `stop_reason:"end_turn"` or a handback with no later activity → `AwaitingNotice`). Gotcha 26 says `stop_reason` is null on half the sidecar blocks, but those are the tool_use blocks: across 626 sidecars, 576 of 586 final text blocks carry `end_turn`, while 2687 of 2902 mid-turn narration texts carry none, so the flag separates "done" from "about to call a tool".

### Parsing boundaries

| file | parsed in | domain type | notes |
|---|---|---|---|
| `sessions/<pid>.json` | `registry::parse` | `RegEntry` | retry once after 25 ms on a torn read; unknown status → `RegStatus::Other` |
| `jobs/<id>/state.json` | `jobs::parse` | `Job` | `WireJob` has no `providerEnv`; errors are a `Problem` category only, never serde's Display (which can quote values) |
| `projects/*/<sid>.jsonl`, `subagents/agent-*.jsonl` | `transcript::Facts::apply_line` | `Facts` | one flat `WireLine` with every field optional; `file-history-*` rejected by a prefix check before parsing; `WireToolInput` has no `command` field |
| `subagents/agent-*.meta.json` | `subagents::parse_meta` | `AgentMeta` | re-parsed when its mtime moves (`stoppedByUser` is merged in later) |
| `tasks/<sid>/N.json`, statusline tee, `teams/*/config.json` | `sidecars::parse_*` | `Task`, `Statusline`, `Team` | |
| `.git` walk | `sidecars::repo_root` | `PathBuf` | no subprocess; worktrees fold to the main repo |
| `ps -o pid=,lstart=` | `registry::parse_ps` | `HashMap<Pid, ProcStart>` | trimmed; a missing pid is dead |
| `tmux list-panes -a` | `world::parse_panes` | `Vec<PaneInfo>` | absence from this list is the only existence test (gotcha 31) |

Every string from a Claude file that reaches the screen is a `SafeText`, and the only way to build one from file content is `SafeText::redact`. The renderer accepts `&SafeText`, so the security rule is enforced by the compiler. Bash commands are never deserialized at all.

Every parse failure is a typed `Diagnostic { source, problem, path }`, where `Problem::UnknownValue { field, value }` names the enum value this build does not know. The header shows `⚠ n`, and `?` lists them, so a Claude release that blanks a column is visible instead of silent.

### Incremental reading and refresh

- **One owner thread.** `World`, `Store`, `App` and `Snapshot` live on the main thread, so there are no locks and no `Arc`. Two feeder threads only send `Msg`: crossterm input and the notify callback.
- **Watch only low-volume dirs.** `World::watch_roots` returns `<dir>/sessions`, `<dir>/jobs`, `<dir>/tasks` per config dir plus the statusline cache dir, each watched recursively. `projects/` is not watched: an active session appends many lines per second and would ring the bell continuously. Transcripts and sidecars are stat-polled on the 1 s tick. Doorbells are coalesced to one refresh per 250 ms.
- **`Wake` decides the depth of a refresh.** `Wake::Doorbell` re-stats registries, jobs, tasks and the statusline cache, and runs `ps` only for pids not seen before. `Wake::Tick` does everything: `ps` for every pid, the tmux pane snapshot, and a stat of every tracked transcript and sidecar. notify is a doorbell and `stat` is the truth, so a dropped or coalesced event costs latency, never correctness.
- **`Host` isolates the impure edges.** `Host { now, proc_starts, panes }` is the only path to the clock, `ps` and `tmux list-panes`. A `FakeHost` makes `World::refresh` a pure function of a fixture tree. When `ps` fails, `World` keeps every candidate alive and records a `Ps / Unavailable` diagnostic. A transient failure never blanks the list.
- **Transcript cursor.** `Transcript { offset, partial, facts }`. `pump()` reads `[offset, len)`, holds back a trailing partial line, and folds each complete line through the pure reducer `Facts::apply_line`. A changed inode or a shrunk length means the file was replaced, so the cursor reopens from a fresh window.
- **Window on first sight (SPEC: tail only).** Read a 256 KB tail and look for a `"type":"last-prompt"` substring. That record starts every metadata block, and the furthest one observed from EOF was 310 KB. If the tail has none, retry at 1 MB, then at 4 MB. The fold starts at the first complete line of the smallest tail that holds the block, not at the block itself: the block is re-appended between turns, so on 5 of 9 live transcripts it was the last thing in the file and a fold from it saw no conversation. `Facts.horizon` records where knowledge starts. Derive turns that into `Count::AtLeast`, and `subagents::agent_state` uses it for "finished before our window".
- **A read budget bounds the frame.** `ReadBudget { bytes_per_refresh }` (default 4 MB) caps what first-sight windows may read in one refresh. A file that does not fit gets a `Deferred` diagnostic and opens on a later refresh. This keeps IO on the UI thread tolerable on a cold start with 20 large files. If measurement shows frame stalls anyway, moving `World` behind a channel is mechanical because the design shares nothing.
- **Subagents.** The dir is re-listed only when its mtime moves. A sidecar cursor (64 KB tail) exists only for agents in a provisional state (`NodeState::is_provisional`: `Running` or `Finished(AwaitingNotice)`), and is pumped each tick so an agent that resumes flips back to `Running` when its file grows. A terminal state (a notified outcome, `Killed`, `Orphaned`) drops the cursor. A session with 100 notified historical agents costs one `stat` per refresh; each agent still awaiting its notice costs one more.
- **Cost at 20 sessions.** A steady tick is about 20 transcript stats, a few sidecar stats, ~20 small registry reads, ~30 `state.json` and task-dir stats, one `ps` and one `tmux list-panes`. Derive is microseconds, so there is no incremental derive and no derived cache to invalidate.

### Workspaces and account limits

A workspace is a scope in `ui::App`, not a filter in `World`: `World` and `derive` still see every session, and `ui::layout` and the header keep the rows whose cwd is under the workspace root. `w` switching scope is therefore instant and needs no refresh.

`Snapshot.limits` maps each config dir to its `Limits { five_hour, seven_day, at }`. Three sources feed it. The statusline tee's `rate_limits` is fresh whenever a session of that account renders. `cachedUsageUtilization` in the account's `.claude.json` is whatever `/usage` last fetched, often days old, and `World` re-parses it only when the file's stamp moves (on the tick). The third is a refusal: an assistant record with `error: "rate_limit"` and `quotaLimits { status: "rejected", rateLimitType, resetsAt }`, which pins that window at 100% until it resets. It matters most for the account you stopped using because it is full, since nothing of that account is on screen to refresh its tee. `limits::RefusalScan` follows every transcript modified in the last week (each file once, since `~/.claude-wukong/projects` can be a symlink to another account's): a 256 KB tail on first sight, then only appended bytes, newest files first, with a 16 MB budget per pass and a pass every 30 s or on every tick until a cold start is caught up. The transcript's location cannot say which account was refused, so `limits::attribute` credits each refusal to the account whose `history.jsonl` holds that session's latest prompt before it, and drops a refusal no history knows. This is a deliberate exception to tail-only reads of live sessions: it reads tails of non-live transcripts, bounded by the week and the budget. `derive::snapshot` starts from the cache and the refusals and merges every live session's tee, hidden spares included, with `Limits::merge` taking each window from the newer observation. A window whose `resets_at` has passed is empty (`Window::used_now`), so a stale 97% that has since reset counts as room. `derive::roomiest` orders a workspace's accounts for `n`: nothing full first (else the earliest to free up), then 7-day use, then 5-hour use, ties in configured order.

### Persistence

There is a single file, `state.toml`, and every mutation is an `Op`. `UserState::apply(op)` is idempotent. `Store::commit` runs reload-if-stamp-moved → apply → write `.tmp` → fsync → `rename`, and skips the write when apply changed nothing. A persistent window and a popup can both commit. Two processes committing different keys inside the sub-millisecond reload→rename window can lose one write. This is accepted. A newer `version` refuses to load, so an old binary never rewrites a newer file. A corrupt file is moved aside, and the app starts empty.

`Op::Rekey { from, to }` copies `from`'s entry to `to` unless `to` has one, then clears `from`'s anchor, so exactly one entry holds any anchor and the next `/clear` is unambiguous. `Op::Anchor` records the process for an entry that has none yet (first label on a session).

### Layout and rendering are separate

`ui::layout(&Snapshot, &UserState, &App) -> Vec<Line>` is a pure function over plain data. It applies user groups and labels, sorts groups by `GroupId`'s derived `Ord` and rows by `model::row_order`, filters, flattens expanded trees with their prefixes, and omits empty user groups. `Line` has no ratatui types and no clock. Elapsed times stay as `since: At`, so the renderer formats them against its own `now` and the layout is value-tested without a backend. `ui::update` calls `layout` to know what is under the cursor, so navigation and drawing can never disagree. `ui::render` is a thin map from `Line` to widgets.

### Actions as data

`Effect { Jump(PaneId), Attach{dir, job}, NewSession{dir, cwd, group, name}, Commit(Op), Quit }` is the whole outside-world surface. No variant can write a Claude file. `Row.open: OpenTarget` is decided in derive against the tmux pane snapshot, so Enter is a lookup and a pane missing from the snapshot is never offered. `main::execute` is the only code that spawns `tmux` or `claude`. Every command is built as an argv vector with no shell string, so a user-typed `--name` needs no quoting, and every `claude` gets `CLAUDE_CONFIG_DIR` (gotcha 32). Only `Jump` needs tmux. `Attach` needs no tmux: `Screen::hand_over` parks the input thread, leaves the alternate screen, runs `claude attach <job>` in the foreground, then re-enters and repaints. `claude attach` exits 0 on Ctrl+Z, which returns `Outcome::Attached`. That keeps the popup open too, since the user came back on purpose. For `n`, the shell runs `claude --bg [--name <name>]` in the row's cwd. `--bg` ignores `--session-id` (2.1.310 warns and picks its own), so the job id is read from the `backgrounded · <id> · ...` line it prints. That returns `Outcome::Spawned{dir, job, group}`, and `update` answers with `Commit(SetGroup)` keyed by `UserKey::Job`, then `Attach`. The new job lands in the right group before its registry record exists. A failed start flashes the last line of stderr, for example the untrusted-workspace message.

### Surviving Claude Code changes

- Every wire field is `Option` or `#[serde(default)]`, with no `deny_unknown_fields`.
- Every enum from a file has an `Other(String)` arm: `RegStatus`, `WaitingFor`, `JobState`, `Tempo`, `FanKind`, `ProcKind`, `TaskStatus`. A new `waitingFor` value still ranks NeedsYou and shows its raw reason.
- An unparseable line or file is counted, never fatal.
- Moved files degrade gracefully. Transcripts are found by globbing the sessionId (never by slugging the cwd), config dirs are rediscovered every refresh, and a row with no transcript still shows the correct registry status, with blank enrichment.
- The registry alone can carry the core status. Every transcript-derived field is enrichment that can be lost without a wrong status, with one exception: Working vs Parked under `busy`. With no transcript, that case shows as Working.

### What it deliberately does not do

It does not scan full files or run background scan threads. It does not estimate cost or keep a window table. It does not read `.key` files, `roster.json` or sockets, and it never writes `.fleetview-heartbeat`. It does not poll `claude agents --json`. It does not compute anything in the key handler.

## Synthesis decision

Base: candidate X (opus). The cross-judge independently picked the same base. Its invariants live in types a maintainer cannot route around (`SafeText`, `Count::AtLeast`, `ProcStart::new`, `PaneId::parse`), it has one owner thread with no shared state, and its pure decomposition is the most complete.

Rulings on contested points:

1. Never full-scan a transcript (SPEC-literal). `Count::{Exact, AtLeast}` carries the ignorance. A per-refresh byte budget inside `World` bounds first-sight windows. Y's one-time refold and Z's backfill were rejected because they contradict this ruling.
2. Single owner thread. The input reader and the notify watcher are feeder threads that only send messages. Cold-start frame time is measured before any collector thread is considered.
3. `n` uses `claude --session-id <uuid>` (flag verified in `claude --help` on 2.1.289), plus `--name <name>` when the user typed one. Y's `PendingGroup` dir+cwd match is superseded.
4. The prose-question heuristic is `Status::Idle { asked_question }`, per decisions.tsv. It sorts first within Idle and gets its own glyph. It never ranks as NeedsYou. X ranked it as NeedsYou and was corrected.

Grafts from Z (sonnet): the watch set (`sessions/`, `jobs/`, `tasks/`, statusline cache) with transcripts stat-polled on the tick and 250 ms coalescing, replacing X's recursive watch of whole config dirs; `Parked::new -> Option` with the registry fallback rules; `RegStatus::Waiting(WaitingFor)` with `Unspecified`; typed `Diagnostic { source, problem, path }` in place of `HealthNote`; the `Host` trait and `TmuxCmd::argv()`; the persisted `Anchor` on interactive entries, replacing X's observation-only `Op::Rekey`. Z's `Effect::SaveState(UserState)` and per-key `touched` merge were rejected: X's `Commit(Op)` is narrower, and reload→apply→rename needs no merge.

Grafts from Y (fable): `Status::Idle { asked_question }` and `Need::Permission { tool }`; the `NodeState`/`TaskOutcome`/`Notice { at, outcome: Option<_> }` vocabulary, whose names match GROUNDING 3.9 one to one where X's `Finished(NoticeStatus::Unknown)` folded "orphaned", "awaiting notice" and "predates window" into one bucket; the pure `layout -> Vec<Line>` with an abstract `Key`, so grouping, sorting, filter, tree flattening and cursor stability are value-tested; Y's 10-step end-to-end plan.

Fixes to X's own defects: `SessionView` moved from `world` to `derive` and `derive::snapshot` takes iterators so `derive` never imports the IO owner; `Store::commit` keeps reload→apply→rename with the sub-millisecond race accepted; `ps` failure degrades to "all alive plus a diagnostic" instead of aborting.

Decisions the synthesis left open, settled in the scaffold:

- `Need` keeps a `Waiting(WaitingFor)` wrapper beside `Permission { tool }` instead of mirroring every reason as its own variant. `Waiting(WaitingFor::Permission)` is representable but `derive::need_of` never constructs it. This costs one unused combination and saves a parallel enum with duplicated labels.
- `Snapshot` is flat (`rows`, `diagnostics`) and `derive` takes no `UserState`. Grouping and labels move to `ui::layout`, so a label edit renders on the next frame instead of the next refresh.
- `Anchor` reconciliation is a pure `user_state::rebind(state, live) -> Vec<Op>` fed from `Row::live_anchor()`, with `Op::Anchor` and `Op::Rekey` as the only outputs. `Rekey` clears the anchor on `from` so one anchor is held by exactly one entry.
- `World::refresh(wake: Wake)` distinguishes `Doorbell` from `Tick`, so a doorbell never stats transcripts and runs `ps` only for new pids.
- `Facts.tail_looks_finished` was added as the input to `agent_state` rule 4.
- `TmuxCmd` lives in `main.rs` (W4) because the brief assigned `TmuxCmd::argv` to W4 and `model.rs` must have no bodies for a worker to fill in. `LIST_PANES_ARGV` and `parse_panes` live in `world.rs` beside `RealHost`, which uses them.
- `Window::max_bytes()` lets `World` charge a first-sight open against the `ReadBudget` before opening.
- `n` prompts for an optional name (`Mode::NewSession`), because ruling 3 says `--name` is passed "when the user typed one".

## Ownership

Four workers implement in parallel with disjoint file ownership. `model.rs` is frozen for the implementation phase; a type two workers need goes there and is already there. Each worker fills `todo!()` bodies and adds tests in its own files.

| worker | files | cross-module contracts it relies on |
|---|---|---|
| W1 | `transcript.rs`, `subagents.rs` | Produces `Facts` (incl. `tail_looks_finished`, `notices: HashMap<TaskId, Notice>`, `open_bg`), `Transcript::{open, pump, facts, stamp}`, `Window::max_bytes`, `FileStamp::of`. Produces `Subagents::{new, refresh(main, alive, budget) -> (spent, problems), agents() -> (&AgentMeta, NodeState, Option<&Facts>)}` and `agent_state`. Consumes only `model`. |
| W2 | `registry.rs`, `jobs.rs`, `sidecars.rs`, `world.rs` | Produces `RegEntry`, `Job`, `Task`, `Statusline`, `Team`, `repo_root`, `parse_ps`, `parse_panes`, `Host` + `RealHost`, `World::{new, refresh(Wake), now, sessions, exited, watch_roots, diagnostics}`. Builds `derive::SessionView` and `derive::ExitedView` (W3's structs, pub fields). Consumes W1's `Transcript`, `Facts`, `FileStamp`, `Subagents`. |
| W3 | `derive.rs` | Owns `SessionView`/`ExitedView`. Produces `snapshot(sessions, exited, diagnostics, now) -> Snapshot` and the decision functions (`status`, `need_of`, `parked`, `title`, `now_line`, `tree`, `meta`, `detail`, `open_target`, `auto_group`, `describe_parked`, `job_outcome`). Consumes `RegEntry`, `Job`, `Facts`, `Subagents::agents`, `Task`, `Statusline`, `Team` by their pub shapes. Never imports `world`. |
| W4 | `ui.rs`, `user_state.rs`, `main.rs` | Produces `App`, `Key`/`key_of`, `update(app, snap, user, input, now)`, `layout(snap, user, app) -> Vec<Line>`, `render`, `glyph`, `elapsed`; `UserState::apply`, `rebind`, `Store::{load, reload_if_changed, commit}`; the loop, `execute(Effect)`, `TmuxCmd::argv`, `mint_uuid_v4`, `spawn_input`, `spawn_doorbell`. Consumes `Snapshot`/`Row` (incl. `Row::live_anchor`), `World`'s pub API, `derive::snapshot`, `FileStamp::of`. |

The integration order that proves itself: W1 and W2 land first (fixtures, `World` against a fixture home with a `FakeHost`), then W3 (table-driven status tests over `SessionView`s built from fixture parses), then W4 (layout value tests, reducer tests, golden render, e2e).

## Tradeoffs accepted

- We accept `1+` and `AtLeast` counts for background work launched before the tail window, in exchange for never full-scanning a transcript and having one forward-only fold with no background scan thread.
- We accept file reads on the UI thread, bounded by `ReadBudget`. That buys no locks and one owner of all state.
- We accept that a doorbell does not refresh transcripts. The now-line and Working/Parked flip lag by up to one tick (1 s). NeedsYou comes from the registry, which is watched, so the costliest transition is not delayed.
- We accept re-deriving the whole snapshot on every refresh. It is microseconds, and it removes every "which derived value is stale" bug.
- We accept no cost on live sessions without the statusline tee. SPEC forbids pricing tables, and a guessed cost is worse than a blank.
- We accept the sub-millisecond lost-write window in `Store::commit` between two instances committing different keys.

## Alternatives considered

- **A collector thread sending `Arc<Snapshot>`** (Y, Z). It keeps IO off the UI thread, but every reader then has a second clock and a second copy of state, and the stall it prevents is unmeasured. Ruling 2 keeps the single owner and measures first.
- **Event-driven reads, where notify events name the file to re-read.** Correctness would depend on FSEvents delivery and path mapping (worktree relocation, atomic-rename tmp names, coalesced dir events). The `stat` sweep hides all of that behind `refresh()`.
- **Grounding's "full scan once in the background, then incremental".** Exact pairing for old launches, at the cost of a second code path, a thread, and a SPEC violation. `Count::AtLeast` keeps the display honest instead.
- **Keying user state by `(pid, procStart)`.** Survives `/clear` for free, but loses every label when the process restarts or resumes, and SPEC asks for sessionId. `UserKey` + `Anchor` + `rebind` gets both.
- **A flat `Need` mirroring every `waitingFor`** (Y). One fewer representable-but-unused state, two parallel enums with the same labels. Not worth it.

## Verification

Fixtures live in `tests/fixtures/`. They are cut from live files named in GROUNDING, and each passes through `scripts/sanitize` before it is committed. That script drops `providerEnv` and replaces every Bash `command` and shell `fan.label` with a sentinel such as `SECRET_SENTINEL_x9`. Security tests grep for the sentinel in `Debug` output and in captured panes.

### Pure functions with fixture tests

| function | cases | defect it catches |
|---|---|---|
| `SafeText::redact` | `FOO=sk-abc… npm test`; `ghp_…`; a 40-char base64 run; a multi-line string; `redact(redact(x)) == redact(x)` | a secret reaching the screen; a newline breaking the two-line row |
| `PaneId::parse` | `"0:@51.%251"` → `%251`; `"%7"`; `""`; `"0:@51"` | jumping with the stale session/window part |
| `ProcStart::new` + `registry::parse_ps` | a `ps` line with trailing spaces and a double-space day | every session dropped as dead |
| `registry::parse` | busy / shell / waiting+`waitingFor` / waiting without `waitingFor` → `Unspecified` / spare / parkedJobId / no `tmux` key / `status:"napping"` → `Other` / missing pid → `MissingField("pid")` | crash or row loss on an absent key |
| `jobs::parse` | live `state.json` with a sentinel in `providerEnv` and in a shell `fan.label`; ISO timestamps; `inFlight:null` | `providerEnv` materialised (assert `format!("{job:?}")` lacks the sentinel); a raw command shown as a label |
| `Facts::apply_line`: turns | human prompt → tool_use → end_turn → `turn_duration{pendingBackgroundAgentCount:3}`; an interrupt with no turn_duration | Working shown for a parked session; an interrupted turn stuck open |
| `Facts::apply_line`: notifications | one completion delivered three ways; an orphan sweep listing 3 ids plus `__orphan_summary__:shell`; a monitor event with no `<status>` → `Notice{outcome: None}` keeps the item open; `[Monitor expired …]` → `Some(Completed)` | double counting (gotcha 20); a fake id shown; a monitor closed by its first event |
| `Facts::apply_line`: re-open | an agent notified, then `resumedAgentId` | a resumed agent shown as finished (gotcha 22) |
| `Facts::apply_line`: launches | Agent `async_launched` with `run_in_background:"true"` and unset; sync Agent `completed`; Bash `backgroundTaskId`; auto-backgrounded Bash; Monitor `taskId` | async decided from the input flag (gotcha 19) |
| `Facts::apply_line`: tail | sidecar ending in assistant text → `tail_looks_finished`; a later tool_use clears it; a handback sets it | an agent shown Running forever with no notice |
| `Facts::apply_line`: secrets | a Bash tool_use whose `command` holds the sentinel | the command reaching `Facts` at all |
| `Facts::apply_line`: metadata | two `ai-title`s, a `custom-title`, a `worktree-state` then `worktreeSession:null`, three `pr-link`s with a duplicate, `<synthetic>` usage after a real one | stale title wins; a left worktree shown; duplicate PRs; context reset to 0 |
| `Transcript::pump` split invariance | append the fixture in chunks of 1, 7, 4096 and 39 000 bytes; compare `Facts` with one read | partial-line holdback bugs |
| `Transcript::pump` replacement | truncate, then rewrite the file (new inode) | a cursor reading past EOF |
| `window_start` | a 2 MB synthetic file with the last `last-prompt` 300 KB from EOF; a file whose final 300 KB is one line | a missing title on the first frame; an infinite grow loop |
| `subagents::agent_state` | one test per rule (1 to 8) | a user-stopped agent shown running (gotcha 24); a zombie depth-2 child; an ancient agent shown running |
| `derive::status` | table-driven over every row, plus `job.state:"done"` with registry `shell`, an unknown registry status, and `idle` with a trailing `?` → `Idle{asked_question: true}` (never NeedsYou) | trusting the classifier (gotcha 4); the heuristic promoted to NeedsYou |
| `derive::need_of` | `Permission` with and without a dangling tool_use; `Input`; `Other("x")` | the tool name lost or faked |
| `derive::parked` | registry `shell`, partial horizon, no launch seen → `shells: AtLeast(1)`; `busy`, closed turn, no evidence → `agents: AtLeast(1)`; bg `fan` vs a stale fold → fan wins; `pendingBackgroundAgentCount` > fold agents | "parked on nothing"; fold preferred over the roster; depth-2 agents missing |
| `derive::open_target` | interactive + pane in snapshot; interactive + pane absent → Unreachable; interactive in VS Code; bg alive; bg exited | Enter offered for a gone pane |
| `derive::auto_group` / `sidecars::repo_root` | a temp repo with a linked worktree under `.claude/worktrees/x`; a cwd outside any repo | worktrees split into separate groups |
| `UserState::apply` | every `Op` applied twice equals once; `Rekey` never overwrites the target and clears `from`'s anchor; an entry emptied by clears is removed | duplicate groups; a label lost on `/clear` |
| `user_state::rebind` | live `(anchor, B)` with an entry at A carrying that anchor and none at B → `[Rekey{A→B}]`; entry at B with no anchor → `[Anchor]`; steady state → `[]` | labels not following `/clear`; the file churned every refresh |
| `user_state::decode/encode` | round-trip with anchors; `version = 2` refused; garbage moved aside | an old binary rewriting a newer file |
| `Store::commit` (temp dir) | two `Store`s on one path commit different keys interleaved | lost updates between popup and window |
| `World::refresh` (fixture home + `FakeHost`) | two config dirs, a `.key` file, a spare, a dead pid; `FakeHost::proc_starts` returning `Err` keeps rows and adds a `Ps` diagnostic; a quiet `Tick` reads zero transcript bytes | discovery bugs; `ps` failure blanking the list |
| `TmuxCmd::argv` | every variant, with and without `cwd` and `name` | a missing `-e CLAUDE_CONFIG_DIR` (gotcha 32) |
| `ui::layout` | rows in all five statuses across two groups with rank ties; Idle `asked_question` first within Idle; a user group overriding the auto group; a filter; an expanded row with a depth-2 tree and finished counts | sort by recency instead of precedence; Finished not last |
| `ui::update` | Enter on a `Pane` row in popup mode → `[Jump]`, then `Done(Jumped)` → `[Quit]`; `Done(Failed)` → flash, no Quit; cursor kept by `RowKey` across a re-sort; Enter on `Unreachable` → no effect; `n`, Enter on an empty name → `NewSession{name: None}`, then `Spawned` → `[Commit(SetGroup), Attach]` | the popup not closing after a jump; the cursor jumping on re-sort |
| `ui::render` golden | a `TestBackend` 100×30 render of a fixture snapshot | layout regressions; glyph mix-ups |

### End to end against live sessions

These run on the dev machine, read-only, in a scratch tmux session so nothing the user is looking at moves. Steps 2 to 7 are `scripts/e2e.sh`, rerunnable, exit code 0 on pass, printing the captured pane on failure.

1. `tmux new-session -d -s agt-test -x 160 -y 50 'agentty'`; wait 2 s.
2. `tmux capture-pane -p -t agt-test` and assert: one group header per expected repo; every live, non-hidden `sessions/*.json` appears exactly once; no line matches the token regex used by `SafeText::redact`; every row the registry marks `waiting` carries `▲` and its `waitingFor` text.
3. Cross-check against the registry read directly with `jq`: the set of pids rendered equals the set of live, non-hidden pids (compare counts per config dir in the header).
4. Status transitions: pick a row whose registry says `shell`; assert `◐` and `waiting on … shell`. Pick one that says `busy` with a recent `turn_duration` carrying `pendingBackgroundAgentCount`; assert `◐` and the agent count.
5. Keys: `tmux send-keys -t agt-test j j l`; capture; assert the third row is expanded. `send-keys /` then `fee` then `Enter`; assert only matching rows remain. `send-keys Esc` restores.
6. Label persistence: `send-keys r`, type `e2e`, `Enter`; assert the label on the row and in `state.toml` under that session id with an `anchor` table; restart the binary in the same tmux session; assert the label is still rendered. Then remove the label.
7. Jump (popup mode): in a second scratch window run `agentty --popup`, move to a row whose open target is a pane, `send-keys Enter`; assert via `tmux display -p '#{pane_id}'` on the client that the active pane changed to the registry's pane and that the popup process exited.
8. Liveness: start a throwaway `sleep` and write a fake `sessions/<pid>.json` in a fixture config dir passed via `CLAUDE_CONFIG_DIR` (never under `~/.claude*`); kill the sleep; assert the row disappears within 2 s. This is the only e2e step that writes, and only to the fixture dir.
9. Cost (planned, no trace flag exists yet): a quiet tick should read no `*.jsonl` bytes. Measured on 2.1.289 with 8 to 13 sessions, a steady refresh takes 27 to 31 ms, of which `ps` is 21 to 27 ms, and a cold start fills in about 0.5 s.
10. `tmux kill-session -t agt-test`.

## Open questions and risks

- Can two terminals `claude attach` the same job at once (GROUNDING open question 4)? If not, `Attach` needs an "already attached" check, and nothing on disk shows that state.
- Should user-state entries for sessions unseen for 30 days be garbage-collected? Today the file only grows, by roughly 150 bytes per labelled session (the anchor table adds about 50).
- Under a busy registry with no transcript (an unprompted bg job, a moved file), Working vs Parked cannot be decided. This design shows Working.
- The `?`-heuristic's false-positive rate on this machine is unmeasured. It is confined to the Idle rank, so a false positive costs a glyph, not a top-of-list row.

## Implementation reconciliation

Accepted by the lead at merge:

- `agent_state` rule order: rule 2 (relaunched) applies only while the session is alive and the parent agent runs, and the Orphaned checks (5, 6) come after rules 3 and 4. In the literal 1-to-8 order, a dead session's open launch read as Running and a child that completed before its parent finished read as Orphaned. The rule table test pins each ordering case.
- The tree collapses every agent that is not Running (Finished, Killed, Orphaned) into the finished count of its level. A cold start on a pstack session otherwise listed every grandchild of a finished agent as a live Orphaned node.
- `ExitedView` lost its `repo` field. Exited rows always group under Finished, so nothing read it.
- `World` admits a first-sight transcript window while the refresh's budget is unspent and charges what `Transcript::window_bytes` reports, instead of charging 4 MB up front. A cold start of 8 sessions went from about 8 refreshes to 2. While anything is deferred, the main loop refreshes again at once instead of waiting for the 1 s tick.
- For bg rows, a fan shell leaf shows the transcript's Bash `description` for the same task id when the fold has one. The fan label is the command line, which redaction cuts to little more than the program name.
- `--dump` refreshes until nothing is deferred (at most 20 times), then prints the frame followed by one line per diagnostic.
- `claude --session-id <uuid> --name <name>` works for interactive launches on 2.1.289. W4 ran it in a private tmux server, and the end-to-end check confirmed the argv, cwd, and `CLAUDE_CONFIG_DIR`.
