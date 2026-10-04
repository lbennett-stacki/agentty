//! Pure derivation: observed facts -> `Snapshot`. No IO, no clock (time is a parameter), no
//! user state (labels and groups are applied by `ui::layout`). Every product decision about
//! status, title, now-line, tree and auto-grouping lives here, as small functions over
//! `SessionView`, so fixture tests pin each one. This module never imports `world`: the
//! observer builds `SessionView`s and hands them in.

use std::collections::HashSet;
use std::path::Path;

use crate::jobs::{FanItem, FanKind, Job, JobState, Tempo};
use crate::model::{
    AgentId, At, AutoGroup, ConfigDir, Count, Cost, Ctx, Detail, DiagSource, Diagnostic,
    JobOutcome, Meta, Need, Node, NodeKind, NodeState, OpenTarget, PaneInfo, Parked, Problem, Row,
    RowKey, SafeText, Snapshot, Status, TaskId, TaskOutcome, Tree, UserKey, WaitingFor,
};
use crate::registry::{Name, NameSource, ProcKind, RegEntry, RegStatus};
use crate::sidecars::{Statusline, Task, TaskStatus, Team};
use crate::subagents::{AgentMeta, Subagents};
use crate::transcript::{BgKind, Facts, Turn};

/// Everything derive needs about one live session, borrowed from `World`. All fields are plain
/// references to parsed domain values, so tests construct a `SessionView` directly from
/// fixture parses without a `World`.
#[derive(Clone, Copy)]
pub struct SessionView<'a> {
    pub dir: &'a ConfigDir,
    pub reg: &'a RegEntry,
    pub job: Option<&'a Job>,
    pub facts: Option<&'a Facts>,
    pub subagents: Option<&'a Subagents>,
    pub tasks: &'a [Task],
    pub statusline: Option<&'a Statusline>,
    pub team: Option<&'a Team>,
    pub repo: Option<&'a Path>,
    /// The tmux pane snapshot of this refresh. `open_target` offers a pane only if it is here.
    pub panes: &'a [PaneInfo],
}

/// A bg job with no live process, retired within the window.
#[derive(Clone, Copy)]
pub struct ExitedView<'a> {
    pub dir: &'a ConfigDir,
    pub job: &'a Job,
    pub facts: Option<&'a Facts>,
    pub repo: Option<&'a Path>,
}

/// One subagent as `Subagents::agents` yields it: meta, rule-derived state, and the sidecar
/// fold while it runs. The decision functions take these as a slice so the agent-state rule
/// stays in `subagents` and tests can supply agents without a subagents dir on disk.
pub type AgentView<'a> = (&'a AgentMeta, NodeState, Option<&'a Facts>);

/// `job.detail` stands in for the now-line while the classifier wrote it this recently.
const DETAIL_FRESH_MS: i64 = 2 * 60 * 1000;

/// Build the whole snapshot. Runs on every loop iteration that refreshed; with 20 sessions it is
/// microseconds, so there is no incremental derive and no cache to invalidate.
/// Rows: every non-hidden live session via `live_row`, every exited job via `exited_row`.
/// Diagnostics: `world_diagnostics` plus one `UnknownValue` per row whose registry status,
/// kind, job state or fan kind was `Other`.
pub fn snapshot<'a>(
    sessions: impl Iterator<Item = SessionView<'a>>,
    exited: impl Iterator<Item = ExitedView<'a>>,
    world_diagnostics: &[Diagnostic],
    now: At,
) -> Snapshot {
    let mut snap = Snapshot { rows: Vec::new(), diagnostics: world_diagnostics.to_vec() };
    for v in sessions.filter(|v| !v.reg.hidden) {
        unknown_values(v.dir, Some(v.reg), v.job, &mut snap.diagnostics);
        snap.rows.push(live_row(v, now));
    }
    for v in exited {
        unknown_values(v.dir, None, Some(v.job), &mut snap.diagnostics);
        snap.rows.push(exited_row(v));
    }
    snap
}

