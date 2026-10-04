use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::model::{At, SafeText, TaskId, TaskOutcome, ToolUseId};

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
        Ok(FileStamp {
            ino: m.ino(),
            len: m.len(),
            mtime_ms,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    Main,
    Sidecar,
}

impl Window {
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

pub struct Transcript {
    path: PathBuf,
    stamp: FileStamp,
    offset: u64,
    partial: Vec<u8>,
    window: Window,
    facts: Facts,
}

impl Transcript {
    pub fn open(path: PathBuf, window: Window) -> std::io::Result<Transcript> {
        let stamp = FileStamp::of(&path)?;
        let mut file = File::open(&path)?;
        let (start, bytes) = window_start(&mut file, stamp.len, window)?;
        let facts = if start == 0 {
            Facts::from_start()
        } else {
            Facts::default()
        };
        let mut t = Transcript {
            path,
            stamp,
            offset: start,
            partial: Vec::new(),
            window,
            facts,
        };
        t.fold(&bytes);
        t.offset = start + bytes.len() as u64;
        Ok(t)
    }

    pub fn window_bytes(&self) -> u64 {
        self.stamp.len.min(self.window.max_bytes())
    }

    // Transcripts are append-only (inode and prefix hash stable while growing, 2.1.289), so a
    // same-inode file at least as long as `offset` only gained lines.
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
        if window == Window::Main && !last && !contains(&buf[first..], LAST_PROMPT_MARK) {
            continue;
        }
        buf.drain(..first);
        return Ok((tail + first as u64, buf));
    }
    unreachable!("tail_sizes is never empty")
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub horizon: Horizon,
    pub turn: Turn,
    pub open_bg: BTreeMap<TaskId, BgItem>,
    pub notices: HashMap<TaskId, Notice>,
    pub last_tool: Option<ToolCall>,
    pub last_text: Option<TextBlock>,
    pub tail_looks_finished: bool,
    pub titles: Titles,
    pub last_prompt: Option<SafeText>,
    pub away_summary: Option<SafeText>,
    pub worktree: Option<Worktree>,
    pub branch: Option<String>,
    pub prs: Vec<u32>,
    pub usage: Option<Usage>,
    pub wakeup: Option<At>,
    pub exit: Option<ExitTotals>,
    pub bridge_active: bool,
    pub last_activity: Option<At>,
    pub lines: u64,
    pub skipped: u64,
    pub unknown: u64,
    pending_tools: HashMap<ToolUseId, PendingTool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Horizon {
    pub from_start: bool,
    pub first_at: Option<At>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Turn {
    #[default]
    Unknown,
    Open {
        since: At,
    },
    Closed {
        at: At,
        pending_agents: u16,
    },
    Interrupted {
        at: At,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct BgItem {
    pub kind: BgKind,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice {
    pub at: At,
    pub outcome: Option<TaskOutcome>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: ToolUseId,
    pub name: String,
    pub summary: SafeText,
    pub at: At,
    pub answered: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextBlock {
    pub first_line: SafeText,
    pub at: At,
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
    pub fn from_start() -> Facts {
        Facts {
            horizon: Horizon {
                from_start: true,
                first_at: None,
            },
            ..Facts::default()
        }
    }

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
            Event::TurnClosed { at, pending_agents } => {
                self.turn = Turn::Closed { at, pending_agents }
            }
            Event::Text {
                at,
                first_line,
                is_question,
            } => {
                self.infer_open(at);
                self.last_text = Some(TextBlock {
                    first_line,
                    at,
                    is_question,
                });
                self.tail_looks_finished = true;
            }
            Event::ToolUse {
                at,
                id,
                name,
                summary,
            } => {
                self.infer_open(at);
                self.pending_tools.insert(
                    id.clone(),
                    PendingTool {
                        name: name.clone(),
                        summary: summary.clone(),
                        at,
                    },
                );
                self.last_tool = Some(ToolCall {
                    id,
                    name,
                    summary,
                    at,
                    answered: false,
                });
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
                self.tail_looks_finished = pending
                    .as_ref()
                    .is_some_and(|p| p.name == "SubagentHandback");
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
                    self.notices.insert(
                        a,
                        Notice {
                            at,
                            outcome: Some(TaskOutcome::Killed),
                        },
                    );
                }
            }
            Event::Usage {
                ctx_tokens,
                model,
                at,
            } => {
                self.usage = Some(Usage {
                    ctx_tokens,
                    model,
                    at,
                })
            }
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

    fn infer_open(&mut self, at: At) {
        if self.turn == Turn::Unknown {
            self.turn = Turn::Open { since: at };
        }
    }

    fn apply_signal(
        &mut self,
        at: At,
        id: ToolUseId,
        pending: Option<PendingTool>,
        s: WireToolResult,
    ) {
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
            self.open_bg.insert(
                TaskId(task),
                BgItem {
                    kind,
                    label,
                    started,
                    tool_use: Some(id),
                },
            );
            return;
        }
        if let Some(t) = s.stopped_task_id {
            let t = TaskId(t);
            self.open_bg.remove(&t);
            self.notices.insert(
                t,
                Notice {
                    at,
                    outcome: Some(TaskOutcome::Stopped),
                },
            );
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

    pub fn open_count(&self, kind: BgKind) -> u16 {
        self.open_bg.values().filter(|b| b.kind == kind).count() as u16
    }
}

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
    /// Absent vs explicit null matter here: null means "left the worktree". serde folds a
    /// null `Option<Option<T>>` to `None`, so the inner option is wrapped explicitly.
    #[serde(default, deserialize_with = "present")]
    worktree_session: Option<Option<WireWorktree>>,
    pr_number: Option<u32>,
    #[serde(rename = "totalCostUSD")]
    total_cost_usd: Option<f64>,
    bridge_session_id: Option<String>,
}

fn present<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct WireMessage {
    model: Option<String>,
    content: Option<WireContent>,
    usage: Option<WireUsage>,
}

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

#[derive(Deserialize, Default)]
struct WireToolInput {
    description: Option<String>,
    file_path: Option<String>,
    pattern: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireResult {
    Signal(WireToolResult),
    Other(IgnoredAny),
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
struct WireToolResult {
    #[serde(rename = "backgroundTaskId")]
    background_task_id: Option<String>,
    status: Option<String>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
    #[serde(rename = "taskId")]
    monitor_task_id: Option<String>,
    #[serde(rename = "task_id")]
    stopped_task_id: Option<String>,
    #[serde(rename = "resumedAgentId")]
    resumed_agent_id: Option<String>,
    #[serde(rename = "scheduledFor")]
    scheduled_for: Option<f64>,
    stopped: Option<bool>,
}

#[derive(Debug, PartialEq)]
enum Event {
    Seen {
        at: At,
    },
    TurnOpened {
        at: At,
    },
    Interrupted {
        at: At,
    },
    TurnClosed {
        at: At,
        pending_agents: u16,
    },
    Text {
        at: At,
        first_line: SafeText,
        is_question: bool,
    },
    ToolUse {
        at: At,
        id: ToolUseId,
        name: String,
        summary: SafeText,
    },
    ToolResult {
        at: At,
        id: ToolUseId,
        signal: WireToolResult,
    },
    Notified {
        at: At,
        tasks: Vec<(TaskId, Option<TaskOutcome>)>,
    },
    AgentsKilled {
        at: At,
    },
    Usage {
        ctx_tokens: u64,
        model: String,
        at: At,
    },
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

fn events(line: &[u8], out: &mut Vec<Event>) -> Result<(), Reject> {
    let w: WireLine = serde_json::from_slice(line).map_err(|_| Reject::Unparsable)?;
    let Some(ty) = w.ty.as_deref() else {
        return Err(Reject::Unknown);
    };
    match ty {
        "user" | "assistant" | "system" | "attachment" => {
            let at = w
                .timestamp
                .as_deref()
                .and_then(At::parse_iso)
                .ok_or(Reject::Unparsable)?;
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
            out.push(Event::Bridge(
                w.bridge_session_id
                    .as_deref()
                    .is_some_and(|s| !s.is_empty()),
            ));
        }
        t if IGNORED_TYPES.contains(&t)
            || t.starts_with("file-history-")
            || t.starts_with("artifact-") => {}
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
                out.push(Event::ToolResult {
                    at,
                    id: ToolUseId(id.clone()),
                    signal: signal.clone(),
                });
            }
        }
        return;
    }
    let text = content.map(WireContent::text).unwrap_or_default();
    let kind = w.origin.as_ref().and_then(|o| o.kind.as_deref());
    if kind == Some("task-notification") {
        out.push(Event::Notified {
            at,
            tasks: parse_task_notification(&text),
        });
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
            let ctx_tokens =
                u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens;
            out.push(Event::Usage {
                ctx_tokens,
                model,
                at,
            });
        }
    }
    for b in m
        .content
        .as_ref()
        .map(WireContent::blocks)
        .unwrap_or_default()
    {
        match b.ty.as_deref() {
            Some("tool_use") => {
                if let Some(id) = &b.id {
                    out.push(Event::ToolUse {
                        at,
                        id: ToolUseId(id.clone()),
                        name: b.name.clone().unwrap_or_default(),
                        summary: tool_summary(
                            b.input.as_ref().unwrap_or(&WireToolInput::default()),
                        ),
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
        out.push(Event::Notified {
            at,
            tasks: parse_task_notification(&prompt.text()),
        });
    }
}

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
            None if event.trim_start().starts_with("[Monitor expired")
                || summary.contains("stream ended") =>
            {
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

fn tool_summary(input: &WireToolInput) -> SafeText {
    let basename = input
        .file_path
        .as_deref()
        .map(|p| p.rsplit('/').next().unwrap_or(p));
    let raw = input
        .description
        .as_deref()
        .or(basename)
        .or(input.pattern.as_deref())
        .unwrap_or("");
    SafeText::redact(raw)
}

fn is_question(text: &str) -> bool {
    text.trim_end_matches(|c: char| c.is_whitespace() || "*_`\"')".contains(c))
        .ends_with('?')
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::fs;
    use std::io::Write as _;

    pub(crate) const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    const SENTINEL: &str = "SECRET_SENTINEL_x9";

    pub(crate) fn ts(n: u32) -> String {
        format!("2026-01-02T10:{:02}:{:02}.000Z", n / 60, n % 60)
    }
    pub(crate) fn at(n: u32) -> At {
        At::parse_iso(&ts(n)).unwrap()
    }
    fn line(v: &Value) -> Vec<u8> {
        serde_json::to_vec(v).unwrap()
    }
    fn conv(n: u32, ty: &str, extra: Value) -> Value {
        let mut v = json!({
            "type": ty, "timestamp": ts(n), "uuid": format!("u{n}"), "cwd": "/Users/dev/work/demo",
            "sessionId": "s-1", "version": "2.1.289", "gitBranch": "feat/x", "isSidechain": false,
        });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }
    fn human(n: u32, text: &str) -> Value {
        conv(
            n,
            "user",
            json!({"message": {"role": "user", "content": text}, "origin": {"kind": "human"}, "turnOrigin": "human"}),
        )
    }
    fn notification(n: u32, xml: &str) -> Value {
        conv(
            n,
            "user",
            json!({"message": {"role": "user", "content": xml}, "origin": {"kind": "task-notification"}}),
        )
    }
    fn queued(n: u32, xml: &str) -> Value {
        conv(
            n,
            "attachment",
            json!({"attachment": {"type": "queued_command", "prompt": xml, "commandMode": "task-notification", "origin": {"kind": "task-notification", "producer": "session-task"}}}),
        )
    }
    fn queue_op(n: u32, xml: &str) -> Value {
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": ts(n), "sessionId": "s-1", "content": xml})
    }
    fn tool_use(n: u32, id: &str, name: &str, input: Value) -> Value {
        conv(
            n,
            "assistant",
            json!({"message": {"model": "claude-opus-5-5", "id": format!("m{n}"), "role": "assistant",
            "content": [{"type": "tool_use", "id": id, "name": name, "input": input}], "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 100}}}),
        )
    }
    fn tool_result(n: u32, id: &str, result: Value) -> Value {
        conv(
            n,
            "user",
            json!({"message": {"role": "user", "content": [{"tool_use_id": id, "type": "tool_result", "content": "ok"}]}, "toolUseResult": result}),
        )
    }
    fn text_from(n: u32, s: &str, model: &str, ctx: u64) -> Value {
        conv(
            n,
            "assistant",
            json!({"message": {"model": model, "id": format!("m{n}"), "role": "assistant",
            "content": [{"type": "text", "text": s}], "stop_reason": "end_turn",
            "usage": {"input_tokens": ctx, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}}),
        )
    }
    fn text(n: u32, s: &str) -> Value {
        text_from(n, s, "claude-opus-5-5", 1000)
    }
    fn system(n: u32, subtype: &str, extra: Value) -> Value {
        let mut v = conv(n, "system", json!({"subtype": subtype}));
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }
    fn turn_duration(n: u32, pending: Option<u16>) -> Value {
        match pending {
            Some(p) => system(
                n,
                "turn_duration",
                json!({"durationMs": 1000, "pendingBackgroundAgentCount": p}),
            ),
            None => system(n, "turn_duration", json!({"durationMs": 1000})),
        }
    }
    fn notif_xml(ids: &[&str], status: Option<&str>, extra: &str) -> String {
        let ids: String = ids
            .iter()
            .map(|i| format!("<task-id>{i}</task-id>\n"))
            .collect();
        let status = status
            .map(|s| format!("<status>{s}</status>\n"))
            .unwrap_or_default();
        format!(
            "<task-notification>\n{ids}{status}<summary>x</summary>\n{extra}</task-notification>"
        )
    }
    fn launch_agent(n: u32, tu: &str, agent: &str, description: &str) -> [Value; 2] {
        [
            tool_use(
                n,
                tu,
                "Agent",
                json!({"description": description, "prompt": "p", "subagent_type": "Explore", "run_in_background": "true"}),
            ),
            tool_result(
                n + 1,
                tu,
                json!({"isAsync": true, "status": "async_launched", "agentId": agent, "description": description}),
            ),
        ]
    }
    fn launch_shell(n: u32, tu: &str, task: &str, description: &str) -> [Value; 2] {
        [
            tool_use(
                n,
                tu,
                "Bash",
                json!({"command": format!("{SENTINEL} npm test"), "description": description, "run_in_background": true}),
            ),
            tool_result(
                n + 1,
                tu,
                json!({"stdout": "", "stderr": "", "interrupted": false, "backgroundTaskId": task}),
            ),
        ]
    }
    fn fold(lines: &[Value]) -> Facts {
        let mut f = Facts::from_start();
        for l in lines {
            f.apply_line(&line(l));
        }
        f
    }
    fn fold_into(f: &mut Facts, lines: &[Value]) {
        for l in lines {
            f.apply_line(&line(l));
        }
    }
    fn fold_file(path: &Path) -> Facts {
        let mut f = Facts::from_start();
        for l in fs::read(path).unwrap().split(|&b| b == b'\n') {
            f.apply_line(l);
        }
        f
    }
    fn open_ids(f: &Facts) -> Vec<&str> {
        f.open_bg.keys().map(|k| k.0.as_str()).collect()
    }
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("agentically-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn turn_opens_on_prompt_and_closes_with_pending_agent_count() {
        let mut lines = vec![human(0, "Audit it")];
        lines.extend(launch_agent(1, "t1", "a1", "Launch audit"));
        lines.push(text(3, "Launched."));
        lines.push(turn_duration(4, Some(3)));
        let mut f = fold(&lines);
        assert_eq!(
            f.turn,
            Turn::Closed {
                at: at(4),
                pending_agents: 3
            }
        );
        assert_eq!(open_ids(&f), ["a1"]);
        assert_eq!(
            f.open_bg[&TaskId("a1".into())].label.as_str(),
            "Launch audit"
        );
        assert_eq!(f.open_bg[&TaskId("a1".into())].started, at(1));
        assert!(f.last_tool.as_ref().unwrap().answered);
        fold_into(&mut f, &[human(5, "Next")]);
        assert_eq!(f.turn, Turn::Open { since: at(5) });
        fold_into(&mut f, &[turn_duration(6, None)]);
        assert_eq!(
            f.turn,
            Turn::Closed {
                at: at(6),
                pending_agents: 0
            }
        );
    }

    #[test]
    fn interrupt_without_turn_duration_is_not_an_open_turn() {
        let interrupt = conv(
            2,
            "user",
            json!({"message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}}),
        );
        let f = fold(&[
            human(0, "go"),
            tool_use(1, "t1", "Read", json!({"file_path": "/a/b.rs"})),
            interrupt,
        ]);
        assert_eq!(f.turn, Turn::Interrupted { at: at(2) });
    }

    #[test]
    fn tool_results_and_local_command_echoes_do_not_open_turns() {
        let mut f = fold(&[human(0, "go"), text(1, "done"), turn_duration(2, None)]);
        let echo = conv(
            3,
            "user",
            json!({"message": {"role": "user", "content": "<command-name>/effort</command-name>"}}),
        );
        let companion = conv(
            4,
            "user",
            json!({"isMeta": true, "turnCompanion": true, "message": {"role": "user", "content": [{"type": "text", "text": "reminder"}]}}),
        );
        fold_into(&mut f, &[echo, companion, tool_result(5, "t9", json!({}))]);
        assert_eq!(
            f.turn,
            Turn::Closed {
                at: at(2),
                pending_agents: 0
            }
        );
    }

    #[test]
    fn window_starting_mid_turn_infers_an_open_turn() {
        let mut f = Facts::default();
        fold_into(
            &mut f,
            &[
                tool_use(1, "t1", "Read", json!({"file_path": "/a/b.rs"})),
                tool_result(2, "t1", json!({})),
            ],
        );
        assert_eq!(f.turn, Turn::Open { since: at(1) });
        assert!(!f.horizon.from_start);
        assert_eq!(f.horizon.first_at, Some(at(1)));
    }

    #[test]
    fn completion_counts_from_delivery_records_never_from_queue_operation() {
        let mut f = fold(&launch_shell(1, "t1", "b1", "Run tests"));
        let done = notif_xml(&["b1"], Some("completed"), "");
        fold_into(&mut f, &[queue_op(3, &done)]);
        assert_eq!(open_ids(&f), ["b1"], "queue-operation must not count");
        assert!(f.notices.is_empty());
        fold_into(&mut f, &[queued(4, &done)]);
        assert!(f.open_bg.is_empty());
        assert_eq!(
            f.notices[&TaskId("b1".into())],
            Notice {
                at: at(4),
                outcome: Some(TaskOutcome::Completed)
            }
        );
        fold_into(&mut f, &[notification(5, &done)]);
        assert_eq!(
            f.notices[&TaskId("b1".into())].at,
            at(5),
            "latest notice wins"
        );
        assert_eq!(
            f.turn,
            Turn::Open { since: at(5) },
            "a delivered notification opens a turn"
        );
    }

    #[test]
    fn orphan_sweep_closes_every_listed_id_and_drops_the_fake_one() {
        let mut lines: Vec<Value> = launch_shell(1, "t1", "b1", "one").into();
        lines.extend(launch_shell(3, "t2", "b2", "two"));
        lines.extend(launch_agent(5, "t3", "a1", "three"));
        let mut f = fold(&lines);
        assert_eq!(open_ids(&f), ["a1", "b1", "b2"]);
        let sweep = notif_xml(
            &["b1", "b2", "a1", "__orphan_summary__:shell"],
            Some("stopped"),
            "",
        );
        fold_into(&mut f, &[notification(8, &sweep)]);
        assert!(f.open_bg.is_empty());
        assert_eq!(f.notices.len(), 3);
        assert!(
            f.notices
                .values()
                .all(|n| n.outcome == Some(TaskOutcome::Stopped))
        );
        assert!(f.notices.keys().all(|k| !k.0.starts_with("__orphan")));
    }

    #[test]
    fn monitor_event_without_status_keeps_it_open_until_expiry() {
        let mut f = fold(&[
            tool_use(
                1,
                "t1",
                "Monitor",
                json!({"command": format!("{SENTINEL} gh run watch"), "description": "Watch CI", "timeout_ms": 1800000}),
            ),
            tool_result(
                2,
                "t1",
                json!({"taskId": "b9", "timeoutMs": 1800000, "persistent": false}),
            ),
            tool_use(
                3,
                "t2",
                "TaskUpdate",
                json!({"taskId": "1", "status": "completed"}),
            ),
            tool_result(
                4,
                "t2",
                json!({"success": true, "taskId": "1", "updatedFields": ["status"]}),
            ),
        ]);
        assert_eq!(open_ids(&f), ["b9"]);
        assert_eq!(f.open_bg[&TaskId("b9".into())].kind, BgKind::Monitor);
        fold_into(
            &mut f,
            &[queued(
                5,
                &notif_xml(&["b9"], None, "<event>run=in_progress</event>\n"),
            )],
        );
        assert_eq!(open_ids(&f), ["b9"], "an event is not a completion");
        assert_eq!(
            f.notices[&TaskId("b9".into())],
            Notice {
                at: at(5),
                outcome: None
            }
        );
        fold_into(
            &mut f,
            &[queued(
                6,
                &notif_xml(
                    &["b9"],
                    None,
                    "<event>[Monitor expired after 30m with 1 events delivered.]</event>\n",
                ),
            )],
        );
        assert!(f.open_bg.is_empty());
        assert_eq!(
            f.notices[&TaskId("b9".into())].outcome,
            Some(TaskOutcome::Completed)
        );
    }

    #[test]
    fn resumed_agent_reopens_after_its_notice() {
        let mut f = fold(&launch_agent(1, "t1", "a1", "audit"));
        fold_into(
            &mut f,
            &[notification(3, &notif_xml(&["a1"], Some("completed"), ""))],
        );
        assert!(f.open_bg.is_empty());
        fold_into(
            &mut f,
            &[
                tool_use(
                    4,
                    "t2",
                    "SendMessage",
                    json!({"to": "a1", "summary": "more", "message": "also check docs"}),
                ),
                tool_result(
                    5,
                    "t2",
                    json!({"success": true, "message": "Resuming agent", "resumedAgentId": "a1"}),
                ),
            ],
        );
        assert_eq!(open_ids(&f), ["a1"]);
        assert_eq!(f.open_bg[&TaskId("a1".into())].kind, BgKind::Agent);
        fold_into(
            &mut f,
            &[notification(9, &notif_xml(&["a1"], Some("failed"), ""))],
        );
        assert!(f.open_bg.is_empty());
        assert_eq!(
            f.notices[&TaskId("a1".into())],
            Notice {
                at: at(9),
                outcome: Some(TaskOutcome::Failed)
            }
        );
    }

    #[test]
    fn agent_async_is_decided_by_the_result_not_the_input_flag() {
        let f = fold(&[
            tool_use(
                1,
                "t1",
                "Agent",
                json!({"description": "sync", "prompt": "p", "run_in_background": "true"}),
            ),
            tool_result(
                2,
                "t1",
                json!({"status": "completed", "agentId": "a2", "content": [{"type": "text", "text": "done"}]}),
            ),
            tool_use(
                3,
                "t2",
                "Agent",
                json!({"description": "unflagged", "prompt": "p"}),
            ),
            tool_result(
                4,
                "t2",
                json!({"isAsync": true, "status": "async_launched", "agentId": "a3"}),
            ),
            tool_use(
                5,
                "t3",
                "Agent",
                json!({"description": "refused", "prompt": "p", "run_in_background": "true"}),
            ),
            tool_result(6, "t3", json!("Error: Concurrent subagent limit reached")),
        ]);
        assert_eq!(open_ids(&f), ["a3"]);
        assert_eq!(f.skipped, 0);
    }

    #[test]
    fn bash_background_variants_open_shells_labelled_by_description() {
        let mut lines: Vec<Value> = launch_shell(1, "t1", "b1", "Run the suite").into();
        lines.push(tool_use(3, "t2", "Bash", json!({"command": format!("{SENTINEL} sleep 500"), "description": "Boot the dev stack"})));
        lines.push(tool_result(4, "t2", json!({"stdout": "", "stderr": "", "backgroundTaskId": "b2", "timedOutAfterMs": 120000})));
        let f = fold(&lines);
        assert_eq!(open_ids(&f), ["b1", "b2"]);
        assert!(f.open_bg.values().all(|b| b.kind == BgKind::Shell));
        assert_eq!(
            f.open_bg[&TaskId("b2".into())].label.as_str(),
            "Boot the dev stack"
        );
    }

    #[test]
    fn bash_command_never_reaches_facts() {
        let mut lines: Vec<Value> = launch_shell(1, "t1", "b1", "Run tests").into();
        lines.push(tool_use(3, "t2", "TaskStop", json!({"task_id": "b1"})));
        lines.push(tool_result(4, "t2", json!({"message": "stopped", "task_id": "b1", "task_type": "local_bash", "command": format!("{SENTINEL} npm test")})));
        let f = fold(&lines);
        assert!(!format!("{f:?}").contains(SENTINEL));
        assert!(f.open_bg.is_empty());
        assert_eq!(
            f.notices[&TaskId("b1".into())].outcome,
            Some(TaskOutcome::Stopped)
        );
    }

    #[test]
    fn agents_killed_closes_agents_only() {
        let mut lines: Vec<Value> = launch_shell(1, "t1", "b1", "shell").into();
        lines.extend(launch_agent(3, "t2", "a1", "one"));
        lines.extend(launch_agent(5, "t3", "a2", "two"));
        lines.push(system(7, "agents_killed", json!({})));
        let f = fold(&lines);
        assert_eq!(open_ids(&f), ["b1"]);
        assert_eq!(
            f.notices[&TaskId("a1".into())],
            Notice {
                at: at(7),
                outcome: Some(TaskOutcome::Killed)
            }
        );
        assert_eq!(
            f.notices[&TaskId("a2".into())].outcome,
            Some(TaskOutcome::Killed)
        );
    }

    #[test]
    fn cost_state_marks_exit_and_a_resume_clears_it() {
        let mut f = fold(&launch_agent(1, "t1", "a1", "one"));
        fold_into(
            &mut f,
            &[
                json!({"type": "cost-state", "sessionId": "s-1", "totalCostUSD": 1.25, "startTime": 1}),
            ],
        );
        assert_eq!(f.exit, Some(ExitTotals { cost_usd: 1.25 }));
        assert!(f.open_bg.is_empty());
        fold_into(
            &mut f,
            &[json!({"type": "last-prompt", "lastPrompt": "x", "sessionId": "s-1"})],
        );
        assert!(f.exit.is_some(), "metadata after cost-state is still exit");
        fold_into(&mut f, &[human(3, "resume")]);
        assert_eq!(f.exit, None);
    }

    #[test]
    fn metadata_latest_wins_prs_dedupe_and_synthetic_usage_ignored() {
        let f = fold(&[
            json!({"type": "ai-title", "aiTitle": "First"}),
            json!({"type": "ai-title", "aiTitle": "Second"}),
            json!({"type": "custom-title", "customTitle": "Renamed"}),
            json!({"type": "worktree-state", "worktreeSession": {"worktreePath": "/w/x", "worktreeName": "x", "worktreeBranch": "worktree-x"}}),
            json!({"type": "pr-link", "prNumber": 42, "prUrl": "u"}),
            json!({"type": "pr-link", "prNumber": 42, "prUrl": "u"}),
            json!({"type": "pr-link", "prNumber": 43, "prUrl": "v"}),
            json!({"type": "bridge-session", "bridgeSessionId": "cse_abc"}),
            text_from(1, "real", "claude-opus-5-5", 5000),
            text_from(2, "fake", "<synthetic>", 0),
            json!({"type": "worktree-state", "worktreeSession": null}),
            json!({"type": "bridge-session", "bridgeSessionId": ""}),
            system(
                3,
                "away_summary",
                json!({"content": "Watching CI for you. (disable recaps in /config)"}),
            ),
        ]);
        assert_eq!(f.titles.ai.as_ref().unwrap().as_str(), "Second");
        assert_eq!(f.titles.custom.as_ref().unwrap().as_str(), "Renamed");
        assert_eq!(f.worktree, None);
        assert_eq!(f.prs, [42, 43]);
        assert_eq!(f.usage.as_ref().unwrap().ctx_tokens, 5000);
        assert!(!f.bridge_active);
        assert_eq!(
            f.away_summary.as_ref().unwrap().as_str(),
            "Watching CI for you."
        );
        assert_eq!(f.last_text.as_ref().unwrap().first_line.as_str(), "fake");
    }

    #[test]
    fn sidecar_tail_tracks_finishedness() {
        let mut f = fold_file(&Path::new(FIXTURES).join("transcript/sidecar_handback.jsonl"));
        assert!(f.tail_looks_finished, "handback tool_result ends the tail");
        assert_eq!(
            f.last_activity,
            Some(At::parse_iso("2026-01-02T10:00:14.000Z").unwrap())
        );
        assert_eq!(f.last_tool.as_ref().unwrap().name, "SubagentHandback");
        fold_into(
            &mut f,
            &[tool_use(20, "t5", "Read", json!({"file_path": "/a/b.rs"}))],
        );
        assert!(!f.tail_looks_finished);
        fold_into(&mut f, &[tool_result(21, "t5", json!({}))]);
        assert!(
            !f.tail_looks_finished,
            "an ordinary tool_result is mid-work"
        );
        fold_into(&mut f, &[text(22, "All done.")]);
        assert!(f.tail_looks_finished);
    }

    #[test]
    fn wakeup_is_replaced_cancelled_and_fired() {
        let mut f = fold(&[
            tool_use(
                1,
                "t1",
                "ScheduleWakeup",
                json!({"delaySeconds": 60, "prompt": "check"}),
            ),
            tool_result(
                2,
                "t1",
                json!({"scheduledFor": 1_700_000_060_000u64, "clampedDelaySeconds": 60}),
            ),
        ]);
        assert_eq!(f.wakeup, Some(At(1_700_000_060_000)));
        fold_into(
            &mut f,
            &[
                tool_use(3, "t2", "ScheduleWakeup", json!({"delaySeconds": 120})),
                tool_result(4, "t2", json!({"scheduledFor": 1_700_000_120_000u64})),
            ],
        );
        assert_eq!(f.wakeup, Some(At(1_700_000_120_000)));
        fold_into(
            &mut f,
            &[
                tool_use(5, "t3", "ScheduleWakeup", json!({"stop": true})),
                tool_result(
                    6,
                    "t3",
                    json!({"scheduledFor": 0, "stopped": true, "cancelledWakeups": 1}),
                ),
            ],
        );
        assert_eq!(f.wakeup, None);
        fold_into(
            &mut f,
            &[
                tool_use(7, "t4", "ScheduleWakeup", json!({"delaySeconds": 5})),
                tool_result(8, "t4", json!({"scheduledFor": 1_700_000_200_000u64})),
                system(9, "scheduled_task_fire", json!({"taskId": "w1"})),
            ],
        );
        assert_eq!(f.wakeup, None);
    }

    #[test]
    fn question_heuristic_trims_markdown_and_quotes() {
        assert!(is_question("Shall I proceed?**"));
        assert!(is_question("Want me to wire it now?\"  "));
        assert!(is_question("Ready? (y/n)".trim_end_matches(" (y/n)")));
        assert!(!is_question("Done."));
        assert!(!is_question(""));
        let f = fold(&[text(1, "CI is still running.\n\nShall I wait for it?")]);
        assert!(f.last_text.as_ref().unwrap().is_question);
        assert_eq!(
            f.last_text.as_ref().unwrap().first_line.as_str(),
            "CI is still running."
        );
    }

    #[test]
    fn garbage_unknown_and_file_history_lines_are_counted_not_fatal() {
        let mut f = Facts::from_start();
        f.apply_line(b"not json at all");
        assert_eq!((f.lines, f.skipped, f.unknown), (1, 1, 0));
        f.apply_line(br#"{"type":"brand-new-record","payload":1}"#);
        assert_eq!((f.lines, f.skipped, f.unknown), (2, 1, 1));
        f.apply_line(br#"{"type":"file-history-snapshot","messageId":"x", this is not valid json"#);
        assert_eq!(
            (f.lines, f.skipped, f.unknown),
            (3, 1, 1),
            "file-history is rejected before parsing"
        );
        f.apply_line(br#"{"type":"user","message":{"role":"user","content":"no timestamp"}}"#);
        assert_eq!(
            f.skipped, 2,
            "a conversational record without a timestamp is unusable"
        );
        f.apply_line(b"   ");
        assert_eq!(f.lines, 4);
    }

    #[test]
    fn fixture_session_folds_to_expected_facts() {
        let f = fold_file(&Path::new(FIXTURES).join("transcript/session.jsonl"));
        assert_eq!((f.skipped, f.unknown), (0, 0));
        assert_eq!(
            f.turn,
            Turn::Closed {
                at: At::parse_iso("2026-01-02T10:03:06.000Z").unwrap(),
                pending_agents: 1
            }
        );
        assert_eq!(open_ids(&f), ["a1111111111111111", "b5e6f7a8b"]);
        assert_eq!(f.open_bg[&TaskId("b5e6f7a8b".into())].kind, BgKind::Monitor);
        assert_eq!(
            f.open_bg[&TaskId("b5e6f7a8b".into())].label.as_str(),
            "Watch CI for PR 42"
        );
        assert_eq!(
            f.notices[&TaskId("b1a2b3c4d".into())].outcome,
            Some(TaskOutcome::Completed)
        );
        assert_eq!(f.notices[&TaskId("b5e6f7a8b".into())].outcome, None);
        assert_eq!(
            f.titles.ai.as_ref().unwrap().as_str(),
            "Retry logic for fetch helper"
        );
        assert_eq!(
            f.last_prompt.as_ref().unwrap().as_str(),
            "Add retry to the fetch helper"
        );
        assert_eq!(f.prs, [42, 43]);
        assert_eq!(f.worktree.as_ref().unwrap().name, "retry");
        assert_eq!(f.branch.as_deref(), Some("feat/retry"));
        assert_eq!(f.usage.as_ref().unwrap().ctx_tokens, 6 + 300 + 3100);
        assert!(f.last_text.as_ref().unwrap().is_question);
        assert_eq!(f.last_tool.as_ref().unwrap().summary.as_str(), "fetch.rs");
        assert!(!format!("{f:?}").contains(SENTINEL));
    }

    #[test]
    fn pump_is_split_invariant() {
        let fixture = fs::read(Path::new(FIXTURES).join("transcript/session.jsonl")).unwrap();
        let whole = fold_file(&Path::new(FIXTURES).join("transcript/session.jsonl"));
        for chunk in [1usize, 7, 4096, 39_000] {
            let dir = scratch(&format!("split-{chunk}"));
            let path = dir.join("s.jsonl");
            fs::write(&path, b"").unwrap();
            let mut t = Transcript::open(path.clone(), Window::Main).unwrap();
            assert!(!t.pump().unwrap(), "quiet pump reports no change");
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            for piece in fixture.chunks(chunk) {
                file.write_all(piece).unwrap();
                file.flush().unwrap();
                t.pump().unwrap();
            }
            assert_eq!(*t.facts(), whole, "chunk size {chunk}");
            assert!(!t.pump().unwrap());
        }
    }

    #[test]
    fn pump_reopens_when_the_file_is_replaced_or_truncated() {
        let dir = scratch("replace");
        let path = dir.join("s.jsonl");
        fs::copy(Path::new(FIXTURES).join("transcript/session.jsonl"), &path).unwrap();
        let mut t = Transcript::open(path.clone(), Window::Main).unwrap();
        assert_eq!(t.facts().prs, [42, 43]);
        fs::remove_file(&path).unwrap();
        fs::write(
            &path,
            [
                line(&json!({"type": "ai-title", "aiTitle": "Fresh"})),
                b"\n".to_vec(),
            ]
            .concat(),
        )
        .unwrap();
        assert!(t.pump().unwrap());
        assert_eq!(t.facts().titles.ai.as_ref().unwrap().as_str(), "Fresh");
        assert!(t.facts().prs.is_empty());
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(0).unwrap();
        f.write_all(&line(&json!({"type": "ai-title", "aiTitle": "S"})))
            .unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);
        assert!(t.pump().unwrap(), "a shrunk file (same inode) reopens");
        assert_eq!(t.facts().titles.ai.as_ref().unwrap().as_str(), "S");
    }

    fn padding_line(ty: &str, kb: usize) -> Vec<u8> {
        let mut v = line(&json!({"type": ty, "sessionId": "s-1", "pad": "x".repeat(kb * 1024)}));
        v.push(b'\n');
        v
    }

    fn metadata_layout(name: &str, padding_kb_before: usize, padding_kb_after: usize) -> PathBuf {
        let dir = scratch(name);
        let path = dir.join("big.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        for _ in 0..padding_kb_before {
            f.write_all(&padding_line("mode", 1)).unwrap();
        }
        for (ty, key, val) in [
            ("last-prompt", "lastPrompt", "old"),
            ("ai-title", "aiTitle", "Old"),
        ] {
            f.write_all(&line(&json!({"type": ty, key: val}))).unwrap();
            f.write_all(b"\n").unwrap();
        }
        for _ in 0..100 {
            f.write_all(&padding_line("mode", 1)).unwrap();
        }
        for (ty, key, val) in [
            ("last-prompt", "lastPrompt", "new"),
            ("ai-title", "aiTitle", "New"),
        ] {
            f.write_all(&line(&json!({"type": ty, key: val}))).unwrap();
            f.write_all(b"\n").unwrap();
        }
        f.write_all(&line(&human(7, "latest ask"))).unwrap();
        f.write_all(b"\n").unwrap();
        for _ in 0..padding_kb_after {
            f.write_all(&padding_line("mode", 1)).unwrap();
        }
        path
    }

    #[test]
    fn window_grows_until_it_holds_the_latest_metadata_block() {
        let far = metadata_layout("window-far", 1600, 300);
        let t = Transcript::open(far, Window::Main).unwrap();
        assert_eq!(t.facts().titles.ai.as_ref().unwrap().as_str(), "New");
        assert_eq!(t.facts().last_prompt.as_ref().unwrap().as_str(), "new");
        assert!(!t.facts().horizon.from_start);
        assert_eq!(t.facts().horizon.first_at, Some(at(7)));
        assert_eq!(t.facts().turn, Turn::Open { since: at(7) });
        assert!(
            t.facts().lines > 300 && t.facts().lines < 1500,
            "the 1 MB retry, not the 4 MB one"
        );

        let trailing = metadata_layout("window-trailing", 600, 0);
        let t = Transcript::open(trailing, Window::Main).unwrap();
        assert_eq!(
            t.facts().turn,
            Turn::Open { since: at(7) },
            "the conversation before a trailing block is folded"
        );
        assert!(t.facts().lines < 260);

        let small = Transcript::open(
            Path::new(FIXTURES).join("transcript/session.jsonl"),
            Window::Main,
        )
        .unwrap();
        assert!(small.facts().horizon.from_start);
    }

    #[test]
    fn window_start_survives_a_giant_single_line() {
        let dir = scratch("giant");
        let path = dir.join("giant.jsonl");
        let mut v = line(&json!({"type": "file-history-snapshot", "pad": "y".repeat(5 << 20)}));
        v.truncate(v.len() - 1);
        fs::write(&path, &v).unwrap();
        let mut t = Transcript::open(path.clone(), Window::Main).unwrap();
        assert_eq!(t.facts().lines, 0);
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"\"}\n").unwrap();
        f.write_all(&line(&human(1, "hello"))).unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);
        assert!(t.pump().unwrap());
        assert_eq!(t.facts().turn, Turn::Open { since: at(1) });
        assert!(!t.facts().horizon.from_start);
    }

    #[test]
    #[ignore]
    fn probe_real_transcripts() {
        let home = dirs::home_dir().unwrap();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for e in fs::read_dir(&home).unwrap().filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            if (name == ".claude" || name.starts_with(".claude-"))
                && e.path().join("sessions").is_dir()
            {
                dirs.push(e.path());
            }
        }
        let (mut files, mut lines, mut skipped, mut unknown, mut sidecars) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut unknown_types: BTreeMap<String, u64> = BTreeMap::new();
        let mut metas = (0u64, 0u64);
        let mut by_sid: HashMap<String, PathBuf> = HashMap::new();
        for dir in &dirs {
            for slug in fs::read_dir(dir.join("projects"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
            {
                for entry in fs::read_dir(slug.path())
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                {
                    let p = entry.path();
                    if p.extension().is_some_and(|x| x == "jsonl") {
                        files += 1;
                        let f = fold_file_counting(&p, &mut unknown_types);
                        lines += f.lines;
                        skipped += f.skipped;
                        unknown += f.unknown;
                        println!(
                            "FULL {} agent={} bash={} monitor={} wakeup={}",
                            p.display(),
                            f.open_count(BgKind::Agent),
                            f.open_count(BgKind::Shell),
                            f.open_count(BgKind::Monitor),
                            u8::from(f.wakeup.is_some())
                        );
                        by_sid.insert(
                            p.file_stem().unwrap().to_string_lossy().into_owned(),
                            p.clone(),
                        );
                    } else if p.is_dir() {
                        for sc in fs::read_dir(p.join("subagents"))
                            .into_iter()
                            .flatten()
                            .filter_map(Result::ok)
                        {
                            let sp = sc.path();
                            let name = sp.file_name().unwrap().to_string_lossy().into_owned();
                            if name.ends_with(".jsonl") {
                                sidecars += 1;
                                let f = fold_file_counting(&sp, &mut unknown_types);
                                lines += f.lines;
                                skipped += f.skipped;
                                unknown += f.unknown;
                            } else if name.ends_with(".meta.json") {
                                metas.0 += 1;
                                let id = crate::model::AgentId(
                                    name.trim_start_matches("agent-")
                                        .trim_end_matches(".meta.json")
                                        .to_owned(),
                                );
                                if crate::subagents::parse_meta(id, None, &fs::read(&sp).unwrap())
                                    .is_err()
                                {
                                    metas.1 += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        println!(
            "PROBE main_files={files} sidecar_files={sidecars} lines={lines} skipped={skipped} unknown={unknown}"
        );
        println!("PROBE unknown_types={unknown_types:?}");
        println!("PROBE metas={} meta_parse_failures={}", metas.0, metas.1);
        for dir in &dirs {
            for e in fs::read_dir(dir.join("sessions"))
                .unwrap()
                .filter_map(Result::ok)
            {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".json")
                    || !name
                        .trim_end_matches(".json")
                        .bytes()
                        .all(|b| b.is_ascii_digit())
                {
                    continue;
                }
                let Ok(reg) = serde_json::from_slice::<Value>(&fs::read(e.path()).unwrap()) else {
                    continue;
                };
                let sid = reg["sessionId"].as_str().unwrap_or("").to_owned();
                let status = reg["status"].as_str().unwrap_or("?");
                let kind = reg["kind"].as_str().unwrap_or("?");
                let Some(path) = by_sid.get(&sid) else {
                    println!(
                        "LIVE {} pid={} {kind} {status} sid={sid} transcript=none",
                        dir.display(),
                        name
                    );
                    continue;
                };
                let full = fold_file(path);
                let tail = Transcript::open(path.clone(), Window::Main).unwrap();
                let counts = |f: &Facts| {
                    format!(
                        "agents={} shells={} monitors={} wakeup={} turn={:?}",
                        f.open_count(BgKind::Agent),
                        f.open_count(BgKind::Shell),
                        f.open_count(BgKind::Monitor),
                        f.wakeup.is_some(),
                        std::mem::discriminant(&f.turn)
                    )
                };
                println!(
                    "LIVE {} pid={} {kind} {status} sid={sid}\n  path={}\n  full: {}\n  tail: {} from_start={} first_at={:?}\n  pending_agents={:?}",
                    dir.display(),
                    name,
                    path.display(),
                    counts(&full),
                    counts(tail.facts()),
                    tail.facts().horizon.from_start,
                    tail.facts().horizon.first_at,
                    match tail.facts().turn {
                        Turn::Closed { pending_agents, .. } => Some(pending_agents),
                        _ => None,
                    }
                );
            }
        }
    }

    fn fold_file_counting(path: &Path, unknown_types: &mut BTreeMap<String, u64>) -> Facts {
        let mut f = Facts::from_start();
        for l in fs::read(path).unwrap().split(|&b| b == b'\n') {
            let before = f.unknown;
            f.apply_line(l);
            if f.unknown > before {
                let s = String::from_utf8_lossy(l);
                let ty = s
                    .split("\"type\":\"")
                    .nth(1)
                    .and_then(|r| r.split('"').next())
                    .unwrap_or("?");
                *unknown_types.entry(ty.to_owned()).or_default() += 1;
            }
        }
        f
    }
}
