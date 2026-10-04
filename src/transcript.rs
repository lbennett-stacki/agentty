//! Boundary and incremental fold for transcript JSONL files: the main
//! `projects/<slug>/<sid>.jsonl` and every `subagents/agent-<id>.jsonl` (same reducer; subagent
//! files simply never produce metadata or turn records).
//!
//! Shape: `Transcript` owns a byte cursor and a `Facts` value. `pump()` reads only appended
//! complete lines and folds each into `Facts` via `Facts::apply_line`, a pure reducer. Wire
//! structs and the intermediate `Event` enum are private to this file; callers only see `Facts`.
//!
//! Window policy (SPEC: tail-read, never full scans): on first open, start at the latest
//! metadata block (`last-prompt` record) found within a bounded tail, so every "latest wins"
//! metadata field is present from the first frame. Anything older than the window start is
//! outside the `Horizon`, and `Facts` says so instead of guessing.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{At, SafeText, TaskId, TaskOutcome, ToolUseId};

// ---------------------------------------------------------------------------------------------
// Public: the cursor
// ---------------------------------------------------------------------------------------------

/// Identity + size of a file at last read. A changed inode or a shrunk length means the file was
/// replaced, not appended, and the fold restarts from a fresh window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub ino: u64,
    pub len: u64,
    pub mtime_ms: i64,
}

impl FileStamp {
    pub fn of(path: &Path) -> std::io::Result<FileStamp> {
        todo!("std::fs::metadata + MetadataExt::ino")
    }
}

/// How much history to take on first open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// Main transcript: tail 256 KB; if it holds no `"type":"last-prompt"` line, retry at 1 MB
    /// then 4 MB (a substring scan, no parse). Start at the last metadata block found, or at
    /// the tail start if none. Max observed metadata distance from EOF is 310 KB.
    Main,
    /// Subagent jsonl: tail 64 KB. Enough for its last tool_use and any depth-2 notifications.
    Sidecar,
}

impl Window {
    /// Upper bound of bytes `open` may read for this window, so `World` can charge it against
    /// the per-refresh read budget before opening.
    pub fn max_bytes(self) -> u64 {
        match self {
            Window::Main => 4 << 20,
            Window::Sidecar => 64 << 10,
        }
    }
}

/// An open transcript: cursor + folded facts. One per file path, shared by every row whose
/// session resolves to that file (two live holders of one sessionId share one fold).
pub struct Transcript {
    path: PathBuf,
    stamp: FileStamp,
    /// Byte offset of the first unread byte.
    offset: u64,
    /// Trailing bytes after the last `\n`, held back until the line completes.
    partial: Vec<u8>,
    window: Window,
    facts: Facts,
}

impl Transcript {
    /// Open and fold the initial window. Fails only on IO; malformed lines are counted in
    /// `Facts::skipped`, never fatal.
    pub fn open(path: PathBuf, window: Window) -> std::io::Result<Transcript> {
        todo!()
    }

    /// Fold newly appended complete lines. Returns whether `facts` changed. Cheap when nothing
    /// changed: one `stat`, compared to `stamp`. If the stamp shows replacement (inode change or
    /// `len < offset`), re-open from a fresh window.
    pub fn pump(&mut self) -> std::io::Result<bool> {
        todo!()
    }

    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stamp(&self) -> FileStamp {
        self.stamp
    }
}

/// Choose the window start for `file` of length `len`. Pure over the bytes it reads; split out
/// because it is the one place the 256 KB / metadata-block policy lives.
fn window_start(file: &mut File, len: u64, window: Window) -> std::io::Result<u64> {
    todo!()
}

// ---------------------------------------------------------------------------------------------
// Public: the folded facts
// ---------------------------------------------------------------------------------------------

/// Everything derive needs from one transcript, kept current by `apply_line`. All "latest
/// wins" fields hold the latest value seen; all collections are bounded by the number of
/// background tasks or PRs, never by file size.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub horizon: Horizon,
    pub turn: Turn,
    /// Background work launched within the horizon and not yet terminal. Keyed by task id.
    pub open_bg: BTreeMap<TaskId, BgItem>,
    /// Latest notice per task id (agents can re-notify and be re-opened, gotcha 22).
    /// For a subagent file this holds its depth-2 children's completions.
    pub notices: HashMap<TaskId, Notice>,
    pub last_tool: Option<ToolCall>,
    pub last_text: Option<TextBlock>,
    /// The last record is end-of-turn assistant text or a handback tool_result, with no later
    /// tool_use or tool_result. Only meaningful for sidecar files (no `turn_duration` there,
    /// and `stop_reason` is unreliable, gotcha 26): it is `NodeState` rule 4's input.
    pub tail_looks_finished: bool,
    pub titles: Titles,
    pub last_prompt: Option<SafeText>,
    pub away_summary: Option<SafeText>,
    pub worktree: Option<Worktree>,
    /// `gitBranch` of the latest conversational record.
    pub branch: Option<String>,
    /// All PRs ever linked within the horizon, deduped by number, in first-seen order.
    pub prs: Vec<u32>,
    pub usage: Option<Usage>,
    /// Pending ScheduleWakeup time; replaced by newer calls, cleared by `{stop:true}`.
    pub wakeup: Option<At>,
    /// A `cost-state` with no later conversational record: the process exited cleanly. Cleared
    /// by any later conversational record (resume).
    pub exit: Option<ExitTotals>,
    /// Latest `bridge-session` says a Remote Control bridge is attached.
    pub bridge_active: bool,
    /// Timestamp of the latest record of any kind; used for subagent liveness.
    pub last_activity: Option<At>,
    pub lines: u64,
    pub skipped: u64,
    /// Tool uses awaiting their tool_result, so a background launch can be labelled with the
    /// tool_use's description. Pruned when the result arrives; bounded by in-flight tools.
    pending_tools: HashMap<ToolUseId, PendingTool>,
}