fn unknown_values(dir: &ConfigDir, reg: Option<&RegEntry>, job: Option<&Job>, out: &mut Vec<Diagnostic>) {
    let mut push = |source, field, value: &str, path: String| {
        out.push(Diagnostic {
            source,
            problem: Problem::UnknownValue { field, value: value.to_owned() },
            path,
        })
    };
    if let Some(reg) = reg {
        let path = format!("{}/sessions/{}.json", dir.0.display(), reg.pid.0);
        if let RegStatus::Other(s) = &reg.status {
            push(DiagSource::Registry, "status", s, path.clone());
        }
        if let ProcKind::Other(s) = &reg.kind {
            push(DiagSource::Registry, "kind", s, path);
        }
    }
    if let Some(job) = job {
        let path = format!("{}/jobs/{}/state.json", dir.0.display(), job.id.0);
        if let JobState::Other(s) = &job.state {
            push(DiagSource::Job, "state", s, path.clone());
        }
        let mut seen = HashSet::new();
        for f in &job.fan {
            if let FanKind::Other(s) = &f.kind
                && seen.insert(s.as_str())
            {
                push(DiagSource::Job, "fan.kind", s, path.clone());
            }
        }
    }
}

/// The subagents of a live session, with their rule-derived states.
pub fn agents<'a>(v: &SessionView<'a>) -> Vec<AgentView<'a>> {
    match (v.subagents, v.facts) {
        (Some(s), Some(f)) => s.agents(f, true).collect(),
        _ => Vec::new(),
    }
}

pub fn live_row(v: SessionView<'_>, now: At) -> Row {
    let agents = agents(&v);
    let (status, since) = status(&v, &agents, now);
    let title = title(&v);
    let key = RowKey::Live { dir: v.dir.clone(), pid: v.reg.pid, proc_start: v.reg.proc_start.clone() };
    let user_key = match &v.reg.job {
        Some(job) => UserKey::Job(job.clone()),
        None => UserKey::Session(v.reg.session_id.clone()),
    };
    Row {
        key,
        user_key,
        auto_group: auto_group(&v),
        now: now_line(&status, &v, now),
        since,
        meta: meta(&v),
        tree: tree(&v, &agents, now),
        detail: detail(&v, now),
        open: open_target(&v, &title),
        title,
        status,
    }
}

pub fn exited_row(v: ExitedView<'_>) -> Row {
    let outcome = job_outcome(v.job);
    let title = pick_title(v.facts, None, Some(v.job), &v.job.id.0);
    let now = v.job.detail.clone().unwrap_or_else(|| SafeText::ours(outcome_word(outcome)));
    let facts = v.facts;
    Row {
        key: RowKey::Exited { dir: v.dir.clone(), job: v.job.id.clone() },
        user_key: UserKey::Job(v.job.id.clone()),
        auto_group: AutoGroup::Finished,
        status: Status::Exited(outcome),
        since: v.job.ended_at.or(v.job.updated_at).unwrap_or(At(0)),
        now,
        meta: Meta {
            branch: branch(facts, Some(v.job)),
            worktree: worktree(facts, Some(v.job)),
            prs: prs(facts, Some(v.job)),
            ctx: None,
            cost: facts.and_then(|f| f.exit).map(|e| Cost::Final(e.cost_usd)),
            remote_control: false,
            bg: true,
            dir_tag: v.dir.short().to_owned(),
        },
        tree: Tree::default(),
        detail: Detail {
            last_prompt: facts.and_then(|f| f.last_prompt.clone()),
            last_text: facts.and_then(|f| f.last_text.as_ref()).map(|t| t.first_line.clone()),
            open_tasks: Vec::new(),
            pending: Vec::new(),
            cwd: v.job.worktree_path.clone().unwrap_or_default(),
            config_dir: v.dir.0.clone(),
            session_id: v.job.session_id.clone(),
            job_id: Some(v.job.id.clone()),
        },
        open: OpenTarget::Attach { dir: v.dir.clone(), job: v.job.id.clone(), title: title.as_str().to_owned() },
        title,
    }
}

fn outcome_word(o: JobOutcome) -> &'static str {
    match o {
        JobOutcome::Done => "done",
        JobOutcome::Failed => "failed",
        JobOutcome::Stopped => "stopped",
        JobOutcome::Unknown => "exited",
    }
}

