//! The shared domain vocabulary: identities, the status enum, the snapshot the UI renders, the
//! user-state ops and the effect type the UI emits. No module outside a parser ever sees a wire
//! type; everything here is what parsers produce and what `derive` and `ui` consume.
//!
//! Four implementers build against this file in parallel without editing it. A type belongs here
//! when two of their modules exchange it; a type one module owns stays in that module.

use std::cmp::Ordering;
use std::path::PathBuf;

// ---------------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------------

/// Epoch milliseconds, UTC. The one time type inside the app. Parsers convert registry/fan
/// epoch-ms ints and transcript/job ISO-8601 strings (gotcha 29) into this at the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct At(pub i64);

impl At {
    /// Parse an ISO-8601 timestamp; `None` on anything unparseable (lenient boundary).
    pub fn parse_iso(s: &str) -> Option<At> {
        chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| At(d.timestamp_millis()))
    }
    pub fn now() -> At {
        At(chrono::Utc::now().timestamp_millis())
    }
    pub fn millis_since(self, earlier: At) -> i64 {
        self.0 - earlier.0
    }
}

// ---------------------------------------------------------------------------------------------
// Identities. Newtypes so a JobId can never be passed where a SessionId is expected.
// ---------------------------------------------------------------------------------------------

/// A Claude config dir root (`~/.claude`, `~/.claude-personal`, `$CLAUDE_CONFIG_DIR`).
/// Each is an independent universe; every other id is only unique *within* one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigDir(pub PathBuf);

