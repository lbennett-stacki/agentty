use std::cmp::Ordering;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct At(pub i64);

impl At {
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigDir(pub PathBuf);

impl ConfigDir {
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(pub String);

impl From<&AgentId> for TaskId {
    fn from(a: &AgentId) -> TaskId {
        TaskId(a.0.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolUseId(pub String);

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: PaneId,
    pub pid: Pid,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowKey {
    Live { dir: ConfigDir, pid: Pid, proc_start: ProcStart },
    Exited { dir: ConfigDir, job: JobId },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UserKey {
    Job(JobId),
    Session(SessionId),
}

impl UserKey {
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    pub dir: ConfigDir,
    pub pid: Pid,
    pub proc_start: ProcStart,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupName(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Default, Hash)]
pub struct SafeText(String);

impl SafeText {
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
            name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
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

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    NeedsYou(Need),
    Working,
    Parked(Parked),
    Idle { asked_question: bool },
    Exited(JobOutcome),
}

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
    Permission { tool: Option<String> },
    Waiting(WaitingFor),
    JobQuestion(SafeText),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitingFor {
    Permission,
    Input,
    Dialog,
    Sandbox,
    Worker,
    GoalProposal,
    Unspecified,
    Other(String),
}

impl WaitingFor {
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
    pub fn wakeup(&self) -> Option<At> {
        self.wakeup
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskOutcome {
    Completed,
    Failed,
    Killed,
    Stopped,
    AwaitingNotice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Running,
    Finished(TaskOutcome),
    Killed,
    Orphaned,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AutoGroup {
    Team(String),
    Repo(PathBuf),
    Elsewhere,
    Finished,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupId {
    User { order: u32, name: GroupName },
    Auto(AutoGroup),
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: RowKey,
    pub user_key: UserKey,
    pub auto_group: AutoGroup,
    pub status: Status,
    pub since: At,
    pub title: SafeText,
    pub now: SafeText,
    pub meta: Meta,
    pub tree: Tree,
    pub detail: Detail,
    pub open: OpenTarget,
}

impl Row {
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
    pub dir_tag: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ctx {
    Percent(u8),
    Tokens(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cost {
    Live(f64),
    Final(f64),
}

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
    pub id: TaskId,
    pub kind: NodeKind,
    pub state: NodeState,
    pub label: SafeText,
    pub since: At,
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
    Pane(PaneId),
    Attach { dir: ConfigDir, job: JobId, title: String },
    Unreachable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub source: DiagSource,
    pub problem: Problem,
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
    Unreadable,
    Unparsable,
    MissingField(&'static str),
    UnknownValue { field: &'static str, value: String },
    Unavailable,
    Deferred,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    SetLabel { key: UserKey, label: Option<String> },
    SetGroup { key: UserKey, group: Option<GroupName> },
    Anchor { session: SessionId, anchor: Anchor },
    Rekey { from: SessionId, to: SessionId },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Jump(PaneId),
    Attach { dir: ConfigDir, job: JobId, title: String },
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