/// THE status table (GROUNDING 3.3). Returns the status and its `since`.
///
/// ```text
/// registry      transcript / job                              -> Status                   since
/// waiting(w)    -                                             -> NeedsYou(need_of(w))     statusUpdatedAt
/// busy          no transcript                                 -> Working                  statusUpdatedAt
/// busy          turn Open                                     -> Working                  max(statusUpdatedAt, turn.since)
/// busy          turn Closed/Interrupted                       -> Parked(parked())         turn end
/// busy          turn Unknown, tail is an unfinished tool call -> Working                  statusUpdatedAt
/// busy          turn Unknown, otherwise                       -> Parked(parked())         statusUpdatedAt
/// shell         -                                             -> Parked(parked())         statusUpdatedAt
/// idle          bg: tempo Blocked && needs                    -> NeedsYou(JobQuestion)    statusUpdatedAt
/// idle          wakeup in the future                          -> Parked(wakeup)           statusUpdatedAt
/// idle          -                                             -> Idle{asked_question}     statusUpdatedAt
/// Other         -                                             -> Idle{false} (+ diagnostic)
/// ```
/// `asked_question` = last turn Closed and `last_text.is_question`. `job.state` is never
/// consulted for liveness or Parked-vs-Idle (it says `done` while work runs, gotcha 4).
///
/// Turn Unknown means the window started mid-history with no turn boundary in it (common on
/// first sight of a long turn). A tool call with no later end-of-turn text is a turn still
/// streaming; anything else is treated as a turn that already ended.
pub fn status(v: &SessionView<'_>, agents: &[AgentView<'_>], now: At) -> (Status, At) {
    let reg_since = v.reg.status_since;
    match &v.reg.status {
        RegStatus::Waiting(w) => (Status::NeedsYou(need_of(w, v.facts)), reg_since),
        RegStatus::Busy => {
            let Some(facts) = v.facts else { return (Status::Working, reg_since) };
            match facts.turn {
                Turn::Open { since } => (Status::Working, reg_since.max(since)),
                Turn::Closed { at, .. } | Turn::Interrupted { at } => {
                    (Status::Parked(parked(v, agents, false, now)), at)
                }
                Turn::Unknown if facts.last_tool.is_some() && !facts.tail_looks_finished => {
                    (Status::Working, reg_since)
                }
                Turn::Unknown => (Status::Parked(parked(v, agents, false, now)), reg_since),
            }
        }
        RegStatus::Shell => (Status::Parked(parked(v, agents, true, now)), reg_since),
        RegStatus::Idle => {
            if let Some(job) = v.job
                && job.tempo == Some(Tempo::Blocked)
                && let Some(needs) = &job.needs
            {
                return (Status::NeedsYou(Need::JobQuestion(needs.clone())), reg_since);
            }
            let zero = Count::Exact(0);
            if let Some(p) = Parked::new(zero, zero, zero, 0, future_wakeup(v.facts, now)) {
                return (Status::Parked(p), reg_since);
            }
            let asked_question = v.facts.is_some_and(|f| {
                matches!(f.turn, Turn::Closed { .. }) && f.last_text.as_ref().is_some_and(|t| t.is_question)
            });
            (Status::Idle { asked_question }, reg_since)
        }
        RegStatus::Other(_) => (Status::Idle { asked_question: false }, reg_since),
    }
}

fn future_wakeup(facts: Option<&Facts>, now: At) -> Option<At> {
    facts.and_then(|f| f.wakeup).filter(|w| *w > now)
}

/// `WaitingFor::Permission` -> `Need::Permission { tool }` where `tool` is the name of
/// `facts.last_tool` if it is unanswered; every other reason -> `Need::Waiting(w)`.
pub fn need_of(w: &WaitingFor, facts: Option<&Facts>) -> Need {
    match w {
        WaitingFor::Permission => Need::Permission {
            tool: facts.and_then(|f| f.last_tool.as_ref()).filter(|t| !t.answered).map(|t| t.name.clone()),
        },
        other => Need::Waiting(other.clone()),
    }
}