impl ConfigDir {
    /// Display form: `~/.claude-personal` -> `personal`, `~/.claude` -> `` (the default dir is
    /// unmarked so the common case adds no noise to a row).
    pub fn short(&self) -> &str {
        let name = self.0.file_name().and_then(|n| n.to_str()).unwrap_or("");
        match name.strip_prefix(".claude") {
            Some(rest) => rest.trim_start_matches('-'),
            None => name,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pid(pub u32);

/// `ps -o lstart=` output in UTC, already trimmed (gotcha 7). Constructed only by `new`, so an
/// untrimmed value can never be compared against the registry's `procStart`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcStart(String);

impl ProcStart {
    pub fn new(raw: &str) -> ProcStart {
        ProcStart(raw.trim().to_owned())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A conversation id (uuid). Can change under a live process (`/clear`, `/resume`, fork), so it
/// is never a row key. It *is* the key for the transcript file and for interactive user state.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub String);

/// 8-hex bg job id: `jobs/<JobId>/` and the argument to `claude attach`. Stable for the life of
/// the job even when its sessionId changes, which makes it the user-state key for bg rows.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub String);

/// Background task id as it appears in `<task-id>`: `b…` (shell/monitor) or `a…` (agent).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub String);

/// Subagent id. For agents the TaskId string equals the AgentId string, so
/// `TaskId::from(&AgentId)` is the join between notifications and sidecar files.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(pub String);

impl From<&AgentId> for TaskId {
    fn from(a: &AgentId) -> TaskId {
        TaskId(a.0.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolUseId(pub String);

/// A tmux pane id, always of the form `%<digits>`. Constructed only by `parse`, which extracts
/// it from the registry `tmux` field (`"0:@51.%251"`); the session and window parts are written
/// once at registration and go stale (gotcha 12), so they are discarded here.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(String);

impl PaneId {
    pub fn parse(tmux_field: &str) -> Option<PaneId> {
        let digits = &tmux_field[tmux_field.rfind('%')? + 1..];
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| PaneId(format!("%{digits}")))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One row of `tmux list-panes -a`. `World` takes one snapshot per tick; `derive::open_target`
/// offers `OpenTarget::Pane` only for a pane present in it (gotcha 31: never `display -p -t`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: PaneId,
    /// `pane_pid`: the login shell, whose child is the registry pid.
    pub pid: Pid,
}

/// Identity of a row in the list. Live rows key on the *process* (gotcha 9): pid alone can be
/// reused, sessionId can change, `(dir, pid, procStart)` cannot. Exited rows exist only for bg
/// jobs, whose job dir outlives the process.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowKey {
    Live { dir: ConfigDir, pid: Pid, proc_start: ProcStart },
    Exited { dir: ConfigDir, job: JobId },
}

/// The key user state (labels, groups) is stored under. bg rows use the JobId (stable across
/// the job's sessionId changes and across process exit, so a label survives into the Exited
/// row); interactive rows use the SessionId, carried across `/clear` by `Anchor` + `Op::Rekey`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UserKey {
    Job(JobId),
    Session(SessionId),
}

impl UserKey {
    /// `job:<id>` / `sid:<uuid>`: the on-disk map key in state.toml.
    pub fn encode(&self) -> String {
        match self {
            UserKey::Job(j) => format!("job:{}", j.0),
            UserKey::Session(s) => format!("sid:{}", s.0),
        }
    }
    pub fn decode(s: &str) -> Option<UserKey> {
        if let Some(j) = s.strip_prefix("job:") {
            return Some(UserKey::Job(JobId(j.to_owned())));
        }
        s.strip_prefix("sid:").map(|sid| UserKey::Session(SessionId(sid.to_owned())))
    }
}

/// The process an interactive user-state entry was last seen under. Persisted beside the label
/// so that, after `/clear` changes the sessionId under the same process, `user_state::rebind`
/// can copy the entry to the new sessionId even when the dashboard was not running at the time.
/// Only `UserKey::Session` entries carry one (a JobId is already stable).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    pub dir: ConfigDir,
    pub pid: Pid,
    pub proc_start: ProcStart,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupName(pub String);

// ---------------------------------------------------------------------------------------------
// Safe text: the only string type the renderer accepts for text sourced from Claude files.
// ---------------------------------------------------------------------------------------------

/// Display text that has passed redaction. The renderer takes `&SafeText` for every field that
/// originates in a Claude file, so a raw fan label or Bash command cannot reach the screen
/// without going through `SafeText::redact` (security rule, gotcha 35). Also collapses the text
/// to one line and caps its length, so a 1.3 MB line can never blow up a render.
#[derive(Clone, Debug, PartialEq, Eq, Default, Hash)]
pub struct SafeText(String);

impl SafeText {
    /// Redact and single-line `raw`:
    /// - drop leading `VAR=value` env assignments (`FOO=bar BAZ=qux cmd …` -> `cmd …`);
    /// - replace token-looking runs with `…`: `sk-…`, `ghp_…`, `xox?-…`, `AKIA…`, `Bearer …`,
    ///   and any [A-Za-z0-9_\-+/=]{32,} run (long base64/hex);
    /// - collapse whitespace, newlines and control chars to single spaces, cap at 512 chars.
    /// Idempotent: `redact(redact(x).as_str()) == redact(x)`.
    pub fn redact(raw: &str) -> SafeText {
        const MAX_CHARS: usize = 512;
        let words: Vec<&str> = raw.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect();
        let first_cmd = words.iter().position(|w| !is_env_assignment(w)).unwrap_or(words.len());
        let mut out = String::new();
        let mut redact_next = false;
        for w in &words[first_cmd..] {
            let token = if redact_next || looks_secret(w) { "…" } else { w };
            redact_next = w.eq_ignore_ascii_case("bearer");
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(token);
        }
        if out.chars().count() > MAX_CHARS {
            out = out.chars().take(MAX_CHARS - 1).collect();
            out.push('…');
        }
        SafeText(out)
    }
    /// For text the app itself authored (fixed strings, counts). Never call with file content.
    pub fn ours(s: impl Into<String>) -> SafeText {
        SafeText(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn is_env_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        }
        None => false,
    }
}

fn looks_secret(word: &str) -> bool {
    const PREFIXES: [&str; 7] = ["sk-", "ghp_", "gho_", "github_pat_", "xoxb-", "xoxp-", "AKIA"];
    let bare = word.rsplit_once('=').map_or(word, |(_, v)| v).trim_matches(|c| c == '"' || c == '\'');
    if PREFIXES.iter().any(|p| bare.starts_with(p) && bare.len() > p.len() + 8) {
        return true;
    }
    let run = bare
        .split(|c: char| !(c.is_ascii_alphanumeric() || "_-+/=".contains(c)))
        .map(str::len)
        .max()
        .unwrap_or(0);
    run >= 32 && !bare.starts_with('/') && !bare.contains("://")
}

// ---------------------------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------------------------

/// The row status. One exhaustive enum; precedence is `rank()`. Every variant carries what the
/// second row line needs, so the now-line never re-derives the status.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Blocked on the human. Highest rank.
    NeedsYou(Need),
    /// A turn is streaming (registry `busy` and the transcript has an open turn).
    Working,
    /// No turn streaming, but the session is waiting on its own background work.
    Parked(Parked),
    /// Nothing in flight. `asked_question`: the last turn's final assistant text ends in `?`.
    /// A heuristic, so it never promotes to NeedsYou; it sorts first within Idle and gets its
    /// own glyph (`?`).
    Idle { asked_question: bool },
    /// bg job whose process retired within 24h. Only bg jobs ever reach this; a dead
    /// interactive session simply disappears (its registry file is gone or stale).
    Exited(JobOutcome),
}

/// Ordering key for `Status`. `Ord` is derived from declaration order: smaller sorts first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    NeedsYou,
    Working,
    Parked,
    Idle,
    Exited,
}

impl Status {
    pub fn rank(&self) -> Rank {
        match self {
            Status::NeedsYou(_) => Rank::NeedsYou,
            Status::Working => Rank::Working,
            Status::Parked(_) => Rank::Parked,
            Status::Idle { .. } => Rank::Idle,
            Status::Exited(_) => Rank::Exited,
        }
    }

    /// Full sort key: rank, then within Idle the asked-question rows first.
    pub fn sort_key(&self) -> (Rank, u8) {
        let sub = match self {
            Status::Idle { asked_question: true } => 0,
            _ => 1,
        };
        (self.rank(), sub)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Need {
    /// Registry `waiting` + `waitingFor: "permission prompt"`. `tool` is the name of the
    /// dangling (unanswered) tool_use in the transcript, if one is visible: the prompt itself is
    /// never on disk (gotcha 5).
    Permission { tool: Option<String> },
    /// Registry `waiting` with any other reason. `derive` never constructs
    /// `Waiting(WaitingFor::Permission)`; that reason becomes `Permission`.
    Waiting(WaitingFor),
    /// bg job with `tempo: blocked` and a `needs` question (classifier output, so a hint, but
    /// only consulted when the registry says `idle`).
    JobQuestion(SafeText),
}

/// Registry `waitingFor`. Folded into `RegStatus::Waiting`, so waiting-without-reason is
/// `Unspecified` and unknown values survive as `Other`: a new Claude release still shows
/// NeedsYou with its raw reason instead of dropping the row to Idle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitingFor {
    Permission,
    Input,
    Dialog,
    Sandbox,
    Worker,
    GoalProposal,
    /// `status: waiting` with no `waitingFor` key.
    Unspecified,
    Other(String),
}

impl WaitingFor {
    /// Display text: the registry's own phrase (`permission prompt`, `input needed`, …), the raw
    /// string for `Other`, `waiting` for `Unspecified`.
    pub fn label(&self) -> &str {
        match self {
            WaitingFor::Permission => "permission prompt",
            WaitingFor::Input => "input needed",
            WaitingFor::Dialog => "dialog open",
            WaitingFor::Sandbox => "sandbox request",
            WaitingFor::Worker => "worker request",
            WaitingFor::GoalProposal => "goal proposal",
            WaitingFor::Unspecified => "waiting",
            WaitingFor::Other(s) => s,
        }
    }
}

/// What a parked session is waiting on. Counts come from the transcript fold or the bg `fan`;
/// the registry only decides *whether* the session is parked.
///
/// Invariant (enforced by `new`): at least one count is positive, or `workflows > 0`, or a
/// wakeup is set. "Parked on nothing" is unrepresentable; `derive::parked` applies the registry
/// fallbacks (`busy` -> `agents: AtLeast(1)`, `shell` -> `shells: AtLeast(1)`) before calling
/// `new`, so its `Option` is only ever `None` in tests.
#[derive(Clone, Debug, PartialEq)]
pub struct Parked {
    agents: Count,
    shells: Count,
    monitors: Count,
    workflows: u16,
    wakeup: Option<At>,
}

impl Parked {
    pub fn new(
        agents: Count,
        shells: Count,
        monitors: Count,
        workflows: u16,
        wakeup: Option<At>,
    ) -> Option<Parked> {
        let something = agents.is_positive()
            || shells.is_positive()
            || monitors.is_positive()
            || workflows > 0
            || wakeup.is_some();
        something.then_some(Parked { agents, shells, monitors, workflows, wakeup })
    }
    pub fn agents(&self) -> Count {
        self.agents
    }
    pub fn shells(&self) -> Count {
        self.shells
    }
    pub fn monitors(&self) -> Count {
        self.monitors
    }
    pub fn workflows(&self) -> u16 {
        self.workflows
    }
    /// A pending ScheduleWakeup in the future.
    pub fn wakeup(&self) -> Option<At> {
        self.wakeup
    }
}

/// A count that admits ignorance. `AtLeast(1)` is what we show when the registry says `shell`
/// but the launch happened before the transcript tail we read (see `transcript::Horizon`): we
/// know there is one, we do not know how many. Rendered as `1+`. Never guessed upward.
/// `AtLeast(0)` means "nothing seen, window incomplete" and counts as nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    Exact(u16),
    AtLeast(u16),
}

impl Count {
    pub fn n(self) -> u16 {
        match self {
            Count::Exact(n) | Count::AtLeast(n) => n,
        }
    }
    pub fn is_positive(self) -> bool {
        self.n() > 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    Done,
    Failed,
    Stopped,
    Unknown,
}

/// Terminal state of a background task as the transcript reports it (GROUNDING 2.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskOutcome {
    Completed,
    Failed,
    Killed,
    Stopped,
    /// The agent's own transcript tail looks finished (end-of-turn text or a handback) but no
    /// notice has been seen yet, or the notice predates our window.
    AwaitingNotice,
}

/// State of a node in the subagent tree (GROUNDING 3.9). Finished is reversible: agents
/// re-notify and can be resumed (gotcha 22), so it is derived every refresh, never latched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Running,
    Finished(TaskOutcome),
    /// `meta.stoppedByUser`: these get no notification (gotcha 24).
    Killed,
    /// The owner is gone with no terminal marker: the session process is dead, or (depth 2) the
    /// parent agent has finished.
    Orphaned,
}

// ---------------------------------------------------------------------------------------------
// Snapshot: everything the renderer reads. Produced by `derive::snapshot`, never mutated.
// Flat: grouping depends on user state the UI edits instantly, so it is applied by
// `ui::layout`, not here.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Every live session plus every exited bg job within the window, in no particular order.
    pub rows: Vec<Row>,
    /// Degradation signals. Non-empty shows a dim `⚠ n` in the header and the details in the
    /// help screen, so a Claude release that blanks a column is visible instead of silent.
    pub diagnostics: Vec<Diagnostic>,
}

/// The group a row falls into when the user has not assigned one. Declaration order is display
/// order among auto groups; `Finished` is always last.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AutoGroup {
    /// The session is a team lead (or member) of `teams/<team>/config.json`.
    Team(String),
    /// Git main-worktree root; linked worktrees fold into it.
    Repo(PathBuf),
    /// cwd not inside a git repo.
    Elsewhere,
    /// Exited bg jobs from the last 24h.
    Finished,
}

/// Group identity in the rendered list. User groups come first in their persisted order, then
/// auto groups in `AutoGroup` order. Derived `Ord` is the display order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupId {
    User { order: u32, name: GroupName },
    Auto(AutoGroup),
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: RowKey,
    pub user_key: UserKey,
    /// Where the row lands unless user state says otherwise.
    pub auto_group: AutoGroup,
    pub status: Status,
    /// "In this status since". Sort tiebreak within a rank (oldest first) and the elapsed
    /// column; the renderer computes elapsed from its own `now`, so ticking needs no re-derive.
    /// For Parked this is the turn end, not `statusUpdatedAt` (the registry stays `busy` across
    /// Working -> Parked).
    pub since: At,
    pub title: SafeText,
    /// Second line, already specific to the status.
    pub now: SafeText,
    pub meta: Meta,
    /// Live subagents, shells, monitors, workflows. Empty means "nothing to expand".
    pub tree: Tree,
    pub detail: Detail,
    /// What Enter does, decided here so the key handler is a lookup.
    pub open: OpenTarget,
}

