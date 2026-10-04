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
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::de::IgnoredAny;

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
        let m = std::fs::metadata(path)?;
        let mtime_ms = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        Ok(FileStamp { ino: m.ino(), len: m.len(), mtime_ms })
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

    fn tail_sizes(self) -> &'static [u64] {
        match self {
            Window::Main => &[256 << 10, 1 << 20, 4 << 20],
            Window::Sidecar => &[64 << 10],
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
        let stamp = FileStamp::of(&path)?;
        let mut file = File::open(&path)?;
        let (start, bytes) = window_start(&mut file, stamp.len, window)?;
        let facts = if start == 0 { Facts::from_start() } else { Facts::default() };
        let mut t = Transcript { path, stamp, offset: start, partial: Vec::new(), window, facts };
        t.fold(&bytes);
        t.offset = start + bytes.len() as u64;
        Ok(t)
    }

    /// Fold newly appended complete lines. Returns whether `facts` changed. Cheap when nothing
    /// changed: one `stat`, compared to `stamp`. If the stamp shows replacement (inode change or
    /// `len < offset`), re-open from a fresh window.
    pub fn pump(&mut self) -> std::io::Result<bool> {
        let stamp = FileStamp::of(&self.path)?;
        if stamp.ino != self.stamp.ino || stamp.len < self.offset {
            *self = Transcript::open(std::mem::take(&mut self.path), self.window)?;
            return Ok(true);
        }
        if stamp.len == self.offset {
            self.stamp = stamp;
            return Ok(false);
        }
        let mut file = File::open(&self.path)?;
        let bytes = read_range(&mut file, self.offset, stamp.len)?;
        let changed = self.fold(&bytes);
        self.offset += bytes.len() as u64;
        self.stamp = stamp;
        Ok(changed)
    }

    /// Split `bytes` (prefixed by the held-back partial line) into complete lines and fold
    /// each; keep the trailing remainder. Returns whether any line was folded.
    fn fold(&mut self, bytes: &[u8]) -> bool {
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(bytes);
        let mut changed = false;
        let mut start = 0;
        while let Some(nl) = data[start..].iter().position(|&b| b == b'\n') {
            let line = &data[start..start + nl];
            if !line.iter().all(u8::is_ascii_whitespace) {
                self.facts.apply_line(line);
                changed = true;
            }
            start += nl + 1;
        }
        self.partial = data.split_off(start);
        changed
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

fn read_range(file: &mut File, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(end.saturating_sub(start) as usize);
    file.seek(SeekFrom::Start(start))?;
    file.take(end.saturating_sub(start)).read_to_end(&mut buf)?;
    Ok(buf)
}

const LAST_PROMPT_MARK: &[u8] = b"\"type\":\"last-prompt\"";

/// Choose the window start for `file` of length `len` and return it with the bytes from there
/// to `len`, so the caller folds what was already read. Pure over the bytes it reads; split out
/// because it is the one place the 256 KB / metadata-block policy lives.
fn window_start(file: &mut File, len: u64, window: Window) -> std::io::Result<(u64, Vec<u8>)> {
    let sizes = window.tail_sizes();
    for (i, &size) in sizes.iter().enumerate() {
        let last = i + 1 == sizes.len();
        let tail = len.saturating_sub(size);
        let mut buf = read_range(file, tail, len)?;
        if tail == 0 {
            return Ok((0, buf));
        }
        let Some(first) = buf.iter().position(|&b| b == b'\n').map(|p| p + 1) else {
            if last {
                return Ok((len, Vec::new()));
            }
            continue;
        };
        if window == Window::Main {
            if let Some(mark) = rfind(&buf[first..], LAST_PROMPT_MARK) {
                let rel = buf[first..first + mark].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
                let line_start = first + rel;
                buf.drain(..line_start);
                return Ok((tail + line_start as u64, buf));
            }
            if !last {
                continue;
            }
        }
        buf.drain(..first);
        return Ok((tail + first as u64, buf));
    }
    unreachable!("tail_sizes is never empty")
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
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
    /// Non-empty lines fed to the reducer.
    pub lines: u64,
    /// Lines that were not JSON, or conversational records without a usable timestamp.
    pub skipped: u64,
    /// Well-formed records whose `type` this build does not know. A diagnostic, never fatal.
    pub unknown: u64,
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
    /// Opened by a `user` record (human, task-notification, peer, scheduled), or inferred from
    /// assistant activity when the window started mid-turn.
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

const FILE_HISTORY_PREFIX: &[u8] = b"{\"type\":\"file-history-";

impl Facts {
    /// Empty facts whose horizon covers the whole file. `Transcript::open` uses this when the
    /// window starts at byte 0; tests use it to fold a complete fixture.
    pub fn from_start() -> Facts {
        Facts { horizon: Horizon { from_start: true, first_at: None }, ..Facts::default() }
    }

    /// The reducer. Parses one complete JSONL line and folds it in. Unknown record types are
    /// counted in `unknown`, unparseable lines in `skipped`, and both are otherwise ignored.
    /// `file-history-*` lines are rejected by a prefix check before any JSON parse.
    ///
    /// Folding rules (GROUNDING 2.3-2.4, 3.3):
    /// - user carrying `origin.kind` or `turnOrigin` and no tool_result -> Turn::Open;
    ///   interrupt text -> Turn::Interrupted. Local-command echoes carry neither and open nothing.
    /// - system/turn_duration -> Turn::Closed{pending_agents}.
    /// - assistant tool_use -> last_tool + pending_tools, tail_looks_finished = false; text ->
    ///   last_text, tail_looks_finished = true; usage -> usage (skip model `<synthetic>`).
    ///   `thinking` ignored (gotcha 37). Any of these while Turn::Unknown infers Turn::Open.
    /// - user tool_result: answer last_tool; tail_looks_finished = the tool was
    ///   SubagentHandback; if `toolUseResult` shows a launch (Agent `status:"async_launched"`,
    ///   Bash `backgroundTaskId`, Monitor `taskId`) insert open_bg; `resumedAgentId` re-opens
    ///   an agent; TaskStop closes its id; ScheduleWakeup sets or clears `wakeup`.
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
        let line = line.trim_ascii();
        if line.is_empty() {
            return;
        }
        self.lines += 1;
        if line.starts_with(FILE_HISTORY_PREFIX) {
            return;
        }
        let mut out = Vec::with_capacity(4);
        match events(line, &mut out) {
            Ok(()) => out.into_iter().for_each(|ev| self.apply(ev)),
            Err(Reject::Unparsable) => self.skipped += 1,
            Err(Reject::Unknown) => self.unknown += 1,
        }
    }

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::Seen { at } => {
                self.last_activity = Some(at);
                self.horizon.first_at.get_or_insert(at);
                self.exit = None;
            }
            Event::Branch(b) => self.branch = Some(b),
            Event::TurnOpened { at } => self.turn = Turn::Open { since: at },
            Event::Interrupted { at } => self.turn = Turn::Interrupted { at },
            Event::TurnClosed { at, pending_agents } => self.turn = Turn::Closed { at, pending_agents },
            Event::Text { at, first_line, is_question } => {
                self.infer_open(at);
                self.last_text = Some(TextBlock { first_line, at, is_question });
                self.tail_looks_finished = true;
            }
            Event::ToolUse { at, id, name, summary } => {
                self.infer_open(at);
                self.pending_tools
                    .insert(id.clone(), PendingTool { name: name.clone(), summary: summary.clone(), at });
                self.last_tool = Some(ToolCall { id, name, summary, at, answered: false });
                self.tail_looks_finished = false;
            }
            Event::ToolResult { at, id, signal } => {
                self.infer_open(at);
                if let Some(t) = &mut self.last_tool
                    && t.id == id
                {
                    t.answered = true;
                }
                let pending = self.pending_tools.remove(&id);
                self.tail_looks_finished =
                    pending.as_ref().is_some_and(|p| p.name == "SubagentHandback");
                self.apply_signal(at, id, pending, signal);
            }
            Event::Notified { at, tasks } => {
                for (task, outcome) in tasks {
                    if outcome.is_some() {
                        self.open_bg.remove(&task);
                    }
                    self.notices.insert(task, Notice { at, outcome });
                }
            }
            Event::AgentsKilled { at } => {
                let agents: Vec<TaskId> = self
                    .open_bg
                    .iter()
                    .filter(|(_, b)| b.kind == BgKind::Agent)
                    .map(|(k, _)| k.clone())
                    .collect();
                for a in agents {
                    self.open_bg.remove(&a);
                    self.notices.insert(a, Notice { at, outcome: Some(TaskOutcome::Killed) });
                }
            }
            Event::Usage { ctx_tokens, model, at } => self.usage = Some(Usage { ctx_tokens, model, at }),
            Event::WakeupFired => self.wakeup = None,
            Event::CustomTitle(t) => self.titles.custom = Some(SafeText::redact(&t)),
            Event::AgentName(t) => self.titles.agent_name = Some(SafeText::redact(&t)),
            Event::AiTitle(t) => self.titles.ai = Some(SafeText::redact(&t)),
            Event::LastPrompt(t) => self.last_prompt = Some(SafeText::redact(&t)),
            Event::AwaySummary(s) => {
                let s = s.trim_end();
                let s = s.strip_suffix("(disable recaps in /config)").unwrap_or(s);
                self.away_summary = Some(SafeText::redact(s.trim()));
            }
            Event::Worktree(w) => self.worktree = w,
            Event::PrLink(n) => {
                if !self.prs.contains(&n) {
                    self.prs.push(n);
                }
            }
            Event::CostState(cost_usd) => {
                self.exit = Some(ExitTotals { cost_usd });
                self.open_bg.clear();
                self.wakeup = None;
            }
            Event::Bridge(b) => self.bridge_active = b,
        }
    }

    /// Assistant activity only happens inside a turn, so a window that starts mid-turn can
    /// still report an open turn instead of `Unknown`.
    fn infer_open(&mut self, at: At) {
        if self.turn == Turn::Unknown {
            self.turn = Turn::Open { since: at };
        }
    }

    /// What a tool_result's `toolUseResult` says about background work, interpreted with the
    /// tool's name where the key alone is ambiguous (`taskId` is also on TaskUpdate results).
    fn apply_signal(&mut self, at: At, id: ToolUseId, pending: Option<PendingTool>, s: WireToolResult) {
        let name = pending.as_ref().map_or("", |p| p.name.as_str());
        let launch = if let Some(b) = s.background_task_id {
            Some((b, BgKind::Shell))
        } else if s.status.as_deref() == Some("async_launched") && s.agent_id.is_some() {
            s.agent_id.map(|a| (a, BgKind::Agent))
        } else if name == "Monitor" {
            s.monitor_task_id.map(|t| (t, BgKind::Monitor))
        } else {
            None
        };
        if let Some((task, kind)) = launch {
            let (label, started) = pending.map_or((SafeText::default(), at), |p| (p.summary, p.at));
            self.open_bg.insert(TaskId(task), BgItem { kind, label, started, tool_use: Some(id) });
            return;
        }
        if let Some(t) = s.stopped_task_id {
            let t = TaskId(t);
            self.open_bg.remove(&t);
            self.notices.insert(t, Notice { at, outcome: Some(TaskOutcome::Stopped) });
        }
        if let Some(a) = s.resumed_agent_id {
            self.open_bg.entry(TaskId(a)).or_insert(BgItem {
                kind: BgKind::Agent,
                label: SafeText::ours("resumed agent"),
                started: at,
                tool_use: Some(id),
            });
        }
        if s.stopped == Some(true) {
            self.wakeup = None;
        } else if let Some(ms) = s.scheduled_for.filter(|&ms| ms > 0.0) {
            self.wakeup = Some(At(ms as i64));
        }
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
    turn_origin: Option<String>,
    tool_use_result: Option<WireResult>,
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
struct WireMessage {
    model: Option<String>,
    content: Option<WireContent>,
    usage: Option<WireUsage>,
}

/// `message.content` / `attachment.prompt`: a plain string or a list of blocks. The third arm
/// keeps a line parseable when a block has a shape this build does not know.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Blocks(Vec<WireBlock>),
    Other(IgnoredAny),
}