/// What a parked session waits on. Source precedence: bg `fan` (authoritative roster) > the
/// transcript fold's `open_bg` > `turn.pending_agents`. Fan counts are `Exact`. Fold shell and
/// monitor counts are `Exact` only when the fold saw the whole file, else `AtLeast`. Fold agents
/// = max(open_bg agents, running tree agents, pending_agents), since pending_agents also counts
/// depth-2 agents the fold cannot see; the subagents dir is listed in full, so this is `Exact`
/// when it is known, else follows the horizon.
///
/// The registry then sets floors and ceilings, so `Parked::new` always succeeds:
/// - `shell`: no agent or workflow is running (it would say `busy`), and a local_bash or monitor
///   is, so agents and workflows are zeroed and an empty shell+monitor count becomes
///   `shells: AtLeast(1)`;
/// - `busy` with a closed turn: an agent or workflow is running, so an empty agent+workflow
///   count becomes `agents: AtLeast(1)`.
pub fn parked(v: &SessionView<'_>, agents: &[AgentView<'_>], registry_says_shell: bool, now: At) -> Parked {
    let (mut n_agents, n_shells, n_monitors, mut n_workflows) = match v.job {
        Some(job) => {
            let count = |k: FanKind| Count::Exact(job.open_work().filter(|f| f.kind == k).count() as u16);
            let workflows = job.open_work().filter(|f| f.kind == FanKind::Workflow).count() as u16;
            (count(FanKind::Agent), count(FanKind::Shell), count(FanKind::Monitor), workflows)
        }
        None => {
            let complete = v.facts.is_some_and(|f| f.horizon.from_start);
            let count = |n: u16, exact: bool| if exact { Count::Exact(n) } else { Count::AtLeast(n) };
            let fold = |k| v.facts.map_or(0, |f| f.open_count(k));
            let pending = match v.facts.map(|f| f.turn) {
                Some(Turn::Closed { pending_agents, .. }) => pending_agents,
                _ => 0,
            };
            let running = agents.iter().filter(|(_, s, _)| *s == NodeState::Running).count() as u16;
            let agents_n = fold(BgKind::Agent).max(running).max(pending);
            (
                count(agents_n, complete || v.subagents.is_some()),
                count(fold(BgKind::Shell), complete),
                count(fold(BgKind::Monitor), complete),
                0,
            )
        }
    };
    let mut n_shells = n_shells;
    if registry_says_shell {
        n_agents = Count::Exact(0);
        n_workflows = 0;
        if !n_shells.is_positive() && !n_monitors.is_positive() {
            n_shells = Count::AtLeast(1);
        }
    } else if !n_agents.is_positive() && n_workflows == 0 {
        n_agents = Count::AtLeast(1);
    }
    Parked::new(n_agents, n_shells, n_monitors, n_workflows, future_wakeup(v.facts, now))
        .expect("the registry floors above guarantee a positive count")
}

/// Title precedence (SPEC; GROUNDING 3.4), user label excluded (applied by `ui::layout`):
/// custom-title > agent-name > registry name (source User|Auto) > ai-title > job name >
/// registry name (any source) > short sessionId. The head-scan "first prompt" fallback is
/// omitted: SPEC forbids full scans.
pub fn title(v: &SessionView<'_>) -> SafeText {
    let sid = &v.reg.session_id.0;
    pick_title(v.facts, v.reg.name.as_ref(), v.job, &sid[..sid.len().min(8)])
}

fn pick_title(facts: Option<&Facts>, reg_name: Option<&Name>, job: Option<&Job>, fallback: &str) -> SafeText {
    let titles = facts.map(|f| &f.titles);
    let strong_name = reg_name.filter(|n| matches!(n.source, NameSource::User | NameSource::Auto));
    [
        titles.and_then(|t| t.custom.clone()),
        titles.and_then(|t| t.agent_name.clone()),
        strong_name.map(|n| SafeText::redact(&n.value)),
        titles.and_then(|t| t.ai.clone()),
        job.and_then(|j| j.name.as_deref()).map(SafeText::redact),
        reg_name.map(|n| SafeText::redact(&n.value)),
    ]
    .into_iter()
    .flatten()
    .find(|t| !t.is_empty())
    .unwrap_or_else(|| SafeText::redact(fallback))
}