/// Where our knowledge starts. Launches before the window start are invisible, so counts
/// derived from `open_bg` are lower bounds unless `from_start`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Horizon {
    pub from_start: bool,
    /// Timestamp of the first record inside the window. A subagent whose jsonl went quiet before
    /// this, with no notice seen, finished before our window (see `subagents::agent_state`).
    pub first_at: Option<At>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Turn {
    /// No turn boundary seen in the window yet.
    #[default]
    Unknown,
    /// Opened by a `user` record (human, task-notification, peer, scheduled).
    Open { since: At },
    /// Closed by `system/turn_duration`. `pending_agents` is `pendingBackgroundAgentCount`
    /// (absent means 0); it counts depth-2 agents, which the fold may not see.
    Closed { at: At, pending_agents: u16 },
    /// `[Request interrupted by user…]`; these rarely get a turn_duration.
    Interrupted { at: At },
}

#[derive(Clone, Debug, PartialEq)]
pub struct BgItem {
    pub kind: BgKind,
    /// The launching tool_use's `description` (Agent/Monitor/Bash), redacted. Never the Bash
    /// command: the wire struct for Bash input does not deserialize `command`.
    pub label: SafeText,
    pub started: At,
    pub tool_use: Option<ToolUseId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgKind {
    Agent,
    Shell,
    Monitor,
}

/// A `<task-notification>` seen for a task id. `outcome: None` is a monitor event (monitor
/// events carry no `<status>`, gotcha 21): the monitor fired and is still running, so the item
/// stays in `open_bg`. `Some` is terminal and removes it. Monitor expiry and `stream ended` map
/// to `Some(Completed)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice {
    pub at: At,
    pub outcome: Option<TaskOutcome>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: ToolUseId,
    pub name: String,
    /// `tool_summary`: description ?? basename(file_path) ?? pattern ?? "".
    pub summary: SafeText,
    pub at: At,
    pub answered: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextBlock {
    pub first_line: SafeText,
    pub at: At,
    /// The text ends with `?` (after trimming whitespace and closing quotes/markdown).
    pub is_question: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Titles {
    pub custom: Option<SafeText>,
    pub agent_name: Option<SafeText>,
    pub ai: Option<SafeText>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub name: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    /// input + cache_read + cache_creation of the latest non-synthetic assistant message.
    pub ctx_tokens: u64,
    pub model: String,
    pub at: At,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExitTotals {
    pub cost_usd: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct PendingTool {
    name: String,
    summary: SafeText,
    at: At,
}

impl Facts {
    /// Empty facts whose horizon covers the whole file. `Transcript::open` uses this when the
    /// window starts at byte 0; tests use it to fold a complete fixture.
    pub fn from_start() -> Facts {
        Facts { horizon: Horizon { from_start: true, first_at: None }, ..Facts::default() }
    }

    /// The reducer. Parses one complete JSONL line and folds it in. Unknown record types, unknown
    /// subtypes and unparseable lines are counted (`skipped` only for unparseable) and otherwise
    /// ignored. `file-history-*` lines are rejected by a prefix check before any JSON parse.
    ///
    /// Folding rules (GROUNDING 2.3-2.4, 3.3):
    /// - user(origin human|task-notification|peer|scheduled, not a tool_result) -> Turn::Open;
    ///   interrupt text -> Turn::Interrupted.
    /// - system/turn_duration -> Turn::Closed{pending_agents}.
    /// - assistant tool_use -> last_tool + pending_tools, tail_looks_finished = false; text ->
    ///   last_text, tail_looks_finished = true; usage -> usage (skip model `<synthetic>`).
    ///   `thinking` ignored (gotcha 37).
    /// - user tool_result: answer last_tool; tail_looks_finished = handback; if `toolUseResult`
    ///   shows a launch (Agent `status:"async_launched"`, Bash `backgroundTaskId`, Monitor
    ///   `taskId`) insert open_bg; `resumedAgentId` re-opens an agent; TaskStop closes its id.
    /// - task notification, counted ONLY from user `origin.kind=="task-notification"` or
    ///   attachment `queued_command` with `commandMode=="task-notification"` (never
    ///   `queue-operation`, gotcha 20): for each task id, record a Notice; remove from open_bg
    ///   when the outcome is Some.
    /// - system/agents_killed: close every open agent with Some(Killed).
    /// - cost-state: set `exit`; close everything in open_bg (the process is gone).
    /// - any later conversational record clears `exit`.
    /// - metadata records: latest wins into titles / last_prompt / worktree / bridge; pr-link
    ///   appends to prs if new.
    pub fn apply_line(&mut self, line: &[u8]) {
        todo!()
    }

    fn apply(&mut self, ev: Event) {
        todo!()
    }

    /// Open background items by kind.
    pub fn open_count(&self, kind: BgKind) -> u16 {
        self.open_bg.values().filter(|b| b.kind == kind).count() as u16
    }
}

// ---------------------------------------------------------------------------------------------
// Private: wire structs and the event vocabulary between parse and fold
// ---------------------------------------------------------------------------------------------

/// One flat struct for every record type: all fields optional, unknown fields ignored. Cheaper
/// and more forgiving than an internally tagged enum (no buffering, no failure on new types).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireLine {
    #[serde(rename = "type")]
    ty: Option<String>,
    subtype: Option<String>,
    timestamp: Option<String>,
    git_branch: Option<String>,
    message: Option<WireMessage>,
    origin: Option<WireOrigin>,
    tool_use_result: Option<serde_json::Value>,
    attachment: Option<WireAttachment>,
    content: Option<serde_json::Value>,
    pending_background_agent_count: Option<u16>,
    ai_title: Option<String>,
    custom_title: Option<String>,
    agent_name: Option<String>,
    last_prompt: Option<String>,
    /// Absent vs explicit null matter here: null means "left the worktree".
    #[serde(default)]
    worktree_session: Option<Option<WireWorktree>>,
    pr_number: Option<u32>,
    #[serde(rename = "totalCostUSD")]
    total_cost_usd: Option<f64>,
    bridge_session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMessage {
    id: Option<String>,
    model: Option<String>,
    stop_reason: Option<String>,
    #[serde(default)]
    content: serde_json::Value,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize)]
struct WireOrigin {
    kind: Option<String>,
    handback: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAttachment {
    #[serde(rename = "type")]
    ty: Option<String>,
    command_mode: Option<String>,
    prompt: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireWorktree {
    worktree_path: Option<PathBuf>,
    worktree_name: Option<String>,
    worktree_branch: Option<String>,
}

/// Tool inputs we read. Note what is missing: Bash `command`. It is never deserialized, so it
/// cannot be displayed or logged by accident.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireToolInput {
    description: Option<String>,
    file_path: Option<String>,
    pattern: Option<String>,
    // ScheduleWakeup
    stop: Option<bool>,
}

/// What one line means, independent of its wire shape. One line can yield several events
/// (an assistant block with usage and a tool_use).
#[derive(Debug, PartialEq)]
enum Event {
    TurnOpened { at: At },
    Interrupted { at: At },
    TurnClosed { at: At, pending_agents: u16 },
    Text { at: At, first_line: SafeText, is_question: bool },
    ToolUse { at: At, id: ToolUseId, name: String, summary: SafeText },
    ToolResult {
        at: At,
        id: ToolUseId,
        launch: Option<(TaskId, BgKind)>,
        resumed: Option<TaskId>,
        stopped: Option<TaskId>,
        handback: bool,
    },
    Notified { at: At, tasks: Vec<(TaskId, Option<TaskOutcome>)> },
    AgentsKilled { at: At },
    Usage { ctx_tokens: u64, model: String, at: At },
    Wakeup { at: Option<At> },
    Branch(String),
    CustomTitle(String),
    AgentName(String),
    AiTitle(String),
    LastPrompt(String),
    AwaySummary(String),
    Worktree(Option<Worktree>),
    PrLink(u32),
    CostState(f64),
    Bridge(bool),
}

/// Line -> events. Pure. Returns nothing for unknown types; increments nothing (the caller
/// counts a failed parse as skipped).
fn events(line: &[u8], out: &mut Vec<Event>) -> Result<(), ()> {
    todo!()
}

/// Parse a `<task-notification>` body into (task id, outcome) pairs. One notification can list
/// several ids (orphan sweep); the fake `__orphan_summary__:*` id is dropped. Monitor events have
/// no `<status>`: `[Monitor expired …]` and `stream ended` yield `Some(Completed)`, any other
/// monitor event yields `None` (the monitor is still running).
fn parse_task_notification(xml: &str) -> Vec<(TaskId, Option<TaskOutcome>)> {
    todo!("tiny tag scanner; no XML crate")
}

/// The display summary of a tool_use: `description` ?? basename(`file_path`) ?? `pattern` ?? "".
/// Redacted. The tool name is prepended by the renderer, not here.
fn tool_summary(input: &WireToolInput) -> SafeText {
    todo!()
}

/// Does assistant text end in a question to the user? Trims trailing whitespace, `*`, `_`,
/// `` ` ``, quotes and `)`, then checks for `?`. Deliberately simple and documented as a
/// heuristic (`Status::Idle { asked_question }`).
fn is_question(text: &str) -> bool {
    todo!()
}