impl WireContent {
    fn text(&self) -> String {
        match self {
            WireContent::Text(s) => s.clone(),
            WireContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
            WireContent::Other(_) => String::new(),
        }
    }

    fn blocks(&self) -> &[WireBlock] {
        match self {
            WireContent::Blocks(b) => b,
            _ => &[],
        }
    }
}

/// One content block. `tool_result` blocks carry a `content` field that can be 40 KB; it is
/// not declared here, so it is never materialised.
#[derive(Deserialize)]
struct WireBlock {
    #[serde(rename = "type")]
    ty: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<WireToolInput>,
    text: Option<String>,
    tool_use_id: Option<String>,
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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAttachment {
    #[serde(rename = "type")]
    ty: Option<String>,
    command_mode: Option<String>,
    prompt: Option<WireContent>,
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
#[derive(Deserialize, Default)]
struct WireToolInput {
    description: Option<String>,
    file_path: Option<String>,
    pattern: Option<String>,
}

/// `toolUseResult`: an object with the launch/stop signals we read, or anything else (error
/// strings), which is ignored without being materialised.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireResult {
    Signal(WireToolResult),
    Other(IgnoredAny),
}

/// The background-work signals a `toolUseResult` can carry (GROUNDING 2.4). TaskStop's result
/// also carries the raw `command`; it is not declared here.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
struct WireToolResult {
    /// Bash: `run_in_background`, or auto-backgrounded.
    #[serde(rename = "backgroundTaskId")]
    background_task_id: Option<String>,
    /// Agent: `async_launched` or `completed`. The input flag does not decide (gotcha 19).
    status: Option<String>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
    /// Monitor. TaskUpdate results carry the same key, so the tool name decides.
    #[serde(rename = "taskId")]
    monitor_task_id: Option<String>,
    /// TaskStop.
    #[serde(rename = "task_id")]
    stopped_task_id: Option<String>,
    /// SendMessage to an agent that had stopped.
    #[serde(rename = "resumedAgentId")]
    resumed_agent_id: Option<String>,
    /// ScheduleWakeup: epoch ms; `0` with `stopped: true` on cancel.
    #[serde(rename = "scheduledFor")]
    scheduled_for: Option<f64>,
    stopped: Option<bool>,
}

/// What one line means, independent of its wire shape. One line can yield several events
/// (an assistant block with usage and a tool_use).
#[derive(Debug, PartialEq)]
enum Event {
    /// Any conversational record (user, assistant, system, attachment).
    Seen { at: At },
    TurnOpened { at: At },
    Interrupted { at: At },
    TurnClosed { at: At, pending_agents: u16 },
    Text { at: At, first_line: SafeText, is_question: bool },
    ToolUse { at: At, id: ToolUseId, name: String, summary: SafeText },
    ToolResult { at: At, id: ToolUseId, signal: WireToolResult },
    Notified { at: At, tasks: Vec<(TaskId, Option<TaskOutcome>)> },
    AgentsKilled { at: At },
    Usage { ctx_tokens: u64, model: String, at: At },
    WakeupFired,
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

#[derive(Debug, PartialEq)]
enum Reject {
    Unparsable,
    Unknown,
}

/// Known metadata types that carry nothing the dashboard shows.
const IGNORED_TYPES: &[&str] = &[
    "atis-latch",
    "mode",
    "permission-mode",
    "queue-operation",
    "relocated",
    "agent-setting",
    "frame-link",
    "continued-in",
    "fork-context-ref",
];

/// Line -> events. Pure.
fn events(line: &[u8], out: &mut Vec<Event>) -> Result<(), Reject> {
    let w: WireLine = serde_json::from_slice(line).map_err(|_| Reject::Unparsable)?;
    let Some(ty) = w.ty.as_deref() else { return Err(Reject::Unknown) };
    match ty {
        "user" | "assistant" | "system" | "attachment" => {
            let at = w.timestamp.as_deref().and_then(At::parse_iso).ok_or(Reject::Unparsable)?;
            out.push(Event::Seen { at });
            if let Some(b) = w.git_branch.clone() {
                out.push(Event::Branch(b));
            }
            match ty {
                "user" => user_events(&w, at, out),
                "assistant" => assistant_events(&w, at, out),
                "system" => system_events(&w, at, out),
                _ => attachment_events(&w, at, out),
            }
        }
        "ai-title" => out.extend(w.ai_title.map(Event::AiTitle)),
        "custom-title" => out.extend(w.custom_title.map(Event::CustomTitle)),
        "agent-name" => out.extend(w.agent_name.map(Event::AgentName)),
        "last-prompt" => out.extend(w.last_prompt.map(Event::LastPrompt)),
        "worktree-state" => {
            if let Some(ws) = w.worktree_session {
                out.push(Event::Worktree(ws.and_then(|ws| {
                    Some(Worktree {
                        path: ws.worktree_path?,
                        name: ws.worktree_name.unwrap_or_default(),
                        branch: ws.worktree_branch,
                    })
                })));
            }
        }
        "pr-link" => out.extend(w.pr_number.map(Event::PrLink)),
        "cost-state" => out.extend(w.total_cost_usd.map(Event::CostState)),
        "bridge-session" => {
            out.push(Event::Bridge(w.bridge_session_id.as_deref().is_some_and(|s| !s.is_empty())));
        }
        t if IGNORED_TYPES.contains(&t) || t.starts_with("file-history-") || t.starts_with("artifact-") => {}
        _ => return Err(Reject::Unknown),
    }
    Ok(())
}

const INTERRUPT_MARK: &str = "[Request interrupted by user";

fn user_events(w: &WireLine, at: At, out: &mut Vec<Event>) {
    let content = w.message.as_ref().and_then(|m| m.content.as_ref());
    let results: Vec<&WireBlock> = content
        .map(WireContent::blocks)
        .unwrap_or_default()
        .iter()
        .filter(|b| b.ty.as_deref() == Some("tool_result"))
        .collect();
    if !results.is_empty() {
        let signal = match &w.tool_use_result {
            Some(WireResult::Signal(s)) => s.clone(),
            _ => WireToolResult::default(),
        };
        for r in results {
            if let Some(id) = &r.tool_use_id {
                out.push(Event::ToolResult { at, id: ToolUseId(id.clone()), signal: signal.clone() });
            }
        }
        return;
    }
    let text = content.map(WireContent::text).unwrap_or_default();
    let kind = w.origin.as_ref().and_then(|o| o.kind.as_deref());
    if kind == Some("task-notification") {
        out.push(Event::Notified { at, tasks: parse_task_notification(&text) });
    }
    if text.contains(INTERRUPT_MARK) {
        out.push(Event::Interrupted { at });
    } else if kind.is_some() || w.turn_origin.is_some() {
        out.push(Event::TurnOpened { at });
    }
}

fn assistant_events(w: &WireLine, at: At, out: &mut Vec<Event>) {
    let Some(m) = &w.message else { return };
    if let Some(u) = &m.usage {
        let model = m.model.clone().unwrap_or_default();
        if model != "<synthetic>" {
            let ctx_tokens = u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens;
            out.push(Event::Usage { ctx_tokens, model, at });
        }
    }
    for b in m.content.as_ref().map(WireContent::blocks).unwrap_or_default() {
        match b.ty.as_deref() {
            Some("tool_use") => {
                if let Some(id) = &b.id {
                    out.push(Event::ToolUse {
                        at,
                        id: ToolUseId(id.clone()),
                        name: b.name.clone().unwrap_or_default(),
                        summary: tool_summary(b.input.as_ref().unwrap_or(&WireToolInput::default())),
                    });
                }
            }
            Some("text") => {
                let text = b.text.as_deref().unwrap_or("").trim();
                if !text.is_empty() {
                    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                    out.push(Event::Text {
                        at,
                        first_line: SafeText::redact(first),
                        is_question: is_question(text),
                    });
                }
            }
            _ => {}
        }
    }
}

fn system_events(w: &WireLine, at: At, out: &mut Vec<Event>) {
    match w.subtype.as_deref() {
        Some("turn_duration") => out.push(Event::TurnClosed {
            at,
            pending_agents: w.pending_background_agent_count.unwrap_or(0),
        }),
        Some("agents_killed") => out.push(Event::AgentsKilled { at }),
        Some("scheduled_task_fire") => out.push(Event::WakeupFired),
        Some("away_summary") => {
            if let Some(s) = w.content.as_ref().and_then(|c| c.as_str()) {
                out.push(Event::AwaySummary(s.to_owned()));
            }
        }
        _ => {}
    }
}

fn attachment_events(w: &WireLine, at: At, out: &mut Vec<Event>) {
    let Some(a) = &w.attachment else { return };
    if a.ty.as_deref() == Some("queued_command")
        && a.command_mode.as_deref() == Some("task-notification")
        && let Some(prompt) = &a.prompt
    {
        out.push(Event::Notified { at, tasks: parse_task_notification(&prompt.text()) });
    }
}

/// Parse a `<task-notification>` body into (task id, outcome) pairs. One notification can list
/// several ids (orphan sweep); the fake `__orphan_summary__:*` id is dropped. Monitor events have
/// no `<status>`: `[Monitor expired …]` and `stream ended` yield `Some(Completed)`, any other
/// monitor event yields `None` (the monitor is still running). An unknown `<status>` value is
/// still terminal and maps to `Completed`.
fn parse_task_notification(xml: &str) -> Vec<(TaskId, Option<TaskOutcome>)> {
    let mut out = Vec::new();
    for block in xml.split("<task-notification>").skip(1) {
        let block = block.split("</task-notification>").next().unwrap_or(block);
        let event = tag(block, "event").unwrap_or("");
        let summary = tag(block, "summary").unwrap_or("");
        let outcome = match tag(block, "status").map(str::trim) {
            Some("completed") => Some(TaskOutcome::Completed),
            Some("failed") => Some(TaskOutcome::Failed),
            Some("killed") => Some(TaskOutcome::Killed),
            Some("stopped") => Some(TaskOutcome::Stopped),
            Some(_) => Some(TaskOutcome::Completed),
            None if event.trim_start().starts_with("[Monitor expired") || summary.contains("stream ended") => {
                Some(TaskOutcome::Completed)
            }
            None => None,
        };
        let mut rest = block;
        while let Some((id, after)) = tag_split(rest, "task-id") {
            let id = id.trim();
            if !id.is_empty() && !id.starts_with("__orphan_summary__") {
                out.push((TaskId(id.to_owned()), outcome));
            }
            rest = after;
        }
    }
    out
}

/// First `<name>…</name>` body in `s`, and the text after its closing tag.
fn tag_split<'a>(s: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let i = s.find(&open)? + open.len();
    let j = i + s[i..].find(&close)?;
    Some((&s[i..j], &s[j + close.len()..]))
}

fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    tag_split(s, name).map(|(body, _)| body)
}

/// The display summary of a tool_use: `description` ?? basename(`file_path`) ?? `pattern` ?? "".
/// Redacted. The tool name is prepended by the renderer, not here.
fn tool_summary(input: &WireToolInput) -> SafeText {
    let basename = input.file_path.as_deref().map(|p| p.rsplit('/').next().unwrap_or(p));
    let raw = input.description.as_deref().or(basename).or(input.pattern.as_deref()).unwrap_or("");
    SafeText::redact(raw)
}

/// Does assistant text end in a question to the user? Trims trailing whitespace, `*`, `_`,
/// `` ` ``, quotes and `)`, then checks for `?`. Deliberately simple and documented as a
/// heuristic (`Status::Idle { asked_question }`).
fn is_question(text: &str) -> bool {
    text.trim_end_matches(|c: char| c.is_whitespace() || "*_`\"')".contains(c)).ends_with('?')
}