/// The second line, specific to the status (GROUNDING 3.5):
/// - NeedsYou(Permission{tool}): "permission prompt · <tool>: <summary of the dangling tool_use>"
/// - NeedsYou(Waiting(w)): w.label()
/// - NeedsYou(JobQuestion): the question text
/// - Working: the latest of last_tool ("<tool>: <summary>") and last_text (first line)
/// - Parked: "waiting on 3 agents, 1+ shell, 1 monitor · wakes 14:05" (`describe_parked`)
/// - Idle: away_summary ?? last_text ?? last_prompt
/// - bg rows: job.detail replaces Working/Idle text when job.updated_at is within 2 minutes.
pub fn now_line(status: &Status, v: &SessionView<'_>, now: At) -> SafeText {
    let fresh_detail = v
        .job
        .filter(|j| j.updated_at.is_some_and(|u| now.millis_since(u) <= DETAIL_FRESH_MS))
        .and_then(|j| j.detail.clone());
    let empty = SafeText::default;
    match status {
        Status::NeedsYou(Need::Permission { .. }) => {
            let label = WaitingFor::Permission.label();
            match v.facts.and_then(|f| f.last_tool.as_ref()).filter(|t| !t.answered) {
                Some(call) => SafeText::redact(&format!("{label} · {}", tool_line(&call.name, &call.summary).as_str())),
                None => SafeText::ours(label),
            }
        }
        Status::NeedsYou(Need::Waiting(w)) => SafeText::redact(w.label()),
        Status::NeedsYou(Need::JobQuestion(q)) => q.clone(),
        Status::Working => fresh_detail.or_else(|| v.facts.and_then(doing)).unwrap_or_else(empty),
        Status::Parked(p) => SafeText::ours(format!("waiting on {}", describe_parked(p))),
        Status::Idle { .. } => fresh_detail
            .or_else(|| {
                let f = v.facts?;
                f.away_summary
                    .clone()
                    .or_else(|| f.last_text.as_ref().map(|t| t.first_line.clone()))
                    .or_else(|| f.last_prompt.clone())
            })
            .unwrap_or_else(empty),
        Status::Exited(o) => v.job.and_then(|j| j.detail.clone()).unwrap_or_else(|| SafeText::ours(outcome_word(*o))),
    }
}

/// The latest thing a fold shows the agent doing: its last tool call or its last text,
/// whichever is newer.
fn doing(f: &Facts) -> Option<SafeText> {
    let tool = f.last_tool.as_ref().map(|t| (t.at, tool_line(&t.name, &t.summary)));
    let text = f.last_text.as_ref().map(|t| (t.at, t.first_line.clone()));
    match (tool, text) {
        (Some(a), Some(b)) => Some(if a.0 >= b.0 { a.1 } else { b.1 }),
        (a, b) => a.or(b).map(|(_, s)| s),
    }
}

fn tool_line(name: &str, summary: &SafeText) -> SafeText {
    if summary.is_empty() {
        SafeText::redact(name)
    } else {
        SafeText::redact(&format!("{name}: {}", summary.as_str()))
    }
}