impl Row {
    /// The anchor `user_state::rebind` matches against: present only for a live interactive row
    /// keyed by sessionId.
    pub fn live_anchor(&self) -> Option<(Anchor, &SessionId)> {
        match (&self.key, &self.user_key) {
            (RowKey::Live { dir, pid, proc_start }, UserKey::Session(sid)) => Some((
                Anchor { dir: dir.clone(), pid: *pid, proc_start: proc_start.clone() },
                sid,
            )),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub prs: Vec<u32>,
    pub ctx: Option<Ctx>,
    pub cost: Option<Cost>,
    pub remote_control: bool,
    pub bg: bool,
    /// `ConfigDir::short()`; empty for the default dir.
    pub dir_tag: String,
}

/// Context use. `Percent` only when the statusline cache gives an authoritative window size;
/// otherwise absolute tokens (SPEC: no context-window tables).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ctx {
    Percent(u8),
    Tokens(u64),
}

/// Cost is shown only from an authoritative source (SPEC: no pricing tables). Live sessions
/// without the statusline tee show no cost at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cost {
    /// `cost.total_cost_usd` from the statusline cache.
    Live(f64),
    /// `cost-state.totalCostUSD`, written at process exit: exact for Exited rows.
    Final(f64),
}

/// One level of the expandable tree. `live` holds every node that is not `Finished` (Running,
/// Killed and Orphaned render with their own glyph); Finished children collapse into a count
/// (SPEC 3).
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub live: Vec<Node>,
    pub finished: u16,
}