/// Expandable tree (depth 2): agents placed by `parent`; open shells, monitors and workflows as
/// leaves; a pending wakeup as a leaf. Finished agents at each level collapse into
/// `Tree::finished`; Running, Killed and Orphaned stay as nodes with their state.
///
/// For bg rows `fan` is the authoritative roster (GROUNDING 3.9): an agent with a fan entry is
/// Running while that entry is open and Finished once it has `doneAt`, whatever its sidecar
/// shows; fan agents with no meta still appear; leaves come from `fan`, not `open_bg`.
pub fn tree(v: &SessionView<'_>, agents: &[AgentView<'_>], now: At) -> Tree {
    let known: HashSet<&AgentId> = agents.iter().map(|(m, _, _)| &m.id).collect();
    let mut t = agent_level(v, agents, &known, None, 0);
    for item in roster(v) {
        if item.kind == Work::Agent && known.contains(&AgentId(item.id.0.clone())) {
            continue;
        }
        t.live.push(Node {
            id: item.id,
            kind: match item.kind {
                Work::Agent => NodeKind::Agent { agent_type: "agent".to_owned(), model: None },
                Work::Shell => NodeKind::Shell,
                Work::Monitor => NodeKind::Monitor,
                Work::Workflow => NodeKind::Workflow,
            },
            state: NodeState::Running,
            label: item.label.clone(),
            since: item.since.unwrap_or(v.reg.status_since),
            now: None,
            children: Tree::default(),
        });
    }
    if let Some(at) = future_wakeup(v.facts, now) {
        t.live.push(Node {
            id: TaskId("wakeup".to_owned()),
            kind: NodeKind::Wakeup,
            state: NodeState::Running,
            label: SafeText::ours(format!("wakes {}", clock(at))),
            since: v.facts.and_then(|f| f.last_activity).unwrap_or(v.reg.status_since),
            now: None,
            children: Tree::default(),
        });
    }
    t
}

fn agent_level(
    v: &SessionView<'_>,
    agents: &[AgentView<'_>],
    known: &HashSet<&AgentId>,
    parent: Option<&AgentId>,
    depth: u8,
) -> Tree {
    let mut t = Tree::default();
    // Real data has depth <= 2; the cap only stops a malformed self-parent from recursing forever.
    if depth > 2 {
        return t;
    }
    for &(meta, state, sidecar) in agents {
        let placed_under = meta.parent.as_ref().filter(|p| known.contains(p));
        if placed_under != parent {
            continue;
        }
        let state = fan_agent(v.job, &meta.id).map_or(state, fan_state);
        if let NodeState::Finished(_) = state {
            t.finished += 1;
            continue;
        }
        t.live.push(Node {
            id: TaskId::from(&meta.id),
            kind: NodeKind::Agent { agent_type: meta.agent_type.clone(), model: meta.model.clone() },
            state,
            label: meta.description.clone(),
            since: agent_started(v, agents, &meta.id, sidecar),
            now: (state == NodeState::Running).then(|| sidecar.and_then(doing)).flatten(),
            children: agent_level(v, agents, known, Some(&meta.id), depth + 1),
        });
    }
    t
}

fn fan_agent<'a>(job: Option<&'a Job>, id: &AgentId) -> Option<&'a FanItem> {
    job?.fan.iter().find(|f| f.kind == FanKind::Agent && f.id == id.0)
}

fn fan_state(f: &FanItem) -> NodeState {
    match (f.done_at, f.failed) {
        (None, _) => NodeState::Running,
        (Some(_), true) => NodeState::Finished(TaskOutcome::Failed),
        (Some(_), false) => NodeState::Finished(TaskOutcome::Completed),
    }
}

/// Launch time: the launching fold's `open_bg` (main file, or a parent agent's sidecar for
/// depth 2), else the fan entry, else the agent's own first record.
fn agent_started(v: &SessionView<'_>, agents: &[AgentView<'_>], id: &AgentId, own: Option<&Facts>) -> At {
    let task = TaskId::from(id);
    v.facts
        .into_iter()
        .chain(agents.iter().filter_map(|(_, _, f)| *f))
        .find_map(|f| f.open_bg.get(&task).map(|b| b.started))
        .or_else(|| fan_agent(v.job, id).and_then(|f| f.started_at))
        .or_else(|| own.and_then(|f| f.horizon.first_at))
        .unwrap_or(v.reg.status_since)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Work {
    Agent,
    Shell,
    Monitor,
    Workflow,
}

struct RosterItem<'a> {
    id: TaskId,
    kind: Work,
    label: &'a SafeText,
    since: Option<At>,
}

/// Open background work (GROUNDING 3.10): `fan` without `doneAt` for bg rows, `open_bg`
/// otherwise. Todos and unknown fan kinds are not background work.
fn roster<'a>(v: &SessionView<'a>) -> Vec<RosterItem<'a>> {
    if let Some(job) = v.job {
        return job
            .open_work()
            .filter_map(|f| {
                let kind = match f.kind {
                    FanKind::Agent => Work::Agent,
                    FanKind::Shell => Work::Shell,
                    FanKind::Monitor => Work::Monitor,
                    FanKind::Workflow => Work::Workflow,
                    _ => return None,
                };
                Some(RosterItem { id: TaskId(f.id.clone()), kind, label: &f.label, since: f.started_at })
            })
            .collect();
    }
    v.facts
        .into_iter()
        .flat_map(|f| &f.open_bg)
        .map(|(id, b)| RosterItem {
            id: id.clone(),
            kind: match b.kind {
                BgKind::Agent => Work::Agent,
                BgKind::Shell => Work::Shell,
                BgKind::Monitor => Work::Monitor,
            },
            label: &b.label,
            since: Some(b.started),
        })
        .collect()
}

/// Right-aligned meta. The statusline is used only when it is at least as new as the latest
/// usage record (else it is stale). ctx: statusline percent when it reports a window, else
/// absolute tokens from `facts.usage`. cost: statusline (Live), else a trailing `cost-state`
/// (Final), else none; never estimated.
pub fn meta(v: &SessionView<'_>) -> Meta {
    let usage = v.facts.and_then(|f| f.usage.as_ref());
    let statusline = v.statusline.filter(|s| usage.is_none_or(|u| s.at >= u.at));
    let percent = statusline.and_then(|s| {
        let window = s.window.filter(|w| *w > 0)?;
        let pct = s.ctx_used_pct.or_else(|| usage.map(|u| u.ctx_tokens as f64 * 100.0 / window as f64))?;
        Some(Ctx::Percent(pct.round().clamp(0.0, 100.0) as u8))
    });
    let cost = statusline
        .and_then(|s| s.cost_usd)
        .map(Cost::Live)
        .or_else(|| v.facts.and_then(|f| f.exit).map(|e| Cost::Final(e.cost_usd)));
    Meta {
        branch: branch(v.facts, v.job),
        worktree: worktree(v.facts, v.job),
        prs: prs(v.facts, v.job),
        ctx: percent.or_else(|| usage.map(|u| Ctx::Tokens(u.ctx_tokens))),
        cost,
        remote_control: v.reg.remote_control || v.job.is_some_and(|j| j.remote_control),
        bg: v.reg.kind == ProcKind::Bg,
        dir_tag: v.dir.short().to_owned(),
    }
}

fn branch(facts: Option<&Facts>, job: Option<&Job>) -> Option<String> {
    facts
        .and_then(|f| f.branch.clone().or_else(|| f.worktree.as_ref().and_then(|w| w.branch.clone())))
        .or_else(|| job.and_then(|j| j.worktree_branch.clone()))
}