impl Tree {
    pub fn is_empty(&self) -> bool {
        self.live.is_empty() && self.finished == 0
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    /// Agent id for agents, `b…` task id for shells and monitors, the fan id for bg rows, a
    /// fixed `wakeup` id for the wakeup leaf. Cursor identity for tree lines.
    pub id: TaskId,
    pub kind: NodeKind,
    pub state: NodeState,
    pub label: SafeText,
    pub since: At,
    /// Last tool_use description of a running agent; None for shells/monitors.
    pub now: Option<SafeText>,
    pub children: Tree,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    Agent { agent_type: String, model: Option<String> },
    Shell,
    Monitor,
    Workflow,
    Wakeup,
}

/// The detail pane (tab / p). Precomputed for every row because it is small and the selection
/// moves faster than refreshes; computing it on demand would put IO in the key handler.
#[derive(Clone, Debug, Default)]
pub struct Detail {
    pub last_prompt: Option<SafeText>,
    pub last_text: Option<SafeText>,
    pub open_tasks: Vec<SafeText>,
    pub pending: Vec<SafeText>,
    pub cwd: PathBuf,
    pub config_dir: PathBuf,
    pub session_id: Option<SessionId>,
    pub job_id: Option<JobId>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OpenTarget {
    /// Interactive session whose pane is in the current tmux snapshot: `switch-client -t %N`.
    Pane(PaneId),
    /// bg job, live or exited: new tmux window running `claude attach`.
    Attach { dir: ConfigDir, job: JobId, title: String },
    /// Interactive outside tmux (VS Code, Desktop, plain terminal) or whose pane is gone. Never
    /// `--resume` a live session (it would become a second holder). The string is the flash.
    Unreachable(String),
}

// ---------------------------------------------------------------------------------------------
// Diagnostics: anything a Claude Code release could break surfaces here instead of crashing
// or being silently dropped.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub source: DiagSource,
    pub problem: Problem,
    /// Path with the home dir abbreviated, or the command name for `Ps`/`Tmux`. Never file
    /// contents.
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagSource {
    Registry,
    Job,
    Transcript,
    Subagent,
    Tasks,
    Statusline,
    Team,
    Ps,
    Tmux,
    UserState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// IO failure reading the file.
    Unreadable,
    /// Not JSON/TOML, or a torn registry write that failed the retry.
    Unparsable,
    /// A field without which the record cannot be used (identity fields).
    MissingField(&'static str),
    /// An enum value this build does not know. The row still renders with a fallback.
    UnknownValue { field: &'static str, value: String },
    /// The command (`ps`, `tmux list-panes`) failed. Rows degrade (all pids kept alive, no pane
    /// verification) rather than disappear.
    Unavailable,
    /// The per-refresh read budget deferred this file's first-sight window to the next refresh.
    Deferred,
}

// ---------------------------------------------------------------------------------------------
// User-state mutations and side effects. The UI returns these; the shell executes them.
// ---------------------------------------------------------------------------------------------

/// A user-state mutation. Every op is idempotent: applying it twice equals applying it once,
/// which is what makes the reload-apply-rename commit safe with two agentically instances
/// (persistent window + popup) writing the same file.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    SetLabel { key: UserKey, label: Option<String> },
    /// `None` returns the session to its automatic group. A new name appends to group order.
    SetGroup { key: UserKey, group: Option<GroupName> },
    /// Record the process a live interactive session runs under. Emitted by
    /// `user_state::rebind`, not by the user.
    Anchor { session: SessionId, anchor: Anchor },
    /// A process switched conversation (`/clear`, `/resume`): copy `from`'s entry to `to` unless
    /// `to` already has one, then clear `from`'s anchor so exactly one entry holds any anchor.
    /// `from` keeps its label (the old conversation may be resumed elsewhere). Emitted by
    /// `user_state::rebind`.
    Rekey { from: SessionId, to: SessionId },
}

/// Everything the app does to the outside world, as data. `ui::update` returns these; the shell
/// in main.rs runs them and feeds an `Outcome` back. No variant writes a Claude file.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Verify `%N` exists (`tmux list-panes -t %N`, rc=1 when gone; never `display -p`, gotcha
    /// 31), then `tmux switch-client -t %N`.
    Jump(PaneId),
    /// `tmux new-window -n <title> -e CLAUDE_CONFIG_DIR=<dir> claude attach <job>`.
    Attach { dir: ConfigDir, job: JobId, title: String },
    /// `tmux new-window -c <cwd> -e CLAUDE_CONFIG_DIR=<dir> claude --session-id <sid> [--name <name>]`.
    /// The sid is minted by the shell (it is random) and returned in `Outcome::Spawned`, so the
    /// UI can file the new session into `group` before its registry record even appears.
    NewSession { dir: ConfigDir, cwd: PathBuf, group: Option<GroupName>, name: Option<String> },
    Commit(Op),
    Quit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Jumped,
    Attached,
    Spawned { session: SessionId, group: Option<GroupName> },
    Committed,
    Failed(String),
}

/// Sort within a group, used by `ui::layout` and asserted by tests: status sort key, then oldest
/// `since` first.
pub fn row_order(a: &Row, b: &Row) -> Ordering {
    a.status.sort_key().cmp(&b.status.sort_key()).then(a.since.cmp(&b.since))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_drops_env_prefix_and_token_values() {
        let shown = SafeText::redact("API_KEY=sk-abcdefghijklmnopqrstuv OTHER=1 curl -H 'Authorization: Bearer abc.def' https://x.io");
        assert_eq!(shown.as_str(), "curl -H 'Authorization: Bearer … https://x.io");
    }

    #[test]
    fn redact_hides_long_opaque_runs_but_keeps_paths() {
        let shown = SafeText::redact("token ghp_0123456789abcdefghijABCDEFGHIJ0123 in /Users/me/workspace/some/really/long/path/name.rs");
        assert_eq!(shown.as_str(), "token … in /Users/me/workspace/some/really/long/path/name.rs");
    }

    #[test]
    fn redact_is_single_line_capped_and_idempotent() {
        let raw = format!("line one\nline\ttwo {}", "word ".repeat(200));
        let once = SafeText::redact(&raw);
        assert!(!once.as_str().contains('\n'));
        assert_eq!(once.as_str().chars().count(), 512);
        assert_eq!(SafeText::redact(once.as_str()), once);
    }

    #[test]
    fn pane_id_keeps_only_the_pane_part() {
        assert_eq!(PaneId::parse("0:@51.%251").map(|p| p.as_str().to_owned()), Some("%251".into()));
        assert_eq!(PaneId::parse("0:@51.%"), None);
        assert_eq!(PaneId::parse("0:@51"), None);
    }

    #[test]
    fn config_dir_short_tags_only_non_default_dirs() {
        assert_eq!(ConfigDir(PathBuf::from("/Users/x/.claude")).short(), "");
        assert_eq!(ConfigDir(PathBuf::from("/Users/x/.claude-personal")).short(), "personal");
    }

    #[test]
    fn user_key_round_trips() {
        for k in [UserKey::Job(JobId("9b3323e1".into())), UserKey::Session(SessionId("u-1".into()))] {
            assert_eq!(UserKey::decode(&k.encode()), Some(k));
        }
    }
}