fn worktree(facts: Option<&Facts>, job: Option<&Job>) -> Option<String> {
    facts.and_then(|f| f.worktree.as_ref()).map(|w| w.name.clone()).or_else(|| {
        job.and_then(|j| j.worktree_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    })
}

fn prs(facts: Option<&Facts>, job: Option<&Job>) -> Vec<u32> {
    let mut out: Vec<u32> = facts.map(|f| f.prs.clone()).unwrap_or_default();
    for pr in job.map(|j| j.prs.as_slice()).unwrap_or_default() {
        if !out.contains(pr) {
            out.push(*pr);
        }
    }
    out
}

pub fn detail(v: &SessionView<'_>, now: At) -> Detail {
    let facts = v.facts;
    let mut pending: Vec<SafeText> = roster(v)
        .into_iter()
        .map(|item| {
            let kind = match item.kind {
                Work::Agent => "agent",
                Work::Shell => "shell",
                Work::Monitor => "monitor",
                Work::Workflow => "workflow",
            };
            SafeText::redact(&format!("{kind} · {}", item.label.as_str()))
        })
        .collect();
    if let Some(at) = future_wakeup(facts, now) {
        pending.push(SafeText::ours(format!("wakeup · {}", clock(at))));
    }
    Detail {
        last_prompt: facts.and_then(|f| f.last_prompt.clone()),
        last_text: facts.and_then(|f| f.last_text.as_ref()).map(|t| t.first_line.clone()),
        open_tasks: v
            .tasks
            .iter()
            .filter(|t| t.status != TaskStatus::Completed)
            .map(|t| t.subject.clone())
            .collect(),
        pending,
        cwd: v.reg.cwd.clone(),
        config_dir: v.dir.0.clone(),
        session_id: Some(v.reg.session_id.clone()),
        job_id: v.reg.job.clone(),
    }
}

/// Enter's behaviour, decided once per row:
/// bg (has a job id) -> Attach; interactive + pane present in `v.panes` -> Pane; otherwise
/// Unreachable, naming the pane that is gone or the surface it runs in (VS Code, Desktop, a
/// terminal outside tmux).
pub fn open_target(v: &SessionView<'_>, title: &SafeText) -> OpenTarget {
    if let Some(job) = &v.reg.job {
        return OpenTarget::Attach { dir: v.dir.clone(), job: job.clone(), title: title.as_str().to_owned() };
    }
    match &v.reg.pane {
        Some(pane) if v.panes.iter().any(|p| &p.id == pane) => OpenTarget::Pane(pane.clone()),
        Some(pane) => OpenTarget::Unreachable(format!("tmux pane {} is gone", pane.as_str())),
        None => {
            let surface = match v.reg.entrypoint.as_str() {
                "cli" | "" => "a terminal outside tmux",
                "claude-vscode" => "VS Code",
                "claude-desktop" => "Claude Desktop",
                other => other,
            };
            OpenTarget::Unreachable(format!("running in {surface}"))
        }
    }
}

/// Automatic group: team (session is the lead, or a member by pane) > repo root > Elsewhere.
/// The user's assignment, applied in `ui::layout`, overrides this.
pub fn auto_group(v: &SessionView<'_>) -> AutoGroup {
    if let Some(team) = v.team {
        let lead = team.lead.as_ref() == Some(&v.reg.session_id);
        let member = v.reg.pane.is_some() && team.members.iter().any(|m| m.pane == v.reg.pane);
        if lead || member {
            return AutoGroup::Team(team.name.clone());
        }
    }
    match v.repo {
        Some(root) => AutoGroup::Repo(root.to_path_buf()),
        None => AutoGroup::Elsewhere,
    }
}

/// Render-ready "3 agents, 1+ shell, 1 monitor · wakes 14:05". Zero counts omitted;
/// `AtLeast(n)` renders `n+`.
pub fn describe_parked(p: &Parked) -> String {
    let counted = [
        (p.agents(), "agent"),
        (p.shells(), "shell"),
        (p.monitors(), "monitor"),
        (Count::Exact(p.workflows()), "workflow"),
    ];
    let parts: Vec<String> = counted
        .into_iter()
        .filter(|(c, _)| c.is_positive())
        .map(|(c, noun)| match c {
            Count::Exact(1) => format!("1 {noun}"),
            Count::Exact(n) => format!("{n} {noun}s"),
            Count::AtLeast(1) => format!("1+ {noun}"),
            Count::AtLeast(n) => format!("{n}+ {noun}s"),
        })
        .collect();
    let mut out = parts.join(", ");
    if let Some(at) = p.wakeup() {
        if !out.is_empty() {
            out.push_str(" · ");
        }
        out.push_str(&format!("wakes {}", clock(at)));
    }
    out
}

/// Local wall-clock `HH:MM`.
fn clock(at: At) -> String {
    chrono::DateTime::from_timestamp_millis(at.0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Map a job's terminal classifier state to the Exited outcome. Non-terminal states on a dead
/// process are Unknown (it crashed or was killed mid-flight).
pub fn job_outcome(job: &Job) -> JobOutcome {
    match job.state {
        JobState::Done => JobOutcome::Done,
        JobState::Failed => JobOutcome::Failed,
        JobState::Stopped => JobOutcome::Stopped,
        _ => JobOutcome::Unknown,
    }
}
